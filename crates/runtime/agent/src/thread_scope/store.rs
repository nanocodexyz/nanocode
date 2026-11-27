use std::{collections::BTreeMap, ops::Bound, sync::Arc};

use nanocode_sync::Atom;
use nanocode_turnlog::Record;

use nanocode_scope::ScopeError;

use super::{
    Staged, ThreadBinding, ThreadFacts, ThreadLog, ThreadState, binding::Bind, facts::refused, sim,
};
use crate::{
    durability::{
        Committed, FactLogs, OwnedState, OwnerId, OwnerToken, Settle, StateCommit, StateStore,
        StoreError, StoreFuture, StoreRecord, StoredState,
    },
    one_log::{
        STORED_MARK,
        shared::{self, Held},
        stored_key,
    },
};

impl Held for ThreadState {
    fn held(&self, key: &str) -> Option<&str> {
        self.stored(key)
    }
}

pub(crate) fn records_of<'a>(
    state: &'a ThreadState,
    state_id: &str,
) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    let prefix = format!("{state_id}\n");
    let start = prefix.len();
    let held = state
        .stored
        .range::<_, str>((Bound::Included(prefix.as_str()), Bound::Unbounded));
    held.take_while(move |(key, _)| key.starts_with(prefix.as_str()))
        .map(move |(key, value)| (&key[start..], value.as_ref()))
        .filter(|(key, _)| *key != STORED_MARK)
}

#[must_use]
pub(crate) fn stored_state(state: &ThreadState, state_id: &str) -> StoredState {
    state
        .checkpoint(state_id)
        .map(|checkpoint| StoredState {
            revision: checkpoint.revision,
            payload: Some(checkpoint.payload.clone()),
        })
        .unwrap_or_default()
}

fn put(staged: &mut Staged, state_id: &str, records: &[StoreRecord]) -> Result<(), StoreError> {
    for record in records {
        let key = stored_key(state_id, &record.key);
        let value = shared::staged(staged.state(), state_id, record);
        if staged.state().stored(&key) == Some(&*value) {
            continue;
        }
        let value = value.into_owned();
        staged.push(Record::Stored { key, value })?;
    }
    Ok(())
}

