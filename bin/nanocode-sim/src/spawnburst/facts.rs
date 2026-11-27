use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use nanocode_backend_codex::durability::{CodexDecision, CodexFacts};
use nanocode_durability_core::{
    Append, Decision, Fact, FactLog, OwnedFact, Replay, Snapshot, StoreError, StoreFuture,
};
use nanocode_sync::Atom;
use serde_json::json;

use super::host::FaultHost;

#[derive(Clone, Copy, Debug)]
pub(super) struct Replayed {
    pub(super) scans: u64,
    pub(super) held_ms: u64,
    pub(super) facts: u64,
    pub(super) opening: bool,
}

pub(super) type Replays = Arc<Atom<Vec<Replayed>>>;

struct Watched {
    log: Arc<dyn FactLog>,
    opened: AtomicBool,
    host: Arc<FaultHost>,
    replays: Replays,
}

impl Watched {
    async fn watched(
        &self,
        replay: StoreFuture<'_, Result<Replay, StoreError>>,
    ) -> Result<Replay, StoreError> {
        let scans = self.host.scans();
        let started = nanocode_platform::now();
        let replayed = replay.await?;
        let held = nanocode_platform::now().saturating_duration_since(started);
        let record = Replayed {
            scans: self.host.scans() - scans,
            held_ms: u64::try_from(held.as_millis())
                .expect("a virtual replay fits in u64 milliseconds"),
            facts: replayed.facts.len() as u64,
            opening: !self.opened.swap(true, Ordering::Relaxed),
        };
        self.replays.update(|replays| replays.push(record));
        Ok(replayed)
    }
}

impl FactLog for Watched {
    fn replay(&self) -> StoreFuture<'_, Result<Replay, StoreError>> {
        Box::pin(self.watched(self.log.replay()))
    }

    fn replay_cached<'a>(
        &'a self,
        family: &'a str,
        format: u32,
    ) -> StoreFuture<'a, Result<Replay, StoreError>> {
        Box::pin(self.watched(self.log.replay_cached(family, format)))
    }

    fn append(&self, after: u64, write: Append) -> StoreFuture<'_, Result<u64, StoreError>> {
        self.log.append(after, write)
    }

    fn remember(&self, snapshot: Snapshot) {
        self.log.remember(snapshot);
    }
}

pub(super) fn notice(text: &str) -> Result<OwnedFact<CodexDecision>, String> {
    let items = serde_json::from_value(json!([{
        "type": "message",
        "role": "developer",
        "content": [{"type": "input_text", "text": text}]
    }]))
    .map_err(|error| format!("a notice does not decode as response items: {error}"))?;
    Ok(Fact::Decision(Decision::Backend(CodexDecision::Items(
        items,
    ))))
}

pub(super) async fn opened(
    log: Arc<dyn FactLog>,
    host: &Arc<FaultHost>,
    replays: &Replays,
) -> Result<CodexFacts, String> {
    let watched = Watched {
        log,
        opened: AtomicBool::new(false),
        host: Arc::clone(host),
        replays: Arc::clone(replays),
    };
    CodexFacts::open(Arc::new(watched))
        .await
        .map_err(|error| error.to_string())
}

pub(super) async fn history(
    session: &nanocode::durability::DurableSession,
    host: &Arc<FaultHost>,
    facts: u64,
    text_bytes: usize,
) -> Result<(), String> {
    let log = session
        .facts()
        .ok_or_else(|| "the parent's session keeps no facts".to_owned())?;
    let replays = Replays::default();
    let owner = opened(log, host, &replays).await?;
    let text = "h".repeat(text_bytes);
    for _ in 0..facts {
        owner
            .commit(notice(&text)?)
            .await
            .map_err(|error| format!("the parent's history fact did not commit: {error}"))?;
    }
    Ok(())
}
