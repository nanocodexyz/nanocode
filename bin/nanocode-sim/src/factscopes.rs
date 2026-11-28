use std::sync::Arc;

use nanocode::{
    backend::Prompt,
    durability::SessionStore,
    session_host::{Submission, Submitted},
    turnlog::{LogHost, OneLog, Policy},
};
use nanocode_backend_codex::durability::CodexFacts;
use serde_json::{Map, value::to_raw_value};

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::Rng,
};

const TURN: &str = "restored-scope";

pub(crate) struct Restored {
    pub(crate) thread: &'static str,
    pub(crate) policy: Policy,
    pub(crate) root: &'static str,
    pub(crate) children: Vec<String>,
}

fn prompt(thread: &str) -> String {
    format!("thread prompt of {thread} for its root session only")
}

async fn opened(host: &Arc<dyn LogHost>, restored: &Restored) -> Result<Arc<OneLog>, String> {
    nanocode::turnlog::thread_scope_sim::open(
        Arc::clone(host),
        restored.thread,
        restored.policy.clone(),
    )
    .await
    .map_err(|error| format!("the thread log did not open: {error}"))
}

async fn turn(log: &OneLog, thread: &str) -> Result<(), String> {
    let metadata = to_raw_value("restored-scope")
        .map_err(|error| format!("the turn's metadata did not encode: {error}"))?;
    let submission = Submission {
        request_id: TURN.to_owned(),
        prompt: Prompt::new(prompt(thread)),
        hand: None,
        metadata,
    };
    match log.submit(&submission).await {
        Ok(Submitted::Accepted { .. }) => {}
        other => return Err(format!("the thread turn was not accepted: {other:?}")),
    }
    log.start(TURN)
        .await
        .map_err(|error| format!("the thread turn did not start: {error}"))?
        .ok_or_else(|| "the thread turn did not start".to_owned())?;
    log.finish(
        TURN,
        nanocode::session_host::TurnState::Completed,
        &Map::new(),
    )
    .await
    .map_err(|error| format!("the thread turn did not finish: {error}"))?
    .ok_or_else(|| "the thread turn did not finish".to_owned())?;
    Ok(())
}

async fn folded(store: &SessionStore, id: &str) -> Result<Option<String>, String> {
    let Ok(session) = store.prepare_resume(id).await else {
        return Ok(None);
    };
    let facts = session
        .facts()
        .ok_or_else(|| format!("the restored session {id} keeps no facts"))?;
    let codex = CodexFacts::open(facts)
        .map_err(|error| format!("the restored session {id}'s facts did not fold: {error}"))?;
    codex
        .read(|fold| serde_json::to_string(fold))
        .await
        .map_err(|error| format!("the restored session {id}'s fold did not read: {error}"))?
        .map(Some)
        .map_err(|error| format!("the restored session {id}'s fold did not encode: {error}"))
}

async fn checked(host: Arc<dyn LogHost>, restored: &Restored) -> Result<Option<String>, String> {
    let written = opened(&host, restored).await?;
    turn(&written, restored.thread).await?;
    drop(written);
    let log = opened(&host, restored).await?;
    let store = nanocode::session_store(log.store())
        .map_err(|error| format!("the session store did not open: {error}"))?;
    let tag = prompt(restored.thread);
    let root = folded(&store, restored.root)
        .await?
        .ok_or_else(|| format!("the root session {} did not restore", restored.root))?;
    assert!(
        root.contains(&tag),
        "the restored root session folds the thread's own prompt"
    );
    for child in &restored.children {
        if folded(&store, child)
            .await?
            .is_some_and(|fold| fold.contains(&tag))
        {
            return Ok(Some(format!(
                "restored child {child} folds the parent thread's owner-less prompt into its model history"
            )));
        }
    }
    Ok(None)
}

pub(crate) fn restored(
    clock: &Arc<Clock>,
    seed: u64,
    host: Arc<dyn LogHost>,
    restored: &Restored,
) -> Option<String> {
    let found = std::cell::RefCell::new(None);
    let mut executor = Executor::new(Arc::clone(clock), Rng::derive(seed, "restored-scope"));
    let task: Task<'_> = Box::pin(async {
        found.replace(Some(checked(host, restored).await));
    });
    match (executor.run(vec![task], None), found.into_inner()) {
        (Ended::Finished, Some(Ok(wrong))) => wrong,
        (Ended::Finished, Some(Err(error))) => Some(format!("the restore check failed: {error}")),
        (ended, _) => Some(format!("the restore check ended {ended:?}")),
    }
}
