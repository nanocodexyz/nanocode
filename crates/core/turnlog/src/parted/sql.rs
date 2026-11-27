//! The partitioned log on SQL: Postgres (LIST partitions by part, one global position sequence,
//! lz4 bodies, NOTIFY on commit) and SQLite (one table, positions per partition), through any
//! nanocode_durability_core::Sql host. A fenced write's first statement is a compare-and-set on
//! its partition's head row at the writer's epoch, so the partition's one owner commits one
//! transaction at a time and positions follow commit order; claims, renewals and releases are
//! compare-and-sets on the lease row, judged on the database clock.

use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    num::NonZeroU32,
    sync::Arc,
};

use nanocode_durability_core::{
    Migrated, Migration, Settle, Sql, SqlAccess, SqlDialect, SqlRow, SqlStatement, StoreError,
    StoreFuture, migrate_format_then,
};
use nanocode_partitions::{Partition, Row};
use nanocode_sync::{Atom, Notify};

use super::{Append, Entry, PartStore, Rows, Verdict};
use crate::host::{
    CHECKPOINT_CREATE, Compaction, Extension, StoredCheckpoint, load_checkpoint, store_checkpoint,
};

/// The channel Postgres notifies on, with payload "scope:head" per committed scope.
pub const CHANNEL: &str = "nanocode_log";

const SCHEMA_PG: &[&str] = &[
    "CREATE SEQUENCE IF NOT EXISTS nanocode_log_pos",
    "CREATE TABLE IF NOT EXISTS nanocode_log_fence (part INTEGER PRIMARY KEY, epoch BIGINT NOT NULL DEFAULT 0, holder TEXT, expires_ms BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS nanocode_log_cursor (part INTEGER NOT NULL, reader TEXT NOT NULL, pos BIGINT NOT NULL, PRIMARY KEY (part, reader))",
    "CREATE TABLE IF NOT EXISTS nanocode_plog (part INTEGER NOT NULL, scope TEXT NOT NULL, seq BIGINT NOT NULL, pos BIGINT NOT NULL, snap BOOLEAN NOT NULL, body TEXT COMPRESSION lz4 NOT NULL, PRIMARY KEY (part, scope, seq)) PARTITION BY LIST (part)",
    "CREATE INDEX IF NOT EXISTS nanocode_plog_pos ON nanocode_plog (part, pos)",
    "DO $data$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'nanocode_plog'::regclass AND attname = 'data' AND NOT attisdropped) THEN ALTER TABLE nanocode_plog ADD COLUMN data BYTEA COMPRESSION lz4; END IF; END $data$",
    CHECKPOINT_CREATE[1],
    "CREATE OR REPLACE FUNCTION nanocode_plog_parts(n integer) RETURNS integer LANGUAGE plpgsql AS $parts$ BEGIN FOR i IN 0..n - 1 LOOP EXECUTE format('CREATE TABLE IF NOT EXISTS nanocode_plog_p%s PARTITION OF nanocode_plog FOR VALUES IN (%s)', i, i); INSERT INTO nanocode_log_fence (part) VALUES (i) ON CONFLICT DO NOTHING; END LOOP; RETURN n; END $parts$",
];
const SCHEMA_SQLITE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS nanocode_log_fence (part INTEGER PRIMARY KEY, epoch INTEGER NOT NULL DEFAULT 0, holder TEXT, expires_ms INTEGER NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS nanocode_log_cursor (part INTEGER NOT NULL, reader TEXT NOT NULL, pos INTEGER NOT NULL, PRIMARY KEY (part, reader)) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS nanocode_plog (part INTEGER NOT NULL, scope TEXT NOT NULL, seq INTEGER NOT NULL, pos INTEGER NOT NULL, snap INTEGER NOT NULL, body TEXT NOT NULL, PRIMARY KEY (part, scope, seq)) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS nanocode_plog_pos ON nanocode_plog (part, pos)",
    CHECKPOINT_CREATE[0],
];
const SCHEMA_PG_2: &[&str] = &[
    "ALTER TABLE nanocode_log_fence ADD COLUMN IF NOT EXISTS cursor BIGINT NOT NULL DEFAULT 0",
    "CREATE TABLE IF NOT EXISTS nanocode_log_head (part INTEGER PRIMARY KEY, epoch BIGINT NOT NULL DEFAULT 0, version BIGINT NOT NULL DEFAULT 0)",
    "INSERT INTO nanocode_log_head (part, epoch) SELECT part, CASE WHEN holder IS NULL THEN 0 ELSE epoch END FROM nanocode_log_fence ON CONFLICT (part) DO NOTHING",
    "CREATE OR REPLACE FUNCTION nanocode_plog_parts(n integer) RETURNS integer LANGUAGE plpgsql AS $parts$ BEGIN FOR i IN 0..n - 1 LOOP EXECUTE format('CREATE TABLE IF NOT EXISTS nanocode_plog_p%s PARTITION OF nanocode_plog FOR VALUES IN (%s)', i, i); INSERT INTO nanocode_log_fence (part) VALUES (i) ON CONFLICT DO NOTHING; INSERT INTO nanocode_log_head (part) VALUES (i) ON CONFLICT DO NOTHING; END LOOP; RETURN n; END $parts$",
];
const SCHEMA_SQLITE_2: &[&str] = &[
    "ALTER TABLE nanocode_log_fence ADD COLUMN cursor INTEGER NOT NULL DEFAULT 0",
    "CREATE TABLE IF NOT EXISTS nanocode_log_head (part INTEGER PRIMARY KEY, epoch INTEGER NOT NULL DEFAULT 0, version INTEGER NOT NULL DEFAULT 0)",
    "INSERT OR IGNORE INTO nanocode_log_head (part, epoch) SELECT part, CASE WHEN holder IS NULL THEN 0 ELSE epoch END FROM nanocode_log_fence",
];
const MIGRATIONS: [Migration; 2] = [
    Migration {
        statements: [SCHEMA_SQLITE, SCHEMA_PG],
        adopt: None,
    },
    Migration {
        statements: [SCHEMA_SQLITE_2, SCHEMA_PG_2],
        adopt: None,
    },
];
const PARTS_SQLITE: [&str; 2] = [
    "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < CAST(?1 AS INTEGER)) INSERT OR IGNORE INTO nanocode_log_fence (part) SELECT i FROM n",
    "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < CAST(?1 AS INTEGER)) INSERT OR IGNORE INTO nanocode_log_head (part) SELECT i FROM n",
];
const PARTS_PG: &str = "SELECT nanocode_plog_parts(CAST($1::text AS INTEGER))";
const PREPARED: Dialects = [
    "SELECT CAST(count(*) AS TEXT) FROM nanocode_log_head WHERE part < CAST(?1 AS INTEGER)",
    "SELECT CAST(count(*) AS TEXT) FROM nanocode_log_head WHERE part < CAST($1::text AS INTEGER)",
];

