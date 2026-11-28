use std::{
    collections::BTreeMap,
    fmt::Write as _,
    ops::{Range, RangeInclusive},
    process::ExitCode,
};

use crate::{
    rng::{FNV_OFFSET, Rng, fnv},
    tidy,
};

const CRATE: &str = "crates/owner/src";
const FILES: RangeInclusive<u64> = 1..=4;
const FUNCTIONS: RangeInclusive<u64> = 1..=6;
const SHAPES: RangeInclusive<u64> = 1..=4;
const OWNER: &str = "pub struct Owner {\n    order: Sequencer,\n    writer: Arc<Owned<State>>,\n    gate: Arc<Semaphore>,\n    capacity: Arc<Semaphore>,\n    store: Store,\n}\n\nimpl Owner {\n    pub fn new(store: Store, capacity: usize) -> Self {\n        Self {\n            order: Sequencer::new(),\n            writer: Arc::new(Owned::new(State::default())),\n            gate: Arc::new(Semaphore::new(1)),\n            capacity: Arc::new(Semaphore::new(capacity)),\n            store,\n        }\n    }\n}\n";
const EXCLUSIVE: &[&str] = &[
    "self.order.enter().await",
    "self.order.try_enter().ok_or(Busy)?",
    "self.writer.acquire().await",
    "self.writer.clone().acquire_owned().await",
    "self.gate.acquire().await.map_err(|_| Busy)?",
    "self.gate.clone().acquire_owned().await.map_err(|_| Busy)?",
];
const SHARED: &[&str] = &[
    "self.capacity.acquire().await.map_err(|_| Busy)?",
    "self.capacity.clone().acquire_owned().await.map_err(|_| Busy)?",
    "tracing::info_span!(\"step\").entered()",
];
const NAMES: &[&str] = &["turn", "held", "permit", "_turn", "_guard", "mut guard"];
const STORE_CALL: &str = "self.store.commit(n).await?;";
const WAITS: RangeInclusive<u64> = 1..=8;
const BARE_ACQUIRE: &str =
    "let permit = self.permits.clone().acquire_owned().await.map_err(|_| Busy)?;";
const GATES: RangeInclusive<u64> = 1..=8;
const MUTEX_GATE: &str = "let gate = Semaphore::new(1);";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldAwaitMutation {
    None,
    HeldAcrossAwait,
    WaitsUnbounded,
    MutexSemaphore,
}

impl HeldAwaitMutation {
    const ALL: [Self; 4] = [
        Self::None,
        Self::HeldAcrossAwait,
        Self::WaitsUnbounded,
        Self::MutexSemaphore,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::HeldAcrossAwait => "held_across_await",
            Self::WaitsUnbounded => "waits_unbounded",
            Self::MutexSemaphore => "mutex_semaphore",
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HeldAwaitStats {
    pub seeds: u64,
    pub files: u64,
    pub functions: u64,
    pub shapes: u64,
    pub held: u64,
    pub waits: u64,
    pub gates: u64,
}

impl HeldAwaitStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.files += other.files;
        self.functions += other.functions;
        self.shapes += other.shapes;
        self.held += other.held;
        self.waits += other.waits;
        self.gates += other.gates;
    }
}

fn draw(rng: &mut Rng, range: &RangeInclusive<u64>) -> u64 {
    rng.range(*range.start(), *range.end())
}

fn word<'a>(rng: &mut Rng, words: &[&'a str]) -> &'a str {
    words[rng.pick(words.len())]
}

fn bound(name: &str) -> &str {
    name.strip_prefix("mut ").unwrap_or(name)
}

fn shape(rng: &mut Rng, body: &mut String) -> u64 {
    let name = word(rng, NAMES);
    let guard = word(rng, EXCLUSIVE);
    let used = bound(name);
    let (text, held) = match rng.below(9) {
        0 => (
            format!("let {name} = {guard};\n{STORE_CALL}\nself.apply(&{used});\n"),
            1,
        ),
        1 => (
            format!("let {name} = {guard};\nself.apply(&{used});\ndrop({used});\n{STORE_CALL}\n"),
            0,
        ),
        2 => (
            format!("{{\n    let {name} = {guard};\n    self.apply(&{used});\n}}\n{STORE_CALL}\n"),
            0,
        ),
        3 => (format!("let _ = {guard};\n{STORE_CALL}\n"), 0),
        4 => (
            format!("{STORE_CALL}\nlet {name} = {guard};\nself.apply(&{used});\ndrop({used});\n"),
            0,
        ),
        5 => (
            format!(
                "let {name} = {};\n{STORE_CALL}\nself.apply(&{used});\n",
                word(rng, SHARED)
            ),
            0,
        ),
        6 => (
            format!(
                "if let Some({used}) = self.order.try_enter() {{\n    {STORE_CALL}\n    self.apply(&{used});\n}}\n"
            ),
            1,
        ),
        7 => (
            format!(
                "let {name} = {guard};\nif n > 0 {{\n    {STORE_CALL}\n}}\nself.apply(&{used});\n"
            ),
            1,
        ),
        _ => (
            format!(
                "let open = '{{';\n// let {name} = {guard}; {STORE_CALL}\nlet quoted = \"let {used} = self.order.enter().await; }} {{ .await\";\nlet raw = r#\"{{ \"let {used} = self.order.enter().await;\" }}\"#;\nself.note(open, quoted, raw);\n"
            ),
            0,
        ),
    };
    body.push_str(&text);
    held
}

