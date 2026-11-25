const TABLE_ENTRIES_MAX: usize = 4096;
const CODE_BITS_MAX: u8 = 12;
const BURST_CODES: usize = 6;
const BIT_BUFFER_BITS: u8 = 64;

pub(crate) type Code = u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Progress,
    NoProgress,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InvalidCode;

pub(crate) struct Decoded {
    pub(crate) read: usize,
    pub(crate) written: usize,
    pub(crate) status: Result<Status, InvalidCode>,
}

#[derive(Clone, Copy)]
struct Link {
    previous: Code,
    byte: u8,
    first: u8,
}

#[derive(Clone, Copy)]
struct Derivation {
    code: Code,
    first: u8,
}

impl Derivation {
    const fn link(self, byte: u8) -> Link {
        Link {
            previous: self.code,
            byte,
            first: self.first,
        }
    }
}

struct Table {
    links: Vec<Link>,
    depths: Vec<u16>,
}

impl Table {
    fn init(&mut self, code_bits_min: u8) {
        self.links.clear();
        self.depths.clear();
        for literal in 0..(1u16 << code_bits_min) {
            let byte = literal as u8;
            self.links.push(Link {
                previous: 0,
                byte,
                first: byte,
            });
            self.depths.push(1);
        }
        for _ in 0..2 {
            self.links.push(Link {
                previous: 0,
                byte: 0,
                first: 0,
            });
            self.depths.push(0);
        }
    }

    fn clear(&mut self, code_bits_min: u8) {
        let fixed = usize::from(1u16 << code_bits_min) + 2;
        self.links.truncate(fixed);
        self.depths.truncate(fixed);
    }

    const fn is_full(&self) -> bool {
        self.links.len() >= TABLE_ENTRIES_MAX
    }

    fn derive(&mut self, from: Derivation, byte: u8) {
        let depth = self.depths[usize::from(from.code)] + 1;
        self.links.push(from.link(byte));
        self.depths.push(depth);
    }

    fn derive_burst(&mut self, from: &mut Derivation, codes: &[Code], firsts: &[u8]) {
        let mut depth_of = from.code;
        for &code in codes {
            let depth = self.depths[usize::from(depth_of)] + 1;
            self.depths.push(depth);
            depth_of = code;
        }
        for (&code, &first) in codes.iter().zip(firsts) {
            self.links.push(from.link(first));
            from.code = code;
            from.first = first;
        }
    }

    fn reconstruct(&self, code: Code, output: &mut [u8]) -> u8 {
        let table = &self.links[..=usize::from(code)];
        let first = table[usize::from(code)].first;
        let mut walk = code;
        for byte in output.iter_mut().rev() {
            let entry = table[usize::from(walk)];
            walk = code.min(entry.previous);
            *byte = entry.byte;
        }
        first
    }
}

struct Bits {
    code_bits: u8,
    mask: u16,
    buffer: u64,
    count: u8,
}

impl Bits {
    const fn new(code_bits_min: u8) -> Self {
        Self {
            code_bits: code_bits_min + 1,
            mask: (1u16 << (code_bits_min + 1)) - 1,
            buffer: 0,
            count: 0,
        }
    }

    const fn reset(&mut self, code_bits_min: u8) {
        self.code_bits = code_bits_min + 1;
        self.mask = (1 << self.code_bits) - 1;
    }

    fn refill(&mut self, input: &mut &[u8]) {
        let wanted = usize::from((BIT_BUFFER_BITS - self.count) / 8);
        let take = wanted.min(input.len());
        let mut bytes = [0u8; 8];
        bytes[..take].copy_from_slice(&input[..take]);
        *input = &input[take..];
        if take > 0 {
            self.buffer |= u64::from_le_bytes(bytes) << self.count;
        }
        self.count += (take * 8) as u8;
    }

    fn next(&mut self, input: &mut &[u8]) -> Option<Code> {
        if self.count < self.code_bits {
            self.refill(input);
        }
        if self.count < self.code_bits {
            return None;
        }
        let code = (self.buffer & u64::from(self.mask)) as Code;
        self.buffer >>= self.code_bits;
        self.count -= self.code_bits;
        Some(code)
    }

