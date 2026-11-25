use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use nanocode::turnlog::{OneLog, Policy, Statements, WakeRider, WakeSummary};
use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::FutureExt;
use nanocode_turnlog::{
    Compaction, Extension, LogHost, Point as LogPoint, Record as LogRecord, Rider, TurnLog, Write,
};
use serde_json::value::to_raw_value;

use crate::{
    disk::Disk,
    executor::{Clock, Ended, Executor, Task},
    rng::Rng,
    turnlog::SimHost,
};

type EndedAt = Cell<Option<(u64, bool)>>;

const STALL_BOUND: Duration = Duration::from_secs(30);
const WRITERS: u64 = 2;
const READS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Stall {
    Answering,
    Unanswered,
    AnswerLost,
}

struct StallHost {
    host: SimHost,
    stall: AtomicU8,
}

impl StallHost {
    fn stall(&self) -> Stall {
        match self.stall.load(Ordering::SeqCst) {
            stall if stall == Stall::Unanswered as u8 => Stall::Unanswered,
            stall if stall == Stall::AnswerLost as u8 => Stall::AnswerLost,
            _ => Stall::Answering,
        }
    }

    fn stalls<'a, T: 'a>(
        &'a self,
        write: impl FnOnce() -> StoreFuture<'a, Result<T, StoreError>>,
    ) -> StoreFuture<'a, Result<T, StoreError>> {
        match self.stall() {
            Stall::Answering => write(),
            Stall::Unanswered => Box::pin(std::future::pending()),
            Stall::AnswerLost => {
                drop(write());
                Box::pin(std::future::pending())
            }
        }
    }

    fn stalls_some<'a, T: 'a>(
        &'a self,
        write: impl FnOnce() -> Option<StoreFuture<'a, Result<T, StoreError>>>,
    ) -> Option<StoreFuture<'a, Result<T, StoreError>>> {
        match self.stall() {
            Stall::Answering => write(),
            Stall::Unanswered => Some(Box::pin(std::future::pending())),
            Stall::AnswerLost => {
                drop(write()?);
                Some(Box::pin(std::future::pending()))
            }
        }
    }
}

impl LogHost for StallHost {
    fn call_bound(&self) -> Option<std::time::Duration> {
        Some(STALL_BOUND)
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.host.scan(scope, from, to)
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.host.load(scope)
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.stalls(|| self.host.append(scope, rows))
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.stalls(|| self.host.append_compacting(scope, rows, compaction))
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.stalls_some(|| self.host.extend(scope, extension))
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        write: Write<'a>,
        rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.stalls_some(|| self.host.carry(scope, write, rider))
    }
}

fn ready<T>(future: impl std::future::Future<Output = T>) -> T {
    future
        .now_or_never()
        .expect("the simulated turn log host answers every call at once")
}

fn marker_key(writer: u64) -> String {
    format!("stalled writer {writer}")
}

fn marker(writer: u64) -> LogRecord {
    LogRecord::Host {
        key: marker_key(writer),
        value: to_raw_value(&writer).expect("a writer number encodes as JSON"),
    }
}

pub fn bounded(scope: &str, stored: &[(u64, String)], seq: u64) {
    for stall in [Stall::Unanswered, Stall::AnswerLost] {
        let clock = Clock::new();
        crate::host::entered(seq, &clock, || probe(scope, stored, seq, stall, &clock));
    }
    let clock = Clock::new();
    crate::host::entered(seq, &clock, || held(scope, stored, seq, &clock));
    let clock = Clock::new();
    crate::host::entered(seq, &clock, || indexed(scope, seq, &clock));
}

struct StalledIndex;

impl WakeRider for StalledIndex {
    fn host(&self) -> usize {
        usize::MAX
    }

    fn statements(&self, _scope: &str, _summary: &WakeSummary) -> Statements {
        Vec::new()
    }

    fn prepare(&self) -> StoreFuture<'_, Result<(), StoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn write<'a>(
        &'a self,
        _scope: &'a str,
        _summary: &'a WakeSummary,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(std::future::pending())
    }
}

