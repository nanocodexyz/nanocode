use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    Applied, Board, BoardError, Entry, EventId, Fold, Op, Phase, Rejection, Task, TaskId,
    sim::{Mutation, mutated},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Can {
    Read,
    Post,
    React,
    Label,
    Sync,
    Manage,
}

impl Can {
    pub const ALL: [Self; 6] = [
        Self::Read,
        Self::Post,
        Self::React,
        Self::Label,
        Self::Sync,
        Self::Manage,
    ];

    pub const fn implies(self, other: Self) -> bool {
        self as u8 == other as u8
            || matches!(self, Self::Manage)
            || (matches!(other, Self::Read)
                && matches!(self, Self::React | Self::Label | Self::Sync))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Who {
    Principal(String),
    Kind(String),
    Everyone,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub who: Vec<Who>,
    pub can: BTreeMap<String, Vec<Can>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denied {
    Unverified,
    Malformed(String),
    Unchartered,
    Missing(Can),
    NotPoster,
    NotHolder,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Actor {
    principal: String,
    profile: Option<String>,
}

const fn principal_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '/' | '@')
}

const fn profile_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

fn principal(id: &str) -> Result<&str, Denied> {
    match id.split_once(':') {
        Some((kind, rest))
            if !kind.is_empty()
                && !rest.is_empty()
                && kind
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && id.chars().all(principal_char) =>
        {
            Ok(id)
        }
        _ => Err(Denied::Malformed(format!("{id} is not a principal"))),
    }
}

impl Actor {
    pub fn parse(id: &str) -> Result<Self, Denied> {
        let (who, profile) = match id.split_once('+') {
            Some((who, profile)) => (who, Some(profile)),
            None => (id, None),
        };
        if let Some(profile) = profile
            && (profile.is_empty() || !profile.chars().all(profile_char))
        {
            return Err(Denied::Malformed(format!("{profile} is not a profile")));
        }
        Ok(Self {
            principal: principal(who)?.to_owned(),
            profile: profile.map(str::to_owned),
        })
    }

    pub fn principal(&self) -> &str {
        &self.principal
    }

    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    pub fn kind(&self) -> &str {
        self.principal.split_once(':').map_or("", |(kind, _)| kind)
    }

    pub fn covers(&self, other: &Self) -> bool {
        self.principal == other.principal
            && match (&self.profile, &other.profile) {
                (None, _) => true,
                (Some(mine), Some(theirs)) => mine.eq_ignore_ascii_case(theirs),
                (Some(_), None) => false,
            }
    }

    pub fn owns(&self, event: &EventId) -> bool {
        event
            .0
            .strip_prefix(&self.to_string())
            .is_some_and(|rest| rest.len() > 1 && (rest.starts_with('.') || rest.starts_with('#')))
    }

    fn source(&self) -> Option<&str> {
        self.principal.strip_prefix("source:")
    }
}

impl std::fmt::Display for Actor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.principal)?;
        if let Some(profile) = &self.profile {
            write!(formatter, "+{profile}")?;
        }
        Ok(())
    }
}

pub trait Verifier {
    fn verify(&self, credential: &str) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    actor: Actor,
}

impl Identity {
    pub fn verify(verifier: &dyn Verifier, credential: &str) -> Result<Self, Denied> {
        let id = verifier.verify(credential).ok_or(Denied::Unverified)?;
        Ok(Self {
            actor: Actor {
                principal: principal(&id)?.to_owned(),
                profile: None,
            },
        })
    }

    pub fn profile(&self, name: &str) -> Result<Self, Denied> {
        let actor = Actor::parse(&format!("{}+{name}", self.actor.principal))?;
        Ok(Self { actor })
    }

    pub const fn actor(&self) -> &Actor {
        &self.actor
    }
}