type Dialects = [&'static str; 2];

/// A fenced write's first statement: the head compare-and-set at the writer's epoch. Zero rows
/// is Lost. The row is the partition's own, touched by its owner's commits and by a claim or
/// release, never by a renewal.
const HEAD: Dialects = [
    "UPDATE nanocode_log_head SET version = version + 1 WHERE part = CAST(?1 AS INTEGER) AND epoch = CAST(?2 AS INTEGER) AND epoch > 0 RETURNING CAST(version AS TEXT)",
    "UPDATE nanocode_log_head SET version = version + 1 WHERE part = CAST($1::text AS INTEGER) AND epoch = CAST($2::text AS BIGINT) AND epoch > 0 RETURNING CAST(version AS TEXT)",
];
/// An unfenced write's first statement: a plain read that the partition exists. It locks nothing.
const PRESENT: Dialects = [
    "SELECT CAST(epoch AS TEXT) FROM nanocode_log_fence WHERE part = CAST(?1 AS INTEGER)",
    "SELECT CAST(epoch AS TEXT) FROM nanocode_log_fence WHERE part = CAST($1::text AS INTEGER)",
];
/// Claim on the database clock: only a released row, the claimant's own, or one past
/// expires_ms + grace. The head then moves to the new epoch in the same transaction.
const CLAIM: Dialects = [
    "UPDATE nanocode_log_fence SET epoch = epoch + 1, holder = ?2, expires_ms = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) + CAST(?3 AS INTEGER) WHERE part = CAST(?1 AS INTEGER) AND (holder IS NULL OR holder = ?2 OR expires_ms + CAST(?4 AS INTEGER) < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER)) RETURNING CAST(epoch AS TEXT), CAST(cursor AS TEXT)",
    "UPDATE nanocode_log_fence SET epoch = epoch + 1, holder = $2::text, expires_ms = CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) + CAST($3::text AS BIGINT) WHERE part = CAST($1::text AS INTEGER) AND (holder IS NULL OR holder = $2::text OR expires_ms + CAST($4::text AS BIGINT) < CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)) RETURNING CAST(epoch AS TEXT), CAST(cursor AS TEXT)",
];
const CLAIM_HEAD: Dialects = [
    "UPDATE nanocode_log_head SET epoch = (SELECT f.epoch FROM nanocode_log_fence f WHERE f.part = CAST(?1 AS INTEGER)) WHERE part = CAST(?1 AS INTEGER) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST(?1 AS INTEGER) AND f.holder = ?2)",
    "UPDATE nanocode_log_head SET epoch = (SELECT f.epoch FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER)) WHERE part = CAST($1::text AS INTEGER) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND f.holder = $2::text)",
];
/// Renewal touches only the lease row, so it never waits behind a commit.
const RENEW: Dialects = [
    "UPDATE nanocode_log_fence SET expires_ms = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) + CAST(?4 AS INTEGER) WHERE part = CAST(?1 AS INTEGER) AND epoch = CAST(?2 AS INTEGER) AND holder = ?3 RETURNING 1",
    "UPDATE nanocode_log_fence SET expires_ms = CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) + CAST($4::text AS BIGINT) WHERE part = CAST($1::text AS INTEGER) AND epoch = CAST($2::text AS BIGINT) AND holder = $3::text RETURNING 1",
];
/// Every renewal of one node in one statement: [[part, epoch], ...] in ?2/$2. It touches only
/// the lease rows, so it never waits behind a commit.
const RENEW_ALL: Dialects = [
    "UPDATE nanocode_log_fence SET expires_ms = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) + CAST(?3 AS INTEGER) WHERE holder = ?1 AND EXISTS (SELECT 1 FROM json_each(?2) AS h WHERE CAST(json_extract(h.value, '$[0]') AS INTEGER) = nanocode_log_fence.part AND CAST(json_extract(h.value, '$[1]') AS INTEGER) = nanocode_log_fence.epoch) RETURNING CAST(part AS TEXT)",
    "UPDATE nanocode_log_fence f SET expires_ms = CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT) + CAST($3::text AS BIGINT) FROM (SELECT CAST(a.e ->> 0 AS INTEGER) AS part, CAST(a.e ->> 1 AS BIGINT) AS epoch FROM jsonb_array_elements(CAST($2::text AS jsonb)) AS a(e)) h WHERE f.part = h.part AND f.epoch = h.epoch AND f.holder = $1::text RETURNING CAST(f.part AS TEXT)",
];
/// Release at the committed position: the gainer claims at once and resumes from the cursor.
const RELEASE: Dialects = [
    "UPDATE nanocode_log_fence SET holder = NULL, cursor = MAX(cursor, COALESCE((SELECT MAX(pos) FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER)), 0)) WHERE part = CAST(?1 AS INTEGER) AND epoch = CAST(?2 AS INTEGER) AND holder = ?3 RETURNING CAST(cursor AS TEXT)",
    "UPDATE nanocode_log_fence SET holder = NULL, cursor = GREATEST(cursor, COALESCE((SELECT max(pos) FROM nanocode_plog WHERE part = CAST($1::text AS INTEGER)), 0)) WHERE part = CAST($1::text AS INTEGER) AND epoch = CAST($2::text AS BIGINT) AND holder = $3::text RETURNING CAST(cursor AS TEXT)",
];
const RELEASE_HEAD: Dialects = [
    "UPDATE nanocode_log_head SET epoch = 0 WHERE part = CAST(?1 AS INTEGER) AND epoch = CAST(?2 AS INTEGER) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST(?1 AS INTEGER) AND f.epoch = CAST(?2 AS INTEGER) AND f.holder IS NULL)",
    "UPDATE nanocode_log_head SET epoch = 0 WHERE part = CAST($1::text AS INTEGER) AND epoch = CAST($2::text AS BIGINT) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND f.epoch = CAST($2::text AS BIGINT) AND f.holder IS NULL)",
];
const INSERT: Dialects = [
    "WITH input AS (SELECT CAST(a.key AS INTEGER) AS app, json_extract(a.value, '$[0]') AS scope, CAST(json_extract(r.value, '$[0]') AS INTEGER) AS seq, json_extract(r.value, '$[1]') AS body, CAST(r.key AS INTEGER) AS rord FROM json_each(?3) AS a, json_each(a.value, '$[1]') AS r), keep AS (SELECT i.* FROM input i WHERE EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST(?1 AS INTEGER) AND (CAST(?2 AS INTEGER) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST(?2 AS INTEGER)))) AND i.app NOT IN (SELECT j.app FROM input j JOIN nanocode_plog p ON p.part = CAST(?1 AS INTEGER) AND p.scope = j.scope AND p.seq = j.seq)) INSERT INTO nanocode_plog (part, scope, seq, pos, snap, body) SELECT CAST(?1 AS INTEGER), scope, seq, (SELECT COALESCE(MAX(pos), 0) FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER)) + row_number() OVER (ORDER BY app, rord), substr(body, 1, 1) = 's', body FROM keep WHERE true RETURNING scope, CAST(seq AS TEXT)",
    "WITH input AS (SELECT a.ord AS app, a.e ->> 0 AS scope, CAST(r.e ->> 0 AS BIGINT) AS seq, r.e ->> 1 AS body, r.e ->> 2 AS data, r.ord AS rord FROM jsonb_array_elements(CAST($3::text AS jsonb)) WITH ORDINALITY AS a(e, ord) CROSS JOIN LATERAL jsonb_array_elements(a.e -> 1) WITH ORDINALITY AS r(e, ord)), clash AS (SELECT DISTINCT i.app FROM input i WHERE (SELECT true FROM nanocode_plog p WHERE p.part = CAST($1::text AS INTEGER) AND p.scope = i.scope AND p.seq = i.seq LIMIT 1)), ins AS (INSERT INTO nanocode_plog (part, scope, seq, pos, snap, body, data) SELECT CAST($1::text AS INTEGER), s.scope, s.seq, nextval('nanocode_log_pos'), substr(s.body, 1, 1) = 's', CASE WHEN s.data IS NULL THEN s.body ELSE substr(s.body, 1, 1) END, decode(s.data, 'base64') FROM (SELECT i.scope, i.seq, i.body, i.data FROM input i WHERE EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND (CAST($2::text AS BIGINT) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST($2::text AS BIGINT)))) AND NOT EXISTS (SELECT 1 FROM clash c WHERE c.app = i.app) ORDER BY i.app, i.rord) s RETURNING scope, seq), note AS (SELECT count(*) AS n FROM (SELECT pg_notify('nanocode_log', h.scope || ':' || h.head) FROM (SELECT scope, max(seq) AS head FROM ins GROUP BY scope) h WHERE CAST($4::text AS INTEGER) = 1) notified) SELECT ins.scope, CAST(ins.seq AS TEXT) FROM ins, note",
];
/// INSERT on Postgres, then in the same statement each landed append's compaction: its archived
/// rows rewritten and its dropped and superseded ranges deleted, never a row some cursor has not
/// consumed. An append that conflicted lands nothing and compacts nothing.
const INSERT_COMPACTING: &str = "WITH input AS (SELECT a.ord AS app, a.e ->> 0 AS scope, CAST(r.e ->> 0 AS BIGINT) AS seq, r.e ->> 1 AS body, r.e ->> 2 AS data, r.ord AS rord FROM jsonb_array_elements(CAST($3::text AS jsonb)) WITH ORDINALITY AS a(e, ord) CROSS JOIN LATERAL jsonb_array_elements(a.e -> 1) WITH ORDINALITY AS r(e, ord)), clash AS (SELECT DISTINCT i.app FROM input i WHERE (SELECT true FROM nanocode_plog p WHERE p.part = CAST($1::text AS INTEGER) AND p.scope = i.scope AND p.seq = i.seq LIMIT 1)), ins AS (INSERT INTO nanocode_plog (part, scope, seq, pos, snap, body, data) SELECT CAST($1::text AS INTEGER), s.scope, s.seq, nextval('nanocode_log_pos'), substr(s.body, 1, 1) = 's', CASE WHEN s.data IS NULL THEN s.body ELSE substr(s.body, 1, 1) END, decode(s.data, 'base64') FROM (SELECT i.scope, i.seq, i.body, i.data FROM input i WHERE EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND (CAST($2::text AS BIGINT) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST($2::text AS BIGINT)))) AND NOT EXISTS (SELECT 1 FROM clash c WHERE c.app = i.app) ORDER BY i.app, i.rord) s RETURNING scope, seq), note AS (SELECT count(*) AS n FROM (SELECT pg_notify('nanocode_log', h.scope || ':' || h.head) FROM (SELECT scope, max(seq) AS head FROM ins GROUP BY scope) h WHERE CAST($4::text AS INTEGER) = 1) notified), compaction AS (SELECT c.e ->> 0 AS scope, CAST(c.e ->> 1 AS BIGINT) AS first, c.e -> 2 AS archived, c.e -> 3 AS ranges FROM jsonb_array_elements(CAST($5::text AS jsonb)) AS c(e)), landed AS (SELECT c.* FROM compaction c WHERE EXISTS (SELECT 1 FROM ins WHERE ins.scope = c.scope AND ins.seq = c.first)), archived AS (UPDATE nanocode_plog p SET body = CASE WHEN x.data IS NULL THEN x.body ELSE substr(x.body, 1, 1) END, data = decode(x.data, 'base64'), snap = substr(x.body, 1, 1) = 's' FROM (SELECT l.scope, CAST(a.e ->> 0 AS BIGINT) AS seq, a.e ->> 1 AS body, a.e ->> 2 AS data FROM landed l CROSS JOIN LATERAL jsonb_array_elements(l.archived) AS a(e)) x WHERE p.part = CAST($1::text AS INTEGER) AND p.scope = x.scope AND p.seq = x.seq RETURNING 1), dropped AS (DELETE FROM nanocode_plog p USING (SELECT l.scope, CAST(d.e ->> 0 AS BIGINT) AS low, CAST(d.e ->> 1 AS BIGINT) AS high FROM landed l CROSS JOIN LATERAL jsonb_array_elements(l.ranges) AS d(e)) y WHERE p.part = CAST($1::text AS INTEGER) AND p.scope = y.scope AND p.seq >= y.low AND p.seq < y.high AND p.pos <= COALESCE((SELECT min(pos) FROM nanocode_log_cursor WHERE part = CAST($1::text AS INTEGER)), 9223372036854775807) RETURNING 1) SELECT ins.scope, CAST(ins.seq AS TEXT) FROM ins, note";
/// Wakes listeners after a commit landed, in its own unsynced transaction: NOTIFY in the commit
/// itself holds Postgres's global notify lock across the WAL flush and serialises every
/// partition's commits (P=16, 64 claimers: 2.5k vs 13.7k claims/s with it off).
const NOTIFY_PG: &str = "SELECT count(pg_notify('nanocode_log', p)) FROM json_array_elements_text(CAST($1::text AS json)) AS p";

const SCAN: Dialects = [
    "SELECT CAST(seq AS TEXT), body FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER) AND scope = ?2 AND seq >= CAST(?3 AS INTEGER) AND seq < CAST(?4 AS INTEGER) ORDER BY seq",
    "SELECT CAST(seq AS TEXT), CASE WHEN data IS NULL THEN body ELSE body || rtrim(translate(encode(data, 'base64'), chr(10), ''), '=') END FROM nanocode_plog WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND seq >= CAST($3::text AS BIGINT) AND seq < CAST($4::text AS BIGINT) ORDER BY nanocode_plog.seq",
];
const LOAD: Dialects = [
    "SELECT CAST(seq AS TEXT), body FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER) AND scope = ?2 AND seq >= COALESCE((SELECT MAX(seq) FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER) AND scope = ?2 AND snap), 0) ORDER BY seq",
    "SELECT CAST(seq AS TEXT), CASE WHEN data IS NULL THEN body ELSE body || rtrim(translate(encode(data, 'base64'), chr(10), ''), '=') END FROM nanocode_plog WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND seq >= COALESCE((SELECT max(seq) FROM nanocode_plog WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND snap), 0) ORDER BY nanocode_plog.seq",
];
const ARCHIVE: Dialects = [
    "UPDATE nanocode_plog SET body = ?4, snap = substr(?4, 1, 1) = 's' WHERE part = CAST(?1 AS INTEGER) AND scope = ?2 AND seq = CAST(?3 AS INTEGER) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST(?1 AS INTEGER) AND (CAST(?5 AS INTEGER) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST(?5 AS INTEGER))))",
    "UPDATE nanocode_plog SET body = $4::text, data = NULL, snap = substr($4::text, 1, 1) = 's' WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND seq = CAST($3::text AS BIGINT) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND (CAST($5::text AS BIGINT) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST($5::text AS BIGINT))))",
];
const ARCHIVE_DATA: &str = "UPDATE nanocode_plog SET body = substr($4::text, 1, 1), data = decode($6::text, 'base64'), snap = substr($4::text, 1, 1) = 's' WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND seq = CAST($3::text AS BIGINT) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND (CAST($5::text AS BIGINT) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST($5::text AS BIGINT))))";
const DROP: Dialects = [
    "DELETE FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER) AND scope = ?2 AND seq >= CAST(?3 AS INTEGER) AND seq < CAST(?4 AS INTEGER) AND pos <= COALESCE((SELECT MIN(pos) FROM nanocode_log_cursor WHERE part = CAST(?1 AS INTEGER)), 9223372036854775807) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST(?1 AS INTEGER) AND (CAST(?5 AS INTEGER) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST(?5 AS INTEGER))))",
    "DELETE FROM nanocode_plog WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND seq >= CAST($3::text AS BIGINT) AND seq < CAST($4::text AS BIGINT) AND pos <= COALESCE((SELECT min(pos) FROM nanocode_log_cursor WHERE part = CAST($1::text AS INTEGER)), 9223372036854775807) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND (CAST($5::text AS BIGINT) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST($5::text AS BIGINT))))",
];
const PRUNE: Dialects = [
    "DELETE FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER) AND scope = ?2 AND seq < MIN(CAST(?3 AS INTEGER), COALESCE((SELECT MAX(seq) FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER) AND scope = ?2 AND snap), 0)) AND pos <= COALESCE((SELECT MIN(pos) FROM nanocode_log_cursor WHERE part = CAST(?1 AS INTEGER)), 9223372036854775807) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST(?1 AS INTEGER) AND (CAST(?4 AS INTEGER) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST(?4 AS INTEGER)))) RETURNING 1",
    "DELETE FROM nanocode_plog WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND seq < LEAST(CAST($3::text AS BIGINT), COALESCE((SELECT max(seq) FROM nanocode_plog WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND snap), 0)) AND pos <= COALESCE((SELECT min(pos) FROM nanocode_log_cursor WHERE part = CAST($1::text AS INTEGER)), 9223372036854775807) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND (CAST($4::text AS BIGINT) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST($4::text AS BIGINT)))) RETURNING 1",
];
const EXTEND: Dialects = [
    "UPDATE nanocode_plog SET seq = CAST(?4 AS INTEGER), pos = (SELECT COALESCE(MAX(pos), 0) + 1 FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER)), body = substr(CAST(body AS BLOB), 1, length(CAST(body AS BLOB)) - CAST(?5 AS INTEGER)) || ?6 WHERE part = CAST(?1 AS INTEGER) AND scope = ?2 AND seq = CAST(?3 AS INTEGER) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST(?1 AS INTEGER) AND (CAST(?7 AS INTEGER) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST(?7 AS INTEGER)))) RETURNING CAST(seq AS TEXT)",
    "UPDATE nanocode_plog SET seq = CAST($4::text AS BIGINT), pos = nextval('nanocode_log_pos'), body = CASE WHEN data IS NULL THEN substr(body, 1, length(body) - CAST($5::text AS INTEGER)) || $6::text ELSE body END, data = CASE WHEN data IS NULL THEN NULL ELSE substr(data, 1, length(data) - CAST($8::text AS INTEGER)) || decode($9::text, 'base64') END WHERE part = CAST($1::text AS INTEGER) AND scope = $2::text AND seq = CAST($3::text AS BIGINT) AND EXISTS (SELECT 1 FROM nanocode_log_fence f WHERE f.part = CAST($1::text AS INTEGER) AND (CAST($7::text AS BIGINT) < 0 OR (f.holder IS NOT NULL AND f.epoch = CAST($7::text AS BIGINT)))) RETURNING CAST(seq AS TEXT)",
];
const FOLLOW: Dialects = [
    "SELECT CAST(pos AS TEXT), scope, CAST(seq AS TEXT), body FROM nanocode_plog WHERE part = CAST(?1 AS INTEGER) AND pos > CAST(?2 AS INTEGER) ORDER BY pos LIMIT CAST(?3 AS INTEGER)",
    "SELECT CAST(pos AS TEXT), scope, CAST(seq AS TEXT), CASE WHEN data IS NULL THEN body ELSE body || rtrim(translate(encode(data, 'base64'), chr(10), ''), '=') END FROM nanocode_plog WHERE part = CAST($1::text AS INTEGER) AND pos > CAST($2::text AS BIGINT) ORDER BY nanocode_plog.pos LIMIT CAST($3::text AS BIGINT)",
];
const FENCE: Dialects = [
    "SELECT CAST(epoch AS TEXT), COALESCE(holder, ''), CAST(expires_ms AS TEXT), CAST(cursor AS TEXT) FROM nanocode_log_fence WHERE part = CAST(?1 AS INTEGER)",
    "SELECT CAST(epoch AS TEXT), COALESCE(holder, ''), CAST(expires_ms AS TEXT), CAST(cursor AS TEXT) FROM nanocode_log_fence WHERE part = CAST($1::text AS INTEGER)",
];
const CURSOR: Dialects = [
    "INSERT INTO nanocode_log_cursor (part, reader, pos) VALUES (CAST(?1 AS INTEGER), ?2, CAST(?3 AS INTEGER)) ON CONFLICT (part, reader) DO UPDATE SET pos = MAX(pos, excluded.pos)",
    "INSERT INTO nanocode_log_cursor (part, reader, pos) VALUES (CAST($1::text AS INTEGER), $2::text, CAST($3::text AS BIGINT)) ON CONFLICT (part, reader) DO UPDATE SET pos = GREATEST(nanocode_log_cursor.pos, EXCLUDED.pos)",
];

pub struct SqlParts<H: ?Sized> {
    host: Arc<H>,
    notify: bool,
    outbox: Arc<Outbox>,
}

/// Heads committed but not yet notified, for the notifier task to send in one unsynced
/// transaction, so a commit's answer never waits on its NOTIFY round trip.
#[derive(Default)]
struct Outbox {
    pending: Atom<Pending>,
    ready: Notify,
}

#[derive(Clone, Default)]
struct Pending {
    heads: BTreeMap<String, u64>,
    driven: bool,
    closed: bool,
}

/// Marks the outbox undriven when the notifier future ends or is dropped, so commits fall back
/// to notifying inline instead of queueing heads nobody sends.
struct Driving(Arc<Outbox>);

impl Drop for Driving {
    fn drop(&mut self) {
        self.0.pending.update(|pending| pending.driven = false);
    }
}

impl<H: ?Sized> Drop for SqlParts<H> {
    fn drop(&mut self) {
        self.outbox.pending.update(|pending| pending.closed = true);
        self.outbox.ready.notify_one();
    }
}

fn notify_payloads<'a>(heads: impl Iterator<Item = (&'a str, u64)>) -> Result<String, StoreError> {
    let payloads: Vec<String> = heads
        .map(|(scope, head)| format!("{scope}:{head}"))
        .collect();
    serde_json::to_string(&payloads)
        .map_err(|error| StoreError::Backend(format!("encode notify: {error}")))
}

