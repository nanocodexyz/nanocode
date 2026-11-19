use std::sync::{Arc, Weak};

use nanocode_agent_core::backend::{AgentFactory, ChildJournal};
use nanocode_subagents::{Registry, SubagentActivity, SubagentControl};
use nanocode_tool_host::embedded::HostFuture;

use crate::{
    Agent, ChildHost, RemoteChildren, SubagentUpdates,
    configuration::DelegationTools,
    runtime::{Delegation, Ownership},
};

pub(crate) trait Delegates: Send + Sync + 'static {
    fn activity<'a>(&'a self, root: &'a str) -> HostFuture<'a, SubagentActivity>;
    fn activity_after<'a>(&'a self, root: &'a str, revision: u64)
    -> HostFuture<'a, SubagentActivity>;
    fn await_restored<'a>(&'a self, parent: &'a Agent, root: &'a str) -> HostFuture<'a, ()>;
    fn control(&self) -> &SubagentControl;
    fn deliver_completions<'a>(&'a self, parent: &'a Agent, root: &'a str) -> HostFuture<'a, ()>;
    fn persist<'a>(&'a self, root: &'a str) -> HostFuture<'a, std::io::Result<()>>;
    fn release_journal(&self, root: &str, journal: &ChildJournal);
    fn remote(&self, host: Arc<dyn ChildHost>) -> (Arc<RemoteChildren>, Arc<dyn AgentFactory>);
    fn reserve_journal(&self, root: &str, journal: ChildJournal) -> bool;
    fn shutdown(&self) -> HostFuture<'_, ()>;
    fn tools(&self) -> Arc<dyn DelegationTools>;
}

pub(crate) type Open = fn(Wiring) -> (Arc<dyn Delegates>, SubagentUpdates);

pub(crate) struct Wiring {
    pub(crate) continuation: bool,
    pub(crate) limit: usize,
    pub(crate) owners: Weak<Ownership>,
    pub(crate) services: Weak<dyn crate::configuration::HostServices>,
    pub(crate) coordination: Option<Arc<dyn nanocode_subagents::Coordination>>,
}

pub(crate) fn open(wiring: Wiring) -> (Arc<dyn Delegates>, SubagentUpdates) {
    let (registry, control, updates) = nanocode_subagents::channel(wiring.limit);
    registry.set_coordination(wiring.coordination);
    if wiring.continuation {
        let owners = wiring.owners;
        registry.set_parent_resolver(Arc::new(move |session_id: &str| {
            owners.upgrade()?.agent(session_id)
        }));
    }
    let released = wiring.services;
    registry.set_session_released(Arc::new(move |session_id: &str| {
        if let Some(services) = released.upgrade() {
            services.unregister(session_id);
        }
    }));
    (Arc::new(Live { control, registry }), updates)
}

struct Live {
    control: SubagentControl,
    registry: Arc<Registry>,
}

impl Delegates for Live {
    fn activity<'a>(&'a self, root: &'a str) -> HostFuture<'a, SubagentActivity> {
        Box::pin(self.control.activity(root))
    }

    fn activity_after<'a>(
        &'a self,
        root: &'a str,
        revision: u64,
    ) -> HostFuture<'a, SubagentActivity> {
        Box::pin(self.control.activity_after(root, revision))
    }

    fn await_restored<'a>(&'a self, parent: &'a Agent, root: &'a str) -> HostFuture<'a, ()> {
        Box::pin(self.control.await_restored(parent, root))
    }

    fn control(&self) -> &SubagentControl {
        &self.control
    }

    fn deliver_completions<'a>(&'a self, parent: &'a Agent, root: &'a str) -> HostFuture<'a, ()> {
        Box::pin(async move {
            self.control.deliver_completions(parent, root).await;
        })
    }

    fn persist<'a>(&'a self, root: &'a str) -> HostFuture<'a, std::io::Result<()>> {
        Box::pin(self.control.persist(root))
    }

    fn release_journal(&self, root: &str, journal: &ChildJournal) {
        self.registry.release_journal(root, journal);
    }

    fn remote(&self, host: Arc<dyn ChildHost>) -> (Arc<RemoteChildren>, Arc<dyn AgentFactory>) {
        let remote = RemoteChildren::new(host, &self.registry);
        let factory = remote.factory();
        (remote, factory)
    }

    fn reserve_journal(&self, root: &str, journal: ChildJournal) -> bool {
        self.registry.reserve_journal(root, journal)
    }

    fn shutdown(&self) -> HostFuture<'_, ()> {
        Box::pin(self.control.shutdown())
    }

    fn tools(&self) -> Arc<dyn DelegationTools> {
        Arc::new(Delegation(Arc::clone(&self.registry)))
    }
}
