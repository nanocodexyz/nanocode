use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use nanocode_agent_core::{
    Agent, AgentEvents, BackendRequest, Result as AgentResult, SteerReceiptStatus,
    backend::{BackendFuture, ContinuationAhead, Prefetched},
};
use nanocode_durability::{
    Admission, BeginStep, Branches, CheckpointFormat, DurableAgentExt, DurableBackendRegistration,
    DurableOwner, DurableSession, EncodedPayload, InitialAcquisition, InitialSettings,
    JournalClaim, JournalSeed, OwnerWrite, PayloadReader, SessionRecord, SharedStore, StepState,
    StepStatus, agent_error,
};
use nanocode_trace::Instrument as _;
use serde_json::{Value, value::RawValue};

use crate::{
    Claude, ClaudeBuilder, PreparedWire,
    execution::{
        Admission as ClaudeAdmission, BeginRequest, ClaudeContinuation, ClaudeExecutionPolicy,
        ClaudeSteer, EncodedState, JournalWrite, PolicyFuture, Settlement, Step,
    },
};

mod facts;
mod fold;
mod format;
mod prefetch;
pub use facts::ClaudeFacts;
pub(crate) use fold::tool_output;
pub use fold::{ClaudeDecision, ClaudeFold};
pub(crate) use format::{Prefix, keep_prefix, prefix};
#[doc(hidden)]
pub use prefetch::{Prefetch, Prefetches};

pub use format::{ClaudeCheckpointFormat, forget_templates};

const MAINTENANCE_KINDS: &[&str] = &["compact", "session_end"];

impl DurableBackendRegistration for ClaudeBuilder {
    fn checkpoint_format(&self) -> Option<Arc<dyn CheckpointFormat>> {
        Some(Arc::new(ClaudeCheckpointFormat))
    }

    fn build_durable(
        &self,
        request: BackendRequest,
        state: DurableSession,
    ) -> BackendFuture<AgentResult<(Agent, AgentEvents)>> {
        let prepared = self.clone().prepare(request);
        Box::pin(async move { prepared?.durability(state).await?.build() })
    }
}

impl DurableBackendRegistration for Claude {
    fn checkpoint_format(&self) -> Option<Arc<dyn CheckpointFormat>> {
        Some(Arc::new(ClaudeCheckpointFormat))
    }

    fn build_durable(
        &self,
        request: BackendRequest,
        state: DurableSession,
    ) -> BackendFuture<AgentResult<(Agent, AgentEvents)>> {
        Agent::builder(self.clone()).build_durable(request, state)
    }
}

impl DurableAgentExt for ClaudeBuilder {
    async fn durability(self, state: DurableSession) -> AgentResult<Self> {
        let state_id = state.state_id().to_owned();
        let journal = state.child_journal();
        let (owner, checkpoint) = async {
            match self.durable_initial_settings() {
                Ok(initial) => Ok(state.acquire_agent_with_initial(initial).await),
                Err(error) => {
                    if state
                        .record()
                        .await
                        .map_err(agent_error)?
                        .is_some_and(|record| record.initial.is_none())
                    {
                        return Err(error);
                    }
                    Ok(state.acquire_agent().await)
                }
            }
        }
        .instrument(nanocode_trace::info_span!(target: "nanocode", "claude.open.acquire"))
        .await?
        .map_err(agent_error)?;
        owner
            .abandon_stale_kinds(MAINTENANCE_KINDS)
            .await
            .map_err(agent_error)?;
        let (preloaded, preloaded_records) = state
            .take_checkpoint_preload_hydrated(checkpoint.as_ref().map_or("", EncodedPayload::key))
            .map(|(preloaded, records, hydrated)| {
                let deferred = preloaded
                    .hydrated()
                    .and_then(nanocode_durability::Hydrated::get::<format::DeferredSnapshot>)
                    .cloned();
                let typed =
                    hydrated.and_then(nanocode_durability::Hydrated::take::<format::TypedSnapshot>);
                let preloaded = match (deferred, typed) {
                    (Some(deferred), _) => Ok(Preload::Deferred(deferred)),
                    (None, Some(typed)) => Ok(Preload::Typed(typed.head, typed.conversation)),
                    (None, None) => preloaded.into_value().map(Preload::Whole),
                };
                (preloaded, records)
            })
            .unzip();
        let preloaded = preloaded.transpose().map_err(agent_error)?;
        let record = state.record().await.map_err(agent_error)?;
        let branches = Branches {
            store: state.shared_store(),
            record,
        };

        let builder = match &branches.record {
            Some(record) => self.lineage(record.lineage.clone()),
            None => self,
        };
        let builder = if checkpoint.is_none()
            && let Some(record) = &branches.record
            && let Some(initial) = record.initial
        {
            record
                .model
                .capabilities(nanocode_agent_core::ModelTransport::Native)
                .check_service_tier(initial.service_tier)?;
            builder
                .thinking(initial.thinking)?
                .fast_mode(initial.service_tier != nanocode_agent_core::ServiceTier::Standard)
        } else {
            builder
        };
        let mut read = preloaded_records
            .flatten()
            .map(Arc::unwrap_or_clone)
            .unwrap_or_default();
        let facts = match state.facts() {
            Some(log) => Some((
                log.logs_facts()
                    .await
                    .map_err(nanocode_durability::agent_store_error)?,
                ClaudeFacts::open(log)?,
            )),
            None => None,
        };
        let defers = !crate::sim::mutated(crate::sim::Mutation::AgentReopenReadsHistory)
            && builder.configured_checkpoint()?.is_none()
            && facts.as_ref().is_none_or(|(logged, _)| *logged);
        let mut deferred = None;
        let (checkpoint, pages) = match (checkpoint, preloaded) {
            (Some(_), Some(Preload::Typed(head, pages))) => (Some(head), Some(pages)),
            (Some(_), Some(Preload::Deferred(preloaded))) if defers => {
                let (head, stored, messages) = preloaded.into_parts();
                deferred = Some((stored, messages));
                (Some(head), None)
            }
            (Some(_), Some(Preload::Deferred(preloaded))) => {
                let (_, stored, _) = preloaded.into_parts();
                let (head, pages) =
                    format::hydrate_paged(PayloadReader::from(&owner), stored, &mut read)
                        .await
                        .map_err(agent_error)?;
                (Some(head), pages)
            }
            (Some(_), Some(Preload::Whole(preloaded))) => (
                Some(
                    hydrate(PayloadReader::from(&owner), preloaded, &mut read)
                        .instrument(nanocode_trace::info_span!(
                            target: "nanocode",
                            "claude.open.hydrate",
                            preloaded = true
                        ))
                        .await?,
                ),
                None,
            ),
            (Some(value), None) => {
                let span = nanocode_trace::info_span!(
                    target: "nanocode",
                    "claude.open.hydrate",
                    preloaded = false
                );
                let decoded: Value = span.in_scope(|| value.decode()).map_err(agent_error)?;
                let reader = PayloadReader::from(&owner);
                match format::deferrable(&decoded).map_err(agent_error)? {
                    Some(messages) if defers => {
                        let head = format::hydrate_head(reader, &decoded, &mut read)
                            .instrument(span)
                            .await
                            .map_err(agent_error)?;
                        deferred = Some((decoded, messages));
                        (Some(head), None)
                    }
                    _ => {
                        let (head, pages) = format::hydrate_paged(reader, decoded, &mut read)
                            .instrument(span)
                            .await
                            .map_err(agent_error)?;
                        (Some(head), pages)
                    }
                }
            }
            (None, _) => {
                let imported = builder.configured_checkpoint()?;
                if let Some(checkpoint) = &imported
                    && state
                        .state()
                        .await
                        .map_err(agent_error)?
                        .pending_operations()
                        .is_empty()
                {
                    owner
                        .commit_checkpoint(EncodedPayload::encode(checkpoint).map_err(agent_error)?)
                        .await
                        .map_err(agent_error)?;
                }
                (imported, None)
            }
        };
        let facts = match facts {
            Some((logged, facts)) => {
                if !logged && let Some(head) = &checkpoint {
                    let whole = match &pages {
                        Some(pages) => pages.whole(head).map_err(agent_error)?,
                        None => head.clone(),
                    };
                    import_base(&facts, &state_id, &whole).await?;
                }
                Some(facts)
            }
            None => None,
        };
        let _decode =
            nanocode_trace::info_span!(target: "nanocode", "claude.open.decode").entered();
        let pages = pages.map(format::HydratedConversation::into_pages);
        let (stored, messages) = deferred.unzip();
        let policy = Arc::new(ClaudeExecution {
            state_id,
            owner,
            session: state,
            branches,
            hydrated: nanocode_sync::Slot::default(),
            prefetching: Prefetches::default(),
            stored: nanocode_sync::Slot::new(Some(read).filter(|read| !read.is_empty())),
            facts,
            deferred: stored,
        });
        let builder = builder.child_journal(journal);
        match (checkpoint, messages) {
            (Some(head), Some(messages)) => {
                builder.deferred_execution_policy(policy, head, messages)
            }
            (checkpoint, _) => builder.paged_execution_policy(policy, checkpoint, pages),
        }
    }
}

