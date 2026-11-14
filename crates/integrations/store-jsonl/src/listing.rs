use std::{
    collections::HashMap,
    fs::{self, File, FileTimes, Metadata, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocode_platform::TemporaryFile;

use super::{JOURNAL_READ_BUFFER_BYTES, identity, sessions::created};

const FILE: &str = ".listing";
const STAMP: &str = "@";
const REMOVED: &str = "-";

pub(super) struct Listing {
    directory: PathBuf,
    path: PathBuf,
    known: Option<Known>,
    stale: bool,
}

#[derive(Clone)]
struct Known {
    stamp: Stamp,
    file: String,
    length: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    directory: String,
    modified: u128,
}

pub(super) struct Entry<'a> {
    pub(super) name: &'a str,
    pub(super) created: SystemTime,
    pub(super) source: &'a str,
    pub(super) length: u64,
}

pub(super) struct Scan {
    directory: File,
    temporary: TemporaryFile,
    stamp: Stamp,
}

fn nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_nanos())
}

fn time(nanos: u128) -> Option<SystemTime> {
    let second = Duration::from_secs(1).as_nanos();
    let seconds = u64::try_from(nanos / second).ok()?;
    let rest = u32::try_from(nanos % second).ok()?;
    UNIX_EPOCH.checked_add(Duration::new(seconds, rest))
}

fn stamp(metadata: &Metadata) -> Option<Stamp> {
    Some(Stamp {
        directory: identity(metadata).ok()?,
        modified: nanos(metadata.modified().ok()?)?,
    })
}

fn unique(current: &Stamp) -> Option<SystemTime> {
    let now = nanocode_platform::system_time();
    if nanos(now)? == current.modified {
        return now.checked_add(Duration::from_nanos(1));
    }
    Some(now)
}

fn mark(directory: &File, at: SystemTime) -> Option<Stamp> {
    directory
        .set_times(FileTimes::new().set_modified(at))
        .ok()?;
    let marked = stamp(&directory.metadata().ok()?)?;
    (marked.modified == nanos(at)?).then_some(marked)
}

fn stamp_line(stamp: &Stamp) -> String {
    format!("{STAMP}\t{}\t{}\n", stamp.directory, stamp.modified)
}

fn entry_line(name: &str, created: SystemTime, source: &str, length: u64) -> Option<String> {
    Some(format!("{}\t{length}\t{source}\t{name}\n", nanos(created)?))
}

fn metadata_line(name: &str, metadata: &Metadata) -> Option<String> {
    entry_line(
        name,
        created(metadata),
        &identity(metadata).ok()?,
        metadata.len(),
    )
}

fn parse(text: &str) -> Option<(Vec<Entry<'_>>, Stamp)> {
    let mut entries = Vec::<Option<Entry<'_>>>::new();
    let mut positions = None::<HashMap<&str, usize>>;
    let mut stamp = None;
    let mut base = None;
    let mut read = 0;
    for line in text.split_inclusive('\n') {
        read += line.len();
        let mut fields = line.strip_suffix('\n')?.split('\t');
        let first = fields.next()?;
        if first == STAMP {
            stamp = Some(Stamp {
                directory: fields.next()?.to_owned(),
                modified: fields.next()?.parse().ok()?,
            });
            base.get_or_insert(read);
        } else {
            let positions = match (base, &mut positions) {
                (None, _) => None,
                (Some(_), Some(positions)) => Some(positions),
                (Some(_), positions @ None) => Some(
                    positions.insert(
                        entries
                            .iter()
                            .enumerate()
                            .filter_map(|(at, entry)| Some((entry.as_ref()?.name, at)))
                            .collect(),
                    ),
                ),
            };
            let (name, entry) = if first == REMOVED {
                (fields.next()?, None)
            } else {
                let created = time(first.parse().ok()?)?;
                let length = fields.next()?.parse().ok()?;
                let source = fields.next()?;
                let name = fields.next()?;
                (
                    name,
                    Some(Entry {
                        name,
                        created,
                        source,
                        length,
                    }),
                )
            };
            stamp = None;
            match positions {
                None => entries.push(entry),
                Some(positions) => match positions.get(name) {
                    Some(&at) => entries[at] = entry,
                    None => {
                        positions.insert(name, entries.len());
                        entries.push(entry);
                    }
                },
            }
        }
        if fields.next().is_some() {
            return None;
        }
    }
    let base = base?;
    if text.len() - base > base {
        return None;
    }
    Some((entries.into_iter().flatten().collect(), stamp?))
}

fn last_stamp(file: &mut File, length: u64) -> Option<Stamp> {
    let mut wanted = JOURNAL_READ_BUFFER_BYTES as u64;
    loop {
        let start = length.saturating_sub(wanted);
        let mut bytes = vec![0; usize::try_from(length - start).ok()?];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        let body = bytes.strip_suffix(b"\n")?;
        let line = match body.iter().rposition(|byte| *byte == b'\n') {
            Some(at) => &body[at + 1..],
            None if start == 0 => body,
            None => {
                wanted = wanted.saturating_mul(2);
                continue;
            }
        };
        let mut fields = std::str::from_utf8(line).ok()?.split('\t');
        if fields.next()? != STAMP {
            return None;
        }
        let stamp = Stamp {
            directory: fields.next()?.to_owned(),
            modified: fields.next()?.parse().ok()?,
        };
        return fields.next().is_none().then_some(stamp);
    }
}

