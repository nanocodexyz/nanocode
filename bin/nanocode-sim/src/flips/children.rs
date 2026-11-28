use std::ops::Range;

use nanocode::turnlog::{ChildScope, ThreadPath, child_scope};
use nanocode_durability_core::{
    ChildPlace, Decision, DurableFold, Fact, FoldError, Seen, StateStore, StoreError, commit_fact,
    fold_facts,
};
use serde::{Deserialize, Serialize};

use super::{Run, THREAD, failed};
use crate::{
    rng::Rng,
    threadcompat,
    threadscope::{World, driven, flag},
};

const SPAWNS: Range<u64> = 1..4;
const REWRITE_PPM: u64 = 500_000;

#[derive(Default)]
pub(super) struct Kids {
    rounds: u64,
    spawned: Vec<Kid>,
}

struct Kid {
    id: String,
    place: ChildPlace,
    said: Vec<u64>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct Said {
    said: Vec<u64>,
}

impl DurableFold for Said {
    const FAMILY: &'static str = "sim.child";
    const FORMAT: u32 = 1;
    type Decision = u64;

    fn apply(&mut self, _seq: u64, fact: Seen<'_, u64>) -> Result<(), FoldError> {
        if let Fact::Decision(Decision::Backend(said)) = fact {
            self.said.push(*said);
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        Ok(Vec::new())
    }
}

const fn expected(setting: ChildScope) -> ChildPlace {
    match setting {
        ChildScope::Own => ChildPlace::Own,
        ChildScope::Parent => ChildPlace::Parent,
    }
}

async fn spawned(states: &dyn StateStore, id: &str) -> Result<ChildPlace, StoreError> {
    let children = states.child_scopes().ok_or_else(|| {
        StoreError::NotCommitted("the facade's state store places no children".to_owned())
    })?;
    children.spawn(id).await
}

async fn said(states: &dyn StateStore, id: &str, said: u64) -> Result<u64, StoreError> {
    let logs = states
        .facts()
        .ok_or_else(|| StoreError::NotCommitted("the facade keeps no fact log".to_owned()))?;
    let scoped = logs.scoped(id, false);
    let fold = fold_facts::<Said>(&*scoped).await?;
    commit_fact(&*scoped, fold, Fact::Decision(Decision::Backend(said))).await?;
    Ok(said)
}

async fn folded(states: &dyn StateStore, id: &str) -> Result<Vec<u64>, StoreError> {
    let logs = states
        .facts()
        .ok_or_else(|| StoreError::NotCommitted("the facade keeps no fact log".to_owned()))?;
    let scoped = logs.scoped(id, false);
    let fold = fold_facts::<Said>(&*scoped).await?;
    Ok(fold.state().said.clone())
}

fn spawn(rng: &mut Rng, world: &World, run: &mut Run, states: &dyn StateStore) -> Option<()> {
    let setting = run.over.child_scope();
    for _ in 0..rng.range(SPAWNS.start, SPAWNS.end) {
        let id = format!("kid-{}", run.kids.spawned.len());
        let place = match driven(world, "a child spawn", spawned(states, &id))? {
            Ok(place) => place,
            Err(error) => {
                failed(world, run, "a child spawn", &error);
                return None;
            }
        };
        if place != expected(setting) {
            flag(
                &world.wrong,
                format!(
                    "flips: child {id} spawned with child_scope {} was placed {place:?}",
                    setting.setting()
                ),
            );
            return None;
        }
        run.kids.spawned.push(Kid {
            id,
            place,
            said: Vec::new(),
        });
    }
    Some(())
}

fn write(rng: &mut Rng, world: &World, run: &mut Run, states: &dyn StateStore) -> Option<()> {
    for index in 0..run.kids.spawned.len() {
        let fresh = run.kids.spawned[index].said.is_empty();
        if !fresh && !rng.chance(REWRITE_PPM) {
            continue;
        }
        let id = run.kids.spawned[index].id.clone();
        let next = run
            .kids
            .spawned
            .iter()
            .map(|kid| kid.said.len() as u64)
            .sum();
        match driven(world, "a child's fact", said(states, &id, next))? {
            Ok(written) => run.kids.spawned[index].said.push(written),
            Err(error) => {
                failed(world, run, "a child's fact", &error);
                return None;
            }
        }
    }
    Some(())
}

fn placed(world: &World, kid: &Kid) -> Option<()> {
    let rows = world.rows(&child_scope(THREAD, &kid.id));
    let own = kid.place == ChildPlace::Own;
    if own == rows.is_empty() {
        flag(
            &world.wrong,
            format!(
                "flips: child {} placed {:?} holds {} rows in its own scope",
                kid.id,
                kid.place,
                rows.len()
            ),
        );
        return None;
    }
    let mut inner = Vec::with_capacity(rows.len());
    for (seq, row) in &rows {
        match nanocode_turnlog::tagged_row(row) {
            Ok(tagged) => inner.push((*seq, tagged.to_owned())),
            Err(error) => {
                flag(
                    &world.wrong,
                    format!(
                        "flips: child {} row {seq} opens to no format 1 row: {error}",
                        kid.id
                    ),
                );
                return None;
            }
        }
    }
    threadcompat::previous_rows(world, &inner, "in a child's own scope")
}

fn agree(world: &World, run: &Run, states: &dyn StateStore) -> Option<()> {
    for kid in &run.kids.spawned {
        placed(world, kid)?;
        match driven(world, "a child's fold", folded(states, &kid.id))? {
            Ok(said) if said == kid.said => {}
            Ok(said) => {
                flag(
                    &world.wrong,
                    format!(
                        "flips: child {} placed {:?} folds {} acknowledged facts as {}",
                        kid.id,
                        kid.place,
                        kid.said.len(),
                        said.len()
                    ),
                );
                return None;
            }
            Err(error) => {
                failed(world, run, "a child's fold", &error);
                return None;
            }
        }
    }
    Some(())
}

pub(super) fn round(rng: &mut Rng, world: &World, run: &mut Run) -> Option<()> {
    if run.over.path() != ThreadPath::Resident {
        return Some(());
    }
    let mut states = match driven(world, "a child scope store", run.over.states())? {
        Ok(states) => states,
        Err(error) => {
            failed(world, run, "the child scope store", &error);
            return None;
        }
    };
    agree(world, run, &*states)?;
    let setting = if run.kids.rounds % 2 == 0 {
        ChildScope::Own
    } else {
        ChildScope::Parent
    };
    run.kids.rounds += 1;
    run.over.set_child_scope(setting);
    spawn(rng, world, run, &*states)?;
    write(rng, world, run, &*states)?;
    if let Err(error) = driven(world, "a child scope flush", states.flush())? {
        failed(world, run, "the child scope flush", &error);
        return None;
    }
    Some(())
}