fn wait(rng: &mut Rng) -> (&'static str, u64) {
    match rng.below(6) {
        0 => (BARE_ACQUIRE, 1),
        1 => (
            "let connection = Connection::connect(&self.options).await?;",
            1,
        ),
        2 => (
            "let permit = timeout(self.acquire_ms, self.permits.clone().acquire_owned()).await.map_err(|_| Busy)?;",
            0,
        ),
        3 => (
            "let connection = timeout(self.connect_ms, Connection::connect_reusing(&self.options, &self.reuse)).await.map_err(|_| Busy)??;",
            0,
        ),
        4 => ("let mut acquire = pin!(self.capacity.acquire());", 0),
        _ => (
            "// let permit = self.permits.acquire().await;\nlet text = \"self.permits.acquire().await\";",
            0,
        ),
    }
}

fn gate(rng: &mut Rng) -> (&'static str, u64) {
    match rng.below(9) {
        0 => (MUTEX_GATE, 1),
        1 => (
            "gate: Arc::new(nanocode_sync::Semaphore::new( 1usize )),",
            1,
        ),
        2 => ("let slots = Arc::new(Semaphore::new(capacity));", 0),
        3 => ("let slots = Semaphore::new(1 << 20);", 0),
        4 => ("let slots = Semaphore::new(1_000);", 0),
        5 => ("let slots = Semaphore::new(16);", 0),
        6 => ("let slots = Semaphore::new(sizes.permits.max(1));", 0),
        7 => ("// let gate = Semaphore::new(1);", 0),
        _ => ("let text = \"Semaphore::new(1)\";", 0),
    }
}

fn gates(rng: &mut Rng, mutation: HeldAwaitMutation, stats: &mut HeldAwaitStats) -> Option<String> {
    let path = format!("{CRATE}/gates.rs");
    let mut source = "pub fn gates(capacity: usize, sizes: Sizes) {\n".to_owned();
    let mut mutexes = 0;
    for _ in 0..draw(rng, &GATES) {
        let (text, count) = gate(rng);
        mutexes += count;
        let _ = writeln!(source, "    {text}");
    }
    if mutation == HeldAwaitMutation::MutexSemaphore {
        let _ = writeln!(source, "    {MUTEX_GATE}");
    }
    source.push_str("}\n");
    let problems = tidy::mutex_problems(&[(path.clone(), source)]);
    let found = u64::try_from(problems.len()).expect("a problem count fits a u64");
    stats.gates += found;
    (found != mutexes).then(|| {
        format!(
            "Semaphore::new(1): tidy reports {found} in {path}, the source builds {mutexes}: {}",
            problems.join("; ")
        )
    })
}

fn pool(rng: &mut Rng, mutation: HeldAwaitMutation) -> (String, u64) {
    let mut source =
        "impl Pool {\n    pub async fn acquire(&self) -> Result<Lent, Busy> {\n".to_owned();
    let mut bare = 0;
    for _ in 0..draw(rng, &WAITS) {
        let (text, count) = wait(rng);
        bare += count;
        for line in text.lines() {
            let _ = writeln!(source, "        {line}");
        }
    }
    if mutation == HeldAwaitMutation::WaitsUnbounded {
        let _ = writeln!(source, "        {BARE_ACQUIRE}");
    }
    source.push_str("        Ok(Lent::new(permit, connection))\n    }\n}\n");
    (source, bare)
}

fn function(rng: &mut Rng, index: u64, stats: &mut HeldAwaitStats) -> (String, u64) {
    let mut source = format!("pub async fn step_{index}(&self, n: u64) -> Result<(), Busy> {{\n");
    let mut held = 0;
    for _ in 0..draw(rng, &SHAPES) {
        let mut body = String::new();
        held += shape(rng, &mut body);
        stats.shapes += 1;
        for line in body.lines() {
            let _ = writeln!(source, "    {line}");
        }
    }
    source.push_str("    Ok(())\n}\n");
    (source, held)
}

