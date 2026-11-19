use std::{
    future::{Future, poll_fn},
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::Poll,
    thread::Thread,
    time::{Duration, Instant},
};

use nanocode_executor::{Builder, Runtime, UnixStream, sim, yield_now};
use nanocode_sync::{AtomicWaker, Slot};

use crate::rng::Rng;

const BUSY_PPM: u64 = 600_000;
const BUSY_YIELDS_PPM: u64 = 200_000;
const BUSY_TIMER_PPM: u64 = 300_000;
const BUSY_ROUNDS: u64 = 1_024;
const BUSY_PATIENCE: Duration = Duration::from_millis(250);
const THREADS_PPM: u64 = 600_000;
const MESSAGES_MIN: u64 = 8;
const MESSAGES_MAX: u64 = 48;
const PAUSES_US: [u64; 3] = [20, 50, 100];
const MULTI_PPM: u64 = 300_000;
const STALL: Duration = Duration::from_secs(1);
const WATCH: Duration = Duration::from_millis(1);
const ACK_WAIT: Duration = Duration::from_millis(10);
const SLEEPERS_PPM: u64 = 500_000;
const SLEEPERS_MIN: u64 = 4;
const SLEEPERS_MAX: u64 = 12;
const NAPS_MIN: u64 = 20;
const NAPS_MAX: u64 = 160;
const NAP_US_MIN: u64 = 50;
const NAP_US_MAX: u64 = 1_000;

#[derive(Clone, Copy, Debug)]
pub struct Busy {
    pub yields: bool,
    pub timer: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Threads {
    pub messages: u64,
    pub pause_us: u64,
    pub receiver_multi: bool,
    pub spinner_multi: bool,
}

#[derive(Clone, Debug)]
pub struct Sleepers {
    pub naps: Vec<Vec<u64>>,
}

pub fn sleepers_of(rng: &mut Rng) -> Option<Sleepers> {
    if !rng.chance(SLEEPERS_PPM) {
        return None;
    }
    let mut naps = Vec::new();
    for _ in 0..rng.range(SLEEPERS_MIN, SLEEPERS_MAX) {
        let mut thread = Vec::new();
        for _ in 0..rng.range(NAPS_MIN, NAPS_MAX) {
            thread.push(rng.range(NAP_US_MIN, NAP_US_MAX));
        }
        naps.push(thread);
    }
    Some(Sleepers { naps })
}

impl Sleepers {
    pub fn describe(&self) -> String {
        let naps: Vec<String> = self.naps.iter().map(|naps| naps.len().to_string()).collect();
        format!("sleepers=[{}]", naps.join(","))
    }

    fn total(&self) -> u64 {
        self.naps
            .iter()
            .map(|naps| u64::try_from(naps.len()).expect("a nap count fits in 64 bits"))
            .sum()
    }
}

pub fn busy_of(rng: &mut Rng) -> Option<Busy> {
    rng.chance(BUSY_PPM).then(|| Busy {
        yields: rng.chance(BUSY_YIELDS_PPM),
        timer: rng.chance(BUSY_TIMER_PPM),
    })
}

pub fn threads_of(rng: &mut Rng) -> Option<Threads> {
    rng.chance(THREADS_PPM).then(|| Threads {
        messages: rng.range(MESSAGES_MIN, MESSAGES_MAX),
        pause_us: PAUSES_US[rng.pick(PAUSES_US.len())],
        receiver_multi: rng.chance(MULTI_PPM),
        spinner_multi: rng.chance(MULTI_PPM),
    })
}

impl Busy {
    pub fn describe(self) -> String {
        format!(
            "busy={}{}",
            if self.timer { "timer" } else { "io" },
            if self.yields { "+yields" } else { "" }
        )
    }
}

impl Threads {
    pub fn describe(self) -> String {
        format!(
            "threads=m{}p{}{}{}",
            self.messages,
            self.pause_us,
            if self.receiver_multi { "R" } else { "r" },
            if self.spinner_multi { "S" } else { "s" }
        )
    }
}

struct Bell {
    rung: AtomicBool,
    waker: AtomicWaker,
}

impl Bell {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            rung: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        })
    }

    fn ring(&self) {
        self.rung.store(true, Ordering::SeqCst);
        self.waker.wake();
    }

    fn wait(&self) -> impl Future<Output = ()> + '_ {
        poll_fn(|context| {
            if self.rung.swap(false, Ordering::SeqCst) {
                return Poll::Ready(());
            }
            self.waker.register(context.waker());
            if self.rung.swap(false, Ordering::SeqCst) {
                return Poll::Ready(());
            }
            Poll::Pending
        })
    }
}

