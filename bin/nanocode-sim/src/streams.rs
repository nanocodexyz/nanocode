use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    future::Future,
    io::{self, IoSlice},
    ops::Range,
    pin::Pin,
    process::ExitCode,
    sync::Arc,
    task::{Context, Poll, Waker},
    time::Duration,
};

use nanocode_executor::{AbortHandle, Id, JoinError, JoinSet};
use nanocode_io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use nanocode_sync::Slot;

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::{FNV_OFFSET, Rng, fnv},
};

const DATA_BYTES_MAX: u64 = 20_000;
const SMALL_DATA_PPM: u64 = 250_000;
const SMALL_DATA_MAX: u64 = 40;
const CAPACITY_MAX: u64 = 9_000;
const SOCKET_BUFFER_MIN: u64 = 4_608;
const TINY_PPM: u64 = 200_000;
const TINY_MAX: u64 = 7;
const PIECE_MAX: u64 = 5_000;
const PENDING_PPM_MAX: u64 = 300_000;
const WRITE_CHUNK_MAX: u64 = 9_000;
const FLUSH_PPM: u64 = 200_000;
const BUFFER_MAX: u64 = 9_000;
const BUFFERED_PPM: u64 = 500_000;
const PREFIX_MAX: u64 = 40;
const NEWLINE_PPM: u64 = 60_000;
const PRINTABLE_LOW: u64 = 0x20;
const PRINTABLE_HIGH: u64 = 0x7e;
const BYTE_VALUES: u64 = 256;
const TASKS_MAX: u64 = 12;
const HOPS_MAX: usize = 4;
const HOP_MS_MAX: u64 = 50;
const PANIC_PPM: u64 = 150_000;
const ABORT_PPM: u64 = 300_000;
const TIMEOUT_PPM: u64 = 400_000;
const TIMEOUT_MS_MAX: u64 = 120;
const DROP_SET_PPM: u64 = 150_000;
const SPAWN_JOINED_PPM: u64 = 500_000;
const TLS_PPM: u64 = 500_000;
const TLS12_PPM: u64 = 400_000;
const TLS_DATA_MAX: u64 = 40_000;
const FAULT_PPM: u64 = 50_000;
const SKIP_WAKE_PPM: u64 = 500_000;
const DROP_SPAWNED_PPM: u64 = 500_000;
const CORRUPT_BITS: u8 = 0x10;
const VALUE_STRIDE: u64 = 1_000;
const NS_PER_MS: u64 = 1_000_000;
const SETTLE: Duration = Duration::from_millis(1);
const TLS_NAME: &str = "tls.sim";
const STRING_PREFIX: &str = "prefix:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamsMutation {
    None,
    DropPipeByte,
    EarlyEof,
    SkipWake,
    DropSpawned,
    CorruptTlsByte,
}

impl StreamsMutation {
    const ALL: [Self; 6] = [
        Self::None,
        Self::DropPipeByte,
        Self::EarlyEof,
        Self::SkipWake,
        Self::DropSpawned,
        Self::CorruptTlsByte,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DropPipeByte => "drop_pipe_byte",
            Self::EarlyEof => "early_eof",
            Self::SkipWake => "skip_wake",
            Self::DropSpawned => "drop_spawned",
            Self::CorruptTlsByte => "corrupt_tls_byte",
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn plain_faults(self) -> Faults {
        Faults {
            drop_byte: matches!(self, Self::DropPipeByte),
            early_eof: matches!(self, Self::EarlyEof),
            skip_wake: matches!(self, Self::SkipWake),
            corrupt: false,
        }
    }

    const fn tls_faults(self) -> Faults {
        Faults {
            drop_byte: false,
            early_eof: false,
            skip_wake: false,
            corrupt: matches!(self, Self::CorruptTlsByte),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Shape {
    capacity: usize,
    read_piece_max: usize,
    write_piece_max: usize,
    pending_ppm: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Faults {
    drop_byte: bool,
    early_eof: bool,
    skip_wake: bool,
    corrupt: bool,
}

struct Flow {
    bytes: VecDeque<u8>,
    shape: Shape,
    faults: Faults,
    rng: Rng,
    closed: bool,
    reader_gone: bool,
    reader: Option<Waker>,
    writer: Option<Waker>,
    delivered: u64,
}

impl Flow {
    fn piece(&mut self, max: usize) -> usize {
        size(self.rng.range(1, count(max)))
    }

    fn wake_reader(&mut self) -> Option<Waker> {
        let reader = self.reader.take();
        if self.faults.skip_wake && self.rng.chance(SKIP_WAKE_PPM) {
            return None;
        }
        reader
    }
}

struct Pipe(Slot<Flow>);

impl Pipe {
    fn with<R>(&self, change: impl FnOnce(&mut Flow) -> (R, Option<Waker>)) -> R {
        let mut flow = self
            .0
            .take()
            .expect("the simulator polls one task at a time, so a pipe is never taken twice");
        let (result, wake) = change(&mut flow);
        assert!(self.0.put(flow).is_none(), "a pipe has one home");
        if let Some(waker) = wake {
            waker.wake();
        }
        result
    }
}

struct PipeReader(Arc<Pipe>);
struct PipeWriter(Arc<Pipe>);

fn pipe(rng: Rng, shape: Shape, faults: Faults) -> (PipeWriter, PipeReader) {
    let pipe = Arc::new(Pipe(Slot::new(Some(Flow {
        bytes: VecDeque::new(),
        shape,
        faults,
        rng,
        closed: false,
        reader_gone: false,
        reader: None,
        writer: None,
        delivered: 0,
    }))));
    (PipeWriter(Arc::clone(&pipe)), PipeReader(pipe))
}

fn broken(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, format!("sim: {what}"))
}

impl AsyncRead for PipeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        self.0.with(|flow| {
            if flow.rng.chance(flow.shape.pending_ppm) {
                return (Poll::Pending, Some(context.waker().clone()));
            }
            if !flow.bytes.is_empty() {
                if flow.faults.early_eof && flow.delivered > 0 && flow.rng.chance(FAULT_PPM) {
                    flow.faults.early_eof = false;
                    return (Poll::Ready(Ok(())), None);
                }
                let piece = flow.piece(flow.shape.read_piece_max);
                let taken = buf.remaining().min(flow.bytes.len()).min(piece);
                let bytes: Vec<u8> = flow.bytes.drain(..taken).collect();
                buf.put_slice(&bytes);
                flow.delivered += count(taken);
                return (Poll::Ready(Ok(())), flow.writer.take());
            }
            if flow.closed {
                return (Poll::Ready(Ok(())), None);
            }
            flow.reader = Some(context.waker().clone());
            (Poll::Pending, None)
        })
    }
}

impl AsyncWrite for PipeWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        self.0.with(|flow| {
            if flow.closed {
                return (Poll::Ready(Err(broken("write after shutdown"))), None);
            }
            if flow.reader_gone {
                return (Poll::Ready(Err(broken("the reader is gone"))), None);
            }
            if flow.rng.chance(flow.shape.pending_ppm) {
                return (Poll::Pending, Some(context.waker().clone()));
            }
            let room = flow.shape.capacity.saturating_sub(flow.bytes.len());
            if room == 0 {
                flow.writer = Some(context.waker().clone());
                return (Poll::Pending, None);
            }
            let piece = flow.piece(flow.shape.write_piece_max);
            let accepted = bytes.len().min(room).min(piece);
            let mut taken = bytes[..accepted].to_vec();
            if flow.faults.drop_byte && flow.rng.chance(FAULT_PPM) {
                flow.faults.drop_byte = false;
                taken.remove(0);
            }
            if flow.faults.corrupt && flow.rng.chance(FAULT_PPM) {
                flow.faults.corrupt = false;
                let at = flow.rng.pick(taken.len());
                taken[at] ^= CORRUPT_BITS;
            }
            flow.bytes.extend(taken);
            let wake = flow.wake_reader();
            (Poll::Ready(Ok(accepted)), wake)
        })
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.with(|flow| {
            if flow.rng.chance(flow.shape.pending_ppm) {
                return (Poll::Pending, Some(context.waker().clone()));
            }
            (Poll::Ready(Ok(())), None)
        })
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.with(|flow| {
            flow.closed = true;
            let wake = flow.wake_reader();
            (Poll::Ready(Ok(())), wake)
        })
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        self.0.with(|flow| {
            flow.closed = true;
            ((), flow.reader.take())
        });
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        self.0.with(|flow| {
            flow.reader_gone = true;
            ((), flow.writer.take())
        });
    }
}

