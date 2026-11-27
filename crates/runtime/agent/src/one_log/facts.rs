use std::{collections::BTreeSet, sync::Arc};

use nanocode_tool_host::ToolOutput;
use nanocode_turnlog::{Fold, Point, Writing};
use serde_json::value::RawValue;

use super::OneLog;
use crate::durability::{
    StoreError, StoreFuture,
    facts::{
        Append, Decision, Digest, EffectResult, Fact, FactLog, FactLogs, FactSink, FoldError,
        Marker, OwnedFact, Replay, Snapshot, StoredFact, Value,
    },
};

pub mod sim {
    use std::sync::atomic::{AtomicU8, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    pub enum Mutation {
        None,
        CommitBuffered,
        UnseenSkipped,
        IntentReplayed,
        IntentCopied,
        ScopeLeaks,
        AdmissionCopied,
        CacheGap,
        HostCopied,
        CancelLeavesIntent,
        IntentOptional,
        AbandonUnsettled,
        RememberedSkipsTail,
    }

    impl Mutation {
        pub const ALL: [Self; 12] = [
            Self::CommitBuffered,
            Self::UnseenSkipped,
            Self::IntentReplayed,
            Self::IntentCopied,
            Self::ScopeLeaks,
            Self::AdmissionCopied,
            Self::CacheGap,
            Self::HostCopied,
            Self::CancelLeavesIntent,
            Self::IntentOptional,
            Self::AbandonUnsettled,
            Self::RememberedSkipsTail,
        ];

        #[must_use]
        pub const fn name(self) -> &'static str {
            match self {
                Self::None => "none",
                Self::CommitBuffered => "commit_buffered",
                Self::UnseenSkipped => "unseen_skipped",
                Self::IntentReplayed => "intent_replayed",
                Self::IntentCopied => "intent_copied",
                Self::ScopeLeaks => "scope_leaks",
                Self::AdmissionCopied => "admission_copied",
                Self::CacheGap => "cache_gap",
                Self::HostCopied => "host_copied",
                Self::CancelLeavesIntent => "cancel_leaves_intent",
                Self::IntentOptional => "intent_optional",
                Self::AbandonUnsettled => "abandon_unsettled",
                Self::RememberedSkipsTail => "remembered_skips_tail",
            }
        }

        #[must_use]
        pub fn parse(name: &str) -> Option<Self> {
            std::iter::once(Self::None)
                .chain(Self::ALL)
                .find(|mutation| mutation.name() == name)
        }
    }

    static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

    pub fn mutate(mutation: Mutation) {
        MUTATION.store(mutation as u8, Ordering::SeqCst);
    }

    pub fn mutated(mutation: Mutation) -> bool {
        MUTATION.load(Ordering::Relaxed) == mutation as u8
    }
}

const fn point_of(fact: &StoredFact) -> Point {
    match fact {
        Fact::Input(_) => Point::Admission,
        Fact::Intent(_) => Point::Call,
        Fact::Marker(Marker::Status { .. }) => Point::Finish,
        Fact::Base(_)
        | Fact::ModelOut(_)
        | Fact::EffectResult(_)
        | Fact::Decision(_)
        | Fact::Marker(_) => Point::Progress,
    }
}

pub(crate) fn in_scope(scope: &Scope, held: Option<&str>, fact: &StoredFact) -> bool {
    if sim::mutated(sim::Mutation::ScopeLeaks) {
        return true;
    }
    match held {
        Some(state) => state == scope.state,
        None => match fact {
            Fact::Marker(Marker::Blob { .. }) => true,
            Fact::Decision(Decision::Host { .. }) => false,
            _ => scope.turns,
        },
    }
}

fn wanted(fold: &Fold, facts: &[(u64, StoredFact)]) -> Vec<Digest> {
    let mut wanted = BTreeSet::new();
    for (_, fact) in facts {
        let named: Result<_, std::convert::Infallible> = fact.try_map(
            |value| {
                if let Value::Blob(blob) = value
                    && let Some(digest) = fold.values().digest(*blob)
                {
                    wanted.insert(*digest);
                }
                Ok(())
            },
            |_| Ok(()),
        );
        let Ok(_) = named;
    }
    wanted.into_iter().collect()
}

pub(super) fn intended(fold: &Fold, call_id: &str) -> Option<String> {
    if sim::mutated(sim::Mutation::IntentCopied) {
        return None;
    }
    fold.intent(call_id).map(|call| call.tool.clone())
}

#[derive(Clone)]
pub(crate) struct Scope {
    pub(crate) state: String,
    pub(crate) turns: bool,
}

struct Scoped {
    log: Arc<OneLog>,
    scope: Scope,
}

