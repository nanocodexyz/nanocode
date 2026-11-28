use super::io::{self, Kind};
use super::{APPLIED, Access, Builtins, Outcome, input};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

const MAX_CHARS: i64 = 100_000;
const TOKEN_LIMIT: i64 = (MAX_CHARS + 3) / 4;
const RANGE_STATUS_TOKENS: i64 = 256;
const TRANSCODE_MAX_BYTES: usize = 10_485_760;
const EMPTY_STATUS: &str = "<system>Tool output is empty.</system>";
const ERROR_STATUS: &str = "<system>ERROR: Tool execution failed.</system>";

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    line_offset: Option<i64>,
    column_offset: Option<i64>,
    n_lines: Option<i64>,
    max_chars: Option<i64>,
}

struct Request<'a> {
    args: &'a ReadInput,
    max_chars: i64,
    /// `Some(true)` for UTF-16 LE, `Some(false)` for UTF-16 BE.
    utf16: Option<bool>,
    lossy: bool,
}

/// Kimi Code's `ReadTool` (`execution`, `readForward`, `readTail`, `finishPage`).
pub async fn read(builtins: &Builtins, args: &Value) -> Outcome {
    let args: ReadInput = match input("Read", args) {
        Ok(args) => args,
        Err(error) => return error,
    };
    if args.column_offset.is_some() && args.line_offset.unwrap_or(1) < 0 {
        return Outcome::error(
            "column_offset is only supported for forward reads. Use a positive line_offset or the forward Next Read arguments.",
        );
    }
    let path = match builtins.resolve("Read", &args.path, Access::Read) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let shown = &args.path;
    match io::kind(builtins.host(), &path).await {
        Ok(Kind::Missing) => {
            return Outcome::error(format!("\"{shown}\" does not exist."));
        }
        Err(error) if missing(&error) => {
            return Outcome::error(format!("\"{shown}\" does not exist."));
        }
        Err(error) => return Outcome::error(fs_error(&error, "stat")),
        Ok(Kind::Dir | Kind::Special) => {
            return Outcome::error(format!("\"{shown}\" is not a file."));
        }
        Ok(Kind::File(_)) => {}
    }
    let bytes = match io::read(builtins.host(), &path).await {
        Ok(bytes) => bytes,
        Err(error) => return Outcome::error(fs_error(&error, "read")),
    };
    let header = &bytes[..bytes.len().min(512)];
    let (kind, _) = file_kind(&path.to_string_lossy(), header, false);
    // K3 takes images but not videos (Kimi Code's `image_in`, no `video_in`), and ReadMediaFile is active.
    if kind == "image" {
        return Outcome::error(format!(
            "\"{shown}\" is an image file. Only text files can be read; use ReadMediaFile for image and video files."
        ));
    }
    if kind == "video" {
        return Outcome::error(format!(
            "\"{shown}\" is a video file. The current model does not support video input (missing video_in capability), so this agent cannot view it. Only text files can be read."
        ));
    }
    let utf16 = sniff_utf16(header);
    let (lines, lossy) = match utf16 {
        Some(le) if bytes.len() > TRANSCODE_MAX_BYTES => {
            return Outcome::error(format!(
                "\"{shown}\" is {} text but too large to transcode ({} bytes > {TRANSCODE_MAX_BYTES}). Convert it to UTF-8 first (e.g. with `iconv`).",
                encoding_name(le),
                bytes.len()
            ));
        }
        Some(le) => {
            let (text, lossy) = decode_utf16(&bytes, le);
            let lines = text
                .split_inclusive('\n')
                .map(|line| Some(line.to_owned()))
                .collect();
            (lines, lossy)
        }
        None if kind == "unknown" => return Outcome::error(not_readable(shown)),
        // Lines split on LF bytes and decoded one by one; `None` marks a line that is not valid UTF-8.
        None => (
            bytes
                .split_inclusive(|&byte| byte == b'\n')
                .enumerate()
                .map(|(index, line)| {
                    let text = std::str::from_utf8(line).ok()?;
                    Some(match index {
                        0 => text.strip_prefix('\u{feff}').unwrap_or(text).to_owned(),
                        _ => text.to_owned(),
                    })
                })
                .collect::<Vec<_>>(),
            false,
        ),
    };
    let request = Request {
        args: &args,
        max_chars: args.max_chars.unwrap_or(MAX_CHARS).min(MAX_CHARS),
        utf16,
        lossy,
    };
    // ponytail: Kimi's "File changed while reading its tail" race check is skipped; the file is read once.
    match args.line_offset.unwrap_or(1) {
        offset if offset >= 0 => read_forward(&request, &lines, offset),
        offset => read_tail(&request, &lines, -offset),
    }
}

