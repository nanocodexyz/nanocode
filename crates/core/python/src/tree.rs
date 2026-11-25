use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    io,
};

use crate::{Files, Kind, Remove, Stat, Write};

pub enum Entry {
    File {
        data: Vec<u8>,
        mode: u32,
        mtime_ns: i64,
    },
    Dir {
        mode: u32,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    pub removed: Vec<Vec<u8>>,
    pub made: Vec<(Vec<u8>, u32)>,
    pub written: Vec<Vec<u8>>,
    pub modes: Vec<(Vec<u8>, u32)>,
    /// Symlinks Python made, as (link, target).
    pub linked: Vec<(Vec<u8>, Vec<u8>)>,
    /// Modification times Python set, as (path, mtime_ns).
    pub times: Vec<(Vec<u8>, i64)>,
    /// Hard links Python made, as (existing, link), in the order it made them.
    pub hard_linked: Vec<(Vec<u8>, Vec<u8>)>,
}

enum Node {
    File {
        data: Vec<u8>,
        mode: u32,
        mtime_ns: i64,
        changed: bool,
        moded: bool,
        /// How many bytes at the start are still the content copied in, when
        /// Python only appended since: write back only the rest, so an append
        /// another writer made meanwhile is kept.
        kept: Option<usize>,
    },
    Dir {
        mode: u32,
        mtime_ns: i64,
    },
}

impl Node {
    const fn kind(&self) -> Kind {
        match self {
            Self::File { .. } => Kind::File,
            Self::Dir { .. } => Kind::Dir,
        }
    }
}

pub struct Tree {
    roots: Vec<Vec<u8>>,
    first: RefCell<BTreeMap<Vec<u8>, Kind>>,
    nodes: RefCell<BTreeMap<Vec<u8>, Node>>,
    /// Symlinks Python made here, by path, each with its target as given.
    links: RefCell<BTreeMap<Vec<u8>, Vec<u8>>>,
    /// Paths whose modification time Python set.
    timed: RefCell<std::collections::BTreeSet<Vec<u8>>>,
    /// The hard-link group of each file that shares its content with other names.
    groups: RefCell<BTreeMap<Vec<u8>, u64>>,
    /// Hard links Python made here, as (existing, link).
    made: RefCell<Vec<(Vec<u8>, Vec<u8>)>>,
    /// The next group a hard link Python makes starts.
    fresh: Cell<u64>,
}

fn parent(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(0) | None => b"/",
        Some(cut) => &path[..cut],
    }
}

fn name(path: &[u8]) -> &[u8] {
    let cut = path.iter().rposition(|&byte| byte == b'/');
    &path[cut.map_or(0, |cut| cut + 1)..]
}

fn under(path: &[u8], dir: &[u8]) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/") || dir == b"/")
}

fn normalize(path: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&byte| byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut out = Vec::new();
    for part in parts {
        out.push(b'/');
        out.extend_from_slice(part);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    assert!(out.first() == Some(&b'/'));
    assert!(out.len() == 1 || out.last() != Some(&b'/'));
    out
}

fn now() -> i64 {
    nanocode_platform::system_time()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_nanos()).unwrap_or(i64::MAX)
        })
}

fn missing() -> io::Error {
    io::ErrorKind::NotFound.into()
}

impl Tree {
    pub const fn new(roots: Vec<Vec<u8>>) -> Self {
        Self {
            roots,
            first: RefCell::new(BTreeMap::new()),
            nodes: RefCell::new(BTreeMap::new()),
            links: RefCell::new(BTreeMap::new()),
            timed: RefCell::new(std::collections::BTreeSet::new()),
            groups: RefCell::new(BTreeMap::new()),
            made: RefCell::new(Vec::new()),
            fresh: Cell::new(1 << 62),
        }
    }

    /// Puts the file at `path` in hard-link group `group`, with the other names
    /// that share its content.
    pub fn join(&self, path: Vec<u8>, group: u64) {
        self.groups.borrow_mut().insert(path, group);
    }

