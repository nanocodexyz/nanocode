use std::sync::Arc;

use nanocode_durability_core::{Migration, Sql, SqlRow, SqlStatement, StoreError, migrate_format};

use crate::embedded::HostFuture;
use crate::remote::RemoteReply;

pub struct PendingRequest {
    pub id: u64,
    pub key: String,
    pub runtime: String,
    pub session_id: String,
    pub call_id: String,
    pub turn_id: Option<String>,
    pub reply: Option<RemoteReply>,
}

pub trait HandRequests: Send + Sync + 'static {
    fn sent<'a>(&'a self, request: &'a PendingRequest) -> HostFuture<'a, Result<(), String>>;
    fn replied<'a>(
        &'a self,
        key: &'a str,
        runtime: &'a str,
        id: u64,
        reply: &'a RemoteReply,
    ) -> HostFuture<'a, Result<(), String>>;
    fn pending<'a>(
        &'a self,
        key: &'a str,
        runtime: &'a str,
    ) -> HostFuture<'a, Result<Vec<u64>, String>>;
    fn find<'a>(
        &'a self,
        session_id: &'a str,
        call_id: &'a str,
    ) -> HostFuture<'a, Result<Option<PendingRequest>, String>>;
    fn settle<'a>(&'a self, session_id: &'a str) -> HostFuture<'a, Result<(), String>>;
}

type Dialects = [&'static str; 2];

const CREATE: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS nanocode_hand_requests (session_id TEXT NOT NULL, call_id TEXT NOT NULL, id TEXT NOT NULL, key TEXT NOT NULL, runtime TEXT NOT NULL, turn_id TEXT NOT NULL, reply TEXT, PRIMARY KEY (session_id, call_id))",
    "CREATE INDEX IF NOT EXISTS nanocode_hand_requests_runtime ON nanocode_hand_requests (key, runtime)",
];
const TABLE: Migration = Migration {
    statements: [&CREATE, &CREATE],
    adopt: None,
};
const SENT: Dialects = [
    "INSERT INTO nanocode_hand_requests (session_id, call_id, id, key, runtime, turn_id, reply) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL) ON CONFLICT (session_id, call_id) DO UPDATE SET id = excluded.id, key = excluded.key, runtime = excluded.runtime, turn_id = excluded.turn_id, reply = NULL",
    "INSERT INTO nanocode_hand_requests (session_id, call_id, id, key, runtime, turn_id, reply) VALUES ($1::text, $2::text, $3::text, $4::text, $5::text, $6::text, NULL) ON CONFLICT (session_id, call_id) DO UPDATE SET id = excluded.id, key = excluded.key, runtime = excluded.runtime, turn_id = excluded.turn_id, reply = NULL",
];
const REPLIED: Dialects = [
    "UPDATE nanocode_hand_requests SET reply = ?4 WHERE key = ?1 AND runtime = ?2 AND id = ?3",
    "UPDATE nanocode_hand_requests SET reply = $4::text WHERE key = $1::text AND runtime = $2::text AND id = $3::text",
];
const PENDING: Dialects = [
    "SELECT id FROM nanocode_hand_requests WHERE key = ?1 AND runtime = ?2 AND reply IS NULL",
    "SELECT id FROM nanocode_hand_requests WHERE key = $1::text AND runtime = $2::text AND reply IS NULL",
];
const FIND: Dialects = [
    "SELECT id, key, runtime, turn_id, reply FROM nanocode_hand_requests WHERE session_id = ?1 AND call_id = ?2",
    "SELECT id, key, runtime, turn_id, reply FROM nanocode_hand_requests WHERE session_id = $1::text AND call_id = $2::text",
];
const SETTLE: Dialects = [
    "DELETE FROM nanocode_hand_requests WHERE session_id = ?1",
    "DELETE FROM nanocode_hand_requests WHERE session_id = $1::text",
];

pub struct SqlRequests {
    host: Arc<dyn Sql>,
}

fn column(row: &SqlRow, index: usize) -> Result<&str, String> {
    row.get(index)
        .and_then(Option::as_deref)
        .ok_or_else(|| format!("hand request row lacks column {index}"))
}

fn id(text: &str) -> Result<u64, String> {
    text.parse()
        .map_err(|_| format!("hand request id '{text}' is not a u64"))
}

fn encoded(error: serde_json::Error) -> String {
    format!("hand request JSON: {error}")
}

impl SqlRequests {
    #[must_use]
    pub fn new(host: Arc<dyn Sql>) -> Self {
        Self { host }
    }

    pub async fn migrate(&self) -> Result<(), StoreError> {
        migrate_format(&*self.host, "hand_requests", &[TABLE]).await
    }

    async fn run(
        &self,
        operation: &'static str,
        statement: &Dialects,
        params: Vec<&str>,
    ) -> Result<Vec<SqlRow>, String> {
        let statements = [SqlStatement {
            sql: self.host.dialect().pick(statement),
            params,
        }];
        let mut results = self
            .host
            .transaction(operation, &statements)
            .await
            .map_err(|error| error.to_string())?;
        results
            .pop()
            .ok_or_else(|| format!("SQL host answered no result for {operation}"))
    }
}

impl HandRequests for SqlRequests {
    fn sent<'a>(&'a self, request: &'a PendingRequest) -> HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let id = request.id.to_string();
            let turn = serde_json::to_string(&request.turn_id).map_err(encoded)?;
            let params = vec![
                request.session_id.as_str(),
                request.call_id.as_str(),
                &id,
                request.key.as_str(),
                request.runtime.as_str(),
                &turn,
            ];
            self.run("hand_request_sent", &SENT, params).await.map(drop)
        })
    }

    fn replied<'a>(
        &'a self,
        key: &'a str,
        runtime: &'a str,
        id: u64,
        reply: &'a RemoteReply,
    ) -> HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let id = id.to_string();
            let reply = serde_json::to_string(reply).map_err(encoded)?;
            let params = vec![key, runtime, &id, &reply];
            self.run("hand_request_replied", &REPLIED, params)
                .await
                .map(drop)
        })
    }

    fn pending<'a>(
        &'a self,
        key: &'a str,
        runtime: &'a str,
    ) -> HostFuture<'a, Result<Vec<u64>, String>> {
        Box::pin(async move {
            self.run("hand_requests_pending", &PENDING, vec![key, runtime])
                .await?
                .iter()
                .map(|row| id(column(row, 0)?))
                .collect()
        })
    }

    fn find<'a>(
        &'a self,
        session_id: &'a str,
        call_id: &'a str,
    ) -> HostFuture<'a, Result<Option<PendingRequest>, String>> {
        Box::pin(async move {
            let rows = self
                .run("hand_request_find", &FIND, vec![session_id, call_id])
                .await?;
            let Some(row) = rows.first() else {
                return Ok(None);
            };
            let reply = row
                .get(4)
                .and_then(Option::as_deref)
                .map(serde_json::from_str)
                .transpose()
                .map_err(encoded)?;
            Ok(Some(PendingRequest {
                id: id(column(row, 0)?)?,
                key: column(row, 1)?.to_owned(),
                runtime: column(row, 2)?.to_owned(),
                session_id: session_id.to_owned(),
                call_id: call_id.to_owned(),
                turn_id: serde_json::from_str(column(row, 3)?).map_err(encoded)?,
                reply,
            }))
        })
    }

    fn settle<'a>(&'a self, session_id: &'a str) -> HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.run("hand_requests_settle", &SETTLE, vec![session_id])
                .await
                .map(drop)
        })
    }
}
