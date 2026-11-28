use nanocode_digest::Blake3;
use nanocode_sys::base64::{URL_SAFE_NO_PAD, encoded_length};

use crate::formats::{Formats, Release};

const CHILD_SCOPES_RELEASE: Release = Release::Unreleased;
const CHILD_SCOPE_DOMAIN: &[u8] = b"nanocode child scope\0";
pub const CHILD_SCOPE_DIGEST_BYTES: usize = 16;
pub const CHILD_SCOPE_CHARS: usize = match encoded_length(CHILD_SCOPE_DIGEST_BYTES, false) {
    Some(chars) => chars,
    None => panic!("a child scope digest encodes to a bounded key"),
};

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

#[must_use]
pub fn child_scope_key(parent: &str, state: &str) -> String {
    assert!(!parent.is_empty(), "a child scope names its parent");
    assert!(!state.is_empty(), "a child scope names its state");
    if crate::sim::mutated(crate::sim::Mutation::ChildKeyFullPath) {
        return format!("{parent}/child/{state}");
    }
    let parent_bytes = u64::try_from(parent.len()).expect("a scope name's length fits in u64");
    let mut hasher = Blake3::new();
    hasher
        .update(CHILD_SCOPE_DOMAIN)
        .update(&parent_bytes.to_le_bytes())
        .update(parent.as_bytes())
        .update(state.as_bytes());
    let digest = hasher.finalize();
    let truncated = digest.as_bytes().get(..CHILD_SCOPE_DIGEST_BYTES);
    let key = URL_SAFE_NO_PAD.encode(truncated.expect("a digest holds the key's bytes"));
    assert_eq!(
        key.len(),
        CHILD_SCOPE_CHARS,
        "a child scope key is fixed-width"
    );
    assert!(child_scope_keyed(&key), "a child scope key decodes as one");
    key
}

#[must_use]
pub fn child_scope_keyed(scope: &str) -> bool {
    if scope.len() != CHILD_SCOPE_CHARS {
        return false;
    }
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(scope) else {
        return false;
    };
    assert_eq!(
        bytes.len(),
        CHILD_SCOPE_DIGEST_BYTES,
        "a fixed-width key decodes to the digest's width"
    );
    URL_SAFE_NO_PAD.encode(&bytes) == scope
}
