use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    HoldForgetsAnswer,
    HoldPrunesUnsettled,
    PlacedInline,
    FencedServesReads,
    CellRootsSkipped,
    HostUnbounded,
    HostBoundIgnored,
    ReadsUnbounded,
    AnswersUnbounded,
    ChainMemberDropped,
    ResidentUntilSnapshot,
    UnindexedPinsPages,
    PlacedStayResident,
    FinishedKeepsPrompt,
    SealedRelowered,
    FactsResident,
    CacheNeverDue,
    ScopedInputLowered,
    BlobsResident,
    CacheInline,
    OpenMigrates,
    PrepareMigrates,
    ConflictSkipsReadBack,
    RetrySkipsReadBack,
    ScansUnbatched,
    SnapshotResetsRecovery,
    AbandonRunningJob,
    WritesUnreleasedFormat,
    OwnRowFences,
    GroupCommitOff,
    QueueDeadlineFixed,
    QueueRunsDropped,
    QueuedUntyped,
    FactsScanOnWriter,
    QueueDepthUncounted,
    SettleReloadsLog,
    ChildScopeIrreversible,
    AdoptionRescans,
    AdoptionRaceFails,
    ChildKeyFullPath,
    LegacyOpenRaceFails,
    LegacyOpenUnbounded,
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);
thread_local! {
    static FORCED: std::cell::Cell<Option<crate::Format>> = const { std::cell::Cell::new(None) };
    static RELEASED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}

pub fn force(format: Option<crate::Format>) {
    FORCED.set(format);
    assert_eq!(
        FORCED.get(),
        format,
        "a seed's thread holds the format it forced"
    );
}

pub(crate) fn forced() -> Option<crate::Format> {
    FORCED.get()
}

pub fn release_layouts(released: bool) {
    RELEASED.set(released);
    assert_eq!(
        RELEASED.get(),
        released,
        "a seed's thread holds the layouts it released"
    );
}

pub(crate) fn released(layout: crate::Layout) -> bool {
    assert!(
        crate::Layout::ALL.contains(&layout),
        "a released layout is registered"
    );
    RELEASED.get()
}

pub async fn open(
    host: std::sync::Arc<dyn crate::LogHost>,
    scope: impl Into<std::sync::Arc<str>>,
) -> Result<crate::TurnLog, nanocode_durability_core::StoreError> {
    crate::TurnLog::open(host, scope).await
}

pub async fn follow(
    host: std::sync::Arc<dyn crate::LogHost>,
    scope: impl Into<std::sync::Arc<str>>,
) -> Result<crate::Follower, nanocode_durability_core::StoreError> {
    crate::TurnLog::follow(host, scope).await
}

pub async fn claim(
    follower: crate::Follower,
) -> Result<crate::TurnLog, nanocode_durability_core::StoreError> {
    follower.claim().await
}

#[must_use]
pub fn stamped(stamp: crate::Stamp, body: &str) -> String {
    crate::formats::stamp_row(stamp, body)
}