    /// The other names of the file at `path`.
    fn peers(&self, path: &[u8]) -> Vec<Vec<u8>> {
        let groups = self.groups.borrow();
        let Some(group) = groups.get(path) else {
            return Vec::new();
        };
        groups
            .iter()
            .filter(|(other, shared)| *shared == group && other.as_slice() != path)
            .map(|(other, _)| other.clone())
            .collect()
    }

    /// Gives every other name of the file at `path` its content, mode and time.
    fn spread(&self, path: &[u8], nodes: &mut BTreeMap<Vec<u8>, Node>) {
        let peers = self.peers(path);
        if peers.is_empty() {
            return;
        }
        let Some(Node::File {
            data,
            mode,
            mtime_ns,
            changed,
            moded,
            ..
        }) = nodes.get(path)
        else {
            return;
        };
        let (data, mode, mtime_ns, changed, moded) =
            (data.clone(), *mode, *mtime_ns, *changed, *moded);
        for peer in peers {
            if let Some(Node::File {
                data: held,
                mode: kept,
                mtime_ns: time,
                changed: written,
                moded: chmoded,
                ..
            }) = nodes.get_mut(&peer)
            {
                held.clone_from(&data);
                *kept = mode;
                *time = mtime_ns;
                *written |= changed;
                *chmoded |= moded;
            }
        }
    }

    pub fn put(&self, path: Vec<u8>, entry: Entry) {
        let node = match entry {
            Entry::File {
                data,
                mode,
                mtime_ns,
            } => Node::File {
                kept: Some(data.len()),
                data,
                mode,
                mtime_ns,
                changed: false,
                moded: false,
            },
            Entry::Dir { mode } => Node::Dir {
                mode,
                mtime_ns: now(),
            },
        };
        self.first.borrow_mut().insert(path.clone(), node.kind());
        self.nodes.borrow_mut().insert(path, node);
    }

    pub fn delta(&self) -> Delta {
        let first = self.first.borrow();
        let nodes = self.nodes.borrow();
        let mut delta = Delta::default();
        for (path, kind) in first.iter().rev() {
            if nodes.get(path).map(Node::kind) != Some(*kind) {
                delta.removed.push(path.clone());
            }
        }
        for (path, target) in self.links.borrow().iter() {
            delta.linked.push((path.clone(), target.clone()));
        }
        for (path, node) in nodes.iter() {
            match node {
                Node::Dir { mode, .. } if first.get(path) != Some(&Kind::Dir) => {
                    delta.made.push((path.clone(), *mode));
                }
                _ => {}
            }
            if let Node::File {
                changed,
                moded,
                mode,
                ..
            } = node
            {
                if *changed {
                    delta.written.push(path.clone());
                }
                if *moded {
                    delta.modes.push((path.clone(), *mode));
                }
            }
        }
        for path in self.timed.borrow().iter() {
            if let Some(Node::File { mtime_ns, .. } | Node::Dir { mtime_ns, .. }) = nodes.get(path)
            {
                delta.times.push((path.clone(), *mtime_ns));
            }
        }
        let groups = self.groups.borrow();
        for (existing, link) in self.made.borrow().iter() {
            let file = |path: &Vec<u8>| matches!(nodes.get(path), Some(Node::File { .. }));
            let shared = groups.get(existing).is_some() && groups.get(existing) == groups.get(link);
            if file(existing) && file(link) && shared {
                // The link takes its content from the name it links to.
                delta.written.retain(|path| path != link);
                delta.modes.retain(|(path, _)| path != link);
                delta.hard_linked.push((existing.clone(), link.clone()));
            } else if file(link) && !delta.written.contains(link) {
                delta.written.push(link.clone());
            }
        }
        delta
    }

    pub fn take(&self, path: &[u8]) -> Option<Vec<u8>> {
        match self.nodes.borrow_mut().get_mut(path) {
            Some(Node::File { data, .. }) => Some(std::mem::take(data)),
            _ => None,
        }
    }

