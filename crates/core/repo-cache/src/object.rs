use nanocode_bytes::Bytes;
use nanocode_digest::Sha1;

use crate::error::Error;

pub const ID_BYTES: usize = 20;
const HEX: &[u8; 16] = b"0123456789abcdef";
const NIBBLE_BITS: u32 = 4;
const NIBBLE_MASK: u8 = 0x0f;
const GITLINK_MODE: &[u8] = b"160000";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Id(pub [u8; ID_BYTES]);

impl Id {
    pub fn hex(&self) -> String {
        let mut out = String::with_capacity(ID_BYTES * 2);
        for byte in self.0 {
            out.push(char::from(HEX[usize::from(byte >> NIBBLE_BITS)]));
            out.push(char::from(HEX[usize::from(byte & NIBBLE_MASK)]));
        }
        out
    }

    pub fn parse(hex: &[u8]) -> Option<Self> {
        if hex.len() != ID_BYTES * 2 {
            return None;
        }
        let mut id = [0; ID_BYTES];
        for (at, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
            id[at] = (nibble(pair[0])? << NIBBLE_BITS) | nibble(pair[1])?;
        }
        Some(Self(id))
    }
}

fn nibble(digit: u8) -> Option<u8> {
    let at = HEX.iter().position(|&known| known == digit)?;
    u8::try_from(at).ok()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub const fn name(self) -> &'static [u8] {
        match self {
            Self::Commit => b"commit",
            Self::Tree => b"tree",
            Self::Blob => b"blob",
            Self::Tag => b"tag",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Object {
    pub kind: Kind,
    pub data: Bytes,
}

impl Object {
    pub fn id(&self) -> Id {
        let mut hash = Sha1::default();
        hash.update(self.kind.name());
        hash.update(b" ");
        hash.update(self.data.len().to_string().as_bytes());
        hash.update(&[0]);
        hash.update(&self.data);
        Id(hash.finalize())
    }

    pub fn links(&self) -> Result<Vec<Id>, Error> {
        match self.kind {
            Kind::Blob => Ok(Vec::new()),
            Kind::Commit => header_links(&self.data, &[b"tree ", b"parent "]),
            Kind::Tag => header_links(&self.data, &[b"object "]),
            Kind::Tree => tree_links(&self.data),
        }
    }
}

fn header_links(data: &[u8], keys: &[&[u8]]) -> Result<Vec<Id>, Error> {
    let mut links = Vec::new();
    for line in data.split(|&byte| byte == b'\n') {
        if line.is_empty() {
            break;
        }
        if let Some(key) = keys.iter().find(|key| line.starts_with(key)) {
            links.push(Id::parse(&line[key.len()..]).ok_or(Error::Corrupt("object header"))?);
        }
    }
    Ok(links)
}

fn tree_links(mut data: &[u8]) -> Result<Vec<Id>, Error> {
    let mut links = Vec::new();
    while !data.is_empty() {
        let space = data
            .iter()
            .position(|&byte| byte == b' ')
            .ok_or(Error::Corrupt("tree entry mode"))?;
        let nul = data
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(Error::Corrupt("tree entry name"))?;
        let end = nul + 1 + ID_BYTES;
        if nul < space || data.len() < end {
            return Err(Error::Corrupt("tree entry"));
        }
        assert!(space < nul);
        if &data[..space] != GITLINK_MODE {
            let mut id = [0; ID_BYTES];
            id.copy_from_slice(&data[nul + 1..end]);
            links.push(Id(id));
        }
        data = &data[end..];
    }
    Ok(links)
}
