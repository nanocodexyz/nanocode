use crate::{
    model::{Corrupt, Digest, Entry, Manifest, Refusal},
    objects::Objects,
    sim::{Mutation, mutated},
    store::{Batch, Blob, Failure, Store},
};
use nanocode_bytes::Bytes;
use nanocode_durability_core::{
    Migrated, Migration, Sql, SqlAccess, SqlDialect, SqlRow, SqlStatement, StoreError, StoreFuture,
    migrate_format,
};
use nanocode_sys::base64::STANDARD;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

type Dialects = [&'static str; 2];

const COMPONENT: &str = "workspace_fs";

const SCHEMA_SQLITE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_heads (workspace TEXT PRIMARY KEY, version INTEGER NOT NULL, token TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_entries (workspace TEXT NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL, digest TEXT, size INTEGER NOT NULL, mode INTEGER NOT NULL, mtime INTEGER NOT NULL, target TEXT, PRIMARY KEY (workspace, path))",
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_blobs (digest TEXT PRIMARY KEY, data TEXT NOT NULL, refs INTEGER NOT NULL)",
];

const SCHEMA_POSTGRES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_heads (workspace TEXT PRIMARY KEY, version BIGINT NOT NULL, token TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_entries (workspace TEXT NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL, digest TEXT, size BIGINT NOT NULL, mode BIGINT NOT NULL, mtime BIGINT NOT NULL, target TEXT, PRIMARY KEY (workspace, path))",
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_blobs (digest TEXT PRIMARY KEY, data TEXT NOT NULL, refs BIGINT NOT NULL)",
];

const DEAD_INDEX: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS nanocode_workspace_blobs_dead ON nanocode_workspace_blobs (digest) WHERE refs <= 0",
];

const REFS_SQLITE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_refs (digest TEXT NOT NULL, workspace TEXT NOT NULL, PRIMARY KEY (digest, workspace)) WITHOUT ROWID",
    "INSERT INTO nanocode_workspace_refs (digest, workspace) SELECT DISTINCT digest, workspace FROM nanocode_workspace_entries WHERE digest IS NOT NULL ON CONFLICT DO NOTHING",
    "ALTER TABLE nanocode_workspace_blobs ADD COLUMN orphaned INTEGER",
    "UPDATE nanocode_workspace_blobs SET orphaned = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE data NOT LIKE '@gone:%' AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_refs WHERE nanocode_workspace_refs.digest = nanocode_workspace_blobs.digest)",
    "DROP INDEX IF EXISTS nanocode_workspace_blobs_dead",
    "ALTER TABLE nanocode_workspace_blobs DROP COLUMN refs",
    "CREATE INDEX IF NOT EXISTS nanocode_workspace_blobs_gone ON nanocode_workspace_blobs (digest) WHERE data LIKE '@gone:%'",
    "CREATE INDEX IF NOT EXISTS nanocode_workspace_blobs_orphaned ON nanocode_workspace_blobs (orphaned) WHERE orphaned IS NOT NULL",
];

const REFS_POSTGRES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_refs (digest TEXT NOT NULL, workspace TEXT NOT NULL, PRIMARY KEY (digest, workspace))",
    "INSERT INTO nanocode_workspace_refs (digest, workspace) SELECT DISTINCT digest, workspace FROM nanocode_workspace_entries WHERE digest IS NOT NULL ON CONFLICT DO NOTHING",
    "ALTER TABLE nanocode_workspace_blobs ADD COLUMN IF NOT EXISTS orphaned BIGINT",
    "UPDATE nanocode_workspace_blobs SET orphaned = CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) WHERE orphaned IS NULL AND data NOT LIKE '@gone:%' AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_refs WHERE nanocode_workspace_refs.digest = nanocode_workspace_blobs.digest)",
    "DROP INDEX IF EXISTS nanocode_workspace_blobs_dead",
    "ALTER TABLE nanocode_workspace_blobs DROP COLUMN IF EXISTS refs",
    "CREATE INDEX IF NOT EXISTS nanocode_workspace_blobs_gone ON nanocode_workspace_blobs (digest) WHERE data LIKE '@gone:%'",
    "CREATE INDEX IF NOT EXISTS nanocode_workspace_blobs_orphaned ON nanocode_workspace_blobs (orphaned) WHERE orphaned IS NOT NULL",
];

const MIGRATIONS: [Migration; 3] = [
    Migration {
        statements: [SCHEMA_SQLITE, SCHEMA_POSTGRES],
        adopt: None,
    },
    Migration {
        statements: [DEAD_INDEX, DEAD_INDEX],
        adopt: None,
    },
    Migration {
        statements: [REFS_SQLITE, REFS_POSTGRES],
        adopt: None,
    },
];

