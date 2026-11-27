import json, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import *


def prompt_order(family, request):
    if family == "claude":
        parts = [request.get("tools"), request.get("system"), request.get("messages")]
    else:
        parts = [request.get("instructions"), request.get("tools"), request.get("input")]
    return json.dumps(parts, separators=(",", ":"))


def shared_prefix(a, b):
    n = 0
    for x, y in zip(a, b):
        if x != y:
            break
        n += 1
    return n


def measure(mock, family):
    home, env = environment(mock, family)
    sessions = os.path.join(home, "sessions")
    start = len(mock.requests)
    Run(exec_argv("say hi"), env, home).finish()
    first = mock.requests[start:]
    after_one = tree_bytes(sessions)
    name = next(f for f in os.listdir(sessions) if f.endswith(".jsonl"))[: -len(".jsonl")]
    start = len(mock.requests)
    Run(exec_argv("--resume", name, "say hi again"), env, home).finish()
    second = mock.requests[start:]
    after_two = tree_bytes(sessions)
    one = json.loads(first[-1][1])
    two = json.loads(second[-1][1])
    a, b = prompt_order(family, one), prompt_order(family, two)
    sections = {"claude": ("tools", "system", "messages"), "codex": ("instructions", "tools", "input")}[family]
    return [
        f"request-bytes {family} {len(first[-1][1])}B",
    ] + [
        f"request-section-bytes {family}/{key} {len(json.dumps(one.get(key), separators=(',', ':'))) if key in one else 0}B" for key in sections
    ] + [
        f"turn2-added-bytes {family} {len(second[-1][1]) - len(first[-1][1])}B",
        f"cached-prefix {family} {100 * shared_prefix(a, b) / len(b):.1f} note \"share of turn 2's prompt that repeats turn 1 in cache order\"",
        f"requests-per-turn {family} {len(first)}",
        f"session-bytes-per-turn {family} {after_two - after_one}B",
        f"session-bytes-first-turn {family} {after_one}B",
    ]


def main():
    build(("nanocode-cli", ("nanocode", "nanocode-exec")))
    mock = Mock()
    lines = [line for family in MODELS for line in measure(mock, family)]
    mock.close()
    emit(lines)


main()
