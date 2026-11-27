import ctypes, fcntl, http.server, json, os, pty, select, signal, statistics, struct, subprocess, sys, tempfile, termios, threading, time

ROOT = os.getcwd()
BIN = os.path.join(os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "target")), "release")
RUNS = int(os.environ.get("NANOCODE_UPHILL_RUNS", "9"))
REPLY = "zqxj-reply"
FRAME_END = b"\x1b[?2026l"
MODELS = {"claude": "claude-sonnet-5-5", "codex": "gpt-6-luna"}


class Mock:
    def __init__(self):
        self.requests = []
        self.deltas = 1
        self.delay = 0.0
        self.tool = None
        mock = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_GET(self):
                self.send(200, "application/json", json.dumps({"data": [{"id": MODELS["codex"]}]}).encode())

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("content-length") or 0))
                mock.requests.append((self.path, body))
                request = json.loads(body)
                if self.path.endswith("/messages"):
                    events = mock.anthropic(request)
                else:
                    events = mock.responses(request)
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("transfer-encoding", "chunked")
                self.end_headers()
                for index, (name, data) in enumerate(events):
                    if mock.delay and index:
                        time.sleep(mock.delay)
                    chunk = f"event: {name}\ndata: {json.dumps(data)}\n\n".encode()
                    self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                    self.wfile.flush()
                self.wfile.write(b"0\r\n\r\n")

            def send(self, status, kind, body):
                self.send_response(status)
                self.send_header("content-type", kind)
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.handle_error = lambda request, address: None
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.port = self.server.server_address[1]

    def texts(self):
        if self.deltas == 1:
            return [REPLY]
        return [REPLY] + [" tok"] * (self.deltas - 1)

    def wants_tool(self, request, family):
        if not self.tool:
            return False
        if family == "claude":
            last = request["messages"][-1]["content"]
            return not (isinstance(last, list) and any(part.get("type") == "tool_result" for part in last))
        return not any(item.get("type") == "function_call_output" for item in request.get("input", []))

    def anthropic(self, request):
        model = request.get("model", MODELS["claude"])
        events = [("message_start", {"type": "message_start", "message": {"id": "msg_uphill", "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 1}}})]
        if self.wants_tool(request, "claude"):
            events += [
                ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_uphill", "name": "exec", "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json.dumps({"code": self.tool})}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": None}, "usage": {"output_tokens": 1}}),
            ]
        else:
            events.append(("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))
            events += [("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}) for text in self.texts()]
            events += [
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}}),
            ]
        return events + [("message_stop", {"type": "message_stop"})]

    def responses(self, request):
        response = {"id": "resp_uphill", "object": "response", "status": "in_progress", "model": request.get("model"), "output": []}
        events = [("response.created", {"type": "response.created", "sequence_number": 0, "response": response})]
        if self.wants_tool(request, "codex"):
            item = {"type": "function_call", "id": "fc_uphill", "call_id": "call_uphill", "name": "exec", "arguments": json.dumps({"code": self.tool}), "status": "completed"}
            events += [
                ("response.output_item.added", {"type": "response.output_item.added", "output_index": 0, "item": dict(item, arguments="", status="in_progress")}),
                ("response.output_item.done", {"type": "response.output_item.done", "output_index": 0, "item": item}),
            ]
            output = [item]
        else:
            text = "".join(self.texts())
            item = {"type": "message", "id": "msg_uphill", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": text, "annotations": []}]}
            events.append(("response.output_item.added", {"type": "response.output_item.added", "output_index": 0, "item": dict(item, content=[], status="in_progress")}))
            events.append(("response.content_part.added", {"type": "response.content_part.added", "item_id": "msg_uphill", "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}))
            events += [("response.output_text.delta", {"type": "response.output_text.delta", "item_id": "msg_uphill", "output_index": 0, "content_index": 0, "delta": delta}) for delta in self.texts()]
            events.append(("response.output_text.done", {"type": "response.output_text.done", "item_id": "msg_uphill", "output_index": 0, "content_index": 0, "text": text}))
            events.append(("response.output_item.done", {"type": "response.output_item.done", "output_index": 0, "item": item}))
            output = [item]
        done = dict(response, status="completed", output=output, usage={"input_tokens": 1, "output_tokens": 1, "total_tokens": 2, "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}})
        return events + [("response.completed", {"type": "response.completed", "sequence_number": len(events), "response": done})]

    def close(self):
        self.server.shutdown()


def build(*packages):
    args = ["cargo", "build", "--locked", "--release"]
    for package, binaries in packages:
        args += ["-p", package] + [flag for binary in binaries for flag in ("--bin", binary)]
    subprocess.run(args, check=True, stdout=sys.stderr)


def environment(mock, family="claude", home=None):
    home = home or tempfile.mkdtemp(prefix="nanocode-uphill-")
    env = {k: v for k, v in os.environ.items() if not k.startswith(("NANOCODE_", "ANTHROPIC_", "OPENAI_", "CODEX_"))}
    env.update(HOME=home, NANOCODE_HOME=home, TERM="xterm-256color", NANOCODE_MODEL=MODELS[family])
    if family == "claude":
        env.update(NANOCODE_ANTHROPIC_BASE_URL=f"http://127.0.0.1:{mock.port}/v1", NANOCODE_ANTHROPIC_API_KEY="uphill")
    else:
        env.update(NANOCODE_OPENAI_BASE_URL=f"http://127.0.0.1:{mock.port}/v1", NANOCODE_OPENAI_API_KEY="uphill")
    return home, env


SYS_PERF_EVENT_OPEN = 298
LIBC = ctypes.CDLL(None, use_errno=True)
EVENTS = {"instructions": (0, 1), "cycles": (0, 0)}


def open_counter(pid, name):
    kind, config = EVENTS[name]
    attr = struct.pack("<IIQQQQQ", kind, 128, config, 0, 0, 0, 1 | 2 | 32 | 64 | 4096).ljust(128, b"\0")
    fd = LIBC.syscall(SYS_PERF_EVENT_OPEN, ctypes.create_string_buffer(attr, 128), pid, -1, -1, 0)
    if fd < 0:
        raise OSError(ctypes.get_errno(), f"perf_event_open {name}")
    return fd


def read_counters(counters):
    values = {name: struct.unpack("<Q", os.read(fd, 8))[0] for name, fd in counters.items()}
    for fd in counters.values():
        os.close(fd)
    return values


class Run:
    def __init__(self, argv, env, cwd, counted=(), tty=False):
        self.start = time.perf_counter_ns()
        gate_r, gate_w = os.pipe()
        if tty:
            self.pid, self.fd = pty.fork()
        else:
            out_r, out_w = os.pipe()
            self.pid = os.fork()
        if self.pid == 0:
            os.close(gate_w)
            os.read(gate_r, 1)
            os.chdir(cwd)
            if not tty:
                os.dup2(out_w, 1)
                null = os.open(os.devnull, os.O_RDWR)
                os.dup2(null, 0)
                os.dup2(null, 2)
            os.execve(argv[0], argv, env)
        os.close(gate_r)
        if tty:
            fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        else:
            os.close(out_w)
            self.fd = out_r
        self.counters = {name: open_counter(self.pid, name) for name in counted}
        self.start = time.perf_counter_ns()
        os.write(gate_w, b"x")
        os.close(gate_w)
        self.seen = b""
        self.chunks = []

    def elapsed(self):
        return time.perf_counter_ns() - self.start

    def read_until(self, marker, after=0, timeout=30):
        deadline = time.monotonic() + timeout
        while marker not in self.seen[after:]:
            left = deadline - time.monotonic()
            if left <= 0:
                raise SystemExit(f"{marker!r} never came: {self.seen[-400:]!r}")
            ready, _, _ = select.select([self.fd], [], [], left)
            if ready:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    chunk = b""
                if not chunk:
                    raise SystemExit(f"exited before {marker!r}: {self.seen[-400:]!r}")
                self.chunks.append((time.perf_counter_ns(), chunk))
                self.seen += chunk
        return self.seen.index(marker, after) + len(marker)

    def drain(self, quiet):
        while True:
            ready, _, _ = select.select([self.fd], [], [], quiet)
            if not ready:
                return
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.chunks.append((time.perf_counter_ns(), chunk))
            self.seen += chunk

    def write(self, data):
        os.write(self.fd, data)

    def resize(self, rows, cols):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def finish(self, kill=False):
        if kill:
            os.kill(self.pid, signal.SIGKILL)
        else:
            while True:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                self.chunks.append((time.perf_counter_ns(), chunk))
                self.seen += chunk
        _, status, usage = os.wait4(self.pid, 0)
        self.total = self.elapsed()
        os.close(self.fd)
        self.status = os.waitstatus_to_exitcode(status)
        self.usage = usage
        self.counts = read_counters(self.counters) if self.counters else {}
        if not kill and self.status != 0:
            raise SystemExit(f"exited {self.status}: {self.seen[-400:]!r}")
        return self


def tasks(pid):
    return os.listdir(f"/proc/{pid}/task")


def proc_cpu(pid):
    total = 0
    switches = 0
    for task in tasks(pid):
        try:
            total += int(open(f"/proc/{pid}/task/{task}/schedstat").read().split()[0])
            for line in open(f"/proc/{pid}/task/{task}/status"):
                if line.startswith(("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")):
                    switches += int(line.split()[1])
        except FileNotFoundError:
            pass
    return total, switches


def rss(pid):
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) * 1024
    raise SystemExit("no VmRSS")


def exec_argv(*args):
    return [os.path.join(BIN, "nanocode-exec"), "exec", *args]


def tui_argv(*args):
    return [os.path.join(BIN, "nanocode"), *args]


def tui_ready(run):
    run.read_until(FRAME_END)
    frame = run.elapsed()
    at = run.read_until(b"new session")
    run.read_until(FRAME_END, at)
    return frame, run.elapsed()


def median(values):
    return statistics.median(list(values))


def tree_bytes(path):
    return sum(os.path.getsize(os.path.join(d, f)) for d, _, files in os.walk(path) for f in files if not os.path.islink(os.path.join(d, f)))


def emit(lines):
    with open(os.environ["UPHILL_METRICS"], "a") as metrics:
        metrics.write("".join(line + "\n" for line in lines))
    print("\n".join(lines), file=sys.stderr)
