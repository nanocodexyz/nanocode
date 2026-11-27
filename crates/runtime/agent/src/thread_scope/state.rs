use std::sync::Arc;

use std::collections::BTreeMap;

use nanocode_sync::{OrdMap, OrdSet, Vector};
use nanocode_turnlog::{
    ArmedWake, CachedAt, Checkpoint, EffectClass, Event, FOLD_SNAPSHOT_KEY, Hold, OpenCall, Outcome, Phase,
    Record, Recovered, Retry, SNAPSHOT_RATIO, Status, Turn, apply_patch, cache_key, input_digest,
    internal_host,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::durability::{
    StoreError,
    facts::{
        BlobRef, Decision, Digest, Fact, Input, InputKind, Marker, StoredFact, TurnStatus, Value,
    },
};

pub(crate) const RESERVED_KEY: &str = "$thread_reserved";
pub(crate) const UNTAGGED: char = '{';

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KeptFact {
    pub fact: StoredFact,
    pub state: Option<String>,
    pub row: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KeptEvent {
    pub event: Event,
    pub row: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Legacy {
    pub next: u64,
    pub last_event: u64,
    pub last_fact: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Reserved {
    pub seq: u64,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinishedFilter {
    pub bytes: usize,
    pub hashes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub recent_rows: usize,
    pub filter: FinishedFilter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    pub turns: u64,
    pub below: u64,
    pub hashes: u32,
    pub bits: Arc<Vec<u8>>,
}

impl Moved {
    fn new(filter: FinishedFilter) -> Self {
        assert!(filter.bytes > 0, "a finished turn filter holds at least one byte");
        assert!(filter.hashes > 0, "a finished turn filter sets at least one bit per turn");
        Self {
            turns: 0,
            below: 0,
            hashes: filter.hashes,
            bits: Arc::new(vec![0; filter.bytes]),
        }
    }

    fn positions(&self, id: &str) -> impl Iterator<Item = (usize, u8)> + use<> {
        let digest = nanocode_digest::blake3(id.as_bytes());
        let (low, high) = digest.as_bytes().split_at(8);
        let low = u64::from_le_bytes(low.try_into().expect("a digest holds eight bytes"));
        let high = u64::from_le_bytes(high[..8].try_into().expect("a digest holds sixteen bytes"));
        self.spread(low, high)
    }

    fn tag_positions(&self, tag: u128) -> impl Iterator<Item = (usize, u8)> + use<> {
        let bytes = tag.to_le_bytes();
        let (low, high) = bytes.split_at(8);
        let low = u64::from_le_bytes(low.try_into().expect("a tag holds eight bytes"));
        let high = u64::from_le_bytes(high.try_into().expect("a tag holds sixteen bytes"));
        self.spread(low, high)
    }

    fn spread(&self, low: u64, high: u64) -> impl Iterator<Item = (usize, u8)> + use<> {
        let high = high | 1;
        let bits = self.bits.len() as u64 * 8;
        (0..u64::from(self.hashes)).map(move |at| {
            let bit = low.wrapping_add(at.wrapping_mul(high)) % bits;
            ((bit / 8) as usize, 1_u8 << (bit % 8))
        })
    }

    #[must_use]
    pub fn may_hold(&self, id: &str) -> bool {
        self.positions(id)
            .all(|(byte, mask)| self.bits[byte] & mask != 0)
    }

    #[must_use]
    pub fn may_hold_tag(&self, tag: u128) -> bool {
        self.tag_positions(tag)
            .all(|(byte, mask)| self.bits[byte] & mask != 0)
    }

    fn insert(&mut self, id: &str, row: u64) {
        let positions: Vec<(usize, u8)> = self.positions(id).collect();
        self.set(&positions, row);
    }

    fn insert_tag(&mut self, tag: u128, row: u64) {
        let positions: Vec<(usize, u8)> = self.tag_positions(tag).collect();
        self.set(&positions, row);
    }

    fn set(&mut self, positions: &[(usize, u8)], row: u64) {
        let bits = Arc::make_mut(&mut self.bits);
        for (byte, mask) in positions {
            bits[*byte] |= mask;
        }
        self.turns += 1;
        self.below = self.below.max(row + 1);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probing {
    pub request_id: String,
    pub from: u64,
    pub to: u64,
    pub legacy: Option<Legacy>,
}

#[derive(Clone, Debug)]
pub struct Probe {
    pub through: u64,
    pub turn: Option<Arc<Turn>>,
}

pub type Probes = Arc<BTreeMap<String, Probe>>;

pub enum Found {
    Turn(Arc<Turn>),
    Absent,
    Probe(Probing),
}

#[derive(Clone, Default)]
pub struct ThreadState {
    pub(crate) turns: OrdMap<Arc<str>, Arc<Turn>>,
    pub(crate) admitted: OrdMap<u64, Arc<str>>,
    pub(crate) starts: OrdMap<Arc<str>, u64>,
    pub(crate) ends: OrdSet<(u64, Arc<str>)>,
    pub(crate) moved: Option<Moved>,
    pub(crate) events: Vector<Arc<KeptEvent>>,
    pub(crate) last_event: u64,
    pub(crate) reserved: Option<Reserved>,
    pub(crate) facts: Vector<Arc<KeptFact>>,
    pub(crate) fact_base: u64,
    pub(crate) legacy: Option<Legacy>,
    pub(crate) cached: OrdMap<Arc<str>, u64>,
    pub(crate) cells: OrdMap<(Arc<str>, u64), Vector<Box<RawValue>>>,
    pub(crate) checkpoints: OrdMap<Arc<str>, Arc<Checkpoint>>,
    pub(crate) stored: OrdMap<Arc<str>, Arc<str>>,
    pub(crate) hosts: OrdMap<Arc<str>, Box<RawValue>>,
    pub(crate) holds: OrdMap<Arc<str>, Arc<Hold>>,
    pub(crate) calls: OrdMap<Arc<str>, Arc<OpenCall>>,
    pub(crate) models: OrdMap<Arc<str>, Vector<Box<RawValue>>>,
    pub(crate) wakes: OrdMap<Arc<str>, Arc<ArmedWake>>,
    pub(crate) intents: OrdMap<Arc<str>, Option<Arc<str>>>,
    pub(crate) uncached: OrdMap<Arc<str>, u64>,
    pub(crate) logging: OrdMap<Arc<str>, u64>,
    pub(crate) blobs: Vector<Digest>,
    pub(crate) cache_only: OrdSet<Digest>,
    pub(crate) generation: u64,
    pub(crate) row: u64,
}

#[derive(serde::Deserialize)]
struct CellHeader {
    origin_call_id: String,
}

#[derive(serde::Deserialize)]
struct CellOrigin {
    header: Option<CellHeader>,
}

pub(crate) fn records(seq: u64, body: &str) -> Result<Vec<Record>, StoreError> {
    let body = nanocode_turnlog::tagged_row(body)?;
    match nanocode_turnlog::standalone_plain(body)? {
        Some(plain) => nanocode_turnlog::decode(&plain),
        None if body.starts_with(UNTAGGED) => nanocode_turnlog::decode(body),
        None => Err(invalid(format!(
            "row {seq} was written by the turn log, not by a resident thread"
        ))),
    }
}

pub(crate) fn invalid(detail: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(format!("thread scope row is invalid: {detail}"))
}

impl ThreadState {
    #[must_use]
    pub fn turn(&self, id: &str) -> Option<&Turn> {
        self.turns.get(id).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn found(&self, id: &str, probes: &Probes) -> Found {
        if let Some(turn) = self.turns.get(id) {
            return Found::Turn(Arc::clone(turn));
        }
        let Some(moved) = &self.moved else {
            return Found::Absent;
        };
        let held = moved.may_hold(id)
            || (self.legacy.is_some() && moved.may_hold_tag(nanocode_turnlog::tag(id)));
        if super::sim::mutated(super::sim::Mutation::AdmissionSkipsHistory) || !held {
            return Found::Absent;
        }
        match probes.get(id) {
            Some(probe) if probe.through >= moved.below => {
                probe.turn.clone().map_or(Found::Absent, Found::Turn)
            }
            _ => Found::Probe(Probing {
                request_id: id.to_owned(),
                from: self.legacy.map_or(0, |legacy| legacy.next),
                to: moved.below,
                legacy: self.legacy,
            }),
        }
    }

    #[must_use]
    pub const fn moved(&self) -> Option<&Moved> {
        self.moved.as_ref()
    }

    pub(crate) fn retired_below(&mut self, tag: u128, below: u64, filter: FinishedFilter) {
        assert!(below > 0, "a retired turn lies in the history below a row");
        self.moved
            .get_or_insert_with(|| Moved::new(filter))
            .insert_tag(tag, below - 1);
    }

    pub(crate) fn moved_below(&mut self, id: &str, below: u64, filter: FinishedFilter) {
        assert!(below > 0, "a turn moves into the history below a row");
        assert!(
            !self.turns.contains_key(id),
            "a turn moves into the history only while it is not resident"
        );
        self.moved
            .get_or_insert_with(|| Moved::new(filter))
            .insert(id, below - 1);
    }

    pub(crate) fn seeded(&mut self, turn: Turn) {
        let id: Arc<str> = Arc::from(turn.id.as_str());
        assert!(
            self.turns.is_empty(),
            "a probe seeds its trace with at most one turn"
        );
        self.admitted.insert(turn.first, Arc::clone(&id));
        self.turns.insert(id, Arc::new(turn));
    }

    pub fn hosts(&self) -> impl Iterator<Item = (&str, &RawValue)> {
        self.hosts
            .iter()
            .map(|(key, value)| (key.as_ref(), value.as_ref()))
    }

    pub fn traced(&mut self, record: &Record, row: u64, id: &str) -> Result<(), StoreError> {
        match record {
            Record::Admitted { turn, .. }
            | Record::Status { turn, .. }
            | Record::Recovery { turn, .. }
                if turn == id =>
            {
                self.take(record, Some(row))
            }
            Record::Fact {
                fact,
                seq,
                state: None,
            } if names(fact, id) => self.lower(*seq, fact, None, row),
            _ => Ok(()),
        }
    }

    pub fn turns(&self) -> impl Iterator<Item = &Turn> {
        self.admitted
            .values()
            .filter_map(|id| self.turns.get(id.as_ref()).map(AsRef::as_ref))
    }

    pub fn pending(&self) -> impl Iterator<Item = &Turn> {
        self.turns()
            .filter(|turn| !matches!(turn.phase, Phase::Finished { .. }))
    }

    pub fn events(&self) -> impl Iterator<Item = &Event> {
        self.events.iter().map(|kept| &kept.event)
    }

    pub fn recent_events(&self) -> impl Iterator<Item = &KeptEvent> {
        self.events.iter().map(AsRef::as_ref)
    }

    pub fn recent_facts(&self) -> impl Iterator<Item = &KeptFact> {
        self.facts.iter().map(AsRef::as_ref)
    }

    pub fn events_after(&self, after: u64) -> impl Iterator<Item = &Event> {
        self.events().filter(move |event| event.seq > after)
    }

    #[must_use]
    pub const fn last_event(&self) -> u64 {
        self.last_event
    }

    #[must_use]
    pub const fn reserved(&self) -> Option<Reserved> {
        self.reserved
    }

    #[must_use]
    pub fn next_event(&self) -> u64 {
        self.last_event.max(self.reserved.map_or(0, |reserved| reserved.seq)) + 1
    }

    #[must_use]
    pub const fn legacy(&self) -> Option<Legacy> {
        self.legacy
    }

    #[must_use]
    pub fn start_row(&self, turn: &str) -> Option<u64> {
        self.starts.get(turn).copied()
    }

    #[must_use]
    pub fn event_row(&self, after: u64) -> u64 {
        self.admitted
            .range(..=after.saturating_add(1))
            .rev()
            .find_map(|(_, id)| self.starts.get(id).copied())
            .unwrap_or_else(|| self.legacy.map_or(0, |legacy| legacy.next))
    }

    #[must_use]
    pub fn cached_row(&self, state: &str) -> Option<u64> {
        self.cached.get(state).copied()
    }

    #[must_use]
    pub const fn next_fact(&self) -> u64 {
        self.fact_base + self.facts.len() as u64 + 1
    }

    #[must_use]
    pub const fn fact_base(&self) -> u64 {
        self.fact_base
    }

    pub fn facts(&self) -> impl Iterator<Item = (u64, &KeptFact)> {
        (self.fact_base + 1..).zip(self.facts.iter().map(AsRef::as_ref))
    }

    pub fn facts_after(&self, after: u64) -> impl Iterator<Item = (u64, &KeptFact)> {
        self.facts().skip_while(move |(seq, _)| *seq <= after)
    }

    #[must_use]
    pub fn blob_digest(&self, blob: BlobRef) -> Option<&Digest> {
        self.blobs.get(blob.index() as usize)
    }

    #[must_use]
    pub fn blob_body(&self, digest: &Digest) -> Option<&str> {
        self.stored(digest.key().as_str())
    }

    pub fn stored_values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.stored.iter().map(|(key, value)| (key.as_ref(), value.as_ref()))
    }

    pub(crate) fn blob_known(&self, digest: &Digest) -> bool {
        self.blobs.iter().any(|known| known == digest)
    }

    fn cached_blob(value: &RawValue) -> Option<Digest> {
        serde_json::from_str::<CachedAt>(value.get())
            .ok()
            .map(|at| at.blob)
    }

    fn folds_name(&self, digest: &Digest) -> bool {
        self.hosts
            .iter()
            .filter(|(key, _)| {
                key.strip_prefix(FOLD_SNAPSHOT_KEY)
                    .is_some_and(|rest| rest.starts_with('\n'))
            })
            .any(|(_, value)| Self::cached_blob(value).as_ref() == Some(digest))
    }

    fn named_by_values(&mut self, fact: &StoredFact) {
        if self.cache_only.is_empty() {
            return;
        }
        let blobs = &self.blobs;
        let cache_only = &mut self.cache_only;
        let named: Result<_, std::convert::Infallible> = fact.try_map(
            |value| {
                if let Value::Blob(blob) = value
                    && let Some(digest) = blobs.get(blob.index() as usize)
                {
                    cache_only.remove(digest);
                }
                Ok(())
            },
            |_| Ok(()),
        );
        let Ok(_) = named;
    }

    fn recached(&mut self, state: &str, replaced: Option<Digest>, value: &RawValue) {
        let Some(blob) = Self::cached_blob(value) else {
            return;
        };
        let fresh = self.facts.back().is_some_and(|kept| {
            matches!(&kept.fact, Fact::Marker(Marker::Blob { digest, .. }) if *digest == blob)
        });
        if fresh {
            self.cache_only.insert(blob);
        }
        let Some(replaced) = replaced.filter(|replaced| *replaced != blob) else {
            return;
        };
        if !self.cache_only.contains(&replaced)
            || self.folds_name(&replaced)
            || super::sim::mutated(super::sim::Mutation::ReplacedFoldKept)
        {
            return;
        }
        self.cache_only.remove(&replaced);
        let dropped = self.stored.remove(replaced.key().as_str());
        assert!(
            dropped.is_some(),
            "the cached fold of {state} replaced a fold whose body the thread held"
        );
        assert!(
            self.blob_body(&replaced).is_none(),
            "a replaced fold's body leaves the thread"
        );
    }

    pub(crate) fn index_blob(&mut self, fact: &StoredFact) {
        if let Fact::Marker(Marker::Blob { digest, .. }) = fact {
            self.blobs.push_back(*digest);
        }
    }

    pub fn cells(&self) -> impl Iterator<Item = (&str, u64, &Vector<Box<RawValue>>)> {
        self.cells
            .iter()
            .map(|((session, cell), updates)| (session.as_ref(), *cell, updates))
    }

    pub fn cells_of<'a>(
        &'a self,
        session: &'a str,
    ) -> impl Iterator<Item = (u64, &'a Vector<Box<RawValue>>)> + 'a {
        self.cells
            .iter()
            .filter(move |((owner, _), _)| owner.as_ref() == session)
            .map(|((_, cell), updates)| (*cell, updates))
    }

    #[must_use]
    pub fn checkpoint(&self, state: &str) -> Option<&Checkpoint> {
        self.checkpoints.get(state).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn stored(&self, key: &str) -> Option<&str> {
        self.stored.get(key).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn host(&self, key: &str) -> Option<&RawValue> {
        self.hosts.get(key).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn hold(&self, key: &str) -> Option<&Hold> {
        self.holds.get(key).map(AsRef::as_ref)
    }

    pub fn holds(&self) -> impl Iterator<Item = (&str, &Hold)> {
        self.holds
            .iter()
            .map(|(key, hold)| (key.as_ref(), hold.as_ref()))
    }

    pub fn open_calls(&self) -> impl Iterator<Item = (&str, &OpenCall)> {
        self.calls
            .iter()
            .map(|(id, call)| (id.as_ref(), call.as_ref()))
    }

    #[must_use]
    pub fn models(&self, turn: &str) -> Option<&Vector<Box<RawValue>>> {
        self.models.get(turn)
    }

    pub fn wakes(&self) -> impl Iterator<Item = (&str, &ArmedWake)> {
        self.wakes
            .iter()
            .map(|(key, wake)| (key.as_ref(), wake.as_ref()))
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn row(&self) -> u64 {
        self.row
    }

    #[must_use]
    pub fn unknown(&self) -> Vec<String> {
        self.calls
            .iter()
            .filter(|(_, call)| call.effect.needs_row())
            .map(|(id, _)| id.as_ref().to_owned())
            .collect()
    }

    pub fn intents(&self) -> impl Iterator<Item = (&str, Option<&str>, &OpenCall)> {
        self.intents.iter().filter_map(|(call_id, state)| {
            let call = self.calls.get(call_id)?;
            Some((call_id.as_ref(), state.as_deref(), call.as_ref()))
        })
    }

    #[must_use]
    pub fn logs_facts(&self, session: &str) -> bool {
        self.logging.contains_key(session)
    }

    pub(crate) fn logged(&mut self, session: &str, seq: u64) {
        if !self.logging.contains_key(session) {
            self.logging.insert(Arc::from(session), seq);
        }
    }

    pub fn intent(&self, call_id: &str) -> Option<&OpenCall> {
        self.intents
            .contains_key(call_id)
            .then(|| self.calls.get(call_id).map(AsRef::as_ref))
            .flatten()
    }

    #[must_use]
    pub fn recovering(&self) -> bool {
        self.turns.values().any(|turn| {
            turn.recovered
                .is_some_and(|recovered| recovered.at == Some(self.row))
        })
    }

    #[must_use]
    pub fn next_attempt(&self, turn: &str) -> u64 {
        match self.turn(turn).and_then(|turn| turn.recovered) {
            Some(Recovered {
                at: Some(at),
                attempt,
            }) if self.row <= at => attempt + 1,
            Some(Recovered { at: None, attempt }) => attempt + 1,
            _ => 1,
        }
    }

    #[must_use]
    pub fn retired(&self, id: &str) -> Option<&Turn> {
        self.turn(id)
            .filter(|turn| turn.digest.starts_with(super::adopt::RETIRED_DIGEST))
    }

    #[must_use]
    pub fn cached_state(&self, state: &str) -> Option<&RawValue> {
        self.host(&cache_key(state))
    }

    #[must_use]
    pub fn cache_due(&self, state: &str) -> bool {
        let cached = match self.cached_state(state) {
            Some(value) if super::sim::mutated(super::sim::Mutation::CacheDueIgnoresBlob) => {
                value.get().len() as u64
            }
            Some(_) if self.cached_row(state).is_none() => 0,
            Some(value) => serde_json::from_str::<CachedAt>(value.get())
                .map_or(value.get().len() as u64, |at| at.bytes),
            None => 0,
        };
        self.uncached
            .get(state)
            .is_some_and(|bytes| *bytes > SNAPSHOT_RATIO as u64 * cached)
    }

    #[must_use]
    pub fn finished(&self) -> usize {
        let resident = self
            .turns
            .values()
            .filter(|turn| matches!(turn.phase, Phase::Finished { .. }))
            .count();
        resident + self.moved.as_ref().map_or(0, |moved| moved.turns as usize)
    }

    #[must_use]
    pub fn digest(&self) -> String {
        let mut text = String::new();
        let mut put = |kind: &str, value: &dyn erased::Encode| {
            text.push_str(kind);
            text.push(' ');
            text.push_str(&value.json());
            text.push('\n');
        };
        put("generation", &self.generation);
        put("row", &self.row);
        for (id, turn) in self.turns.iter() {
            put("turn", &(id.as_ref(), turn.as_ref()));
        }
        for (first, id) in self.admitted.iter() {
            put("admitted", &(first, id.as_ref()));
        }
        for (id, row) in self.starts.iter() {
            put("start", &(id.as_ref(), row));
        }
        for (row, id) in self.ends.iter() {
            put("end", &(row, id.as_ref()));
        }
        if let Some(moved) = &self.moved {
            put("moved", &(moved.turns, moved.below, moved.hashes, moved.bits.as_ref()));
        }
        put("last_event", &self.last_event);
        put("reserved", &self.reserved);
        put("fact_base", &self.fact_base);
        put("legacy", &self.legacy);
        for event in self.events.iter() {
            put("event", event.as_ref());
        }
        for fact in self.facts.iter() {
            put("fact", fact.as_ref());
        }
        for (state, row) in self.cached.iter() {
            put("cached", &(state.as_ref(), row));
        }
        for digest in self.blobs.iter() {
            put("blob", digest);
        }
        for digest in self.cache_only.iter() {
            put("cache_only", digest);
        }
        for ((session, cell), updates) in self.cells.iter() {
            put("cell", &(session.as_ref(), cell, updates));
        }
        for (state, checkpoint) in self.checkpoints.iter() {
            put("checkpoint", &(state.as_ref(), checkpoint.as_ref()));
        }
        for (key, value) in self.stored.iter() {
            put("stored", &(key.as_ref(), value.as_ref()));
        }
        for (key, value) in self.hosts.iter() {
            put("host", &(key.as_ref(), value));
        }
        for (key, hold) in self.holds.iter() {
            put("hold", &(key.as_ref(), hold.as_ref()));
        }
        for (id, call) in self.calls.iter() {
            put("call", &(id.as_ref(), call.as_ref()));
        }
        for (turn, responses) in self.models.iter() {
            put("model", &(turn.as_ref(), responses));
        }
        for (key, wake) in self.wakes.iter() {
            put("wake", &(key.as_ref(), wake.as_ref()));
        }
        for (call_id, state) in self.intents.iter() {
            put("intent", &(call_id.as_ref(), state.as_deref()));
        }
        for (state, bytes) in self.uncached.iter() {
            put("uncached", &(state.as_ref(), bytes));
        }
        for (state, first) in self.logging.iter() {
            put("logging", &(state.as_ref(), first));
        }
        nanocode_digest::blake3(text.as_bytes())
            .to_hex()
            .as_str()
            .to_owned()
    }

    pub fn load(&mut self, seq: u64, body: &str, window: Window) -> Result<(), StoreError> {
        for record in records(seq, body)? {
            self.take(&record, Some(seq))?;
        }
        self.row = seq;
        self.forget_before(seq, window.recent_rows);
        self.move_before(seq, window);
        Ok(())
    }

    fn move_before(&mut self, seq: u64, window: Window) {
        if super::sim::mutated(super::sim::Mutation::KeepFinishedTurns) {
            return;
        }
        let kept = |row: u64| row + window.recent_rows as u64 > seq;
        while let Some((row, id)) = self.ends.get_min().cloned()
            && !kept(row)
        {
            self.ends.remove(&(row, Arc::clone(&id)));
            let turn = self
                .turns
                .remove(&id)
                .expect("a turn that finished in the thread's rows is resident until it moves");
            assert!(
                matches!(turn.phase, Phase::Finished { .. }),
                "only a finished turn moves into history"
            );
            assert_eq!(
                self.admitted.remove(&turn.first).as_deref(),
                Some(id.as_ref()),
                "a moved turn is admitted at its first event"
            );
            assert!(
                self.starts.remove(&id).is_some(),
                "a moved turn started in the thread's rows"
            );
            self.moved
                .get_or_insert_with(|| Moved::new(window.filter))
                .insert(&id, row);
        }
    }

    fn forget_before(&mut self, seq: u64, recent_rows: usize) {
        assert!(recent_rows > 0, "a thread keeps the events of at least its last row");
        if super::sim::mutated(super::sim::Mutation::KeepAllEvents) {
            return;
        }
        let kept = |row: u64| row + recent_rows as u64 > seq;
        while self.events.front().is_some_and(|front| !kept(front.row)) {
            self.events.pop_front();
        }
        while self.facts.front().is_some_and(|front| !kept(front.row)) {
            self.facts.pop_front();
            self.fact_base += 1;
        }
    }

    pub(crate) fn weighed(&self) -> usize {
        let events: usize = self.events.iter().map(|kept| kept.event.event.get().len()).sum();
        let facts = self.facts.len() * std::mem::size_of::<KeptFact>();
        let turns: usize = self
            .turns
            .values()
            .map(|turn| turn.prompt.get().len() + turn.metadata.get().len())
            .sum();
        let stored: usize = self.stored.values().map(|value| value.len()).sum();
        let hosts: usize = self.hosts.values().map(|value| value.get().len()).sum();
        let cells: usize = self
            .cells
            .values()
            .flat_map(|updates| updates.iter())
            .map(|update| update.get().len())
            .sum();
        let moved = self.moved.as_ref().map_or(0, |moved| moved.bits.len());
        events + facts + turns + stored + hosts + cells + moved
    }

    pub fn take(&mut self, record: &Record, row: Option<u64>) -> Result<(), StoreError> {
        let at = row.unwrap_or(self.row + 1);
        match record {
            Record::Admitted {
                digest,
                first,
                hand,
                metadata,
                prompt,
                turn,
            } => self.admit(
                Turn {
                    digest: digest.clone(),
                    edits: std::collections::BTreeMap::new(),
                    first: *first,
                    hand: hand.clone(),
                    id: turn.clone(),
                    metadata: metadata.clone(),
                    phase: Phase::Queued { retry: None },
                    prompt: prompt.clone(),
                    recovered: None,
                },
                at,
            ),
            Record::Fact { fact, seq, state } => self.fact(fact, *seq, state.as_deref(), at),
            Record::Event {
                at: stamp,
                event,
                seq,
                turn,
            } => self.event(
                Event {
                    at: *stamp,
                    event: event.clone(),
                    seq: *seq,
                    turn: turn.clone(),
                },
                at,
            ),
            Record::Host { key, value } => self.host_field(key, value, at),
            Record::Status { status, turn } => self.status(turn, status, at),
            Record::Recovery {
                at,
                attempt,
                turn,
                unknown,
            } => self.recovery(turn, at.or(row), *attempt, unknown),
            record => self.keep(record),
        }
    }

    fn keep(&mut self, record: &Record) -> Result<(), StoreError> {
        match record {
            Record::Cell {
                cell,
                fresh,
                session,
                update,
            } => {
                let key = (Arc::from(session.as_str()), *cell);
                let mut updates = self.cells.get(&key).cloned().unwrap_or_default();
                if *fresh {
                    updates.clear();
                }
                updates.push_back(update.clone());
                self.cells.insert(key, updates);
                Ok(())
            }
            Record::Forget { cell, session } => {
                self.forget(session, *cell);
                Ok(())
            }
            Record::Checkpoint {
                payload,
                revision,
                state,
            } => {
                self.checkpoints.insert(
                    Arc::from(state.as_str()),
                    Arc::new(Checkpoint {
                        payload: payload.clone(),
                        revision: *revision,
                    }),
                );
                Ok(())
            }
            Record::Patched {
                base,
                edits,
                revision,
                state,
            } => self.patched(state, *base, edits, *revision),
            Record::Stored { key, value } => {
                self.stored
                    .insert(Arc::from(key.as_str()), Arc::from(value.as_str()));
                Ok(())
            }
            Record::Hold { hold, key } => self.held(key, hold),
            record => self.call(record),
        }
    }

    fn call(&mut self, record: &Record) -> Result<(), StoreError> {
        match record {
            Record::Call {
                call_id,
                effect,
                input,
                tool,
                turn,
            } => {
                self.open(call_id, *effect, input.clone(), tool, turn.clone());
                Ok(())
            }
            Record::Intended {
                call_id,
                input,
                state,
                tool,
                turn,
            } => {
                self.open(
                    call_id,
                    EffectClass::External,
                    input.clone(),
                    tool,
                    turn.clone(),
                );
                self.intents
                    .insert(Arc::from(call_id.as_str()), state.as_deref().map(Arc::from));
                Ok(())
            }
            Record::Result { call_id, .. } => {
                self.calls.remove(call_id.as_str());
                Ok(())
            }
            Record::Model { response, turn } => {
                let mut responses = self.models.get(turn.as_str()).cloned().unwrap_or_default();
                responses.push_back(response.clone());
                self.models.insert(Arc::from(turn.as_str()), responses);
                Ok(())
            }
            record => self.wake(record),
        }
    }

    fn open(
        &mut self,
        call_id: &str,
        effect: EffectClass,
        input: Box<RawValue>,
        tool: &str,
        turn: Option<String>,
    ) {
        self.calls.insert(
            Arc::from(call_id),
            Arc::new(OpenCall {
                effect,
                input,
                tool: tool.to_owned(),
                turn,
            }),
        );
    }

    fn admit(&mut self, turn: Turn, row: u64) -> Result<(), StoreError> {
        if let Some(existing) = self.turns.get(turn.id.as_str()) {
            if !matches!(existing.phase, Phase::Finished { .. }) {
                return Err(invalid(format!("turn {} was admitted twice", turn.id)));
            }
            let first = existing.first;
            self.admitted.remove(&first);
            let ended: Vec<(u64, Arc<str>)> = self
                .ends
                .iter()
                .filter(|(_, held)| held.as_ref() == turn.id)
                .cloned()
                .collect();
            for end in ended {
                self.ends.remove(&end);
            }
        }
        if self.admitted.contains_key(&turn.first) {
            return Err(invalid(format!(
                "turn {} is admitted at event {}, where another turn starts",
                turn.id, turn.first
            )));
        }
        let id: Arc<str> = Arc::from(turn.id.as_str());
        self.admitted.insert(turn.first, Arc::clone(&id));
        self.starts.insert(Arc::clone(&id), row);
        self.turns.insert(id, Arc::new(turn));
        Ok(())
    }

    fn event(&mut self, event: Event, row: u64) -> Result<(), StoreError> {
        if event.seq <= self.last_event {
            return Err(invalid(format!(
                "event {} does not follow event {}",
                event.seq, self.last_event
            )));
        }
        self.last_event = event.seq;
        self.events.push_back(Arc::new(KeptEvent { event, row }));
        Ok(())
    }

    fn fact(
        &mut self,
        fact: &StoredFact,
        seq: u64,
        state: Option<&str>,
        row: u64,
    ) -> Result<(), StoreError> {
        let next = self.next_fact();
        if seq != next {
            return Err(invalid(format!(
                "fact {seq} does not follow fact {}",
                next - 1
            )));
        }
        self.lower(seq, fact, state, row)?;
        self.named_by_values(fact);
        self.index_blob(fact);
        if let Some(state) = state {
            self.logged(state, seq);
            let bytes = serde_json::to_string(fact).map_err(invalid)?.len() as u64;
            let held = self.uncached.get(state).copied().unwrap_or_default();
            self.uncached.insert(Arc::from(state), held + bytes);
        }
        self.facts.push_back(Arc::new(KeptFact {
            fact: fact.clone(),
            state: state.map(str::to_owned),
            row,
        }));
        Ok(())
    }

    fn lower(
        &mut self,
        seq: u64,
        fact: &StoredFact,
        state: Option<&str>,
        row: u64,
    ) -> Result<(), StoreError> {
        let runtime = state.is_none();
        if !runtime && super::sim::mutated(super::sim::Mutation::ScopedIntentSkipped) {
            return Ok(());
        }
        match fact {
            Fact::Input(_) | Fact::Marker(Marker::Status { .. } | Marker::Recovery { .. })
                if !runtime =>
            {
                Ok(())
            }
            Fact::Input(input) => self.lower_input(seq, input, row),
            Fact::Marker(Marker::Status { turn, status }) => {
                self.status(turn, &lowered(status), row)
            }
            Fact::Marker(Marker::Recovery { turn, attempt, at }) => {
                self.recovery(turn, *at, *attempt, &[])
            }
            Fact::Intent(intent) => {
                let input = serde_json::value::to_raw_value(&intent.target).map_err(invalid)?;
                self.open(
                    &intent.call_id,
                    EffectClass::External,
                    input,
                    &intent.tool,
                    intent.turn.clone(),
                );
                self.intents
                    .insert(Arc::from(intent.call_id.as_str()), state.map(Arc::from));
                Ok(())
            }
            Fact::EffectResult(result) => {
                self.calls.remove(result.call_id.as_str());
                self.intents.remove(result.call_id.as_str());
                Ok(())
            }
            Fact::Decision(Decision::Host { key, value }) => {
                if internal_host(key) || key == RESERVED_KEY {
                    return Err(invalid(format!("host field {key} names a turn log key")));
                }
                self.hosts.insert(Arc::from(key.as_str()), value.clone());
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn lower_input(&mut self, seq: u64, input: &Input<Value>, row: u64) -> Result<(), StoreError> {
        let prompt = inline(&input.value)?.to_owned();
        let metadata = inline(&input.metadata)?.to_owned();
        match &input.kind {
            InputKind::Prompt => self.admit(
                Turn {
                    digest: input_digest(&prompt, input.hand.as_deref()),
                    edits: std::collections::BTreeMap::new(),
                    first: input.first.unwrap_or(seq),
                    hand: input.hand.clone(),
                    id: input.turn.clone(),
                    metadata,
                    phase: Phase::Queued { retry: None },
                    prompt,
                    recovered: None,
                },
                row,
            ),
            InputKind::Edit { edit_id } => {
                let hand = self.turn(&input.turn).and_then(|turn| turn.hand.clone());
                self.status(
                    &input.turn,
                    &Status::Replaced {
                        digest: input_digest(&prompt, hand.as_deref()),
                        edit: input_digest(&metadata, None),
                        edit_id: edit_id.clone(),
                        prompt,
                    },
                    row,
                )
            }
            InputKind::Steer { .. } => Ok(()),
        }
    }

    fn status(&mut self, id: &str, status: &Status, row: u64) -> Result<(), StoreError> {
        let Some(turn) = self.turns.get(id) else {
            return Err(invalid(format!("status of unknown turn {id}")));
        };
        if let Phase::Finished { .. } = turn.phase {
            return Err(invalid(format!("status of finished turn {id}")));
        }
        let mut turn = turn.as_ref().clone();
        moved(&mut turn, status);
        if matches!(status, Status::Finished { .. }) {
            let closed: Vec<Arc<str>> = self
                .calls
                .iter()
                .filter(|(_, call)| call.turn.as_deref() == Some(id))
                .map(|(call_id, _)| Arc::clone(call_id))
                .collect();
            for call_id in closed {
                self.calls.remove(&call_id);
            }
            if self.starts.contains_key(id) {
                self.ends.insert((row, Arc::from(id)));
            }
        }
        self.turns.insert(Arc::from(id), Arc::new(turn));
        Ok(())
    }

    fn recovery(
        &mut self,
        id: &str,
        at: Option<u64>,
        attempt: u64,
        unknown: &[String],
    ) -> Result<(), StoreError> {
        for call_id in unknown {
            self.calls.remove(call_id.as_str());
        }
        let Some(turn) = self.turns.get(id) else {
            return Err(invalid(format!("recovery of unknown turn {id}")));
        };
        if turn.digest.starts_with(super::adopt::RETIRED_DIGEST) {
            return Err(invalid(format!(
                "recovery of turn {id}, which the turn log retired"
            )));
        }
        let mut turn = turn.as_ref().clone();
        turn.recovered = Some(Recovered { at, attempt });
        self.turns.insert(Arc::from(id), Arc::new(turn));
        Ok(())
    }

    fn forget(&mut self, session: &str, cell: u64) {
        let origin = self
            .cells
            .remove(&(Arc::from(session), cell))
            .and_then(|updates| updates.front().cloned())
            .and_then(|first| {
                serde_json::from_str::<CellOrigin>(first.get())
                    .ok()
                    .and_then(|update| update.header)
                    .map(|header| format!("{}/", header.origin_call_id))
            });
        let Some(origin) = origin else {
            return;
        };
        let forgotten: Vec<Arc<str>> = self
            .calls
            .iter()
            .map(|(id, _)| id)
            .filter(|id| id.starts_with(&origin))
            .cloned()
            .collect();
        for id in forgotten {
            self.calls.remove(&id);
        }
    }

    fn patched(
        &mut self,
        state: &str,
        base: u64,
        edits: &RawValue,
        revision: u64,
    ) -> Result<(), StoreError> {
        let payload = self
            .checkpoints
            .get(state)
            .filter(|held| held.revision == base)
            .map(|held| apply_patch(&held.payload, edits))
            .ok_or_else(|| {
                invalid(format!(
                    "checkpoint {state} patches revision {base}, which the scope does not hold"
                ))
            })??;
        self.checkpoints
            .insert(Arc::from(state), Arc::new(Checkpoint { payload, revision }));
        Ok(())
    }

    fn host_field(&mut self, key: &str, value: &RawValue, row: u64) -> Result<(), StoreError> {
        if key == nanocode_turnlog::RETIRED_KEY {
            return Err(invalid(
                "the row retires turns into older rows; a resident thread keeps its turns",
            ));
        }
        if key == RESERVED_KEY {
            return self.reserve(value);
        }
        let replaced = self.hosts.insert(Arc::from(key), value.to_owned());
        if let Some(state) = key
            .strip_prefix(FOLD_SNAPSHOT_KEY)
            .and_then(|rest| rest.strip_prefix('\n'))
        {
            self.uncached.remove(state);
            self.cached.insert(Arc::from(state), row);
            let replaced = replaced.as_deref().and_then(Self::cached_blob);
            self.recached(state, replaced, value);
        }
        Ok(())
    }

    fn reserve(&mut self, value: &RawValue) -> Result<(), StoreError> {
        let reserved: Reserved = serde_json::from_str(value.get())
            .map_err(|error| invalid(format!("event reservation: {error}")))?;
        if reserved.seq < self.next_event() - 1 {
            return Err(invalid(format!(
                "event reservation through {} is below event {}",
                reserved.seq, self.last_event
            )));
        }
        if let Some(previous) = self.reserved
            && previous.epoch != reserved.epoch
        {
            self.last_event = self.last_event.max(previous.seq);
        }
        self.reserved = Some(reserved);
        Ok(())
    }

    fn held(&mut self, key: &str, hold: &Hold) -> Result<(), StoreError> {
        let previous = self.holds.get(key).map(AsRef::as_ref);
        if !hold.follows(previous) {
            return Err(invalid(format!(
                "hold {key} moves from {} to {}",
                previous.map_or("nothing", Hold::phase),
                hold.phase()
            )));
        }
        self.holds.insert(Arc::from(key), Arc::new(hold.clone()));
        Ok(())
    }

    fn wake(&mut self, record: &Record) -> Result<(), StoreError> {
        match record {
            Record::Armed {
                generation,
                key,
                wake,
            } => {
                if *generation <= self.generation {
                    return Err(invalid(format!(
                        "wake {key} generation {generation} does not follow {}",
                        self.generation
                    )));
                }
                wake.fit()
                    .map_err(|unfit| invalid(format!("wake {key} is unfit: {unfit:?}")))?;
                let heard = self
                    .wakes
                    .remove(key.as_str())
                    .map(|held| held.heard.clone())
                    .unwrap_or_default();
                self.generation = *generation;
                self.wakes.insert(
                    Arc::from(key.as_str()),
                    Arc::new(ArmedWake {
                        generation: *generation,
                        heard,
                        wake: wake.clone(),
                    }),
                );
                Ok(())
            }
            Record::Heard {
                event,
                generation,
                key,
            } => {
                let mut held = self.armed(key, *generation)?.as_ref().clone();
                held.heard.push(event.clone());
                self.wakes.insert(Arc::from(key.as_str()), Arc::new(held));
                Ok(())
            }
            Record::Fired {
                generation,
                key,
                turn,
            } => {
                if self.turn(turn).is_none() {
                    return Err(invalid(format!(
                        "wake {key} fired turn {turn}, which the scope did not admit"
                    )));
                }
                self.armed(key, *generation)?;
                self.wakes.remove(key.as_str());
                Ok(())
            }
            Record::Disarmed { generation, key } => {
                self.armed(key, *generation)?;
                self.wakes.remove(key.as_str());
                Ok(())
            }
            Record::Woken { generation } => {
                if *generation < self.generation {
                    return Err(invalid(format!(
                        "wake floor {generation} is below generation {}",
                        self.generation
                    )));
                }
                self.generation = *generation;
                Ok(())
            }
            record => Err(paged(record)),
        }
    }

    fn armed(&self, key: &str, generation: u64) -> Result<&Arc<ArmedWake>, StoreError> {
        match self.wakes.get(key) {
            Some(held) if held.generation == generation => Ok(held),
            Some(held) => Err(invalid(format!(
                "wake {key} is at generation {}, not {generation}",
                held.generation
            ))),
            None => Err(invalid(format!("wake {key} is not armed"))),
        }
    }
}

mod erased {
    pub trait Encode {
        fn json(&self) -> String;
    }

    impl<T: serde::Serialize> Encode for T {
        fn json(&self) -> String {
            serde_json::to_string(self).expect("a resident thread's values encode as JSON")
        }
    }
}

fn paged(record: &Record) -> StoreError {
    let kind = serde_json::to_value(record)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .and_then(|object| object.keys().next().cloned())
        })
        .unwrap_or_default();
    invalid(format!(
        "a {kind} record points at older rows; a resident thread folds values, so its host moves paged history on its first claim"
    ))
}

fn names(fact: &StoredFact, id: &str) -> bool {
    match fact {
        Fact::Input(input) => input.turn == id,
        Fact::Marker(Marker::Status { turn, .. } | Marker::Recovery { turn, .. }) => turn == id,
        _ => false,
    }
}

fn inline(value: &Value) -> Result<&RawValue, StoreError> {
    match value {
        Value::Inline(json) => Ok(json),
        Value::Blob(blob) => Err(invalid(format!(
            "fact value {} is a blob reference; a resident thread keeps values inline",
            blob.index()
        ))),
    }
}

fn moved(turn: &mut Turn, status: &Status) {
    match status {
        Status::Cancelling => {
            if let Phase::Running { cancelling, .. } = &mut turn.phase {
                *cancelling = true;
            }
        }
        Status::Finished {
            last,
            outcome,
            result,
        } => {
            turn.phase = Phase::Finished {
                last: *last,
                outcome: *outcome,
                result: result.clone(),
            };
            turn.prompt = RawValue::NULL.to_owned();
        }
        Status::Interrupted { note: text } => {
            if let Phase::Running { note, .. } = &mut turn.phase {
                *note = Some(text.clone());
            }
        }
        Status::Replaced {
            digest,
            edit,
            edit_id,
            prompt,
        } => {
            turn.digest.clone_from(digest);
            turn.prompt = prompt.clone();
            turn.edits.insert(edit_id.clone(), edit.clone());
        }
        Status::Retrying {
            attempts,
            error,
            kind,
            retry_at,
        } => {
            let next = Some(Retry {
                attempts: *attempts,
                error: error.clone(),
                kind: kind.clone(),
                retry_at: *retry_at,
            });
            match &mut turn.phase {
                Phase::Queued { retry } | Phase::Running { retry, .. } => *retry = next,
                Phase::Finished { .. } => {}
            }
        }
        Status::Running => {
            turn.phase = Phase::Running {
                cancelling: false,
                note: None,
                retry: None,
            };
        }
    }
}

fn lowered(status: &TurnStatus) -> Status {
    match status {
        TurnStatus::Running => Status::Running,
        TurnStatus::Cancelling => Status::Cancelling,
        TurnStatus::Interrupted { note } => Status::Interrupted { note: note.clone() },
        TurnStatus::Retrying {
            attempts,
            error,
            kind,
            retry_at,
        } => Status::Retrying {
            attempts: *attempts,
            error: error.clone(),
            kind: kind.clone(),
            retry_at: *retry_at,
        },
        TurnStatus::Finished {
            last,
            outcome,
            result,
        } => Status::Finished {
            last: *last,
            outcome: match outcome {
                crate::durability::facts::Outcome::Cancelled => Outcome::Cancelled,
                crate::durability::facts::Outcome::Completed => Outcome::Completed,
                crate::durability::facts::Outcome::Failed => Outcome::Failed,
            },
            result: result.clone(),
        },
    }
}
