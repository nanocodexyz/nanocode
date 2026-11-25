use crate::sim::{Mutation, mutated};
use nanocode_digest::Sha256;
use nanocode_sync::OrdMap;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
};

pub const DIGEST_BYTES: usize = Sha256::OUTPUT_BYTES;
pub const PARENT_MODE: u32 = 0o755;
pub const MODE_BITS: u32 = 0o7777;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; DIGEST_BYTES]);

impl Digest {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes))
    }

    #[must_use]
    pub fn hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(DIGEST_BYTES * 2);
        for byte in self.0 {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 15)]));
        }
        out
    }

    #[must_use]
    pub fn parse(hex: &str) -> Option<Self> {
        let hex = hex.as_bytes();
        if hex.len() != DIGEST_BYTES * 2 {
            return None;
        }
        let nibble = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let mut out = [0; DIGEST_BYTES];
        for (slot, pair) in out.iter_mut().zip(hex.as_chunks::<2>().0) {
            *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Digest({})", self.hex())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub file_bytes: u64,
    pub workspace_bytes: u64,
    pub paths: usize,
    pub ops: usize,
    pub path_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A file. Names that share a `link` are hard links of one file: they hold the
    /// same content, mode and time, and a change through one shows through all.
    File {
        digest: Digest,
        size: u64,
        mode: u32,
        mtime_ns: i64,
        link: Option<u64>,
    },
    Dir {
        mode: u32,
        mtime_ns: i64,
    },
    Symlink {
        target: String,
        mtime_ns: i64,
    },
}

impl Entry {
    #[must_use]
    pub const fn size(&self) -> u64 {
        match self {
            Self::File { size, .. } => *size,
            Self::Dir { .. } | Self::Symlink { .. } => 0,
        }
    }

    #[must_use]
    pub const fn digest(&self) -> Option<Digest> {
        match self {
            Self::File { digest, .. } => Some(*digest),
            Self::Dir { .. } | Self::Symlink { .. } => None,
        }
    }

    #[must_use]
    pub const fn mtime_ns(&self) -> i64 {
        match self {
            Self::File { mtime_ns, .. }
            | Self::Dir { mtime_ns, .. }
            | Self::Symlink { mtime_ns, .. } => *mtime_ns,
        }
    }

    /// The hard-link group a file belongs to, if it was ever linked.
    #[must_use]
    pub const fn link(&self) -> Option<u64> {
        match self {
            Self::File { link, .. } => *link,
            Self::Dir { .. } | Self::Symlink { .. } => None,
        }
    }

    #[must_use]
    pub const fn is_dir(&self) -> bool {
        matches!(self, Self::Dir { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Put {
        path: String,
        digest: Digest,
        size: u64,
        mode: u32,
        mtime_ns: i64,
    },
    Mkdir {
        path: String,
        mode: u32,
        mtime_ns: i64,
    },
    Symlink {
        path: String,
        target: String,
        mtime_ns: i64,
    },
    Remove {
        path: String,
        recursive: bool,
    },
    Move {
        from: String,
        to: String,
    },
    /// Makes `to` another name of the file at `from` (a hard link).
    Link {
        from: String,
        to: String,
    },
    SetMode {
        path: String,
        mode: u32,
    },
    SetMtime {
        path: String,
        mtime_ns: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    BadPath { path: String, why: &'static str },
    BadMode { path: String, mode: u32 },
    FileTooLarge { path: String, size: u64, max: u64 },
    WorkspaceFull { bytes: u64, max: u64 },
    TooManyPaths { count: usize, max: usize },
    TooManyOps { count: usize, max: usize },
    Stale { base: u64, head: u64 },
    NotFound { path: String },
    NotADirectory { path: String },
    IsADirectory { path: String },
    DirectoryNotEmpty { path: String },
    Exists { path: String },
}

impl Refusal {
    #[must_use]
    pub const fn kind(&self) -> io::ErrorKind {
        match self {
            Self::BadPath { .. } | Self::BadMode { .. } => io::ErrorKind::InvalidInput,
            Self::FileTooLarge { .. } => io::ErrorKind::FileTooLarge,
            Self::WorkspaceFull { .. } | Self::TooManyPaths { .. } => io::ErrorKind::StorageFull,
            Self::TooManyOps { .. } => io::ErrorKind::ArgumentListTooLong,
            Self::Stale { .. } => io::ErrorKind::ResourceBusy,
            Self::NotFound { .. } => io::ErrorKind::NotFound,
            Self::NotADirectory { .. } => io::ErrorKind::NotADirectory,
            Self::IsADirectory { .. } => io::ErrorKind::IsADirectory,
            Self::DirectoryNotEmpty { .. } => io::ErrorKind::DirectoryNotEmpty,
            Self::Exists { .. } => io::ErrorKind::AlreadyExists,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadPath { path, why } => write!(formatter, "bad path {path:?}: {why}"),
            Self::BadMode { path, mode } => write!(formatter, "{path}: bad mode {mode:o}"),
            Self::FileTooLarge { path, size, max } => write!(
                formatter,
                "{path} is {size} bytes, over the {max}-byte file limit"
            ),
            Self::WorkspaceFull { bytes, max } => write!(
                formatter,
                "the workspace would hold {bytes} bytes, over its {max}-byte limit"
            ),
            Self::TooManyPaths { count, max } => write!(
                formatter,
                "the workspace would hold {count} paths, over its limit of {max}"
            ),
            Self::TooManyOps { count, max } => write!(
                formatter,
                "{count} changes in one commit, over the limit of {max}"
            ),
            Self::Stale { base, head } => write!(
                formatter,
                "the workspace changed: read at version {base}, now at {head}"
            ),
            Self::NotFound { path } => write!(formatter, "{path}: no such file or directory"),
            Self::NotADirectory { path } => write!(formatter, "{path}: not a directory"),
            Self::IsADirectory { path } => write!(formatter, "{path}: is a directory"),
            Self::DirectoryNotEmpty { path } => write!(formatter, "{path}: directory not empty"),
            Self::Exists { path } => write!(formatter, "{path}: file exists"),
        }
    }
}

impl std::error::Error for Refusal {}

impl From<Refusal> for io::Error {
    fn from(refusal: Refusal) -> Self {
        Self::new(refusal.kind(), refusal)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Corrupt {
    pub path: String,
    pub why: &'static str,
}

impl fmt::Display for Corrupt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "stored workspace entry {:?} is invalid: {}",
            self.path, self.why
        )
    }
}

impl std::error::Error for Corrupt {}

pub fn normalize(path: &str, caps: &Caps) -> Result<String, Refusal> {
    let bad = |why| {
        Err(Refusal::BadPath {
            path: path.to_owned(),
            why,
        })
    };
    if path.len() > caps.path_bytes {
        return bad("too long");
    }
    if path.starts_with('/') {
        return bad("absolute");
    }
    if path.contains('\0') {
        return bad("contains NUL");
    }
    let mut out = String::with_capacity(path.len());
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return bad("contains '..'"),
            part => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(part);
            }
        }
    }
    if out.is_empty() {
        return bad("empty");
    }
    Ok(out)
}

fn normalized(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

#[must_use]
pub fn parent(path: &str) -> Option<&str> {
    path.rfind('/').map(|at| &path[..at])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub base: u64,
    pub version: u64,
    pub bytes: u64,
    pub paths: usize,
    pub entries: BTreeMap<String, Option<Entry>>,
    pub added: Vec<Digest>,
    pub dropped: Vec<Digest>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    version: u64,
    entries: OrdMap<String, Entry>,
    bytes: u64,
    refs: OrdMap<Digest, u64>,
    /// Each hard-link group's names.
    links: OrdMap<u64, BTreeSet<String>>,
    /// How many entries are symbolic links.
    symlinks: u64,
}

struct Draft<'a> {
    base: &'a OrdMap<String, Entry>,
    links: &'a OrdMap<u64, BTreeSet<String>>,
    next_link: u64,
    changes: BTreeMap<String, Option<Entry>>,
    caps: &'a Caps,
}

impl Draft<'_> {
    fn get(&self, path: &str) -> Option<&Entry> {
        match self.changes.get(path) {
            Some(entry) => entry.as_ref(),
            None => self.base.get(path),
        }
    }

    fn insert(&mut self, path: String, entry: Entry) {
        self.changes.insert(path, Some(entry));
    }

    fn remove(&mut self, path: &str) -> Option<Entry> {
        let old = self.get(path).cloned();
        self.changes.insert(path.to_owned(), None);
        old
    }

    fn below<'b>(&'b self, prefix: &'b str) -> impl Iterator<Item = &'b String> + 'b {
        let held = self
            .base
            .range::<_, str>((
                std::ops::Bound::Included(prefix),
                std::ops::Bound::Unbounded,
            ))
            .map(|(key, _)| key)
            .take_while(move |key| key.starts_with(prefix))
            .filter(|key| !matches!(self.changes.get(key.as_str()), Some(None)));
        let added = self
            .changes
            .range::<str, _>((
                std::ops::Bound::Included(prefix),
                std::ops::Bound::Unbounded,
            ))
            .take_while(move |(key, _)| key.starts_with(prefix))
            .filter(|(key, entry)| entry.is_some() && !self.base.contains_key(key.as_str()))
            .map(|(key, _)| key);
        held.chain(added)
    }

    fn subtree(&self, path: &str) -> Vec<String> {
        let prefix = format!("{path}/");
        let mut below: Vec<String> = self.below(&prefix).cloned().collect();
        below.sort_unstable();
        let mut paths = vec![path.to_owned()];
        paths.extend(below);
        paths
    }

    fn has_children(&self, path: &str) -> bool {
        let prefix = format!("{path}/");
        self.below(&prefix).next().is_some()
    }

    fn make_parents(&mut self, path: &str, mtime_ns: i64) -> Result<(), Refusal> {
        let mut at = 0;
        while let Some(slash) = path[at..].find('/') {
            let dir = &path[..at + slash];
            match self.get(dir) {
                Some(Entry::Dir { .. }) => {}
                Some(_) => {
                    return Err(Refusal::NotADirectory {
                        path: dir.to_owned(),
                    });
                }
                None => self.insert(
                    dir.to_owned(),
                    Entry::Dir {
                        mode: PARENT_MODE,
                        mtime_ns,
                    },
                ),
            }
            at += slash + 1;
        }
        Ok(())
    }

    /// The names of hard-link group `link` as the draft holds them.
    fn members(&self, link: u64) -> Vec<String> {
        let held = self.links.get(&link).into_iter().flatten().cloned();
        let added = self
            .changes
            .iter()
            .filter(|(_, entry)| entry.as_ref().and_then(Entry::link) == Some(link))
            .map(|(path, _)| path.clone());
        let names: BTreeSet<String> = held.chain(added).collect();
        names
            .into_iter()
            .filter(|path| self.get(path).and_then(Entry::link) == Some(link))
            .collect()
    }

    /// Puts `entry` at `path` and, when it is a linked file, at every other name of
    /// its file, as one file seen through each.
    fn put_file(&mut self, path: String, entry: Entry) {
        match entry.link() {
            Some(link) => {
                for name in self.members(link) {
                    if name != path {
                        self.insert(name, entry.clone());
                    }
                }
                self.insert(path, entry);
            }
            None => self.insert(path, entry),
        }
    }

    fn not_a_dir(&self, path: &str) -> Result<(), Refusal> {
        match self.get(path) {
            Some(Entry::Dir { .. }) => Err(Refusal::IsADirectory {
                path: path.to_owned(),
            }),
            _ => Ok(()),
        }
    }

    fn path(&self, path: &str) -> Result<String, Refusal> {
        normalize(path, self.caps)
    }

    fn mode(path: &str, mode: u32) -> Result<(), Refusal> {
        if mode & !MODE_BITS == 0 {
            Ok(())
        } else {
            Err(Refusal::BadMode {
                path: path.to_owned(),
                mode,
            })
        }
    }

    fn apply(&mut self, op: &Op) -> Result<(), Refusal> {
        match op {
            Op::Put {
                path,
                digest,
                size,
                mode,
                mtime_ns,
            } => self.put(path, *digest, *size, *mode, *mtime_ns),
            Op::Mkdir {
                path,
                mode,
                mtime_ns,
            } => self.mkdir(path, *mode, *mtime_ns),
            Op::Symlink {
                path,
                target,
                mtime_ns,
            } => self.symlink(path, target, *mtime_ns),
            Op::Remove { path, recursive } => self.remove_path(path, *recursive),
            Op::Move { from, to } => self.move_path(from, to),
            Op::Link { from, to } => self.link(from, to),
            Op::SetMode { path, mode } => self.set_mode(path, *mode),
            Op::SetMtime { path, mtime_ns } => self.set_mtime(path, *mtime_ns),
        }
    }

    fn put(
        &mut self,
        path: &str,
        digest: Digest,
        size: u64,
        mode: u32,
        mtime_ns: i64,
    ) -> Result<(), Refusal> {
        let path = self.path(path)?;
        Self::mode(&path, mode)?;
        if size > self.caps.file_bytes && !mutated(Mutation::FileCapIgnored) {
            return Err(Refusal::FileTooLarge {
                path,
                size,
                max: self.caps.file_bytes,
            });
        }
        self.make_parents(&path, mtime_ns)?;
        self.not_a_dir(&path)?;
        let link = self.get(&path).and_then(Entry::link);
        self.put_file(
            path,
            Entry::File {
                digest,
                size,
                mode,
                mtime_ns,
                link,
            },
        );
        Ok(())
    }

    fn link(&mut self, from: &str, to: &str) -> Result<(), Refusal> {
        let from = self.path(from)?;
        let to = self.path(to)?;
        let entry = match self.get(&from).cloned() {
            None => return Err(Refusal::NotFound { path: from }),
            Some(Entry::Dir { .. }) => return Err(Refusal::IsADirectory { path: from }),
            Some(entry) => entry,
        };
        if self.get(&to).is_some() {
            return Err(Refusal::Exists { path: to });
        }
        self.make_parents(&to, entry.mtime_ns())?;
        let entry = match entry {
            Entry::File {
                digest,
                size,
                mode,
                mtime_ns,
                link,
            } => {
                let link = link.unwrap_or_else(|| {
                    self.next_link += 1;
                    self.next_link
                });
                let file = Entry::File {
                    digest,
                    size,
                    mode,
                    mtime_ns,
                    link: Some(link),
                };
                self.insert(from, file.clone());
                file
            }
            symlink => symlink,
        };
        self.insert(to, entry);
        Ok(())
    }

    fn mkdir(&mut self, path: &str, mode: u32, mtime_ns: i64) -> Result<(), Refusal> {
        let path = self.path(path)?;
        Self::mode(&path, mode)?;
        self.make_parents(&path, mtime_ns)?;
        match self.get(&path) {
            Some(Entry::Dir { .. }) => Ok(()),
            None => {
                self.insert(path, Entry::Dir { mode, mtime_ns });
                Ok(())
            }
            Some(_) => Err(Refusal::NotADirectory { path }),
        }
    }

    fn symlink(&mut self, path: &str, target: &str, mtime_ns: i64) -> Result<(), Refusal> {
        let path = self.path(path)?;
        if target.is_empty() || target.len() > self.caps.path_bytes || target.contains('\0') {
            return Err(Refusal::BadPath {
                path: target.to_owned(),
                why: "not a symlink target",
            });
        }
        self.make_parents(&path, mtime_ns)?;
        self.not_a_dir(&path)?;
        self.insert(
            path,
            Entry::Symlink {
                target: target.to_owned(),
                mtime_ns,
            },
        );
        Ok(())
    }

    fn remove_path(&mut self, path: &str, recursive: bool) -> Result<(), Refusal> {
        let path = self.path(path)?;
        if !self.get(&path).is_some() {
            return Err(Refusal::NotFound { path });
        }
        if !recursive && self.has_children(&path) {
            return Err(Refusal::DirectoryNotEmpty { path });
        }
        for key in self.subtree(&path) {
            self.remove(&key);
        }
        Ok(())
    }

    fn move_path(&mut self, from: &str, to: &str) -> Result<(), Refusal> {
        let from = self.path(from)?;
        let to = self.path(to)?;
        let Some(moving) = self.get(&from) else {
            return Err(Refusal::NotFound { path: from });
        };
        if from == to {
            return Ok(());
        }
        if to.starts_with(&format!("{from}/")) {
            return Err(Refusal::BadPath {
                path: to,
                why: "moves a directory into itself",
            });
        }
        let moving_dir = moving.is_dir();
        let mtime_ns = moving.mtime_ns();
        match self.get(&to) {
            Some(Entry::Dir { .. }) if !moving_dir => {
                return Err(Refusal::IsADirectory { path: to });
            }
            Some(Entry::Dir { .. }) if self.has_children(&to) => {
                return Err(Refusal::DirectoryNotEmpty { path: to });
            }
            Some(Entry::File { .. } | Entry::Symlink { .. }) if moving_dir => {
                return Err(Refusal::NotADirectory { path: to });
            }
            _ => {}
        }
        let moved: Vec<(String, Entry)> = self
            .subtree(&from)
            .into_iter()
            .filter_map(|key| {
                let entry = self.remove(&key)?;
                Some((format!("{to}{}", &key[from.len()..]), entry))
            })
            .collect();
        self.make_parents(&to, mtime_ns)?;
        for (path, entry) in moved {
            self.insert(path, entry);
        }
        Ok(())
    }

    fn set_mode(&mut self, path: &str, mode: u32) -> Result<(), Refusal> {
        let path = self.path(path)?;
        Self::mode(&path, mode)?;
        match self.get(&path).cloned() {
            Some(Entry::File {
                digest,
                size,
                mtime_ns,
                link,
                ..
            }) => self.put_file(
                path,
                Entry::File {
                    digest,
                    size,
                    mode,
                    mtime_ns,
                    link,
                },
            ),
            Some(Entry::Dir { mtime_ns, .. }) => self.insert(path, Entry::Dir { mode, mtime_ns }),
            Some(Entry::Symlink { .. }) => {
                return Err(Refusal::BadPath {
                    path,
                    why: "a symlink has no mode",
                });
            }
            None => return Err(Refusal::NotFound { path }),
        }
        Ok(())
    }

    fn set_mtime(&mut self, path: &str, mtime_ns: i64) -> Result<(), Refusal> {
        let path = self.path(path)?;
        let Some(mut entry) = self.get(&path).cloned() else {
            return Err(Refusal::NotFound { path });
        };
        match &mut entry {
            Entry::File { mtime_ns: held, .. }
            | Entry::Dir { mtime_ns: held, .. }
            | Entry::Symlink { mtime_ns: held, .. } => *held = mtime_ns,
        }
        self.put_file(path, entry);
        Ok(())
    }
}

