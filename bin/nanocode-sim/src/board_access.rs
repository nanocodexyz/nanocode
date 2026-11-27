use std::collections::{BTreeMap, BTreeSet};

use nanocode_board::{
    BoardError, Event, EventId, Fold, Op, Rejection, Say, TaskId,
    access::{Actor, Can, Grant, Identity, Verifier, Who},
    steer::SteerOp,
    tasks::{Need, Planned, TaskOp, Terms},
    watch::{self, Key, Step},
};
use nanocode_turnlog::LogHost;

use super::{Expect, Reference, SCOPE, World, watching};
use crate::rng::Rng;

pub const OWNER: &str = "user:lead";
pub const EVE: &str = "user:eve";
pub const THIEF: &str = "agent:w9";
pub const IVY: &str = "user:ivy";
pub const SOURCE: &str = "source:github";
pub const LOCKED: &str = "repo0";

struct Credentials;

impl Verifier for Credentials {
    fn verify(&self, credential: &str) -> Option<String> {
        credential.strip_prefix("token:").map(str::to_owned)
    }
}

pub fn identity(actor: &str) -> Identity {
    let (principal, profile) = match actor.split_once('+') {
        Some((principal, profile)) => (principal, Some(profile)),
        None => (actor, None),
    };
    let verified = Identity::verify(&Credentials, &format!("token:{principal}"))
        .expect("a simulated principal verifies");
    profile.map_or(verified.clone(), |name| {
        verified
            .profile(name)
            .expect("a simulated profile is valid")
    })
}

fn grant(who: Who, pattern: &str, can: &[Can]) -> Grant {
    Grant {
        who: vec![who],
        can: BTreeMap::from([(pattern.to_owned(), can.to_vec())]),
    }
}

pub fn grants() -> Vec<(&'static str, Grant)> {
    vec![
        (
            "grant:agents",
            grant(Who::Kind("agent".to_owned()), "*", &[Can::Label]),
        ),
        (
            "grant:sources",
            grant(Who::Kind("source".to_owned()), "*", &[Can::Read, Can::Post]),
        ),
        (
            "grant:eve",
            grant(
                Who::Principal(EVE.to_owned()),
                "*",
                &[Can::Read, Can::Label],
            ),
        ),
        (
            "grant:lock",
            grant(Who::Principal(EVE.to_owned()), &format!("{LOCKED}/"), &[]),
        ),
        (
            "grant:temp",
            grant(Who::Principal(EVE.to_owned()), "goal", &[Can::Post]),
        ),
        (
            "grant:ivy",
            grant(Who::Principal(IVY.to_owned()), "*", &[Can::Read]),
        ),
        (
            "grant:rank",
            grant(Who::Principal(IVY.to_owned()), "*", &[Can::Rank]),
        ),
        (
            "grant:chat",
            Grant {
                who: super::messages::WRITERS[..2]
                    .iter()
                    .map(|who| Who::Principal((*who).to_owned()))
                    .collect(),
                can: BTreeMap::from([(
                    "*".to_owned(),
                    vec![Can::Read, Can::Post, Can::React, Can::Label],
                )]),
            },
        ),
        (
            "grant:watch",
            Grant {
                who: [watching::TALLY, watching::ECHO, watching::CLOCK]
                    .map(|name| Who::Principal(name.to_owned()))
                    .to_vec(),
                can: BTreeMap::from([("*".to_owned(), vec![Can::Read, Can::Post])]),
            },
        ),
    ]
}

pub fn keyed(key: &str) -> EventId {
    EventId(format!("{OWNER}#{key}"))
}

pub const REVOKE: &str = "revoke:temp";

pub async fn setup(world: &World) {
    let mut board = world.open(OWNER).await;
    let mut writes = vec![("charter", Op::Charter {})];
    writes.extend(
        grants()
            .into_iter()
            .map(|(key, grant)| (key, Op::Grant { grant })),
    );
    for (key, op) in writes {
        loop {
            match board.submit_keyed(key, world.now_ms(), op.clone()).await {
                Ok(_) | Err(BoardError::Rejected(Rejection::Exists)) => break,
                Err(error @ BoardError::Rejected(_)) => {
                    world
                        .oracle
                        .borrow_mut()
                        .wrong
                        .push(format!("the owner could not write {key}: {error}"));
                    break;
                }
                Err(error) => {
                    world.note(&error);
                    world.nap(world.shape.poll_ms).await;
                }
            }
        }
    }
}

