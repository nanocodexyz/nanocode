use std::{collections::BTreeSet, convert::Infallible, ops::Range, sync::Arc};

use nanocode::{
    backend::Prompt,
    session_host::{LogEntry, Submission, Submitted, TurnState},
    turnlog::{LogHost, OneLog, Policy, ThreadState},
};
use nanocode_durability_core::{
    Decision, DurableFold, Fact, FactLog, FactLogs, FoldError, Folded, Input, InputKind, ModelOut,
    OwnedFact, Seen, StateStore, VALUE_INLINE_BYTES, Value, commit_fact, fold_facts,
};
use nanocode_turnlog::{CachedAt, FOLD_SNAPSHOT_KEY, Record, SNAPSHOT_RATIO, cache_key};
use serde::{Deserialize, Serialize};
use serde_json::{
    Map,
    value::{RawValue, to_raw_value},
};

use crate::rng::Rng;

const TURNS: [(u64, u64); 11] = [
    (2, 4_310),
    (4, 1_800),
    (8, 1_500),
    (16, 1_050),
    (40, 700),
    (100, 400),
    (250, 160),
    (600, 65),
    (1_200, 20),
    (2_000, 5),
    (2_600, 3),
];
const STATES: u64 = 2;
const SNAPSHOT_ROWS: Range<u64> = 2..8;
const MESSAGE_BYTES: Range<u64> = 64..1_024;
const TOOL_RESULT_PPM: u64 = 50_000;

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct Transcript {
    system: String,
    prompts: u64,
    messages: Vec<String>,
}

impl DurableFold for Transcript {
    const FAMILY: &'static str = "sim.transcript";
    const FORMAT: u32 = 1;
    type Decision = u64;

