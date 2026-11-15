use std::{
    borrow::Cow,
    collections::{BTreeSet, VecDeque},
    ops::Range,
    sync::Arc,
};

use nanocode_durability_core::StoreError;
use nanocode_sync::{Held, Owned};

use crate::{
    Compaction, Event, Extension, Fold, LogHost, Record, Segment,
    codec::{
        self, ARCHIVE, BOUNDARY, History, PART, RECORDS, SEAL, SECTION, SNAPSHOT, Stream, WINDOW,
    },
    fold::{Group, Section, appended},
};

pub const EVENT_GAP: u64 = 1 << 14;

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
    pub page_rows: u64,
    pub pages: u64,
    pub parts: u64,
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
    sections: Sections,
    seq: u64,
    superseded: Vec<Range<u64>>,
}

struct Sections {
    bytes: usize,
    groups: Vec<Group>,
    next: u64,
    plain: usize,
    rewritten: usize,
    rows: Vec<(u64, String)>,
}

impl Sections {
    fn seal(
        &mut self,
        packer: codec::Packer,
        owners: Vec<(String, bool)>,
        limit: Option<usize>,
    ) -> Result<(), StoreError> {
        let rows = stored(self.next, packer.finish(SECTION)?.packed, limit)?;
        self.bytes += rows.iter().map(|(_, body)| body.len()).sum::<usize>();
        self.groups.push((
            Section {
                row: self.next,
                rows: rows.len() as u64,
            },
            owners,
        ));
        self.next += rows.len() as u64;
        self.rows.extend(rows);
        Ok(())
    }

    fn rewrite(&self) -> usize {
        (self.bytes as u128 * self.rewritten as u128)
            .checked_div(self.plain as u128)
            .map_or(0, |rewrite| usize::try_from(rewrite).unwrap_or(usize::MAX))
    }
}

fn sections(
    fold: &Fold,
    from: u64,
    level: i32,
    limit: Option<usize>,
) -> Result<Sections, StoreError> {
    let mut sections = Sections {
        bytes: 0,
        groups: Vec::new(),
        next: from,
        plain: 0,
        rewritten: 0,
        rows: Vec::new(),
    };
    let mut open: Option<(codec::Packer, Vec<(String, bool)>)> = None;
    let mut line = Vec::new();
    for owner in fold.writes() {
        let (packer, owners) = match &mut open {
            Some(open) => open,
            None => open.insert((codec::Packer::new(level, &[])?, Vec::new())),
        };
        let mut written = false;
        let before = packer.plain();
        let checkpoint = fold.owner_records(owner, |record| {
            if packer.plain() > 0 {
                line.push(b'\n');
            }
            crate::record::write(&record, &mut line)?;
            packer.write(&line)?;
            line.clear();
            written = true;
            Ok(())
        })?;
        if written {
            owners.push((owner.to_owned(), checkpoint));
        }
        let added = packer.plain() - before;
        sections.plain += added;
        if !appended(owner) {
            sections.rewritten += added;
        }
        if packer.plain() >= WINDOW
            && let Some((packer, owners)) = open.take()
        {
            sections.seal(packer, owners, limit)?;
        }
    }
    if let Some((packer, owners)) = open.take()
        && !owners.is_empty()
    {
        sections.seal(packer, owners, limit)?;
    }
    Ok(sections)
}

async fn warm(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    owner: &str,
) -> Result<u64, StoreError> {
    let Some(sections) = fold.cold(owner).map(<[Section]>::to_vec) else {
        return Ok(0);
    };
    let mut groups = Vec::with_capacity(sections.len());
    for section in sections {
        let end = section.row + section.rows;
        let stored = host.scan(scope, section.row, end).await?;
        if stored.first().map(|(seq, _)| *seq) != Some(section.row)
            || stored.len() as u64 != section.rows
            || !stored[0].1.starts_with(SECTION)
        {
            return Err(StoreError::Backend(format!(
                "turn log section of {owner} at rows {}..{end} reads {} rows",
                section.row,
                stored.len()
            )));
        }
        let mut records = Vec::new();
        each_row(stored, |row| {
            crate::record::each(&row.plain, |record| {
                records.push(record);
                Ok(())
            })?;
            Ok(true)
        })?;
        groups.push((section, records));
    }
    let loads = groups.len() as u64;
    fold.warm(owner, groups)?;
    Ok(loads)
}

