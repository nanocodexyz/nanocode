use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    ops::Range,
    sync::Arc,
    time::Duration,
};

use nanocode_durability_core::{
    Digest, Fact, FactSink, FoldError, InputKind, Marker, OwnedFact, Snapshot as FoldSnapshot,
    SqlStatement, StoreError, StoreFuture, StoredFact, VALUE_INLINE_BYTES, Value,
};
#[cfg(not(target_family = "wasm"))]
use nanocode_sync::future::Shared;
use nanocode_sync::{
    FutureExt,
    future::{Either, select},
    mpsc, oneshot,
};
use nanocode_trace::Instrument as _;
use serde_json::value::RawValue;

use crate::{
    Checkpoint, Compaction, Event, Extension, Fold, LogHost, Record, Rider, Segment, Write,
    codec::{
        self, ARCHIVE, BOUNDARY, History, LINKED, PART, RECORDS, SEAL, SECTION, SNAPSHOT, Stream,
        WINDOW,
    },
    fold::{CachedAt, CachedFold, Entry, FOLD_SNAPSHOT_KEY, Page, Section, legacy_name},
};

pub const EVENT_GAP: u64 = 1 << 14;
pub const SNAPSHOT_RATIO: usize = 2;
pub const FIRST_ROW: u64 = 1;
const CLAIM: &str = "writer";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Point {
    Admission,
    Call,
    Cancel,
    Finish,
    Floor,
    Progress,
    Recovery,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub admissions: u64,
    pub boundaries: u64,
    pub calls: u64,
    pub cancels: u64,
    pub collected: u64,
    pub finishes: u64,
    pub floors: u64,
    pub index_loads: u64,
    pub page_rows: u64,
    pub pages: u64,
    pub parts: u64,
    pub placed_bytes: u64,
    pub plain_bytes: u64,
    pub progress: u64,
    pub recoveries: u64,
    pub retired: u64,
    pub row_bytes: u64,
    pub section_loads: u64,
    pub sections: u64,
    pub snapshot_bytes: u64,
    pub snapshots: u64,
}

impl Stats {
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.admissions
            + self.calls
            + self.cancels
            + self.finishes
            + self.floors
            + self.parts
            + self.progress
            + self.recoveries
            + self.sections
    }
}

#[derive(Debug)]
pub struct Row {
    pub boundary: bool,
    pub bytes: usize,
    pub plain: String,
    pub seq: u64,
    pub snapshot: bool,
}

impl Row {
    pub fn records(&self) -> Result<Vec<Record>, StoreError> {
        crate::decode(&self.plain)
    }
}

struct Snapshot {
    base: usize,
    packed: String,
    plain: usize,
    start: usize,
    tail: Vec<u8>,
}

impl Snapshot {
    fn suffix(&self) -> &[u8] {
        &self.tail[self.start - self.base..]
    }

    const fn trimmed(&self) -> bool {
        self.start > 0
    }
}

struct Partitioned {
    head: Snapshot,
    index: Index,
    rewrite: usize,
    sections: Sections,
    seq: u64,
}

struct Sections {
    bytes: usize,
    entries: BTreeMap<String, Option<Entry>>,
    groups: Vec<(Section, u64)>,
    next: u64,
    plain: u64,
    rewritten: u64,
    rows: Vec<(u64, String)>,
}

struct Index {
    bytes: usize,
    groups: BTreeMap<u64, (u64, u64)>,
    next: u64,
    pages: BTreeMap<String, Page>,
    root: Option<Section>,
    rows: Vec<(u64, String)>,
    superseded: Vec<Range<u64>>,
}

impl Index {
    fn seal(
        &mut self,
        spare: &mut codec::Spare,
        first: String,
        packer: codec::Packer,
        limit: Option<usize>,
    ) -> Result<(), StoreError> {
        let rows = stored(self.next, packer.finish(SECTION, spare)?.packed, limit)?;
        self.bytes += rows.iter().map(|(_, body)| body.len()).sum::<usize>();
        self.pages.insert(
            first,
            Page::Loaded(Section {
                row: self.next,
                rows: rows.len() as u64,
            }),
        );
        self.next += rows.len() as u64;
        self.rows.extend(rows);
        Ok(())
    }
}

impl Sections {
    fn seal(
        &mut self,
        spare: &mut codec::Spare,
        packer: codec::Packer,
        owners: Vec<(String, Entry)>,
        limit: Option<usize>,
    ) -> Result<(), StoreError> {
        let rows = stored(self.next, packer.finish(SECTION, spare)?.packed, limit)?;
        self.bytes += rows.iter().map(|(_, body)| body.len()).sum::<usize>();
        let section = Section {
            row: self.next,
            rows: rows.len() as u64,
        };
        self.groups.push((section, owners.len() as u64));
        for (owner, mut entry) in owners {
            entry.sections.push(section);
            self.entries.insert(owner, Some(entry));
        }
        self.next += rows.len() as u64;
        self.rows.extend(rows);
        Ok(())
    }
}

fn line(lines: &mut Vec<u8>, record: &Record) -> Result<(), StoreError> {
    if !lines.is_empty() {
        lines.push(b'\n');
    }
    crate::record::write(record, lines)
}

struct Placing {
    keyframe: u64,
    keys: BTreeSet<String>,
    owners: Vec<(String, u64)>,
    plain: Vec<u8>,
    rows: Vec<(u64, String)>,
    section: Section,
    state: nanocode_compression::xz::LzmaState,
}

fn placing(
    fold: &Fold,
    history: &History,
    buffer: &[Record],
    seq: u64,
    limit: Option<usize>,
    chain_bytes: Option<usize>,
) -> Result<Option<Placing>, StoreError> {
    let mut values = BTreeMap::new();
    for record in buffer {
        if let Record::Stored { key, value } = record
            && fold.placeable(key)
        {
            values.insert(key.as_str(), value.as_str());
        }
    }
    if values.is_empty() {
        return Ok(None);
    }
    let keyframe = match history.keyframe() {
        Some(_) if chain_bytes.is_some_and(|max| history.chain_bytes() >= max) => seq,
        Some(keyframe) => keyframe,
        None => seq,
    };
    let mut plain = Vec::new();
    let mut lines = Vec::new();
    let mut owners = Vec::with_capacity(values.len());
    for (key, value) in values {
        lines.clear();
        line(
            &mut lines,
            &Record::Stored {
                key: key.to_owned(),
                value: value.to_owned(),
            },
        )?;
        if !plain.is_empty() {
            plain.push(b'\n');
        }
        plain.extend_from_slice(&lines);
        owners.push((key.to_owned(), lines.len() as u64));
    }
    let (body, state) = codec::link(history, keyframe, seq - keyframe, &plain)?;
    let rows = stored(seq, body, limit)?;
    let section = Section {
        row: seq,
        rows: rows.len() as u64,
    };
    Ok(Some(Placing {
        keyframe,
        keys: owners.iter().map(|(owner, _)| owner.clone()).collect(),
        owners,
        plain,
        rows,
        section,
        state,
    }))
}

fn sections(
    spare: &mut codec::Spare,
    fold: &Fold,
    from: u64,
    level: i32,
    limit: Option<usize>,
    ratio: usize,
) -> Result<Sections, StoreError> {
    let mut sections = Sections {
        bytes: 0,
        entries: BTreeMap::new(),
        groups: Vec::new(),
        next: from,
        plain: 0,
        rewritten: 0,
        rows: Vec::new(),
    };
    let mut open: Option<(codec::Packer, Vec<(String, Entry)>)> = None;
    let mut lines = Vec::new();
    for owner in fold.writes() {
        lines.clear();
        let changed = fold.delta_records(owner, |record| line(&mut lines, &record))?;
        let base = fold.entry(owner).cloned().unwrap_or_default();
        let full = fold.loaded(owner)
            && (base.sections.is_empty()
                || base.written + lines.len() as u64 >= ratio as u64 * base.full);
        let checkpoint = if full {
            lines.clear();
            fold.owner_records(owner, |record| line(&mut lines, &record))?
        } else {
            base.checkpoint || changed
        };
        let bytes = lines.len() as u64;
        sections.plain += bytes;
        if full && !base.sections.is_empty() {
            sections.rewritten += bytes;
        }
        if lines.is_empty() {
            sections
                .entries
                .insert(owner.to_owned(), (!full).then_some(base));
            continue;
        }
        let entry = if full {
            Entry {
                checkpoint,
                full: bytes,
                legacy: false,
                sections: Vec::new(),
                written: bytes,
            }
        } else {
            Entry {
                checkpoint,
                full: base.full,
                legacy: base.legacy,
                sections: base.sections,
                written: base.written + bytes,
            }
        };
        let (packer, owners) = match &mut open {
            Some(open) => open,
            None => open.insert((codec::Packer::new(spare, level, &[])?, Vec::new())),
        };
        if packer.plain() > 0 {
            packer.write(b"\n")?;
        }
        packer.write(&lines)?;
        owners.push((owner.to_owned(), entry));
        if packer.plain() >= WINDOW
            && let Some((packer, owners)) = open.take()
        {
            sections.seal(spare, packer, owners, limit)?;
        }
    }
    if let Some((packer, owners)) = open.take() {
        sections.seal(spare, packer, owners, limit)?;
    }
    Ok(sections)
}

fn index(
    spare: &mut codec::Spare,
    fold: &Fold,
    sections: &Sections,
    level: i32,
    limit: Option<usize>,
) -> Result<Index, StoreError> {
    let plan = fold.plan(&sections.entries, &sections.groups)?;
    let mut index = Index {
        bytes: 0,
        groups: plan.groups,
        next: sections.next,
        pages: plan.kept,
        root: None,
        rows: Vec::new(),
        superseded: plan.superseded,
    };
    let mut lines = Vec::new();
    for (first, records) in plan.pages {
        let mut fence = Some(first);
        let mut open: Option<(String, codec::Packer)> = None;
        for record in records {
            let Record::Owner { owner, .. } = &record else {
                continue;
            };
            let (_, packer) = match &mut open {
                Some(open) => open,
                None => open.insert((
                    fence.take().unwrap_or_else(|| owner.clone()),
                    codec::Packer::new(spare, level, &[])?,
                )),
            };
            lines.clear();
            line(&mut lines, &record)?;
            if packer.plain() > 0 {
                packer.write(b"\n")?;
            }
            packer.write(&lines)?;
            if packer.plain() >= WINDOW
                && let Some((first, packer)) = open.take()
            {
                index.seal(spare, first, packer, limit)?;
            }
        }
        if let Some((first, packer)) = open.take() {
            index.seal(spare, first, packer, limit)?;
        }
    }
    if let Some((first, _)) = index.pages.first_key_value()
        && !first.is_empty()
        && let Some((_, page)) = index.pages.pop_first()
    {
        index.pages.insert(String::new(), page);
    }
    index.root = match fold.indexed(&index.pages, &index.groups) {
        Some(section) => Some(section),
        None => {
            if let Some(old) = fold.root_section() {
                index.superseded.push(old.row..old.row + old.rows);
                index.superseded.sort_by_key(|range| range.start);
            }
            if index.pages.is_empty() && index.groups.is_empty() {
                None
            } else {
                let mut packer = codec::Packer::new(spare, level, &[])?;
                for (first, page) in &index.pages {
                    let section = page.section();
                    lines.clear();
                    line(
                        &mut lines,
                        &Record::Index {
                            first: first.clone(),
                            row: section.row,
                            rows: section.rows,
                        },
                    )?;
                    if packer.plain() > 0 {
                        packer.write(b"\n")?;
                    }
                    packer.write(&lines)?;
                }
                if !index.groups.is_empty() {
                    lines.clear();
                    line(
                        &mut lines,
                        &Record::Groups {
                            groups: index
                                .groups
                                .iter()
                                .map(|(&row, &(rows, count))| (row, rows, count))
                                .collect(),
                            chains: fold.chains_of(&index.groups),
                        },
                    )?;
                    if packer.plain() > 0 {
                        packer.write(b"\n")?;
                    }
                    packer.write(&lines)?;
                }
                let rows = stored(index.next, packer.finish(SECTION, spare)?.packed, limit)?;
                index.bytes += rows.iter().map(|(_, body)| body.len()).sum::<usize>();
                let section = Section {
                    row: index.next,
                    rows: rows.len() as u64,
                };
                index.next += rows.len() as u64;
                index.rows.extend(rows);
                Some(section)
            }
        }
    };
    Ok(index)
}

