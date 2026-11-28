use std::{
    collections::BTreeMap, num::NonZeroU32, ops::Range, process::ExitCode, sync::Arc,
    time::Duration,
};

use nanocode_durability_core::{
    Migrated, Sql, SqlDialect, SqlRow, SqlStatement, StoreError, StoreFuture,
    drop_index_concurrently,
    sim::{self as durability_sim, Mutation as DurabilityRule},
};
use nanocode_sync::{Atom, block_on};
use nanocode_turnlog::{
    Compaction, LogHost, SqlLog, WakeIndex, WakeRider, WakeSummary,
    parted::{PartStore, SqlParts},
    sim::{self as log_sim, Mutation as Rule},
};

use crate::rng::{FNV_OFFSET, Rng, fnv};

const TURN_LOG: &str = "turn_log";
const WAKE_INDEX: &str = "wake_index";
const COMPONENTS: [&str; 2] = [TURN_LOG, WAKE_INDEX];
const KNOWN: u64 = 1;
const STORED_MAX: u64 = KNOWN + 1;
const OPS: Range<u64> = 4..40;
const SCOPES: u64 = 4;
const ROWS_MAX: u64 = 3;
const ABSENT_PPM: u64 = 250_000;
const DDL: [&str; 4] = ["CREATE ", "ALTER ", "DROP ", "DO "];
const PARTS: NonZeroU32 = NonZeroU32::MIN;
const READ_FORMAT: &str = "FROM nanocode_formats WHERE component";
const WRITE_FORMAT: &str = "INSERT INTO nanocode_formats";
const CREATE_FORMATS: &str = "CREATE TABLE IF NOT EXISTS nanocode_formats";
const FORMATS_PRESENT: &str = "to_regclass('nanocode_formats')";
const LOCK_TIMEOUT: &str = "set_config('lock_timeout'";
const SUPERSEDED_INDEX: &str = "DROP INDEX CONCURRENTLY IF EXISTS nanocode_sim_superseded";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigratedMutation {
    None,
    OpenMigrates,
    CheckMissesFormats,
    PrepareMigrates,
    ConcurrentSkipsLockTimeout,
}

