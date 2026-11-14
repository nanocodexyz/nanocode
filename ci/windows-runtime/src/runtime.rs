use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use nanocode_executor::{
    Builder, JoinSet, Runtime, TcpListener, TcpStream, process::Command, sleep, spawn, spawn_blocking,
    timeout,
};
use nanocode_io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};

use crate::{Outcome, Report, ensure};

pub fn run(report: &mut Report) {
    let checks: &[(&str, fn() -> Outcome)] = &[
        ("executor.block_on_spawn", block_on_spawn),
        ("executor.current_thread", current_thread),
        ("executor.spawn_blocking", spawn_blocking_check),
        ("executor.join_set_abort", join_set_abort),
        ("executor.timers", timers),
        ("executor.timer_storm", timer_storm),
        ("executor.timeout", timeout_check),
        ("executor.tcp_echo", tcp_echo),
        ("executor.tcp_bulk", tcp_bulk),
        ("executor.tcp_split_peek", tcp_split_peek),
        ("executor.tcp_single_worker", tcp_single_worker),
        ("executor.tcp_refused", tcp_refused),
        ("executor.lookup_host", lookup_host),
        ("executor.process_output", process_output),
        ("executor.process_exit_codes", process_exit_codes),
        ("executor.process_stdin", process_stdin),
        ("executor.process_interactive", process_interactive),
        ("executor.process_interactive_single_worker", process_interactive_single_worker),
        ("executor.process_flood", process_flood),
        ("executor.process_kill", process_kill),
        ("executor.process_timeout_kill", process_timeout_kill),
        ("executor.files", files),
        ("executor.read_dir", read_dir),
        ("executor.stdout", stdout),
        ("executor.ctrl_c_registration", ctrl_c_registration),
        ("executor.ctrl_c_delivery", ctrl_c_delivery),
        ("platform.spawn_task", platform_spawn_task),
        ("platform.time_random_uuid", platform_misc),
        ("platform.temporary", platform_temporary),
    ];
    for (name, check) in checks {
        report.guarded(name, Duration::from_secs(60), *check);
    }
}

fn runtime() -> Result<Runtime, String> {
    Builder::new_multi_thread().worker_threads(2).enable_all().build().map_err(|error| error.to_string())
}

fn single() -> Result<Runtime, String> {
    Builder::new_current_thread().enable_all().build().map_err(|error| error.to_string())
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn exe() -> std::path::PathBuf {
    std::env::current_exe().expect("the current executable has a path")
}

fn child(arguments: &[&str]) -> Command {
    let mut command = Command::new(exe());
    command.arg("--child").args(arguments);
    command
}

fn block_on_spawn() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for index in 0..100 {
            let counter = counter.clone();
            handles.push(spawn(async move {
                nanocode_executor::yield_now().await;
                counter.fetch_add(1, Ordering::SeqCst);
                index * 2
            }));
        }
        let mut sum = 0;
        for handle in handles {
            sum += handle.await.map_err(text)?;
        }
        ensure(sum == 9900 && counter.load(Ordering::SeqCst) == 100, || format!("sum {sum}"))?;
        let panicked = spawn(async { panic!("expected panic") }).await;
        ensure(panicked.as_ref().is_err_and(|error| error.is_panic()), || "panic not reported".into())?;
        Ok("100 tasks joined, a panicking task reports a panic".into())
    })
}

fn current_thread() -> Outcome {
    let runtime = single()?;
    runtime.block_on(async {
        let handle = spawn(async {
            sleep(Duration::from_millis(5)).await;
            7
        });
        let value = handle.await.map_err(text)?;
        ensure(value == 7, || format!("value {value}"))?;
        Ok("current-thread runtime ran a sleeping task".into())
    })
}

fn spawn_blocking_check() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(spawn_blocking(|| {
                std::thread::sleep(Duration::from_millis(100));
                std::thread::current().name().map(str::to_string)
            }));
        }
        let mut names = Vec::new();
        for handle in handles {
            names.push(handle.await.map_err(text)?);
        }
        let elapsed = start.elapsed();
        ensure(elapsed < Duration::from_millis(700), || format!("8 blocking sleeps of 100 ms took {elapsed:?}"))?;
        Ok(format!("8 blocking jobs in {elapsed:?}, thread {:?}", names[0]))
    })
}