    /// What Python appended to a file it copied in and only appended to,
    /// taken out of the tree; None for any other file, which is written whole,
    /// and for a file with other names, whose every name is written back.
    pub fn take_appended(&self, path: &[u8]) -> Option<Vec<u8>> {
        if !self.peers(path).is_empty() {
            return None;
        }
        match self.nodes.borrow_mut().get_mut(path) {
            Some(Node::File {
                data,
                kept: Some(kept),
                ..
            }) => Some(data.split_off(*kept)),
            _ => None,
        }
    }

    pub fn holds(&self, path: &[u8]) -> bool {
        self.roots.iter().any(|root| under(path, root))
    }

    /// Where a path leads once the symlinks Python made here are followed.
    fn resolve(&self, path: &[u8]) -> io::Result<Vec<u8>> {
        let links = self.links.borrow();
        let mut path = path.to_vec();
        for _ in 0..40 {
            let Some(target) = links.get(&path) else {
                return Ok(path);
            };
            path = if target.starts_with(b"/") {
                normalize(target)
            } else {
                let mut joined = parent(&path).to_vec();
                joined.push(b'/');
                joined.extend_from_slice(target);
                normalize(&joined)
            };
            self.within(&path)?;
        }
        Err(io::Error::other("Too many levels of symbolic links"))
    }

    fn within(&self, path: &[u8]) -> io::Result<()> {
        if self.holds(path) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "outside the directories this shell copied in for Python",
            ))
        }
    }
}

impl Files for Tree {
    fn stat(&self, path: &[u8]) -> io::Result<Stat> {
        self.within(path)?;
        let path = &self.resolve(path)?[..];
        match self.nodes.borrow().get(path) {
            Some(Node::File {
                data,
                mode,
                mtime_ns,
                ..
            }) => Ok(Stat {
                kind: Kind::File,
                size: data.len() as u64,
                mode: *mode,
                mtime_ns: *mtime_ns,
            }),
            Some(Node::Dir { mode, mtime_ns }) => Ok(Stat {
                kind: Kind::Dir,
                size: 0,
                mode: *mode,
                mtime_ns: *mtime_ns,
            }),
            None => Err(missing()),
        }
    }

    fn read(&self, path: &[u8], _limit: u64) -> io::Result<Vec<u8>> {
        self.within(path)?;
        let path = &self.resolve(path)?[..];
        match self.nodes.borrow().get(path) {
            Some(Node::File { data, .. }) => Ok(data.clone()),
            Some(Node::Dir { .. }) => Err(io::ErrorKind::IsADirectory.into()),
            None => Err(missing()),
        }
    }

    fn write(&self, path: &[u8], more: &[u8], write: Write) -> io::Result<()> {
        self.within(path)?;
        let path = &self.resolve(path)?[..];
        let mut nodes = self.nodes.borrow_mut();
        if !matches!(nodes.get(parent(path)), Some(Node::Dir { .. })) {
            return Err(missing());
        }
        let done = match (nodes.get_mut(path), write) {
            (Some(Node::Dir { .. }), _) => Err(io::ErrorKind::IsADirectory.into()),
            (Some(Node::File { .. }), Write::New) => Err(io::ErrorKind::AlreadyExists.into()),
            (
                Some(Node::File {
                    data,
                    changed,
                    mtime_ns,
                    ..
                }),
                Write::Append,
            ) => {
                data.extend_from_slice(more);
                *changed = true;
                *mtime_ns = now();
                Ok(())
            }
            (
                Some(Node::File {
                    data,
                    changed,
                    mtime_ns,
                    kept,
                    ..
                }),
                Write::Replace,
            ) => {
                *data = more.to_vec();
                *kept = None;
                *changed = true;
                *mtime_ns = now();
                Ok(())
            }
            (None, _) => {
                let node = Node::File {
                    data: more.to_vec(),
                    mode: 0o644,
                    mtime_ns: now(),
                    changed: true,
                    moded: false,
                    kept: None,
                };
                nodes.insert(path.to_vec(), node);
                Ok(())
            }
        };
        if done.is_ok() {
            self.spread(path, &mut nodes);
        }
        done
    }