async fn send_notify<H: Sql + ?Sized>(host: &H, payloads: &str) {
    // The appends are committed; a lost wake only delays followers, so its error is dropped.
    let _ = host
        .write(
            Settle::Ordered,
            "plog_notify",
            &[SqlStatement {
                sql: NOTIFY_PG,
                params: vec![payloads],
            }],
        )
        .await;
}

impl<H: Sql + ?Sized> SqlParts<H> {
    /// Postgres notifies CHANNEL after every commit (see notifier); with_notify(false) turns it off.
    pub fn new(host: Arc<H>) -> Self {
        Self {
            host,
            notify: true,
            outbox: Arc::new(Outbox::default()),
        }
    }

    #[must_use]
    pub const fn with_notify(mut self, notify: bool) -> Self {
        self.notify = notify;
        self
    }

    /// Sends committed heads' NOTIFY off the commit path: run it on an executor for as long as
    /// the store lives. Each pass sends every head queued meanwhile in one unsynced transaction,
    /// so commits on every partition share notify transactions and never wait on them. Without
    /// it running, commit notifies inline after the append lands.
    pub fn notifier(&self) -> impl Future<Output = ()> + use<H>
    where
        H: Send + Sync + 'static,
    {
        let host = Arc::clone(&self.host);
        let outbox = Arc::clone(&self.outbox);
        outbox.pending.update(|pending| pending.driven = true);
        async move {
            let _driving = Driving(Arc::clone(&outbox));
            loop {
                outbox.ready.notified().await;
                let (heads, closed) = outbox
                    .pending
                    .update(|pending| (std::mem::take(&mut pending.heads), pending.closed));
                if !heads.is_empty()
                    && let Ok(payloads) =
                        notify_payloads(heads.iter().map(|(scope, head)| (scope.as_str(), *head)))
                {
                    send_notify(&*host, &payloads).await;
                }
                if closed {
                    return;
                }
            }
        }
    }

