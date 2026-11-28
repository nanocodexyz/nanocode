use std::{collections::BTreeSet, ops::Range, sync::Arc};

use nanocode::{
    backend::Prompt,
    session_host::{Submission, Submitted, TurnState},
    turnlog::{OneLog, OneLogStore, Policy},
};
use nanocode_durability_core::{
    EncodedPayload, OwnerId, OwnerToken, StateStore, StoreError, StoreRecord, StoredState,
};
use nanocode_turnlog::{LogHost, MemoryLog, Record, Row, rows};
use serde_json::{Map, Value, json};

use crate::rng::Rng;
use crate::threadscope::{World, driven, flag, raw};

const SCOPE: &str = "tail";
const STATE: &str = "agent";
const BODY_BYTES: Range<u64> = 256..2_049;
const MESSAGE_BYTES: Range<u64> = 128..1_025;
const BOUND_TURNS: Range<u64> = 2..9;
const TURNS_PER_BOUND: Range<u64> = 4..7;
const REOPEN_EVERY: Range<u64> = 1..9;
const PAGE_MESSAGES: Range<u64> = 4..33;
const RETAIN_FINISHED: usize = 16;
const RETIRED_MAX: usize = 1_000_000;
const SNAPSHOT_RATIO: usize = 2;
const SNAPSHOTS_SLACK: u64 = 16;

#[derive(Clone, Copy)]
struct Shape {
    body: usize,
    message: usize,
    page: usize,
}

struct Opened {
    log: Arc<OneLog>,
    store: OneLogStore,
    owner: OwnerToken,
}

type Resumed = (StoredState, Vec<(String, Option<String>)>, (usize, usize));

fn prose(at: usize, what: &str, bytes: usize) -> String {
    let mut text = String::with_capacity(bytes);
    let mut line = 0;
    while text.len() < bytes {
        text.push_str(&format!(
            "{what} {at}, line {line}: the model answers as it always does. "
        ));
        line += 1;
    }
    text
}

fn hash(kind: u64, at: usize) -> String {
    format!("b3:{:064x}", (kind << 48) + at as u64)
}

fn inline(value: &Value) -> Arc<str> {
    Arc::from(format!("={value}"))
}

fn records(shape: Shape, at: usize) -> Vec<StoreRecord> {
    let page = at / shape.page;
    let page_keys: Vec<String> = (page * shape.page..=at).map(|key| hash(1, key)).collect();
    let pages: Vec<String> = (0..page)
        .map(|sealed| hash(2, sealed * shape.page + shape.page - 1))
        .chain(std::iter::once(hash(2, at)))
        .collect();
    vec![
        StoreRecord {
            key: hash(1, at),
            value: inline(
                &json!({"role": "assistant", "text": prose(at, "message", shape.message)}),
            ),
        },
        StoreRecord {
            key: hash(2, at),
            value: inline(&json!({"messages": page_keys})),
        },
        StoreRecord {
            key: hash(3, at),
            value: inline(&json!({"pages": pages})),
        },
    ]
}

fn policy(bound: Option<usize>) -> Policy {
    Policy {
        restarts: u64::MAX,
        retain_finished: Some(RETAIN_FINISHED),
        retired_max: Some(RETIRED_MAX),
        snapshot_ratio: SNAPSHOT_RATIO,
        max_tail_bytes: bound,
        ..Policy::default()
    }
}

async fn open(host: &Arc<MemoryLog>, bound: Option<usize>) -> Result<Opened, StoreError> {
    let log = nanocode::turnlog::thread_scope_sim::open(
        Arc::clone(host) as Arc<dyn LogHost>,
        SCOPE,
        policy(bound),
    )
    .await?;
    let mut store = log.store();
    let owner = store.acquire(STATE, OwnerId::new()).await?.owner;
    Ok(Opened { log, store, owner })
}