enum Preload {
    Typed(Value, format::HydratedConversation),
    Whole(Value),
    Deferred(format::DeferredSnapshot),
}

async fn import_base(facts: &ClaudeFacts, state_id: &str, whole: &Value) -> AgentResult<()> {
    if facts.seq().await? != 0 {
        return Ok(());
    }
    let base = nanocode_durability::facts::Fact::Base(nanocode_durability::facts::Base {
        state: state_id.to_owned(),
        revision: 0,
        checkpoint: serde_json::value::to_raw_value(whole)
            .map_err(|error| agent_error(nanocode_durability::Error::InvalidPayload(error)))?,
    });
    facts.commit(base).await.map(drop)
}

async fn import_payload(
    facts: &ClaudeFacts,
    owner: &DurableOwner,
    state_id: &str,
    payload: &EncodedPayload,
) -> AgentResult<()> {
    if facts.seq().await? != 0 {
        return Ok(());
    }
    let stored = payload.decode().map_err(agent_error)?;
    let whole = hydrate(PayloadReader::from(owner), stored, &mut HashSet::new()).await?;
    import_base(facts, state_id, &whole).await
}

impl ClaudeBuilder {
    pub(crate) fn deferred_durability(
        self,
        state: &DurableSession,
        record: SessionRecord,
        notice: String,
    ) -> AgentResult<(Self, Arc<AtomicBool>, JournalSeed)> {
        let initial = self.durable_initial_settings()?;
        record
            .model
            .capabilities(nanocode_agent_core::ModelTransport::Native)
            .check_service_tier(initial.service_tier)?;
        let existing = Arc::new(AtomicBool::new(false));
        let (journal, seed) = state.seeded_child_journal();
        let builder = self
            .lineage(record.lineage.clone())
            .thinking(initial.thinking)?
            .fast_mode(initial.service_tier != nanocode_agent_core::ServiceTier::Standard)
            .initial_developer_notices(vec![(nanocode_platform::uuid_v7().to_string(), notice)])
            .child_journal(journal)
            .execution_policy(
                Arc::new(LazyClaudeExecution::deferred(
                    state.shared_store(),
                    record,
                    initial,
                    Arc::clone(&existing),
                    seed.claim(),
                )),
                None,
            )?;
        Ok((builder, existing, seed))
    }
}

struct ClaudeExecution {
    state_id: String,
    owner: DurableOwner,
    session: DurableSession,
    branches: Branches,
    hydrated: nanocode_sync::Slot<(String, ClaudeContinuation, bool)>,
    prefetching: Prefetches,
    stored: nanocode_sync::Slot<HashSet<String>>,
    facts: Option<ClaudeFacts>,
    deferred: Option<Value>,
}

struct Deferred {
    initial: InitialSettings,
    existing: Arc<AtomicBool>,
    journal: JournalClaim,
    attempted: AtomicBool,
}

struct Acquired {
    execution: ClaudeExecution,
    reopened: bool,
}

struct LazyClaudeExecution {
    store: SharedStore,
    record: SessionRecord,
    ready: nanocode_sync::OnceCell<Acquired>,

    initialized: std::sync::atomic::AtomicBool,

    model: nanocode_sync::Atom<Option<nanocode_agent_core::ModelSelection>>,

    deferred: Option<Deferred>,

    /// A continuation read ahead before the owner was acquired: the
    /// acquired execution starts with it, under its key.
    ahead: nanocode_sync::Slot<(String, Value)>,
}

impl LazyClaudeExecution {
    fn new(store: SharedStore, record: SessionRecord) -> Self {
        Self {
            store,
            record,
            ready: nanocode_sync::OnceCell::new(),
            initialized: std::sync::atomic::AtomicBool::new(false),
            model: nanocode_sync::Atom::default(),
            deferred: None,
            ahead: nanocode_sync::Slot::default(),
        }
    }