fn indexed(scope: &str, seq: u64, clock: &Arc<Clock>) {
    let (host, _) = opened(scope, &[]);
    let host: Arc<dyn LogHost> = host;
    let log = ready(OneLog::open(host, scope, Policy::default())).unwrap_or_else(|error| {
        panic!("{scope} does not open as a OneLog for a stall probe: {error}")
    });
    let indexed = Rc::new(Cell::new(None));
    let read = Rc::new(Cell::new(None));
    let tasks = vec![
        {
            let (log, clock, indexed) = (&log, Arc::clone(clock), Rc::clone(&indexed));
            Box::pin(async move {
                let written = log.index_wakes(Arc::new(StalledIndex)).await;
                indexed.set(Some((clock.now_ns(), written.is_ok())));
            }) as Task<'_>
        },
        {
            let (log, clock, read) = (&log, Arc::clone(clock), Rc::clone(&read));
            Box::pin(async move {
                nanocode_platform::sleep(STALL_BOUND / 2).await;
                let due = log.next_due().await;
                let stats = log.stats().await;
                read.set(Some((
                    clock.now_ns(),
                    due.is_none() && stats.admissions == 0,
                )));
            }) as Task<'_>
        },
    ];
    let run = Executor::new(Arc::clone(clock), Rng::derive(seq, "stall index")).run(tasks, None);
    assert!(
        run == Ended::Finished,
        "{scope} reads wait on a stalled wake index write past every bound after commit {seq}: {run:?}"
    );
    let bound_ns = u64::try_from(STALL_BOUND.as_nanos()).expect("the stall bound fits in u64");
    let (indexed_ns, written) = indexed
        .get()
        .unwrap_or_else(|| panic!("{scope} index_wakes over a stalled index never ended"));
    assert!(
        !written,
        "{scope} index_wakes reports a write its stalled index never answered"
    );
    assert!(
        indexed_ns <= bound_ns,
        "{scope} index_wakes over a stalled index ended at {indexed_ns} ns, past the bound of {bound_ns} ns"
    );
    let (read_ns, empty) = read
        .get()
        .unwrap_or_else(|| panic!("{scope} reads behind a stalled index never ended"));
    assert!(
        read_ns <= bound_ns,
        "{scope} reads behind a stalled wake index ended at {read_ns} ns, past the bound of {bound_ns} ns"
    );
    assert!(
        empty,
        "{scope} a fresh log reads wakes or appends it never wrote"
    );
}

fn opened(scope: &str, stored: &[(u64, String)]) -> (Arc<StallHost>, TurnLog) {
    let disk = Disk::new();
    disk.with(|rows| rows.insert(scope, stored));
    let host = Arc::new(StallHost {
        host: SimHost::new(Arc::clone(&disk)),
        stall: AtomicU8::new(Stall::Answering as u8),
    });
    let opened: Arc<dyn LogHost> = host.clone();
    let log = ready(TurnLog::open(opened, scope))
        .unwrap_or_else(|error| panic!("{scope} does not reopen before a stall probe: {error}"));
    (host, log)
}

async fn read(log: &TurnLog, scope: &str, read: usize) -> Result<(), StoreError> {
    let owners = [scope.to_owned()];
    match read {
        0 => log.load_owner(scope).await,
        1 => log.read_owner(scope, |_| ()).await,
        2 => log.read_owners(&owners, |_| ()).await,
        3 => log.history(0, |_| true).await,
        _ => log.load_index().await,
    }
}

