use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use nanocode_board::access::{Identity, Verifier};
use nanocode_board::{
    Applied, Board, BoardError, Change, ClaimId, EventId, NewTask, Op, Outcome, Phase, Rejection,
    TaskId,
};
use nanocode_sync::Mailbox;
use nanocode_turnlog::LogHost;

use crate::model::{AgentId, AgentStatus, AgentUpdate, MessageSender};

pub trait Coordination: Send + Sync {
    fn observe(&self, root_session_id: &str, update: &AgentUpdate);
}

pub const LEASE_MS: u64 = 1 << 30;
pub const PRINCIPAL: &str = "nanocode:subagents";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardStats {
    pub posts: u64,
    pub claims: u64,
    pub reclaims: u64,
    pub takeovers: u64,
    pub finishes: u64,
    pub finish_replays: u64,
    pub reuses: u64,
    pub replies: u64,
    pub steers: u64,
    pub cancels: u64,
    pub fenced: u64,
    pub late: u64,
    pub deferred: u64,
}

impl BoardStats {
    pub const fn add(&mut self, other: &Self) {
        self.posts += other.posts;
        self.claims += other.claims;
        self.reclaims += other.reclaims;
        self.takeovers += other.takeovers;
        self.finishes += other.finishes;
        self.finish_replays += other.finish_replays;
        self.reuses += other.reuses;
        self.replies += other.replies;
        self.steers += other.steers;
        self.cancels += other.cancels;
        self.fenced += other.fenced;
        self.late += other.late;
        self.deferred += other.deferred;
    }
}

struct Named;

impl Verifier for Named {
    fn verify(&self, credential: &str) -> Option<String> {
        Some(credential.to_owned())
    }
}

pub fn profile_of(session: &str) -> String {
    let plain = !session.is_empty()
        && !session.starts_with("x-")
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
    if plain {
        return session.to_owned();
    }
    let mut hex = String::from("x-");
    for byte in session.bytes() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

pub fn worker_of(session: &str) -> String {
    format!("{PRINCIPAL}+{}", profile_of(session))
}

fn identity(profile: Option<&str>) -> Result<Identity, String> {
    let owner = Identity::verify(&Named, PRINCIPAL).map_err(|denied| format!("{denied:?}"))?;
    match profile {
        Some(profile) => owner.profile(profile).map_err(|denied| format!("{denied:?}")),
        None => Ok(owner),
    }
}

struct Session {
    parent: TaskId,
    k: u64,
    claim: Option<ClaimId>,
    last_output: Option<Option<String>>,
}

impl Session {
    fn task(&self, session: &str) -> TaskId {
        task_of(session, self.k)
    }
}

fn task_of(session: &str, k: u64) -> TaskId {
    TaskId(format!("{session}/{k}"))
}

fn root_task(root: &str) -> TaskId {
    TaskId(format!("root:{root}"))
}

struct Inner {
    host: Arc<dyn LogHost>,
    scope: String,
    clock: u64,
    dispatcher: Option<Board>,
    workers: BTreeMap<String, Board>,
    sessions: BTreeMap<String, Session>,
    agents: BTreeMap<(String, AgentId), String>,
    roots: BTreeSet<String>,
    stats: BoardStats,
    errors: Vec<String>,
}

pub struct BoardCoordination {
    inner: Mailbox<Inner>,
}

fn ready<F: Future>(future: F) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    }
}

fn open(host: &Arc<dyn LogHost>, scope: &str, profile: Option<&str>) -> Result<Option<Board>, String> {
    let identity = identity(profile)?;
    match ready(Board::open(Arc::clone(host), scope, identity)) {
        Some(Ok(board)) => Ok(Some(board)),
        Some(Err(error)) => Err(error.to_string()),
        None => Ok(None),
    }
}