async fn turn(opened: &mut Opened, shape: Shape, at: usize) -> Result<u64, String> {
    let before = opened.log.stats().await.snapshots;
    let request = format!("turn-{at}");
    let submission = Submission {
        request_id: request.clone(),
        prompt: Prompt::new(prose(at, "question", shape.body)),
        hand: None,
        metadata: raw(&json!({ "seq": at })),
    };
    match opened.log.submit(&submission).await {
        Ok(Submitted::Accepted { .. }) => {}
        answer => return Err(format!("turn {at} submits as {answer:?}")),
    }
    match opened.log.start(&request).await {
        Ok(Some(_)) => {}
        answer => return Err(format!("turn {at} starts as {answer:?}")),
    }
    let store = &mut opened.store;
    store
        .collect_unreachable(STATE)
        .await
        .map_err(|error| format!("turn {at} marks: {error}"))?;
    if at > 0 {
        let held: Vec<String> = (0..at)
            .map(|key| hash(1, key))
            .chain((0..at).map(|key| hash(2, key)))
            .chain(std::iter::once(hash(3, at - 1)))
            .collect();
        store
            .read_records(STATE, &held)
            .await
            .map_err(|error| format!("turn {at} reads: {error}"))?;
    }
    let payload = json!({"turn": at, "history": hash(3, at)}).to_string();
    store
        .replace(
            STATE,
            &opened.owner,
            at as u64,
            &payload,
            &records(shape, at),
        )
        .await
        .map_err(|error| format!("turn {at} replaces: {error}"))?;
    let result = Map::from_iter([(
        "text".to_owned(),
        Value::String(prose(at, "answer", shape.body)),
    )]);
    match opened
        .log
        .finish(&request, TurnState::Completed, &result)
        .await
    {
        Ok(Some(_)) => {}
        answer => return Err(format!("turn {at} finishes as {answer:?}")),
    }
    Ok(opened.log.stats().await.snapshots - before)
}

struct Tail {
    rows: usize,
    plain: usize,
    row_max: usize,
}

async fn tail(host: &Arc<MemoryLog>) -> Result<Tail, StoreError> {
    let stored = host.load(SCOPE).await?;
    let decoded = rows(&stored)?;
    let last = decoded.iter().rposition(|row| row.snapshot);
    let after = last.map_or(&decoded[..], |at| &decoded[at + 1..]);
    let mut placed = BTreeSet::new();
    for row in after {
        for record in row.records()? {
            if let Record::Placed { row, rows, .. } = record {
                placed.extend(row..row + rows);
            }
        }
    }
    let counted: Vec<&Row> = after
        .iter()
        .filter(|row| !placed.contains(&row.seq))
        .collect();
    Ok(Tail {
        rows: counted.len(),
        plain: counted.iter().map(|row| row.plain.len()).sum(),
        row_max: counted.iter().map(|row| row.plain.len()).max().unwrap_or(0),
    })
}

