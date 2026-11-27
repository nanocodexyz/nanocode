import os, re, subprocess, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, emit

TURNS = int(os.environ.get("NANOCODE_UPHILL_LEAK_TURNS", "120"))
ROW = re.compile(r"^(\w+/\w+)\s+([\d.]+)M\s+(\d+)\s+([\d.]+)M\s+([\d.]+)\s+([\d.]+)")

subprocess.run(["cargo", "build", "--locked", "--release", "-p", "nanocode-sim"], check=True, stdout=sys.stderr)
out = subprocess.run([os.path.join(BIN, "nanocode-sim"), "--memory", "--only", "claude/memo,codex/memo", "--turns", str(TURNS)], capture_output=True, text=True)
sys.stderr.write(out.stdout + out.stderr)
lines = []
for row in out.stdout.splitlines():
    match = ROW.match(row)
    if match:
        combo, history, _, retained, ratio, final = match.groups()
        lines += [
            f"long-retained-ratio {combo} {final} note \"after {TURNS} turns\"",
            f"retained-per-turn {combo} {round(float(retained) * 1048576 / TURNS)}B",
        ]
if not lines:
    raise SystemExit(f"nanocode-sim --memory printed no rows (exit {out.returncode})")
emit(lines)