impl BoardCoordination {
    pub fn new(host: Arc<dyn LogHost>, scope: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: Mailbox::new(Inner {
                host,
                scope: scope.to_owned(),
                clock: 0,
                dispatcher: None,
                workers: BTreeMap::new(),
                sessions: BTreeMap::new(),
                agents: BTreeMap::new(),
                roots: BTreeSet::new(),
                stats: BoardStats::default(),
                errors: Vec::new(),
            }),
        })
    }

    pub async fn stats(&self) -> BoardStats {
        self.inner.call(|inner| inner.stats()).await
    }

    pub async fn errors(&self) -> Vec<String> {
        self.inner.call(|inner| inner.errors()).await
    }

    pub async fn terminal_after(&self, cursor: u64) -> (Vec<(TaskId, Phase)>, u64) {
        self.inner.call(move |inner| inner.terminal_after(cursor)).await
    }

    pub async fn children(&self, parent: &str) -> Vec<String> {
        let parent = parent.to_owned();
        self.inner.call(move |inner| inner.children(&parent)).await
    }

    pub async fn audit(&self) -> Result<BoardStats, String> {
        self.inner.call(|inner| inner.audit()).await
    }
}

impl Inner {
    fn stats(&self) -> BoardStats {
        self.stats.clone()
    }

    fn errors(&self) -> Vec<String> {
        self.errors.clone()
    }

    fn terminal_after(&mut self, cursor: u64) -> (Vec<(TaskId, Phase)>, u64) {
        let Some(board) = self.dispatcher_board() else {
            return (Vec::new(), cursor);
        };
        let _ = ready(board.refresh());
        let fold = board.fold();
        let terminal = fold
            .entries()
            .iter()
            .filter(|entry| entry.seq > cursor && entry.change.terminal())
            .filter_map(|entry| {
                fold.task(&entry.task)
                    .map(|task| (entry.task.clone(), task.phase.clone()))
            })
            .collect();
        (terminal, fold.next().saturating_sub(1))
    }

    fn children(&mut self, parent: &str) -> Vec<String> {
        let Some(board) = self.dispatcher_board() else {
            return Vec::new();
        };
        let _ = ready(board.refresh());
        let prefix = format!("{parent}/");
        let root = root_task(parent);
        board
            .fold()
            .tasks()
            .iter()
            .filter(|(id, task)| {
                id.0.ends_with("/1")
                    && task
                        .spec
                        .parent
                        .as_ref()
                        .is_some_and(|up| up.0.starts_with(&prefix) || *up == root)
            })
            .filter_map(|(_, task)| task.spec.labels.get("session").cloned())
            .collect()
    }

    fn audit(&self) -> Result<BoardStats, String> {
        if let Some(error) = self.errors.first() {
            return Err(format!("board coordination error: {error}"));
        }
        let board = match open(&self.host, &self.scope, Some("auditor")) {
            Ok(Some(board)) => board,
            Ok(None) => return Err("board audit open is pending".to_owned()),
            Err(error) => return Err(format!("board audit open: {error}")),
        };
        let fold = board.fold();
        if fold.access().owner() != Some(PRINCIPAL) && !fold.tasks().is_empty() {
            return Err(format!("board owner is {:?}", fold.access().owner()));
        }
        for entry in fold.entries() {
            if let Change::Claimed { worker, .. } = &entry.change {
                let session = fold
                    .task(&entry.task)
                    .and_then(|task| task.spec.labels.get("session"));
                if session.map(|session| worker_of(session)).as_ref() != Some(worker) {
                    return Err(format!(
                        "board claim of {} by {worker}, which is not its session {session:?}",
                        entry.task
                    ));
                }
            }
        }
        for (id, task) in fold.tasks() {
            if let Some(parent) = &task.spec.parent
                && fold.task(parent).is_none()
            {
                return Err(format!("board task {id} has no parent {parent}"));
            }
        }
        for (session, state) in &self.sessions {
            let Some(expected) = &state.last_output else {
                continue;
            };
            let id = state.task(session);
            let phase = fold.task(&id).map(|task| &task.phase);
            let matches = match (expected, phase) {
                (Some(output), Some(Phase::Done { result })) => result == output,
                (None, Some(Phase::Failed { .. })) => true,
                _ => false,
            };
            if !matches {
                return Err(format!(
                    "board task {id} is {phase:?}, but the registry reported {expected:?}"
                ));
            }
        }
        Ok(self.stats.clone())
    }
}

