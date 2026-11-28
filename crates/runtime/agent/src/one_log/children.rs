use std::sync::Arc;

use nanocode_scope::switches::{CHILD_SCOPE, OnDisk};
use nanocode_sync::{Atom, AtomMap, OnceCell};
use nanocode_tool_host::{
    ToolOutput,
    code_mode::{CellJournal, CellJournalSink, CellJournalUpdate},
    effects::{EffectCall, EffectGate},
    embedded::HostFuture,
};
use nanocode_turnlog::{EffectClass, Formats, Layout, LayoutError, LogHost, Point, Record};

use super::{OneLog, OneLogJournals, OneLogStore, ParentStore, Policy, facts, legacy};
use crate::{
    durability::{
        ChildPlace, ChildScopes, Committed, FactLogs, OwnedState, OwnerId, OwnerToken, StateCommit,
        StateStore, StoreError, StoreFuture, StoreRecord, StoredState,
        facts::{Append, FactLog, Replay, Snapshot},
    },
    thread_scope::sim,
};

pub(crate) const CHILD_KEY: &str = "$nanocode_child\n";
const CHILD_SEPARATOR: &str = "/child/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildScope {
    Own,
    Parent,
}

impl Default for ChildScope {
    fn default() -> Self {
        Self::writable()
    }
}

impl ChildScope {
    pub const SWITCH: OnDisk = CHILD_SCOPE;
    pub const ALL: [Self; 2] = [Self::Own, Self::Parent];

    #[must_use]
    pub const fn setting(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Parent => "parent",
        }
    }

    #[must_use]
    pub fn named(setting: &str) -> Option<Self> {
        let scope = Self::ALL
            .into_iter()
            .find(|scope| scope.setting() == setting);
        assert!(
            scope.is_none_or(|scope| Self::SWITCH.settings().contains(&scope.setting())),
            "every child scope is a registered setting of its switch"
        );
        scope
    }

    #[must_use]
    pub fn writable() -> Self {
        if Formats::writes(Layout::ChildScopes) {
            return Self::Own;
        }
        Self::Parent
    }

    pub fn checked(self) -> Result<Self, LayoutError> {
        if self == Self::Own {
            let layout = Formats::layout(Layout::ChildScopes)?;
            assert_eq!(layout, Layout::ChildScopes);
        }
        assert!(
            Self::SWITCH.settings().contains(&self.setting()),
            "a checked child scope is a registered setting"
        );
        Ok(self)
    }

    #[must_use]
    pub const fn flipped(self) -> Self {
        match self {
            Self::Own => Self::Parent,
            Self::Parent => Self::Own,
        }
    }
}

#[must_use]
pub fn child_scope(parent: &str, state: &str) -> String {
    assert!(!parent.is_empty(), "a child scope names its parent");
    assert!(!state.is_empty(), "a child scope names its state");
    format!("{parent}{CHILD_SEPARATOR}{state}")
}

pub(crate) fn child_key(state: &str) -> String {
    format!("{CHILD_KEY}{state}")
}

pub(crate) fn spawned_ids<'a>(
    hosts: impl Iterator<Item = (&'a str, &'a serde_json::value::RawValue)>,
) -> Vec<String> {
    hosts
        .filter_map(|(key, _)| key.strip_prefix(CHILD_KEY))
        .map(str::to_owned)
        .collect()
}

pub(crate) fn spawned_record(parent: &str, state: &str) -> Result<Record, StoreError> {
    let value = serde_json::value::to_raw_value(&child_scope(parent, state)).map_err(|error| {
        StoreError::NotCommitted(format!("encode a spawned child's scope: {error}"))
    })?;
    Ok(Record::Host {
        key: child_key(state),
        value,
    })
}

pub(crate) fn placed(held: bool, recorded: bool, setting: ChildScope) -> Option<ChildPlace> {
    if held && !sim::mutated(sim::Mutation::MixedChildRouted) {
        return Some(ChildPlace::Parent);
    }
    if recorded {
        return Some(ChildPlace::Own);
    }
    if held || setting == ChildScope::Parent {
        return Some(ChildPlace::Parent);
    }
    None
}

pub(crate) fn routed(held: bool, recorded: bool) -> bool {
    placed(held, recorded, ChildScope::Parent) == Some(ChildPlace::Own)
}

pub(crate) async fn record(log: &OneLog, state: &str) -> Result<(), StoreError> {
    let parent = log.scope().to_owned();
    let state = state.to_owned();
    log.appends(move |writing| {
        Box::pin(async move {
            writing.push(spawned_record(&parent, &state)?)?;
            writing.commit(Point::Progress).await?;
            Ok::<_, StoreError>(())
        })
    })
    .await
}

