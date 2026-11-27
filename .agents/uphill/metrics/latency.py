import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import *

DELTAS = 400
DELAY = 0.004


def keypress(mock):
    home, env = environment(mock)
    run = Run(tui_argv(), env, home, tty=True)
    tui_ready(run)
    run.drain(0.2)
    samples = []
    for key in b"uphillkeys":
        at = len(run.seen)
        sent = time.perf_counter_ns()
        run.write(bytes([key]))
        run.read_until(bytes([key]), at)
        samples.append(time.perf_counter_ns() - sent)
        run.drain(0.05)
    at = len(run.seen)
    sent = time.perf_counter_ns()
    run.resize(70, 240)
    run.read_until(b"F1 for keys", at)
    resize = time.perf_counter_ns() - sent
    run.finish(kill=True)
    return median(samples), max(samples), resize


def stream(mock):
    mock.deltas, mock.delay = DELTAS, DELAY
    home, env = environment(mock)
    run = Run(tui_argv(), env, home, tty=True)
    tui_ready(run)
    run.drain(0.2)
    at = len(run.seen)
    count = len(run.chunks)
    run.write(b"hi\r")
    run.read_until(REPLY.encode(), at)
    begin = len(run.chunks)
    run.drain(0.5)
    mock.deltas, mock.delay = 1, 0.0
    frames = [t for t, chunk in run.chunks[begin:] if FRAME_END in chunk]
    gaps = [b - a for a, b in zip(frames, frames[1:])]
    run.finish(kill=True)
    if len(gaps) < 2:
        raise SystemExit(f"streaming drew {len(frames)} frames")
    return median(gaps), max(gaps), len(frames)


def main():
    build(("nanocode-cli", ("nanocode", "nanocode-exec")))
    mock = Mock()
    keys = [keypress(mock) for _ in range(3)]
    streams = [stream(mock) for _ in range(3)]
    mock.close()
    emit([
        f"keypress-latency {median(k[0] for k in keys) / 1e3:.1f}us",
        f"keypress-worst {median(k[1] for k in keys) / 1e3:.1f}us",
        f"resize-redraw {median(k[2] for k in keys) / 1e3:.1f}us note \"80x24 to 240x70\"",
        f"stream-frame-gap {median(s[0] for s in streams) / 1e3:.1f}us note \"{DELTAS} deltas {DELAY * 1000:.0f} ms apart\"",
        f"stream-longest-stall {median(s[1] for s in streams) / 1e3:.1f}us",
        f"stream-frames {int(median(s[2] for s in streams))}",
    ])


main()
