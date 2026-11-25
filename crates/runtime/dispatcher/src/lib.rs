use std::{collections::BTreeSet, future::Future, pin::Pin};

use nanocode_board::{
    Board, BoardError, ClaimId, Op, Phase, TaskId, Write,
    brief::{Brief, BriefError, BriefPolicy},
    tasks::TaskOp,
    wake::{Timer, Wake},
};
use nanocode_dispatch::{Action, Agent, AgentPhase, Gauge, IntentState, Policy, Spawn};

pub mod sim;

use sim::{Mutation, mutated};

pub const BRIEF_BLOCK: &str = "brief";

pub const BRIEF: BriefPolicy = BriefPolicy {
    tokens_max: 8_000,
    bytes_per_token: 4,
};

#[cfg(not(target_family = "wasm"))]
pub type PoolFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(target_family = "wasm")]
pub type PoolFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
    pub spawn: Spawn,
    pub agent: String,
    pub worker: String,
    pub brief: Brief,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolError {
    Placement(String),
    Unknown(String),
}

pub trait Pool: nanocode_platform::MaybeSync {
    fn agents(&self) -> PoolFuture<'_, Vec<Agent>>;

    fn start(&self, start: Start) -> PoolFuture<'_, Result<(), PoolError>>;

    fn retire<'a>(&'a self, agent: &'a str) -> PoolFuture<'a, Result<(), PoolError>>;
}

#[derive(Debug)]
pub enum DispatchError {
    Board(BoardError),
}