fn sources(
    rng: &mut Rng,
    mutation: HeldAwaitMutation,
    stats: &mut HeldAwaitStats,
) -> (Vec<(String, String)>, BTreeMap<String, u64>) {
    let mut files = vec![(format!("{CRATE}/owner.rs"), OWNER.to_owned())];
    let mut truth = BTreeMap::new();
    let mut index = 0;
    for file in 0..draw(rng, &FILES) {
        let path = format!("{CRATE}/step_{file}.rs");
        let mut source = "impl Owner {\n".to_owned();
        let mut held = 0;
        for _ in 0..draw(rng, &FUNCTIONS) {
            let (text, count) = function(rng, index, stats);
            index += 1;
            held += count;
            source.push_str(&text);
            stats.functions += 1;
        }
        source.push_str("}\n");
        if held > 0 {
            truth.insert(path.clone(), held);
        }
        files.push((path, source));
        stats.files += 1;
    }
    if mutation == HeldAwaitMutation::HeldAcrossAwait {
        let at = 1 + rng.pick(files.len() - 1);
        let source = &mut files[at].1;
        source.truncate(source.len() - "}\n".len());
        let _ = write!(
            source,
            "pub async fn commit(&self, n: u64) -> Result<(), Busy> {{\n    let turn = self.order.enter().await;\n    {STORE_CALL}\n    drop(turn);\n    Ok(())\n}}\n}}\n"
        );
    }
    (files, truth)
}

fn waits(rng: &mut Rng, mutation: HeldAwaitMutation, stats: &mut HeldAwaitStats) -> Option<String> {
    let path = format!("{CRATE}/pool.rs");
    let (source, bare) = pool(rng, mutation);
    let found = u64::try_from(tidy::bare_waits(&tidy::code_only(&source)).len())
        .expect("a wait count fits a u64");
    stats.waits += found;
    if mutation != HeldAwaitMutation::WaitsUnbounded && found != bare {
        return Some(format!(
            "detector found {found} bare waits in {path}, the pool has {bare}"
        ));
    }
    let allowed = if bare > 0 {
        format!("{path} {bare}\n")
    } else {
        String::new()
    };
    let problems = tidy::wait_problems(&[(path, source)], &allowed);
    (!problems.is_empty()).then(|| format!("I6 waits unbounded: {}", problems.join("; ")))
}

fn run_seed(seed: u64, mutation: HeldAwaitMutation) -> (u64, Option<String>, HeldAwaitStats) {
    let mut rng = Rng::derive(seed, "held_await");
    let mut stats = HeldAwaitStats {
        seeds: 1,
        ..HeldAwaitStats::default()
    };
    let (files, truth) = sources(&mut rng, mutation, &mut stats);
    let mut found: BTreeMap<String, u64> = BTreeMap::new();
    for (path, _) in tidy::guards_held(&files) {
        *found.entry(path).or_default() += 1;
    }
    stats.held = found.values().sum();
    let digest = files
        .iter()
        .fold(FNV_OFFSET, |hash, (_, text)| fnv(hash, text.as_bytes()));
    if mutation != HeldAwaitMutation::HeldAcrossAwait && found != truth {
        return (
            digest,
            Some(format!(
                "detector found {found:?}, the owners hold {truth:?}"
            )),
            stats,
        );
    }
    let problems = tidy::held_problems(&files);
    let held: u64 = truth.values().sum();
    let reported = u64::try_from(problems.len()).expect("a problem count fits a u64");
    let wrong = (reported != held)
        .then(|| format!("I3 held across await: tidy reports {reported} guards where the owners hold {held}: {}", problems.join("; ")))
        .or_else(|| waits(&mut rng, mutation, &mut stats))
        .or_else(|| gates(&mut rng, mutation, &mut stats));
    (digest, wrong, stats)
}

pub fn judge(
    seed: u64,
    mutation: HeldAwaitMutation,
    twice: bool,
    verbose: bool,
    totals: &mut HeldAwaitStats,
) -> Option<String> {
    let (digest, wrong, stats) = run_seed(seed, mutation);
    totals.add(&stats);
    if verbose {
        println!("seed {seed} digest {digest:016x} stats={stats:?}");
    }
    if wrong.is_some() || !twice {
        return wrong;
    }
    let (again, wrong, _) = run_seed(seed, mutation);
    if again != digest {
        return Some(format!(
            "nondeterminism: digest {digest:016x} then {again:016x}"
        ));
    }
    wrong
}

pub fn replay(seed: u64, mutation: HeldAwaitMutation) -> String {
    let mut command = format!("nanocode-sim --scenario held_await --seed {seed}");
    if mutation != HeldAwaitMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: HeldAwaitMutation,
    totals: &HeldAwaitStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=held_await mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        HeldAwaitMutation::None if failed == 0 => ExitCode::SUCCESS,
        HeldAwaitMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}
