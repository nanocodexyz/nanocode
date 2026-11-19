use std::{
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    UncountedPolls,
    UncheckedDrive,
    StaleWaiters,
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);
static PAUSE_NS: AtomicU64 = AtomicU64::new(0);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub fn pause_before_drive(pause: Duration) {
    let nanos = u64::try_from(pause.as_nanos()).expect("a pause fits in u64 nanoseconds");
    PAUSE_NS.store(nanos, Ordering::SeqCst);
}

pub(crate) fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}

pub(crate) fn pause() {
    let nanos = PAUSE_NS.load(Ordering::Relaxed);
    if nanos > 0 {
        std::thread::sleep(Duration::from_nanos(nanos));
    }
}