async fn resumed(host: &Arc<MemoryLog>, bound: Option<usize>) -> Result<Resumed, StoreError> {
    let log = nanocode::turnlog::thread_scope_sim::open(
        Arc::clone(host) as Arc<dyn LogHost>,
        SCOPE,
        policy(bound),
    )
    .await?;
    let mut store = log.store();
    let state = store.acquire(STATE, OwnerId::new()).await?.state;
    let mut queue: Vec<String> = state
        .payload
        .as_deref()
        .map(|payload| {
            EncodedPayload::references(payload)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut keys = Vec::new();
    let mut reached = Vec::new();
    while let Some(key) = queue.pop() {
        if keys.contains(&key) {
            continue;
        }
        keys.push(key.clone());
        let value = store.read_record(STATE, &key).await?;
        if let Some(value) = &value {
            queue.extend(EncodedPayload::references(value).map(str::to_owned));
        }
        reached.push((key, value));
    }
    reached.sort();
    let turns = log
        .read(|fold| (fold.retired_turns(), fold.turns().len()))
        .await;
    Ok((state, reached, turns))
}

fn bounded(world: &World, host: &Arc<MemoryLog>, bound: usize, at: usize) -> bool {
    let Some(read) = driven(world, "a tail read", tail(host)) else {
        return false;
    };
    match read {
        Ok(Tail {
            rows,
            plain,
            row_max,
        }) if plain >= bound + row_max => {
            flag(
                &world.wrong,
                format!(
                    "tail: after turn {at} a reopen reads {plain} plain tail bytes in {rows} rows, past the bound {bound} plus its largest row {row_max}"
                ),
            );
            false
        }
        Ok(_) => true,
        Err(error) => {
            flag(&world.wrong, format!("tail: the log decodes: {error}"));
            false
        }
    }
}

fn agree(
    world: &World,
    sides: (&Arc<MemoryLog>, &Arc<MemoryLog>),
    bound: usize,
    at: usize,
) -> bool {
    let left = driven(world, "a bounded resume", resumed(sides.0, Some(bound)));
    let right = driven(world, "an unbounded resume", resumed(sides.1, None));
    match (left, right) {
        (Some(Ok(left)), Some(Ok(right))) if left == right => true,
        (Some(Ok(_)), Some(Ok(_))) => {
            flag(
                &world.wrong,
                format!(
                    "tail: after turn {at} the bounded log resumes another state or other turns than the unbounded one"
                ),
            );
            false
        }
        (Some(Err(error)), _) | (_, Some(Err(error))) => {
            flag(
                &world.wrong,
                format!("tail: after turn {at} a resume fails: {error}"),
            );
            false
        }
        _ => false,
    }
}

fn opened(world: &World, host: &Arc<MemoryLog>, bound: Option<usize>) -> Option<Opened> {
    match driven(world, "a turn log open", open(host, bound))? {
        Ok(opened) => Some(opened),
        Err(error) => {
            flag(&world.wrong, format!("tail: the log opens: {error}"));
            None
        }
    }
}

pub(crate) fn run(rng: &mut Rng, world: &World) {
    let shape = Shape {
        body: usize::try_from(rng.range(BODY_BYTES.start, BODY_BYTES.end))
            .expect("a byte count fits"),
        message: usize::try_from(rng.range(MESSAGE_BYTES.start, MESSAGE_BYTES.end))
            .expect("a byte count fits"),
        page: usize::try_from(rng.range(PAGE_MESSAGES.start, PAGE_MESSAGES.end))
            .expect("a page size fits"),
    };
    let bound_turns = rng.range(BOUND_TURNS.start, BOUND_TURNS.end);
    let turns = bound_turns * rng.range(TURNS_PER_BOUND.start, TURNS_PER_BOUND.end);
    let turn_plain = 2 * shape.body + shape.message;
    let bound = usize::try_from(bound_turns).expect("a turn count fits") * turn_plain;
    let reopen = rng.range(REOPEN_EVERY.start, REOPEN_EVERY.end);
    let (left, right) = (Arc::new(MemoryLog::new()), Arc::new(MemoryLog::new()));
    let (Some(mut bounded_log), Some(mut unbounded_log)) = (
        opened(world, &left, Some(bound)),
        opened(world, &right, None),
    ) else {
        return;
    };
    let mut snapshots = 0;
    for at in 0..turns {
        if at % reopen == 0 {
            let (Some(reopened), Some(other)) = (
                opened(world, &left, Some(bound)),
                opened(world, &right, None),
            ) else {
                return;
            };
            (bounded_log, unbounded_log) = (reopened, other);
        }
        let at = usize::try_from(at).expect("a turn fits");
        for (side, log) in [
            ("bounded", &mut bounded_log),
            ("unbounded", &mut unbounded_log),
        ] {
            match driven(world, "a turn", turn(log, shape, at)) {
                Some(Ok(taken)) if side == "bounded" => snapshots += taken,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    flag(&world.wrong, format!("tail: the {side} log: {error}"));
                    return;
                }
                None => return,
            }
        }
        if !bounded(world, &left, bound, at) || !agree(world, (&left, &right), bound, at) {
            return;
        }
    }
    let plain_total = turns * turn_plain as u64;
    if snapshots > 2 * plain_total / bound as u64 + SNAPSHOTS_SLACK {
        flag(
            &world.wrong,
            format!(
                "tail: the bound {bound} takes {snapshots} snapshots in {turns} turns of {turn_plain} plain bytes"
            ),
        );
    }
}
