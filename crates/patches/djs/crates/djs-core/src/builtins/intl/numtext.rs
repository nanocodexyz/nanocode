use super::numdata::{ENDS, TEXT};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct S(pub(super) u16);

impl S {
    pub(super) fn get(self) -> &'static str {
        let i = usize::from(self.0);
        &TEXT[ENDS[i] as usize..ENDS[i + 1] as usize]
    }
}

impl PartialEq<&str> for S {
    fn eq(&self, other: &&str) -> bool {
        self.get() == *other
    }
}

impl PartialEq<str> for S {
    fn eq(&self, other: &str) -> bool {
        self.get() == other
    }
}

impl core::ops::Deref for S {
    type Target = str;

    fn deref(&self) -> &'static str {
        self.get()
    }
}
