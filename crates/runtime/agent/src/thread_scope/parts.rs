use std::{collections::BTreeMap, sync::Arc};

use nanocode_scope::Part;
use nanocode_sync::Vector;
use nanocode_turnlog::{ArmedWake, Checkpoint, Hold, OpenCall, Turn};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;

use super::{
    sim::{self, Mutation},
    state::{KeptEvent, KeptFact, Legacy, Moved, Reserved, ThreadState, invalid},
};
use crate::durability::{StoreError, facts::Digest};

#[derive(Deserialize, Serialize)]
struct Meta {
    generation: u64,
    row: u64,
    last_event: u64,
    reserved: Option<Reserved>,
    fact_base: u64,
    legacy: Option<Legacy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cache_only: Vec<Digest>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Bits {
    Dense(String),
    Sparse(Vec<u64>),
}

#[derive(Deserialize, Serialize)]
struct MovedPart {
    turns: u64,
    below: u64,
    hashes: u32,
    bytes: usize,
    bits: Bits,
}

fn set_bits(bytes: &[u8]) -> Vec<u64> {
    let mut set = Vec::new();
    for (at, byte) in (0_u64..).zip(bytes) {
        for bit in 0..8 {
            if byte & (1 << bit) != 0 {
                set.push(at * 8 + bit);
            }
        }
    }
    set
}

fn bits_of(bytes: &[u8]) -> Bits {
    let set = set_bits(bytes);
    let sparse: usize = set.iter().map(|bit| bit.to_string().len() + 1).sum();
    if sparse < bytes.len() * 2 {
        Bits::Sparse(set)
    } else {
        Bits::Dense(hex(bytes))
    }
}

fn bytes_of(name: &str, moved: &MovedPart) -> Result<Vec<u8>, StoreError> {
    let bytes = match &moved.bits {
        Bits::Dense(text) => unhex(name, text)?,
        Bits::Sparse(set) => {
            let mut bytes = vec![0_u8; moved.bytes];
            let mut last = None;
            for bit in set {
                if last.is_some_and(|last| *bit <= last) {
                    return Err(invalid(format!("part {name} lists bit {bit} out of order")));
                }
                last = Some(*bit);
                let byte = usize::try_from(bit / 8)
                    .ok()
                    .filter(|byte| *byte < moved.bytes)
                    .ok_or_else(|| {
                        invalid(format!("part {name} sets bit {bit} past its filter"))
                    })?;
                bytes[byte] |= 1 << (bit % 8);
            }
            bytes
        }
    };
    if bytes.len() != moved.bytes || bytes.is_empty() {
        return Err(invalid(format!(
            "part {name} holds {} filter bytes, not the {} it names",
            bytes.len(),
            moved.bytes
        )));
    }
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("a string takes hex digits");
    }
    text
}

fn unhex(name: &str, text: &str) -> Result<Vec<u8>, StoreError> {
    if !text.is_ascii() || text.len() % 2 != 0 || text.is_empty() {
        return Err(invalid(format!("part {name} holds no whole bytes")));
    }
    (0..text.len())
        .step_by(2)
        .map(|at| {
            u8::from_str_radix(&text[at..at + 2], 16)
                .map_err(|_| invalid(format!("part {name} holds a byte that is not hex")))
        })
        .collect()
}

fn json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("a resident thread's values encode as JSON")
}

fn parsed<T: DeserializeOwned>(name: &str, body: &str) -> Result<T, StoreError> {
    serde_json::from_str(body).map_err(|error| invalid(format!("part {name}: {error}")))
}

fn chunked<T: Serialize>(
    parts: &mut Vec<Part>,
    kind: &str,
    rows: usize,
    values: impl Iterator<Item = T>,
) {
    let mut chunk = Vec::with_capacity(rows);
    let mut at = 0_usize;
    for value in values {
        chunk.push(value);
        if chunk.len() == rows {
            parts.push(Part {
                name: format!("{kind}/{at}"),
                body: json(&chunk),
            });
            chunk.clear();
            at += 1;
        }
    }
    if !chunk.is_empty() {
        parts.push(Part {
            name: format!("{kind}/{at}"),
            body: json(&chunk),
        });
    }
}