fn read_forward(request: &Request, lines: &[Option<String>], offset: i64) -> Outcome {
    let args = request.args;
    let column = args.column_offset.unwrap_or(0);
    let wanted = args.n_lines.unwrap_or(i64::MAX);
    let mut selected: Vec<(i64, &str)> = Vec::new();
    let (mut closed, mut minimum, mut number) = (false, 0, 0);
    for line in lines {
        let Some(raw) = line else {
            return Outcome::error(not_utf8(&args.path));
        };
        if raw.contains('\0') {
            return Outcome::error(not_readable(&args.path));
        }
        number += 1;
        if closed || number < offset {
            continue;
        }
        if selected.len() as i64 >= wanted {
            closed = true;
            continue;
        }
        let content = raw.strip_suffix('\n').unwrap_or(raw);
        let skipped = if number == offset { column } else { 0 };
        let chars = digits(number)
            + 1
            + (units(content) - i64::from(content.ends_with('\r')) - skipped).max(0)
            + i64::from(!selected.is_empty());
        if minimum + chars > request.max_chars && !selected.is_empty() {
            closed = true;
            continue;
        }
        selected.push((number, content));
        minimum += chars;
        closed = selected.len() as i64 >= wanted || minimum >= request.max_chars;
    }
    let style = ending(lines.iter().flatten().map(String::as_str));
    let mut rendered: Vec<String> = selected
        .iter()
        .map(|&(number, content)| render_line(number, content, style))
        .collect();
    if column > 0 {
        let prefix = format!("{offset}\t");
        let Some(text) = rendered
            .first()
            .map(|line| &line[prefix.len()..])
            .filter(|text| column <= units(text))
        else {
            return Outcome::error(format!(
                "column_offset={column} is past the end of the starting line {offset}. Read the line from column 0 to inspect its current contents."
            ));
        };
        let (at, splits) = utf16_offset(text, column);
        if splits {
            return Outcome::error(format!(
                "column_offset={column} splits a Unicode character in line {offset}. Use a character boundary or the Next Read arguments."
            ));
        }
        rendered[0] = format!("{prefix}{}", &text[at..]);
    }
    let end = number.min(offset.saturating_add(wanted) - 1);
    finish_page(
        request,
        &rendered,
        offset,
        (offset, end),
        number,
        false,
        style == Ending::Mixed,
    )
}

fn read_tail<'a>(request: &Request, lines: &'a [Option<String>], tail: i64) -> Outcome {
    let args = request.args;
    let wanted = args.n_lines.unwrap_or(i64::MAX);
    let single_pass = wanted >= tail;
    let mut kept = std::collections::VecDeque::<(i64, &str, i64)>::new();
    let mut chars = 0;
    let mut retain = |raw: &'a str, number: i64| {
        let content = raw.strip_suffix('\n').unwrap_or(raw);
        let minimum = digits(number) + 2 + units(content) - i64::from(content.ends_with('\r'));
        kept.push_back((number, content, minimum));
        chars += minimum;
        while !kept.is_empty() && (chars - 1 > request.max_chars || kept.len() as i64 > tail) {
            chars -= kept.pop_front().map_or(0, |entry| entry.2);
        }
    };
    let mut total = 0;
    for line in lines {
        let Some(raw) = line else {
            return Outcome::error(not_utf8(&args.path));
        };
        if raw.contains('\0') {
            return Outcome::error(not_readable(&args.path));
        }
        total += 1;
        if single_pass {
            retain(raw, total);
        }
    }
    let start = (total - tail + 1).max(1);
    let end = total.min(start.saturating_add(wanted) - 1);
    if !single_pass {
        for (number, line) in (1..=end)
            .zip(lines)
            .skip_while(|(number, _)| *number < start)
        {
            retain(line.as_deref().unwrap_or_default(), number);
        }
    }
    let style = ending(lines.iter().flatten().map(String::as_str));
    let rendered: Vec<String> = kept
        .iter()
        .map(|&(number, content, _)| render_line(number, content, style))
        .collect();
    let first = end - rendered.len() as i64 + 1;
    finish_page(
        request,
        &rendered,
        first,
        (start, end),
        total,
        true,
        style == Ending::Mixed,
    )
}