struct Duplex {
    reader: PipeReader,
    writer: PipeWriter,
}

impl AsyncRead for Duplex {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().reader).poll_read(context, buf)
    }
}

impl AsyncWrite for Duplex {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(context, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(context)
    }
}

fn size(value: u64) -> usize {
    usize::try_from(value).expect("a simulated size fits in usize")
}

fn count(value: usize) -> u64 {
    u64::try_from(value).expect("a simulated count fits in u64")
}

fn small_or(rng: &mut Rng, max: u64) -> usize {
    if rng.chance(TINY_PPM) {
        size(rng.range(1, TINY_MAX))
    } else {
        size(rng.range(1, max))
    }
}

fn socket_shape_of(rng: &mut Rng) -> Shape {
    Shape {
        capacity: size(rng.range(SOCKET_BUFFER_MIN, CAPACITY_MAX)),
        ..shape_of(rng)
    }
}

fn shape_of(rng: &mut Rng) -> Shape {
    Shape {
        capacity: small_or(rng, CAPACITY_MAX),
        read_piece_max: small_or(rng, PIECE_MAX),
        write_piece_max: small_or(rng, PIECE_MAX),
        pending_ppm: rng.below(PENDING_PPM_MAX),
    }
}

fn data_of(rng: &mut Rng, max: u64, text: bool) -> Vec<u8> {
    let length = if rng.chance(SMALL_DATA_PPM) {
        rng.below(SMALL_DATA_MAX + 1)
    } else {
        rng.below(max + 1)
    };
    (0..length)
        .map(|_| {
            let value = if !text {
                rng.below(BYTE_VALUES)
            } else if rng.chance(NEWLINE_PPM) {
                u64::from(b'\n')
            } else {
                rng.range(PRINTABLE_LOW, PRINTABLE_HIGH)
            };
            u8::try_from(value).expect("a byte value fits in u8")
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    ToEnd,
    Until,
    Line,
    Lines,
    Exact,
    Take,
    FillBuf,
    ReadBufVec,
    ToString,
}

impl Style {
    const ALL: [Self; 9] = [
        Self::ToEnd,
        Self::Until,
        Self::Line,
        Self::Lines,
        Self::Exact,
        Self::Take,
        Self::FillBuf,
        Self::ReadBufVec,
        Self::ToString,
    ];

    const fn text(self) -> bool {
        matches!(self, Self::Line | Self::Lines | Self::ToString)
    }
}

struct CopyPlan {
    data: Vec<u8>,
    style: Style,
    buffered: Option<usize>,
    consumer_buffer: usize,
    upstream: Shape,
    downstream: Shape,
}

struct EchoPlan {
    data: Vec<u8>,
    buffered: Option<usize>,
    there: Shape,
    back: Shape,
}

struct ProxyPlan {
    up: Vec<u8>,
    down: Vec<u8>,
    shapes: [Shape; 4],
}

struct TlsPlan {
    up: Vec<u8>,
    down: Vec<u8>,
    tls12: bool,
    there: Shape,
    back: Shape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    Return,
    Panic,
}

#[derive(Clone, Copy, Debug)]
struct TaskPlan {
    hops: [u64; HOPS_MAX],
    hop_count: usize,
    ending: Ending,
    abort_ms: Option<u64>,
    timeout: Option<(u64, u64)>,
}

struct JoinPlan {
    tasks: Vec<TaskPlan>,
    drop_after: Option<usize>,
}

struct Plan {
    copy: CopyPlan,
    echo: EchoPlan,
    proxy: ProxyPlan,
    tls: Option<TlsPlan>,
    join: JoinPlan,
}

fn task_plan(rng: &mut Rng) -> TaskPlan {
    let mut hops = [0; HOPS_MAX];
    let hop_count = rng.pick(HOPS_MAX + 1);
    for hop in hops.iter_mut().take(hop_count) {
        *hop = rng.below(HOP_MS_MAX + 1);
    }
    let total: u64 = hops.iter().sum();
    TaskPlan {
        hops,
        hop_count,
        ending: if rng.chance(PANIC_PPM) {
            Ending::Panic
        } else {
            Ending::Return
        },
        abort_ms: rng
            .chance(ABORT_PPM)
            .then(|| rng.below(total + HOP_MS_MAX + 1)),
        timeout: rng
            .chance(TIMEOUT_PPM)
            .then(|| (rng.below(TIMEOUT_MS_MAX + 1), rng.below(TIMEOUT_MS_MAX + 1))),
    }
}

impl Plan {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "streams.plan");
        let style = Style::ALL[rng.pick(Style::ALL.len())];
        let copy = CopyPlan {
            data: data_of(&mut rng, DATA_BYTES_MAX, style.text()),
            style,
            buffered: rng
                .chance(BUFFERED_PPM)
                .then(|| small_or(&mut rng, BUFFER_MAX)),
            consumer_buffer: small_or(&mut rng, BUFFER_MAX),
            upstream: shape_of(&mut rng),
            downstream: shape_of(&mut rng),
        };
        let echo = EchoPlan {
            data: data_of(&mut rng, DATA_BYTES_MAX, false),
            buffered: rng
                .chance(BUFFERED_PPM)
                .then(|| small_or(&mut rng, BUFFER_MAX)),
            there: shape_of(&mut rng),
            back: shape_of(&mut rng),
        };
        let proxy = ProxyPlan {
            up: data_of(&mut rng, DATA_BYTES_MAX, false),
            down: data_of(&mut rng, DATA_BYTES_MAX, false),
            shapes: [
                shape_of(&mut rng),
                shape_of(&mut rng),
                shape_of(&mut rng),
                shape_of(&mut rng),
            ],
        };
        let tls = rng.chance(TLS_PPM).then(|| TlsPlan {
            up: data_of(&mut rng, TLS_DATA_MAX, false),
            down: data_of(&mut rng, TLS_DATA_MAX, false),
            tls12: rng.chance(TLS12_PPM),
            there: socket_shape_of(&mut rng),
            back: socket_shape_of(&mut rng),
        });
        let tasks: Vec<TaskPlan> = (0..rng.range(1, TASKS_MAX))
            .map(|_| task_plan(&mut rng))
            .collect();
        let drop_after = rng.chance(DROP_SET_PPM).then(|| rng.pick(tasks.len()));
        Self {
            copy,
            echo,
            proxy,
            tls,
            join: JoinPlan { tasks, drop_after },
        }
    }

    fn describe(&self) -> String {
        let tls = self.tls.as_ref().map_or_else(
            || "off".to_owned(),
            |tls| {
                format!(
                    "{}B+{}B/{}",
                    tls.up.len(),
                    tls.down.len(),
                    if tls.tls12 { "1.2" } else { "1.3" }
                )
            },
        );
        let tasks = &self.join.tasks;
        format!(
            "copy={}B/{:?}/buf{:?} echo={}B/buf{:?} proxy={}B+{}B tls={tls} tasks={} aborts={} panics={} drop_after={:?}",
            self.copy.data.len(),
            self.copy.style,
            self.copy.buffered,
            self.echo.data.len(),
            self.echo.buffered,
            self.proxy.up.len(),
            self.proxy.down.len(),
            tasks.len(),
            tasks.iter().filter(|task| task.abort_ms.is_some()).count(),
            tasks
                .iter()
                .filter(|task| task.ending == Ending::Panic)
                .count(),
            self.join.drop_after,
        )
    }
}

