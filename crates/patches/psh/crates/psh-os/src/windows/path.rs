//! Paths at the boundary between the engine and Windows.
//!
//! The engine's paths are bytes, absolute and `/`-separated: `C:/Users/x` (the Host
//! contract). Windows wants UTF-16 with `\`. The host also accepts what scripts written
//! for Git Bash use, `/c/Users/x`, and the virtual paths every bash script assumes:
//! `/dev/null` is `NUL` and `/tmp` is the user's temp directory (D1075).

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

/// The user's temp directory, `/`-separated, without a trailing slash, with long names
/// (`TEMP` is often in 8.3 form, `C:/Users/ABCDEF~1/...`).
pub(crate) fn temp_dir() -> Vec<u8> {
    let tmp = std::env::temp_dir();
    let tmp = long_path(&tmp).unwrap_or(tmp);
    let mut t = engine_path(&tmp);
    while t.len() > 3 && t.last() == Some(&b'/') {
        t.pop();
    }
    t
}

/// The long form of an existing path (`GetLongPathNameW`).
fn long_path(p: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;
    let w = wide(p.as_os_str()).ok()?;
    let mut buf = vec![0u16; 1024];
    // SAFETY: a NUL-terminated input and a writable buffer of the given length.
    let n = unsafe { GetLongPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
}

/// An engine path as the host's native path. Invalid UTF-8 cannot name a Windows file
/// (the engine's bytes came from UTF-16 through UTF-8), so it is an error.
pub(crate) fn native(p: &[u8]) -> io::Result<PathBuf> {
    let s = std::str::from_utf8(p).map_err(|_| invalid("path is not valid UTF-8"))?;
    if s.contains('\0') {
        return Err(invalid("path contains a NUL byte"));
    }
    Ok(PathBuf::from(native_str(s)))
}

/// Win32 reserves NUL even with an extension or trailing spaces. Resolve it before
/// std adds a long-path prefix: verbatim paths can otherwise create real NUL.txt
/// files. Explicit verbatim paths stay literal so existing artifacts can be removed.
pub(crate) fn is_null_device(s: &str) -> bool {
    if s == "/dev/null" {
        return true;
    }
    if s.starts_with(r"\\?\") || s.starts_with("//?/") {
        return false;
    }
    let name = s.rsplit(['/', '\\']).next().unwrap_or(s);
    let stem = name.split('.').next().unwrap_or(name);
    stem.trim_end_matches(' ').eq_ignore_ascii_case("NUL")
}

fn native_str(s: &str) -> String {
    if is_null_device(s) {
        return "NUL".to_string();
    }
    if let Some(rest) = strip_dir(s, "/tmp") {
        let t = String::from_utf8_lossy(&temp_dir()).into_owned();
        return backslashes(&format!("{t}{rest}"));
    }
    // `/c` and `/c/...`: a drive, as Git Bash writes it.
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/')
    {
        let drive = (b[1] as char).to_ascii_uppercase();
        let rest = if b.len() == 2 { "/" } else { &s[2..] };
        return backslashes(&format!("{drive}:{rest}"));
    }
    backslashes(s)
}

/// `rest` (empty or starting with `/`) when `s` is `dir` or below it.
fn strip_dir<'a>(s: &'a str, dir: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(dir)?;
    (rest.is_empty() || rest.starts_with('/')).then_some(rest)
}

fn backslashes(s: &str) -> String {
    s.replace('/', "\\")
}

/// NUL-terminated UTF-16 of a native path or other OS string.
pub(crate) fn wide(s: &std::ffi::OsStr) -> io::Result<Vec<u16>> {
    let mut w: Vec<u16> = s.encode_wide().collect();
    if w.contains(&0) {
        return Err(invalid("string contains a NUL character"));
    }
    w.push(0);
    Ok(w)
}

/// A native path as the engine sees it: `/`-separated, without the `\\?\` prefix that
/// `canonicalize` adds (`\\?\UNC\server\share` becomes `//server/share`).
pub(crate) fn engine_path(p: &Path) -> Vec<u8> {
    let s = p.to_string_lossy();
    let s = if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = s.strip_prefix(r"\\?\") {
        local.to_string()
    } else {
        s.into_owned()
    };
    s.replace('\\', "/").into_bytes()
}

/// Splits the shell's `PATH` into entries (D1076). Entries are separated by `;`, the
/// native separator. A `:` separates too, as scripts written for bash assume
/// (`PATH="$HOME/bin:$PATH"`), except right after a single drive letter at the start of
/// an entry (`C:/x`, `C:\x`): that one belongs to the drive.
pub(crate) fn split_path_var(var: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    for part in var.split(|&b| b == b';') {
        let mut start = 0;
        let mut i = 0;
        while i < part.len() {
            if part[i] == b':' {
                let drive = i == start + 1 && part[start].is_ascii_alphabetic();
                if !drive {
                    out.push(&part[start..i]);
                    start = i + 1;
                }
            }
            i += 1;
        }
        out.push(&part[start..]);
    }
    out
}

/// Whether `p` (engine bytes) is absolute: `C:/...`, `/...` (`/c/...`, `/tmp`, or the
/// current drive's root) or `//server/share`.
pub(crate) fn is_absolute(p: &[u8]) -> bool {
    p.first() == Some(&b'/')
        || p.first() == Some(&b'\\')
        || (p.len() >= 3
            && p[0].is_ascii_alphabetic()
            && p[1] == b':'
            && (p[2] == b'/' || p[2] == b'\\'))
}
