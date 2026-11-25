use std::collections::{BTreeMap, BTreeSet};

use nanocode_board_fold::{Change, ClaimId, Fold, NewTask, Op, Outcome, Phase, TaskId};

pub mod sim;

use sim::{Mutation, mutated};

pub const ROLE: &str = "dispatch.role";
pub const MEMBER: &str = "dispatch.member";
pub const PROJECT: &str = "dispatch.project";
pub const WORK: &str = "dispatch.task";
pub const PLACEMENT: &str = "dispatch.placement";
pub const COST: &str = "dispatch.cost";
const SPEC: &str = "spawn";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub cost_nano_usd: u64,
    pub placement: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budgets {
    pub agents_per_member_max: u64,
    pub agents_per_project_max: u64,
    pub nano_usd_max: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub roles: Vec<Role>,
    pub member_label: String,
    pub project_label: String,
    pub intent_prefix: String,
    pub budgets: Budgets,
    pub idle_retire_ms: u64,
    pub start_timeout_ms: u64,
    pub start_attempts_max: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Domain {
    pub role: String,
    pub member: String,
    pub project: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentPhase {
    Starting,
    Idle { since_ms: u64 },
    Busy,
    Retiring,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub id: String,
    pub domain: Domain,
    pub cost_nano_usd: u64,
    pub intent: Option<TaskId>,
    pub phase: AgentPhase,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Budget {
    Member(String),
    Project(String),
    Dollars,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    WorkGone,
    StartsExhausted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub intent: TaskId,
    pub task: TaskId,
    pub domain: Domain,
    pub placement: String,
    pub cost_nano_usd: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Spawn(Spawn),
    Start { intent: TaskId },
    Settle { intent: TaskId, agent: String },
    Release { intent: TaskId, why: Why },
    Retire { agent: String },
    Wait { task: TaskId, reason: Budget },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gauge {
    pub alive: u64,
    pub starting: u64,
    pub nano_usd: u64,
    pub open: u64,
    pub covered: u64,
    pub spawns: u64,
    pub waiting: u64,
    pub idle: u64,
    pub idle_with_work: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub gauge: Gauge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentState {
    Recorded,
    Starting { claim: ClaimId, until_ms: u64 },
    Live { agent: String },
    Ended,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub id: TaskId,
    pub task: TaskId,
    pub spawn: Spawn,
    pub attempts: u64,
    pub state: IntentState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    pub id: TaskId,
    pub domain: Domain,
    pub rank: Option<u64>,
    pub order: usize,
}

impl Work {
    pub const fn key(&self) -> (bool, Option<u64>, usize) {
        (self.rank.is_none(), self.rank, self.order)
    }
}

pub fn domain_of(policy: &Policy, labels: &BTreeMap<String, String>) -> Option<Domain> {
    let role = policy.roles.iter().find(|role| {
        role.labels
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    })?;
    let label = |key: &String| labels.get(key).cloned().unwrap_or_default();
    Some(Domain {
        role: role.name.clone(),
        member: label(&policy.member_label),
        project: label(&policy.project_label),
    })
}

pub fn is_intent(policy: &Policy, task: &TaskId) -> bool {
    task.0.starts_with(&policy.intent_prefix)
}

pub fn intents(fold: &Fold, policy: &Policy) -> Vec<Intent> {
    fold.tasks()
        .range(TaskId(policy.intent_prefix.clone())..)
        .take_while(|(id, _)| is_intent(policy, id))
        .filter_map(|(id, task)| {
            let labels = &task.spec.labels;
            let get = |key: &str| labels.get(key).cloned();
            let work = TaskId(get(WORK)?);
            let spawn = Spawn {
                intent: id.clone(),
                task: work.clone(),
                domain: Domain {
                    role: get(ROLE)?,
                    member: get(MEMBER)?,
                    project: get(PROJECT)?,
                },
                placement: get(PLACEMENT)?,
                cost_nano_usd: get(COST)?.parse().ok()?,
            };
            let state = match &task.phase {
                Phase::Open => IntentState::Recorded,
                Phase::Claimed {
                    claim, until_ms, ..
                } => IntentState::Starting {
                    claim: claim.clone(),
                    until_ms: *until_ms,
                },
                Phase::Done { result, .. } => IntentState::Live {
                    agent: result.clone(),
                },
                Phase::Failed { .. } | Phase::Cancelled { .. } | Phase::Blocked { .. } => {
                    IntentState::Ended
                }
            };
            Some(Intent {
                id: id.clone(),
                task: work,
                spawn,
                attempts: task.attempts,
                state,
            })
        })
        .collect()
}

pub fn work(fold: &Fold, policy: &Policy, at_ms: u64) -> Vec<Work> {
    let mut seen = BTreeSet::new();
    let mut found = fold
        .entries()
        .iter()
        .filter(|entry| entry.change == Change::Posted)
        .enumerate()
        .filter_map(|(order, entry)| {
            if is_intent(policy, &entry.task) || !seen.insert(entry.task.clone()) {
                return None;
            }
            let task = fold.task(&entry.task)?;
            if task.assignee.is_some() || fold.claimable(&entry.task, at_ms).is_err() {
                return None;
            }
            Some(Work {
                id: entry.task.clone(),
                domain: domain_of(policy, &task.spec.labels)?,
                rank: task.rank,
                order,
            })
        })
        .collect::<Vec<_>>();
    if mutated(Mutation::SkipsRank) {
        found.sort_by(|a, b| a.id.cmp(&b.id));
    } else {
        found.sort_by_key(Work::key);
    }
    found
}

#[derive(Default)]
struct Usage {
    members: BTreeMap<String, u64>,
    projects: BTreeMap<String, u64>,
    nano_usd: u64,
}

impl Usage {
    fn add(&mut self, domain: &Domain, cost: u64) {
        *self.members.entry(domain.member.clone()).or_default() += 1;
        *self.projects.entry(domain.project.clone()).or_default() += 1;
        self.nano_usd = self.nano_usd.saturating_add(cost);
    }

    fn refuse(&self, budgets: &Budgets, domain: &Domain, cost: u64) -> Option<Budget> {
        if mutated(Mutation::IgnoresBudget) {
            return None;
        }
        let count = |map: &BTreeMap<String, u64>, key: &String| map.get(key).copied().unwrap_or(0);
        if count(&self.members, &domain.member) >= budgets.agents_per_member_max {
            return Some(Budget::Member(domain.member.clone()));
        }
        if count(&self.projects, &domain.project) >= budgets.agents_per_project_max {
            return Some(Budget::Project(domain.project.clone()));
        }
        if self.nano_usd.saturating_add(cost) > budgets.nano_usd_max {
            return Some(Budget::Dollars);
        }
        None
    }
}

#[derive(Default)]
struct Blocked {
    dollars: bool,
    members: BTreeSet<String>,
    projects: BTreeSet<String>,
}

impl Blocked {
    fn hit(&self, domain: &Domain) -> Option<Budget> {
        if self.members.contains(&domain.member) {
            return Some(Budget::Member(domain.member.clone()));
        }
        if self.projects.contains(&domain.project) {
            return Some(Budget::Project(domain.project.clone()));
        }
        self.dollars.then_some(Budget::Dollars)
    }

    fn mark(&mut self, reason: &Budget) {
        match reason {
            Budget::Member(member) => {
                self.members.insert(member.clone());
            }
            Budget::Project(project) => {
                self.projects.insert(project.clone());
            }
            Budget::Dollars => self.dollars = true,
        }
    }

    fn shares(&self, domain: &Domain) -> bool {
        self.hit(domain).is_some()
    }
}

#[derive(Default)]
struct Planning {
    actions: Vec<Action>,
    gauge: Gauge,
    usage: Usage,
    supply: BTreeMap<Domain, u64>,
    taken: BTreeMap<Domain, u64>,
}

pub fn plan(fold: &Fold, at_ms: u64, pool: &[Agent], policy: &Policy) -> Plan {
    let open = work(fold, policy, at_ms);
    let recorded = intents(fold, policy);
    let mut planning = Planning::default();
    let idle = planning.tally_pool(pool);
    let (pending, mut attempts) = planning.resume_intents(&recorded, pool, &open, at_ms, policy);
    planning.gauge.open = open.len() as u64;
    let blocked = planning.cover_work(&open, &pending, &mut attempts, policy);
    assert!(
        planning.gauge.covered <= planning.gauge.open,
        "each open work is covered at most once"
    );
    assert!(
        planning.gauge.covered + planning.gauge.waiting + planning.gauge.spawns
            <= planning.gauge.open,
        "each open work is covered, waits or spawns at most once"
    );
    planning.retire_idle(idle, &blocked, at_ms, policy);
    planning.gauge.nano_usd = planning.usage.nano_usd;
    Plan {
        actions: planning.actions,
        gauge: planning.gauge,
    }
}

impl Planning {
    fn take(&mut self, domain: &Domain) -> bool {
        let used = self.taken.entry(domain.clone()).or_default();
        let have = self.supply.get(domain).copied().unwrap_or(0);
        (*used < have).then(|| *used += 1).is_some()
    }

    fn tally_pool<'p>(&mut self, pool: &'p [Agent]) -> BTreeMap<Domain, Vec<&'p Agent>> {
        let mut idle = BTreeMap::<Domain, Vec<&Agent>>::new();
        for agent in pool {
            self.usage.add(&agent.domain, agent.cost_nano_usd);
            self.gauge.alive += 1;
            match agent.phase {
                AgentPhase::Starting => {
                    self.gauge.starting += 1;
                    *self.supply.entry(agent.domain.clone()).or_default() += 1;
                }
                AgentPhase::Idle { .. } => {
                    self.gauge.idle += 1;
                    *self.supply.entry(agent.domain.clone()).or_default() += 1;
                    idle.entry(agent.domain.clone()).or_default().push(agent);
                }
                AgentPhase::Busy if mutated(Mutation::StrandsWork) => {
                    *self.supply.entry(agent.domain.clone()).or_default() += 1;
                }
                AgentPhase::Busy | AgentPhase::Retiring => {}
            }
        }
        idle
    }

    fn resume_intents<'a>(
        &mut self,
        recorded: &'a [Intent],
        pool: &[Agent],
        open: &[Work],
        at_ms: u64,
        policy: &Policy,
    ) -> (BTreeSet<TaskId>, BTreeMap<&'a TaskId, u64>) {
        let claimable = open.iter().map(|work| &work.id).collect::<BTreeSet<_>>();
        let by_intent = pool
            .iter()
            .filter_map(|agent| Some((agent.intent.as_ref()?, agent)))
            .collect::<BTreeMap<_, _>>();
        let mut pending = BTreeSet::new();
        let mut attempts = BTreeMap::<&TaskId, u64>::new();
        for intent in recorded {
            *attempts.entry(&intent.task).or_default() += 1;
            if matches!(intent.state, IntentState::Ended | IntentState::Live { .. }) {
                continue;
            }
            if let Some(agent) = by_intent.get(&intent.id) {
                self.actions.push(Action::Settle {
                    intent: intent.id.clone(),
                    agent: agent.id.clone(),
                });
                continue;
            }
            let expired = match &intent.state {
                IntentState::Starting { until_ms, .. } => *until_ms <= at_ms,
                _ => true,
            };
            if expired && !claimable.contains(&intent.task) {
                self.actions.push(Action::Release {
                    intent: intent.id.clone(),
                    why: Why::WorkGone,
                });
                continue;
            }
            if expired && intent.attempts >= policy.start_attempts_max {
                self.actions.push(Action::Release {
                    intent: intent.id.clone(),
                    why: Why::StartsExhausted,
                });
                continue;
            }
            if expired {
                self.actions.push(Action::Start {
                    intent: intent.id.clone(),
                });
            }
            if mutated(Mutation::ForgetsIntent) {
                continue;
            }
            self.usage
                .add(&intent.spawn.domain, intent.spawn.cost_nano_usd);
            self.gauge.starting += 1;
            *self.supply.entry(intent.spawn.domain.clone()).or_default() += 1;
            pending.insert(intent.task.clone());
        }
        (pending, attempts)
    }

    fn cover_work<'a>(
        &mut self,
        open: &'a [Work],
        pending: &BTreeSet<TaskId>,
        attempts: &mut BTreeMap<&'a TaskId, u64>,
        policy: &Policy,
    ) -> Blocked {
        for work in open.iter().filter(|work| pending.contains(&work.id)) {
            self.take(&work.domain);
            self.gauge.covered += 1;
        }
        let mut blocked = Blocked::default();
        for work in open.iter().filter(|work| !pending.contains(&work.id)) {
            if self.take(&work.domain) {
                self.gauge.covered += 1;
                continue;
            }
            let Some(role) = policy
                .roles
                .iter()
                .find(|role| role.name == work.domain.role)
            else {
                continue;
            };
            let reason = blocked.hit(&work.domain).or_else(|| {
                self.usage
                    .refuse(&policy.budgets, &work.domain, role.cost_nano_usd)
            });
            if let Some(reason) = reason {
                blocked.mark(&reason);
                self.gauge.waiting += 1;
                self.actions.push(Action::Wait {
                    task: work.id.clone(),
                    reason,
                });
                continue;
            }
            let n = attempts.entry(&work.id).or_default();
            let intent = TaskId(format!("{}{}/{}", policy.intent_prefix, work.id.0, n));
            *n += 1;
            self.usage.add(&work.domain, role.cost_nano_usd);
            self.gauge.spawns += 1;
            self.actions.push(Action::Spawn(Spawn {
                intent,
                task: work.id.clone(),
                domain: work.domain.clone(),
                placement: role.placement.clone(),
                cost_nano_usd: role.cost_nano_usd,
            }));
        }
        blocked
    }

    fn retire_idle(
        &mut self,
        idle: BTreeMap<Domain, Vec<&Agent>>,
        blocked: &Blocked,
        at_ms: u64,
        policy: &Policy,
    ) {
        for (domain, mut agents) in idle {
            let used = self.taken.get(&domain).copied().unwrap_or(0);
            let have = self.supply.get(&domain).copied().unwrap_or(0);
            let spare = have.saturating_sub(used).min(agents.len() as u64);
            self.gauge.idle_with_work += (agents.len() as u64).min(used);
            if mutated(Mutation::LeavesIdle) {
                continue;
            }
            agents.sort_by_key(|agent| match agent.phase {
                AgentPhase::Idle { since_ms } => (since_ms, agent.id.clone()),
                _ => (u64::MAX, agent.id.clone()),
            });
            let pressed = blocked.shares(&domain);
            for agent in agents.into_iter().take(spare as usize) {
                let AgentPhase::Idle { since_ms } = agent.phase else {
                    continue;
                };
                if pressed || since_ms.saturating_add(policy.idle_retire_ms) <= at_ms {
                    self.actions.push(Action::Retire {
                        agent: agent.id.clone(),
                    });
                }
            }
        }
    }
}

pub fn intent_task(spawn: &Spawn) -> NewTask {
    let labels = [
        (ROLE, spawn.domain.role.clone()),
        (MEMBER, spawn.domain.member.clone()),
        (PROJECT, spawn.domain.project.clone()),
        (WORK, spawn.task.0.clone()),
        (PLACEMENT, spawn.placement.clone()),
        (COST, spawn.cost_nano_usd.to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect();
    NewTask {
        id: spawn.intent.clone(),
        parent: None,
        spec: SPEC.to_owned(),
        labels,
        needs: Vec::new(),
        terms: Default::default(),
    }
}

fn start(intent: &TaskId, attempts: u64, policy: &Policy) -> (String, Op) {
    (
        format!("{}/start/{attempts}", intent.0),
        Op::Claim {
            task: intent.clone(),
            lease_ms: policy.start_timeout_ms,
            fence: None,
            cap: None,
        },
    )
}

pub fn ops(fold: &Fold, policy: &Policy, dispatcher: &str, action: &Action) -> Vec<(String, Op)> {
    let attempts = |intent: &TaskId| fold.task(intent).map_or(0, |task| task.attempts);
    match action {
        Action::Spawn(spawn) => vec![
            (
                spawn.intent.0.clone(),
                Op::Post {
                    task: intent_task(spawn),
                },
            ),
            start(&spawn.intent, 0, policy),
        ],
        Action::Start { intent } => vec![start(intent, attempts(intent), policy)],
        Action::Settle { intent, agent } => match fold.task(intent).map(|task| &task.phase) {
            Some(Phase::Claimed { claim, worker, .. }) if worker == dispatcher => vec![(
                format!("{}/settle", intent.0),
                Op::Finish {
                    task: intent.clone(),
                    claim: claim.clone(),
                    outcome: Outcome::Done(agent.clone()),
                },
            )],
            Some(Phase::Open | Phase::Claimed { .. }) => {
                vec![start(intent, attempts(intent), policy)]
            }
            _ => Vec::new(),
        },
        Action::Release { intent, why } => vec![(
            format!("{}/release", intent.0),
            Op::Cancel {
                task: intent.clone(),
                reason: match why {
                    Why::WorkGone => "work gone".to_owned(),
                    Why::StartsExhausted => "starts exhausted".to_owned(),
                },
            },
        )],
        Action::Retire { .. } | Action::Wait { .. } => Vec::new(),
    }
}