/// Kimi Code's `finishPage`: fits the selected lines and the `<system>` status into `max_chars` and the
/// token budget, cutting a single forward line into a fragment when it alone does not fit.
fn finish_page(
    request: &Request,
    lines: &[String],
    start: i64,
    range: (i64, i64),
    total: i64,
    tail: bool,
    mixed: bool,
) -> Outcome {
    let (args, max) = (request.args, request.max_chars);
    let counts: Vec<(i64, i64)> = lines.iter().map(|line| token_chars(line)).collect();
    let (mut first, mut end) = (0, lines.len());
    let mut content = lines.iter().map(|line| units(line) + 1).sum::<i64>() - 1;
    let (mut ascii, mut non_ascii) = counts
        .iter()
        .fold((0, 0), |sum, count| (sum.0 + count.0, sum.1 + count.1));
    let column = if tail {
        0
    } else {
        args.column_offset.unwrap_or(0)
    };
    let prefix = format!("{start}\t");
    let text = lines.first().map_or("", |line| &line[prefix.len()..]);
    let text_units = units(text);
    let mut fragment: Option<i64> = None;
    loop {
        let count = (end - first) as i64;
        let from = start + first as i64;
        let to = start + end as i64 - 1;
        let partial = fragment.is_some();
        let complete =
            range.0 > range.1 || (count > 0 && from == range.0 && to == range.1 && !partial);
        let mut parts = vec![
            match count {
                0 => "No lines read from file.".to_owned(),
                1 => format!("1 line read from file starting from line {from}."),
                _ => format!("{count} lines read from file starting from line {from}."),
            },
            format!("Total lines in file: {total}."),
            (if complete {
                "Requested range complete."
            } else {
                "Character limit reached."
            })
            .to_owned(),
            format!("Effective max_chars: {max}."),
        ];
        if !partial && ((count > 0 && to == total) || range.0 > total) {
            parts.push("End of file reached.".to_owned());
        }
        let summary = parts.len();
        if count > 0 && !tail && (column > 0 || partial) {
            parts.push(format!(
                "Line {from} fragment: columns [{column}, {}) of {}. {}",
                column + fragment.unwrap_or(text_units),
                column + text_units,
                if partial {
                    "Line continues."
                } else {
                    "Line complete."
                }
            ));
        }
        if let Some(asked) = args.max_chars.filter(|&asked| asked > MAX_CHARS) {
            parts.push(format!(
                "Requested max_chars={asked} was capped at the configured maximum {MAX_CHARS}."
            ));
        }
        if !complete && (count > 0 || tail) {
            let next_start = if tail {
                range.0
            } else if partial {
                from
            } else {
                to + 1
            };
            let next_end = if tail && count > 0 { from - 1 } else { range.1 };
            let mut next = format!(
                "{{\"path\":{},\"line_offset\":{next_start}",
                json_string(&args.path)
            );
            if let Some(fragment) = fragment {
                next += &format!(",\"column_offset\":{}", column + fragment);
            }
            if tail || args.n_lines.is_some() {
                next += &format!(",\"n_lines\":{}", next_end - next_start + 1);
            }
            parts.push(format!("Next Read: {next},\"max_chars\":{max}}}"));
            if count > 0 {
                parts.push("If the answer may be further on, continue with Next Read or find the section with Grep instead of answering from this page alone.".to_owned());
            }
        }
        if mixed {
            parts.push("Mixed or lone carriage-return line endings are shown as \\r. Use exact \\r\\n or \\r escapes in Edit.old_string for those lines.".to_owned());
        }
        if let Some(le) = request.utf16 {
            parts.push(format!("Detected file encoding: {}; content transcoded to UTF-8 for display. Edit and Write expect UTF-8 — convert the file's encoding first (e.g. `iconv` via Bash).", encoding_name(le)));
        }
        if request.lossy {
            parts.push("Lossy UTF-16 decoding: malformed sequences were replaced with U+FFFD. The decoded text may differ from the original file.".to_owned());
        }
        let quiet = complete
            && count > 0
            && column == 0
            && from == 1
            && to == total
            && parts.len() == summary;
        let note = (!quiet).then(|| format!("<system>{}</system>", parts.join(" ")));
        let note_units = note.as_deref().map_or(0, |note| 1 + units(note));
        let rendered = (if count == 0 {
            units(EMPTY_STATUS)
        } else {
            content
        }) + note_units;
        let note_counts = note.as_deref().map_or((0, 0), |note| {
            let (ascii, non_ascii) = token_chars(note);
            (1 + ascii, non_ascii)
        });
        let over = match count {
            0 => 0,
            _ => {
                (ascii + count - 1 + note_counts.0 + 3) / 4 + non_ascii + note_counts.1
                    - TOKEN_LIMIT
            }
        };
        if rendered <= max
            && over > 0
            && count > 1
            && column == 0
            && (args.n_lines.is_some() || tail)
        {
            return range_too_large(&args.path, &counts, tail, range);
        }
        if rendered <= max && over <= 0 && (complete || count > 0) {
            let output = match fragment {
                Some(fragment) => format!("{prefix}{}", &text[..utf16_offset(text, fragment).0]),
                None => lines[first..end].join("\n"),
            };
            return Outcome {
                output,
                note,
                ..Outcome::default()
            };
        }
        if count == 1 && !tail {
            let previous = fragment.unwrap_or(text_units);
            let mut cut = (previous - 1).min(previous - (rendered - max));
            if over > 0 {
                cut = cut.min(cut_for_tokens(text, previous, over));
            }
            if cut > 0 && utf16_offset(text, cut).1 {
                cut -= 1;
            }
            if cut <= 0 {
                return Outcome::error(format!(
                    "max_chars={max} is too small for file text and the Read status. Increase max_chars."
                ));
            }
            content = units(&prefix) + cut;
            (ascii, non_ascii) =
                token_chars(&format!("{prefix}{}", &text[..utf16_offset(text, cut).0]));
            fragment = Some(cut);
            continue;
        }
        if count == 0 {
            let output = "No complete line fits. Continue with the forward Next Read.";
            if !complete && units(ERROR_STATUS) + 1 + units(output) + note_units <= max {
                return Outcome {
                    output: output.to_owned(),
                    failed: true,
                    note,
                    ..Outcome::default()
                };
            }
            return Outcome::error(format!(
                "max_chars={max} is too small for the Read status. Increase max_chars."
            ));
        }
        let mut remaining = if rendered <= max && over > 0 {
            over as f64
        } else {
            0.0
        };
        loop {
            let dropped = if tail {
                first += 1;
                first - 1
            } else {
                end -= 1;
                end
            };
            let (dropped_ascii, dropped_non_ascii) = counts[dropped];
            content -= units(&lines[dropped]) + 1;
            ascii -= dropped_ascii;
            non_ascii -= dropped_non_ascii;
            remaining -= dropped_non_ascii as f64 + (dropped_ascii + 1) as f64 / 4.0;
            if remaining <= 0.0 || end - first <= usize::from(!tail) {
                break;
            }
        }
    }
}