fn join_set_abort() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut set = JoinSet::new();
        for index in 0..10_u64 {
            set.spawn(async move {
                sleep(Duration::from_millis(index * 3)).await;
                index
            });
        }
        let mut seen = Vec::new();
        while let Some(result) = set.join_next().await {
            seen.push(result.map_err(text)?);
        }
        ensure(seen.len() == 10, || format!("seen {seen:?}"))?;
        let long = spawn(async {
            sleep(Duration::from_secs(30)).await;
        });
        long.abort();
        let aborted = long.await;
        ensure(aborted.as_ref().is_err_and(|error| error.is_cancelled()), || "abort not reported".into())?;
        Ok(format!("JoinSet joined in order {seen:?}, aborted task cancelled"))
    })
}

fn timers() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut lines = Vec::new();
        for millis in [1_u64, 10, 50, 200] {
            let start = Instant::now();
            sleep(Duration::from_millis(millis)).await;
            let elapsed = start.elapsed();
            ensure(elapsed >= Duration::from_millis(millis), || format!("sleep {millis} ms woke after {elapsed:?}"))?;
            ensure(elapsed < Duration::from_millis(millis + 100), || format!("sleep {millis} ms took {elapsed:?}"))?;
            lines.push(format!("{millis}ms->{:.1}ms", elapsed.as_secs_f64() * 1000.0));
        }
        let deadline = Instant::now() + Duration::from_millis(20);
        nanocode_executor::sleep_until(deadline.into()).await;
        ensure(Instant::now() >= deadline, || "sleep_until woke early".into())?;
        Ok(lines.join(" "))
    })
}

fn timer_storm() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let start = Instant::now();
        let mut set = JoinSet::new();
        for index in 0..2000_u64 {
            set.spawn(async move {
                sleep(Duration::from_millis(index % 50)).await;
            });
        }
        while let Some(result) = set.join_next().await {
            result.map_err(text)?;
        }
        let elapsed = start.elapsed();
        ensure(elapsed < Duration::from_secs(3), || format!("2000 timers took {elapsed:?}"))?;
        Ok(format!("2000 timers up to 49 ms done in {elapsed:?}"))
    })
}

fn timeout_check() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let start = Instant::now();
        let elapsed = timeout(Duration::from_millis(30), std::future::pending::<()>()).await;
        ensure(elapsed.is_err() && start.elapsed() >= Duration::from_millis(30), || "pending future did not time out".into())?;
        let done = timeout(Duration::from_secs(5), async { 5 }).await;
        ensure(done == Ok(5), || "ready future timed out".into())?;
        Ok(format!("timeout fired after {:?}", start.elapsed()))
    })
}

async fn echo_server(listener: TcpListener) -> std::io::Result<()> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        spawn(async move {
            let mut buffer = vec![0_u8; 65536];
            loop {
                let read = match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                if stream.write_all(&buffer[..read]).await.is_err() {
                    return;
                }
            }
        });
    }
}

fn tcp_echo() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(text)?;
        let address = listener.local_addr().map_err(text)?;
        let server = spawn(echo_server(listener));
        let start = Instant::now();
        let mut stream = TcpStream::connect(address).await.map_err(text)?;
        stream.set_nodelay(true).map_err(text)?;
        let connected = start.elapsed();
        let mut rounds = Vec::new();
        for round in 0..20_u32 {
            let message = format!("round {round}");
            let begin = Instant::now();
            stream.write_all(message.as_bytes()).await.map_err(text)?;
            let mut back = vec![0_u8; message.len()];
            stream.read_exact(&mut back).await.map_err(text)?;
            ensure(back == message.as_bytes(), || format!("echo {back:?}"))?;
            rounds.push(begin.elapsed());
        }
        rounds.sort();
        stream.shutdown().await.map_err(text)?;
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.map_err(text)?;
        ensure(rest.is_empty(), || format!("{} bytes after shutdown", rest.len()))?;
        server.abort();
        Ok(format!(
            "connect {:?}, 20 round trips, median {:?} max {:?}, clean shutdown",
            connected, rounds[10], rounds[19]
        ))
    })
}

