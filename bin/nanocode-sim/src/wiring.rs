use std::sync::atomic::{AtomicU8, Ordering};

use serde_json::value::RawValue;

use crate::rng::Rng;

const UNFORCED: u8 = 0;

static FORCED: AtomicU8 = AtomicU8::new(UNFORCED);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Wiring {
    #[default]
    Served,
    Plain,
    Routed,
}

impl Wiring {
    const ALL: [Self; 3] = [Self::Served, Self::Plain, Self::Routed];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Served => "served",
            Self::Plain => "plain",
            Self::Routed => "routed",
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::Served => 1,
            Self::Plain => 2,
            Self::Routed => 3,
        }
    }

    pub(crate) fn force(name: &str) -> bool {
        let Some(wiring) = Self::ALL.into_iter().find(|wiring| wiring.name() == name) else {
            return false;
        };
        FORCED.store(wiring.code(), Ordering::Relaxed);
        assert_eq!(Self::forced(), Some(wiring), "a forced wiring reads back");
        true
    }

    pub(crate) fn forced() -> Option<Self> {
        let code = FORCED.load(Ordering::Relaxed);
        Self::ALL.into_iter().find(|wiring| wiring.code() == code)
    }

    pub(crate) fn drawn(rng: &mut Rng, minority: Self, minority_ppm: u64) -> Self {
        assert_ne!(
            minority,
            Self::Served,
            "the host's served wiring is the default"
        );
        let drawn = if rng.chance(minority_ppm) {
            minority
        } else {
            Self::Served
        };
        Self::forced().unwrap_or(drawn)
    }

    pub(crate) fn flag() -> String {
        Self::forced().map_or_else(String::new, |wiring| format!(" --wiring {}", wiring.name()))
    }
}

pub(crate) fn recorded<'a>(
    mut hosts: impl Iterator<Item = (&'a str, &'a RawValue)>,
    scope: &str,
) -> bool {
    assert!(!scope.is_empty(), "a child scope has a name");
    hosts.any(|(_, value)| {
        serde_json::from_str::<String>(value.get()).is_ok_and(|named| named == scope)
    })
}

pub(crate) fn misplaced(
    state: &str,
    scope: &str,
    recorded: bool,
    own_rows: usize,
) -> Option<String> {
    match (recorded, own_rows) {
        (true, 0) => Some(format!(
            "child {state} is recorded in its own scope {scope}, which holds no rows: its state stayed on the parent"
        )),
        (false, rows) if rows > 0 => Some(format!(
            "child {state} holds {rows} rows in its own scope {scope} with no record on the parent"
        )),
        _ => None,
    }
}