pub async fn revoke(world: &World) {
    let mut board = world.open(OWNER).await;
    loop {
        match board
            .revoke(REVOKE, world.now_ms(), keyed("grant:temp"))
            .await
        {
            Ok(_) => return,
            Err(error @ BoardError::Rejected(_)) => {
                world
                    .oracle
                    .borrow_mut()
                    .wrong
                    .push(format!("the owner could not revoke: {error}"));
                return;
            }
            Err(error) => {
                world.note(&error);
                world.nap(world.shape.poll_ms).await;
            }
        }
    }
}

async fn raw(world: &World) -> (Vec<(u64, String)>, Fold) {
    let rows = world
        .host
        .inner
        .scan(SCOPE, 0, i64::MAX as u64)
        .await
        .unwrap_or_default();
    let mut fold = Fold::new();
    for (seq, body) in &rows {
        let _ = fold.apply(*seq, body);
    }
    (rows, fold)
}

async fn forge(world: &World, by: &str, id: String, op: Op) {
    loop {
        let (rows, _) = raw(world).await;
        let next = rows.last().map_or(1, |(seq, _)| seq + 1);
        let event = Event {
            id: EventId(id.clone()),
            by: by.to_owned(),
            at_ms: world.now_ms(),
            op: op.clone(),
        };
        let body = serde_json::to_string(&event).expect("an event serializes");
        if world
            .host
            .inner
            .append(SCOPE, &[(next, body)])
            .await
            .is_ok()
        {
            world.oracle.borrow_mut().stats.forged += 1;
            return;
        }
    }
}

async fn probe(world: &World, epoch: u64) {
    let writer = if world.shape.stable {
        EVE.to_owned()
    } else {
        format!("{EVE}+e{epoch}")
    };
    let mut board = world.open(&writer).await;
    for task in [
        TaskId(LOCKED.to_owned()),
        TaskId(format!("{LOCKED}/nowhere")),
    ] {
        for _ in 0..8 {
            let sent = board
                .submit(
                    world.now_ms(),
                    Op::Reply {
                        task: task.clone(),
                        body: "probe".to_owned(),
                    },
                )
                .await;
            match sent {
                Err(BoardError::Rejected(Rejection::UnknownTask)) => {
                    if board.task(&task).is_some() {
                        world
                            .oracle
                            .borrow_mut()
                            .wrong
                            .push(format!("{EVE} reads locked {task}"));
                    }
                    break;
                }
                Err(BoardError::Store(_)) => world.nap(world.shape.poll_ms).await,
                other => {
                    world.oracle.borrow_mut().wrong.push(format!(
                        "{EVE} probing locked {task} got {other:?}, not an unknown task"
                    ));
                    break;
                }
            }
        }
    }
}