fn range_too_large(path: &str, counts: &[(i64, i64)], tail: bool, range: (i64, i64)) -> Outcome {
    let mut order: Vec<&(i64, i64)> = counts.iter().collect();
    if tail {
        order.reverse();
    }
    let (mut ascii, mut non_ascii, mut fit) = (0, 0, 0);
    for count in order {
        ascii += count.0 + i64::from(fit > 0);
        non_ascii += count.1;
        if (ascii + 3) / 4 + non_ascii > TOKEN_LIMIT - RANGE_STATUS_TOKENS {
            break;
        }
        fit += 1;
    }
    let lines = fit.max(1);
    let offset = if tail { range.1 - lines + 1 } else { range.0 };
    Outcome::error(format!(
        "Lines {}-{} are more than one Read can return (about {TOKEN_LIMIT} estimated tokens). Read at most {lines} lines at a time, for example {{\"path\":{},\"line_offset\":{offset},\"n_lines\":{lines}}}, or find the section with Grep.",
        range.0,
        range.1,
        json_string(path)
    ))
}

/// Kimi Code's `cutForTokens`: walks back from UTF-16 offset `end` until `overflow` estimated tokens are cut.
fn cut_for_tokens(text: &str, end: i64, overflow: i64) -> i64 {
    let units: Vec<u16> = text.encode_utf16().collect();
    let (mut index, mut removed) = (end, 0.0);
    while index > 0 && removed < overflow as f64 {
        let code = units[index as usize - 1];
        index -= if (0xDC00..=0xDFFF).contains(&code) && index > 1 {
            2
        } else {
            1
        };
        removed += if code <= 127 { 0.25 } else { 1.0 };
    }
    index
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    Lf,
    Crlf,
    Mixed,
}

