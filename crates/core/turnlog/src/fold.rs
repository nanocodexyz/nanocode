use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap},
    hash::{BuildHasher, BuildHasherDefault, DefaultHasher},
    ops::Bound,
};

use nanocode_durability_core::{
    BlobRef, Decision, Digest, EncodedPayload, Fact, FoldError, Input, InputKind, Marker,
    Outcome as FactOutcome, Snapshot, StoreError, StoredFact, TurnStatus, Value, Values,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::{ArmedWake, EffectClass, Hold, Outcome, Record, Status, Wakes};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Checkpoint {
    pub payload: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub at: u64,
    pub event: Box<RawValue>,
    pub seq: u64,
    pub turn: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Retry {
    pub attempts: u64,
    pub error: String,
    pub kind: String,
    pub retry_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) checkpoint: bool,
    pub(crate) full: u64,
    pub(crate) legacy: bool,
    pub(crate) sections: Vec<Section>,
    pub(crate) written: u64,
}

#[derive(Clone, Debug, Default)]
struct Delta {
    cells: BTreeMap<u64, bool>,
    checkpoint: bool,
    keys: BTreeSet<String>,
}

impl Delta {
    fn note(&mut self, record: &Record) {
        match record {
            Record::Cell { cell, .. } => {
                self.cells.entry(*cell).or_insert(false);
            }
            Record::Forget { cell, .. } => {
                self.cells.insert(*cell, true);
            }
            Record::Checkpoint { .. } | Record::Patched { .. } => self.checkpoint = true,
            Record::Stored { key, .. } if !self.keys.contains(key) => {
                self.keys.insert(key.clone());
            }
            _ => {}
        }
    }

    fn is_empty(&self) -> bool {
        self.cells.is_empty() && !self.checkpoint && self.keys.is_empty()
    }
}

#[derive(Clone, Debug)]
enum Owner {
    Cold { entry: Entry, pending: Vec<Record> },
    Waiting { pending: Vec<Record> },
    Warm { delta: Delta, entry: Entry },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Page {
    Cold(Section),
    Loaded(Section),
}

impl Page {
    pub(crate) const fn section(self) -> Section {
        match self {
            Self::Cold(section) | Self::Loaded(section) => section,
        }
    }
}

pub(crate) struct Plan {
    pub(crate) groups: BTreeMap<u64, (u64, u64)>,
    pub(crate) kept: BTreeMap<String, Page>,
    pub(crate) pages: Vec<(String, Vec<Record>)>,
    pub(crate) superseded: Vec<std::ops::Range<u64>>,
}

#[derive(Clone, Debug)]
struct Shelf {
    bytes: Box<[u64]>,
    ends: Box<[usize]>,
    keys: Box<str>,
}

impl Shelf {
    fn key(&self, index: usize) -> &str {
        let start = index.checked_sub(1).map_or(0, |before| self.ends[before]);
        &self.keys[start..self.ends[index]]
    }

    fn position(&self, owner: &str) -> Result<usize, usize> {
        let (mut low, mut high) = (0, self.ends.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.key(middle).cmp(owner) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(middle),
            }
        }
        Err(low)
    }

    fn holds_state(&self, state: &str) -> bool {
        let prefix = format!("{state}\n");
        let at = self.position(&prefix).unwrap_or_else(|at| at);
        at < self.ends.len() && self.key(at).starts_with(&prefix)
    }
}

#[derive(Clone, Debug)]
struct Links {
    chunks: Vec<String>,
    references: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Retired {
    pub digest: u128,
    pub first: u64,
    pub hand: Option<Box<str>>,
    pub last: u64,
    pub outcome: Outcome,
}

pub const RETIRED_KEY: &str = "$turnlog_retired";
pub const FOLD_SNAPSHOT_KEY: &str = "$fold_snapshot";
pub const UNPINNED_RECOVERY_ROW: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CachedAt {
    pub blob: Digest,
    pub bytes: u64,
}

pub enum CachedFold {
    At(CachedAt),
    Inline(Snapshot),
}

fn cached_at(value: &RawValue) -> Option<CachedAt> {
    serde_json::from_str(value.get()).ok()
}

#[must_use]
pub fn tag(text: &str) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    text.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u128::from(byte)).wrapping_mul(PRIME)
    })
}

const RETIRED_ROW: usize = 97;

fn text(row: &[u8], from: usize, to: usize) -> Result<&str, StoreError> {
    std::str::from_utf8(&row[from..to]).map_err(invalid)
}

#[derive(serde::Deserialize, serde::Serialize)]
struct RetiredIndex {
    hands: BTreeMap<String, Box<str>>,
    rows: String,
}

const fn outcome_code(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Cancelled => "x",
        Outcome::Completed => "c",
        Outcome::Failed => "f",
    }
}