    fn apply(&mut self, _seq: u64, fact: Seen<'_, u64>) -> Result<(), FoldError> {
        match fact {
            Fact::Input(input) => {
                self.prompts += 1;
                if self.system.is_empty() {
                    input.value.get().clone_into(&mut self.system);
                }
            }
            Fact::ModelOut(out) => self.messages.push(out.response.get().to_owned()),
            Fact::Decision(Decision::Backend(value)) => self.messages.push(value.to_string()),
            _ => {}
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        Ok(Vec::new())
    }
}

pub(crate) struct Expected {
    state: String,
    fold: Option<(u64, String)>,
}

pub(crate) struct Sessions {
    pub next: u64,
    pub ids: Vec<String>,
    pub turns: u64,
    pub bytes: u64,
    system: Box<RawValue>,
    expected: Vec<Expected>,
}

impl Sessions {
    pub(crate) fn states(&self) -> Vec<String> {
        let states: Vec<String> = self.expected.iter().map(|held| held.state.clone()).collect();
        assert!(!states.is_empty(), "an old writer's history holds a session");
        states
    }
}

fn turns(rng: &mut Rng) -> u64 {
    let total: u64 = TURNS.iter().map(|(_, weight)| weight).sum();
    let pick = rng.below(total);
    let mut floor = 0;
    TURNS
        .iter()
        .find(|(_, weight)| {
            floor += weight;
            pick < floor
        })
        .map(|(turns, _)| *turns)
        .expect("a pick below the total weight lands in a band")
}

fn raw(text: &str) -> Box<RawValue> {
    to_raw_value(text).expect("a string encodes")
}

fn message(rng: &mut Rng) -> Box<RawValue> {
    let bytes = if rng.chance(TOOL_RESULT_PPM) {
        VALUE_INLINE_BYTES
            + usize::try_from(rng.below(VALUE_INLINE_BYTES as u64)).expect("a length fits")
    } else {
        usize::try_from(rng.range(MESSAGE_BYTES.start, MESSAGE_BYTES.end)).expect("a length fits")
    };
    raw(&format!("{} {}", rng.below(1_000_000), "m".repeat(bytes)))
}

fn json(fold: &Folded<Transcript>) -> Result<(u64, String), String> {
    let state = serde_json::to_string(fold.state()).map_err(|error| error.to_string())?;
    Ok((fold.seq(), state))
}

async fn commit(
    log: &dyn FactLog,
    fold: Folded<Transcript>,
    fact: OwnedFact<u64>,
) -> Result<Folded<Transcript>, String> {
    commit_fact(log, fold, fact)
        .await
        .map(|(fold, _)| fold)
        .map_err(|error| format!("commit a session fact: {error}"))
}

async fn turn(
    rng: &mut Rng,
    log: &OneLog,
    scoped: &[Arc<dyn FactLog>],
    folds: &mut [Option<Folded<Transcript>>],
    system: &RawValue,
    id: &str,
) -> Result<(), String> {
    let submission = Submission {
        request_id: id.to_owned(),
        prompt: Prompt::new(format!("prompt {}", rng.below(1_000_000))),
        hand: None,
        metadata: raw("legacy"),
    };
    match log.submit(&submission).await {
        Ok(Submitted::Accepted { .. }) => {}
        other => return Err(format!("submit {id}: {other:?}")),
    }
    log.start(id)
        .await
        .map_err(|error| format!("start {id}: {error}"))?
        .ok_or_else(|| format!("turn {id} did not start"))?;
    for (at, fold) in folds.iter_mut().enumerate() {
        let facts = &*scoped[at];
        let mut live = fold
            .take()
            .expect("every session fold is live between turns");
        live = commit(
            facts,
            live,
            Fact::Input(Input {
                turn: id.to_owned(),
                kind: InputKind::Prompt,
                value: system.to_owned(),
                metadata: raw("system"),
                hand: None,
                first: None,
            }),
        )
        .await?;
        let response = message(rng);
        live = commit(
            facts,
            live,
            Fact::ModelOut(ModelOut {
                turn: id.to_owned(),
                response,
            }),
        )
        .await?;
        *fold = Some(live);
    }
    let entries = [LogEntry {
        request_id: Some(id.to_owned()),
        event: raw(&format!("note {id}")),
    }];
    log.append(&entries)
        .await
        .map_err(|error| format!("append to {id}: {error}"))?;
    log.finish(id, TurnState::Completed, &Map::new())
        .await
        .map_err(|error| format!("finish {id}: {error}"))?
        .ok_or_else(|| format!("turn {id} did not finish"))?;
    Ok(())
}

pub(crate) async fn write(
    rng: &mut Rng,
    host: Arc<dyn LogHost>,
    thread: &str,
) -> Result<Sessions, String> {
    let rows = usize::try_from(rng.range(SNAPSHOT_ROWS.start, SNAPSHOT_ROWS.end))
        .expect("a row count fits");
    let policy = Policy {
        snapshot_rows: Some(rows),
        ..Policy::default()
    };
    let log =
        nanocode::turnlog::thread_scope_sim::open(Arc::clone(&host), thread.to_owned(), policy)
            .await
            .map_err(|error| format!("open the old writer: {error}"))?;
    let logs = StateStore::facts(&log.store())
        .ok_or_else(|| "the old writer offers no fact log".to_owned())?;
    let states: Vec<String> = (0..1 + rng.below(STATES))
        .map(|at| format!("session-{at}"))
        .collect();
    let scoped: Vec<Arc<dyn FactLog>> = states
        .iter()
        .map(|state| logs.scoped(state, false))
        .collect();
    let mut folds = Vec::with_capacity(states.len());
    for facts in &scoped {
        let fold = fold_facts::<Transcript>(&**facts)
            .await
            .map_err(|error| format!("fold a new session: {error}"))?;
        folds.push(Some(fold));
    }
    let system = raw(&format!(
        "system {thread} {}",
        "s".repeat(VALUE_INLINE_BYTES)
    ));
    let turns = turns(rng);
    let ids: Vec<String> = (0..turns).map(|at| format!("old{at}")).collect();
    for id in &ids {
        turn(rng, &log, &scoped, &mut folds, &system, id).await?;
    }
    let mut expected = Vec::with_capacity(states.len());
    for ((state, fold), facts) in states.into_iter().zip(folds).zip(&scoped) {
        let live = fold.expect("every session fold is live after the turns");
        let again = fold_facts::<Transcript>(&**facts)
            .await
            .map_err(|error| format!("refold {state} on the old writer: {error}"))?;
        let fold = json(&live)?;
        if json(&again)?.1 != fold.1 {
            return Err(format!(
                "the old writer's refold of {state} differs from its live fold"
            ));
        }
        expected.push(Expected {
            state,
            fold: Some(fold),
        });
    }
    let stored = host
        .load(thread)
        .await
        .map_err(|error| format!("read the old writer's rows: {error}"))?;
    let next = stored.last().map_or(0, |(seq, _)| seq + 1);
    let bytes = host
        .scan(thread, 0, nanocode_turnlog::SCAN_END)
        .await
        .map_err(|error| format!("scan the old writer's rows: {error}"))?
        .iter()
        .map(|(_, body)| body.len() as u64)
        .sum();
    Ok(Sessions {
        next,
        ids,
        turns,
        bytes,
        system,
        expected,
    })
}

fn resolves(
    state: &ThreadState,
    thread: &str,
    digest: &nanocode_durability_core::Digest,
    what: &str,
) -> Result<(), String> {
    match state.blob_body(digest) {
        Some(_) => Ok(()),
        None => Err(format!(
            "the resident state of {thread} names blob {} for {what} without its body",
            digest.key().as_str()
        )),
    }
}

pub(crate) fn held(state: &ThreadState, thread: &str, sessions: &Sessions) -> Result<u64, String> {
    let mut checked = 0;
    for expected in &sessions.expected {
        let Some(value) = state.cached_state(&expected.state) else {
            continue;
        };
        if let Ok(at) = serde_json::from_str::<CachedAt>(value.get()) {
            resolves(
                state,
                thread,
                &at.blob,
                &format!("the cached fold of {}", expected.state),
            )?;
            checked += 1;
        }
    }
    for (seq, kept) in state.facts() {
        let named: Result<_, String> = kept.fact.try_map(
            |value| {
                let Value::Blob(blob) = value else {
                    return Ok(());
                };
                let digest = state.blob_digest(*blob).ok_or_else(|| {
                    format!(
                        "fact {seq} of {thread} names blob {} that no marker holds",
                        blob.index()
                    )
                })?;
                resolves(state, thread, digest, &format!("fact {seq}"))
            },
            |_| Ok(()),
        );
        named.map(drop)?;
        checked += 1;
    }
    Ok(checked)
}

fn row_records(body: &str) -> Vec<Record> {
    let inner = nanocode_turnlog::tagged_row(body).unwrap_or(body);
    match nanocode_turnlog::standalone_plain(inner) {
        Ok(Some(plain)) => nanocode_turnlog::decode(&plain).unwrap_or_default(),
        _ => nanocode_turnlog::decode(inner).unwrap_or_default(),
    }
}

fn named(state: &ThreadState, rows: &[(u64, String)]) -> BTreeSet<String> {
    let mut named = BTreeSet::new();
    for (key, value) in state.hosts() {
        if key
            .strip_prefix(FOLD_SNAPSHOT_KEY)
            .is_some_and(|rest| rest.starts_with('\n'))
            && let Ok(at) = serde_json::from_str::<CachedAt>(value.get())
        {
            named.insert(at.blob.key().as_str().to_owned());
        }
    }
    for (_, body) in rows {
        for record in row_records(body) {
            let Record::Fact { fact, .. } = record else {
                continue;
            };
            let seen: Result<_, Infallible> = fact.try_map(
                |value| {
                    if let Value::Blob(blob) = value
                        && let Some(digest) = state.blob_digest(*blob)
                    {
                        named.insert(digest.key().as_str().to_owned());
                    }
                    Ok(())
                },
                |_| Ok(()),
            );
            let Ok(_) = seen;
        }
    }
    named
}

pub(crate) struct Kept {
    pub(crate) blobs: u64,
    pub(crate) bytes: u64,
}

pub(crate) fn kept(
    state: &ThreadState,
    thread: &str,
    rows: &[(u64, String)],
) -> Result<Kept, String> {
    let named = named(state, rows);
    let mut kept = Kept { blobs: 0, bytes: 0 };
    for (key, value) in state.stored_values() {
        if nanocode_durability_core::BlobKey::parse(key).is_none() {
            continue;
        }
        if !named.contains(key) {
            return Err(format!(
                "the resident state of {thread} holds the {} byte body of blob {key}, which no cached fold or fact names",
                value.len()
            ));
        }
        kept.blobs += 1;
        kept.bytes += value.len() as u64;
    }
    Ok(kept)
}

pub(crate) fn cached(records: &[Record], thread: &str, sessions: &Sessions) -> Result<u64, String> {
    let mut checked = 0;
    for expected in &sessions.expected {
        let key = cache_key(&expected.state);
        let mut uncached = 0_u64;
        let mut last: Option<u64> = None;
        for record in records {
            match record {
                Record::Fact {
                    fact,
                    state: Some(state),
                    ..
                } if *state == expected.state => {
                    uncached += serde_json::to_string(fact)
                        .map_err(|error| error.to_string())?
                        .len() as u64;
                }
                Record::Host { key: written, value } if *written == key => {
                    let at: CachedAt = serde_json::from_str(value.get())
                        .map_err(|error| format!("a cached fold of {thread}: {error}"))?;
                    if let Some(bytes) = last
                        && uncached <= SNAPSHOT_RATIO as u64 * bytes
                    {
                        return Err(format!(
                            "{thread} rewrote the {}-byte cached fold of {} after {uncached} bytes of facts",
                            bytes, expected.state
                        ));
                    }
                    last = Some(at.bytes);
                    uncached = 0;
                    checked += 1;
                }
                _ => {}
            }
        }
    }
    Ok(checked)
}

pub(crate) async fn agrees(
    rng: &mut Rng,
    facts: &dyn FactLogs,
    thread: &str,
    sessions: &mut Sessions,
) -> Result<u64, String> {
    let mut checked = 0;
    for expected in &mut sessions.expected {
        let scoped = facts.scoped(&expected.state, false);
        let fold = fold_facts::<Transcript>(&*scoped).await.map_err(|error| {
            format!("a resident fold of {} on {thread}: {error}", expected.state)
        })?;
        let now = json(&fold)?;
        if let Some(before) = &expected.fold
            && before.1 != now.1
        {
            return Err(format!(
                "the resident fold of {} on {thread} at fact {} differs from the fold its writers left at fact {}",
                expected.state, now.0, before.0
            ));
        }
        checked += 1;
        let fact = if rng.chance(500_000) {
            Fact::Input(Input {
                turn: "resident".to_owned(),
                kind: InputKind::Prompt,
                value: sessions.system.clone(),
                metadata: raw("system"),
                hand: None,
                first: None,
            })
        } else {
            Fact::ModelOut(ModelOut {
                turn: "resident".to_owned(),
                response: message(rng),
            })
        };
        expected.fold = match commit_fact(&*scoped, fold, fact).await {
            Ok((fold, _)) => Some(json(&fold)?),
            Err(_) => None,
        };
    }
    Ok(checked)
}
