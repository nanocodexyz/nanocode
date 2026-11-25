use nanocode_process_contracts::{FileHost, ProcessFuture};
use std::path::Path;

pub const MAX_LINES: usize = 800;
pub const MAX_BYTES: usize = 46_080;

fn absolute(path: &str) -> Result<&Path, String> {
    let candidate = Path::new(path);
    match candidate.has_root() {
        true => Ok(candidate),
        false => Err(format!(
            "declaring permissions: failed to parse path: {path} must be an absolute path: path is not absolute"
        )),
    }
}

pub async fn read_file(
    files: &dyn FileHost,
    path: &str,
    start: Option<i64>,
    end: Option<i64>,
    offset: Option<i64>,
) -> Result<String, String> {
    let file = absolute(path)?;
    check_range(start, end)?;
    let bytes = files
        .read_bytes(file)
        .await?
        .ok_or_else(|| "file does not exist".to_owned())?;
    if bytes.is_empty() {
        return Ok("File is empty".to_owned());
    }
    let text = String::from_utf8_lossy(&bytes);
    let trailing = text.ends_with('\n');
    let mut lines = text.split('\n').collect::<Vec<_>>();
    if trailing {
        lines.pop();
    }
    let total = lines.len();
    let first = usize::try_from(start.unwrap_or(1))
        .unwrap_or(1)
        .clamp(1, total);
    let requested = end.map_or(total, |end| usize::try_from(end).unwrap_or(total));
    let last = requested.min(total).min(first + MAX_LINES - 1).max(first);
    assert!(first >= 1);
    assert!(first <= last);
    assert!(last <= total);
    let mut rendered = String::new();
    for (index, line) in lines.iter().enumerate().take(last).skip(first - 1) {
        rendered.push_str(&format!("{}: {}\n", index + 1, line.trim_end_matches('\r')));
    }
    let skip = usize::try_from(offset.unwrap_or(0)).unwrap_or(0);
    if skip == 0 && rendered.len() <= MAX_BYTES {
        rendered.pop();
        let mut out = format!("Showing lines {first}-{last} of {total}\n{rendered}");
        if !trailing && last == total {
            out.push_str("\n(no newline at end of file)");
        }
        return Ok(out);
    }
    Ok(partial(&rendered, skip, (first, last, total)))
}

fn check_range(start: Option<i64>, end: Option<i64>) -> Result<(), String> {
    if let Some(start) = start
        && start < 1
    {
        return Err(format!("invalid StartLine {start}: must be at least 1"));
    }
    if let Some(end) = end
        && end < 1
    {
        return Err(format!("invalid EndLine {end}: must be at least 1"));
    }
    if let (Some(start), Some(end)) = (start, end)
        && start > end
    {
        return Err(format!(
            "invalid line range: StartLine {start} is greater than EndLine {end}"
        ));
    }
    Ok(())
}

fn partial(rendered: &str, skip: usize, (first, last, total): (usize, usize, usize)) -> String {
    let size = rendered.len();
    let from = floor(rendered, skip.min(size));
    let to = floor(rendered, (from + MAX_BYTES).min(size));
    assert!(from <= to);
    assert!(to <= size);
    let mut out = format!(
        "Partial read (lines {first}-{last} of {total}). Showing bytes {from}-{to} of {size} in this line range"
    );
    if to < size {
        out.push_str(&format!(
            ", truncated at the {MAX_BYTES}-byte limit. To read the rest of this line range, call this tool again with the same StartLine and EndLine and ByteOffset={to}"
        ));
    }
    out.push_str(".\n");
    out.push_str(&rendered[from..to]);
    out
}

const fn floor(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub async fn write_file(
    files: &dyn FileHost,
    path: &str,
    content: &str,
    overwrite: bool,
    append: bool,
) -> Result<String, String> {
    let file = absolute(path)?;
    if overwrite && append {
        return Err("Overwrite and Append cannot both be true".to_owned());
    }
    let existing = files.read_bytes(file).await;
    if !matches!(existing, Ok(None)) && !overwrite && !append {
        return Err(format!(
            "{path} already exists. To overwrite it, retry with Overwrite=true. To edit it, view its contents first and use a code edit tool. Otherwise, choose a different file name"
        ));
    }
    match (append, existing) {
        (true, Ok(Some(mut bytes))) => {
            bytes.extend_from_slice(content.as_bytes());
            files.write_bytes(file, &bytes).await?;
        }
        (true, Err(error)) => return Err(error),
        _ => files.write(file, content).await?,
    }
    Ok("Created file.".to_owned())
}

pub async fn edit_file(
    files: &dyn FileHost,
    path: &str,
    target: &str,
    replacement: &str,
) -> Result<String, String> {
    let file = absolute(path)?;
    let Ok(Some(text)) = files.read(file).await else {
        return Err(format!(
            "{path} does not exist; the file may have been moved, renamed, or deleted"
        ));
    };
    let count = match target.is_empty() {
        true => 0,
        false => text.matches(target).count(),
    };
    let Some(first) = text.find(target).filter(|_| count > 0) else {
        return Err("target content not found in the file; the Target must exactly match existing file text, including whitespace".to_owned());
    };
    let line = text[..first].matches('\n').count() + 1;
    files
        .write(file, &text.replace(target, replacement))
        .await?;
    let noun = match count {
        1 => "replacement",
        _ => "replacements",
    };
    Ok(format!(
        "Made {count} {noun} in {path} starting at line {line} (+{}/-{} lines).",
        replacement.lines().count(),
        target.lines().count()
    ))
}

pub struct LocalFiles;

fn local(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
        Ok(metadata) if metadata.is_dir() => Err("path is a directory, not a file".to_owned()),
        Ok(_) => std::fs::read(path)
            .map(Some)
            .map_err(|error| error.to_string()),
    }
}

fn store(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, bytes).map_err(|error| error.to_string())
}

impl FileHost for LocalFiles {
    fn read<'a>(&'a self, path: &'a Path) -> ProcessFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            local(path)?
                .map(|bytes| String::from_utf8(bytes).map_err(|error| error.to_string()))
                .transpose()
        })
    }

    fn write<'a>(
        &'a self,
        path: &'a Path,
        contents: &'a str,
    ) -> ProcessFuture<'a, Result<(), String>> {
        Box::pin(async move { store(path, contents.as_bytes()) })
    }

    fn remove<'a>(&'a self, path: &'a Path) -> ProcessFuture<'a, Result<(), String>> {
        Box::pin(async move { std::fs::remove_file(path).map_err(|error| error.to_string()) })
    }

    fn read_bytes<'a>(
        &'a self,
        path: &'a Path,
    ) -> ProcessFuture<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move { local(path) })
    }

    fn write_bytes<'a>(
        &'a self,
        path: &'a Path,
        bytes: &'a [u8],
    ) -> ProcessFuture<'a, Result<(), String>> {
        Box::pin(async move { store(path, bytes) })
    }
}