impl Manifest {
    pub fn from_entries(version: u64, entries: BTreeMap<String, Entry>) -> Result<Self, Corrupt> {
        if version == 0 && !entries.is_empty() {
            return Err(Corrupt {
                path: String::new(),
                why: "version 0 holds entries",
            });
        }
        let mut bytes: u64 = 0;
        let mut refs = OrdMap::new();
        let mut links: OrdMap<u64, BTreeSet<String>> = OrdMap::new();
        let mut symlinks = 0;
        for (path, entry) in &entries {
            let corrupt = |why| {
                Err(Corrupt {
                    path: path.clone(),
                    why,
                })
            };
            if !normalized(path) {
                return corrupt("not a normalized path");
            }
            if let Some(dir) = parent(path)
                && !entries.get(dir).is_some_and(Entry::is_dir)
            {
                return corrupt("its parent is not a directory");
            }
            if let Some(digest) = entry.digest() {
                *refs.entry(digest).or_insert(0) += 1;
            }
            if let Some(link) = entry.link() {
                let names = links.entry(link).or_default();
                if let Some(first) = names.first()
                    && entries.get(first) != Some(entry)
                {
                    return corrupt("its hard links hold another file");
                }
                names.insert(path.clone());
            }
            let Some(total) = bytes.checked_add(entry.size()) else {
                return corrupt("file sizes overflow");
            };
            bytes = total;
            symlinks += u64::from(matches!(entry, Entry::Symlink { .. }));
        }
        Ok(Self {
            version,
            entries: entries.into_iter().collect(),
            bytes,
            refs,
            links,
            symlinks,
        })
    }