    fn set_mode(&self, path: &[u8], mode: u32) -> io::Result<()> {
        self.within(path)?;
        let path = &self.resolve(path)?[..];
        let mut nodes = self.nodes.borrow_mut();
        match nodes.get_mut(path) {
            Some(Node::File {
                mode: kept, moded, ..
            }) => {
                *kept = mode & 0o7777;
                *moded = true;
            }
            Some(Node::Dir { mode: kept, .. }) => {
                *kept = mode & 0o7777;
                return Ok(());
            }
            None => return Err(missing()),
        }
        self.spread(path, &mut nodes);
        Ok(())
    }

    fn set_mtime(&self, path: &[u8], mtime_ns: i64) -> io::Result<()> {
        self.within(path)?;
        let path = self.resolve(path)?;
        let mut nodes = self.nodes.borrow_mut();
        match nodes.get_mut(&path) {
            Some(Node::File { mtime_ns: kept, .. } | Node::Dir { mtime_ns: kept, .. }) => {
                *kept = mtime_ns;
            }
            None => return Err(missing()),
        }
        self.spread(&path, &mut nodes);
        drop(nodes);
        self.timed.borrow_mut().insert(path);
        Ok(())
    }

    fn list(&self, path: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.within(path)?;
        let nodes = self.nodes.borrow();
        match nodes.get(path) {
            Some(Node::Dir { .. }) => Ok(nodes
                .keys()
                .chain(self.links.borrow().keys())
                .filter(|other| other.len() > path.len() && parent(other) == path)
                .map(|other| name(other).to_vec())
                .collect()),
            Some(Node::File { .. }) => Err(io::ErrorKind::NotADirectory.into()),
            None => Err(missing()),
        }
    }

    fn mkdir(&self, path: &[u8], mode: u32) -> io::Result<()> {
        self.within(path)?;
        let mut nodes = self.nodes.borrow_mut();
        if nodes.contains_key(path) || self.links.borrow().contains_key(path) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        if !matches!(nodes.get(parent(path)), Some(Node::Dir { .. })) {
            return Err(missing());
        }
        let mtime_ns = now();
        nodes.insert(path.to_vec(), Node::Dir { mode, mtime_ns });
        Ok(())
    }