impl Inner {
    const fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn fail(&mut self, error: String) {
        if self.errors.len() < 16 {
            self.errors.push(error);
        }
    }

    fn dispatcher_board(&mut self) -> Option<&mut Board> {
        if self.dispatcher.is_none() {
            match open(&self.host, &self.scope, None) {
                Ok(Some(mut board)) => {
                    let now = board.fold().now_ms();
                    let start = if now > 0 { now + LEASE_MS + 1 } else { 0 };
                    self.clock = self.clock.max(start) + 1;
                    if board.fold().access().owner().is_none() {
                        match ready(board.charter(self.clock)) {
                            Some(Ok(_) | Err(BoardError::Rejected(Rejection::Exists))) => {}
                            Some(Err(error)) => {
                                self.fail(format!("charter: {error}"));
                                return None;
                            }
                            None => {
                                self.stats.deferred += 1;
                                return None;
                            }
                        }
                    }
                    self.dispatcher = Some(board);
                }
                Ok(None) => {
                    self.stats.deferred += 1;
                    return None;
                }
                Err(error) => {
                    self.fail(format!("open dispatcher: {error}"));
                    return None;
                }
            }
        }
        self.dispatcher.as_mut()
    }

    fn worker(&mut self, session: &str) -> Option<&mut Board> {
        if !self.workers.contains_key(session) {
            match open(&self.host, &self.scope, Some(&profile_of(session))) {
                Ok(Some(board)) => {
                    self.workers.insert(session.to_owned(), board);
                }
                Ok(None) => {
                    self.stats.deferred += 1;
                    return None;
                }
                Err(error) => {
                    self.fail(format!("open worker {session}: {error}"));
                    return None;
                }
            }
        }
        self.workers.get_mut(session)
    }

    fn dispatch(&mut self, key: &str, op: Op) -> Option<Result<(Applied, bool), BoardError>> {
        let at = self.tick();
        let board = self.dispatcher_board()?;
        let _ = ready(board.refresh());
        let before = board.fold().next();
        if let Some(result) = ready(board.submit_keyed(key, at, op)) {
            return Some(result.map(|applied| {
                let replay = applied.seq < before;
                (applied, replay)
            }));
        }
        self.stats.deferred += 1;
        None
    }

    fn phase(&mut self, task: &TaskId) -> Option<Phase> {
        let board = self.dispatcher_board()?;
        let _ = ready(board.refresh());
        board.fold().task(task).map(|task| task.phase.clone())
    }

    fn ensure_root(&mut self, root: &str) -> TaskId {
        let id = root_task(root);
        if self.roots.insert(root.to_owned()) {
            let task = NewTask {
                id: id.clone(),
                parent: None,
                spec: format!("root session {root}"),
                labels: BTreeMap::from([("root".to_owned(), root.to_owned())]),
                needs: Vec::new(),
            };
            match self.dispatch(&format!("post:{id}"), Op::Post { task }) {
                Some(Ok((_, false))) => self.stats.posts += 1,
                Some(Ok((_, true))) | None => {}
                Some(Err(error)) => self.fail(format!("post {id}: {error}")),
            }
        }
        id
    }

