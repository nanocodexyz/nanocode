use std::{collections::BTreeMap, sync::Arc};

use nanocode::turnlog::{ChildScope, OneLog, Policy, child_scope, thread_scope_sim};
use nanocode_durability_core::{
    ChildPlace, Decision, DurableFold, Fact, FoldError, Input, InputKind, ModelOut, OwnedFact,
    OwnerId, Seen, StateStore, StoreError, StoredState, commit_fact, fold_facts,
};
use nanocode_turnlog::{LogHost, MemoryLog};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Kid, Kids, THREAD, folded, said};
use crate::{
    rng::Rng,
    threadscope::{World, driven, flag, raw},
};

const SESSION_PPM: u64 = 500_000;
const TURN_PPM: u64 = 500_000;
const DOWNGRADED: u64 = u64::MAX;

#[derive(Default)]
pub(super) struct Kept {
    kids: BTreeMap<String, Resume>,
    turns: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Resume {
    session: StoredState,
    pending: Vec<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct Resumed {
    said: Vec<u64>,
    pending: Vec<String>,
}

impl DurableFold for Resumed {
    const FAMILY: &'static str = "sim.child.resumed";
    const FORMAT: u32 = 1;
    type Decision = u64;

    fn apply(&mut self, _seq: u64, fact: Seen<'_, u64>) -> Result<(), FoldError> {
        match fact {
            Fact::Decision(Decision::Backend(said)) => self.said.push(*said),
            Fact::Input(input) => self.pending.push(input.turn.clone()),
            Fact::ModelOut(out) => self.pending.retain(|turn| *turn != out.turn),
            _ => {}
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        Ok(Vec::new())
    }
}

fn unlogged() -> StoreError {
    StoreError::NotCommitted("the child's store keeps no fact log".to_owned())
}

async fn session(
    states: &mut dyn StateStore,
    id: &str,
    payload: &str,
) -> Result<StoredState, StoreError> {
    let owned = states.acquire(id, OwnerId::new()).await?;
    states
        .replace(id, &owned.owner, owned.state.revision, payload, &[])
        .await?;
    states.peek(id).await
}

async fn turn(states: &dyn StateStore, id: &str, fact: OwnedFact<u64>) -> Result<(), StoreError> {
    let logs = states.facts().ok_or_else(unlogged)?;
    let scoped = logs.scoped(id, false);
    let fold = fold_facts::<Resumed>(&*scoped).await?;
    commit_fact(&*scoped, fold, fact).await?;
    Ok(())
}

async fn resumed(
    states: &mut dyn StateStore,
    id: &str,
) -> Result<(StoredState, Resumed), StoreError> {
    let stored = states.peek(id).await?;
    let logs = states.facts().ok_or_else(unlogged)?;
    let fold = fold_facts::<Resumed>(&*logs.scoped(id, false)).await?;
    let state = fold.state();
    let resumed = Resumed {
        said: state.said.clone(),
        pending: state.pending.clone(),
    };
    Ok((stored, resumed))
}

fn opened_fact(kept: &mut Kept, id: &str) -> (String, OwnedFact<u64>) {
    let turn = format!("{id}-turn-{}", kept.turns);
    kept.turns += 1;
    let fact = Fact::Input(Input {
        turn: turn.clone(),
        kind: InputKind::Prompt,
        value: raw(&json!("a child's turn")),
        metadata: raw(&json!(null)),
        hand: None,
        first: None,
    });
    (turn, fact)
}

fn next_turn(rng: &mut Rng, kept: &mut Kept, id: &str) -> Option<(Vec<String>, OwnedFact<u64>)> {
    let pending = kept.kids.get(id).map(|resume| resume.pending.clone());
    match pending.as_deref() {
        None | Some([]) => {
            if !rng.chance(TURN_PPM) {
                return None;
            }
            let (turn, fact) = opened_fact(kept, id);
            Some((vec![turn], fact))
        }
        Some([turn, ..]) => {
            if !rng.chance(TURN_PPM) {
                return None;
            }
            let fact = Fact::ModelOut(ModelOut {
                turn: turn.clone(),
                response: raw(&json!("a child's answer")),
            });
            Some((Vec::new(), fact))
        }
    }
}

fn kept_turn(
    rng: &mut Rng,
    world: &World,
    kids: &mut Kids,
    index: usize,
    states: &dyn StateStore,
) -> Option<()> {
    let id = kids.spawned[index].id.clone();
    let Some((pending, fact)) = next_turn(rng, &mut kids.kept, &id) else {
        return Some(());
    };
    if let Err(error) = driven(world, "a child's turn", turn(states, &id, fact))? {
        flag(
            &world.wrong,
            format!("flips: child {id}'s turn fact answers {error}"),
        );
        return None;
    }
    kids.kept.kids.entry(id).or_default().pending = pending;
    Some(())
}

fn kept_session(
    rng: &mut Rng,
    world: &World,
    kids: &mut Kids,
    index: usize,
    states: &mut dyn StateStore,
) -> Option<()> {
    let id = kids.spawned[index].id.clone();
    let fresh = !kids.kept.kids.contains_key(&id);
    if !fresh && !rng.chance(SESSION_PPM) {
        return Some(());
    }
    let payload = format!("{id} at round {}", kids.rounds);
    match driven(world, "a child's session", session(states, &id, &payload))? {
        Ok(stored) if stored.payload.as_deref() == Some(payload.as_str()) => {
            kids.kept.kids.entry(id).or_default().session = stored;
            Some(())
        }
        Ok(stored) => {
            flag(
                &world.wrong,
                format!("flips: child {id}'s session reads {stored:?} after it wrote {payload}"),
            );
            None
        }
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: child {id}'s session answers {error}"),
            );
            None
        }
    }
}

pub(super) fn write(
    rng: &mut Rng,
    world: &World,
    kids: &mut Kids,
    states: &mut dyn StateStore,
) -> Option<()> {
    for index in 0..kids.spawned.len() {
        kept_session(rng, world, kids, index, states)?;
        kept_turn(rng, world, kids, index, states)?;
    }
    Some(())
}

fn copied(world: &World, kids: &[Kid]) -> Option<Arc<dyn LogHost>> {
    let copy = Arc::new(MemoryLog::new());
    let scopes = std::iter::once(THREAD.to_owned())
        .chain(kids.iter().map(|kid| child_scope(THREAD, &kid.id)));
    for scope in scopes {
        let rows = world.rows(&scope);
        if rows.is_empty() {
            continue;
        }
        if let Err(error) = driven(world, "a rollback copy", copy.append(&scope, &rows))? {
            flag(
                &world.wrong,
                format!("flips: a copy of scope {scope} refuses its own rows: {error}"),
            );
            return None;
        }
    }
    Some(copy)
}

async fn previous(copy: Arc<dyn LogHost>) -> Result<Arc<OneLog>, StoreError> {
    let follower = nanocode_turnlog::sim::follow(copy, THREAD).await?;
    thread_scope_sim::claim(follower, Policy::default()).await
}

async fn parent_set(copy: Arc<dyn LogHost>) -> Result<Arc<OneLog>, StoreError> {
    let log = thread_scope_sim::open(copy, THREAD, Policy::default()).await?;
    log.set_child_scope(ChildScope::Parent)
        .map_err(|error| StoreError::NotCommitted(error.to_string()))?;
    Ok(log)
}

fn lost(world: &World, kid: &Kid, how: &str, detail: &str) {
    flag(
        &world.wrong,
        format!(
            "flips: child {} placed {:?} is lost {how}: {detail}",
            kid.id, kid.place
        ),
    );
}

fn resume(world: &World, kids: &Kids, states: &mut dyn StateStore, how: &str) -> Option<()> {
    for kid in &kids.spawned {
        let want = kids.kept.kids.get(&kid.id).cloned().unwrap_or_default();
        let (stored, have) = match driven(world, "a child's resume", resumed(states, &kid.id))? {
            Ok(resumed) => resumed,
            Err(error) => {
                lost(world, kid, how, &format!("its resume answers {error}"));
                return None;
            }
        };
        if stored != want.session {
            lost(
                world,
                kid,
                how,
                &format!(
                    "its session reads {stored:?} where {:?} was acknowledged",
                    want.session
                ),
            );
            return None;
        }
        if have.said != kid.said {
            lost(
                world,
                kid,
                how,
                &format!(
                    "it folds {} facts where {} were acknowledged",
                    have.said.len(),
                    kid.said.len()
                ),
            );
            return None;
        }
        if have.pending != want.pending {
            lost(
                world,
                kid,
                how,
                &format!(
                    "its pending turns read {:?} where {:?}",
                    have.pending, want.pending
                ),
            );
            return None;
        }
    }
    Some(())
}

fn opened(world: &World, what: &str, log: Result<Arc<OneLog>, StoreError>) -> Option<Arc<OneLog>> {
    match log {
        Ok(log) => Some(log),
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: {what} refuses the thread: {error}"),
            );
            None
        }
    }
}

