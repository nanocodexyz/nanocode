use std::sync::Arc;

use nanocode_sync::{Atom, AtomMap, OnceCell};
use nanocode_tool_host::{
    ToolOutput,
    effects::{EffectCall, EffectGate},
    embedded::HostFuture,
};
use nanocode_turnlog::{EffectClass, LayoutError};

use super::{OneLogOver, Serving, refused};
use crate::{
    durability::{ChildPlace, ChildScopes, StoreError, StoreFuture},
    one_log::{
        ChildScope, OneLog,
        children::{
            CHILD_KEY, child_key, child_path, child_scope, placed, routed, spawn_setting,
            spawned_record,
        },
    },
    thread_scope::ThreadState,
};

type Cell = Arc<OnceCell<Arc<OneLogOver>>>;

#[derive(Default)]
pub(super) struct Kids {
    setting: Atom<ChildScope>,
    opened: AtomMap<String, Cell>,
    calls: AtomMap<String, Arc<OneLogOver>>,
}

impl Kids {
    fn cell(&self, state: &str) -> Cell {
        self.opened.update(|opened| {
            if let Some(cell) = opened.get(state) {
                return Arc::clone(cell);
            }
            let cell = Cell::default();
            opened.insert(state.to_owned(), Arc::clone(&cell));
            cell
        })
    }

    fn children(&self) -> Vec<Arc<OneLogOver>> {
        let mut children = self.opened.update(|opened| {
            opened
                .iter()
                .filter_map(|(state, cell)| Some((state.clone(), cell.get().cloned()?)))
                .collect::<Vec<_>>()
        });
        children.sort_by(|one, two| one.0.cmp(&two.0));
        children.into_iter().map(|(_, child)| child).collect()
    }
}

fn recorded(state: &ThreadState, child: &str) -> bool {
    state.host(&child_key(child)).is_some()
}

fn held(state: &ThreadState, child: &str) -> bool {
    state.checkpoint(child).is_some() || state.logs_facts(child)
}

fn listed(state: &ThreadState) -> Vec<String> {
    state
        .hosts()
        .filter_map(|(key, _)| key.strip_prefix(CHILD_KEY))
        .filter(|child| routed(held(state, child), true))
        .map(str::to_owned)
        .collect()
}

impl OneLogOver {
    #[must_use]
    pub fn child_scope(&self) -> ChildScope {
        *self.kids.setting.get()
    }

    pub fn set_child_scope(&self, setting: ChildScope) -> Result<(), LayoutError> {
        let setting = setting.checked()?;
        self.kids.setting.set(setting);
        for child in self.kids.children() {
            child.kids.setting.set(setting);
        }
        Ok(())
    }

    async fn resident_state(&self, serving: &Serving) -> Result<Arc<ThreadState>, StoreError> {
        match serving {
            Serving::Resident(bound) => match bound.log.state() {
                Some(state) => Ok(state),
                None => self.published().await,
            },
            Serving::Legacy(_) => self.published().await,
        }
    }

    async fn spawned(&self, state: &str) -> Result<bool, StoreError> {
        if self.kids.opened.contains_key(state) {
            return Ok(true);
        }
        let serving = self.fresh().await?;
        match &serving {
            Serving::Legacy(old) => Ok(old.log.spawned(state)),
            Serving::Resident(_) => {
                let held_state = self.resident_state(&serving).await?;
                Ok(routed(
                    held(&held_state, state),
                    recorded(&held_state, state),
                ))
            }
        }
    }

    pub async fn spawned_children(&self) -> Result<Vec<String>, StoreError> {
        let serving = self.fresh().await?;
        let mut spawned = match &serving {
            Serving::Legacy(old) => old.log.spawned_children(),
            Serving::Resident(_) => listed(&*self.resident_state(&serving).await?),
        };
        spawned.sort();
        Ok(spawned)
    }

    pub async fn child_path(&self, scope: &str) -> Result<Option<String>, StoreError> {
        let serving = self.fresh().await?;
        let path = match &serving {
            Serving::Legacy(old) => old.log.child_path(scope).await,
            Serving::Resident(_) => {
                let held = self.resident_state(&serving).await?;
                child_path(&self.thread, held.hosts(), scope)
            }
        };
        assert!(
            path.as_ref()
                .is_none_or(|path| path.starts_with(self.thread.as_str())),
            "a child path starts at its parent"
        );
        Ok(path)
    }