#[derive(Default)]
struct Chains(BTreeMap<u64, (u64, History)>);

impl Chains {
    fn clear(&mut self) {
        self.0.clear();
    }
}

const SECTIONS_PER_CALL: usize = 64;

#[derive(Default)]
struct Reads {
    covered: Vec<Range<u64>>,
    rows: BTreeMap<u64, String>,
}

impl Reads {
    fn holds(&self, range: &Range<u64>) -> bool {
        self.covered
            .iter()
            .any(|held| held.start <= range.start && range.end <= held.end)
    }

    async fn fetch(
        &mut self,
        host: &dyn LogHost,
        scope: &str,
        mut wanted: Vec<Range<u64>>,
    ) -> Result<(), StoreError> {
        if crate::sim::mutated(crate::sim::Mutation::ScansUnbatched) {
            return Ok(());
        }
        wanted.retain(|range| !range.is_empty() && !self.holds(range));
        wanted.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<u64>> = Vec::with_capacity(wanted.len());
        for range in wanted {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        for batch in merged.chunks(SECTIONS_PER_CALL) {
            let answered = host.scan_ranges(scope, batch).await?;
            if answered.len() != batch.len() {
                return Err(StoreError::Backend(format!(
                    "turn log host answered {} of {} row ranges of {scope}",
                    answered.len(),
                    batch.len()
                )));
            }
            for (range, rows) in batch.iter().zip(answered) {
                for (seq, body) in rows {
                    if !range.contains(&seq) {
                        return Err(StoreError::Backend(format!(
                            "turn log host answered row {seq} of {scope} for rows {}..{}",
                            range.start, range.end
                        )));
                    }
                    self.rows.insert(seq, body);
                }
                self.covered.push(range.clone());
            }
        }
        Ok(())
    }

    async fn scan(
        &self,
        host: &dyn LogHost,
        scope: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, String)>, StoreError> {
        if self.holds(&(from..to)) {
            return Ok(self
                .rows
                .range(from..to)
                .map(|(seq, body)| (*seq, body.clone()))
                .collect());
        }
        host.scan(scope, from, to).await
    }
}

fn chained(fold: &Fold, chains: &Chains, section: Section) -> Range<u64> {
    let from = match fold.keyframe(section.row) {
        Some(keyframe) if keyframe < section.row => match chains.0.get(&keyframe) {
            Some((next, _)) if *next <= section.row => *next,
            _ => keyframe,
        },
        _ => section.row,
    };
    from..section.row + section.rows
}

async fn read_sections(
    host: &dyn LogHost,
    scope: &str,
    fold: &Fold,
    chains: &mut Chains,
    sections: &[(String, Section)],
) -> Result<Vec<Vec<Record>>, StoreError> {
    let mut read = Vec::with_capacity(sections.len());
    for batch in sections.chunks(SECTIONS_PER_CALL) {
        let wanted = batch
            .iter()
            .map(|(_, section)| chained(fold, chains, *section))
            .collect();
        let mut reads = Reads::default();
        reads.fetch(host, scope, wanted).await?;
        for (owner, section) in batch {
            read.push(section_records(&reads, host, scope, owner, *section, chains).await?);
        }
    }
    Ok(read)
}

async fn index_pages(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
    pages: Vec<(String, Section)>,
) -> Result<(), StoreError> {
    let read = read_sections(host, scope, fold, chains, &pages).await?;
    for ((first, _), records) in pages.iter().zip(read) {
        fold.paged(first, records)?;
        stats.index_loads += 1;
    }
    Ok(())
}

async fn section_records(
    reads: &Reads,
    host: &dyn LogHost,
    scope: &str,
    owner: &str,
    section: Section,
    chains: &mut Chains,
) -> Result<Vec<Record>, StoreError> {
    let end = section.row + section.rows;
    let mut stored = reads.scan(host, scope, section.row, end).await?;
    let linked = stored
        .first()
        .is_some_and(|(_, body)| body.starts_with(LINKED));
    if stored.first().map(|(seq, _)| *seq) != Some(section.row)
        || stored.len() as u64 != section.rows
        || !(linked || stored[0].1.starts_with(SECTION))
    {
        return Err(StoreError::Backend(format!(
            "turn log section of {owner} at rows {}..{end} reads {} rows",
            section.row,
            stored.len()
        )));
    }
    let mut history = History::default();
    let mut keyframe = None;
    if linked {
        let (back, _) = codec::linked(&stored[0].1[LINKED.len_utf8()..])?;
        let first = section.row.checked_sub(back).ok_or_else(|| {
            StoreError::Backend(format!(
                "turn log section of {owner} at row {} links back past the log's start",
                section.row
            ))
        })?;
        keyframe = Some(first);
        if first < section.row {
            let from = match chains.0.remove(&first) {
                Some((next, held)) if next <= section.row => {
                    history = held;
                    next
                }
                _ => first,
            };
            nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
                monotonic_counter.nc.turnlog.chain_rows_skipped = from - first
            });
            if from < section.row {
                stored = chain_rows(reads.scan(host, scope, from, end).await?);
            }
        }
    }
    let mut records = Vec::new();
    each_row(stored, &mut history, |row| {
        if row.seq >= section.row {
            crate::record::each(&row.plain, |record| {
                records.push(record);
                Ok(())
            })?;
        }
        Ok(true)
    })?;
    if let Some(keyframe) = keyframe
        && history.keyframe() == Some(keyframe)
    {
        chains.0.insert(keyframe, (end, history));
    }
    Ok(records)
}

async fn root(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
) -> Result<(), StoreError> {
    if let Some(section) = fold.cold_root() {
        let records = section_records(
            &Reads::default(),
            host,
            scope,
            "the index root",
            section,
            chains,
        )
        .await?;
        fold.rooted(records)?;
        stats.index_loads += 1;
    }
    Ok(())
}

async fn page(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
    owner: &str,
) -> Result<(), StoreError> {
    fold.unshelve_owner(owner);
    root(host, scope, fold, stats, chains).await?;
    if let Some(page) = fold.unpaged(owner) {
        index_pages(host, scope, fold, stats, chains, vec![page]).await?;
    }
    Ok(())
}

enum Written<'a> {
    Appended(&'a [(u64, String)]),
    Extended(Extension<'a>, usize),
}

#[derive(PartialEq, Eq)]
enum Unsettled {
    Appended(Vec<(u64, String)>),
    Extended {
        cut: usize,
        row: u64,
        seq: u64,
        tail: String,
        merged: usize,
    },
}

impl Unsettled {
    fn of(written: &Written<'_>) -> Self {
        match written {
            Written::Appended(rows) => Self::Appended(rows.to_vec()),
            Written::Extended(extension, merged) => Self::Extended {
                cut: extension.cut,
                row: extension.row,
                seq: extension.seq,
                tail: extension.tail.to_owned(),
                merged: *merged,
            },
        }
    }

    fn written(&self) -> Written<'_> {
        match self {
            Self::Appended(rows) => Written::Appended(rows),
            Self::Extended {
                cut,
                row,
                seq,
                tail,
                merged,
            } => Written::Extended(
                Extension {
                    cut: *cut,
                    row: *row,
                    seq: *seq,
                    tail,
                },
                *merged,
            ),
        }
    }

    fn repeats(
        &self,
        extension: Option<Extension<'_>>,
        merged: Option<usize>,
        rows: &[(u64, String)],
    ) -> bool {
        match (self, extension.zip(merged)) {
            (Self::Appended(earlier), None) => earlier == rows,
            (Self::Extended { .. }, Some((extension, merged))) => {
                *self == Self::of(&Written::Extended(extension, merged))
            }
            _ => false,
        }
    }
}

enum Found {
    Absent,
    Ours,
    Other,
}

async fn read_back(
    host: &dyn LogHost,
    scope: &str,
    written: &Written<'_>,
) -> Result<Found, StoreError> {
    let (from, to) = match written {
        Written::Appended(rows) => match (rows.first(), rows.last()) {
            (Some((first, _)), Some((last, _))) => (*first, last + 1),
            _ => return Ok(Found::Absent),
        },
        Written::Extended(extension, _) => (extension.seq, extension.seq + 1),
    };
    let found = host.scan(scope, from, to).await?;
    let ours = match written {
        Written::Appended(rows) => codec::same_rows(&found, rows),
        Written::Extended(extension, merged) => match found.as_slice() {
            [(seq, body)] => {
                *seq == extension.seq && body.len() == *merged && body.ends_with(extension.tail)
            }
            _ => false,
        },
    };
    Ok(match (found.is_empty(), ours) {
        (true, _) => Found::Absent,
        (false, true) => Found::Ours,
        (false, false) => Found::Other,
    })
}

async fn landed(
    host: &dyn LogHost,
    scope: &str,
    written: &Written<'_>,
    error: StoreError,
) -> Result<(), StoreError> {
    match read_back(host, scope, written).await {
        Ok(Found::Ours) => Ok(()),
        Ok(Found::Other) => Err(StoreError::Fenced),
        Ok(Found::Absent) | Err(_) => Err(error),
    }
}

async fn warm(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
    owners: &[String],
) -> Result<(), StoreError> {
    locate(host, scope, fold, stats, chains, owners).await?;
    load_many(host, scope, fold, stats, chains, owners).await
}

async fn locate(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
    owners: &[String],
) -> Result<(), StoreError> {
    let legacy = owners
        .iter()
        .filter_map(|owner| legacy_name(owner))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !legacy.is_empty() {
        pages(host, scope, fold, stats, chains, &legacy).await?;
        for name in &legacy {
            if fold.is_legacy(name) {
                load(host, scope, fold, stats, chains, name).await?;
            }
        }
    }
    pages(host, scope, fold, stats, chains, owners).await
}

async fn pages(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
    owners: &[String],
) -> Result<(), StoreError> {
    for owner in owners {
        fold.unshelve_owner(owner);
    }
    root(host, scope, fold, stats, chains).await?;
    let pages = owners
        .iter()
        .filter_map(|owner| fold.unpaged(owner))
        .collect::<BTreeMap<_, _>>();
    index_pages(
        host,
        scope,
        fold,
        stats,
        chains,
        pages.into_iter().collect(),
    )
    .await
}

async fn load(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
    owner: &str,
) -> Result<(), StoreError> {
    page(host, scope, fold, stats, chains, owner).await?;
    if fold.is_legacy(owner) {
        let pages = fold.key_pages(owner.trim_end_matches('\n'));
        index_pages(host, scope, fold, stats, chains, pages).await?;
    }
    let Some(sections) = fold.cold(owner).map(<[Section]>::to_vec) else {
        return Ok(());
    };
    let legacy = fold.is_legacy(owner);
    let mut groups = Vec::with_capacity(sections.len());
    for section in sections {
        let records =
            section_records(&Reads::default(), host, scope, owner, section, chains).await?;
        if !legacy {
            let pages = fold.group_pages(&records);
            index_pages(host, scope, fold, stats, chains, pages).await?;
        }
        groups.push((section, records));
    }
    stats.section_loads += groups.len() as u64;
    fold.warm(owner, groups)
}

