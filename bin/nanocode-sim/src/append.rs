use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Range,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use nanocode_durability_core::{
    Migrated, Sql, SqlDialect, SqlRow, SqlStatement, StoreError, StoreFuture,
};
use nanocode_sync::{Atom, block_on};
use nanocode_turnlog::{
    Format, LogHost, Point, Record, SqlLog, TurnLog,
    sim::{self as log_sim, Mutation as Rule},
};

use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
};

mod queue;

const COMMITS: Range<u64> = 1..8;
const LOST_MAX: u64 = 5;
const VALUE_BYTES: Range<u64> = 8..3_000;
const ROW_LIMIT_BYTES: usize = 1 << 10;
const ROW_LIMIT_PPM: u64 = 500_000;
const LANDS_PPM: u64 = 600_000;
const LATER_PPM: u64 = 500_000;
const READ_FAILS_PPM: u64 = 400_000;
const CHANGED_PPM: u64 = 150_000;
const STAMPED_PPM: u64 = 500_000;
const AFTER_RETRY_PPM: u64 = 300_000;
const READ_BACK_CALLS: u8 = 1;
const RETRY_CALLS: u8 = 2;
const SNAPSHOT_RATIO_NEVER: usize = u32::MAX as usize;
const HEX: &[u8; 16] = b"0123456789abcdef";
const LOG_INSERT: &str = "INSERT INTO nanocode_log (scope, seq, body";
const LOG_INSERT_DATA: &str = "INSERT INTO nanocode_log (scope, seq, body, data)";
const ON_CONFLICT: &str = "ON CONFLICT (scope, seq) DO NOTHING";
const INSERTED_GUARD: &str = "SELECT CAST(concat(";
const ROWS_READ: &str = "SELECT CAST(seq AS TEXT)";
const SCAN_BOUND: &str = "seq < CAST($3";
const LOAD_SNAPSHOT: &str = "COALESCE(";
const ARCHIVE_PLAIN: &str = "UPDATE nanocode_log SET body = $3::text, data = NULL";
const ARCHIVE_DATA: &str = "UPDATE nanocode_log SET body = $3::text, data = decode(";
const DROP_ROWS: &str = "DELETE FROM nanocode_log WHERE";
const CHECKPOINT_TABLE: &str = "nanocode_checkpoint";
const CHECKPOINT_READ: &str = "SELECT CAST(part AS TEXT)";
const READ_FORMAT: &str = "FROM nanocode_formats WHERE component";
const WRITE_FORMAT: &str = "INSERT INTO nanocode_formats";
const FORMATS_PRESENT: &str = "to_regclass('nanocode_formats')";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendMutation {
    None,
    RetrySkipsReadBack,
    ConflictSkipsReadBack,
    AbandonRunningJob,
    OwnRowFences,
    GroupCommitOff,
    QueueDeadlineFixed,
    QueueRunsDropped,
    QueuedUntyped,
    QueueDepthUncounted,
}