    fn peek(&self, codes: &mut [Code; BURST_CODES]) -> usize {
        let mut buffer = self.buffer;
        let mut used = 0;
        let mut found = 0;
        for code in codes {
            let after = used + self.code_bits;
            if after > self.count {
                break;
            }
            found += 1;
            used = after;
            *code = (buffer & u64::from(self.mask)) as Code;
            buffer >>= self.code_bits;
        }
        found
    }

    fn consume(&mut self, codes: usize) {
        let bits = self.code_bits * codes as u8;
        assert!(bits <= self.count);
        self.buffer = if bits >= self.count {
            0
        } else {
            self.buffer >> bits
        };
        self.count -= bits;
    }

    const fn bump(&mut self) {
        self.code_bits += 1;
        self.mask = (self.mask << 1) | 1;
    }
}

struct Burst {
    codes: [Code; BURST_CODES],
    lengths: [u16; BURST_CODES],
    firsts: [u8; BURST_CODES],
    targets: [(usize, usize); BURST_CODES],
}

impl Burst {
    const fn new() -> Self {
        Self {
            codes: [0; BURST_CODES],
            lengths: [0; BURST_CODES],
            firsts: [0; BURST_CODES],
            targets: [(0, 0); BURST_CODES],
        }
    }
}

struct Pending {
    bytes: Box<[u8; TABLE_ENTRIES_MAX]>,
    read: usize,
    write: usize,
}

impl Pending {
    fn fill_repeat(&mut self) -> u8 {
        self.bytes[self.write] = self.bytes[0];
        self.write += 1;
        self.read = 0;
        self.bytes[0]
    }

    fn fill_reconstruct(&mut self, table: &Table, code: Code) -> u8 {
        let depth = usize::from(table.depths[usize::from(code)]);
        let last = table.reconstruct(code, &mut self.bytes[..depth]);
        self.read = 0;
        self.write = depth;
        last
    }

    fn keep(&mut self, tail: &[u8]) {
        self.bytes[..tail.len()].copy_from_slice(tail);
        self.write = tail.len();
        self.read = tail.len();
    }
}

pub(crate) struct Lzw {
    code_bits_min: u8,
    table: Table,
    pending: Pending,
    last: Option<Derivation>,
    clear_code: Code,
    end_code: Code,
    next_code: Code,
    ended: bool,
    bits: Bits,
}

impl Lzw {
    pub(crate) fn new(code_bits_min: u8) -> Self {
        assert!(code_bits_min <= CODE_BITS_MAX);
        Self {
            code_bits_min,
            table: Table {
                links: Vec::with_capacity(TABLE_ENTRIES_MAX),
                depths: Vec::with_capacity(TABLE_ENTRIES_MAX),
            },
            pending: Pending {
                bytes: Box::new([0; TABLE_ENTRIES_MAX]),
                read: 0,
                write: 0,
            },
            last: None,
            clear_code: 1 << code_bits_min,
            end_code: (1 << code_bits_min) + 1,
            next_code: (1 << code_bits_min) + 2,
            ended: false,
            bits: Bits::new(code_bits_min),
        }
    }

    pub(crate) const fn code_bits_min(&self) -> u8 {
        self.code_bits_min
    }

    pub(crate) const fn ended(&self) -> bool {
        self.ended
    }

    pub(crate) fn reset(&mut self) {
        self.table.init(self.code_bits_min);
        self.next_code = (1 << self.code_bits_min) + 2;
        self.pending.read = 0;
        self.pending.write = 0;
        self.last = None;
        self.ended = false;
        self.bits = Bits::new(self.code_bits_min);
    }

    fn init_tables(&mut self) {
        self.bits.reset(self.code_bits_min);
        self.next_code = (1 << self.code_bits_min) + 2;
        self.table.init(self.code_bits_min);
    }

    fn reset_tables(&mut self) {
        self.bits.reset(self.code_bits_min);
        self.next_code = (1 << self.code_bits_min) + 2;
        self.table.clear(self.code_bits_min);
    }