/// Kimi Code's `detectLineEndingStyle`.
fn ending<'a>(texts: impl Iterator<Item = &'a str>) -> Ending {
    let (mut crlf, mut lf, mut lone) = (false, false, false);
    for text in texts {
        let bytes = text.as_bytes();
        for (index, &byte) in bytes.iter().enumerate() {
            match byte {
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => crlf = true,
                b'\r' => lone = true,
                b'\n' if index == 0 || bytes[index - 1] != b'\r' => lf = true,
                _ => {}
            }
        }
    }
    match (crlf, lf, lone) {
        (_, _, true) | (true, true, _) => Ending::Mixed,
        (true, _, _) => Ending::Crlf,
        _ => Ending::Lf,
    }
}

fn render_line(number: i64, content: &str, style: Ending) -> String {
    match style {
        Ending::Crlf => format!(
            "{number}\t{}",
            content.strip_suffix('\r').unwrap_or(content)
        ),
        Ending::Mixed => format!("{number}\t{}", content.replace('\r', "\\r")),
        Ending::Lf => format!("{number}\t{content}"),
    }
}

/// Length in UTF-16 code units, as JavaScript counts string length.
fn units(text: &str) -> i64 {
    text.encode_utf16().count() as i64
}

/// The byte index of UTF-16 offset `offset` in `text`, and whether that offset splits a surrogate pair.
fn utf16_offset(text: &str, offset: i64) -> (usize, bool) {
    let mut seen = 0;
    for (index, char) in text.char_indices() {
        if seen >= offset {
            return (index, seen > offset);
        }
        seen += char.len_utf16() as i64;
    }
    (text.len(), seen > offset)
}

/// Kimi Code's `countTokenChars`: ASCII and non-ASCII code points.
fn token_chars(text: &str) -> (i64, i64) {
    let ascii = text.bytes().filter(u8::is_ascii).count() as i64;
    (ascii, text.chars().count() as i64 - ascii)
}

fn digits(number: i64) -> i64 {
    number.to_string().len() as i64
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

const fn encoding_name(little_endian: bool) -> &'static str {
    if little_endian {
        "UTF-16 LE"
    } else {
        "UTF-16 BE"
    }
}

fn not_readable(path: &str) -> String {
    format!("\"{path}\" is not readable as UTF-8 text. Only text files can be read.")
}

fn not_utf8(path: &str) -> String {
    format!(
        "\"{path}\" is not valid UTF-8 or UTF-16 text. Only UTF-8 and UTF-16 text files can be read; for other encodings (e.g. GBK), convert the file to UTF-8 first (e.g. with `iconv`)."
    )
}

/// `TextDecoder("utf-16le"/"utf-16be")`: strips a BOM; malformed input is replaced with U+FFFD and
/// reported as lossy.
fn decode_utf16(bytes: &[u8], little_endian: bool) -> (String, bool) {
    let units = bytes.chunks_exact(2).map(|pair| match little_endian {
        true => u16::from_le_bytes([pair[0], pair[1]]),
        false => u16::from_be_bytes([pair[0], pair[1]]),
    });
    let mut lossy = bytes.len() % 2 == 1;
    let mut text: String = char::decode_utf16(units)
        .map(|unit| {
            unit.unwrap_or_else(|_| {
                lossy = true;
                char::REPLACEMENT_CHARACTER
            })
        })
        .collect();
    if bytes.len() % 2 == 1 {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    match text.strip_prefix('\u{feff}') {
        Some(rest) => (rest.to_owned(), lossy),
        None => (text, lossy),
    }
}

/// Kimi Code's `sniffTextEncoding`, reduced to what Read uses: `Some(little_endian)` for UTF-16.
fn sniff_utf16(header: &[u8]) -> Option<bool> {
    if header.starts_with(&[0xFE, 0xFF]) {
        return Some(false);
    }
    if header.starts_with(&[0xFF, 0xFE]) {
        return Some(true);
    }
    if header.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return None;
    }
    let zeros = |skip| {
        header
            .iter()
            .skip(skip)
            .step_by(2)
            .filter(|&&byte| byte == 0)
            .count()
    };
    match (zeros(0), zeros(1)) {
        (0, odd) if odd >= 2 => Some(true),
        (even, 0) if even >= 2 => Some(false),
        _ => None,
    }
}