fn held(scope: &str, stored: &[(u64, String)], seq: u64, clock: &Arc<Clock>) {
    let (_host, log) = opened(scope, stored);
    let lease = ready(log.lease());
    let ended: Rc<Vec<EndedAt>> = Rc::new((0..READS).map(|_| Cell::new(None)).collect());
    let mut tasks = (0..READS)
        .map(|index| {
            let (log, clock, ended) = (&log, Arc::clone(clock), Rc::clone(&ended));
            Box::pin(async move {
                let answered = read(log, scope, index).await;
                ended[index].set(Some((clock.now_ns(), answered.is_ok())));
            }) as Task<'_>
        })
        .collect::<Vec<_>>();
    tasks.push(Box::pin(async move {
        nanocode_platform::sleep(STALL_BOUND * 2).await;
        drop(lease);
    }));
    let run = Executor::new(Arc::clone(clock), Rng::derive(seq, "stall reads")).run(tasks, None);
    assert!(
        run == Ended::Finished,
        "{scope} reads wait on a stalled lease holder past every bound after commit {seq}: {run:?}"
    );
    let bound_ns = u64::try_from(STALL_BOUND.as_nanos()).expect("the stall bound fits in u64");
    for (index, end) in ended.iter().enumerate() {
        let (at_ns, answered) = end
            .get()
            .unwrap_or_else(|| panic!("{scope} read {index} behind a stalled lease never ended"));
        assert!(
            at_ns <= bound_ns,
            "{scope} read {index} behind a stalled lease ended at {at_ns} ns, past the bound of {bound_ns} ns"
        );
        assert!(
            !answered,
            "{scope} read {index} answered while a stalled lease still held the writer"
        );
    }
}

fn probe(scope: &str, stored: &[(u64, String)], seq: u64, stall: Stall, clock: &Arc<Clock>) {
    let (host, log) = opened(scope, stored);
    host.stall.store(stall as u8, Ordering::SeqCst);
    let ended: Rc<Vec<EndedAt>> = Rc::new((0..WRITERS).map(|_| Cell::new(None)).collect());
    let tasks = (0..WRITERS)
        .map(|writer| {
            let (log, clock, ended) = (&log, Arc::clone(clock), Rc::clone(&ended));
            Box::pin(async move {
                let committed = async {
                    let mut lease = log.lease_within().await?;
                    lease.push(marker(writer))?;
                    lease.commit(LogPoint::Progress).await
                };
                let committed = committed.await;
                let slot = usize::try_from(writer).expect("a writer index fits in usize");
                ended[slot].set(Some((clock.now_ns(), committed.is_ok())));
            }) as Task<'_>
        })
        .collect::<Vec<_>>();
    let run = Executor::new(Arc::clone(clock), Rng::derive(seq, "stall")).run(tasks, None);
    let bound_ns = u64::try_from(STALL_BOUND.as_nanos()).expect("the stall bound fits in u64");
    assert!(
        run == Ended::Finished,
        "{scope} writers wait on a {stall:?} host past every bound after commit {seq}: {run:?}"
    );
    host.stall.store(Stall::Answering as u8, Ordering::SeqCst);
    drop(log);
    let reopened: Arc<dyn LogHost> = host;
    let log = ready(TurnLog::open(reopened, scope))
        .unwrap_or_else(|error| panic!("{scope} does not reopen after a {stall:?} stall: {error}"));
    for (writer, end) in (0..WRITERS).zip(ended.iter()) {
        let (at_ns, committed) = end
            .get()
            .unwrap_or_else(|| panic!("{scope} {stall:?} stall writer {writer} never ended"));
        assert!(
            at_ns <= WRITERS * bound_ns,
            "{scope} {stall:?} stall writer {writer} ended at {at_ns} ns, past {WRITERS} bounds of {bound_ns} ns"
        );
        let landed = ready(log.read(|fold| fold.host(&marker_key(writer)).is_some()));
        assert!(
            !committed || landed,
            "{scope} {stall:?} stall writer {writer} reports a commit its reopened log does not hold"
        );
        assert!(
            stall != Stall::Unanswered || !committed,
            "{scope} stall writer {writer} reports a commit the unanswered host never wrote"
        );
    }
    let mut lease = ready(log.lease());
    lease.push(marker(WRITERS)).unwrap_or_else(|error| {
        panic!("{scope} refuses a record after a {stall:?} stall: {error}")
    });
    ready(lease.commit(LogPoint::Progress))
        .unwrap_or_else(|error| panic!("{scope} does not commit after a {stall:?} stall: {error}"));
}
