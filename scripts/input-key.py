#!/usr/bin/env python3
import hashlib
import json
import os
import subprocess
import sys

package, target, *extra = sys.argv[1:]
root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(*args):
    return subprocess.run(args, cwd=root, capture_output=True, text=True, check=True).stdout


meta = json.loads(run("cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target))
packages = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
start = next(p["id"] for p in meta["packages"] if p["name"] == package and p["source"] is None)
seen = set()
stack = [start]
while stack:
    current = stack.pop()
    if current in seen:
        continue
    seen.add(current)
    for dep in nodes[current]["deps"]:
        if any(kind["kind"] != "dev" for kind in dep["dep_kinds"]):
            stack.append(dep["pkg"])
dirs = {os.path.relpath(os.path.dirname(packages[i]["manifest_path"]), root) for i in seen if packages[i]["source"] is None}
paths = sorted(d for d in dirs if not d.startswith("..")) + [p for p in ("Cargo.lock", "Cargo.toml", ".cargo", "rust-toolchain", "rust-toolchain.toml", "scripts") if os.path.exists(os.path.join(root, p))]
if run("git", "status", "--porcelain", "--untracked-files=no", "--", *paths, ":(exclude)scripts/release-cli.sh").strip():
    sys.exit(0)
tree = "".join(line + "\n" for line in run("git", "ls-tree", "-r", "HEAD", "--", *paths).splitlines() if not line.endswith("\tscripts/release-cli.sh"))
digest = hashlib.sha256()
for part in (tree, run("rustc", "-vV"), target, os.environ.get("PROFILE", "dist"), *extra):
    digest.update(part.encode())
    digest.update(b"\0")
print(digest.hexdigest())