const LOAD: Dialects = [
    "SELECT CAST(h.version AS TEXT), e.path, e.kind, e.digest, CAST(e.size AS TEXT), CAST(e.mode AS TEXT), CAST(e.mtime AS TEXT), e.target FROM nanocode_workspace_heads AS h LEFT JOIN nanocode_workspace_entries AS e ON e.workspace = h.workspace WHERE h.workspace = ?1 ORDER BY e.path",
    "SELECT CAST(h.version AS TEXT), e.path, e.kind, e.digest, CAST(e.size AS TEXT), CAST(e.mode AS TEXT), CAST(e.mtime AS TEXT), e.target FROM nanocode_workspace_heads AS h LEFT JOIN nanocode_workspace_entries AS e ON e.workspace = h.workspace WHERE h.workspace = $1::text ORDER BY e.path",
];

const CLAIM_FIRST: Dialects = [
    "INSERT INTO nanocode_workspace_heads (workspace, version, token) SELECT ?1, 1, ?2 WHERE NOT EXISTS (SELECT 1 FROM nanocode_workspace_blobs WHERE digest IN (SELECT value FROM json_each(?3)) AND data LIKE '@gone:%') ON CONFLICT (workspace) DO NOTHING",
    "INSERT INTO nanocode_workspace_heads (workspace, version, token) SELECT $1::text, 1, $2::text WHERE NOT EXISTS (SELECT 1 FROM nanocode_workspace_blobs WHERE digest IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND data LIKE '@gone:%') ON CONFLICT (workspace) DO NOTHING",
];

const CLAIM_NEXT: Dialects = [
    "UPDATE nanocode_workspace_heads SET version = version + 1, token = ?2 WHERE workspace = ?1 AND version = CAST(?3 AS INTEGER) AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_blobs WHERE digest IN (SELECT value FROM json_each(?4)) AND data LIKE '@gone:%')",
    "UPDATE nanocode_workspace_heads SET version = version + 1, token = $2::text WHERE workspace = $1::text AND version = CAST($3::text AS BIGINT) AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_blobs WHERE digest IN (SELECT jsonb_array_elements_text($4::text::jsonb)) AND data LIKE '@gone:%')",
];

const CLAIM_ANY: Dialects = [
    "UPDATE nanocode_workspace_heads SET version = version + 1, token = ?2 WHERE workspace = ?1 AND ?3 IS NOT NULL AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_blobs WHERE digest IN (SELECT value FROM json_each(?4)) AND data LIKE '@gone:%')",
    "UPDATE nanocode_workspace_heads SET version = version + 1, token = $2::text WHERE workspace = $1::text AND $3::text IS NOT NULL AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_blobs WHERE digest IN (SELECT jsonb_array_elements_text($4::text::jsonb)) AND data LIKE '@gone:%')",
];

