use std::collections::{BTreeMap, BTreeSet};

use nanocode_board::{
    BoardError, Event, EventId, Fold, Op, Rejection, TaskId,
    access::{Actor, Can, Grant, Identity, Verifier, Who},
};
use nanocode_turnlog::LogHost;

use super::{Expect, Reference, SCOPE, World};
use crate::rng::Rng;

pub const OWNER: &str = "user:lead";
pub const EVE: &str = "user:eve";
pub const THIEF: &str = "agent:w9";
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
    let verified = Identity::verify(&Credentials, &format!("token:{principal}")).expect("a simulated principal verifies");
    profile.map_or(verified.clone(), |name| verified.profile(name).expect("a simulated profile is valid"))
}

fn grant(who: Who, pattern: &str, can: &[Can]) -> Grant {
    Grant {
        who: vec![who],
        can: BTreeMap::from([(pattern.to_owned(), can.to_vec())]),
    }
}

pub fn grants() -> Vec<(&'static str, Grant)> {
    vec![
        ("grant:agents", grant(Who::Kind("agent".to_owned()), "*", &[Can::Label])),
        ("grant:sources", grant(Who::Kind("source".to_owned()), "*", &[Can::Read, Can::Post])),
        ("grant:eve", grant(Who::Principal(EVE.to_owned()), "*", &[Can::Read, Can::Label])),
        ("grant:lock", grant(Who::Principal(EVE.to_owned()), &format!("{LOCKED}/"), &[])),
        ("grant:temp", grant(Who::Principal(EVE.to_owned()), "goal", &[Can::Post])),
    ]
}

pub fn keyed(key: &str) -> EventId {
    EventId(format!("{OWNER}#{key}"))
}

pub const REVOKE: &str = "revoke:temp";