impl Listing {
    pub(super) fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_owned(),
            path: directory.join(FILE),
            known: None,
            stale: false,
        }
    }

    pub(super) fn entries<'a>(&mut self, text: &'a mut String) -> Option<Vec<Entry<'a>>> {
        if self.stale {
            return None;
        }
        let mut file = File::open(&self.path).ok()?;
        let source = identity(&file.metadata().ok()?).ok()?;
        file.read_to_string(text).ok()?;
        let (entries, stamp) = parse(text)?;
        let current = self.current()?;
        if current != stamp {
            return None;
        }
        self.known = Some(Known {
            stamp,
            file: source,
            length: text.len() as u64,
        });
        Some(entries)
    }

    fn current(&self) -> Option<Stamp> {
        stamp(&fs::metadata(&self.directory).ok()?)
    }

    pub(super) fn settled(&mut self) -> bool {
        self.synced().is_some()
    }

    fn synced(&mut self) -> Option<Known> {
        if self.stale {
            return None;
        }
        let mut file = File::open(&self.path).ok()?;
        let metadata = file.metadata().ok()?;
        let source = identity(&metadata).ok()?;
        let length = metadata.len();
        let stamp = match &self.known {
            Some(known) if known.file == source && known.length == length => known.stamp.clone(),
            _ => last_stamp(&mut file, length)?,
        };
        let known = Known {
            stamp,
            file: source,
            length,
        };
        (self.current()? == known.stamp).then(|| {
            self.known = Some(known.clone());
            known
        })
    }

    pub(super) fn forget(&mut self) {
        self.known = None;
        self.stale = true;
    }

    pub(super) fn change<T, E>(
        &mut self,
        names: &[String],
        change: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let held = File::open(&self.directory)
            .ok()
            .filter(|directory| directory.try_lock().is_ok())
            .and_then(|directory| Some((self.synced()?, directory)));
        let changed = change();
        if let (Ok(_), Some((known, directory))) = (&changed, held)
            && self.record(&directory, known, names).is_none()
        {
            self.known = None;
        }
        changed
    }

    fn record(&mut self, directory: &File, known: Known, names: &[String]) -> Option<()> {
        let after = stamp(&directory.metadata().ok()?)?;
        if after == known.stamp {
            self.known = Some(known);
            return Some(());
        }
        let mut lines = String::new();
        for name in names {
            match fs::symlink_metadata(self.directory.join(name)) {
                Ok(metadata) if metadata.is_file() => {
                    lines.push_str(&metadata_line(name, &metadata)?);
                }
                Ok(_) => lines.push_str(&format!("{REMOVED}\t{name}\n")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    lines.push_str(&format!("{REMOVED}\t{name}\n"));
                }
                Err(_) => return None,
            }
        }
        let mut file = OpenOptions::new().append(true).open(&self.path).ok()?;
        let metadata = file.metadata().ok()?;
        if identity(&metadata).ok()? != known.file || metadata.len() != known.length {
            return None;
        }
        let stamp = mark(directory, unique(&after)?)?;
        lines.push_str(&stamp_line(&stamp));
        file.write_all(lines.as_bytes()).ok()?;
        self.known = Some(Known {
            stamp,
            file: known.file,
            length: known.length + lines.len() as u64,
        });
        Some(())
    }

    pub(super) fn scan(&mut self) -> Option<Scan> {
        let directory = File::open(&self.directory).ok()?;
        directory.try_lock().ok()?;
        let temporary = TemporaryFile::create_in(&self.directory, FILE, None).ok()?;
        let stamp = mark(&directory, unique(&stamp(&directory.metadata().ok()?)?)?)?;
        Some(Scan {
            directory,
            temporary,
            stamp,
        })
    }

    pub(super) fn snapshot<'a>(
        &mut self,
        scan: Scan,
        files: impl Iterator<Item = (&'a Path, SystemTime, &'a str, u64)>,
    ) -> Option<()> {
        let Scan {
            directory,
            mut temporary,
            stamp,
        } = scan;
        let mut text = String::new();
        for (path, created, source, length) in files {
            let name = path.file_name()?.to_str()?;
            text.push_str(&entry_line(name, created, source, length)?);
        }
        let at = unique(&stamp)?;
        let next = Stamp {
            directory: stamp.directory.clone(),
            modified: nanos(at)?,
        };
        text.push_str(&stamp_line(&next));
        temporary.write_all(text.as_bytes()).ok()?;
        if self::stamp(&directory.metadata().ok()?)? != stamp {
            return None;
        }
        let file = temporary.persist(&self.path).ok()?;
        let source = identity(&file.metadata().ok()?).ok()?;
        let marked = mark(&directory, at)?;
        self.stale = false;
        self.known = Some(Known {
            stamp: marked,
            file: source,
            length: text.len() as u64,
        });
        Some(())
    }
}
