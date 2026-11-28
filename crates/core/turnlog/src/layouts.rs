use crate::formats::{Formats, Release};

const CHILD_SCOPES_RELEASE: Release = Release::Unreleased;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Layout {
    ChildScopes,
}

impl Layout {
    pub const ALL: [Self; 1] = [Self::ChildScopes];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ChildScopes => "child_scopes",
        }
    }

    const fn release(self) -> Release {
        match self {
            Self::ChildScopes => CHILD_SCOPES_RELEASE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutError {
    Unreleased { layout: Layout },
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreleased { layout } => write!(
                formatter,
                "layout {} has no release that reads it, so the previous release would not find what it writes",
                layout.name()
            ),
        }
    }
}

impl std::error::Error for LayoutError {}

impl Formats {
    #[must_use]
    pub fn layouts() -> Vec<(Layout, Release)> {
        let layouts: Vec<(Layout, Release)> = Layout::ALL
            .iter()
            .map(|layout| (*layout, layout.release()))
            .collect();
        assert_eq!(layouts.len(), Layout::ALL.len());
        layouts
    }

    pub fn layout(layout: Layout) -> Result<Layout, LayoutError> {
        if crate::sim::mutated(crate::sim::Mutation::ChildScopeIrreversible) {
            return Ok(layout);
        }
        if layout.release() == Release::Unreleased && !crate::sim::released(layout) {
            return Err(LayoutError::Unreleased { layout });
        }
        assert!(
            Layout::ALL.contains(&layout),
            "a writable layout is registered"
        );
        Ok(layout)
    }

    #[must_use]
    pub fn writes(layout: Layout) -> bool {
        Self::layout(layout).is_ok()
    }
}