    fn deferred(
        store: SharedStore,
        record: SessionRecord,
        initial: InitialSettings,
        existing: Arc<AtomicBool>,
        journal: JournalClaim,
    ) -> Self {
        Self {
            deferred: Some(Deferred {
                initial,
                existing,
                journal,
                attempted: AtomicBool::new(false),
            }),
            ..Self::new(store, record)
        }
    }

    fn acquired(&self) -> Option<&ClaudeExecution> {
        self.ready.get().map(|acquired| &acquired.execution)
    }

    async fn get(&self) -> AgentResult<&ClaudeExecution> {
        self.acquire(None)
            .await
            .map(|(acquired, _)| &acquired.execution)
    }

    async fn acquire(&self, initial: Option<EncodedPayload>) -> AgentResult<(&Acquired, bool)> {
        let committed = AtomicBool::new(false);
        let policy = self
            .ready
            .get_or_try_init(|| async {
                let mut record = self.record.clone();
                if let Some(model) = Option::clone(&self.model.get()) {
                    record.model = model;
                }
                let state = DurableSession::prepare_shared_described(
                    self.store.clone(),
                    record.session_id.clone(),
                    None,
                    Some(record),
                )
                .await
                .map_err(agent_error)?;
                let seeded = initial.clone();
                let (owner, checkpoint) = match (
                    initial,
                    self.deferred.as_ref().map(|deferred| deferred.initial),
                ) {
                    (Some(payload), settings) => {
                        match match self.deferred.as_ref().map(|deferred| &deferred.journal) {
                            Some(journal) => {
                                state
                                    .acquire_agent_with_initial_checkpoint_and_journal(
                                        settings, payload, journal,
                                    )
                                    .await
                            }
                            None => {
                                state
                                    .acquire_agent_with_initial_checkpoint(settings, payload)
                                    .await
                            }
                        }
                        .map_err(agent_error)?
                        {
                            (owner, InitialAcquisition::Committed) => {
                                committed.store(true, Ordering::SeqCst);
                                (owner, None)
                            }
                            (owner, InitialAcquisition::Existing(checkpoint)) => {
                                (owner, checkpoint)
                            }
                        }
                    }
                    (None, Some(settings)) => state
                        .acquire_agent_with_initial(settings)
                        .await
                        .map_err(agent_error)?,
                    (None, None) => state.acquire_agent().await.map_err(agent_error)?,
                };
                owner
                    .abandon_stale_kinds(MAINTENANCE_KINDS)
                    .await
                    .map_err(agent_error)?;
                let record = state.record().await.map_err(agent_error)?.ok_or_else(|| {
                    agent_error(nanocode_durability::Error::InvalidState(
                        "described durable session has no record".to_owned(),
                    ))
                })?;
                let reopened = checkpoint.is_some()
                    || !state
                        .state()
                        .await
                        .map_err(agent_error)?
                        .pending_operations()
                        .is_empty();
                let based = checkpoint
                    .clone()
                    .or_else(|| seeded.filter(|_| committed.load(Ordering::SeqCst)));
                let facts = match state.facts() {
                    Some(log) => {
                        let logged = log
                            .logs_facts()
                            .await
                            .map_err(nanocode_durability::agent_store_error)?;
                        let facts = ClaudeFacts::open(log)?;
                        if let Some(payload) = based.as_ref().filter(|_| !logged) {
                            import_payload(&facts, &owner, &record.session_id, payload).await?;
                        }
                        Some(facts)
                    }
                    None => None,
                };
                let hydrated = nanocode_sync::Slot::default();
                if let Some((key, cursor)) = self.ahead.take() {
                    hydrated.put((key, ClaudeContinuation::whole(cursor), true));
                }
                Ok::<_, nanocode_agent_core::NanocodeError>(Acquired {
                    execution: ClaudeExecution {
                        state_id: record.session_id.clone(),
                        owner,
                        session: state,
                        branches: Branches {
                            store: self.store.clone(),
                            record: Some(record),
                        },
                        hydrated,
                        prefetching: Prefetches::default(),
                        stored: nanocode_sync::Slot::default(),
                        facts,
                        deferred: None,
                    },
                    reopened,
                })
            })
            .await?;
        Ok((policy, committed.load(Ordering::SeqCst)))
    }
}

fn branch_child(
    branches: &Branches,
    child: &nanocode_agent_core::SessionInfo,
) -> AgentResult<Option<Arc<dyn ClaudeExecutionPolicy>>> {
    if branches.record.is_none() {
        return Ok(None);
    }
    Ok(Some(Arc::new(LazyClaudeExecution::new(
        branches.store.clone(),
        branches.child_record(child)?,
    ))))
}

impl ClaudeExecutionPolicy for LazyClaudeExecution {
    fn facts(&self) -> Option<&ClaudeFacts> {
        self.acquired()?.facts.as_ref()
    }

    fn state_id(&self) -> &str {
        &self.record.session_id
    }