async fn load_many(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
    owners: &[String],
) -> Result<(), StoreError> {
    let mut plain = Vec::with_capacity(owners.len());
    for owner in owners {
        if fold.is_legacy(owner) {
            load(host, scope, fold, stats, chains, owner).await?;
        } else {
            plain.push(owner);
        }
    }
    let mut wanted = BTreeMap::new();
    let mut uses = BTreeMap::new();
    for owner in &plain {
        for section in fold.cold(owner).unwrap_or_default() {
            wanted
                .entry(section.row)
                .or_insert_with(|| ((*owner).clone(), *section));
            *uses.entry(section.row).or_insert(0_usize) += 1;
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }
    let sections = wanted.into_values().collect::<Vec<_>>();
    let read = read_sections(host, scope, fold, chains, &sections).await?;
    let mut decoded = sections
        .iter()
        .map(|(_, section)| section.row)
        .zip(read)
        .collect::<BTreeMap<_, _>>();
    let mut listed = BTreeMap::new();
    for records in decoded.values() {
        listed.extend(fold.group_pages(records));
    }
    index_pages(
        host,
        scope,
        fold,
        stats,
        chains,
        listed.into_iter().collect(),
    )
    .await?;
    for owner in plain {
        let Some(sections) = fold.cold(owner).map(<[Section]>::to_vec) else {
            continue;
        };
        let mut groups = Vec::with_capacity(sections.len());
        for section in sections {
            let left = uses.get_mut(&section.row).map(|left| {
                *left -= 1;
                *left
            });
            let records = match left {
                Some(0) => decoded.remove(&section.row),
                Some(_) => decoded.get(&section.row).cloned(),
                None => None,
            };
            let records = match records {
                Some(records) => records,
                None => {
                    section_records(&Reads::default(), host, scope, owner, section, chains).await?
                }
            };
            groups.push((section, records));
        }
        stats.section_loads += groups.len() as u64;
        fold.warm(owner, groups)?;
    }
    Ok(())
}

async fn warm_state(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    stats: &mut Stats,
    chains: &mut Chains,
    state: &str,
) -> Result<(), StoreError> {
    warm(host, scope, fold, stats, chains, &[state.to_owned()]).await?;
    fold.unshelve_state(state);
    let pages = fold.key_pages(state);
    index_pages(host, scope, fold, stats, chains, pages).await?;
    let keys = fold.cold_keys(state);
    warm(host, scope, fold, stats, chains, &keys).await?;
    fold.touch(state);
    Ok(())
}

fn packed(
    spare: &mut codec::Spare,
    fold: &Fold,
    (keep, keep_fact): (u64, u64),
    level: i32,
    index: Option<Section>,
) -> Result<Snapshot, StoreError> {
    let mut packer = codec::Packer::new(spare, level, &[])?;
    let mut line = Vec::new();
    let mut base = 0;
    let mut starts = VecDeque::new();
    let mut tail = Vec::new();
    fold.head_records(keep, keep_fact, index, |record| {
        if packer.plain() > 0 {
            line.push(b'\n');
        }
        let start = base + tail.len() + line.len();
        crate::record::write(&record, &mut line)?;
        packer.write(&line)?;
        tail.extend_from_slice(&line);
        line.clear();
        starts.push_back((start, sealed(&record)));
        let end = base + tail.len();
        while let (Some(&(_, false)), Some(&(next, _))) = (starts.front(), starts.get(1)) {
            if end - next < WINDOW {
                break;
            }
            starts.pop_front();
        }
        if let Some(&(front, _)) = starts.front()
            && front - base > end - front
        {
            tail.drain(..front - base);
            base = front;
        }
        Ok(())
    })?;
    let start = starts.front().map_or(0, |&(start, _)| start);
    let packed = packer.finish(SNAPSHOT, spare)?;
    Ok(Snapshot {
        base,
        packed: packed.packed,
        plain: packed.plain,
        start,
        tail,
    })
}

fn measured(fold: &Fold, keep: u64, keep_fact: u64) -> Result<usize, StoreError> {
    let mut bytes = 0;
    let mut line = Vec::new();
    fold.head_records(keep, keep_fact, None, |record| {
        crate::record::write(&record, &mut line)?;
        bytes += line.len() + usize::from(bytes > 0);
        line.clear();
        Ok(())
    })?;
    Ok(bytes)
}

pub fn rows(stored: &[(u64, String)]) -> Result<Vec<Row>, StoreError> {
    let mut decoded = Vec::new();
    each_row(stored.to_vec(), &mut History::default(), |row| {
        decoded.push(row);
        Ok(true)
    })?;
    Ok(decoded)
}

struct Logical {
    bytes: usize,
    seq: u64,
    tag: char,
    text: String,
}

fn each_row(
    stored: Vec<(u64, String)>,
    history: &mut History,
    mut take: impl FnMut(Row) -> Result<bool, StoreError>,
) -> Result<bool, StoreError> {
    let mut pending: Option<Logical> = None;
    for (seq, body) in stored {
        let mut chars = body.chars();
        match chars.next() {
            Some(PART) => match &mut pending {
                Some(logical) => {
                    logical.text.push_str(chars.as_str());
                    logical.bytes += body.len();
                }
                None => {
                    return Err(StoreError::Backend(format!(
                        "turn log row {seq} continues no row"
                    )));
                }
            },
            Some(tag @ (ARCHIVE | BOUNDARY | LINKED | RECORDS | SECTION | SNAPSHOT)) => {
                if let Some(logical) = pending.take()
                    && !take(decode_row(logical, history)?)?
                {
                    return Ok(false);
                }
                pending = Some(Logical {
                    bytes: body.len(),
                    seq,
                    tag,
                    text: body,
                });
            }
            _ => {
                return Err(StoreError::Backend(format!(
                    "turn log row {seq} has no known tag"
                )));
            }
        }
    }
    match pending {
        Some(logical) => take(decode_row(logical, history)?),
        None => Ok(true),
    }
}

fn job_site(site: &std::panic::Location<'_>) -> String {
    let file = site.file();
    let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
    format!("{name}:{}", site.line())
}

fn timed() -> bool {
    nanocode_trace::enabled!(target: "nc_metrics", nanocode_trace::Level::INFO)
}

fn decoded(tag: char, started: nanocode_platform::Instant, bytes: usize) {
    let micros = u64::try_from(
        nanocode_platform::now()
            .saturating_duration_since(started)
            .as_micros(),
    )
    .unwrap_or(u64::MAX);
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    if tag == LINKED {
        nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
            histogram.nc.turnlog.decode_linked_micros = micros,
            monotonic_counter.nc.turnlog.decoded_linked_bytes = bytes
        });
    } else if tag == SECTION {
        nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
            histogram.nc.turnlog.decode_section_micros = micros,
            monotonic_counter.nc.turnlog.decoded_section_bytes = bytes
        });
    } else {
        nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
            histogram.nc.turnlog.decode_records_micros = micros,
            monotonic_counter.nc.turnlog.decoded_records_bytes = bytes
        });
    }
}

fn decode_row(logical: Logical, history: &mut History) -> Result<Row, StoreError> {
    let Logical {
        bytes,
        seq,
        tag,
        text,
    } = logical;
    let snapshot = tag == SNAPSHOT;
    let section = tag == SECTION || tag == LINKED;
    let boundary = snapshot || tag == BOUNDARY || tag == ARCHIVE;
    if boundary {
        history.clear();
    }
    let decoding = timed().then(nanocode_platform::now);
    let plain = if tag == LINKED {
        let (back, compressed) = codec::linked(&text[tag.len_utf8()..])?;
        let keyframe = seq.checked_sub(back).ok_or_else(|| {
            StoreError::Backend(format!(
                "turn log row {seq} links back past the log's start"
            ))
        })?;
        history.decode_linked(seq, keyframe, &compressed)?
    } else if section {
        codec::unpack(&text[tag.len_utf8()..], &[])?
    } else {
        history.decode_records(seq, &text[tag.len_utf8()..])?
    };
    if let Some(decoding) = decoding {
        decoded(tag, decoding, plain.len());
    }
    drop(text);
    let plain = String::from_utf8(plain).map_err(|error| {
        StoreError::Backend(format!("turn log row {seq} is not UTF-8: {error}"))
    })?;
    Ok(Row {
        boundary,
        bytes,
        plain,
        seq,
        snapshot,
    })
}

fn stored(
    seq: u64,
    mut body: String,
    limit: Option<usize>,
) -> Result<Vec<(u64, String)>, StoreError> {
    let parts = codec::parts(&body, limit)?;
    let first = parts[0].len();
    let rest = parts[1..]
        .iter()
        .zip(seq + 1..)
        .map(|(part, row)| (row, format!("{PART}{part}")))
        .collect::<Vec<_>>();
    if !rest.is_empty() {
        body.truncate(first);
        body.shrink_to_fit();
    }
    Ok(std::iter::once((seq, body)).chain(rest).collect())
}

async fn scan(
    host: &dyn LogHost,
    scope: &str,
    stats: &mut Stats,
    range: (Segment, u64),
    after: u64,
    take: &mut impl FnMut(&Event) -> bool,
) -> Result<bool, StoreError> {
    let (segment, end) = range;
    if segment.last <= after {
        return Ok(true);
    }
    let stored = unsectioned(host.scan(scope, segment.row, end).await?);
    stats.pages += 1;
    stats.page_rows += stored.len() as u64;
    each_row(stored, &mut History::default(), |row| {
        for record in row.records()? {
            if let Record::Event {
                at,
                event,
                seq,
                turn,
            } = record
                && seq > after
                && seq <= segment.last
                && !take(&Event {
                    at,
                    event,
                    seq,
                    turn,
                })
            {
                return Ok(false);
            }
        }
        Ok(true)
    })
}

fn chain_rows(stored: Vec<(u64, String)>) -> Vec<(u64, String)> {
    let mut linked = false;
    stored
        .into_iter()
        .filter(|(_, body)| {
            if !body.starts_with(PART) {
                linked = body.starts_with(LINKED);
            }
            linked
        })
        .collect()
}

fn unsectioned(stored: Vec<(u64, String)>) -> Vec<(u64, String)> {
    let mut section = false;
    stored
        .into_iter()
        .filter(|(_, body)| {
            if !body.starts_with(PART) {
                section = body.starts_with(SECTION) || body.starts_with(LINKED);
            }
            !section
        })
        .collect()
}

fn sealed(record: &Record) -> bool {
    match record {
        Record::Event { .. } | Record::Fact { .. } => true,
        Record::Stored { key, .. } => !key.contains('\n'),
        _ => false,
    }
}

fn suffix(plain: &str, events: usize) -> Option<&str> {
    let floor = plain.len().saturating_sub(WINDOW);
    if floor == 0 {
        return None;
    }
    let (mut start, mut cut, mut found) = (plain.len(), plain.len(), 0);
    while start > floor || found < events {
        let boundary = plain[..cut].rfind('\n')? + 1;
        if let Ok(records) = crate::decode(&plain[boundary..start]) {
            found += records.iter().filter(|record| sealed(record)).count();
            start = boundary;
        }
        cut = boundary - 1;
    }
    Some(&plain[start..])
}

struct Previous {
    bytes: usize,
    parts: usize,
    seq: u64,
    suffix: String,
}

impl Previous {
    fn of(seq: u64, parts: usize, bytes: usize, plain: &str, events: usize) -> Option<Self> {
        suffix(plain, events).map(|suffix| Self {
            bytes,
            parts,
            seq,
            suffix: suffix.to_owned(),
        })
    }

    fn archive(
        &self,
        spare: &mut codec::Spare,
        limit: Option<usize>,
    ) -> Result<Option<Vec<(u64, String)>>, StoreError> {
        let body = codec::pack(spare, ARCHIVE, self.suffix.as_bytes(), &[])?;
        let rows = stored(self.seq, body, limit)?;
        let bytes = rows.iter().map(|(_, body)| body.len()).sum::<usize>();
        Ok((bytes < self.bytes && rows.len() <= self.parts).then_some(rows))
    }
}

struct TailRow {
    bytes: usize,
    seq: u64,
}

const fn snapshot_level(due: bool, bound: bool) -> i32 {
    if due && !bound {
        codec::FAST_LEVEL
    } else {
        codec::DEFAULT_LEVEL
    }
}