async fn warm_state(
    host: &dyn LogHost,
    scope: &str,
    fold: &mut Fold,
    state: &str,
) -> Result<u64, StoreError> {
    Ok(warm(host, scope, fold, state).await?
        + warm(host, scope, fold, &format!("{state}\n")).await?)
}

fn packed(fold: &Fold, keep: u64, level: i32, groups: &[Group]) -> Result<Snapshot, StoreError> {
    let mut packer = codec::Packer::new(level, &[])?;
    let mut line = Vec::new();
    let mut base = 0;
    let mut starts = VecDeque::new();
    let mut tail = Vec::new();
    fold.head_records(keep, groups, |record| {
        if packer.plain() > 0 {
            line.push(b'\n');
        }
        let start = base + tail.len() + line.len();
        crate::record::write(&record, &mut line)?;
        packer.write(&line)?;
        tail.extend_from_slice(&line);
        line.clear();
        starts.push_back((start, matches!(record, Record::Event { .. })));
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
    let packed = packer.finish(SNAPSHOT)?;
    Ok(Snapshot {
        base,
        packed: packed.packed,
        plain: packed.plain,
        start,
        tail,
    })
}

fn measured(fold: &Fold, keep: u64) -> Result<usize, StoreError> {
    let mut bytes = 0;
    let mut line = Vec::new();
    fold.head_records(keep, &[], |record| {
        crate::record::write(&record, &mut line)?;
        bytes += line.len() + usize::from(bytes > 0);
        line.clear();
        Ok(())
    })?;
    Ok(bytes)
}

pub fn rows(stored: &[(u64, String)]) -> Result<Vec<Row>, StoreError> {
    let mut decoded = Vec::new();
    each_row(stored.to_vec(), |row| {
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
    mut take: impl FnMut(Row) -> Result<bool, StoreError>,
) -> Result<bool, StoreError> {
    let mut history = History::default();
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
            Some(tag @ (ARCHIVE | BOUNDARY | RECORDS | SECTION | SNAPSHOT)) => {
                if let Some(logical) = pending.take()
                    && !take(decode_row(logical, &mut history)?)?
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
        Some(logical) => take(decode_row(logical, &mut history)?),
        None => Ok(true),
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
    let boundary = snapshot || tag == BOUNDARY || tag == ARCHIVE || tag == SECTION;
    if boundary {
        history.clear();
    }
    let plain = codec::unpack(&text[tag.len_utf8()..], history.bytes())?;
    drop(text);
    history.extend(&plain);
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
    let stored = host.scan(scope, segment.row, end).await?;
    stats.pages += 1;
    stats.page_rows += stored.len() as u64;
    each_row(stored, |row| {
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

fn suffix(plain: &str, events: usize) -> Option<&str> {
    let floor = plain.len().saturating_sub(WINDOW);
    if floor == 0 {
        return None;
    }
    let (mut start, mut cut, mut found) = (plain.len(), plain.len(), 0);
    while start > floor || found < events {
        let boundary = plain[..cut].rfind('\n')? + 1;
        if let Ok(records) = crate::decode(&plain[boundary..start]) {
            found += records
                .iter()
                .filter(|record| matches!(record, Record::Event { .. }))
                .count();
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

    fn archive(&self, limit: Option<usize>) -> Result<Option<Vec<(u64, String)>>, StoreError> {
        let body = codec::pack(ARCHIVE, self.suffix.as_bytes(), &[])?;
        let rows = stored(self.seq, body, limit)?;
        let bytes = rows.iter().map(|(_, body)| body.len()).sum::<usize>();
        Ok((bytes < self.bytes && rows.len() <= self.parts).then_some(rows))
    }
}

struct TailRow {
    bytes: usize,
    seq: u64,
}

struct Writer {
    buffer: Vec<Record>,
    check: usize,
    collectable: BTreeSet<String>,
    floor: Option<u64>,
    fold: Fold,
    history: History,
    next: u64,
    previous: Option<Previous>,
    snapshot: usize,
    state: usize,
    stats: Stats,
    stream: Option<Stream>,
    tail: usize,
    tail_row: Option<TailRow>,
    tail_rows: usize,
    tried: Option<(usize, usize)>,
}

pub struct TurnLog {
    host: Arc<dyn LogHost>,
    limit: Option<usize>,
    ratio: usize,
    retain: Option<usize>,
    rows: Option<usize>,
    scope: Arc<str>,
    writer: Owned<Writer>,
}

pub struct Lease<'a> {
    host: &'a dyn LogHost,
    limit: Option<usize>,
    ratio: usize,
    retain: Option<usize>,
    rows: Option<usize>,
    scope: &'a str,
    writer: Held<'a, Writer>,
}

impl TurnLog {
    pub async fn open(
        host: Arc<dyn LogHost>,
        scope: impl Into<Arc<str>>,
    ) -> Result<Self, StoreError> {
        let scope = scope.into();
        let stored = host.load(&scope).await?;
        let next = stored.last().map_or(1, |(seq, _)| seq + 1);
        let tail_rows = usize::try_from(
            match stored
                .iter()
                .rposition(|(_, body)| body.starts_with(SNAPSHOT))
            {
                Some(index) => next - 1 - stored[index].0,
                None => stored.first().map_or(0, |(first, _)| next - first),
            },
        )
        .map_err(|error| StoreError::Backend(format!("turn log tail row count: {error}")))?;
        let mut extensible = stored
            .last()
            .filter(|(_, body)| codec::extensible(body))
            .map(|(seq, body)| TailRow {
                bytes: body.len(),
                seq: *seq,
            });
        let mut tail_row = None;
        let mut fold = Fold::default();
        let mut history = History::default();
        let (mut snapshot, mut state, mut tail) = (0, 0, 0);
        let mut previous = None;
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
        if let Some((first, _)) = stored.first() {
            fold.start_segment(*first);
        }
        each_row(stored, |row| {
            if row.snapshot {
                fold = Fold::default();
                fold.start_segment(row.seq);
                history.clear();
                snapshot = row.bytes;
                state = row.plain.len();
                tail = 0;
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
                    fold.seal(row.seq, keep);
                    history.clear();
                }
                tail += row.bytes;
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
            fold.load(row.seq, &row.plain)?;
            if let Some((seq, parts)) = last
                && row.seq == seq
            {
                previous = Previous::of(seq, parts, row.bytes, &row.plain, fold.events().len());
            }
            fold.release();
            history.extend(row.plain.as_bytes());
            Ok(true)
        })?;
        let mut stats = Stats::default();
        for owner in fold.forgetting() {
            stats.section_loads += warm(&*host, &scope, &mut fold, &owner).await?;
        }
        fold.reopen(EVENT_GAP);
        let floor = Some(fold.last_event());
        Ok(Self {
            host,
            limit: None,
            ratio: 2,
            retain: None,
            rows: None,
            scope,
            writer: Owned::new(Writer {
                buffer: Vec::new(),
                check: snapshot,
                collectable: BTreeSet::new(),
                floor,
                fold,
                history,
                next,
                previous,
                snapshot,
                state,
                stats,
                stream: None,
                tail,
                tail_row,
                tail_rows,
                tried: None,
            }),
        })
    }

    #[must_use]
    pub const fn snapshot_ratio(mut self, ratio: usize) -> Self {
        self.ratio = ratio;
        self
    }

    #[must_use]
    pub const fn retain_finished(mut self, keep: Option<usize>) -> Self {
        self.retain = keep;
        self
    }

    #[must_use]
    pub const fn snapshot_rows(mut self, rows: Option<usize>) -> Self {
        self.rows = rows;
        self
    }

    #[must_use]
    pub const fn row_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub async fn lease(&self) -> Lease<'_> {
        Lease {
            host: &*self.host,
            limit: self.limit,
            ratio: self.ratio,
            retain: self.retain,
            rows: self.rows,
            scope: &self.scope,
            writer: self.writer.acquire().await,
        }
    }

    pub async fn read<R>(&self, read: impl FnOnce(&Fold) -> R) -> R {
        read(&self.writer.acquire().await.fold)
    }

    pub async fn load_owner(&self, owner: &str) -> Result<(), StoreError> {
        let mut writer = self.writer.acquire().await;
        let writer = &mut *writer;
        writer.stats.section_loads +=
            warm_state(&*self.host, &self.scope, &mut writer.fold, owner).await?;
        Ok(())
    }

    pub async fn read_owner<R>(
        &self,
        owner: &str,
        read: impl FnOnce(&Fold) -> R,
    ) -> Result<R, StoreError> {
        let mut writer = self.writer.acquire().await;
        let writer = &mut *writer;
        writer.stats.section_loads +=
            warm_state(&*self.host, &self.scope, &mut writer.fold, owner).await?;
        Ok(read(&writer.fold))
    }

    pub async fn stats(&self) -> Stats {
        self.writer.acquire().await.stats
    }

    pub async fn history(
        &self,
        after: u64,
        mut take: impl FnMut(&Event) -> bool,
    ) -> Result<(), StoreError> {
        let mut writer = self.writer.acquire().await;
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

impl Lease<'_> {
    pub async fn held(
        &mut self,
        after: u64,
        mut take: impl FnMut(&Event) -> bool,
    ) -> Result<(), StoreError> {
        let writer = &mut *self.writer;
        if let Some(segment) = writer.fold.unsealed()
            && !scan(
                self.host,
                self.scope,
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

    pub async fn load_owner(&mut self, owner: &str) -> Result<(), StoreError> {
        let writer = &mut *self.writer;
        writer.stats.section_loads +=
            warm_state(self.host, self.scope, &mut writer.fold, owner).await?;
        Ok(())
    }

    #[must_use]
    pub fn buffered(&self) -> usize {
        self.writer.buffer.len()
    }

    pub fn next_event(&mut self) -> u64 {
        self.writer.fold.next_event()
    }

    pub fn collect_unreachable(&mut self, state: &str) {
        if !self.writer.collectable.contains(state) {
            self.writer.collectable.insert(state.to_owned());
        }
    }

    pub fn push(&mut self, record: Record) -> Result<(), StoreError> {
        let writer = &mut *self.writer;
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
        let writer = &mut *self.writer;
        writer.buffer.retain(|record| match record {
            Record::Admitted { turn: admitted, .. } => admitted != turn,
            Record::Event { seq: event, .. } => *event != seq,
            _ => true,
        });
        writer.fold.unadmit(turn, seq);
    }

    #[must_use]
    pub fn floor_pending(&self) -> bool {
        self.writer.floor.is_some()
    }

    pub async fn secure(&mut self) -> Result<(), StoreError> {
        if self.writer.floor.is_some() {
            self.commit(Point::Floor).await?;
        }
        Ok(())
    }

    pub async fn commit(&mut self, point: Point) -> Result<u64, StoreError> {
        let writer = &mut *self.writer;
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
        let plain = crate::record::encode_all(floor.iter().chain(&writer.buffer))?;
        let mut stream = match writer.stream.take() {
            Some(stream) => stream,
            None => Stream::new(writer.history.bytes())?,
        };
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
        if writer.fold.segment_row() == 0 {
            writer.fold.start_segment(seq);
        }
        let mut snapshot = None;
        let due = self.rows.is_some_and(|rows| writer.tail_rows >= rows);
        if due || writer.tail + body.len() >= writer.check {
            for owner in writer.fold.unloaded() {
                writer.stats.section_loads +=
                    warm(self.host, self.scope, &mut writer.fold, &owner).await?;
            }
            writer.stats.collected += writer.fold.collect(&writer.collectable) as u64;
            let level = if due {
                codec::FAST_LEVEL
            } else {
                codec::DEFAULT_LEVEL
            };
            let sections = sections(&writer.fold, seq, level, self.limit)?;
            let head = sections.next;
            let mut groups = writer.fold.kept_groups();
            let superseded = writer.fold.superseded(&groups);
            groups.extend(
                sections
                    .groups
                    .iter()
                    .map(|group| (group.0.row, group.clone())),
            );
            let groups = groups.into_values().collect::<Vec<_>>();
            for turn in &recovered {
                writer.fold.recovered_at(turn, head);
            }
            writer.fold.committed(head);
            let packed = packed(&writer.fold, keep, level, &groups);
            writer.fold.committed(previous);
            let packed = packed?;
            writer.state = packed.plain;
            let size = packed.packed.len();
            let rewrite = sections.rewrite();
            let tail = writer.tail + body.len();
            if due
                || (writer.snapshot + tail > self.ratio * size && self.ratio * tail >= rewrite)
            {
                snapshot = Some(Partitioned {
                    head: packed,
                    sections,
                    seq: head,
                    superseded,
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
                    .max(rewrite.div_ceil(self.ratio));
            }
        }
        let mut boundary = None;
        if snapshot.is_none() && writer.fold.hot_bytes() > writer.state {
            writer.state = measured(&writer.fold, keep)?;
            if writer.fold.hot_bytes() > writer.state {
                boundary = Some(codec::pack(BOUNDARY, &plain, &[])?);
            }
        }
        let seq = snapshot.as_ref().map_or(seq, |part| part.seq);
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
                .archive(self.limit)?
                .map(|rows| (rows, previous.seq, previous.parts)),
            _ => None,
        };
        let extension = match (&snapshot, &boundary, &writer.tail_row) {
            (None, None, Some(tail))
                if self.rows.is_some()
                    && recovered.is_empty()
                    && stored.len() == 1
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
        let (extending, extended) = match extension {
            Some((extension, merged)) => (self.host.extend(self.scope, extension), Some(merged)),
            None => (None, None),
        };
        let extended = extending.is_some().then_some(extended).flatten();
        let (sectioned, superseded) = match &mut snapshot {
            Some(part) => (
                std::mem::take(&mut part.sections.rows),
                std::mem::take(&mut part.superseded),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let sectioned_rows = sectioned.len() as u64;
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
        match (extending, &archived) {
            (Some(extending), _) => extending.await?,
            (None, archived) if archived.is_some() || !superseded.is_empty() => {
                let (rows, dropped) = match archived {
                    Some((rows, from, parts)) => (
                        rows.as_slice(),
                        from + rows.len() as u64..from + *parts as u64,
                    ),
                    None => (&[][..], 0..0),
                };
                self.host
                    .append_compacting(
                        self.scope,
                        &appended,
                        Compaction {
                            archived: rows,
                            dropped,
                            superseded: &superseded,
                        },
                    )
                    .await?;
            }
            (None, _) => self.host.append(self.scope, &appended).await?,
        }
        drop(appended);
        let added = if extended.is_some() {
            body_len - 1 - SEAL
        } else {
            bytes
        };
        let writer = &mut *self.writer;
        writer.tail_row = match (extended, &snapshot, &boundary) {
            (Some(merged), _, _) => Some(TailRow { bytes: merged, seq }),
            (None, None, None) if recovered.is_empty() && stored.len() == 1 => {
                Some(TailRow { bytes, seq })
            }
            _ => None,
        };
        writer.next = seq + stored.len() as u64;
        writer.fold.committed(seq);
        for turn in &recovered {
            writer.fold.recovered_at(turn, seq);
        }
        writer.buffer.clear();
        writer.floor = None;
        if let Some(keep) = self.retain {
            writer.stats.retired += writer.fold.retire(keep) as u64;
        }
        writer.stats.parts += stored.len() as u64 - 1;
        writer.stats.plain_bytes += plain.len() as u64;
        match snapshot {
            Some(part) => {
                let packed = part.head;
                writer.fold.sectioned(&part.sections.groups);
                writer.stats.sections += sectioned_rows;
                writer.stats.snapshot_bytes += part.sections.bytes as u64;
                writer.fold.seal(seq, keep);
                writer.history.clear();
                writer.history.extend(packed.suffix());
                writer.previous = packed.trimmed().then(|| Previous {
                    bytes,
                    parts: stored.len(),
                    seq,
                    suffix: String::from_utf8(packed.suffix().to_vec())
                        .expect("turn log records encode as UTF-8"),
                });
                writer.snapshot = bytes;
                writer.check = bytes;
                writer.tail = 0;
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
                if boundary.is_none() {
                    writer.stream = Some(stream);
                }
                writer
                    .history
                    .extend(if boundary.is_some() { &plain } else { &framed });
                writer.tail += added;
                writer.tail_rows += stored.len();
                writer.stats.row_bytes += added as u64;
            }
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
        Ok(seq)
    }
}