pub fn pattern(text: &str) -> Result<(), Denied> {
    let base = text.strip_suffix('/').unwrap_or(text);
    if text == "*"
        || (!base.is_empty() && !base.split('/').any(str::is_empty) && !text.contains('*'))
    {
        Ok(())
    } else {
        Err(Denied::Malformed(format!(
            "{text} is not a channel pattern"
        )))
    }
}

pub fn reach(pattern: &str, target: &str) -> Option<(u8, usize)> {
    if pattern == "*" {
        return Some((0, 0));
    }
    if target == "*" {
        return None;
    }
    match pattern.strip_suffix('/') {
        Some(base) => {
            let spot = target.strip_suffix('/').unwrap_or(target);
            let root = spot == base && !mutated(Mutation::SubtreeSkipsRoot);
            (root || spot.starts_with(pattern)).then_some((1, pattern.len()))
        }
        None => (pattern == target).then_some((2, pattern.len())),
    }
}

fn closeness(who: &[Who], actor: &Actor) -> Option<u8> {
    who.iter()
        .filter_map(|who| match who {
            Who::Principal(id) if *id == actor.principal => Some(2),
            Who::Kind(kind) if kind == actor.kind() => Some(1),
            Who::Everyone => Some(0),
            _ => None,
        })
        .max()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Access {
    owner: Option<String>,
    grants: BTreeMap<EventId, Grant>,
    posters: BTreeMap<TaskId, Actor>,
    holders: BTreeMap<TaskId, Actor>,
}

impl Access {
    pub const fn new() -> Self {
        Self {
            owner: None,
            grants: BTreeMap::new(),
            posters: BTreeMap::new(),
            holders: BTreeMap::new(),
        }
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    pub const fn grants(&self) -> &BTreeMap<EventId, Grant> {
        &self.grants
    }

    pub fn poster(&self, task: &TaskId) -> Option<&Actor> {
        self.posters.get(task)
    }

    fn decide(&self, actor: &Actor, target: &str, everyone: bool) -> BTreeSet<Can> {
        let mut found = Vec::new();
        for grant in self.grants.values() {
            let Some(close) = closeness(&grant.who, actor) else {
                continue;
            };
            if close == 0 && !everyone {
                continue;
            }
            for (pattern, cans) in &grant.can {
                if let Some(rank) = reach(pattern, target) {
                    found.push((rank, close, cans));
                }
            }
        }
        if mutated(Mutation::SpecificityIgnored) {
            return found
                .iter()
                .flat_map(|(_, _, cans)| cans.iter().copied())
                .collect();
        }
        let Some(best) = found.iter().map(|(rank, close, _)| (*rank, *close)).max() else {
            return BTreeSet::new();
        };
        found
            .iter()
            .filter(|(rank, close, _)| (*rank, *close) == best)
            .flat_map(|(_, _, cans)| cans.iter().copied())
            .collect()
    }

    fn holds(&self, actor: &Actor, target: &str, can: Can, everyone: bool) -> bool {
        if self.owner.as_deref() == Some(actor.principal()) {
            return true;
        }
        if let Some(source) = actor.source()
            && !mutated(Mutation::SourceUncapped)
            && reach(&format!("{source}/"), target).is_none()
        {
            return false;
        }
        self.decide(actor, target, everyone)
            .iter()
            .any(|held| held.implies(can))
    }

    pub fn may(&self, actor: &Actor, channel: &str, can: Can) -> bool {
        self.holds(actor, channel, can, true)
    }

    pub fn reads(&self, actor: &Actor, channel: &str) -> bool {
        mutated(Mutation::ReadLeaks) || self.may(actor, channel, Can::Read)
    }

    pub fn reaches(&self, actor: &Actor, pattern: &str) -> bool {
        if mutated(Mutation::ManageUnchecked) {
            return true;
        }
        self.holds(actor, pattern, Can::Manage, true)
            && self
                .grants
                .values()
                .flat_map(|grant| grant.can.keys())
                .filter(|under| *under != pattern && reach(pattern, under).is_some())
                .all(|under| self.holds(actor, under, Can::Manage, false))
    }

    fn chain(&self, fold: &Fold, actor: &Actor, task: &TaskId) -> bool {
        let mut at = Some(task);
        while let Some(id) = at {
            if self
                .posters
                .get(id)
                .is_some_and(|poster| actor.covers(poster))
            {
                return true;
            }
            at = fold.task(id).and_then(|task| task.spec.parent.as_ref());
        }
        false
    }

    fn need(&self, actor: &Actor, channel: &str, can: Can) -> Result<(), Rejection> {
        if self.may(actor, channel, can) {
            Ok(())
        } else {
            Err(Rejection::Access(Denied::Missing(can)))
        }
    }

    fn see(&self, actor: &Actor, task: &TaskId) -> Result<(), Rejection> {
        if self.reads(actor, &task.0) {
            Ok(())
        } else {
            Err(Rejection::UnknownTask)
        }
    }

    pub(crate) fn record(&mut self, by: &str, event: &EventId, op: &Op) {
        let Ok(actor) = Actor::parse(by) else {
            return;
        };
        match op {
            Op::Charter {} => self.owner = Some(actor.principal),
            Op::Grant { grant } => {
                self.grants.insert(event.clone(), grant.clone());
            }
            Op::Revoke { grant } => {
                if !mutated(Mutation::RevokeIgnored) {
                    self.grants.remove(grant);
                }
            }
            Op::Post { task } => {
                self.posters.insert(task.id.clone(), actor);
            }
            Op::Claim { task, .. } => {
                self.holders.insert(task.clone(), actor);
            }
            _ => {}
        }
    }
}

pub fn owns(by: &str, event: &EventId) -> bool {
    mutated(Mutation::KeySquatting) || Actor::parse(by).is_ok_and(|actor| actor.owns(event))
}

impl Fold {
    pub const fn access(&self) -> &Access {
        &self.access
    }

    pub fn admit(&self, by: &str, at_ms: u64, op: &Op) -> Result<(), Rejection> {
        self.authorize(by, op)?;
        self.check(at_ms, op)
    }

    pub fn authorize(&self, by: &str, op: &Op) -> Result<(), Rejection> {
        let access = &self.access;
        let actor = Actor::parse(by).map_err(Rejection::Access)?;
        if let Op::Charter {} = op {
            return match (&access.owner, &actor.profile) {
                (Some(_), _) => Err(Rejection::Exists),
                (None, Some(_)) => Err(Rejection::Access(Denied::Malformed(
                    "a profile cannot charter a board".to_owned(),
                ))),
                (None, None) => Ok(()),
            };
        }
        if access.owner.is_none() {
            return Err(Rejection::Access(Denied::Unchartered));
        }
        if mutated(Mutation::GrantsIgnored) {
            return Ok(());
        }
        match op {
            Op::Charter {} => Ok(()),
            Op::Post { task } => {
                if let Some(parent) = &task.parent
                    && !access.reads(&actor, &parent.0)
                {
                    return Err(Rejection::UnknownParent(parent.clone()));
                }
                if let Some(need) = task
                    .needs
                    .iter()
                    .find(|need| !access.reads(&actor, &need.0))
                {
                    return Err(Rejection::UnknownNeed(need.clone()));
                }
                access.need(&actor, &task.id.0, Can::Read)?;
                access.need(&actor, &task.id.0, Can::Post)
            }
            Op::Claim { task, .. } => {
                access.see(&actor, task)?;
                access.need(&actor, &task.0, Can::Label)
            }
            Op::Renew { task, claim, .. }
            | Op::Release { task, claim, .. }
            | Op::Finish { task, claim, .. } => {
                access.see(&actor, task)?;
                access.need(&actor, &task.0, Can::Label)?;
                match (
                    self.task(task).map(|task| &task.phase),
                    access.holders.get(task),
                ) {
                    (Some(Phase::Claimed { claim: held, .. }), Some(holder))
                        if held == claim
                            && !actor.covers(holder)
                            && !mutated(Mutation::ForeignFinish) =>
                    {
                        Err(Rejection::Access(Denied::NotHolder))
                    }
                    _ => Ok(()),
                }
            }
            Op::Cancel { task, .. } => {
                access.see(&actor, task)?;
                if access.may(&actor, &task.0, Can::Manage)
                    || access.chain(self, &actor, task)
                    || mutated(Mutation::CancelByAnyone)
                {
                    Ok(())
                } else {
                    Err(Rejection::Access(Denied::NotPoster))
                }
            }
            Op::Reply { task, .. } => {
                access.see(&actor, task)?;
                access.need(&actor, &task.0, Can::Post)
            }
            Op::Grant { grant } => {
                if grant.who.is_empty() || grant.can.is_empty() {
                    return Err(Rejection::Access(Denied::Malformed(
                        "an empty grant".to_owned(),
                    )));
                }
                for who in &grant.who {
                    match who {
                        Who::Principal(id) => {
                            principal(id).map_err(Rejection::Access)?;
                        }
                        Who::Kind(kind) if kind.is_empty() => {
                            return Err(Rejection::Access(Denied::Malformed(
                                "an empty kind".to_owned(),
                            )));
                        }
                        Who::Kind(_) | Who::Everyone => {}
                    }
                }
                for text in grant.can.keys() {
                    pattern(text).map_err(Rejection::Access)?;
                    if !access.reaches(&actor, text) {
                        return Err(Rejection::Access(Denied::Missing(Can::Manage)));
                    }
                }
                Ok(())
            }
            Op::Revoke { grant } => match access.grants.get(grant) {
                Some(stored) if stored.can.keys().all(|text| access.reaches(&actor, text)) => {
                    Ok(())
                }
                _ => Err(Rejection::Access(Denied::Missing(Can::Manage))),
            },
        }
    }

    pub fn view<'a>(
        &'a self,
        actor: &'a Actor,
    ) -> impl Iterator<Item = (&'a TaskId, &'a Task)> + 'a {
        self.tasks()
            .iter()
            .filter(move |(id, _)| self.access.reads(actor, &id.0))
    }

    pub fn seen_by<'a>(&'a self, actor: &'a Actor) -> impl Iterator<Item = &'a Entry> + 'a {
        self.entries()
            .iter()
            .filter(move |entry| self.access.reads(actor, &entry.task.0))
    }
}

