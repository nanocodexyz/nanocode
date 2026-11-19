use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use nanocode_subagents::{BoardCoordination, BoardStats, Coordination};
use nanocode_turnlog::{LogHost, MemoryLog};

static ENABLED: AtomicBool = AtomicBool::new(false);
static SEED: Mutex<Option<Seed>> = Mutex::new(None);
static TOTALS: Mutex<Option<(u64, BoardStats)>> = Mutex::new(None);

struct Seed {
    host: Arc<MemoryLog>,
    planes: Vec<Arc<BoardCoordination>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn begin() {
    if enabled() {
        *lock(&SEED) = Some(Seed {
            host: Arc::new(MemoryLog::new()),
            planes: Vec::new(),
        });
    }
}

pub fn plane() -> Option<Arc<dyn Coordination>> {
    if !enabled() {
        return None;
    }
    let mut seed = lock(&SEED);
    let seed = seed.get_or_insert_with(|| Seed {
        host: Arc::new(MemoryLog::new()),
        planes: Vec::new(),
    });
    let host: Arc<dyn LogHost> = seed.host.clone();
    let plane = BoardCoordination::new(host, "subagents");
    seed.planes.push(Arc::clone(&plane));
    Some(plane)
}

pub fn finish() -> Option<String> {
    if !enabled() {
        return None;
    }
    let seed = lock(&SEED).take()?;
    let mut total = BoardStats::default();
    for plane in &seed.planes {
        match nanocode_sync::block_on(plane.audit()) {
            Ok(stats) => total.add(&stats),
            Err(error) => return Some(format!("board: {error}")),
        }
    }
    if total.deferred > 0 {
        return Some(format!("board: {} operations stayed pending", total.deferred));
    }
    let mut totals = lock(&TOTALS);
    let totals = totals.get_or_insert_with(|| (0, BoardStats::default()));
    totals.0 += 1;
    totals.1.add(&total);
    None
}

pub fn report() {
    if !enabled() {
        return;
    }
    if let Some((seeds, stats)) = lock(&TOTALS).as_ref() {
        println!("BOARD seeds={seeds} {stats:?}");
    } else {
        println!("BOARD seeds=0");
    }
}
