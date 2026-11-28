mod previous;

use std::{collections::BTreeSet, ops::Range, path::Path, process::ExitCode, sync::Arc};

use nanocode_digest::blake3;
use nanocode_durability_core::StoreError;
use nanocode_sync::block_on;
use nanocode_turnlog::{
    Encoder, Format, FormatError, Formats, LogHost, MemoryLog, Point, Record, Release, SCAN_END,
    Stamp,
    sim::{self as log_sim, Mutation as Rule},
    tagged_row,
};
use serde_json::value::RawValue;

use crate::rng::{FNV_OFFSET, Rng, fnv};
use crate::tidy_formats::{CORPUS, DIGEST_NAME_CHARS};
use crate::{threadlegacy, tidy, tidy_formats};

const THREAD: &str = "formats";
const ROWS: &str = "rows";
const EXPECTED: &str = "expected";
const PART: char = '+';
const TAGGED: [char; 7] = ['a', 'b', 'P', 'r', 'p', 's', '+'];
const CORPUS_SEEDS: Range<u64> = 0..32;
const ABSENT: &str = "-";
const STAMP_SEPARATOR: char = ':';
const HEX_RADIX: u32 = 16;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const EVENTED_PPM: u64 = 500_000;
const ROW_LIMIT_PPM: u64 = 500_000;
const ROW_BYTES: Range<u64> = 700..3_000;
const EVENTS: Range<u64> = 4..24;
const EVENT_BYTES: Range<u64> = 64..4_096;
const SNAPSHOT_RATIO_NEVER: usize = u32::MAX as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatsMutation {
    None,
    WritesUnreleasedFormat,
}

