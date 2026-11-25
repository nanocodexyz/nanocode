const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64, iteration: u64, target: &str) -> Self {
        let mut state = Self(seed);
        let mixed = state.next() ^ iteration.wrapping_mul(GOLDEN);
        let named = target.bytes().fold(FNV_OFFSET, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
        });
        state.0 = mixed ^ named;
        state.next();
        state
    }

    pub const fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GOLDEN);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub const fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0);
        self.next() % bound
    }

    pub const fn range(&mut self, low: u64, high: u64) -> u64 {
        assert!(low < high);
        low + self.below(high - low)
    }

    pub const fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        assert!(numerator <= denominator);
        self.below(denominator) < numerator
    }

    pub fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.below(len as u64)).expect("an index below a usize length fits")
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.index(items.len())]
    }

    pub fn word(&mut self, words: &[&str]) -> String {
        (*self.pick(words)).to_owned()
    }

    pub fn text(&mut self, max: u64) -> String {
        const PIECES: [&str; 12] = [
            "a",
            "b",
            "z",
            "0",
            " ",
            "@",
            "/",
            ":",
            "\"",
            "\\",
            "\u{e9}",
            "\u{1f600}",
        ];
        let len = self.below(max + 1);
        (0..len)
            .map(|_| match self.below(16) {
                0 => char::from_u32(u32::try_from(self.below(0x20)).expect("a control fits"))
                    .expect("a control character is a char")
                    .to_string(),
                _ => (*self.pick(&PIECES)).to_owned(),
            })
            .collect()
    }

    pub fn bytes(&mut self, max: u64) -> Vec<u8> {
        let len = self.below(max + 1);
        (0..len).map(|_| self.next().to_le_bytes()[0]).collect()
    }

    pub fn mutate(&mut self, text: &str) -> String {
        String::from_utf8_lossy(&self.mutate_bytes(text.as_bytes())).into_owned()
    }

    pub fn mutate_bytes(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut bytes = bytes.to_vec();
        for _ in 0..self.range(1, 4) {
            if bytes.is_empty() {
                bytes.push(self.next().to_le_bytes()[0]);
                continue;
            }
            let at = self.index(bytes.len());
            match self.below(6) {
                0 => bytes[at] ^= 1 << self.below(8),
                1 => bytes.truncate(at),
                2 => {
                    bytes.remove(at);
                }
                3 => bytes.insert(at, *self.pick(b"{}[]\":,0-e.\\nul ")),
                4 => {
                    let end = at + self.index(bytes.len() - at);
                    let copy = bytes[at..=end].to_vec();
                    bytes.splice(at..at, copy);
                }
                _ => bytes[at] = self.next().to_le_bytes()[0],
            }
        }
        bytes
    }
}
