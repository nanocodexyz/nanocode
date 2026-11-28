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
    open(logs.scopes().legacy(), thread, policy).await
}

pub async fn open(
    host: Arc<dyn LogHost>,
    scope: impl Into<Arc<str>>,
    policy: Policy,
) -> Result<Arc<OneLog>, StoreError> {
    OneLog::opened(nanocode_turnlog::legacy::open(host, scope).await?, policy).await
}

pub async fn claim(follower: Follower, policy: Policy) -> Result<Arc<OneLog>, StoreError> {
    OneLog::opened(nanocode_turnlog::legacy::claim(follower).await?, policy).await
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
