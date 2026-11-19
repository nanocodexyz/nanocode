use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
};

use nanocode_board::{
    Board, BoardError, Change, NewTask, Op, Outcome, Phase, Rejection, TaskId,
    access::Can,
    tasks::{Need, OnFail, Pace, Planned, Refusal, TaskOp},
};

use super::{Expect, Reference, World, Worker};
use crate::rng::Rng;

const STEPS: [&str; 6] = ["goal", "lane", "fix", "verify", "consolidate", "review"];
const TRIES: u64 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Random,
    Pick,
    Run,
}

pub struct Plan {
    pub modes: Vec<Mode>,
    pub batched: BTreeSet<TaskId>,
    pub junk: bool,
    pub block_ppm: u64,
    pub hold_ms: u64,
    pub acts: u64,
    pub act_ppm: u64,
    pub drowse_ppm: u64,
    pub desk: u64,
}

impl Plan {
    pub fn of(seed: u64, plan: &mut [NewTask], workers: u64, lease_ms: u64) -> Self {
        let mut rng = Rng::derive(seed, "board tasks shape");
        let wait_ppm = rng.range(0, 500_000);
        let assign_ppm = rng.range(0, 300_000);
        for task in plan.iter_mut() {
            if task.labels["step"] == "fix" && rng.chance(wait_ppm) {
                task.terms.on_fail = OnFail::Wait;
            }
            if rng.chance(assign_ppm) {
                task.terms.assignee = Some(format!("agent:w{}", rng.below(workers)));
            }
        }
        let modes = (0..workers)
            .map(|_| [Mode::Random, Mode::Pick, Mode::Run][rng.pick(3)])
            .collect();
        let batched = plan
            .iter()
            .filter(|task| task.labels["step"] == "lane")
            .filter(|_| rng.chance(500_000))
            .map(|task| task.id.clone())
            .collect();
        Self {
            modes,
            batched,
            junk: rng.chance(300_000),
            block_ppm: rng.range(0, 80_000),
            hold_ms: rng.range(0, lease_ms * 2),
            acts: rng.range(0, plan.len() as u64),
            act_ppm: rng.range(0, 300_000),
            drowse_ppm: rng.range(0, 200_000),
            desk: rng.draw(),
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "modes={:?} batched={} junk={} block_ppm={} hold_ms={} acts={} drowse_ppm={}",
            self.modes,
            self.batched.len(),
            self.junk,
            self.block_ppm,
            self.hold_ms,
            self.acts,
            self.drowse_ppm
        )
    }
}

fn planned(world: &World, lane: &TaskId) -> Vec<Planned> {
    let prefix = format!("{lane}/");
    let key = |id: &TaskId| id.0.strip_prefix(&prefix).expect("a lane's tasks are named under it").to_owned();
    world
        .shape
        .plan
        .iter()
        .filter(|task| task.parent.as_ref() == Some(lane))
        .map(|task| Planned {
            key: key(&task.id),
            parent: Some(Need::Task(lane.clone())),
            spec: task.spec.clone(),
            labels: task.labels.clone(),
            needs: task.needs.iter().map(|need| Need::Key(key(need))).collect(),
            terms: task.terms.clone(),
        })
        .collect()
}

async fn send(world: &World, board: &mut Board, name: &TaskId, tasks: &[Planned]) -> Result<(), BoardError> {
    loop {
        match board.batch(world.now_ms(), name.clone(), tasks.to_vec()).await {
            Ok(_) => return Ok(()),
            Err(error @ BoardError::Rejected(_)) => return Err(error),
            Err(error) => {
                world.note(&error);
                world.nap(world.shape.poll_ms).await;
            }
        }
    }
}

pub async fn post_batch(world: &World, board: &mut Board, lane: &TaskId) {
    match send(world, board, lane, &planned(world, lane)).await {
        Ok(()) | Err(BoardError::Rejected(Rejection::Exists)) => world.oracle.borrow_mut().stats.batched += 1,
        Err(error) => world.oracle.borrow_mut().wrong.push(format!("lead could not post the batch {lane}: {error}")),
    }
}

