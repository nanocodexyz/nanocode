use std::sync::atomic::{AtomicU8, Ordering};

pub use crate::one_log::legacy::{claim, open};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    PartDropped,
    AdoptionSkipsRetired,
    AdoptionSkipsBlobs,
    JobForwarded,
    JobRowsDropped,
    ScopedIntentSkipped,
    FactsUnseenSkipped,
    RecoveryUncounted,
    HoldStaged,
    WakesUnbounded,
    WakeIndexSkipped,
    GateComparesIntentToItself,
    LostUnretried,
    KeepAllEvents,
    ReservationSkipped,
    ReimportAdvances,
    StoreSkipsFence,
    JournalLoadsFinished,
    KeepFinishedTurns,
    AdmissionSkipsHistory,
    StoreLostUnretried,
    JournalLostUnretried,
    ScopeWritesUntagged,
    ScopeStartsAtRowZero,
    FlipOffUnreadable,
    TailUnbounded,
    OpenPerRequest,
    ResidentDropsBlob,
    LegacyOpenClaims,
    CacheDueIgnoresBlob,
    ClaimSkipsRow,
    AdoptionKeepsHistory,
    AdoptedServesLegacy,
    ReplacedFoldKept,
    SwitchMissesTail,
    HandedOverWrites,
    ServedHandlesPinned,
}

impl Mutation {
    pub const ALL: [Self; 30] = [
        Self::PartDropped,
        Self::AdoptionSkipsRetired,
        Self::AdoptionSkipsBlobs,
        Self::JobForwarded,
        Self::JobRowsDropped,
        Self::ScopedIntentSkipped,
        Self::FactsUnseenSkipped,
        Self::RecoveryUncounted,
        Self::HoldStaged,
        Self::WakesUnbounded,
        Self::WakeIndexSkipped,
        Self::GateComparesIntentToItself,
        Self::LostUnretried,
        Self::KeepAllEvents,
        Self::ReservationSkipped,
        Self::ReimportAdvances,
        Self::StoreSkipsFence,
        Self::JournalLoadsFinished,
        Self::KeepFinishedTurns,
        Self::AdmissionSkipsHistory,
        Self::StoreLostUnretried,
        Self::JournalLostUnretried,
        Self::ScopeWritesUntagged,
        Self::ScopeStartsAtRowZero,
        Self::TailUnbounded,
        Self::OpenPerRequest,
        Self::ResidentDropsBlob,
        Self::CacheDueIgnoresBlob,
        Self::AdoptionKeepsHistory,
        Self::ReplacedFoldKept,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PartDropped => "part_dropped",
            Self::AdoptionSkipsRetired => "adoption_skips_retired",
            Self::AdoptionSkipsBlobs => "adoption_skips_blobs",
            Self::JobForwarded => "job_forwarded",
            Self::JobRowsDropped => "job_rows_dropped",
            Self::ScopedIntentSkipped => "scoped_intent_skipped",
            Self::FactsUnseenSkipped => "facts_unseen_skipped",
            Self::RecoveryUncounted => "recovery_uncounted",
            Self::HoldStaged => "hold_staged",
            Self::WakesUnbounded => "wakes_unbounded",
            Self::WakeIndexSkipped => "wake_index_skipped",
            Self::GateComparesIntentToItself => "gate_compares_intent_to_itself",
            Self::LostUnretried => "lost_unretried",
            Self::KeepAllEvents => "keep_all_events",
            Self::ReservationSkipped => "reservation_skipped",
            Self::ReimportAdvances => "reimport_advances",
            Self::StoreSkipsFence => "store_skips_fence",
            Self::JournalLoadsFinished => "journal_loads_finished",
            Self::KeepFinishedTurns => "keep_finished_turns",
            Self::AdmissionSkipsHistory => "admission_skips_history",
            Self::StoreLostUnretried => "store_lost_unretried",
            Self::JournalLostUnretried => "journal_lost_unretried",
            Self::ScopeWritesUntagged => "scope_writes_untagged",
            Self::ScopeStartsAtRowZero => "scope_starts_at_row_zero",
            Self::FlipOffUnreadable => "flip_off_unreadable",
            Self::TailUnbounded => "tail_unbounded",
            Self::OpenPerRequest => "open_per_request",
            Self::ResidentDropsBlob => "resident_drops_blob",
            Self::LegacyOpenClaims => "legacy_open_claims",
            Self::CacheDueIgnoresBlob => "cache_due_ignores_blob",
            Self::ClaimSkipsRow => "claim_skips_row",
            Self::AdoptionKeepsHistory => "adoption_keeps_history",
            Self::AdoptedServesLegacy => "adopted_serves_legacy",
            Self::ReplacedFoldKept => "replaced_fold_kept",
            Self::SwitchMissesTail => "switch_misses_tail",
            Self::HandedOverWrites => "handed_over_writes",
            Self::ServedHandlesPinned => "served_handles_pinned",
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

#[must_use]
pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}