    fn post(&mut self, root: &str, session: &str, agent: AgentId, parent: &TaskId, k: u64, spec: &str) {
        let id = task_of(session, k);
        let task = NewTask {
            id: id.clone(),
            parent: Some(parent.clone()),
            spec: spec.to_owned(),
            labels: BTreeMap::from([
                ("session".to_owned(), session.to_owned()),
                ("root".to_owned(), root.to_owned()),
                ("agent".to_owned(), agent.to_string()),
            ]),
            needs: Vec::new(),
        };
        match self.dispatch(&format!("post:{id}"), Op::Post { task }) {
            Some(Ok((_, false))) => self.stats.posts += 1,
            Some(Ok((_, true))) | None => {}
            Some(Err(error)) => self.fail(format!("post {id}: {error}")),
        }
    }

    fn claim(&mut self, session: &str, restored: bool) {
        let Some(state) = self.sessions.get(session) else {
            return;
        };
        let id = state.task(session);
        let at = self.tick();
        let worker = worker_of(session);
        let Some(board) = self.worker(session) else {
            return;
        };
        let _ = ready(board.refresh());
        let (phase, attempts) = board
            .fold()
            .task(&id)
            .map(|task| (task.phase.clone(), task.attempts))
            .unzip();
        let mut claims = 0;
        let mut took_over = false;
        let mut failure = None;
        let claim = match phase {
            Some(Phase::Claimed {
                claim,
                worker: holder,
                until_ms,
            }) if holder == worker && until_ms > at => Some(claim),
            Some(phase) if phase.terminal() => None,
            Some(_) => {
                let key = format!("claim:{id}:{}", attempts.unwrap_or(0) + 1);
                match ready(board.submit_keyed(&key, at, Op::Claim {
                    task: id.clone(),
                    lease_ms: LEASE_MS,
                })) {
                    Some(Ok(_)) => {
                        claims = 1;
                        board.fold().entries().iter().rev().find_map(|entry| match &entry.change {
                            Change::Claimed {
                                claim,
                                took_over: over,
                                worker: holder,
                                ..
                            } if entry.task == id && *holder == worker => {
                                took_over = over.is_some();
                                Some(claim.clone())
                            }
                            _ => None,
                        })
                    }
                    Some(Err(error)) => {
                        failure = Some(format!("claim {id} for {session}: {error}"));
                        None
                    }
                    None => {
                        failure = Some(format!("claim {id} for {session} is pending"));
                        None
                    }
                }
            }
            None => {
                failure = Some(format!("claim {id}: the task was never posted"));
                None
            }
        };
        if restored && claim.is_some() {
            self.stats.reclaims += 1;
        }
        self.stats.claims += claims;
        if took_over {
            self.stats.takeovers += 1;
        }
        if let Some(failure) = failure {
            self.fail(failure);
        }
        if let Some(state) = self.sessions.get_mut(session) {
            state.claim = claim;
        }
    }

    fn added(&mut self, root: &str, id: AgentId, session: &str, parent: Option<AgentId>, spec: &str) {
        self.agents.insert((root.to_owned(), id), session.to_owned());
        if self.sessions.contains_key(session) {
            return;
        }
        let root_id = self.ensure_root(root);
        let parent_task = parent
            .and_then(|parent| self.agents.get(&(root.to_owned(), parent)).cloned())
            .and_then(|parent| self.sessions.get(&parent).map(|state| state.task(&parent)))
            .unwrap_or(root_id);
        let mut k = 0;
        if let Some(board) = self.dispatcher_board() {
            let _ = ready(board.refresh());
            while board.fold().task(&task_of(session, k + 1)).is_some() {
                k += 1;
            }
        }
        let restored = k > 0;
        if !restored {
            k = 1;
            self.post(root, session, id, &parent_task, k, spec);
        }
        self.sessions.insert(
            session.to_owned(),
            Session {
                parent: parent_task,
                k,
                claim: None,
                last_output: None,
            },
        );
        self.claim(session, restored);
    }

    fn session_of(&self, root: &str, id: AgentId) -> Option<String> {
        self.agents.get(&(root.to_owned(), id)).cloned()
    }