    fn start(
        &mut self,
        input: &mut &[u8],
        status: &mut Result<Status, InvalidCode>,
    ) -> Option<Derivation> {
        let code = match self.bits.next(input) {
            None => {
                *status = Ok(Status::NoProgress);
                return None;
            }
            Some(code) if code >= self.next_code => {
                *status = Err(InvalidCode);
                return None;
            }
            Some(code) => code,
        };
        if code == self.clear_code {
            self.init_tables();
            return None;
        }
        if code == self.end_code {
            self.ended = true;
            *status = Ok(Status::Done);
            return None;
        }
        if self.table.links.is_empty() {
            self.init_tables();
        }
        self.pending.fill_reconstruct(&self.table, code);
        Some(Derivation {
            code,
            first: self.table.links[usize::from(code)].first,
        })
    }

    pub(crate) fn decode(&mut self, mut input: &[u8], output: &mut [u8]) -> Decoded {
        if self.ended {
            return Decoded {
                read: 0,
                written: 0,
                status: Ok(Status::Done),
            };
        }
        let input_bytes = input.len();
        let mut status = Ok(Status::Progress);
        let mut link = match self.last.take() {
            None => self.start(&mut input, &mut status),
            Some(link) => Some(link),
        };
        let mut position = 0usize;
        let mut undecoded = false;
        if link.is_some() {
            let pending = &self.pending.bytes[self.pending.read..self.pending.write];
            let room = output.len() - position;
            if pending.len() > room {
                if room == 0 {
                    status = Ok(Status::NoProgress);
                } else {
                    output[position..].copy_from_slice(&pending[..room]);
                    self.pending.read += room;
                    position = output.len();
                }
            } else if pending.is_empty() {
                status = Ok(Status::NoProgress);
                undecoded = true;
            } else {
                let count = pending.len();
                output[position..position + count].copy_from_slice(pending);
                self.pending.read += count;
                position += count;
            }
        }
        let mut last_decoded: Option<(usize, usize)> = None;
        if self.pending.read == self.pending.write {
            (link, last_decoded, position) =
                self.burst_loop(&mut input, output, link, position, undecoded, &mut status);
        }
        if let Some((start, length)) = last_decoded {
            let tail = &output[start..start + length];
            self.pending.keep(tail);
        }
        if input_bytes > input.len() && status == Ok(Status::NoProgress) {
            status = Ok(Status::Progress);
        }
        self.last = link;
        Decoded {
            read: input_bytes - input.len(),
            written: position,
            status,
        }
    }

    fn burst_loop(
        &mut self,
        input: &mut &[u8],
        output: &mut [u8],
        mut link: Option<Derivation>,
        mut position: usize,
        mut undecoded: bool,
        status: &mut Result<Status, InvalidCode>,
    ) -> (Option<Derivation>, Option<(usize, usize)>, usize) {
        let mut last_decoded: Option<(usize, usize)> = None;
        let mut burst = Burst::new();
        let iterations_max = input.len() * 8 + usize::from(BIT_BUFFER_BITS) + 1;
        let mut iterations = 0usize;
        loop {
            iterations += 1;
            assert!(
                iterations <= iterations_max,
                "every burst consumes at least one code"
            );
            let Some(mut derivation) = link.take() else {
                break;
            };
            self.bits.refill(input);
            let available = self.bits.peek(&mut burst.codes);
            if available == 0 {
                if undecoded {
                    *status = Ok(Status::NoProgress);
                }
                link = Some(derivation);
                break;
            }
            let size;
            (size, position) = self.scan_burst(&mut burst, available, output.len(), position);
            self.bits.consume(size);
            undecoded = false;
            let new_code = burst.codes[size - 1];
            self.decode_burst(output, &mut derivation, &mut burst, size - 1);
            if self.burst_terminal(new_code, status) {
                last_decoded = None;
                break;
            }
            let repeat = new_code == self.next_code;
            if repeat && size > 1 {
                last_decoded = Some(burst.targets[size - 2]);
            }
            let first;
            let in_pending;
            (first, in_pending, position) = self.decode_new_code(
                output,
                derivation,
                new_code,
                repeat,
                position,
                &mut last_decoded,
            );
            self.grow_table(derivation, first);
            link = Some(Derivation {
                code: new_code,
                first,
            });
            if in_pending || position == output.len() {
                break;
            }
        }
        (link, last_decoded, position)
    }

