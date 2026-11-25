use std::sync::Arc;

use nanocode_turnlog::LogHost;

pub use nanocode_board_fold::*;

pub mod access;
pub mod checkpoint;
pub mod cost;
pub mod files;
pub mod middleware;
pub mod shards;
pub mod steer;
pub mod tasks;
pub mod wake;
pub mod watch;

use sim::{Mutation, mutated};

const END: u64 = i64::MAX as u64;

/// How many times [`Board::catch_up`] tries a read that fails.
pub const READ_TRIES: usize = 3;

pub struct Board {
    host: Arc<dyn LogHost>,
    scope: String,
    writer: String,
    identity: access::Identity,
    minted: u64,
    minted_by: std::collections::BTreeMap<String, u64>,
    fold: Fold,
    contended: bool,
    chain: middleware::Chain,
    opened: checkpoint::Opened,
    /// The seq of the last checkpoint this board saved or opened from.
    checkpointed: u64,
    /// Cleared once the host says it keeps no checkpoints.
    checkpoints: bool,
    /// The saver's fold while no save runs; empty while one does.
    saver: Arc<nanocode_sync::Slot<checkpoint::Shadow>>,
}

impl Board {
    pub async fn open(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
    ) -> Result<Self, BoardError> {
        Self::open_into(host, scope, identity, Fold::new()).await
    }

    /// Opens as open does, folding the scope's rows into fold (a retaining one, say) rather than an empty fold. A
    /// fold reopened from a checkpoint takes fold's retention and kept labels.
    pub async fn open_into(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
        fold: Fold,
    ) -> Result<Self, BoardError> {
        let mut board = match checkpoint::reopen(&host, scope).await {
            Ok((reopened, rows)) => {
                let fold = checkpoint::adopt(reopened, fold.retain(), fold.kept_labels());
                let seq = fold.next();
                let tail = rows.len() as u64;
                let mut board = Self::start(host, scope, identity, fold, rows)?;
                board.opened = checkpoint::Opened::Checkpoint { seq, tail };
                board.checkpointed = seq;
                board
            }
            Err(fallback) => {
                let rows = host.load(scope).await?;
                let mut board = Self::start(host, scope, identity, fold, rows)?;
                board.opened = checkpoint::Opened::Replay(fallback);
                board
            }
        };
        board.keep_checkpoint();
        Ok(board)
    }

    pub async fn resume(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
        fold: Fold,
    ) -> Result<Self, BoardError> {
        let from = fold.next() + u64::from(mutated(Mutation::ResumeSkipsRows));
        let rows = host.scan(scope, from, END).await?;
        Self::start(host, scope, identity, fold, rows)
    }

    fn start(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
        fold: Fold,
        rows: Vec<(u64, String)>,
    ) -> Result<Self, BoardError> {
        let mut board = Self {
            host,
            scope: scope.to_owned(),
            writer: identity.actor().to_string(),
            identity,
            minted: 0,
            minted_by: std::collections::BTreeMap::new(),
            fold,
            contended: false,
            chain: middleware::Chain::default(),
            opened: checkpoint::Opened::Resumed,
            checkpointed: 0,
            checkpoints: true,
            saver: Arc::new(nanocode_sync::Slot::new(Some(checkpoint::Shadow::Unbuilt))),
        };
        board.fold_rows(rows)?;
        if !mutated(Mutation::MintedRestarts) {
            board.minted = board.fold.minted(&board.writer);
        }
        Ok(board)
    }

    pub const fn fold(&self) -> &Fold {
        &self.fold
    }

    pub fn writer(&self) -> &str {
        &self.writer
    }

    pub fn read(&self, query: &messages::Query) -> messages::Page<'_> {
        self.fold.read_as(Some(self.identity.actor()), query)
    }

    pub fn heard(&self) -> impl Iterator<Item = &messages::Message> + '_ {
        self.fold.heard_by(self.identity.actor())
    }

    fn fold_rows(&mut self, rows: Vec<(u64, String)>) -> Result<(), BoardError> {
        for (seq, body) in rows {
            self.fold.apply(seq, &body)?;
        }
        Ok(())
    }

    pub async fn refresh(&mut self) -> Result<(), BoardError> {
        let rows = self.host.scan(&self.scope, self.fold.next(), END).await?;
        self.fold_rows(rows)
    }

    /// Refreshes, trying a read that fails with a store error up to [`READ_TRIES`] times. A
    /// failed refresh leaves the fold where it was, so it hides tasks, claims, steers and
    /// phases that landed since: a caller that answers or decides from the fold calls this
    /// and learns whether the fold is current.
    pub async fn catch_up(&mut self) -> Result<(), BoardError> {
        let tries = if mutated(Mutation::RefreshOnce) {
            1
        } else {
            READ_TRIES
        };
        let mut tried = 1;
        loop {
            match self.refresh().await {
                Err(BoardError::Store(_)) if tried < tries => tried += 1,
                result => return result,
            }
        }
    }

    /// The log scope this board reads and appends.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// The host this board reads and appends through.
    pub fn host(&self) -> &Arc<dyn LogHost> {
        &self.host
    }

    pub async fn submit(&mut self, at_ms: u64, op: Op) -> Result<Applied, BoardError> {
        self.minted += 1;
        let id = EventId(format!("{}.{}", self.writer, self.minted));
        self.submit_as(id, at_ms, op).await
    }

    pub async fn submit_keyed(
        &mut self,
        key: &str,
        at_ms: u64,
        op: Op,
    ) -> Result<Applied, BoardError> {
        if mutated(Mutation::KeysIgnored) {
            return self.submit(at_ms, op).await;
        }
        self.submit_as(EventId(format!("{}#{key}", self.writer)), at_ms, op)
            .await
    }

    async fn submit_as(&mut self, id: EventId, at_ms: u64, op: Op) -> Result<Applied, BoardError> {
        let applied = self.submit_once(id, at_ms, op).await;
        self.keep_checkpoint();
        applied
    }

    async fn submit_once(
        &mut self,
        id: EventId,
        at_ms: u64,
        op: Op,
    ) -> Result<Applied, BoardError> {
        if mutated(Mutation::MiddlewareRerunsLanded) {
            self.chain.vet(&id, &self.writer, &op)?;
        }
        if let Some(verdict) = self.fold.outcome(&id) {
            return verdict
                .clone()
                .map(|seq| Applied { seq, event: id })
                .map_err(BoardError::Rejected);
        }
        let op = self.chain.vet(&id, &self.writer, &op)?;
        let mut pending = self.fold.submit(Event {
            id,
            by: self.writer.clone(),
            at_ms,
            op,
        })?;
        let mut step = if self.contended {
            None
        } else {
            pending.optimistic(&self.fold)
        };
        loop {
            let step = match step.take() {
                Some(step) => step,
                None => {
                    let rows = self.host.scan(&self.scope, self.fold.next(), END).await;
                    if rows.as_ref().is_ok_and(Vec::is_empty) {
                        self.contended = false;
                    }
                    pending.read(&mut self.fold, rows)?
                }
            };
            match step {
                Step::Done(applied) => return Ok(applied),
                Step::Append { seq, body } => {
                    let appended = self.host.append(&self.scope, &[(seq, body)]).await;
                    self.contended |= appended.is_err();
                    if let Some(applied) = pending.acknowledged(&mut self.fold, seq, appended)? {
                        return Ok(applied);
                    }
                }
            }
        }
    }
}

