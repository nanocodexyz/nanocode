use std::{
    future::Future,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use nanocode_scope::ScopeError;
use nanocode_sync::{Atom, SingleFlight};
use nanocode_tool_host::{
    ToolOutput,
    code_mode::CellJournalSink,
    effects::{Carried, EffectCall, EffectGate, RemoteBarrier},
    embedded::HostFuture,
    hold::{HoldCall, HoldLedger, LedgerError, Running},
};
use nanocode_turnlog::{
    Answer, ArmedWake, EffectClass, Effects, Fold, Point, Resumption, SCAN_END, Wake, WakeRider,
    adopt::{self, Reading},
};
use serde_json::{Map, Value, value::RawValue};

use super::{Exported, Fire, Heard, Imported, OneLog, OneLogStore, Policy, WakeError, legacy};
use crate::{
    durability::{StateStore, StoreError, StoreRecord},
    session_log::{LogEntry, LoggedEvent, Started, Submission, Submitted, TurnRecord, TurnState},
    thread_scope::{
        HoldsError, ThreadBinding, ThreadCommand, ThreadHolds, ThreadJournals, ThreadLog,
        ThreadLogs, ThreadPath, ThreadState, ThreadStore, ThreadTurns, ThreadWakes, legacy_refused,
        sim, wakes,
    },
};

mod routed;

type Answered = Option<(TurnRecord, Option<LoggedEvent>)>;

pub enum ServedStore {
    Resident(ThreadStore),
    Legacy(OneLogStore),
}

pub struct OneLogOver {
    logs: Arc<ThreadLogs>,
    thread: String,
    policy: Policy,
    binding: Arc<ThreadBinding>,
    serving: Atom<Serving>,
    switching: SingleFlight<Result<Serving, StoreError>>,
    store: ThreadStore,
    journals: ThreadJournals,
    effects: Arc<Atom<Effects>>,
    index: Atom<Option<Arc<dyn WakeRider>>>,
    durable: AtomicBool,
    deferred: AtomicBool,
    this: Weak<Self>,
}

struct Bound {
    log: Arc<ThreadLog>,
    turns: ThreadTurns,
    holds: ThreadHolds,
    wakes: ThreadWakes,
}

struct Old {
    log: Arc<OneLog>,
    resident: Option<Arc<Bound>>,
    uncertain: Vec<String>,
    adopting: bool,
}

impl Old {
    fn adopting(&self, adopting: bool) -> Self {
        Self {
            log: Arc::clone(&self.log),
            resident: self.resident.clone(),
            uncertain: self.uncertain.clone(),
            adopting,
        }
    }
}

#[derive(Clone)]
enum Serving {
    Resident(Arc<Bound>),
    Legacy(Arc<Old>),
}

impl Serving {
    const fn path(&self) -> ThreadPath {
        match self {
            Self::Resident(_) => ThreadPath::Resident,
            Self::Legacy(_) => ThreadPath::Legacy,
        }
    }

    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Resident(one), Self::Resident(two)) => Arc::ptr_eq(one, two),
            (Self::Legacy(one), Self::Legacy(two)) => Arc::ptr_eq(one, two),
            (Self::Resident(_) | Self::Legacy(_), _) => false,
        }
    }
}

trait Refusal: Sized {
    fn lost(&self) -> bool;

    fn of(error: StoreError) -> Self;
}

impl Refusal for StoreError {
    fn lost(&self) -> bool {
        matches!(self, Self::Fenced)
    }

    fn of(error: StoreError) -> Self {
        error
    }
}

impl Refusal for WakeError {
    fn lost(&self) -> bool {
        matches!(self, Self::Store(StoreError::Fenced))
    }