fn tcp_bulk() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(text)?;
        let address = listener.local_addr().map_err(text)?;
        let server = spawn(echo_server(listener));
        let stream = TcpStream::connect(address).await.map_err(text)?;
        let (mut reader, mut writer) = stream.into_split();
        let total = 32 << 20;
        let start = Instant::now();
        let writing = spawn(async move {
            let chunk: Vec<u8> = (0..65536_u32).map(|index| (index % 253) as u8).collect();
            let mut sent = 0;
            while sent < total {
                writer.write_all(&chunk).await?;
                sent += chunk.len();
            }
            writer.shutdown().await?;
            Ok::<_, std::io::Error>(sent)
        });
        let mut received = 0_usize;
        let mut buffer = vec![0_u8; 65536];
        let mut mismatch = None;
        loop {
            let read = reader.read(&mut buffer).await.map_err(text)?;
            if read == 0 {
                break;
            }
            for (offset, byte) in buffer[..read].iter().enumerate() {
                let expected = (((received + offset) % 65536) % 253) as u8;
                if *byte != expected && mismatch.is_none() {
                    mismatch = Some(received + offset);
                }
            }
            received += read;
        }
        let sent = writing.await.map_err(text)?.map_err(text)?;
        let elapsed = start.elapsed();
        server.abort();
        ensure(mismatch.is_none(), || format!("byte mismatch at {mismatch:?}"))?;
        ensure(received == sent, || format!("sent {sent} received {received}"))?;
        Ok(format!("32 MiB echoed in {elapsed:?} ({:.0} MiB/s)", 32.0 / elapsed.as_secs_f64()))
    })
}

fn tcp_split_peek() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(text)?;
        let address = listener.local_addr().map_err(text)?;
        let accepting = spawn(async move { listener.accept().await });
        let mut client = TcpStream::connect(address).await.map_err(text)?;
        let (mut server, peer) = accepting.await.map_err(text)?.map_err(text)?;
        ensure(peer == client.local_addr().map_err(text)?, || "peer address differs".into())?;
        client.write_all(b"peek me").await.map_err(text)?;
        let mut peeked = [0_u8; 4];
        let count = server.peek(&mut peeked).await.map_err(text)?;
        ensure(count > 0 && &peeked[..count] == &b"peek"[..count], || format!("peeked {peeked:?}"))?;
        let mut all = [0_u8; 7];
        server.read_exact(&mut all).await.map_err(text)?;
        ensure(&all == b"peek me", || "read after peek differs".into())?;
        server.readable().await.map_err(text)?;
        let try_read = server.try_read(&mut all);
        let would_block = matches!(&try_read, Err(error) if error.kind() == std::io::ErrorKind::WouldBlock);
        ensure(would_block, || format!("try_read on an empty socket {try_read:?}"))?;
        client.writable().await.map_err(text)?;
        let wrote = client.try_write(b"x").map_err(text)?;
        ensure(wrote == 1, || "try_write".into())?;
        let reading = timeout(Duration::from_secs(5), async {
            let mut one = [0_u8; 1];
            server.read_exact(&mut one).await.map(|_| one[0])
        })
        .await;
        ensure(matches!(reading, Ok(Ok(b'x'))), || format!("read after try_write {reading:?}"))?;
        let std_stream = client.into_std().map_err(text)?;
        let back = TcpStream::from_std(std_stream).map_err(text)?;
        let _ = back.peer_addr().map_err(text)?;
        Ok("accept peer address, peek, try_read WouldBlock, try_write, into_std/from_std".into())
    })
}

fn tcp_single_worker() -> Outcome {
    let runtime = single()?;
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(text)?;
        let address = listener.local_addr().map_err(text)?;
        let server = spawn(echo_server(listener));
        let stream = TcpStream::connect(address).await.map_err(text)?;
        let payload = vec![7_u8; 4 << 20];
        let (mut reader, mut writer) = stream.into_split();
        let write = async {
            writer.write_all(&payload).await?;
            writer.shutdown().await
        };
        let read = async {
            let mut back = Vec::new();
            reader.read_to_end(&mut back).await.map(|_| back)
        };
        let (written, back) = join(write, read).await;
        written.map_err(text)?;
        let back = back.map_err(text)?;
        server.abort();
        ensure(back.len() == payload.len(), || format!("echoed {} of {}", back.len(), payload.len()))?;
        Ok("4 MiB echoed through one worker with reads and writes interleaved".into())
    })
}