pub async fn intruder(world: &World, epoch: u64) {
    let mut rng = Rng::derive(world.shape.seed ^ epoch.rotate_left(32), "board intruder");
    if epoch == 0
        && let Some(hook) = world.shape.hooks.iter().find(|hook| hook.review.is_some())
    {
        let reply = Op::Reply {
            task: super::pr_task(hook.pr),
            body: "squat".to_owned(),
        };
        forge(world, EVE, format!("{SOURCE}#{}", hook.key), reply).await;
    }
    let span = world.shape.lease_ms * 4;
    for round in 0..rng.range(1, 7) {
        world.nap(rng.range(0, span)).await;
        let (_, fold) = raw(world).await;
        let tasks = fold.tasks().keys().cloned().collect::<Vec<_>>();
        let key = |by: &str| format!("{by}#forged.e{epoch}.{round}");
        let action = rng.range(0, 12);
        match action {
            0 | 1 if !tasks.is_empty() => {
                let task = tasks[rng.pick(tasks.len())].clone();
                let op = if action == 0 {
                    Op::Claim {
                        task,
                        lease_ms: world.shape.lease_ms,
                        fence: None,
                        cap: None,
                    }
                } else {
                    Op::Cancel {
                        task,
                        reason: "forged".to_owned(),
                    }
                };
                forge(world, EVE, key(EVE), op).await;
            }
            2 => {
                let grant = Grant {
                    who: vec![Who::Principal(EVE.to_owned())],
                    can: BTreeMap::from([("*".to_owned(), vec![Can::Manage])]),
                };
                forge(world, EVE, key(EVE), Op::Grant { grant }).await;
            }
            3 if fold.outcome(&keyed(REVOKE)).is_some_and(Result::is_ok) => {
                let op = Op::Reply {
                    task: TaskId("goal".to_owned()),
                    body: "after revoke".to_owned(),
                };
                forge(world, EVE, key(EVE), op).await;
            }
            4 => {
                let held = fold
                    .tasks()
                    .iter()
                    .find_map(|(id, task)| match &task.phase {
                        nanocode_board::Phase::Claimed { claim, .. } => {
                            Some((id.clone(), claim.clone()))
                        }
                        _ => None,
                    });
                if let Some((task, claim)) = held {
                    let outcome = nanocode_board::Outcome::Done("stolen".to_owned());
                    forge(
                        world,
                        THIEF,
                        key(THIEF),
                        Op::Finish {
                            task,
                            claim,
                            outcome,
                        },
                    )
                    .await;
                }
            }
            5 if fold.task(&TaskId("goal".to_owned())).is_some() => {
                let op = Op::Reply {
                    task: TaskId("goal".to_owned()),
                    body: "outside its source".to_owned(),
                };
                forge(world, SOURCE, key(SOURCE), op).await;
            }
            6 => {
                forge(
                    world,
                    EVE,
                    key(EVE),
                    Op::Revoke {
                        grant: keyed("grant:agents"),
                    },
                )
                .await
            }
            7 if fold
                .outcome(&keyed("grant:lock"))
                .is_some_and(Result::is_ok) =>
            {
                probe(world, epoch).await
            }
            8 if !tasks.is_empty() => {
                let task = tasks[rng.pick(tasks.len())].clone();
                let op = match rng.pick(3) {
                    0 => TaskOp::Assign {
                        task,
                        to: Some(EVE.to_owned()),
                    },
                    1 => TaskOp::Unblock { task },
                    _ => TaskOp::Batch {
                        name: TaskId("goal".to_owned()),
                        tasks: vec![Planned {
                            key: format!("forged-e{epoch}-{round}"),
                            parent: None,
                            spec: "forged".to_owned(),
                            labels: BTreeMap::new(),
                            needs: Vec::new(),
                            terms: Terms::default(),
                        }],
                    },
                };
                forge(world, EVE, key(EVE), Op::Tasks(op)).await;
            }
            9 => {
                let cursor = Key::Cursor {
                    actor: watching::TALLY.to_owned(),
                    scope: None,
                };
                let from = fold.position(&cursor);
                let to = raw(world).await.0.last().map_or(0, |(seq, _)| *seq);
                if to > from {
                    let step = Step::Consume {
                        key: cursor,
                        lease: None,
                        from,
                        to,
                        effects: Vec::new(),
                    };
                    forge(world, EVE, key(EVE), Op::Watch(step)).await;
                }
            }
            10 if !tasks.is_empty() => {
                let send = Op::Reply {
                    task: tasks[rng.pick(tasks.len())].clone(),
                    body: "forged".to_owned(),
                };
                let step = Step::Schedule {
                    id: format!("forged.e{epoch}.{round}"),
                    by: EVE.to_owned(),
                    send: Box::new(send),
                    first_ms: 0,
                    every_ms: None,
                    until_ms: None,
                    times: None,
                };
                forge(world, EVE, key(EVE), Op::Watch(step)).await;
            }
            11 => {
                if let Some(id) = fold.marks().schedules().keys().next().cloned() {
                    let step = Step::Unschedule {
                        id,
                        by: watching::CLOCK.to_owned(),
                    };
                    forge(world, EVE, key(EVE), Op::Watch(step)).await;
                }
            }
            _ => {}
        }
    }
    relabel(world, epoch, span).await;
    rank(world, epoch, span).await;
    reparent(world, epoch, span).await;
}

