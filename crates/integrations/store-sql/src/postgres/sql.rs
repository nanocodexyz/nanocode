use crate::postgres::{
    Lent, PgBounds, PgCancel, PgConnection, PgError, PgLimits, PgPool, Pooled,
    connection::BOUND_SESSION,
};
use nanocode_durability_core::{
    Settle, Sql, SqlAccess, SqlDialect, SqlRow, SqlStatement, StoreError, StoreFuture,
};
use nanocode_platform::Elapsed;
use std::{sync::Arc, time::Duration};

const RESET_SESSION: &str = "RESET ALL";
const LOCK_NOT_AVAILABLE: &str = "55P03";
const QUERY_CANCELED: &str = "57014";
const VARLENA_SIZE_MAX: u64 = (1 << 30) - 1;
const VARLENA_HEADER_BYTES: u64 = 4;
const TEXT_BYTES_MAX: u64 = VARLENA_SIZE_MAX - VARLENA_HEADER_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PgTimes {
    pub waited: Duration,
    pub ran: Duration,
}

pub struct PgSql {
    writes: PgPool,
    reads: PgPool,
    timed: Option<fn(&'static str, PgTimes)>,
}

impl PgSql {
    #[must_use]
    pub const fn reading(writes: PgPool, reads: PgPool) -> Self {
        Self {
            writes,
            reads,
            timed: None,
        }
    }

    #[must_use]
    pub fn timed(mut self, timed: fn(&'static str, PgTimes)) -> Self {
        self.timed = Some(timed);
        self
    }

    fn time(
        &self,
        operation: &'static str,
        started: nanocode_platform::Instant,
        acquired: nanocode_platform::Instant,
    ) {
        if let Some(timed) = self.timed {
            timed(
                operation,
                PgTimes {
                    waited: acquired - started,
                    ran: nanocode_platform::elapsed(acquired),
                },
            );
        }
    }

    async fn transact(
        &self,
        access: SqlAccess,
        operation: &'static str,
        settle: Settle,
        statements: &[SqlStatement<'_>],
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        let pool = match access {
            SqlAccess::Read => &self.reads,
            SqlAccess::Write | SqlAccess::Schema | SqlAccess::Autocommit => &self.writes,
        };
        let started = nanocode_platform::now();
        let connection = pool
            .acquire()
            .await
            .map_err(|error| refused(access, error))?;
        let acquired = nanocode_platform::now();
        let result = answer_within(
            connection,
            pool.limits(),
            access,
            settle,
            statements,
            |key| async move { pool.cancel(&key).await },
        )
        .await;
        self.time(operation, started, acquired);
        result
    }

    async fn autocommit(
        &self,
        operation: &'static str,
        statements: &[SqlStatement<'_>],
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        let access = SqlAccess::Autocommit;
        let started = nanocode_platform::now();
        let mut connection = self
            .writes
            .acquire()
            .await
            .map_err(|error| refused(access, error))?;
        let acquired = nanocode_platform::now();
        let bounds = self.writes.limits().bounds(access);
        let key = connection.cancel_key();
        let settings = bounds.settings();
        let bound = SqlStatement {
            sql: BOUND_SESSION,
            params: settings.iter().map(String::as_str).collect(),
        };
        let reset = SqlStatement {
            sql: RESET_SESSION,
            params: Vec::new(),
        };
        let mut results = Vec::with_capacity(statements.len());
        let mut failed = None;
        for (at, statement) in std::iter::once(&bound).chain(statements).enumerate() {
            match within(&mut connection, bounds.statement, statement).await {
                Some(Ok(answer)) if at > 0 => results.extend(answer),
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    failed = Some(error);
                    break;
                }
                None => {
                    let error = unanswered(&self.writes, connection, &key).await;
                    self.time(operation, started, acquired);
                    return Err(refused(access, error));
                }
            }
        }
        let Some(reset) = within(&mut connection, bounds.statement, &reset).await else {
            let error = unanswered(&self.writes, connection, &key).await;
            self.time(operation, started, acquired);
            return Err(refused(access, error));
        };
        self.time(operation, started, acquired);
        if let Some(error) = failed {
            return Err(refused(access, error));
        }
        reset.map_err(|error| refused(access, error))?;
        Ok(results)
    }
}

pub trait Answering: Send + 'static {
    type Key;

    fn key(&self) -> Self::Key;

    fn answer<'a>(
        &'a mut self,
        settle: Settle,
        bounds: PgBounds,
        statements: &'a [SqlStatement<'a>],
    ) -> impl Future<Output = Result<Vec<Vec<SqlRow>>, PgError>> + Send + 'a;
}