pub async fn setup(world: &World) {
    let mut board = world.open(OWNER).await;
    let mut writes = vec![("charter", Op::Charter {})];
    writes.extend(grants().into_iter().map(|(key, grant)| (key, Op::Grant { grant })));
    for (key, op) in writes {
        loop {
            match board.submit_keyed(key, world.now_ms(), op.clone()).await {
                Ok(_) | Err(BoardError::Rejected(Rejection::Exists)) => break,
                Err(error @ BoardError::Rejected(_)) => {
                    world.oracle.borrow_mut().wrong.push(format!("the owner could not write {key}: {error}"));
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
        match board.revoke(REVOKE, world.now_ms(), keyed("grant:temp")).await {
            Ok(_) => return,
            Err(error @ BoardError::Rejected(_)) => {
                world.oracle.borrow_mut().wrong.push(format!("the owner could not revoke: {error}"));
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
    let rows = world.host.inner.scan(SCOPE, 0, i64::MAX as u64).await.unwrap_or_default();
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
        let event = Event { id: EventId(id.clone()), by: by.to_owned(), at_ms: world.now_ms(), op: op.clone() };
        let body = serde_json::to_string(&event).expect("an event serializes");
        if world.host.inner.append(SCOPE, &[(next, body)]).await.is_ok() {
            world.oracle.borrow_mut().stats.forged += 1;
            return;
        }
    }
}

async fn probe(world: &World, epoch: u64) {
    let writer = if world.shape.stable { EVE.to_owned() } else { format!("{EVE}+e{epoch}") };
    let mut board = world.open(&writer).await;
    for task in [TaskId(LOCKED.to_owned()), TaskId(format!("{LOCKED}/nowhere"))] {
        for _ in 0..8 {
            let sent = board.submit(world.now_ms(), Op::Reply { task: task.clone(), body: "probe".to_owned() }).await;
            match sent {
                Err(BoardError::Rejected(Rejection::UnknownTask)) => {
                    if board.task(&task).is_some() {
                        world.oracle.borrow_mut().wrong.push(format!("{EVE} reads locked {task}"));
                    }
                    break;
                }
                Err(BoardError::Store(_)) => world.nap(world.shape.poll_ms).await,
                other => {
                    world.oracle.borrow_mut().wrong.push(format!("{EVE} probing locked {task} got {other:?}, not an unknown task"));
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
        let reply = Op::Reply { task: super::pr_task(hook.pr), body: "squat".to_owned() };
        forge(world, EVE, format!("{SOURCE}#{}", hook.key), reply).await;
    }
    let span = world.shape.lease_ms * 4;
    for round in 0..rng.range(1, 7) {
        world.nap(rng.range(0, span)).await;
        let (_, fold) = raw(world).await;
        let tasks = fold.tasks().keys().cloned().collect::<Vec<_>>();
        let key = |by: &str| format!("{by}#forged.e{epoch}.{round}");
        let action = rng.range(0, 8);
        match action {
            0 | 1 if !tasks.is_empty() => {
                let task = tasks[rng.pick(tasks.len())].clone();
                let op = if action == 0 {
                    Op::Claim { task, lease_ms: world.shape.lease_ms }
                } else {
                    Op::Cancel { task, reason: "forged".to_owned() }
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
                let op = Op::Reply { task: TaskId("goal".to_owned()), body: "after revoke".to_owned() };
                forge(world, EVE, key(EVE), op).await;
            }
            4 => {
                let held = fold.tasks().iter().find_map(|(id, task)| match &task.phase {
                    nanocode_board::Phase::Claimed { claim, .. } => Some((id.clone(), claim.clone())),
                    _ => None,
                });
                if let Some((task, claim)) = held {
                    let outcome = nanocode_board::Outcome::Done("stolen".to_owned());
                    forge(world, THIEF, key(THIEF), Op::Finish { task, claim, outcome }).await;
                }
            }
            5 if fold.task(&TaskId("goal".to_owned())).is_some() => {
                let op = Op::Reply { task: TaskId("goal".to_owned()), body: "outside its source".to_owned() };
                forge(world, SOURCE, key(SOURCE), op).await;
            }
            6 => forge(world, EVE, key(EVE), Op::Revoke { grant: keyed("grant:agents") }).await,
            7 if fold.outcome(&keyed("grant:lock")).is_some_and(Result::is_ok) => probe(world, epoch).await,
            _ => {}
        }
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
    held == wanted || held == Can::Manage || (wanted == Can::Read && matches!(held, Can::React | Can::Label | Can::Sync))
}

fn split(by: &str) -> (&str, Option<&str>) {
    by.split_once('+').map_or((by, None), |(principal, profile)| (principal, Some(profile)))
}

fn principal_ok(principal: &str) -> bool {
    principal.split_once(':').is_some_and(|(kind, rest)| {
        !kind.is_empty()
            && !rest.is_empty()
            && kind.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && principal.bytes().all(|byte| byte.is_ascii_alphanumeric() || b":_-/@".contains(&byte))
    })
}

fn actor_ok(by: &str) -> bool {
    let (principal, profile) = split(by);
    principal_ok(principal)
        && profile.is_none_or(|profile| !profile.is_empty() && profile.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)))
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
        self.level(by, target, everyone).into_iter().any(|held| gives(held, can))
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

    pub fn allows(&self, by: &str, op: &Op, tasks: &BTreeMap<String, (Option<String>, Vec<String>, Expect)>) -> bool {
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
            Op::Post { task } => {
                task.parent.iter().chain(&task.needs).all(read)
                    && self.can(by, &task.id.0, Can::Read)
                    && self.can(by, &task.id.0, Can::Post)
            }
            Op::Claim { task, .. } => self.can(by, &task.0, Can::Label),
            Op::Renew { task, claim, .. } | Op::Release { task, claim, .. } | Op::Finish { task, claim, .. } => {
                let stolen = matches!(tasks.get(&task.0), Some((_, _, Expect::Claimed(held, _))) if *held == claim.0)
                    && self.holders.get(&task.0).is_some_and(|holder| !covers(by, holder));
                self.can(by, &task.0, Can::Label) && !stolen
            }
            Op::Cancel { task, .. } => {
                let mut chain = false;
                let mut at = Some(task.0.clone());
                while let Some(id) = at {
                    chain |= self.posters.get(&id).is_some_and(|poster| covers(by, poster));
                    at = tasks.get(&id).and_then(|(parent, _, _)| parent.clone());
                }
                read(task) && (chain || self.can(by, &task.0, Can::Manage))
            }
            Op::Reply { task, .. } => read(task) && self.can(by, &task.0, Can::Post),
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
                        (pattern == "*" || (!base.is_empty() && !base.split('/').any(str::is_empty) && !pattern.contains('*')))
                            && self.reaches(by, pattern)
                    })
            }
            Op::Revoke { grant } => self
                .grants
                .get(&grant.0)
                .is_some_and(|stored| stored.can.keys().all(|pattern| self.reaches(by, pattern))),
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
                self.posters.insert(task.id.0.clone(), by.to_owned());
            }
            Op::Claim { task, .. } => {
                self.holders.insert(task.0.clone(), by.to_owned());
            }
            _ => {}
        }
    }
}

pub fn views(fold: &Fold, gate: &Gate, tasks: &BTreeMap<String, (Option<String>, Vec<String>, Expect)>) -> Result<(), String> {
    for reader in [EVE, SOURCE, THIEF] {
        let actor = Actor::parse(reader).map_err(|error| format!("{reader}: {error:?}"))?;
        let shown = fold.view(&actor).map(|(id, _)| id.0.clone()).collect::<BTreeSet<_>>();
        let allowed = tasks.keys().filter(|id| gate.can(reader, id, Can::Read)).cloned().collect::<BTreeSet<_>>();
        if shown != allowed {
            let extra = shown.difference(&allowed).take(3).collect::<Vec<_>>();
            let missing = allowed.difference(&shown).take(3).collect::<Vec<_>>();
            return Err(format!("{reader} sees the wrong tasks: extra {extra:?}, missing {missing:?}"));
        }
    }
    Ok(())
}

pub fn audit(fold: &Fold, model: &Reference) -> Result<(), String> {
    for (id, accepted) in &model.verdicts {
        let applied = fold.outcome(&EventId(id.clone())).map(Result::is_ok);
        if applied != Some(*accepted) {
            return Err(format!("event {id}: the board says {applied:?}, the reference model says {accepted}"));
        }
    }
    views(fold, &model.gate, &model.tasks)
}

pub fn live(world: &World) -> Result<(), String> {
    let rows = nanocode_sync::block_on(world.host.inner.scan(SCOPE, 0, i64::MAX as u64)).unwrap_or_default();
    let mut fold = Fold::new();
    for (seq, body) in &rows {
        fold.apply(*seq, body).map_err(|error| format!("live refold: {error}"))?;
    }
    audit(&fold, &Reference::of(&rows)?)
}