/// Kimi Code's `IMAGE_MIME_BY_SUFFIX` and `VIDEO_MIME_BY_SUFFIX`.
pub const IMAGE_SUFFIXES: [(&str, &str); 13] = [
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".bmp", "image/bmp"),
    (".tif", "image/tiff"),
    (".tiff", "image/tiff"),
    (".webp", "image/webp"),
    (".ico", "image/x-icon"),
    (".heic", "image/heic"),
    (".heif", "image/heif"),
    (".avif", "image/avif"),
    (".svgz", "image/svg+xml"),
];
pub const VIDEO_SUFFIXES: [(&str, &str); 13] = [
    (".mp4", "video/mp4"),
    (".mpg", "video/mpeg"),
    (".mpeg", "video/mpeg"),
    (".mkv", "video/x-matroska"),
    (".avi", "video/x-msvideo"),
    (".mov", "video/quicktime"),
    (".ogv", "video/ogg"),
    (".wmv", "video/x-ms-wmv"),
    (".webm", "video/webm"),
    (".m4v", "video/x-m4v"),
    (".flv", "video/x-flv"),
    (".3gp", "video/3gpp"),
    (".3g2", "video/3gpp2"),
];

/// Kimi Code's `detectFileType(path, header, type)`: the kind (`text`, `image`, `video` or `unknown`) and
/// MIME type. In `media` mode a sniffed signature wins over the extension, and a video extension stands
/// without one.
pub fn file_kind(path: &str, header: &[u8], media: bool) -> (&'static str, &'static str) {
    const UNKNOWN: (&str, &str) = ("unknown", "");
    const NON_TEXT: &[&str] = &[
        ".icns", ".psd", ".ai", ".eps", ".pdf", ".doc", ".docx", ".dot", ".dotx", ".rtf", ".odt",
        ".xls", ".xlsx", ".xlsm", ".xlt", ".xltx", ".xltm", ".ods", ".ppt", ".pptx", ".pptm",
        ".pps", ".ppsx", ".odp", ".pages", ".numbers", ".key", ".zip", ".rar", ".7z", ".tar",
        ".gz", ".tgz", ".bz2", ".xz", ".zst", ".lz", ".lz4", ".br", ".cab", ".ar", ".deb", ".rpm",
        ".mp3", ".wav", ".flac", ".ogg", ".oga", ".opus", ".aac", ".m4a", ".wma", ".ttf", ".otf",
        ".woff", ".woff2", ".exe", ".dll", ".so", ".dylib", ".bin", ".apk", ".ipa", ".jar",
        ".class", ".pyc", ".pyo", ".wasm", ".dmg", ".iso", ".img", ".sqlite", ".sqlite3", ".db",
        ".db3",
    ];
    let floor = path.rfind(['/', '\\']).map_or(0, |slash| slash + 1);
    let suffix = match path.rfind('.') {
        Some(dot) if dot > floor => path[dot..].to_lowercase(),
        _ => String::new(),
    };
    let suffix = suffix.as_str();
    let find = |table: &[(&'static str, &'static str)], kind| {
        table
            .iter()
            .find(|(known, _)| *known == suffix)
            .map(|&(_, mime)| (kind, mime))
    };
    let hint = match suffix {
        ".svg" => Some(("text", "image/svg+xml")),
        _ => find(&IMAGE_SUFFIXES, "image").or_else(|| find(&VIDEO_SUFFIXES, "video")),
    };
    if let Some(sniffed) = sniff_media(header) {
        return match hint {
            _ if media => sniffed,
            Some(hint) if hint.0 != sniffed.0 => UNKNOWN,
            Some(hint) => hint,
            None => sniffed,
        };
    }
    match hint {
        Some(("image", _)) => UNKNOWN,
        Some(hint @ ("video", _)) if media => hint,
        _ if header.contains(&0) => UNKNOWN,
        Some(hint) => hint,
        None if NON_TEXT.contains(&suffix) => UNKNOWN,
        None => ("text", "text/plain"),
    }
}