    fn admit(
        &self,
        id: String,
        input: Value,
        automatic: bool,
    ) -> PolicyFuture<'_, (String, ClaudeAdmission)> {
        Box::pin(async move { self.get().await?.admit(id, input, automatic).await })
    }

    fn begin_attempt(&self, id: String) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.begin_attempt(id).await })
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn identified_steer_accepted(
        &self,
        message_id: String,
        input_json: String,
    ) -> PolicyFuture<'_, bool> {
        Box::pin(async move {
            self.get()
                .await?
                .identified_steer_accepted(message_id, input_json)
                .await
        })
    }

    fn unfinished_operation(&self, id: String) -> PolicyFuture<'_, bool> {
        Box::pin(async move { self.get().await?.unfinished_operation(id).await })
    }

    fn steer_receipt(
        &self,
        message_id: String,
        input_json: String,
    ) -> PolicyFuture<'_, SteerReceiptStatus> {
        Box::pin(async move {
            self.get()
                .await?
                .steer_receipt(message_id, input_json)
                .await
        })
    }

    fn accept_steer(
        &self,
        id: String,
        message_id: Option<String>,
        after: u32,
        identity_json: String,
        input_json: String,
    ) -> PolicyFuture<'_, Option<u32>> {
        Box::pin(async move {
            self.get()
                .await?
                .accept_steer(id, message_id, after, identity_json, input_json)
                .await
        })
    }

    fn carry_steers(
        &self,
        id: String,
        steers: Vec<(String, String, String)>,
    ) -> PolicyFuture<'_, Vec<Option<(u32, u32)>>> {
        Box::pin(async move { self.get().await?.carry_steers(id, steers).await })
    }

    fn retained_steers(&self, id: String) -> PolicyFuture<'_, Vec<ClaudeSteer>> {
        Box::pin(async move { self.get().await?.retained_steers(id).await })
    }

    fn withdraw_steer(&self, id: String, index: u32) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.withdraw_steer(id, index).await })
    }

    fn bind_steer(&self, id: String, index: u32, boundary: u32) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.bind_steer(id, index, boundary).await })
    }

    fn bind_steers(
        &self,
        id: String,
        steers: std::ops::RangeInclusive<u32>,
        boundary: u32,
    ) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.bind_steers(id, steers, boundary).await })
    }

    fn continuation(&self, id: String) -> PolicyFuture<'_, Option<Value>> {
        Box::pin(async move { self.get().await?.continuation(id).await })
    }

    fn continuation_model(&self, id: String) -> PolicyFuture<'_, Option<String>> {
        Box::pin(async move { self.get().await?.continuation_model(id).await })
    }

    fn continuation_paged(&self, id: String) -> PolicyFuture<'_, Option<ClaudeContinuation>> {
        Box::pin(async move { self.get().await?.continuation_paged(id).await })
    }

    fn prefetch_continuation(&self, id: String) -> PolicyFuture<'_, bool> {
        Box::pin(async move {
            match self.acquired() {
                Some(execution) => execution.prefetch_continuation(id).await,
                None => Ok(false),
            }
        })
    }

    fn prefetch_continuation_ahead(
        &self,
        id: String,
        ahead: BackendFuture<Option<ContinuationAhead>>,
    ) -> PolicyFuture<'_, Prefetched> {
        Box::pin(async move {
            if let Some(execution) = self.acquired() {
                return execution.prefetch_continuation_ahead(id, ahead).await;
            }
            let ahead = ahead.await;
            if let Some(execution) = self.acquired() {
                return execution
                    .prefetch_continuation_ahead(id, Box::pin(async move { ahead }))
                    .await;
            }
            // No owner yet: the execution it acquires starts with the
            // continuation read ahead, which its run's read keeps only while
            // the stored continuation has that key.
            let Some(ahead) = ahead else {
                return Ok(Prefetched::Nothing);
            };
            self.ahead.put(ahead.into_parts());
            Ok(Prefetched::Seeded)
        })
    }

    fn advance(&self, id: String, state: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.advance(id, state).await })
    }

    fn advance_encoded(&self, id: String, state: Box<RawValue>) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.advance_encoded(id, state).await })
    }

    fn begin_step(
        &self,
        id: String,
        step_id: String,
        kind: String,
        input: Value,
    ) -> PolicyFuture<'_, Step> {
        Box::pin(async move { self.get().await?.begin_step(id, step_id, kind, input).await })
    }

    fn commit_writes<'a>(
        &'a self,
        id: String,
        writes: Vec<JournalWrite>,
        begin: Option<BeginRequest<'a>>,
    ) -> PolicyFuture<'a, Option<Step>> {
        Box::pin(async move { self.get().await?.commit_writes(id, writes, begin).await })
    }

    fn defer_writes(&self, id: String, writes: Vec<JournalWrite>) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.defer_writes(id, writes).await })
    }

    fn complete_step(&self, id: String, step_id: String, output: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.complete_step(id, step_id, output).await })
    }

    fn complete(&self, id: String, checkpoint: Value, output: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.complete(id, checkpoint, output).await })
    }

    fn fail(&self, id: String, checkpoint: Value, error: String) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.fail(id, checkpoint, error).await })
    }

    fn fail_attempt(&self, id: String, checkpoint: Value, error: String) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.fail_attempt(id, checkpoint, error).await })
    }

    fn cancel(&self, id: String, checkpoint: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.cancel(id, checkpoint).await })
    }

    fn pages_messages(&self) -> bool {
        true
    }

    fn take_stored_records(&self) -> Option<HashSet<String>> {
        self.ready
            .get()
            .and_then(|acquired| acquired.execution.stored.take())
    }

    fn load_sequence(
        &self,
        sequence: nanocode_durability::Sequence,
    ) -> PolicyFuture<'_, Vec<EncodedPayload>> {
        Box::pin(async move { self.get().await?.load_sequence(sequence).await })
    }

    fn load_records(&self, records: Vec<EncodedPayload>) -> PolicyFuture<'_, Vec<EncodedPayload>> {
        Box::pin(async move { self.get().await?.load_records(records).await })
    }

    fn holds_back_formats(&self) -> bool {
        self.store.holds_back_formats()
    }

    fn pool(&self, payload: EncodedPayload) -> PolicyFuture<'_, bool> {
        Box::pin(async move { self.get().await?.pool(payload).await })
    }

    fn settle_encoded(
        &self,
        id: String,
        checkpoint: EncodedState,
        settlement: Settlement,
    ) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.get()
                .await?
                .settle_encoded(id, checkpoint, settlement)
                .await
        })
    }

    fn release(&self, id: String) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            match self.acquired() {
                Some(policy) => policy.release(id).await,
                None => Ok(()),
            }
        })
    }

    fn shutdown(&self) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            match self.acquired() {
                Some(policy) => policy.shutdown().await,
                None => Ok(()),
            }
        })
    }

    fn checkpoint(&self, state: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.get().await?.checkpoint(state).await })
    }

    fn developer_notices(&self, id: String) -> PolicyFuture<'_, Vec<(String, String)>> {
        Box::pin(async move { self.get().await?.developer_notices(id).await })
    }

    fn initial_checkpoint(
        &self,
        state: Value,
        model: nanocode_agent_core::ModelSelection,
    ) -> PolicyFuture<'_, ()> {
        match serde_json::value::to_raw_value(&state) {
            Ok(state) => self.initial_checkpoint_encoded(state, model),
            Err(error) => Box::pin(async move {
                Err(agent_error(nanocode_durability::Error::InvalidPayload(
                    error,
                )))
            }),
        }
    }

    fn initial_checkpoint_encoded(
        &self,
        state: Box<RawValue>,
        model: nanocode_agent_core::ModelSelection,
    ) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            if self.initialized.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            if self
                .deferred
                .as_ref()
                .is_some_and(|deferred| deferred.attempted.swap(true, Ordering::SeqCst))
            {
                return Err(nanocode_agent_core::NanocodeError::InvalidExecutionPolicy(
                    "the deferred durable acquisition already failed".to_owned(),
                ));
            }
            self.model.set(Some(model));

            let prepared = self
                .ready
                .get()
                .is_none()
                .then(|| EncodedPayload::from_json(Arc::from(state.get())));
            let (acquired, committed) = self.acquire(prepared).await?;
            if let Some(deferred) = self.deferred.as_ref().filter(|_| !committed) {
                deferred.existing.store(true, Ordering::SeqCst);
                return Err(nanocode_agent_core::NanocodeError::InvalidExecutionPolicy(
                    "the prepared durable session already holds state".to_owned(),
                ));
            }
            if acquired.reopened {
                return Ok(());
            }
            if !committed {
                acquired
                    .execution
                    .owner
                    .commit_checkpoint(EncodedPayload::from_json(Arc::from(state.get())))
                    .await
                    .map_err(agent_error)?;
            }
            self.initialized
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    }

    fn discard_initial(&self) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            let Some(policy) = self.acquired() else {
                return Ok(());
            };
            if !self.initialized.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            policy.owner.discard_unused().await.map_err(agent_error)
        })
    }

    fn branch(
        &self,
        child: &nanocode_agent_core::SessionInfo,
    ) -> AgentResult<Option<Arc<dyn ClaudeExecutionPolicy>>> {
        branch_child(
            &Branches {
                store: self.store.clone(),
                record: Some(self.record.clone()),
            },
            child,
        )
    }
}