async fn produce<W: AsyncWrite + Unpin>(
    writer: &mut W,
    data: &[u8],
    rng: &mut Rng,
) -> io::Result<()> {
    let mut at = 0;
    while at < data.len() {
        let chunk = size(rng.range(1, WRITE_CHUNK_MAX)).min(data.len() - at);
        let piece = &data[at..at + chunk];
        let written = match rng.below(3) {
            0 => {
                writer.write_all(piece).await?;
                chunk
            }
            1 => writer.write(piece).await?,
            _ => {
                let middle = rng.pick(chunk + 1);
                let slices = [
                    IoSlice::new(&piece[..middle]),
                    IoSlice::new(&piece[middle..]),
                ];
                writer.write_vectored(&slices).await?
            }
        };
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        at += written;
        if rng.chance(FLUSH_PPM) {
            writer.flush().await?;
        }
    }
    writer.shutdown().await
}

async fn consume<R: AsyncRead + Unpin>(
    mut reader: R,
    style: Style,
    capacity: usize,
    expected: &[u8],
    rng: &mut Rng,
) -> Result<Vec<u8>, String> {
    let failed = |error: io::Error| format!("{style:?} read failed: {error}");
    match style {
        Style::ToEnd => {
            let prefix: Vec<u8> = (0..rng.below(PREFIX_MAX + 1))
                .map(|at| u8::try_from(at).expect("a prefix byte fits in u8"))
                .collect();
            let mut out = Vec::with_capacity(size(rng.below(BUFFER_MAX)));
            out.extend_from_slice(&prefix);
            let read = reader.read_to_end(&mut out).await.map_err(failed)?;
            if !out.starts_with(&prefix) {
                return Err("read_to_end lost its buffer's prefix".to_owned());
            }
            let body = out.split_off(prefix.len());
            if read != body.len() {
                return Err(format!(
                    "read_to_end reported {read} bytes but appended {}",
                    body.len()
                ));
            }
            Ok(body)
        }
        Style::Until => {
            let mut buffered = BufReader::with_capacity(capacity, reader);
            let mut out = Vec::new();
            loop {
                let mut line = Vec::new();
                let read = buffered
                    .read_until(b'\n', &mut line)
                    .await
                    .map_err(failed)?;
                if read != line.len() {
                    return Err(format!(
                        "read_until reported {read} bytes for {}",
                        line.len()
                    ));
                }
                if read == 0 {
                    return Ok(out);
                }
                if line[..line.len() - 1].contains(&b'\n') {
                    return Err("read_until read past its delimiter".to_owned());
                }
                out.extend_from_slice(&line);
            }
        }
        Style::Line => {
            let mut buffered = BufReader::with_capacity(capacity, reader);
            let mut out = String::new();
            loop {
                let before = out.len();
                let read = buffered.read_line(&mut out).await.map_err(failed)?;
                if out.len() - before != read {
                    return Err(format!(
                        "read_line reported {read} bytes but appended {}",
                        out.len() - before
                    ));
                }
                if read == 0 {
                    return Ok(out.into_bytes());
                }
            }
        }
        Style::Lines => {
            let mut lines = BufReader::with_capacity(capacity, reader).lines();
            let mut joined: Vec<String> = Vec::new();
            while let Some(line) = lines.next_line().await.map_err(failed)? {
                joined.push(line);
            }
            let mut out = joined.join("\n").into_bytes();
            if !joined.is_empty() && expected.last() == Some(&b'\n') {
                out.push(b'\n');
            }
            Ok(out)
        }
        Style::Exact => {
            let mut out = vec![0; expected.len()];
            let mut at = 0;
            while at < expected.len() {
                let chunk = size(rng.range(1, count(capacity))).min(expected.len() - at);
                let read = reader
                    .read_exact(&mut out[at..at + chunk])
                    .await
                    .map_err(failed)?;
                if read != chunk {
                    return Err(format!("read_exact reported {read} of {chunk} bytes"));
                }
                at += chunk;
            }
            let mut probe = [0; 1];
            let tail = reader.read(&mut probe).await.map_err(failed)?;
            if tail != 0 {
                out.extend_from_slice(&probe[..tail]);
                reader.read_to_end(&mut out).await.map_err(failed)?;
            }
            Ok(out)
        }
        Style::Take => {
            let limit = rng.below(count(expected.len()) + SMALL_DATA_MAX);
            let mut out = Vec::new();
            let read = reader
                .take(limit)
                .read_to_end(&mut out)
                .await
                .map_err(failed)?;
            if count(read) > limit {
                return Err(format!("take({limit}) read {read} bytes"));
            }
            reader.read_to_end(&mut out).await.map_err(failed)?;
            Ok(out)
        }
        Style::FillBuf => {
            let mut buffered = BufReader::with_capacity(capacity, reader);
            let mut out = Vec::new();
            loop {
                let available = buffered.fill_buf().await.map_err(failed)?;
                if available.is_empty() {
                    return Ok(out);
                }
                let used = rng.pick(available.len()) + 1;
                out.extend_from_slice(&available[..used]);
                AsyncBufReadExt::consume(&mut buffered, used);
            }
        }
        Style::ReadBufVec => {
            let mut out = Vec::new();
            loop {
                let before = out.len();
                let read = reader.read_buf(&mut out).await.map_err(failed)?;
                if out.len() - before != read {
                    return Err(format!(
                        "read_buf reported {read} bytes for {}",
                        out.len() - before
                    ));
                }
                if read == 0 {
                    return Ok(out);
                }
            }
        }
        Style::ToString => {
            let mut out = String::from(STRING_PREFIX);
            reader.read_to_string(&mut out).await.map_err(failed)?;
            out.strip_prefix(STRING_PREFIX).map_or_else(
                || Err("read_to_string lost its prefix".to_owned()),
                |rest| Ok(rest.as_bytes().to_vec()),
            )
        }
    }
}