    fn running(&mut self, root: &str, id: AgentId, session: &str) {
        let Some(state) = self.sessions.get(session) else {
            return;
        };
        let current = state.task(session);
        let parent = state.parent.clone();
        let k = state.k;
        match self.phase(&current) {
            Some(Phase::Cancelled { .. }) => {
                self.stats.late += 1;
                return;
            }
            Some(phase) if phase.terminal() => {}
            _ => return,
        }
        self.post(root, session, id, &parent, k + 1, "reused");
        self.stats.reuses += 1;
        if let Some(state) = self.sessions.get_mut(session) {
            state.k = k + 1;
            state.claim = None;
            state.last_output = None;
        }
        self.claim(session, false);
    }

    fn finish(&mut self, root: &str, id: AgentId, session: &str, status: &AgentStatus, revision: Option<u64>) {
        let outcome = match status {
            AgentStatus::Completed { output } => Outcome::Done(output.to_string()),
            AgentStatus::Failed { error } => Outcome::Failed(error.clone()),
            _ => return,
        };
        let key = match revision {
            Some(revision) => format!("finish:{session}:{revision}"),
            None => format!(
                "finish:{session}:k{}",
                self.sessions.get(session).map_or(0, |state| state.k)
            ),
        };
        let event = EventId(format!("{}#{key}", worker_of(session)));
        let replayed = self.dispatcher_board().is_some_and(|board| {
            let _ = ready(board.refresh());
            board.fold().outcome(&event).is_some()
        });
        if replayed {
            self.stats.finish_replays += 1;
            return;
        }
        if let Some(current) = self.sessions.get(session).map(|state| state.task(session))
            && matches!(self.phase(&current), Some(Phase::Cancelled { .. }))
        {
            self.stats.late += 1;
            return;
        }
        self.running(root, id, session);
        if self.sessions.get(session).is_some_and(|state| state.claim.is_none()) {
            self.claim(session, false);
        }
        let Some(state) = self.sessions.get(session) else {
            return;
        };
        let task = state.task(session);
        let Some(claim) = state.claim.clone() else {
            let phase = self.phase(&task);
            self.fail(format!("finish {task} ({key}): {session} holds no claim, the task is {phase:?}"));
            return;
        };
        let last = match &outcome {
            Outcome::Done(output) => Some(output.clone()),
            Outcome::Failed(_) => None,
        };
        let at = self.tick();
        let Some(board) = self.worker(session) else {
            return;
        };
        let _ = ready(board.refresh());
        let before = board.fold().next();
        let submitted = ready(board.submit_keyed(&key, at, Op::Finish {
            task: task.clone(),
            claim,
            outcome,
        }));
        match submitted {
            Some(Ok(applied)) => {
                if applied.seq < before {
                    self.stats.finish_replays += 1;
                } else {
                    self.stats.finishes += 1;
                }
                if let Some(state) = self.sessions.get_mut(session) {
                    state.last_output = Some(last);
                    state.claim = None;
                }
            }
            Some(Err(BoardError::Rejected(Rejection::NotHolder))) => {
                self.stats.fenced += 1;
                self.fail(format!("finish {task}: fenced, {session} no longer holds the claim"));
            }
            Some(Err(error)) => self.fail(format!("finish {task} ({key}): {error}")),
            None => self.stats.deferred += 1,
        }
    }

    fn reply(&mut self, session: &str, key: &str, body: String, steer: bool) {
        let Some(state) = self.sessions.get(session) else {
            return;
        };
        let task = state.task(session);
        match self.dispatch(key, Op::Reply { task, body }) {
            Some(Ok((_, false))) if steer => self.stats.steers += 1,
            Some(Ok((_, false))) => self.stats.replies += 1,
            Some(Ok((_, true))) | None => {}
            Some(Err(error)) => self.fail(format!("reply {key}: {error}")),
        }
    }