pub struct Follower {
    decoded: History,
    first: Option<u64>,
    fold: Fold,
    held: Vec<(u64, String)>,
    history: History,
    host: Arc<dyn LogHost>,
    next: u64,
    previous: Option<Previous>,
    raw: Arc<dyn LogHost>,
    scope: Arc<str>,
    snapshot: usize,
    snapshot_row: Option<u64>,
    state: usize,
    stats: Stats,
    tail: usize,
    tail_plain: usize,
    within: Option<Duration>,
}

impl Follower {
    pub async fn advance(&mut self) -> Result<usize, StoreError> {
        let from = self.held.first().map_or(self.next, |(seq, _)| *seq);
        let stored = self
            .host
            .scan(&self.scope, from, i64::MAX.unsigned_abs())
            .await?;
        let taken = self.take(stored)?;
        let mut chains = Chains::default();
        let forgets = self.fold.take_forgets().into_iter().collect::<Vec<_>>();
        warm(
            &*self.host,
            &self.scope,
            &mut self.fold,
            &mut self.stats,
            &mut chains,
            &forgets,
        )
        .await?;
        Ok(taken)
    }

    pub async fn claim(mut self) -> Result<TurnLog, StoreError> {
        self.advance().await?;
        let log = self.finish().await?;
        log.write(|writing| Box::pin(writing.claimed())).await?;
        Ok(log)
    }

    #[must_use]
    pub const fn next(&self) -> u64 {
        self.next
    }

    fn take(&mut self, stored: Vec<(u64, String)>) -> Result<usize, StoreError> {
        let (Some(&(first, _)), Some(&(last, _))) = (stored.first(), stored.last()) else {
            return Ok(0);
        };
        if self.first.is_none() {
            self.first = Some(first);
            self.fold.start_segment(first);
        }
        self.next = last + 1;
        let mut stored = unsectioned(stored);
        if stored.is_empty() {
            self.held = Vec::new();
            return Ok(0);
        }
        let start = stored
            .iter()
            .rposition(|(_, body)| !body.starts_with(PART))
            .ok_or_else(|| {
                StoreError::Backend(format!("turn log row {} continues no row", stored[0].0))
            })?;
        self.held = stored.split_off(start);
        let taken = stored.len();
        self.fold_rows(stored, None)?;
        Ok(taken)
    }

    fn fold_rows(
        &mut self,
        stored: Vec<(u64, String)>,
        extensible: Option<TailRow>,
    ) -> Result<Option<TailRow>, StoreError> {
        let mut extensible = extensible;
        let mut tail_row = None;
        let last = stored
            .iter()
            .rposition(|(_, body)| body.starts_with(SNAPSHOT))
            .map(|index| {
                let parts = stored[index + 1..]
                    .iter()
                    .take_while(|(_, body)| body.starts_with(PART))
                    .count();
                (stored[index].0, parts + 1)
            });
        each_row(stored, &mut self.decoded, |row| {
            if row.snapshot {
                self.fold = Fold::default();
                self.fold.start_segment(row.seq);
                self.history.clear();
                self.snapshot = row.bytes;
                self.snapshot_row = Some(row.seq);
                self.state = row.plain.len();
                nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
                    histogram.nc.turnlog.opened_snapshot_plain_bytes = u64::try_from(row.plain.len()).unwrap_or(u64::MAX)
                });
                self.tail = 0;
                self.tail_plain = 0;
            } else {
                if row.boundary {
                    let keep = row
                        .records()?
                        .iter()
                        .find_map(|record| match record {
                            Record::Event { seq, .. } => Some(*seq),
                            _ => None,
                        })
                        .unwrap_or(u64::MAX);
                    self.fold.seal(row.seq, keep);
                    self.history.clear();
                }
                self.tail += row.bytes;
                self.tail_plain += row.plain.len();
            }
            if extensible
                .as_ref()
                .is_some_and(|candidate| candidate.seq == row.seq)
                && !row
                    .records()?
                    .iter()
                    .any(|record| matches!(record, Record::Recovery { .. }))
            {
                tail_row = extensible.take();
            }
            self.fold.load(row.seq, &row.plain)?;
            if let Some((seq, parts)) = last
                && row.seq == seq
            {
                self.previous = Previous::of(
                    seq,
                    parts,
                    row.bytes,
                    &row.plain,
                    self.fold.events().len()
                        + self.fold.facts().count()
                        + self.fold.unowned_stored(),
                );
            }
            self.fold.release();
            self.history.extend(row.plain.as_bytes());
            Ok(true)
        })?;
        Ok(tail_row)
    }

    async fn finish(mut self) -> Result<TurnLog, StoreError> {
        let held = std::mem::take(&mut self.held);
        let extensible =
            held.last()
                .filter(|(_, body)| codec::extensible(body))
                .map(|(seq, body)| TailRow {
                    bytes: body.len(),
                    seq: *seq,
                });
        let tail_row = self.fold_rows(held, extensible)?;
        let next = self.next;
        let tail_rows = usize::try_from(match (self.snapshot_row, self.first) {
            (Some(row), _) => next - 1 - row,
            (None, first) => first.map_or(0, |first| next - first),
        })
        .map_err(|error| StoreError::Backend(format!("turn log tail row count: {error}")))?;
        let Self {
            mut fold,
            history,
            host,
            previous,
            raw,
            scope,
            snapshot,
            state,
            mut stats,
            tail,
            tail_plain,
            within,
            ..
        } = self;
        let mut chains = Chains::default();
        let forgetting = fold.forgetting();
        warm(
            &*host,
            &scope,
            &mut fold,
            &mut stats,
            &mut chains,
            &forgetting,
        )
        .await?;
        fold.reopen(EVENT_GAP);
        let floor = Some(fold.last_event());
        let (riding, riders) = mpsc::unbounded_channel();
        let writing = Writing {
            host: Arc::clone(&host),
            limit: None,
            max_tail: None,
            chain_bytes: None,
            ratio: SNAPSHOT_RATIO,
            retain: None,
            rows: None,
            scope: Arc::clone(&scope),
            whole: false,
            writer: Writer {
                bases: BTreeMap::new(),
                buffer: Vec::new(),
                chains: Chains::default(),
                check: snapshot,
                collectable: BTreeSet::new(),
                fenced: false,
                floor,
                fold,
                history,
                next,
                previous,
                riders,
                snapshot,
                state,
                stats,
                tail,
                tail_plain,
                tail_row,
                tail_rows,
                tails: BTreeMap::new(),
                tried: None,
                unsettled: None,
            },
        };
        let (letters, received) = mpsc::unbounded_channel();
        let serving: StoreFuture<'static, ()> = Box::pin(serve(writing, received));
        #[cfg(target_family = "wasm")]
        nanocode_platform::spawn_local(serving);
        Ok(TurnLog {
            letters,
            raw,
            riding,
            scope,
            #[cfg(not(target_family = "wasm"))]
            serving: serving.shared(),
            within,
        })
    }
}

impl Writer {
    fn live(&self) -> Result<(), StoreError> {
        if self.fenced && !crate::sim::mutated(crate::sim::Mutation::FencedServesReads) {
            return Err(StoreError::Fenced);
        }
        Ok(())
    }
}

struct Riding {
    host: usize,
    statements: Vec<(&'static str, Vec<String>)>,
    rode: oneshot::Sender<bool>,
}

struct Writer {
    bases: BTreeMap<String, Option<Checkpoint>>,
    buffer: Vec<Record>,
    chains: Chains,
    check: usize,
    collectable: BTreeSet<String>,
    fenced: bool,
    floor: Option<u64>,
    fold: Fold,
    history: History,
    next: u64,
    previous: Option<Previous>,
    riders: mpsc::UnboundedReceiver<Riding>,
    snapshot: usize,
    state: usize,
    stats: Stats,
    tail: usize,
    tail_plain: usize,
    tail_row: Option<TailRow>,
    tail_rows: usize,
    tails: BTreeMap<String, u64>,
    tried: Option<(usize, usize)>,
    unsettled: Option<Unsettled>,
}

type Job = Box<dyn for<'a> FnOnce(&'a mut Writing) -> StoreFuture<'a, ()> + Send>;

type Replied<R> = std::thread::Result<R>;

async fn serve(mut writing: Writing, mut letters: mpsc::UnboundedReceiver<Job>) {
    while let Some(job) = letters.recv().await {
        job(&mut writing).await;
    }
}

pub struct TurnLog {
    letters: mpsc::UnboundedSender<Job>,
    raw: Arc<dyn LogHost>,
    riding: mpsc::UnboundedSender<Riding>,
    scope: Arc<str>,
    #[cfg(not(target_family = "wasm"))]
    serving: Shared<StoreFuture<'static, ()>>,
    within: Option<Duration>,
}

pub struct Writing {
    host: Arc<dyn LogHost>,
    limit: Option<usize>,
    max_tail: Option<usize>,
    chain_bytes: Option<usize>,
    ratio: usize,
    retain: Option<usize>,
    rows: Option<usize>,
    scope: Arc<str>,
    whole: bool,
    writer: Writer,
}

impl TurnLog {
    pub async fn open(
        host: Arc<dyn LogHost>,
        scope: impl Into<Arc<str>>,
    ) -> Result<Self, StoreError> {
        Self::follow(host, scope).await?.finish().await
    }

    pub async fn follow(
        host: Arc<dyn LogHost>,
        scope: impl Into<Arc<str>>,
    ) -> Result<Follower, StoreError> {
        let scope = scope.into();
        let within = crate::bound::own(&*host);
        let raw = host;
        let host = crate::bound::bounded(&raw, within);
        let stored = host.load(&scope).await?;
        let mut follower = Follower {
            decoded: History::default(),
            first: None,
            fold: Fold::default(),
            held: Vec::new(),
            history: History::default(),
            host,
            next: FIRST_ROW,
            previous: None,
            raw,
            scope,
            snapshot: 0,
            snapshot_row: None,
            state: 0,
            stats: Stats::default(),
            tail: 0,
            tail_plain: 0,
            within,
        };
        follower.take(stored)?;
        Ok(follower)
    }

    fn configure(&self, change: impl FnOnce(&mut Writing) + Send + 'static) {
        let job: Job = Box::new(move |writing| {
            change(writing);
            Box::pin(std::future::ready(()))
        });
        assert!(
            self.letters.send(job).is_ok(),
            "a turn log's writer takes letters while the log lives"
        );
    }

    #[must_use]
    pub fn snapshot_ratio(self, ratio: usize) -> Self {
        self.configure(move |writing| writing.ratio = ratio);
        self
    }

    #[must_use]
    pub fn retain_finished(self, keep: Option<usize>) -> Self {
        self.configure(move |writing| writing.retain = keep);
        self
    }

    #[must_use]
    pub fn snapshot_rows(self, rows: Option<usize>) -> Self {
        self.configure(move |writing| writing.rows = rows);
        self
    }

    #[must_use]
    pub fn max_tail_bytes(self, max: Option<usize>) -> Self {
        self.configure(move |writing| writing.max_tail = max);
        self
    }

    #[must_use]
    pub fn chain_bytes_max(self, max: Option<usize>) -> Self {
        assert!(
            max.is_none_or(|max| max >= codec::WINDOW),
            "a linked chain restarts no sooner than its window"
        );
        self.configure(move |writing| writing.chain_bytes = max);
        self
    }

    #[must_use]
    pub fn row_limit(self, limit: Option<usize>) -> Self {
        self.configure(move |writing| writing.limit = limit);
        self
    }

    #[must_use]
    pub fn host_calls_within(mut self, within: Option<Duration>) -> Self {
        let within = within.or_else(|| crate::bound::own(&*self.raw));
        let host = crate::bound::bounded(&self.raw, within);
        self.configure(move |writing| writing.host = host);
        self.within = within;
        self
    }

