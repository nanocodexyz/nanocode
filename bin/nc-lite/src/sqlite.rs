use std::{path::Path, time::Duration};

use nanocode_durability_core::{Sql, SqlAccess, SqlRow, SqlStatement, StoreError, StoreFuture};
use nanocode_sync::{mpsc, oneshot};
use rusqlite::{Connection, TransactionBehavior, params_from_iter, types::ValueRef};

type Rows = Result<Vec<Vec<SqlRow>>, StoreError>;

const JOBS_WAITING: usize = 1;

struct Job {
    behavior: TransactionBehavior,
    statements: Vec<(&'static str, Vec<String>)>,
    reply: oneshot::Sender<Rows>,
}

pub struct Sqlite {
    jobs: mpsc::Sender<Job>,
}

impl Sqlite {
    pub fn open(path: &Path, busy: Duration) -> Result<Self, StoreError> {
        let connection = Connection::open(path).map_err(backend)?;
        connection.busy_timeout(busy).map_err(backend)?;
        connection
            .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .map_err(backend)?;
        connection
            .execute_batch("PRAGMA synchronous = NORMAL")
            .map_err(backend)?;
        let (jobs, receiver) = mpsc::channel(JOBS_WAITING);
        std::thread::Builder::new()
            .name("nc-lite-sqlite".to_owned())
            .spawn(move || serve(connection, receiver))
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(Self { jobs })
    }
}

fn serve(mut connection: Connection, mut jobs: mpsc::Receiver<Job>) {
    while let Some(job) = jobs.blocking_recv() {
        let _ = job
            .reply
            .send(run(&mut connection, job.behavior, &job.statements));
    }
}

fn run(
    connection: &mut Connection,
    behavior: TransactionBehavior,
    statements: &[(&'static str, Vec<String>)],
) -> Rows {
    let transaction = connection
        .transaction_with_behavior(behavior)
        .map_err(backend)?;
    let mut answered = Vec::with_capacity(statements.len());
    for (sql, params) in statements {
        let mut statement = transaction.prepare_cached(sql).map_err(backend)?;
        let columns = statement.column_count();
        let mut rows = statement
            .query(params_from_iter(params.iter()))
            .map_err(backend)?;
        let mut collected = Vec::new();
        while let Some(row) = rows.next().map_err(backend)? {
            let values = (0..columns)
                .map(|at| row.get_ref(at).map(text))
                .collect::<Result<SqlRow, _>>()
                .map_err(backend)?;
            collected.push(values);
        }
        answered.push(collected);
    }
    transaction.commit().map_err(backend)?;
    Ok(answered)
}

fn text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(number) => Some(number.to_string()),
        ValueRef::Real(number) => Some(number.to_string()),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

fn backend(error: rusqlite::Error) -> StoreError {
    StoreError::Backend(error.to_string())
}

fn stopped() -> StoreError {
    StoreError::Backend("the SQLite thread stopped".to_owned())
}

impl Sql for Sqlite {
    fn call_bound(&self) -> Option<Duration> {
        None
    }

    fn transaction<'a>(
        &'a self,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Rows> {
        self.run(SqlAccess::Write, operation, statements)
    }

    fn run<'a>(
        &'a self,
        access: SqlAccess,
        _operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Rows> {
        let behavior = match access {
            SqlAccess::Read => TransactionBehavior::Deferred,
            SqlAccess::Write | SqlAccess::Schema | SqlAccess::Autocommit => {
                TransactionBehavior::Immediate
            }
        };
        let (reply, answer) = oneshot::channel();
        let job = Job {
            behavior,
            statements: statements
                .iter()
                .map(|statement| {
                    let params = statement.params.iter().map(|&param| param.to_owned());
                    (statement.sql, params.collect())
                })
                .collect(),
            reply,
        };
        Box::pin(async move {
            self.jobs.send(job).await.map_err(|_| stopped())?;
            answer.await.map_err(|_| stopped())?
        })
    }
}