pub(crate) fn spawn_setting(setting: ChildScope) -> ChildScope {
    assert!(
        setting == ChildScope::Parent || Formats::writes(Layout::ChildScopes),
        "a child gets its own scope only once the previous release reads child scopes"
    );
    if sim::mutated(sim::Mutation::ChildOnParentLog) {
        return ChildScope::Parent;
    }
    setting
}

type Cell = Arc<OnceCell<Arc<OneLog>>>;

struct Opener {
    host: Arc<dyn LogHost>,
    policy: Policy,
}

pub(crate) struct Children {
    opener: Option<Opener>,
    setting: Atom<ChildScope>,
    spawned: AtomMap<String, Cell>,
    calls: AtomMap<String, Arc<OneLog>>,
}

impl Children {
    pub(crate) fn new(
        host: Option<Arc<dyn LogHost>>,
        policy: &Policy,
        spawned: Vec<String>,
    ) -> Self {
        let children = Self {
            opener: host.map(|host| Opener {
                host,
                policy: policy.clone(),
            }),
            setting: Atom::new(ChildScope::default()),
            spawned: AtomMap::new(),
            calls: AtomMap::new(),
        };
        for state in spawned {
            children.spawned.insert(state, Cell::default());
        }
        children
    }

    fn cell(&self, state: &str) -> Cell {
        self.spawned.update(|spawned| {
            if let Some(cell) = spawned.get(state) {
                return Arc::clone(cell);
            }
            let cell = Cell::default();
            spawned.insert(state.to_owned(), Arc::clone(&cell));
            cell
        })
    }
}

impl OneLog {
    #[must_use]
    pub fn child_scope(&self) -> ChildScope {
        *self.children.setting.get()
    }

    pub fn set_child_scope(&self, setting: ChildScope) -> Result<(), LayoutError> {
        self.children.setting.set(setting.checked()?);
        Ok(())
    }

    #[must_use]
    pub fn spawned(&self, state: &str) -> bool {
        self.children.spawned.contains_key(state)
    }

    #[must_use]
    pub fn spawned_children(&self) -> Vec<String> {
        let mut spawned = self
            .children
            .spawned
            .update(|map| map.keys().cloned().collect::<Vec<_>>());
        spawned.sort();
        spawned
    }

    pub(crate) async fn spawn_child(
        &self,
        state: &str,
        setting: ChildScope,
    ) -> Result<ChildPlace, StoreError> {
        if self.spawned(state) {
            return Ok(ChildPlace::Own);
        }
        let parent = self.scope().to_owned();
        let state_id = state.to_owned();
        let place = self
            .appends(move |writing| {
                Box::pin(async move {
                    let fold = writing.fold();
                    let found = placed(
                        fold.knows_state(&state_id),
                        fold.host(&child_key(&state_id)).is_some(),
                        setting,
                    );
                    if let Some(place) = found {
                        return Ok(place);
                    }
                    writing.push(spawned_record(&parent, &state_id)?)?;
                    if sim::mutated(sim::Mutation::SpawnBatchSplit) {
                        writing.commit(Point::Progress).await?;
                    }
                    Ok::<_, StoreError>(ChildPlace::Own)
                })
            })
            .await?;
        if place == ChildPlace::Own {
            drop(self.children.cell(state));
        }
        assert!(
            place == ChildPlace::Parent || self.spawned(state),
            "a child placed in its own scope is routed there"
        );
        Ok(place)
    }

    async fn child(&self, state: &str) -> Result<Option<Arc<OneLog>>, StoreError> {
        let Some(opener) = &self.children.opener else {
            return Ok(None);
        };
        let Some(cell) = self.children.spawned.get(state) else {
            return Ok(None);
        };
        let scope = child_scope(self.scope(), state);
        let child = cell
            .get_or_try_init(|| legacy::child(Arc::clone(&opener.host), scope, &opener.policy))
            .await?;
        child.children.setting.set(self.child_scope());
        Ok(Some(Arc::clone(child)))
    }

    async fn target(self: &Arc<Self>, state: &str) -> Result<Arc<OneLog>, StoreError> {
        Ok(self.child(state).await?.unwrap_or_else(|| Arc::clone(self)))
    }

    fn opened_children(&self) -> Vec<Arc<OneLog>> {
        let mut opened = self.children.spawned.update(|spawned| {
            spawned
                .iter()
                .filter_map(|(state, cell)| Some((state.clone(), cell.get().cloned()?)))
                .collect::<Vec<_>>()
        });
        opened.sort_by(|one, two| one.0.cmp(&two.0));
        opened.into_iter().map(|(_, child)| child).collect()
    }