    pub const fn host(&self) -> &Arc<H> {
        &self.host
    }

    fn pick(&self, statements: &Dialects) -> &'static str {
        self.host.dialect().pick(statements)
    }

    fn postgres(&self) -> bool {
        self.host.dialect() == SqlDialect::Postgres
    }
}

fn behind(detail: &str) -> StoreError {
    StoreError::Backend(format!(
        "the partitioned log is not migrated ({detail}); run its migrate step before serving"
    ))
}

fn invalid(detail: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(format!("SQL host answered an invalid log row: {detail}"))
}

fn cell(cells: &[Option<String>], index: usize) -> Result<&str, StoreError> {
    cells
        .get(index)
        .and_then(Option::as_deref)
        .ok_or_else(|| invalid(format!("missing cell {index}")))
}

fn number(cells: &[Option<String>], index: usize) -> Result<u64, StoreError> {
    let text = cell(cells, index)?;
    text.parse()
        .map_err(|error| invalid(format!("{text}: {error}")))
}

fn rows(answer: Vec<SqlRow>) -> Result<Rows, StoreError> {
    answer
        .into_iter()
        .map(|cells| Ok((number(&cells, 0)?, cell(&cells, 1)?.to_owned())))
        .collect()
}

fn token(epoch: Option<u64>) -> String {
    epoch.map_or_else(|| "-1".to_owned(), |epoch| epoch.to_string())
}