fn wholes(
    state: &ThreadState,
    state_id: &str,
    keys: &[String],
) -> Result<Vec<Option<String>>, StoreError> {
    keys.iter()
        .map(|key| {
            state
                .stored(&stored_key(state_id, key))
                .map(|value| shared::whole(state, state_id, key, value.to_owned()))
                .transpose()
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    Stage,
    Flush,
    Owe,
}

struct Replacement {
    state_id: String,
    owner: OwnerToken,
    expected_revision: u64,
    payload: String,
    records: Vec<StoreRecord>,
}

struct Inner {
    bind: Bind,
    owners: Atom<BTreeMap<String, OwnerToken>>,
    durable: Atom<bool>,
    deferred: Atom<bool>,
    owed: Atom<bool>,
}

#[derive(Clone)]
pub struct ThreadStore(Arc<Inner>);

impl ThreadStore {
    #[must_use]
    pub fn new(log: Arc<ThreadLog>) -> Self {
        Self::bound(Bind::Fixed(log))
    }

    pub(crate) fn rebinding(binding: Arc<ThreadBinding>) -> Self {
        Self::bound(Bind::Rebinds(binding))
    }

    fn bound(bind: Bind) -> Self {
        Self(Arc::new(Inner {
            bind,
            owners: Atom::new(BTreeMap::new()),
            durable: Atom::new(false),
            deferred: Atom::new(false),
            owed: Atom::new(false),
        }))
    }

    #[must_use]
    pub fn rebound(&self, log: Arc<ThreadLog>) -> Self {
        Self(Arc::new(Inner {
            bind: Bind::Fixed(log),
            owners: Atom::new(self.0.owners.read(Clone::clone)),
            durable: Atom::new(*self.0.durable.get()),
            deferred: Atom::new(*self.0.deferred.get()),
            owed: Atom::new(false),
        }))
    }

    #[must_use]
    pub fn log(&self) -> Option<Arc<ThreadLog>> {
        self.0.bind.log()
    }

    pub(super) async fn retried<T, F, Fut>(&self, command: F) -> Result<T, ScopeError>
    where
        F: Fn(Arc<ThreadLog>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ScopeError>>,
    {
        self.0
            .bind
            .retried(sim::Mutation::StoreLostUnretried, command)
            .await
    }

    pub(crate) fn owning(&self, owners: Vec<(String, OwnerToken)>) {
        self.0.owners.update(|held| {
            for (state_id, owner) in &owners {
                held.insert(state_id.clone(), owner.clone());
            }
        });
    }

    pub(super) fn owned(&self, state_id: &str) -> bool {
        self.0.owners.read(|owners| owners.contains_key(state_id))
    }

    pub fn settle_durably(&self) {
        self.0.durable.set(true);
    }

    pub fn defer_settles(&self) {
        self.0.deferred.set(true);
    }

    #[must_use]
    pub fn owed(&self) -> bool {
        *self.0.owed.get()
    }

    pub fn settled(&self) {
        self.0.owed.set(false);
    }

    async fn read<T: Send + 'static>(
        &self,
        read: impl Fn(&ThreadState) -> Result<T, StoreError> + Send + Sync + 'static,
    ) -> Result<T, StoreError> {
        let read = Arc::new(read);
        self.retried(|log| {
            let read = Arc::clone(&read);
            async move { log.job_staged(move |staged| read(staged.state())).await }
        })
        .await
        .map_err(refused)
    }

    async fn replaced(
        &self,
        replacement: Replacement,
        settle: impl Fn(&ThreadState) -> Then + Send + Sync + 'static,
    ) -> Result<u64, StoreError> {
        let replacement = Arc::new(replacement);
        let settle = Arc::new(settle);
        let (revision, then) = self
            .retried(|log| {
                let inner = Arc::clone(&self.0);
                let replacement = Arc::clone(&replacement);
                let settle = Arc::clone(&settle);
                let bind = self.0.bind.clone();
                async move {
                    let (revision, then) = log
                        .job_staged(move |staged| {
                            let revision = inner.replace(staged, &replacement)?;
                            Ok((revision, settle(staged.state())))
                        })
                        .await?;
                    if then == Then::Flush {
                        log.flush(None).await?;
                        bind.settled(&log, sim::Mutation::StoreLostUnretried)
                            .await?;
                    }
                    Ok((revision, then))
                }
            })
            .await
            .map_err(refused)?;
        if then == Then::Owe {
            self.0.owed.set(true);
        }
        Ok(revision)
    }
}

impl Inner {
    fn replace(&self, staged: &mut Staged, replacement: &Replacement) -> Result<u64, StoreError> {
        let Replacement {
            state_id,
            owner,
            expected_revision,
            payload,
            records,
        } = replacement;
        let fenced = self
            .owners
            .read(|owners| owners.get(state_id) != Some(owner));
        if fenced && !sim::mutated(sim::Mutation::StoreSkipsFence) {
            return Err(StoreError::Fenced);
        }
        let actual = staged
            .state()
            .checkpoint(state_id)
            .map_or(0, |checkpoint| checkpoint.revision);
        if actual != *expected_revision {
            return Err(StoreError::Conflict {
                expected: *expected_revision,
                actual,
            });
        }
        let revision = actual + 1;
        put(staged, state_id, records)?;
        staged.push(Record::Checkpoint {
            payload: payload.clone(),
            revision,
            state: state_id.clone(),
        })?;
        Ok(revision)
    }
}

impl StateStore for ThreadStore {
    fn facts(&self) -> Option<Arc<dyn FactLogs>> {
        self.log()
            .map(|log| Arc::new(ThreadFacts::new(log)) as Arc<dyn FactLogs>)
    }

    fn read_record<'a>(
        &'a mut self,
        state_id: &'a str,
        key: &'a str,
    ) -> StoreFuture<'a, Result<Option<String>, StoreError>> {
        let state_id = state_id.to_owned();
        let keys = vec![key.to_owned()];
        Box::pin(async move {
            let mut values = self
                .read(move |state| wholes(state, &state_id, &keys))
                .await?;
            Ok(values.pop().flatten())
        })
    }

    fn read_records<'a>(
        &'a mut self,
        state_id: &'a str,
        keys: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<Option<String>>, StoreError>> {
        let state_id = state_id.to_owned();
        let keys = keys.to_vec();
        Box::pin(self.read(move |state| wholes(state, &state_id, &keys)))
    }

    fn peek<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<StoredState, StoreError>> {
        let state_id = state_id.to_owned();
        Box::pin(self.read(move |state| Ok(stored_state(state, &state_id))))
    }

    fn list_states<'a>(
        &'a mut self,
        limit: usize,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        Box::pin(self.read(move |state| {
            Ok(state
                .checkpoints
                .iter()
                .take(limit)
                .map(|(state_id, _)| state_id.as_ref().to_owned())
                .collect())
        }))
    }

    fn list_records<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        let state_id = state_id.to_owned();
        Box::pin(self.read(move |state| {
            Ok(records_of(state, &state_id)
                .map(|(key, _)| key.to_owned())
                .collect())
        }))
    }

    fn owner<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<Option<OwnerToken>, StoreError>> {
        let owner = self.0.owners.read(|owners| owners.get(state_id).cloned());
        Box::pin(async move { Ok(owner) })
    }

    fn acquire<'a>(
        &'a mut self,
        state_id: &'a str,
        owner_id: OwnerId,
    ) -> StoreFuture<'a, Result<OwnedState, StoreError>> {
        let state_id = state_id.to_owned();
        Box::pin(async move {
            let owner = self.0.owners.update(|owners| {
                let fence = owners.get(&state_id).map_or(0, OwnerToken::fence) + 1;
                let token = OwnerToken::new(owner_id.clone(), fence);
                owners.insert(state_id.clone(), token.clone());
                token
            });
            let state = self
                .read(move |state| Ok(stored_state(state, &state_id)))
                .await?;
            Ok(OwnedState { owner, state })
        })
    }

    fn replace<'a>(
        &'a mut self,
        state_id: &'a str,
        owner: &'a OwnerToken,
        expected_revision: u64,
        payload: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<u64, StoreError>> {
        let replacement = Replacement {
            state_id: state_id.to_owned(),
            owner: owner.clone(),
            expected_revision,
            payload: payload.to_owned(),
            records: records.to_vec(),
        };
        let durable = *self.0.durable.get();
        Box::pin(self.replaced(
            replacement,
            move |_| {
                if durable { Then::Flush } else { Then::Stage }
            },
        ))
    }

    fn commit<'a>(
        &'a mut self,
        state_id: &'a str,
        owner: &'a OwnerToken,
        expected_revision: u64,
        commit: StateCommit<'a>,
    ) -> StoreFuture<'a, Result<Committed, StoreError>> {
        if !commit.events.is_empty() {
            return Box::pin(std::future::ready(Err(StoreError::NotCommitted(
                "this store does not record state events".to_owned(),
            ))));
        }
        let replacement = Replacement {
            state_id: state_id.to_owned(),
            owner: owner.clone(),
            expected_revision,
            payload: commit.payload.to_owned(),
            records: commit.records.to_vec(),
        };
        let durable = commit.settle == Settle::Durable;
        let settles = *self.0.durable.get();
        let deferred = *self.0.deferred.get();
        Box::pin(async move {
            let revision = self
                .replaced(replacement, move |state| {
                    let recovering = state.recovering();
                    match (durable && (recovering || settles), recovering) {
                        (false, _) => Then::Stage,
                        (true, false) if deferred => Then::Owe,
                        (true, _) => Then::Flush,
                    }
                })
                .await?;
            Ok(Committed {
                revision,
                first_event: None,
            })
        })
    }

    fn flush(&mut self) -> StoreFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            let Some(log) = self.log() else {
                return Ok(());
            };
            let unretried = sim::Mutation::StoreLostUnretried;
            match log.flush(None).await {
                Ok(_) => self.0.bind.settled(&log, unretried).await.map_err(refused),
                Err(ScopeError::Lost) => {
                    if let Some(fresh) = self.0.bind.again(Some(&log), unretried).await {
                        fresh.map_err(refused)?;
                    }
                    Err(StoreError::Fenced)
                }
                Err(error) => Err(refused(error)),
            }
        })
    }

    fn put_records<'a>(
        &'a mut self,
        state_id: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        let state_id: Arc<str> = Arc::from(state_id);
        let records: Arc<[StoreRecord]> = Arc::from(records);
        Box::pin(async move {
            self.retried(|log| {
                let state_id = Arc::clone(&state_id);
                let records = Arc::clone(&records);
                async move {
                    log.job_staged(move |staged| put(staged, &state_id, &records).map(|()| true))
                        .await
                }
            })
            .await
            .map_err(refused)
        })
    }
}