    #[must_use]
    pub fn whole_checkpoints(self, whole: bool) -> Self {
        self.configure(move |writing| writing.whole = whole);
        self
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn ride(
        &self,
        host: usize,
        statements: Vec<(&'static str, Vec<String>)>,
    ) -> oneshot::Receiver<bool> {
        let (rode, receiver) = oneshot::channel();
        if let Err(error) = self.riding.send(Riding {
            host,
            statements,
            rode,
        }) {
            let _ = error.0.rode.send(false);
        }
        receiver
    }

    #[must_use]
    pub const fn host_call_bound(&self) -> Option<Duration> {
        self.within
    }

    pub async fn answered_within<T>(
        &self,
        call: &'static str,
        answer: impl std::future::Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        match self.within {
            Some(_) if crate::sim::mutated(crate::sim::Mutation::AnswersUnbounded) => answer.await,
            Some(within) => crate::bound::within(within, call, &self.scope, answer).await,
            None => answer.await,
        }
    }

    #[track_caller]
    pub fn write<R, J>(&self, job: J) -> impl Future<Output = R>
    where
        R: Send + 'static,
        J: for<'a> FnOnce(&'a mut Writing) -> StoreFuture<'a, R> + Send + 'static,
    {
        let (_, replied) = self.post(job);
        async move { answered(self.served(replied).await) }
    }

    #[track_caller]
    pub fn write_within<R, E, J>(&self, job: J) -> impl Future<Output = Result<R, E>>
    where
        R: Send + 'static,
        E: From<StoreError> + Send + 'static,
        J: for<'a> FnOnce(&'a mut Writing) -> StoreFuture<'a, Result<R, E>> + Send + 'static,
    {
        let bounded = self.bounded(false, job);
        async move { bounded.await? }
    }

    #[track_caller]
    fn post<R, J>(&self, job: J) -> (oneshot::Receiver<()>, oneshot::Receiver<Replied<R>>)
    where
        R: Send + 'static,
        J: for<'a> FnOnce(&'a mut Writing) -> StoreFuture<'a, R> + Send + 'static,
    {
        let (started, start) = oneshot::channel();
        let (mut reply, replied) = oneshot::channel();
        let caller = nanocode_trace::Span::current();
        let site = std::panic::Location::caller();
        let queued = timed().then(nanocode_platform::now);
        let letter: Job = Box::new(move |writing| {
            Box::pin(async move {
                if reply.is_closed() {
                    return;
                }
                let taken = queued.map(|queued| (queued, nanocode_platform::now()));
                let span = match taken {
                    Some((queued, taken)) => {
                        let waited = taken.saturating_duration_since(queued);
                        let job = job_site(site);
                        let span = nanocode_trace::info_span!(
                            target: "nanocode",
                            parent: caller.id(),
                            "turnlog.writer.job",
                            job = job.as_str(),
                            queue.ms = waited.as_secs_f64() * 1_000.0,
                        );
                        span.in_scope(|| {
                            nanocode_trace::event!(
                                target: "nc_metrics",
                                nanocode_trace::Level::INFO,
                                {
                                    histogram.nc.turnlog.writer_queue_micros =
                                        u64::try_from(waited.as_micros()).unwrap_or(u64::MAX)
                                }
                            );
                        });
                        span
                    }
                    None => nanocode_trace::Span::none(),
                };
                let _ = started.send(());
                writing.writer.chains.clear();
                let work = std::panic::AssertUnwindSafe(async move { job(writing).await });
                let answer = if crate::sim::mutated(crate::sim::Mutation::AbandonRunningJob) {
                    let abandoned = Box::pin(reply.closed());
                    match select(
                        abandoned,
                        Box::pin(work.catch_unwind().instrument(span.clone())),
                    )
                    .await
                    {
                        Either::Left(_) => None,
                        Either::Right((answer, _)) => Some(answer),
                    }
                } else {
                    Some(work.catch_unwind().instrument(span.clone()).await)
                };
                if let Some((_, taken)) = taken {
                    let ran = nanocode_platform::now().saturating_duration_since(taken);
                    span.in_scope(|| {
                        nanocode_trace::event!(
                            target: "nc_metrics",
                            nanocode_trace::Level::INFO,
                            {
                                histogram.nc.turnlog.writer_job_micros =
                                    u64::try_from(ran.as_micros()).unwrap_or(u64::MAX)
                            }
                        );
                    });
                }
                if let Some(answer) = answer {
                    drop(reply.send(answer));
                }
            })
        });
        assert!(
            self.letters.send(letter).is_ok(),
            "a turn log's writer takes letters while the log lives"
        );
        (start, replied)
    }

    #[cfg(target_family = "wasm")]
    async fn served<T>(&self, receiver: oneshot::Receiver<T>) -> T {
        receiver
            .await
            .unwrap_or_else(|_| panic!("a turn log's writer answers every job it takes"))
    }

    #[cfg(not(target_family = "wasm"))]
    async fn served<T>(&self, mut receiver: oneshot::Receiver<T>) -> T {
        let mut serving = self.serving.clone();
        let answer = std::future::poll_fn(|context| {
            if let std::task::Poll::Ready(answer) = std::pin::Pin::new(&mut receiver).poll(context)
            {
                return std::task::Poll::Ready(answer);
            }
            assert!(
                std::pin::Pin::new(&mut serving).poll(context).is_pending(),
                "a turn log's writer serves while the log lives"
            );
            std::pin::Pin::new(&mut receiver).poll(context)
        })
        .await;
        answer.unwrap_or_else(|_| panic!("a turn log's writer answers every job it takes"))
    }

    #[track_caller]
    fn bounded<R, J>(&self, reading: bool, job: J) -> impl Future<Output = Result<R, StoreError>>
    where
        R: Send + 'static,
        J: for<'a> FnOnce(&'a mut Writing) -> StoreFuture<'a, R> + Send + 'static,
    {
        let (start, replied) = self.post(job);
        async move {
            if let Some(within) = self.within {
                let unbounded = crate::sim::mutated(crate::sim::Mutation::HostUnbounded)
                    || (reading && crate::sim::mutated(crate::sim::Mutation::ReadsUnbounded));
                if !unbounded {
                    nanocode_platform::timeout(within, self.served(start))
                        .await
                        .map_err(|_| {
                            StoreError::Backend(format!(
                                "turn log writer of {} stayed held past {} ms",
                                self.scope,
                                within.as_millis()
                            ))
                        })?;
                }
            }
            Ok(answered(self.served(replied).await))
        }
    }

    pub async fn read<R>(&self, read: impl FnOnce(&Fold) -> R + Send + 'static) -> R
    where
        R: Send + 'static,
    {
        self.write(move |writing| Box::pin(std::future::ready(read(&writing.writer.fold))))
            .await
    }

    pub async fn load_owner(&self, owner: &str) -> Result<(), StoreError> {
        let owner = owner.to_owned();
        self.bounded(true, move |writing| {
            Box::pin(async move { writing.load_owner(&owner).await })
        })
        .await?
    }

    pub async fn load_index(&self) -> Result<(), StoreError> {
        self.bounded(true, |writing| Box::pin(writing.load_index()))
            .await?
    }

    pub async fn load_all(&self) -> Result<u64, StoreError> {
        self.bounded(true, |writing| Box::pin(writing.load_all()))
            .await?
    }

    pub async fn read_owners<R>(
        &self,
        owners: &[String],
        read: impl FnOnce(&Fold) -> R + Send + 'static,
    ) -> Result<R, StoreError>
    where
        R: Send + 'static,
    {
        let owners = owners.to_vec();
        self.bounded(true, move |writing| {
            Box::pin(async move {
                writing.warm_owners(&owners).await?;
                Ok(read(&writing.writer.fold))
            })
        })
        .await?
    }

    pub async fn read_owner<R>(
        &self,
        owner: &str,
        read: impl FnOnce(&Fold) -> R + Send + 'static,
    ) -> Result<R, StoreError>
    where
        R: Send + 'static,
    {
        let owner = owner.to_owned();
        self.bounded(true, move |writing| {
            Box::pin(async move {
                writing.load_owner(&owner).await?;
                Ok(read(&writing.writer.fold))
            })
        })
        .await?
    }

    pub async fn reclaim(&self) -> Result<(), StoreError> {
        self.bounded(false, |writing| Box::pin(writing.claimed()))
            .await?
    }

    pub async fn stats(&self) -> Stats {
        self.write(|writing| Box::pin(std::future::ready(writing.writer.stats)))
            .await
    }

    pub async fn history<S>(
        &self,
        after: u64,
        kept: S,
        mut take: impl FnMut(&mut S, &Event) -> bool + Send + 'static,
    ) -> Result<S, StoreError>
    where
        S: Send + 'static,
    {
        self.bounded(true, move |writing| {
            Box::pin(async move {
                let mut kept = kept;
                writing
                    .history(after, |event| take(&mut kept, event))
                    .await?;
                Ok(kept)
            })
        })
        .await?
    }

    pub async fn blobs(
        &self,
        digests: &[Digest],
    ) -> Result<BTreeMap<Digest, Box<RawValue>>, StoreError> {
        let digests = digests.to_vec();
        self.bounded(true, move |writing| {
            Box::pin(async move {
                writing.writer.live()?;
                writing.blobs(&digests).await
            })
        })
        .await?
    }

    pub async fn cached_state(&self, state: &str) -> Result<Option<FoldSnapshot>, StoreError> {
        let wanted = state.to_owned();
        let at = match self.read(move |fold| fold.cached_state(&wanted)).await? {
            None => return Ok(None),
            Some(CachedFold::Inline(snapshot)) => return Ok(Some(snapshot)),
            Some(CachedFold::At(at)) => at,
        };
        let blobs = self.blobs(&[at.blob]).await?;
        let json = blobs
            .get(&at.blob)
            .ok_or(FoldError::MissingBlob { digest: at.blob })?;
        serde_json::from_str(json.get()).map(Some).map_err(|error| {
            StoreError::Backend(format!("cached fold of {state} does not decode: {error}"))
        })
    }

    pub async fn facts_from<S>(
        &self,
        after: u64,
        kept: S,
        mut take: impl FnMut(&mut S, u64, Option<&str>, &StoredFact) + Send + 'static,
    ) -> Result<S, StoreError>
    where
        S: Send + 'static,
    {
        self.bounded(true, move |writing| {
            Box::pin(async move {
                let mut kept = kept;
                writing.writer.live()?;
                writing
                    .facts_from(after, |seq, state, fact| take(&mut kept, seq, state, fact))
                    .await?;
                Ok(kept)
            })
        })
        .await?
    }
}

fn answered<R>(answer: Replied<R>) -> R {
    match answer {
        Ok(output) => output,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

impl Writing {
    async fn load_index(&mut self) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        writer.live()?;
        root(
            &*self.host,
            &self.scope,
            &mut writer.fold,
            &mut writer.stats,
            &mut writer.chains,
        )
        .await?;
        let pages = writer.fold.cold_pages();
        index_pages(
            &*self.host,
            &self.scope,
            &mut writer.fold,
            &mut writer.stats,
            &mut writer.chains,
            pages,
        )
        .await?;
        writer.fold.unshelve_all();
        Ok(())
    }

    async fn load_all(&mut self) -> Result<u64, StoreError> {
        self.load_index().await?;
        let mut cold = self.writer.fold.cold_owners();
        while !cold.is_empty() {
            self.warm_owners(&cold).await?;
            let left = self.writer.fold.cold_owners();
            if left == cold {
                return Err(StoreError::Backend(format!(
                    "turn log owners of {} stay cold after their sections load: {}",
                    self.scope,
                    left.join(", ")
                )));
            }
            cold = left;
        }
        Ok(self.writer.next)
    }

    async fn warm_owners(&mut self, owners: &[String]) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        writer.live()?;
        warm(
            &*self.host,
            &self.scope,
            &mut writer.fold,
            &mut writer.stats,
            &mut writer.chains,
            owners,
        )
        .await
    }

    async fn history(
        &mut self,
        after: u64,
        mut take: impl FnMut(&Event) -> bool,
    ) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        writer.live()?;
        let unsealed = writer.fold.unsealed();
        let end = match unsealed {
            Some(_) => writer.next,
            None => writer.fold.segment_row(),
        };
        let mut segments = writer.fold.segments().to_vec();
        segments.extend(unsealed);
        let ends = segments
            .iter()
            .skip(1)
            .map(|segment| segment.row)
            .chain([end])
            .collect::<Vec<_>>();
        for (segment, end) in segments.into_iter().zip(ends) {
            if !scan(
                &*self.host,
                &self.scope,
                &mut writer.stats,
                (segment, end),
                after,
                &mut take,
            )
            .await?
            {
                return Ok(());
            }
        }
        for event in writer.fold.events() {
            if event.seq > after && !take(event) {
                return Ok(());
            }
        }
        Ok(())
    }
}

