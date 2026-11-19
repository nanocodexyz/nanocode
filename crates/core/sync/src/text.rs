use std::{fmt, hash, ops::Deref, sync::Arc};

#[derive(Clone)]
pub struct SharedStr {
    text: Arc<str>,
    start: usize,
}

impl SharedStr {
    #[must_use]
    pub const fn new(text: Arc<str>) -> Self {
        Self { text, start: 0 }
    }

    #[must_use]
    pub fn suffix(text: Arc<str>, start: usize) -> Option<Self> {
        text.is_char_boundary(start).then_some(Self { text, start })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text[self.start..]
    }

    #[must_use]
    pub const fn owner(&self) -> &Arc<str> {
        &self.text
    }

    #[must_use]
    pub fn into_whole(self) -> Result<Arc<str>, Self> {
        if self.start == 0 {
            Ok(self.text)
        } else {
            Err(self)
        }
    }
}

impl Default for SharedStr {
    fn default() -> Self {
        Self::new(Arc::from(""))
    }
}

impl Deref for SharedStr {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for SharedStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for SharedStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<Arc<str>> for SharedStr {
    fn from(text: Arc<str>) -> Self {
        Self::new(text)
    }
}

impl From<String> for SharedStr {
    fn from(text: String) -> Self {
        Self::new(text.into())
    }
}

impl From<&str> for SharedStr {
    fn from(text: &str) -> Self {
        Self::new(text.into())
    }
}

impl PartialEq for SharedStr {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SharedStr {}

impl PartialEq<str> for SharedStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl hash::Hash for SharedStr {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Debug for SharedStr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl fmt::Display for SharedStr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), formatter)
    }
}