    fn of(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl Refusal for HoldsError {
    fn lost(&self) -> bool {
        HoldsError::lost(self)
    }

    fn of(error: StoreError) -> Self {
        Self::Scope(ScopeError::Store(error))
    }
}

impl Refusal for String {
    fn lost(&self) -> bool {
        false
    }

    fn of(error: StoreError) -> Self {
        error.to_string()
    }
}

fn refused(error: ScopeError) -> StoreError {
    match error {
        ScopeError::Store(error) => error,
        ScopeError::Busy => StoreError::Busy,
        ScopeError::Lost => StoreError::Fenced,
        error => StoreError::NotCommitted(error.to_string()),
    }
}

fn local(log: &ThreadLog) -> Result<Arc<ThreadState>, StoreError> {
    log.state().ok_or_else(|| {
        refused(ScopeError::Routed {
            holder: log.holder().unwrap_or_default().to_owned(),
        })
    })
}

fn legacy_read(thread: &str) -> StoreError {
    StoreError::NotCommitted(legacy_refused(thread))
}

async fn handed_over(log: &OneLog) -> Result<(), StoreError> {
    let handed = Arc::clone(&log.handed);
    log.log
        .write_within(move |writing| {
            Box::pin(async move {
                if writing.buffered() > 0 {
                    writing.commit(Point::Progress).await?;
                } else {
                    writing.unride();
                }
                if !sim::mutated(sim::Mutation::HandedOverWrites) {
                    handed.store(true, Ordering::Release);
                }
                Ok::<_, StoreError>(())
            })
        })
        .await?;
    log.owed.set(false);
    assert!(!*log.owed.get(), "a handed over writer owes no settle");
    Ok(())
}

fn bound(
    log: Arc<ThreadLog>,
    thread: &str,
    policy: &Policy,
    effects: &Arc<Atom<Effects>>,
) -> Result<Bound, StoreError> {
    let holds = ThreadHolds::sharing(Arc::clone(&log), Arc::clone(effects))?;
    Ok(Bound {
        turns: ThreadTurns::new(Arc::clone(&log), policy.restarts)?,
        wakes: ThreadWakes::new(Arc::clone(&log), thread, policy.wakes_max)?,
        holds,
        log,
    })
}

async fn unclaimed(logs: &ThreadLogs, thread: &str) -> Result<(), StoreError> {
    match logs.scopes().drain(thread).await {
        Ok(_) | Err(ScopeError::Moved) => Ok(()),
        Err(error) => Err(refused(error)),
    }
}

async fn held(logs: &ThreadLogs, thread: &str) -> Result<(), StoreError> {
    match logs.scopes().hold(thread).await {
        Ok(_) | Err(ScopeError::Routed { .. } | ScopeError::Busy) => {}
        Err(error) => return Err(refused(error)),
    }
    unclaimed(logs, thread).await
}

async fn unadopted(
    logs: &ThreadLogs,
    thread: &str,
    policy: &Policy,
    effects: &Arc<Atom<Effects>>,
    adopting: bool,
) -> Result<Old, StoreError> {
    held(logs, thread).await?;
    let log = legacy::writer(logs, thread, policy, effects).await?;
    let uncertain = log.uncertain_holds().await;
    Ok(Old {
        log,
        resident: None,
        uncertain,
        adopting,
    })
}

impl OneLog {
    pub async fn over(
        logs: Arc<ThreadLogs>,
        thread: &str,
        policy: &Policy,
    ) -> Result<Arc<OneLogOver>, StoreError> {
        let effects = Arc::new(Atom::new(Effects::builtin().merged(policy.effects.clone())));
        let (log, serving) = if logs.path(thread) == ThreadPath::Legacy {
            if sim::mutated(sim::Mutation::LegacyOpenClaims) {
                drop(logs.reach(thread).await.map_err(refused)?);
            }
            let old = unadopted(&logs, thread, policy, &effects, false).await?;
            (None, Serving::Legacy(Arc::new(old)))
        } else {
            match logs.reach(thread).await {
                Ok(log) => {
                    let log = Arc::new(log);
                    let bound = bound(Arc::clone(&log), thread, policy, &effects)?;
                    (Some(log), Serving::Resident(Arc::new(bound)))
                }
                Err(ScopeError::Unadopted) => {
                    let old = unadopted(&logs, thread, policy, &effects, true).await?;
                    (None, Serving::Legacy(Arc::new(old)))
                }
                Err(error) => return Err(refused(error)),
            }
        };
        let binding = Arc::new(ThreadBinding::new(Arc::clone(&logs), thread, log));
        let store = ThreadStore::rebinding(Arc::clone(&binding));
        let journals = ThreadJournals::rebinding(Arc::clone(&binding), Arc::clone(&effects));
        Ok(Arc::new_cyclic(|this| OneLogOver {
            logs,
            thread: thread.to_owned(),
            policy: policy.clone(),
            binding,
            serving: Atom::new(serving),
            switching: SingleFlight::default(),
            store,
            journals,
            effects,
            index: Atom::new(None),
            durable: AtomicBool::new(false),
            deferred: AtomicBool::new(false),
            this: Weak::clone(this),
        }))
    }
}

enum Gated<L> {
    Resident(HoldsError),
    Legacy(L),
}

impl<L> Refusal for Gated<L> {
    fn lost(&self) -> bool {
        match self {
            Self::Resident(error) => error.lost(),
            Self::Legacy(_) => false,
        }
    }

    fn of(error: StoreError) -> Self {
        Self::Resident(HoldsError::of(error))
    }
}

impl Gated<String> {
    fn said(self) -> String {
        match self {
            Self::Resident(error) => error.said(),
            Self::Legacy(said) => said,
        }
    }
}

impl Gated<LedgerError> {
    fn ledger(self) -> LedgerError {
        match self {
            Self::Resident(error) => error.ledger(),
            Self::Legacy(error) => error,
        }
    }
}

impl OneLogOver {
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.thread
    }

    #[must_use]
    pub fn path(&self) -> ThreadPath {
        self.current().path()
    }

    fn current(&self) -> Serving {
        self.serving.read(Clone::clone)
    }

    #[must_use]
    pub fn legacy(&self) -> Option<Arc<OneLog>> {
        match self.current() {
            Serving::Legacy(old) => Some(Arc::clone(&old.log)),
            Serving::Resident(_) => None,
        }
    }

    fn settled(&self, stale: &Serving, fresh: Serving) -> Serving {
        self.serving.update(|serving| {
            if serving.same(stale) {
                *serving = fresh.clone();
            }
            serving.clone()
        })
    }

    async fn fresh(&self) -> Result<Serving, StoreError> {
        if sim::mutated(sim::Mutation::OpenPerRequest) {
            adopt::read_all(
                &self.logs.scopes().history(),
                &self.thread,
                Reading::Below(SCAN_END),
            )
            .await?;
        }
        let serving = self.current();
        if sim::mutated(sim::Mutation::OverReopensLegacyWriter)
            && let Serving::Legacy(old) = &serving
            && self.logs.path(&self.thread) == ThreadPath::Legacy
        {
            let mut reopened = old.adopting(old.adopting);
            reopened.log =
                legacy::writer(&self.logs, &self.thread, &self.policy, &self.effects).await?;
            return Ok(Serving::Legacy(Arc::new(reopened)));
        }
        if self.serves(&serving) {
            return Ok(serving);
        }
        self.switching.run(|| self.switched()).await
    }

    fn serves(&self, serving: &Serving) -> bool {
        match (serving, self.logs.path(&self.thread)) {
            (Serving::Legacy(old), ThreadPath::Legacy) => !old.adopting,
            (Serving::Resident(bound), ThreadPath::Resident) => self
                .binding
                .log()
                .is_some_and(|log| Arc::ptr_eq(&bound.log, &log)),
            (Serving::Legacy(old), ThreadPath::Resident) => {
                old.adopting
                    && (self.logs.scopes().adopting(&self.thread)
                        || sim::mutated(sim::Mutation::AdoptedServesLegacy))
            }
            (Serving::Resident(_), ThreadPath::Legacy) => false,
        }
    }

    async fn switched(&self) -> Result<Serving, StoreError> {
        let serving = self.current();
        let path = self.logs.path(&self.thread);
        let flipped = match &serving {
            Serving::Legacy(old) if old.adopting => path == ThreadPath::Legacy,
            serving => serving.path() != path,
        };
        let fresh = match (&serving, path) {
            (Serving::Legacy(old), ThreadPath::Legacy) if old.adopting => {
                Serving::Legacy(Arc::new(old.adopting(false)))
            }
            (Serving::Legacy(_), ThreadPath::Legacy) => return Ok(serving),
            (Serving::Resident(bound), ThreadPath::Resident) => match self.binding.log() {
                Some(log) if Arc::ptr_eq(&bound.log, &log) => return Ok(serving),
                Some(log) => Serving::Resident(self.bind(bound, log).await?),
                None => Serving::Resident(self.rebound(bound).await?),
            },
            (Serving::Resident(bound), ThreadPath::Legacy) => {
                Serving::Legacy(self.old(Some(bound)).await?)
            }
            (Serving::Legacy(old), ThreadPath::Resident)
                if old.adopting
                    && (self.logs.scopes().adopting(&self.thread)
                        || sim::mutated(sim::Mutation::AdoptedServesLegacy)) =>
            {
                return Ok(serving);
            }
            (Serving::Legacy(old), ThreadPath::Resident) => match self.adopted(old).await {
                Ok(Some(bound)) => Serving::Resident(bound),
                Ok(None) => {
                    old.log.handed.store(false, Ordering::Release);
                    Serving::Legacy(Arc::new(old.adopting(true)))
                }
                Err(error) => {
                    old.log.handed.store(false, Ordering::Release);
                    return Err(error);
                }
            },
        };
        if flipped {
            self.logs.flipped();
        }
        Ok(self.settled(&serving, fresh))
    }

    async fn drained(&self) -> Result<(), StoreError> {
        let mut reached = self.logs.reach(&self.thread).await.map_err(refused)?;
        if reached.state().is_none()
            && let Err(ScopeError::Unreachable { .. }) = reached.send(ThreadCommand::Pending).await
        {
            reached = self.logs.reach(&self.thread).await.map_err(refused)?;
        }
        local(&reached)?;
        unclaimed(&self.logs, &self.thread).await?;
        drop(reached);
        Ok(())
    }

    async fn old(&self, resident: Option<&Arc<Bound>>) -> Result<Arc<Old>, StoreError> {
        if resident.is_some() {
            self.drained().await?;
        } else {
            held(&self.logs, &self.thread).await?;
        }
        let log = legacy::writer(&self.logs, &self.thread, &self.policy, &self.effects).await?;
        if self.durable.load(Ordering::Relaxed) {
            log.settle_durably();
        }
        if self.deferred.load(Ordering::Relaxed) {
            log.deferred_settles.set(true);
        }
        if let Some(index) = self.index.read(Clone::clone) {
            log.index_wakes(index).await?;
        }
        let uncertain = log.uncertain_holds().await;
        Ok(Arc::new(Old {
            log,
            resident: resident.cloned(),
            uncertain,
            adopting: false,
        }))
    }

    async fn missed(&self, log: &ThreadLog) -> Result<bool, StoreError> {
        let Some(next) = log.next() else {
            return Ok(false);
        };
        if sim::mutated(sim::Mutation::SwitchMissesTail) {
            return Ok(false);
        }
        let past = self
            .logs
            .scopes()
            .legacy()
            .scan(&self.thread, next, SCAN_END)
            .await?;
        assert!(
            past.iter().all(|(seq, _)| *seq >= next),
            "a scan of {} from row {next} answers no row below it",
            self.thread
        );
        Ok(!past.is_empty())
    }

    async fn caught_up(&self, log: Arc<ThreadLog>) -> Result<Option<Arc<ThreadLog>>, StoreError> {
        if !self.missed(&log).await? {
            return Ok(Some(log));
        }
        unclaimed(&self.logs, &self.thread).await?;
        match self.binding.rebound(Some(&log)).await {
            Ok(log) => Ok(Some(log)),
            Err(ScopeError::Unadopted) => Ok(None),
            Err(error) => Err(refused(error)),
        }
    }

    async fn adopted(&self, old: &Old) -> Result<Option<Arc<Bound>>, StoreError> {
        handed_over(&old.log).await?;
        let stale = old.resident.as_ref().map(|bound| &bound.log);
        let log = match self.binding.log() {
            Some(log) if !log.retired() && !stale.is_some_and(|stale| Arc::ptr_eq(&log, stale)) => {
                log
            }
            current => match self.binding.rebound(current.as_ref()).await {
                Ok(log) => log,
                Err(ScopeError::Unadopted) => return Ok(None),
                Err(error) => return Err(refused(error)),
            },
        };
        let Some(log) = self.caught_up(log).await? else {
            return Ok(None);
        };
        self.store.owning(old.log.owner_tokens());
        let Some(resident) = &old.resident else {
            let bound = Arc::new(bound(log, &self.thread, &self.policy, &self.effects)?);
            if let Some(index) = self.index.read(Clone::clone) {
                bound.wakes.index_wakes(index).await?;
            }
            return Ok(Some(bound));
        };
        self.bind(resident, log).await.map(Some)
    }

    async fn again(&self, stale: &Serving) -> Result<Serving, StoreError> {
        if self.logs.path(&self.thread) != stale.path() {
            return self.fresh().await;
        }
        let fresh = match stale {
            Serving::Resident(bound) => Serving::Resident(self.rebound(bound).await?),
            Serving::Legacy(old) => Serving::Legacy(self.old(old.resident.as_ref()).await?),
        };
        Ok(self.settled(stale, fresh))
    }

    async fn rebound(&self, stale: &Bound) -> Result<Arc<Bound>, StoreError> {
        let log = self
            .binding
            .rebound(Some(&stale.log))
            .await
            .map_err(refused)?;
        self.bind(stale, log).await
    }

    async fn bind(&self, stale: &Bound, log: Arc<ThreadLog>) -> Result<Arc<Bound>, StoreError> {
        let bound = Arc::new(Bound {
            turns: stale.turns.rebound(Arc::clone(&log))?,
            holds: stale.holds.rebound(Arc::clone(&log))?,
            wakes: stale.wakes.rebound(Arc::clone(&log))?,
            log,
        });
        if let Some(index) = self.index.read(Clone::clone) {
            bound.wakes.index_wakes(index).await?;
        }
        Ok(bound)
    }

    async fn retried<T, E, F, Fut>(&self, command: F) -> Result<T, E>
    where
        E: Refusal,
        F: Fn(Serving) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let serving = self.fresh().await.map_err(E::of)?;
        match command(serving.clone()).await {
            Err(_) if matches!(&serving, Serving::Legacy(old) if old.log.handed()) => {
                command(self.fresh().await.map_err(E::of)?).await
            }
            Err(error) if error.lost() && !sim::mutated(sim::Mutation::LostUnretried) => {
                let serving = self.again(&serving).await.map_err(E::of)?;
                command(serving).await
            }
            answered => answered,
        }
    }

