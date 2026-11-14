import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.environ.get("REPO", os.path.dirname(os.path.dirname(ROOT)))
SOURCE = os.path.join(REPO, "crates/core/sys/src/windows_sys.rs")
LIBC = os.path.join(REPO, "crates/core/sys/src/libc/windows.rs")
WORK = os.path.join(ROOT, "gen/work")
CC = os.environ.get("CC_WIN", "x86_64-w64-mingw32-gcc")
WINE = os.environ.get("WINE", "wine")

INTEGERS = {"u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "usize", "isize", "c_int", "c_uint", "c_long", "c_ulong", "c_short", "c_ushort", "uintptr_t", "intptr_t", "sighandler_t", "size_t"}

HEADERS = """#define _WIN32_WINNT 0x0A00
#define CERT_CHAIN_PARA_HAS_EXTRA_FIELDS
#define NTDDI_VERSION 0x0A000000
#include <winsock2.h>
#include <ws2tcpip.h>
#include <mswsock.h>
#include <windows.h>
#include <winternl.h>
#include <wincrypt.h>
#include <bcrypt.h>
#include <tlhelp32.h>
#include <winioctl.h>
#include <shellapi.h>
#include <shlobj.h>
#include <sddl.h>
#include <rpc.h>
#include <oleauto.h>
#include <winver.h>
#include <mmsystem.h>
#include <lmcons.h>
#include <errno.h>
#include <fcntl.h>
#include <io.h>
#include <signal.h>
#include <sys/stat.h>
#include <time.h>
#include <locale.h>
#include <stdio.h>
#include <stddef.h>
#include <stdlib.h>
"""


def parse(path, root):
    items = []
    aliases = {}
    stack = []
    depth = 0
    lines = open(path).read().split("\n")
    index = 0
    while index < len(lines):
        line = lines[index]
        stripped = line.strip()
        module = re.match(r"pub mod (\w+) \{$", stripped)
        if module:
            stack.append((module.group(1), depth))
            depth += 1
            index += 1
            continue
        alias = re.match(r"pub type (\w+) = ([\w:]+);$", stripped)
        if alias:
            aliases[alias.group(1)] = alias.group(2).split("::")[-1]
        record = re.match(r"pub (struct|union) (\w+) \{$", stripped)
        if record:
            fields = []
            index += 1
            while lines[index].strip() != "}":
                field = re.match(r"pub (r#)?(\w+): ", lines[index].strip())
                if field:
                    fields.append(field.group(2))
                index += 1
            items.append(("record", [name for name, _ in stack], record.group(2), fields, record.group(1)))
            index += 1
            continue
        constant = re.match(r"pub const (\w+): ([\w:]+) = (.*)", stripped)
        if constant:
            items.append(("const", [name for name, _ in stack], constant.group(1), constant.group(2).split("::")[-1], None))
        depth += line.count("{") - line.count("}")
        while stack and depth <= stack[-1][1]:
            stack.pop()
        index += 1
    resolved = {}
    for name in aliases:
        seen = name
        steps = 0
        while seen in aliases and steps < len(aliases):
            seen = aliases[seen]
            steps += 1
        resolved[name] = seen
    kept = []
    for kind, path_parts, name, extra, flavor in items:
        if kind == "const":
            base = resolved.get(extra, extra)
            if base not in INTEGERS:
                continue
        kept.append((kind, root + path_parts, name, extra, flavor))
    return kept


LIBC_NAMES = {"stat": "struct _stat64", "timespec": "struct timespec", "tm": "struct tm", "timeval": "struct timeval", "sockaddr": "struct sockaddr"}


def entries(items):
    out = []
    for kind, path_parts, name, extra, flavor in items:
        rust = "::".join(path_parts + [name])
        label = "::".join(path_parts[1:] + [name])
        libc = path_parts[-1] == "libc"
        if kind == "record":
            if libc and name not in LIBC_NAMES:
                continue
            c_name = LIBC_NAMES[name] if libc else name
            out.append((f"S {label}", f"sizeof({c_name})", f"core::mem::size_of::<{rust}>()"))
            out.append((f"A {label}", f"_Alignof({c_name})", f"core::mem::align_of::<{rust}>()"))
            if flavor == "union":
                continue
            for field in extra:
                if field.startswith("_") or field.startswith("Anonymous"):
                    continue
                out.append((f"F {label}.{field}", f"offsetof({c_name}, {field})", f"core::mem::offset_of!({rust}, {field})"))
        else:
            out.append((f"C {label}", f"{name}", f"{rust}"))
    return out


def compile_c(rows):
    os.makedirs(WORK, exist_ok=True)
    source = os.path.join(WORK, "layout.c")
    first = HEADERS.count("\n") + 2
    with open(source, "w") as handle:
        handle.write(HEADERS)
        handle.write("int main(void) {\n")
        for label, c_expr, _ in rows:
            handle.write(f'printf("%s %lld\\n", "{label}", (long long)({c_expr}));\n')
        handle.write("return 0;\n}\n")
    result = subprocess.run(
        [CC, "-w", "-fmax-errors=0", "-o", os.path.join(WORK, "layout.exe"), source, "-lws2_32"],
        capture_output=True,
        text=True,
    )
    bad = set()
    for match in re.finditer(r"layout\.c:(\d+):\d+: error", result.stderr):
        bad.add(int(match.group(1)) - first)
    return result.returncode, bad, result.stderr


def main():
    items = parse(SOURCE, ["nanocode_sys", "windows_sys"]) + parse(LIBC, ["nanocode_sys", "libc"])
    rows = entries(items)
    print("candidates", len(rows))
    dropped = []
    for _ in range(50):
        code, bad, stderr = compile_c(rows)
        if code == 0:
            break
        bad = {i for i in bad if 0 <= i < len(rows)}
        if not bad:
            print(stderr[-4000:])
            sys.exit(1)
        dropped += [rows[i][0] for i in sorted(bad)]
        rows = [row for i, row in enumerate(rows) if i not in bad]
    output = subprocess.run([WINE, os.path.join(WORK, "layout.exe")], capture_output=True, text=True)
    expected = [line.strip() for line in output.stdout.splitlines() if line.strip()]
    if len(expected) != len(rows):
        print("layout.exe printed", len(expected), "rows for", len(rows))
        print(output.stderr[-3000:])
        sys.exit(1)
    values = {}
    for line in expected:
        label, value = line.rsplit(" ", 1)
        values[label] = int(value)
    with open(os.path.join(ROOT, "src/layout.rs"), "w") as handle:
        handle.write("pub const ROWS: &[(&str, i128, i128, usize)] = &[\n")
        for label, _, rust in rows:
            if label.startswith("C "):
                handle.write(f'    ("{label}", {rust} as i128, {values[label]}, core::mem::size_of_val(&{rust})),\n')
            else:
                handle.write(f'    ("{label}", {rust} as i128, {values[label]}, 8),\n')
        handle.write("];\n")
    with open(os.path.join(WORK, "dropped.txt"), "w") as handle:
        handle.write("\n".join(dropped) + "\n")
    print("rows", len(rows), "dropped", len(dropped))


main()