    fn cancel(&mut self, session: &str) {
        let Some(state) = self.sessions.get(session) else {
            return;
        };
        let task = state.task(session);
        if self.phase(&task).is_none_or(|phase| phase.terminal()) {
            return;
        }
        let key = format!("cancel:{task}");
        match self.dispatch(&key, Op::Cancel {
            task,
            reason: "closed".to_owned(),
        }) {
            Some(Ok((_, false))) => self.stats.cancels += 1,
            Some(Ok((_, true)) | Err(BoardError::Rejected(Rejection::Terminal))) | None => {}
            Some(Err(error)) => self.fail(format!("{key}: {error}")),
        }
        if let Some(state) = self.sessions.get_mut(session) {
            state.claim = None;
            state.last_output = None;
        }
    }
}

enum Seen {
    Added {
        id: AgentId,
        session: String,
        parent: Option<AgentId>,
        task: String,
    },
    Status {
        id: AgentId,
        status: AgentStatus,
    },
    Completion {
        id: AgentId,
        status: AgentStatus,
        revision: u64,
    },
    Message {
        message_id: String,
        agents: Vec<AgentId>,
    },
}

impl Seen {
    fn of(update: &AgentUpdate) -> Option<Self> {
        match update {
            AgentUpdate::Added(descriptor) => Some(Self::Added {
                id: descriptor.id,
                session: descriptor.session_id.clone(),
                parent: descriptor.parent,
                task: descriptor.task.clone(),
            }),
            AgentUpdate::Status { id, status } => Some(Self::Status {
                id: *id,
                status: status.clone(),
            }),
            AgentUpdate::Completion {
                id,
                status,
                revision,
            } => Some(Self::Completion {
                id: *id,
                status: status.clone(),
                revision: *revision,
            }),
            AgentUpdate::Message(message) => Some(Self::Message {
                message_id: message.message_id.value().to_string(),
                agents: message
                    .thread
                    .participants
                    .iter()
                    .filter_map(|participant| match participant {
                        MessageSender::Agent { agent_id } => Some(*agent_id),
                        _ => None,
                    })
                    .collect(),
            }),
            AgentUpdate::Event { .. } => None,
        }
    }
}

impl Inner {
    fn seen(&mut self, root: &str, seen: Seen) {
        match seen {
            Seen::Added {
                id,
                session,
                parent,
                task,
            } => self.added(root, id, &session, parent, &task),
            Seen::Status { id, status } => {
                let Some(session) = self.session_of(root, id) else {
                    return;
                };
                match &status {
                    AgentStatus::Running => self.running(root, id, &session),
                    AgentStatus::Interrupted => {
                        let seq = self.tick();
                        self.reply(&session, &format!("steer:{session}:{seq}"), "interrupt".to_owned(), true);
                    }
                    AgentStatus::Closed => self.cancel(&session),
                    AgentStatus::Completed { .. } | AgentStatus::Failed { .. } => {
                        self.finish(root, id, &session, &status, None);
                    }
                    AgentStatus::Pending | AgentStatus::Closing => {}
                }
            }
            Seen::Completion {
                id,
                status,
                revision,
            } => {
                let Some(session) = self.session_of(root, id) else {
                    return;
                };
                self.finish(root, id, &session, &status, Some(revision));
            }
            Seen::Message { message_id, agents } => {
                for agent_id in agents {
                    if let Some(session) = self.session_of(root, agent_id) {
                        self.reply(
                            &session,
                            &format!("reply:{root}:{message_id}:{agent_id}"),
                            format!("message {message_id}"),
                            false,
                        );
                    }
                }
            }
        }
    }
}

impl Coordination for BoardCoordination {
    fn observe(&self, root: &str, update: &AgentUpdate) {
        let Some(seen) = Seen::of(update) else {
            return;
        };
        let root = root.to_owned();
        self.inner.send(move |inner| inner.seen(&root, seen));
    }
}
