use std::collections::VecDeque;

use serde_json::Value;

use crate::client::Limits;
use crate::error::Error;
use crate::transport::Chunks;

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Added(Value),
    Modified(Value),
    Deleted(Value),
    Bookmark { resource_version: String },
}

pub struct Watch<B> {
    body: Option<B>,
    pending: Vec<u8>,
    decoded: VecDeque<Result<Event, Error>>,
    limits: Limits,
}

impl<B: Chunks> Watch<B> {
    pub fn new(body: B, limits: Limits) -> Self {
        assert!(limits.events_buffered_max > 0);
        Self {
            body: Some(body),
            pending: Vec::new(),
            decoded: VecDeque::with_capacity(limits.events_buffered_max),
            limits,
        }
    }

    pub fn buffered(&self) -> usize {
        self.decoded.len()
    }

    pub async fn next(&mut self) -> Result<Option<Event>, Error> {
        loop {
            if let Some(decoded) = self.decoded.pop_front() {
                if decoded.is_err() {
                    self.body = None;
                    self.decoded.clear();
                }
                return decoded.map(Some);
            }
            let Some(body) = self.body.as_mut() else {
                return Ok(None);
            };
            match body.chunk().await {
                Err(error) => {
                    self.body = None;
                    return Err(error);
                }
                Ok(None) => {
                    self.body = None;
                    if !self.pending.iter().all(u8::is_ascii_whitespace) {
                        return Err(Error::malformed("watch stream ended inside an event"));
                    }
                    return Ok(None);
                }
                Ok(Some(chunk)) => self.push(&chunk)?,
            }
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), Error> {
        self.pending.extend_from_slice(chunk);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if self.decoded.len() >= self.limits.events_buffered_max {
                self.body = None;
                return Err(Error::Full {
                    bound: "events_buffered_max",
                    limit: self.limits.events_buffered_max as u64,
                });
            }
            self.decoded.push_back(decode_event(&line));
        }
        if self.pending.len() as u64 > self.limits.line_bytes_max {
            self.body = None;
            return Err(Error::TooLarge {
                bound: "line_bytes_max",
                limit: self.limits.line_bytes_max,
            });
        }
        Ok(())
    }
}

fn decode_event(line: &[u8]) -> Result<Event, Error> {
    let mut event: Value = serde_json::from_slice(line)
        .map_err(|error| Error::malformed(format!("watch event is not JSON: {error}")))?;
    let object = event
        .get_mut("object")
        .map(Value::take)
        .ok_or_else(|| Error::malformed("watch event has no object"))?;
    match event.get("type").and_then(Value::as_str) {
        Some("ADDED") => Ok(Event::Added(object)),
        Some("MODIFIED") => Ok(Event::Modified(object)),
        Some("DELETED") => Ok(Event::Deleted(object)),
        Some("BOOKMARK") => {
            let resource_version = crate::types::object_resource_version(&object)
                .ok_or_else(|| Error::malformed("bookmark has no resourceVersion"))?;
            Ok(Event::Bookmark {
                resource_version: resource_version.to_owned(),
            })
        }
        Some("ERROR") => Err(decode_status(&object)),
        _ => Err(Error::malformed("watch event has an unknown type")),
    }
}

pub fn decode_status(status: &Value) -> Error {
    let code = status
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .unwrap_or(0);
    let text = |key: &str| {
        status
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match code {
        409 => Error::Conflict {
            message: text("message"),
        },
        410 => Error::Gone {
            message: text("message"),
        },
        _ => Error::Status {
            code,
            reason: text("reason"),
            message: text("message"),
        },
    }
}