impl FormatsMutation {
    const ALL: [Self; 2] = [Self::None, Self::WritesUnreleasedFormat];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WritesUnreleasedFormat => "writes_unreleased_format",
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn rule(self) -> Rule {
        match self {
            Self::None => Rule::None,
            Self::WritesUnreleasedFormat => Rule::WritesUnreleasedFormat,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FormatsStats {
    pub seeds: u64,
    pub corpus_rows: u64,
    pub written_rows: u64,
    pub stamped_rows: u64,
    pub refusals: u64,
}

impl FormatsStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.corpus_rows += other.corpus_rows;
        self.written_rows += other.written_rows;
        self.stamped_rows += other.stamped_rows;
        self.refusals += other.refusals;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    seq: u64,
    boundary: bool,
    snapshot: bool,
    stamp: Option<Stamp>,
    plain: String,
}

impl Seen {
    fn line(&self) -> String {
        let stamp = self.stamp.map_or_else(
            || ABSENT.to_owned(),
            |stamp| {
                format!(
                    "{:x}{STAMP_SEPARATOR}{:x}{STAMP_SEPARATOR}{:x}",
                    stamp.writer(),
                    stamp.epoch(),
                    stamp.op()
                )
            },
        );
        format!(
            "{} {} {} {stamp} {}",
            self.seq,
            u8::from(self.boundary),
            u8::from(self.snapshot),
            self.plain
        )
    }

    fn unstamped(&self) -> Self {
        Self {
            stamp: None,
            ..self.clone()
        }
    }
}

fn current(rows: &[(u64, String)]) -> Result<Vec<Seen>, StoreError> {
    Ok(nanocode_turnlog::rows(rows)?
        .into_iter()
        .map(|row| Seen {
            seq: row.seq,
            boundary: row.boundary,
            snapshot: row.snapshot,
            stamp: row.stamp,
            plain: blake3(row.plain.as_bytes()).to_hex().as_str().to_owned(),
        })
        .collect())
}

fn previous_release(rows: &[(u64, String)]) -> Result<Vec<Seen>, StoreError> {
    Ok(previous::rows(rows)?
        .into_iter()
        .map(|row| Seen {
            seq: row.seq,
            boundary: row.boundary,
            snapshot: row.snapshot,
            stamp: None,
            plain: blake3(row.plain.as_bytes()).to_hex().as_str().to_owned(),
        })
        .collect())
}

fn stamp(rng: &mut Rng, op: u64) -> Stamp {
    Stamp::new(rng.draw(), rng.draw(), op)
}

fn restamped(
    rows: &[(u64, String)],
    rng: &mut Rng,
    mut encode: impl FnMut(Stamp, String) -> String,
) -> (Vec<(u64, String)>, Vec<Stamp>) {
    let mut op = rng.draw() >> 1;
    let mut stamps = Vec::new();
    let rows = rows
        .iter()
        .map(|(seq, body)| {
            if body.starts_with(PART) {
                return (*seq, body.clone());
            }
            op += 1;
            let stamp = stamp(rng, op);
            stamps.push(stamp);
            (*seq, encode(stamp, body.clone()))
        })
        .collect();
    (rows, stamps)
}

async fn written(rng: &mut Rng) -> Result<Vec<(u64, String)>, String> {
    let host: Arc<dyn LogHost> = Arc::new(MemoryLog::new());
    if rng.chance(EVENTED_PPM) {
        evented(rng, Arc::clone(&host)).await?;
    } else {
        threadlegacy::write(rng, Arc::clone(&host), THREAD).await?;
    }
    host.scan(THREAD, 0, SCAN_END)
        .await
        .map_err(|error| format!("formats: the written log does not scan: {error}"))
}

async fn evented(rng: &mut Rng, host: Arc<dyn LogHost>) -> Result<(), String> {
    let failed = |error: StoreError| format!("formats: an event log write: {error}");
    let row_limit = rng.chance(ROW_LIMIT_PPM).then(|| {
        usize::try_from(rng.range(ROW_BYTES.start, ROW_BYTES.end)).expect("a byte count fits")
    });
    let log = nanocode_turnlog::sim::open(host, THREAD)
        .await
        .map_err(failed)?
        .snapshot_ratio(SNAPSHOT_RATIO_NEVER)
        .row_limit(row_limit);
    for _ in 0..rng.range(EVENTS.start, EVENTS.end) {
        let text = (0..rng.range(EVENT_BYTES.start, EVENT_BYTES.end))
            .map(|_| char::from(HEX_DIGITS[rng.pick(HEX_DIGITS.len())]))
            .collect::<String>();
        let event = RawValue::from_string(format!("\"{text}\""))
            .map_err(|error| format!("formats: an event is JSON: {error}"))?;
        log.write(move |writing| {
            Box::pin(async move {
                let seq = writing.next_event();
                writing.push(Record::Event {
                    at: seq,
                    event,
                    seq,
                    turn: None,
                })?;
                writing.commit(Point::Progress).await
            })
        })
        .await
        .map_err(failed)?;
    }
    Ok(())
}

fn agree(
    label: &str,
    rows: &[(u64, String)],
    want: &[Seen],
    stats: &mut FormatsStats,
) -> Result<(), String> {
    let ours = current(rows)
        .map_err(|error| format!("formats: {label}: the current registry refuses: {error}"))?;
    if ours != want {
        return Err(format!(
            "formats: {label}: the current registry decodes other rows than expected"
        ));
    }
    stats.corpus_rows += rows.len() as u64;
    Ok(())
}

fn read_by_previous(
    label: &str,
    format: Format,
    rows: &[(u64, String)],
    want: &[Seen],
    stats: &mut FormatsStats,
) -> Result<(), String> {
    let released = Formats::all()
        .into_iter()
        .any(|(registered, release)| registered == format && release != Release::Unreleased);
    match (previous_release(rows), released) {
        (Ok(theirs), true) => {
            let unstamped = want.iter().map(Seen::unstamped).collect::<Vec<_>>();
            if theirs == unstamped {
                Ok(())
            } else {
                Err(format!(
                    "formats: {label}: the previous release decodes other rows than the current one"
                ))
            }
        }
        (Err(error), true) => Err(format!(
            "formats: {label}: the previous release refuses format {}: {error}",
            format.version()
        )),
        (Ok(_), false) => Err(format!(
            "formats: {label}: the previous release decodes unreleased format {}",
            format.version()
        )),
        (Err(_), false) => {
            stats.refusals += 1;
            Ok(())
        }
    }
}

fn corpus(root: &Path, stats: &mut FormatsStats) -> Result<(), String> {
    let samples = samples(root).map_err(|error| format!("formats: corpus: {error}"))?;
    for (format, _) in Formats::all() {
        if !samples.iter().any(|sample| sample.format == format) {
            return Err(format!(
                "formats: corpus: no sample of format {}",
                format.version()
            ));
        }
    }
    for sample in &samples {
        agree(&sample.name, &sample.rows, &sample.want, stats)?;
        read_by_previous(
            &sample.name,
            sample.format,
            &sample.rows,
            &sample.want,
            stats,
        )?;
    }
    Ok(())
}

struct Sample {
    name: String,
    format: Format,
    rows: Vec<(u64, String)>,
    want: Vec<Seen>,
}

fn samples(root: &Path) -> std::io::Result<Vec<Sample>> {
    let directory = root.join(CORPUS);
    let mut names = std::fs::read_dir(&directory)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    let mut samples = Vec::new();
    for name in names {
        let Some(stem) = name.strip_suffix(&format!(".{ROWS}")) else {
            continue;
        };
        let version = stem
            .split('-')
            .next()
            .and_then(|version| version.parse().ok());
        let format = Formats::all()
            .into_iter()
            .map(|(format, _)| format)
            .find(|format| Some(format.version()) == version)
            .ok_or_else(|| invalid(format!("{name} names no registered format")))?;
        let rows = parse_rows(&std::fs::read_to_string(directory.join(&name))?)
            .ok_or_else(|| invalid(format!("{name} holds a line that is not a row")))?;
        let expected = std::fs::read_to_string(directory.join(format!("{stem}.{EXPECTED}")))?;
        let want = parse_seen(&expected)
            .ok_or_else(|| invalid(format!("{stem}.{EXPECTED} holds a line that is not a row")))?;
        samples.push(Sample {
            name: stem.to_owned(),
            format,
            rows,
            want,
        });
    }
    Ok(samples)
}

fn invalid(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn parse_rows(text: &str) -> Option<Vec<(u64, String)>> {
    text.lines()
        .map(|line| {
            let (seq, body) = line.split_once(' ')?;
            Some((seq.parse().ok()?, body.to_owned()))
        })
        .collect()
}

fn parse_seen(text: &str) -> Option<Vec<Seen>> {
    text.lines()
        .map(|line| {
            let mut fields = line.split(' ');
            let seq = fields.next()?.parse().ok()?;
            let boundary = fields.next()? == "1";
            let snapshot = fields.next()? == "1";
            let stamp = match fields.next()? {
                ABSENT => None,
                stamp => {
                    let mut parts = stamp.split(STAMP_SEPARATOR);
                    let mut part = || u64::from_str_radix(parts.next()?, HEX_RADIX).ok();
                    Some(Stamp::new(part()?, part()?, part()?))
                }
            };
            let plain = fields.next()?.to_owned();
            fields.next().is_none().then_some(Seen {
                seq,
                boundary,
                snapshot,
                stamp,
                plain,
            })
        })
        .collect()
}

fn refusals() -> Result<(), String> {
    let writable = Formats::writable();
    for (format, _) in Formats::all() {
        match (Encoder::new(format), format > writable) {
            (Ok(encoder), false) if encoder.format() == format => {}
            (
                Err(FormatError::Unreleased {
                    asked,
                    writable: at,
                }),
                true,
            ) if asked == format && at == writable => {}
            (answer, newer) => {
                return Err(format!(
                    "formats: an encoder for format {} (newer than writable: {newer}) answers {:?}",
                    format.version(),
                    answer.map(|encoder| encoder.format())
                ));
            }
        }
    }
    Ok(())
}

async fn seeded(rng: &mut Rng, stats: &mut FormatsStats) -> Result<u64, String> {
    let rows = written(rng).await?;
    let plain = current(&rows)
        .map_err(|error| format!("formats: the current registry refuses written rows: {error}"))?;
    let writable = Formats::writable();
    previous_release(&rows).map_err(|error| {
        format!(
            "formats: the previous release refuses a row the turn log's writer wrote at format {}: {error}",
            writable.version()
        )
    })?;
    let rows = rows
        .iter()
        .map(|(seq, body)| tagged_row(body).map(|tagged| (*seq, tagged.to_owned())))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("formats: a written row opens to no format 1 row: {error}"))?;
    let encoder = Encoder::new(writable).map_err(|error| format!("formats: {error}"))?;
    let (at_writable, _) = restamped(&rows, rng, |stamp, body| encoder.row(stamp, body));
    stats.written_rows += at_writable.len() as u64;
    let theirs = previous_release(&at_writable).map_err(|error| {
        format!(
            "formats: the previous release refuses a row written at format {}: {error}",
            writable.version()
        )
    })?;
    if theirs != plain.iter().map(Seen::unstamped).collect::<Vec<_>>() {
        return Err(format!(
            "formats: the previous release decodes rows written at format {} differently",
            writable.version()
        ));
    }
    let (stamped, issued) = restamped(&rows, rng, |stamp, body| log_sim::stamped(stamp, &body));
    let ours = current(&stamped)
        .map_err(|error| format!("formats: the current registry refuses stamped rows: {error}"))?;
    if ours.iter().map(|seen| seen.stamp).collect::<Vec<_>>()
        != issued.into_iter().map(Some).collect::<Vec<_>>()
    {
        return Err(
            "formats: a stamped row decodes to another stamp than it was written with".to_owned(),
        );
    }
    if ours.iter().map(Seen::unstamped).collect::<Vec<_>>() != plain {
        return Err(
            "formats: a stamped row decodes to other records than its format 1 body".to_owned(),
        );
    }
    stats.stamped_rows += stamped.len() as u64;
    Ok(stamped.iter().fold(FNV_OFFSET, |hash, (seq, body)| {
        fnv(fnv(hash, &seq.to_le_bytes()), body.as_bytes())
    }))
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: FormatsStats,
}

fn run_seed(seed: u64, mutation: FormatsMutation) -> Outcome {
    let time = crate::executor::Clock::new();
    crate::host::entered(seed, &time, || outcome(seed, mutation))
}

fn outcome(seed: u64, mutation: FormatsMutation) -> Outcome {
    let mut rng = Rng::derive(seed, "formats");
    let mut stats = FormatsStats {
        seeds: 1,
        ..FormatsStats::default()
    };
    log_sim::mutate(mutation.rule());
    let checked = refusals()
        .and_then(|()| corpus(&tidy::workspace_root(), &mut stats))
        .and_then(|()| block_on(seeded(&mut rng, &mut stats)));
    log_sim::mutate(Rule::None);
    let (digest, wrong) = match checked {
        Ok(digest) => (fnv(digest, &seed.to_le_bytes()), None),
        Err(wrong) => (0, Some(wrong)),
    };
    Outcome {
        digest,
        wrong,
        stats,
    }
}

pub fn judge(
    seed: u64,
    mutation: FormatsMutation,
    twice: bool,
    verbose: bool,
    totals: &mut FormatsStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} stats={:?}",
            first.digest, first.stats
        );
    }
    if first.wrong.is_some() || !twice {
        return first.wrong;
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "nondeterminism: digest {:016x} then {:016x}",
            first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: FormatsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario formats --seed {seed}");
    if mutation != FormatsMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: FormatsMutation,
    totals: &FormatsStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=formats mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        FormatsMutation::None if failed == 0 => ExitCode::SUCCESS,
        FormatsMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}