    fn scan_burst(
        &self,
        burst: &mut Burst,
        available: usize,
        output_len: usize,
        mut position: usize,
    ) -> (usize, usize) {
        let mut size = 0usize;
        let left_before_switch = self.bits.mask.saturating_sub(self.next_code);
        for &code in &burst.codes[..available] {
            if size > 0 {
                let length = usize::from(burst.lengths[size - 1]);
                burst.targets[size - 1] = (position, length);
                position += length;
            }
            size += 1;
            if size > usize::from(left_before_switch) {
                break;
            }
            if code == self.clear_code || code == self.end_code || code >= self.next_code {
                break;
            }
            let length = self.table.depths[usize::from(code)];
            if output_len - position < usize::from(length) {
                break;
            }
            burst.lengths[size - 1] = length;
        }
        (size, position)
    }

    fn decode_burst(
        &mut self,
        output: &mut [u8],
        derivation: &mut Derivation,
        burst: &mut Burst,
        count: usize,
    ) {
        let burst_codes = &burst.codes[..count];
        if !self.table.is_full() {
            self.next_code += burst_codes.len() as Code;
        }
        for ((&code, &(start, length)), first) in burst_codes
            .iter()
            .zip(&burst.targets)
            .zip(burst.firsts.iter_mut())
        {
            *first = self
                .table
                .reconstruct(code, &mut output[start..start + length]);
        }
        self.table
            .derive_burst(derivation, burst_codes, &burst.firsts[..count]);
    }

    fn burst_terminal(&mut self, new_code: Code, status: &mut Result<Status, InvalidCode>) -> bool {
        if new_code == self.clear_code {
            self.reset_tables();
            return true;
        }
        if new_code == self.end_code {
            self.ended = true;
            *status = Ok(Status::Done);
            return true;
        }
        if new_code > self.next_code {
            *status = Err(InvalidCode);
            return true;
        }
        false
    }

    fn decode_new_code(
        &mut self,
        output: &mut [u8],
        derivation: Derivation,
        new_code: Code,
        repeat: bool,
        position: usize,
        last_decoded: &mut Option<(usize, usize)>,
    ) -> (u8, bool, usize) {
        let required = usize::from(if repeat {
            self.table.depths[usize::from(derivation.code)] + 1
        } else {
            self.table.depths[usize::from(new_code)]
        });
        if required > output.len() - position {
            let first = if repeat {
                if let Some((start, length)) = last_decoded.take() {
                    self.pending.keep(&output[start..start + length]);
                }
                self.pending.fill_repeat()
            } else {
                *last_decoded = None;
                self.pending.fill_reconstruct(&self.table, new_code)
            };
            return (first, true, position);
        }
        let start = position;
        let first = if repeat {
            self.write_repeat(output, start, last_decoded.take())
        } else {
            self.table
                .reconstruct(new_code, &mut output[start..start + required])
        };
        *last_decoded = Some((start, required));
        (first, false, position + required)
    }

    fn write_repeat(&self, output: &mut [u8], start: usize, source: Option<(usize, usize)>) -> u8 {
        let source_length = match source {
            Some((source_start, length)) => {
                output.copy_within(source_start..source_start + length, start);
                length
            }
            None => {
                let length = self.pending.write;
                output[start..start + length].copy_from_slice(&self.pending.bytes[..length]);
                length
            }
        };
        let first = if source_length > 0 { output[start] } else { 0 };
        output[start + source_length] = first;
        first
    }

    fn grow_table(&mut self, derivation: Derivation, first: u8) {
        if !self.table.is_full() {
            self.table.derive(derivation, first);
            if self.next_code >= self.bits.mask && self.bits.code_bits < CODE_BITS_MAX {
                self.bits.bump();
            }
            self.next_code += 1;
        }
    }
}