pub async fn busy(plan: Busy) -> Result<(), String> {
    let delivered = Arc::new(AtomicBool::new(false));
    let event = {
        let delivered = Arc::clone(&delivered);
        if plan.timer {
            nanocode_executor::spawn(async move {
                nanocode_executor::sleep(Duration::from_micros(1)).await;
                delivered.store(true, Ordering::SeqCst);
            })
        } else {
            let (reader, mut writer) =
                std::os::unix::net::UnixStream::pair().expect("a socket pair opens");
            writer
                .write_all(&[1])
                .expect("one byte fits an empty socket");
            let reader = UnixStream::from_std(reader).expect("a socket registers");
            nanocode_executor::spawn(async move {
                reader.readable().await.expect("a socket pair reads");
                delivered.store(true, Ordering::SeqCst);
                drop(writer);
            })
        }
    };
    let ping = Bell::new();
    let pong = Bell::new();
    let stop = Arc::new(AtomicBool::new(false));
    let pinger = {
        let (ping, pong, stop) = (Arc::clone(&ping), Arc::clone(&pong), Arc::clone(&stop));
        nanocode_executor::spawn(async move {
            loop {
                ping.wait().await;
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                pong.ring();
            }
        })
    };
    let mut seen = false;
    let started = Instant::now();
    let mut rounds = 0_u64;
    while rounds < BUSY_ROUNDS || started.elapsed() < BUSY_PATIENCE {
        if delivered.load(Ordering::SeqCst) {
            seen = true;
            break;
        }
        rounds += 1;
        ping.ring();
        pong.wait().await;
        if plan.yields {
            yield_now().await;
        }
    }
    stop.store(true, Ordering::SeqCst);
    ping.ring();
    pinger.await.expect("the pinger ends");
    event.await.expect("the event task ends");
    if seen {
        Ok(())
    } else {
        Err(format!(
            "a ready {} was not delivered within {rounds} rounds ({BUSY_PATIENCE:?}) of ping-pong between block_on and a task: the reactor did not turn",
            if plan.timer { "timer" } else { "socket" }
        ))
    }
}

fn runtime(multi: bool) -> Runtime {
    if multi {
        Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("a multi-thread runtime builds")
    } else {
        Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime builds")
    }
}

async fn receive(
    inbox: Arc<Bell>,
    messages: u64,
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    sender: Arc<Slot<Thread>>,
) {
    for _ in 0..messages {
        inbox.wait().await;
        if stop.load(Ordering::SeqCst) {
            break;
        }
        progress.fetch_add(1, Ordering::SeqCst);
        if let Some(thread) = sender.take() {
            thread.unpark();
            sender.put(thread);
        }
    }
}

fn nudge(mut doorbell: &std::os::unix::net::UnixStream) {
    match doorbell.write(&[1]) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => panic!("the doorbell socket writes: {error}"),
    }
}

struct Halt {
    inbox: Arc<Bell>,
    stop: Arc<AtomicBool>,
    spinning: Arc<AtomicBool>,
}

impl Drop for Halt {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.spinning.store(false, Ordering::SeqCst);
        self.inbox.ring();
    }
}

pub struct Threaded {
    pub received: u64,
    pub stalled: Option<String>,
}

