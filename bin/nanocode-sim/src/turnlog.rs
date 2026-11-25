use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
};

use nanocode_compression::miniz::{Format, Level, compress};
use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::FutureExt;
use nanocode_sys::base64::STANDARD_NO_PAD;
use nanocode_turnlog::{
    Compaction, EffectClass, Extension, Fold, Follower, LogHost, Outcome as LogOutcome, Phase,
    Point as LogPoint, Record as LogRecord, Status as LogStatus, TurnLog,
};
use serde_json::value::{RawValue, to_raw_value};

use crate::{
    disk::Disk,
    onelog::{
        Appended, Class, Decision, EventId, Kind, Log, Outcome, Point, Protections, RESTART_CAP,
        Record, Reply, ScopeState, Status, TurnDurability, encode,
    },
};

const WINDOW: usize = 1 << 15;
const SEAL: usize = 3;
const PART_EVERY: u64 = 3;
const OWNERS_PER_COMMIT: u64 = 16;

pub struct SimHost {
    disk: Arc<Disk>,
    extending: bool,
    kept: bool,
    limit: Option<usize>,
    pruned: bool,
    sections_kept: bool,
    short: bool,
}

fn prune(
    rows: &mut crate::disk::Rows,
    scope: &str,
    appended: &[(u64, String)],
    superseded: &[std::ops::Range<u64>],
) {
    let Some((first, _)) = appended.first() else {
        return;
    };
    if !appended.iter().any(|(_, body)| body.starts_with('s')) {
        return;
    }
    let sections = rows
        .view(scope)
        .into_iter()
        .filter(|(seq, body)| {
            seq < first
                && body.starts_with('p')
                && !superseded.iter().any(|range| range.contains(seq))
        })
        .map(|(seq, _)| (scope.to_owned(), Vec::new(), seq..seq + 1))
        .collect::<Vec<_>>();
    rows.tcompactions.extend(sections);
}

fn window_only(archived: &[(u64, String)], limit: Option<usize>) -> Vec<(u64, String)> {
    let Some((first, _)) = archived.first() else {
        return Vec::new();
    };
    let plain = nanocode_turnlog::rows(archived)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .map(|row| row.plain)
        .unwrap_or_default();
    let floor = plain.len().saturating_sub(WINDOW);
    let (mut start, mut cut) = (plain.len(), plain.len());
    while start > floor {
        let Some(newline) = plain[..cut].rfind('\n') else {
            return archived.to_vec();
        };
        let boundary = newline + 1;
        if nanocode_turnlog::decode(&plain[boundary..start]).is_ok() {
            start = boundary;
        }
        cut = newline;
    }
    let mut body = String::from("a");
    STANDARD_NO_PAD.encode_string(
        compress(&plain.as_bytes()[start..], Level::DEFAULT, Format::Raw),
        &mut body,
    );
    let mut parts = Vec::new();
    let mut rest = body.as_str();
    while !rest.is_empty() {
        let room = match (limit, parts.is_empty()) {
            (Some(limit), true) => limit,
            (Some(limit), false) => limit - 1,
            (None, _) => rest.len(),
        };
        let (part, next) = rest.split_at(rest.len().min(room));
        let seq = first + parts.len() as u64;
        parts.push(if parts.is_empty() {
            (seq, part.to_owned())
        } else {
            (seq, format!("+{part}"))
        });
        rest = next;
    }
    parts
}

impl SimHost {
    pub const fn new(disk: Arc<Disk>) -> Self {
        Self {
            disk,
            extending: false,
            kept: false,
            limit: None,
            pruned: false,
            sections_kept: false,
            short: false,
        }
    }
}