    async fn published(&self) -> Result<Arc<ThreadState>, StoreError> {
        local(&self.logs.get(&self.thread).await.map_err(refused)?)
    }

    pub async fn read<R>(&self, read: impl FnOnce(&ThreadState) -> R) -> Result<R, StoreError> {
        match self.fresh().await? {
            Serving::Resident(_) => Ok(read(&*self.published().await?)),
            Serving::Legacy(_) => Err(legacy_read(&self.thread)),
        }
    }

    #[must_use]
    pub fn remembered_bytes(&self) -> usize {
        match self.current() {
            Serving::Resident(bound) => bound.log.state().map_or(0, |state| state.weighed()),
            Serving::Legacy(old) => old.log.remembered_bytes(),
        }
    }

    pub async fn reclaim(&self) -> Result<(), StoreError> {
        match self.fresh().await? {
            Serving::Resident(_) => self.published().await.map(drop),
            Serving::Legacy(old) => old.log.reclaim().await,
        }
    }

    pub async fn submit(&self, submission: &Submission) -> Result<Submitted, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.submit(submission).await,
                Serving::Legacy(old) => old.log.submit(submission).await,
            }
        })
        .await
    }

    pub async fn submit_buffered(&self, submission: &Submission) -> Result<Submitted, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.submit_buffered(submission).await,
                Serving::Legacy(old) => old.log.submit_buffered(submission).await,
            }
        })
        .await
    }

    pub async fn append(&self, entries: &[LogEntry]) -> Result<Vec<LoggedEvent>, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.append(entries).await,
                Serving::Legacy(old) => old.log.append(entries).await,
            }
        })
        .await
    }

    pub async fn turn(&self, request_id: &str) -> Result<Option<TurnRecord>, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.turn(request_id).await,
                Serving::Legacy(old) => old.log.turn(request_id).await,
            }
        })
        .await
    }

    pub async fn pending(&self) -> Result<Vec<TurnRecord>, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.pending().await,
                Serving::Legacy(old) => old.log.pending().await,
            }
        })
        .await
    }

    pub async fn start(&self, request_id: &str) -> Result<Option<Started>, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.start(request_id).await,
                Serving::Legacy(old) => old.log.start(request_id).await,
            }
        })
        .await
    }

    pub async fn finish(
        &self,
        request_id: &str,
        state: TurnState,
        fields: &Map<String, Value>,
    ) -> Result<Option<(TurnRecord, LoggedEvent)>, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.finish(request_id, state, fields).await,
                Serving::Legacy(old) => old.log.finish(request_id, state, fields).await,
            }
        })
        .await
    }

    pub async fn cancel(
        &self,
        request_id: &str,
        fields: &Map<String, Value>,
    ) -> Result<Answered, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.cancel(request_id, fields).await,
                Serving::Legacy(old) => old.log.cancel(request_id, fields).await,
            }
        })
        .await
    }

    pub async fn withdraw_steered(
        &self,
        request_id: &str,
        digest: &str,
    ) -> Result<Option<(TurnRecord, LoggedEvent)>, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.withdraw_steered(request_id, digest).await,
                Serving::Legacy(old) => old.log.withdraw_steered(request_id, digest).await,
            }
        })
        .await
    }

    pub async fn interrupted(&self, request_id: &str, note: &str) -> Result<(), StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.turns.interrupted(request_id, note).await,
                Serving::Legacy(old) => old.log.interrupted(request_id, note).await,
            }
        })
        .await
    }

    pub async fn retry(
        &self,
        request_id: &str,
        error: &str,
        kind: &str,
        retry_at: u64,
    ) -> Result<Answered, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => {
                    bound.turns.retry(request_id, error, kind, retry_at).await
                }
                Serving::Legacy(old) => old.log.retry(request_id, error, kind, retry_at).await,
            }
        })
        .await
    }

    #[must_use]
    pub fn store(&self) -> ThreadStore {
        self.store.clone()
    }

    #[must_use]
    pub fn journals(&self) -> ThreadJournals {
        self.journals.clone()
    }

    pub async fn claim(&self) -> Result<(), StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(_) => Ok(()),
                Serving::Legacy(old) => legacy::claimed(&old.log).await,
            }
        })
        .await
    }

    pub async fn served_store(&self) -> Result<ServedStore, StoreError> {
        Ok(match self.fresh().await? {
            Serving::Resident(_) => ServedStore::Resident(self.store.clone()),
            Serving::Legacy(old) => ServedStore::Legacy(old.log.store()),
        })
    }

    pub async fn states(&self) -> Result<Box<dyn StateStore>, StoreError> {
        if !sim::mutated(sim::Mutation::ServedHandlesPinned) {
            self.fresh().await?;
            return Ok(Box::new(routed::Routed(self.this())));
        }
        Ok(match self.served_store().await? {
            ServedStore::Resident(store) => Box::new(store),
            ServedStore::Legacy(store) => Box::new(store),
        })
    }

    pub async fn turn_count(&self) -> Result<usize, StoreError> {
        match self.fresh().await? {
            Serving::Resident(_) => self.read(|state| state.turns().count()).await,
            Serving::Legacy(old) => Ok(old.log.read(|fold| fold.turns().len()).await),
        }
    }

    pub async fn held_bytes(&self) -> usize {
        match self.current() {
            Serving::Resident(_) => self.remembered_bytes(),
            Serving::Legacy(old) => {
                old.log.read(Fold::resident_bytes).await + old.log.remembered_bytes()
            }
        }
    }

    pub async fn sink(&self) -> Result<Arc<dyn CellJournalSink>, StoreError> {
        if !sim::mutated(sim::Mutation::ServedHandlesPinned) {
            self.fresh().await?;
            return Ok(Arc::new(routed::RoutedSink(self.this())));
        }
        Ok(match self.fresh().await? {
            Serving::Resident(_) => self.journals.sink(),
            Serving::Legacy(old) => old.log.sink(),
        })
    }

    pub fn settle_durably(&self) {
        self.durable.store(true, Ordering::Relaxed);
        self.store.settle_durably();
        if let Serving::Legacy(old) = self.current() {
            old.log.settle_durably();
        }
    }

    pub async fn import_state(
        &self,
        state_id: &str,
        revision: u64,
        payload: &str,
        records: &[StoreRecord],
    ) -> Result<Imported, StoreError> {
        match self.fresh().await? {
            Serving::Resident(_) => {
                self.store
                    .import_state(state_id, revision, payload, records)
                    .await
            }
            Serving::Legacy(old) => {
                old.log
                    .import_state(state_id, revision, payload, records)
                    .await
            }
        }
    }

    pub async fn export_state(&self, state_id: &str) -> Result<Option<Exported>, StoreError> {
        match self.fresh().await? {
            Serving::Resident(_) => self.store.export_state(state_id).await,
            Serving::Legacy(old) => old.log.export_state(state_id).await,
        }
    }

    pub fn declare(&self, tool: impl Into<String>, class: EffectClass) {
        let tool = tool.into();
        self.effects
            .update(|effects| effects.insert(tool.clone(), class));
        if let Serving::Legacy(old) = self.current() {
            old.log.declare(tool, class);
        }
    }

    #[must_use]
    pub fn effect_of(&self, tool: &str) -> EffectClass {
        match self.current() {
            Serving::Resident(bound) => bound.holds.effect_of(tool),
            Serving::Legacy(old) => old.log.effect_of(tool),
        }
    }

    #[must_use]
    pub fn uncertain_holds(&self) -> Vec<String> {
        match self.current() {
            Serving::Resident(bound) => bound.holds.uncertain_holds(),
            Serving::Legacy(old) => {
                let resolved = old.log.resolved.get();
                old.uncertain
                    .iter()
                    .filter(|key| !resolved.contains(*key))
                    .cloned()
                    .collect()
            }
        }
    }

    fn this(&self) -> Arc<Self> {
        self.this
            .upgrade()
            .expect("a OneLog over a thread is reached through its Arc")
    }

    #[must_use]
    pub fn gate(&self) -> Arc<dyn EffectGate> {
        self.this()
    }

    #[must_use]
    pub fn ledger(&self) -> Arc<dyn HoldLedger> {
        self.this()
    }

    #[must_use]
    pub fn barrier(&self) -> Arc<dyn RemoteBarrier> {
        self.deferred.store(true, Ordering::Relaxed);
        if let Serving::Legacy(old) = self.current() {
            old.log.deferred_settles.set(true);
        }
        self.this()
    }

    pub async fn index_wakes(&self, index: Arc<dyn WakeRider>) -> Result<(), StoreError> {
        self.index.set(Some(Arc::clone(&index)));
        self.retried(|serving| {
            let index = Arc::clone(&index);
            async move {
                match serving {
                    Serving::Resident(bound) => bound.wakes.index_wakes(index).await,
                    Serving::Legacy(old) => old.log.index_wakes(index).await,
                }
            }
        })
        .await
    }

    pub async fn arm(&self, key: &str, wake: Wake) -> Result<u64, WakeError> {
        self.retried(|serving| {
            let wake = wake.clone();
            async move {
                match serving {
                    Serving::Resident(bound) => bound.wakes.arm(key, wake).await,
                    Serving::Legacy(old) => old.log.arm(key, wake).await,
                }
            }
        })
        .await
    }

    pub async fn disarm(&self, key: &str) -> Result<Option<u64>, StoreError> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.wakes.disarm(key).await,
                Serving::Legacy(old) => old.log.disarm(key).await,
            }
        })
        .await
    }

    pub async fn hear(
        &self,
        topic: &str,
        fields: &Map<String, Value>,
        event: Box<RawValue>,
    ) -> Result<Heard, StoreError> {
        self.retried(|serving| {
            let event = event.clone();
            async move {
                match serving {
                    Serving::Resident(bound) => bound.wakes.hear(topic, fields, event).await,
                    Serving::Legacy(old) => old.log.hear(topic, fields, event).await,
                }
            }
        })
        .await
    }

    pub async fn fire(
        &self,
        key: &str,
        generation: u64,
        now_ms: u64,
        submission: &Submission,
        next: Option<Wake>,
    ) -> Result<Fire, WakeError> {
        self.retried(|serving| {
            let next = next.clone();
            async move {
                match serving {
                    Serving::Resident(bound) => {
                        bound
                            .wakes
                            .fire(key, generation, now_ms, submission, next)
                            .await
                    }
                    Serving::Legacy(old) => {
                        old.log
                            .fire(key, generation, now_ms, submission, next)
                            .await
                    }
                }
            }
        })
        .await
    }

    pub async fn wake(&self, key: &str) -> Result<Option<ArmedWake>, StoreError> {
        match self.fresh().await? {
            Serving::Resident(_) => self.read(|state| wakes::armed_at(state, key)).await,
            Serving::Legacy(old) => Ok(old.log.wake(key).await),
        }
    }

    pub async fn due_wakes(&self, now_ms: u64) -> Result<Vec<(String, ArmedWake)>, StoreError> {
        match self.fresh().await? {
            Serving::Resident(_) => self.read(|state| wakes::due(state, now_ms)).await,
            Serving::Legacy(old) => Ok(old.log.due_wakes(now_ms).await),
        }
    }

    pub async fn next_due(&self) -> Result<Option<u64>, StoreError> {
        match self.fresh().await? {
            Serving::Resident(_) => self.read(|state| wakes::summary(state).next_due).await,
            Serving::Legacy(old) => Ok(old.log.next_due().await),
        }
    }
}