fn outcome_of(code: &str) -> Option<Outcome> {
    match code {
        "x" => Some(Outcome::Cancelled),
        "c" => Some(Outcome::Completed),
        "f" => Some(Outcome::Failed),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Recovered {
    pub at: Option<u64>,
    pub attempt: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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
    chains: BTreeMap<u64, u64>,
    cells: BTreeMap<(String, u64), Vec<Box<RawValue>>>,
    checkpoints: BTreeMap<String, Checkpoint>,
    dead: BTreeMap<String, String>,
    ends: u64,
    events: Vec<Event>,
    facts: Vec<StoredFact>,
    fact_states: Vec<Option<u32>>,
    fact_base: u64,
    fact_row: u64,
    fact_segments: Vec<Segment>,
    folded_through: u64,
    uncached: BTreeMap<String, u64>,
    warmed: Vec<String>,
    blob_seqs: Vec<u64>,
    unsequenced_blobs: u32,
    state_names: Vec<String>,
    state_index: BTreeMap<String, u32>,
    derived_calls: BTreeMap<String, Option<u32>>,
    values: Values,
    filters: BTreeMap<u64, Box<[u64]>>,
    forgets: BTreeSet<String>,
    groups: BTreeMap<u64, (u64, u64)>,
    holds: BTreeMap<String, Hold>,
    hot_bytes: usize,
    hosts: BTreeMap<String, Box<RawValue>>,
    index: BTreeMap<String, usize>,
    kept: Option<(u64, u64)>,
    last_event: u64,
    links: HashMap<String, Links>,
    marks: BTreeMap<String, Vec<String>>,
    models: BTreeMap<String, Vec<Box<RawValue>>>,
    owners: BTreeMap<String, Owner>,
    pages: BTreeMap<String, Page>,
    placed: BTreeMap<u64, (u64, u64)>,
    read: BTreeSet<String>,
    root: Option<Page>,
    segment_row: u64,
    segments: Vec<Segment>,
    shelved: BTreeMap<u64, Shelf>,
    seq: u64,
    stored: BTreeMap<String, String>,
    retired: BTreeMap<u128, Retired>,
    turns: Vec<Turn>,
    wakes: Wakes,
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
        Record::Checkpoint { state, .. } | Record::Patched { state, .. } => Some(state),
        Record::Stored { key, .. } => key.contains('\n').then_some(key.as_str()),
        _ => None,
    }
}

fn legacy_owner_of(record: &Record) -> Option<&str> {
    match record {
        Record::Stored { key, .. } => key.split_once('\n').map(|(state, content)| {
            if content.starts_with("b3:") {
                &key[..=state.len()]
            } else {
                state
            }
        }),
        record => owner_of(record),
    }
}

pub(crate) fn legacy_name(key: &str) -> Option<String> {
    let record = Record::Stored {
        key: key.to_owned(),
        value: String::new(),
    };
    legacy_owner_of(&record)
        .filter(|owner| *owner != key)
        .map(str::to_owned)
}

fn key_hash(owner: &str) -> u64 {
    BuildHasherDefault::<DefaultHasher>::default().hash_one(owner)
}

fn unheld(state: &str, base: u64) -> StoreError {
    invalid(format!(
        "checkpoint {state} patches revision {base}, which the log does not hold"
    ))
}

fn wait(pending: &mut Vec<Record>, record: Record) -> Result<(), StoreError> {
    let record = match record {
        Record::Patched {
            base,
            edits,
            revision,
            state,
        } => {
            let payload = pending
                .iter()
                .rev()
                .find_map(|waiting| match waiting {
                    Record::Checkpoint {
                        payload, revision, ..
                    } => Some((payload, *revision)),
                    _ => None,
                })
                .filter(|(_, held)| *held == base)
                .map(|(payload, _)| crate::patch::apply(payload, &edits))
                .ok_or_else(|| unheld(&state, base))??;
            Record::Checkpoint {
                payload,
                revision,
                state,
            }
        }
        record => record,
    };
    if matches!(record, Record::Checkpoint { .. }) {
        pending.retain(|waiting| !matches!(waiting, Record::Checkpoint { .. }));
    }
    pending.push(record);
    Ok(())
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
            let forget = match &record {
                Record::Forget { session, .. } => Some(session.clone()),
                _ => None,
            };
            if let Some(pending) = self.defer(&record) {
                wait(pending, record)?;
                if let Some(session) = forget {
                    self.forgets.insert(session);
                }
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

    fn page_of(&self, owner: &str) -> Option<(&String, &Page)> {
        self.pages
            .range::<str, _>((Bound::Unbounded, Bound::Included(owner)))
            .next_back()
    }

    fn cold_page(&self, owner: &str) -> Option<(&String, Section)> {
        match self.page_of(owner) {
            Some((first, Page::Cold(section)))
                if self
                    .filters
                    .get(&section.row)
                    .is_none_or(|filter| filter.binary_search(&key_hash(owner)).is_ok()) =>
            {
                Some((first, *section))
            }
            _ => None,
        }
    }

    fn page_end(&self, first: &str) -> Option<&String> {
        self.pages
            .range::<str, _>((Bound::Excluded(first), Bound::Unbounded))
            .next()
            .map(|(next, _)| next)
    }

    fn defer(&mut self, record: &Record) -> Option<&mut Vec<Record>> {
        let owner = owner_of(record)?;
        self.unshelve_owner(owner);
        if !self.owners.contains_key(owner) {
            let state =
                if matches!(self.root, Some(Page::Cold(_))) || self.cold_page(owner).is_some() {
                    Owner::Waiting {
                        pending: Vec::new(),
                    }
                } else {
                    Owner::Warm {
                        delta: Delta::default(),
                        entry: Entry::default(),
                    }
                };
            self.owners.insert(owner.to_owned(), state);
        }
        match self.owners.get_mut(owner) {
            Some(Owner::Cold { pending, .. } | Owner::Waiting { pending }) => Some(pending),
            Some(Owner::Warm { delta, .. }) => {
                delta.note(record);
                None
            }
            None => None,
        }
    }

    pub(crate) fn unpaged(&self, owner: &str) -> Option<(String, Section)> {
        self.cold_page(owner)
            .map(|(first, section)| (first.clone(), section))
    }

    pub(crate) fn group_pages(&self, records: &[Record]) -> Vec<(String, Section)> {
        let mut pages = BTreeMap::new();
        for owner in records.iter().filter_map(owner_of) {
            if matches!(self.owners.get(owner), None | Some(Owner::Waiting { .. }))
                && let Some((first, section)) = self.cold_page(owner)
            {
                pages.insert(first, section);
            }
        }
        pages
            .into_iter()
            .map(|(first, section)| (first.clone(), section))
            .collect()
    }

    pub(crate) const fn cold_root(&self) -> Option<Section> {
        match self.root {
            Some(Page::Cold(section)) => Some(section),
            Some(Page::Loaded(_)) | None => None,
        }
    }

    pub(crate) fn rooted(&mut self, records: Vec<Record>) -> Result<(), StoreError> {
        let Some(Page::Cold(section)) = self.root else {
            return Ok(());
        };
        for record in records {
            match record {
                Record::Groups { groups, chains } => {
                    self.groups = groups
                        .into_iter()
                        .map(|(row, rows, count)| (row, (rows, count)))
                        .collect();
                    self.chains.extend(chains);
                }
                Record::Index { first, row, rows } => {
                    self.pages.insert(first, Page::Cold(Section { row, rows }));
                }
                _ => {
                    return Err(invalid(
                        "an index root holds a record that is not an index page or a group",
                    ));
                }
            }
        }
        self.root = Some(Page::Loaded(section));
        Ok(())
    }

    pub(crate) fn indexed(
        &self,
        pages: &BTreeMap<String, Page>,
        groups: &BTreeMap<u64, (u64, u64)>,
    ) -> Option<Section> {
        let Some(Page::Loaded(section)) = self.root else {
            return None;
        };
        (self.groups == *groups
            && self.pages.len() == pages.len()
            && self
                .pages
                .iter()
                .zip(pages)
                .all(|((first, page), (other, kept))| {
                    first == other && page.section() == kept.section()
                }))
        .then_some(section)
    }

    pub(crate) fn root_section(&self) -> Option<Section> {
        self.root.map(Page::section)
    }

    pub(crate) fn cold_pages(&self) -> Vec<(String, Section)> {
        self.pages
            .iter()
            .filter_map(|(first, page)| match page {
                Page::Cold(section) => Some((first.clone(), *section)),
                Page::Loaded(_) => None,
            })
            .collect()
    }

    pub(crate) fn unlisted(&self) -> Vec<(String, Section)> {
        let mut pages = BTreeMap::new();
        for (owner, state) in &self.owners {
            if matches!(state, Owner::Warm { delta, .. } if !delta.is_empty())
                && let Some((first, Page::Cold(section))) = self.page_of(owner)
            {
                pages.insert(first.clone(), *section);
            }
        }
        for owner in self.unindexed() {
            if let Some((first, Page::Cold(section))) = self.page_of(owner) {
                pages.insert(first.clone(), *section);
            }
        }
        pages.into_iter().collect()
    }

    pub(crate) fn paged(&mut self, first: &str, records: Vec<Record>) -> Result<(), StoreError> {
        let Some(page) = self.pages.get_mut(first) else {
            return Ok(());
        };
        let Page::Cold(section) = *page else {
            return Ok(());
        };
        *page = Page::Loaded(section);
        self.filters.remove(&section.row);
        let end = self.page_end(first).cloned();
        for record in records {
            let Record::Owner {
                checkpoint,
                full,
                legacy,
                owner,
                sections,
                written,
            } = record
            else {
                return Err(invalid("an index page holds a record that is not an owner"));
            };
            if owner.as_str() < first || end.as_ref().is_some_and(|end| owner >= *end) {
                return Err(invalid(format!(
                    "index page {first} holds owner {owner} outside its range"
                )));
            }
            let entry = Entry {
                checkpoint,
                full,
                legacy,
                sections: sections
                    .into_iter()
                    .map(|(row, rows)| Section { row, rows })
                    .collect(),
                written,
            };
            let pending = match self.owners.remove(&owner) {
                None => Vec::new(),
                Some(Owner::Waiting { pending }) => pending,
                Some(_) => {
                    return Err(invalid(format!(
                        "owner {owner} was loaded before its index page"
                    )));
                }
            };
            self.owners.insert(owner, Owner::Cold { entry, pending });
        }
        let fresh = self
            .owners
            .range::<str, _>((
                Bound::Included(first),
                end.as_deref().map_or(Bound::Unbounded, Bound::Excluded),
            ))
            .filter(|(_, state)| matches!(state, Owner::Waiting { .. }))
            .map(|(owner, _)| owner.clone())
            .collect::<Vec<_>>();
        for owner in fresh {
            if let Some(Owner::Waiting { pending }) = self.owners.remove(&owner) {
                for record in &pending {
                    self.apply(record, None)?;
                }
            }
        }
        Ok(())
    }

    fn shelf_of(&self, owner: &str) -> Option<u64> {
        self.shelved
            .iter()
            .find(|(_, shelf)| shelf.position(owner).is_ok())
            .map(|(row, _)| *row)
    }

    fn unshelve_row(&mut self, row: u64) {
        let Some(shelf) = self.shelved.remove(&row) else {
            return;
        };
        let Some((rows, count)) = self.placed.get(&row).copied() else {
            panic!("turn log shelved row {row}, which it did not place since the last snapshot");
        };
        assert!(
            count == shelf.ends.len() as u64,
            "turn log shelved {} owners of row {row}, which holds {count}",
            shelf.ends.len()
        );
        let section = Section { row, rows };
        for (index, bytes) in shelf.bytes.iter().enumerate() {
            let owner = shelf.key(index);
            assert!(
                !self.owners.contains_key(owner),
                "turn log owner {owner} is resident while its row {row} is shelved"
            );
            self.owners.insert(
                owner.to_owned(),
                Owner::Cold {
                    entry: Entry {
                        checkpoint: false,
                        full: *bytes,
                        legacy: false,
                        sections: vec![section],
                        written: *bytes,
                    },
                    pending: Vec::new(),
                },
            );
        }
    }

    pub(crate) fn unshelve_owner(&mut self, owner: &str) {
        if owner.contains('\n')
            && !self.owners.contains_key(owner)
            && let Some(row) = self.shelf_of(owner)
        {
            self.unshelve_row(row);
        }
    }

    pub(crate) fn unshelve_state(&mut self, state: &str) {
        let rows = self
            .shelved
            .iter()
            .filter(|(_, shelf)| shelf.holds_state(state))
            .map(|(row, _)| *row)
            .collect::<Vec<_>>();
        for row in rows {
            self.unshelve_row(row);
        }
    }

    pub(crate) fn unshelve_all(&mut self) {
        let rows = self.shelved.keys().copied().collect::<Vec<_>>();
        for row in rows {
            self.unshelve_row(row);
        }
    }

    fn shelve(&mut self) {
        if crate::sim::mutated(crate::sim::Mutation::PlacedStayResident) {
            return;
        }
        let mut rows: BTreeMap<u64, (bool, Vec<(&String, u64)>)> = BTreeMap::new();
        for (owner, state) in &self.owners {
            let (Owner::Cold { entry, .. } | Owner::Warm { entry, .. }) = state else {
                continue;
            };
            let [section] = entry.sections.as_slice() else {
                continue;
            };
            if !self.placed.contains_key(&section.row) {
                continue;
            }
            let clean = matches!(state, Owner::Cold { entry, pending } if pending.is_empty() && !entry.legacy && !entry.checkpoint && entry.full == entry.written);
            let row = rows.entry(section.row).or_insert((true, Vec::new()));
            row.0 &= clean;
            row.1.push((owner, entry.full));
        }
        let mut shelves = Vec::new();
        for (row, (clean, owners)) in rows {
            let placed = self.placed.get(&row).map(|(_, count)| *count);
            if !clean || placed != Some(owners.len() as u64) {
                continue;
            }
            let mut keys = String::with_capacity(owners.iter().map(|(owner, _)| owner.len()).sum());
            let mut ends = Vec::with_capacity(owners.len());
            let mut bytes = Vec::with_capacity(owners.len());
            for (owner, full) in owners {
                keys.push_str(owner);
                ends.push(keys.len());
                bytes.push(full);
            }
            shelves.push((
                row,
                Shelf {
                    bytes: bytes.into_boxed_slice(),
                    ends: ends.into_boxed_slice(),
                    keys: keys.into_boxed_str(),
                },
            ));
        }
        for (row, shelf) in shelves {
            for index in 0..shelf.ends.len() {
                self.owners.remove(shelf.key(index));
            }
            self.shelved.insert(row, shelf);
        }
    }

    #[must_use]
    pub fn resident_owners(&self) -> usize {
        self.owners.len()
    }

    pub(crate) fn cold_owners(&self) -> Vec<String> {
        self.owners
            .iter()
            .filter(|(_, state)| matches!(state, Owner::Cold { .. }))
            .map(|(owner, _)| owner.clone())
            .collect()
    }

    pub(crate) fn cold(&self, owner: &str) -> Option<&[Section]> {
        match self.owners.get(owner) {
            Some(Owner::Cold { entry, .. }) => Some(&entry.sections),
            _ => None,
        }
    }

    pub(crate) fn warm(
        &mut self,
        owner: &str,
        groups: Vec<(Section, Vec<Record>)>,
    ) -> Result<(), StoreError> {
        let mut pendings = Vec::new();
        let Some(Owner::Cold { entry, pending }) = self.owners.remove(owner) else {
            return Ok(());
        };
        pendings.push(pending);
        self.warmed.push(owner.to_owned());
        let legacy = entry.legacy;
        self.owners.insert(
            owner.to_owned(),
            Owner::Warm {
                delta: Delta {
                    checkpoint: legacy,
                    ..Delta::default()
                },
                entry,
            },
        );
        let mut joined: BTreeMap<String, Section> = BTreeMap::new();
        for (section, records) in groups {
            for record in records {
                if legacy {
                    if legacy_owner_of(&record) != Some(owner) {
                        continue;
                    }
                    if let Record::Stored { key, .. } = &record
                        && key != owner
                    {
                        if self.owners.contains_key(key) {
                            continue;
                        }
                        self.owners.insert(
                            key.clone(),
                            Owner::Warm {
                                delta: Delta {
                                    keys: BTreeSet::from([key.clone()]),
                                    ..Delta::default()
                                },
                                entry: Entry::default(),
                            },
                        );
                    }
                    self.take(record, None)?;
                    continue;
                }
                let Some(other) = owner_of(&record) else {
                    continue;
                };
                let take = if other == owner {
                    true
                } else if let Some(at) = joined.get(other) {
                    *at == section
                } else if matches!(self.owners.get(other), Some(Owner::Cold { entry, .. }) if entry.sections.as_slice() == [section] && !entry.legacy)
                {
                    if let Some(Owner::Cold { entry, pending }) = self.owners.remove(other) {
                        pendings.push(pending);
                        self.warmed.push(other.to_owned());
                        self.owners.insert(
                            other.to_owned(),
                            Owner::Warm {
                                delta: Delta::default(),
                                entry,
                            },
                        );
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

    pub(crate) fn touch(&mut self, state: &str) {
        let known = self
            .owners
            .range::<str, _>((Bound::Included(state), Bound::Unbounded))
            .next()
            .is_some_and(|(owner, _)| {
                owner
                    .strip_prefix(state)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('\n'))
            });
        if known && !self.read.contains(state) {
            self.read.insert(state.to_owned());
        }
        let keys = format!("{state}\n");
        if known && !state.contains('\n') && !self.read.contains(&keys) {
            self.read.insert(keys);
        }
    }

    pub(crate) fn read_owner(&mut self, owner: &str) {
        if self.owners.contains_key(owner) && !self.read.contains(owner) {
            self.read.insert(owner.to_owned());
        }
    }

    pub(crate) fn take_warmed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warmed)
    }

    pub(crate) fn cool(&mut self, owners: &[String]) {
        for owner in owners {
            let Some(end) = owner.find('\n') else {
                continue;
            };
            let clean = matches!(self.owners.get(owner), Some(Owner::Warm { delta, entry }) if delta.is_empty() && !entry.legacy && !entry.sections.is_empty());
            if !clean || end == 0 {
                continue;
            }
            let Some(Owner::Warm { entry, .. }) = self.owners.remove(owner) else {
                continue;
            };
            self.stored.remove(owner);
            self.dead.remove(owner);
            self.owners.insert(
                owner.clone(),
                Owner::Cold {
                    entry,
                    pending: Vec::new(),
                },
            );
        }
    }

    pub(crate) fn evict(&mut self) {
        self.warmed.clear();
        let read = std::mem::take(&mut self.read);
        let evicted = self
            .owners
            .iter()
            .filter(|(owner, state)| {
                matches!(state, Owner::Warm { delta, entry } if delta.is_empty() && !entry.legacy && !entry.sections.is_empty())
                    && !read.contains(owner.as_str())
                    && !owner
                        .find('\n')
                        .is_some_and(|end| read.contains(&owner[..=end]))
            })
            .map(|(owner, _)| owner.clone())
            .collect::<Vec<_>>();
        for owner in evicted {
            let Some(Owner::Warm { entry, .. }) = self.owners.remove(&owner) else {
                continue;
            };
            if owner.contains('\n') {
                self.stored.remove(&owner);
                self.dead.remove(&owner);
            } else {
                if self.checkpoints.remove(&owner).is_some() {
                    self.marks.remove(&owner);
                }
                let cells = self
                    .cells
                    .range((owner.clone(), 0)..=(owner.clone(), u64::MAX))
                    .map(|(cell, _)| cell.clone())
                    .collect::<Vec<_>>();
                for cell in cells {
                    self.cells.remove(&cell);
                }
            }
            self.owners.insert(
                owner,
                Owner::Cold {
                    entry,
                    pending: Vec::new(),
                },
            );
        }
        let idle = self
            .pages
            .iter()
            .filter_map(|(first, page)| match page {
                Page::Loaded(section) => Some((first.clone(), *section)),
                Page::Cold(_) => None,
            })
            .filter(|(first, _)| {
                self.page_owners(first).all(|(_, state)| {
                    let listed = !self.placed_owner(state);
                    let pins = crate::sim::mutated(crate::sim::Mutation::UnindexedPinsPages);
                    (!listed && !pins)
                        || (listed
                            && matches!(state, Owner::Cold { entry, pending } if pending.is_empty() && !entry.legacy && !entry.sections.is_empty()))
                })
            })
            .collect::<Vec<_>>();
        for (first, section) in idle {
            let owners = self
                .page_owners(&first)
                .filter(|(_, state)| !self.placed_owner(state))
                .map(|(owner, _)| owner.clone())
                .collect::<Vec<_>>();
            let mut filter = owners
                .iter()
                .map(|owner| key_hash(owner))
                .collect::<Vec<_>>();
            filter.sort_unstable();
            for owner in owners {
                self.owners.remove(&owner);
            }
            self.filters.insert(section.row, filter.into_boxed_slice());
            self.pages.insert(first, Page::Cold(section));
        }
        self.shelve();
    }

    fn placed_owner(&self, state: &Owner) -> bool {
        matches!(state, Owner::Cold { entry, .. } | Owner::Warm { entry, .. }
            if matches!(entry.sections.as_slice(), [section] if self.placed.contains_key(&section.row)))
    }

    fn unindexed(&self) -> impl Iterator<Item = &String> {
        self.owners
            .iter()
            .filter(|(_, state)| self.placed_owner(state))
            .map(|(owner, _)| owner)
    }

    fn page_owners(&self, first: &str) -> impl Iterator<Item = (&String, &Owner)> {
        let end = self.page_end(first);
        self.owners.range::<str, _>((
            Bound::Included(first),
            end.map_or(Bound::Unbounded, |end| Bound::Excluded(end.as_str())),
        ))
    }

    pub(crate) fn is_legacy(&self, owner: &str) -> bool {
        matches!(self.owners.get(owner), Some(Owner::Cold { entry, .. }) if entry.legacy)
    }

    pub(crate) fn cold_keys(&self, state: &str) -> Vec<String> {
        let (low, high) = (format!("{state}\n"), format!("{state}\u{b}"));
        self.owners
            .range::<str, _>((
                Bound::Included(low.as_str()),
                Bound::Excluded(high.as_str()),
            ))
            .filter(|(_, owner)| !matches!(owner, Owner::Warm { .. }))
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub(crate) fn key_pages(&self, state: &str) -> Vec<(String, Section)> {
        let (low, high) = (format!("{state}\n"), format!("{state}\u{b}"));
        self.page_of(&low)
            .into_iter()
            .chain(self.pages.range::<str, _>((
                Bound::Excluded(low.as_str()),
                Bound::Excluded(high.as_str()),
            )))
            .filter_map(|(first, page)| match page {
                Page::Cold(section) => Some((first.clone(), *section)),
                Page::Loaded(_) => None,
            })
            .collect()
    }

    fn complete(&self, state: &str) -> bool {
        self.shelved.is_empty()
            && !matches!(
                self.owners.get(state),
                Some(Owner::Cold { .. } | Owner::Waiting { .. })
            )
            && self.cold_page(state).is_none()
            && self.key_pages(state).is_empty()
            && self.cold_keys(state).is_empty()
    }

    pub(crate) fn waiting(&self) -> Vec<String> {
        self.owners
            .iter()
            .filter(|(_, state)| matches!(state, Owner::Waiting { .. }))
            .map(|(owner, _)| owner.clone())
            .collect()
    }

    pub(crate) fn forgetting(&self) -> Vec<String> {
        self.owners
            .iter()
            .filter(|(_, state)| {
                matches!(state, Owner::Cold { pending, .. } | Owner::Waiting { pending } if pending.iter().any(|record| matches!(record, Record::Forget { .. })))
            })
            .map(|(owner, _)| owner.clone())
            .collect()
    }

    pub(crate) fn take_forgets(&mut self) -> BTreeSet<String> {
        std::mem::take(&mut self.forgets)
    }

    pub(crate) fn writes(&self) -> Vec<&str> {
        self.owners
            .iter()
            .filter(|(_, state)| match state {
                Owner::Cold { pending, .. } | Owner::Waiting { pending } => !pending.is_empty(),
                Owner::Warm { delta, .. } => !delta.is_empty(),
            })
            .map(|(owner, _)| owner.as_str())
            .collect()
    }

    pub(crate) fn entry(&self, owner: &str) -> Option<&Entry> {
        match self.owners.get(owner) {
            Some(Owner::Cold { entry, .. } | Owner::Warm { entry, .. }) => Some(entry),
            Some(Owner::Waiting { .. }) | None => None,
        }
    }

    pub(crate) fn loaded(&self, owner: &str) -> bool {
        matches!(self.owners.get(owner), Some(Owner::Warm { .. }))
    }

    pub(crate) fn plan(
        &self,
        written: &BTreeMap<String, Option<Entry>>,
        groups: &[(Section, u64)],
    ) -> Result<Plan, StoreError> {
        if let Some(section) = self.cold_root() {
            return Err(invalid(format!(
                "a snapshot rewrites the index before its root at row {} is loaded",
                section.row
            )));
        }
        let mut refs = self.groups.clone();
        refs.extend(&self.placed);
        for (owner, entry) in written {
            let Some(old) = self.entry(owner) else {
                continue;
            };
            if entry
                .as_ref()
                .is_some_and(|entry| entry.sections.starts_with(&old.sections))
            {
                continue;
            }
            for section in &old.sections {
                let Some((_, count)) = refs.get_mut(&section.row) else {
                    return Err(invalid(format!(
                        "owner {owner} holds section {} that no group counts",
                        section.row
                    )));
                };
                *count = count.checked_sub(1).ok_or_else(|| {
                    invalid(format!("section {} is counted below zero", section.row))
                })?;
            }
        }
        for (section, count) in groups {
            refs.insert(section.row, (section.rows, *count));
        }
        let living: BTreeSet<u64> = refs
            .iter()
            .filter(|(_, (_, count))| *count > 0)
            .filter_map(|(row, _)| self.chains.get(row).copied())
            .collect();
        let alive = |row: &u64| {
            !crate::sim::mutated(crate::sim::Mutation::ChainMemberDropped)
                && self
                    .chains
                    .get(row)
                    .is_some_and(|keyframe| living.contains(keyframe))
        };
        let mut superseded = Vec::new();
        let mut dropped = BTreeSet::new();
        refs.retain(|row, (rows, count)| {
            let kept = *count > 0 || alive(row);
            if !kept {
                superseded.push(*row..*row + *rows);
                dropped.insert(*row);
            }
            kept
        });
        for row in &dropped {
            if let Some(keyframe) = self.chains.get(row) {
                assert!(
                    !living.contains(keyframe),
                    "turn log drops row {row} of the chain from {keyframe} while a member lives"
                );
            }
        }
        for (row, (_, count)) in &refs {
            if let Some(keyframe) = self.chains.get(row)
                && *count == 0
            {
                assert!(
                    living.contains(keyframe),
                    "turn log keeps row {row} of the chain from {keyframe} with no member alive"
                );
            }
        }
        let touched = if self.pages.is_empty() {
            if self.owners.is_empty() && written.is_empty() {
                BTreeSet::new()
            } else {
                BTreeSet::from([String::new()])
            }
        } else {
            written
                .iter()
                .filter(|(owner, entry)| entry.as_ref() != self.entry(owner))
                .map(|(owner, _)| owner)
                .chain(self.unindexed())
                .filter_map(|owner| self.page_of(owner).map(|(first, _)| first.clone()))
                .collect()
        };
        let mut kept = self.pages.clone();
        let mut pages = Vec::new();
        for first in touched {
            let end = self.page_end(&first).cloned();
            match kept.remove(&first) {
                Some(Page::Cold(_)) => {
                    return Err(invalid(format!(
                        "a snapshot rewrites index page {first} before it is loaded"
                    )));
                }
                Some(Page::Loaded(section)) => {
                    superseded.push(section.row..section.row + section.rows);
                }
                None => {}
            }
            let mut records = Vec::new();
            for (owner, state) in self.owners.range::<str, _>((
                Bound::Included(first.as_str()),
                end.as_deref().map_or(Bound::Unbounded, Bound::Excluded),
            )) {
                let entry = match (written.get(owner), state) {
                    (Some(entry), _) => entry.as_ref(),
                    (None, Owner::Cold { entry, .. } | Owner::Warm { entry, .. }) => Some(entry),
                    (None, Owner::Waiting { .. }) => {
                        return Err(invalid(format!(
                            "owner {owner} waits for index page {first}, which a snapshot rewrites"
                        )));
                    }
                };
                let Some(entry) = entry.filter(|entry| !entry.sections.is_empty()) else {
                    continue;
                };
                records.push(Record::Owner {
                    checkpoint: entry.checkpoint,
                    full: entry.full,
                    legacy: entry.legacy,
                    owner: owner.clone(),
                    sections: entry
                        .sections
                        .iter()
                        .map(|section| (section.row, section.rows))
                        .collect(),
                    written: entry.written,
                });
            }
            pages.push((first, records));
        }
        superseded.sort_by_key(|range| range.start);
        Ok(Plan {
            groups: refs,
            kept,
            pages,
            superseded,
        })
    }

    pub(crate) fn sectioned(
        &mut self,
        written: BTreeMap<String, Option<Entry>>,
        pages: BTreeMap<String, Page>,
        groups: BTreeMap<u64, (u64, u64)>,
        root: Option<Section>,
    ) {
        for (owner, entry) in written {
            let state = match (self.owners.remove(&owner), entry) {
                (Some(Owner::Warm { .. }), Some(entry)) => Owner::Warm {
                    delta: Delta::default(),
                    entry,
                },
                (Some(Owner::Cold { .. } | Owner::Waiting { .. }), Some(entry)) => Owner::Cold {
                    entry,
                    pending: Vec::new(),
                },
                (_, None) | (None, _) => continue,
            };
            self.owners.insert(owner, state);
        }
        self.pages = pages;
        let cold = self
            .pages
            .values()
            .filter_map(|page| match page {
                Page::Cold(section) => Some(section.row),
                Page::Loaded(_) => None,
            })
            .collect::<BTreeSet<_>>();
        self.filters.retain(|row, _| cold.contains(row));
        self.groups = groups;
        self.root = root.map(Page::Loaded);
        assert!(
            self.shelved.is_empty(),
            "turn log indexed a snapshot while {} placed sections were shelved",
            self.shelved.len()
        );
        self.placed.clear();
        let groups = &self.groups;
        self.chains.retain(|row, _| groups.contains_key(row));
    }

    pub(crate) fn waits(&self, owner: &str) -> bool {
        matches!(self.owners.get(owner), Some(Owner::Waiting { .. }))
    }

    pub(crate) fn placeable(&self, key: &str) -> bool {
        key.contains('\n')
            && self.stored.contains_key(key)
            && matches!(
                self.owners.get(key),
                Some(Owner::Warm { delta, entry })
                    if entry.sections.is_empty()
                        && !entry.legacy
                        && delta.cells.is_empty()
                        && !delta.checkpoint
                        && delta.keys.len() == 1
                        && delta.keys.contains(key)
            )
    }

    pub(crate) fn place(&mut self, section: Section, owners: &[(String, u64)], chain: u64) {
        for (owner, bytes) in owners {
            let Some(Owner::Warm { delta, entry }) = self.owners.get_mut(owner) else {
                panic!("turn log placed owner {owner} is not warm");
            };
            assert!(
                entry.sections.is_empty(),
                "turn log placed owner {owner} twice"
            );
            assert!(
                delta.keys.remove(owner),
                "turn log placed owner {owner} had no delta"
            );
            assert!(
                delta.is_empty(),
                "turn log placed owner {owner} holds more than its value"
            );
            *entry = Entry {
                checkpoint: false,
                full: *bytes,
                legacy: false,
                sections: vec![section],
                written: *bytes,
            };
        }
        self.place_group(section, owners.len() as u64, Some(chain));
    }

    fn place_group(&mut self, section: Section, count: u64, chain: Option<u64>) {
        assert!(
            count > 0,
            "turn log placed an empty section at row {}",
            section.row
        );
        let previous = self.placed.insert(section.row, (section.rows, count));
        assert!(
            previous.is_none(),
            "turn log placed row {} twice",
            section.row
        );
        if let Some(keyframe) = chain {
            assert!(
                keyframe <= section.row,
                "turn log chain at row {} starts after it",
                section.row
            );
            self.chains.insert(section.row, keyframe);
        }
    }

    pub(crate) fn keyframe(&self, row: u64) -> Option<u64> {
        self.chains.get(&row).copied()
    }

    pub(crate) fn chain_links(&self, section: Section) -> Option<Vec<std::ops::Range<u64>>> {
        let keyframe = self.keyframe(section.row)?;
        assert!(
            keyframe <= section.row,
            "turn log chain at row {} starts after it",
            section.row
        );
        let mut links = Vec::new();
        for (&row, &first) in self.chains.range(keyframe..section.row) {
            if first != keyframe {
                continue;
            }
            let (rows, _) = self.groups.get(&row).or_else(|| self.placed.get(&row))?;
            links.push(row..row + rows);
        }
        links.push(section.row..section.row + section.rows);
        assert!(
            links.windows(2).all(|pair| pair[0].end <= pair[1].start),
            "turn log chain links at row {} overlap",
            section.row
        );
        (links.first().map(|link| link.start) == Some(keyframe)).then_some(links)
    }

    pub(crate) fn chains_of(&self, groups: &BTreeMap<u64, (u64, u64)>) -> Vec<(u64, u64)> {
        self.chains
            .iter()
            .filter(|(row, _)| groups.contains_key(row))
            .map(|(&row, &keyframe)| (row, keyframe))
            .collect()
    }

    fn placed_in_tail(
        &mut self,
        owners: &[(String, u64)],
        section: Section,
        chain: Option<u64>,
    ) -> Result<(), StoreError> {
        if chain.is_some_and(|keyframe| keyframe > section.row) {
            return Err(invalid(format!(
                "placed section at row {} links to a chain that starts after it",
                section.row
            )));
        }
        if owners.is_empty() || self.placed.contains_key(&section.row) {
            return Err(invalid(format!(
                "placed section at row {} is empty or listed twice",
                section.row
            )));
        }
        for (owner, bytes) in owners {
            if !owner.contains('\n') || self.owners.contains_key(owner) {
                return Err(invalid(format!(
                    "owner {owner} is placed at row {} but is not a new stored value",
                    section.row
                )));
            }
            self.owners.insert(
                owner.clone(),
                Owner::Cold {
                    entry: Entry {
                        checkpoint: false,
                        full: *bytes,
                        legacy: false,
                        sections: vec![section],
                        written: *bytes,
                    },
                    pending: Vec::new(),
                },
            );
        }
        self.place_group(section, owners.len() as u64, chain);
        Ok(())
    }

    pub(crate) fn delta_records(
        &self,
        owner: &str,
        mut take: impl FnMut(Record) -> Result<(), StoreError>,
    ) -> Result<bool, StoreError> {
        match self.owners.get(owner) {
            Some(Owner::Cold { pending, .. } | Owner::Waiting { pending }) => {
                let mut last = BTreeMap::new();
                for (position, record) in pending.iter().enumerate() {
                    if let Record::Stored { key, .. } = record {
                        last.insert(key.as_str(), position);
                    }
                }
                let mut checkpoint = false;
                for (position, record) in pending.iter().enumerate() {
                    match record {
                        Record::Stored { key, .. } if last.get(key.as_str()) != Some(&position) => {
                        }
                        record => {
                            checkpoint |= matches!(record, Record::Checkpoint { .. });
                            take(record.clone())?;
                        }
                    }
                }
                Ok(checkpoint)
            }
            Some(Owner::Warm { delta, .. }) => {
                for key in &delta.keys {
                    if let Some(value) = self.stored(key) {
                        take(Record::Stored {
                            key: key.clone(),
                            value: value.to_owned(),
                        })?;
                    }
                }
                let checkpoint = self.checkpoints.get(owner).filter(|_| delta.checkpoint);
                if let Some(checkpoint) = checkpoint {
                    take(Record::Checkpoint {
                        payload: checkpoint.payload.clone(),
                        revision: checkpoint.revision,
                        state: owner.to_owned(),
                    })?;
                }
                for (cell, forgotten) in &delta.cells {
                    if *forgotten {
                        take(Record::Forget {
                            cell: *cell,
                            session: owner.to_owned(),
                        })?;
                    }
                    let updates = self.cells.get(&(owner.to_owned(), *cell));
                    for (position, update) in updates.into_iter().flatten().enumerate() {
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
            None => Ok(false),
        }
    }

    pub(crate) fn owner_records(
        &self,
        owner: &str,
        mut take: impl FnMut(Record) -> Result<(), StoreError>,
    ) -> Result<bool, StoreError> {
        if owner.contains('\n') {
            if let Some(value) = self.stored(owner) {
                take(Record::Stored {
                    key: owner.to_owned(),
                    value: value.to_owned(),
                })?;
            }
            return Ok(false);
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
            let listed = match state {
                Owner::Cold { entry, pending } => {
                    entry.checkpoint
                        || pending
                            .iter()
                            .any(|record| matches!(record, Record::Checkpoint { .. }))
                }
                Owner::Waiting { pending } => pending
                    .iter()
                    .any(|record| matches!(record, Record::Checkpoint { .. })),
                Owner::Warm { .. } => false,
            };
            if listed {
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

    #[must_use]
    pub fn retired(&self, id: &str) -> Option<&Retired> {
        self.retired.get(&tag(id))
    }

    #[must_use]
    pub fn loaded_pages(&self) -> usize {
        self.pages
            .values()
            .filter(|page| matches!(page, Page::Loaded(_)))
            .count()
    }

    #[must_use]
    pub fn retired_turns(&self) -> usize {
        self.retired.len()
    }

    pub fn retired_tags(&self) -> impl Iterator<Item = u128> + '_ {
        self.retired.keys().copied()
    }

    pub fn fact_states(&self) -> impl Iterator<Item = &str> {
        self.state_index.keys().map(String::as_str)
    }

    fn retired_value(&self) -> Result<Box<RawValue>, StoreError> {
        use std::fmt::Write as _;
        let mut rows = String::with_capacity(self.retired.len() * RETIRED_ROW);
        let mut hands = BTreeMap::new();
        for (id, entry) in &self.retired {
            write!(
                rows,
                "{id:032x}{:032x}{:016x}{:016x}{}",
                entry.digest,
                entry.first,
                entry.last,
                outcome_code(entry.outcome)
            )
            .map_err(invalid)?;
            if let Some(hand) = &entry.hand {
                hands.insert(format!("{id:032x}"), hand.clone());
            }
        }
        serde_json::value::to_raw_value(&RetiredIndex { hands, rows }).map_err(invalid)
    }

    fn retired_from(&mut self, value: &RawValue) -> Result<(), StoreError> {
        let index: RetiredIndex = serde_json::from_str(value.get()).map_err(invalid)?;
        let rows = index.rows.as_bytes();
        if !index.rows.is_ascii() || rows.len() % RETIRED_ROW != 0 {
            return Err(invalid("retired index rows are malformed"));
        }

        let mut entries = Vec::with_capacity(rows.len() / RETIRED_ROW);
        for row in rows.chunks_exact(RETIRED_ROW) {
            let id = u128::from_str_radix(text(row, 0, 32)?, 16).map_err(invalid)?;
            let digest = u128::from_str_radix(text(row, 32, 64)?, 16).map_err(invalid)?;
            let first = u64::from_str_radix(text(row, 64, 80)?, 16).map_err(invalid)?;
            let last = u64::from_str_radix(text(row, 80, 96)?, 16).map_err(invalid)?;
            let code = text(row, 96, 97)?;
            let outcome =
                outcome_of(code).ok_or_else(|| invalid(format!("retired turn outcome {code}")))?;
            let hand = index.hands.get(text(row, 0, 32)?).cloned();
            entries.push((
                id,
                Retired {
                    digest,
                    first,
                    hand,
                    last,
                    outcome,
                },
            ));
        }
        if self.retired.is_empty() {
            self.retired = entries.into_iter().collect();
        } else {
            self.retired.extend(entries);
        }
        Ok(())
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
    pub fn resident_bytes(&self) -> usize {
        let stored = self
            .stored
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>();
        let hosts = self
            .hosts
            .iter()
            .map(|(key, value)| key.len() + value.get().len())
            .sum::<usize>();
        let checkpoints = self
            .checkpoints
            .values()
            .map(|checkpoint| checkpoint.payload.len())
            .sum::<usize>();
        let updates = self
            .cells
            .values()
            .chain(self.models.values())
            .flatten()
            .map(|update| update.get().len())
            .sum::<usize>();
        stored + hosts + checkpoints + updates + self.hot_bytes
    }

    #[must_use]
    pub const fn segment_row(&self) -> u64 {
        self.segment_row
    }

    pub(crate) const fn start_segment(&mut self, row: u64) {
        self.segment_row = row;
        self.fact_row = row;
    }

    #[must_use]
    pub fn fact_segments(&self) -> &[Segment] {
        &self.fact_segments
    }

    #[must_use]
    pub const fn fact_row(&self) -> u64 {
        self.fact_row
    }

    #[must_use]
    pub const fn sealed_facts(&self) -> u64 {
        self.fact_base
    }

    fn fact_cut(&self, keep: u64) -> u64 {
        if crate::sim::mutated(crate::sim::Mutation::FactsResident) {
            return self.fact_base + 1;
        }
        keep.min(self.next_fact())
    }

    fn sealed_fact_segment(&self, keep: u64) -> Option<Segment> {
        let last = self.fact_cut(keep).checked_sub(1)?;
        (last > self.fact_base).then_some(Segment {
            first: self.fact_base + 1,
            last,
            row: self.fact_row,
        })
    }

    pub(crate) fn seal_facts(&mut self, row: u64, keep: u64) {
        if let Some(segment) = self.sealed_fact_segment(keep) {
            let sealed = usize::try_from(segment.last - self.fact_base)
                .expect("a sealed fact count fits in usize");
            self.facts.drain(..sealed);
            self.fact_states.drain(..sealed);
            self.fact_base = segment.last;
            self.fact_segments.push(segment);
        }
        if !crate::sim::mutated(crate::sim::Mutation::BlobsResident) {
            let cut = self.fact_cut(keep);
            for key in self.blob_keys(|seq| seq < cut) {
                self.stored.remove(&key);
            }
        }
        self.fact_row = row;
        self.folded_through = self.next_fact() - 1;
    }

    fn blob_keys(&self, sealed: impl Fn(u64) -> bool) -> Vec<String> {
        self.blob_seqs
            .iter()
            .zip(self.unsequenced_blobs..)
            .filter(|(seq, _)| sealed(**seq))
            .filter_map(|(_, index)| {
                let digest = self.values.digest(BlobRef::from_index(index))?;
                Some(digest.key().as_str().to_owned())
            })
            .collect()
    }

    #[must_use]
    pub fn sealed_blob(&self, digest: &Digest) -> Option<Segment> {
        let index = self
            .values
            .blob(digest)?
            .index()
            .checked_sub(self.unsequenced_blobs)?;
        let seq = *self.blob_seqs.get(index as usize)?;
        self.fact_segments
            .iter()
            .copied()
            .find(|segment| segment.first <= seq && seq <= segment.last)
    }

    #[must_use]
    pub fn blob_resident(&self, digest: &Digest) -> bool {
        self.stored.contains_key(digest.key().as_str())
    }

    #[must_use]
    pub fn unowned_stored(&self) -> usize {
        self.stored.keys().filter(|key| !key.contains('\n')).count()
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

    fn garbage(
        &self,
        links: &mut HashMap<String, Links>,
        marks: &mut BTreeMap<String, Vec<String>>,
        collectable: impl Fn(&str) -> bool,
    ) -> Option<Vec<bool>> {
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
        marks.retain(|state, _| self.checkpoints.contains_key(state));
        let mut queue = Vec::new();
        for (state, checkpoint) in self
            .checkpoints
            .iter()
            .filter(|(state, _)| collectable(state))
        {
            let references = marks.entry(state.clone()).or_insert_with(|| {
                EncodedPayload::references(&checkpoint.payload)
                    .map(str::to_owned)
                    .collect()
            });
            queue.extend(
                references
                    .iter()
                    .filter_map(|content| firsts.get_key_value(content.as_str()))
                    .map(|(content, _)| *content),
            );
        }
        if !crate::sim::mutated(crate::sim::Mutation::CellRootsSkipped) {
            let live = self
                .cells
                .iter()
                .filter(|((session, _), _)| collectable(session))
                .flat_map(|(_, updates)| updates);
            for update in live {
                queue.extend(
                    EncodedPayload::references(update.get())
                        .filter_map(|content| firsts.get_key_value(content))
                        .map(|(content, _)| *content),
                );
            }
        }
        let mut visited = vec![false; holders.len()];
        while let Some(content) = queue.pop() {
            let Some(&first) = firsts.get(content) else {
                continue;
            };
            if std::mem::replace(&mut visited[first], true) {
                continue;
            }
            let (_, _, state, value) = holders[first];
            let chunk_key = |chunk: &str| format!("{state}\n{chunk}");
            let known = match links.get(content) {
                Some(known) => {
                    if !known
                        .chunks
                        .iter()
                        .all(|chunk| self.stored.contains_key(&chunk_key(chunk)))
                    {
                        return None;
                    }
                    known
                }
                None => {
                    let requested = Cell::new(Vec::new());
                    let chunk = |chunk: &str| {
                        let mut seen = requested.take();
                        seen.push(chunk.to_owned());
                        requested.set(seen);
                        self.stored.get(&chunk_key(chunk)).map(String::as_str)
                    };
                    let json = EncodedPayload::assembled(value, &chunk)?;
                    let references = EncodedPayload::references(&json)
                        .map(str::to_owned)
                        .collect();
                    links.entry(content.to_owned()).or_insert(Links {
                        chunks: requested.take(),
                        references,
                    })
                }
            };
            queue.extend(
                known
                    .references
                    .iter()
                    .filter_map(|content| firsts.get_key_value(content.as_str()))
                    .map(|(content, _)| *content),
            );
        }
        links.retain(|content, _| firsts.contains_key(content.as_str()));
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
        let states = self
            .stored
            .keys()
            .filter_map(|key| key.split_once('\n').map(|(state, _)| state))
            .collect::<BTreeSet<_>>();
        let complete = states
            .into_iter()
            .filter(|state| self.complete(state))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let mut links = std::mem::take(&mut self.links);
        let mut marks = std::mem::take(&mut self.marks);
        let garbage = self.garbage(&mut links, &mut marks, |state| complete.contains(state));
        self.links = links;
        self.marks = marks;
        let Some(garbage) = garbage else {
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
        let excess = self.retire_turns(keep);
        let index = &self.index;
        self.holds
            .retain(|_, hold| !hold.prunable(|turn| index.contains_key(turn)));
        excess
    }

    fn retire_turns(&mut self, keep: usize) -> usize {
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
        for turn in &self.turns {
            if let Phase::Finished { last, outcome, .. } = turn.phase
                && last <= cutoff
            {
                self.retired.insert(
                    tag(&turn.id),
                    Retired {
                        digest: tag(&turn.digest),
                        first: turn.first,
                        hand: turn.hand.as_deref().map(Box::from),
                        last,
                        outcome,
                    },
                );
            }
        }
        self.drop_turns(
            |turn| matches!(turn.phase, Phase::Finished { last, .. } if last <= cutoff),
        );
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

    pub fn stored_of<'a>(&'a self, state: &str) -> impl Iterator<Item = (&'a str, &'a str)> {
        let prefix = format!("{state}\n");
        let start = prefix.len();
        self.stored
            .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .take_while(move |(key, _)| key.starts_with(&prefix))
            .map(move |(key, value)| (&key[start..], value.as_str()))
    }

    #[must_use]
    pub fn stored_bytes(&self) -> usize {
        self.stored
            .iter()
            .chain(&self.dead)
            .map(|(key, value)| key.len() + value.len())
            .sum()
    }

    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.stored(key).is_some()
            || self.shelf_of(key).is_some()
            || matches!(
                self.owners.get(key),
                Some(Owner::Cold { entry, .. }) if !entry.legacy && !entry.sections.is_empty()
            )
    }

    #[must_use]
    pub fn stored(&self, key: &str) -> Option<&str> {
        self.stored
            .get(key)
            .or_else(|| self.dead.get(key))
            .map(String::as_str)
    }

    #[must_use]
    pub const fn wakes(&self) -> &Wakes {
        &self.wakes
    }

    pub(crate) fn restore_wake(&mut self, key: &str, held: Option<ArmedWake>) {
        self.wakes.restore(key, held);
    }

    #[must_use]
    pub fn host(&self, key: &str) -> Option<&RawValue> {
        self.hosts.get(key).map(AsRef::as_ref)
    }

    pub fn hosts(&self) -> impl Iterator<Item = (&str, &RawValue)> {
        self.hosts
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_ref()))
    }

    pub fn stored_all(&self) -> impl Iterator<Item = (&str, &str)> {
        let dead = self
            .dead
            .iter()
            .filter(|(key, _)| !self.stored.contains_key(*key));
        self.stored
            .iter()
            .chain(dead)
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn models_all(&self) -> impl Iterator<Item = (&str, &[Box<RawValue>])> {
        self.models
            .iter()
            .map(|(turn, responses)| (turn.as_str(), responses.as_slice()))
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

    #[must_use]
    pub fn hold(&self, key: &str) -> Option<&Hold> {
        self.holds.get(key)
    }

    pub fn holds(&self) -> impl Iterator<Item = (&str, &Hold)> {
        self.holds.iter().map(|(key, hold)| (key.as_str(), hold))
    }

    #[must_use]
    pub fn running_hold(&self, call_id: &str) -> Option<(&str, &Hold)> {
        self.holds()
            .find(|(_, hold)| matches!(hold, Hold::Running { .. }) && hold.call_id() == call_id)
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

    #[must_use]
    pub fn next_fact(&self) -> u64 {
        self.fact_base + self.facts.len() as u64 + 1
    }

    pub fn facts(&self) -> impl Iterator<Item = (u64, &StoredFact)> {
        (self.fact_base + 1..).zip(&self.facts)
    }

    fn resident_from(&self, after: u64) -> usize {
        assert!(
            after >= self.fact_base,
            "a reader of resident facts starts at or after the sealed ones"
        );
        usize::try_from(after - self.fact_base)
            .map_or(self.facts.len(), |skipped| skipped.min(self.facts.len()))
    }

    #[must_use]
    pub const fn values(&self) -> &Values {
        &self.values
    }

    fn state_id(&mut self, state: &str) -> u32 {
        if let Some(id) = self.state_index.get(state) {
            return *id;
        }
        let id = u32::try_from(self.state_names.len())
            .expect("a log holds fewer states than u32 counts");
        self.state_names.push(state.to_owned());
        self.state_index.insert(state.to_owned(), id);
        id
    }

    pub(crate) fn withdraw_facts(&mut self, from: u64) {
        assert!(
            from > self.fact_base,
            "a withdrawn admission was never sealed"
        );
        let kept =
            usize::try_from(from - 1 - self.fact_base).expect("a fact position fits in usize");
        assert!(
            self.facts[kept..]
                .iter()
                .all(|fact| !matches!(fact, Fact::Marker(Marker::Blob { .. }) | Fact::Intent(_))),
            "a withdrawn admission takes back no blob or intent"
        );
        self.facts.truncate(kept);
        self.fact_states.truncate(kept);
    }

    pub fn resident_facts(
        &self,
        after: u64,
    ) -> impl Iterator<Item = (u64, Option<&str>, &StoredFact)> {
        let skipped = self.resident_from(after.max(self.fact_base));
        (self.fact_base + skipped as u64 + 1..)
            .zip(
                self.facts[skipped..]
                    .iter()
                    .zip(&self.fact_states[skipped..]),
            )
            .map(|(seq, (fact, state))| {
                let state = state.map(|state| self.state_names[state as usize].as_str());
                (seq, state, fact)
            })
    }

    pub fn facts_after(&self, after: u64) -> impl Iterator<Item = (u64, &StoredFact)> {
        let skipped = self.resident_from(after);
        (self.fact_base + skipped as u64 + 1..).zip(&self.facts[skipped..])
    }

    pub fn facts_of<'a>(
        &'a self,
        state: &str,
        turns: bool,
        after: u64,
    ) -> impl Iterator<Item = (u64, &'a StoredFact)> + 'a {
        let own = self.state_index.get(state).copied();
        let skipped = self.resident_from(after);
        (self.fact_base + skipped as u64 + 1..)
            .zip(
                self.facts[skipped..]
                    .iter()
                    .zip(&self.fact_states[skipped..]),
            )
            .filter(move |(_, (fact, held))| match held {
                Some(_) => own.is_some() && **held == own,
                None => turns || matches!(fact, Fact::Marker(Marker::Blob { .. })),
            })
            .map(|(seq, (fact, _))| (seq, fact))
    }

    pub fn intents(&self) -> impl Iterator<Item = (&str, Option<&str>, &OpenCall)> {
        self.derived_calls.iter().filter_map(|(call_id, state)| {
            let state = state.map(|state| self.state_names[state as usize].as_str());
            Some((call_id.as_str(), state, self.calls.get(call_id)?))
        })
    }

    #[must_use]
    pub fn intent(&self, call_id: &str) -> Option<&OpenCall> {
        self.derived_calls
            .contains_key(call_id)
            .then(|| self.calls.get(call_id))
            .flatten()
    }

    fn value<'a>(&'a self, value: &'a Value) -> Result<&'a RawValue, StoreError> {
        match value {
            Value::Inline(json) => Ok(json),
            Value::Blob(blob) => {
                let digest = self
                    .values
                    .digest(*blob)
                    .ok_or_else(|| invalid(format!("blob {} was never interned", blob.index())))?;
                self.blob(digest).map_err(StoreError::from)
            }
        }
    }

    fn lower(&mut self, seq: u64, state: Option<u32>, fact: &StoredFact) -> Result<(), StoreError> {
        let runtime =
            state.is_none() || crate::sim::mutated(crate::sim::Mutation::ScopedInputLowered);
        match fact {
            Fact::Input(_) | Fact::Marker(Marker::Status { .. } | Marker::Recovery { .. })
                if !runtime =>
            {
                Ok(())
            }
            Fact::Input(input) => self.lower_input(seq, input),
            Fact::Marker(Marker::Status { turn, status }) => {
                if self.turn(turn).is_none() && self.retired.contains_key(&tag(turn)) {
                    return Ok(());
                }
                self.status(turn, &lowered(status))
            }
            Fact::Marker(Marker::Recovery { turn, attempt, at }) => self.fold_in(
                &Record::Recovery {
                    at: *at,
                    attempt: *attempt,
                    turn: turn.clone(),
                    unknown: Vec::new(),
                },
                None,
            ),
            Fact::Intent(intent) => {
                let input = serde_json::value::to_raw_value(&intent.target).map_err(invalid)?;
                self.derived_calls.insert(intent.call_id.clone(), state);
                self.calls.insert(
                    intent.call_id.clone(),
                    OpenCall {
                        effect: EffectClass::External,
                        input,
                        tool: intent.tool.clone(),
                        turn: intent.turn.clone(),
                    },
                );
                Ok(())
            }
            Fact::EffectResult(result) => {
                self.derived_calls.remove(&result.call_id);
                self.calls.remove(&result.call_id);
                Ok(())
            }
            Fact::Decision(Decision::Host { key, value }) => {
                if internal_host(key) {
                    return Err(invalid(format!("host field {key} names a turn log key")));
                }
                self.hosts.insert(key.clone(), value.clone());
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn lower_input(&mut self, seq: u64, input: &Input<Value>) -> Result<(), StoreError> {
        let prompt = self.value(&input.value)?.to_owned();
        let metadata = self.value(&input.metadata)?.to_owned();
        let record = match &input.kind {
            InputKind::Prompt => Record::Admitted {
                digest: input_digest(&prompt, input.hand.as_deref()),
                first: input.first.unwrap_or(seq),
                hand: input.hand.clone(),
                metadata,
                prompt,
                turn: input.turn.clone(),
            },
            InputKind::Edit { edit_id } => {
                let hand = self.turn(&input.turn).and_then(|turn| turn.hand.as_deref());
                Record::Status {
                    status: Status::Replaced {
                        digest: input_digest(&prompt, hand),
                        edit: input_digest(&metadata, None),
                        edit_id: edit_id.clone(),
                        prompt,
                    },
                    turn: input.turn.clone(),
                }
            }
            InputKind::Steer { .. } => return Ok(()),
        };
        self.fold_in(&record, None)
    }

    #[must_use]
    pub fn knows_state(&self, state: &str) -> bool {
        self.checkpoints.contains_key(state)
            || self.owners.contains_key(state)
            || self.state_index.contains_key(state)
    }

    #[must_use]
    pub fn logs_facts(&self, state: &str) -> bool {
        self.state_index.contains_key(state)
    }

    #[must_use]
    pub fn cache_due(&self, state: &str) -> bool {
        if crate::sim::mutated(crate::sim::Mutation::CacheNeverDue) {
            return false;
        }
        let cached = self.hosts.get(&cache_key(state)).map_or(0, |value| {
            cached_at(value).map_or(value.get().len() as u64, |at| at.bytes)
        });
        let ratio = crate::SNAPSHOT_RATIO as u64;
        self.uncached
            .get(state)
            .is_some_and(|bytes| *bytes > ratio * cached)
    }

    pub fn cached_state(&self, state: &str) -> Result<Option<CachedFold>, StoreError> {
        let Some(value) = self.hosts.get(&cache_key(state)) else {
            return Ok(None);
        };
        if let Some(at) = cached_at(value) {
            return Ok(Some(CachedFold::At(at)));
        }
        serde_json::from_str(value.get())
            .map(|snapshot| Some(CachedFold::Inline(snapshot)))
            .map_err(|error| invalid(format!("cached fold of {state} does not decode: {error}")))
    }

    fn fact_bytes(&self, fact: &StoredFact) -> u64 {
        let bytes = std::cell::Cell::new(0);
        let measured: Result<_, std::convert::Infallible> = fact.try_map(
            |value| {
                bytes.set(
                    bytes.get()
                        + match value {
                            Value::Inline(json) => json.get().len() as u64,
                            Value::Blob(blob) => self
                                .values
                                .digest(*blob)
                                .and_then(|digest| self.stored.get(digest.key().as_str()))
                                .map_or(0, |json| json.len() as u64),
                        },
                );
                Ok(())
            },
            |decision| {
                bytes.set(bytes.get() + decision.get().len() as u64);
                Ok(())
            },
        );
        let Ok(_) = measured;
        bytes.get()
    }

    pub fn cached(&self) -> Result<Option<Snapshot>, StoreError> {
        self.hosts
            .get(FOLD_SNAPSHOT_KEY)
            .map(|value| {
                serde_json::from_str(value.get())
                    .map_err(|error| invalid(format!("cached fold does not decode: {error}")))
            })
            .transpose()
    }

    pub fn blob(&self, digest: &Digest) -> Result<&RawValue, FoldError> {
        let value = self
            .stored
            .get(digest.key().as_str())
            .ok_or(FoldError::MissingBlob { digest: *digest })?;
        if Digest::of(value.as_bytes()) != *digest {
            return Err(FoldError::CorruptBlob { digest: *digest });
        }
        serde_json::from_str(value).map_err(|_| FoldError::CorruptBlob { digest: *digest })
    }

    pub fn apply(&mut self, record: &Record, row: Option<u64>) -> Result<(), StoreError> {
        if let Some(pending) = self.defer(record) {
            return wait(pending, record.clone());
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
                self.retired.remove(&tag(turn));
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
            Record::Fact { fact, seq, state } => {
                let next = self.next_fact();
                if *seq != next {
                    return Err(invalid(format!(
                        "fact {seq} does not follow fact {}",
                        next - 1
                    )));
                }
                if let Some(state) = state.as_deref() {
                    let bytes = self.fact_bytes(fact);
                    *self.uncached.entry(state.to_owned()).or_default() += bytes;
                }
                let state = state.as_deref().map(|state| self.state_id(state));
                if *seq > self.folded_through
                    || crate::sim::mutated(crate::sim::Mutation::SealedRelowered)
                {
                    self.values
                        .observe(*seq, fact)
                        .map_err(|error| invalid(error.to_string()))?;
                    if matches!(fact, Fact::Marker(Marker::Blob { .. })) {
                        self.blob_seqs.push(*seq);
                    }
                    self.lower(*seq, state, fact)?;
                }
                self.facts.push(fact.clone());
                self.fact_states.push(state);
            }
            Record::Facts { first, last, row } => {
                if !self.facts.is_empty() || *first != self.fact_base + 1 || last < first {
                    return Err(invalid(format!(
                        "sealed facts {first}..={last} do not follow fact {}",
                        self.next_fact() - 1
                    )));
                }
                self.fact_base = *last;
                self.fact_segments.push(Segment {
                    first: *first,
                    last: *last,
                    row: *row,
                });
            }
            Record::Folded {
                through,
                values,
                states,
                blob_seqs,
            } => {
                let unsequenced = values.len().checked_sub(blob_seqs.len()).ok_or_else(|| {
                    invalid(format!(
                        "a fold places {} blob facts but holds {} blobs",
                        blob_seqs.len(),
                        values.len()
                    ))
                })?;
                self.unsequenced_blobs = u32::try_from(unsequenced).map_err(|_| {
                    invalid(format!(
                        "a fold holds {unsequenced} blobs placed by no fact"
                    ))
                })?;
                self.blob_seqs.clone_from(blob_seqs);
                for state in states {
                    self.state_id(state);
                }
                if *through < self.fact_base {
                    return Err(invalid(format!(
                        "a fold through fact {through} precedes sealed fact {}",
                        self.fact_base
                    )));
                }
                self.folded_through = *through;
                self.values = values.clone();
                self.values.reindex();
            }
            Record::Intended {
                call_id,
                input,
                state,
                tool,
                turn,
            } => {
                let state = state.as_deref().map(|state| self.state_id(state));
                self.derived_calls.insert(call_id.clone(), state);
                self.calls.insert(
                    call_id.clone(),
                    OpenCall {
                        effect: EffectClass::External,
                        input: input.clone(),
                        tool: tool.clone(),
                        turn: turn.clone(),
                    },
                );
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
            Record::Patched {
                base,
                edits,
                revision,
                state,
            } => {
                let payload = self
                    .checkpoints
                    .get(state)
                    .filter(|held| held.revision == *base)
                    .map(|held| crate::patch::apply(&held.payload, edits))
                    .ok_or_else(|| unheld(state, *base))??;
                return self.fold_in(
                    &Record::Checkpoint {
                        payload,
                        revision: *revision,
                        state: state.clone(),
                    },
                    row,
                );
            }
            Record::Checkpoint {
                payload,
                revision,
                state,
            } => {
                self.marks.remove(state);
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
                    let forgotten = self
                        .calls
                        .range(origin.clone()..)
                        .map(|(id, _)| id)
                        .take_while(|id| id.starts_with(&origin))
                        .cloned()
                        .collect::<Vec<_>>();
                    for id in forgotten {
                        self.calls.remove(&id);
                    }
                }
            }
            Record::Hold { hold, key } => {
                let previous = self.holds.get(key);
                if !hold.follows(previous) {
                    return Err(invalid(format!(
                        "hold {key} moves from {} to {}",
                        previous.map_or("nothing", Hold::phase),
                        hold.phase()
                    )));
                }
                let forgets = crate::sim::mutated(crate::sim::Mutation::HoldForgetsAnswer)
                    && matches!(hold, Hold::Running { .. });
                if !forgets {
                    self.holds.insert(key.clone(), hold.clone());
                }
            }
            Record::Host { key, value } if key == RETIRED_KEY => {
                self.retired_from(value)?;
                let gone: BTreeSet<String> = self
                    .turns
                    .iter()
                    .filter(|turn| self.retired.contains_key(&tag(&turn.id)))
                    .map(|turn| turn.id.clone())
                    .collect();
                self.drop_turns(|turn| gone.contains(&turn.id));
            }
            Record::Host { key, value } => {
                if let Some(state) = cached_state_of(key) {
                    self.uncached.remove(state);
                }
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
            Record::Groups { groups, chains } => {
                self.groups = groups
                    .iter()
                    .map(|&(row, rows, count)| (row, (rows, count)))
                    .collect();
                self.chains.extend(chains.iter().copied());
            }
            Record::Index { first, row, rows } => {
                self.pages.insert(
                    first.clone(),
                    Page::Cold(Section {
                        row: *row,
                        rows: *rows,
                    }),
                );
            }
            Record::Root { row, rows } => {
                self.root = Some(Page::Cold(Section {
                    row: *row,
                    rows: *rows,
                }));
            }
            Record::Owner { owner, .. } => {
                return Err(invalid(format!(
                    "owner {owner} is listed outside an index page"
                )));
            }
            Record::Placed {
                chain,
                owners,
                row,
                rows,
            } => self.placed_in_tail(
                owners,
                Section {
                    row: *row,
                    rows: *rows,
                },
                *chain,
            )?,
            Record::Section { owners, row, rows } => {
                let section = Section {
                    row: *row,
                    rows: *rows,
                };
                self.groups.entry(*row).or_insert((*rows, 0)).1 += owners.len() as u64;
                for (owner, checkpoint) in owners {
                    if let Owner::Cold { entry, .. } = self
                        .owners
                        .entry(owner.clone())
                        .or_insert_with(|| Owner::Cold {
                            entry: Entry {
                                legacy: true,
                                ..Entry::default()
                            },
                            pending: Vec::new(),
                        })
                    {
                        entry.checkpoint |= *checkpoint;
                        entry.sections.push(section);
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
            Record::Fired { key, turn, .. } => {
                if self.turn(turn).is_none() {
                    return Err(invalid(format!(
                        "wake {key} fired turn {turn}, which the log did not admit"
                    )));
                }
                self.wakes.apply(record).map_err(invalid)?;
            }
            Record::Armed { .. }
            | Record::Disarmed { .. }
            | Record::Heard { .. }
            | Record::Woken { .. } => self.wakes.apply(record).map_err(invalid)?,
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
                if !crate::sim::mutated(crate::sim::Mutation::FinishedKeepsPrompt) {
                    turn.prompt = RawValue::NULL.to_owned();
                }
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
        self.snapshot_records(keep, self.fact_base + 1, |record| {
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
        keep_fact: u64,
        take: impl FnMut(Record) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        self.records(keep, keep_fact, true, take)
    }

    pub(crate) fn head_records(
        &self,
        keep: u64,
        keep_fact: u64,
        index: Option<Section>,
        mut take: impl FnMut(Record) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        if let Some(section) = index {
            take(Record::Root {
                row: section.row,
                rows: section.rows,
            })?;
        }
        self.records(keep, keep_fact, false, take)
    }

    fn records(
        &self,
        keep: u64,
        keep_fact: u64,
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
        let cut = self.fact_cut(keep_fact);
        let resident_blobs = self
            .blob_keys(|seq| seq >= cut)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let sealed_blobs = if crate::sim::mutated(crate::sim::Mutation::BlobsResident) {
            BTreeSet::new()
        } else {
            self.blob_keys(|seq| seq < cut)
                .into_iter()
                .collect::<BTreeSet<_>>()
        };
        for (key, value) in &self.stored {
            if (!owned && key.contains('\n'))
                || resident_blobs.contains(key)
                || sealed_blobs.contains(key)
            {
                continue;
            }
            take(Record::Stored {
                key: key.clone(),
                value: value.clone(),
            })?;
        }
        for segment in self
            .fact_segments
            .iter()
            .copied()
            .chain(self.sealed_fact_segment(keep_fact))
        {
            take(Record::Facts {
                first: segment.first,
                last: segment.last,
                row: segment.row,
            })?;
        }
        take(Record::Folded {
            through: self.next_fact() - 1,
            values: self.values.clone(),
            states: self.state_names.clone(),
            blob_seqs: self.blob_seqs.clone(),
        })?;
        for (state, checkpoint) in self.checkpoints.iter().filter(|_| owned) {
            take(Record::Checkpoint {
                payload: checkpoint.payload.clone(),
                revision: checkpoint.revision,
                state: state.clone(),
            })?;
        }
        if !self.retired.is_empty() {
            take(Record::Host {
                key: RETIRED_KEY.to_owned(),
                value: self.retired_value()?,
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
                let at = match recovered.at {
                    None if !crate::sim::mutated(crate::sim::Mutation::SnapshotResetsRecovery) => {
                        Some(UNPINNED_RECOVERY_ROW)
                    }
                    at => at,
                };
                take(Record::Recovery {
                    at,
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
            if let Some(state) = self.derived_calls.get(call_id) {
                take(Record::Intended {
                    call_id: call_id.clone(),
                    input: call.input.clone(),
                    state: state.map(|state| self.state_names[state as usize].clone()),
                    tool: call.tool.clone(),
                    turn: call.turn.clone(),
                })?;
                continue;
            }
            take(Record::Call {
                call_id: call_id.clone(),
                effect: call.effect,
                input: call.input.clone(),
                tool: call.tool.clone(),
                turn: call.turn.clone(),
            })?;
        }
        for (key, hold) in &self.holds {
            take(Record::Hold {
                hold: hold.clone(),
                key: key.clone(),
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
        self.wakes.records(&mut take)?;
        for key in &resident_blobs {
            if let Some(value) = self.stored.get(key) {
                take(Record::Stored {
                    key: key.clone(),
                    value: value.clone(),
                })?;
            }
        }
        for ((seq, fact), state) in self.facts().zip(&self.fact_states) {
            if seq < cut {
                continue;
            }
            take(Record::Fact {
                fact: fact.clone(),
                seq,
                state: state.map(|state| self.state_names[state as usize].clone()),
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

#[must_use]
pub fn internal_host(key: &str) -> bool {
    key == RETIRED_KEY || key == FOLD_SNAPSHOT_KEY || cached_state_of(key).is_some()
}

#[must_use]
pub fn cache_key(state: &str) -> String {
    format!("{FOLD_SNAPSHOT_KEY}\n{state}")
}

fn cached_state_of(key: &str) -> Option<&str> {
    key.strip_prefix(FOLD_SNAPSHOT_KEY)?.strip_prefix('\n')
}

#[must_use]
pub fn input_digest(value: &RawValue, hand: Option<&str>) -> String {
    let mut bytes = value.get().as_bytes().to_vec();
    if let Some(hand) = hand {
        bytes.push(0);
        bytes.extend_from_slice(hand.as_bytes());
    }
    Digest::of(&bytes).key().as_str().to_owned()
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
                FactOutcome::Cancelled => Outcome::Cancelled,
                FactOutcome::Completed => Outcome::Completed,
                FactOutcome::Failed => Outcome::Failed,
            },
            result: result.clone(),
        },
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

#[cfg(test)]
mod stored_bytes_tests {
    use super::*;

    #[test]
    fn stored_bytes_counts_every_stored_record_once() {
        let mut fold = Fold::default();
        assert_eq!(fold.stored_bytes(), 0);
        let stored = |key: &str, value: &str| Record::Stored {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        fold.apply(
            &stored(
                "state
b3:a",
                "1234",
            ),
            None,
        )
        .unwrap();
        fold.apply(
            &stored(
                "state
b3:b",
                "12",
            ),
            None,
        )
        .unwrap();
        assert_eq!(
            fold.stored_bytes(),
            2 * "state
b3:a"
                .len()
                + 6
        );
        fold.apply(
            &stored(
                "state
b3:a",
                "1",
            ),
            None,
        )
        .unwrap();
        assert_eq!(
            fold.stored_bytes(),
            2 * "state
b3:a"
                .len()
                + 3
        );
    }
}
