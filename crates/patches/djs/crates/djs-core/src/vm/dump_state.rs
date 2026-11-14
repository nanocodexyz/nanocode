//! The host's state of an open call (D2770): JSON text the dumper writes as the fourth
//! argument of `$dump.pending`, where the loader evaluates it as a JavaScript expression.
//! [`check`] accepts exactly the JSON texts (RFC 8259) that mean the same as an
//! expression: everything but an object key `__proto__`, which an object literal takes
//! as its prototype (and refuses twice) where `JSON.parse` makes an own property. So the
//! host's text cannot inject code, and the loader gives back what the host saved.

use alloc::string::String;

/// Nesting deeper than this is refused: the loader's parser recurses on it.
const MAX_DEPTH: u32 = 512;

/// Whether `text` is JSON that reads the same as a JavaScript expression; the reason if not.
pub(crate) fn check(text: &str) -> Result<(), &'static str> {
    let mut p = Parser {
        b: text.as_bytes(),
        at: 0,
    };
    p.ws();
    p.value(0)?;
    p.ws();
    if p.at != p.b.len() {
        return Err("the call's state is not JSON (text after the value)");
    }
    Ok(())
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
}

const NOT_JSON: &str = "the call's state is not JSON";

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        let yes = self.peek() == Some(c);
        if yes {
            self.at += 1;
        }
        yes
    }

    fn word(&mut self, w: &[u8]) -> Result<(), &'static str> {
        if self.b[self.at..].starts_with(w) {
            self.at += w.len();
            Ok(())
        } else {
            Err(NOT_JSON)
        }
    }

    fn value(&mut self, depth: u32) -> Result<(), &'static str> {
        if depth > MAX_DEPTH {
            return Err("the call's state nests too deeply");
        }
        match self.peek().ok_or(NOT_JSON)? {
            b'{' => {
                self.at += 1;
                self.ws();
                if self.eat(b'}') {
                    return Ok(());
                }
                loop {
                    self.ws();
                    if self.string()? == "__proto__" {
                        return Err("the call's state has a key __proto__");
                    }
                    self.ws();
                    if !self.eat(b':') {
                        return Err(NOT_JSON);
                    }
                    self.ws();
                    self.value(depth + 1)?;
                    self.ws();
                    if self.eat(b'}') {
                        return Ok(());
                    }
                    if !self.eat(b',') {
                        return Err(NOT_JSON);
                    }
                }
            }
            b'[' => {
                self.at += 1;
                self.ws();
                if self.eat(b']') {
                    return Ok(());
                }
                loop {
                    self.ws();
                    self.value(depth + 1)?;
                    self.ws();
                    if self.eat(b']') {
                        return Ok(());
                    }
                    if !self.eat(b',') {
                        return Err(NOT_JSON);
                    }
                }
            }
            b'"' => self.string().map(drop),
            b't' => self.word(b"true"),
            b'f' => self.word(b"false"),
            b'n' => self.word(b"null"),
            _ => self.number(),
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<(), &'static str> {
        self.eat(b'-');
        if !self.eat(b'0') && self.digits() == 0 {
            return Err(NOT_JSON);
        }
        if self.eat(b'.') && self.digits() == 0 {
            return Err(NOT_JSON);
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if self.digits() == 0 {
                return Err(NOT_JSON);
            }
        }
        Ok(())
    }

    /// A string; its value only as far as it may be `__proto__` (keys), else empty.
    fn string(&mut self) -> Result<String, &'static str> {
        if !self.eat(b'"') {
            return Err(NOT_JSON);
        }
        let mut s = String::new();
        loop {
            let c = self.peek().ok_or(NOT_JSON)?;
            self.at += 1;
            match c {
                b'"' => return Ok(s),
                0..=0x1f => return Err(NOT_JSON),
                b'\\' => {
                    let e = self.peek().ok_or(NOT_JSON)?;
                    self.at += 1;
                    let ch = match e {
                        b'"' | b'\\' | b'/' => e as char,
                        b'b' | b'f' | b'n' | b'r' | b't' => ' ',
                        b'u' => {
                            let hex = self.b.get(self.at..self.at + 4).ok_or(NOT_JSON)?;
                            let mut n = 0u32;
                            for &d in hex {
                                n = n * 16 + (d as char).to_digit(16).ok_or(NOT_JSON)?;
                            }
                            self.at += 4;
                            // A surrogate cannot be part of `__proto__`.
                            char::from_u32(n).unwrap_or(' ')
                        }
                        _ => return Err(NOT_JSON),
                    };
                    if s.len() < 16 {
                        s.push(ch);
                    }
                }
                // UTF-8 continues as is: `text` is a str.
                _ => {
                    if s.len() < 16 {
                        s.push(c as char);
                    }
                }
            }
        }
    }
}