fn result(answer: &[Vec<SqlRow>], index: usize) -> Result<&Vec<SqlRow>, StoreError> {
    answer
        .get(index)
        .ok_or_else(|| invalid(format!("no result {index}")))
}

fn data(postgres: bool, rows: &[(u64, String)]) -> Vec<(u64, &str, Option<String>)> {
    let payloads = if postgres {
        crate::codec::binaries(rows)
    } else {
        vec![None; rows.len()]
    };
    rows.iter()
        .zip(payloads)
        .map(|((seq, body), payload)| (*seq, body.as_str(), payload.map(|(_, padded)| padded)))
        .collect()
}

type Compacted<'a> = (
    &'a str,
    u64,
    Vec<(u64, &'a str, Option<String>)>,
    Vec<(u64, u64)>,
);

fn compacting(
    batch: &[Append],
    compactions: &[Option<Compaction<'_>>],
) -> Result<String, StoreError> {
    let entries: Vec<Compacted<'_>> = batch
        .iter()
        .zip(compactions)
        .filter_map(|(append, compaction)| {
            compaction.as_ref().map(|compaction| (append, compaction))
        })
        .map(|(append, compaction)| {
            let (first, _) = append
                .rows
                .first()
                .expect("a compacting append carries rows");
            let archived = data(true, compaction.archived);
            let ranges = std::iter::once(&compaction.dropped)
                .chain(compaction.superseded)
                .filter(|range| !range.is_empty())
                .map(|range| (range.start, range.end.min(i64::MAX as u64)))
                .collect();
            (append.scope.as_ref(), *first, archived, ranges)
        })
        .collect();
    serde_json::to_string(&entries)
        .map_err(|error| StoreError::Backend(format!("encode compactions: {error}")))
}

impl<H: Sql + Send + Sync + ?Sized> SqlParts<H> {
    /// One transaction: the fence, every append, and with compactions each landed append's
    /// compaction (INSERT_COMPACTING).
    async fn committing(
        &self,
        part: Partition,
        epoch: Option<u64>,
        batch: &[Append],
        compactions: Option<&str>,
    ) -> Result<Vec<Verdict>, StoreError> {
        let postgres = self.postgres();
        let encoded: Vec<(&str, Vec<(u64, &str, Option<String>)>)> = batch
            .iter()
            .map(|append| (append.scope.as_ref(), data(postgres, &append.rows)))
            .collect();
        let json = serde_json::to_string(&encoded)
            .map_err(|error| StoreError::Backend(format!("encode batch: {error}")))?;
        let (part, epoch_text) = (part.0.to_string(), token(epoch));
        let mut params = vec![part.as_str(), epoch_text.as_str(), json.as_str()];
        if self.postgres() {
            params.push("0");
        }
        let sql = match compactions {
            Some(compactions) => {
                params.push(compactions);
                INSERT_COMPACTING
            }
            None => self.pick(&INSERT),
        };
        let insert = SqlStatement { sql, params };
        let answer = match self.fenced("plog_commit", &part, epoch, vec![insert]).await {
            Err(StoreError::Fenced) if epoch.is_none() => {
                return Err(StoreError::Backend(format!(
                    "log partition {part} is not prepared"
                )));
            }
            answer => answer?,
        };
        let mut landed = HashSet::new();
        let mut heads = std::collections::BTreeMap::new();
        for cells in result(&answer, 1)? {
            let (scope, seq) = (cell(cells, 0)?, number(cells, 1)?);
            let head = heads.entry(scope).or_insert(seq);
            *head = (*head).max(seq);
            landed.insert((scope.to_owned(), seq));
        }
        if self.notify && self.postgres() && !heads.is_empty() {
            let queued = self.outbox.pending.update(|pending| {
                if pending.driven {
                    for (scope, head) in &heads {
                        let queued = pending.heads.entry((*scope).to_owned()).or_insert(*head);
                        *queued = (*queued).max(*head);
                    }
                }
                pending.driven
            });
            if queued {
                self.outbox.ready.notify_one();
            } else {
                send_notify(&*self.host, &notify_payloads(heads.into_iter())?).await;
            }
        }
        Ok(batch
            .iter()
            .map(|append| {
                let first = append.rows.first().map_or(0, |(seq, _)| *seq);
                if landed.contains(&(append.scope.to_string(), first)) {
                    Verdict::Committed
                } else {
                    Verdict::Conflict
                }
            })
            .collect())
    }

    /// Runs the head compare-and-set (fenced) or the partition read (unfenced), then the
    /// statements, in one transaction; a stale token is Fenced.
    async fn fenced(
        &self,
        operation: &'static str,
        part: &str,
        epoch: Option<u64>,
        statements: Vec<SqlStatement<'_>>,
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        let (first, params) = match epoch {
            Some(epoch) => (self.pick(&HEAD), vec![part.to_owned(), epoch.to_string()]),
            None => (self.pick(&PRESENT), vec![part.to_owned()]),
        };
        let mut all = vec![SqlStatement {
            sql: first,
            params: params.iter().map(String::as_str).collect(),
        }];
        all.extend(statements);
        let answer = self.host.transaction(operation, &all).await?;
        if result(&answer, 0)?.is_empty() {
            return Err(StoreError::Fenced);
        }
        Ok(answer)
    }
}

impl<H: Sql + Send + Sync + ?Sized> PartStore for SqlParts<H> {
    fn read_capacity(&self) -> u32 {
        self.host.read_capacity()
    }

    fn row_bytes_max(&self) -> Option<u64> {
        self.host.row_bytes_max()
    }

    fn migrate(&self, count: NonZeroU32) -> StoreFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            let count = count.get().to_string();
            let parts: Vec<SqlStatement<'_>> = if self.postgres() {
                vec![SqlStatement {
                    sql: PARTS_PG,
                    params: vec![count.as_str()],
                }]
            } else {
                PARTS_SQLITE
                    .iter()
                    .map(|sql| SqlStatement {
                        sql,
                        params: vec![count.as_str()],
                    })
                    .collect()
            };
            migrate_format_then(&*self.host, "plog", &MIGRATIONS, &parts)
                .await
                .map(drop)
        })
    }

    fn prepare(&self, count: NonZeroU32) -> StoreFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            if crate::sim::mutated(crate::sim::Mutation::PrepareMigrates) {
                self.migrate(count).await?;
            }
            Migrated::check(&*self.host, "plog", &MIGRATIONS).await?;
            let wanted = count.get().to_string();
            let statements = [SqlStatement {
                sql: self.pick(&PREPARED),
                params: vec![wanted.as_str()],
            }];
            let answer = self
                .host
                .run(SqlAccess::Read, "plog_prepared", &statements)
                .await?;
            let cells = answer
                .into_iter()
                .next()
                .and_then(|rows| rows.into_iter().next())
                .ok_or_else(|| invalid("no partition count"))?;
            let parts = number(&cells, 0)?;
            if parts < u64::from(count.get()) {
                return Err(behind(&format!("{parts} of {count} partitions")));
            }
            Ok(())
        })
    }

    fn commit<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        batch: &'a [Append],
    ) -> StoreFuture<'a, Result<Vec<Verdict>, StoreError>> {
        Box::pin(self.committing(part, epoch, batch, None))
    }

    fn commit_compacting<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        batch: &'a [Append],
        compactions: &'a [Option<Compaction<'a>>],
    ) -> Option<StoreFuture<'a, Result<Vec<Verdict>, StoreError>>> {
        if !self.postgres() {
            return None;
        }
        Some(Box::pin(async move {
            let compactions = compacting(batch, compactions)?;
            self.committing(part, epoch, batch, Some(&compactions))
                .await
        }))
    }

    fn scan<'a>(
        &'a self,
        part: Partition,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Rows, StoreError>> {
        Box::pin(async move {
            let (part, from, to) = (
                part.0.to_string(),
                from.to_string(),
                to.min(i64::MAX as u64).to_string(),
            );
            let statements = [SqlStatement {
                sql: self.pick(&SCAN),
                params: vec![&part, scope, &from, &to],
            }];
            let answer = self.host.transaction("plog_scan", &statements).await?;
            rows(answer.into_iter().next().unwrap_or_default())
        })
    }

    fn load<'a>(
        &'a self,
        part: Partition,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Rows, StoreError>> {
        Box::pin(async move {
            let part = part.0.to_string();
            let statements = [SqlStatement {
                sql: self.pick(&LOAD),
                params: vec![&part, scope],
            }];
            let answer = self.host.transaction("plog_load", &statements).await?;
            rows(answer.into_iter().next().unwrap_or_default())
        })
    }

    fn compact<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        scope: &'a str,
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let (part, epoch_text) = (part.0.to_string(), token(epoch));
            let seqs: Vec<String> = compaction
                .archived
                .iter()
                .map(|(seq, _)| seq.to_string())
                .collect();
            let ranges: Vec<(String, String)> = std::iter::once(&compaction.dropped)
                .chain(compaction.superseded)
                .filter(|range| !range.is_empty())
                .map(|range| {
                    (
                        range.start.to_string(),
                        range.end.min(i64::MAX as u64).to_string(),
                    )
                })
                .collect();
            let payloads: Vec<Option<String>> = if self.postgres() {
                crate::codec::binaries(compaction.archived)
                    .into_iter()
                    .map(|payload| payload.map(|(_, padded)| padded))
                    .collect()
            } else {
                vec![None; compaction.archived.len()]
            };
            let mut statements = Vec::new();
            for (((_, body), seq), payload) in compaction.archived.iter().zip(&seqs).zip(&payloads)
            {
                statements.push(match payload {
                    Some(padded) => SqlStatement {
                        sql: ARCHIVE_DATA,
                        params: vec![&part, scope, seq, body, &epoch_text, padded],
                    },
                    None => SqlStatement {
                        sql: self.pick(&ARCHIVE),
                        params: vec![&part, scope, seq, body, &epoch_text],
                    },
                });
            }
            for (from, to) in &ranges {
                statements.push(SqlStatement {
                    sql: self.pick(&DROP),
                    params: vec![&part, scope, from, to, &epoch_text],
                });
            }
            self.fenced("plog_compact", &part, epoch, statements)
                .await
                .map(drop)
        })
    }

    fn extend<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let (part, epoch_text) = (part.0.to_string(), token(epoch));
            let (row, seq, cut) = (
                extension.row.to_string(),
                extension.seq.to_string(),
                extension.cut.to_string(),
            );
            let binary = self.postgres().then(|| {
                let tail = crate::codec::tail(extension.tail)
                    .expect("an extension's tail is one whole row's encoding");
                (crate::codec::decoded_bytes(extension.cut).to_string(), tail)
            });
            let mut params = vec![&*part, scope, &row, &seq, &cut, extension.tail, &epoch_text];
            if let Some((cut, tail)) = &binary {
                params.extend([cut.as_str(), tail.as_str()]);
            }
            let statements = vec![SqlStatement {
                sql: self.pick(&EXTEND),
                params,
            }];
            let answer = self.fenced("plog_extend", &part, epoch, statements).await?;
            if result(&answer, 1)?.is_empty() {
                return Err(StoreError::Fenced);
            }
            Ok(())
        })
    }

    fn follow(
        &self,
        part: Partition,
        after: u64,
        limit: usize,
    ) -> StoreFuture<'_, Result<Vec<Entry>, StoreError>> {
        Box::pin(async move {
            let (part, after, limit) = (part.0.to_string(), after.to_string(), limit.to_string());
            let statements = [SqlStatement {
                sql: self.pick(&FOLLOW),
                params: vec![&part, &after, &limit],
            }];
            let answer = self.host.transaction("plog_follow", &statements).await?;
            answer
                .into_iter()
                .next()
                .unwrap_or_default()
                .into_iter()
                .map(|cells| {
                    Ok(Entry {
                        pos: number(&cells, 0)?,
                        scope: Arc::from(cell(&cells, 1)?),
                        seq: number(&cells, 2)?,
                        body: cell(&cells, 3)?.to_owned(),
                    })
                })
                .collect()
        })
    }

    fn fence(&self, part: Partition) -> StoreFuture<'_, Result<Row<String>, StoreError>> {
        Box::pin(async move {
            let part_text = part.0.to_string();
            let statements = [SqlStatement {
                sql: self.pick(&FENCE),
                params: vec![&part_text],
            }];
            let answer = self.host.transaction("plog_fence", &statements).await?;
            let cells = answer
                .into_iter()
                .next()
                .and_then(|rows| rows.into_iter().next())
                .ok_or_else(|| {
                    StoreError::Backend(format!("log partition {} is not prepared", part.0))
                })?;
            let epoch = number(&cells, 0)?;
            let holder = cell(&cells, 1)?;
            let cursor = number(&cells, 3)?;
            Ok(if holder.is_empty() {
                Row::Released { epoch, cursor }
            } else {
                Row::Held {
                    holder: holder.to_owned(),
                    epoch,
                    expires_at: number(&cells, 2)?,
                    cursor,
                }
            })
        })
    }

    fn claim_lease<'a>(
        &'a self,
        part: Partition,
        node: &'a str,
        ttl_ms: u64,
        grace_ms: u64,
    ) -> StoreFuture<'a, Result<Option<(u64, u64)>, StoreError>> {
        Box::pin(async move {
            let (part, ttl, grace) = (part.0.to_string(), ttl_ms.to_string(), grace_ms.to_string());
            let statements = [
                SqlStatement {
                    sql: self.pick(&CLAIM),
                    params: vec![&part, node, &ttl, &grace],
                },
                SqlStatement {
                    sql: self.pick(&CLAIM_HEAD),
                    params: vec![&part, node],
                },
            ];
            let answer = self.host.transaction("plog_claim", &statements).await?;
            let Some(cells) = result(&answer, 0)?.first() else {
                return Ok(None);
            };
            Ok(Some((number(cells, 0)?, number(cells, 1)?)))
        })
    }

    fn renew_lease<'a>(
        &'a self,
        part: Partition,
        node: &'a str,
        epoch: u64,
        ttl_ms: u64,
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            let (part, epoch, ttl) = (part.0.to_string(), epoch.to_string(), ttl_ms.to_string());
            let statements = [SqlStatement {
                sql: self.pick(&RENEW),
                params: vec![&part, &epoch, node, &ttl],
            }];
            let answer = self.host.transaction("plog_renew", &statements).await?;
            Ok(!result(&answer, 0)?.is_empty())
        })
    }

    fn renew_leases<'a>(
        &'a self,
        node: &'a str,
        held: &'a [(Partition, u64)],
        ttl_ms: u64,
    ) -> StoreFuture<'a, Result<Vec<Partition>, StoreError>> {
        Box::pin(async move {
            let pairs: Vec<[u64; 2]> = held
                .iter()
                .map(|(part, epoch)| [u64::from(part.0), *epoch])
                .collect();
            let json = serde_json::to_string(&pairs)
                .map_err(|error| StoreError::Backend(format!("encode renewals: {error}")))?;
            let ttl = ttl_ms.to_string();
            let statements = [SqlStatement {
                sql: self.pick(&RENEW_ALL),
                params: vec![node, &json, &ttl],
            }];
            let answer = self.host.transaction("plog_renew_all", &statements).await?;
            result(&answer, 0)?
                .iter()
                .map(|cells| {
                    let part = number(cells, 0)?;
                    u32::try_from(part)
                        .map(Partition)
                        .map_err(|error| invalid(format!("partition {part}: {error}")))
                })
                .collect()
        })
    }

    fn release_lease<'a>(
        &'a self,
        part: Partition,
        node: &'a str,
        epoch: u64,
    ) -> StoreFuture<'a, Result<Option<u64>, StoreError>> {
        Box::pin(async move {
            let (part, epoch) = (part.0.to_string(), epoch.to_string());
            let statements = [
                SqlStatement {
                    sql: self.pick(&RELEASE),
                    params: vec![&part, &epoch, node],
                },
                SqlStatement {
                    sql: self.pick(&RELEASE_HEAD),
                    params: vec![&part, &epoch],
                },
            ];
            let answer = self.host.transaction("plog_release", &statements).await?;
            result(&answer, 0)?
                .first()
                .map(|cells| number(cells, 0))
                .transpose()
        })
    }

    fn cursor<'a>(
        &'a self,
        part: Partition,
        reader: &'a str,
        pos: u64,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let (part, pos) = (part.0.to_string(), pos.to_string());
            let statements = [SqlStatement {
                sql: self.pick(&CURSOR),
                params: vec![&part, reader, &pos],
            }];
            self.host
                .transaction("plog_cursor", &statements)
                .await
                .map(drop)
        })
    }

    fn prune<'a>(
        &'a self,
        part: Partition,
        epoch: Option<u64>,
        scope: &'a str,
        before: u64,
    ) -> StoreFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let (part, epoch_text, before) = (
                part.0.to_string(),
                token(epoch),
                before.min(i64::MAX as u64).to_string(),
            );
            let statements = vec![SqlStatement {
                sql: self.pick(&PRUNE),
                params: vec![&part, scope, &before, &epoch_text],
            }];
            let answer = self.fenced("plog_prune", &part, epoch, statements).await?;
            Ok(result(&answer, 1)?.len() as u64)
        })
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        Some(Box::pin(load_checkpoint(&*self.host, scope)))
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        Some(Box::pin(store_checkpoint(&*self.host, scope, checkpoint)))
    }
}
