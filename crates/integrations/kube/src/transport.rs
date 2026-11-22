use std::future::Future;

use crate::error::Error;

pub const APPLY_PATCH: &str = "application/apply-patch+yaml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Get,
    Patch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub verb: Verb,
    pub path: String,
    pub query: Vec<(&'static str, String)>,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn target(&self) -> String {
        let mut target = self.path.clone();
        for (index, (key, value)) in self.query.iter().enumerate() {
            target.push(if index == 0 { '?' } else { '&' });
            target.push_str(key);
            target.push('=');
            encode_component(&mut target, value);
        }
        target
    }

    pub fn query(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }
}

fn encode_component(target: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            target.push(char::from(byte));
        } else {
            target.push('%');
            target.push(char::from(HEX[usize::from(byte >> 4)]));
            target.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
}

pub struct Response<B> {
    pub status: u16,
    pub body: B,
}

pub trait Chunks {
    fn chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Error>>;
}

pub trait Transport {
    type Body: Chunks;

    fn send(&self, request: Request) -> impl Future<Output = Result<Response<Self::Body>, Error>>;
}
