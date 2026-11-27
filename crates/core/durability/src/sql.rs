use std::{collections::HashSet, sync::Arc, time::Duration};

use nanocode_platform::{MaybeSend, MaybeSync};

use serde::ser::SerializeSeq;

use crate::{
    Committed, EncodedPayload, Lease, OwnedCommit, OwnedState, OwnerId, OwnerToken, Settle,
    StateCommit, StateStore, StoreError, StoreFuture, StoreRecord, StoredState,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlStatement<'a> {
    pub sql: &'static str,
    pub params: Vec<&'a str>,
}

impl serde::Serialize for SqlStatement<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.params.len() + 1))?;
        sequence.serialize_element(self.sql)?;
        for param in &self.params {
            sequence.serialize_element(param)?;
        }
        sequence.end()
    }
}

pub type SqlRow = Vec<Option<String>>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SqlAccess {
    Read,
    Write,
    Schema,
    Autocommit,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum SqlDialect {
    #[default]
    Sqlite,
    Postgres,
}

impl SqlDialect {
    #[must_use]
    pub const fn pick(self, statements: &[&'static str; 2]) -> &'static str {
        statements[self as usize]
    }
}

pub trait Sql: MaybeSend + MaybeSync {
    fn transaction<'a>(
        &'a self,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>>;

    fn call_bound(&self) -> Option<Duration>;

    fn read_capacity(&self) -> u32 {
        1
    }

    fn row_bytes_max(&self) -> Option<u64> {
        None
    }

    fn run<'a>(
        &'a self,
        access: SqlAccess,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        let _ = access;
        self.transaction(operation, statements)
    }

    fn write<'a>(
        &'a self,
        settle: Settle,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        let _ = settle;
        self.run(SqlAccess::Write, operation, statements)
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }

    fn concurrent(self: Arc<Self>) -> Option<Arc<dyn Sql>> {
        None
    }

    fn writers(&self) -> usize {
        1
    }

    fn readers(&self) -> usize {
        1
    }
}

type Dialects = [&'static str; 2];

const READ_PART_KEYS: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct MissingColumn {
    pub probe: Dialects,
    pub add: Dialects,
}

#[derive(Clone, Copy, Debug)]
pub struct Migration {
    pub statements: [&'static [&'static str]; 2],
    pub adopt: Option<MissingColumn>,
}

const CREATE_FORMATS: Dialects = [
    "CREATE TABLE IF NOT EXISTS nanocode_formats (component TEXT PRIMARY KEY, version INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS nanocode_formats (component TEXT PRIMARY KEY, version BIGINT NOT NULL)",
];
const READ_FORMAT: Dialects = [
    "SELECT CAST(version AS TEXT) FROM nanocode_formats WHERE component = ?1",
    "SELECT CAST(version AS TEXT) FROM nanocode_formats WHERE component = $1::text",
];
const FORMATS_PRESENT: Dialects = [
    "SELECT CAST(count(*) AS TEXT) FROM sqlite_master WHERE type = 'table' AND name = 'nanocode_formats'",
    "SELECT CAST(CASE WHEN to_regclass('nanocode_formats') IS NULL THEN 0 ELSE 1 END AS TEXT)",
];
const MIGRATE_LOCK_TIMEOUT_MS: u64 = 2_000;
const MIGRATE_LOCK_TRIES: u32 = 3;
const LOCK_NOT_AVAILABLE: &str = "55P03";
const LOCK_TIMEOUT_PG: &str = "SELECT set_config('lock_timeout', $1::text, true)";
const SESSION_LOCK_TIMEOUT_PG: &str = "SELECT set_config('lock_timeout', $1::text, false)";
const INDEX_VALID_PG: &str =
    "SELECT CAST(indisvalid AS TEXT) FROM pg_index WHERE indexrelid = to_regclass($1::text)";
const WRITE_FORMAT: Dialects = [
    "INSERT INTO nanocode_formats (component, version) VALUES (?1, CAST(?2 AS INTEGER)) ON CONFLICT (component) DO UPDATE SET version = excluded.version",
    "INSERT INTO nanocode_formats (component, version) VALUES ($1::text, CAST($2::text AS BIGINT)) ON CONFLICT (component) DO UPDATE SET version = excluded.version",
];

pub(crate) async fn run_checked<H: Sql + ?Sized>(
    host: &H,
    access: SqlAccess,
    operation: &'static str,
    statements: &[SqlStatement<'_>],
) -> Result<Vec<Vec<SqlRow>>, StoreError> {
    answered(host.run(access, operation, statements).await?, statements)
}

pub(crate) async fn written<H: Sql + ?Sized>(
    host: &H,
    settle: Settle,
    operation: &'static str,
    statements: &[SqlStatement<'_>],
) -> Result<Vec<Vec<SqlRow>>, StoreError> {
    answered(host.write(settle, operation, statements).await?, statements)
}

fn answered(
    results: Vec<Vec<SqlRow>>,
    statements: &[SqlStatement<'_>],
) -> Result<Vec<Vec<SqlRow>>, StoreError> {
    if results.len() != statements.len() {
        return Err(StoreError::Backend(format!(
            "SQL host answered {} of {} statements",
            results.len(),
            statements.len()
        )));
    }
    Ok(results)
}

fn lock_timed_out(error: &StoreError) -> bool {
    match error {
        StoreError::Busy => true,
        StoreError::Backend(message) => message.contains(LOCK_NOT_AVAILABLE),
        _ => false,
    }
}

async fn schema_batch<H: Sql + ?Sized>(
    host: &H,
    statements: &[SqlStatement<'_>],
) -> Result<Vec<Vec<SqlRow>>, StoreError> {
    if host.dialect() != SqlDialect::Postgres {
        return run_checked(host, SqlAccess::Schema, "format", statements).await;
    }
    let timeout = MIGRATE_LOCK_TIMEOUT_MS.to_string();
    let bounded: Vec<SqlStatement<'_>> = std::iter::once(SqlStatement {
        sql: LOCK_TIMEOUT_PG,
        params: vec![timeout.as_str()],
    })
    .chain(statements.iter().cloned())
    .collect();
    let mut tries = 1;
    loop {
        match run_checked(host, SqlAccess::Schema, "format", &bounded).await {
            Ok(mut results) => {
                results.remove(0);
                return Ok(results);
            }
            Err(error) if tries < MIGRATE_LOCK_TRIES && lock_timed_out(&error) => {
                tries += 1;
                nanocode_platform::sleep(Duration::from_millis(MIGRATE_LOCK_TIMEOUT_MS)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ConcurrentIndex {
    pub name: &'static str,
    pub create: &'static str,
    pub drop: &'static str,
}

pub async fn build_index_concurrently<H: Sql + ?Sized>(
    host: &H,
    index: &ConcurrentIndex,
) -> Result<(), StoreError> {
    if host.dialect() != SqlDialect::Postgres {
        return Err(StoreError::Backend(format!(
            "index {} is built CONCURRENTLY, which only Postgres has",
            index.name
        )));
    }
    let timeout = MIGRATE_LOCK_TIMEOUT_MS.to_string();
    let bounded = |sql| lock_bounded(&timeout, sql);
    for build in 1..=MIGRATE_LOCK_TRIES {
        match run_checked(host, SqlAccess::Autocommit, "index", &bounded(index.create)).await {
            Ok(_) => {}
            Err(error) if build < MIGRATE_LOCK_TRIES && lock_timed_out(&error) => {
                nanocode_platform::sleep(Duration::from_millis(MIGRATE_LOCK_TIMEOUT_MS)).await;
                continue;
            }
            Err(error) => return Err(error),
        }
        let read = [SqlStatement {
            sql: INDEX_VALID_PG,
            params: vec![index.name],
        }];
        let valid = run_checked(host, SqlAccess::Schema, "index", &read).await?;
        match single(&valid[0])?.map(|row| text(row, 0)).transpose()? {
            Some("true") => return Ok(()),
            Some(_) => {
                run_checked(host, SqlAccess::Autocommit, "index", &bounded(index.drop)).await?;
            }
            None => {
                return Err(StoreError::Backend(format!(
                    "index {} is missing after its build",
                    index.name
                )));
            }
        }
    }
    Err(StoreError::Backend(format!(
        "index {} is still not valid after {MIGRATE_LOCK_TRIES} builds",
        index.name
    )))
}

fn lock_bounded<'a>(timeout: &'a str, sql: &'static str) -> Vec<SqlStatement<'a>> {
    let bound = SqlStatement {
        sql: SESSION_LOCK_TIMEOUT_PG,
        params: vec![timeout],
    };
    let statement = SqlStatement {
        sql,
        params: Vec::new(),
    };
    if crate::sim::mutated(crate::sim::Mutation::ConcurrentSkipsLockTimeout) {
        return vec![statement];
    }
    vec![bound, statement]
}

pub async fn drop_index_concurrently<H: Sql + ?Sized>(
    host: &H,
    drop: &'static str,
) -> Result<(), StoreError> {
    if host.dialect() != SqlDialect::Postgres {
        return Err(StoreError::Backend(format!(
            "{drop} runs CONCURRENTLY, which only Postgres has"
        )));
    }
    let timeout = MIGRATE_LOCK_TIMEOUT_MS.to_string();
    for attempt in 1..=MIGRATE_LOCK_TRIES {
        match run_checked(host, SqlAccess::Autocommit, "index", &lock_bounded(&timeout, drop)).await {
            Ok(_) => return Ok(()),
            Err(error) if attempt < MIGRATE_LOCK_TRIES && lock_timed_out(&error) => {
                nanocode_platform::sleep(Duration::from_millis(MIGRATE_LOCK_TIMEOUT_MS)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the last attempt returns its answer")
}

pub async fn migrate_format<H: Sql + ?Sized>(
    host: &H,
    component: &'static str,
    migrations: &[Migration],
) -> Result<(), StoreError> {
    migrate_format_then(host, component, migrations, &[])
        .await
        .map(drop)
}

pub async fn migrate_format_then<H: Sql + ?Sized>(
    host: &H,
    component: &'static str,
    migrations: &[Migration],
    then: &[SqlStatement<'_>],
) -> Result<Vec<Vec<SqlRow>>, StoreError> {
    let mut last: Option<String> = None;
    loop {
        match migrate_once(host, component, migrations, then).await {
            Err(StoreError::Backend(text))
                if created_concurrently(&text) && last.as_deref() != Some(text.as_str()) =>
            {
                last = Some(text);
            }
            answer => return answer,
        }
    }
}

const CREATED_CONCURRENTLY: [&str; 5] = [
    "pg_type_typname_nsp_index",
    "pg_class_relname_nsp_index",
    "pg_proc_proname_args_nsp_index",
    "(ERROR 42710)",
    "(ERROR 42P07)",
];

fn created_concurrently(text: &str) -> bool {
    CREATED_CONCURRENTLY.iter().any(|mark| text.contains(mark))
}

async fn migrate_once<H: Sql + ?Sized>(
    host: &H,
    component: &'static str,
    migrations: &[Migration],
    then: &[SqlStatement<'_>],
) -> Result<Vec<Vec<SqlRow>>, StoreError> {
    let dialect = host.dialect();
    let check = [
        statement(dialect, &CREATE_FORMATS, Vec::new()),
        statement(dialect, &READ_FORMAT, vec![component]),
    ];
    let known = u32::try_from(migrations.len()).unwrap_or(u32::MAX);
    let current = |results: &[Vec<SqlRow>]| -> Result<u64, StoreError> {
        Ok(match single(&results[1])? {
            Some(row) => counter(row, 0)?,
            None => 0,
        })
    };
    let stored = current(&schema_batch(host, &check).await?)?;
    if stored > u64::from(known) {
        return Err(StoreError::UnsupportedFormat {
            component,
            stored,
            known,
        });
    }
    if stored == u64::from(known) {
        return if then.is_empty() {
            Ok(Vec::new())
        } else {
            schema_batch(host, then).await
        };
    }
    let last = migrations.len() - 1;
    for (index, migration) in migrations.iter().enumerate().skip(stored as usize) {
        let version = (index + 1).to_string();
        let write = statement(dialect, &WRITE_FORMAT, vec![component, &version]);
        let trailing: &[SqlStatement<'_>] = if index == last { then } else { &[] };
        let schema = migration.statements[dialect as usize]
            .iter()
            .map(|sql| SqlStatement {
                sql,
                params: Vec::new(),
            });
        let Some(adopt) = migration.adopt else {
            let statements: Vec<_> = schema
                .chain([write])
                .chain(trailing.iter().cloned())
                .collect();
            let mut results = schema_batch(host, &statements).await?;
            if index == last {
                return Ok(results.split_off(statements.len() - trailing.len()));
            }
            continue;
        };
        let probe = || statement(dialect, &adopt.probe, Vec::new());
        let statements: Vec<_> = schema.chain([probe()]).collect();
        let results = schema_batch(host, &statements).await?;
        if !column_present(&results[statements.len() - 1])? {
            let add = [statement(dialect, &adopt.add, Vec::new())];
            if let Err(error) = schema_batch(host, &add).await
                && !column_present(
                    &schema_batch(host, &[probe()]).await?[0],
                )?
            {
                return Err(error);
            }
        }
        let statements: Vec<_> = [write]
            .into_iter()
            .chain(trailing.iter().cloned())
            .collect();
        let mut results = schema_batch(host, &statements).await?;
        if index == last {
            return Ok(results.split_off(1));
        }
    }
    Ok(Vec::new())
}

pub(crate) async fn check_format<H: Sql + ?Sized>(
    host: &H,
    component: &'static str,
    known: u32,
) -> Result<(), StoreError> {
    let read = [statement(host.dialect(), &READ_FORMAT, vec![component])];
    let stored = match run_checked(host, SqlAccess::Read, "format", &read).await {
        Ok(results) => match single(&results[0])? {
            Some(row) => counter(row, 0)?,
            None => 0,
        },
        Err(error) => {
            if !formats_absent(host).await? {
                return Err(error);
            }
            0
        }
    };
    if stored > u64::from(known) {
        return Err(StoreError::UnsupportedFormat {
            component,
            stored,
            known,
        });
    }
    if stored < u64::from(known) {
        return Err(StoreError::SchemaBehind {
            component,
            stored,
            known,
        });
    }
    Ok(())
}

async fn formats_absent<H: Sql + ?Sized>(host: &H) -> Result<bool, StoreError> {
    if crate::sim::mutated(crate::sim::Mutation::CheckMissesFormats) {
        return Ok(false);
    }
    let probe = [statement(host.dialect(), &FORMATS_PRESENT, Vec::new())];
    let results = run_checked(host, SqlAccess::Read, "format", &probe).await?;
    Ok(!column_present(&results[0])?)
}

fn column_present(rows: &[SqlRow]) -> Result<bool, StoreError> {
    match single(rows)? {
        Some(row) => Ok(counter(row, 0)? > 0),
        None => Ok(false),
    }
}

const SCHEMA: [&[&str]; 2] = [
    &[
        "CREATE TABLE IF NOT EXISTS states (id TEXT PRIMARY KEY, revision TEXT NOT NULL, payload TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS owners (id TEXT PRIMARY KEY, owner TEXT NOT NULL, fence TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS records (state_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (state_id, key)) WITHOUT ROWID",
        "CREATE TABLE IF NOT EXISTS blobs (id INTEGER PRIMARY KEY, state_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL)",
    ],
    &[
        "CREATE TABLE IF NOT EXISTS states (id TEXT PRIMARY KEY, revision TEXT NOT NULL, payload TEXT NOT NULL, ord BIGINT GENERATED BY DEFAULT AS IDENTITY)",
        "CREATE TABLE IF NOT EXISTS owners (id TEXT PRIMARY KEY, owner TEXT NOT NULL, fence TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS blobs (id BIGINT PRIMARY KEY, state_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL)",
    ],
];
const SHARED: [&[&str]; 2] = [
    &[
        "ALTER TABLE records RENAME TO legacy_records",
        "CREATE TABLE records (id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, value TEXT NOT NULL)",
        "CREATE TABLE state_records (record INTEGER NOT NULL, state_id TEXT NOT NULL, PRIMARY KEY (record, state_id)) WITHOUT ROWID",
        "CREATE INDEX state_records_state ON state_records (state_id)",
        "INSERT INTO records (key, value) SELECT key, value FROM blobs WHERE true ON CONFLICT (key) DO NOTHING",
        "INSERT INTO records (key, value) SELECT key, value FROM legacy_records WHERE true ON CONFLICT (key) DO NOTHING",
        "INSERT INTO state_records (state_id, record) SELECT blobs.state_id, records.id FROM blobs JOIN records ON records.key = blobs.key WHERE true ON CONFLICT DO NOTHING",
        "INSERT INTO state_records (state_id, record) SELECT legacy_records.state_id, records.id FROM legacy_records JOIN records ON records.key = legacy_records.key WHERE true ON CONFLICT DO NOTHING",
        "DROP TABLE legacy_records",
        "DROP TABLE blobs",
    ],
    &[
        "CREATE TABLE IF NOT EXISTS records (id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, key TEXT NOT NULL UNIQUE, value TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS state_records (record BIGINT NOT NULL, state_id TEXT NOT NULL, PRIMARY KEY (record, state_id))",
        "CREATE INDEX IF NOT EXISTS state_records_state ON state_records (state_id)",
        "INSERT INTO records (key, value) SELECT DISTINCT ON (key) key, value FROM blobs ORDER BY key ON CONFLICT (key) DO NOTHING",
        "INSERT INTO state_records (state_id, record) SELECT DISTINCT blobs.state_id, records.id FROM blobs JOIN records ON records.key = blobs.key ON CONFLICT DO NOTHING",
        "DROP TABLE blobs",
    ],
];
const HOLDERS: [&[&str]; 2] = [
    &[
        "ALTER TABLE records ADD COLUMN holder TEXT",
        "CREATE INDEX records_holder ON records (holder) WHERE holder IS NOT NULL",
    ],
    &[
        "ALTER TABLE records ADD COLUMN IF NOT EXISTS holder TEXT",
        "CREATE INDEX IF NOT EXISTS records_holder ON records (holder) WHERE holder IS NOT NULL",
    ],
];
const LEASES: [&[&str]; 2] = [
    &[
        "ALTER TABLE owners ADD COLUMN revision TEXT",
        "ALTER TABLE owners ADD COLUMN expires INTEGER",
        "ALTER TABLE records ADD COLUMN orphaned INTEGER",
        "INSERT INTO owners (id, owner, fence) SELECT id, '', '0' FROM states WHERE id NOT IN (SELECT id FROM owners)",
        "UPDATE owners SET revision = COALESCE((SELECT revision FROM states WHERE states.id = owners.id), '0') WHERE revision IS NULL",
        "UPDATE records SET orphaned = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE holder IS NULL AND NOT EXISTS (SELECT 1 FROM state_records WHERE state_records.record = records.id)",
        "CREATE INDEX records_orphaned ON records (orphaned) WHERE orphaned IS NOT NULL",
    ],
    &[
        "ALTER TABLE owners ADD COLUMN IF NOT EXISTS revision TEXT",
        "ALTER TABLE owners ADD COLUMN IF NOT EXISTS expires BIGINT",
        "ALTER TABLE records ADD COLUMN IF NOT EXISTS orphaned BIGINT",
        "INSERT INTO owners (id, owner, fence) SELECT id, '', '0' FROM states ON CONFLICT (id) DO NOTHING",
        "UPDATE owners SET revision = COALESCE((SELECT revision FROM states WHERE states.id = owners.id), '0') WHERE revision IS NULL",
        "UPDATE records SET orphaned = CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) WHERE holder IS NULL AND orphaned IS NULL AND NOT EXISTS (SELECT 1 FROM state_records WHERE state_records.record = records.id)",
        "CREATE INDEX IF NOT EXISTS records_orphaned ON records (orphaned) WHERE orphaned IS NOT NULL",
    ],
];
const PEEK: Dialects = [
    "SELECT revision, payload FROM states WHERE id = ?1",
    "SELECT revision, payload FROM states WHERE id = $1::text",
];
const OWNER: Dialects = [
    "SELECT owner, fence FROM owners WHERE id = ?1 AND owner <> ''",
    "SELECT owner, fence FROM owners WHERE id = $1::text AND owner <> ''",
];
const HEAD: Dialects = [
    "SELECT owner, fence, COALESCE(revision, '0') FROM owners WHERE id = ?1",
    "SELECT owner, fence, COALESCE(revision, '0') FROM owners WHERE id = $1::text",
];
const RECORDS: Dialects = [
    "SELECT records.value FROM json_each(?1) AS keys LEFT JOIN records ON records.key = keys.value ORDER BY keys.key",
    "SELECT records.value FROM jsonb_array_elements_text($1::text::jsonb) WITH ORDINALITY AS keys (key, ord) LEFT JOIN records ON records.key = keys.key ORDER BY keys.ord",
];
const STATES: Dialects = [
    "SELECT id FROM states ORDER BY rowid DESC LIMIT CAST(?1 AS INTEGER)",
    "SELECT id FROM states ORDER BY ord DESC LIMIT CAST($1::text AS BIGINT)",
];
const RECORD_KEYS: Dialects = [
    "SELECT key FROM records WHERE holder = ?1 UNION SELECT records.key FROM state_records JOIN records ON records.id = state_records.record WHERE state_records.state_id = ?1",
    "SELECT key FROM records WHERE holder = $1::text UNION SELECT records.key FROM state_records JOIN records ON records.id = state_records.record WHERE state_records.state_id = $1::text",
];
const CLAIM: Dialects = [
    "INSERT INTO owners (id, owner, fence, revision, expires) VALUES (?1, ?2, '1', '0', CAST(NULLIF(?3, '') AS INTEGER) + CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER)) ON CONFLICT (id) DO UPDATE SET owner = excluded.owner, fence = CAST(CAST(fence AS INTEGER) + 1 AS TEXT), expires = excluded.expires WHERE CAST(fence AS INTEGER) < 9223372036854775807 AND (owner = '' OR owner = excluded.owner OR expires IS NULL OR ?4 = '' OR expires + CAST(NULLIF(?4, '') AS INTEGER) < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER)) RETURNING fence",
    "INSERT INTO owners (id, owner, fence, revision, expires) VALUES ($1::text, $2::text, '1', '0', CAST(NULLIF($3::text, '') AS BIGINT) + CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)) ON CONFLICT (id) DO UPDATE SET owner = excluded.owner, fence = CAST(CAST(owners.fence AS BIGINT) + 1 AS TEXT), expires = excluded.expires WHERE CAST(owners.fence AS BIGINT) < 9223372036854775807 AND (owners.owner = '' OR owners.owner = excluded.owner OR owners.expires IS NULL OR $4::text = '' OR owners.expires + CAST(NULLIF($4::text, '') AS BIGINT) < CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)) RETURNING fence",
];
const CLAIM_MANY: Dialects = [
    "INSERT INTO owners (id, owner, fence, revision, expires) SELECT value ->> 0, value ->> 1, '1', '0', CAST(NULLIF(?2, '') AS INTEGER) + CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) FROM json_each(?1) WHERE true ON CONFLICT (id) DO UPDATE SET owner = excluded.owner, fence = CAST(CAST(fence AS INTEGER) + 1 AS TEXT), expires = excluded.expires WHERE CAST(fence AS INTEGER) < 9223372036854775807 AND (owner = '' OR owner = excluded.owner OR expires IS NULL OR ?3 = '' OR expires + CAST(NULLIF(?3, '') AS INTEGER) < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER)) RETURNING id, fence",
    "INSERT INTO owners (id, owner, fence, revision, expires) SELECT wanted.value ->> 0, wanted.value ->> 1, '1', '0', CAST(NULLIF($2::text, '') AS BIGINT) + CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) FROM json_array_elements($1::text::json) AS wanted (value) ON CONFLICT (id) DO UPDATE SET owner = excluded.owner, fence = CAST(CAST(owners.fence AS BIGINT) + 1 AS TEXT), expires = excluded.expires WHERE CAST(owners.fence AS BIGINT) < 9223372036854775807 AND (owners.owner = '' OR owners.owner = excluded.owner OR owners.expires IS NULL OR $3::text = '' OR owners.expires + CAST(NULLIF($3::text, '') AS BIGINT) < CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)) RETURNING id, fence",
];
const PEEK_MANY: Dialects = [
    "SELECT states.id, states.revision, states.payload FROM json_each(?1) AS wanted JOIN states ON states.id = wanted.value",
    "SELECT states.id, states.revision, states.payload FROM json_array_elements_text($1::text::json) AS wanted (id) JOIN states ON states.id = wanted.id",
];
const RELEASE: Dialects = [
    "UPDATE owners SET owner = '', expires = NULL WHERE id = ?1 AND owner = ?2 AND fence = ?3 RETURNING fence",
    "UPDATE owners SET owner = '', expires = NULL WHERE id = $1::text AND owner = $2::text AND fence = $3::text RETURNING fence",
];
const HEAD_ONE: &str = "UPDATE owners SET revision = ?2, expires = CAST(NULLIF(?5, '') AS INTEGER) + CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = ?1 AND owner = ?3 AND fence = ?4 AND revision = ?6 RETURNING revision";
const STATE_ONE: &str = "INSERT INTO states (id, revision, payload) SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM owners WHERE id = ?1 AND owner = ?4 AND fence = ?5 AND revision = ?2) ON CONFLICT (id) DO UPDATE SET revision = excluded.revision, payload = excluded.payload RETURNING id, revision";
const REVIVE_ONE: &str = "UPDATE records SET orphaned = NULL WHERE key IN (SELECT value FROM json_each(?5)) AND orphaned IS NOT NULL AND EXISTS (SELECT 1 FROM owners WHERE id = ?1 AND owner = ?2 AND fence = ?3 AND revision = ?4)";
const RECORD_ONE: &str = "INSERT INTO records (key, value, holder) SELECT ?5, ?6, ?1 WHERE EXISTS (SELECT 1 FROM owners WHERE id = ?1 AND owner = ?2 AND fence = ?3 AND revision = ?4) ON CONFLICT (key) DO NOTHING";
const HOLD_ONE: &str = "INSERT INTO state_records (record, state_id) SELECT records.id, ?1 FROM json_each(?5) AS staged JOIN records ON records.key = staged.value WHERE records.holder IS NOT ?1 AND EXISTS (SELECT 1 FROM owners WHERE id = ?1 AND owner = ?2 AND fence = ?3 AND revision = ?4) ON CONFLICT DO NOTHING";
const GROUP_HEADS_PG: &str = "WITH commits AS (SELECT * FROM json_to_recordset($1::text::json) AS commits (state text, owner text, fence text, expected text, revision text, payload text) ORDER BY state), head AS (UPDATE owners SET revision = commits.revision, expires = CAST(NULLIF($2::text, '') AS BIGINT) + CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) FROM commits WHERE owners.id = commits.state AND owners.owner = commits.owner AND owners.fence = commits.fence AND owners.revision = commits.expected RETURNING owners.id, owners.revision) INSERT INTO states (id, revision, payload) SELECT head.id, head.revision, commits.payload FROM head JOIN commits ON commits.state = head.id ORDER BY head.id ON CONFLICT (id) DO UPDATE SET revision = excluded.revision, payload = excluded.payload RETURNING id, revision";
const GROUP_REVIVE_PG: &str = "UPDATE records SET orphaned = NULL FROM (SELECT DISTINCT staged.key FROM json_to_recordset($1::text::json) AS commits (state text, owner text, fence text, revision text, keys json) CROSS JOIN LATERAL json_array_elements_text(commits.keys) AS staged (key) JOIN owners ON owners.id = commits.state AND owners.owner = commits.owner AND owners.fence = commits.fence AND owners.revision = commits.revision ORDER BY staged.key) AS landed WHERE records.key = landed.key AND records.orphaned IS NOT NULL";
const GROUP_RECORDS_PG: &str = "INSERT INTO records (key, value, holder) SELECT DISTINCT ON (staged.key) staged.key, staged.value, commits.state FROM json_to_recordset($1::text::json) AS commits (state text, owner text, fence text, revision text, records json) CROSS JOIN LATERAL json_to_recordset(commits.records) AS staged (key text, value text) JOIN owners ON owners.id = commits.state AND owners.owner = commits.owner AND owners.fence = commits.fence AND owners.revision = commits.revision ORDER BY staged.key, commits.state ON CONFLICT (key) DO NOTHING";
const GROUP_HOLDS_PG: &str = "INSERT INTO state_records (record, state_id) SELECT records.id, commits.state FROM json_to_recordset($1::text::json) AS commits (state text, owner text, fence text, revision text, keys json) CROSS JOIN LATERAL json_array_elements_text(commits.keys) AS staged (key) JOIN owners ON owners.id = commits.state AND owners.owner = commits.owner AND owners.fence = commits.fence AND owners.revision = commits.revision JOIN records ON records.key = staged.key WHERE records.holder IS DISTINCT FROM commits.state ON CONFLICT DO NOTHING";
const PUT_REVIVE: Dialects = [
    "UPDATE records SET orphaned = NULL WHERE key IN (SELECT value FROM json_each(?1)) AND orphaned IS NOT NULL",
    "UPDATE records SET orphaned = NULL FROM (SELECT staged.key FROM jsonb_array_elements_text($1::text::jsonb) AS staged (key) ORDER BY staged.key) AS staged WHERE records.key = staged.key AND records.orphaned IS NOT NULL",
];
const PUT_RECORD: &str =
    "INSERT INTO records (key, value, holder) VALUES (?1, ?2, ?3) ON CONFLICT (key) DO NOTHING";
const PUT_RECORDS: &str = "INSERT INTO records (key, value, holder) SELECT staged.key, staged.value, $1::text FROM json_to_recordset($2::text::json) AS staged (key text, value text) ORDER BY staged.key ON CONFLICT (key) DO NOTHING";
const PUT_HOLDS: Dialects = [
    "INSERT INTO state_records (record, state_id) SELECT records.id, ?1 FROM json_each(?2) AS staged JOIN records ON records.key = staged.value WHERE records.holder IS NOT ?1 ON CONFLICT DO NOTHING",
    "INSERT INTO state_records (record, state_id) SELECT records.id, $1::text FROM jsonb_array_elements_text($2::text::jsonb) AS staged (key) JOIN records ON records.key = staged.key WHERE records.holder IS DISTINCT FROM $1::text ON CONFLICT DO NOTHING",
];
const COLLECT_HEAD: Dialects = [
    "UPDATE owners SET expires = CAST(NULLIF(?5, '') AS INTEGER) + CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = ?1 AND owner = ?2 AND fence = ?3 AND revision = ?4 RETURNING revision",
    "UPDATE owners SET expires = CAST(NULLIF($5::text, '') AS BIGINT) + CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) WHERE id = $1::text AND owner = $2::text AND fence = $3::text AND revision = $4::text RETURNING revision",
];
const COLLECT_REFS: Dialects = [
    "DELETE FROM state_records WHERE state_id = ?1 AND record IN (SELECT records.id FROM json_each(?5) AS dead JOIN records ON records.key = dead.value) AND EXISTS (SELECT 1 FROM owners WHERE id = ?1 AND owner = ?2 AND fence = ?3 AND revision = ?4) RETURNING record",
    "DELETE FROM state_records USING jsonb_array_elements_text($5::text::jsonb) AS dead (key), records, owners WHERE records.key = dead.key AND state_records.state_id = $1::text AND state_records.record = records.id AND owners.id = $1::text AND owners.owner = $2::text AND owners.fence = $3::text AND owners.revision = $4::text RETURNING CAST(state_records.record AS TEXT)",
];
const COLLECT_HELD: Dialects = [
    "UPDATE records SET holder = NULL WHERE holder = ?1 AND key IN (SELECT value FROM json_each(?5)) AND EXISTS (SELECT 1 FROM owners WHERE id = ?1 AND owner = ?2 AND fence = ?3 AND revision = ?4) RETURNING id",
    "UPDATE records SET holder = NULL FROM (SELECT dead.key FROM jsonb_array_elements_text($5::text::jsonb) AS dead (key) ORDER BY dead.key) AS dead, owners WHERE records.key = dead.key AND records.holder = $1::text AND owners.id = $1::text AND owners.owner = $2::text AND owners.fence = $3::text AND owners.revision = $4::text RETURNING CAST(records.id AS TEXT)",
];
const COLLECT_STAMP: Dialects = [
    "UPDATE records SET orphaned = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE key IN (SELECT value FROM json_each(?5)) AND holder IS NULL AND orphaned IS NULL AND NOT EXISTS (SELECT 1 FROM state_records WHERE state_records.record = records.id) AND EXISTS (SELECT 1 FROM owners WHERE id = ?1 AND owner = ?2 AND fence = ?3 AND revision = ?4) RETURNING id",
    "UPDATE records SET orphaned = CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) FROM (SELECT dead.key FROM jsonb_array_elements_text($5::text::jsonb) AS dead (key) ORDER BY dead.key) AS dead, owners WHERE records.key = dead.key AND records.holder IS NULL AND records.orphaned IS NULL AND NOT EXISTS (SELECT 1 FROM state_records WHERE state_records.record = records.id) AND owners.id = $1::text AND owners.owner = $2::text AND owners.fence = $3::text AND owners.revision = $4::text RETURNING CAST(records.id AS TEXT)",
];
const PURGE: Dialects = [
    "DELETE FROM records WHERE id IN (SELECT id FROM records WHERE holder IS NULL AND orphaned IS NOT NULL AND orphaned < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) - CAST(?1 AS INTEGER) AND NOT EXISTS (SELECT 1 FROM state_records WHERE state_records.record = records.id) LIMIT CAST(?2 AS INTEGER)) RETURNING id",
    "DELETE FROM records USING (SELECT picked.id FROM (SELECT id, key FROM records WHERE holder IS NULL AND orphaned IS NOT NULL AND orphaned < CAST(EXTRACT(EPOCH FROM statement_timestamp()) * 1000 AS BIGINT) - CAST($1::text AS BIGINT) AND NOT EXISTS (SELECT 1 FROM state_records WHERE state_records.record = records.id) LIMIT CAST($2::text AS BIGINT)) AS picked ORDER BY picked.key) AS dead WHERE records.id = dead.id AND records.holder IS NULL AND records.orphaned < CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) - CAST($1::text AS BIGINT) RETURNING CAST(records.id AS TEXT)",
];
const PURGE_DEADLINE_PG: &str = "SELECT set_config('statement_timeout', $1::text, true)";

struct Text<T>(T);

impl<T: AsRef<str>> serde::Serialize for Text<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_ref())
    }
}

struct Each<I>(std::cell::Cell<Option<I>>);

impl<I: Iterator<Item = T>, T: serde::Serialize> serde::Serialize for Each<I> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.take().into_iter().flatten())
    }
}

pub(crate) fn keys_json(keys: impl Iterator<Item = impl AsRef<str>>) -> Result<String, StoreError> {
    serde_json::to_string(&Each(std::cell::Cell::new(Some(keys.map(Text)))))
        .map_err(|error| StoreError::NotCommitted(format!("record keys: {error}")))
}

pub(crate) const fn statement<'a>(
    dialect: SqlDialect,
    sql: &Dialects,
    params: Vec<&'a str>,
) -> SqlStatement<'a> {
    SqlStatement {
        sql: dialect.pick(sql),
        params,
    }
}

pub(crate) fn invalid(detail: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(format!("SQL host answered an invalid row: {detail}"))
}

pub(crate) fn text(row: &SqlRow, column: usize) -> Result<&str, StoreError> {
    row.get(column)
        .and_then(Option::as_deref)
        .ok_or_else(|| invalid(format!("column {column} is missing")))
}

pub(crate) fn counter(row: &SqlRow, column: usize) -> Result<u64, StoreError> {
    let value = text(row, column)?;
    value
        .parse()
        .map_err(|error| invalid(format!("counter `{value}`: {error}")))
}

pub(crate) fn limit_param(limit: usize) -> String {
    u64::try_from(limit)
        .unwrap_or(u64::MAX)
        .min(i64::MAX as u64)
        .to_string()
}

pub(crate) fn names(rows: &[SqlRow]) -> Result<Vec<String>, StoreError> {
    rows.iter()
        .map(|row| text(row, 0).map(str::to_owned))
        .collect()
}

pub(crate) fn owner_token(rows: &[SqlRow]) -> Result<Option<OwnerToken>, StoreError> {
    single(rows)?
        .map(|row| {
            Ok(OwnerToken::new(
                OwnerId::from(text(row, 0)?.to_owned()),
                counter(row, 1)?,
            ))
        })
        .transpose()
}

pub(crate) fn single(rows: &[SqlRow]) -> Result<Option<&SqlRow>, StoreError> {
    match rows {
        [] => Ok(None),
        [row] => Ok(Some(row)),
        _ => Err(invalid(format!(
            "{} rows for a single-row query",
            rows.len()
        ))),
    }
}

fn stored(rows: &[SqlRow]) -> Result<StoredState, StoreError> {
    match single(rows)? {
        None => Ok(StoredState::default()),
        Some(row) => Ok(StoredState {
            revision: counter(row, 0)?,
            payload: Some(text(row, 1)?.to_owned()),
        }),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Unreachable {
    pub revision: u64,
    pub keys: Vec<String>,
    pub reachable: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Purged {
    pub deleted: u64,
    pub batches: u64,
    pub finished: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Collected {
    pub released: u64,
    pub deleted: u64,
}

pub(crate) async fn unreachable_in(
    store: &mut dyn StateStore,
    state_id: &str,
) -> Result<Unreachable, StoreError> {
    let head = store.peek(state_id).await?;
    let held = store.list_records(state_id).await?;
    let mut unreachable = Unreachable {
        revision: head.revision,
        keys: Vec::new(),
        reachable: held.len(),
    };
    if !held.iter().all(|key| EncodedPayload::is_layout_key(key)) {
        return Ok(unreachable);
    }
    let holds: HashSet<&str> = held.iter().map(String::as_str).collect();
    let mut reached = HashSet::new();
    let mut level = Vec::new();
    let follow = |json: &str, reached: &mut HashSet<String>, level: &mut Vec<_>| {
        for key in EncodedPayload::references(json) {
            if holds.contains(key) && reached.insert(key.to_owned()) {
                level.push(EncodedPayload::by_key(key));
            }
        }
    };
    follow(
        head.payload.as_deref().unwrap_or_default(),
        &mut reached,
        &mut level,
    );
    while !level.is_empty() {
        let loaded = EncodedPayload::load_many_noting(&level, &mut *store, state_id, &mut reached)
            .await
            .map_err(|error| {
                StoreError::Backend(format!("state {state_id} is not whole: {error}"))
            })?;
        level.clear();
        for payload in &loaded {
            let json = payload
                .json()
                .map_err(|error| StoreError::Backend(error.to_string()))?;
            follow(json, &mut reached, &mut level);
        }
    }
    unreachable.keys = held
        .iter()
        .filter(|key| !reached.contains(*key))
        .cloned()
        .collect();
    unreachable.reachable = held.len() - unreachable.keys.len();
    Ok(unreachable)
}

struct Replacement<'a> {
    state_id: &'a str,
    owner: &'a OwnerToken,
    expected_revision: u64,
    revision: u64,
    payload: &'a str,
    records: &'a [StoreRecord],
}

#[derive(serde::Serialize)]
struct GroupHead<'a> {
    state: &'a str,
    owner: &'a str,
    fence: &'a str,
    expected: &'a str,
    revision: &'a str,
    payload: &'a str,
}

#[derive(serde::Serialize)]
struct GroupValue<'a> {
    key: &'a str,
    value: &'a str,
}

#[derive(serde::Serialize)]
struct GroupRecords<'a> {
    state: &'a str,
    owner: &'a str,
    fence: &'a str,
    revision: &'a str,
    keys: Vec<&'a str>,
    records: Vec<GroupValue<'a>>,
}

struct Counters {
    fence: String,
    expected: String,
    revision: String,
    staged: String,
}

impl Replacement<'_> {
    fn counters(&self) -> Result<Counters, StoreError> {
        if self.revision <= self.expected_revision {
            return Err(StoreError::NotCommitted(format!(
                "a replacement of revision {} must advance it, not set {}",
                self.expected_revision, self.revision
            )));
        }
        Ok(Counters {
            fence: self.owner.fence().to_string(),
            expected: self.expected_revision.to_string(),
            revision: self.revision.to_string(),
            staged: keys_json(self.keys().into_iter())?,
        })
    }

    fn keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self
            .records
            .iter()
            .map(|record| record.key.as_str())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    fn values(&self) -> Vec<GroupValue<'_>> {
        let mut values: Vec<GroupValue<'_>> = self
            .records
            .iter()
            .map(|record| GroupValue {
                key: &record.key,
                value: &record.value,
            })
            .collect();
        values.sort_by(|left, right| left.key.cmp(right.key));
        values.dedup_by(|left, right| left.key == right.key);
        values
    }

    fn sqlite<'a>(
        &'a self,
        counters: &'a Counters,
        ttl: &'a str,
        statements: &mut Vec<SqlStatement<'a>>,
    ) -> usize {
        let owner = self.owner.owner_id().as_str();
        let gate = [
            self.state_id,
            owner,
            counters.fence.as_str(),
            counters.revision.as_str(),
        ];
        statements.push(SqlStatement {
            sql: HEAD_ONE,
            params: vec![
                self.state_id,
                &counters.revision,
                owner,
                &counters.fence,
                ttl,
                &counters.expected,
            ],
        });
        statements.push(SqlStatement {
            sql: STATE_ONE,
            params: vec![
                self.state_id,
                &counters.revision,
                self.payload,
                owner,
                &counters.fence,
            ],
        });
        let landed = statements.len() - 1;
        if self.records.is_empty() {
            return landed;
        }
        statements.push(SqlStatement {
            sql: REVIVE_ONE,
            params: [&gate[..], &[counters.staged.as_str()]].concat(),
        });
        statements.extend(self.values().into_iter().map(|record| SqlStatement {
            sql: RECORD_ONE,
            params: [&gate[..], &[record.key, record.value]].concat(),
        }));
        statements.push(SqlStatement {
            sql: HOLD_ONE,
            params: [&gate[..], &[counters.staged.as_str()]].concat(),
        });
        landed
    }

    fn diagnose(&self, head: &[SqlRow]) -> Result<StoreError, StoreError> {
        let Some(head) = single(head)? else {
            return Ok(StoreError::Fenced);
        };
        if text(head, 0)? != self.owner.owner_id().as_str()
            || counter(head, 1)? != self.owner.fence()
        {
            return Ok(StoreError::Fenced);
        }
        let actual = counter(head, 2)?;
        if actual != self.expected_revision {
            return Ok(StoreError::Conflict {
                expected: self.expected_revision,
                actual,
            });
        }
        Ok(StoreError::Backend(format!(
            "SQL host refused to replace revision {actual} of state {} while reporting it as current",
            self.state_id
        )))
    }
}

fn group_documents(
    committing: &[(&Replacement<'_>, &Counters)],
) -> Result<[String; 2], StoreError> {
    let heads: Vec<GroupHead<'_>> = committing
        .iter()
        .map(|(replacement, counters)| GroupHead {
            state: replacement.state_id,
            owner: replacement.owner.owner_id().as_str(),
            fence: &counters.fence,
            expected: &counters.expected,
            revision: &counters.revision,
            payload: replacement.payload,
        })
        .collect();
    let records: Vec<GroupRecords<'_>> = committing
        .iter()
        .filter(|(replacement, _)| !replacement.records.is_empty())
        .map(|(replacement, counters)| GroupRecords {
            state: replacement.state_id,
            owner: replacement.owner.owner_id().as_str(),
            fence: &counters.fence,
            revision: &counters.revision,
            keys: replacement.keys(),
            records: replacement.values(),
        })
        .collect();
    let encode = |error: serde_json::Error| StoreError::NotCommitted(format!("records: {error}"));
    Ok([
        serde_json::to_string(&heads).map_err(encode)?,
        if records.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&records).map_err(encode)?
        },
    ])
}

fn group_statements<'a>(
    documents: &'a [String; 2],
    ttl: &'a str,
    statements: &mut Vec<SqlStatement<'a>>,
) {
    statements.push(SqlStatement {
        sql: GROUP_HEADS_PG,
        params: vec![&documents[0], ttl],
    });
    if documents[1].is_empty() {
        return;
    }
    for sql in [GROUP_REVIVE_PG, GROUP_RECORDS_PG, GROUP_HOLDS_PG] {
        statements.push(SqlStatement {
            sql,
            params: vec![&documents[1]],
        });
    }
}

const MIGRATIONS: [Migration; 4] = [
    Migration {
        statements: SCHEMA,
        adopt: None,
    },
    Migration {
        statements: SHARED,
        adopt: None,
    },
    Migration {
        statements: HOLDERS,
        adopt: None,
    },
    Migration {
        statements: LEASES,
        adopt: None,
    },
];

pub struct SqlStore<H: ?Sized> {
    host: Arc<H>,
    ready: Arc<std::sync::atomic::AtomicBool>,
    lease: Option<Lease>,
}

impl<H: Sql + ?Sized> SqlStore<H> {
    pub fn new(host: H) -> Self
    where
        H: Sized,
    {
        Self::shared(Arc::new(host))
    }

    pub fn shared(host: Arc<H>) -> Self {
        Self {
            host,
            ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lease: None,
        }
    }

    pub const FORMAT: u32 = MIGRATIONS.len() as u32;

    #[must_use]
    pub fn migrated(self) -> Self {
        self.ready.store(true, std::sync::atomic::Ordering::Release);
        self
    }

    #[must_use]
    pub const fn leased(mut self, lease: Lease) -> Self {
        self.lease = Some(lease);
        self
    }

    pub async fn migrate(&mut self) -> Result<(), StoreError> {
        migrate_format(&*self.host, "records", &MIGRATIONS).await?;
        self.ready.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    fn handle_on<T: Sql + ?Sized>(&self, host: Arc<T>) -> SqlStore<T> {
        SqlStore {
            host,
            ready: Arc::clone(&self.ready),
            lease: self.lease,
        }
    }

    fn lease_params(&self) -> (String, String) {
        self.lease.map_or_else(
            || (String::new(), String::new()),
            |lease| (lease.ttl_ms.to_string(), lease.grace_ms.to_string()),
        )
    }

    async fn checked(&self) -> Result<(), StoreError> {
        if self.ready.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        check_format(&*self.host, "records", Self::FORMAT).await?;
        self.ready.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    async fn transaction(
        &mut self,
        operation: &'static str,
        statements: Vec<SqlStatement<'_>>,
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        self.run(SqlAccess::Write, operation, statements).await
    }

    async fn run(
        &mut self,
        access: SqlAccess,
        operation: &'static str,
        statements: Vec<SqlStatement<'_>>,
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        self.checked().await?;
        run_checked(&*self.host, access, operation, &statements).await
    }

    async fn settled(
        &mut self,
        settle: Settle,
        operation: &'static str,
        statements: Vec<SqlStatement<'_>>,
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        self.checked().await?;
        written(&*self.host, settle, operation, &statements).await
    }

    async fn query(
        &mut self,
        operation: &'static str,
        sql: &Dialects,
        params: Vec<&str>,
    ) -> Result<Vec<SqlRow>, StoreError> {
        let dialect = self.host.dialect();
        let mut results = self
            .run(
                SqlAccess::Read,
                operation,
                vec![statement(dialect, sql, params)],
            )
            .await?;
        Ok(results.pop().unwrap_or_default())
    }

    async fn record(&mut self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .records(std::slice::from_ref(&key.to_owned()))
            .await?
            .pop()
            .flatten())
    }

    async fn records(&mut self, keys: &[String]) -> Result<Vec<Option<String>>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let parts = self
            .host
            .readers()
            .clamp(1, keys.len().div_ceil(READ_PART_KEYS).max(1));
        let rows = if parts == 1 {
            let encoded = keys_json(keys.iter())?;
            self.query("read_records", &RECORDS, vec![&encoded]).await?
        } else {
            self.checked().await?;
            let dialect = self.host.dialect();
            let encoded = keys
                .chunks(keys.len().div_ceil(parts))
                .map(|chunk| keys_json(chunk.iter()))
                .collect::<Result<Vec<_>, _>>()?;
            let statements = encoded
                .iter()
                .map(|encoded| [statement(dialect, &RECORDS, vec![encoded])])
                .collect::<Vec<_>>();
            let host = &*self.host;
            nanocode_sync::try_join_in_order(
                statements
                    .iter()
                    .map(|statement| run_checked(host, SqlAccess::Read, "read_records", statement)),
            )
            .await?
            .into_iter()
            .flat_map(|mut results| results.pop().unwrap_or_default())
            .collect()
        };
        if rows.len() != keys.len() {
            return Err(StoreError::Backend(format!(
                "SQL host answered {} of {} record reads",
                rows.len(),
                keys.len()
            )));
        }
        rows.into_iter()
            .map(|row| match <[Option<String>; 1]>::try_from(row) {
                Ok([value]) => Ok(value),
                Err(row) => Err(invalid(format!("{} columns for a record read", row.len()))),
            })
            .collect()
    }

    async fn state(&mut self, state_id: &str) -> Result<StoredState, StoreError> {
        stored(&self.query("peek", &PEEK, vec![state_id]).await?)
    }

    async fn state_ids(&mut self, limit: usize) -> Result<Vec<String>, StoreError> {
        let limit = limit_param(limit);
        names(&self.query("list_states", &STATES, vec![&limit]).await?)
    }

    async fn record_keys(&mut self, state_id: &str) -> Result<Vec<String>, StoreError> {
        names(
            &self
                .query("list_records", &RECORD_KEYS, vec![state_id])
                .await?,
        )
    }

    async fn current_owner(&mut self, state_id: &str) -> Result<Option<OwnerToken>, StoreError> {
        owner_token(&self.query("owner", &OWNER, vec![state_id]).await?)
    }

    async fn claim_refused(&mut self, state_id: &str) -> StoreError {
        let head = match self.query("head", &HEAD, vec![state_id]).await {
            Ok(head) => head,
            Err(error) => return error,
        };
        let fence = match single(&head) {
            Ok(Some(row)) => counter(row, 1),
            Ok(None) => return invalid(format!("claim of {state_id} refused without a row")),
            Err(error) => return error,
        };
        match fence {
            Ok(fence) if fence >= i64::MAX as u64 => {
                StoreError::NotCommitted("SQL durability owner fence overflow".to_owned())
            }
            Ok(_) => StoreError::Busy,
            Err(error) => error,
        }
    }

    async fn acquire_owner(
        &mut self,
        state_id: &str,
        owner_id: OwnerId,
    ) -> Result<OwnedState, StoreError> {
        if owner_id.as_str().is_empty() {
            return Err(unnamed_owner());
        }
        let dialect = self.host.dialect();
        let (ttl, grace) = self.lease_params();
        let results = self
            .transaction(
                "acquire",
                vec![
                    statement(
                        dialect,
                        &CLAIM,
                        vec![state_id, owner_id.as_str(), &ttl, &grace],
                    ),
                    statement(dialect, &PEEK, vec![state_id]),
                ],
            )
            .await?;
        let Some(acquired) = single(&results[0])? else {
            return Err(self.claim_refused(state_id).await);
        };
        let fence = counter(acquired, 0)?;
        Ok(OwnedState {
            owner: OwnerToken::new(owner_id, fence),
            state: stored(&results[1])?,
        })
    }

    async fn acquire_owners(
        &mut self,
        acquisitions: Vec<(String, OwnerId)>,
    ) -> Vec<Result<OwnedState, StoreError>> {
        let mut order = acquisitions.iter().collect::<Vec<_>>();
        order.sort_unstable_by_key(|(state_id, _)| state_id.as_str());
        order.dedup_by_key(|(state_id, _)| state_id.as_str());
        if order.len() != acquisitions.len()
            || acquisitions.len() == 1
            || order
                .iter()
                .any(|(_, owner_id)| owner_id.as_str().is_empty())
        {
            let mut results = Vec::with_capacity(acquisitions.len());
            for (state_id, owner_id) in acquisitions {
                results.push(self.acquire_owner(&state_id, owner_id).await);
            }
            return results;
        }
        let dialect = self.host.dialect();
        let (ttl, grace) = self.lease_params();
        let documents = serde_json::to_string(
            &order
                .iter()
                .map(|(state_id, owner_id)| [state_id.as_str(), owner_id.as_str()])
                .collect::<Vec<_>>(),
        )
        .map_err(|error| StoreError::NotCommitted(format!("owners: {error}")))
        .and_then(|owners| {
            Ok((
                owners,
                keys_json(order.iter().map(|(state_id, _)| state_id))?,
            ))
        });
        let acquired = match documents {
            Ok((owners, ids)) => {
                self.transaction(
                    "acquire_many",
                    vec![
                        statement(dialect, &CLAIM_MANY, vec![&owners, &ttl, &grace]),
                        statement(dialect, &PEEK_MANY, vec![&ids]),
                    ],
                )
                .await
            }
            Err(error) => Err(error),
        }
        .and_then(|results| {
            let fences = results[0]
                .iter()
                .map(|row| Ok((text(row, 0)?.to_owned(), counter(row, 1)?)))
                .collect::<Result<std::collections::HashMap<_, _>, StoreError>>()?;
            let heads = results[1]
                .iter()
                .map(|row| {
                    Ok((
                        text(row, 0)?.to_owned(),
                        StoredState {
                            revision: counter(row, 1)?,
                            payload: Some(text(row, 2)?.to_owned()),
                        },
                    ))
                })
                .collect::<Result<std::collections::HashMap<_, _>, StoreError>>()?;
            Ok((fences, heads))
        });
        let (fences, mut heads) = match acquired {
            Ok(acquired) => acquired,
            Err(error) => return acquisitions.iter().map(|_| Err(error.clone())).collect(),
        };
        let mut results = Vec::with_capacity(acquisitions.len());
        for (state_id, owner_id) in acquisitions {
            results.push(match fences.get(&state_id) {
                Some(fence) => Ok(OwnedState {
                    owner: OwnerToken::new(owner_id, *fence),
                    state: heads.remove(&state_id).unwrap_or_default(),
                }),
                None => Err(self.claim_refused(&state_id).await),
            });
        }
        results
    }

    async fn release_owner(
        &mut self,
        state_id: &str,
        owner: &OwnerToken,
    ) -> Result<bool, StoreError> {
        let dialect = self.host.dialect();
        let fence = owner.fence().to_string();
        let results = self
            .transaction(
                "release",
                vec![statement(
                    dialect,
                    &RELEASE,
                    vec![state_id, owner.owner_id().as_str(), &fence],
                )],
            )
            .await?;
        match single(&results[0])? {
            Some(_) => Ok(true),
            None => Err(StoreError::Fenced),
        }
    }

    async fn replace_state(
        &mut self,
        replacement: Replacement<'_>,
        settle: Settle,
    ) -> Result<u64, StoreError> {
        let counters = replacement.counters()?;
        let landed = self
            .write_group(&[(&replacement, &counters)], settle, "replace")
            .await?;
        match landed.into_iter().next().flatten() {
            Some(revision) => Ok(revision),
            None => Err(self.refusal(&replacement).await),
        }
    }

    async fn write_group(
        &mut self,
        committing: &[(&Replacement<'_>, &Counters)],
        settle: Settle,
        operation: &'static str,
    ) -> Result<Vec<Option<u64>>, StoreError> {
        let (ttl, _) = self.lease_params();
        let mut statements = Vec::new();
        let documents;
        let landed: Vec<usize> = match self.host.dialect() {
            SqlDialect::Postgres => {
                documents = group_documents(committing)?;
                group_statements(&documents, &ttl, &mut statements);
                vec![0; committing.len()]
            }
            SqlDialect::Sqlite => committing
                .iter()
                .map(|(replacement, counters)| replacement.sqlite(counters, &ttl, &mut statements))
                .collect(),
        };
        let results = self.settled(settle, operation, statements).await?;
        committing
            .iter()
            .zip(&landed)
            .map(|((replacement, _), at)| {
                for row in &results[*at] {
                    if text(row, 0)? == replacement.state_id {
                        return Ok(Some(counter(row, 1)?));
                    }
                }
                Ok(None)
            })
            .collect()
    }

    async fn put(&mut self, state_id: &str, records: &[StoreRecord]) -> Result<bool, StoreError> {
        if records.is_empty() {
            return Ok(true);
        }
        let dialect = self.host.dialect();
        let keys = keys_json(records.iter().map(|record| &record.key))?;
        let values = (dialect == SqlDialect::Postgres)
            .then(|| {
                let mut values: Vec<GroupValue<'_>> = records
                    .iter()
                    .map(|record| GroupValue {
                        key: &record.key,
                        value: &record.value,
                    })
                    .collect();
                values.sort_by(|left, right| left.key.cmp(right.key));
                values.dedup_by(|left, right| left.key == right.key);
                serde_json::to_string(&values)
                    .map_err(|error| StoreError::NotCommitted(format!("records: {error}")))
            })
            .transpose()?;
        let mut statements = vec![statement(dialect, &PUT_REVIVE, vec![&keys])];
        match &values {
            Some(values) => statements.push(SqlStatement {
                sql: PUT_RECORDS,
                params: vec![state_id, values],
            }),
            None => statements.extend(records.iter().map(|record| SqlStatement {
                sql: PUT_RECORD,
                params: vec![&record.key, &record.value, state_id],
            })),
        }
        statements.push(statement(dialect, &PUT_HOLDS, vec![state_id, &keys]));
        self.transaction("put_records", statements).await?;
        Ok(true)
    }

    pub async fn unreachable(&mut self, state_id: &str) -> Result<Unreachable, StoreError> {
        unreachable_in(self, state_id).await
    }

    pub async fn collect(
        &mut self,
        state_id: &str,
        owner: &OwnerToken,
        revision: u64,
        keys: &[String],
    ) -> Result<Option<Collected>, StoreError> {
        if keys.is_empty() {
            return Ok(Some(Collected::default()));
        }
        let dialect = self.host.dialect();
        let (ttl, _) = self.lease_params();
        let revision = revision.to_string();
        let fence = owner.fence().to_string();
        let mut sorted: Vec<&String> = keys.iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        let dead = keys_json(sorted.into_iter())?;
        let fenced = [state_id, owner.owner_id().as_str(), &fence, &revision];
        let statements = vec![
            statement(
                dialect,
                &COLLECT_HEAD,
                [&fenced[..], &[ttl.as_str()]].concat(),
            ),
            statement(
                dialect,
                &COLLECT_REFS,
                [&fenced[..], &[dead.as_str()]].concat(),
            ),
            statement(
                dialect,
                &COLLECT_HELD,
                [&fenced[..], &[dead.as_str()]].concat(),
            ),
            statement(
                dialect,
                &COLLECT_STAMP,
                [&fenced[..], &[dead.as_str()]].concat(),
            ),
        ];
        let results = self.transaction("collect", statements).await?;
        if results[0].is_empty() {
            return Ok(None);
        }
        Ok(Some(Collected {
            released: (results[1].len() + results[2].len()) as u64,
            deleted: 0,
        }))
    }

    pub async fn sweep(
        &mut self,
        state_id: &str,
        owner: &OwnerToken,
        batch: usize,
    ) -> Result<Option<Collected>, StoreError> {
        let unreachable = self.unreachable(state_id).await?;
        let mut collected = Collected::default();
        for keys in unreachable.keys.chunks(batch.max(1)) {
            let Some(swept) = self
                .collect(state_id, owner, unreachable.revision, keys)
                .await?
            else {
                return Ok(None);
            };
            collected.released += swept.released;
            collected.deleted += swept.deleted;
        }
        Ok(Some(collected))
    }

    pub async fn purge(&mut self, grace_ms: u64, limit: usize) -> Result<u64, StoreError> {
        self.purge_batch(grace_ms, limit, None).await
    }

    pub async fn purge_within(
        &mut self,
        grace_ms: u64,
        batch: usize,
        budget: Duration,
    ) -> Result<Purged, StoreError> {
        if batch == 0 {
            return Err(invalid("a purge batch of 0 records deletes nothing"));
        }
        let deadline = nanocode_platform::now() + budget;
        let mut purged = Purged::default();
        loop {
            let left = deadline.saturating_duration_since(nanocode_platform::now());
            if left.is_zero() {
                return Ok(purged);
            }
            let limit = if crate::sim::mutated(crate::sim::Mutation::PurgeUnbatched) {
                usize::MAX
            } else {
                batch
            };
            let deadline =
                (!crate::sim::mutated(crate::sim::Mutation::PurgeSkipsDeadline)).then_some(left);
            let deleted = self.purge_batch(grace_ms, limit, deadline).await?;
            purged.deleted += deleted;
            purged.batches += 1;
            if deleted < batch as u64
                || crate::sim::mutated(crate::sim::Mutation::PurgeFinishesEarly)
            {
                purged.finished = true;
                return Ok(purged);
            }
        }
    }

    async fn purge_batch(
        &mut self,
        grace_ms: u64,
        limit: usize,
        deadline: Option<Duration>,
    ) -> Result<u64, StoreError> {
        let grace = grace_ms.to_string();
        let limit = limit_param(limit);
        let dialect = self.host.dialect();
        let deadline_ms = deadline.map(|left| left.as_millis().max(1).to_string());
        let mut statements = Vec::with_capacity(2);
        if let (Some(deadline_ms), SqlDialect::Postgres) = (&deadline_ms, dialect) {
            statements.push(SqlStatement {
                sql: PURGE_DEADLINE_PG,
                params: vec![deadline_ms.as_str()],
            });
        }
        statements.push(statement(dialect, &PURGE, vec![&grace, &limit]));
        let results = self.transaction("purge", statements).await?;
        let deleted = results.last().map_or(0, Vec::len) as u64;
        if limit.parse::<u64>().is_ok_and(|limit| deleted > limit) {
            return Err(invalid(format!(
                "a purge with a limit of {limit} deleted {deleted} records"
            )));
        }
        Ok(deleted)
    }

    async fn refusal(&mut self, replacement: &Replacement<'_>) -> StoreError {
        match self.query("head", &HEAD, vec![replacement.state_id]).await {
            Ok(head) => replacement.diagnose(&head).unwrap_or_else(|error| error),
            Err(error) => error,
        }
    }

    async fn commit_all(
        &mut self,
        commits: Vec<OwnedCommit>,
    ) -> Vec<Result<Committed, StoreError>> {
        let prepared: Vec<Result<(Replacement<'_>, Counters), StoreError>> = commits
            .iter()
            .map(|commit| {
                if !commit.events.is_empty() {
                    return Err(StoreError::NotCommitted(
                        "this store does not record state events".to_owned(),
                    ));
                }
                let replacement = Replacement {
                    state_id: &commit.state_id,
                    owner: &commit.owner,
                    expected_revision: commit.expected_revision,
                    revision: commit.expected_revision.checked_add(1).ok_or_else(|| {
                        StoreError::NotCommitted("SQL durability revision overflow".to_owned())
                    })?,
                    payload: &commit.payload,
                    records: &commit.records,
                };
                let counters = replacement.counters()?;
                Ok((replacement, counters))
            })
            .collect();
        let settle = if commits
            .iter()
            .all(|commit| commit.settle == Settle::Ordered)
        {
            Settle::Ordered
        } else {
            Settle::Durable
        };
        let mut committing: Vec<(&Replacement<'_>, &Counters)> = prepared
            .iter()
            .flatten()
            .map(|(replacement, counters)| (replacement, counters))
            .collect();
        committing.sort_unstable_by_key(|(replacement, _)| replacement.state_id);
        let distinct = committing
            .windows(2)
            .all(|pair| pair[0].0.state_id != pair[1].0.state_id);
        let landed: Result<std::collections::HashMap<&str, Option<u64>>, StoreError> =
            if committing.is_empty() {
                Ok(std::collections::HashMap::new())
            } else if distinct {
                self.write_group(&committing, settle, "commit_many")
                    .await
                    .map(|landed| {
                        committing
                            .iter()
                            .map(|(replacement, _)| replacement.state_id)
                            .zip(landed)
                            .collect()
                    })
            } else {
                return self.commit_each(&prepared, settle).await;
            };
        let mut outcomes = Vec::with_capacity(prepared.len());
        for entry in &prepared {
            outcomes.push(match (entry, &landed) {
                (Err(error), _) | (Ok(_), Err(error)) => Err(error.clone()),
                (Ok((replacement, _)), Ok(landed)) => {
                    match landed.get(replacement.state_id).copied().flatten() {
                        Some(revision) => Ok(Committed {
                            revision,
                            first_event: None,
                        }),
                        None => Err(self.refusal(replacement).await),
                    }
                }
            });
        }
        outcomes
    }

    async fn commit_each(
        &mut self,
        prepared: &[Result<(Replacement<'_>, Counters), StoreError>],
        settle: Settle,
    ) -> Vec<Result<Committed, StoreError>> {
        let mut outcomes = Vec::with_capacity(prepared.len());
        for entry in prepared {
            outcomes.push(match entry {
                Err(error) => Err(error.clone()),
                Ok((replacement, counters)) => {
                    match self
                        .write_group(&[(replacement, counters)], settle, "commit_many")
                        .await
                    {
                        Ok(landed) => match landed.into_iter().next().flatten() {
                            Some(revision) => Ok(Committed {
                                revision,
                                first_event: None,
                            }),
                            None => Err(self.refusal(replacement).await),
                        },
                        Err(error) => Err(error),
                    }
                }
            });
        }
        outcomes
    }
}

fn unnamed_owner() -> StoreError {
    StoreError::NotCommitted("a SQL durability owner needs a non-empty id".to_owned())
}

impl<H: Sql + ?Sized> StateStore for SqlStore<H> {
    fn read_record<'a>(
        &'a mut self,
        _state_id: &'a str,
        key: &'a str,
    ) -> StoreFuture<'a, Result<Option<String>, StoreError>> {
        Box::pin(self.record(key))
    }

    fn read_records<'a>(
        &'a mut self,
        _state_id: &'a str,
        keys: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<Option<String>>, StoreError>> {
        Box::pin(self.records(keys))
    }

    fn peek<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<StoredState, StoreError>> {
        Box::pin(self.state(state_id))
    }

    fn list_states<'a>(
        &'a mut self,
        limit: usize,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        Box::pin(self.state_ids(limit))
    }

    fn list_records<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        Box::pin(self.record_keys(state_id))
    }

    fn owner<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<Option<OwnerToken>, StoreError>> {
        Box::pin(self.current_owner(state_id))
    }

    fn acquire<'a>(
        &'a mut self,
        state_id: &'a str,
        owner_id: OwnerId,
    ) -> StoreFuture<'a, Result<OwnedState, StoreError>> {
        Box::pin(self.acquire_owner(state_id, owner_id))
    }

    fn acquire_many(
        &mut self,
        acquisitions: Vec<(String, OwnerId)>,
    ) -> StoreFuture<'_, Vec<Result<OwnedState, StoreError>>> {
        Box::pin(self.acquire_owners(acquisitions))
    }

    fn release<'a>(
        &'a mut self,
        state_id: &'a str,
        owner: &'a OwnerToken,
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        Box::pin(self.release_owner(state_id, owner))
    }

    fn replace<'a>(
        &'a mut self,
        state_id: &'a str,
        owner: &'a OwnerToken,
        expected_revision: u64,
        payload: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            self.replace_state(
                Replacement {
                    state_id,
                    owner,
                    expected_revision,
                    revision: expected_revision.checked_add(1).ok_or_else(|| {
                        StoreError::NotCommitted("SQL durability revision overflow".to_owned())
                    })?,
                    payload,
                    records,
                },
                Settle::Durable,
            )
            .await
        })
    }

    fn commit<'a>(
        &'a mut self,
        state_id: &'a str,
        owner: &'a OwnerToken,
        expected_revision: u64,
        commit: StateCommit<'a>,
    ) -> StoreFuture<'a, Result<Committed, StoreError>> {
        Box::pin(async move {
            if !commit.events.is_empty() {
                return Err(StoreError::NotCommitted(
                    "this store does not record state events".to_owned(),
                ));
            }
            let revision = self
                .replace_state(
                    Replacement {
                        state_id,
                        owner,
                        expected_revision,
                        revision: expected_revision.checked_add(1).ok_or_else(|| {
                            StoreError::NotCommitted("SQL durability revision overflow".to_owned())
                        })?,
                        payload: commit.payload,
                        records: commit.records,
                    },
                    commit.settle,
                )
                .await?;
            Ok(Committed {
                revision,
                first_event: None,
            })
        })
    }

    fn put_records<'a>(
        &'a mut self,
        state_id: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        Box::pin(self.put(state_id, records))
    }

    fn replaces_to(&self) -> bool {
        true
    }

    fn replace_to<'a>(
        &'a mut self,
        state_id: &'a str,
        owner: &'a OwnerToken,
        expected_revision: u64,
        revision: u64,
        payload: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<Option<u64>, StoreError>> {
        Box::pin(async move {
            self.replace_state(
                Replacement {
                    state_id,
                    owner,
                    expected_revision,
                    revision,
                    payload,
                    records,
                },
                Settle::Durable,
            )
            .await
            .map(Some)
        })
    }

    fn commit_many(
        &mut self,
        commits: Vec<OwnedCommit>,
    ) -> StoreFuture<'_, Vec<Result<Committed, StoreError>>> {
        Box::pin(self.commit_all(commits))
    }

    fn group_writers(&self) -> usize {
        self.host.writers()
    }

    fn pipeline_handle(&self) -> Option<Box<dyn StateStore>> {
        let host = Arc::clone(&self.host).concurrent()?;
        Some(Box::new(self.handle_on(host)))
    }

    #[cfg(not(target_family = "wasm"))]
    fn handles(&self) -> Option<Arc<dyn crate::StoreHandles>> {
        let host = Arc::clone(&self.host).concurrent()?;
        Some(Arc::new(self.handle_on(host)))
    }
}

#[cfg(not(target_family = "wasm"))]
impl<H: Sql + ?Sized + 'static> crate::StoreHandles for SqlStore<H> {
    fn handle(&self) -> Box<dyn StateStore> {
        Box::new(self.handle_on(Arc::clone(&self.host)))
    }
}