    pub(crate) fn routes(&self) -> bool {
        self.children.opener.is_some()
    }
}

struct Spawns(Arc<OneLog>);

impl ChildScopes for Spawns {
    fn spawn<'a>(&'a self, state_id: &'a str) -> StoreFuture<'a, Result<ChildPlace, StoreError>> {
        Box::pin(async move {
            let setting = if self.0.routes() || sim::mutated(sim::Mutation::UnroutedSpawnsOwn) {
                spawn_setting(self.0.child_scope())
            } else {
                ChildScope::Parent
            };
            self.0.spawn_child(state_id, setting).await
        })
    }
}

impl OneLogStore {
    async fn target(&self, state_id: &str) -> Result<ParentStore, StoreError> {
        Ok(ParentStore(self.0.target(state_id).await?))
    }
}

impl StateStore for OneLogStore {
    fn child_scopes(&self) -> Option<Arc<dyn ChildScopes>> {
        Some(Arc::new(Spawns(Arc::clone(&self.0))))
    }

    fn facts(&self) -> Option<Arc<dyn FactLogs>> {
        Some(Arc::new(ChildLogs(Arc::clone(&self.0))))
    }

    fn read_record<'a>(
        &'a mut self,
        state_id: &'a str,
        key: &'a str,
    ) -> StoreFuture<'a, Result<Option<String>, StoreError>> {
        Box::pin(async move {
            let mut store = self.target(state_id).await?;
            store.read_record(state_id, key).await
        })
    }

    fn read_records<'a>(
        &'a mut self,
        state_id: &'a str,
        keys: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<Option<String>>, StoreError>> {
        Box::pin(async move {
            let mut store = self.target(state_id).await?;
            store.read_records(state_id, keys).await
        })
    }

    fn peek<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<StoredState, StoreError>> {
        Box::pin(async move {
            let mut store = self.target(state_id).await?;
            store.peek(state_id).await
        })
    }

    fn list_states<'a>(
        &'a mut self,
        limit: usize,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        Box::pin(async move {
            let mut states = ParentStore(Arc::clone(&self.0)).list_states(limit).await?;
            if self.0.routes() {
                for state in self.0.spawned_children() {
                    if states.len() >= limit {
                        break;
                    }
                    if !states.contains(&state) {
                        states.push(state);
                    }
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
            let mut store = self.target(state_id).await?;
            store.list_records(state_id).await
        })
    }

    fn owner<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<Option<OwnerToken>, StoreError>> {
        Box::pin(async move {
            let mut store = self.target(state_id).await?;
            store.owner(state_id).await
        })
    }

    fn acquire<'a>(
        &'a mut self,
        state_id: &'a str,
        owner_id: OwnerId,
    ) -> StoreFuture<'a, Result<OwnedState, StoreError>> {
        Box::pin(async move {
            let mut store = self.target(state_id).await?;
            store.acquire(state_id, owner_id).await
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
            let mut store = self.target(state_id).await?;
            store
                .replace(state_id, owner, expected_revision, payload, records)
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
            let mut store = self.target(state_id).await?;
            store
                .commit(state_id, owner, expected_revision, commit)
                .await
        })
    }

    fn collect_unreachable<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut store = self.target(state_id).await?;
            store.collect_unreachable(state_id).await
        })
    }

    fn flush(&mut self) -> StoreFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            for child in self.0.opened_children() {
                ParentStore(child).flush().await?;
            }
            ParentStore(Arc::clone(&self.0)).flush().await
        })
    }

    fn put_records<'a>(
        &'a mut self,
        state_id: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        Box::pin(async move {
            let mut store = self.target(state_id).await?;
            store.put_records(state_id, records).await
        })
    }
}

struct ChildLogs(Arc<OneLog>);

struct ChildFact {
    log: Arc<OneLog>,
    state: String,
    turns: bool,
}

impl ChildFact {
    async fn on(&self) -> Result<Arc<dyn FactLog>, StoreError> {
        let target = self.log.target(&self.state).await?;
        Ok(facts::Logs(target).scoped(&self.state, self.turns))
    }
}

impl FactLog for ChildFact {
    fn replay(&self) -> StoreFuture<'_, Result<Replay, StoreError>> {
        Box::pin(async move { self.on().await?.replay().await })
    }

    fn replay_cached<'a>(
        &'a self,
        family: &'a str,
        format: u32,
    ) -> StoreFuture<'a, Result<Replay, StoreError>> {
        Box::pin(async move { self.on().await?.replay_cached(family, format).await })
    }

    fn replay_head<'a>(
        &'a self,
        family: &'a str,
        format: u32,
    ) -> StoreFuture<'a, Result<(Replay, u64), StoreError>> {
        Box::pin(async move { self.on().await?.replay_head(family, format).await })
    }

    fn append(&self, after: u64, write: Append) -> StoreFuture<'_, Result<u64, StoreError>> {
        Box::pin(async move { self.on().await?.append(after, write).await })
    }

    fn remember(&self, snapshot: Snapshot) {
        let opened = self
            .log
            .children
            .spawned
            .get(&self.state)
            .and_then(|cell| cell.get().cloned());
        if let Some(child) = opened {
            facts::Logs(child)
                .scoped(&self.state, self.turns)
                .remember(snapshot);
        }
    }
}