impl LogHost for SimHost {
    fn call_bound(&self) -> Option<std::time::Duration> {
        None
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        let rows = self.disk.with(|rows| rows.tscan(scope, from, to));
        Box::pin(async move { Ok(rows) })
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        let rows = self.disk.with(|rows| rows.tload(scope));
        Box::pin(async move { Ok(rows) })
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let pruned = self.pruned;
        let inserted = self.disk.with(|disk| {
            let inserted = disk.insert(scope, rows);
            if inserted {
                disk.tops.push(crate::disk::FileOp::Append {
                    archived: Vec::new(),
                    dropped: 0..0,
                    rows: rows.to_vec(),
                    scope: scope.to_owned(),
                    superseded: Vec::new(),
                });
            }
            if inserted && pruned {
                prune(disk, scope, rows, &[]);
            }
            inserted
        });
        Box::pin(async move {
            if inserted {
                Ok(())
            } else {
                Err(StoreError::Fenced)
            }
        })
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let kept = self.kept;
        if let Some((first, _)) = compaction.archived.first() {
            let replaced = self.disk.with(|disk| {
                disk.view(scope)
                    .into_iter()
                    .filter(|(seq, _)| *seq >= *first)
                    .take(compaction.archived.len() + compaction.dropped.clone().count())
                    .collect::<Vec<_>>()
            });
            let plain = |stored: &[(u64, String)]| {
                let rows = nanocode_turnlog::rows(stored).unwrap_or_else(|error| {
                    panic!("an archived or replaced turn log row does not decode: {error}")
                });
                assert_eq!(rows.len(), 1, "an archive replaces one snapshot row");
                rows.into_iter()
                    .next()
                    .map(|row| row.plain)
                    .unwrap_or_default()
            };
            let (snapshot, archive) = (plain(&replaced), plain(compaction.archived));
            let events = |text: &str| {
                nanocode_turnlog::decode(text)
                    .unwrap_or_else(|error| panic!("a turn log row does not decode: {error}"))
                    .into_iter()
                    .filter_map(|record| match record {
                        LogRecord::Event { seq, .. } => Some(seq),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            assert!(
                replaced
                    .first()
                    .is_some_and(|(_, body)| body.starts_with('s')),
                "the archive of {scope} row {first} is not a suffix of its snapshot holding every event"
            );
            assert!(
                snapshot.ends_with(&archive),
                "the archive of {scope} row {first} is not a suffix of its snapshot holding every event"
            );
            assert!(
                events(&snapshot) == events(&archive),
                "the archive of {scope} row {first} is not a suffix of its snapshot holding every event"
            );
        }
        let mut compaction = compaction;
        let shortened;
        if self.short
            && let Some((first, _)) = compaction.archived.first()
        {
            let end = compaction
                .dropped
                .end
                .max(first + compaction.archived.len() as u64);
            shortened = window_only(compaction.archived, self.limit);
            compaction = Compaction {
                archived: &shortened,
                dropped: first + shortened.len() as u64..end,
                superseded: compaction.superseded,
            };
        }
        let pruned = self.pruned;
        let superseded = if self.sections_kept {
            &[][..]
        } else {
            compaction.superseded
        };
        let inserted = self.disk.with(|disk| {
            let inserted = disk.insert(scope, rows);
            if inserted {
                disk.tops.push(crate::disk::FileOp::Append {
                    archived: if kept {
                        Vec::new()
                    } else {
                        compaction.archived.to_vec()
                    },
                    dropped: if kept {
                        0..0
                    } else {
                        compaction.dropped.clone()
                    },
                    rows: rows.to_vec(),
                    scope: scope.to_owned(),
                    superseded: if kept {
                        Vec::new()
                    } else {
                        superseded.to_vec()
                    },
                });
            }
            if inserted && !kept {
                disk.tcompactions.push((
                    scope.to_owned(),
                    compaction.archived.to_vec(),
                    compaction.dropped,
                ));
                disk.tcompactions.extend(
                    superseded
                        .iter()
                        .map(|range| (scope.to_owned(), Vec::new(), range.clone())),
                );
            }
            if inserted && pruned {
                prune(disk, scope, rows, if kept { &[] } else { superseded });
            }
            inserted
        });
        Box::pin(async move {
            if inserted {
                Ok(())
            } else {
                Err(StoreError::Fenced)
            }
        })
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        if !self.extending {
            return None;
        }
        let extended = self.disk.with(|disk| {
            let Some((_, body)) = disk
                .view(scope)
                .into_iter()
                .find(|(seq, _)| *seq == extension.row)
            else {
                return false;
            };
            let kept = body.len().checked_sub(extension.cut).unwrap_or_else(|| {
                panic!("turn log row {} is shorter than its seal", extension.row)
            });
            let moved = [(
                extension.seq,
                format!("{}{}", &body[..kept], extension.tail),
            )];
            let inserted = disk.insert(scope, &moved);
            if inserted {
                disk.tcompactions.push((
                    scope.to_owned(),
                    Vec::new(),
                    extension.row..extension.row + 1,
                ));
                disk.tops.push(crate::disk::FileOp::Extend {
                    cut: extension.cut,
                    row: extension.row,
                    scope: scope.to_owned(),
                    seq: extension.seq,
                    tail: extension.tail.to_owned(),
                });
            }
            inserted
        });
        Some(Box::pin(async move {
            if extended {
                Ok(())
            } else {
                Err(StoreError::Fenced)
            }
        }))
    }
}

fn recovers(plain: &str) -> bool {
    nanocode_turnlog::decode(plain)
        .unwrap_or_else(|error| panic!("a turn log row does not decode: {error}"))
        .iter()
        .any(|record| matches!(record, LogRecord::Recovery { .. }))
}

pub fn padding(scope: &str, bytes: usize) -> String {
    let mut seed = scope.bytes().fold(0x9e37_79b9_7f4a_7c15_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    (0..bytes)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            char::from(b'a' + u8::try_from(seed % 26).expect("a letter index fits in u8"))
        })
        .collect()
}

fn ready<T>(future: impl Future<Output = T>) -> T {
    future
        .now_or_never()
        .expect("the simulated turn log host answers every call at once")
}

fn refused<T>(result: Result<T, StoreError>) -> T {
    result.unwrap_or_else(|error| panic!("nanocode-turnlog refused: {error}"))
}

fn content(id: u64) -> String {
    format!("b3:{id:064x}")
}

fn chain(top: u64) -> Vec<u64> {
    std::iter::successors(Some(top), |id| Some(id / 2).filter(|id| *id > 0)).collect()
}

fn content_value(scope: &str, id: u64) -> String {
    match id / 2 {
        0 => format!("={scope} content {id}"),
        next => format!("={scope} content {id} {}", content(next)),
    }
}

fn content_keys(fold: &Fold, scope: &str) -> Vec<String> {
    let state = format!("{scope}:gc");
    fold.checkpoint(&state)
        .and_then(|checkpoint| serde_json::from_str::<Vec<String>>(&checkpoint.payload).ok())
        .and_then(|keys| keys.into_iter().next())
        .and_then(|key| u64::from_str_radix(key.strip_prefix("b3:")?, 16).ok())
        .map(|top| {
            chain(top)
                .into_iter()
                .map(|id| format!("{state}\n{}", content(id)))
                .collect()
        })
        .unwrap_or_default()
}

fn contents_kept(fold: &Fold, scope: &str, when: &str) -> Vec<u64> {
    let state = format!("{scope}:gc");
    let Some(checkpoint) = fold.checkpoint(&state) else {
        return Vec::new();
    };
    let top = serde_json::from_str::<Vec<String>>(&checkpoint.payload)
        .ok()
        .and_then(|keys| keys.into_iter().next())
        .and_then(|key| u64::from_str_radix(key.strip_prefix("b3:")?, 16).ok())
        .unwrap_or_else(|| panic!("{state} checkpoint {} names no content", checkpoint.payload));
    let ids = chain(top);
    for &id in &ids {
        let key = format!("{state}\n{}", content(id));
        assert!(
            fold.stored(&key) == Some(content_value(scope, id).as_str()),
            "{state} content {id} reached from revision {} reads back as {:?} {when}",
            checkpoint.revision,
            fold.stored(&key)
        );
    }
    ids
}

fn written_once(
    disk: &Disk,
    scope: &str,
    end: u64,
    seq: u64,
    fresh: &BTreeSet<String>,
    pushed: &BTreeSet<String>,
) {
    let stored = disk.with(|rows| rows.view(scope));
    let start = stored
        .iter()
        .rposition(|(row, body)| *row <= end && (body.starts_with('s') || body.starts_with('a')))
        .unwrap_or(0);
    let mut section = false;
    let sections = stored[start..]
        .iter()
        .filter(|(_, body)| {
            if !body.starts_with('+') {
                section = body.starts_with('p');
            }
            section && !body.starts_with('+')
        })
        .map(|(row, _)| *row)
        .collect::<BTreeSet<_>>();
    let decoded = nanocode_turnlog::rows(&stored[start..])
        .unwrap_or_else(|error| panic!("a turn log row of {scope} does not decode: {error}"));
    let prefix = format!("{scope}:gc\n");
    let mut holders = BTreeMap::<String, Vec<u64>>::new();
    let mut inline = BTreeSet::new();
    for row in &decoded {
        let records = row
            .records()
            .unwrap_or_else(|error| panic!("a turn log row of {scope} does not decode: {error}"));
        for record in records {
            if let LogRecord::Stored { key, .. } = record
                && key.starts_with(&prefix)
            {
                if row.seq > end {
                    holders.entry(key).or_default().push(row.seq);
                } else if !sections.contains(&row.seq) {
                    inline.insert(key);
                }
            }
        }
    }
    for key in fresh {
        let rows = holders.get(key).map_or(&[][..], Vec::as_slice);
        assert!(
            rows.len() == 1,
            "{scope} commit {seq} wrote new content {key:?} into rows {rows:?}, not exactly once"
        );
    }
    for (key, rows) in &holders {
        let sectioned = rows
            .iter()
            .filter(|row| sections.contains(row))
            .collect::<Vec<_>>();
        assert!(
            sectioned.is_empty() || pushed.contains(key) || inline.contains(key),
            "{scope} commit {seq} rewrote content {key:?} into sections {sectioned:?}, though no row since its last snapshot holds it inline"
        );
    }
}

fn owners_written(revision: u64, owners: u64) -> impl Iterator<Item = u64> {
    (0..OWNERS_PER_COMMIT.min(owners))
        .map(move |offset| (revision * OWNERS_PER_COMMIT + offset) % owners)
}

fn owner_value(scope: &str, revision: u64) -> String {
    format!("{scope} owner {revision}")
}

fn referenced(stored: &[(u64, String)]) -> Vec<std::ops::Range<u64>> {
    let Some(start) = stored.iter().rposition(|(_, body)| body.starts_with('s')) else {
        return Vec::new();
    };
    let end = stored[start + 1..]
        .iter()
        .take_while(|(_, body)| body.starts_with('+'))
        .count()
        + start
        + 1;
    let records = |rows: &[(u64, String)]| {
        nanocode_turnlog::rows(rows)
            .unwrap_or_else(|error| panic!("a turn log head does not decode: {error}"))
            .iter()
            .flat_map(|row| {
                row.records()
                    .unwrap_or_else(|error| panic!("a turn log head does not decode: {error}"))
            })
            .collect::<Vec<_>>()
    };
    records(&stored[start..end])
        .into_iter()
        .flat_map(|record| match record {
            LogRecord::Root { row, rows } => {
                let root = stored
                    .iter()
                    .filter(|(seq, _)| (row..row + rows).contains(seq))
                    .cloned()
                    .collect::<Vec<_>>();
                std::iter::once(row..row + rows)
                    .chain(records(&root).into_iter().flat_map(|record| {
                        match record {
                            LogRecord::Index { row, rows, .. } => {
                                std::iter::once(row..row + rows).collect()
                            }
                            LogRecord::Groups { groups, .. } => groups
                                .into_iter()
                                .map(|(row, rows, _)| row..row + rows)
                                .collect(),
                            _ => Vec::new(),
                        }
                    }))
                    .collect()
            }
            LogRecord::Index { row, rows, .. } => std::iter::once(row..row + rows).collect(),
            LogRecord::Groups { groups, .. } => groups
                .into_iter()
                .map(|(row, rows, _)| row..row + rows)
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn part_value(scope: &str, revision: u64) -> String {
    format!("{scope} part {revision}")
}

fn part_revision(revision: u64) -> Option<u64> {
    std::iter::successors(Some(PART_EVERY), |part| part.checked_mul(PART_EVERY))
        .take_while(|part| *part <= revision)
        .last()
}

fn raw(text: &str) -> Box<RawValue> {
    to_raw_value(text).expect("a string encodes as JSON")
}

pub fn text(value: &RawValue) -> String {
    serde_json::from_str(value.get()).unwrap_or_else(|_| value.get().to_owned())
}

pub struct TurnLogPolicy {
    bound: Option<usize>,
    chain_bytes: Option<usize>,
    cached: BTreeMap<String, BTreeSet<u64>>,
    collect_live: bool,
    disk: Arc<Disk>,
    followers: BTreeMap<String, Follower>,
    host: Arc<dyn LogHost>,
    limit: Option<usize>,
    logs: BTreeMap<String, TurnLog>,
    opens: u64,
    owners: Option<u64>,
    ratio: usize,
    ratio_ignored: bool,
    protections: Protections,
    read_unloaded: bool,
    restarting: BTreeMap<String, String>,
    retain: Option<usize>,
    rewrite_stale: bool,
    retention: Option<usize>,
    rows: Option<usize>,
    single: bool,
    state: Option<usize>,
    takeover: Option<bool>,
    unsealed: BTreeSet<String>,
}

impl TurnLogPolicy {
    #[must_use]
    pub const fn chain_bytes(mut self, chain_bytes: Option<usize>) -> Self {
        self.chain_bytes = chain_bytes;
        self
    }

    pub fn new(
        disk: Arc<Disk>,
        protections: Protections,
        limit: Option<usize>,
        ratio: usize,
        retain: Option<usize>,
        retention: Option<usize>,
    ) -> Self {
        Self {
            bound: None,
            chain_bytes: None,
            cached: BTreeMap::new(),
            collect_live: false,
            disk: Arc::clone(&disk),
            followers: BTreeMap::new(),
            host: Arc::new(SimHost {
                disk,
                extending: true,
                kept: false,
                limit,
                pruned: false,
                sections_kept: false,
                short: false,
            }),
            limit,
            logs: BTreeMap::new(),
            opens: 0,
            owners: None,
            ratio,
            ratio_ignored: false,
            protections,
            read_unloaded: false,
            restarting: BTreeMap::new(),
            retain,
            rewrite_stale: false,
            retention,
            rows: None,
            single: true,
            state: None,
            takeover: None,
            unsealed: BTreeSet::new(),
        }
    }

    pub fn state_bytes(
        mut self,
        bytes: Option<usize>,
        kept: bool,
        short: bool,
        extending: bool,
        pruned: bool,
        sections_kept: bool,
    ) -> Self {
        self.state = bytes;
        self.host = Arc::new(SimHost {
            disk: Arc::clone(&self.disk),
            extending,
            kept,
            limit: self.limit,
            pruned,
            sections_kept,
            short,
        });
        self
    }

    pub const fn owners(mut self, owners: Option<u64>) -> Self {
        self.owners = owners;
        self
    }

    pub const fn collect_live(mut self, live: bool) -> Self {
        self.collect_live = live;
        self
    }

    pub const fn rewrite_stale(mut self, stale: bool) -> Self {
        self.rewrite_stale = stale;
        self
    }

    pub const fn read_unloaded(mut self, unloaded: bool) -> Self {
        self.read_unloaded = unloaded;
        self
    }

    pub const fn ratio_ignored(mut self, ignored: bool) -> Self {
        self.ratio_ignored = ignored;
        self
    }

    pub const fn single_snapshot(mut self, single: bool) -> Self {
        self.single = single;
        self
    }

    pub const fn takeover(mut self, takeover: bool, opens: bool) -> Self {
        self.takeover = if takeover { Some(opens) } else { None };
        self
    }

    fn take_over(&mut self, scope: &str, seq: u64) {
        let Some(opens) = self.takeover else {
            return;
        };
        let whole = self.whole();
        if !self.followers.contains_key(scope) {
            let follower = refused(ready(TurnLog::follow(Arc::clone(&self.host), scope)));
            self.followers.insert(scope.to_owned(), follower);
        }
        let follower = self
            .followers
            .get_mut(scope)
            .expect("a follower was inserted above");
        let view = |fold: &Fold| {
            (
                fold.events().len(),
                fold.turns()
                    .iter()
                    .map(|turn| turn.id.clone())
                    .collect::<Vec<_>>(),
                fold.open_calls()
                    .map(|(id, _)| id.to_owned())
                    .collect::<Vec<_>>(),
                fold.states()
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
                fold.cells()
                    .map(|(cell, count, _)| (cell.to_owned(), count))
                    .collect::<Vec<_>>(),
            )
        };
        refused(ready(follower.advance()));
        if follower.next() <= seq {
            return;
        }
        let follower = self
            .followers
            .remove(scope)
            .expect("a follower was read above");
        let claimed = if opens {
            refused(ready(TurnLog::open(Arc::clone(&self.host), scope)))
        } else {
            refused(ready(follower.claim()))
        }
        .retain_finished(self.retain)
        .row_limit(self.limit)
        .chain_bytes_max(self.chain_bytes)
        .snapshot_ratio(if self.ratio_ignored {
            u32::MAX as usize
        } else {
            self.ratio
        })
        .snapshot_rows(self.rows)
        .whole_checkpoints(whole);
        let stale = self
            .logs
            .remove(scope)
            .expect("a takeover follows a commit of an open log");
        let acknowledged =
            ready(stale.read(|fold| (fold.last_event(), fold.events().len(), fold.recovering())));
        let stale_key = format!("{scope}:stale");
        let fenced = crate::facts::writing(&stale, move |writing| {
            refused(writing.push(LogRecord::Host {
                key: stale_key,
                value: raw("true"),
            }));
            ready(writing.commit(LogPoint::Recovery))
        });
        assert!(
            matches!(fenced, Err(StoreError::Fenced)),
            "{scope} accepts a commit from its stale writer after a takeover at commit {seq}: {fenced:?}"
        );
        let served = ready(stale.read_owner(scope, |_| ()));
        assert!(
            matches!(served, Err(StoreError::Fenced)),
            "{scope} serves a read from its fenced writer after a takeover at commit {seq}: {served:?}"
        );
        let cold = refused(ready(TurnLog::open(Arc::clone(&self.host), scope)));
        let stale_key = format!("{scope}:stale");
        let read = move |fold: &Fold| {
            (
                fold.last_event(),
                fold.events().len(),
                fold.recovering(),
                fold.host(&stale_key).is_some(),
            )
        };
        let taken = ready(claimed.read(read.clone()));
        let reopened = ready(cold.read(read));
        let claimed_view = ready(claimed.read(view));
        let cold_view = ready(cold.read(view));
        assert!(
            claimed_view == cold_view,
            "{scope} takes over at commit {seq} with turns, open calls, states and cells {claimed_view:?}, but a cold open reads {cold_view:?}"
        );
        assert!(
            (taken.1, taken.2, taken.3) == (reopened.1, reopened.2, reopened.3),
            "{scope} takes over at commit {seq} with last event, events, recovery and stale write {taken:?}, but a cold open reads {reopened:?} and the stale writer acknowledged {acknowledged:?}"
        );
        assert!(
            taken.1 == acknowledged.1,
            "{scope} takes over at commit {seq} with last event, events, recovery and stale write {taken:?}, but a cold open reads {reopened:?} and the stale writer acknowledged {acknowledged:?}"
        );
        assert!(
            taken.0 > acknowledged.0,
            "{scope} takes over at commit {seq} with last event, events, recovery and stale write {taken:?}, but a cold open reads {reopened:?} and the stale writer acknowledged {acknowledged:?}"
        );
        assert!(
            !taken.3,
            "{scope} takes over at commit {seq} with last event, events, recovery and stale write {taken:?}, but a cold open reads {reopened:?} and the stale writer acknowledged {acknowledged:?}"
        );
        self.logs.insert(scope.to_owned(), claimed);
    }

    pub const fn snapshot_rows(mut self, rows: Option<usize>, bound: Option<usize>) -> Self {
        self.rows = rows;
        self.bound = bound;
        self
    }

    const fn whole(&mut self) -> bool {
        self.opens += 1;
        self.opens.is_multiple_of(2)
    }

    fn log(&mut self, scope: &str) -> &TurnLog {
        if !self.logs.contains_key(scope) {
            let whole = self.whole();
            let log = refused(ready(TurnLog::open(Arc::clone(&self.host), scope)))
                .retain_finished(self.retain)
                .row_limit(self.limit)
                .chain_bytes_max(self.chain_bytes)
                .snapshot_ratio(if self.ratio_ignored {
                    u32::MAX as usize
                } else {
                    self.ratio
                })
                .snapshot_rows(self.rows)
                .whole_checkpoints(whole);
            self.logs.insert(scope.to_owned(), log);
        }
        &self.logs[scope]
    }

    fn push(&mut self, scope: &str, record: LogRecord) {
        let log = self.log(scope);
        refused(crate::facts::writing(log, move |writing| {
            writing.push(record)
        }));
    }

    fn records(&mut self, scope: &str, record: Record) -> Vec<LogRecord> {
        let restarting = self.restarting.get(scope).cloned();
        let log = self.log(scope);
        ready(log.read(move |fold| match record {
            Record::Admitted { origin, request } => vec![
                LogRecord::Admitted {
                    digest: String::new(),
                    first: fold.last_event(),
                    hand: None,
                    metadata: raw(&format!("{origin:?}")),
                    prompt: raw(&request),
                    turn: request.clone(),
                },
                LogRecord::Status {
                    status: LogStatus::Running,
                    turn: request,
                },
            ],
            Record::Ended {
                nonce,
                request,
                status,
            } => vec![LogRecord::Status {
                status: LogStatus::Finished {
                    last: fold.last_event(),
                    outcome: match status {
                        Status::Cancelled => LogOutcome::Cancelled,
                        Status::Failed => LogOutcome::Failed,
                        Status::Finished => LogOutcome::Completed,
                    },
                    result: raw(&format!("{}:{nonce}", status.code())),
                },
                turn: request,
            }],
            Record::Event { id, text } => {
                let Some((0, seq)) = id else {
                    panic!("an event without a turn log seq: {text}");
                };
                vec![LogRecord::Event {
                    at: 0,
                    event: raw(&text),
                    seq,
                    turn: None,
                }]
            }
            Record::Floor { .. } => Vec::new(),
            Record::Model {
                ref reply,
                ref request,
                ..
            } => {
                let mut records = vec![LogRecord::Model {
                    response: raw(encode(std::slice::from_ref(&record)).trim_end()),
                    turn: request.clone(),
                }];
                if let Reply::Calls(calls) = reply {
                    for call in calls {
                        let effect = match call.tool.class() {
                            Class::External => EffectClass::External,
                            Class::Idempotent => EffectClass::Idempotent,
                            Class::InObject => {
                                records.push(LogRecord::Host {
                                    key: format!("effect:{}", call.id),
                                    value: raw(&format!("{:?}", call.tool)),
                                });
                                continue;
                            }
                            Class::None => continue,
                        };
                        records.push(LogRecord::Call {
                            call_id: call.id.clone(),
                            effect,
                            input: raw(&format!("{:?}", call.tool)),
                            tool: format!("{:?}", call.tool),
                            turn: Some(request.clone()),
                        });
                    }
                }
                records
            }
            Record::Restart { attempt, .. } => {
                let turn =
                    restarting.expect("a restart record follows the policy's restart decision");
                vec![
                    LogRecord::Recovery {
                        at: None,
                        attempt,
                        turn: turn.clone(),
                        unknown: fold.unknown(),
                    },
                    LogRecord::Status {
                        status: LogStatus::Running,
                        turn,
                    },
                ]
            }
            Record::Result { call, outcome } => vec![LogRecord::Result {
                call_id: call,
                output: match outcome {
                    Outcome::Done(text) => raw(&text),
                    Outcome::Unknown => raw("unknown"),
                },
            }],
        }))
    }

    const fn commit_point(
        &self,
        point: Point,
        recovering: bool,
        floor: bool,
    ) -> Option<(LogPoint, Kind)> {
        let protections = &self.protections;
        match point {
            Point::Admission if protections.admission => {
                Some((LogPoint::Admission, Kind::Admission))
            }
            Point::Calls {
                external,
                in_object,
            } if (external && protections.call_external)
                || (in_object && protections.call_in_object) =>
            {
                Some((LogPoint::Call, Kind::Call))
            }
            Point::Calls { .. } | Point::Results { .. } if protections.progress && recovering => {
                Some((LogPoint::Progress, Kind::Progress))
            }
            Point::Finish if protections.finish => Some((LogPoint::Finish, Kind::Finish)),
            Point::Restart if protections.restart => Some((LogPoint::Recovery, Kind::Restart)),
            Point::Seal if floor => Some((LogPoint::Floor, Kind::Floor)),
            _ => None,
        }
    }
}

impl TurnDurability for TurnLogPolicy {
    fn event_id(&mut self, scope: &str, _state: &mut ScopeState) -> Option<EventId> {
        let log = self.log(scope);
        let (floor, seq) = crate::facts::writing(log, |writing| {
            (writing.floor_pending(), writing.next_event())
        });
        if floor {
            self.unsealed.insert(scope.to_owned());
        }
        Some((0, seq))
    }

    fn load(&mut self, _log: &mut dyn Log, scope: &str) -> ScopeState {
        let whole = self.whole();
        let log = refused(ready(TurnLog::open(Arc::clone(&self.host), scope)))
            .retain_finished(self.retain)
            .row_limit(self.limit)
            .chain_bytes_max(self.chain_bytes)
            .snapshot_ratio(if self.ratio_ignored {
                u32::MAX as usize
            } else {
                self.ratio
            })
            .snapshot_rows(self.rows)
            .whole_checkpoints(whole);
        let owned = scope.to_owned();
        let (state, revision) = refused(ready(log.read_owner(scope, move |fold| {
            fold.checkpoint(&owned).map_or_else(
                || (ScopeState::default(), 0),
                |checkpoint| {
                    (
                        ScopeState::restore(&checkpoint.payload),
                        checkpoint.revision,
                    )
                },
            )
        })));
        let part = format!("{scope}:part");
        let read = part.clone();
        let written = refused(ready(log.read_owner(&part, move |fold| {
            fold.checkpoint(&read)
                .map(|checkpoint| checkpoint.payload.clone())
        })));
        let expected = part_revision(revision);
        assert!(
            written == expected.map(|expected| part_value(scope, expected)),
            "{part} reads {written:?} after a restart at revision {revision}, which committed revision {expected:?}"
        );
        if let Some(owners) = self.owners {
            for owner in 0..owners {
                let name = format!("{scope}:o{owner}");
                let owned = name.clone();
                let read = refused(ready(log.read_owner(&name, move |fold| {
                    (
                        fold.checkpoint(&owned)
                            .map(|checkpoint| checkpoint.payload.clone()),
                        fold.stored(&format!("{owned}\nlast")).map(str::to_owned),
                    )
                })));
                let expected = (1..=revision)
                    .rev()
                    .find(|written| owners_written(*written, owners).any(|other| other == owner))
                    .map(|written| owner_value(scope, written));
                assert!(
                    read == (expected.clone(), expected.clone()),
                    "{name} reads {read:?} after a restart at revision {revision}, which committed {expected:?}"
                );
            }
        }
        let events = refused(ready(log.history(0, Vec::new(), |events, event| {
            events.push((Some((0, event.seq)), text(&event.event)));
            true
        })));
        let mut state = state;
        state.events = events;
        let contents = format!("{scope}:gc");
        let owned = scope.to_owned();
        let kept = refused(ready(log.read_owner(&contents, move |fold| {
            contents_kept(fold, &owned, "after a restart")
        })));
        self.cached
            .insert(scope.to_owned(), kept.into_iter().collect());
        self.logs.insert(scope.to_owned(), log);
        state
    }

    fn point(
        &mut self,
        _log: &mut dyn Log,
        scope: &str,
        point: Point,
        state: &mut ScopeState,
    ) -> Option<Appended> {
        let log = self.log(scope);
        refused(ready(log.read_owners(
            &[scope.to_owned(), format!("{scope}:gc")],
            |_| (),
        )));
        let owned = scope.to_owned();
        let (recovering, floor, revision) = crate::facts::writing(log, move |writing| {
            let fold = writing.fold();
            (
                fold.recovering(),
                writing.floor_pending(),
                fold.checkpoint(&owned)
                    .map_or(0, |checkpoint| checkpoint.revision),
            )
        });
        let (log_point, kind) = self.commit_point(point, recovering, floor)?;
        let log = &self.logs[scope];
        let before = ready(log.stats());
        let contents = format!("{scope}:gc");
        let held = contents.clone();
        let (stated, next) = ready(log.read(move |fold| {
            (
                fold.host("state").is_some(),
                fold.checkpoint(&held)
                    .map_or(0, |checkpoint| checkpoint.revision)
                    + 1,
            )
        }));
        let mut first = Vec::new();
        if let Some(bytes) = self.state
            && !stated
        {
            first.push(LogRecord::Host {
                key: "state".to_owned(),
                value: raw(&padding(scope, bytes)),
            });
        }
        let mut checkpoint = state.clone();
        checkpoint.events.clear();
        first.push(LogRecord::Checkpoint {
            payload: checkpoint.snapshot(),
            revision: revision + 1,
            state: scope.to_owned(),
        });
        let part = (part_revision(revision + 1) == Some(revision + 1)).then(|| {
            let part = format!("{scope}:part");
            let record = LogRecord::Checkpoint {
                payload: part_value(scope, revision + 1),
                revision: revision + 1,
                state: part.clone(),
            };
            (part, record)
        });
        let mut then = Vec::new();
        if let Some(owners) = self.owners {
            for owner in owners_written(revision + 1, owners) {
                let name = format!("{scope}:o{owner}");
                let value = owner_value(scope, revision + 1);
                then.push(LogRecord::Stored {
                    key: format!("{name}\nlast"),
                    value: value.clone(),
                });
                then.push(LogRecord::Checkpoint {
                    payload: value,
                    revision: revision + 1,
                    state: name,
                });
            }
        }
        let top = next * 5 % 13 + 1;
        let cached = self.cached.entry(scope.to_owned()).or_default();
        let ids = chain(top);
        let mut fresh = BTreeSet::new();
        let mut pushed = BTreeSet::new();
        let again = ids
            .iter()
            .copied()
            .find(|id| !self.collect_live && cached.contains(id));
        for id in ids {
            if cached.insert(id) {
                let key = format!("{contents}\n{}", content(id));
                fresh.insert(key.clone());
                pushed.insert(key.clone());
                then.push(LogRecord::Stored {
                    key,
                    value: content_value(scope, id),
                });
            }
        }
        if let Some(id) = again {
            pushed.insert(format!("{contents}\n{}", content(id)));
            let value = content_value(scope, id);
            then.push(LogRecord::Stored {
                key: format!("{contents}\n{}", content(id)),
                value: if self.rewrite_stale {
                    value.replacen("content", "stale content", 1)
                } else {
                    value
                },
            });
        }
        let collect = self.collect_live;
        let last = LogRecord::Checkpoint {
            payload: serde_json::to_string(&[content(top)])
                .unwrap_or_else(|error| panic!("a content list does not encode: {error}")),
            revision: next,
            state: contents.clone(),
        };
        let viewed = self.rows.map(|_| self.disk.with(|rows| rows.view(scope)));
        let end = self
            .disk
            .with(|rows| rows.view(scope).last().map_or(0, |(seq, _)| *seq));
        let retention = self.retention;
        let tracked = self.rows.is_some();
        let owned = scope.to_owned();
        let (seq, live) = crate::facts::writing(log, move |writing| {
            let scope = owned.as_str();
            for record in first {
                refused(writing.push(record));
            }
            if let Some((part, record)) = part {
                refused(ready(writing.load_owner(&part)));
                refused(writing.push(record));
            }
            for record in then {
                refused(writing.push(record));
            }
            if collect {
                writing.collect_unreachable(&contents);
            }
            refused(writing.push(last));
            let seq = refused(ready(writing.commit(log_point)));
            let keys = content_keys(writing.fold(), scope);
            refused(ready(writing.load_owners(&keys)));
            contents_kept(writing.fold(), scope, &format!("after commit {seq}"));
            for turn in writing.fold().turns() {
                assert!(
                    !matches!(turn.phase, Phase::Finished { .. }) || turn.prompt.get() == "null",
                    "{scope} keeps the prompt of finished turn {} in the fold",
                    turn.id
                );
            }
            if let Some(keep) = retention {
                let finished = writing
                    .fold()
                    .turns()
                    .iter()
                    .filter(|turn| matches!(turn.phase, Phase::Finished { .. }))
                    .count();
                assert!(
                    finished <= keep,
                    "{finished} finished turns stay in the fold past a retention of {keep}"
                );
            }
            let attempts = |fold: &nanocode_turnlog::Fold| {
                (
                    fold.recovering(),
                    fold.turns()
                        .iter()
                        .filter(|turn| !matches!(turn.phase, Phase::Finished { .. }))
                        .map(|turn| (turn.id.clone(), fold.next_attempt(&turn.id)))
                        .collect::<Vec<_>>(),
                )
            };
            (seq, tracked.then(|| attempts(writing.fold())))
        });
        written_once(&self.disk, scope, end, seq, &fresh, &pushed);
        if let Some(viewed) = viewed {
            let stored = self.disk.with(|rows| rows.view(scope));
            if let ([.., (row, previous)], [.., (_, written)]) =
                (viewed.as_slice(), stored.as_slice())
                && stored.len() == viewed.len() + 1
                && written.starts_with('r')
                && nanocode_turnlog::extensible(previous)
                && self
                    .limit
                    .is_none_or(|limit| previous.len() - SEAL + written.len() - 1 <= limit)
            {
                let start = stored
                    .iter()
                    .rposition(|(_, body)| body.starts_with('s'))
                    .unwrap_or(0);
                let decoded = nanocode_turnlog::rows(&stored[start..])
                    .unwrap_or_else(|error| panic!("a turn log row does not decode: {error}"));
                let plain = |seq: u64| {
                    decoded
                        .iter()
                        .find(|stored| stored.seq == seq)
                        .map(|stored| stored.plain.clone())
                        .unwrap_or_default()
                };
                assert!(
                    recovers(&plain(*row)) || recovers(&plain(seq)),
                    "{scope} commit {seq} wrote a new row after row {row}, which it could extend"
                );
            }
        }
        let part = format!("{scope}:part");
        let held = part.clone();
        let read = move |fold: &Fold| {
            fold.checkpoint(&held)
                .map(|checkpoint| checkpoint.payload.clone())
        };
        let written = if self.read_unloaded {
            ready(log.read(read))
        } else {
            refused(ready(log.read_owner(&part, read)))
        };
        let expected = part_revision(revision + 1);
        assert!(
            written == expected.map(|expected| part_value(scope, expected)),
            "{part} reads {written:?} after commit {seq} at revision {}, which committed revision {expected:?}",
            revision + 1
        );
        if let Some(live) = live {
            let reads = self.disk.with(|rows| rows.reads);
            let reopened = refused(ready(TurnLog::open(Arc::clone(&self.host), scope)));
            self.disk.with(|rows| rows.reads = reads);
            let ids = live.1.clone();
            let loaded = ready(reopened.read(move |fold| {
                (
                    fold.recovering(),
                    ids.iter()
                        .map(|(id, _)| (id.clone(), fold.next_attempt(id)))
                        .collect::<Vec<_>>(),
                )
            }));
            assert!(
                (loaded.0, &loaded.1) == (live.0, &live.1),
                "{scope} reloads recovering {} and attempts {:?} after commit {seq}, but its writer holds {} and {:?}",
                loaded.0,
                loaded.1,
                live.0,
                live.1
            );
        }
        self.unsealed.remove(scope);
        let after = ready(log.stats());
        let parts = after.parts - before.parts;
        if let Some(bound) = self.bound {
            let loaded = self.disk.with(|rows| {
                let reads = rows.reads;
                let loaded = rows.tload(scope).len();
                rows.reads = reads;
                loaded
            });
            assert!(
                loaded as u64 <= bound as u64 + parts + 1,
                "a cold open of {scope} reads {loaded} rows past a snapshot row bound of {bound}"
            );
        }
        let stored = self.disk.with(|rows| rows.view(scope));
        if !stored.iter().any(|(_, body)| body.starts_with('s')) {
            let scratch = Disk::new();
            scratch.with(|rows| rows.insert(scope, &stored));
            let host: Arc<dyn LogHost> = Arc::new(SimHost::new(Arc::clone(&scratch)));
            let forced = refused(ready(TurnLog::open(host, scope)))
                .retain_finished(self.retain)
                .row_limit(self.limit)
                .chain_bytes_max(self.chain_bytes)
                .snapshot_ratio(self.ratio)
                .snapshot_rows(Some(0));
            crate::facts::writing(&forced, move |writing| {
                refused(writing.push(LogRecord::Host {
                    key: "forced".to_owned(),
                    value: raw("0"),
                }));
                refused(ready(writing.commit(log_point)));
            });
            let forced = scratch.with(|rows| rows.view(scope));
            let end = stored.last().map_or(0, |(seq, _)| *seq);
            let written = forced
                .iter()
                .filter(|(seq, _)| *seq > end)
                .map(|(_, body)| body.len())
                .sum::<usize>();
            let mut section = false;
            let tail = stored
                .iter()
                .filter(|(_, body)| {
                    if !body.starts_with('+') {
                        section = body.starts_with('p');
                    }
                    !section
                })
                .map(|(_, body)| body.len())
                .sum::<usize>();
            let last = stored
                .iter()
                .rposition(|(_, body)| !body.starts_with('+'))
                .map_or(0, |start| {
                    stored[start..]
                        .iter()
                        .map(|(_, body)| body.len())
                        .sum::<usize>()
                });
            assert!(
                tail - last <= self.ratio * written,
                "{scope} replays {tail} bytes after commit {seq} without a snapshot row, {last} of them in its last row, past {} times the {written} bytes a snapshot writes",
                self.ratio
            );
        }
        if after.snapshots > before.snapshots {
            let stored = self.disk.with(|rows| rows.view(scope));
            crate::stall::bounded(scope, &stored, seq);
            crate::resident::probe(scope, &stored, seq, self.limit, self.ratio);
            let referenced = referenced(&stored);
            for (row, body) in &stored {
                assert!(
                    !body.starts_with('p') || referenced.iter().any(|range| range.contains(row)),
                    "{scope} keeps section row {row} after commit {seq}, which its head does not reference"
                );
            }
        }
        if self.state.is_some() && self.single {
            let snapshots = self.disk.with(|rows| {
                rows.view(scope)
                    .iter()
                    .filter(|(_, body)| body.starts_with('s'))
                    .count()
            });
            assert!(
                snapshots <= 1,
                "{scope} stores {snapshots} snapshot rows, so a superseded snapshot was not archived"
            );
        }
        if after.snapshots > before.snapshots {
            self.take_over(scope, seq);
        } else if let Some(follower) = self.followers.get_mut(scope) {
            refused(ready(follower.advance()));
        }
        Some(Appended {
            events: Vec::new(),
            kind,
            parts,
            seq,
            snapshot: after.snapshots > before.snapshots,
        })
    }

    fn record(&mut self, scope: &str, record: Record) {
        for translated in self.records(scope, record) {
            self.push(scope, translated);
        }
    }

    fn restart(&mut self, scope: &str, state: &ScopeState) -> Decision {
        if !self.protections.restart {
            return Decision::Resume(None);
        }
        let request = state
            .active
            .as_ref()
            .expect("a restart decision is asked for an active turn")
            .request
            .clone();
        let log = self.log(scope);
        let wanted = request.clone();
        let attempt = ready(log.read(move |fold| fold.next_attempt(&wanted)));
        if attempt > RESTART_CAP {
            return Decision::Fail;
        }
        self.restarting.insert(scope.to_owned(), request);
        Decision::Resume(Some(Record::Restart { attempt, floor: 0 }))
    }

    fn unsealed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unsealed).into_iter().collect()
    }
}