pub async fn junk(world: &World, board: &mut Board) {
    let task = |key: &str, need: &str| Planned {
        key: key.to_owned(),
        parent: None,
        spec: "loop".to_owned(),
        labels: BTreeMap::from([("step".to_owned(), "junk".to_owned())]),
        needs: vec![Need::Key(need.to_owned())],
        terms: nanocode_board::tasks::Terms::default(),
    };
    match send(world, board, &TaskId("junk".to_owned()), &[task("a", "b"), task("b", "a")]).await {
        Err(BoardError::Rejected(Rejection::Tasks(Refusal::Cycle(_)))) => {}
        Ok(()) => world.oracle.borrow_mut().wrong.push("the board took a batch whose tasks need each other".to_owned()),
        Err(error) => world.oracle.borrow_mut().wrong.push(format!("the looping batch was refused for {error}")),
    }
}

fn expected(board: &Board, labels: &BTreeMap<String, String>, now: u64) -> Option<TaskId> {
    let fold = board.fold();
    let me = board.writer().split('+').next().unwrap_or_default();
    let at = fold.now_ms().max(now);
    let order = fold
        .entries()
        .iter()
        .filter(|entry| entry.change == Change::Posted)
        .map(|entry| &entry.task)
        .collect::<Vec<_>>();
    let fits = |id: &TaskId, mine: bool| {
        let Some(task) = fold.task(id) else {
            return false;
        };
        let free = match &task.phase {
            Phase::Open => true,
            Phase::Claimed { until_ms, .. } => *until_ms <= at,
            _ => false,
        };
        let met = task
            .spec
            .needs
            .iter()
            .all(|need| matches!(fold.task(need).map(|need| &need.phase), Some(Phase::Done { .. })));
        let assigned = if mine { task.assignee.as_deref() == Some(me) } else { task.assignee.is_none() };
        free && met && assigned && board.may(&id.0, Can::Label) && labels.iter().all(|(key, value)| task.spec.labels.get(key) == Some(value))
    };
    order
        .iter()
        .find(|id| fits(id, true))
        .or_else(|| order.iter().find(|id| fits(id, false)))
        .map(|id| (*id).clone())
}

pub fn pick(world: &World, board: &Board, rng: &mut Rng) -> Option<TaskId> {
    let now = world.now_ms();
    let mut labels = BTreeMap::new();
    if rng.chance(500_000) {
        labels.insert("step".to_owned(), STEPS[rng.pick(STEPS.len())].to_owned());
    }
    for filter in [labels, BTreeMap::new()] {
        let got = board.fold().pick(board.writer(), &filter, now).cloned();
        let want = expected(board, &filter, now);
        let mut oracle = world.oracle.borrow_mut();
        oracle.stats.picks += 1;
        if got != want {
            oracle.wrong.push(format!("{} picked {got:?} for {filter:?}, the oldest ready task for it is {want:?}", board.writer()));
        }
        if got.is_some() {
            return got;
        }
    }
    None
}

struct Drowsy<'a> {
    world: &'a World,
    rng: RefCell<Rng>,
}

impl Pace for Drowsy<'_> {
    fn now_ms(&self) -> u64 {
        self.world.now_ms()
    }

    fn sleep(&self, ms: u64) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        let lease_ms = self.world.shape.lease_ms;
        let mut rng = self.rng.borrow_mut();
        let extra = if rng.chance(self.world.shape.tasks.drowse_ppm) { lease_ms + rng.range(1, lease_ms) } else { 0 };
        Box::pin(self.world.nap(ms + extra))
    }
}

pub async fn run(worker: &mut Worker<'_>, task: TaskId) {
    let world = worker.world;
    let shape = &world.shape;
    let steps = (0..worker.rng.range(1, 6)).map(|_| worker.rng.range(1, shape.step_ms)).collect::<Vec<_>>();
    let failed = worker.rng.chance(shape.fail_ppm);
    let pace = Drowsy { world, rng: RefCell::new(Rng::derive(worker.rng.draw(), "board drowse")) };
    world.oracle.borrow_mut().stats.runs += 1;
    let ran = worker
        .board
        .run(&task, shape.lease_ms, &pace, |lease| async move {
            for step in steps {
                world.nap(step).await;
                if world.now_ms() < lease.until_ms() {
                    world.oracle.borrow_mut().effects.push((lease.task.clone(), lease.claim.clone(), world.now_ms()));
                }
            }
            if failed {
                Outcome::Failed(format!("{} failed under {}", lease.task, lease.claim))
            } else {
                Outcome::Done(format!("{} by {}", lease.task, lease.claim))
            }
        })
        .await;
    match ran {
        Ok(_) => world.oracle.borrow_mut().stats.claims += 1,
        Err(BoardError::Rejected(_)) => world.oracle.borrow_mut().stats.lost += 1,
        Err(error) => world.note(&error),
    }
}