fn forward(world: &World, kids: &Kids, copy: Arc<dyn LogHost>, how: &str) -> Option<()> {
    let log = opened(
        world,
        "a roll forward",
        driven(world, "a roll forward", parent_set(copy))?,
    )?;
    let states = log.store();
    for kid in &kids.spawned {
        let mut want = kid.said.clone();
        want.push(DOWNGRADED);
        match driven(world, "a rolled forward fold", folded(&states, &kid.id))? {
            Ok(have) if have == want => {}
            Ok(have) => {
                flag(
                    &world.wrong,
                    format!(
                        "flips: child {} placed {:?} forks {how}: the rolled forward build folds {} facts where {} were acknowledged",
                        kid.id,
                        kid.place,
                        have.len(),
                        want.len()
                    ),
                );
                return None;
            }
            Err(error) => {
                lost(
                    world,
                    kid,
                    how,
                    &format!("the rolled forward fold answers {error}"),
                );
                return None;
            }
        }
    }
    Some(())
}

fn downgraded(world: &World, kids: &Kids) -> Option<()> {
    let how = if kids.released {
        "on a downgrade to a release that reads child scopes"
    } else {
        super::super::formats_agree(world, "on a downgrade")?;
        "on a downgrade to the release before child scopes"
    };
    let copy = copied(world, &kids.spawned)?;
    let log = if kids.released {
        driven(world, "a downgrade", parent_set(Arc::clone(&copy)))?
    } else {
        driven(world, "a downgrade", previous(Arc::clone(&copy)))?
    };
    let log = opened(world, "a downgraded open", log)?;
    let mut states = log.store();
    resume(world, kids, &mut states, how)?;
    for kid in &kids.spawned {
        if let Err(error) = driven(
            world,
            "a downgraded fact",
            said(&states, &kid.id, DOWNGRADED),
        )? {
            lost(world, kid, how, &format!("its next fact answers {error}"));
            return None;
        }
    }
    if let Err(error) = driven(world, "a downgraded flush", states.flush())? {
        flag(
            &world.wrong,
            format!("flips: a downgraded flush answers {error}"),
        );
        return None;
    }
    drop(states);
    drop(log);
    forward(world, kids, copy, how)
}