    /// Whether any entry is a symbolic link; without one every normalized path names
    /// itself.
    #[must_use]
    pub const fn has_symlinks(&self) -> bool {
        self.symlinks > 0
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    #[must_use]
    pub const fn entries(&self) -> &OrdMap<String, Entry> {
        &self.entries
    }

    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    #[must_use]
    pub fn holds(&self, digest: &Digest) -> bool {
        self.refs.contains_key(digest)
    }

    pub fn digests(&self) -> impl Iterator<Item = &Digest> {
        self.refs.keys()
    }

    /// How many names the file at `path` has: 1 for one never hard-linked.
    #[must_use]
    pub fn names(&self, path: &str) -> u64 {
        self.get(path)
            .and_then(Entry::link)
            .and_then(|link| self.links.get(&link))
            .map_or(1, |names| names.len() as u64)
    }

    pub fn children<'a>(&'a self, dir: &str) -> impl Iterator<Item = &'a str> + 'a {
        self.child_entries(dir).into_iter().map(|(name, _)| name)
    }

    /// The entries directly in `dir`, by name in order. Each child's own subtree is
    /// skipped with one seek rather than read key by key.
    #[must_use]
    pub fn child_entries<'a>(&'a self, dir: &str) -> Vec<(&'a str, &'a Entry)> {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let skip = prefix.len();
        let mut found = Vec::new();
        let mut keys = self.entries.range(prefix.clone()..);
        while let Some((key, entry)) = keys.next() {
            if !key.starts_with(&prefix) {
                break;
            }
            let name = &key[skip..];
            match name.find('/') {
                None => found.push((name, entry)),
                Some(at) => {
                    // Every key under "child/" sorts below "child0", '0' being the byte after '/'.
                    let mut past = key[..skip + at].to_owned();
                    past.push('0');
                    keys = self.entries.range(past..);
                }
            }
        }
        found
    }

    pub fn apply(&self, base: u64, ops: &[Op], caps: &Caps) -> Result<Change, Refusal> {
        if base != self.version {
            return Err(Refusal::Stale {
                base,
                head: self.version,
            });
        }
        if ops.len() > caps.ops {
            return Err(Refusal::TooManyOps {
                count: ops.len(),
                max: caps.ops,
            });
        }
        let mut draft = Draft {
            base: &self.entries,
            links: &self.links,
            next_link: self.links.get_max().map_or(0, |(link, _)| *link),
            changes: BTreeMap::new(),
            caps,
        };
        for op in ops {
            draft.apply(op)?;
        }
        let Draft { changes, .. } = draft;
        let delta: BTreeMap<String, Option<Entry>> = changes
            .into_iter()
            .filter(|(path, new)| self.entries.get(path) != new.as_ref())
            .collect();
        let (paths, bytes, counts) = self.tally(&delta);
        if paths > caps.paths {
            return Err(Refusal::TooManyPaths {
                count: paths,
                max: caps.paths,
            });
        }
        if bytes > caps.workspace_bytes {
            return Err(Refusal::WorkspaceFull {
                bytes,
                max: caps.workspace_bytes,
            });
        }
        let (added, dropped) = self.ref_changes(counts);
        self.assert_valid_delta(&delta);
        Ok(Change {
            base,
            version: base + 1,
            bytes,
            paths,
            entries: delta,
            added,
            dropped,
        })
    }

    fn tally(
        &self,
        delta: &BTreeMap<String, Option<Entry>>,
    ) -> (usize, u64, BTreeMap<Digest, i128>) {
        let mut paths = self.entries.len();
        let mut bytes = self.bytes;
        let mut counts: BTreeMap<Digest, i128> = BTreeMap::new();
        for (path, new) in delta {
            if let Some(old) = self.entries.get(path) {
                paths -= 1;
                bytes -= old.size();
                if let Some(digest) = old.digest() {
                    *counts.entry(digest).or_insert(0) -= 1;
                }
            }
            if let Some(new) = new {
                paths += 1;
                bytes = bytes.saturating_add(new.size());
                if let Some(digest) = new.digest() {
                    *counts.entry(digest).or_insert(0) += 1;
                }
            }
        }
        (paths, bytes, counts)
    }

    fn ref_changes(&self, counts: BTreeMap<Digest, i128>) -> (Vec<Digest>, Vec<Digest>) {
        let mut added = Vec::new();
        let mut dropped = Vec::new();
        for (digest, count) in counts {
            let before = self.refs.get(&digest).copied().unwrap_or(0);
            let after = i128::from(before) + count;
            match (before > 0, after > 0) {
                (false, true) => added.push(digest),
                (true, false) => dropped.push(digest),
                _ => {}
            }
        }
        (added, dropped)
    }

    fn assert_valid_delta(&self, delta: &BTreeMap<String, Option<Entry>>) {
        let held = |path: &str| match delta.get(path) {
            Some(entry) => entry.as_ref(),
            None => self.entries.get(path),
        };
        for (path, entry) in delta {
            assert!(normalized(path), "manifest paths are normalized");
            if entry.is_some()
                && let Some(dir) = parent(path)
            {
                assert!(
                    held(dir).is_some_and(Entry::is_dir),
                    "a manifest entry's parent is a directory"
                );
            }
            if !entry.as_ref().is_some_and(Entry::is_dir) {
                let prefix = format!("{path}/");
                let child = self
                    .entries
                    .range(prefix.clone()..)
                    .map(|(key, _)| key)
                    .take_while(|key| key.starts_with(&prefix))
                    .any(|key| held(key).is_some())
                    || delta
                        .range(prefix.clone()..)
                        .take_while(|(key, _)| key.starts_with(&prefix))
                        .any(|(_, entry)| entry.is_some());
                assert!(!child, "a manifest entry's parent is a directory");
            }
        }
    }

    pub fn advance(&mut self, change: &Change) {
        assert_eq!(change.base, self.version, "a change advances its base");
        assert_eq!(
            change.version,
            change.base + 1,
            "a change moves one version"
        );
        let (added, dropped) = self.replay(&change.entries);
        self.version = change.version;
        assert_eq!(
            self.bytes, change.bytes,
            "a change's bytes match its replay"
        );
        assert_eq!(
            self.entries.len(),
            change.paths,
            "a change's paths match its replay"
        );
        assert_eq!(
            added, change.added,
            "a change's added blobs match its replay"
        );
        assert_eq!(
            dropped, change.dropped,
            "a change's dropped blobs match its replay"
        );
    }

    fn replay(&mut self, delta: &BTreeMap<String, Option<Entry>>) -> (Vec<Digest>, Vec<Digest>) {
        let mut held_before = BTreeMap::new();
        for (path, entry) in delta {
            let old = match entry {
                Some(entry) => self.entries.insert(path.clone(), entry.clone()),
                None => self.entries.remove(path),
            };
            if let Some(link) = old.as_ref().and_then(Entry::link)
                && let Some(names) = self.links.get_mut(&link)
            {
                names.remove(path);
                if names.is_empty() {
                    self.links.remove(&link);
                }
            }
            if let Some(link) = entry.as_ref().and_then(Entry::link) {
                self.links.entry(link).or_default().insert(path.clone());
            }
            let symlink =
                |entry: Option<&Entry>| u64::from(matches!(entry, Some(Entry::Symlink { .. })));
            self.symlinks = self.symlinks - symlink(old.as_ref()) + symlink(entry.as_ref());
            if let Some(old) = &old {
                self.bytes -= old.size();
                if let Some(digest) = old.digest() {
                    held_before.entry(digest).or_insert(true);
                    let count = self
                        .refs
                        .get_mut(&digest)
                        .expect("a held digest is counted");
                    *count -= 1;
                    if *count == 0 {
                        self.refs.remove(&digest);
                    }
                }
            }
            if let Some(new) = entry {
                self.bytes += new.size();
                if let Some(digest) = new.digest() {
                    let count = self.refs.entry(digest).or_insert(0);
                    held_before.entry(digest).or_insert(*count > 0);
                    *count += 1;
                }
            }
        }
        let mut added = Vec::new();
        let mut dropped = Vec::new();
        for (digest, before) in held_before {
            match (before, self.refs.contains_key(&digest)) {
                (false, true) => added.push(digest),
                (true, false) => dropped.push(digest),
                _ => {}
            }
        }
        (added, dropped)
    }
}
