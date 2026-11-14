use std::sync::{Mutex, PoisonError};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
};

/// A replaceable append-only log destination, shared by its owner's consumers.
/// File replacement and complete record writes are serialized per destination.
#[derive(Debug, Default)]
pub struct AppendLog {
    file: Mutex<Option<File>>,
}

impl AppendLog {
    /// Close the old destination before opening its replacement. On failure the
    /// log stays disabled. The header is written only to empty regular files.
    pub fn set_path(&self, path: Option<&Path>, header: &[u8]) -> io::Result<()> {
        let mut slot = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        *slot = None;
        let Some(path) = path else {
            return Ok(());
        };
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let metadata = file.metadata()?;
        if metadata.is_file() && metadata.len() == 0 {
            file.write_all(header)?;
            file.flush()?;
        }
        *slot = Some(file);
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.file.lock().unwrap_or_else(PoisonError::into_inner).is_some()
    }

    /// Append and flush a complete record, or do nothing if disabled.
    pub fn write(&self, record: &[u8]) -> io::Result<()> {
        if let Some(file) = self.file.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            file.write_all(record)?;
            file.flush()?;
        }
        Ok(())
    }
}
