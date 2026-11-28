use std::{future::Future, sync::Arc};

use nanocode_sync::Slot;
use nanocode_tool_host::{
    code_mode::{CellJournal, CellJournalSink, CellJournalUpdate},
    embedded::HostFuture,
};

use super::{OneLogOver, Serving, children::Spawns};
use crate::durability::{
    ChildScopes, Committed, FactLogs, OwnedState, OwnerId, OwnerToken, StateCommit, StateStore,
    StoreError, StoreFuture, StoreRecord, StoredState,
    facts::{Append, FactLog, FoldError, Replay, Snapshot},
};

pub(super) struct Routed(pub(super) Arc<OneLogOver>);

pub(super) struct RoutedSink(pub(super) Arc<OneLogOver>);

struct RoutedFacts(Arc<OneLogOver>);

struct RoutedFact {
    over: Arc<OneLogOver>,
    state: String,
    turns: bool,
}

fn handed(serving: &Serving) -> bool {
    matches!(serving, Serving::Legacy(old) if old.log.handed())
}

impl OneLogOver {
    fn served(&self, serving: &Serving) -> Box<dyn StateStore> {
        match serving {
            Serving::Resident(_) => Box::new(self.store.clone()),
            Serving::Legacy(old) => Box::new(old.log.store()),
        }
    }

    fn journaled(&self, serving: &Serving) -> Arc<dyn CellJournalSink> {
        match serving {
            Serving::Resident(_) => self.journals.sink(),
            Serving::Legacy(old) => old.log.sink(),
        }
    }

    async fn routed<T, F, Fut>(&self, call: F) -> Result<T, StoreError>
    where
        F: Fn(Box<dyn StateStore>) -> Fut,
        Fut: Future<Output = Result<T, StoreError>>,
    {
        let serving = self.fresh().await?;
        match call(self.served(&serving)).await {
            Err(_) if handed(&serving) => call(self.served(&self.fresh().await?)).await,
            answered => answered,
        }
    }

    async fn journal_of<T, F, Fut>(self: &Arc<Self>, session: &str, call: F) -> Result<T, String>
    where
        F: Fn(Arc<dyn CellJournalSink>) -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let over = self
            .target(session)
            .await
            .map_err(|error| error.to_string())?;
        over.journal(call).await
    }

    async fn journal<T, F, Fut>(&self, call: F) -> Result<T, String>
    where
        F: Fn(Arc<dyn CellJournalSink>) -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let serving = self.fresh().await.map_err(|error| error.to_string())?;
        match call(self.journaled(&serving)).await {
            Err(_) if handed(&serving) => {
                let serving = self.fresh().await.map_err(|error| error.to_string())?;
                call(self.journaled(&serving)).await
            }
            answered => answered,
        }
    }

    async fn rerouted(&self, serving: &Serving) -> Result<Serving, StoreError> {
        match serving {
            Serving::Legacy(_) => self.fresh().await,
            Serving::Resident(_) => self.again(serving).await,
        }
    }
}

impl RoutedFact {
    fn on(&self, serving: &Serving) -> Result<Arc<dyn FactLog>, StoreError> {
        let facts = self.over.served(serving).facts().ok_or_else(|| {
            StoreError::NotCommitted(format!("{} keeps no fact log", self.over.thread))
        })?;
        Ok(facts.scoped(&self.state, self.turns))
    }

    async fn aimed(&self) -> Result<Self, StoreError> {
        Ok(Self {
            over: self.over.target(&self.state).await?,
            state: self.state.clone(),
            turns: self.turns,
        })
    }

    async fn replayed(&self, cached: Option<(&str, u32)>) -> Result<Replay, StoreError> {
        self.aimed().await?.replayed_here(cached).await
    }

    async fn replayed_here(&self, cached: Option<(&str, u32)>) -> Result<Replay, StoreError> {
        let mut serving = self.over.fresh().await?;
        let mut tries = 0;
        loop {
            tries += 1;
            let log = self.on(&serving)?;
            let answer = match cached {
                Some((family, format)) => log.replay_cached(family, format).await,
                None => log.replay().await,
            };
            match answer {
                Err(error) if tries == 1 && moved(&serving, &error) => {
                    serving = self.over.rerouted(&serving).await?;
                }
                answer => return answer,
            }
        }
    }
}

fn moved(serving: &Serving, error: &StoreError) -> bool {
    match serving {
        Serving::Legacy(_) => handed(serving),
        Serving::Resident(_) => matches!(error, StoreError::Fenced),
    }
}

fn once(write: &Arc<Slot<Append>>) -> Append {
    let write = Arc::clone(write);
    Box::new(move |replay, sink| match write.take() {
        Some(write) => write(replay, sink),
        None => Err(FoldError::Sink(StoreError::NotCommitted(
            "a fact append ran twice through a routed fact log".to_owned(),
        ))),
    })
}