async fn both<A: Future, B: Future>(first: A, second: B) -> (A::Output, B::Output) {
    let mut first = std::pin::pin!(first);
    let mut second = std::pin::pin!(second);
    let mut left = None;
    let mut right = None;
    std::future::poll_fn(|context| {
        if left.is_none()
            && let Poll::Ready(output) = first.as_mut().poll(context)
        {
            left = Some(output);
        }
        if right.is_none()
            && let Poll::Ready(output) = second.as_mut().poll(context)
        {
            right = Some(output);
        }
        match (left.take(), right.take()) {
            (Some(left), Some(right)) => Poll::Ready((left, right)),
            (held_left, held_right) => {
                left = held_left;
                right = held_right;
                Poll::Pending
            }
        }
    })
    .await
}

fn check_bytes(what: &str, expected: &[u8], got: &[u8]) -> Result<(), String> {
    if expected == got {
        return Ok(());
    }
    let first = expected
        .iter()
        .zip(got)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| expected.len().min(got.len()));
    Err(format!(
        "{what}: expected {} bytes, got {}, first difference at {first}",
        expected.len(),
        got.len()
    ))
}

type Sides = (io::Result<()>, io::Result<Vec<u8>>);

#[derive(Default)]
struct Results {
    copy_count: Option<io::Result<u64>>,
    copy_shutdown: Option<io::Result<()>>,
    copy_producer: Option<io::Result<()>>,
    copy_out: Option<Result<Vec<u8>, String>>,
    echo_count: Option<io::Result<u64>>,
    echo_paired: Option<bool>,
    echo_producer: Option<io::Result<()>>,
    echo_out: Option<io::Result<Vec<u8>>>,
    proxy_counts: Option<io::Result<(u64, u64)>>,
    proxy_client: Option<Sides>,
    proxy_server: Option<Sides>,
    join: Vec<(usize, Joined)>,
    join_lens: Vec<String>,
    join_end: Option<bool>,
    dropped_check: Option<Vec<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Joined {
    Value(u64),
    Panic(Option<usize>),
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Note {
    Returned(usize),
    Panicking(usize),
    Aborted(usize),
    Dropped(usize),
    TimedOut(usize, bool),
    SetDropped,
}

struct Notes(Slot<Vec<Note>>);

impl Notes {
    fn change<R>(&self, change: impl FnOnce(&mut Vec<Note>) -> R) -> R {
        let mut notes = self
            .0
            .take()
            .expect("the simulator polls one task at a time, so the notes are never taken twice");
        let result = change(&mut notes);
        assert!(self.0.put(notes).is_none(), "the notes have one home");
        result
    }

    fn push(&self, note: Note) {
        self.change(|notes| notes.push(note));
    }

    fn read(&self) -> Vec<Note> {
        self.change(|notes| notes.clone())
    }
}

struct Guard {
    index: usize,
    notes: Arc<Notes>,
    settled: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.settled {
            self.notes.push(Note::Dropped(self.index));
        }
    }
}

fn value_of(index: usize) -> u64 {
    (count(index) + 1) * VALUE_STRIDE + 7
}

fn job(index: usize, plan: TaskPlan, notes: Arc<Notes>) -> impl Future<Output = u64> + Send {
    let guard = Guard {
        index,
        notes: Arc::clone(&notes),
        settled: false,
    };
    run_job(index, plan, notes, guard)
}

async fn run_job(index: usize, plan: TaskPlan, notes: Arc<Notes>, mut guard: Guard) -> u64 {
    for hop in plan.hops.iter().take(plan.hop_count) {
        nanocode_platform::sleep(Duration::from_millis(*hop)).await;
    }
    if let Some((limit, inner)) = plan.timeout {
        let timed = nanocode_platform::timeout(
            Duration::from_millis(limit),
            nanocode_platform::sleep(Duration::from_millis(inner)),
        )
        .await;
        notes.push(Note::TimedOut(index, timed.is_err()));
    }
    guard.settled = true;
    match plan.ending {
        Ending::Return => {
            notes.push(Note::Returned(index));
            value_of(index)
        }
        Ending::Panic => {
            notes.push(Note::Panicking(index));
            std::panic::resume_unwind(Box::new(index))
        }
    }
}

fn joined_of(outcome: Result<u64, JoinError>) -> Joined {
    match outcome {
        Ok(value) => Joined::Value(value),
        Err(error) if error.is_cancelled() => Joined::Cancelled,
        Err(error) => Joined::Panic(
            error
                .into_panic()
                .downcast::<usize>()
                .ok()
                .map(|index| *index),
        ),
    }
}

fn spawn_jobs(
    plan: &JoinPlan,
    notes: &Arc<Notes>,
    mutation: StreamsMutation,
    rng: &mut Rng,
) -> (JoinSet<u64>, Vec<AbortHandle>) {
    let mut set = JoinSet::new();
    let mut handles = Vec::new();
    for (index, task) in plan.tasks.iter().enumerate() {
        let future = job(index, *task, Arc::clone(notes));
        let handle = if mutation == StreamsMutation::DropSpawned && rng.chance(DROP_SPAWNED_PPM) {
            set.push(nanocode_executor::spawn_with(future, drop))
        } else if rng.chance(SPAWN_JOINED_PPM) {
            nanocode_platform::spawn_joined(&mut set, future)
        } else {
            set.push(nanocode_platform::spawn_task(future))
        };
        handles.push(handle);
    }
    (set, handles)
}

fn unsettled(plan: &JoinPlan, notes: &[Note]) -> Vec<usize> {
    (0..plan.tasks.len())
        .filter(|index| {
            !notes.iter().any(|note| {
                matches!(note, Note::Returned(at) | Note::Panicking(at) | Note::Dropped(at) if at == index)
            })
        })
        .collect()
}

async fn join_jobs(
    mut set: JoinSet<u64>,
    ids: BTreeMap<Id, usize>,
    plan: &JoinPlan,
    notes: &Arc<Notes>,
    results: &RefCell<Results>,
) {
    let mut joined = 0;
    loop {
        if plan.drop_after == Some(joined) {
            drop(set);
            notes.push(Note::SetDropped);
            nanocode_platform::sleep(SETTLE).await;
            results.borrow_mut().dropped_check = Some(unsettled(plan, &notes.read()));
            return;
        }
        let before = set.len();
        let Some(outcome) = set.join_next_with_id().await else {
            results.borrow_mut().join_end = Some(before == 0);
            return;
        };
        if set.len() + 1 != before {
            results
                .borrow_mut()
                .join_lens
                .push(format!("len {before} became {} after a join", set.len()));
        }
        joined += 1;
        let (id, outcome) = match outcome {
            Ok((id, value)) => (id, Ok(value)),
            Err(error) => (error.id(), Err(error)),
        };
        let index = ids.get(&id).copied().unwrap_or(usize::MAX);
        results.borrow_mut().join.push((index, joined_of(outcome)));
    }
}

async fn abort_jobs(plan: &JoinPlan, handles: Vec<AbortHandle>, notes: &Arc<Notes>) {
    let mut planned: Vec<(u64, usize)> = plan
        .tasks
        .iter()
        .enumerate()
        .filter_map(|(index, task)| task.abort_ms.map(|at| (at, index)))
        .collect();
    planned.sort_unstable();
    let mut now = 0;
    for (at, index) in planned {
        nanocode_platform::sleep(Duration::from_millis(at - now)).await;
        now = at;
        notes.push(Note::Aborted(index));
        handles[index].abort();
    }
}

fn check_timeouts(plan: &JoinPlan, notes: &[Note]) -> Result<(), String> {
    for (index, task) in plan.tasks.iter().enumerate() {
        let Some((limit, inner)) = task.timeout else {
            continue;
        };
        let seen = notes.iter().find_map(|note| match note {
            Note::TimedOut(at, elapsed) if *at == index => Some(*elapsed),
            _ => None,
        });
        if let Some(elapsed) = seen
            && elapsed != (inner > limit)
        {
            return Err(format!(
                "timeout_verdict: task {index} slept {inner} ms under a {limit} ms timeout and elapsed={elapsed}"
            ));
        }
    }
    Ok(())
}

fn expected_join(index: usize, task: TaskPlan, notes: &[Note]) -> Joined {
    let position = |wanted: Note| notes.iter().position(|note| *note == wanted);
    let finished = match task.ending {
        Ending::Return => position(Note::Returned(index)),
        Ending::Panic => position(Note::Panicking(index)),
    };
    let cancelled_first = match (position(Note::Aborted(index)), finished) {
        (Some(abort), Some(finish)) => abort < finish,
        (Some(_), None) => true,
        (None, _) => false,
    };
    match (cancelled_first, task.ending) {
        (true, _) => Joined::Cancelled,
        (false, Ending::Return) => Joined::Value(value_of(index)),
        (false, Ending::Panic) => Joined::Panic(Some(index)),
    }
}

fn check_join(plan: &JoinPlan, results: &Results, notes: &[Note]) -> Result<(), String> {
    check_timeouts(plan, notes)?;
    if let Some(unsettled) = &results.dropped_check {
        if !unsettled.is_empty() {
            return Err(format!(
                "abort_drops: tasks {unsettled:?} still held their futures after their JoinSet was dropped"
            ));
        }
        return Ok(());
    }
    if let Some(problem) = results.join_lens.first() {
        return Err(format!("join_len: {problem}"));
    }
    if results.join_end != Some(true) {
        return Err(format!(
            "joined_once: join_next ended {:?}",
            results.join_end
        ));
    }
    let mut seen = vec![0_usize; plan.tasks.len()];
    for (index, _) in &results.join {
        match seen.get_mut(*index) {
            Some(times) => *times += 1,
            None => return Err("joined_once: a join returned an unknown task id".to_owned()),
        }
    }
    if let Some(index) = seen.iter().position(|times| *times != 1) {
        return Err(format!(
            "joined_once: task {index} joined {} times",
            seen[index]
        ));
    }
    for (index, joined) in &results.join {
        let task = plan.tasks[*index];
        let expected = expected_join(*index, task, notes);
        if *joined != expected {
            return Err(format!(
                "joined_output: task {index} ({:?}, abort {:?}) joined {joined:?}, expected {expected:?}",
                task.ending, task.abort_ms
            ));
        }
    }
    Ok(())
}

fn check_copy(plan: &CopyPlan, results: &Results) -> Result<(), String> {
    match &results.copy_producer {
        Some(Ok(())) => {}
        other => return Err(format!("copy_exact: the producer ended {other:?}")),
    }
    match &results.copy_count {
        Some(Ok(copied)) if *copied == count(plan.data.len()) => {}
        other => {
            return Err(format!(
                "copy_exact: copy of {} bytes returned {other:?}",
                plan.data.len()
            ));
        }
    }
    match &results.copy_shutdown {
        Some(Ok(())) => {}
        other => return Err(format!("copy_exact: the copier's shutdown ended {other:?}")),
    }
    match &results.copy_out {
        Some(Ok(out)) => check_bytes("copy_exact", &plan.data, out),
        other => Err(format!("copy_exact: the consumer ended {other:?}")),
    }
}

fn check_echo(plan: &EchoPlan, results: &Results) -> Result<(), String> {
    if results.echo_paired != Some(true) {
        return Err("echo_exact: split's halves are not a pair".to_owned());
    }
    match (
        &results.echo_producer,
        &results.echo_count,
        &results.echo_out,
    ) {
        (Some(Ok(())), Some(Ok(copied)), Some(Ok(out))) => {
            if *copied != count(plan.data.len()) {
                return Err(format!(
                    "echo_exact: the echo copied {copied} of {} bytes",
                    plan.data.len()
                ));
            }
            check_bytes("echo_exact", &plan.data, out)
        }
        other => Err(format!("echo_exact: ended {other:?}")),
    }
}

fn check_proxy(plan: &ProxyPlan, results: &Results) -> Result<(), String> {
    match (
        &results.proxy_counts,
        &results.proxy_client,
        &results.proxy_server,
    ) {
        (Some(Ok((up, down))), Some((Ok(()), Ok(client_got))), Some((Ok(()), Ok(server_got)))) => {
            if (*up, *down) != (count(plan.up.len()), count(plan.down.len())) {
                return Err(format!(
                    "proxy_exact: copy_bidirectional returned ({up}, {down}) for ({}, {})",
                    plan.up.len(),
                    plan.down.len()
                ));
            }
            check_bytes("proxy_exact up", &plan.up, server_got)?;
            check_bytes("proxy_exact down", &plan.down, client_got)
        }
        other => Err(format!("proxy_exact: ended {other:?}")),
    }
}

fn check_run(plan: &Plan, results: &Results, notes: &[Note], ended: Ended) -> Result<(), String> {
    if ended != Ended::Finished {
        return Err(format!("progress: the run ended {ended:?}"));
    }
    check_copy(&plan.copy, results)?;
    check_echo(&plan.echo, results)?;
    check_proxy(&plan.proxy, results)?;
    check_join(&plan.join, results, notes)
}

struct Trace {
    digest: Cell<u64>,
    verbose: bool,
}

impl Trace {
    fn line(&self, text: &str) {
        self.digest.set(fnv(self.digest.get(), text.as_bytes()));
        self.digest.set(fnv(self.digest.get(), b"\n"));
        if self.verbose {
            println!("{text}");
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StreamsStats {
    bytes: u64,
    joined: u64,
    cancelled: u64,
    panicked: u64,
    dropped_sets: u64,
    tls: u64,
    tls12: u64,
    elapsed_ms: u64,
}

impl StreamsStats {
    pub const fn add(&mut self, other: &Self) {
        self.bytes += other.bytes;
        self.joined += other.joined;
        self.cancelled += other.cancelled;
        self.panicked += other.panicked;
        self.dropped_sets += other.dropped_sets;
        self.tls += other.tls;
        self.tls12 += other.tls12;
        self.elapsed_ms += other.elapsed_ms;
    }
}

pub struct StreamsOutcome {
    digest: u64,
    wrong: Option<String>,
    stats: StreamsStats,
    shape: String,
}

fn plain_pipe(
    seed: u64,
    label: &str,
    shape: Shape,
    mutation: StreamsMutation,
) -> (PipeWriter, PipeReader) {
    pipe(Rng::derive(seed, label), shape, mutation.plain_faults())
}

async fn copy_through(
    reader: PipeReader,
    buffered: Option<usize>,
    writer: &mut (impl AsyncWrite + Unpin),
) -> io::Result<u64> {
    match buffered {
        Some(capacity) => {
            nanocode_io::copy(&mut BufReader::with_capacity(capacity, reader), writer).await
        }
        None => nanocode_io::copy(&mut { reader }, writer).await,
    }
}

async fn echo(plan: &EchoPlan, reader: PipeReader, writer: PipeWriter, results: &RefCell<Results>) {
    let (reader, mut writer) = nanocode_io::split(Duplex { reader, writer });
    results.borrow_mut().echo_paired = Some(reader.is_pair_of(&writer));
    let copied = match plan.buffered {
        Some(capacity) => {
            let mut reader = BufReader::with_capacity(capacity, reader);
            nanocode_io::copy(&mut reader, &mut writer).await
        }
        None => nanocode_io::copy(&mut { reader }, &mut writer).await,
    };
    let copied = match copied {
        Ok(copied) => writer.shutdown().await.map(|()| copied),
        Err(error) => Err(error),
    };
    results.borrow_mut().echo_count = Some(copied);
}

async fn talk(
    seed: u64,
    label: &str,
    data: &[u8],
    mut writer: PipeWriter,
    mut reader: PipeReader,
) -> Sides {
    let mut rng = Rng::derive(seed, label);
    let mut out = Vec::new();
    let (sent, got) = both(
        produce(&mut writer, data, &mut rng),
        reader.read_to_end(&mut out),
    )
    .await;
    (sent, got.map(|_| out))
}

fn plain_run(
    seed: u64,
    plan: &Plan,
    mutation: StreamsMutation,
    clock: &Arc<Clock>,
) -> (Results, Vec<Note>, Ended) {
    let results = RefCell::new(Results::default());
    let notes = Arc::new(Notes(Slot::new(Some(Vec::new()))));
    let copy = &plan.copy;
    let (mut upstream_writer, upstream_reader) =
        plain_pipe(seed, "copy.up", copy.upstream, mutation);
    let (mut downstream_writer, downstream_reader) =
        plain_pipe(seed, "copy.down", copy.downstream, mutation);
    let (mut there_writer, there_reader) =
        plain_pipe(seed, "echo.there", plan.echo.there, mutation);
    let (back_writer, mut back_reader) = plain_pipe(seed, "echo.back", plan.echo.back, mutation);
    let shapes = plan.proxy.shapes;
    let (client_up, proxy_up) = plain_pipe(seed, "proxy.client_up", shapes[0], mutation);
    let (proxy_down, client_down) = plain_pipe(seed, "proxy.client_down", shapes[1], mutation);
    let (proxy_out, server_in) = plain_pipe(seed, "proxy.server_in", shapes[2], mutation);
    let (server_out, proxy_back) = plain_pipe(seed, "proxy.server_out", shapes[3], mutation);
    let mut spawn_rng = Rng::derive(seed, "join.spawn");
    let (set, handles) = spawn_jobs(&plan.join, &notes, mutation, &mut spawn_rng);
    let ids: BTreeMap<Id, usize> = handles
        .iter()
        .enumerate()
        .map(|(index, handle)| (handle.id(), index))
        .collect();
    let results_ref = &results;
    let join_notes = Arc::clone(&notes);
    let abort_notes = Arc::clone(&notes);
    let tasks: Vec<Task<'_>> = vec![
        Box::pin(async move {
            let mut rng = Rng::derive(seed, "copy.producer");
            let produced = produce(&mut upstream_writer, &copy.data, &mut rng).await;
            results_ref.borrow_mut().copy_producer = Some(produced);
        }),
        Box::pin(async move {
            let copied = copy_through(upstream_reader, copy.buffered, &mut downstream_writer).await;
            let shutdown = downstream_writer.shutdown().await;
            let mut results = results_ref.borrow_mut();
            results.copy_count = Some(copied);
            results.copy_shutdown = Some(shutdown);
        }),
        Box::pin(async move {
            let mut rng = Rng::derive(seed, "copy.consumer");
            let out = consume(
                downstream_reader,
                copy.style,
                copy.consumer_buffer,
                &copy.data,
                &mut rng,
            )
            .await;
            results_ref.borrow_mut().copy_out = Some(out);
        }),
        Box::pin(async move {
            let mut rng = Rng::derive(seed, "echo.producer");
            let produced = produce(&mut there_writer, &plan.echo.data, &mut rng).await;
            results_ref.borrow_mut().echo_producer = Some(produced);
        }),
        Box::pin(async move {
            let mut out = Vec::new();
            let read = back_reader.read_to_end(&mut out).await.map(|_| out);
            results_ref.borrow_mut().echo_out = Some(read);
        }),
        Box::pin(echo(&plan.echo, there_reader, back_writer, results_ref)),
        Box::pin(async move {
            let mut client = Duplex {
                reader: proxy_up,
                writer: proxy_down,
            };
            let mut server = Duplex {
                reader: proxy_back,
                writer: proxy_out,
            };
            let counts = nanocode_io::copy_bidirectional(&mut client, &mut server).await;
            results_ref.borrow_mut().proxy_counts = Some(counts);
        }),
        Box::pin(async move {
            let sides = talk(seed, "proxy.client", &plan.proxy.up, client_up, client_down).await;
            results_ref.borrow_mut().proxy_client = Some(sides);
        }),
        Box::pin(async move {
            let sides = talk(
                seed,
                "proxy.server",
                &plan.proxy.down,
                server_out,
                server_in,
            )
            .await;
            results_ref.borrow_mut().proxy_server = Some(sides);
        }),
        Box::pin(async move { join_jobs(set, ids, &plan.join, &join_notes, results_ref).await }),
        Box::pin(async move { abort_jobs(&plan.join, handles, &abort_notes).await }),
    ];
    let ended = Executor::new(Arc::clone(clock), Rng::derive(seed, "scheduler")).run(tasks, None);
    let notes = notes.read();
    (results.into_inner(), notes, ended)
}

#[cfg(unix)]
mod tls {
    use std::sync::{Arc, OnceLock};

    use nanocode_sandbox::egress::Tls;
    use rustls::{ClientConfig, RootCertStore, ServerConfig, SupportedProtocolVersion};

    pub struct Configs {
        pub server: Arc<ServerConfig>,
        pub client13: Arc<ClientConfig>,
        pub client12: Arc<ClientConfig>,
    }

    static CONFIGS: OnceLock<Result<Configs, String>> = OnceLock::new();

    fn client(
        roots: &RootCertStore,
        versions: &[&'static SupportedProtocolVersion],
    ) -> Result<Arc<ClientConfig>, String> {
        Ok(Arc::new(
            ClientConfig::builder_with_provider(Arc::new(nanocode_tls::provider()))
                .with_protocol_versions(versions)
                .map_err(|error| error.to_string())?
                .with_root_certificates(roots.clone())
                .with_no_client_auth(),
        ))
    }

    fn build() -> Result<Configs, String> {
        let issuer = Tls::with_roots("nanocode-sim", RootCertStore::empty())
            .map_err(|error| error.to_string())?;
        let server = issuer
            .server(super::TLS_NAME)
            .map_err(|error| error.to_string())?;
        let mut roots = RootCertStore::empty();
        roots
            .add(issuer.ca_der().clone())
            .map_err(|error| error.to_string())?;
        Ok(Configs {
            server,
            client13: client(&roots, &[&rustls::version::TLS13])?,
            client12: client(&roots, &[&rustls::version::TLS12])?,
        })
    }

    pub fn configs() -> Result<&'static Configs, String> {
        CONFIGS.get_or_init(build).as_ref().map_err(Clone::clone)
    }
}

#[cfg(unix)]
async fn tls_side<S>(stream: io::Result<S>, data: &[u8], rng: &mut Rng) -> io::Result<Sides>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = nanocode_io::split(stream?);
    let mut out = Vec::new();
    let (sent, got) = both(
        produce(&mut writer, data, rng),
        reader.read_to_end(&mut out),
    )
    .await;
    Ok((sent, got.map(|_| out)))
}

#[cfg(unix)]
fn tls_run(
    seed: u64,
    plan: &TlsPlan,
    mutation: StreamsMutation,
    clock: &Arc<Clock>,
) -> Result<(), String> {
    let configs = tls::configs()?;
    let client_config = Arc::clone(if plan.tls12 {
        &configs.client12
    } else {
        &configs.client13
    });
    let server_config = Arc::clone(&configs.server);
    let (there_writer, there_reader) = pipe(
        Rng::derive(seed, "tls.there"),
        plan.there,
        mutation.tls_faults(),
    );
    let (back_writer, back_reader) = pipe(
        Rng::derive(seed, "tls.back"),
        plan.back,
        mutation.tls_faults(),
    );
    let client_end: RefCell<Option<io::Result<Sides>>> = RefCell::new(None);
    let server_end: RefCell<Option<io::Result<Sides>>> = RefCell::new(None);
    let client_ref = &client_end;
    let server_ref = &server_end;
    let name =
        rustls::pki_types::ServerName::try_from(TLS_NAME).map_err(|error| error.to_string())?;
    let tasks: Vec<Task<'_>> = vec![
        Box::pin(async move {
            let mut rng = Rng::derive(seed, "tls.client");
            let duplex = Duplex {
                reader: back_reader,
                writer: there_writer,
            };
            let stream = nanocode_tls::connect(client_config, name, duplex).await;
            *client_ref.borrow_mut() = Some(tls_side(stream, &plan.up, &mut rng).await);
        }),
        Box::pin(async move {
            let mut rng = Rng::derive(seed, "tls.server");
            let duplex = Duplex {
                reader: there_reader,
                writer: back_writer,
            };
            let stream = nanocode_tls::accept(server_config, duplex).await;
            *server_ref.borrow_mut() = Some(tls_side(stream, &plan.down, &mut rng).await);
        }),
    ];
    let ended =
        Executor::new(Arc::clone(clock), Rng::derive(seed, "tls.scheduler")).run(tasks, None);
    if ended != Ended::Finished {
        return Err(format!("tls_exact: the run ended {ended:?}"));
    }
    match (client_end.into_inner(), server_end.into_inner()) {
        (Some(Ok((Ok(()), Ok(client_got)))), Some(Ok((Ok(()), Ok(server_got))))) => {
            check_bytes("tls_exact up", &plan.up, &server_got)?;
            check_bytes("tls_exact down", &plan.down, &client_got)
        }
        (client, server) => Err(format!(
            "tls_exact: client ended {client:?}, server ended {server:?}"
        )),
    }
}

#[cfg(not(unix))]
const fn tls_run(
    _seed: u64,
    _plan: &TlsPlan,
    _mutation: StreamsMutation,
    _clock: &Arc<Clock>,
) -> Result<(), String> {
    Ok(())
}

fn stats_of(plan: &Plan, results: &Results, clock: &Clock) -> StreamsStats {
    let count_of = |wanted: fn(&Joined) -> bool| {
        count(
            results
                .join
                .iter()
                .filter(|(_, joined)| wanted(joined))
                .count(),
        )
    };
    StreamsStats {
        bytes: count(
            plan.copy.data.len()
                + plan.echo.data.len()
                + plan.proxy.up.len()
                + plan.proxy.down.len()
                + plan
                    .tls
                    .as_ref()
                    .map_or(0, |tls| tls.up.len() + tls.down.len()),
        ),
        joined: count(results.join.len()),
        cancelled: count_of(|joined| *joined == Joined::Cancelled),
        panicked: count_of(|joined| matches!(joined, Joined::Panic(_))),
        dropped_sets: u64::from(results.dropped_check.is_some()),
        tls: u64::from(plan.tls.is_some()),
        tls12: u64::from(plan.tls.as_ref().is_some_and(|tls| tls.tls12)),
        elapsed_ms: clock.now_ns() / NS_PER_MS,
    }
}

fn simulate(seed: u64, mutation: StreamsMutation, verbose: bool) -> StreamsOutcome {
    let plan = Plan::of(seed);
    let shape = plan.describe();
    let clock = Clock::new();
    crate::host::enter(seed, &clock);
    let trace = Trace {
        digest: Cell::new(FNV_OFFSET),
        verbose,
    };
    trace.line(&format!("seed {seed} {shape}"));
    let (results, notes, ended) = plain_run(seed, &plan, mutation, &clock);
    trace.line(&format!("plain ended {ended:?} at {} ns", clock.now_ns()));
    for (index, joined) in &results.join {
        trace.line(&format!("joined {index} {joined:?}"));
    }
    for note in &notes {
        trace.line(&format!("note {note:?}"));
    }
    let stats = stats_of(&plan, &results, &clock);
    let mut wrong = check_run(&plan, &results, &notes, ended).err();
    if wrong.is_none()
        && let Some(tls_plan) = &plan.tls
    {
        let tls_clock = Clock::new();
        crate::host::enter(seed, &tls_clock);
        wrong = tls_run(seed, tls_plan, mutation, &tls_clock).err();
    }
    trace.line(&format!("verdict {wrong:?}"));
    StreamsOutcome {
        digest: trace.digest.get(),
        wrong,
        stats,
        shape,
    }
}

pub fn run_seed(seed: u64, mutation: StreamsMutation, verbose: bool) -> StreamsOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation, verbose)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        StreamsOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: StreamsStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: StreamsMutation,
    twice: bool,
    verbose: bool,
    totals: &mut StreamsStats,
) -> Option<String> {
    let first = run_seed(seed, mutation, verbose);
    totals.add(&first.stats);
    if verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation, false);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: StreamsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario streams --seed {seed}");
    if mutation != StreamsMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: StreamsMutation,
    totals: &StreamsStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=streams mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        StreamsMutation::None if failed == 0 => ExitCode::SUCCESS,
        StreamsMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}
