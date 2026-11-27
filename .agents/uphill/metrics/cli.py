import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import *

RESUME_TURNS = int(os.environ.get("NANOCODE_UPHILL_RESUME_TURNS", "200"))


def version_once():
    run = Run([os.path.join(BIN, "nanocode-exec"), "--version"], dict(os.environ), ROOT)
    return run.finish().total


def exec_once(mock, family, tool=None):
    mock.tool = tool
    home, env = environment(mock, family)
    run = Run(exec_argv("say hi"), env, home)
    run.read_until(REPLY.encode())
    first = run.elapsed()
    run.finish()
    mock.tool = None
    return first, run.total, run.usage.ru_maxrss * 1024


def tui_once(mock):
    home, env = environment(mock)
    run = Run(tui_argv(), env, home, tty=True)
    frame, ready = tui_ready(run)
    idle = rss(run.pid)
    at = len(run.seen)
    sent = time.perf_counter_ns()
    run.write(b"hi\r")
    run.read_until(REPLY.encode(), at)
    token = time.perf_counter_ns() - sent
    run.finish(kill=True)
    return frame, ready, token, idle


def session(mock):
    home, env = environment(mock)
    Run(exec_argv("turn 0"), env, home).finish()
    sessions = os.path.join(home, "sessions")
    name = next(f for f in os.listdir(sessions) if f.endswith(".jsonl"))[: -len(".jsonl")]
    for turn in range(1, RESUME_TURNS):
        Run(exec_argv("--resume", name, f"turn {turn}"), env, home).finish()
    return home, env, name


def resume_tui(home, env, name):
    run = Run(tui_argv("--resume", name), env, home, tty=True)
    run.read_until(FRAME_END)
    at = run.read_until(REPLY.encode())
    run.read_until(FRAME_END, at)
    ready = run.elapsed()
    memory = rss(run.pid)
    run.finish(kill=True)
    return ready, memory


def resume_exec(home, env, name):
    run = Run(exec_argv("--resume", name, "again"), env, home)
    run.read_until(REPLY.encode())
    first = run.elapsed()
    run.finish()
    return first


def main():
    build(("nanocode-cli", ("nanocode", "nanocode-exec")))
    mock = Mock()
    exec_once(mock, "claude")
    tui_once(mock)
    version = [version_once() for _ in range(RUNS)]
    execs = {family: [exec_once(mock, family) for _ in range(RUNS)] for family in MODELS}
    tools = {family: [exec_once(mock, family, tool="text(1 + 1)") for _ in range(RUNS)] for family in MODELS}
    tuis = [tui_once(mock) for _ in range(RUNS)]
    home, env, name = session(mock)
    resumed = [resume_tui(home, env, name) for _ in range(3)]
    resumed_exec = [resume_exec(home, env, name) for _ in range(3)]
    mock.close()
    lines = [
        f"startup {median(version) / 1e3:.1f}us",
        f"tui-first-frame {median(t[0] for t in tuis) / 1e3:.1f}us",
        f"tui-ready {median(t[1] for t in tuis) / 1e3:.1f}us",
        f"tui-ttft {median(t[2] for t in tuis) / 1e3:.1f}us",
        f"tui-rss {int(median(t[3] for t in tuis))}B",
        f"resume-ready {median(r[0] for r in resumed) / 1e3:.1f}us note \"{RESUME_TURNS} turns\"",
        f"resume-rss {int(median(r[1] for r in resumed))}B",
        f"resume-ttft {median(resumed_exec) / 1e3:.1f}us",
    ]
    for family in MODELS:
        lines += [
            f"exec-ttft {family} {median(e[0] for e in execs[family]) / 1e3:.1f}us",
            f"exec-turn {family} {median(e[1] for e in execs[family]) / 1e3:.1f}us",
            f"tool-turn {family} {median(e[1] for e in tools[family]) / 1e3:.1f}us",
            f"exec-peak-rss {family} {int(median(e[2] for e in execs[family]))}B",
        ]
    lines += [f"cli-size {binary} {os.path.getsize(os.path.join(BIN, binary))}B" for binary in ("nanocode", "nanocode-exec")]
    emit(lines)


main()
