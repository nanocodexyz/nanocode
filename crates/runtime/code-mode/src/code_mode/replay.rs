use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};

use nanocode_sync::watch;
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use nanocode_code_mode_engine::{JournalEntry, SharedJournal};
use nanocode_sync::{Atom, Inbox, OnceCell, Set, mpsc, oneshot};

use crate::embedded::HostFuture;

pub const CELL_JOURNAL_VERSION: u32 = 1;

pub(super) const UNKNOWN_OUTCOME: &str = "This nested call was running when its cell moved to another host; its outcome is unknown. Do not assume it did not run or automatically repeat it.";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellJournal {
    pub version: u32,
    pub cell_id: u64,
    pub session_id: String,
    pub origin_call_id: String,
    #[serde(default)]
    pub observer_call_id: Option<String>,
    #[serde(default)]
    pub observer_yield_ms: Option<u64>,
    #[serde(default)]
    pub observer_since_ms: Option<u64>,
    pub script: String,
    pub tools: Arc<[Value]>,
    #[serde(default)]
    pub stored: BTreeMap<String, Value>,
    #[serde(default)]
    pub started_at_ms: Option<u64>,
    #[serde(default)]
    pub prelude: bool,
    pub yield_time_ms: u64,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
    #[serde(default)]
    pub content_returned: u64,
    pub entries: Vec<JournalEntry>,
    #[serde(default)]
    pub calls: Vec<JournaledCall>,
    #[serde(default)]
    pub finished: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournaledCall {
    pub id: u64,
    pub call_id: String,
    pub name: String,
    pub input: Value,
    #[serde(default)]
    pub host: bool,
    pub state: JournaledCallState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JournaledCallState {
    Queued,
    Running,
    Completed { encoded: String, success: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellJournalHeader {
    pub version: u32,
    pub origin_call_id: String,
    pub script: String,
    pub tools: Arc<[Value]>,
    #[serde(default, deserialize_with = "stored_values")]
    pub stored: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default)]
    pub prelude: bool,
    pub yield_time_ms: u64,
    #[serde(default)]
    pub max_output_tokens: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellJournalUpdate {
    pub session_id: String,
    pub cell_id: u64,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<CellJournalHeader>,
    pub entries_from: usize,
    #[serde(default)]
    pub entries: Vec<JournalEntry>,
    #[serde(default)]
    pub calls: Vec<Arc<JournaledCall>>,
    #[serde(default)]
    pub observer_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_yield_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_since_ms: Option<u64>,
    #[serde(default)]
    pub content_returned: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub finished: bool,
}

pub trait CellJournalSink: Send + Sync + 'static {
    fn save(&self, update: CellJournalUpdate) -> HostFuture<'static, Result<(), String>>;

    fn forget(&self, session_id: &str, cell_id: u64) -> HostFuture<'static, ()>;

    fn load(&self, _session_id: &str) -> HostFuture<'static, Result<Vec<CellJournal>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn save_stored(
        &self,
        _session_id: &str,
        _stored: String,
    ) -> HostFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn load_stored(
        &self,
        _session_id: &str,
    ) -> HostFuture<'static, Result<Option<String>, String>> {
        Box::pin(async { Ok(None) })
    }
}

