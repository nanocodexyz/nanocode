use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Bound,
};

use nanocode_durability_core::{EncodedPayload, StoreError};
use serde_json::value::RawValue;

use crate::{EffectClass, Outcome, Record, Status};

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub payload: String,
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub at: u64,
    pub event: Box<RawValue>,
    pub seq: u64,
    pub turn: Option<String>,
}

#[derive(Clone, Debug)]
pub struct OpenCall {
    pub effect: EffectClass,
    pub input: Box<RawValue>,
    pub tool: String,
    pub turn: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment {
    pub first: u64,
    pub last: u64,
    pub row: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Retry {
    pub attempts: u64,
    pub error: String,
    pub kind: String,
    pub retry_at: u64,
}

#[derive(Clone, Debug)]
pub enum Phase {
    Finished {
        last: u64,
        outcome: Outcome,
        result: Box<RawValue>,
    },
    Queued {
        retry: Option<Retry>,
    },
    Running {
        cancelling: bool,
        note: Option<String>,
        retry: Option<Retry>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Section {
    pub(crate) row: u64,
    pub(crate) rows: u64,
}

#[derive(Clone, Debug)]
enum Owner {
    Cold {
        checkpoint: bool,
        pending: Vec<Record>,
        sections: Vec<Section>,
    },
    Dirty {
        sections: Vec<Section>,
    },
    Grown {
        fresh: BTreeSet<String>,
        sections: Vec<Section>,
    },
    Warm {
        sections: Vec<Section>,
    },
}

pub(crate) type Group = (Section, Vec<(String, bool)>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Recovered {
    pub at: Option<u64>,
    pub attempt: u64,
}

#[derive(Clone, Debug)]
pub struct Turn {
    pub digest: String,
    pub edits: BTreeMap<String, String>,
    pub first: u64,
    pub hand: Option<String>,
    pub id: String,
    pub metadata: Box<RawValue>,
    pub phase: Phase,
    pub prompt: Box<RawValue>,
    pub recovered: Option<Recovered>,
}

#[derive(Clone, Debug, Default)]
pub struct Fold {
    calls: BTreeMap<String, OpenCall>,
    cells: BTreeMap<(String, u64), Vec<Box<RawValue>>>,
    checkpoints: BTreeMap<String, Checkpoint>,
    dead: BTreeMap<String, String>,
    ends: u64,
    events: Vec<Event>,
    hot_bytes: usize,
    hosts: BTreeMap<String, Box<RawValue>>,
    index: BTreeMap<String, usize>,
    kept: Option<(u64, u64)>,
    last_event: u64,
    models: BTreeMap<String, Vec<Box<RawValue>>>,
    owners: BTreeMap<String, Owner>,
    segment_row: u64,
    segments: Vec<Segment>,
    seq: u64,
    stored: BTreeMap<String, String>,
    turns: Vec<Turn>,
}

#[derive(serde::Deserialize)]
struct CellHeader {
    origin_call_id: String,
}

#[derive(serde::Deserialize)]
struct CellOrigin {
    header: Option<CellHeader>,
}

fn invalid(detail: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(format!("turn log record is invalid: {detail}"))
}

fn owner_of(record: &Record) -> Option<&str> {
    match record {
        Record::Cell { session, .. } | Record::Forget { session, .. } => Some(session),
        Record::Checkpoint { state, .. } => Some(state),
        Record::Stored { key, .. } => key.split_once('\n').map(|(state, content)| {
            if content.starts_with("b3:") {
                &key[..=state.len()]
            } else {
                state
            }
        }),
        _ => None,
    }
}

pub(crate) fn appended(owner: &str) -> bool {
    owner.ends_with('\n')
}

impl Fold {
    pub fn load(&mut self, seq: u64, body: &str) -> Result<(), StoreError> {
        if seq <= self.seq {
            return Err(invalid(format!(
                "row {seq} does not follow row {}",
                self.seq
            )));
        }
        crate::record::each(body, |record| {
            if let Some(pending) = self.defer(&record) {
                pending.push(record);
                return Ok(());
            }
            self.take(record, Some(seq))
        })?;
        self.seq = seq;
        Ok(())
    }

    fn take(&mut self, record: Record, row: Option<u64>) -> Result<(), StoreError> {
        match record {
            Record::Stored { key, value } => {
                self.dead.remove(&key);
                self.stored.insert(key, value);
                Ok(())
            }
            record => self.fold_in(&record, row),
        }
    }

    fn defer(&mut self, record: &Record) -> Option<&mut Vec<Record>> {
        let owner = owner_of(record)?;
        let cold = match self.owners.get_mut(owner) {
            Some(state) => match state {
                Owner::Cold { .. } => true,
                Owner::Warm { sections } => {
                    let sections = std::mem::take(sections);
                    *state = if appended(owner) {
                        Owner::Grown {
                            fresh: BTreeSet::new(),
                            sections,
                        }
                    } else {
                        Owner::Dirty { sections }
                    };
                    false
                }
                Owner::Dirty { .. } | Owner::Grown { .. } => false,
            },
            None => {
                self.owners.insert(
                    owner.to_owned(),
                    if appended(owner) {
                        Owner::Grown {
                            fresh: BTreeSet::new(),
                            sections: Vec::new(),
                        }
                    } else {
                        Owner::Dirty {
                            sections: Vec::new(),
                        }
                    },
                );
                false
            }
        };
        if cold {
            return match self.owners.get_mut(owner) {
                Some(Owner::Cold { pending, .. }) => Some(pending),
                _ => None,
            };
        }
        if let (Record::Stored { key, .. }, Some(Owner::Grown { fresh, .. })) =
            (record, self.owners.get_mut(owner))
        {
            fresh.insert(key.clone());
        }
        None
    }

    pub(crate) fn cold(&self, owner: &str) -> Option<&[Section]> {
        match self.owners.get(owner) {
            Some(Owner::Cold { sections, .. }) => Some(sections),
            _ => None,
        }
    }

    pub(crate) fn warm(
        &mut self,
        owner: &str,
        groups: Vec<(Section, Vec<Record>)>,
    ) -> Result<(), StoreError> {
        let Some(sections) = self.cold(owner).map(<[Section]>::to_vec) else {
            return Ok(());
        };
        let mut pendings = Vec::new();
        if let Some(Owner::Cold { pending, .. }) = self
            .owners
            .insert(owner.to_owned(), Owner::Warm { sections })
        {
            pendings.push(pending);
        }
        let mut joined: BTreeMap<String, Section> = BTreeMap::new();
        for (section, records) in groups {
            for record in records {
                let Some(other) = owner_of(&record) else {
                    continue;
                };
                let take = if other == owner {
                    true
                } else if let Some(at) = joined.get(other) {
                    *at == section
                } else if matches!(self.owners.get(other), Some(Owner::Cold { sections: cold, .. }) if cold.as_slice() == [section])
                    && !(appended(other)
                        && matches!(
                            self.owners.get(&other[..other.len() - 1]),
                            Some(Owner::Cold { .. })
                        ))
                {
                    if let Some(Owner::Cold { pending, .. }) = self.owners.insert(
                        other.to_owned(),
                        Owner::Warm {
                            sections: vec![section],
                        },
                    ) {
                        pendings.push(pending);
                    }
                    joined.insert(other.to_owned(), section);
                    true
                } else {
                    false
                };
                if take {
                    self.take(record, None)?;
                }
            }
        }
        for pending in pendings {
            for record in &pending {
                self.apply(record, None)?;
            }
        }
        Ok(())
    }

    pub(crate) fn unloaded(&self) -> Vec<String> {
        self.owners
            .iter()
            .filter(|(owner, state)| {
                !appended(owner)
                    && matches!(state, Owner::Cold { pending, .. } if !pending.is_empty())
            })
            .map(|(owner, _)| owner.clone())
            .collect()
    }

    pub(crate) fn forgetting(&self) -> Vec<String> {
        self.owners
            .iter()
            .filter(|(_, state)| {
                matches!(state, Owner::Cold { pending, .. } if pending.iter().any(|record| matches!(record, Record::Forget { .. })))
            })
            .map(|(owner, _)| owner.clone())
            .collect()
    }

    pub(crate) fn writes(&self) -> Vec<&str> {
        self.owners
            .iter()
            .filter(|(owner, state)| match state {
                Owner::Cold { pending, .. } => appended(owner) && !pending.is_empty(),
                Owner::Dirty { .. } | Owner::Grown { .. } => true,
                Owner::Warm { .. } => false,
            })
            .map(|(owner, _)| owner.as_str())
            .collect()
    }

    pub(crate) fn kept_groups(&self) -> BTreeMap<u64, Group> {
        let mut groups: BTreeMap<u64, Group> = BTreeMap::new();
        for (owner, state) in &self.owners {
            let (sections, checkpoint) = match state {
                Owner::Cold {
                    checkpoint,
                    sections,
                    ..
                } => (sections, *checkpoint),
                Owner::Grown { sections, .. } | Owner::Warm { sections } => {
                    (sections, self.checkpoints.contains_key(owner))
                }
                Owner::Dirty { .. } => continue,
            };
            for section in sections {
                groups
                    .entry(section.row)
                    .or_insert_with(|| (*section, Vec::new()))
                    .1
                    .push((owner.clone(), checkpoint));
            }
        }
        groups
    }

    pub(crate) fn superseded(&self, kept: &BTreeMap<u64, Group>) -> Vec<std::ops::Range<u64>> {
        self.owners
            .values()
            .filter_map(|state| match state {
                Owner::Dirty { sections } => Some(sections),
                _ => None,
            })
            .flatten()
            .filter(|section| !kept.contains_key(&section.row))
            .map(|section| (section.row, section.rows))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .map(|(row, rows)| row..row + rows)
            .collect()
    }

    pub(crate) fn sectioned(&mut self, written: &[Group]) {
        let fresh = written
            .iter()
            .flat_map(|(section, owners)| owners.iter().map(move |(owner, _)| (owner.clone(), *section)))
            .collect::<BTreeMap<_, _>>();
        let owners = std::mem::take(&mut self.owners);
        self.owners = owners
            .into_iter()
            .filter_map(|(owner, state)| {
                let new = fresh.get(&owner).copied();
                let state = match state {
                    Owner::Dirty { .. } => Owner::Warm {
                        sections: vec![new?],
                    },
                    Owner::Grown { mut sections, .. } => {
                        sections.extend(new);
                        Owner::Warm { sections }
                    }
                    Owner::Cold {
                        checkpoint,
                        pending,
                        mut sections,
                    } if appended(&owner) && !pending.is_empty() => {
                        sections.extend(new);
                        Owner::Cold {
                            checkpoint,
                            pending: Vec::new(),
                            sections,
                        }
                    }
                    state => state,
                };
                Some((owner, state))
            })
            .collect();
    }

    pub(crate) fn owner_records(
        &self,
        owner: &str,
        mut take: impl FnMut(Record) -> Result<(), StoreError>,
    ) -> Result<bool, StoreError> {
        match self.owners.get(owner) {
            Some(Owner::Grown { fresh, .. }) => {
                for key in fresh {
                    if let Some(value) = self.stored.get(key) {
                        take(Record::Stored {
                            key: key.clone(),
                            value: value.clone(),
                        })?;
                    }
                }
                return Ok(false);
            }
            Some(Owner::Cold { pending, .. }) => {
                for record in pending {
                    take(record.clone())?;
                }
                return Ok(false);
            }
            _ => {}
        }
        let prefix = format!("{owner}\n");
        for (key, value) in self
            .stored
            .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .take_while(|(key, _)| key.starts_with(&prefix))
            .filter(|(key, _)| !key[prefix.len()..].starts_with("b3:"))
        {
            take(Record::Stored {
                key: key.clone(),
                value: value.clone(),
            })?;
        }
        let checkpoint = self.checkpoints.get(owner);
        if let Some(checkpoint) = checkpoint {
            take(Record::Checkpoint {
                payload: checkpoint.payload.clone(),
                revision: checkpoint.revision,
                state: owner.to_owned(),
            })?;
        }
        for ((_, cell), updates) in self
            .cells
            .range((owner.to_owned(), 0)..=(owner.to_owned(), u64::MAX))
        {
            for (position, update) in updates.iter().enumerate() {
                take(Record::Cell {
                    cell: *cell,
                    fresh: position == 0,
                    session: owner.to_owned(),
                    update: update.clone(),
                })?;
            }
        }
        Ok(checkpoint.is_some())
    }

    #[must_use]
    pub fn states(&self) -> Vec<&str> {
        let mut states = self
            .checkpoints
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for (owner, state) in &self.owners {
            if let Owner::Cold {
                checkpoint,
                pending,
                ..
            } = state
                && (*checkpoint
                    || pending
                        .iter()
                        .any(|record| matches!(record, Record::Checkpoint { .. })))
            {
                states.insert(owner.as_str());
            }
        }
        states.into_iter().collect()
    }

    pub(crate) const fn committed(&mut self, seq: u64) {
        self.seq = seq;
    }

    pub(crate) fn recovered_at(&mut self, turn: &str, seq: u64) {
        if let Some(turn) = self.turn_mut(turn)
            && let Some(recovered) = &mut turn.recovered
        {
            recovered.at = Some(seq);
        }
    }

    pub(crate) const fn reopen(&mut self, gap: u64) {
        self.last_event = self.last_event.saturating_add(gap);
    }

    pub(crate) const fn next_event(&mut self) -> u64 {
        self.last_event += 1;
        self.last_event
    }

    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    #[must_use]
    pub const fn last_event(&self) -> u64 {
        self.last_event
    }

    #[must_use]
    pub fn turn(&self, id: &str) -> Option<&Turn> {
        self.index.get(id).map(|&position| &self.turns[position])
    }

    fn turn_mut(&mut self, id: &str) -> Option<&mut Turn> {
        self.index
            .get(id)
            .copied()
            .map(|position| &mut self.turns[position])
    }

    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    #[must_use]
    pub const fn ends(&self) -> u64 {
        self.ends
    }

    #[must_use]
    pub const fn hot_bytes(&self) -> usize {
        self.hot_bytes
    }

    #[must_use]
    pub const fn segment_row(&self) -> u64 {
        self.segment_row
    }

    pub(crate) const fn start_segment(&mut self, row: u64) {
        self.segment_row = row;
    }

    #[must_use]
    pub fn unsealed(&self) -> Option<Segment> {
        self.kept.map(|(first, last)| Segment {
            first,
            last,
            row: self.segment_row,
        })
    }

    pub(crate) fn release(&mut self) {
        let events = std::mem::take(&mut self.events);
        if let (Some(first), Some(last)) = (events.first(), events.last()) {
            self.kept = Some((self.kept.map_or(first.seq, |(first, _)| first), last.seq));
        }
    }

    fn sealed(&self, keep: u64) -> Option<Segment> {
        let first = self
            .kept
            .map(|(first, _)| first)
            .or_else(|| self.events.first().map(|event| event.seq))?;
        let last = self
            .events
            .iter()
            .take_while(|event| event.seq < keep)
            .last()
            .map(|event| event.seq)
            .or_else(|| self.kept.map(|(_, last)| last))?;
        Some(Segment {
            first,
            last,
            row: self.segment_row,
        })
    }

    fn garbage(&self, collectable: impl Fn(&str) -> bool) -> Option<Vec<bool>> {
        let mut firsts: HashMap<&str, usize> = HashMap::new();
        let mut holders = Vec::new();
        for (position, (key, value)) in self.stored.iter().enumerate() {
            if let Some((state, content)) = key.split_once('\n')
                && content.starts_with("b3:")
                && collectable(state)
            {
                let first = *firsts.entry(content).or_insert(holders.len());
                holders.push((first, position, state, value.as_str()));
            }
        }
        let mut queue = self
            .checkpoints
            .iter()
            .filter(|(state, _)| collectable(state))
            .flat_map(|(_, checkpoint)| EncodedPayload::references(&checkpoint.payload))
            .collect::<Vec<_>>();
        let mut visited = vec![false; holders.len()];
        while let Some(content) = queue.pop() {
            let Some(&first) = firsts.get(content) else {
                continue;
            };
            if std::mem::replace(&mut visited[first], true) {
                continue;
            }
            let (_, _, state, value) = holders[first];
            let chunk = |chunk: &str| {
                self.stored
                    .get(&format!("{state}\n{chunk}"))
                    .map(String::as_str)
            };
            match EncodedPayload::assembled(value, &chunk) {
                Some(Cow::Borrowed(json)) => queue.extend(EncodedPayload::references(json)),
                Some(Cow::Owned(json)) => queue.extend(
                    EncodedPayload::references(&json)
                        .filter_map(|content| firsts.get_key_value(content))
                        .map(|(content, _)| *content),
                ),
                None => return None,
            }
        }
        let mut garbage = vec![false; self.stored.len()];
        for &(first, position, _, _) in &holders {
            garbage[position] = !visited[first];
        }
        Some(garbage)
    }

    pub(crate) fn collect(&mut self, collectable: &BTreeSet<String>) -> usize {
        let in_collectable = |key: &str| {
            key.split_once('\n')
                .is_some_and(|(state, _)| collectable.contains(state))
        };
        let before = self.dead.len();
        self.dead.retain(|key, _| !in_collectable(key));
        let mut collected = before - self.dead.len();
        let Some(garbage) = self.garbage(|_| true) else {
            return collected;
        };
        let mut garbage = garbage.into_iter();
        let unreached = self
            .stored
            .extract_if(.., |_, _| garbage.next() == Some(true))
            .collect::<Vec<_>>();
        collected += unreached.len();
        self.dead.extend(
            unreached
                .into_iter()
                .filter(|(key, _)| !in_collectable(key)),
        );
        collected
    }

    fn references_of(&self, state: &str, value: &str) -> Option<Vec<String>> {
        let chunk = |chunk: &str| {
            self.stored
                .get(&format!("{state}\n{chunk}"))
                .map(String::as_str)
        };
        EncodedPayload::assembled(value, &chunk).map(|json| {
            EncodedPayload::references(&json)
                .map(|content| format!("{state}\n{content}"))
                .collect()
        })
    }

    fn holds_dead(&self, state: &str) -> bool {
        let prefix = format!("{state}\n");
        self.dead
            .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .next()
            .is_some_and(|(key, _)| key.starts_with(&prefix))
    }

    pub(crate) fn revive(&mut self, pushed: &[Record]) -> Vec<Record> {
        if self.dead.is_empty() {
            return Vec::new();
        }
        let mut queue = Vec::new();
        for record in pushed {
            match record {
                Record::Checkpoint { payload, state, .. } if self.holds_dead(state) => queue
                    .extend(
                        EncodedPayload::references(payload)
                            .map(|content| format!("{state}\n{content}")),
                    ),
                Record::Stored { key, value } => {
                    if let Some((state, content)) = key.split_once('\n')
                        && content.starts_with("b3:")
                        && self.holds_dead(state)
                    {
                        queue.extend(self.references_of(state, value).into_iter().flatten());
                    }
                }
                _ => {}
            }
        }
        let mut revived = Vec::new();
        while let Some(key) = queue.pop() {
            let Some((key, value)) = self.dead.remove_entry(&key) else {
                continue;
            };
            if let Some((state, _)) = key.split_once('\n') {
                queue.extend(self.references_of(state, &value).into_iter().flatten());
            }
            let record = Record::Stored {
                key: key.clone(),
                value: value.clone(),
            };
            match self.defer(&record) {
                Some(pending) => pending.push(record.clone()),
                None => {
                    self.stored.insert(key, value);
                }
            }
            revived.push(record);
        }
        revived
    }

    fn drop_turns(&mut self, drop: impl Fn(&Turn) -> bool) {
        let models = &mut self.models;
        self.turns.retain(|turn| {
            let gone = drop(turn);
            if gone {
                models.remove(&turn.id);
            }
            !gone
        });
        self.index = self
            .turns
            .iter()
            .enumerate()
            .map(|(position, turn)| (turn.id.clone(), position))
            .collect();
    }

    pub(crate) fn unadmit(&mut self, id: &str, seq: u64) {
        self.drop_turns(|turn| turn.id == id);
        if let Some(position) = self.events.iter().rposition(|event| event.seq == seq) {
            self.hot_bytes -= self.events.remove(position).event.get().len();
        }
    }

    pub(crate) fn retire(&mut self, keep: usize) -> usize {
        let mut finished = self
            .turns
            .iter()
            .filter_map(|turn| match turn.phase {
                Phase::Finished { last, .. } => Some(last),
                _ => None,
            })
            .collect::<Vec<_>>();
        let excess = finished.len().saturating_sub(keep);
        if excess == 0 {
            return 0;
        }
        finished.sort_unstable();
        let cutoff = finished[excess - 1];
        self.drop_turns(|turn| matches!(turn.phase, Phase::Finished { last, .. } if last <= cutoff));
        excess
    }

    pub(crate) fn seal(&mut self, row: u64, keep: u64) {
        if let Some(segment) = self.sealed(keep) {
            self.segments.push(segment);
            self.kept = None;
            self.events.retain(|event| event.seq >= keep);
            self.hot_bytes = self
                .events
                .iter()
                .map(|event| event.event.get().len())
                .sum();
        }
        self.segment_row = row;
    }

    #[must_use]
    pub fn checkpoint(&self, state: &str) -> Option<&Checkpoint> {
        self.checkpoints.get(state)
    }

    pub fn checkpoints(&self) -> impl Iterator<Item = (&str, &Checkpoint)> {
        self.checkpoints
            .iter()
            .map(|(state, checkpoint)| (state.as_str(), checkpoint))
    }

    #[must_use]
    pub fn stored(&self, key: &str) -> Option<&str> {
        self.stored
            .get(key)
            .or_else(|| self.dead.get(key))
            .map(String::as_str)
    }

    #[must_use]
    pub fn host(&self, key: &str) -> Option<&RawValue> {
        self.hosts.get(key).map(AsRef::as_ref)
    }

    pub fn cells(&self) -> impl Iterator<Item = (&str, u64, &[Box<RawValue>])> {
        self.cells
            .iter()
            .map(|((session, cell), updates)| (session.as_str(), *cell, updates.as_slice()))
    }

    pub fn cells_of(
        &self,
        session: Option<&str>,
    ) -> impl Iterator<Item = (&str, u64, &[Box<RawValue>])> {
        let range = match session {
            Some(session) => (
                Bound::Included((session.to_owned(), 0)),
                Bound::Included((session.to_owned(), u64::MAX)),
            ),
            None => (Bound::Unbounded, Bound::Unbounded),
        };
        self.cells
            .range(range)
            .map(|((session, cell), updates)| (session.as_str(), *cell, updates.as_slice()))
    }

    #[must_use]
    pub fn models(&self, turn: &str) -> &[Box<RawValue>] {
        self.models.get(turn).map_or(&[], Vec::as_slice)
    }

    pub fn open_calls(&self) -> impl Iterator<Item = (&str, &OpenCall)> {
        self.calls.iter().map(|(id, call)| (id.as_str(), call))
    }

    #[must_use]
    pub fn unknown(&self) -> Vec<String> {
        self.calls
            .iter()
            .filter(|(_, call)| call.effect.needs_row())
            .map(|(id, _)| id.clone())
            .collect()
    }

    #[must_use]
    pub fn recovering(&self) -> bool {
        self.turns.iter().any(|turn| {
            turn.recovered
                .is_some_and(|recovered| recovered.at == Some(self.seq))
        })
    }

    #[must_use]
    pub fn next_attempt(&self, turn: &str) -> u64 {
        match self.turn(turn).and_then(|turn| turn.recovered) {
            Some(Recovered {
                at: Some(at),
                attempt,
            }) if self.seq <= at => attempt + 1,
            Some(Recovered { at: None, attempt }) => attempt + 1,
            _ => 1,
        }
    }

    pub fn apply(&mut self, record: &Record, row: Option<u64>) -> Result<(), StoreError> {
        if let Some(pending) = self.defer(record) {
            pending.push(record.clone());
            return Ok(());
        }
        self.fold_in(record, row)
    }

    fn fold_in(&mut self, record: &Record, row: Option<u64>) -> Result<(), StoreError> {
        match record {
            Record::Admitted {
                digest,
                first,
                hand,
                metadata,
                prompt,
                turn,
            } => {
                if let Some(existing) = self.turn(turn) {
                    if !matches!(existing.phase, Phase::Finished { .. }) {
                        return Err(invalid(format!("turn {turn} was admitted twice")));
                    }
                    self.drop_turns(|old| old.id == *turn);
                }
                self.index.insert(turn.clone(), self.turns.len());
                self.turns.push(Turn {
                    digest: digest.clone(),
                    edits: BTreeMap::new(),
                    first: *first,
                    hand: hand.clone(),
                    id: turn.clone(),
                    metadata: metadata.clone(),
                    phase: Phase::Queued { retry: None },
                    prompt: prompt.clone(),
                    recovered: None,
                });
            }
            Record::Call {
                call_id,
                effect,
                input,
                tool,
                turn,
            } => {
                self.calls.insert(
                    call_id.clone(),
                    OpenCall {
                        effect: *effect,
                        input: input.clone(),
                        tool: tool.clone(),
                        turn: turn.clone(),
                    },
                );
            }
            Record::Cell {
                cell,
                fresh,
                session,
                update,
            } => {
                let updates = self.cells.entry((session.clone(), *cell)).or_default();
                if *fresh {
                    updates.clear();
                }
                updates.push(update.clone());
            }
            Record::Checkpoint {
                payload,
                revision,
                state,
            } => {
                self.checkpoints.insert(
                    state.clone(),
                    Checkpoint {
                        payload: payload.clone(),
                        revision: *revision,
                    },
                );
            }
            Record::Event {
                at,
                event,
                seq,
                turn,
            } => {
                if let Some(last) = self
                    .events
                    .last()
                    .map(|event| event.seq)
                    .or_else(|| self.kept.map(|(_, last)| last))
                    && *seq <= last
                {
                    return Err(invalid(format!("event {seq} does not follow event {last}")));
                }
                self.last_event = self.last_event.max(*seq);
                self.ends = self.ends.max(*seq);
                self.hot_bytes += event.get().len();
                self.events.push(Event {
                    at: *at,
                    event: event.clone(),
                    seq: *seq,
                    turn: turn.clone(),
                });
            }
            Record::Floor { seq } => {
                self.last_event = self.last_event.max(*seq);
            }
            Record::Forget { cell, session } => {
                let origin = self
                    .cells
                    .remove(&(session.clone(), *cell))
                    .and_then(|updates| updates.into_iter().next())
                    .and_then(|first| {
                        serde_json::from_str::<CellOrigin>(first.get())
                            .ok()
                            .and_then(|update| update.header)
                            .map(|header| format!("{}/", header.origin_call_id))
                    });
                if let Some(origin) = origin {
                    self.calls.retain(|id, _| !id.starts_with(&origin));
                }
            }
            Record::Host { key, value } => {
                self.hosts.insert(key.clone(), value.clone());
            }
            Record::Model { response, turn } => {
                self.models
                    .entry(turn.clone())
                    .or_default()
                    .push(response.clone());
            }
            Record::Recovery {
                at,
                attempt,
                turn,
                unknown,
            } => {
                for call_id in unknown {
                    self.calls.remove(call_id);
                }
                let Some(found) = self.turn_mut(turn) else {
                    return Err(invalid(format!("recovery of unknown turn {turn}")));
                };
                found.recovered = Some(Recovered {
                    at: at.or(row),
                    attempt: *attempt,
                });
            }
            Record::Result { call_id, .. } => {
                self.calls.remove(call_id);
            }
            Record::Section { owners, row, rows } => {
                let section = Section {
                    row: *row,
                    rows: *rows,
                };
                for (owner, checkpoint) in owners {
                    if let Owner::Cold {
                        checkpoint: kept,
                        sections,
                        ..
                    } = self
                        .owners
                        .entry(owner.clone())
                        .or_insert_with(|| Owner::Cold {
                            checkpoint: false,
                            pending: Vec::new(),
                            sections: Vec::new(),
                        })
                    {
                        *kept |= *checkpoint;
                        sections.push(section);
                    }
                }
            }
            Record::Segment { first, last, row } => {
                self.ends = self.ends.max(*last);
                self.segments.push(Segment {
                    first: *first,
                    last: *last,
                    row: *row,
                });
            }
            Record::Status { status, turn } => self.status(turn, status)?,
            Record::Stored { key, value } => {
                self.dead.remove(key);
                self.stored.insert(key.clone(), value.clone());
            }
        }
        Ok(())
    }

    fn status(&mut self, id: &str, status: &Status) -> Result<(), StoreError> {
        let Some(turn) = self.turn_mut(id) else {
            return Err(invalid(format!("status of unknown turn {id}")));
        };
        if let Phase::Finished { .. } = turn.phase {
            return Err(invalid(format!("status of finished turn {id}")));
        }
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
                self.calls
                    .retain(|_, call| call.turn.as_deref() != Some(id));
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
        Ok(())
    }

    pub fn snapshot(&self, keep: u64) -> Result<String, StoreError> {
        let mut body = String::new();
        self.snapshot_records(keep, |record| {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&crate::encode(std::slice::from_ref(&record))?);
            Ok(())
        })?;
        Ok(body)
    }

    pub(crate) fn snapshot_records(
        &self,
        keep: u64,
        take: impl FnMut(Record) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        self.records(keep, true, take)
    }

    pub(crate) fn head_records(
        &self,
        keep: u64,
        groups: &[Group],
        mut take: impl FnMut(Record) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        for (section, owners) in groups {
            take(Record::Section {
                owners: owners.clone(),
                row: section.row,
                rows: section.rows,
            })?;
        }
        self.records(keep, false, take)
    }

    fn records(
        &self,
        keep: u64,
        owned: bool,
        mut take: impl FnMut(Record) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        for segment in self.segments.iter().copied().chain(self.sealed(keep)) {
            take(Record::Segment {
                first: segment.first,
                last: segment.last,
                row: segment.row,
            })?;
        }
        for (key, value) in &self.stored {
            if !owned && key.contains('\n') {
                continue;
            }
            take(Record::Stored {
                key: key.clone(),
                value: value.clone(),
            })?;
        }
        for (state, checkpoint) in self.checkpoints.iter().filter(|_| owned) {
            take(Record::Checkpoint {
                payload: checkpoint.payload.clone(),
                revision: checkpoint.revision,
                state: state.clone(),
            })?;
        }
        for turn in &self.turns {
            take(Record::Admitted {
                digest: turn.digest.clone(),
                first: turn.first,
                hand: turn.hand.clone(),
                metadata: turn.metadata.clone(),
                prompt: turn.prompt.clone(),
                turn: turn.id.clone(),
            })?;
            let status = |status| Record::Status {
                status,
                turn: turn.id.clone(),
            };
            for (edit_id, edit) in &turn.edits {
                take(status(Status::Replaced {
                    digest: turn.digest.clone(),
                    edit: edit.clone(),
                    edit_id: edit_id.clone(),
                    prompt: turn.prompt.clone(),
                }))?;
            }
            match &turn.phase {
                Phase::Queued { retry } => {
                    if let Some(retry) = retry {
                        take(status(retrying(retry)))?;
                    }
                }
                Phase::Running {
                    cancelling,
                    note,
                    retry,
                } => {
                    take(status(Status::Running))?;
                    if *cancelling {
                        take(status(Status::Cancelling))?;
                    }
                    if let Some(note) = note {
                        take(status(Status::Interrupted { note: note.clone() }))?;
                    }
                    if let Some(retry) = retry {
                        take(status(retrying(retry)))?;
                    }
                }
                Phase::Finished {
                    last,
                    outcome,
                    result,
                } => take(status(Status::Finished {
                    last: *last,
                    outcome: *outcome,
                    result: result.clone(),
                }))?,
            }
            if let Some(recovered) = turn.recovered {
                take(Record::Recovery {
                    at: recovered.at,
                    attempt: recovered.attempt,
                    turn: turn.id.clone(),
                    unknown: Vec::new(),
                })?;
            }
            for response in self.models(&turn.id) {
                take(Record::Model {
                    response: response.clone(),
                    turn: turn.id.clone(),
                })?;
            }
        }
        for (call_id, call) in &self.calls {
            take(Record::Call {
                call_id: call_id.clone(),
                effect: call.effect,
                input: call.input.clone(),
                tool: call.tool.clone(),
                turn: call.turn.clone(),
            })?;
        }
        for ((session, cell), updates) in self.cells.iter().filter(|_| owned) {
            for (position, update) in updates.iter().enumerate() {
                take(Record::Cell {
                    cell: *cell,
                    fresh: position == 0,
                    session: session.clone(),
                    update: update.clone(),
                })?;
            }
        }
        for (key, value) in &self.hosts {
            take(Record::Host {
                key: key.clone(),
                value: value.clone(),
            })?;
        }
        for event in self.events.iter().filter(|event| event.seq >= keep) {
            take(Record::Event {
                at: event.at,
                event: event.event.clone(),
                seq: event.seq,
                turn: event.turn.clone(),
            })?;
        }
        take(Record::Floor {
            seq: self.last_event,
        })?;
        Ok(())
    }
}

fn retrying(retry: &Retry) -> Status {
    Status::Retrying {
        attempts: retry.attempts,
        error: retry.error.clone(),
        kind: retry.kind.clone(),
        retry_at: retry.retry_at,
    }
}
