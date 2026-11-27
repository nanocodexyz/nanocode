import fcntl, hashlib, json, os, re, shutil, subprocess, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import ROOT, emit

CACHE = os.path.join(os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")), "nanocode-uphill", "build")
EDITS = {"nanocode-sys": "crates/core/sys/src/lib.rs", "nanocode-code-mode": "crates/runtime/code-mode/src/lib.rs", "nanocode-cli": "bin/nanocode/src/lib.rs"}


def git(*args, cwd=ROOT):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout


def revision():
    head = git("rev-parse", "HEAD").strip()
    diff = subprocess.run(["git", "diff", "HEAD", "--", "Cargo.toml", "Cargo.lock", "crates", "bin"], cwd=ROOT, check=True, capture_output=True).stdout
    return f"{head}-{hashlib.sha256(diff).hexdigest()[:12]}"


def timed(args, cwd, env):
    start = time.perf_counter()
    subprocess.run(args, cwd=cwd, env=env, check=True, stdout=sys.stderr, stderr=sys.stderr)
    return time.perf_counter() - start


def measure(work):
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    tree = os.path.join(work, "tree")
    os.makedirs(tree)
    archive = subprocess.run(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT, check=True, capture_output=True).stdout.split(b"\0")
    for name in filter(None, archive):
        source = os.path.join(ROOT, name.decode())
        target = os.path.join(tree, name.decode())
        if os.path.isfile(source):
            os.makedirs(os.path.dirname(target), exist_ok=True)
            shutil.copy2(source, target)
    env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(work, "target"), NANOCODE_BUILDCACHE_URL="off", SHIPPY_REMOTE_BUILD="0", CARGO_TERM_COLOR="never")
    check = ["cargo", "check", "--locked", "--workspace", "--all-targets"]
    lines = [f"cold-check {timed(check, tree, env):.1f}s"]
    for crate, path in EDITS.items():
        file = os.path.join(tree, path)
        original = open(file).read()
        with open(file, "a") as source:
            source.write("\npub fn uphill_edit_probe() {}\n")
        lines.append(f"edit-check {crate} {timed(check, tree, env):.2f}s")
        with open(file, "w") as source:
            source.write(original)
        timed(check, tree, env)
    clippy = subprocess.run(["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--message-format", "json"], cwd=tree, env=env, capture_output=True, text=True)
    warnings = sum(1 for line in clippy.stdout.splitlines() if line.startswith("{") and json.loads(line).get("reason") == "compiler-message" and json.loads(line)["message"]["level"] == "warning")
    lines.append(f"clippy-warnings {warnings}")
    shutil.rmtree(work, ignore_errors=True)
    return lines


def unsafe_count():
    count = 0
    for top in ("crates", "bin"):
        for directory, dirs, files in os.walk(os.path.join(ROOT, top)):
            dirs[:] = [d for d in dirs if d not in ("patches", "target", "vendor", "site", "ext")]
            for name in files:
                if name.endswith(".rs"):
                    count += len(re.findall(r"\bunsafe\s*(?:\{|fn|impl|extern)", open(os.path.join(directory, name), errors="replace").read()))
    return count


def main():
    os.makedirs(CACHE, exist_ok=True)
    key = revision()
    cached = os.path.join(CACHE, key + ".txt")
    lock = open(os.path.join(CACHE, "lock"), "w")
    fcntl.flock(lock, fcntl.LOCK_EX)
    if not os.path.exists(cached):
        lines = measure(os.path.join(CACHE, "work-" + key))
        with open(cached, "w") as out:
            out.write("\n".join(lines))
    lines = open(cached).read().splitlines()
    emit(lines + [f"unsafe-blocks {unsafe_count()}"])


main()