async fn blobs(
    host: &dyn LogHost,
    scope: &str,
    (fold, stats): (&Fold, &mut Stats),
    digests: &[Digest],
) -> Result<BTreeMap<Digest, Box<RawValue>>, StoreError> {
    let mut found = BTreeMap::new();
    let mut sealed: BTreeMap<u64, (Segment, Vec<Digest>)> = BTreeMap::new();
    for digest in digests {
        if fold.blob_resident(digest) {
            found.insert(*digest, fold.blob(digest)?.to_owned());
            continue;
        }
        let segment = fold
            .sealed_blob(digest)
            .ok_or(FoldError::MissingBlob { digest: *digest })?;
        sealed
            .entry(segment.row)
            .or_insert_with(|| (segment, Vec::new()))
            .1
            .push(*digest);
    }
    let (mut scanned, mut rows, mut plain) = (0_u64, 0_u64, 0_u64);
    let looked = digests.len() as u64;
    for (segment, wanted) in sealed.into_values() {
        let end = fold
            .fact_segments()
            .iter()
            .find(|later| later.row > segment.row)
            .map_or(fold.fact_row(), |later| later.row);
        let stored = unsectioned(host.scan(scope, segment.row, end).await?);
        stats.pages += 1;
        stats.page_rows += stored.len() as u64;
        scanned += 1;
        rows += stored.len() as u64;
        let keys = wanted
            .iter()
            .map(|digest| (digest.key().as_str().to_owned(), *digest))
            .collect::<BTreeMap<_, _>>();
        each_row(stored, &mut History::default(), |row| {
            plain += row.plain.len() as u64;
            for record in row.records()? {
                if let Record::Stored { key, value } = record
                    && let Some(digest) = keys.get(&key)
                    && Digest::of(value.as_bytes()) == *digest
                {
                    let json = serde_json::from_str::<Box<RawValue>>(&value)
                        .map_err(|_| FoldError::CorruptBlob { digest: *digest })?;
                    found.insert(*digest, json);
                }
            }
            Ok(true)
        })?;
        if let Some(missing) = wanted.iter().find(|digest| !found.contains_key(*digest)) {
            return Err(FoldError::MissingBlob { digest: *missing }.into());
        }
    }
    nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
        monotonic_counter.nc.turnlog.blob_lookups = 1_u64,
        monotonic_counter.nc.turnlog.blobs_wanted = looked,
        monotonic_counter.nc.turnlog.blob_segments_scanned = scanned,
        monotonic_counter.nc.turnlog.blob_rows_scanned = rows,
        histogram.nc.turnlog.blob_scan_plain_bytes = plain
    });
    Ok(found)
}

async fn facts_from(
    host: &dyn LogHost,
    scope: &str,
    (fold, stats): (&Fold, &mut Stats),
    after: u64,
    mut take: impl FnMut(u64, Option<&str>, &StoredFact),
) -> Result<(), StoreError> {
    let segments = fold.fact_segments();
    let ends = segments
        .iter()
        .skip(1)
        .map(|segment| segment.row)
        .chain([fold.fact_row()]);
    let (mut scanned, mut rows, mut plain, mut taken) = (0_u64, 0_u64, 0_u64, 0_u64);
    let mut skipped = 0_u64;
    for (segment, end) in segments.iter().zip(ends) {
        if segment.last <= after {
            continue;
        }
        let stored = unsectioned(host.scan(scope, segment.row, end).await?);
        stats.pages += 1;
        stats.page_rows += stored.len() as u64;
        scanned += 1;
        rows += stored.len() as u64;
        skipped += after
            .saturating_sub(segment.first.saturating_sub(1))
            .min(segment.last - segment.first + 1);
        let mut next = segment.first.max(after + 1);
        let before = next;
        each_row(stored, &mut History::default(), |row| {
            plain += row.plain.len() as u64;
            for record in row.records()? {
                if let Record::Fact { fact, seq, state } = record
                    && seq == next
                    && seq <= segment.last
                {
                    take(seq, state.as_deref(), &fact);
                    next += 1;
                }
            }
            Ok(true)
        })?;
        taken += next - before;
        if next <= segment.last {
            return Err(StoreError::Backend(format!(
                "turn log rows {}..{end} of {scope} hold sealed facts up to {} of {}..={}",
                segment.row,
                next - 1,
                segment.first,
                segment.last
            )));
        }
    }
    nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
        monotonic_counter.nc.turnlog.facts_scans = 1_u64,
        monotonic_counter.nc.turnlog.facts_segments_scanned = scanned,
        monotonic_counter.nc.turnlog.facts_rows_scanned = rows,
        monotonic_counter.nc.turnlog.facts_taken = taken,
        monotonic_counter.nc.turnlog.facts_skipped = skipped,
        monotonic_counter.nc.turnlog.facts_segments_total = segments.len() as u64,
        histogram.nc.turnlog.facts_scan_plain_bytes = plain
    });
    for (seq, state, fact) in fold.resident_facts(after) {
        take(seq, state, fact);
    }
    Ok(())
}

impl Writing {
    pub async fn blobs(
        &mut self,
        digests: &[Digest],
    ) -> Result<BTreeMap<Digest, Box<RawValue>>, StoreError> {
        let writer = &mut self.writer;
        blobs(
            &*self.host,
            &self.scope,
            (&writer.fold, &mut writer.stats),
            digests,
        )
        .await
    }

    pub async fn facts_from(
        &mut self,
        after: u64,
        take: impl FnMut(u64, Option<&str>, &StoredFact),
    ) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        facts_from(
            &*self.host,
            &self.scope,
            (&writer.fold, &mut writer.stats),
            after,
            take,
        )
        .await
    }

    pub fn cache_state(
        &mut self,
        state: &str,
        snapshot: &FoldSnapshot,
    ) -> Result<Vec<(u64, StoredFact)>, StoreError> {
        assert!(
            snapshot.seq < self.writer.fold.next_fact(),
            "a cached fold names a fact the log holds"
        );
        let encode = |error: serde_json::Error| {
            StoreError::NotCommitted(format!("encode the cached fold of {state}: {error}"))
        };
        let value = serde_json::value::to_raw_value(snapshot).map_err(encode)?;
        if crate::sim::mutated(crate::sim::Mutation::CacheInline) {
            self.push(Record::Host {
                key: crate::fold::cache_key(state),
                value,
            })?;
            return Ok(Vec::new());
        }
        let blob = Digest::of(value.get().as_bytes());
        let bytes = value.get().len() as u64;
        let mut appended = Vec::new();
        if self.writer.fold.values().blob(&blob).is_none() {
            self.put_blob(&blob, &value)?;
            let marker: StoredFact = Fact::Marker(Marker::Blob {
                digest: blob,
                bytes,
            });
            appended.push((self.append(marker.clone())?, marker));
        }
        let at = serde_json::value::to_raw_value(&CachedAt { blob, bytes }).map_err(encode)?;
        self.push(Record::Host {
            key: crate::fold::cache_key(state),
            value: at,
        })?;
        Ok(appended)
    }

    pub fn cache(&mut self, snapshot: &FoldSnapshot) -> Result<(), StoreError> {
        assert!(
            snapshot.seq < self.writer.fold.next_fact(),
            "a cached fold names a fact the log holds"
        );
        let value = serde_json::value::to_raw_value(snapshot)
            .map_err(|error| StoreError::NotCommitted(format!("encode a cached fold: {error}")))?;
        self.push(Record::Host {
            key: FOLD_SNAPSHOT_KEY.to_owned(),
            value,
        })
    }
}

impl Writing {
    pub fn append_owned<D: serde::Serialize>(
        &mut self,
        state: Option<&str>,
        fact: &OwnedFact<D>,
    ) -> Result<u64, FoldError> {
        let mut fresh: Vec<(Digest, &RawValue)> = Vec::new();
        let mut sealed: Vec<(Digest, &RawValue)> = Vec::new();
        let named: Result<_, FoldError> = fact.try_map(
            |json| {
                if json.get().len() >= VALUE_INLINE_BYTES {
                    let digest = Digest::of(json.get().as_bytes());
                    let fold = &self.writer.fold;
                    let interned = fold.values().blob(&digest).is_some();
                    let known = interned || fresh.iter().any(|(held, _)| *held == digest);
                    if !known {
                        fresh.push((digest, &**json));
                    } else if interned && state.is_none() && !fold.blob_resident(&digest) {
                        sealed.push((digest, &**json));
                    }
                }
                Ok(())
            },
            |_| Ok(()),
        );
        named?;
        for (digest, json) in sealed {
            self.put_blob(&digest, json)?;
        }
        for (digest, json) in fresh {
            self.put_blob(&digest, json)?;
            let bytes = json.get().len() as u64;
            self.append(Fact::Marker(Marker::Blob { digest, bytes }))?;
        }
        let values = self.writer.fold.values();
        let stored: StoredFact = fact.try_map(
            |json| {
                if json.get().len() < VALUE_INLINE_BYTES {
                    return Ok(Value::Inline(json.clone()));
                }
                let digest = Digest::of(json.get().as_bytes());
                let blob = values
                    .blob(&digest)
                    .expect("a value past the inline bound was interned before its fact");
                Ok(Value::Blob(blob))
            },
            |decision| {
                serde_json::value::to_raw_value(decision)
                    .map_err(|error| FoldError::Encode(error.to_string()))
            },
        )?;
        self.append_in(state, stored)
    }

    pub fn append_in(&mut self, state: Option<&str>, fact: StoredFact) -> Result<u64, FoldError> {
        let seq = self.writer.fold.next_fact();
        self.push(Record::Fact {
            fact,
            seq,
            state: state.map(str::to_owned),
        })
        .map_err(FoldError::Sink)?;
        assert_eq!(
            self.writer.fold.next_fact(),
            seq + 1,
            "an appended fact folds at once"
        );
        Ok(seq)
    }
}

impl FactSink for Writing {
    fn next(&self) -> u64 {
        self.writer.fold.next_fact()
    }

    fn put_blob(&mut self, digest: &Digest, json: &RawValue) -> Result<(), FoldError> {
        assert_eq!(
            Digest::of(json.get().as_bytes()),
            *digest,
            "a blob is put under the digest of its value"
        );
        let key = digest.key().as_str().to_owned();
        if self.writer.fold.contains(&key) {
            return Ok(());
        }
        self.push(Record::Stored {
            key,
            value: json.get().to_owned(),
        })
        .map_err(FoldError::Sink)
    }

    fn append(&mut self, fact: StoredFact) -> Result<u64, FoldError> {
        self.append_in(None, fact)
    }
}

