use std::{ffi::OsString, sync::OnceLock};

static TAKEN: OnceLock<Vec<(&'static str, OsString)>> = OnceLock::new();

#[doc(hidden)]
pub fn take_before_threads(names: &'static [&'static str]) {
    TAKEN.get_or_init(|| {
        names
            .iter()
            .filter_map(|name| {
                let value = std::env::var_os(name)?;
                unsafe { std::env::remove_var(name) };
                Some((*name, value))
            })
            .collect()
    });
}

pub fn taken(name: &str) -> Option<&'static OsString> {
    TAKEN
        .get()?
        .iter()
        .find(|(taken, _)| *taken == name)
        .map(|(_, value)| value)
}