pub async fn desk(world: &World, epoch: u64) {
    let writer = if world.shape.stable {
        format!("{}+desk", super::access::OWNER)
    } else {
        format!("{}+desk-e{epoch}", super::access::OWNER)
    };
    let mut board = world.open(&writer).await;
    let plan = &world.shape.tasks;
    let mut rng = Rng::derive(plan.desk.wrapping_add(epoch), "board desk");
    let mut acts = plan.acts;
    let mut since = BTreeMap::new();
    loop {
        if let Err(error) = board.refresh().await {
            world.note(&error);
            world.nap(world.shape.poll_ms).await;
            continue;
        }
        if world.planned(&board) {
            return;
        }
        let now = world.now_ms();
        let tasks = board
            .fold()
            .tasks()
            .iter()
            .map(|(id, task)| (id.clone(), task.phase.clone(), task.spec.terms.on_fail))
            .collect::<Vec<_>>();
        for (id, phase, on_fail) in &tasks {
            let op = match phase {
                Phase::Blocked { .. } => {
                    let at = *since.entry(id.clone()).or_insert(now);
                    (now >= at + plan.hold_ms).then(|| Op::Tasks(TaskOp::Unblock { task: id.clone() }))
                }
                Phase::Failed { .. } if *on_fail == OnFail::Wait => {
                    let mut reopens = world.reopens.borrow_mut();
                    let tries = reopens.entry(id.clone()).or_default();
                    *tries += 1;
                    Some(if *tries <= TRIES {
                        Op::Tasks(TaskOp::Reopen { task: id.clone() })
                    } else {
                        Op::Cancel { task: id.clone(), reason: "gave up after retries".to_owned() }
                    })
                }
                _ => None,
            };
            let Some(op) = op else {
                continue;
            };
            let reopen = matches!(op, Op::Tasks(TaskOp::Reopen { .. }));
            match world.submit(&mut board, op).await {
                Ok(_) if reopen => world.oracle.borrow_mut().stats.reopens += 1,
                Ok(_) => {
                    since.remove(id);
                }
                Err(error) => world.note(&error),
            }
        }
        if acts > 0 && !tasks.is_empty() && rng.chance(plan.act_ppm) {
            acts -= 1;
            let task = tasks[rng.pick(tasks.len())].0.clone();
            let op = match rng.pick(4) {
                0 => TaskOp::Unblock { task },
                1 => TaskOp::Reopen { task },
                2 => TaskOp::Block {
                    task,
                    claim: None,
                    kind: "awaiting_input".to_owned(),
                    reason: "which repository?".to_owned(),
                },
                _ => TaskOp::Assign {
                    task,
                    to: rng.chance(500_000).then(|| format!("agent:w{}", rng.below(world.shape.workers))),
                },
            };
            if let Err(error) = world.submit(&mut board, Op::Tasks(op)).await {
                world.note(&error);
            }
        }
        world.nap(world.shape.poll_ms).await;
    }
}

impl Reference {
    pub(super) fn finished(&self, id: &str) -> bool {
        match self.tasks.get(id).map(|(_, _, phase)| phase) {
            Some(Expect::Done | Expect::Cancelled) => true,
            Some(Expect::Failed) => !self.waits.contains(id),
            _ => false,
        }
    }

    pub(super) fn dooms(&self, above: &str) -> bool {
        match self.tasks.get(above).map(|(_, _, phase)| phase) {
            Some(Expect::Cancelled) => true,
            Some(Expect::Failed) => !self.waits.contains(above),
            _ => false,
        }
    }