/// Kimi Code's `sniffMediaFromMagic`.
fn sniff_media(header: &[u8]) -> Option<(&'static str, &'static str)> {
    const IMAGES: [(&[u8], &str); 8] = [
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"BM", "image/bmp"),
        (b"II*\0", "image/tiff"),
        (b"MM\0*", "image/tiff"),
        (b"\0\0\x01\0", "image/x-icon"),
    ];
    const ASF: &[u8] = b"\x30\x26\xb2\x75\x8e\x66\xcf\x11\xa6\xd9\x00\xaa\x00\x62\xce\x6c";
    const IMAGE_BRANDS: [(&str, &str); 8] = [
        ("avif", "image/avif"),
        ("avis", "image/avif"),
        ("heic", "image/heic"),
        ("heif", "image/heif"),
        ("heix", "image/heif"),
        ("hevc", "image/heic"),
        ("mif1", "image/heif"),
        ("msf1", "image/heif"),
    ];
    const VIDEO_BRANDS: [(&str, &str); 14] = [
        ("isom", "video/mp4"),
        ("iso2", "video/mp4"),
        ("iso5", "video/mp4"),
        ("mp41", "video/mp4"),
        ("mp42", "video/mp4"),
        ("avc1", "video/mp4"),
        ("mp4v", "video/mp4"),
        ("m4v", "video/x-m4v"),
        ("qt", "video/quicktime"),
        ("3gp4", "video/3gpp"),
        ("3gp5", "video/3gpp"),
        ("3gp6", "video/3gpp"),
        ("3gp7", "video/3gpp"),
        ("3g2", "video/3gpp2"),
    ];
    let has = |needle: &[u8]| {
        header
            .to_ascii_lowercase()
            .windows(needle.len())
            .any(|window| window == needle)
    };
    if let Some(&(_, mime)) = IMAGES.iter().find(|(magic, _)| header.starts_with(magic)) {
        return Some(("image", mime));
    }
    if header.starts_with(b"RIFF") && header.len() >= 12 {
        match &header[8..12] {
            b"WEBP" => return Some(("image", "image/webp")),
            b"AVI " => return Some(("video", "video/x-msvideo")),
            _ => {}
        }
    }
    if header.starts_with(b"FLV") {
        return Some(("video", "video/x-flv"));
    }
    if header.starts_with(ASF) {
        return Some(("video", "video/x-ms-wmv"));
    }
    if header.starts_with(b"\x1a\x45\xdf\xa3") {
        if has(b"webm") {
            return Some(("video", "video/webm"));
        }
        if has(b"matroska") {
            return Some(("video", "video/x-matroska"));
        }
    }
    if header.len() >= 12 && &header[4..8] == b"ftyp" {
        let space = |byte: &u8| matches!(byte, b'\t'..=b'\r' | b' ' | 0xA0);
        let brand: String = header[8..12]
            .iter()
            .map(|&byte| char::from(byte.to_ascii_lowercase()))
            .collect();
        let brand =
            brand.trim_end_matches(|c: char| c == '\0' || u8::try_from(c).is_ok_and(|b| space(&b)));
        let brand = brand.trim_start_matches(|c: char| u8::try_from(c).is_ok_and(|b| space(&b)));
        let find = |table: &[(&str, &'static str)]| {
            table
                .iter()
                .find(|(known, _)| *known == brand)
                .map(|&(_, mime)| mime)
        };
        if let Some(mime) = find(&IMAGE_BRANDS) {
            return Some(("image", mime));
        }
        if let Some(mime) = find(&VIDEO_BRANDS) {
            return Some(("video", mime));
        }
    }
    None
}

#[derive(Deserialize)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