pub fn write_corpus() -> ExitCode {
    let root = tidy::workspace_root();
    match block_on(generate(&root)) {
        Ok(written) => {
            println!(
                "FORMATS wrote {written} corpus files and {}",
                tidy_formats::FORMATS
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("FORMATS {error}");
            ExitCode::FAILURE
        }
    }
}

async fn logs() -> Result<Vec<(u64, Vec<(u64, String)>)>, String> {
    let mut logs = Vec::new();
    let mut covered = BTreeSet::new();
    for seed in CORPUS_SEEDS {
        if TAGGED.iter().all(|tag| covered.contains(tag)) {
            break;
        }
        let rows = written(&mut Rng::derive(seed, "formats corpus")).await?;
        let tags = rows
            .iter()
            .filter_map(|(_, body)| body.chars().next())
            .collect::<BTreeSet<_>>();
        if tags.iter().any(|tag| !covered.contains(tag)) {
            covered.extend(tags);
            logs.push((seed, rows));
        }
    }
    let missing = TAGGED
        .iter()
        .filter(|tag| !covered.contains(tag))
        .collect::<String>();
    if !missing.is_empty() {
        return Err(format!(
            "no corpus seed in {CORPUS_SEEDS:?} writes a row tagged with any of {missing:?}"
        ));
    }
    Ok(logs)
}

async fn generate(root: &Path) -> Result<usize, String> {
    let logs = logs().await?;
    let directory = root.join(CORPUS);
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let mut written = 0;
    for (format, _) in Formats::all() {
        let digest = tidy_formats::digest(root, format).map_err(|error| error.to_string())?;
        for (index, (seed, rows)) in logs.iter().enumerate() {
            let rows = match format {
                Format::Tagged => rows.clone(),
                Format::Stamped => {
                    let mut rng = Rng::derive(*seed, "formats corpus stamps");
                    restamped(rows, &mut rng, |stamp, body| log_sim::stamped(stamp, &body)).0
                }
            };
            let stem = format!(
                "{}-{}-{index}",
                format.version(),
                &digest[..DIGEST_NAME_CHARS]
            );
            let path = directory.join(format!("{stem}.{ROWS}"));
            if path.exists() {
                continue;
            }
            let want = current(&rows).map_err(|error| format!("{stem}: {error}"))?;
            let rows_text = rows
                .iter()
                .map(|(seq, body)| format!("{seq} {body}\n"))
                .collect::<String>();
            let want_text = want
                .iter()
                .map(|seen| seen.line() + "\n")
                .collect::<String>();
            std::fs::write(&path, rows_text).map_err(|error| error.to_string())?;
            std::fs::write(directory.join(format!("{stem}.{EXPECTED}")), want_text)
                .map_err(|error| error.to_string())?;
            written += 2;
        }
    }
    std::fs::write(
        root.join(tidy_formats::FORMATS),
        tidy_formats::lines(root).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(written)
}
