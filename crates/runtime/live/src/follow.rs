use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_live_fold::ViewKey;
use nanocode_sync::{
    Atom, Notify,
    future::{join, join_all},
};

use crate::hub::Follow;
use sim::{Mutation, mutated};

pub mod sim;

pub struct Tail<R> {
    pub epoch: u64,
    pub records: Vec<(u64, R)>,
}

pub trait Source: Send + Sync + 'static {
    type Record: Send + Sync + 'static;

    fn read(
        &self,
        partition: u32,
        after: u64,
        limit: usize,
    ) -> StoreFuture<'_, Result<Tail<Self::Record>, StoreError>>;

    fn partition(&self, scope: &str) -> u32;
}

pub trait Hold: Send + Sync + 'static {
    fn hold(&self, partition: u32);

    fn release(&self, partition: u32);
}

pub trait Feed: Send + Sync {
    type Record;

    fn commit(&self, partition: u32, epoch: u64, batch: Vec<(u64, Self::Record)>);

    fn backfilled(
        &self,
        key: ViewKey,
        epoch: u64,
        records: Vec<(u64, Self::Record)>,
        exhausted: bool,
    );
}

#[derive(Clone)]
enum Command {
    Follow(u32, u64),
    Unfollow(u32),
    Hold(u32),
    Backfill(ViewKey, u32, u64),
    Heard(u32),
    Everything,
}

#[derive(Default)]
pub struct Stats {
    pub reads: AtomicU64,
    pub empty: AtomicU64,
    pub batches: AtomicU64,
    pub records: AtomicU64,
    pub backfills: AtomicU64,
    pub errors: AtomicU64,
}

pub struct Tailer<S> {
    source: Arc<S>,
    page: NonZeroUsize,
    pending: Atom<Vec<Command>>,
    wake: Notify,
    stats: Stats,
}

struct Part {
    pos: u64,
    epoch: u64,
    behind: bool,
    holds: usize,
}

struct Fill {
    key: ViewKey,
    partition: u32,
    after: u64,
}

impl<S: Source> Tailer<S> {
    pub fn new(source: Arc<S>, page: NonZeroUsize) -> Arc<Self> {
        Arc::new(Self {
            source,
            page,
            pending: Atom::new(Vec::new()),
            wake: Notify::new(),
            stats: Stats::default(),
        })
    }

    pub const fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn follow(&self, partition: u32, after: u64) {
        self.push(Command::Follow(partition, after));
    }

    pub fn unfollow(&self, partition: u32) {
        self.push(Command::Unfollow(partition));
    }

    pub fn backfill(&self, key: ViewKey, partition: u32, after: u64) {
        self.push(Command::Backfill(key, partition, after));
    }

    pub fn heard(&self, scope: &str) {
        if mutated(Mutation::HeardIgnored) {
            return;
        }
        let partition = self.source.partition(scope);
        let fresh = self.pending.update(|pending| {
            let fresh = !pending
                .iter()
                .any(|command| matches!(command, Command::Heard(part) if *part == partition));
            if fresh {
                pending.push(Command::Heard(partition));
            }
            fresh
        });
        if fresh {
            self.wake.notify_one();
        }
    }

    pub fn reconnected(&self) {
        self.push(Command::Everything);
    }

    fn push(&self, command: Command) {
        self.pending.update(|pending| pending.push(command.clone()));
        self.wake.notify_one();
    }