struct Observed<'l> {
    writing: &'l mut Writing,
    state: &'l str,
    point: Point,
}

impl FactSink for Observed<'_> {
    fn next(&self) -> u64 {
        self.writing.next()
    }

    fn put_blob(&mut self, digest: &Digest, json: &RawValue) -> Result<(), FoldError> {
        self.writing.put_blob(digest, json)
    }

    fn append(&mut self, fact: StoredFact) -> Result<u64, FoldError> {
        self.point = point_of(&fact);
        let state = match fact {
            Fact::Marker(Marker::Blob { .. }) => None,
            _ => Some(self.state),
        };
        self.writing.append_in(state, fact)
    }

    fn cache_due(&self) -> bool {
        self.writing.fold().cache_due(self.state)
    }

    fn cache(&mut self, snapshot: &Snapshot) -> Result<Vec<(u64, StoredFact)>, FoldError> {
        self.writing
            .cache_state(self.state, snapshot)
            .map_err(FoldError::Sink)
    }
}

impl OneLog {
    async fn replayed(&self, scope: &Scope) -> Result<Replay, StoreError> {
        self.replayed_after(scope, None).await
    }

    async fn replayed_cached(
        &self,
        scope: &Scope,
        family: &str,
        format: u32,
    ) -> Result<Replay, StoreError> {
        let key = (scope.state.clone(), scope.turns);
        let remembered = self
            .remembered
            .read(|remembered| remembered.get(&key).cloned())
            .filter(|snapshot| snapshot.family == family && snapshot.format == format);
        if let Some(remembered) = remembered {
            nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
                monotonic_counter.nc.facts.replays_remembered = 1_u64
            });
            if sim::mutated(sim::Mutation::RememberedSkipsTail) {
                return Ok(Replay {
                    snapshot: Some(Snapshot::clone(&remembered)),
                    facts: Vec::new(),
                    blobs: std::collections::BTreeMap::new(),
                });
            }
            return self
                .replayed_after(scope, Some(Snapshot::clone(&remembered)))
                .await;
        }
        nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
            monotonic_counter.nc.facts.replays_cached = 1_u64
        });
        let cached = self
            .log
            .cached_state(&scope.state)
            .await?
            .filter(|snapshot| snapshot.family == family && snapshot.format == format);
        self.replayed_after(scope, cached).await
    }

    fn remember(&self, scope: &Scope, snapshot: Snapshot) {
        let key = (scope.state.clone(), scope.turns);
        let snapshot = Arc::new(snapshot);
        self.remembered.update(|remembered| {
            if remembered
                .get(&key)
                .is_none_or(|held| held.seq < snapshot.seq)
            {
                remembered.insert(key.clone(), Arc::clone(&snapshot));
            }
        });
    }

    async fn replayed_after(
        &self,
        scope: &Scope,
        snapshot: Option<Snapshot>,
    ) -> Result<Replay, StoreError> {
        let mut after = snapshot.as_ref().map_or(0, |snapshot| snapshot.seq);
        if snapshot.is_some() && sim::mutated(sim::Mutation::CacheGap) {
            after += 1;
        }
        let held = scope.clone();
        let facts = self
            .log
            .facts_from(after, Vec::new(), move |facts, seq, state, fact| {
                if in_scope(&held, state, fact) {
                    facts.push((seq, fact.clone()));
                }
            })
            .await?;
        let (facts, digests) = self
            .log
            .read(move |fold| {
                let digests = wanted(fold, &facts);
                (facts, digests)
            })
            .await;
        let blobs = self.log.blobs(&digests).await?;
        Ok(Replay {
            snapshot,
            facts,
            blobs,
        })
    }

    async fn appended(&self, scope: &Scope, after: u64, write: Append) -> Result<u64, StoreError> {
        let scope = scope.clone();
        self.writes(move |writing| {
                Box::pin(async move {
                    let scope = &scope;
                    writing.secure().await?;
                    let mut facts = Vec::new();
                    if after == 0 || !sim::mutated(sim::Mutation::UnseenSkipped) {
                        writing
                            .facts_from(after, |seq, held, fact| {
                                if in_scope(scope, held, fact) {
                                    facts.push((seq, fact.clone()));
                                }
                            })
                            .await?;
                    }
                    let digests = wanted(writing.fold(), &facts);
                    let blobs = writing.blobs(&digests).await?;
                    let unseen = Replay {
                        snapshot: None,
                        facts,
                        blobs,
                    };
                    let mut sink = Observed {
                        writing: writing,
                        state: &scope.state,
                        point: Point::Progress,
                    };
                    let seq = write(unseen, &mut sink)?;
                    let point = sink.point;
                    if !sim::mutated(sim::Mutation::CommitBuffered) {
                        writing.commit(point).await?;
                    }
                    Ok(seq)
                })
            })
            .await
    }
}