impl Board {
    pub const fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn may(&self, channel: &str, can: Can) -> bool {
        self.fold.access.may(self.identity.actor(), channel, can)
    }

    pub fn tasks(&self) -> impl Iterator<Item = (&TaskId, &Task)> + '_ {
        self.fold.view(self.identity.actor())
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.fold
            .task(id)
            .filter(|_| self.fold.access.reads(self.identity.actor(), &id.0))
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> + '_ {
        self.fold.seen_by(self.identity.actor())
    }

    pub fn ready(&self, at_ms: u64) -> Vec<TaskId> {
        self.fold
            .ready(at_ms)
            .filter(|id| self.may(&id.0, Can::Label))
            .cloned()
            .collect()
    }

    pub async fn charter(&mut self, at_ms: u64) -> Result<Applied, BoardError> {
        self.submit_keyed("charter", at_ms, Op::Charter {}).await
    }

    pub async fn grant(
        &mut self,
        key: &str,
        at_ms: u64,
        grant: Grant,
    ) -> Result<Applied, BoardError> {
        self.submit_keyed(key, at_ms, Op::Grant { grant }).await
    }

    pub async fn revoke(
        &mut self,
        key: &str,
        at_ms: u64,
        grant: EventId,
    ) -> Result<Applied, BoardError> {
        self.submit_keyed(key, at_ms, Op::Revoke { grant }).await
    }
}