impl Answering for PgConnection {
    type Key = PgCancel;

    fn key(&self) -> PgCancel {
        self.cancel_key()
    }

    fn answer<'a>(
        &'a mut self,
        settle: Settle,
        bounds: PgBounds,
        statements: &'a [SqlStatement<'a>],
    ) -> impl Future<Output = Result<Vec<Vec<SqlRow>>, PgError>> + Send + 'a {
        self.bounded(settle, bounds, statements)
    }
}

pub async fn answer_within<C: Answering, F: Future<Output = Result<(), PgError>>>(
    mut connection: Lent<C>,
    limits: &PgLimits,
    access: SqlAccess,
    settle: Settle,
    statements: &[SqlStatement<'_>],
    cancel: impl FnOnce(C::Key) -> F,
) -> Result<Vec<Vec<SqlRow>>, StoreError> {
    let bounds = limits.bounds(access);
    let key = connection.key();
    let answered = nanocode_platform::timeout(
        bounds.statement,
        connection.answer(settle, bounds, statements),
    )
    .await;
    match answered {
        Ok(result) => result.map_err(|error| refused(access, error)),
        Err(Elapsed) => {
            let cancelled = nanocode_platform::timeout(limits.acquire, cancel(key)).await;
            connection.discard();
            match cancelled {
                Ok(Ok(()) | Err(_)) | Err(Elapsed) => Err(refused(access, PgError::Unanswered)),
            }
        }
    }
}

async fn within(
    connection: &mut Pooled,
    wait: Duration,
    statement: &SqlStatement<'_>,
) -> Option<Result<Vec<Vec<SqlRow>>, PgError>> {
    nanocode_platform::timeout(wait, connection.run(std::slice::from_ref(statement)))
        .await
        .ok()
}

async fn unanswered(pool: &PgPool, connection: Pooled, key: &PgCancel) -> PgError {
    let cancelled = pool.cancel(key).await;
    connection.discard();
    match cancelled {
        Ok(()) | Err(_) => PgError::Unanswered,
    }
}

fn refused(access: SqlAccess, error: PgError) -> StoreError {
    match &error {
        PgError::Busy => StoreError::Busy,
        PgError::Unanswered if access == SqlAccess::Read => StoreError::Busy,
        PgError::Database(database)
            if database.code == LOCK_NOT_AVAILABLE || database.code == QUERY_CANCELED =>
        {
            StoreError::Busy
        }
        _ => backend(error),
    }
}

impl Sql for PgSql {
    fn transaction<'a>(
        &'a self,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        Box::pin(self.transact(SqlAccess::Write, operation, Settle::Durable, statements))
    }

    fn call_bound(&self) -> Option<Duration> {
        Some(
            self.writes
                .limits()
                .call_bound()
                .max(self.reads.limits().call_bound()),
        )
    }

    fn read_capacity(&self) -> u32 {
        u32::try_from(self.reads.connections()).expect("a pool's connection count fits in u32")
    }

    fn row_bytes_max(&self) -> Option<u64> {
        Some(TEXT_BYTES_MAX)
    }

    fn run<'a>(
        &'a self,
        access: SqlAccess,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        if access == SqlAccess::Autocommit {
            return Box::pin(self.autocommit(operation, statements));
        }
        Box::pin(self.transact(access, operation, Settle::Durable, statements))
    }

    fn write<'a>(
        &'a self,
        settle: Settle,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        Box::pin(self.transact(SqlAccess::Write, operation, settle, statements))
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    fn concurrent(self: Arc<Self>) -> Option<Arc<dyn Sql>> {
        (self.writers() > 1 || self.readers() > 1).then_some(self as Arc<dyn Sql>)
    }

    fn writers(&self) -> usize {
        self.writes.connections()
    }

    fn readers(&self) -> usize {
        self.reads.connections()
    }
}

fn backend(error: PgError) -> StoreError {
    StoreError::Backend(error.to_string())
}
