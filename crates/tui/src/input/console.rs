use std::{
    io,
    os::windows::io::AsHandle,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use nanocode_executor::{JoinHandle, spawn_blocking};
use nanocode_sys::{
    console::{ConsoleInput, Wakeup, pending_input, read_input, wait_input},
    windows_sys::Win32::System::Console::INPUT_RECORD,
};

use super::{Event, parse};
use crate::terminal;

const INPUT_RECORDS: usize = 256;
const HIGH_SURROGATES: std::ops::Range<u16> = 0xD800..0xDC00;

struct Chunk {
    resized: bool,
    more: bool,
}

struct Reader {
    console: terminal::Tty,
    wakeup: Arc<Wakeup>,
    records: Box<[INPUT_RECORD; INPUT_RECORDS]>,
    surrogate: Option<u16>,
    bytes: Vec<u8>,
}

impl Reader {
    fn push(&mut self, unit: u16) {
        if HIGH_SURROGATES.contains(&unit) {
            if self.surrogate.replace(unit).is_some() {
                self.push_char(char::REPLACEMENT_CHARACTER);
            }
            return;
        }
        let high = self.surrogate.take();
        for decoded in char::decode_utf16(high.into_iter().chain([unit])) {
            self.push_char(decoded.unwrap_or(char::REPLACEMENT_CHARACTER));
        }
    }

    fn push_char(&mut self, character: char) {
        let mut encoded = [0; 4];
        self.bytes
            .extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
    }

    fn fill(&mut self) -> io::Result<Option<Chunk>> {
        self.bytes.clear();
        let mut resized = false;
        while self.bytes.is_empty() && !resized {
            if !wait_input(self.console.as_handle(), &self.wakeup)? {
                return Ok(None);
            }
            let read = read_input(self.console.as_handle(), &mut self.records[..])?.len();
            for index in 0..read {
                match ConsoleInput::from(&self.records[index]) {
                    ConsoleInput::Unit(unit) => self.push(unit),
                    ConsoleInput::Resize => resized = true,
                    ConsoleInput::Other => {}
                }
            }
        }
        let more = self.surrogate.is_some() || pending_input(self.console.as_handle())? > 0;
        Ok(Some(Chunk { resized, more }))
    }
}

type Filled = (Box<Reader>, io::Result<Option<Chunk>>);

enum State {
    Idle(Box<Reader>),
    Busy(JoinHandle<Filled>),
    Ended,
}

pub struct EventStream {
    state: State,
    wakeup: Arc<Wakeup>,
    parser: parse::Parser,
    resized: bool,
}

impl EventStream {
    pub fn new() -> io::Result<Self> {
        let wakeup = Arc::new(Wakeup::new()?);
        let reader = Reader {
            console: terminal::tty()?,
            wakeup: wakeup.clone(),
            records: Box::new([INPUT_RECORD::default(); INPUT_RECORDS]),
            surrogate: None,
            bytes: Vec::new(),
        };
        Ok(Self {
            state: State::Idle(Box::new(reader)),
            wakeup,
            parser: parse::Parser::default(),
            resized: false,
        })
    }

    pub async fn next(&mut self) -> Option<io::Result<Event>> {
        std::future::poll_fn(|context| self.poll_next(context)).await
    }

    pub fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<Option<io::Result<Event>>> {
        loop {
            if let Some(event) = self.parser.next_event() {
                return Poll::Ready(Some(Ok(event)));
            }
            if self.resized {
                self.resized = false;
                return Poll::Ready(Some(
                    terminal::size().map(|(columns, rows)| Event::Resize(columns, rows)),
                ));
            }
            match std::mem::replace(&mut self.state, State::Ended) {
                State::Idle(mut reader) => {
                    self.state = State::Busy(spawn_blocking(move || {
                        let filled = reader.fill();
                        (reader, filled)
                    }));
                }
                State::Busy(mut handle) => {
                    let joined = match Pin::new(&mut handle).poll(context) {
                        Poll::Ready(joined) => joined,
                        Poll::Pending => {
                            self.state = State::Busy(handle);
                            return Poll::Pending;
                        }
                    };
                    let Ok((reader, filled)) = joined else {
                        return Poll::Ready(Some(Err(io::Error::other(
                            "the console input reader stopped",
                        ))));
                    };
                    match filled {
                        Ok(Some(chunk)) => {
                            self.parser.advance(&reader.bytes, chunk.more);
                            self.resized |= chunk.resized;
                            self.state = State::Idle(reader);
                        }
                        Ok(None) => return Poll::Ready(None),
                        Err(error) => {
                            self.state = State::Idle(reader);
                            return Poll::Ready(Some(Err(error)));
                        }
                    }
                }
                State::Ended => return Poll::Ready(None),
            }
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        if matches!(self.state, State::Busy(_)) {
            drop(self.wakeup.wake());
        }
    }
}