    pub async fn run<F: Feed<Record = S::Record>>(&self, feed: &F) -> StoreError {
        let mut parts = BTreeMap::<u32, Part>::new();
        let mut last = BTreeMap::<u32, u64>::new();
        let mut fills = Vec::<Fill>::new();
        let mut stalled = Vec::<Fill>::new();
        loop {
            let commands = self.pending.update(std::mem::take);
            if !commands.is_empty() {
                fills.append(&mut stalled);
            }
            for command in commands {
                match command {
                    Command::Follow(partition, after) => join_part(&mut parts, partition, after),
                    Command::Hold(partition) => {
                        let after = last.get(&partition).copied().unwrap_or(0);
                        join_part(&mut parts, partition, after);
                    }
                    Command::Unfollow(partition) => {
                        if !mutated(Mutation::UnfollowIgnored) {
                            leave_part(&mut parts, &mut last, partition);
                        }
                    }
                    Command::Backfill(key, partition, after) => fills.push(Fill {
                        key,
                        partition,
                        after,
                    }),
                    Command::Heard(partition) => {
                        if let Some(part) = parts.get_mut(&partition) {
                            part.behind = true;
                        }
                    }
                    Command::Everything => {
                        for part in parts.values_mut() {
                            part.behind = true;
                        }
                    }
                }
            }
            let behind = parts
                .iter()
                .filter(|(_, part)| part.behind)
                .map(|(partition, part)| (*partition, part.pos))
                .collect::<Vec<_>>();
            if behind.is_empty() && fills.is_empty() {
                self.wake.notified().await;
                continue;
            }
            let page = self.page.get();
            let source = &self.source;
            let (tails, filled) = join(
                join_all(
                    behind
                        .iter()
                        .map(|(partition, pos)| source.read(*partition, *pos, page)),
                ),
                join_all(
                    fills
                        .iter()
                        .map(|fill| source.read(fill.partition, fill.after, page)),
                ),
            )
            .await;
            for ((partition, after), tail) in behind.into_iter().zip(tails) {
                self.stats.reads.fetch_add(1, Ordering::Relaxed);
                let Some(part) = parts.get_mut(&partition) else {
                    continue;
                };
                let Ok(mut tail) = tail else {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    part.behind = false;
                    continue;
                };
                if let Some(error) = disorder(partition, after, &tail.records) {
                    return error;
                }
                part.behind = tail.records.len() == page;
                part.epoch = part.epoch.max(tail.epoch);
                let Some((last, _)) = tail.records.last() else {
                    self.stats.empty.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                if !mutated(Mutation::PosKept) {
                    part.pos = *last;
                }
                if mutated(Mutation::FirstSkipped) {
                    tail.records.remove(0);
                }
                self.stats.batches.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .records
                    .fetch_add(tail.records.len() as u64, Ordering::Relaxed);
                feed.commit(partition, part.epoch, tail.records);
            }
            for (fill, tail) in std::mem::take(&mut fills).into_iter().zip(filled) {
                self.stats.backfills.fetch_add(1, Ordering::Relaxed);
                let Ok(tail) = tail else {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    stalled.push(fill);
                    continue;
                };
                if let Some(error) = disorder(fill.partition, fill.after, &tail.records) {
                    return error;
                }
                let exhausted = tail.records.len() < page;
                feed.backfilled(fill.key, tail.epoch, tail.records, exhausted);
            }
        }
    }
}

fn join_part(parts: &mut BTreeMap<u32, Part>, partition: u32, after: u64) {
    let part = parts.entry(partition).or_insert(Part {
        pos: after,
        epoch: 0,
        behind: true,
        holds: 0,
    });
    part.holds += 1;
    if after < part.pos {
        part.pos = after;
        part.behind = true;
    }
}

fn leave_part(parts: &mut BTreeMap<u32, Part>, last: &mut BTreeMap<u32, u64>, partition: u32) {
    let Some(part) = parts.get_mut(&partition) else {
        return;
    };
    assert!(part.holds > 0);
    part.holds = part.holds.saturating_sub(1);
    if part.holds == 0 {
        last.insert(partition, part.pos);
        parts.remove(&partition);
    }
}

fn disorder<R>(partition: u32, after: u64, records: &[(u64, R)]) -> Option<StoreError> {
    let mut last = after;
    for (pos, _) in records {
        if *pos <= last {
            return Some(StoreError::Backend(format!(
                "partition {partition} read after {after} answered pos {pos} after {last}"
            )));
        }
        last = *pos;
    }
    None
}

impl<S: Source> Follow for Tailer<S> {
    fn follow(&self, partition: u32, after: u64) {
        Self::follow(self, partition, after);
    }

    fn unfollow(&self, partition: u32) {
        Self::unfollow(self, partition);
    }

    fn backfill(&self, key: ViewKey, partition: u32, after: u64) {
        Self::backfill(self, key, partition, after);
    }
}

impl<S: Source> Hold for Tailer<S> {
    fn hold(&self, partition: u32) {
        self.push(Command::Hold(partition));
    }

    fn release(&self, partition: u32) {
        self.push(Command::Unfollow(partition));
    }
}
