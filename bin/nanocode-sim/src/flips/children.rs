mod rollback;

use std::ops::Range;

use nanocode::turnlog::{
    ChildScope, OneLogOver, ServedStore, ThreadPath, child_scope, child_scope_keyed,
};
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
    wiring::{self, Wiring},
};

const SPAWNS: Range<u64> = 1..4;
const REWRITE_PPM: u64 = 500_000;

#[derive(Default)]
pub(super) struct Kids {
    rounds: u64,
    spawned: Vec<Kid>,
    released: bool,
    wiring: Wiring,
    kept: rollback::Kept,
}

impl Kids {
    pub(super) const fn release(&mut self, released: bool) {
        self.released = released;
    }

    pub(super) const fn wire(&mut self, wiring: Wiring) {
        self.wiring = wiring;
    }

    pub(super) const fn wiring(&self) -> Wiring {
        self.wiring
    }
}

pub(super) async fn wired(
    over: &OneLogOver,
    wiring: Wiring,
) -> Result<Box<dyn StateStore>, StoreError> {
    match wiring {
        Wiring::Routed => over.states().await,
        Wiring::Served => Ok(match over.served_store().await? {
            ServedStore::Resident(store) => Box::new(store),
            ServedStore::Legacy(store) => Box::new(store),
        }),
        Wiring::Plain => Err(StoreError::NotCommitted(
            "flips opens its facade, never a plain log".to_owned(),
        )),
    }
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

const fn expected(setting: ChildScope, wiring: Wiring) -> ChildPlace {
    match (wiring, setting) {
        (Wiring::Routed, ChildScope::Own) => ChildPlace::Own,
        (Wiring::Routed, ChildScope::Parent) | (Wiring::Served | Wiring::Plain, _) => {
            ChildPlace::Parent
        }
    }
}

async fn spawned(
    states: &dyn StateStore,
    id: &str,
    wiring: Wiring,
) -> Result<ChildPlace, StoreError> {
    match (states.child_scopes(), wiring) {
        (Some(children), _) => children.spawn(id).await,
        (None, Wiring::Served) => Ok(ChildPlace::Parent),
        (None, Wiring::Routed | Wiring::Plain) => Err(StoreError::NotCommitted(
            "the facade's routed state store places no children".to_owned(),
        )),
    }
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
    let wiring = run.kids.wiring;
    for _ in 0..rng.range(SPAWNS.start, SPAWNS.end) {
        let id = format!("kid-{}", run.kids.spawned.len());
        let place = match driven(world, "a child spawn", spawned(states, &id, wiring))? {
            Ok(place) => place,
            Err(error) => {
                failed(world, run, "a child spawn", &error);
                return None;
            }
        };
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

fn keyed(world: &World, run: &Run, kid: &Kid) -> Option<()> {
    if kid.place != ChildPlace::Own {
        return Some(());
    }
    let scope = child_scope(THREAD, &kid.id);
    if !child_scope_keyed(&scope) {
        flag(
            &world.wrong,
            format!(
                "flips: child {} lives in scope {scope}, not a fixed-width child scope key",
                kid.id
            ),
        );
        return None;
    }
    let path = match driven(world, "a child path", run.over.child_path(&scope))? {
        Ok(path) => path,
        Err(error) => {
            failed(world, run, "a child path", &error);
            return None;
        }
    };
    let wanted = format!("{THREAD}/{}", kid.id);
    if path.as_deref() != Some(wanted.as_str()) {
        flag(
            &world.wrong,
            format!(
                "flips: child scope {scope} resolves to {path:?} where its parent recorded {wanted}"
            ),
        );
        return None;
    }
    Some(())
}

fn placed(world: &World, run: &Run, kid: &Kid) -> Option<()> {
    keyed(world, run, kid)?;
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
        placed(world, run, kid)?;
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

async fn records(over: &OneLogOver, scope: &str) -> Result<bool, StoreError> {
    if let Some(log) = over.legacy() {
        let named = scope.to_owned();
        return Ok(log
            .read(move |fold| wiring::recorded(fold.hosts(), &named))
            .await);
    }
    over.read(|state| wiring::recorded(state.hosts(), scope))
        .await
}

fn recorded(world: &World, run: &Run, kid: &Kid) -> Option<()> {
    let scope = child_scope(THREAD, &kid.id);
    let found = match driven(world, "a child's record", records(&run.over, &scope))? {
        Ok(found) => found,
        Err(error) => {
            failed(world, run, "a child's record", &error);
            return None;
        }
    };
    let own = world.rows(&scope).len();
    let Some(wrong) = wiring::misplaced(&kid.id, &scope, found, own) else {
        return Some(());
    };
    flag(
        &world.wrong,
        format!("flips: placed {:?}: {wrong}", kid.place),
    );
    None
}

fn placements(world: &World, run: &Run, from: usize) -> Option<()> {
    let setting = run.over.child_scope();
    let want = expected(setting, run.kids.wiring);
    for kid in run.kids.spawned.iter().skip(from) {
        recorded(world, run, kid)?;
        if kid.place != want {
            flag(
                &world.wrong,
                format!(
                    "flips: child {} spawned with child_scope {} through the {} store was placed {:?}",
                    kid.id,
                    setting.setting(),
                    run.kids.wiring.name(),
                    kid.place
                ),
            );
            return None;
        }
    }
    Some(())
}

pub(super) fn round(rng: &mut Rng, world: &World, run: &mut Run) -> Option<()> {
    if run.kids.wiring() == Wiring::Routed && run.over.path() != ThreadPath::Resident {
        return Some(());
    }
    let wired = wired(&run.over, run.kids.wiring);
    let mut states = match driven(world, "a child scope store", wired)? {
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
    if let Err(error) = run.over.set_child_scope(setting)
        && (setting == ChildScope::Parent || run.kids.released)
    {
        flag(
            &world.wrong,
            format!(
                "flips: child_scope {} is refused: {error}",
                setting.setting()
            ),
        );
        return None;
    }
    let from = run.kids.spawned.len();
    spawn(rng, world, run, &*states)?;
    write(rng, world, run, &*states)?;
    rollback::write(rng, world, &mut run.kids, &mut *states)?;
    if let Err(error) = driven(world, "a child scope flush", states.flush())? {
        failed(world, run, "the child scope flush", &error);
        return None;
    }
    placements(world, run, from)?;
    rollback::check(world, &run.kids)
}
