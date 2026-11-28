use std::{collections::BTreeMap, ops::Range, sync::Arc};

use nanocode::{
    backend::Prompt,
    session_host::{Submission, Submitted},
    turnlog::{OneLog, OneLogOver, Policy},
};
use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_scope::{ClaimHook, ScopeError};
use nanocode_sync::Atom;
use nanocode_turnlog::{
    Compaction, Extension, LogHost, Rider, SCAN_END, ScopeRows, StoredCheckpoint, Write,
};
use serde_json::json;

use crate::rng::Rng;
use crate::threadscope::{Node, World, driven, flag, publish, raw};

const THREAD: &str = "opens";
const OPS: Range<u64> = 8..32;
const CLAIM_READS: u64 = 2;
const PAGE_EVENTS: usize = 4;
const SUBMIT_PPM: u64 = 400_000;
const PAGE_PPM: u64 = 200_000;
const DRAIN_PPM: u64 = 100_000;

type Wrong = Arc<Atom<Option<String>>>;

struct Reads {
    log: Arc<dyn LogHost>,
    credits: Atom<BTreeMap<String, u64>>,
    wrong: Wrong,
}

impl Reads {
    fn claimed(&self, scope: &str) {
        self.credits
            .update(|credits| credits.insert(scope.to_owned(), CLAIM_READS));
    }

    fn wrote(&self, scope: &str) {
        self.credits.update(|credits| credits.remove(scope));
    }

    fn open_ended(&self, scope: &str, read: &str) {
        let spent = self.credits.update(|credits| match credits.get_mut(scope) {
            Some(left) if *left > 0 => {
                *left -= 1;
                true
            }
            _ => false,
        });
        if !spent {
            flag(
                &self.wrong,
                format!(
                    "I14: {read} of {scope} is reached from neither a Scopes::get claim nor a history page"
                ),
            );
        }
    }
}

impl LogHost for Reads {
    fn call_bound(&self) -> Option<std::time::Duration> {
        self.log.call_bound()
    }

    fn read_capacity(&self) -> u32 {
        self.log.read_capacity()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        if to == SCAN_END {
            self.open_ended(scope, &format!("a scan from row {from}"));
        }
        self.log.scan(scope, from, to)
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        for (scope, from) in scans {
            self.open_ended(scope, &format!("a batched scan from row {from}"));
        }
        self.log.scans(scans)
    }

    fn scan_ranges<'a>(
        &'a self,
        scope: &'a str,
        ranges: &'a [Range<u64>],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        if ranges.iter().any(|range| range.end == SCAN_END) {
            self.open_ended(scope, "an open-ended range scan");
        }
        self.log.scan_ranges(scope, ranges)
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.open_ended(scope, "a load");
        self.log.load(scope)
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.wrote(scope);
        self.log.append(scope, rows)
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.wrote(scope);
        self.log.append_compacting(scope, rows, compaction)
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.wrote(scope);
        self.log.extend(scope, extension)
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        write: Write<'a>,
        rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.wrote(scope);
        self.log.carry(scope, write, rider)
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        self.open_ended(scope, "a log checkpoint read");
        self.log.checkpoint(scope)
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        self.wrote(scope);
        self.log.save_checkpoint(scope, checkpoint)
    }
}

fn node(world: &World) -> Node {
    let wrong = Arc::clone(&world.wrong);
    world.node_wrapped("opens0", move |log| {
        let reads = Arc::new(Reads {
            log,
            credits: Atom::default(),
            wrong,
        });
        let claims = Arc::clone(&reads);
        let hook: ClaimHook = Arc::new(move |scope: Arc<str>| {
            claims.claimed(&scope);
            Box::pin(std::future::ready(Ok(())))
        });
        (reads as Arc<dyn LogHost>, Some(hook))
    })
}

fn step(
    rng: &mut Rng,
    world: &World,
    node: &Node,
    over: &OneLogOver,
    turn: &mut u64,
) -> Option<()> {
    if rng.chance(SUBMIT_PPM) {
        let id = format!("p{turn}");
        *turn += 1;
        let submission = Submission {
            request_id: id.clone(),
            prompt: Prompt::new(format!("turn {id}")),
            hand: None,
            metadata: raw(&json!(null)),
        };
        return match driven(world, "a submit", over.submit(&submission))? {
            Ok(Submitted::Accepted { .. }) => Some(()),
            answer => {
                flag(
                    &world.wrong,
                    format!("opens: turn {id} submits as {answer:?}"),
                );
                None
            }
        };
    }
    if rng.chance(PAGE_PPM) {
        let log = match driven(world, "a thread get", node.logs.get(THREAD))? {
            Ok(log) => log,
            Err(error) => {
                flag(
                    &world.wrong,
                    format!("opens: the thread is reached: {error}"),
                );
                return None;
            }
        };
        return match driven(world, "a history page", log.page(0, PAGE_EVENTS, None))? {
            Ok(_) => Some(()),
            Err(error) => {
                flag(
                    &world.wrong,
                    format!("opens: a history page answers {error}"),
                );
                None
            }
        };
    }
    if rng.chance(DRAIN_PPM) {
        return match driven(world, "a drain", node.logs.scopes().drain(THREAD))? {
            Ok(_) | Err(ScopeError::Moved) => Some(()),
            Err(error) => {
                flag(&world.wrong, format!("opens: a drain answers {error}"));
                None
            }
        };
    }
    let id = format!("p{}", rng.below((*turn).max(1)));
    match driven(world, "a turn read", over.turn(&id))? {
        Ok(_) => Some(()),
        Err(error) => {
            flag(&world.wrong, format!("opens: turn {id} reads as {error}"));
            None
        }
    }
}

pub(crate) fn run(rng: &mut Rng, world: &World) {
    let node = node(world);
    publish(world, std::slice::from_ref(&node));
    let logs = Arc::new(node.logs.clone());
    let over = match driven(
        world,
        "a thread over",
        OneLog::over(logs, THREAD, &Policy::default()),
    ) {
        Some(Ok(over)) => over,
        Some(Err(error)) => {
            flag(&world.wrong, format!("opens: the thread opens: {error}"));
            return;
        }
        None => return,
    };
    let mut turn = 0;
    for _ in 0..rng.range(OPS.start, OPS.end) {
        if step(rng, world, &node, &over, &mut turn).is_none() || world.wrong.read(Option::is_some)
        {
            return;
        }
    }
}