    pub(super) fn terms(&mut self, task: &NewTask) {
        if let Some(to) = &task.terms.assignee {
            self.assignees.insert(task.id.0.clone(), to.clone());
        }
        if task.terms.on_fail == OnFail::Wait {
            self.waits.insert(task.id.0.clone());
        }
    }

    pub(super) fn steer(&mut self, op: TaskOp, now: u64) -> bool {
        let phase = |model: &Self, task: &TaskId| model.tasks.get(&task.0).map(|(_, _, phase)| phase.clone());
        match op {
            TaskOp::Block { task, claim, .. } => {
                let may = match (phase(self, &task), &claim) {
                    (Some(Expect::Open), None) => true,
                    (Some(Expect::Claimed(held, _)), Some(claim)) => held == claim.0,
                    (Some(Expect::Claimed(_, until)), None) => until <= now,
                    _ => false,
                };
                if may {
                    self.tasks.get_mut(&task.0).expect("present").2 = Expect::Blocked;
                }
                may
            }
            TaskOp::Unblock { task } if phase(self, &task) == Some(Expect::Blocked) => {
                self.tasks.get_mut(&task.0).expect("present").2 = Expect::Open;
                true
            }
            TaskOp::Reopen { task } if phase(self, &task) == Some(Expect::Failed) && self.waits.contains(&task.0) => {
                self.tasks.get_mut(&task.0).expect("present").2 = Expect::Open;
                *self.reopens.entry(task.0).or_default() += 1;
                true
            }
            TaskOp::Assign { task, to } if self.tasks.contains_key(&task.0) && !self.finished(&task.0) => {
                match to {
                    Some(to) => {
                        self.assignees.insert(task.0, to);
                    }
                    None => {
                        self.assignees.remove(&task.0);
                    }
                }
                true
            }
            TaskOp::Batch { name, tasks } => self.batch(&name, &tasks),
            TaskOp::Unblock { .. } | TaskOp::Reopen { .. } | TaskOp::Assign { .. } => false,
        }
    }

    fn batch(&mut self, name: &TaskId, tasks: &[Planned]) -> bool {
        let id = |key: &str| format!("{}/{key}", name.0);
        let keys = tasks.iter().map(|task| task.key.as_str()).collect::<BTreeSet<_>>();
        if tasks.is_empty()
            || tasks.len() > nanocode_board::tasks::BATCH
            || keys.len() != tasks.len()
            || tasks.iter().any(|task| self.tasks.contains_key(&id(&task.key)))
        {
            return false;
        }
        for (index, task) in tasks.iter().enumerate() {
            let parent = match &task.parent {
                None => true,
                Some(Need::Task(parent)) => self.tasks.contains_key(&parent.0),
                Some(Need::Key(key)) => tasks[..index].iter().any(|before| before.key == *key),
            };
            let needs = task.needs.iter().all(|need| match need {
                Need::Task(need) => self.tasks.contains_key(&need.0),
                Need::Key(key) => keys.contains(key.as_str()),
            });
            if !parent || !needs {
                return false;
            }
        }
        let mut left = tasks.iter().collect::<Vec<_>>();
        loop {
            let before = left.len();
            let waiting = left.iter().map(|task| task.key.clone()).collect::<BTreeSet<_>>();
            left.retain(|task| task.needs.iter().any(|need| matches!(need, Need::Key(key) if waiting.contains(key))));
            if left.is_empty() {
                break;
            }
            if left.len() == before {
                return false;
            }
        }
        let resolve = |need: &Need| match need {
            Need::Task(task) => task.0.clone(),
            Need::Key(key) => id(key),
        };
        for task in tasks {
            self.tasks.insert(
                id(&task.key),
                (task.parent.as_ref().map(resolve), task.needs.iter().map(resolve).collect(), Expect::Open),
            );
            if let Some(to) = &task.terms.assignee {
                self.assignees.insert(id(&task.key), to.clone());
            }
            if task.terms.on_fail == OnFail::Wait {
                self.waits.insert(id(&task.key));
            }
        }
        for task in tasks {
            let (parent, needs, _) = self.tasks[&id(&task.key)].clone();
            if parent.iter().chain(&needs).any(|above| self.dooms(above)) {
                self.doom(&id(&task.key));
            }
        }
        true
    }
}