pub fn threads(plan: Threads) -> Threaded {
    sim::pause_before_drive(Duration::from_micros(plan.pause_us));
    let inbox = Bell::new();
    let stop = Arc::new(AtomicBool::new(false));
    let spinning = Arc::new(AtomicBool::new(true));
    let progress = Arc::new(AtomicU64::new(0));
    let sender_thread = Arc::new(Slot::new(None));
    let mut stalled = None;
    let (door, doorbell) = std::os::unix::net::UnixStream::pair().expect("a socket pair opens");
    doorbell
        .set_nonblocking(true)
        .expect("a socket becomes nonblocking");
    let door = UnixStream::from_std(door).expect("a socket registers");
    std::thread::scope(|scope| {
        let _halt = Halt {
            inbox: Arc::clone(&inbox),
            stop: Arc::clone(&stop),
            spinning: Arc::clone(&spinning),
        };
        let spinner = {
            let spinning = Arc::clone(&spinning);
            scope.spawn(move || {
                let runtime = runtime(plan.spinner_multi);
                runtime.block_on(async move {
                    let spin = nanocode_executor::spawn(async move {
                        while spinning.load(Ordering::SeqCst) {
                            yield_now().await;
                        }
                    });
                    spin.await.expect("the spinner ends");
                });
            })
        };
        let receiver = {
            let (inbox, stop, progress, sender_thread) = (
                Arc::clone(&inbox),
                Arc::clone(&stop),
                Arc::clone(&progress),
                Arc::clone(&sender_thread),
            );
            scope.spawn(move || {
                let runtime = runtime(plan.receiver_multi);
                let work = receive(inbox, plan.messages, stop, progress, sender_thread);
                if plan.receiver_multi {
                    let task = runtime.spawn(work);
                    runtime.block_on(task).expect("the receiver ends");
                } else {
                    runtime.block_on(work);
                }
            })
        };
        let sender = {
            let (inbox, stop, progress) =
                (Arc::clone(&inbox), Arc::clone(&stop), Arc::clone(&progress));
            let sender_thread = Arc::clone(&sender_thread);
            scope.spawn(move || {
                sender_thread.put(std::thread::current());
                for sent in 1..=plan.messages {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    inbox.ring();
                    while progress.load(Ordering::SeqCst) < sent && !stop.load(Ordering::SeqCst) {
                        std::thread::park_timeout(ACK_WAIT);
                    }
                }
            })
        };
        let mut seen = 0;
        let mut since = Instant::now();
        while !receiver.is_finished() {
            std::thread::sleep(WATCH);
            let now = progress.load(Ordering::SeqCst);
            if now != seen {
                seen = now;
                since = Instant::now();
            } else if since.elapsed() >= STALL {
                if stalled.is_none() {
                    stalled = Some(format!(
                        "the receiving runtime stalled for {STALL:?} after {now} of {} messages: a wake was lost",
                        plan.messages
                    ));
                    stop.store(true, Ordering::SeqCst);
                    inbox.ring();
                }
                nudge(&doorbell);
                since = Instant::now();
            }
        }
        stop.store(true, Ordering::SeqCst);
        sender.thread().unpark();
        sender.join().expect("the sender ends");
        receiver.join().expect("the receiver ends");
        spinning.store(false, Ordering::SeqCst);
        let mut since = Instant::now();
        while !spinner.is_finished() {
            std::thread::sleep(WATCH);
            if since.elapsed() >= STALL {
                if stalled.is_none() {
                    stalled = Some(format!(
                        "the spinning runtime did not shut down within {STALL:?}: a wake was lost"
                    ));
                }
                nudge(&doorbell);
                since = Instant::now();
            }
        }
        spinner.join().expect("the spinner ends");
    });
    drop(door);
    sim::pause_before_drive(Duration::ZERO);
    Threaded {
        received: progress.load(Ordering::SeqCst),
        stalled,
    }
}

pub struct Slept {
    pub naps: u64,
    pub stalled: Option<String>,
}

pub fn sleepers(plan: &Sleepers) -> Slept {
    let progress = Arc::new(AtomicU64::new(0));
    let total = plan.total();
    let mut stalled = None;
    std::thread::scope(|scope| {
        let sleepers: Vec<_> = plan
            .naps
            .iter()
            .map(|naps| {
                let progress = Arc::clone(&progress);
                scope.spawn(move || {
                    runtime(false).block_on(async {
                        for nap in naps {
                            nanocode_executor::sleep(Duration::from_micros(*nap)).await;
                            progress.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                })
            })
            .collect();
        let mut seen = 0;
        let mut since = Instant::now();
        while sleepers.iter().any(|sleeper| !sleeper.is_finished()) {
            std::thread::sleep(WATCH);
            let now = progress.load(Ordering::SeqCst);
            if now != seen {
                seen = now;
                since = Instant::now();
            } else if since.elapsed() >= STALL {
                if stalled.is_none() {
                    stalled = Some(format!(
                        "{} sleeping runtimes stalled for {STALL:?} after {now} of {total} naps: the reactor's driver was released to no waiting thread",
                        plan.naps.len()
                    ));
                }
                runtime(false).block_on(nanocode_executor::sleep(WATCH));
                since = Instant::now();
            }
        }
        for sleeper in sleepers {
            sleeper.join().expect("a sleeper ends");
        }
    });
    Slept {
        naps: progress.load(Ordering::SeqCst),
        stalled,
    }
}