async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    let mut first = None;
    let mut second = None;
    std::future::poll_fn(|context| {
        if first.is_none() {
            if let std::task::Poll::Ready(value) = a.as_mut().poll(context) {
                first = Some(value);
            }
        }
        if second.is_none() {
            if let std::task::Poll::Ready(value) = b.as_mut().poll(context) {
                second = Some(value);
            }
        }
        if first.is_some() && second.is_some() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    (first.expect("first finished"), second.expect("second finished"))
}

fn tcp_refused() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let address = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(text)?;
            listener.local_addr().map_err(text)?
        };
        let start = Instant::now();
        let result = timeout(Duration::from_secs(10), TcpStream::connect(address)).await;
        let elapsed = start.elapsed();
        match result {
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                Ok(format!("refused after {elapsed:?}"))
            }
            other => Err(format!("connect to a closed port gave {other:?} after {elapsed:?}")),
        }
    })
}

fn lookup_host() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let addresses: Vec<_> = nanocode_executor::lookup_host("localhost:8080").await.map_err(text)?.collect();
        ensure(!addresses.is_empty() && addresses.iter().all(|address| address.port() == 8080), || format!("{addresses:?}"))?;
        Ok(format!("localhost resolves to {addresses:?}"))
    })
}

fn process_output() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let output = child(&["print", "to stdout", "to stderr"]).output().await.map_err(text)?;
        ensure(output.status.success(), || format!("status {:?}", output.status))?;
        ensure(output.stdout == b"to stdout" && output.stderr == b"to stderr", || {
            format!("stdout {:?} stderr {:?}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
        })?;
        Ok("output() captured stdout and stderr".into())
    })
}

fn process_exit_codes() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut codes = Vec::new();
        for code in [0, 1, 42, 255, 3_000_000] {
            let status = child(&["exit", &code.to_string()]).status().await.map_err(text)?;
            ensure(status.code() == Some(code), || format!("exit {code} gave {status:?}"))?;
            codes.push(code);
        }
        let mut spawned = child(&["exit", "9"]).spawn().map_err(text)?;
        let id = spawned.id();
        let status = spawned.wait().await.map_err(text)?;
        ensure(status.code() == Some(9) && spawned.try_wait().map_err(text)? == Some(status), || "wait/try_wait".into())?;
        let missing = Command::new("nanocode-no-such-program-here").status().await;
        ensure(matches!(&missing, Err(error) if error.kind() == std::io::ErrorKind::NotFound), || format!("{missing:?}"))?;
        Ok(format!("exit codes {codes:?}, pid {id:?}, missing program is NotFound"))
    })
}

fn process_stdin() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut spawned = child(&["echo"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(text)?;
        let mut stdin = spawned.stdin.take().ok_or("no stdin")?;
        let payload = b"lowercase input\n".repeat(1000);
        let expected = payload.to_ascii_uppercase();
        let writing = spawn(async move {
            stdin.write_all(&payload).await?;
            drop(stdin);
            Ok::<_, std::io::Error>(())
        });
        let output = spawned.wait_with_output().await.map_err(text)?;
        writing.await.map_err(text)?.map_err(text)?;
        ensure(output.status.success(), || format!("status {:?}", output.status))?;
        ensure(output.stdout == expected, || format!("stdout {} bytes", output.stdout.len()))?;
        ensure(output.stderr == b"read 16000", || format!("stderr {:?}", String::from_utf8_lossy(&output.stderr)))?;
        Ok("16000 B through stdin, uppercased back on stdout, count on stderr".into())
    })
}

