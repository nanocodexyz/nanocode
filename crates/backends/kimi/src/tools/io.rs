//! Where the built-in tools touch files and processes: the local machine, or a host's `FileHost` and
//! `ForegroundExecutor` (`KimiHarness::host`, the way Codex's and Claude's `host_tools` run on wasm).
//! Host failures come back as `std::io::Error`s so the tools keep Kimi Code's messages.

pub use nanocode_process_contracts::ForegroundOutput;
use nanocode_process_contracts::{FileHost, ForegroundCommand, ForegroundExecutor, OutputSink};
use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A host's file system and shell, which must see the same files.
#[derive(Clone)]
pub struct Host {
    pub files: Arc<dyn FileHost>,
    pub shell: Arc<dyn ForegroundExecutor>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    File(u64),
    Dir,
    /// Neither a regular file nor a directory (a socket, a FIFO, a device).
    Special,
    Missing,
}

/// Single-quotes `text` for a POSIX shell.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// What is at `path`: a file and its size, a directory, or nothing.
pub async fn kind(host: Option<&Host>, path: &Path) -> Result<Kind> {
    let Some(host) = host else {
        return match std::fs::metadata(path) {
            Ok(stat) if stat.is_dir() => Ok(Kind::Dir),
            Ok(stat) if stat.is_file() => Ok(Kind::File(stat.len())),
            Ok(_) => Ok(Kind::Special),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Kind::Missing),
            Err(error) => Err(error),
        };
    };
    let path = quote(&path.display().to_string());
    // A missing path under a file is ENOTDIR, and a link that resolves nowhere is a loop or dangling,
    // as `stat` tells them apart.
    let probe = format!(
        "p={path}; if [ -d \"$p\" ]; then echo dir; elif [ -f \"$p\" ]; then wc -c < \"$p\"; \
         elif [ -e \"$p\" ]; then echo special; \
         elif [ -L \"$p\" ] && ls -L \"$p\" 2>&1 >/dev/null | grep -q 'levels of symbolic'; then echo loop; \
         else q=\"$p\"; while q=$(dirname \"$q\"); [ \"$q\" != / ] && [ ! -e \"$q\" ]; do :; done; \
         if [ -e \"$q\" ] && [ ! -d \"$q\" ]; then echo notdir; else echo missing; fi; fi"
    );
    let out = run(
        host,
        Path::new("/"),
        &probe,
        Duration::from_secs(30),
        4096,
        None,
    )
    .await?;
    match out.stdout.trim() {
        "dir" => Ok(Kind::Dir),
        "special" => Ok(Kind::Special),
        "missing" => Ok(Kind::Missing),
        "notdir" => Err(ErrorKind::NotADirectory.into()),
        "loop" => Err(Error::other("too many levels of symbolic links")),
        size => size
            .parse()
            .map(Kind::File)
            .map_err(|_| host_error(format!("{} {}", out.stdout, out.stderr).trim())),
    }
}

pub async fn read(host: Option<&Host>, path: &Path) -> Result<Vec<u8>> {
    let Some(host) = host else {
        return std::fs::read(path);
    };
    match host.files.read_bytes(path).await {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Err(ErrorKind::NotFound.into()),
        Err(message) => Err(host_error(&message)),
    }
}

/// Writes `bytes`, or appends them; a missing parent directory is an error (`NotFound`).
pub async fn write(host: Option<&Host>, path: &Path, bytes: &[u8], append: bool) -> Result<()> {
    let Some(host) = host else {
        use std::io::Write as _;
        return std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(path)?
            .write_all(bytes);
    };
    let bytes = match append {
        true => match read(Some(host), path).await {
            Ok(mut existing) => {
                existing.extend_from_slice(bytes);
                existing
            }
            Err(error) if error.kind() == ErrorKind::NotFound => bytes.to_vec(),
            Err(error) => return Err(error),
        },
        false => bytes.to_vec(),
    };
    host.files
        .write_bytes(path, &bytes)
        .await
        .map_err(|message| host_error(&message))
}

/// Saves `bytes` at `path`, creating its directory: the tools' own stores (spilled results, media, task
/// logs).
pub async fn store(host: Option<&Host>, path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(host, parent).await?;
    }
    write(host, path, bytes, false).await
}

pub async fn create_dir_all(host: Option<&Host>, path: &Path) -> Result<()> {
    let Some(host) = host else {
        return std::fs::create_dir_all(path);
    };
    let command = format!("mkdir -p {}", quote(&path.display().to_string()));
    let out = run(
        host,
        Path::new("/"),
        &command,
        Duration::from_secs(30),
        4096,
        None,
    )
    .await?;
    match out.exit_code {
        0 => Ok(()),
        _ => Err(host_error(out.stderr.trim())),
    }
}

/// Runs `command` through the host's shell in `cwd`.
pub async fn run(
    host: &Host,
    cwd: &Path,
    command: &str,
    timeout: Duration,
    max_output: usize,
    output: Option<&dyn OutputSink>,
) -> Result<ForegroundOutput> {
    host.shell
        .execute(ForegroundCommand {
            workspace: cwd,
            command,
            timeout,
            max_stdout: max_output,
            max_stderr: max_output,
            session: None,
            output,
        })
        .await
        .map_err(|message| host_error(&message))
}

/// A host's error message as the `std::io::Error` it names; Kimi Code reports any other one as
/// unrecognized.
fn host_error(message: &str) -> Error {
    let lower = message.to_lowercase();
    let kind = match () {
        () if lower.contains("is a directory") || lower.contains("eisdir") => {
            ErrorKind::IsADirectory
        }
        () if lower.contains("not a directory") || lower.contains("enotdir") => {
            ErrorKind::NotADirectory
        }
        () if lower.contains("no such file")
            || lower.contains("enoent")
            || lower.contains("not found") =>
        {
            ErrorKind::NotFound
        }
        () if lower.contains("permission denied") || lower.contains("eacces") => {
            ErrorKind::PermissionDenied
        }
        () => return Error::other(message.to_owned()),
    };
    kind.into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_errors_keep_kimi_kinds() {
        use std::io::ErrorKind;
        assert_eq!(
            super::host_error("EISDIR: illegal operation").kind(),
            ErrorKind::IsADirectory
        );
        assert_eq!(
            super::host_error("No such file or directory").kind(),
            ErrorKind::NotFound
        );
        assert_eq!(super::quote("it's"), r"'it'\''s'");
    }
}
