import json, os, re, subprocess, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import ROOT, emit

FUNCTION_LINES_MAX = 70
SKIPPED = ("target", "patches", "vendor", "output", "node_modules", "tests", "benches")
FUNCTION = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|extern\s+\"[^\"]*\")\s+)*fn\s+\w+")
LOCK = re.compile(r"\b(?:Mutex|RwLock|Condvar)\b|\bparking_lot\b")
TIME = re.compile(r"\bSystemTime::now\b|\bstd::time::(?:Instant|SystemTime)\b|\btime::\{[^}]*\b(?:Instant|SystemTime)\b|\btokio::time\b|\bgetrandom\b|\benv::var(?:_os)?\s*\(|\brand::")
SPAWN = re.compile(r"\btokio::(?:task::)?spawn(?:_local|_blocking)?\b")
UNBOUNDED = re.compile(r"\bunbounded_channel\b|\bmpsc::unbounded\b|\bunbounded\s*\(|\bunbounded::(?:channel|Sender|Receiver)\b|\bUnbounded(?:Sender|Receiver)\b")
ASSERT = re.compile(r"(?<![\w!])assert(?:_eq|_ne)?!\s*\(")
LIMIT = re.compile(r"^\s*(?:pub\s*\((?:crate|super|self|in [^)]*)\)\s*)?(?:const|static)\s+(?:MAX|MIN)_[A-Z0-9_]*\s*:", re.M)


def code_only(text):
    out = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            depth = 0
            while i < n:
                if text.startswith("/*", i):
                    depth += 1
                    i += 2
                elif text.startswith("*/", i):
                    depth -= 1
                    i += 2
                    if depth == 0:
                        break
                else:
                    out.append("\n" if text[i] == "\n" else "")
                    i += 1
        elif re.match(r"b?r#*\"", text[i:i + 260]) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            match = re.match(r"b?r(#*)\"", text[i:])
            end = text.find("\"" + match.group(1), i + match.end())
            end = n if end < 0 else end + 1 + len(match.group(1))
            out.append("\"\"" + "\n" * text.count("\n", i, end))
            i = end
        elif c == "\"" or (c == "b" and text.startswith("b\"", i) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_"))):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != "\"":
                j += 2 if text[j] == "\\" else 1
            out.append("\"\"" + "\n" * text.count("\n", i, j))
            i = j + 1
        elif c == "'":
            match = re.match(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'\n])'", text[i:])
            if match:
                out.append("' '")
                i += match.end()
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def long_functions(lines):
    found = []
    for start, line in enumerate(lines):
        if not FUNCTION.match(line):
            continue
        depth = 0
        opened = False
        for end in range(start, len(lines)):
            for c in lines[end]:
                if c == "{":
                    depth += 1
                    opened = True
                elif c == "}":
                    depth -= 1
                elif c == ";" and not opened and depth == 0:
                    break
            else:
                if not (opened and depth <= 0):
                    continue
                if end - start + 1 > FUNCTION_LINES_MAX:
                    found.append((start + 1, end - start + 1))
                break
            break
    return found


def compound_asserts(code):
    found = []
    for match in ASSERT.finditer(code):
        depth = 0
        i = match.end()
        argument = []
        while i < len(code):
            c = code[i]
            if c in "([{":
                depth += 1
            elif c in ")]}":
                if depth == 0:
                    break
                depth -= 1
            elif c == "," and depth == 0:
                break
            argument.append(c if depth == 0 else " ")
            i += 1
        if "&&" in "".join(argument) and match.group(0).startswith("assert!"):
            found.append(code.count("\n", 0, match.start()) + 1)
    return found


def packages():
    metadata = json.loads(subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"], cwd=ROOT, check=True, capture_output=True, text=True).stdout)
    roots = {os.path.dirname(p["manifest_path"]): p["name"] for p in metadata["packages"]}
    return {root: name for root, name in roots.items() if "/patches/" not in root and "/vendor/" not in root}


def sources(root, roots):
    for directory, dirs, files in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d not in SKIPPED and os.path.join(directory, d) not in roots)
        for name in sorted(files):
            if name.endswith(".rs"):
                yield os.path.join(directory, name)


def deterministic(root):
    relative = os.path.relpath(root, ROOT)
    return relative.startswith("crates/runtime/") or "durability" in relative or "/store-" in relative


def check(metric, name, places):
    if places:
        listed = ", ".join(places[:3]) + (f" and {len(places) - 3} more" if len(places) > 3 else "")
        return f"{metric} {name} fail note \"{len(places)}: {listed}\""
    return f"{metric} {name} pass"


def main():
    roots = packages()
    lines = []
    for root, name in sorted(roots.items(), key=lambda item: item[1]):
        found = {key: [] for key in ("locks", "platform-time", "runtime-spawn", "unbounded-queues", "assertions", "split-assertions", "limit-names", "function-length")}
        asserts = 0
        functions = 0
        for path in sources(root, roots):
            relative = os.path.relpath(path, ROOT)
            code = code_only(open(path, errors="replace").read())
            rows = code.split("\n")
            source = f"{os.sep}src{os.sep}" in path
            for number, row in enumerate(rows, 1):
                place = f"{relative}:{number}"
                if LOCK.search(row):
                    found["locks"].append(place)
                if source and TIME.search(row):
                    found["platform-time"].append(place)
                if source and SPAWN.search(row):
                    found["runtime-spawn"].append(place)
                if source and UNBOUNDED.search(row):
                    found["unbounded-queues"].append(place)
                if FUNCTION.match(row):
                    functions += 1
            asserts += len(ASSERT.findall(code))
            found["split-assertions"] += [f"{relative}:{number}" for number in compound_asserts(code)]
            found["limit-names"] += [f"{relative}:{code.count(chr(10), 0, m.start()) + 1}" for m in LIMIT.finditer(code)]
            found["function-length"] += [f"{relative}:{start} ({length} lines)" for start, length in long_functions(rows)]
        for metric, places in found.items():
            if metric in ("platform-time", "runtime-spawn") and not deterministic(root):
                continue
            if metric == "assertions":
                if functions:
                    lines.append(f"assertions {name} pass" if asserts else f"assertions {name} fail note \"no assert! in {functions} functions\"")
                continue
            lines.append(check(metric, name, places))
    emit(lines)


main()
