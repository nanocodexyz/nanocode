use std::path::Path;

use nanocode_digest::blake3;
use nanocode_turnlog::{Format, Formats, Release};

pub const FORMATS: &str = "bin/nanocode-sim/formats.txt";
pub const CORPUS: &str = "bin/nanocode-sim/formats";
const CODEC: &str = "crates/core/turnlog/src/codec.rs";
const REGISTRY: &str = "crates/core/turnlog/src/formats.rs";
const UNRELEASED: &str = "unreleased";
const COMMAND: &str = "nanocode-sim --formats-corpus";
pub const DIGEST_NAME_CHARS: usize = 12;

const fn sources(format: Format) -> &'static [&'static str] {
    match format {
        Format::Tagged => &[CODEC],
        Format::Stamped => &[CODEC, REGISTRY],
    }
}

pub fn digest(root: &Path, format: Format) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    for source in sources(format) {
        bytes.extend_from_slice(source.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&std::fs::read(root.join(source))?);
        bytes.push(0);
    }
    Ok(blake3(&bytes).to_hex().as_str().to_owned())
}

pub fn lines(root: &Path) -> std::io::Result<String> {
    let mut text = String::new();
    for (format, release) in Formats::all() {
        let release = match release {
            Release::First(name) => name,
            Release::Unreleased => UNRELEASED,
        };
        text.push_str(&format!(
            "{} {release} {}\n",
            format.version(),
            digest(root, format)?
        ));
    }
    for (layout, release) in Formats::layouts() {
        let release = match release {
            Release::First(name) => name,
            Release::Unreleased => UNRELEASED,
        };
        text.push_str(&format!("{} {release}\n", layout.name()));
    }
    Ok(text)
}

pub fn check(root: &Path) -> std::io::Result<Vec<String>> {
    let recorded = std::fs::read_to_string(root.join(FORMATS))?;
    let wanted = lines(root)?;
    let mut problems = Vec::new();
    for (line, want) in recorded.lines().zip(wanted.lines()) {
        if line != want {
            problems.push(format!(
                "{FORMATS}: records '{line}' where the registry and the encoder sources give '{want}'; an encoder source changed, so run {COMMAND} to regenerate the corpus and the digests, then check the previous release still reads them with nanocode-sim --scenario formats"
            ));
        }
    }
    if recorded.lines().count() != wanted.lines().count() {
        problems.push(format!(
            "{FORMATS}: records {} formats where the registry holds {}; run {COMMAND}",
            recorded.lines().count(),
            wanted.lines().count()
        ));
    }
    for (format, _) in Formats::all() {
        let digest = digest(root, format)?;
        let sample = format!(
            "{}-{}-0.rows",
            format.version(),
            &digest[..DIGEST_NAME_CHARS]
        );
        if !root.join(CORPUS).join(&sample).exists() {
            problems.push(format!(
                "{CORPUS}: no {sample} for format {}'s encoder digest; run {COMMAND}",
                format.version()
            ));
        }
    }
    Ok(problems)
}