impl From<BoardError> for DispatchError {
    fn from(error: BoardError) -> Self {
        Self::Board(error)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub passes: u64,
    pub starts: u64,
    pub retires: u64,
    pub reaped: u64,
    pub claims_released: u64,
    pub briefs_blocked: u64,
    pub start_failures: u64,
    pub retire_failures: u64,
    pub waits: u64,
    /// Board appends the passes' op groups took (one per round of a group).
    pub appends: u64,
    /// Ops the passes sent to the board in groups.
    pub ops: u64,
}

/// One pass's board ops, sent as one `Board::submit_group` so a pass with many spawns, starts
/// and settles commits them in one or two appends instead of one per op. Starts wait for the
/// group: an agent starts only once its intent's claim landed and is held.
#[derive(Default)]
struct Staged {
    writes: Vec<Write>,
    intents: BTreeSet<TaskId>,
    starts: Vec<(Spawn, Brief)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Start { intent: TaskId, error: PoolError },
    Retire { agent: String, error: PoolError },
    Brief { task: TaskId, error: BriefError },
    Release { task: TaskId, error: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    pub gauge: Gauge,
    pub wake_at_ms: Option<u64>,
    pub failures: Vec<Failure>,
}

pub fn agent_name(intent: &TaskId) -> String {
    intent
        .0
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

const fn intent_of(action: &Action) -> Option<&TaskId> {
    match action {
        Action::Spawn(spawn) => Some(&spawn.intent),
        Action::Start { intent }
        | Action::Settle { intent, .. }
        | Action::Release { intent, .. } => Some(intent),
        Action::Retire { .. } | Action::Wait { .. } => None,
    }
}

pub const EAGER_PASSES_MAX: u64 = 4;

pub const RETIRE_RETRY_MS: u64 = 1_000;

pub struct Dispatcher {
    board: Board,
    policy: Policy,
    brief: BriefPolicy,
    counters: Counters,
    eager: u64,
}

impl Dispatcher {
    pub fn new(board: Board, policy: Policy, brief: BriefPolicy) -> Self {
        assert!(
            !board.writer().contains('+'),
            "a dispatcher acts as a whole principal so it covers its workers"
        );
        assert!(
            brief.tokens_max > 0,
            "a dispatcher brief policy needs tokens"
        );
        Self {
            board,
            policy,
            brief,
            counters: Counters::default(),
            eager: 0,
        }
    }

    pub const fn board(&self) -> &Board {
        &self.board
    }

    pub const fn counters(&self) -> Counters {
        self.counters
    }

    pub fn worker_of(&self, agent: &str) -> String {
        format!("{}+{agent}", self.board.writer())
    }

    pub async fn pass(&mut self, pool: &dyn Pool, at_ms: u64) -> Result<Pass, DispatchError> {
        self.board.refresh().await?;
        let agents = pool.agents().await;
        let plan = nanocode_dispatch::plan(self.board.fold(), at_ms, &agents, &self.policy);
        self.counters.passes += 1;
        let mut failures = Vec::new();
        let mut acted = false;
        let mut staged = Staged::default();
        let mut failed = None;
        for action in &plan.actions {
            match self
                .act(pool, at_ms, action, &mut staged, &mut failures, &mut acted)
                .await
            {
                Ok(Some(failure)) => failures.push(failure),
                Ok(None) => acted |= !matches!(action, Action::Wait { .. }),
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        if let Err(error) = self
            .flush(pool, at_ms, &mut staged, &mut failures, &mut acted)
            .await
        {
            failed.get_or_insert(error);
        }
        if let Some(error) = failed {
            return Err(error);
        }
        if !mutated(Mutation::NeverReaps) {
            acted |= self.reap(pool, at_ms, &plan.actions, &mut failures).await?;
        }
        let eager = acted && self.eager < EAGER_PASSES_MAX && !mutated(Mutation::SleepsAfterActing);
        self.eager = if eager { self.eager + 1 } else { 0 };
        let wake_at_ms = if eager {
            Some(at_ms)
        } else {
            self.wake_at(&pool.agents().await, at_ms)
        };
        Ok(Pass {
            gauge: plan.gauge,
            wake_at_ms,
            failures,
        })
    }

    async fn reap(
        &mut self,
        pool: &dyn Pool,
        at_ms: u64,
        actions: &[Action],
        failures: &mut Vec<Failure>,
    ) -> Result<bool, DispatchError> {
        let retired = actions
            .iter()
            .filter_map(|action| match action {
                Action::Retire { agent } => Some(agent.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut acted = false;
        for agent in pool.agents().await {
            let AgentPhase::Idle { since_ms } = agent.phase else {
                continue;
            };
            if since_ms.saturating_add(self.policy.idle_retire_ms) > at_ms
                || retired.contains(&agent.id.as_str())
            {
                continue;
            }
            match self.retire(pool, at_ms, &agent.id).await? {
                Some(failure) => failures.push(failure),
                None => {
                    self.counters.reaped += 1;
                    acted = true;
                }
            }
        }
        Ok(acted)
    }

    pub async fn run(
        &mut self,
        pool: &dyn Pool,
        wake: &dyn Wake,
        timer: &dyn Timer,
        until_ms: u64,
    ) -> Result<Vec<Failure>, DispatchError> {
        let mut failures = Vec::new();
        loop {
            let now = timer.now_ms();
            let pass = self.pass(pool, now).await?;
            failures.extend(pass.failures);
            if now >= until_ms {
                return Ok(failures);
            }
            let wake_at = pass.wake_at_ms.map_or(until_ms, |at| at.min(until_ms));
            let seen = self.board.fold().next() - 1;
            wake.wait(self.board.writer(), seen, wake_at).await;
        }
    }

    /// Stages one action's ops into the pass's group, or acts at once for retires and waits. An
    /// action that must see the group's ops land first flushes it, so its ops are computed
    /// against a fold that includes the earlier ones, as one op at a time did.
    async fn act(
        &mut self,
        pool: &dyn Pool,
        at_ms: u64,
        action: &Action,
        staged: &mut Staged,
        failures: &mut Vec<Failure>,
        acted: &mut bool,
    ) -> Result<Option<Failure>, DispatchError> {
        // A retire writes its own ops (releasing what the agent held) and must see the earlier
        // ones land first, as must an action on an intent the group already touches.
        if matches!(action, Action::Retire { .. })
            || intent_of(action).is_some_and(|intent| staged.intents.contains(intent))
        {
            self.flush(pool, at_ms, staged, failures, acted).await?;
        }
        match action {
            Action::Spawn(spawn) => {
                self.spawn(pool, at_ms, spawn.clone(), action, staged, acted)
                    .await
            }
            Action::Start { intent } => {
                let spawn = nanocode_dispatch::intents(self.board.fold(), &self.policy)
                    .into_iter()
                    .find(|recorded| recorded.id == *intent)
                    .map(|recorded| recorded.spawn);
                let Some(spawn) = spawn else {
                    panic!("the plan started intent {intent}, which is not on the board");
                };
                self.spawn(pool, at_ms, spawn, action, staged, acted).await
            }
            Action::Settle { .. } | Action::Release { .. } => {
                self.stage(at_ms, action, staged);
                *acted = true;
                Ok(None)
            }
            Action::Retire { agent } => {
                let failure = self.retire(pool, at_ms, agent).await?;
                *acted |= failure.is_none();
                Ok(failure)
            }
            Action::Wait { .. } => {
                self.counters.waits += 1;
                Ok(None)
            }
        }
    }

    fn stage(&self, at_ms: u64, action: &Action, staged: &mut Staged) {
        let ops =
            nanocode_dispatch::ops(self.board.fold(), &self.policy, self.board.writer(), action);
        if let Some(intent) = intent_of(action) {
            staged.intents.insert(intent.clone());
        }
        staged.writes.extend(ops.into_iter().map(|(key, op)| Write {
            by: None,
            key: Some(key),
            at_ms,
            op,
        }));
    }

    /// Sends the staged ops as one group, then starts each staged spawn whose intent the
    /// dispatcher now holds. Every op gets its own verdict: a rejected one (another writer's
    /// claim, a stale fence, a cap) is skipped as one op at a time skipped it, and a store error
    /// on one op still lets the others' starts go ahead before the pass reports it.
    async fn flush(
        &mut self,
        pool: &dyn Pool,
        at_ms: u64,
        staged: &mut Staged,
        failures: &mut Vec<Failure>,
        acted: &mut bool,
    ) -> Result<(), DispatchError> {
        let writes = std::mem::take(&mut staged.writes);
        staged.intents.clear();
        let mut failed = None;
        if !writes.is_empty() {
            self.counters.ops += writes.len() as u64;
            let (verdicts, appends) = self.board.submit_group(writes).await;
            self.counters.appends += appends;
            for verdict in verdicts {
                match verdict {
                    Ok(_) | Err(BoardError::Rejected(_)) => {}
                    Err(error) => {
                        failed.get_or_insert(error);
                    }
                }
            }
        }
        for (spawn, brief) in std::mem::take(&mut staged.starts) {
            if self.holds(&spawn.intent, at_ms).is_none() {
                *acted = true;
                continue;
            }
            match self.start(pool, spawn, brief).await? {
                Some(failure) => failures.push(failure),
                None => *acted = true,
            }
        }
        failed.map_or(Ok(()), |error| Err(error.into()))
    }

    async fn spawn(
        &mut self,
        pool: &dyn Pool,
        at_ms: u64,
        spawn: Spawn,
        action: &Action,
        staged: &mut Staged,
        acted: &mut bool,
    ) -> Result<Option<Failure>, DispatchError> {
        let brief = match self.board.fold().brief(&spawn.task, &self.brief) {
            Ok(brief) => brief,
            Err(error) => return self.block(at_ms, &spawn.task, error).await,
        };
        if mutated(Mutation::StartsBeforeIntent) {
            let failure = self.start(pool, spawn, brief).await?;
            *acted |= failure.is_none();
            return Ok(failure);
        }
        self.stage(at_ms, action, staged);
        staged.starts.push((spawn, brief));
        Ok(None)
    }

    async fn start(
        &mut self,
        pool: &dyn Pool,
        spawn: Spawn,
        brief: Brief,
    ) -> Result<Option<Failure>, DispatchError> {
        assert_eq!(
            brief.task, spawn.task,
            "a start carries its own task's brief"
        );
        assert!(
            brief.tokens <= self.brief.tokens_max,
            "a brief fits its policy"
        );
        let intent = spawn.intent.clone();
        let agent = agent_name(&intent);
        let worker = self.worker_of(&agent);
        self.counters.starts += 1;
        let start = Start {
            spawn,
            agent,
            worker,
            brief,
        };
        match pool.start(start).await {
            Ok(()) => Ok(None),
            Err(error) => {
                self.counters.start_failures += 1;
                Ok(Some(Failure::Start { intent, error }))
            }
        }
    }

    fn holds(&self, intent: &TaskId, at_ms: u64) -> Option<ClaimId> {
        match nanocode_dispatch::intents(self.board.fold(), &self.policy)
            .into_iter()
            .find(|recorded| recorded.id == *intent)
            .map(|recorded| recorded.state)
        {
            Some(IntentState::Starting { claim, until_ms }) if until_ms > at_ms => {
                let task = self.board.fold().task(intent)?;
                match &task.phase {
                    Phase::Claimed { worker, .. } if worker == self.board.writer() => Some(claim),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    async fn block(
        &mut self,
        at_ms: u64,
        task: &TaskId,
        error: BriefError,
    ) -> Result<Option<Failure>, DispatchError> {
        let op = Op::Tasks(TaskOp::Block {
            task: task.clone(),
            claim: None,
            kind: BRIEF_BLOCK.to_owned(),
            reason: format!("{error:?}"),
        });
        let key = format!("{}/brief", task.0);
        match self.board.submit_keyed(&key, at_ms, op).await {
            Ok(_) | Err(BoardError::Rejected(_)) => {}
            Err(error) => return Err(error.into()),
        }
        self.counters.briefs_blocked += 1;
        Ok(Some(Failure::Brief {
            task: task.clone(),
            error,
        }))
    }

    async fn retire(
        &mut self,
        pool: &dyn Pool,
        at_ms: u64,
        agent: &str,
    ) -> Result<Option<Failure>, DispatchError> {
        if let Err(error) = pool.retire(agent).await {
            self.counters.retire_failures += 1;
            return Ok(Some(Failure::Retire {
                agent: agent.to_owned(),
                error,
            }));
        }
        self.counters.retires += 1;
        if mutated(Mutation::RetireKeepsClaims) {
            return Ok(None);
        }
        self.board.refresh().await?;
        let worker = self.worker_of(agent);
        let held = self
            .board
            .fold()
            .tasks()
            .iter()
            .filter_map(|(id, task)| match &task.phase {
                Phase::Claimed {
                    claim, worker: by, ..
                } if *by == worker => Some((id.clone(), claim.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut failure = None;
        for (task, claim) in held {
            let op = Op::Release {
                task: task.clone(),
                claim: claim.clone(),
                note: format!("{agent} retired"),
            };
            match self.board.submit(at_ms, op).await {
                Ok(_) => self.counters.claims_released += 1,
                Err(BoardError::Rejected(rejection)) => {
                    failure.get_or_insert(Failure::Release {
                        task,
                        error: format!("{rejection:?}"),
                    });
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(failure)
    }

    fn wake_at(&self, agents: &[Agent], at_ms: u64) -> Option<u64> {
        let idle = agents.iter().filter_map(|agent| match agent.phase {
            AgentPhase::Idle { since_ms } if !mutated(Mutation::SleepsThroughIdle) => {
                let due = since_ms.saturating_add(self.policy.idle_retire_ms);
                Some(if due > at_ms {
                    due
                } else {
                    at_ms.saturating_add(RETIRE_RETRY_MS)
                })
            }
            _ => None,
        });
        let starting = nanocode_dispatch::intents(self.board.fold(), &self.policy)
            .into_iter()
            .filter_map(|intent| match intent.state {
                IntentState::Starting { until_ms, .. } => Some(until_ms),
                _ => None,
            });
        idle.chain(starting)
            .chain(self.board.due_ms())
            .filter(|at| *at > at_ms)
            .min()
    }
}