impl AppendMutation {
    const ALL: [Self; 10] = [
        Self::None,
        Self::RetrySkipsReadBack,
        Self::ConflictSkipsReadBack,
        Self::AbandonRunningJob,
        Self::OwnRowFences,
        Self::GroupCommitOff,
        Self::QueueDeadlineFixed,
        Self::QueueRunsDropped,
        Self::QueuedUntyped,
        Self::QueueDepthUncounted,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RetrySkipsReadBack => "retry_skips_read_back",
            Self::ConflictSkipsReadBack => "conflict_skips_read_back",
            Self::AbandonRunningJob => "abandon_running_job",
            Self::OwnRowFences => "own_row_fences",
            Self::GroupCommitOff => "group_commit_off",
            Self::QueueDeadlineFixed => "queue_deadline_fixed",
            Self::QueueRunsDropped => "queue_runs_dropped",
            Self::QueuedUntyped => "queued_untyped",
            Self::QueueDepthUncounted => "queue_depth_uncounted",
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn rule(self) -> Rule {
        match self {
            Self::None => Rule::None,
            Self::RetrySkipsReadBack => Rule::RetrySkipsReadBack,
            Self::ConflictSkipsReadBack => Rule::ConflictSkipsReadBack,
            Self::AbandonRunningJob => Rule::AbandonRunningJob,
            Self::OwnRowFences => Rule::OwnRowFences,
            Self::GroupCommitOff => Rule::GroupCommitOff,
            Self::QueueDeadlineFixed => Rule::QueueDeadlineFixed,
            Self::QueueRunsDropped => Rule::QueueRunsDropped,
            Self::QueuedUntyped => Rule::QueuedUntyped,
            Self::QueueDepthUncounted => Rule::QueueDepthUncounted,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AppendStats {
    pub seeds: u64,
    pub commits: u64,
    pub answers_lost: u64,
    pub landed_unanswered: u64,
    pub settled_by_read_back: u64,
    pub settled_changed: u64,
    pub late_landings: u64,
    pub stamped_seeds: u64,
    pub dropped_before: u64,
    pub refused_conflicts: u64,
    pub abandoned_in_flight: u64,
    pub multi_row_appends: u64,
    pub grouped_appends: u64,
    pub queued_refusals: u64,
}

impl AppendStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.commits += other.commits;
        self.answers_lost += other.answers_lost;
        self.landed_unanswered += other.landed_unanswered;
        self.settled_by_read_back += other.settled_by_read_back;
        self.settled_changed += other.settled_changed;
        self.late_landings += other.late_landings;
        self.stamped_seeds += other.stamped_seeds;
        self.dropped_before += other.dropped_before;
        self.refused_conflicts += other.refused_conflicts;
        self.abandoned_in_flight += other.abandoned_in_flight;
        self.multi_row_appends += other.multi_row_appends;
        self.grouped_appends += other.grouped_appends;
        self.queued_refusals += other.queued_refusals;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lands {
    Now,
    After(u8),
    Never,
}

#[derive(Clone, Copy, Debug)]
struct Fault {
    lands: Lands,
    read_fails: bool,
}

type Rows = BTreeMap<(String, u64), String>;

#[derive(Clone)]
struct Flight {
    before: Rows,
    after: Rows,
    calls: u8,
    late: bool,
}

#[derive(Clone, Default)]
struct Database {
    migrating: bool,
    formats: BTreeMap<String, String>,
    rows: Rows,
    faults: VecDeque<Fault>,
    flight: Option<Flight>,
    late: BTreeSet<(String, u64)>,
    late_landings: u64,
    read_fails: bool,
    pause_next: bool,
    appends: u64,
    landed_unanswered: u64,
    multi_row_appends: u64,
    delay: Option<Duration>,
    duplicates: u64,
    conflicts: u64,
}

fn unknown() -> StoreError {
    StoreError::Backend(
        "Postgres did not answer within the statement deadline; its outcome is unknown".to_owned(),
    )
}

fn param<'a>(statement: &SqlStatement<'a>, at: usize) -> &'a str {
    statement
        .params
        .get(at)
        .copied()
        .unwrap_or_else(|| panic!("{} has no param {at}", statement.sql))
}

fn number(text: &str) -> u64 {
    text.parse()
        .unwrap_or_else(|error| panic!("the turn log sent seq {text}: {error}"))
}

fn scope_rows(rows: &Rows, scope: &str, from: u64, to: u64) -> Vec<SqlRow> {
    rows.range((scope.to_owned(), from)..(scope.to_owned(), to))
        .map(|((_, seq), body)| vec![Some(seq.to_string()), Some(body.clone())])
        .collect()
}

fn written_body(statement: &SqlStatement<'_>, data: bool) -> String {
    if data {
        format!(
            "{}{}",
            param(statement, 2),
            param(statement, 3).trim_end_matches('=')
        )
    } else {
        param(statement, 2).to_owned()
    }
}

impl Database {
    fn land(&mut self) {
        match self.flight.take() {
            Some(flight) if flight.calls == 0 && flight.late && self.rows != flight.before => {}
            Some(flight) if flight.calls == 0 => {
                assert!(
                    self.rows == flight.before,
                    "no write lands between an unanswered append and its landing"
                );
                if flight.late {
                    self.late_landings += 1;
                    self.late.extend(
                        flight
                            .after
                            .keys()
                            .filter(|key| !flight.before.contains_key(*key))
                            .cloned(),
                    );
                }
                self.rows = flight.after;
            }
            Some(flight) => {
                self.flight = Some(Flight {
                    calls: flight.calls - 1,
                    ..flight
                });
            }
            None => {}
        }
    }

    fn migrate(&mut self, statements: &[SqlStatement<'_>]) -> Vec<Vec<SqlRow>> {
        statements
            .iter()
            .map(|statement| {
                if statement.sql.contains(FORMATS_PRESENT) {
                    vec![vec![Some("1".to_owned())]]
                } else if statement.sql.contains(READ_FORMAT) {
                    self.formats
                        .get(param(statement, 0))
                        .map_or_else(Vec::new, |version| vec![vec![Some(version.clone())]])
                } else if statement.sql.starts_with(WRITE_FORMAT) {
                    self.formats.insert(
                        param(statement, 0).to_owned(),
                        param(statement, 1).to_owned(),
                    );
                    Vec::new()
                } else {
                    Vec::new()
                }
            })
            .collect()
    }

    fn read(&mut self, statement: &SqlStatement<'_>) -> Result<Vec<SqlRow>, StoreError> {
        if std::mem::take(&mut self.read_fails) {
            return Err(unknown());
        }
        let scope = param(statement, 0);
        if statement.sql.contains(SCAN_BOUND) {
            let (from, to) = (number(param(statement, 1)), number(param(statement, 2)));
            return Ok(scope_rows(&self.rows, scope, from, to));
        }
        assert!(
            statement.sql.contains(LOAD_SNAPSHOT),
            "the append model reads only scans and loads, not {}",
            statement.sql
        );
        let from = self
            .rows
            .range((scope.to_owned(), 0)..=(scope.to_owned(), u64::MAX))
            .rev()
            .find(|(_, body)| body.starts_with('s'))
            .map_or(0, |((_, seq), _)| *seq);
        Ok(scope_rows(&self.rows, scope, from, u64::MAX))
    }

    fn write(
        &mut self,
        rows: &mut Rows,
        ours: &mut BTreeSet<u64>,
        statement: &SqlStatement<'_>,
    ) -> Result<Vec<SqlRow>, StoreError> {
        let sql = statement.sql;
        if sql.starts_with(LOG_INSERT) {
            let key = (param(statement, 0).to_owned(), number(param(statement, 1)));
            let body = written_body(statement, sql.starts_with(LOG_INSERT_DATA));
            if rows.contains_key(&key) && self.late.contains(&key) {
                if !sql.contains(ON_CONFLICT) {
                    return Err(StoreError::Backend(
                        "duplicate key value violates unique constraint".to_owned(),
                    ));
                }
                return Ok(Vec::new());
            }
            if rows.contains_key(&key) {
                if !sql.contains(ON_CONFLICT) {
                    self.duplicates += 1;
                    return Err(StoreError::Backend(
                        "duplicate key value violates unique constraint".to_owned(),
                    ));
                }
                self.conflicts += 1;
                return Ok(Vec::new());
            }
            ours.insert(key.1);
            let seq = key.1;
            rows.insert(key, body);
            return Ok(vec![vec![Some(seq.to_string())]]);
        }
        if sql.starts_with(INSERTED_GUARD) {
            let written = param(statement, 1)
                .trim_matches(|c| c == '{' || c == '}')
                .split(',')
                .map(number)
                .filter(|seq| ours.contains(seq))
                .count();
            if written as u64 != number(param(statement, 2)) {
                return Err(StoreError::Backend(
                    "invalid input syntax for type bigint".to_owned(),
                ));
            }
            return Ok(Vec::new());
        }
        if sql.starts_with(ARCHIVE_PLAIN) || sql.starts_with(ARCHIVE_DATA) {
            let key = (param(statement, 0).to_owned(), number(param(statement, 1)));
            let body = written_body(statement, sql.starts_with(ARCHIVE_DATA));
            if let Some(stored) = rows.get_mut(&key) {
                *stored = body;
            }
            return Ok(Vec::new());
        }
        if sql.starts_with(DROP_ROWS) {
            let scope = param(statement, 0).to_owned();
            let dropped = number(param(statement, 1))..number(param(statement, 2));
            rows.retain(|(at, seq), _| *at != scope || !dropped.contains(seq));
            return Ok(Vec::new());
        }
        assert!(
            sql.contains(CHECKPOINT_TABLE),
            "the append model does not know {sql}"
        );
        Ok(Vec::new())
    }

    fn run(&mut self, statements: &[SqlStatement<'_>]) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        self.land();
        if self.migrating {
            return Ok(self.migrate(statements));
        }
        if statements
            .iter()
            .all(|statement| statement.sql.starts_with(ROWS_READ))
        {
            return statements
                .iter()
                .map(|statement| self.read(statement))
                .collect();
        }
        if statements
            .iter()
            .all(|statement| statement.sql.starts_with(CHECKPOINT_READ))
        {
            return Ok(vec![Vec::new(); statements.len()]);
        }
        let inserts = statements
            .iter()
            .filter(|statement| statement.sql.starts_with(LOG_INSERT))
            .count();
        let mut rows = self.rows.clone();
        let mut ours = BTreeSet::new();
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.write(&mut rows, &mut ours, statement)?);
        }
        if inserts == 0 {
            self.rows = rows;
            return Ok(results);
        }
        self.appends += 1;
        if inserts > 1 {
            self.multi_row_appends += 1;
        }
        let Some(fault) = self.faults.pop_front() else {
            self.rows = rows;
            return Ok(results);
        };
        self.read_fails = fault.read_fails;
        match fault.lands {
            Lands::Now => {
                self.landed_unanswered += 1;
                self.rows = rows;
            }
            Lands::After(calls) => {
                self.landed_unanswered += 1;
                self.flight = Some(Flight {
                    before: self.rows.clone(),
                    after: rows,
                    calls,
                    late: calls > READ_BACK_CALLS,
                });
            }
            Lands::Never => {}
        }
        Err(unknown())
    }
}

struct Model {
    database: Atom<Database>,
}

impl Sql for Model {
    fn transaction<'a>(
        &'a self,
        _operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        let (answer, pause, delay) = self.database.update(|database| {
            let inserts = statements
                .iter()
                .filter(|statement| statement.sql.starts_with(LOG_INSERT))
                .count();
            let pause = inserts > 0 && std::mem::take(&mut database.pause_next);
            (database.run(statements), pause, database.delay)
        });
        if let Some(delay) = delay {
            return Box::pin(async move {
                nanocode_platform::sleep(delay).await;
                answer
            });
        }
        let mut paused = pause;
        let mut answer = Some(answer);
        Box::pin(std::future::poll_fn(move |context| {
            if std::mem::take(&mut paused) {
                context.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            std::task::Poll::Ready(
                answer
                    .take()
                    .unwrap_or_else(|| panic!("an append transaction was polled after ready")),
            )
        }))
    }

    fn call_bound(&self) -> Option<Duration> {
        None
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
}

struct World {
    model: Arc<Model>,
    host: Arc<dyn LogHost>,
    row_limit: Option<usize>,
    stats: AppendStats,
}

fn value(rng: &mut Rng) -> String {
    let bytes = rng.range(VALUE_BYTES.start, VALUE_BYTES.end);
    let text = (0..bytes)
        .map(|_| char::from(HEX[rng.below(HEX.len() as u64) as usize]))
        .collect::<String>();
    format!("\"{text}\"")
}

fn host_record(key: &str, value: &str) -> Record {
    Record::Host {
        key: key.to_owned(),
        value: serde_json::value::RawValue::from_string(value.to_owned())
            .unwrap_or_else(|error| panic!("a host value is JSON: {error}")),
    }
}

async fn push(log: &TurnLog, key: &str, value: &str) -> Result<(), String> {
    let record = host_record(key, value);
    log.write(move |writing| Box::pin(std::future::ready(writing.push(record))))
        .await
        .map_err(|error| format!("push {key}: {error}"))
}

async fn commit(log: &TurnLog) -> Result<u64, StoreError> {
    log.write(|writing| Box::pin(writing.commit(Point::Progress)))
        .await
}

impl World {
    async fn open(&self, scope: &str) -> Result<TurnLog, StoreError> {
        Ok(nanocode_turnlog::sim::open(Arc::clone(&self.host), scope)
            .await?
            .row_limit(self.row_limit)
            .snapshot_ratio(SNAPSHOT_RATIO_NEVER))
    }

    async fn claim(&self, scope: &str) -> Result<TurnLog, StoreError> {
        let follower = nanocode_turnlog::sim::follow(Arc::clone(&self.host), scope).await?;
        Ok(nanocode_turnlog::sim::claim(follower)
            .await?
            .row_limit(self.row_limit)
            .snapshot_ratio(SNAPSHOT_RATIO_NEVER))
    }

    fn stored(&self, scope: &str) -> Vec<(u64, String)> {
        self.model.database.read(|database| {
            database
                .rows
                .range((scope.to_owned(), 0)..=(scope.to_owned(), u64::MAX))
                .map(|((_, seq), body)| (*seq, body.clone()))
                .collect()
        })
    }

    fn next_seq(&self, scope: &str) -> u64 {
        self.stored(scope).last().map_or(1, |(seq, _)| seq + 1)
    }

    fn holding(&self, scope: &str, key: &str) -> Result<Vec<u64>, String> {
        let rows = nanocode_turnlog::rows(&self.stored(scope))
            .map_err(|error| format!("the rows of {scope} do not decode: {error}"))?;
        let mut seqs = Vec::new();
        for row in rows.iter().filter(|row| !row.snapshot) {
            let records = nanocode_turnlog::decode(&row.plain)
                .map_err(|error| format!("row {} of {scope} does not decode: {error}", row.seq))?;
            seqs.extend(
                records
                    .iter()
                    .filter(
                        |record| matches!(record, Record::Host { key: held, .. } if held == key),
                    )
                    .map(|_| row.seq),
            );
        }
        Ok(seqs)
    }

    fn faults(&self, faults: &VecDeque<Fault>) {
        self.model.database.update(|database| {
            database.faults.clone_from(faults);
            database.read_fails = false;
        });
    }

    fn appends(&self) -> u64 {
        self.model.database.read(|database| database.appends)
    }

    fn plan(rng: &mut Rng, lost: u64) -> VecDeque<Fault> {
        (0..lost)
            .map(|_| Fault {
                lands: if !rng.chance(LANDS_PPM) {
                    Lands::Never
                } else if rng.chance(LATER_PPM) {
                    Lands::After(if rng.chance(AFTER_RETRY_PPM) {
                        RETRY_CALLS
                    } else {
                        READ_BACK_CALLS
                    })
                } else {
                    Lands::Now
                },
                read_fails: rng.chance(READ_FAILS_PPM),
            })
            .collect()
    }

    async fn retried(
        &mut self,
        rng: &mut Rng,
        log: &TurnLog,
        key: &str,
        lost: u64,
    ) -> Result<(Result<u64, StoreError>, bool), String> {
        let mut changed = false;
        for attempt in 0..=lost {
            let sent = self.appends();
            match commit(log).await {
                Ok(seq) => {
                    if self.appends() == sent {
                        self.stats.settled_by_read_back += 1;
                    }
                    return Ok((Ok(seq), changed));
                }
                Err(StoreError::Fenced) => {
                    return Err(format!(
                        "{key} was refused as Fenced with no other writer: the writer read its own row as a stranger's"
                    ));
                }
                Err(_) if attempt < lost => {
                    if !changed && rng.chance(CHANGED_PPM) {
                        changed = true;
                        push(log, &format!("{key}-changed"), &value(rng)).await?;
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "{key} still answered {error} after {lost} lost answers"
                    ));
                }
            }
        }
        Err(format!("{key} made no attempt"))
    }

    fn store_clean(&self, key: &str) -> Result<(), String> {
        let (duplicates, conflicts) = self
            .model
            .database
            .read(|database| (database.duplicates, database.conflicts));
        if duplicates > 0 {
            return Err(format!(
                "{duplicates} appends reached the store as duplicate-key errors"
            ));
        }
        if conflicts > 0 {
            return Err(format!(
                "{key} sent {conflicts} inserts onto rows an earlier attempt of its own had landed, without reading them back"
            ));
        }
        Ok(())
    }

    async fn lost_answers(&mut self, rng: &mut Rng) -> Result<(), String> {
        let scope = "scope-lost";
        let log = self
            .claim(scope)
            .await
            .map_err(|error| format!("claim: {error}"))?;
        for commit in 0..rng.range(COMMITS.start, COMMITS.end) {
            let key = format!("key-{commit}");
            let held = value(rng);
            push(&log, &key, &held).await?;
            let lost = rng.range(1, LOST_MAX + 1);
            self.stats.answers_lost += lost;
            self.faults(&Self::plan(rng, lost));
            let expected = self.next_seq(scope);
            let (answer, changed) = self.retried(rng, &log, &key, lost).await?;
            self.faults(&VecDeque::new());
            self.stats.commits += 1;
            let Ok(seq) = answer else {
                return Err(format!("{key} answered {answer:?}"));
            };
            self.settled(&log, scope, &key, &held, expected, seq, changed)
                .await?;
            self.store_clean(&key)?;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one commit's key, value, first seq, answer and change are what the check compares"
    )]
    async fn settled(
        &mut self,
        log: &TurnLog,
        scope: &str,
        key: &str,
        held: &str,
        expected: u64,
        seq: u64,
        changed: bool,
    ) -> Result<(), String> {
        let holding = self.holding(scope, key)?;
        if holding != [expected] {
            return Err(format!(
                "{key} first wrote at seq {expected} but is held by rows {holding:?}"
            ));
        }
        if changed {
            let moved = self.holding(scope, &format!("{key}-changed"))?;
            if moved != [seq] {
                return Err(format!(
                    "{key}-changed answered seq {seq} but is held by rows {moved:?}"
                ));
            }
            if seq != expected {
                self.stats.settled_changed += 1;
            }
        } else if seq != expected {
            return Err(format!(
                "{key} answered seq {seq}, its first attempt wrote at {expected}"
            ));
        }
        let wanted = key.to_owned();
        let folded = log
            .read(move |fold| fold.host(&wanted).map(|raw| raw.get().to_owned()))
            .await;
        if folded.as_deref() != Some(held) {
            return Err(format!(
                "{key} landed but the writer's fold holds {folded:?}"
            ));
        }
        let next = self.next_seq(scope);
        let last = commit(log)
            .await
            .map_err(|error| format!("a commit after {key}: {error}"))?;
        if last != next {
            return Err(format!(
                "after {key} the writer's next commit answered seq {last}, not {next}, the seq past every landed row"
            ));
        }
        self.stamped(scope)
    }

    fn stamped(&self, scope: &str) -> Result<(), String> {
        let rows = nanocode_turnlog::rows(&self.stored(scope))
            .map_err(|error| format!("the rows of {scope} do not decode: {error}"))?;
        let mut writers = BTreeSet::new();
        let mut last = None;
        for row in &rows {
            let Some(stamp) = row.stamp else {
                continue;
            };
            writers.insert(stamp.writer());
            if last.is_some_and(|last| stamp.op() <= last) {
                return Err(format!(
                    "row {} of {scope} carries op {} after op {last:?}: an op landed twice or went back",
                    row.seq,
                    stamp.op()
                ));
            }
            last = Some(stamp.op());
        }
        if writers.len() > 1 {
            return Err(format!(
                "the rows of {scope} carry {} writer ids from one writer",
                writers.len()
            ));
        }
        Ok(())
    }

    async fn conflicting(&mut self, rng: &mut Rng) -> Result<(), String> {
        let scope = "scope-conflict";
        let first = self
            .open(scope)
            .await
            .map_err(|error| format!("open: {error}"))?;
        let second = self
            .open(scope)
            .await
            .map_err(|error| format!("open: {error}"))?;
        push(&first, "first", &value(rng)).await?;
        let seq = commit(&first)
            .await
            .map_err(|error| format!("the first writer's commit: {error}"))?;
        push(&second, "second", &value(rng)).await?;
        match commit(&second).await {
            Err(StoreError::Fenced) => self.stats.refused_conflicts += 1,
            answer => {
                return Err(format!(
                    "a second writer at the seq the first holds answered {answer:?}"
                ));
            }
        }
        if self.holding(scope, "first")? != [seq] || !self.holding(scope, "second")?.is_empty() {
            return Err(format!(
                "the conflicting write changed the rows at seq {seq}"
            ));
        }
        let duplicates = self.model.database.read(|database| database.duplicates);
        if duplicates > 0 {
            return Err(format!(
                "{duplicates} conflicting appends reached the store as duplicate-key errors"
            ));
        }
        Ok(())
    }

    async fn abandoned_in_flight(&mut self, rng: &mut Rng) -> Result<(), String> {
        let scope = "scope-abandoned";
        let log = self
            .claim(scope)
            .await
            .map_err(|error| format!("claim abandoned scope: {error}"))?;
        let early = value(rng);
        push(&log, "dropped-before", &early).await?;
        drop(commit(&log));
        self.stats.dropped_before += 1;
        let first_val = value(rng);
        push(&log, "abandoned", &first_val).await?;
        let before_first = self.next_seq(scope);
        self.model
            .database
            .update(|database| database.pause_next = true);
        {
            let mut pending = std::pin::pin!(commit(&log));
            let polled = std::future::poll_fn(|context| {
                std::task::Poll::Ready(pending.as_mut().poll(context))
            })
            .await;
            if polled.is_ready() {
                return Err("a paused append completed on its first poll".into());
            }
        }
        let first_holding = self.holding(scope, "abandoned")?;
        if self.holding(scope, "dropped-before")? != first_holding {
            return Err(format!(
                "a record whose commit was dropped before it ran is not held with the next commit's rows {first_holding:?}"
            ));
        }
        if first_holding != [before_first] {
            return Err(format!(
                "abandoned append did not land at seq {before_first} before its caller dropped (held by {first_holding:?})"
            ));
        }
        let expected_second = self.next_seq(scope);
        let second_val = value(rng);
        push(&log, "after-abandoned", &second_val).await?;
        let second_seq = commit(&log)
            .await
            .map_err(|error| format!("commit after abandoned in-flight job: {error}"))?;
        if second_seq != expected_second {
            return Err(format!(
                "after-abandoned answered seq {second_seq}, expected {expected_second}"
            ));
        }
        let second_holding = self.holding(scope, "after-abandoned")?;
        if second_holding != [second_seq] {
            return Err(format!(
                "after-abandoned answered seq {second_seq} but is held by {second_holding:?}"
            ));
        }
        let held_first = log
            .read(|fold| fold.host("abandoned").map(|raw| raw.get().to_owned()))
            .await;
        if held_first.as_deref() != Some(first_val.as_str()) {
            return Err(format!(
                "abandoned key landed but writer fold holds {held_first:?}"
            ));
        }
        let held_second = log
            .read(|fold| fold.host("after-abandoned").map(|raw| raw.get().to_owned()))
            .await;
        if held_second.as_deref() != Some(second_val.as_str()) {
            return Err(format!(
                "after-abandoned key landed but writer fold holds {held_second:?}"
            ));
        }
        self.stats.abandoned_in_flight += 1;
        self.store_clean("abandoned-in-flight")
    }
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: AppendStats,
    shape: String,
}

async fn world(
    model: &Arc<Model>,
    clock: &Arc<Clock>,
    row_limit: Option<usize>,
    rng: &mut Rng,
) -> (Option<String>, AppendStats) {
    let mut stats = AppendStats {
        seeds: 1,
        ..AppendStats::default()
    };
    let migrated: Migrated = match SqlLog::migrate(&**model).await {
        Ok(migrated) => migrated,
        Err(error) => return (Some(format!("migrate: {error}")), stats),
    };
    model.database.update(|database| database.migrating = false);
    let mut world = World {
        host: Arc::new(SqlLog::new(Arc::clone(model), &migrated)),
        model: Arc::clone(model),
        row_limit,
        stats,
    };
    let wrong = match world.lost_answers(rng).await {
        Ok(()) => match world.abandoned_in_flight(rng).await {
            Ok(()) => match world.conflicting(rng).await {
                Ok(()) => world.queue(clock, rng).await.err(),
                Err(wrong) => Some(wrong),
            },
            Err(wrong) => Some(wrong),
        },
        Err(wrong) => Some(wrong),
    };
    stats = world.stats;
    model.database.read(|database| {
        stats.landed_unanswered = database.landed_unanswered;
        stats.multi_row_appends = database.multi_row_appends;
        stats.late_landings = database.late_landings;
    });
    (wrong, stats)
}

fn run_seed(seed: u64, mutation: AppendMutation) -> Outcome {
    let time = Clock::new();
    crate::host::entered(seed, &time, || seeded(seed, &time, mutation))
}

fn seeded(seed: u64, clock: &Arc<Clock>, mutation: AppendMutation) -> Outcome {
    let mut rng = Rng::derive(seed, "append");
    let row_limit = rng.chance(ROW_LIMIT_PPM).then_some(ROW_LIMIT_BYTES);
    let stamped = rng.chance(STAMPED_PPM);
    let model = Arc::new(Model {
        database: Atom::new(Database {
            migrating: true,
            ..Database::default()
        }),
    });
    log_sim::mutate(mutation.rule());
    log_sim::force(stamped.then_some(Format::Stamped));
    let (wrong, mut stats) = block_on(world(&model, clock, row_limit, &mut rng));
    log_sim::force(None);
    log_sim::mutate(Rule::None);
    stats.stamped_seeds += u64::from(stamped);
    let digest = model.database.read(|database| {
        database.rows.iter().fold(
            fnv(FNV_OFFSET, &seed.to_le_bytes()),
            |hash, ((scope, seq), body)| {
                fnv(
                    fnv(fnv(hash, scope.as_bytes()), &seq.to_le_bytes()),
                    body.as_bytes(),
                )
            },
        )
    });
    Outcome {
        digest,
        wrong,
        stats,
        shape: format!("row_limit={row_limit:?} stamped={stamped}"),
    }
}

pub fn judge(
    seed: u64,
    mutation: AppendMutation,
    twice: bool,
    verbose: bool,
    totals: &mut AppendStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={:?}",
            first.digest, first.shape, first.stats
        );
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: AppendMutation) -> String {
    let mut command = format!("nanocode-sim --scenario append --seed {seed}");
    if mutation != AppendMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: AppendMutation,
    totals: &AppendStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=append mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        AppendMutation::None if failed == 0 => ExitCode::SUCCESS,
        AppendMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}