impl ClaudeExecution {
    async fn read_continuation(
        &self,
        value: EncodedPayload,
        read: &mut HashSet<String>,
    ) -> nanocode_agent_core::Result<ClaudeContinuation> {
        let (head, pages) = format::hydrate_cursor_paged(
            PayloadReader::from(&self.owner),
            value.decode().map_err(agent_error)?,
            read,
        )
        .await
        .map_err(agent_error)?;
        Ok(ClaudeContinuation {
            head,
            pages: pages.map(format::HydratedConversation::into_pages),
        })
    }
}

impl ClaudeExecutionPolicy for ClaudeExecution {
    fn state_id(&self) -> &str {
        &self.state_id
    }

    fn facts(&self) -> Option<&ClaudeFacts> {
        self.facts.as_ref()
    }

    fn branch(
        &self,
        child: &nanocode_agent_core::SessionInfo,
    ) -> AgentResult<Option<Arc<dyn ClaudeExecutionPolicy>>> {
        branch_child(&self.branches, child)
    }

    fn admit(
        &self,
        id: String,
        input: Value,
        automatic: bool,
    ) -> PolicyFuture<'_, (String, ClaudeAdmission)> {
        Box::pin(async move {
            let (id, admission) = if automatic {
                self.owner
                    .admit_automatic_typed::<_, Value, Value>(id, &input)
                    .await
                    .map_err(agent_error)?
                    .into_parts()
            } else {
                let admission = self
                    .owner
                    .admit_typed::<_, Value, Value>(id.clone(), &input)
                    .await
                    .map_err(agent_error)?;
                (id, admission)
            };
            let reader = PayloadReader::from(&self.owner);
            let admission = match admission {
                Admission::Completed { checkpoint, output } => ClaudeAdmission::Completed {
                    // None: the durable head with nothing in flight, which the
                    // live conversation already holds; Null tells the agent to
                    // reuse it rather than hydrate a second full copy.
                    checkpoint: match checkpoint {
                        Some(checkpoint) => {
                            hydrate(reader, checkpoint, &mut HashSet::new()).await?
                        }
                        None => Value::Null,
                    },
                    output: format::hydrate_output(reader, output)
                        .await
                        .map_err(agent_error)?,
                },
                // Nothing reads a failed request's checkpoint: replay only
                // reports its error.
                Admission::Failed { error, .. } => ClaudeAdmission::Failed {
                    checkpoint: Value::Null,
                    error,
                },
                admission => map_admission(admission),
            };
            Ok((id, admission))
        })
    }

    fn begin_attempt(&self, id: String) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.owner.begin_attempt(id).await.map_err(agent_error) })
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn identified_steer_accepted(
        &self,
        message_id: String,
        input_json: String,
    ) -> PolicyFuture<'_, bool> {
        Box::pin(async move {
            let input: Box<serde_json::value::RawValue> = serde_json::from_str(&input_json)
                .map_err(|error| {
                    nanocode_agent_core::NanocodeError::InvalidRequest(error.to_string())
                })?;
            self.owner
                .identified_steer_accepted(message_id, &input)
                .await
                .map_err(agent_error)
        })
    }

    fn unfinished_operation(&self, id: String) -> PolicyFuture<'_, bool> {
        Box::pin(async move {
            self.owner
                .unfinished_operation(id)
                .await
                .map_err(agent_error)
        })
    }

    fn steer_receipt(
        &self,
        message_id: String,
        input_json: String,
    ) -> PolicyFuture<'_, SteerReceiptStatus> {
        Box::pin(async move {
            let input: Box<RawValue> = serde_json::from_str(&input_json).map_err(|error| {
                nanocode_agent_core::NanocodeError::InvalidRequest(error.to_string())
            })?;
            self.owner
                .steer_receipt(message_id, &input)
                .await
                .map_err(agent_error)
        })
    }

    fn accept_steer(
        &self,
        id: String,
        message_id: Option<String>,
        after: u32,
        identity_json: String,
        input_json: String,
    ) -> PolicyFuture<'_, Option<u32>> {
        Box::pin(async move {
            let raw = |json: &str| {
                serde_json::from_str::<Box<RawValue>>(json).map_err(|error| {
                    nanocode_agent_core::NanocodeError::InvalidRequest(error.to_string())
                })
            };
            let input = raw(&input_json)?;
            match message_id {
                Some(message_id) => {
                    self.owner
                        .accept_identified_steer(
                            id,
                            message_id,
                            after,
                            &raw(&identity_json)?,
                            &input,
                        )
                        .await
                }
                None => self.owner.accept_steer(id, after, &input, None).await,
            }
            .map_err(agent_error)
        })
    }
    fn carry_steers(
        &self,
        id: String,
        steers: Vec<(String, String, String)>,
    ) -> PolicyFuture<'_, Vec<Option<(u32, u32)>>> {
        Box::pin(async move {
            let raw = |json: &str| {
                serde_json::from_str::<Box<RawValue>>(json).map_err(|error| {
                    nanocode_agent_core::NanocodeError::InvalidRequest(error.to_string())
                })
            };
            let steers = steers
                .iter()
                .map(|(message_id, identity_json, input_json)| {
                    Ok((message_id.clone(), raw(identity_json)?, raw(input_json)?))
                })
                .collect::<nanocode_agent_core::Result<Vec<_>>>()?;
            self.owner
                .carry_steers(id, &steers)
                .await
                .map_err(agent_error)
        })
    }
    fn retained_steers(&self, id: String) -> PolicyFuture<'_, Vec<ClaudeSteer>> {
        Box::pin(async move {
            self.owner
                .retained_steers(id)
                .await
                .and_then(|steers| {
                    steers
                        .into_iter()
                        .map(|steer| {
                            Ok(ClaudeSteer {
                                message_id: steer.state.message_id,
                                index: steer.index,
                                accepted_after_model_call_index: steer
                                    .state
                                    .accepted_after_model_call_index,
                                model_call_index: steer.state.model_call_index,
                                input_json: steer.state.input.json()?.to_owned(),
                            })
                        })
                        .collect()
                })
                .map_err(agent_error)
        })
    }
    fn withdraw_steer(&self, id: String, index: u32) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .withdraw_steer(id, index)
                .await
                .map_err(agent_error)
        })
    }
    fn bind_steer(&self, id: String, index: u32, boundary: u32) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .bind_steer(id, index, boundary)
                .await
                .map_err(agent_error)
        })
    }
    fn bind_steers(
        &self,
        id: String,
        steers: std::ops::RangeInclusive<u32>,
        boundary: u32,
    ) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .bind_steers(id, steers, boundary)
                .await
                .map_err(agent_error)
        })
    }
    fn continuation(&self, id: String) -> PolicyFuture<'_, Option<Value>> {
        Box::pin(async move {
            let Some(continuation) = self.continuation_paged(id).await? else {
                return Ok(None);
            };
            match &continuation.pages {
                Some(pages) => format::assemble_cursor(continuation.head, pages)
                    .map(Some)
                    .map_err(agent_error),
                None => Ok(Some(continuation.head)),
            }
        })
    }

    fn continuation_paged(&self, id: String) -> PolicyFuture<'_, Option<ClaudeContinuation>> {
        Box::pin(async move {
            let span = nanocode_trace::info_span!(
                target: "nanocode",
                "agent.continuation",
                hydrated = nanocode_trace::field::Empty,
            );
            self.prefetching.settled(&id).instrument(span.clone()).await;
            let Some(value) = self
                .owner
                .continuation(id)
                .instrument(span.clone())
                .await
                .map_err(agent_error)?
            else {
                self.hydrated.take();
                return Ok(None);
            };
            if let Some((key, hydrated, ahead)) = self.hydrated.take()
                && key == value.key()
            {
                span.record("hydrated", if ahead { "ahead" } else { "kept" });
                return Ok(Some(hydrated));
            }
            span.record("hydrated", "read");
            let mut read = HashSet::new();
            let hydrated = self
                .read_continuation(value, &mut read)
                .instrument(span)
                .await?;
            self.stored.put(read);
            Ok(Some(hydrated))
        })
    }

    fn continuation_model(&self, id: String) -> PolicyFuture<'_, Option<String>> {
        Box::pin(async move {
            let span = nanocode_trace::info_span!(
                target: "nanocode",
                "agent.continuation",
                hydrated = nanocode_trace::field::Empty,
                model_only = true,
            );
            self.prefetching.settled(&id).instrument(span.clone()).await;
            let Some(value) = self
                .owner
                .continuation(id)
                .instrument(span.clone())
                .await
                .map_err(agent_error)?
            else {
                self.hydrated.take();
                return Ok(None);
            };
            let (key, hydrated) = match self.hydrated.take() {
                Some((key, hydrated, ahead)) if key == value.key() => {
                    span.record("hydrated", if ahead { "ahead" } else { "kept" });
                    (key, hydrated)
                }
                _ => {
                    span.record("hydrated", "read");
                    let mut read = HashSet::new();
                    let key = value.key().to_owned();
                    let hydrated = self
                        .read_continuation(value, &mut read)
                        .instrument(span)
                        .await?;
                    self.stored.put(read);
                    (key, hydrated)
                }
            };
            let model = hydrated.model().map(str::to_owned);
            self.hydrated.put((key, hydrated, false));
            Ok(model)
        })
    }

    fn prefetch_continuation(&self, id: String) -> PolicyFuture<'_, bool> {
        Box::pin(async move {
            let _prefetch = self.prefetching.claim(&id);
            let Some(value) = self
                .owner
                .peek_continuation(id)
                .await
                .map_err(agent_error)?
            else {
                return Ok(false);
            };
            if let Some((key, hydrated, ahead)) = self.hydrated.take()
                && key == value.key()
            {
                self.hydrated.put((key, hydrated, ahead));
                return Ok(true);
            }
            let mut read = HashSet::new();
            let key = value.key().to_owned();
            let hydrated = self.read_continuation(value, &mut read).await?;
            self.stored.put(read);
            self.hydrated.put((key, hydrated, true));
            Ok(true)
        })
    }

    fn prefetch_continuation_ahead(
        &self,
        id: String,
        ahead: BackendFuture<Option<ContinuationAhead>>,
    ) -> PolicyFuture<'_, Prefetched> {
        Box::pin(async move {
            let _prefetch = self.prefetching.claim(&id);
            let ahead = ahead.await;
            let Some(value) = self
                .owner
                .peek_continuation(id.clone())
                .await
                .map_err(agent_error)?
            else {
                return Ok(Prefetched::Nothing);
            };
            if let Some((key, hydrated, kept)) = self.hydrated.take()
                && key == value.key()
            {
                self.hydrated.put((key, hydrated, kept));
                return Ok(Prefetched::Kept);
            }
            // Read ahead from the store: kept only under the key of the
            // continuation the owner holds now, so a stale one is read again.
            if let Some(ahead) = ahead
                && ahead.operation_id() == id
                && ahead.key() == value.key()
            {
                let (key, cursor) = ahead.into_parts();
                self.hydrated
                    .put((key, ClaudeContinuation::whole(cursor), true));
                return Ok(Prefetched::Kept);
            }
            let mut read = HashSet::new();
            let key = value.key().to_owned();
            let hydrated = self.read_continuation(value, &mut read).await?;
            self.stored.put(read);
            self.hydrated.put((key, hydrated, true));
            Ok(Prefetched::Read)
        })
    }

    fn advance_encoded(&self, id: String, state: Box<RawValue>) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            let state = EncodedPayload::from_json(Arc::from(state.get()));
            self.owner.advance(id, state).await.map_err(agent_error)
        })
    }

    fn begin_step_encoded(
        &self,
        id: String,
        step_id: String,
        kind: String,
        input: Box<RawValue>,
    ) -> PolicyFuture<'_, Step> {
        Box::pin(async move {
            let begun = self
                .owner
                .begin_step(id, step_id, kind.clone(), &*input)
                .await;
            self.admitted(begun, &kind).await
        })
    }

    fn commit_writes<'a>(
        &'a self,
        id: String,
        writes: Vec<JournalWrite>,
        begin: Option<BeginRequest<'a>>,
    ) -> PolicyFuture<'a, Option<Step>> {
        Box::pin(async move {
            let fused = writes.iter().map(owner_write).collect::<Vec<_>>();
            let Some(begin) = begin else {
                if !fused.is_empty() {
                    self.owner
                        .commit_writes(id, fused)
                        .await
                        .map_err(agent_error)?;
                }
                return Ok(None);
            };
            let committed = self
                .owner
                .admit_step(
                    id.clone(),
                    fused,
                    begin.step_id.clone(),
                    begin.kind.clone(),
                    EncodedPayload::from_json(Arc::from(begin.input.get())),
                    begin.recovery,
                )
                .await;
            let begun = match committed {
                Err(error @ nanocode_durability::Error::StepChanged { .. })
                    if let Some(wire) = begin.wire =>
                {
                    if !writes.is_empty() {
                        self.owner
                            .commit_writes(id.clone(), writes.iter().map(owner_write).collect())
                            .await
                            .map_err(agent_error)?;
                    }
                    let state = self.session.state().await.map_err(agent_error)?;
                    let Some(step) = state
                        .operation(&id)
                        .and_then(|operation| operation.steps.get(&begin.step_id))
                        .filter(|step| step.kind == begin.kind)
                    else {
                        return Err(agent_error(error));
                    };
                    let Some(earlier) = self.earlier_input(wire, &begin.step_id, step).await?
                    else {
                        return Err(agent_error(error));
                    };
                    self.owner
                        .begin_step(id, begin.step_id, begin.kind.clone(), &*earlier)
                        .await
                }
                begun => begun,
            };
            self.admitted(begun, &begin.kind).await.map(Some)
        })
    }

    fn defer_writes(&self, id: String, writes: Vec<JournalWrite>) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            for write in &writes {
                match owner_write(write) {
                    OwnerWrite::Advance { continuation } => {
                        self.owner.advance_deferred(id.clone(), continuation).await
                    }
                    OwnerWrite::CompleteStep { step_id, output } => {
                        self.owner
                            .complete_step_deferred_encoded(id.clone(), step_id, output)
                            .await
                    }
                    OwnerWrite::BeginStep { .. } => Err(nanocode_durability::Error::InvalidState(
                        "a deferred journal write cannot begin a step".to_owned(),
                    )),
                }
                .map_err(agent_error)?;
            }
            Ok(())
        })
    }

    fn advance(&self, id: String, state: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .advance(id, payload(&state)?)
                .await
                .map_err(agent_error)
        })
    }

    fn begin_step(
        &self,
        id: String,
        step_id: String,
        kind: String,
        input: Value,
    ) -> PolicyFuture<'_, Step> {
        Box::pin(async move {
            let begun = self
                .owner
                .begin_step(id, step_id, kind.clone(), &input)
                .await;
            self.admitted(begun, &kind).await
        })
    }

    fn complete_step(&self, id: String, step_id: String, output: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .complete_step(id, step_id, &output)
                .await
                .map_err(agent_error)
        })
    }

    fn complete(&self, id: String, checkpoint: Value, output: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .complete(id, payload(&checkpoint)?, &output)
                .await
                .map_err(agent_error)
        })
    }

    fn fail(&self, id: String, checkpoint: Value, error: String) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .fail(id, payload(&checkpoint)?, error)
                .await
                .map_err(agent_error)
        })
    }

    fn fail_attempt(&self, id: String, _checkpoint: Value, error: String) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .fail_attempt(id, error)
                .await
                .map_err(agent_error)
        })
    }

    fn cancel(&self, id: String, checkpoint: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .cancel(id, Some(payload(&checkpoint)?))
                .await
                .map_err(agent_error)
        })
    }

    fn pages_messages(&self) -> bool {
        true
    }

    fn take_stored_records(&self) -> Option<HashSet<String>> {
        self.stored.take()
    }

    fn restored_conversation(&self) -> PolicyFuture<'_, Option<ClaudeContinuation>> {
        Box::pin(async move {
            let Some(stored) = self.deferred.clone() else {
                return Ok(None);
            };
            let mut read = self.stored.take().unwrap_or_default();
            let hydrated =
                format::hydrate_paged(PayloadReader::from(&self.owner), stored, &mut read).await;
            self.stored.put(read);
            let (head, pages) = hydrated.map_err(agent_error)?;
            Ok(Some(ClaudeContinuation {
                head,
                pages: pages.map(format::HydratedConversation::into_pages),
            }))
        })
    }

    fn load_sequence(
        &self,
        sequence: nanocode_durability::Sequence,
    ) -> PolicyFuture<'_, Vec<EncodedPayload>> {
        Box::pin(async move {
            nanocode_durability::load_sequence_payloads(
                PayloadReader::from(&self.owner),
                sequence,
                &mut HashSet::new(),
            )
            .await
            .map_err(agent_error)
        })
    }

    fn load_records(&self, records: Vec<EncodedPayload>) -> PolicyFuture<'_, Vec<EncodedPayload>> {
        Box::pin(async move {
            PayloadReader::from(&self.owner)
                .take_payloads(records)
                .await
                .map_err(agent_error)
        })
    }

    fn holds_back_formats(&self) -> bool {
        self.session.holds_back_formats()
    }

    fn pool(&self, payload: EncodedPayload) -> PolicyFuture<'_, bool> {
        Box::pin(async move {
            self.session.pool(payload).await.map_err(agent_error)?;
            Ok(true)
        })
    }

    fn settle_encoded(
        &self,
        id: String,
        checkpoint: EncodedState,
        settlement: Settlement,
    ) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            let checkpoint = EncodedPayload::from_json(Arc::from(checkpoint.json.get()))
                .with_payloads(checkpoint.records);
            match settlement {
                Settlement::Complete(output) => self.owner.complete(id, checkpoint, &*output).await,
                Settlement::Fail(error) => self.owner.fail(id, checkpoint, error).await,
                Settlement::FailAttempt(error) => self.owner.fail_attempt(id, error).await,
                Settlement::Cancel => self.owner.cancel(id, Some(checkpoint)).await,
            }
            .map_err(agent_error)
        })
    }

    fn cancel_unstarted(&self, id: String) -> PolicyFuture<'_, bool> {
        Box::pin(async move {
            match self.owner.cancel(id, None).await {
                Ok(()) => Ok(true),
                Err(nanocode_durability::Error::CancellationCheckpointRequired { .. }) => Ok(false),
                Err(error) => Err(agent_error(error)),
            }
        })
    }

    fn release(&self, id: String) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.owner.release_claim(id).await.map_err(agent_error) })
    }

    fn shutdown(&self) -> PolicyFuture<'_, ()> {
        Box::pin(async move { self.owner.shutdown().await.map_err(agent_error) })
    }

    fn developer_notices(&self, id: String) -> PolicyFuture<'_, Vec<(String, String)>> {
        Box::pin(async move {
            let state = self.session.state().await.map_err(agent_error)?;
            let operation = state.operation(&id).ok_or_else(|| {
                nanocode_agent_core::NanocodeError::InvalidCheckpoint(
                    "Claude notice operation is missing".into(),
                )
            })?;
            let mut notices = Vec::new();
            for (step_id, step) in &operation.steps {
                if step.kind != "developer_notice" {
                    continue;
                }
                let notice = self
                    .owner
                    .load_payload(step.input.clone())
                    .await
                    .map_err(agent_error)?
                    .decode()
                    .map_err(agent_error)?;
                if matches!(step.status, nanocode_durability::StepStatus::EffectPending) {
                    self.owner
                        .complete_step(id.clone(), step_id.clone(), &Value::Null)
                        .await
                        .map_err(agent_error)?;
                }
                notices.push(notice);
            }
            Ok(notices)
        })
    }

    fn checkpoint(&self, state: Value) -> PolicyFuture<'_, ()> {
        Box::pin(async move {
            self.owner
                .commit_checkpoint(payload(&state)?)
                .await
                .map_err(agent_error)
        })
    }
}