impl FactLog for RoutedFact {
    fn replay(&self) -> StoreFuture<'_, Result<Replay, StoreError>> {
        Box::pin(self.replayed(None))
    }

    fn replay_cached<'a>(
        &'a self,
        family: &'a str,
        format: u32,
    ) -> StoreFuture<'a, Result<Replay, StoreError>> {
        Box::pin(self.replayed(Some((family, format))))
    }

    fn append(&self, after: u64, write: Append) -> StoreFuture<'_, Result<u64, StoreError>> {
        let write = Arc::new(Slot::new(Some(write)));
        Box::pin(async move {
            let aimed = self.aimed().await?;
            let mut serving = aimed.over.fresh().await?;
            let mut tries = 0;
            loop {
                tries += 1;
                let answer = aimed.on(&serving)?.append(after, once(&write)).await;
                match answer {
                    Err(error) if tries == 1 && !write.is_empty() && moved(&serving, &error) => {
                        serving = aimed.over.rerouted(&serving).await?;
                    }
                    answer => return answer,
                }
            }
        })
    }

    fn logs_facts(&self) -> StoreFuture<'_, Result<bool, StoreError>> {
        Box::pin(async move {
            let aimed = self.aimed().await?;
            let serving = aimed.over.fresh().await?;
            aimed.on(&serving)?.logs_facts().await
        })
    }

    fn remember(&self, snapshot: Snapshot) {
        let over = self
            .over
            .opened_child(&self.state)
            .unwrap_or_else(|| Arc::clone(&self.over));
        let aimed = Self {
            over,
            state: self.state.clone(),
            turns: self.turns,
        };
        if let Ok(log) = aimed.on(&aimed.over.serving.read(Clone::clone)) {
            log.remember(snapshot);
        }
    }
}

impl FactLogs for RoutedFacts {
    fn scoped(&self, state: &str, turns: bool) -> Arc<dyn FactLog> {
        Arc::new(RoutedFact {
            over: Arc::clone(&self.0),
            state: state.to_owned(),
            turns,
        })
    }
}

impl StateStore for Routed {
    fn child_scopes(&self) -> Option<Arc<dyn ChildScopes>> {
        Some(Arc::new(Spawns(Arc::clone(&self.0))))
    }

    fn facts(&self) -> Option<Arc<dyn FactLogs>> {
        Some(Arc::new(RoutedFacts(Arc::clone(&self.0))))
    }

    fn read_record<'a>(
        &'a mut self,
        state_id: &'a str,
        key: &'a str,
    ) -> StoreFuture<'a, Result<Option<String>, StoreError>> {
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move { store.read_record(state_id, key).await })
                .await
        })
    }

    fn read_records<'a>(
        &'a mut self,
        state_id: &'a str,
        keys: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<Option<String>>, StoreError>> {
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move { store.read_records(state_id, keys).await })
                .await
        })
    }

    fn peek<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<StoredState, StoreError>> {
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move { store.peek(state_id).await })
                .await
        })
    }

    fn list_states<'a>(
        &'a mut self,
        limit: usize,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        Box::pin(async move {
            let mut states = self
                .0
                .routed(move |mut store| async move { store.list_states(limit).await })
                .await?;
            for state in self.0.spawned_children().await? {
                if states.len() >= limit {
                    break;
                }
                if !states.contains(&state) {
                    states.push(state);
                }
            }
            assert!(states.len() <= limit, "a listing keeps to its limit");
            Ok(states)
        })
    }

    fn list_records<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move { store.list_records(state_id).await })
                .await
        })
    }

    fn owner<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<Option<OwnerToken>, StoreError>> {
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move { store.owner(state_id).await })
                .await
        })
    }

    fn acquire<'a>(
        &'a mut self,
        state_id: &'a str,
        owner_id: OwnerId,
    ) -> StoreFuture<'a, Result<OwnedState, StoreError>> {
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| {
                let owner_id = owner_id.clone();
                async move { store.acquire(state_id, owner_id).await }
            })
            .await
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
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move {
                store
                    .replace(state_id, owner, expected_revision, payload, records)
                    .await
            })
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
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move {
                store
                    .commit(state_id, owner, expected_revision, commit)
                    .await
            })
            .await
        })
    }

    fn collect_unreachable<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move { store.collect_unreachable(state_id).await })
                .await
        })
    }

    fn flush(&mut self) -> StoreFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            for child in self.0.opened_children() {
                child
                    .routed(|mut store| async move { store.flush().await })
                    .await?;
            }
            self.0
                .routed(|mut store| async move { store.flush().await })
                .await
        })
    }

    fn put_records<'a>(
        &'a mut self,
        state_id: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            let over = self.0.target(state_id).await?;
            over.routed(move |mut store| async move { store.put_records(state_id, records).await })
                .await
        })
    }
}

impl CellJournalSink for RoutedSink {
    fn save(&self, update: CellJournalUpdate) -> HostFuture<'static, Result<(), String>> {
        let over = Arc::clone(&self.0);
        Box::pin(async move {
            let session = update.session_id.clone();
            over.journal_of(&session, |sink| sink.save(update.clone()))
                .await
        })
    }

    fn forget(&self, session_id: &str, cell_id: u64) -> HostFuture<'static, ()> {
        let over = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let Ok(over) = over.target(&session_id).await else {
                return;
            };
            if let Ok(serving) = over.fresh().await {
                over.journaled(&serving).forget(&session_id, cell_id).await;
            }
        })
    }

    fn load(&self, session_id: &str) -> HostFuture<'static, Result<Vec<CellJournal>, String>> {
        let over = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            over.journal_of(&session_id, |sink| sink.load(&session_id))
                .await
        })
    }

    fn load_cell(
        &self,
        session_id: &str,
        cell_id: u64,
    ) -> HostFuture<'static, Result<Option<CellJournal>, String>> {
        let over = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            over.journal_of(&session_id, |sink| sink.load_cell(&session_id, cell_id))
                .await
        })
    }

    fn save_stored(
        &self,
        session_id: &str,
        stored: String,
    ) -> HostFuture<'static, Result<(), String>> {
        let over = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            over.journal_of(&session_id, |sink| {
                sink.save_stored(&session_id, stored.clone())
            })
            .await
        })
    }

    fn load_stored(&self, session_id: &str) -> HostFuture<'static, Result<Option<String>, String>> {
        let over = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            over.journal_of(&session_id, |sink| sink.load_stored(&session_id))
                .await
        })
    }
}