pub(crate) fn encode(state: &ThreadState, rows: usize) -> Vec<Part> {
    let mut parts = vec![Part {
        name: "meta".to_owned(),
        body: json(&Meta {
            generation: state.generation,
            row: state.row,
            last_event: state.last_event,
            reserved: state.reserved,
            fact_base: state.fact_base,
            legacy: state.legacy,
            cache_only: state.cache_only.iter().copied().collect(),
        }),
    }];
    assert_eq!(
        state.admitted.len(),
        state.turns.len(),
        "every resident turn is admitted at one event"
    );
    chunked(
        &mut parts,
        "turns",
        rows,
        state
            .admitted
            .values()
            .map(|id| state.turns.get(id).expect("an admitted turn is resident"))
            .map(AsRef::as_ref),
    );
    chunked(
        &mut parts,
        "starts",
        rows,
        state
            .admitted
            .values()
            .filter_map(|id| state.starts.get(id).map(|row| (id.as_ref(), *row))),
    );
    chunked(
        &mut parts,
        "ends",
        rows,
        state.ends.iter().map(|(row, id)| (*row, id.as_ref())),
    );
    if let Some(moved) = &state.moved {
        parts.push(Part {
            name: "moved".to_owned(),
            body: json(&MovedPart {
                turns: moved.turns,
                below: moved.below,
                hashes: moved.hashes,
                bytes: moved.bits.len(),
                bits: bits_of(&moved.bits),
            }),
        });
    }
    chunked(
        &mut parts,
        "events",
        rows,
        state.events.iter().map(AsRef::as_ref),
    );
    chunked(
        &mut parts,
        "facts",
        rows,
        state.facts.iter().map(AsRef::as_ref),
    );
    chunked(&mut parts, "blobs", rows, state.blobs.iter());
    keyed(&mut parts, "cached", state.cached.iter());
    for ((session, cell), updates) in state.cells.iter() {
        parts.push(Part {
            name: format!("cell/{cell}/{session}"),
            body: json(updates),
        });
    }
    keyed(&mut parts, "checkpoint", state.checkpoints.iter());
    for (key, value) in state.stored.iter() {
        parts.push(Part {
            name: format!("stored/{key}"),
            body: value.as_ref().to_owned(),
        });
    }
    keyed(&mut parts, "host", state.hosts.iter());
    keyed(&mut parts, "hold", state.holds.iter());
    keyed(&mut parts, "call", state.calls.iter());
    for (turn, responses) in state.models.iter() {
        let mut models = Vec::new();
        chunked(&mut models, "model", rows, responses.iter());
        parts.extend(models.into_iter().map(|part| Part {
            name: format!("{}/{turn}", part.name),
            body: part.body,
        }));
    }
    keyed(&mut parts, "wake", state.wakes.iter());
    keyed(&mut parts, "intent", state.intents.iter());
    keyed(&mut parts, "uncached", state.uncached.iter());
    keyed(&mut parts, "logging", state.logging.iter());
    if sim::mutated(Mutation::PartDropped) {
        parts.pop();
    }
    parts
}

fn keyed<'a, V: Serialize + 'a>(
    parts: &mut Vec<Part>,
    kind: &str,
    values: impl Iterator<Item = (&'a Arc<str>, &'a V)>,
) {
    for (key, value) in values {
        parts.push(Part {
            name: format!("{kind}/{key}"),
            body: json(value),
        });
    }
}

#[derive(Default)]
struct Pieces {
    events: BTreeMap<u64, Vec<KeptEvent>>,
    facts: BTreeMap<u64, Vec<KeptFact>>,
    blobs: BTreeMap<u64, Vec<Digest>>,
    starts: BTreeMap<u64, Vec<(String, u64)>>,
    ends: BTreeMap<u64, Vec<(u64, String)>>,
    models: BTreeMap<(Arc<str>, u64), Vec<Box<RawValue>>>,
}

fn index(name: &str, at: &str) -> Result<u64, StoreError> {
    at.parse()
        .map_err(|_| invalid(format!("part {name} names no chunk")))
}

fn ordered<T>(kind: &str, chunks: BTreeMap<u64, Vec<T>>) -> Result<Vec<T>, StoreError> {
    let mut values = Vec::new();
    for (expected, (at, chunk)) in (0_u64..).zip(chunks) {
        if at != expected {
            return Err(invalid(format!("{kind} chunk {expected} is missing")));
        }
        values.extend(chunk);
    }
    Ok(values)
}