/// One write in a group: by another actor than the board's writer when `by` is set (a node
/// committing for the agents it hosts), keyed for idempotence when `key` is set.
#[derive(Clone, Debug)]
pub struct Write {
    pub by: Option<String>,
    pub key: Option<String>,
    pub at_ms: u64,
    pub op: Op,
}

impl Board {
    fn mint(&mut self, by: &str, key: Option<&str>) -> EventId {
        if let Some(key) = key.filter(|_| !mutated(Mutation::KeysIgnored)) {
            return EventId(format!("{by}#{key}"));
        }
        let minted = if by == self.writer {
            &mut self.minted
        } else {
            let fold = &self.fold;
            self.minted_by
                .entry(by.to_owned())
                .or_insert_with(|| fold.minted(by))
        };
        *minted += 1;
        EventId(format!("{by}.{minted}"))
    }

    /// Commits many writes as one group: their rows go down in one append at the fold's next
    /// seq, and the fold judges each in order. Returns each write's verdict, in order, and how
    /// many appends the group took.
    pub async fn submit_group(
        &mut self,
        writes: Vec<Write>,
    ) -> (Vec<Result<Applied, BoardError>>, u64) {
        let mut verdicts: Vec<Option<Result<Applied, BoardError>>> = vec![None; writes.len()];
        if self.contended
            && let Err(error) = self.refresh().await
        {
            return (writes.iter().map(|_| Err(error.clone())).collect(), 0);
        }
        let mut members = Vec::new();
        let mut events = Vec::new();
        for (index, write) in writes.into_iter().enumerate() {
            let by = write.by.unwrap_or_else(|| self.writer.clone());
            let id = self.mint(&by, write.key.as_deref());
            match self.chain.vet(&id, &by, &write.op) {
                Ok(op) => {
                    members.push(index);
                    events.push(Event {
                        id,
                        by,
                        at_ms: write.at_ms,
                        op,
                    });
                }
                Err(error) => verdicts[index] = Some(Err(error)),
            }
        }
        let mut group = self.fold.group(events);
        self.drive_group(&mut group).await;
        let rounds = group.rounds();
        for (index, verdict) in members.into_iter().zip(group.settled()) {
            verdicts[index] = Some(verdict);
        }
        let verdicts = verdicts
            .into_iter()
            .map(|verdict| verdict.unwrap_or_else(|| Err(BoardError::Dropped("unsettled".into()))))
            .collect();
        self.keep_checkpoint();
        (verdicts, rounds)
    }

    async fn drive_group(&mut self, group: &mut group::Group) {
        loop {
            let (first, rows) = match group.plan(&self.fold) {
                group::GroupStep::Done => break,
                group::GroupStep::Scan { from } => {
                    let rows = self.host.scan(&self.scope, from, END).await;
                    if let Err(error) = group.read(&mut self.fold, rows) {
                        group.fail_open(&error);
                        break;
                    }
                    continue;
                }
                group::GroupStep::Append { first, rows } => (first, rows),
            };
            let appended = if mutated(Mutation::GroupSplits) {
                let mut appended = Ok(());
                for row in &rows {
                    appended = self
                        .host
                        .append(&self.scope, std::slice::from_ref(row))
                        .await;
                    if appended.is_err() {
                        break;
                    }
                }
                appended
            } else {
                self.host.append(&self.scope, &rows).await
            };
            let settled = match appended {
                Ok(()) => group.acknowledged(&mut self.fold, first),
                Err(error) => {
                    self.contended = true;
                    group.failed(first, error);
                    let rows = self.host.scan(&self.scope, self.fold.next(), END).await;
                    if rows.as_ref().is_ok_and(Vec::is_empty) {
                        self.contended = false;
                    }
                    group.read(&mut self.fold, rows)
                }
            };
            if let Err(error) = settled {
                group.fail_open(&error);
                break;
            }
        }
    }
}