async fn hydrate(
    reader: PayloadReader<'_>,
    checkpoint: Value,
    read: &mut HashSet<String>,
) -> AgentResult<Value> {
    format::hydrate_snapshot(reader, checkpoint, read)
        .await
        .map_err(agent_error)
}

fn owner_write(write: &JournalWrite) -> OwnerWrite {
    match write {
        JournalWrite::Advance(state) => OwnerWrite::Advance {
            continuation: EncodedPayload::from_json(Arc::from(state.json.get()))
                .with_payloads(state.records.clone()),
        },
        JournalWrite::Complete {
            step_id,
            output,
            records,
        } => OwnerWrite::CompleteStep {
            step_id: step_id.clone(),
            output: EncodedPayload::from_json(Arc::from(output.get()))
                .with_payloads(records.clone()),
        },
    }
}

impl ClaudeExecution {
    async fn earlier_input(
        &self,
        wire: &PreparedWire,
        step_id: &str,
        step: &StepState,
    ) -> AgentResult<Option<Box<RawValue>>> {
        for earlier in [
            PreparedWire::stream_durable_input,
            PreparedWire::body_durable_input,
            PreparedWire::blake3_durable_input,
            PreparedWire::sha256_durable_input,
            PreparedWire::legacy_durable_input,
        ] {
            let earlier = earlier(wire).map_err(crate::agent::provider_error)?;
            if EncodedPayload::encode(&*earlier)
                .map_err(agent_error)?
                .matches_key(step.input.key())
            {
                return Ok(Some(earlier));
            }
        }
        let recorded = self
            .owner
            .load_payload(step.input.clone())
            .await
            .map_err(agent_error)?;
        let recorded = recorded.json().map_err(agent_error)?;
        if matches!(step.status, StepStatus::Completed(_)) {
            if !wire.same_conversation(recorded) {
                return Ok(None);
            }
            nanocode_trace::warn!(
                target: "nanocode_claude",
                step_id,
                "replaying a completed model step whose tools or system prompt changed since it was recorded"
            );
            nanocode_trace::event!(
                target: "nc_metrics",
                nanocode_trace::Level::INFO,
                { monotonic_counter.nc.claude.steps_replayed_across_prefix = 1_u64 }
            );
        } else {
            nanocode_trace::warn!(
                target: "nanocode_claude",
                step_id,
                "sending again a model step that recorded no result although its request changed since it was recorded"
            );
            nanocode_trace::event!(
                target: "nc_metrics",
                nanocode_trace::Level::INFO,
                { monotonic_counter.nc.claude.steps_rerun_across_request = 1_u64 }
            );
        }
        RawValue::from_string(recorded.to_owned())
            .map(Some)
            .map_err(|error| agent_error(nanocode_durability::Error::InvalidPayload(error)))
    }