    async fn child(&self, state: &str) -> Result<Option<Arc<OneLogOver>>, StoreError> {
        if !self.spawned(state).await? {
            return Ok(None);
        }
        let cell = self.kids.cell(state);
        let scope = child_scope(&self.thread, state);
        let child = cell
            .get_or_try_init(|| OneLog::over(Arc::clone(&self.logs), &scope, &self.policy))
            .await?;
        child.kids.setting.set(self.child_scope());
        Ok(Some(Arc::clone(child)))
    }

    pub(super) async fn target(self: &Arc<Self>, state: &str) -> Result<Arc<Self>, StoreError> {
        Ok(self.child(state).await?.unwrap_or_else(|| Arc::clone(self)))
    }

    pub(super) fn opened_child(&self, state: &str) -> Option<Arc<Self>> {
        self.kids
            .opened
            .get(state)
            .and_then(|cell| cell.get().cloned())
    }

    pub(super) fn opened_children(&self) -> Vec<Arc<Self>> {
        self.kids.children()
    }

    async fn spawn_child(&self, state: &str) -> Result<ChildPlace, StoreError> {
        if self.kids.opened.contains_key(state) {
            return Ok(ChildPlace::Own);
        }
        let setting = spawn_setting(self.child_scope());
        let place = self
            .retried(|serving| async move {
                match serving {
                    Serving::Legacy(old) => old.log.spawn_child(state, setting).await,
                    Serving::Resident(bound) => {
                        let parent = self.thread.clone();
                        let child = state.to_owned();
                        bound
                            .log
                            .job(move |staged| {
                                let held = staged.state();
                                let found = placed(
                                    self::held(held, &child),
                                    recorded(held, &child),
                                    setting,
                                );
                                if let Some(place) = found {
                                    return Ok(place);
                                }
                                staged.push(spawned_record(&parent, &child)?)?;
                                Ok(ChildPlace::Own)
                            })
                            .await
                            .map_err(refused)
                    }
                }
            })
            .await?;
        if place == ChildPlace::Own {
            drop(self.kids.cell(state));
        }
        Ok(place)
    }
}

pub(super) struct Spawns(pub(super) Arc<OneLogOver>);

impl ChildScopes for Spawns {
    fn spawn<'a>(&'a self, state_id: &'a str) -> StoreFuture<'a, Result<ChildPlace, StoreError>> {
        Box::pin(self.0.spawn_child(state_id))
    }
}

pub(super) struct FacadeGate(pub(super) Arc<OneLogOver>);

impl EffectGate for FacadeGate {
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
                return EffectGate::before(&*self.0, call).await;
            };
            self.0
                .kids
                .calls
                .insert(call.call_id.to_owned(), Arc::clone(&child));
            EffectGate::before(&*child, call).await
        })
    }

    fn after<'a>(&'a self, call_id: &'a str, output: &'a ToolOutput) -> HostFuture<'a, ()> {
        Box::pin(async move {
            match self.0.kids.calls.remove_if(call_id, |_| true) {
                Some(child) => EffectGate::after(&*child, call_id, output).await,
                None => EffectGate::after(&*self.0, call_id, output).await,
            }
        })
    }

    fn settle(&self) -> HostFuture<'_, Result<(), String>> {
        Box::pin(async move {
            for child in self.0.opened_children() {
                EffectGate::settle(&*child).await?;
            }
            EffectGate::settle(&*self.0).await
        })
    }

    fn abandon<'a>(
        &'a self,
        call_id: &'a str,
    ) -> HostFuture<'a, Result<Option<ToolOutput>, String>> {
        Box::pin(async move {
            if let Some(abandoned) = EffectGate::abandon(&*self.0, call_id).await? {
                return Ok(Some(abandoned));
            }
            for child in self.0.opened_children() {
                if let Some(abandoned) = EffectGate::abandon(&*child, call_id).await? {
                    return Ok(Some(abandoned));
                }
            }
            Ok(None)
        })
    }
}