/// Kimi Code's `EditTool.execution` and `FileEditService.edit`.
pub async fn edit(builtins: &Builtins, args: &Value) -> Outcome {
    let args: EditInput = match input("Edit", args) {
        Ok(args) => args,
        Err(error) => return error,
    };
    let path = match builtins.resolve("Edit", &args.path, Access::Write) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let (old, new, shown) = (&args.old_string, &args.new_string, &args.path);
    if old == new {
        return Outcome::error(
            "No changes to make: old_string and new_string are exactly the same.",
        );
    }
    let raw = match io::read(builtins.host(), &path)
        .await
        .map(String::from_utf8)
    {
        Ok(Ok(raw)) => raw,
        Ok(Err(_)) => return Outcome::error("read failed: unrecognized filesystem error"),
        Err(error) if error.kind() == std::io::ErrorKind::IsADirectory => {
            return Outcome::error(format!("{shown} is not a file."));
        }
        Err(error) => return Outcome::error(fs_error(&error, "read")),
    };
    let crlf = ending(std::iter::once(raw.as_str())) == Ending::Crlf;
    let text = if crlf { raw.replace("\r\n", "\n") } else { raw };
    let count = text.matches(old.as_str()).count();
    if count == 0 {
        return Outcome::error(format!(
            "old_string not found in {shown}, the file contents may be out of date. Please use the Read Tool to reload the content.\n"
        ));
    }
    let all = args.replace_all.unwrap_or(false);
    if count > 1 && !all {
        return Outcome::error(format!(
            "old_string is not unique in {shown} (found {count} occurrences). To replace every occurrence, set replace_all=true. To replace only one occurrence, include more surrounding context in old_string."
        ));
    }
    let text = text.replacen(old.as_str(), new, if all { count } else { 1 });
    let text = if crlf {
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        text
    };
    if let Err(error) = io::write(builtins.host(), &path, text.as_bytes(), false).await {
        return Outcome::error(fs_error(&error, "write"));
    }
    let word = if count == 1 {
        "occurrence"
    } else {
        "occurrences"
    };
    Outcome::ok(format!("Replaced {count} {word} in {shown}")).with_note(APPLIED)
}

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
    mode: Option<String>,
}

/// Kimi Code's `WriteTool.execution`.
pub async fn write(builtins: &Builtins, args: &Value) -> Outcome {
    let args: WriteInput = match input("Write", args) {
        Ok(args) => args,
        Err(error) => return error,
    };
    let path = match builtins.resolve("Write", &args.path, Access::Write) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let append = args.mode.as_deref() == Some("append");
    let host = builtins.host();
    let content = args.content.as_bytes();
    let outcome = match io::write(host, &path, content, append).await {
        Err(error) if missing(&error) => {
            if let Some(parent) = path.parent()
                && let Some(message) = ensure_parent(host, parent).await
            {
                return Outcome::error(message);
            }
            io::write(host, &path, content, append).await
        }
        other => other,
    };
    match outcome {
        Ok(()) => Outcome::ok(format!(
            "{} {} bytes to {}",
            if append { "Appended" } else { "Wrote" },
            args.content.len(),
            args.path
        ))
        .with_note(APPLIED),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Outcome::error(format!(
            "Failed to write {}: parent directory does not exist.",
            args.path
        )),
        Err(error) => Outcome::error(fs_error(&error, if append { "append" } else { "write" })),
    }
}

fn missing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

async fn ensure_parent(host: Option<&io::Host>, parent: &Path) -> Option<String> {
    match io::kind(host, parent).await {
        Ok(Kind::File(_) | Kind::Special) => Some(format!(
            "Parent path is not a directory: {}.",
            parent.display()
        )),
        Ok(Kind::Dir) | Err(_) => None,
        Ok(Kind::Missing) => io::create_dir_all(host, parent)
            .await
            .err()
            .map(|error| fs_error(&error, "mkdir")),
    }
}

/// Kimi Code's host filesystem error message: `<op> failed: <reason>`, such as
/// `write failed: path is a directory`.
pub fn fs_error(error: &std::io::Error, op: &str) -> String {
    use std::io::ErrorKind;
    let reason = match error.kind() {
        ErrorKind::NotFound => "path does not exist",
        ErrorKind::IsADirectory => "path is a directory",
        ErrorKind::NotADirectory => "a path component is not a directory",
        ErrorKind::AlreadyExists => "path already exists",
        ErrorKind::PermissionDenied => "permission denied",
        ErrorKind::DirectoryNotEmpty => "directory is not empty",
        _ => "unrecognized filesystem error",
    };
    format!("{op} failed: {reason}")
}
