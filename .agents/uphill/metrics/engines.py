import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import *

BASE = "text(1 + 1)"
CASES = {
    "djs/int-loop": "let s = 0; for (let i = 0; i < 2000000; i++) s += i % 7; text(s)",
    "djs/objects": "const a = []; for (let i = 0; i < 200000; i++) a.push({ id: i, name: 'n' + i }); text(a.filter(x => x.id % 3 == 0).map(x => x.name.length).reduce((p, q) => p + q, 0))",
    "djs/json": "const v = []; for (let i = 0; i < 20000; i++) v.push({ i, s: 'x'.repeat(16), l: [i, i + 1] }); const t = JSON.stringify(v); text(JSON.parse(t).length + t.length)",
    "djs/strings": "let s = ''; for (let i = 0; i < 100000; i++) s += String.fromCharCode(97 + i % 26); text(s.split('a').length + s.replace(/b/g, 'c').length)",
    "psh/awk": "const r = await tools.Bash({ command: \"seq 1 150000 | awk '{ s += $1 * 2 } END { printf \\\"%d\\\\n\\\", s }'\" }); text(r.stdout)",
    "psh/sed": "const r = await tools.Bash({ command: \"seq 1 100000 | sed -e 's/1/one/g' -e '/9$/d' | wc -l\" }); text(r.stdout)",
    "psh/sort": "const r = await tools.Bash({ command: \"seq 1 100000 | sort -r | uniq | head -1\" }); text(r.stdout)",
    "psh/jq": "const r = await tools.Bash({ command: \"seq 1 20000 | jq -s 'map(. * 2) | add'\" }); text(r.stdout)",
}


def turn(mock, code):
    mock.tool = code
    home, env = environment(mock)
    run = Run(exec_argv("run it"), env, home, counted=("instructions",)).finish()
    mock.tool = None
    if REPLY.encode() not in run.seen:
        raise SystemExit(f"no reply for {code!r}")
    result = mock.requests[-1][1].decode()
    if "Script completed" not in result or "rror" in result.split("Script completed", 1)[1][:400]:
        raise SystemExit(f"{code!r} did not complete: {result[-400:]}")
    return run


def main():
    build(("nanocode-cli", ("nanocode", "nanocode-exec")))
    mock = Mock()
    turn(mock, BASE)
    base = median(turn(mock, BASE).counts["instructions"] for _ in range(3))
    lines = []
    for name, code in CASES.items():
        runs = [turn(mock, code) for _ in range(3)]
        lines.append(f"engine-instructions {name} {int(median(r.counts['instructions'] for r in runs) - base)}")
    mock.close()
    emit(lines)


main()
