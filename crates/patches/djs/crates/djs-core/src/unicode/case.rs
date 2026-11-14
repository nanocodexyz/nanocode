//! Case mapping and folding (UnicodeData.txt simple mappings, CaseFolding.txt simple
//! folding, unconditional SpecialCasing.txt mappings), and the `Cased` / `Case_Ignorable`
//! properties that the Final_Sigma rule of `String.prototype.toLowerCase` needs.
//!
//! Context and language dependent mappings (Final_Sigma, Turkish, Lithuanian) are not in the
//! tables; Final_Sigma is the caller's job (see [`is_cased`] and [`is_case_ignorable`]).

use super::case_tables as t;
use super::in_table;

/// Finds the run containing `c` and returns its delta.
fn run_delta(runs: &[u32], deltas: &[i32], c: u32) -> Option<i32> {
    let idx = runs.partition_point(|&r| (r >> t::RUN_START_SHIFT) <= c);
    if idx == 0 {
        return None;
    }
    let r = runs[idx - 1];
    let start = r >> t::RUN_START_SHIFT;
    let stride = ((r >> t::RUN_LEN_BITS) & 1) + 1;
    let len = (r & ((1 << t::RUN_LEN_BITS) - 1)) + 1;
    let off = c - start;
    (off.is_multiple_of(stride) && off / stride < len).then(|| deltas[idx - 1])
}

fn apply(c: u32, delta: Option<i32>) -> u32 {
    match delta {
        Some(d) => c.wrapping_add_signed(d),
        None => c,
    }
}

/// Simple uppercase mapping (one code point to one code point).
pub fn to_upper_simple(c: u32) -> u32 {
    if c < 0x80 {
        return if (b'a' as u32..=b'z' as u32).contains(&c) {
            c - 0x20
        } else {
            c
        };
    }
    apply(c, run_delta(&t::UPPER_RUNS, &t::UPPER_DELTAS, c))
}

/// Simple lowercase mapping.
pub fn to_lower_simple(c: u32) -> u32 {
    if c < 0x80 {
        return if (b'A' as u32..=b'Z' as u32).contains(&c) {
            c + 0x20
        } else {
            c
        };
    }
    apply(c, run_delta(&t::LOWER_RUNS, &t::LOWER_DELTAS, c))
}

/// Simple case folding (`scf`, the C and S entries of CaseFolding.txt).
pub fn simple_fold(c: u32) -> u32 {
    if c < 0x80 {
        return to_lower_simple(c);
    }
    match run_delta(&t::FOLD_EXCEPTIONS_RUNS, &t::FOLD_EXCEPTIONS_DELTAS, c) {
        Some(d) => c.wrapping_add_signed(d),
        None => to_lower_simple(c),
    }
}

/// The result of a full case mapping: one to three code points.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CaseMapping {
    len: u8,
    chars: [u32; 3],
}

impl CaseMapping {
    fn one(c: u32) -> CaseMapping {
        CaseMapping {
            len: 1,
            chars: [c, 0, 0],
        }
    }

    fn special(e: &[u16; 4]) -> CaseMapping {
        let len = if e[3] != 0 { 3 } else { 2 };
        CaseMapping {
            len,
            chars: [e[1] as u32, e[2] as u32, e[3] as u32],
        }
    }

    pub fn as_slice(&self) -> &[u32] {
        &self.chars[..self.len as usize]
    }

    /// The single code point, if the mapping is not a multi-character one.
    pub fn single(&self) -> Option<u32> {
        (self.len == 1).then_some(self.chars[0])
    }
}

fn special(table: &[[u16; 4]], c: u32) -> Option<&[u16; 4]> {
    if c > 0xFFFF {
        return None;
    }
    table
        .binary_search_by_key(&(c as u16), |e| e[0])
        .ok()
        .map(|i| &table[i])
}

/// Full uppercase mapping (unconditional SpecialCasing, else the simple mapping).
pub fn to_upper_full(c: u32) -> CaseMapping {
    match special(&t::SPECIAL_UPPER, c) {
        Some(e) => CaseMapping::special(e),
        None => CaseMapping::one(to_upper_simple(c)),
    }
}

/// Full lowercase mapping, without the context-dependent Final_Sigma rule.
pub fn to_lower_full(c: u32) -> CaseMapping {
    match special(&t::SPECIAL_LOWER, c) {
        Some(e) => CaseMapping::special(e),
        None => CaseMapping::one(to_lower_simple(c)),
    }
}

/// Unicode `Cased`.
pub fn is_cased(c: u32) -> bool {
    if c < 0x80 {
        return (c as u8).is_ascii_alphabetic();
    }
    in_table(&t::CASED, c)
}

/// Unicode `Case_Ignorable`.
pub fn is_case_ignorable(c: u32) -> bool {
    if c < 0x80 {
        // ' . : ^ ` (Case_Ignorable via Word_Break MidLetter/MidNumLet and Sk/Lm).
        return matches!(c, 0x27 | 0x2E | 0x3A | 0x5E | 0x60);
    }
    in_table(&t::CASE_IGNORABLE, c)
}

/// The code points covered by a run table, in increasing order (runs have disjoint spans).
struct RunCodePoints<'a> {
    runs: &'a [u32],
    run: usize,
    k: u32,
}

impl Iterator for RunCodePoints<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let &r = self.runs.get(self.run)?;
        let start = r >> t::RUN_START_SHIFT;
        let stride = ((r >> t::RUN_LEN_BITS) & 1) + 1;
        let len = (r & ((1 << t::RUN_LEN_BITS) - 1)) + 1;
        let c = start + self.k * stride;
        self.k += 1;
        if self.k == len {
            self.k = 0;
            self.run += 1;
        }
        Some(c)
    }
}

fn run_code_points(runs: &[u32]) -> core::iter::Peekable<RunCodePoints<'_>> {
    RunCodePoints { runs, run: 0, k: 0 }.peekable()
}

/// Calls `f(c, to_upper_simple(c))` for every code point that has a simple uppercase
/// mapping, in increasing order.
pub fn for_each_upper(mut f: impl FnMut(u32, u32)) {
    for c in b'a' as u32..=b'z' as u32 {
        f(c, c - 0x20);
    }
    for c in run_code_points(&t::UPPER_RUNS) {
        let m = to_upper_simple(c);
        if m != c {
            f(c, m);
        }
    }
}

/// Calls `f(c, simple_fold(c))` for every code point with `simple_fold(c) != c`, in
/// increasing order.
pub fn for_each_fold(mut f: impl FnMut(u32, u32)) {
    for c in b'A' as u32..=b'Z' as u32 {
        f(c, c + 0x20);
    }
    // Merge the lowercase mappings with the fold exceptions (both sorted).
    let mut a = run_code_points(&t::LOWER_RUNS);
    let mut b = run_code_points(&t::FOLD_EXCEPTIONS_RUNS);
    loop {
        let c = match (a.peek(), b.peek()) {
            (None, None) => return,
            (Some(&x), Some(&y)) if x == y => {
                b.next();
                a.next().unwrap()
            }
            (Some(&x), Some(&y)) if x < y => a.next().unwrap(),
            (Some(_), None) => a.next().unwrap(),
            _ => b.next().unwrap(),
        };
        let m = simple_fold(c);
        if m != c {
            f(c, m);
        }
    }
}