    async fn admitted(
        &self,
        begun: nanocode_durability::Result<BeginStep>,
        kind: &str,
    ) -> AgentResult<Step> {
        match begun.map_err(agent_error)? {
            BeginStep::Execute => Ok(Step::Execute),
            BeginStep::Interrupted if kind == "tool" => Ok(Step::Interrupted),
            BeginStep::Interrupted => Ok(Step::Execute),
            BeginStep::Replay(value) if kind == "model" || kind == "model_call" => {
                format::hydrate_receipt(
                    PayloadReader::from(&self.owner),
                    value.decode().map_err(agent_error)?,
                )
                .await
                .map(Step::Replay)
                .map_err(agent_error)
            }
            BeginStep::Replay(value) => Ok(Step::Replay(value.decode().map_err(agent_error)?)),
        }
    }
}

fn payload(value: &Value) -> AgentResult<EncodedPayload> {
    EncodedPayload::encode(value).map_err(agent_error)
}

fn map_admission(admission: Admission<Value, Value>) -> ClaudeAdmission {
    match admission {
        Admission::Accepted => ClaudeAdmission::Execute,
        Admission::Pending => ClaudeAdmission::Resume,
        Admission::Completed { checkpoint, output } => ClaudeAdmission::Completed {
            checkpoint: checkpoint.unwrap_or(Value::Null),
            output,
        },
        Admission::Failed { checkpoint, error } => ClaudeAdmission::Failed { checkpoint, error },
        Admission::Cancelled => ClaudeAdmission::Cancelled,
    }
}
