#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flipped {
    scenario: &'static str,
}

impl Flipped {
    #[must_use]
    pub const fn by(scenario: &'static str) -> Self {
        assert!(
            !scenario.is_empty(),
            "a switch is flipped by a named scenario"
        );
        Self { scenario }
    }

    #[must_use]
    pub const fn scenario(self) -> &'static str {
        self.scenario
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OnDisk {
    name: &'static str,
    settings: &'static [&'static str],
    flipped: Flipped,
}

impl OnDisk {
    #[must_use]
    pub const fn new(
        name: &'static str,
        settings: &'static [&'static str],
        flipped: Flipped,
    ) -> Self {
        assert!(!name.is_empty(), "an on-disk switch has a name");
        assert!(
            settings.len() > 1,
            "an on-disk switch has a setting to flip back to"
        );
        Self {
            name,
            settings,
            flipped,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    #[must_use]
    pub const fn settings(self) -> &'static [&'static str] {
        self.settings
    }

    #[must_use]
    pub const fn flipped(self) -> Flipped {
        self.flipped
    }
}

pub const THREAD_PATH: OnDisk =
    OnDisk::new("thread_path", &["resident", "legacy"], Flipped::by("flips"));

pub const ROW_FORMAT: OnDisk = OnDisk::new("row_format", &["1", "2"], Flipped::by("flips"));

pub const CHILD_SCOPE: OnDisk =
    OnDisk::new("child_scope", &["own", "parent"], Flipped::by("flips"));

pub const ON_DISK: [OnDisk; 3] = [THREAD_PATH, ROW_FORMAT, CHILD_SCOPE];

#[must_use]
pub fn registered(name: &str) -> Option<OnDisk> {
    let found = ON_DISK.iter().copied().find(|switch| switch.name == name);
    assert!(
        found.is_none_or(|switch| switch.settings.len() > 1),
        "a registered switch has a setting to flip back to"
    );
    found
}
