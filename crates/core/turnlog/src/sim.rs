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
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}