async fn interactive() -> Outcome {
    let mut spawned = child(&["lines"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().map_err(text)?;
    let mut stdin = spawned.stdin.take().ok_or("no stdin")?;
    let stdout = spawned.stdout.take().ok_or("no stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let ticker = spawn(async {
        let mut ticks = 0_u32;
        loop {
            sleep(Duration::from_millis(5)).await;
            ticks += 1;
            if ticks == 1000 {
                return ticks;
            }
        }
    });
    for index in 0..5 {
        stdin.write_all(format!("line {index}\n").as_bytes()).await.map_err(text)?;
        let line = timeout(Duration::from_secs(5), lines.next_line()).await.map_err(|_| format!("no reply to line {index}"))?.map_err(text)?;
        ensure(line.as_deref() == Some(format!("{}:line {index}", index + 1).as_str()), || format!("reply {line:?}"))?;
    }
    drop(stdin);
    let status = timeout(Duration::from_secs(5), spawned.wait()).await.map_err(|_| "child did not exit after stdin closed".to_string())?.map_err(text)?;
    ensure(status.success(), || format!("status {status:?}"))?;
    let ticking = !ticker.is_finished();
    ticker.abort();
    Ok(format!("5 request/reply lines, child exited on EOF, other task kept running: {ticking}"))
}

fn process_interactive() -> Outcome {
    runtime()?.block_on(interactive())
}

fn process_interactive_single_worker() -> Outcome {
    let runtime = single()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let watcher = std::thread::spawn(move || receiver.recv_timeout(Duration::from_secs(20)).is_ok());
    let outcome = runtime.block_on(async {
        let start = Instant::now();
        let mut spawned = child(&["sleep", "300"]).stdout(Stdio::piped()).spawn().map_err(text)?;
        let mut stdout = spawned.stdout.take().ok_or("no stdout")?;
        let begin = Instant::now();
        let timer = spawn(async move {
            sleep(Duration::from_millis(20)).await;
            begin.elapsed()
        });
        let mut rest = Vec::new();
        stdout.read_to_end(&mut rest).await.map_err(text)?;
        let fired = timer.await.map_err(text)?;
        spawned.wait().await.map_err(text)?;
        let total = start.elapsed();
        ensure(fired < Duration::from_millis(200), || {
            format!("a 20 ms timer on the same worker fired after {fired:?} while a child pipe was read (total {total:?}): pipe reads block the worker")
        })?;
        Ok(format!("20 ms timer fired after {fired:?} while a silent child's stdout was awaited"))
    });
    let _ = sender.send(());
    let _ = watcher.join();
    outcome
}

fn process_flood() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let start = Instant::now();
        let output = timeout(Duration::from_secs(30), child(&["flood", "8388608"]).output()).await.map_err(|_| "deadlocked".to_string())?.map_err(text)?;
        ensure(output.status.success() && output.stdout.len() == 8 << 20 && output.stderr.len() == 8 << 20, || {
            format!("status {:?} stdout {} stderr {}", output.status, output.stdout.len(), output.stderr.len())
        })?;
        Ok(format!("8 MiB on each of stdout and stderr in {:?}", start.elapsed()))
    })
}

fn process_kill() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut spawned = child(&["sleep", "60000"]).spawn().map_err(text)?;
        ensure(spawned.try_wait().map_err(text)?.is_none(), || "sleeping child already exited".into())?;
        let start = Instant::now();
        spawned.kill().await.map_err(text)?;
        let status = spawned.wait().await.map_err(text)?;
        ensure(!status.success(), || format!("killed child status {status:?}"))?;
        let mut second = child(&["sleep", "60000"]).kill_on_drop(true).spawn().map_err(text)?;
        let id = second.id().ok_or("no pid")?;
        second.start_kill().map_err(text)?;
        let second_status = second.wait().await.map_err(text)?;
        Ok(format!("kill took {:?}, status {status:?}; start_kill on {id} gave {second_status:?}", start.elapsed()))
    })
}

fn process_timeout_kill() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut spawned = child(&["sleep", "60000"]).kill_on_drop(true).spawn().map_err(text)?;
        let waited = timeout(Duration::from_millis(100), spawned.wait()).await;
        ensure(waited.is_err(), || "wait finished early".into())?;
        let id = spawned.id().ok_or("no pid")?;
        drop(spawned);
        sleep(Duration::from_millis(300)).await;
        let alive = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {id}"), "/NH"])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains(&id.to_string()));
        ensure(alive.as_ref().map_or(true, |alive| !alive), || format!("child {id} still alive after kill_on_drop"))?;
        Ok(format!("wait timed out, kill_on_drop ended {id} (tasklist {})", if alive.is_ok() { "checked" } else { "unavailable" }))
    })
}

