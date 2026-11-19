use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    ClaimIgnoresLease,
    ClaimIgnoresNeeds,
    FinishIgnoresClaim,
    PostOverwrites,
    LeasesNeverExpire,
    ErrorsLoseWrites,
    PostIgnoresDoomedNeeds,
    KeysIgnored,
    GrantsIgnored,
    ManageUnchecked,
    RevokeIgnored,
    SpecificityIgnored,
    SubtreeSkipsRoot,
    SourceUncapped,
    CancelByAnyone,
    ForeignFinish,
    ReadLeaks,
    KeySquatting,
    MintedRestarts,
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub(crate) fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}
