import os, re, subprocess, sys

ROOT = os.getcwd()
SIM = os.path.join(os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "target")), "release", "nanocode-sim")
ROW = re.compile(r"^(\w+/\w+)\s+([\d.]+)M\s+(\d+)\s+([\d.]+)M\s+([\d.]+)\s+([\d.]+)\s+(\d+)\s+([\d.]+)M\s+([\d.]+)\s+(\w+)\s+(.*)$")

subprocess.run(["cargo", "build", "--locked", "--release", "-p", "nanocode-sim"], check=True, stdout=sys.stderr)
out = subprocess.run([SIM, "--memory"], check=False, capture_output=True, text=True)
sys.stderr.write(out.stdout + out.stderr)
lines = []
for row in out.stdout.splitlines():
    match = ROW.match(row)
    if not match:
        continue
    combo, _, _, retained, ratio, _, per_message, transient, xbody, prefix, verdict = match.groups()
    lines += [
        f"retained-ratio {combo} {ratio}",
        f"transient-xbody {combo} {float(xbody):.1f}",
        f"retained {combo} {round(float(retained) * 1048576)}B",
        f"transient {combo} {round(float(transient) * 1048576)}B",
        f"bytes-per-message {combo} {per_message}",
        f"invariant {combo} {'pass' if verdict == 'PASS' and prefix == 'stable' else 'fail'}" + ("" if verdict == "PASS" else f' note "{verdict}"'),
    ]
if not lines:
    raise SystemExit(f"nanocode-sim --memory printed no rows (exit {out.returncode})")
with open(os.environ["UPHILL_METRICS"], "a") as metrics:
    metrics.write("".join(line + "\n" for line in lines))