fn files() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let directory = std::env::temp_dir().join(format!("nanocode-executor-files-{}", std::process::id()));
        nanocode_executor::fs::create_dir_all(&directory).await.map_err(text)?;
        let path = directory.join("file.txt");
        nanocode_executor::fs::write(&path, b"first").await.map_err(text)?;
        let read = nanocode_executor::fs::read_to_string(&path).await.map_err(text)?;
        ensure(read == "first", || format!("read {read}"))?;
        let mut file = nanocode_executor::fs::OpenOptions::new().append(true).open(&path).await.map_err(text)?;
        file.write_all(b" second").await.map_err(text)?;
        file.flush().await.map_err(text)?;
        file.sync_all().await.map_err(text)?;
        drop(file);
        let mut file = nanocode_executor::fs::File::open(&path).await.map_err(text)?;
        file.seek(std::io::SeekFrom::Start(6)).await.map_err(text)?;
        let mut tail = String::new();
        file.read_to_string(&mut tail).await.map_err(text)?;
        ensure(tail == "second", || format!("tail {tail}"))?;
        let metadata = file.metadata().await.map_err(text)?;
        ensure(metadata.len() == 12, || format!("len {}", metadata.len()))?;
        drop(file);
        let renamed = directory.join("renamed.txt");
        nanocode_executor::fs::rename(&path, &renamed).await.map_err(text)?;
        ensure(!nanocode_executor::fs::try_exists(&path).await.map_err(text)?, || "old name exists".into())?;
        let copied = nanocode_executor::fs::copy(&renamed, directory.join("copy.txt")).await.map_err(text)?;
        let copy_length = nanocode_executor::fs::metadata(directory.join("copy.txt")).await.map_err(text)?.len();
        ensure(copy_length == 12, || format!("copy has {copy_length} bytes"))?;
        crate::wine_gap(copied == 12, || format!("std::fs::copy reported {copied} bytes for a 12 byte copy: CopyFileExW never calls its progress routine"))?;
        let canonical = nanocode_executor::fs::canonicalize(&renamed).await.map_err(text)?;
        let mut created = nanocode_executor::fs::File::create(directory.join("sized.bin")).await.map_err(text)?;
        created.set_len(1 << 20).await.map_err(text)?;
        drop(created);
        let sized = nanocode_executor::fs::metadata(directory.join("sized.bin")).await.map_err(text)?;
        ensure(sized.len() == 1 << 20, || format!("set_len gave {}", sized.len()))?;
        nanocode_executor::fs::remove_dir_all(&directory).await.map_err(text)?;
        Ok(format!("write/read/append/seek/metadata/rename/copy/set_len/remove, canonical {}", canonical.display()))
    })
}

fn read_dir() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let directory = std::env::temp_dir().join(format!("nanocode-executor-dir-{}", std::process::id()));
        nanocode_executor::fs::create_dir_all(&directory).await.map_err(text)?;
        for index in 0..20 {
            nanocode_executor::fs::write(directory.join(format!("{index}.txt")), b"x").await.map_err(text)?;
        }
        nanocode_executor::fs::create_dir(directory.join("nested")).await.map_err(text)?;
        let mut entries = nanocode_executor::fs::read_dir(&directory).await.map_err(text)?;
        let mut files = 0;
        let mut directories = 0;
        while let Some(entry) = entries.next_entry().await.map_err(text)? {
            if entry.file_type().await.map_err(text)?.is_dir() {
                directories += 1;
            } else {
                files += 1;
            }
        }
        nanocode_executor::fs::remove_dir_all(&directory).await.map_err(text)?;
        ensure(files == 20 && directories == 1, || format!("{files} files {directories} directories"))?;
        Ok("20 files and 1 directory listed".into())
    })
}

fn stdout() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let mut out = nanocode_executor::fs::stdout();
        out.write_all(b"executor stdout line\n").await.map_err(text)?;
        out.flush().await.map_err(text)?;
        let mut err = nanocode_executor::fs::stderr();
        err.write_all(b"").await.map_err(text)?;
        Ok("wrote a line through nanocode_executor::fs::stdout".into())
    })
}