impl Writing {
    pub async fn held(
        &mut self,
        after: u64,
        mut take: impl FnMut(&Event) -> bool,
    ) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        writer.live()?;
        if let Some(segment) = writer.fold.unsealed()
            && !scan(
                &*self.host,
                &self.scope,
                &mut writer.stats,
                (segment, writer.next),
                after,
                &mut take,
            )
            .await?
        {
            return Ok(());
        }
        for event in writer.fold.events() {
            if event.seq > after && !take(event) {
                return Ok(());
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn fold(&self) -> &Fold {
        &self.writer.fold
    }

    pub async fn load_owners(&mut self, owners: &[String]) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        writer.live()?;
        warm(
            &*self.host,
            &self.scope,
            &mut writer.fold,
            &mut writer.stats,
            &mut writer.chains,
            owners,
        )
        .await?;
        for owner in owners {
            writer.fold.read_owner(owner);
        }
        Ok(())
    }

    pub async fn index_owners(&mut self, owners: &[String]) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        writer.live()?;
        locate(
            &*self.host,
            &self.scope,
            &mut writer.fold,
            &mut writer.stats,
            &mut writer.chains,
            owners,
        )
        .await
    }

    pub async fn load_owner(&mut self, owner: &str) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        writer.live()?;
        warm_state(
            &*self.host,
            &self.scope,
            &mut writer.fold,
            &mut writer.stats,
            &mut writer.chains,
            owner,
        )
        .await
    }

    #[must_use]
    pub fn buffered(&self) -> usize {
        self.writer.buffer.len()
    }

    pub fn unride(&mut self) {
        while let Ok(riding) = self.writer.riders.try_recv() {
            let _ = riding.rode.send(false);
        }
    }

    pub fn next_event(&mut self) -> u64 {
        self.writer.fold.next_event()
    }

    pub fn retire_finished(&mut self) -> usize {
        let Some(keep) = self.retain else {
            return 0;
        };
        let retired = self.writer.fold.retire(keep);
        self.writer.stats.retired += retired as u64;
        retired
    }

    pub fn collect_unreachable(&mut self, state: &str) {
        if !self.writer.collectable.contains(state) {
            self.writer.collectable.insert(state.to_owned());
        }
    }

    pub fn push(&mut self, record: Record) -> Result<(), StoreError> {
        let writer = &mut self.writer;
        if let Record::Checkpoint { state, .. } = &record
            && !self.whole
            && !writer.bases.contains_key(state)
        {
            let base = writer
                .fold
                .checkpoint(state)
                .filter(|held| writer.tails.get(state) == Some(&held.revision))
                .cloned();
            writer.bases.insert(state.clone(), base);
        }
        writer.fold.apply(&record, None)?;
        if let Record::Checkpoint { state, .. } = &record {
            writer.buffer.retain(
                |buffered| !matches!(buffered, Record::Checkpoint { state: other, .. } if other == state),
            );
        }
        writer.buffer.push(record);
        Ok(())
    }

    pub fn unadmit(&mut self, turn: &str, seq: u64) {
        let writer = &mut self.writer;
        let withdrawn = writer.buffer.iter().find_map(|record| match record {
            Record::Fact {
                fact: Fact::Input(input),
                seq,
                ..
            } if input.turn == turn && matches!(input.kind, InputKind::Prompt) => Some(*seq),
            _ => None,
        });
        if let Some(from) = withdrawn {
            writer.buffer.retain(
                |record| !matches!(record, Record::Fact { seq: fact, .. } if *fact >= from),
            );
            writer.fold.withdraw_facts(from);
        }
        writer.buffer.retain(|record| match record {
            Record::Admitted { turn: admitted, .. } => admitted != turn,
            Record::Event { seq: event, .. } => *event != seq,
            _ => true,
        });
        writer.fold.unadmit(turn, seq);
    }

    pub fn unwake(&mut self, key: &str, held: Option<crate::ArmedWake>) {
        let writer = &mut self.writer;
        writer.buffer.retain(|record| match record {
            Record::Armed { key: buffered, .. }
            | Record::Disarmed { key: buffered, .. }
            | Record::Fired { key: buffered, .. }
            | Record::Heard { key: buffered, .. } => buffered != key,
            _ => true,
        });
        writer.fold.restore_wake(key, held);
    }

    #[must_use]
    pub fn floor_pending(&self) -> bool {
        self.writer.floor.is_some()
    }

    async fn claimed(&mut self) -> Result<(), StoreError> {
        self.writer.live()?;
        let next = self.writer.next;
        self.push(Record::Host {
            key: CLAIM.to_owned(),
            value: serde_json::value::to_raw_value(&next)
                .map_err(|error| StoreError::Backend(format!("turn log claim: {error}")))?,
        })?;
        let check = std::mem::replace(&mut self.writer.check, usize::MAX);
        let claimed = self.commit(Point::Recovery).await;
        self.writer.check = check;
        claimed.map(drop)
    }

    pub async fn secure(&mut self) -> Result<(), StoreError> {
        if self.writer.floor.is_some() {
            self.commit(Point::Floor).await?;
        }
        Ok(())
    }

    pub async fn commit(&mut self, point: Point) -> Result<u64, StoreError> {
        self.committed(point, None).await.map(|(seq, _)| seq)
    }

    pub async fn commit_carrying(
        &mut self,
        point: Point,
        rider: Rider<'_>,
    ) -> Result<(u64, bool), StoreError> {
        self.committed(point, Some(rider)).await
    }

    async fn committed(
        &mut self,
        point: Point,
        rider: Option<Rider<'_>>,
    ) -> Result<(u64, bool), StoreError> {
        let writer = &mut self.writer;
        writer.live()?;
        let revived = writer.fold.revive(&writer.buffer);
        writer.buffer.extend(revived);
        let seq = writer.next;
        let previous = writer.fold.seq();
        let recovered = writer
            .buffer
            .iter()
            .filter_map(|record| match record {
                Record::Recovery { turn, .. } => Some(turn.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let floor = writer.floor.map(|seq| Record::Floor { seq });
        let unpaged = writer
            .buffer
            .iter()
            .filter_map(|record| match record {
                Record::Stored { key, .. } if writer.fold.waits(key) => Some(key.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        pages(
            &*self.host,
            &self.scope,
            &mut writer.fold,
            &mut writer.stats,
            &mut writer.chains,
            &unpaged,
        )
        .await?;
        let mut spare = codec::Spare::default();
        let mut placing = placing(
            &writer.fold,
            &writer.history,
            &writer.buffer,
            seq,
            self.limit,
            self.chain_bytes,
        )?;
        let placed = placing.as_ref().map(|placing| Record::Placed {
            chain: Some(placing.keyframe),
            owners: placing.owners.clone(),
            row: placing.section.row,
            rows: placing.section.rows,
        });
        let plain = {
            let mut written = Vec::with_capacity(writer.buffer.len());
            for record in &writer.buffer {
                if let Record::Stored { key, .. } = record
                    && placing
                        .as_ref()
                        .is_some_and(|placing| placing.keys.contains(key))
                    && !crate::sim::mutated(crate::sim::Mutation::PlacedInline)
                {
                    continue;
                }
                written.push(match record {
                    Record::Checkpoint {
                        payload,
                        revision,
                        state,
                    } => match writer.bases.get(state) {
                        Some(Some(base)) => crate::patch::diff(&base.payload, payload)?.map_or(
                            Cow::Borrowed(record),
                            |edits| {
                                Cow::Owned(Record::Patched {
                                    base: base.revision,
                                    edits,
                                    revision: *revision,
                                    state: state.clone(),
                                })
                            },
                        ),
                        _ => Cow::Borrowed(record),
                    },
                    record => Cow::Borrowed(record),
                });
            }
            crate::record::encode_all(
                floor
                    .iter()
                    .chain(placed.iter())
                    .chain(written.iter().map(|record| &**record)),
            )?
        };
        let mut stream = Stream::new(&writer.history);
        let framed = if self.rows.is_some() && recovered.is_empty() && writer.tail_row.is_some() {
            let mut framed = Vec::with_capacity(plain.len() + 1);
            framed.push(b'\n');
            framed.extend_from_slice(&plain);
            Cow::Owned(framed)
        } else {
            Cow::Borrowed(plain.as_slice())
        };
        let mut body = stream.pack(RECORDS, &framed)?;
        let keep = writer
            .buffer
            .iter()
            .find_map(|record| match record {
                Record::Event { seq, .. } => Some(*seq),
                _ => None,
            })
            .unwrap_or(u64::MAX);
        let keep_fact = writer
            .buffer
            .iter()
            .find_map(|record| match record {
                Record::Fact { seq, .. } => Some(*seq),
                _ => None,
            })
            .unwrap_or(u64::MAX);
        if writer.fold.segment_row() == 0 {
            writer.fold.start_segment(seq);
        }
        let mut snapshot = None;
        let placed_rows = placing
            .as_ref()
            .map_or(0, |placing| placing.section.rows as usize);
        let due = self
            .rows
            .is_some_and(|rows| writer.tail_rows + placed_rows >= rows);
        let bound = self
            .max_tail
            .is_some_and(|max| writer.tail_plain + plain.len() >= max);
        if due || bound || writer.tail + body.len() >= writer.check {
            let forgetting = writer.fold.forgetting();
            warm(
                &*self.host,
                &self.scope,
                &mut writer.fold,
                &mut writer.stats,
                &mut writer.chains,
                &forgetting,
            )
            .await?;
            writer.fold.unshelve_all();
            let waiting = writer.fold.waiting();
            pages(
                &*self.host,
                &self.scope,
                &mut writer.fold,
                &mut writer.stats,
                &mut writer.chains,
                &waiting,
            )
            .await?;
            root(
                &*self.host,
                &self.scope,
                &mut writer.fold,
                &mut writer.stats,
                &mut writer.chains,
            )
            .await?;
            let unlisted = writer.fold.unlisted();
            index_pages(
                &*self.host,
                &self.scope,
                &mut writer.fold,
                &mut writer.stats,
                &mut writer.chains,
                unlisted,
            )
            .await?;
            writer.stats.collected += writer.fold.collect(&writer.collectable) as u64;
            let level = snapshot_level(due, bound);
            let sections = sections(&mut spare, &writer.fold, seq, level, self.limit, self.ratio)?;
            let index = index(&mut spare, &writer.fold, &sections, level, self.limit)?;
            let head = index.next;
            for turn in &recovered {
                writer.fold.recovered_at(turn, head);
            }
            writer.fold.committed(head);
            let packed = packed(
                &mut spare,
                &writer.fold,
                (keep, keep_fact),
                level,
                index.root,
            );
            writer.fold.committed(previous);
            let packed = packed?;
            writer.state = packed.plain;
            let rewrite = index.bytes
                + usize::try_from(
                    (sections.bytes as u128 * u128::from(sections.rewritten))
                        .checked_div(u128::from(sections.plain))
                        .unwrap_or(0),
                )
                .map_err(|error| {
                    StoreError::NotCommitted(format!("turn log rewrite size: {error}"))
                })?;
            let mut line = Vec::new();
            let mut inline = 0;
            for record in &writer.buffer {
                if matches!(record, Record::Event { .. }) {
                    crate::record::write(record, &mut line)?;
                    inline += line.len() + 1;
                    line.clear();
                }
            }
            let inline = usize::try_from(
                (inline.min(packed.plain) as u128 * packed.packed.len() as u128)
                    .checked_div(packed.plain as u128)
                    .unwrap_or(0),
            )
            .unwrap_or(usize::MAX);
            let size = packed.packed.len() - inline.min(packed.packed.len()) + rewrite;
            let tail = writer.tail + body.len();
            if due
                || bound
                || (writer.snapshot + writer.tail > self.ratio * size
                    && tail >= self.ratio * rewrite)
            {
                snapshot = Some(Partitioned {
                    head: packed,
                    index,
                    rewrite,
                    sections,
                    seq: head,
                });
            } else {
                let packed = size;
                let due = (self.ratio * packed).saturating_sub(writer.snapshot);
                let projected = match writer.tried {
                    Some((before, packed_before)) if tail > before && packed > packed_before => {
                        let gap = (tail - before) as u128;
                        let grown = (self.ratio * (packed - packed_before)) as u128;
                        if grown >= gap {
                            self.ratio * tail
                        } else {
                            let reach = gap * due as u128;
                            let rise = grown * tail as u128;
                            usize::try_from(reach.saturating_sub(rise) / (gap - grown))
                                .unwrap_or(usize::MAX)
                        }
                    }
                    _ => due,
                };
                writer.tried = Some((tail, packed));
                writer.check = writer
                    .snapshot
                    .max(due)
                    .max(projected)
                    .max(self.ratio * rewrite);
            }
        }
        if snapshot.is_some() {
            placing = None;
        }
        let mut boundary = None;
        if snapshot.is_none() && writer.fold.hot_bytes() > writer.state {
            writer.state = measured(&writer.fold, keep, keep_fact)?;
            if writer.fold.hot_bytes() > writer.state {
                boundary = Some(codec::pack(&mut spare, BOUNDARY, &plain, &[])?);
            }
        }
        let seq = snapshot.as_ref().map_or_else(
            || seq + placing.as_ref().map_or(0, |placing| placing.section.rows),
            |part| part.seq,
        );
        let body_len = body.len();
        let written = match (&mut snapshot, &mut boundary) {
            (Some(part), _) => std::mem::take(&mut part.head.packed),
            (None, Some(boundary)) => std::mem::take(boundary),
            (None, None) => std::mem::take(&mut body),
        };
        let written_len = written.len();
        let stored = stored(seq, written, self.limit)?;
        let bytes = stored.iter().map(|(_, body)| body.len()).sum::<usize>();
        let archived = match (&snapshot, &writer.previous) {
            (Some(_), Some(previous)) => previous
                .archive(&mut spare, self.limit)?
                .map(|rows| (rows, previous.seq, previous.parts)),
            _ => None,
        };
        let extension = match (&snapshot, &boundary, &writer.tail_row) {
            (None, None, Some(tail))
                if self.rows.is_some()
                    && placing.is_none()
                    && recovered.is_empty()
                    && stored.len() == 1
                    && codec::extensible(&stored[0].1)
                    && self
                        .limit
                        .is_none_or(|limit| tail.bytes - SEAL + body_len - 1 <= limit) =>
            {
                Some((
                    Extension {
                        cut: SEAL,
                        row: tail.seq,
                        seq,
                        tail: &stored[0].1[1..],
                    },
                    tail.bytes - SEAL + body_len - 1,
                ))
            }
            _ => None,
        };
        let (extension, merged) = extension.unzip();
        let (sectioned, superseded) = match &mut snapshot {
            Some(part) => {
                let mut rows = std::mem::take(&mut part.sections.rows);
                rows.append(&mut part.index.rows);
                (rows, std::mem::take(&mut part.index.superseded))
            }
            None => (
                placing
                    .as_mut()
                    .map_or_else(Vec::new, |placing| std::mem::take(&mut placing.rows)),
                Vec::new(),
            ),
        };
        let sectioned_rows = sectioned.len() as u64;
        let sectioned_bytes = sectioned
            .iter()
            .map(|(_, body)| body.len() as u64)
            .sum::<u64>();
        let appended = if sectioned.is_empty() {
            Cow::Borrowed(stored.as_slice())
        } else {
            Cow::Owned(
                sectioned
                    .into_iter()
                    .chain(stored.iter().cloned())
                    .collect::<Vec<_>>(),
            )
        };
        let (kept, dropped) = match &archived {
            Some((rows, from, parts)) => (
                rows.as_slice(),
                from + rows.len() as u64..from + *parts as u64,
            ),
            None => (&[][..], 0..0),
        };
        let compaction = Compaction {
            archived: kept,
            dropped,
            superseded: &superseded,
        };
        let repeated = match self.writer.unsettled.take() {
            Some(_) if crate::sim::mutated(crate::sim::Mutation::RetrySkipsReadBack) => false,
            Some(earlier) => match read_back(&*self.host, &self.scope, &earlier.written()).await {
                Ok(Found::Absent) => false,
                Ok(Found::Ours) if earlier.repeats(extension, merged, &appended) => true,
                Ok(Found::Ours | Found::Other) => {
                    self.writer.fenced = true;
                    return Err(StoreError::Fenced);
                }
                Err(error) => {
                    self.writer.unsettled = Some(earlier);
                    return Err(error);
                }
            },
            None => false,
        };
        let mut riding = Vec::new();
        while let Ok(ride) = self.writer.riders.try_recv() {
            riding.push(ride);
        }
        let host = rider
            .map(|rider| rider.host)
            .or_else(|| riding.first().map(|ride| ride.host));
        let (riding, apart): (Vec<_>, Vec<_>) =
            riding.into_iter().partition(|ride| Some(ride.host) == host);
        for ride in apart {
            let _ = ride.rode.send(false);
        }
        let (held, rodes): (Vec<_>, Vec<_>) = riding
            .into_iter()
            .map(|ride| (ride.statements, ride.rode))
            .unzip();
        let mut statements = rider.map_or_else(Vec::new, |rider| rider.statements.to_vec());
        statements.extend(held.iter().flatten().map(|(sql, params)| SqlStatement {
            sql,
            params: params.iter().map(String::as_str).collect(),
        }));
        let explicit = rider.is_some();
        let rider = host.map(|host| Rider {
            host,
            statements: &statements,
        });
        let answer = |rodes: Vec<oneshot::Sender<bool>>, rode: bool| {
            for sender in rodes {
                let _ = sender.send(rode);
            }
        };
        let (carried, extended) = {
            let carrying = rider.filter(|_| !repeated).and_then(|rider| {
                let write = match extension {
                    Some(extension) => Write::Extend(extension),
                    None => Write::Append {
                        rows: &appended,
                        compaction: compaction.clone(),
                    },
                };
                self.host.carry(&self.scope, write, rider)
            });
            let carried = carrying.is_some();
            let (rodes, extended, done) = match carrying {
                Some(carrying) => (rodes, extension.and(merged), carrying.await),
                None if repeated => {
                    answer(rodes, false);
                    (Vec::new(), extension.and(merged), Ok(()))
                }
                None => {
                    answer(rodes, false);
                    let extending =
                        extension.and_then(|extension| self.host.extend(&self.scope, extension));
                    let extended = extending.is_some().then_some(merged).flatten();
                    let done = match extending {
                        Some(extending) => extending.await,
                        None if archived.is_some() || !superseded.is_empty() => {
                            self.host
                                .append_compacting(&self.scope, &appended, compaction)
                                .await
                        }
                        None => self.host.append(&self.scope, &appended).await,
                    };
                    (Vec::new(), extended, done)
                }
            };
            let done = match done {
                Ok(()) => Ok(()),
                Err(error) => {
                    let written = match extension.zip(extended) {
                        Some((extension, merged)) => Written::Extended(extension, merged),
                        None => Written::Appended(&appended[..]),
                    };
                    let landed = landed(&*self.host, &self.scope, &written, error).await;
                    match &landed {
                        Err(StoreError::Fenced) => self.writer.fenced = true,
                        Err(_) => self.writer.unsettled = Some(Unsettled::of(&written)),
                        Ok(()) => {}
                    }
                    landed
                }
            };
            answer(rodes, done.is_ok());
            done?;
            (carried, extended)
        };
        drop(appended);
        drop(statements);
        drop(held);
        let added = if extended.is_some() {
            body_len - 1 - SEAL
        } else {
            bytes
        };
        let writer = &mut self.writer;
        writer.tail_row = match (extended, &snapshot, &boundary) {
            (Some(merged), _, _) => Some(TailRow { bytes: merged, seq }),
            (None, None, None)
                if recovered.is_empty() && stored.len() == 1 && codec::extensible(&stored[0].1) =>
            {
                Some(TailRow { bytes, seq })
            }
            _ => None,
        };
        writer.next = seq + stored.len() as u64;
        writer.fold.committed(seq);
        if let Some(placing) = &placing {
            writer
                .fold
                .place(placing.section, &placing.owners, placing.keyframe);
            writer.history.link(placing.keyframe, &placing.plain);
            writer
                .history
                .resume_chain(placing.keyframe, placing.state.clone());
            writer.tail_rows += sectioned_rows as usize;
            writer.stats.sections += sectioned_rows;
            writer.stats.placed_bytes += sectioned_bytes;
        }
        for turn in &recovered {
            writer.fold.recovered_at(turn, seq);
        }
        if snapshot.is_some() {
            writer.tails.clear();
        } else {
            for record in &writer.buffer {
                if let Record::Checkpoint {
                    revision, state, ..
                } = record
                {
                    writer.tails.insert(state.clone(), *revision);
                }
            }
        }
        writer.bases.clear();
        writer.buffer.clear();
        writer.floor = None;
        if let Some(keep) = self.retain {
            writer.stats.retired += writer.fold.retire(keep) as u64;
        }
        writer.stats.parts += stored.len() as u64 - 1;
        writer.stats.plain_bytes += plain.len() as u64;
        let snapshotted = snapshot.is_some();
        match snapshot {
            Some(part) => {
                let packed = part.head;
                let rewritten = part.sections.bytes + part.index.bytes;
                writer.fold.sectioned(
                    part.sections.entries,
                    part.index.pages,
                    part.index.groups,
                    part.index.root,
                );
                writer.stats.sections += sectioned_rows;
                writer.stats.snapshot_bytes += rewritten as u64;
                writer.fold.seal(seq, keep);
                writer.fold.seal_facts(seq, keep_fact);
                writer.history.clear();
                writer.history.extend(packed.suffix());
                writer.previous = packed.trimmed().then(|| Previous {
                    bytes,
                    parts: stored.len(),
                    seq,
                    suffix: String::from_utf8(packed.suffix().to_vec())
                        .expect("turn log records encode as UTF-8"),
                });
                writer.snapshot = bytes + part.rewrite;
                writer.check = bytes + part.rewrite;
                nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
                    histogram.nc.turnlog.snapshot_bytes = u64::try_from(bytes + part.rewrite).unwrap_or(u64::MAX),
                    histogram.nc.turnlog.snapshot_plain_bytes = u64::try_from(writer.state).unwrap_or(u64::MAX)
                });
                writer.tail = 0;
                writer.tail_plain = 0;
                writer.tail_rows = stored.len() - 1;
                writer.tried = None;
                writer.stats.snapshots += 1;
                writer.stats.snapshot_bytes += written_len as u64;
            }
            None => {
                if boundary.is_some() {
                    writer.fold.seal(seq, keep);
                    writer.history.clear();
                    writer.stats.boundaries += 1;
                }
                writer
                    .history
                    .extend(if boundary.is_some() { &plain } else { &framed });
                if boundary.is_none() {
                    writer.history.resume_records(stream.state());
                }
                writer.tail += added;
                writer.tail_plain += plain.len();
                writer.tail_rows += stored.len();
                writer.stats.row_bytes += added as u64;
            }
        }
        if snapshotted || !crate::sim::mutated(crate::sim::Mutation::ResidentUntilSnapshot) {
            writer.fold.evict();
        }
        writer.fold.release();
        match point {
            Point::Admission => writer.stats.admissions += 1,
            Point::Call => writer.stats.calls += 1,
            Point::Cancel => writer.stats.cancels += 1,
            Point::Finish => writer.stats.finishes += 1,
            Point::Floor => writer.stats.floors += 1,
            Point::Progress => writer.stats.progress += 1,
            Point::Recovery => writer.stats.recoveries += 1,
        }
        Ok((seq, carried && explicit))
    }
}

#[cfg(test)]
mod level_tests {
    use super::{codec, snapshot_level};

    #[test]
    fn a_snapshot_the_tail_bound_forces_packs_at_the_default_level() {
        assert_eq!(snapshot_level(false, true), codec::DEFAULT_LEVEL);
        assert_eq!(snapshot_level(true, true), codec::DEFAULT_LEVEL);
        assert_eq!(snapshot_level(false, false), codec::DEFAULT_LEVEL);
        assert_eq!(snapshot_level(true, false), codec::FAST_LEVEL);
    }
}