impl CellJournal {
    #[must_use]
    pub fn fold(updates: impl IntoIterator<Item = CellJournalUpdate>) -> Option<Self> {
        let mut updates = updates.into_iter().collect::<Vec<_>>();
        updates.sort_by_key(|update| update.seq);
        let start = updates.iter().position(|update| update.header.is_some())?;
        let mut updates = updates.into_iter().skip(start);
        let mut first = updates.next()?;
        let header = first.header.take()?;
        let mut journal = Self {
            version: header.version,
            cell_id: first.cell_id,
            session_id: first.session_id.clone(),
            origin_call_id: header.origin_call_id,
            observer_call_id: None,
            observer_yield_ms: None,
            observer_since_ms: None,
            script: header.script,
            tools: header.tools,
            stored: header.stored,
            started_at_ms: header.started_at_ms,
            prelude: header.prelude,
            yield_time_ms: header.yield_time_ms,
            max_output_tokens: header.max_output_tokens,
            content_returned: 0,
            entries: Vec::new(),
            calls: Vec::new(),
            finished: false,
        };
        let mut calls = BTreeMap::new();
        let mut seq = first.seq;
        for update in std::iter::once(first).chain(updates) {
            if update.seq != seq
                || update.cell_id != journal.cell_id
                || update.entries_from > journal.entries.len()
            {
                break;
            }
            seq = seq.saturating_add(1);
            let known = journal.entries.len() - update.entries_from;
            journal
                .entries
                .extend(update.entries.into_iter().skip(known));
            for call in update.calls {
                calls.insert(call.id, call);
            }
            if update.observer_call_id.is_some() {
                journal.observer_call_id = update.observer_call_id;
                journal.observer_yield_ms = update.observer_yield_ms;
            }
            if update.observer_since_ms.is_some() {
                journal.observer_since_ms = update.observer_since_ms;
            }
            journal.content_returned = journal.content_returned.max(update.content_returned);
            journal.finished |= update.finished;
        }
        let delivered = journal
            .entries
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::Result { id, .. } => Some(*id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        for entry in &journal.entries {
            if let JournalEntry::Call {
                id,
                name,
                input,
                host,
            } = entry
                && !delivered.contains(id)
            {
                calls.entry(*id).or_insert_with(|| {
                    Arc::new(JournaledCall {
                        id: *id,
                        call_id: format!("{}/code-{id}", journal.origin_call_id),
                        name: name.clone(),
                        input: input.clone(),
                        host: *host,
                        state: JournaledCallState::Queued,
                    })
                });
            }
        }
        journal.calls = calls
            .into_values()
            .filter(|call| !delivered.contains(&call.id))
            .map(Arc::unwrap_or_clone)
            .collect();
        Some(journal)
    }

    pub fn unknown_calls(&self) -> impl Iterator<Item = &JournaledCall> {
        self.calls
            .iter()
            .filter(|call| matches!(call.state, JournaledCallState::Running))
    }

    pub(super) fn stored_values(&self) -> HashMap<String, Arc<RawValue>> {
        self.stored
            .iter()
            .filter_map(|(key, value)| {
                serde_json::value::to_raw_value(value)
                    .ok()
                    .map(|raw| (key.clone(), Arc::from(raw)))
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CellJournalState {
    pub cells: usize,
    pub held: usize,
    pub running_calls: usize,
    pub replayable: bool,
}

impl CellJournalState {
    #[must_use]
    pub const fn settled(&self) -> bool {
        self.replayable && self.held == self.cells && self.running_calls == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellHold {
    None,
    Wait,
    Replay,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MovedCells {
    pub turn: String,
    pub run: u32,
    pub cells: Vec<CellJournal>,
}

#[derive(Clone, Default)]
pub struct CellJournals(Arc<Journals>);

type RestoreGate = (watch::Receiver<bool>, Arc<AtomicUsize>);

#[derive(Default)]
struct Journals {
    cells: Atom<Vec<Weak<CellRecord>>>,
    held: Atom<Set<String>>,
    restored: Atom<HashMap<String, Arc<Restored>>>,
    pending: Atom<HashMap<String, RestoreGate>>,
    finished: Atom<Vec<Arc<CellRecord>>>,
    dormant: Atom<Vec<(Arc<CellJournal>, Arc<Restored>)>>,
    loaded: Atom<Set<String>>,
    preempted: Atom<Set<String>>,
    stored: Atom<HashMap<String, HashMap<String, Arc<RawValue>>>>,
    saving_stored: Arc<OnceCell<StoredJobs>>,
    sink: Option<Arc<dyn CellJournalSink>>,
}

pub(super) struct Persistence {
    sink: Arc<dyn CellJournalSink>,
    saving: OnceCell<SavingJobs>,
    saving_stored: Arc<OnceCell<StoredJobs>>,
    queued: Inbox<Arc<JournaledCall>>,
}

const SAVING_STOPPED: &str = "the journal's saving task stopped";

enum SavingJob {
    Persist {
        record: Arc<CellRecord>,
        started: Vec<(u64, u8)>,
    },
    Finish(Arc<CellRecord>),
    Stop,
}

type SavingJobs = mpsc::UnboundedSender<(SavingJob, Option<oneshot::Sender<()>>)>;

type StoredJob = (
    Arc<CellRecord>,
    Arc<Atom<HashMap<String, Arc<RawValue>>>>,
    oneshot::Sender<Result<(), String>>,
);

type StoredJobs = mpsc::Sender<StoredJob>;

const STORED_SAVES_IN_FLIGHT: usize = 1;

impl Persistence {
    fn jobs(&self) -> std::io::Result<&SavingJobs> {
        if let Some(jobs) = self.saving.get() {
            return Ok(jobs);
        }
        let _ = self.saving.set(start_saving()?);
        Ok(self
            .saving
            .get()
            .expect("the saving task is set before it is read"))
    }

    fn queue(&self, job: SavingJob) -> std::io::Result<()> {
        self.jobs()?
            .send((job, None))
            .map_err(|_| std::io::Error::other(SAVING_STOPPED))
    }

    async fn save(&self, job: SavingJob) -> std::io::Result<()> {
        let (reply, saved) = oneshot::channel();
        self.jobs()?
            .send((job, Some(reply)))
            .map_err(|_| std::io::Error::other(SAVING_STOPPED))?;
        saved
            .await
            .map_err(|_| std::io::Error::other(SAVING_STOPPED))
    }

    async fn stop(&self) -> std::io::Result<()> {
        self.save(SavingJob::Stop).await
    }

    async fn save_stored(
        &self,
        record: Arc<CellRecord>,
        stored: Arc<Atom<HashMap<String, Arc<RawValue>>>>,
    ) -> Result<(), String> {
        let jobs = self
            .saving_stored
            .get_or_try_init(|| std::future::ready(start_saving_stored()))
            .await
            .map_err(|error| error.to_string())?;
        let (reply, saved) = oneshot::channel();
        jobs.send((record, stored, reply))
            .await
            .map_err(|_| SAVING_STOPPED.to_owned())?;
        saved.await.map_err(|_| SAVING_STOPPED.to_owned())?
    }
}

fn start_saving() -> std::io::Result<SavingJobs> {
    let (jobs, mut received) =
        mpsc::unbounded_channel::<(SavingJob, Option<oneshot::Sender<()>>)>();
    super::spawn_owner(async move {
        let mut saving = Saving::Unsaved;
        while let Some((job, reply)) = received.recv().await {
            let saving = &mut saving;
            let work = async move {
                match job {
                    SavingJob::Persist { record, started } => record.save(saving, &started).await,
                    SavingJob::Finish(record) => record.save_finished(saving).await,
                    SavingJob::Stop => *saving = Saving::Stopped,
                }
            };
            match reply {
                Some(reply) => super::answer(reply, work).await,
                None => work.await,
            }
        }
    })?;
    Ok(jobs)
}

fn start_saving_stored() -> std::io::Result<StoredJobs> {
    let (jobs, mut received) = mpsc::channel::<StoredJob>(STORED_SAVES_IN_FLIGHT);
    super::spawn_owner(async move {
        while let Some((record, stored, reply)) = received.recv().await {
            super::answer(reply, async move { record.write_stored(&stored).await }).await;
        }
    })?;
    Ok(jobs)
}

enum Saving {
    Unsaved,
    Saved {
        persisted: usize,
        next_seq: u64,
        calls: nanocode_sync::OrdMap<u64, u8>,
        observer_call_id: Option<String>,
        observer_yield_ms: Option<u64>,
        observer_since_ms: Option<u64>,
        content_returned: u64,
    },
    Stopped,
}

const fn call_mark(state: &JournaledCallState) -> u8 {
    match state {
        JournaledCallState::Queued => 0,
        JournaledCallState::Running => 1,
        JournaledCallState::Completed { .. } => 2,
    }
}

#[must_use]
pub struct PendingRestore {
    journals: CellJournals,
    session_id: String,
    ready: watch::Sender<bool>,
    waiting: Arc<AtomicUsize>,
}

impl PendingRestore {
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Acquire)
    }

    pub fn restore(
        self,
        cells: Vec<CellJournal>,
        fallback: Option<String>,
        outcomes: HashMap<String, (Value, bool)>,
    ) {
        self.journals
            .restore(&self.session_id, cells, fallback, outcomes);
    }
}

impl Drop for PendingRestore {
    fn drop(&mut self) {
        let mine = self.ready.subscribe();
        self.journals.0.pending.update(|pending| {
            if pending
                .get(&self.session_id)
                .is_some_and(|(waiting, _)| waiting.same_channel(&mine))
            {
                pending.remove(&self.session_id);
            }
        });
        self.ready.send_replace(true);
    }
}

#[derive(Clone)]
pub(super) struct Restored {
    pub(super) cells: Vec<CellJournal>,
    pub(super) fallback: Option<String>,
    pub(super) outcomes: HashMap<String, (Value, bool)>,
}

impl CellJournals {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_sink(sink: Arc<dyn CellJournalSink>) -> Self {
        Self(Arc::new(Journals {
            sink: Some(sink),
            ..Journals::default()
        }))
    }

    pub(super) fn persistence(&self) -> Option<Persistence> {
        self.0.sink.as_ref().map(|sink| Persistence {
            sink: Arc::clone(sink),
            saving: OnceCell::new(),
            saving_stored: Arc::clone(&self.0.saving_stored),
            queued: Inbox::default(),
        })
    }

    pub(super) async fn finished(&self, record: &Arc<CellRecord>) {
        record.update(|status| status.life = CellLife::Retired);
        if record.persistence.is_none() {
            return;
        }
        self.0
            .finished
            .update(|finished| finished.push(Arc::clone(record)));
        record.mark_finished().await;
    }

    pub(super) async fn release_finished(&self, session_id: &str) {
        self.forget_finished(session_id).await;
        let dormant = self.0.dormant.update(|dormant| {
            let (released, kept): (Vec<_>, Vec<_>) = std::mem::take(dormant)
                .into_iter()
                .partition(|(journal, _)| journal.session_id == session_id);
            *dormant = kept;
            released
        });
        if let Some(sink) = &self.0.sink {
            for (journal, _) in dormant {
                sink.forget(&journal.session_id, journal.cell_id).await;
            }
        }
    }

    pub(super) async fn forget_finished(&self, session_id: &str) {
        let released = self.0.finished.update(|finished| {
            let (released, kept): (Vec<_>, Vec<_>) = std::mem::take(finished)
                .into_iter()
                .partition(|record| record.session_id == session_id);
            *finished = kept;
            released
        });
        for record in released {
            record.forget_now().await;
        }
    }

    pub(super) async fn load(&self, session_id: &str) {
        let Some(sink) = &self.0.sink else {
            return;
        };
        let first = self
            .0
            .loaded
            .update(|loaded| loaded.insert(session_id.to_owned()));
        if !first {
            return;
        }
        match sink.load(session_id).await {
            Ok(cells) => self.restore(session_id, cells, None, HashMap::new()),
            Err(error) => nanocode_trace::warn!(
                target: "nanocode_code_mode",
                %error,
                "stored Code Mode cell journals could not be loaded"
            ),
        }
        let stored = sink.load_stored(session_id).await.and_then(|stored| {
            stored.map_or_else(
                || Ok(HashMap::new()),
                |stored| {
                    serde_json::from_str::<HashMap<String, Box<RawValue>>>(&stored)
                        .map_err(|error| error.to_string())
                },
            )
        });
        match stored {
            Ok(stored) if stored.is_empty() => {}
            Ok(stored) => {
                let stored = stored
                    .into_iter()
                    .map(|(key, value)| (key, Arc::from(value)))
                    .collect::<HashMap<_, _>>();
                self.0.stored.update(|sessions| {
                    sessions.insert(session_id.to_owned(), stored.clone());
                });
            }
            Err(error) => nanocode_trace::warn!(
                target: "nanocode_code_mode",
                %error,
                "stored Code Mode values could not be loaded"
            ),
        }
    }

    pub(super) fn take_stored(&self, session_id: &str) -> Option<HashMap<String, Arc<RawValue>>> {
        self.0
            .stored
            .try_update(|sessions| sessions.remove(session_id))
    }

    pub(super) async fn discard_restored(&self, session_id: &str) {
        let Some(restored) = self.take_restored(session_id) else {
            return;
        };
        if let Some(sink) = &self.0.sink {
            for journal in restored.cells {
                sink.forget(&journal.session_id, journal.cell_id).await;
            }
        }
    }

    pub(super) fn keep_dormant(&self, journal: CellJournal, restored: &Arc<Restored>) {
        let journal = Arc::new(journal);
        self.0
            .dormant
            .update(|dormant| dormant.push((Arc::clone(&journal), Arc::clone(restored))));
    }

    pub(super) fn take_dormant(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Option<(Arc<CellJournal>, Arc<Restored>)> {
        self.0.dormant.update(|dormant| {
            let index = dormant.iter().position(|(journal, _)| {
                journal.session_id == session_id
                    && (journal.origin_call_id == call_id
                        || journal.observer_call_id.as_deref() == Some(call_id))
            })?;
            Some(dormant.swap_remove(index))
        })
    }

    pub(super) fn register(&self, record: &Arc<CellRecord>) {
        let record = Arc::downgrade(record);
        self.0.cells.update(|cells| {
            cells.retain(|cell| cell.strong_count() > 0);
            cells.push(Weak::clone(&record));
        });
    }

    /// Let a steer cut short the call of `session_id` that a move cut and
    /// the runtime recovers (`CodeModeRuntime::recover_with_updates`), as it
    /// cuts any `wait` short. Off by default: a moved subagent's runtime
    /// steers it with its resume note at once, and that note must not cut
    /// its recovered `exec` short.
    pub fn preempt_recovery(&self, session_id: &str) {
        self.0
            .preempted
            .update(|preempted| preempted.insert(session_id.to_owned()));
    }

    /// Whether [`Self::preempt_recovery`] marked `session_id`.
    #[must_use]
    pub fn preempts_recovery(&self, session_id: &str) -> bool {
        self.0
            .preempted
            .read(|preempted| preempted.contains(session_id))
    }

    #[must_use]
    pub fn holding(&self, session_id: &str) -> bool {
        self.0.held.read(|held| held.contains(session_id))
    }

    fn live(&self, session_id: &str) -> Vec<Arc<CellRecord>> {
        self.0.cells.read(|cells| {
            cells
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|record| {
                    record.session_id == session_id
                        && record.status(|status| status.life != CellLife::Retired)
                })
                .collect()
        })
    }

    pub fn hold(&self, session_id: &str) {
        self.0
            .held
            .update(|held| held.insert(session_id.to_owned()));
        for record in self.live(session_id) {
            record.hold.send_replace(true);
        }
    }

    pub fn release(&self, session_id: &str) {
        self.0.held.update(|held| held.remove(session_id));
        for record in self.live(session_id) {
            record.hold.send_replace(false);
        }
    }

    #[must_use]
    pub fn state(&self, session_id: &str) -> CellJournalState {
        let mut state = CellJournalState {
            replayable: true,
            ..CellJournalState::default()
        };
        for record in self.live(session_id) {
            let overflowed = record.journal.overflowed();
            record.status(|status| {
                state.cells += 1;
                if status.life.settled() {
                    state.held += 1;
                }
                state.running_calls += status
                    .calls
                    .values()
                    .filter(|call| matches!(call.state, JournaledCallState::Running))
                    .count();
                state.replayable &= !status.unsupported && !overflowed;
            });
        }
        state
    }

    #[must_use]
    pub fn journals(&self, session_id: &str) -> Option<Vec<CellJournal>> {
        let mut journals = Vec::new();
        for record in self.live(session_id) {
            journals.push(record.snapshot()?);
        }
        journals.sort_by_key(|journal| journal.cell_id);
        Some(journals)
    }

    #[must_use]
    pub fn live_journals(&self, session_id: &str) -> Vec<CellJournal> {
        let mut journals = self
            .live(session_id)
            .iter()
            .filter_map(|record| record.snapshot_live())
            .collect::<Vec<_>>();
        journals.sort_by_key(|journal| journal.cell_id);
        journals
    }

    pub fn restore(
        &self,
        session_id: &str,
        cells: Vec<CellJournal>,
        fallback: Option<String>,
        outcomes: HashMap<String, (Value, bool)>,
    ) {
        if cells.is_empty() {
            return;
        }
        let restored = Arc::new(Restored {
            cells,
            fallback,
            outcomes,
        });
        self.0.restored.update(|map| {
            map.insert(session_id.to_owned(), Arc::clone(&restored));
        });
    }

    pub fn restore_sessions(&self, sessions: impl IntoIterator<Item = (String, Vec<CellJournal>)>) {
        let restored = sessions
            .into_iter()
            .filter(|(_, cells)| !cells.is_empty())
            .map(|(session_id, cells)| {
                let restored = Restored {
                    cells,
                    fallback: None,
                    outcomes: HashMap::new(),
                };
                (session_id, Arc::new(restored))
            })
            .collect::<Vec<_>>();
        if restored.is_empty() {
            return;
        }
        self.0.restored.update(|map| {
            map.extend(
                restored
                    .iter()
                    .map(|(session_id, restored)| (session_id.clone(), Arc::clone(restored))),
            );
        });
    }

    pub fn expect_restore(&self, session_id: &str) -> PendingRestore {
        let (ready, receiver) = watch::channel(false);
        let waiting = Arc::new(AtomicUsize::new(0));
        self.0.pending.update(|pending| {
            pending.insert(
                session_id.to_owned(),
                (receiver.clone(), Arc::clone(&waiting)),
            );
        });
        PendingRestore {
            journals: self.clone(),
            session_id: session_id.to_owned(),
            ready,
            waiting,
        }
    }

    #[must_use]
    pub fn restore_pending(&self, session_id: &str) -> bool {
        self.0
            .pending
            .read(|pending| pending.contains_key(session_id))
    }

    pub async fn await_restore(&self, session_id: &str) {
        let Some((mut receiver, waiting)) = self
            .0
            .pending
            .read(|pending| pending.get(session_id).cloned())
        else {
            return;
        };
        waiting.fetch_add(1, Ordering::AcqRel);
        drop(receiver.wait_for(|ready| *ready).await);
    }

    #[must_use]
    pub fn has_restored(&self, session_id: &str) -> bool {
        self.0.restored.read(|map| map.contains_key(session_id))
    }

    #[must_use]
    pub fn hold_for_handoff(&self, session_id: &str, replayable: bool, forced: bool) -> CellHold {
        let state = self.state(session_id);
        if !replayable || state.cells == 0 || !state.replayable {
            self.release_held(session_id);
            return CellHold::None;
        }
        if !self.holding(session_id) {
            self.hold(session_id);
        }
        if self.state(session_id).settled() {
            CellHold::Replay
        } else if forced {
            CellHold::None
        } else {
            CellHold::Wait
        }
    }

    pub fn release_held(&self, session_id: &str) {
        if self.holding(session_id) {
            self.release(session_id);
        }
    }

    #[must_use]
    pub fn moved(&self, session_id: &str, request_id: &str, run: u32) -> MovedCells {
        MovedCells {
            turn: request_id.to_owned(),
            run,
            cells: self.journals(session_id).unwrap_or_default(),
        }
    }

    pub fn restore_moved(
        &self,
        session_id: &str,
        moved: MovedCells,
        request_id: &str,
        run: u32,
        fallback: Option<String>,
        outcome: impl Fn(&JournaledCall) -> Option<(Value, bool)>,
    ) -> usize {
        if moved.turn != request_id
            || moved.run.checked_add(1) != Some(run)
            || moved.cells.is_empty()
        {
            return 0;
        }
        let outcomes = moved
            .cells
            .iter()
            .flat_map(CellJournal::unknown_calls)
            .filter_map(|call| Some((call.call_id.clone(), outcome(call)?)))
            .collect();
        let restored = moved.cells.len();
        self.restore(session_id, moved.cells, fallback, outcomes);
        restored
    }

    pub(super) fn take_restored(&self, session_id: &str) -> Option<Restored> {
        self.0
            .restored
            .try_update(|map| map.remove(session_id))
            .map(Arc::unwrap_or_clone)
    }
}

pub(super) struct CellRecord {
    pub(super) cell_id: u64,
    pub(super) session_id: String,
    pub(super) origin_call_id: String,
    pub(super) script: String,
    pub(super) tools: Arc<[Value]>,
    pub(super) stored: HashMap<String, Arc<RawValue>>,
    pub(super) started_at_ms: Option<u64>,
    pub(super) prelude: bool,
    pub(super) yield_time_ms: u64,
    pub(super) max_output_tokens: Option<usize>,
    pub(super) journal: SharedJournal,
    pub(super) status: Atom<CellStatus>,
    pub(super) hold: watch::Sender<bool>,
    pub(super) persistence: Option<Persistence>,
}

#[derive(Clone)]
pub(super) struct CellStatus {
    pub(super) observer_call_id: Option<String>,
    pub(super) observer_yield_ms: Option<u64>,
    pub(super) observer_since_ms: Option<u64>,
    pub(super) life: CellLife,
    pub(super) unsupported: bool,
    pub(super) content_returned: u64,
    pub(super) calls: nanocode_sync::OrdMap<u64, Arc<JournaledCall>>,
}

impl CellStatus {
    pub(super) const fn new(life: CellLife) -> Self {
        Self {
            observer_call_id: None,
            observer_yield_ms: None,
            observer_since_ms: None,
            life,
            unsupported: false,
            content_returned: 0,
            calls: nanocode_sync::OrdMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CellLife {
    Active { replay: Replay, hold: Hold },
    Finished,
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Replay {
    Replaying,
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hold {
    Running,
    Held,
}

impl CellLife {
    pub(super) const fn started(replay: Replay) -> Self {
        Self::Active {
            replay,
            hold: Hold::Running,
        }
    }

    #[must_use]
    pub(super) const fn holding(self, hold: Hold) -> Self {
        match self {
            Self::Active { replay, .. } => Self::Active { replay, hold },
            ended => ended,
        }
    }

    #[must_use]
    pub(super) const fn replayed(self) -> Self {
        match self {
            Self::Active { hold, .. } => Self::Active {
                replay: Replay::Live,
                hold,
            },
            ended => ended,
        }
    }

    #[must_use]
    pub(super) const fn finished(self) -> Self {
        match self {
            Self::Active { .. } => Self::Finished,
            ended => ended,
        }
    }

    pub(super) const fn replaying(self) -> bool {
        matches!(
            self,
            Self::Active {
                replay: Replay::Replaying,
                ..
            }
        )
    }

    pub(super) const fn settled(self) -> bool {
        matches!(
            self,
            Self::Active {
                hold: Hold::Held,
                ..
            } | Self::Finished
        )
    }
}

pub(super) struct ReplayPlan {
    pub(super) entries: Vec<JournalEntry>,
    pub(super) calls: Vec<JournaledCall>,
    pub(super) fallback: Option<String>,
    pub(super) outcomes: HashMap<String, (Value, bool)>,
    pub(super) content_returned: u64,
}

impl ReplayPlan {
    pub(super) fn outcome(&self, call: &JournaledCall) -> (String, bool) {
        match &call.state {
            JournaledCallState::Completed { encoded, success } => (encoded.clone(), *success),
            JournaledCallState::Running | JournaledCallState::Queued => {
                let (value, success) = self
                    .outcomes
                    .get(&call.call_id)
                    .cloned()
                    .unwrap_or_else(|| (unknown_outcome(), false));
                (value.to_string(), success)
            }
        }
    }

    pub(super) fn divergence(&self, cell_id: u64, reason: &str) -> String {
        divergence_text(self.fallback.as_deref(), cell_id, reason)
    }
}

pub(super) fn wall_ms() -> u64 {
    nanocode_platform::system_time()
        .duration_since(nanocode_platform::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(super) fn unknown_outcome() -> Value {
    serde_json::json!({
        "error": UNKNOWN_OUTCOME,
        "code": "CODE_MODE_CALL_INTERRUPTED",
        "outcome": "unknown",
    })
}

fn stored_values<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Value>, D::Error> {
    BTreeMap::<String, Box<RawValue>>::deserialize(deserializer)?
        .into_iter()
        .map(|(key, value)| {
            serde_json::from_str(value.get())
                .map(|value| (key, value))
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

pub(super) fn divergence_text(fallback: Option<&str>, cell_id: u64, reason: &str) -> String {
    fallback.map_or_else(
        || {
            format!(
                "This cell moved to another host and could not be resumed ({reason}), so it stopped and its outcome is unknown: nested calls it made may have run. Cell {cell_id} no longer exists: don't wait on it again."
            )
        },
        str::to_owned,
    )
}

impl CellRecord {
    pub(super) fn status<R>(&self, read: impl FnOnce(&CellStatus) -> R) -> R {
        self.status.read(read)
    }

    pub(super) fn update<R>(&self, change: impl FnMut(&mut CellStatus) -> R) -> R {
        self.status.update(change)
    }

    fn stored_json(&self) -> BTreeMap<String, Value> {
        self.stored
            .iter()
            .filter_map(|(key, value)| {
                serde_json::from_str(value.get())
                    .ok()
                    .map(|value| (key.clone(), value))
            })
            .collect()
    }

    pub(super) async fn persist(self: &Arc<Self>, calls: Vec<JournaledCall>) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        let started = calls
            .iter()
            .map(|call| (call.id, call_mark(&call.state)))
            .collect::<Vec<_>>();
        for call in calls {
            persistence.queued.push(Arc::new(call));
        }
        let saved = persistence
            .save(SavingJob::Persist {
                record: Arc::clone(self),
                started,
            })
            .await;
        if let Err(error) = saved {
            self.unsaved(&error);
        }
    }

    pub(super) fn queue_persist(self: &Arc<Self>) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        let queued = persistence.queue(SavingJob::Persist {
            record: Arc::clone(self),
            started: Vec::new(),
        });
        if let Err(error) = queued {
            self.unsaved(&error);
        }
    }

    fn unsaved(&self, error: &dyn std::fmt::Display) {
        nanocode_trace::warn!(
            target: "nanocode_code_mode",
            cell_id = self.cell_id,
            %error,
            "a Code Mode cell journal could not be saved, so the cell cannot be resumed after a restart"
        );
    }

    async fn save(&self, saving: &mut Saving, started: &[(u64, u8)]) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        if let Saving::Saved { calls: saved, .. } = &*saving
            && !started.is_empty()
            && started
                .iter()
                .all(|(id, mark)| saved.get(id).is_some_and(|saved| saved >= mark))
        {
            return;
        }
        let mut latest = match &*saving {
            Saving::Unsaved => self.status(|status| {
                status
                    .calls
                    .iter()
                    .map(|(id, call)| (*id, Arc::clone(call)))
                    .collect::<BTreeMap<_, _>>()
            }),
            _ => BTreeMap::new(),
        };
        for call in persistence.queued.drain() {
            match latest.get(&call.id) {
                Some(queued) if call_mark(&queued.state) > call_mark(&call.state) => {}
                _ => drop(latest.insert(call.id, call)),
            }
        }
        let calls: Vec<_> = latest.into_values().collect();
        let (from, seq) = match &*saving {
            Saving::Stopped => return,
            Saving::Unsaved => (0, 0),
            Saving::Saved {
                persisted,
                next_seq,
                ..
            } => (*persisted, *next_seq),
        };
        if self.status(|status| status.life.replaying()) {
            return;
        }
        let entries = self.journal.entries_from(from);
        let Some(entries) = entries else {
            *saving = Saving::Stopped;
            persistence
                .sink
                .forget(&self.session_id, self.cell_id)
                .await;
            return;
        };
        let (observer_call_id, observer_yield_ms, observer_since_ms, content_returned) = self
            .status(|status| {
                (
                    status.observer_call_id.clone(),
                    status.observer_yield_ms,
                    status.observer_since_ms,
                    status.content_returned,
                )
            });
        let mut marks = match &*saving {
            Saving::Saved {
                calls: saved,
                observer_call_id: saved_observer,
                observer_yield_ms: saved_yield,
                observer_since_ms: saved_since,
                content_returned: saved_content,
                ..
            } => {
                if entries.is_empty()
                    && *saved_observer == observer_call_id
                    && *saved_yield == observer_yield_ms
                    && *saved_since == observer_since_ms
                    && *saved_content == content_returned
                    && calls
                        .iter()
                        .all(|call| saved.get(&call.id) == Some(&call_mark(&call.state)))
                {
                    return;
                }
                saved.clone()
            }
            _ => nanocode_sync::OrdMap::new(),
        };
        marks.extend(calls.iter().map(|call| (call.id, call_mark(&call.state))));
        let header = matches!(*saving, Saving::Unsaved).then(|| CellJournalHeader {
            version: CELL_JOURNAL_VERSION,
            origin_call_id: self.origin_call_id.clone(),
            script: self.script.clone(),
            tools: Arc::clone(&self.tools),
            stored: self.stored_json(),
            started_at_ms: self.started_at_ms,
            prelude: self.prelude,
            yield_time_ms: self.yield_time_ms,
            max_output_tokens: self.max_output_tokens,
        });
        let persisted = from + entries.len();
        let update = CellJournalUpdate {
            session_id: self.session_id.clone(),
            cell_id: self.cell_id,
            seq,
            header,
            entries_from: from,
            entries,
            calls,
            observer_call_id: observer_call_id.clone(),
            observer_yield_ms,
            observer_since_ms,
            content_returned,
            finished: false,
        };
        match persistence.sink.save(update).await {
            Ok(()) => {
                *saving = Saving::Saved {
                    persisted,
                    next_seq: seq + 1,
                    calls: marks,
                    observer_call_id,
                    observer_yield_ms,
                    observer_since_ms,
                    content_returned,
                };
            }
            Err(error) => {
                self.unsaved(&error);
                *saving = Saving::Stopped;
                persistence
                    .sink
                    .forget(&self.session_id, self.cell_id)
                    .await;
            }
        }
    }

    pub(super) async fn save_stored(
        self: &Arc<Self>,
        stored: &Arc<Atom<HashMap<String, Arc<RawValue>>>>,
    ) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        let saved = persistence
            .save_stored(Arc::clone(self), Arc::clone(stored))
            .await;
        if let Err(error) = saved {
            nanocode_trace::warn!(
                target: "nanocode_code_mode",
                cell_id = self.cell_id,
                %error,
                "Code Mode stored values could not be saved, so load() after a restart reads the values saved before"
            );
        }
    }

    async fn write_stored(
        &self,
        stored: &Atom<HashMap<String, Arc<RawValue>>>,
    ) -> Result<(), String> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let values = stored.get();
        let encoded = serde_json::to_string(
            &values
                .iter()
                .map(|(key, value)| (key.as_str(), &**value))
                .collect::<BTreeMap<_, _>>(),
        );
        match encoded {
            Ok(encoded) => {
                persistence
                    .sink
                    .save_stored(&self.session_id, encoded)
                    .await
            }
            Err(error) => Err(error.to_string()),
        }
    }

    pub(super) fn journal(&self, id: u64) {
        if let Some(persistence) = &self.persistence
            && let Some(call) = self.status(|status| status.calls.get(&id).cloned())
        {
            persistence.queued.push(call);
        }
    }

    pub(super) async fn flush(self: &Arc<Self>) {
        self.persist(Vec::new()).await;
    }

    async fn mark_finished(self: &Arc<Self>) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        let marked = persistence.save(SavingJob::Finish(Arc::clone(self))).await;
        if let Err(error) = marked {
            self.unfinished(&error);
        }
    }

    fn unfinished(&self, error: &dyn std::fmt::Display) {
        nanocode_trace::warn!(
            target: "nanocode_code_mode",
            cell_id = self.cell_id,
            %error,
            "a finished Code Mode cell's journal could not be marked finished"
        );
    }

    async fn save_finished(&self, saving: &mut Saving) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        let Saving::Saved {
            persisted,
            next_seq,
            observer_call_id,
            observer_yield_ms,
            observer_since_ms,
            content_returned,
            ..
        } = &*saving
        else {
            return;
        };
        let Some(entries) = self.journal.entries_from(*persisted) else {
            return;
        };
        let update = CellJournalUpdate {
            session_id: self.session_id.clone(),
            cell_id: self.cell_id,
            seq: *next_seq,
            header: None,
            entries_from: *persisted,
            entries,
            calls: Vec::new(),
            observer_call_id: observer_call_id.clone(),
            observer_yield_ms: *observer_yield_ms,
            observer_since_ms: *observer_since_ms,
            content_returned: *content_returned,
            finished: true,
        };
        if let Err(error) = persistence.sink.save(update).await {
            self.unfinished(&error);
        }
        *saving = Saving::Stopped;
    }

    async fn forget_now(&self) {
        let Some(persistence) = &self.persistence else {
            return;
        };
        if let Err(error) = persistence.stop().await {
            self.unsaved(&error);
        }
        persistence
            .sink
            .forget(&self.session_id, self.cell_id)
            .await;
    }

    pub(super) fn forget(self: &Arc<Self>) {
        if self.persistence.is_none() {
            return;
        }
        let record = Arc::clone(self);
        super::spawn(async move {
            let Some(persistence) = &record.persistence else {
                return;
            };
            if let Err(error) = persistence.stop().await {
                record.unsaved(&error);
            }
            persistence
                .sink
                .forget(&record.session_id, record.cell_id)
                .await;
        });
    }

    fn snapshot(&self) -> Option<CellJournal> {
        self.snapshot_when(CellLife::settled)
    }

    fn snapshot_live(&self) -> Option<CellJournal> {
        self.snapshot_when(|life| !life.replaying())
    }

    fn snapshot_when(&self, ready: impl FnOnce(CellLife) -> bool) -> Option<CellJournal> {
        let entries = self.journal.entries()?;
        let status = self.status.get();
        if status.unsupported || !ready(status.life) {
            return None;
        }
        Some(CellJournal {
            version: CELL_JOURNAL_VERSION,
            cell_id: self.cell_id,
            session_id: self.session_id.clone(),
            origin_call_id: self.origin_call_id.clone(),
            observer_call_id: status.observer_call_id.clone(),
            observer_yield_ms: status.observer_yield_ms,
            observer_since_ms: status.observer_since_ms,
            script: self.script.clone(),
            tools: Arc::clone(&self.tools),
            stored: self
                .stored
                .iter()
                .filter_map(|(key, value)| {
                    serde_json::from_str(value.get())
                        .ok()
                        .map(|value| (key.clone(), value))
                })
                .collect(),
            started_at_ms: self.started_at_ms,
            prelude: self.prelude,
            yield_time_ms: self.yield_time_ms,
            max_output_tokens: self.max_output_tokens,
            content_returned: status.content_returned,
            entries,
            calls: status
                .calls
                .values()
                .map(|call| JournaledCall::clone(call))
                .collect(),
            finished: false,
        })
    }
}