fn ctrl_c_registration() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let waited = timeout(Duration::from_millis(50), nanocode_executor::signal::ctrl_c()).await;
        ensure(waited.is_err(), || format!("ctrl_c resolved without a signal: {waited:?}"))?;
        Ok("ctrl_c() registers and stays pending without a signal".into())
    })
}

const CTRL_C_CAUGHT: i32 = 30;

pub fn ctrl_c_child() -> i32 {
    use nanocode_sys::windows_sys::Win32::System::Console::{CTRL_C_EVENT, GenerateConsoleCtrlEvent};
    let Ok(runtime) = runtime() else {
        return 120;
    };
    runtime.block_on(async {
        let interrupt = spawn(nanocode_executor::signal::ctrl_c());
        sleep(Duration::from_millis(50)).await;
        let sender = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) != 0 }
        });
        let received = timeout(Duration::from_secs(5), interrupt).await;
        let sent = sender.join().unwrap_or(false);
        match (sent, received) {
            (false, _) => 122,
            (true, Ok(Ok(Ok(())))) => CTRL_C_CAUGHT,
            (true, Ok(Ok(Err(_)))) => 121,
            (true, _) => 123,
        }
    })
}

fn ctrl_c_delivery() -> Outcome {
    use std::os::windows::process::CommandExt;
    use nanocode_sys::windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;
    let status = std::process::Command::new(exe())
        .args(["--child", "ctrl-c-executor"])
        .creation_flags(CREATE_NEW_CONSOLE)
        .status()
        .map_err(text)?;
    match status.code() {
        Some(CTRL_C_CAUGHT) => Ok("a child on its own console caught its CTRL_C_EVENT through signal::ctrl_c and kept running".into()),
        Some(122) => {
            crate::wine_gap(false, || "GenerateConsoleCtrlEvent failed in a child with its own console".into())?;
            Err("GenerateConsoleCtrlEvent failed in a child with its own console".into())
        }
        Some(121) => Err("ctrl_c returned an error".into()),
        Some(123) => Err("ctrl_c did not resolve within 5 s of CTRL_C_EVENT".into()),
        other => Err(format!("child exited {other:?} (the default handler ended it: 0xC000013A on Windows, 0 under Wine)")),
    }
}

fn platform_spawn_task() -> Outcome {
    let runtime = runtime()?;
    runtime.block_on(async {
        let handle = nanocode_platform::spawn_task(async {
            nanocode_platform::sleep(Duration::from_millis(10)).await;
            11
        });
        let value = handle.await.map_err(text)?;
        ensure(value == 11, || format!("value {value}"))?;
        let mut set = JoinSet::new();
        nanocode_platform::spawn_joined(&mut set, async { 12 });
        let joined = set.join_next().await.ok_or("empty set")?.map_err(text)?;
        ensure(joined == 12, || format!("joined {joined}"))?;
        let timed = nanocode_platform::timeout(Duration::from_millis(20), std::future::pending::<()>()).await;
        ensure(timed.is_err(), || "platform timeout did not fire".into())?;
        Ok("spawn_task, spawn_joined, platform sleep and timeout".into())
    })
}

fn platform_misc() -> Outcome {
    let mut bytes = [0_u8; 32];
    nanocode_platform::fill_random(&mut bytes).map_err(text)?;
    let first = nanocode_platform::uuid_v4();
    let second = nanocode_platform::uuid_v7();
    let millis = nanocode_platform::unix_millis();
    let system = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(text)?.as_millis() as u64;
    ensure(millis.abs_diff(system) < 2000, || format!("unix_millis {millis} vs {system}"))?;
    let date = nanocode_platform::format_http_date(nanocode_platform::system_time());
    Ok(format!("uuid v4 {first}, v7 {second}, {date:?}"))
}

fn platform_temporary() -> Outcome {
    let file = nanocode_platform::TemporaryFile::new().map_err(text)?;
    let path = file.path().to_path_buf();
    ensure(path.exists(), || "temporary file missing".into())?;
    drop(file);
    ensure(!path.exists(), || format!("{} survived drop", path.display()))?;
    Ok(format!("temporary file {} removed on drop", path.display()))
}