const CLEAR: Dialects = [
    "DELETE FROM nanocode_workspace_entries WHERE workspace = ?1 AND path IN (SELECT value FROM json_each(?3)) AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "DELETE FROM nanocode_workspace_entries WHERE workspace = $1::text AND path IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const PUT: Dialects = [
    "INSERT INTO nanocode_workspace_entries (workspace, path, kind, digest, size, mode, mtime, target) SELECT ?1, json_extract(value, '$[0]'), json_extract(value, '$[1]'), json_extract(value, '$[2]'), CAST(json_extract(value, '$[3]') AS INTEGER), CAST(json_extract(value, '$[4]') AS INTEGER), CAST(json_extract(value, '$[5]') AS INTEGER), json_extract(value, '$[6]') FROM json_each(?3) WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "INSERT INTO nanocode_workspace_entries (workspace, path, kind, digest, size, mode, mtime, target) SELECT $1::text, e->>0, e->>1, e->>2, CAST(e->>3 AS BIGINT), CAST(e->>4 AS BIGINT), CAST(e->>5 AS BIGINT), e->>6 FROM jsonb_array_elements($3::text::jsonb) AS e WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const REVIVE: Dialects = [
    "UPDATE nanocode_workspace_blobs SET orphaned = NULL WHERE digest IN (SELECT value FROM json_each(?3)) AND orphaned IS NOT NULL AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "UPDATE nanocode_workspace_blobs SET orphaned = NULL FROM (SELECT e FROM jsonb_array_elements_text($3::text::jsonb) AS e ORDER BY e) AS fresh WHERE nanocode_workspace_blobs.digest = fresh.e AND nanocode_workspace_blobs.orphaned IS NOT NULL AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const ADD_BLOB: Dialects = [
    "INSERT INTO nanocode_workspace_blobs (digest, data) SELECT ?3, ?4 WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2) ON CONFLICT (digest) DO NOTHING",
    "INSERT INTO nanocode_workspace_blobs (digest, data) SELECT $3::text, $4::text WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text) ON CONFLICT (digest) DO NOTHING",
];

const ADD_REFS: Dialects = [
    "INSERT INTO nanocode_workspace_refs (digest, workspace) SELECT value, ?1 FROM json_each(?3) WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "INSERT INTO nanocode_workspace_refs (digest, workspace) SELECT e, $1::text FROM jsonb_array_elements_text($3::text::jsonb) AS e WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const DROP_REFS: Dialects = [
    "DELETE FROM nanocode_workspace_refs WHERE workspace = ?1 AND digest IN (SELECT value FROM json_each(?3)) AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "DELETE FROM nanocode_workspace_refs WHERE workspace = $1::text AND digest IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const DROP_EARLY: Dialects = [
    "DELETE FROM nanocode_workspace_refs WHERE workspace = ?1 AND digest IN (SELECT value FROM json_each(?3)) AND ?2 IS NOT NULL",
    "DELETE FROM nanocode_workspace_refs WHERE workspace = $1::text AND digest IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND $2::text IS NOT NULL",
];

const PAGE: Dialects = [
    "SELECT digest FROM nanocode_workspace_blobs WHERE digest > ?1 ORDER BY digest LIMIT CAST(?2 AS INTEGER)",
    "SELECT digest FROM nanocode_workspace_blobs WHERE digest > $1::text ORDER BY digest LIMIT CAST($2::text AS BIGINT)",
];

const UNMARK: Dialects = [
    "UPDATE nanocode_workspace_blobs SET orphaned = NULL WHERE digest IN (SELECT digest FROM nanocode_workspace_blobs WHERE digest > ?1 ORDER BY digest LIMIT CAST(?2 AS INTEGER)) AND orphaned IS NOT NULL AND EXISTS (SELECT 1 FROM nanocode_workspace_refs WHERE nanocode_workspace_refs.digest = nanocode_workspace_blobs.digest) RETURNING digest",
    "UPDATE nanocode_workspace_blobs SET orphaned = NULL FROM (SELECT digest FROM nanocode_workspace_blobs WHERE digest > $1::text ORDER BY digest LIMIT CAST($2::text AS BIGINT)) AS page WHERE nanocode_workspace_blobs.digest = page.digest AND nanocode_workspace_blobs.orphaned IS NOT NULL AND EXISTS (SELECT 1 FROM nanocode_workspace_refs WHERE nanocode_workspace_refs.digest = nanocode_workspace_blobs.digest) RETURNING nanocode_workspace_blobs.digest",
];

const MARK: Dialects = [
    "UPDATE nanocode_workspace_blobs SET orphaned = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE digest IN (SELECT digest FROM nanocode_workspace_blobs WHERE digest > ?1 ORDER BY digest LIMIT CAST(?2 AS INTEGER)) AND orphaned IS NULL AND data NOT LIKE '@gone:%' AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_refs WHERE nanocode_workspace_refs.digest = nanocode_workspace_blobs.digest) RETURNING digest",
    "UPDATE nanocode_workspace_blobs SET orphaned = CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) FROM (SELECT digest FROM nanocode_workspace_blobs WHERE digest > $1::text ORDER BY digest LIMIT CAST($2::text AS BIGINT)) AS page WHERE nanocode_workspace_blobs.digest = page.digest AND nanocode_workspace_blobs.orphaned IS NULL AND nanocode_workspace_blobs.data NOT LIKE '@gone:%' AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_refs WHERE nanocode_workspace_refs.digest = nanocode_workspace_blobs.digest) RETURNING nanocode_workspace_blobs.digest",
];

const MARK_ANY: Dialects = [
    "UPDATE nanocode_workspace_blobs SET orphaned = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE digest IN (SELECT digest FROM nanocode_workspace_blobs WHERE digest > ?1 ORDER BY digest LIMIT CAST(?2 AS INTEGER)) AND orphaned IS NULL AND data NOT LIKE '@gone:%' RETURNING digest",
    "UPDATE nanocode_workspace_blobs SET orphaned = CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) FROM (SELECT digest FROM nanocode_workspace_blobs WHERE digest > $1::text ORDER BY digest LIMIT CAST($2::text AS BIGINT)) AS page WHERE nanocode_workspace_blobs.digest = page.digest AND nanocode_workspace_blobs.orphaned IS NULL AND nanocode_workspace_blobs.data NOT LIKE '@gone:%' RETURNING nanocode_workspace_blobs.digest",
];

const PURGE: Dialects = [
    "DELETE FROM nanocode_workspace_blobs WHERE digest IN (SELECT digest FROM nanocode_workspace_blobs WHERE orphaned IS NOT NULL AND orphaned < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) - CAST(?1 AS INTEGER) ORDER BY orphaned, digest LIMIT CAST(?2 AS INTEGER)) AND orphaned < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) - CAST(?1 AS INTEGER) AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_refs WHERE nanocode_workspace_refs.digest = nanocode_workspace_blobs.digest) RETURNING digest",
    "DELETE FROM nanocode_workspace_blobs USING (SELECT digest FROM nanocode_workspace_blobs WHERE orphaned IS NOT NULL AND orphaned < CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) - CAST($1::text AS BIGINT) ORDER BY orphaned, digest LIMIT CAST($2::text AS BIGINT)) AS dead WHERE nanocode_workspace_blobs.digest = dead.digest AND nanocode_workspace_blobs.orphaned < CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) - CAST($1::text AS BIGINT) AND NOT EXISTS (SELECT 1 FROM nanocode_workspace_refs WHERE nanocode_workspace_refs.digest = nanocode_workspace_blobs.digest) RETURNING nanocode_workspace_blobs.digest",
];

const PURGE_ANY: Dialects = [
    "DELETE FROM nanocode_workspace_blobs WHERE digest IN (SELECT digest FROM nanocode_workspace_blobs WHERE orphaned IS NOT NULL AND orphaned < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) - CAST(?1 AS INTEGER) ORDER BY orphaned, digest LIMIT CAST(?2 AS INTEGER)) AND orphaned < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) - CAST(?1 AS INTEGER) RETURNING digest",
    "DELETE FROM nanocode_workspace_blobs USING (SELECT digest FROM nanocode_workspace_blobs WHERE orphaned IS NOT NULL AND orphaned < CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) - CAST($1::text AS BIGINT) ORDER BY orphaned, digest LIMIT CAST($2::text AS BIGINT)) AS dead WHERE nanocode_workspace_blobs.digest = dead.digest AND nanocode_workspace_blobs.orphaned < CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) - CAST($1::text AS BIGINT) RETURNING nanocode_workspace_blobs.digest",
];

const HEAD: Dialects = [
    "SELECT CAST(version AS TEXT), token FROM nanocode_workspace_heads WHERE workspace = ?1",
    "SELECT CAST(version AS TEXT), token FROM nanocode_workspace_heads WHERE workspace = $1::text",
];

const BLOB: Dialects = [
    "SELECT data FROM nanocode_workspace_blobs WHERE digest = ?1",
    "SELECT data FROM nanocode_workspace_blobs WHERE digest = $1::text",
];

const TOMBSTONE: Dialects = [
    "INSERT INTO nanocode_workspace_blobs (digest, data) SELECT value, ?2 FROM json_each(?1) WHERE true ON CONFLICT (digest) DO NOTHING",
    "INSERT INTO nanocode_workspace_blobs (digest, data) SELECT e, $2::text FROM jsonb_array_elements_text($1::text::jsonb) AS e ON CONFLICT (digest) DO NOTHING",
];

const DEAD: Dialects = [
    "SELECT digest, data FROM nanocode_workspace_blobs WHERE data LIKE '@gone:%' ORDER BY digest LIMIT CAST(?1 AS INTEGER)",
    "SELECT digest, data FROM nanocode_workspace_blobs WHERE data LIKE '@gone:%' ORDER BY digest LIMIT CAST($1::text AS BIGINT)",
];

const EXPIRE: Dialects = [
    "DELETE FROM nanocode_workspace_blobs WHERE digest IN (SELECT value FROM json_each(?1)) AND data IN (SELECT value FROM json_each(?2))",
    "DELETE FROM nanocode_workspace_blobs WHERE digest IN (SELECT jsonb_array_elements_text($1::text::jsonb)) AND data IN (SELECT jsonb_array_elements_text($2::text::jsonb))",
];

const OBJECT: &str = "@object";
const GONE: &str = "@gone:";
const OBJECT_PREFIX: &str = "blobs/";
const STAMP_DIGITS: usize = 20;

const FILE: &str = "file";
const DIR: &str = "dir";
const SYMLINK: &str = "symlink";

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sweep {
    pub settle_ms: u64,
    pub window_ms: u64,
    pub put_ms: u64,
}

impl Sweep {
    pub const DEFAULT: Self = Self {
        settle_ms: 5 * 60 * 1000,
        window_ms: 30 * 60 * 1000,
        put_ms: 10 * 60 * 1000,
    };

    #[must_use]
    pub const fn grace_ms(self) -> u64 {
        self.settle_ms
            .saturating_add(self.window_ms)
            .saturating_add(self.put_ms)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub next: Option<String>,
    pub marked: usize,
    pub unmarked: usize,
    pub deleted: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub next: Option<String>,
    pub marked: usize,
    pub deleted: usize,
    pub expired: usize,
}

struct Bucket {
    objects: Arc<dyn Objects>,
    sweep: Sweep,
    clock: Clock,
}

pub struct SqlStore {
    host: Arc<dyn Sql>,
    dialect: SqlDialect,
    bucket: Option<Bucket>,
}

fn object_key(hex: &str) -> String {
    format!("{OBJECT_PREFIX}{hex}")
}

fn stamp(data: &str) -> Option<u64> {
    data.strip_prefix(GONE)?.get(..STAMP_DIGITS)?.parse().ok()
}

#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(|| {
        nanocode_platform::system_time()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    })
}

fn backend(error: StoreError) -> Failure {
    Failure::Backend(error.to_string())
}

fn corrupt(path: &str, why: &'static str) -> Failure {
    Failure::Corrupt(Corrupt {
        path: path.to_owned(),
        why,
    })
}

fn column(row: &SqlRow, at: usize) -> Option<&str> {
    row.get(at).and_then(Option::as_deref)
}

fn number<T: std::str::FromStr>(row: &SqlRow, at: usize, path: &str) -> Result<T, Failure> {
    column(row, at)
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| corrupt(path, "a stored number does not parse"))
}

fn entry_row(path: &str, entry: &Entry) -> Value {
    let text = |value: String| Value::String(value);
    match entry {
        Entry::File {
            digest,
            size,
            mode,
            mtime_ns,
            link,
        } => Value::Array(vec![
            text(path.to_owned()),
            text(FILE.to_owned()),
            text(digest.hex()),
            text(size.to_string()),
            text(mode.to_string()),
            text(mtime_ns.to_string()),
            link.map_or(Value::Null, |link| text(link.to_string())),
        ]),
        Entry::Dir { mode, mtime_ns } => Value::Array(vec![
            text(path.to_owned()),
            text(DIR.to_owned()),
            Value::Null,
            text("0".to_owned()),
            text(mode.to_string()),
            text(mtime_ns.to_string()),
            Value::Null,
        ]),
        Entry::Symlink { target, mtime_ns } => Value::Array(vec![
            text(path.to_owned()),
            text(SYMLINK.to_owned()),
            Value::Null,
            text("0".to_owned()),
            text("0".to_owned()),
            text(mtime_ns.to_string()),
            text(target.clone()),
        ]),
    }
}

fn parse_entry(row: &SqlRow) -> Result<Option<(String, Entry)>, Failure> {
    let Some(path) = column(row, 1) else {
        return Ok(None);
    };
    let mtime_ns = number(row, 6, path)?;
    let entry = match column(row, 2) {
        Some(FILE) => Entry::File {
            digest: column(row, 3)
                .and_then(Digest::parse)
                .ok_or_else(|| corrupt(path, "a stored digest does not parse"))?,
            size: number(row, 4, path)?,
            mode: number(row, 5, path)?,
            mtime_ns,
            link: column(row, 7).map(|_| number(row, 7, path)).transpose()?,
        },
        Some(DIR) => Entry::Dir {
            mode: number(row, 5, path)?,
            mtime_ns,
        },
        Some(SYMLINK) => Entry::Symlink {
            target: column(row, 7)
                .ok_or_else(|| corrupt(path, "a stored symlink has no target"))?
                .to_owned(),
            mtime_ns,
        },
        _ => return Err(corrupt(path, "a stored entry has an unknown kind")),
    };
    Ok(Some((path.to_owned(), entry)))
}

fn json(values: Vec<Value>) -> String {
    Value::Array(values).to_string()
}

fn claimed_head(
    results: &[Vec<SqlRow>],
    token: &str,
    base: u64,
    version: u64,
) -> Result<u64, Failure> {
    let head = results
        .last()
        .and_then(|rows| rows.first())
        .map(|row| -> Result<(u64, bool), Failure> {
            Ok((number(row, 0, "")?, column(row, 1) == Some(token)))
        })
        .transpose()?;
    match head {
        Some((found, true)) if found == version => Ok(found),
        Some((_, true)) => Err(corrupt("", "a claimed head skipped a version")),
        Some((head, false)) => Err(Failure::Refused(Refusal::Stale { base, head })),
        None => Err(Failure::Refused(Refusal::Stale { base, head: 0 })),
    }
}

impl SqlStore {
    pub const FORMAT: u32 = MIGRATIONS.len() as u32;

    pub async fn migrate(host: &dyn Sql) -> Result<Migrated, Failure> {
        Migrated::migrate(host, COMPONENT, &MIGRATIONS)
            .await
            .map_err(backend)
    }

    pub async fn check(host: &dyn Sql) -> Result<Migrated, Failure> {
        Migrated::check(host, COMPONENT, &MIGRATIONS)
            .await
            .map_err(backend)
    }

    #[must_use]
    pub fn open(host: Arc<dyn Sql>, migrated: &Migrated) -> Self {
        migrated.assert_opens(COMPONENT, &MIGRATIONS);
        let dialect = host.dialect();
        Self {
            host,
            dialect,
            bucket: None,
        }
    }

    #[must_use]
    pub fn with_objects(
        host: Arc<dyn Sql>,
        migrated: &Migrated,
        objects: Arc<dyn Objects>,
        sweep: Sweep,
        clock: Clock,
    ) -> Self {
        let mut store = Self::open(host, migrated);
        store.bucket = Some(Bucket {
            objects,
            sweep,
            clock,
        });
        store
    }

    async fn put_objects(&self, batch: &Batch) -> Result<(), Failure> {
        let Some(bucket) = &self.bucket else {
            return Ok(());
        };
        let started = (bucket.clock)();
        let mut blobs: Vec<&Blob> = batch.blobs().iter().collect();
        blobs.sort_unstable_by_key(|blob| blob.digest().hex());
        for blob in blobs {
            let bytes = Bytes::copy_from_slice(blob.bytes());
            bucket
                .objects
                .put(&object_key(&blob.digest().hex()), &bytes)
                .await?;
        }
        if (bucket.clock)().saturating_sub(started) > bucket.sweep.put_ms {
            return Err(Failure::Backend(
                "object puts outlasted the sweep's put window".to_owned(),
            ));
        }
        Ok(())
    }

    pub async fn sweep(&self, after: &str, limit: usize) -> Result<Swept, Failure> {
        let Some(bucket) = &self.bucket else {
            return Ok(Swept::default());
        };
        let mut swept = self.reap(bucket, limit).await?;
        let listed = bucket.objects.list(OBJECT_PREFIX, after, limit).await?;
        swept.next = (listed.len() >= limit)
            .then(|| listed.last().map(|last| last.key.clone()))
            .flatten();
        let now = (bucket.clock)();
        let young = bucket.sweep.put_ms.saturating_add(bucket.sweep.settle_ms);
        let mut old: Vec<String> = listed
            .iter()
            .filter(|listed| listed.modified_ms.saturating_add(young) < now)
            .filter_map(|listed| listed.key.strip_prefix(OBJECT_PREFIX))
            .filter(|hex| Digest::parse(hex).is_some())
            .map(str::to_owned)
            .collect();
        old.sort_unstable();
        if old.is_empty() {
            return Ok(swept);
        }
        if mutated(Mutation::SqlSweepSkipsTombstone) {
            for hex in &old {
                bucket.objects.delete(&object_key(hex)).await?;
                swept.deleted += 1;
            }
            return Ok(swept);
        }
        let mark = format!(
            "{GONE}{now:0width$}:{}",
            nanocode_platform::uuid_v4().simple(),
            width = STAMP_DIGITS
        );
        let marked = old.len();
        let list = json(old.into_iter().map(Value::String).collect());
        let statements = [SqlStatement {
            sql: self.pick(&TOMBSTONE),
            params: vec![&list, &mark],
        }];
        self.host
            .transaction("workspace_tombstone", &statements)
            .await
            .map_err(backend)?;
        swept.marked = marked;
        Ok(swept)
    }

    pub async fn collect(
        &self,
        after: &str,
        grace_ms: u64,
        limit: usize,
    ) -> Result<Collected, Failure> {
        let (limit_text, grace) = (limit.to_string(), grace_ms.to_string());
        let page = vec![after, limit_text.as_str()];
        let ignores = mutated(Mutation::SqlSweepIgnoresRefs);
        let statements = [
            SqlStatement {
                sql: self.pick(&PAGE),
                params: page.clone(),
            },
            SqlStatement {
                sql: self.pick(&UNMARK),
                params: page.clone(),
            },
            SqlStatement {
                sql: self.pick(if ignores { &MARK_ANY } else { &MARK }),
                params: page,
            },
            SqlStatement {
                sql: self.pick(if ignores { &PURGE_ANY } else { &PURGE }),
                params: vec![&grace, &limit_text],
            },
        ];
        let results = self
            .host
            .transaction("workspace_collect", &statements)
            .await
            .map_err(backend)?;
        if results.len() != statements.len() {
            return Err(Failure::Backend(format!(
                "the SQL host answered {} of {} statements",
                results.len(),
                statements.len()
            )));
        }
        let paged = &results[0];
        let next = (paged.len() >= limit)
            .then(|| {
                paged
                    .last()
                    .and_then(|row| column(row, 0))
                    .map(str::to_owned)
            })
            .flatten();
        Ok(Collected {
            next,
            unmarked: results[1].len(),
            marked: results[2].len(),
            deleted: results[3].len(),
        })
    }

    async fn reap(&self, bucket: &Bucket, limit: usize) -> Result<Swept, Failure> {
        let mut swept = Swept::default();
        let limit_text = limit.to_string();
        let statements = [SqlStatement {
            sql: self.pick(&DEAD),
            params: vec![&limit_text],
        }];
        let results = self
            .host
            .run(SqlAccess::Read, "workspace_dead", &statements)
            .await
            .map_err(backend)?;
        let now = (bucket.clock)();
        let sweep = bucket.sweep;
        let mut expired = (Vec::new(), Vec::new());
        for row in results.first().into_iter().flatten() {
            let (Some(hex), Some(data)) = (column(row, 0), column(row, 1)) else {
                continue;
            };
            let Some(at) = stamp(data) else {
                continue;
            };
            let age = now.saturating_sub(at);
            if age >= sweep.grace_ms() {
                expired.0.push(Value::String(hex.to_owned()));
                expired.1.push(Value::String(data.to_owned()));
            } else if age >= sweep.settle_ms && age < sweep.settle_ms + sweep.window_ms {
                bucket.objects.delete(&object_key(hex)).await?;
                swept.deleted += 1;
            }
        }
        if !expired.0.is_empty() {
            swept.expired = expired.0.len();
            let (digests, marks) = (json(expired.0), json(expired.1));
            let statements = [SqlStatement {
                sql: self.pick(&EXPIRE),
                params: vec![&digests, &marks],
            }];
            self.host
                .transaction("workspace_expire", &statements)
                .await
                .map_err(backend)?;
        }
        Ok(swept)
    }

    const fn pick(&self, sql: &Dialects) -> &'static str {
        self.dialect.pick(sql)
    }

    async fn load_manifest(&self, workspace: &str) -> Result<Manifest, Failure> {
        if mutated(Mutation::SqlOpenMigrates) {
            migrate_format(&*self.host, COMPONENT, &MIGRATIONS)
                .await
                .map_err(backend)?;
        }
        let statements = [SqlStatement {
            sql: self.pick(&LOAD),
            params: vec![workspace],
        }];
        let results = self
            .host
            .run(SqlAccess::Read, "workspace_load", &statements)
            .await
            .map_err(backend)?;
        let rows = results
            .first()
            .ok_or_else(|| Failure::Backend("the SQL host answered no rows set".to_owned()))?;
        let Some(first) = rows.first() else {
            return Ok(Manifest::default());
        };
        let version: u64 = number(first, 0, "")?;
        let mut entries = BTreeMap::new();
        for row in rows {
            if number::<u64>(row, 0, "")? != version {
                return Err(corrupt("", "rows disagree on the head version"));
            }
            if let Some((path, entry)) = parse_entry(row)?
                && entries.insert(path.clone(), entry).is_some()
            {
                return Err(corrupt(&path, "a path is stored twice"));
            }
        }
        Ok(Manifest::from_entries(version, entries)?)
    }

    async fn commit_batch(&self, workspace: &str, batch: &Batch) -> Result<u64, Failure> {
        let change = batch.change();
        let token = nanocode_platform::uuid_v4().to_string();
        let base = change.base.to_string();
        let cleared = json(
            change
                .entries
                .keys()
                .map(|path| Value::String(path.clone()))
                .collect(),
        );
        let put = json(
            change
                .entries
                .iter()
                .filter_map(|(path, entry)| entry.as_ref().map(|entry| entry_row(path, entry)))
                .collect(),
        );
        let dropped = json(
            change
                .dropped
                .iter()
                .map(|digest| Value::String(digest.hex()))
                .collect(),
        );
        self.put_objects(batch).await?;
        let fresh = json(
            batch
                .blobs()
                .iter()
                .map(|blob| Value::String(blob.digest().hex()))
                .collect(),
        );
        let early = mutated(Mutation::SqlReleaseBeforeCommit) && !change.dropped.is_empty();
        let blobs = self.blob_rows(batch);
        let mut statements = Vec::with_capacity(7 + blobs.len());
        statements.push(self.claim(change.base, [workspace, &token, &base, &fresh]));
        if !change.entries.is_empty() {
            statements.push(SqlStatement {
                sql: self.pick(&CLEAR),
                params: vec![workspace, &token, &cleared],
            });
            statements.push(SqlStatement {
                sql: self.pick(&PUT),
                params: vec![workspace, &token, &put],
            });
        }
        self.push_blobs(&mut statements, [workspace, &token, &fresh], &blobs);
        if early {
            self.release_early(workspace, &token, &dropped).await?;
        }
        if !change.dropped.is_empty() && !early {
            statements.push(SqlStatement {
                sql: self.pick(&DROP_REFS),
                params: vec![workspace, &token, &dropped],
            });
        }
        statements.push(SqlStatement {
            sql: self.pick(&HEAD),
            params: vec![workspace],
        });
        let results = self.run_commit(&statements).await?;
        claimed_head(&results, &token, change.base, change.version)
    }

    fn blob_rows(&self, batch: &Batch) -> Vec<(String, String)> {
        let mut blobs: Vec<(String, String)> = batch
            .blobs()
            .iter()
            .map(|blob| {
                let data = if self.bucket.is_some() {
                    OBJECT.to_owned()
                } else {
                    STANDARD.encode(blob.bytes())
                };
                (blob.digest().hex(), data)
            })
            .collect();
        blobs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        blobs
    }

    fn claim<'a>(
        &self,
        version: u64,
        [workspace, token, base, fresh]: [&'a str; 4],
    ) -> SqlStatement<'a> {
        if version == 0 {
            SqlStatement {
                sql: self.pick(&CLAIM_FIRST),
                params: vec![workspace, token, fresh],
            }
        } else {
            SqlStatement {
                sql: self.pick(if mutated(Mutation::SqlSkipVersionCas) {
                    &CLAIM_ANY
                } else {
                    &CLAIM_NEXT
                }),
                params: vec![workspace, token, base, fresh],
            }
        }
    }

    fn push_blobs<'a>(
        &self,
        statements: &mut Vec<SqlStatement<'a>>,
        [workspace, token, fresh]: [&'a str; 3],
        blobs: &'a [(String, String)],
    ) {
        if !blobs.is_empty() {
            statements.push(SqlStatement {
                sql: self.pick(&REVIVE),
                params: vec![workspace, token, fresh],
            });
        }
        for (digest, data) in blobs {
            statements.push(SqlStatement {
                sql: self.pick(&ADD_BLOB),
                params: vec![workspace, token, digest, data],
            });
        }
        if !blobs.is_empty() {
            statements.push(SqlStatement {
                sql: self.pick(&ADD_REFS),
                params: vec![workspace, token, fresh],
            });
        }
    }

    async fn release_early(
        &self,
        workspace: &str,
        token: &str,
        dropped: &str,
    ) -> Result<(), Failure> {
        let release = [SqlStatement {
            sql: self.pick(&DROP_EARLY),
            params: vec![workspace, token, dropped],
        }];
        self.host
            .run(SqlAccess::Write, "workspace_release", &release)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn run_commit(
        &self,
        statements: &[SqlStatement<'_>],
    ) -> Result<Vec<Vec<SqlRow>>, Failure> {
        let results = if mutated(Mutation::SqlSplitTransaction) {
            let mut results = Vec::with_capacity(statements.len());
            for statement in statements {
                let one = std::slice::from_ref(statement);
                let answered = self
                    .host
                    .run(SqlAccess::Write, "workspace_commit", one)
                    .await
                    .map_err(backend)?;
                results.extend(answered);
            }
            results
        } else {
            self.host
                .transaction("workspace_commit", statements)
                .await
                .map_err(backend)?
        };
        if results.len() != statements.len() {
            return Err(Failure::Backend(format!(
                "the SQL host answered {} of {} statements",
                results.len(),
                statements.len()
            )));
        }
        Ok(results)
    }

    async fn read_blob(&self, digest: &Digest) -> Result<Blob, Failure> {
        let hex = digest.hex();
        let statements = [SqlStatement {
            sql: self.pick(&BLOB),
            params: vec![&hex],
        }];
        let results = self
            .host
            .run(SqlAccess::Read, "workspace_blob", &statements)
            .await
            .map_err(backend)?;
        let Some(data) = results
            .first()
            .and_then(|rows| rows.first())
            .and_then(|row| column(row, 0))
        else {
            return Err(Failure::Missing(*digest));
        };
        if data.starts_with(GONE) {
            return Err(Failure::Missing(*digest));
        }
        if data == OBJECT {
            let Some(bucket) = &self.bucket else {
                return Err(Failure::Backend(format!(
                    "blob {hex} lives in object storage but this store has none"
                )));
            };
            let bytes = bucket
                .objects
                .get(&object_key(&hex))
                .await?
                .ok_or(Failure::Missing(*digest))?;
            return Blob::verified(*digest, bytes);
        }
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| corrupt(&hex, "a stored blob is not base64"))?;
        Blob::verified(*digest, Bytes::from(bytes))
    }
}

impl Store for SqlStore {
    fn load<'a>(&'a self, workspace: &'a str) -> StoreFuture<'a, Result<Manifest, Failure>> {
        Box::pin(self.load_manifest(workspace))
    }

    fn commit<'a>(
        &'a self,
        workspace: &'a str,
        batch: &'a Batch,
    ) -> StoreFuture<'a, Result<u64, Failure>> {
        Box::pin(self.commit_batch(workspace, batch))
    }

    fn blob<'a>(&'a self, digest: &'a Digest) -> StoreFuture<'a, Result<Blob, Failure>> {
        Box::pin(self.read_blob(digest))
    }
}