pub(crate) fn decode(parts: Vec<Part>) -> Result<ThreadState, StoreError> {
    let mut state = ThreadState::default();
    let mut pieces = Pieces::default();
    let mut meta = None;
    for part in parts {
        if part.name == "meta" {
            meta = Some(parsed::<Meta>(&part.name, &part.body)?);
            continue;
        }
        if part.name == "moved" {
            let moved: MovedPart = parsed(&part.name, &part.body)?;
            state.moved = Some(Moved {
                turns: moved.turns,
                below: moved.below,
                hashes: moved.hashes,
                bits: Arc::new(bytes_of(&part.name, &moved)?),
            });
            continue;
        }
        let Some((kind, rest)) = part.name.split_once('/') else {
            return Err(invalid(format!("part {} names no kind", part.name)));
        };
        place(&mut state, &mut pieces, kind, rest, &part)?;
    }
    let Some(meta) = meta else {
        return Err(invalid("the checkpoint has no meta part"));
    };
    state.generation = meta.generation;
    state.row = meta.row;
    state.last_event = meta.last_event;
    state.reserved = meta.reserved;
    state.fact_base = meta.fact_base;
    state.legacy = meta.legacy;
    state.cache_only = meta.cache_only.into_iter().collect();
    for event in ordered("events", pieces.events)? {
        state.events.push_back(Arc::new(event));
    }
    for fact in ordered("facts", pieces.facts)? {
        state.facts.push_back(Arc::new(fact));
    }
    state.blobs = ordered("blobs", pieces.blobs)?.into_iter().collect();
    for (id, row) in ordered("starts", pieces.starts)? {
        if state.starts.insert(Arc::from(id.as_str()), row).is_some() {
            return Err(invalid(format!("turn {id} starts twice")));
        }
    }
    for (row, id) in ordered("ends", pieces.ends)? {
        if !state.starts.contains_key(id.as_str()) {
            return Err(invalid(format!(
                "turn {id} ends in the thread's rows without a start"
            )));
        }
        if state.ends.insert((row, Arc::from(id.as_str()))).is_some() {
            return Err(invalid(format!("turn {id} ends twice at row {row}")));
        }
    }
    let mut turns: BTreeMap<Arc<str>, BTreeMap<u64, Vec<Box<RawValue>>>> = BTreeMap::new();
    for ((turn, at), chunk) in pieces.models {
        turns.entry(turn).or_default().insert(at, chunk);
    }
    for (turn, chunks) in turns {
        let responses: Vector<Box<RawValue>> = ordered("model", chunks)?.into_iter().collect();
        state.models.insert(turn, responses);
    }
    for (id, turn) in state.turns.iter() {
        if state.admitted.contains_key(&turn.first) {
            return Err(invalid(format!("two turns start at event {}", turn.first)));
        }
        state.admitted.insert(turn.first, Arc::clone(id));
    }
    Ok(state)
}

fn place(
    state: &mut ThreadState,
    pieces: &mut Pieces,
    kind: &str,
    rest: &str,
    part: &Part,
) -> Result<(), StoreError> {
    let name = part.name.as_str();
    let body = part.body.as_str();
    let key: Arc<str> = Arc::from(rest);
    match kind {
        "turn" => {
            let turn: Turn = parsed(name, body)?;
            if turn.id.as_str() != rest {
                return Err(invalid(format!("part {name} holds turn {}", turn.id)));
            }
            state.turns.insert(key, Arc::new(turn));
        }
        "turns" => {
            index(name, rest)?;
            for turn in parsed::<Vec<Turn>>(name, body)? {
                let id: Arc<str> = Arc::from(turn.id.as_str());
                if state.turns.contains_key(&id) {
                    return Err(invalid(format!("part {name} holds turn {id} twice")));
                }
                state.turns.insert(id, Arc::new(turn));
            }
        }
        "events" => {
            pieces
                .events
                .insert(index(name, rest)?, parsed(name, body)?);
        }
        "facts" => {
            pieces.facts.insert(index(name, rest)?, parsed(name, body)?);
        }
        "blobs" => {
            pieces.blobs.insert(index(name, rest)?, parsed(name, body)?);
        }
        "starts" => {
            pieces
                .starts
                .insert(index(name, rest)?, parsed(name, body)?);
        }
        "ends" => {
            pieces.ends.insert(index(name, rest)?, parsed(name, body)?);
        }
        "cell" => {
            let Some((cell, session)) = rest.split_once('/') else {
                return Err(invalid(format!("part {name} names no session")));
            };
            let updates: Vector<Box<RawValue>> = parsed(name, body)?;
            state
                .cells
                .insert((Arc::from(session), index(name, cell)?), updates);
        }
        "model" => {
            let Some((at, turn)) = rest.split_once('/') else {
                return Err(invalid(format!("part {name} names no turn")));
            };
            pieces
                .models
                .insert((Arc::from(turn), index(name, at)?), parsed(name, body)?);
        }
        "stored" => {
            state.stored.insert(key, Arc::from(body));
        }
        kind => keep(state, kind, key, name, body)?,
    }
    Ok(())
}

fn keep(
    state: &mut ThreadState,
    kind: &str,
    key: Arc<str>,
    name: &str,
    body: &str,
) -> Result<(), StoreError> {
    match kind {
        "checkpoint" => {
            state
                .checkpoints
                .insert(key, Arc::new(parsed::<Checkpoint>(name, body)?));
        }
        "host" => {
            state.hosts.insert(key, parsed(name, body)?);
        }
        "hold" => {
            state
                .holds
                .insert(key, Arc::new(parsed::<Hold>(name, body)?));
        }
        "call" => {
            state
                .calls
                .insert(key, Arc::new(parsed::<OpenCall>(name, body)?));
        }
        "wake" => {
            state
                .wakes
                .insert(key, Arc::new(parsed::<ArmedWake>(name, body)?));
        }
        "intent" => {
            let held: Option<String> = parsed(name, body)?;
            state.intents.insert(key, held.map(Arc::from));
        }
        "uncached" => {
            state.uncached.insert(key, parsed(name, body)?);
        }
        "logging" => {
            state.logging.insert(key, parsed(name, body)?);
        }
        "cached" => {
            state.cached.insert(key, parsed(name, body)?);
        }
        kind => return Err(invalid(format!("part {name} has unknown kind {kind}"))),
    }
    Ok(())
}