impl EffectGate for OneLogOver {
    fn effect(&self, tool: &str) -> EffectClass {
        self.effect_of(tool)
    }

    fn before<'a>(&'a self, call: EffectCall<'a>) -> HostFuture<'a, Result<(), String>> {
        let EffectCall {
            call_id,
            effect,
            input,
            session_id,
            tool,
            turn_id,
        } = call;
        Box::pin(async move {
            self.retried(move |serving| async move {
                let call = EffectCall {
                    call_id,
                    effect,
                    input,
                    session_id,
                    tool,
                    turn_id,
                };
                match serving {
                    Serving::Resident(bound) => {
                        bound.holds.before_call(call).await.map_err(Gated::Resident)
                    }
                    Serving::Legacy(old) => EffectGate::before(&*old.log, call)
                        .await
                        .map_err(Gated::Legacy),
                }
            })
            .await
            .map_err(Gated::said)
        })
    }

    fn settle(&self) -> HostFuture<'_, Result<(), String>> {
        Box::pin(async move { self.settled_holds().await })
    }

    fn abandon<'a>(
        &'a self,
        call_id: &'a str,
    ) -> HostFuture<'a, Result<Option<ToolOutput>, String>> {
        Box::pin(async move {
            self.retried(move |serving| async move {
                match serving {
                    Serving::Resident(bound) => bound
                        .holds
                        .abandon_call(call_id)
                        .await
                        .map_err(Gated::Resident),
                    Serving::Legacy(old) => EffectGate::abandon(&*old.log, call_id)
                        .await
                        .map_err(Gated::Legacy),
                }
            })
            .await
            .map_err(Gated::said)
        })
    }

    fn after<'a>(&'a self, call_id: &'a str, output: &'a ToolOutput) -> HostFuture<'a, ()> {
        Box::pin(async move {
            match self.current() {
                Serving::Resident(bound) => bound.holds.after(call_id, output).await,
                Serving::Legacy(old) => EffectGate::after(&*old.log, call_id, output).await,
            }
        })
    }
}

