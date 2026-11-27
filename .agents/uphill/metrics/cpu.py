import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import *

COUNTED = ("instructions", "cycles")
IDLE = float(os.environ.get("NANOCODE_UPHILL_IDLE_SECONDS", "5"))


def version_once():
    return Run([os.path.join(BIN, "nanocode-exec"), "--version"], dict(os.environ), ROOT, counted=COUNTED).finish()


def exec_once(mock, family):
    home, env = environment(mock, family)
    return Run(exec_argv("say hi"), env, home, counted=COUNTED).finish()


def frame_once(mock):
    home, env = environment(mock)
    run = Run(tui_argv(), env, home, counted=COUNTED, tty=True)
    run.read_until(FRAME_END)
    threads = len(tasks(run.pid))
    run.finish(kill=True)
    return run, threads


def idle_once(mock):
    home, env = environment(mock)
    run = Run(tui_argv(), env, home, tty=True)
    tui_ready(run)
    run.drain(1.0)
    cpu0, switches0 = proc_cpu(run.pid)
    threads = len(tasks(run.pid))
    run.drain(IDLE)
    cpu1, switches1 = proc_cpu(run.pid)
    run.finish(kill=True)
    return (cpu1 - cpu0) / IDLE, (switches1 - switches0) / IDLE, threads


def main():
    build(("nanocode-cli", ("nanocode", "nanocode-exec")))
    mock = Mock()
    exec_once(mock, "claude")
    versions = [version_once() for _ in range(RUNS)]
    execs = {family: [exec_once(mock, family) for _ in range(RUNS)] for family in MODELS}
    frames = [frame_once(mock) for _ in range(RUNS)]
    idles = [idle_once(mock) for _ in range(2)]
    mock.close()
    lines = []
    for name, runs in [("startup", versions), ("first-frame", [f[0] for f in frames])] + [(f"exec-turn.{family}", runs) for family, runs in execs.items()]:
        metric, _, item = name.partition(".")
        target = f"{metric}-{{}} {item} " if item else f"{metric}-{{}} "
        lines.append(target.format("instructions") + str(int(median(r.counts["instructions"] for r in runs))))
        lines.append(target.format("cycles") + str(int(median(r.counts["cycles"] for r in runs))))
    for family, runs in execs.items():
        lines += [
            f"exec-cpu-time {family} {median((r.usage.ru_utime + r.usage.ru_stime) * 1e6 for r in runs):.0f}us",
            f"exec-page-faults {family} {int(median(r.usage.ru_minflt + r.usage.ru_majflt for r in runs))}",
            f"exec-context-switches {family} {int(median(r.usage.ru_nvcsw + r.usage.ru_nivcsw for r in runs))}",
        ]
    lines += [
        f"first-frame-threads {int(median(f[1] for f in frames))}",
        f"idle-cpu {median(i[0] for i in idles) / 1e3:.1f}us note \"CPU time per second idle in the TUI\"",
        f"idle-wakeups {median(i[1] for i in idles):.1f}",
        f"idle-threads {int(median(i[2] for i in idles))}",
    ]
    emit(lines)


main()