impl FactLog for Scoped {
    fn replay(&self) -> StoreFuture<'_, Result<Replay, StoreError>> {
        Box::pin(self.log.replayed(&self.scope))
    }

    fn replay_cached<'a>(
        &'a self,
        family: &'a str,
        format: u32,
    ) -> StoreFuture<'a, Result<Replay, StoreError>> {
        Box::pin(self.log.replayed_cached(&self.scope, family, format))
    }

    fn append<'a>(&'a self, after: u64, write: Append) -> StoreFuture<'a, Result<u64, StoreError>> {
        Box::pin(self.log.appended(&self.scope, after, write))
    }

    fn remember(&self, snapshot: Snapshot) {
        self.log.remember(&self.scope, snapshot);
    }
}

pub(super) struct Logs(pub(super) Arc<OneLog>);

impl FactLogs for Logs {
    fn scoped(&self, state: &str, turns: bool) -> Arc<dyn FactLog> {
        Arc::new(Scoped {
            log: Arc::clone(&self.0),
            scope: Scope {
                state: state.to_owned(),
                turns,
            },
        })
    }
}

pub(crate) const UNKNOWN_OUTCOME: &str = "Tool execution interrupted by a host crash; outcome unknown. Do not assume it did not run or automatically repeat it.";

pub(crate) fn unknown_outcome() -> Result<Box<RawValue>, StoreError> {
    let wire = ToolOutput::error(UNKNOWN_OUTCOME)
        .into_wire()
        .map_err(|error| StoreError::NotCommitted(format!("encode an unknown outcome: {error}")))?;
    serde_json::value::to_raw_value(&wire)
        .map_err(|error| StoreError::NotCommitted(format!("encode an unknown outcome: {error}")))
}

#[derive(Clone, Copy)]
pub(crate) enum Owners {
    TurnOrNone,
    Turn,
}

pub(super) fn settle(
    writing: &mut Writing,
    turn: &str,
    owners: Owners,
) -> Result<Vec<String>, StoreError> {
    let dangling = writing
        .fold()
        .intents()
        .filter(|(_, _, call)| match (owners, call.turn.as_deref()) {
            (_, Some(owner)) => owner == turn,
            (Owners::TurnOrNone, None) => true,
            (Owners::Turn, None) => false,
        })
        .map(|(call_id, state, _)| (call_id.to_owned(), state.map(str::to_owned)))
        .collect::<Vec<_>>();
    if dangling.is_empty() {
        return Ok(Vec::new());
    }
    let output = unknown_outcome()?;
    for (call_id, state) in &dangling {
        let result: OwnedFact<Box<RawValue>> = Fact::EffectResult(EffectResult {
            call_id: call_id.clone(),
            output: output.clone(),
        });
        writing.append_owned(state.as_deref(), &result)?;
    }
    Ok(dangling.into_iter().map(|(call_id, _)| call_id).collect())
}

impl OneLog {
    pub(super) async fn abandoned(&self, call_id: &str) -> Result<Option<ToolOutput>, StoreError> {
        let call_id = call_id.to_owned();
        self.writes(move |writing| {
                Box::pin(async move {
                    let call_id = call_id.as_str();
                    let Some(state) = writing
                        .fold()
                        .intents()
                        .find(|(open, _, _)| *open == call_id)
                        .map(|(_, state, _)| state.map(str::to_owned))
                    else {
                        return Ok(None);
                    };
                    if sim::mutated(sim::Mutation::AbandonUnsettled) {
                        return Ok(Some(ToolOutput::error(UNKNOWN_OUTCOME)));
                    }
                    let result: OwnedFact<Box<RawValue>> = Fact::EffectResult(EffectResult {
                        call_id: call_id.to_owned(),
                        output: unknown_outcome()?,
                    });
                    writing.append_owned(state.as_deref(), &result)?;
                    writing.commit(Point::Cancel).await?;
                    Ok(Some(ToolOutput::error(UNKNOWN_OUTCOME)))
                })
            })
            .await
    }

    pub async fn settle_intents(&self, turn: &str) -> Result<Vec<String>, StoreError> {
        let turn = turn.to_owned();
        self.writes(move |writing| {
                Box::pin(async move {
                    let turn = turn.as_str();
                    writing.secure().await?;
                    if sim::mutated(sim::Mutation::IntentReplayed) {
                        return Ok(Vec::new());
                    }
                    let settled = settle(writing, turn, Owners::TurnOrNone)?;
                    if !settled.is_empty() {
                        writing.commit(Point::Recovery).await?;
                    }
                    Ok(settled)
                })
            })
            .await
    }
}