/// Share of forged moves that pick a task with children, which a move can put under one of
/// its own descendants.
const MOVE_PARENT_PPM: u64 = 700_000;

async fn reparent(world: &World, epoch: u64, span: u64) {
    let mut rng = Rng::derive(world.shape.seed ^ epoch.rotate_left(32), "board moves");
    for round in 0..rng.range(0, 4) {
        world.nap(rng.range(0, span / 2)).await;
        let (_, fold) = raw(world).await;
        let tasks = fold.tasks().keys().cloned().collect::<Vec<_>>();
        if tasks.is_empty() {
            continue;
        }
        // Aim most moves at a task with children: only moving one under its own child or
        // grandchild makes a cycle, and a leaf can only be moved under itself.
        let parents = tasks
            .iter()
            .filter_map(|id| fold.task(id).and_then(|task| task.spec.parent.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let task = if !parents.is_empty() && rng.chance(MOVE_PARENT_PPM) {
            parents[rng.pick(parents.len())].clone()
        } else {
            tasks[rng.pick(tasks.len())].clone()
        };
        let under = |id: &TaskId| watch::under(fold.tasks(), id, &task);
        let below = tasks
            .iter()
            .filter(|id| **id != task && under(id))
            .cloned()
            .collect::<Vec<_>>();
        let parent = match rng.pick(4) {
            0 => None,
            1 if below.is_empty() => Some(task.clone()),
            1 => Some(below[rng.pick(below.len())].clone()),
            2 => Some(TaskId(format!("{LOCKED}/nowhere"))),
            _ => Some(tasks[rng.pick(tasks.len())].clone()),
        };
        let cyclic = parent.as_ref().is_some_and(under);
        let by = [OWNER, OWNER, EVE, IVY, SOURCE][rng.pick(5)];
        let id = format!("{by}#moves.e{epoch}.{round}");
        forge(
            world,
            by,
            id.clone(),
            Op::Move {
                task: task.clone(),
                parent: parent.clone(),
            },
        )
        .await;
        let (_, fold) = raw(world).await;
        if cyclic && fold.outcome(&EventId(id)).is_some_and(Result::is_ok) {
            // A cycle makes every later walk up the parent links spin, so stop the run here.
            panic!("the board moved {task} under {parent:?}, which sits below it");
        }
    }
}

async fn rank(world: &World, epoch: u64, span: u64) {
    let mut rng = Rng::derive(world.shape.seed ^ epoch.rotate_left(32), "board ranks");
    for round in 0..rng.range(0, 6) {
        world.nap(rng.range(0, span)).await;
        let (_, fold) = raw(world).await;
        let tasks = fold.tasks().keys().cloned().collect::<Vec<_>>();
        if tasks.is_empty() {
            continue;
        }
        let task = tasks[rng.pick(tasks.len())].clone();
        let by = [OWNER, EVE, IVY, SOURCE][rng.pick(4)];
        let rank = if rng.chance(200_000) {
            None
        } else {
            Some(rng.range(0, 4))
        };
        forge(
            world,
            by,
            format!("{by}#rank.e{epoch}.{round}"),
            Op::Rank { task, rank },
        )
        .await;
    }
}

async fn relabel(world: &World, epoch: u64, span: u64) {
    let mut rng = Rng::derive(world.shape.seed ^ epoch.rotate_left(32), "board labels");
    for round in 0..rng.range(0, 5) {
        world.nap(rng.range(0, span)).await;
        let (_, fold) = raw(world).await;
        let live = rng.chance(500_000);
        let tasks = fold
            .tasks()
            .iter()
            .filter(|(_, task)| !live || !task.phase.terminal())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if tasks.is_empty() {
            continue;
        }
        let task = tasks[rng.pick(tasks.len())].clone();
        let by = [OWNER, EVE, IVY, SOURCE][rng.pick(4)];
        let keys = ["step", "k0", "k1"];
        let set = BTreeMap::from([(
            keys[1 + rng.pick(2)].to_owned(),
            format!("v{epoch}.{round}"),
        )]);
        let unset = BTreeSet::from([keys[rng.pick(3)].to_owned()]);
        forge(
            world,
            by,
            format!("{by}#labels.e{epoch}.{round}"),
            Op::Relabel { task, set, unset },
        )
        .await;
    }
}

fn spot(pattern: &str, target: &str) -> Option<(u8, usize)> {
    if pattern == "*" {
        return Some((0, 0));
    }
    let channel = target.trim_end_matches('/');
    if target == "*" {
        None
    } else if let Some(base) = pattern.strip_suffix('/') {
        (channel == base || channel.starts_with(pattern)).then_some((1, pattern.len()))
    } else {
        (pattern == target).then_some((2, pattern.len()))
    }
}

fn gives(held: Can, wanted: Can) -> bool {
    held == wanted
        || held == Can::Manage
        || (wanted == Can::Read && matches!(held, Can::React | Can::Label | Can::Sync | Can::Rank))
}

fn split(by: &str) -> (&str, Option<&str>) {
    by.split_once('+')
        .map_or((by, None), |(principal, profile)| {
            (principal, Some(profile))
        })
}

fn principal_ok(principal: &str) -> bool {
    principal.split_once(':').is_some_and(|(kind, rest)| {
        !kind.is_empty()
            && !rest.is_empty()
            && kind
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && principal
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b":_-/@".contains(&byte))
    })
}

fn actor_ok(by: &str) -> bool {
    let (principal, profile) = split(by);
    principal_ok(principal)
        && profile.is_none_or(|profile| {
            !profile.is_empty()
                && profile
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
}

pub fn owns(by: &str, id: &str) -> bool {
    actor_ok(by)
        && id
            .strip_prefix(by)
            .is_some_and(|rest| rest.len() > 1 && (rest.starts_with('.') || rest.starts_with('#')))
}

fn covers(by: &str, other: &str) -> bool {
    let (mine, profile) = split(by);
    let (theirs, their_profile) = split(other);
    mine == theirs
        && match (profile, their_profile) {
            (None, _) => true,
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            (Some(_), None) => false,
        }
}

#[derive(Default)]
pub struct Gate {
    owner: Option<String>,
    grants: BTreeMap<String, Grant>,
    posters: BTreeMap<String, String>,
    holders: BTreeMap<String, String>,
    roots: BTreeMap<String, (String, String)>,
}

impl Gate {
    fn level(&self, by: &str, target: &str, everyone: bool) -> BTreeSet<Can> {
        let (principal, _) = split(by);
        let kind = principal.split(':').next().unwrap_or_default();
        let mut best = None;
        let mut held = BTreeSet::new();
        for grant in self.grants.values() {
            let close = grant
                .who
                .iter()
                .filter_map(|who| match who {
                    Who::Principal(id) if id == principal => Some(2),
                    Who::Kind(named) if named == kind => Some(1),
                    Who::Everyone if everyone => Some(0),
                    _ => None,
                })
                .max();
            let Some(close) = close else {
                continue;
            };
            for (pattern, cans) in &grant.can {
                let Some(rank) = spot(pattern, target) else {
                    continue;
                };
                let here = (rank, close);
                if best.is_none_or(|best| here > best) {
                    best = Some(here);
                    held.clear();
                }
                if best == Some(here) {
                    held.extend(cans.iter().copied());
                }
            }
        }
        held
    }

    fn has(&self, by: &str, target: &str, can: Can, everyone: bool) -> bool {
        let (principal, _) = split(by);
        if self.owner.as_deref() == Some(principal) {
            return true;
        }
        if let Some(source) = principal.strip_prefix("source:") {
            let channel = target.trim_end_matches('/');
            if target == "*" || (channel != source && !channel.starts_with(&format!("{source}/"))) {
                return false;
            }
        }
        self.level(by, target, everyone)
            .into_iter()
            .any(|held| gives(held, can))
    }

    pub fn can(&self, by: &str, target: &str, can: Can) -> bool {
        self.has(by, target, can, true)
    }

    fn reaches(&self, by: &str, pattern: &str) -> bool {
        self.has(by, pattern, Can::Manage, true)
            && self
                .grants
                .values()
                .flat_map(|grant| grant.can.keys())
                .filter(|under| *under != pattern && spot(pattern, under).is_some())
                .all(|under| self.has(by, under, Can::Manage, false))
    }

    fn chain(
        &self,
        by: &str,
        task: &TaskId,
        tasks: &BTreeMap<String, (Option<String>, Vec<String>, Expect)>,
    ) -> bool {
        let mut at = Some(task.0.clone());
        while let Some(id) = at {
            if self
                .posters
                .get(&id)
                .is_some_and(|poster| covers(by, poster))
            {
                return true;
            }
            at = tasks.get(&id).and_then(|(parent, _, _)| parent.clone());
        }
        false
    }

    pub fn allows(
        &self,
        by: &str,
        op: &Op,
        tasks: &BTreeMap<String, (Option<String>, Vec<String>, Expect)>,
    ) -> bool {
        if !actor_ok(by) {
            return false;
        }
        if let Op::Charter {} = op {
            return self.owner.is_none() && split(by).1.is_none();
        }
        if self.owner.is_none() {
            return false;
        }
        let read = |task: &TaskId| self.can(by, &task.0, Can::Read);
        match op {
            Op::Charter {} => false,
            Op::Epoch { .. } | Op::TextLimit { .. } => self.can(by, "*", Can::Manage),
            Op::Post { task } => {
                task.parent.iter().chain(&task.needs).all(read)
                    && self.can(by, &task.id.0, Can::Read)
                    && self.can(by, &task.id.0, Can::Post)
            }
            Op::Claim { task, .. } => self.can(by, &task.0, Can::Label),
            Op::Renew { task, claim, .. }
            | Op::Release { task, claim, .. }
            | Op::Finish { task, claim, .. }
            | Op::Steer(SteerOp::Heed { task, claim, .. })
            | Op::Tasks(TaskOp::Block {
                task,
                claim: Some(claim),
                ..
            }) => {
                let stolen = matches!(tasks.get(&task.0), Some((_, _, Expect::Claimed(held, _))) if *held == claim.0)
                    && self
                        .holders
                        .get(&task.0)
                        .is_some_and(|holder| !covers(by, holder));
                self.can(by, &task.0, Can::Label) && !stolen
            }
            Op::Cancel { task, .. }
            | Op::Tasks(
                TaskOp::Block {
                    task, claim: None, ..
                }
                | TaskOp::Unblock { task }
                | TaskOp::Reopen { task }
                | TaskOp::Assign { task, .. },
            ) => read(task) && (self.chain(by, task, tasks) || self.can(by, &task.0, Can::Manage)),
            Op::Tasks(TaskOp::Batch { name, tasks }) => tasks.iter().all(|planned| {
                let id = |need: &Need| match need {
                    Need::Task(task) => task.0.clone(),
                    Need::Key(key) => format!("{}/{key}", name.0),
                };
                let own = format!("{}/{}", name.0, planned.key);
                planned
                    .parent
                    .iter()
                    .chain(&planned.needs)
                    .all(|above| self.can(by, &id(above), Can::Read))
                    && self.can(by, &own, Can::Read)
                    && self.can(by, &own, Can::Post)
            }),
            Op::Reply { task, .. } | Op::Steer(SteerOp::Send { task, .. }) => {
                read(task) && self.can(by, &task.0, Can::Post)
            }
            Op::Relabel { task, .. } => {
                read(task) && (self.chain(by, task, tasks) || self.can(by, &task.0, Can::Label))
            }
            Op::Rank { task, .. } => read(task) && self.can(by, &task.0, Can::Rank),
            Op::Move { task, parent } => {
                read(task)
                    && parent.iter().all(read)
                    && self.can(by, &parent.as_ref().unwrap_or(task).0, Can::Post)
                    && (self.chain(by, task, tasks) || self.can(by, &task.0, Can::Manage))
            }
            Op::Say { say } => self.says(by, say, tasks),
            Op::Sync { sync } => match sync.task() {
                Some(task) => read(task) && self.can(by, &task.0, sync.needs()),
                None => self.can(by, "*", Can::Sync),
            },
            Op::Grant { grant } => {
                !grant.who.is_empty()
                    && !grant.can.is_empty()
                    && grant.who.iter().all(|who| match who {
                        Who::Principal(id) => principal_ok(id),
                        Who::Kind(kind) => !kind.is_empty(),
                        Who::Everyone => true,
                    })
                    && grant.can.keys().all(|pattern| {
                        let base = pattern.strip_suffix('/').unwrap_or(pattern);
                        (pattern == "*"
                            || (!base.is_empty()
                                && !base.split('/').any(str::is_empty)
                                && !pattern.contains('*')))
                            && self.reaches(by, pattern)
                    })
            }
            Op::Revoke { grant } => self
                .grants
                .get(&grant.0)
                .is_some_and(|stored| stored.can.keys().all(|pattern| self.reaches(by, pattern))),
            Op::Watch(step) => self.allows_step(by, step, tasks),
        }
    }

    fn allows_step(
        &self,
        by: &str,
        step: &Step,
        tasks: &BTreeMap<String, (Option<String>, Vec<String>, Expect)>,
    ) -> bool {
        let acts = |named: &str| covers(named, by) || covers(by, named);
        let see = |task: &TaskId| self.can(by, &task.0, Can::Read);
        match step {
            Step::Consume { key, effects, .. } => {
                let (named, thread) = match key {
                    Key::Cursor { actor, scope } => (actor, scope.as_ref()),
                    Key::Watcher { watcher, thread } => (watcher, Some(thread)),
                };
                acts(named)
                    && thread.is_none_or(see)
                    && effects.iter().all(|op| self.allows(by, op, tasks))
            }
            Step::Watch {
                watcher, filter, ..
            } => acts(watcher) && filter.under.as_ref().is_none_or(see),
            Step::Unwatch { watcher } => acts(watcher),
            Step::Take {
                watcher, thread, ..
            }
            | Step::Fail {
                watcher, thread, ..
            }
            | Step::Bury {
                watcher, thread, ..
            } => acts(watcher) && see(thread),
            Step::Schedule { by: made, send, .. } => made == by && self.allows(by, send, tasks),
            Step::Fire { .. } => true,
            Step::Unschedule { by: made, .. } => made == by,
        }
    }

    fn home(
        &self,
        id: &str,
        tasks: &BTreeMap<String, (Option<String>, Vec<String>, Expect)>,
    ) -> Option<String> {
        if tasks.contains_key(id) {
            return Some(id.to_owned());
        }
        let (root, channel) = self.roots.get(id)?;
        Some(if tasks.contains_key(root) {
            root.clone()
        } else {
            channel.clone()
        })
    }

    pub fn says(
        &self,
        by: &str,
        say: &Say,
        tasks: &BTreeMap<String, (Option<String>, Vec<String>, Expect)>,
    ) -> bool {
        if !actor_ok(by) || self.owner.is_none() {
            return false;
        }
        let (place, can) = match say {
            Say::Post { channel, re, .. } => (
                re.as_ref()
                    .and_then(|root| self.home(&root.0, tasks))
                    .unwrap_or_else(|| channel.clone()),
                Can::Post,
            ),
            Say::Edit { message, .. }
            | Say::Delete { message }
            | Say::React { message, .. }
            | Say::Label { message, .. } => {
                let Some(home) = self.home(&message.0, tasks) else {
                    return false;
                };
                let can = match say {
                    Say::React { .. } => Can::React,
                    Say::Label { .. } => Can::Label,
                    _ => Can::Post,
                };
                (home, can)
            }
            Say::Schema { name, .. } => (format!("schemas/{name}"), Can::Post),
            Say::File { .. } => ("files".to_owned(), Can::Post),
        };
        self.can(by, &place, Can::Read) && self.can(by, &place, can)
    }

    pub fn heard(&mut self, id: &str, say: &Say) {
        if let Say::Post { channel, re, .. } = say {
            let root = re.as_ref().map_or(id, |root| &root.0);
            let channel = self
                .roots
                .get(root)
                .map_or_else(|| channel.clone(), |(_, channel)| channel.clone());
            self.roots
                .entry(id.to_owned())
                .or_insert((root.to_owned(), channel));
        }
    }

    pub fn record(&mut self, by: &str, id: &str, op: &Op) {
        match op {
            Op::Charter {} => self.owner = Some(split(by).0.to_owned()),
            Op::Grant { grant } => {
                self.grants.insert(id.to_owned(), grant.clone());
            }
            Op::Revoke { grant } => {
                self.grants.remove(&grant.0);
            }
            Op::Post { task } => {
                self.posters
                    .entry(task.id.0.clone())
                    .or_insert_with(|| by.to_owned());
            }
            Op::Claim { task, .. } => {
                self.holders.insert(task.0.clone(), by.to_owned());
            }
            Op::Tasks(TaskOp::Batch { name, tasks }) => {
                for task in tasks {
                    self.posters
                        .insert(format!("{}/{}", name.0, task.key), by.to_owned());
                }
            }
            _ => {}
        }
    }
}

pub fn views(
    fold: &Fold,
    gate: &Gate,
    tasks: &BTreeMap<String, (Option<String>, Vec<String>, Expect)>,
) -> Result<(), String> {
    for reader in [EVE, SOURCE, THIEF] {
        let actor = Actor::parse(reader).map_err(|error| format!("{reader}: {error:?}"))?;
        let shown = fold
            .view(&actor)
            .map(|(id, _)| id.0.clone())
            .collect::<BTreeSet<_>>();
        let allowed = tasks
            .keys()
            .filter(|id| gate.can(reader, id, Can::Read))
            .cloned()
            .collect::<BTreeSet<_>>();
        if shown != allowed {
            let extra = shown.difference(&allowed).take(3).collect::<Vec<_>>();
            let missing = allowed.difference(&shown).take(3).collect::<Vec<_>>();
            return Err(format!(
                "{reader} sees the wrong tasks: extra {extra:?}, missing {missing:?}"
            ));
        }
    }
    Ok(())
}

pub fn audit(fold: &Fold, model: &Reference) -> Result<(), String> {
    for (id, accepted) in &model.verdicts {
        let applied = fold.outcome(&EventId(id.clone())).map(Result::is_ok);
        if applied != Some(*accepted) {
            return Err(format!(
                "event {id}: the board says {applied:?}, the reference model says {accepted}"
            ));
        }
    }
    for (id, task) in fold.tasks() {
        let labels = model.labels.get(&id.0).cloned().unwrap_or_default();
        if task.spec.labels != labels {
            return Err(format!(
                "{id}: the board's labels are {:?}, the reference model's {labels:?}",
                task.spec.labels
            ));
        }
        let rank = model.ranks.get(&id.0).copied();
        if task.rank != rank {
            return Err(format!(
                "{id}: the board ranks it {:?}, the reference model {rank:?}",
                task.rank
            ));
        }
    }
    for (id, task) in fold.tasks() {
        let parent = model
            .tasks
            .get(&id.0)
            .and_then(|(parent, _, _)| parent.as_deref());
        if task.spec.parent.as_ref().map(|parent| parent.0.as_str()) != parent {
            return Err(format!(
                "{id}: the board's parent is {:?}, the reference model's {parent:?}",
                task.spec.parent
            ));
        }
        if let Some(parent) = &task.spec.parent
            && !fold.children(parent).any(|child| child == id)
        {
            return Err(format!(
                "{id} is missing from the children of its parent {parent}"
            ));
        }
        if let Some(child) = fold.children(id).find(|child| {
            fold.task(child)
                .is_none_or(|below| below.spec.parent.as_ref() != Some(id))
        }) {
            return Err(format!("{child} is listed under {id} but is not its child"));
        }
    }
    views(fold, &model.gate, &model.tasks)
}

pub fn live(world: &World) -> Result<(), String> {
    let rows = nanocode_sync::block_on(world.host.inner.scan(SCOPE, 0, i64::MAX as u64))
        .unwrap_or_default();
    let mut fold = Fold::new();
    for (seq, body) in &rows {
        fold.apply(*seq, body)
            .map_err(|error| format!("live refold: {error}"))?;
    }
    audit(&fold, &Reference::of(&rows)?)
}
