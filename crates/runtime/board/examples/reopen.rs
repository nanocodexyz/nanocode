//! How long Board::open takes over a log of 10k, 50k and 100k events: a full replay, a reopen
//! from a checkpoint at the head, and one from a checkpoint CHECKPOINT_EVERY events behind it.
//!
//! The load is the one nanocode-ts's bench/board-do.ts serves: swarms of a root and 20 children,
//! each task posted, claimed, renewed and finished, about 2% of them steered. The log is a
//! MemoryLog, so the times are the fold's and the checkpoint's own, without a store's read.
//!
//! cargo run --release -p nanocode-board --example reopen -- 10000 50000 100000

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use nanocode_board::{
    Board, NewTask, Op, Outcome, Phase, TaskId,
    access::{Identity, Verifier},
    checkpoint::{self, CHECKPOINT_EVERY, Opened},
    steer::SteerOp,
};
use nanocode_turnlog::{LogHost, MemoryLog};

const SCOPE: &str = "bench";
const LEASE_MS: u64 = 60_000;

struct Tokens;

impl Verifier for Tokens {
    fn verify(&self, credential: &str) -> Option<String> {
        credential.strip_prefix("token:").map(str::to_owned)
    }
}

fn identity() -> Identity {
    Identity::verify(&Tokens, "token:user:bench").expect("the bench token verifies")
}

fn task(id: String, parent: Option<&TaskId>) -> NewTask {
    NewTask {
        id: TaskId(id),
        parent: parent.cloned(),
        spec: "do the work".to_owned(),
        labels: BTreeMap::new(),
        needs: Vec::new(),
        terms: nanocode_board::tasks::Terms::default(),
    }
}

async fn post_swarm(board: &mut Board, at_ms: u64, swarm: u64) -> Vec<TaskId> {
    let root = TaskId(format!("s{swarm}"));
    let mut ids = vec![root.clone()];
    board
        .submit(
            at_ms,
            Op::Post {
                task: task(root.0.clone(), None),
            },
        )
        .await
        .expect("a root posts");
    for child in 0..20 {
        let new = task(format!("s{swarm}.c{child}"), Some(&root));
        ids.push(new.id.clone());
        board
            .submit(at_ms, Op::Post { task: new })
            .await
            .expect("a child posts");
    }
    ids
}

async fn work(board: &mut Board, at_ms: u64, swarm: u64, index: usize, id: &TaskId) {
    board
        .submit(
            at_ms,
            Op::Claim {
                task: id.clone(),
                lease_ms: LEASE_MS,
                fence: None,
                cap: None,
            },
        )
        .await
        .expect("a claim lands");
    let Some(Phase::Claimed { claim, .. }) = board.task(id).map(|task| task.phase.clone()) else {
        panic!("{id} is claimed");
    };
    board
        .submit(
            at_ms,
            Op::Renew {
                task: id.clone(),
                claim: claim.clone(),
                lease_ms: LEASE_MS,
            },
        )
        .await
        .expect("a renew lands");
    if (swarm * 21 + index as u64).is_multiple_of(50) {
        let send = Op::Steer(SteerOp::Send {
            task: id.clone(),
            claim: Some(claim.clone()),
            body: "change course".to_owned(),
        });
        board.submit(at_ms, send).await.expect("a steer lands");
        // The worker heeds it before it finishes, as a finish over an unheeded steer
        // is refused.
        let heed = Op::Steer(SteerOp::Heed {
            task: id.clone(),
            claim: claim.clone(),
            upto: board.fold().next() - 1,
        });
        board.submit(at_ms, heed).await.expect("a heed lands");
    }
    board
        .submit(
            at_ms,
            Op::Finish {
                task: id.clone(),
                claim,
                outcome: Outcome::Done("ok".to_owned()),
            },
        )
        .await
        .expect("a finish lands");
}