impl OneLogOver {
    async fn settled_holds(&self) -> Result<(), String> {
        self.retried(|serving| async move {
            match serving {
                Serving::Resident(bound) => bound.holds.settled().await.map_err(Gated::Resident),
                Serving::Legacy(old) => EffectGate::settle(&*old.log).await.map_err(Gated::Legacy),
            }
        })
        .await
        .map_err(Gated::said)
    }
}

impl HoldLedger for OneLogOver {
    fn hold<'a>(
        &'a self,
        key: &'a str,
        call: HoldCall<'a>,
        card: &'a RawValue,
    ) -> HostFuture<'a, Result<(), LedgerError>> {
        let HoldCall {
            call_id,
            input,
            session_id,
            tool,
            turn_id,
        } = call;
        Box::pin(async move {
            self.retried(move |serving| async move {
                let call = HoldCall {
                    call_id,
                    input,
                    session_id,
                    tool,
                    turn_id,
                };
                match serving {
                    Serving::Resident(bound) => bound
                        .holds
                        .hold_call(key, call, card)
                        .await
                        .map_err(Gated::Resident),
                    Serving::Legacy(old) => HoldLedger::hold(&*old.log, key, call, card)
                        .await
                        .map_err(Gated::Legacy),
                }
            })
            .await
            .map_err(Gated::ledger)
        })
    }

    fn resolve<'a>(
        &'a self,
        key: &'a str,
        answer: Answer,
    ) -> HostFuture<'a, Result<Resumption, LedgerError>> {
        Box::pin(async move {
            self.retried(move |serving| {
                let answer = answer.clone();
                async move {
                    match serving {
                        Serving::Resident(bound) => bound
                            .holds
                            .resolve_hold(key, answer)
                            .await
                            .map_err(Gated::Resident),
                        Serving::Legacy(old) => HoldLedger::resolve(&*old.log, key, answer)
                            .await
                            .map_err(Gated::Legacy),
                    }
                }
            })
            .await
            .map_err(Gated::ledger)
        })
    }

    fn running<'a>(&'a self, call_id: &'a str) -> HostFuture<'a, Option<Running>> {
        Box::pin(async move {
            match self.current() {
                Serving::Resident(bound) => bound.holds.running(call_id).await,
                Serving::Legacy(old) => HoldLedger::running(&*old.log, call_id).await,
            }
        })
    }

    fn claim<'a>(&'a self, key: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async move {
            match self.current() {
                Serving::Resident(bound) => bound.holds.claim(key).await,
                Serving::Legacy(old) => HoldLedger::claim(&*old.log, key).await,
            }
        })
    }

    fn release<'a>(&'a self, key: &'a str) -> HostFuture<'a, Result<(), LedgerError>> {
        Box::pin(async move {
            self.retried(move |serving| async move {
                match serving {
                    Serving::Resident(bound) => {
                        bound.holds.release_hold(key).await.map_err(Gated::Resident)
                    }
                    Serving::Legacy(old) => HoldLedger::release(&*old.log, key)
                        .await
                        .map_err(Gated::Legacy),
                }
            })
            .await
            .map_err(Gated::ledger)
        })
    }
}

impl RemoteBarrier for OneLogOver {
    fn settle(&self) -> HostFuture<'_, Result<(), String>> {
        Box::pin(async move {
            match self.current() {
                Serving::Legacy(old) => RemoteBarrier::settle(&*old.log).await,
                Serving::Resident(_) => self.settled_holds().await,
            }
        })
    }

    fn carry(&self, carried: Carried) -> HostFuture<'_, Result<bool, String>> {
        Box::pin(async move {
            match self.current() {
                Serving::Resident(bound) => bound.holds.carry(carried).await,
                Serving::Legacy(old) => RemoteBarrier::carry(&*old.log, carried).await,
            }
        })
    }
}