async fn unrouted(
    log: &Arc<OneLog>,
    id: &str,
    payload: &str,
) -> Result<(ChildPlace, StoredState), StoreError> {
    let mut store = log.store();
    let children = store.child_scopes().ok_or_else(|| {
        StoreError::NotCommitted("an unrouted store places no children".to_owned())
    })?;
    let place = children.spawn(id).await?;
    let stored = session(&mut store, id, payload).await?;
    said(&store, id, DOWNGRADED).await?;
    store.flush().await?;
    Ok((place, stored))
}

async fn mixed_resume(
    copy: Arc<dyn LogHost>,
    id: &str,
) -> Result<(StoredState, Vec<u64>), StoreError> {
    let log = thread_scope_sim::open(copy, THREAD, Policy::default()).await?;
    let mut store = log.store();
    let stored = store.peek(id).await?;
    let facts = folded(&store, id).await?;
    Ok((stored, facts))
}

fn mixed(world: &World, kids: &Kids) -> Option<()> {
    let id = format!("mixed-{}", kids.rounds);
    let payload = format!("{id} on the parent's log");
    let copy = copied(world, &kids.spawned)?;
    let log = opened(
        world,
        "an unrouted open",
        driven(world, "an unrouted open", previous(Arc::clone(&copy)))?,
    )?;
    let (place, written) = match driven(world, "an unrouted spawn", unrouted(&log, &id, &payload))?
    {
        Ok(spawned) => spawned,
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: an unrouted spawn of {id} answers {error}"),
            );
            return None;
        }
    };
    if place != ChildPlace::Parent || log.spawned(&id) {
        flag(
            &world.wrong,
            format!(
                "flips: an unrouted store places child {id} {place:?} and records its own scope"
            ),
        );
        return None;
    }
    if let Err(error) = driven(
        world,
        "a mixed record",
        thread_scope_sim::record_child(&log, &id),
    )? {
        flag(
            &world.wrong,
            format!("flips: a mixed record of {id} answers {error}"),
        );
        return None;
    }
    drop(log);
    match driven(world, "a mixed resume", mixed_resume(copy, &id))? {
        Ok((stored, facts)) if stored == written && facts == [DOWNGRADED] => Some(()),
        Ok((stored, facts)) => {
            flag(
                &world.wrong,
                format!(
                    "flips: child {id}, recorded with its rows on the parent's log, resumes with session {stored:?} and {} facts where {written:?} and 1 were acknowledged",
                    facts.len()
                ),
            );
            None
        }
        Err(error) => {
            flag(
                &world.wrong,
                format!("flips: a mixed resume of {id} answers {error}"),
            );
            None
        }
    }
}

pub(super) fn check(world: &World, kids: &Kids) -> Option<()> {
    let copy = copied(world, &kids.spawned)?;
    let log = opened(
        world,
        "a restart with child_scope parent",
        driven(world, "a restart with child_scope parent", parent_set(copy))?,
    )?;
    let mut states = log.store();
    resume(
        world,
        kids,
        &mut states,
        "after child_scope flips to parent",
    )?;
    drop(states);
    drop(log);
    downgraded(world, kids)?;
    mixed(world, kids)?;
    if !kids.released && ChildScope::Own.checked().is_ok() {
        flag(
            &world.wrong,
            "flips: child_scope own is accepted while the previous release reads no child scopes"
                .to_owned(),
        );
        return None;
    }
    Some(())
}