impl FactLogs for ChildLogs {
    fn scoped(&self, state: &str, turns: bool) -> Arc<dyn FactLog> {
        if !(self.0.routes() && self.0.spawned(state)) {
            return facts::Logs(Arc::clone(&self.0)).scoped(state, turns);
        }
        Arc::new(ChildFact {
            log: Arc::clone(&self.0),
            state: state.to_owned(),
            turns,
        })
    }
}

pub(crate) struct ChildSink(pub(crate) Arc<OneLog>);

impl ChildSink {
    async fn on(log: &Arc<OneLog>, session: &str) -> Result<OneLogJournals, String> {
        let target = log
            .target(session)
            .await
            .map_err(|error| error.to_string())?;
        Ok(OneLogJournals(target))
    }
}

impl CellJournalSink for ChildSink {
    fn save(&self, update: CellJournalUpdate) -> HostFuture<'static, Result<(), String>> {
        let log = Arc::clone(&self.0);
        Box::pin(async move { Self::on(&log, &update.session_id).await?.save(update).await })
    }

    fn forget(&self, session_id: &str, cell_id: u64) -> HostFuture<'static, ()> {
        let log = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            if let Ok(sink) = Self::on(&log, &session_id).await {
                sink.forget(&session_id, cell_id).await;
            }
        })
    }

    fn load(&self, session_id: &str) -> HostFuture<'static, Result<Vec<CellJournal>, String>> {
        let log = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move { Self::on(&log, &session_id).await?.load(&session_id).await })
    }

    fn load_cell(
        &self,
        session_id: &str,
        cell_id: u64,
    ) -> HostFuture<'static, Result<Option<CellJournal>, String>> {
        let log = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            Self::on(&log, &session_id)
                .await?
                .load_cell(&session_id, cell_id)
                .await
        })
    }

    fn save_stored(
        &self,
        session_id: &str,
        stored: String,
    ) -> HostFuture<'static, Result<(), String>> {
        let log = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            Self::on(&log, &session_id)
                .await?
                .save_stored(&session_id, stored)
                .await
        })
    }

    fn load_stored(&self, session_id: &str) -> HostFuture<'static, Result<Option<String>, String>> {
        let log = Arc::clone(&self.0);
        let session_id = session_id.to_owned();
        Box::pin(async move {
            Self::on(&log, &session_id)
                .await?
                .load_stored(&session_id)
                .await
        })
    }
}

pub(crate) struct ChildGate(pub(crate) Arc<OneLog>);

impl EffectGate for ChildGate {
    fn effect(&self, tool: &str) -> EffectClass {
        self.0.effect_of(tool)
    }

    fn before<'a>(&'a self, call: EffectCall<'a>) -> HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let child = self
                .0
                .child(call.session_id)
                .await
                .map_err(|error| error.to_string())?;
            let Some(child) = child else {
                return self.0.before(call).await;
            };
            self.0
                .children
                .calls
                .insert(call.call_id.to_owned(), Arc::clone(&child));
            child.before(call).await
        })
    }

    fn after<'a>(&'a self, call_id: &'a str, output: &'a ToolOutput) -> HostFuture<'a, ()> {
        Box::pin(async move {
            match self.0.children.calls.remove_if(call_id, |_| true) {
                Some(child) => child.after(call_id, output).await,
                None => self.0.after(call_id, output).await,
            }
        })
    }

    fn settle(&self) -> HostFuture<'_, Result<(), String>> {
        Box::pin(async move {
            for child in self.0.opened_children() {
                child.settle().await?;
            }
            self.0.settle().await
        })
    }

    fn abandon<'a>(
        &'a self,
        call_id: &'a str,
    ) -> HostFuture<'a, Result<Option<ToolOutput>, String>> {
        Box::pin(async move {
            if let Some(abandoned) = self.0.abandon(call_id).await? {
                return Ok(Some(abandoned));
            }
            for child in self.0.opened_children() {
                if let Some(abandoned) = child.abandon(call_id).await? {
                    return Ok(Some(abandoned));
                }
            }
            Ok(None)
        })
    }
}
