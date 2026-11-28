//! Switches the simulator flips to restore a journal bug its faults must catch.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    /// The subagent journal keeps its cached head revision after a write whose
    /// outcome is unknown.
    SubagentJournalKeepsRevision,
    /// The cell journals keep their cached head after a failed write.
    CellJournalKeepsHead,
    CarryIgnoresEarlierRead,
    CarryIgnoresPrunedReceipts,
    AdmissionBusyFatal,
    RestoredChildTakesTurns,
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

#[must_use]
pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}