    fn remove(&self, path: &[u8], remove: Remove) -> io::Result<()> {
        self.within(path)?;
        if self.links.borrow().contains_key(path) {
            if matches!(remove, Remove::Dir) {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            self.links.borrow_mut().remove(path);
            return Ok(());
        }
        let mut nodes = self.nodes.borrow_mut();
        match (nodes.get(path), remove) {
            (None, _) => return Err(missing()),
            (Some(Node::Dir { .. }), Remove::File) => {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            (Some(Node::File { .. }), Remove::Dir) => {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            (Some(Node::Dir { .. }), Remove::Dir) => {
                if self.roots.iter().any(|root| root == path) {
                    return Err(io::ErrorKind::ResourceBusy.into());
                }
                if nodes
                    .keys()
                    .any(|other| other.len() > path.len() && parent(other) == path)
                {
                    return Err(io::ErrorKind::DirectoryNotEmpty.into());
                }
            }
            (Some(Node::File { .. }), Remove::File) => {}
        }
        nodes.remove(path);
        self.groups.borrow_mut().remove(path);
        Ok(())
    }

    fn rename(&self, from: &[u8], to: &[u8]) -> io::Result<()> {
        self.within(from)?;
        self.within(to)?;
        let link = self.links.borrow_mut().remove(from);
        if let Some(target) = link {
            if matches!(self.nodes.borrow().get(to), Some(Node::Dir { .. })) {
                self.links.borrow_mut().insert(from.to_vec(), target);
                return Err(io::ErrorKind::IsADirectory.into());
            }
            self.nodes.borrow_mut().remove(to);
            self.links.borrow_mut().insert(to.to_vec(), target);
            return Ok(());
        }
        self.links.borrow_mut().remove(to);
        let mut nodes = self.nodes.borrow_mut();
        if !nodes.contains_key(from) {
            return Err(missing());
        }
        if !matches!(nodes.get(parent(to)), Some(Node::Dir { .. })) {
            return Err(missing());
        }
        let moved: Vec<Vec<u8>> = nodes
            .keys()
            .filter(|path| {
                path.strip_prefix(from)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
            })
            .cloned()
            .collect();
        let mut groups = self.groups.borrow_mut();
        let mut made = self.made.borrow_mut();
        if matches!(nodes.get(to), Some(Node::File { .. })) {
            groups.remove(to);
        }
        for path in moved {
            let Some(mut node) = nodes.remove(&path) else {
                continue;
            };
            if let Node::File { changed, kept, .. } = &mut node {
                *changed = true;
                *kept = None;
            }
            let mut placed = to.to_vec();
            placed.extend_from_slice(&path[from.len()..]);
            if let Some(group) = groups.remove(&path) {
                groups.insert(placed.clone(), group);
            }
            for (existing, link) in made.iter_mut() {
                if *existing == path {
                    existing.clone_from(&placed);
                }
                if *link == path {
                    link.clone_from(&placed);
                }
            }
            nodes.insert(placed, node);
        }
        Ok(())
    }

    fn lstat(&self, path: &[u8]) -> io::Result<Stat> {
        self.within(path)?;
        let size = self
            .links
            .borrow()
            .get(path)
            .map(|target| target.len() as u64);
        match size {
            Some(size) => Ok(Stat {
                kind: Kind::Symlink,
                size,
                mode: 0o777,
                mtime_ns: now(),
            }),
            None => self.stat(path),
        }
    }

    fn symlink(&self, target: &[u8], link: &[u8]) -> io::Result<()> {
        self.within(link)?;
        let nodes = self.nodes.borrow();
        if nodes.contains_key(link) || self.links.borrow().contains_key(link) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        if !matches!(nodes.get(parent(link)), Some(Node::Dir { .. })) {
            return Err(missing());
        }
        self.links
            .borrow_mut()
            .insert(link.to_vec(), target.to_vec());
        Ok(())
    }

    fn read_link(&self, path: &[u8]) -> io::Result<Vec<u8>> {
        self.within(path)?;
        if let Some(target) = self.links.borrow().get(path) {
            return Ok(target.clone());
        }
        if self.nodes.borrow().contains_key(path) {
            Err(io::ErrorKind::InvalidInput.into())
        } else {
            Err(missing())
        }
    }

    fn hard_link(&self, existing: &[u8], link: &[u8]) -> io::Result<()> {
        self.within(existing)?;
        self.within(link)?;
        if self.links.borrow().contains_key(existing) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let mut nodes = self.nodes.borrow_mut();
        if nodes.contains_key(link) || self.links.borrow().contains_key(link) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        if !matches!(nodes.get(parent(link)), Some(Node::Dir { .. })) {
            return Err(missing());
        }
        let copy = match nodes.get(existing) {
            Some(Node::File {
                data,
                mode,
                mtime_ns,
                ..
            }) => Node::File {
                data: data.clone(),
                mode: *mode,
                mtime_ns: *mtime_ns,
                changed: false,
                moded: false,
                kept: None,
            },
            Some(Node::Dir { .. }) => return Err(io::ErrorKind::PermissionDenied.into()),
            None => return Err(missing()),
        };
        nodes.insert(link.to_vec(), copy);
        let mut groups = self.groups.borrow_mut();
        let group = if let Some(group) = groups.get(existing) {
            *group
        } else {
            let group = self.fresh.get();
            self.fresh.set(group + 1);
            groups.insert(existing.to_vec(), group);
            group
        };
        groups.insert(link.to_vec(), group);
        self.made
            .borrow_mut()
            .push((existing.to_vec(), link.to_vec()));
        Ok(())
    }

    fn links(&self, path: &[u8], follow: bool) -> Option<(u64, u64)> {
        let path = if follow {
            self.resolve(path).ok()?
        } else {
            path.to_vec()
        };
        let groups = self.groups.borrow();
        let group = *groups.get(&path)?;
        let names = groups.values().filter(|shared| **shared == group).count() as u64;
        Some((group, names))
    }
}
