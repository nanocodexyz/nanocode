use std::sync::Arc;

use nanocode_sync::Atom;
use nanocode_turnlog::{Effects, Follower, LogHost};

use super::{OneLog, Policy};
use crate::{
    durability::StoreError,
    thread_scope::{ThreadLogs, sim},
};

pub(crate) async fn writer(
    logs: &ThreadLogs,
    thread: &str,
    policy: &Policy,
    effects: &Atom<Effects>,
) -> Result<Arc<OneLog>, StoreError> {
    let mut policy = policy.clone();
    policy.effects = effects.read(Clone::clone);
    opening(logs.scopes().legacy(), thread, policy, false).await
}

pub async fn open(
    host: Arc<dyn LogHost>,
    scope: impl Into<Arc<str>>,
    policy: Policy,
) -> Result<Arc<OneLog>, StoreError> {
    opening(host, scope, policy, true).await
}

async fn opening(
    host: Arc<dyn LogHost>,
    scope: impl Into<Arc<str>>,
    policy: Policy,
    routed: bool,
) -> Result<Arc<OneLog>, StoreError> {
    let log = nanocode_turnlog::legacy::open(Arc::clone(&host), scope).await?;
    OneLog::opened(log, policy, routed.then_some(host)).await
}

pub(crate) async fn child(
    host: Arc<dyn LogHost>,
    scope: String,
    policy: &Policy,
) -> Result<Arc<OneLog>, StoreError> {
    let mut policy = policy.clone();
    if sim::mutated(sim::Mutation::ChildSnapshotsEveryRow) {
        policy.snapshot_rows = Some(1);
    }
    opening(host, scope, policy, false).await
}

pub async fn claim(follower: Follower, policy: Policy) -> Result<Arc<OneLog>, StoreError> {
    OneLog::opened(
        nanocode_turnlog::legacy::claim(follower).await?,
        policy,
        None,
    )
    .await
}

pub(crate) async fn recorded_child(log: &OneLog, state: &str) -> Result<(), StoreError> {
    super::children::record(log, state).await
}

pub(crate) async fn held(log: &OneLog, held: std::time::Duration) {
    log.log
        .write(move |_| Box::pin(nanocode_platform::sleep(held)))
        .await;
}

pub(crate) async fn claimed(log: &OneLog) -> Result<(), StoreError> {
    if sim::mutated(sim::Mutation::ClaimSkipsRow) {
        return Ok(());
    }
    if sim::mutated(sim::Mutation::ClaimIgnoresSeal) {
        log.writes(|_| Box::pin(std::future::ready(Ok::<_, StoreError>(()))))
            .await?;
        let sealed = Arc::clone(&log.handed);
        return log
            .log
            .write(move |writing| {
                sim::claim_landed(sealed.load(std::sync::atomic::Ordering::Acquire));
                nanocode_turnlog::legacy::claimed_in(writing)
            })
            .await;
    }
    let sealed = Arc::clone(&log.handed);
    log.writes(move |writing| {
        sim::claim_landed(sealed.load(std::sync::atomic::Ordering::Acquire));
        nanocode_turnlog::legacy::claimed_in(writing)
    })
    .await
}