/// Writes swarms until the log holds at least `events` rows, saving a checkpoint once it is
/// `behind` events short of that, or none when `behind` is None.
async fn fill(board: &mut Board, events: u64, behind: Option<u64>) {
    let mut at_ms = 1;
    let mut swarm = 0_u64;
    let mut saved = false;
    while board.fold().next() < events {
        let ids = post_swarm(board, at_ms, swarm).await;
        for (index, id) in ids.iter().enumerate().rev() {
            at_ms += 1;
            work(board, at_ms, swarm, index, id).await;
        }
        if let Some(behind) = behind
            && !saved
            && board.fold().next() + behind >= events
        {
            board.checkpoint().await.expect("a checkpoint saves");
            saved = true;
        }
        swarm += 1;
    }
}

async fn open(host: &Arc<dyn LogHost>) -> (Duration, Opened, u64) {
    let started = Instant::now();
    let board = Board::open(Arc::clone(host), SCOPE, identity())
        .await
        .expect("the board opens");
    (
        started.elapsed(),
        board.opened().clone(),
        board.fold().next(),
    )
}

async fn median(host: &Arc<dyn LogHost>) -> (Duration, Opened, u64) {
    let mut runs = Vec::new();
    for _ in 0..5 {
        runs.push(open(host).await);
    }
    runs.sort_by_key(|run| run.0);
    runs.swap_remove(2)
}

fn main() {
    let sizes: Vec<u64> = std::env::args()
        .skip(1)
        .map(|size| size.parse().expect("each argument is an event count"))
        .collect();
    let sizes = if sizes.is_empty() {
        vec![10_000, 50_000, 100_000]
    } else {
        sizes
    };
    checkpoint::checkpoint_every(0);
    nanocode_sync::block_on(async {
        for events in sizes {
            for behind in [None, Some(0), Some(CHECKPOINT_EVERY)] {
                let log: Arc<dyn LogHost> = Arc::new(MemoryLog::new());
                let mut board = Board::open(Arc::clone(&log), SCOPE, identity())
                    .await
                    .expect("a fresh board opens");
                // The first writer charters the board and owns it.
                board
                    .submit(0, Op::Charter {})
                    .await
                    .expect("the board charters");
                fill(&mut board, events, behind).await;
                let saved = log
                    .checkpoint(SCOPE)
                    .expect("a memory log keeps checkpoints")
                    .await
                    .expect("the checkpoint reads");
                let bytes = saved.as_ref().map_or(0, |saved| saved.body.len());
                let (took, opened, head) = median(&log).await;
                let mode = match behind {
                    None => "replay".to_owned(),
                    Some(behind) => format!("checkpoint-{behind}-behind"),
                };
                // Where a reopen's time goes: a bare fold of every row, and for a checkpoint
                // its base64 decode and restore.
                let rows = log.load(SCOPE).await.expect("the log loads");
                let started = Instant::now();
                let mut fold = nanocode_board::Fold::new();
                for (seq, body) in &rows {
                    fold.apply(*seq, body).expect("a row folds");
                }
                let fold_ms = started.elapsed().as_secs_f64() * 1000.0;
                let (decode_ms, restore_ms) = saved.as_ref().map_or((0.0, 0.0), |saved| {
                    let started = Instant::now();
                    let (_, encoded) = saved.body.split_once(':').expect("a row hash");
                    let snapshot = nanocode_sys::base64::STANDARD_NO_PAD
                        .decode(encoded)
                        .expect("the checkpoint decodes");
                    let decoded = started.elapsed().as_secs_f64() * 1000.0;
                    let started = Instant::now();
                    nanocode_board::Fold::restore(&snapshot).expect("the snapshot restores");
                    (decoded, started.elapsed().as_secs_f64() * 1000.0)
                });
                println!(
                    "REOPEN events={head} mode={mode} open_ms={:.1} fold_all_ms={fold_ms:.1} decode_ms={decode_ms:.1} restore_ms={restore_ms:.1} checkpoint_bytes={bytes} opened={opened:?}",
                    took.as_secs_f64() * 1000.0
                );
            }
        }
    });
}