impl MigratedMutation {
    const ALL: [Self; 5] = [
        Self::None,
        Self::OpenMigrates,
        Self::CheckMissesFormats,
        Self::PrepareMigrates,
        Self::ConcurrentSkipsLockTimeout,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::OpenMigrates => "open_migrates",
            Self::CheckMissesFormats => "check_misses_formats",
            Self::PrepareMigrates => "prepare_migrates",
            Self::ConcurrentSkipsLockTimeout => "concurrent_skips_lock_timeout",
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn rule(self) -> Rule {
        match self {
            Self::None | Self::CheckMissesFormats | Self::ConcurrentSkipsLockTimeout => Rule::None,
            Self::OpenMigrates => Rule::OpenMigrates,
            Self::PrepareMigrates => Rule::PrepareMigrates,
        }
    }

    const fn durability(self) -> DurabilityRule {
        match self {
            Self::CheckMissesFormats => DurabilityRule::CheckMissesFormats,
            Self::ConcurrentSkipsLockTimeout => DurabilityRule::ConcurrentSkipsLockTimeout,
            Self::None | Self::OpenMigrates | Self::PrepareMigrates => DurabilityRule::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MigratedStats {
    pub seeds: u64,
    pub checks: u64,
    pub behind: u64,
    pub ahead: u64,
    pub migrations: u64,
    pub serving_calls: u64,
    pub statements: u64,
}

impl MigratedStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.checks += other.checks;
        self.behind += other.behind;
        self.ahead += other.ahead;
        self.migrations += other.migrations;
        self.serving_calls += other.serving_calls;
        self.statements += other.statements;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Check,
    Migrate,
    Serve,
}

#[derive(Clone, Debug, Default)]
struct Database {
    formats_table: bool,
    formats: BTreeMap<String, u64>,
    issued: Vec<(Phase, String)>,
    unbounded: Vec<String>,
}

struct Recorder {
    phase: Atom<Phase>,
    database: Atom<Database>,
}

fn is_ddl(sql: &str) -> bool {
    let head = sql.trim_start().to_ascii_uppercase();
    DDL.iter().any(|word| head.starts_with(word))
}

impl Recorder {
    fn answer(&self, statements: &[SqlStatement<'_>]) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        let phase = *self.phase.get();
        self.database.modify(|database| {
            let mut next = database.clone();
            let mut results = Vec::with_capacity(statements.len());
            let mut bounded = false;
            for statement in statements {
                next.issued.push((phase, statement.sql.to_owned()));
                bounded |= statement.sql.contains(LOCK_TIMEOUT);
                if phase == Phase::Migrate && is_ddl(statement.sql) && !bounded {
                    next.unbounded.push(statement.sql.to_owned());
                }
                let param = |at: usize| statement.params.get(at).map(|param| (*param).to_owned());
                if statement.sql.starts_with(CREATE_FORMATS) {
                    next.formats_table = true;
                    results.push(Vec::new());
                } else if statement.sql.contains(FORMATS_PRESENT) {
                    let present = u8::from(next.formats_table);
                    results.push(vec![vec![Some(present.to_string())]]);
                } else if statement.sql.contains(READ_FORMAT) {
                    if !next.formats_table {
                        return Err(StoreError::Backend(
                            "relation nanocode_formats does not exist".to_owned(),
                        ));
                    }
                    let stored = param(0).and_then(|component| next.formats.get(&component));
                    results
                        .push(stored.map_or_else(Vec::new, |version| {
                            vec![vec![Some(version.to_string())]]
                        }));
                } else if statement.sql.starts_with(WRITE_FORMAT) {
                    let (Some(component), Some(version)) = (param(0), param(1)) else {
                        return Err(StoreError::Backend(
                            "format write without params".to_owned(),
                        ));
                    };
                    let version = version
                        .parse()
                        .map_err(|_| StoreError::Backend(format!("format version {version}")))?;
                    next.formats.insert(component, version);
                    results.push(Vec::new());
                } else {
                    results.push(Vec::new());
                }
            }
            *database = next;
            Ok(results)
        })
    }
}

impl Sql for Recorder {
    fn transaction<'a>(
        &'a self,
        _operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        let answer = self.answer(statements);
        Box::pin(async move { answer })
    }

    fn call_bound(&self) -> Option<Duration> {
        None
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: MigratedStats,
    shape: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Current,
    Behind,
    Ahead,
}

fn expect(database: &Database, component: &str) -> Expect {
    let stored = database.formats.get(component).copied().unwrap_or(0);
    match stored.cmp(&KNOWN) {
        std::cmp::Ordering::Less => Expect::Behind,
        std::cmp::Ordering::Equal => Expect::Current,
        std::cmp::Ordering::Greater => Expect::Ahead,
    }
}

fn judged(
    component: &str,
    want: Expect,
    answer: &Result<Migrated, StoreError>,
    formats_table: bool,
) -> Option<String> {
    match (want, answer) {
        (Expect::Current, Ok(migrated)) if migrated.component() == component => None,
        (Expect::Behind, Err(StoreError::SchemaBehind { stored, .. }))
            if formats_table || *stored == 0 =>
        {
            None
        }
        (Expect::Ahead, Err(StoreError::UnsupportedFormat { .. })) => None,
        _ => Some(format!(
            "{component} check expected {want:?}, answered {answer:?}"
        )),
    }
}

fn ddl_in(database: &Database, phase: Phase) -> Option<String> {
    database
        .issued
        .iter()
        .find(|(at, sql)| *at == phase && is_ddl(sql))
        .map(|(_, sql)| format!("{phase:?} issued DDL: {sql}"))
}

async fn checks(host: &Recorder) -> [Result<Migrated, StoreError>; 2] {
    [SqlLog::check(host).await, WakeIndex::check(host).await]
}

async fn migrations(host: &Recorder) -> [Result<Migrated, StoreError>; 2] {
    [SqlLog::migrate(host).await, WakeIndex::migrate(host).await]
}

async fn serve(
    host: &Arc<Recorder>,
    rng: &mut Rng,
    migrated: &[Migrated; 2],
    stats: &mut MigratedStats,
) {
    let log = SqlLog::new(Arc::clone(host), &migrated[0]);
    let index = WakeIndex::new(Arc::clone(host), &migrated[1]);
    let parts = SqlParts::new(Arc::clone(host)).with_notify(false);
    for _ in 0..rng.range(OPS.start, OPS.end) {
        stats.serving_calls += 1;
        let scope = format!("scope-{}", rng.below(SCOPES));
        let rows: Vec<(u64, String)> = (0..rng.range(1, ROWS_MAX + 1))
            .map(|seq| (seq, format!("row-{seq}")))
            .collect();
        let summary = WakeSummary {
            next_due: rng.chance(500_000).then(|| rng.below(1_000)),
            topics: vec![format!("topic-{}", rng.below(SCOPES))],
        };
        let _answered = match rng.below(8) {
            0 => log.load(&scope).await.map(drop),
            1 => log.scan(&scope, 0, rng.below(8)).await.map(drop),
            2 => log.scans(&[(scope.as_str(), 0)]).await.map(drop),
            3 => log.append(&scope, &rows).await,
            4 => {
                log.append_compacting(
                    &scope,
                    &rows,
                    Compaction {
                        archived: &[],
                        dropped: 0..0,
                        superseded: &[],
                    },
                )
                .await
            }
            5 => index.prepare().await,
            6 => parts.prepare(PARTS).await,
            _ => index.write(&scope, &summary).await,
        };
    }
}

fn run_seed(seed: u64, mutation: MigratedMutation) -> Outcome {
    let mut rng = Rng::derive(seed, "migrated");
    let mut stats = MigratedStats {
        seeds: 1,
        ..MigratedStats::default()
    };
    let mut database = Database {
        formats_table: !rng.chance(ABSENT_PPM),
        ..Database::default()
    };
    if database.formats_table {
        for component in COMPONENTS {
            let stored = rng.below(STORED_MAX + 1);
            if stored > 0 {
                database.formats.insert(component.to_owned(), stored);
            }
        }
    }
    let shape = format!(
        "formats_table={} stored={:?}",
        database.formats_table, database.formats
    );
    let host = Arc::new(Recorder {
        phase: Atom::new(Phase::Check),
        database: Atom::new(database),
    });
    log_sim::mutate(mutation.rule());
    durability_sim::mutate(mutation.durability());
    let wrong = block_on(async {
        let before = (*host.database.get()).clone();
        let wants = COMPONENTS.map(|component| expect(&before, component));
        stats.checks += 2;
        let checked = checks(&host).await;
        for ((component, want), answer) in COMPONENTS.iter().zip(wants).zip(&checked) {
            if let Some(wrong) = judged(component, want, answer, before.formats_table) {
                return Some(wrong);
            }
        }
        if let Some(wrong) = ddl_in(&host.database.get(), Phase::Check) {
            return Some(wrong);
        }
        let migrated = if let [Ok(log), Ok(index)] = &checked {
            [log.clone(), index.clone()]
        } else {
            stats.behind += wants.iter().filter(|want| **want == Expect::Behind).count() as u64;
            stats.ahead += wants.iter().filter(|want| **want == Expect::Ahead).count() as u64;
            host.phase.set(Phase::Migrate);
            stats.migrations += 1;
            let migrated = migrations(&host).await;
            if let Err(error) = drop_index_concurrently(&*host, SUPERSEDED_INDEX).await {
                return Some(format!("dropping a superseded index answered {error:?}"));
            }
            if let Some(sql) = host.database.get().unbounded.first() {
                return Some(format!(
                    "migrate waited on a lock with no lock_timeout: {sql}"
                ));
            }
            if wants.contains(&Expect::Ahead) {
                return migrated
                    .iter()
                    .zip(wants)
                    .find(|(answer, want)| {
                        *want == Expect::Ahead
                            && !matches!(answer, Err(StoreError::UnsupportedFormat { .. }))
                    })
                    .map(|(answer, _)| format!("migrate over a newer format answered {answer:?}"));
            }
            host.phase.set(Phase::Check);
            stats.checks += 2;
            let rechecked = checks(&host).await;
            match (&migrated, &rechecked) {
                ([Ok(log), Ok(index)], [Ok(_), Ok(_)]) => [log.clone(), index.clone()],
                _ => {
                    return Some(format!(
                        "migrate answered {migrated:?}, then check answered {rechecked:?}"
                    ));
                }
            }
        };
        host.phase.set(Phase::Serve);
        serve(&host, &mut rng, &migrated, &mut stats).await;
        ddl_in(&host.database.get(), Phase::Serve)
    });
    log_sim::mutate(Rule::None);
    durability_sim::mutate(DurabilityRule::None);
    let database = host.database.get();
    stats.statements = database.issued.len() as u64;
    let digest = database.issued.iter().fold(
        fnv(FNV_OFFSET, &seed.to_le_bytes()),
        |hash, (phase, sql)| fnv(fnv(hash, &[*phase as u8]), sql.as_bytes()),
    );
    Outcome {
        digest,
        wrong,
        stats,
        shape,
    }
}

pub fn judge(
    seed: u64,
    mutation: MigratedMutation,
    twice: bool,
    verbose: bool,
    totals: &mut MigratedStats,
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

pub fn replay(seed: u64, mutation: MigratedMutation) -> String {
    let mut command = format!("nanocode-sim --scenario migrated --seed {seed}");
    if mutation != MigratedMutation::None {
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
    mutation: MigratedMutation,
    totals: &MigratedStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=migrated mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        MigratedMutation::None if failed == 0 => ExitCode::SUCCESS,
        MigratedMutation::None => ExitCode::FAILURE,
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
