use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use nanocode_durability_core::{Sql, SqlStatement};
use nanocode_executor::{sleep, spawn, timeout};
use nanocode_store_sql::PgSql;
use nanocode_sync::future::join_all;
use nanocode_turnlog::parted::place;
use serde_json::{Value, json};

use crate::{
    Options,
    http::{Conn, Response},
    pg, runtime,
};

const LOCKS: &str = "SELECT CAST((SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock') AS TEXT), CAST((SELECT count(*) FROM pg_locks WHERE NOT granted) AS TEXT)";
const WAITING: &str = "SELECT left(query, 160), CAST(count(*) AS TEXT) FROM pg_stat_activity WHERE wait_event_type = 'Lock' GROUP BY 1";
const RETRYABLE: [&str; 6] = [
    "unowned",
    "unreachable",
    "not_owner",
    "draining",
    "lost",
    "busy",
];
const KEYS: u64 = 8;
const STABLE_POLLS: u32 = 5;

enum Event {
    Kill {
        pid: String,
        victim: String,
        at: Duration,
    },
    Handoff {
        victim: String,
        at: Duration,
    },
}

struct Plan {
    replicas: Vec<String>,
    route: bool,
    owners: Vec<AtomicUsize>,
    route_every: Duration,
    parts: NonZeroU32,
    concurrency: u64,
    scopes: u64,
    seconds: u64,
    writes: u64,
    retry: Duration,
    pause: Duration,
    sample: Duration,
    ready: Duration,
    event: Option<Event>,
}

#[derive(Clone, Copy)]
struct Ack {
    scope: u64,
    key: u64,
    id: u64,
    seq: u64,
    at_us: u64,
}

#[derive(Default)]
struct Tally {
    appends: u64,
    reads: u64,
    latencies_us: Vec<u64>,
    errors: BTreeMap<String, u64>,
    retries: BTreeMap<String, u64>,
    acks: Vec<Ack>,
}

#[derive(Default)]
struct Sampled {
    samples: u64,
    failed: u64,
    lock_waits: u64,
    lock_waits_max: u64,
    ungranted: u64,
    ungranted_max: u64,
    waiting: BTreeMap<String, u64>,
}

struct Happened {
    kind: &'static str,
    at_us: u64,
    moved: Vec<u32>,
    answer: Value,
}

struct Rng(u64);

impl Rng {
    const fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

fn at(text: &str) -> Result<(String, Duration), String> {
    let (what, seconds) = text
        .rsplit_once('@')
        .ok_or_else(|| format!("{text} is not WHAT@SECONDS"))?;
    let seconds: f64 = seconds
        .parse()
        .map_err(|_| format!("{text}: bad seconds"))?;
    Ok((what.to_owned(), Duration::from_secs_f64(seconds)))
}

fn planned(options: &Options) -> Result<Plan, String> {
    let replicas: Vec<String> = options
        .text("replicas")
        .ok_or("drive needs replicas=HOST:PORT,...")?
        .split(',')
        .map(str::to_owned)
        .collect();
    let event = match (options.text("kill"), options.text("handoff")) {
        (Some(kill), None) => {
            let (pid, at) = at(kill)?;
            let victim = options
                .text("victim")
                .ok_or("kill needs victim=HOST:PORT")?
                .to_owned();
            Some(Event::Kill { pid, victim, at })
        }
        (None, Some(handoff)) => {
            let (victim, at) = at(handoff)?;
            Some(Event::Handoff { victim, at })
        }
        (None, None) => None,
        (Some(_), Some(_)) => return Err("one of kill= and handoff= per run".to_owned()),
    };
    let millis = |key: &str, default: u64| options.number(key, default).map(Duration::from_millis);
    let parts = options.parts()?;
    let route = match options.text("route") {
        None | Some("any") => false,
        Some("owner") => true,
        Some(other) => return Err(format!("route={other}: expected any or owner")),
    };
    Ok(Plan {
        replicas,
        route,
        owners: (0..parts.get()).map(|_| AtomicUsize::new(usize::MAX)).collect(),
        route_every: millis("route_ms", 100)?,
        parts,
        concurrency: options.number("concurrency", 20)?.max(1),
        scopes: options.number("scopes", 10_000)?.max(1),
        seconds: options.number("seconds", 30)?,
        writes: options.number("writes", 50)?,
        retry: millis("retry_ms", 10_000)?,
        pause: millis("pause_ms", 5)?,
        sample: millis("sample_ms", 100)?,
        ready: Duration::from_secs(options.number("ready_s", 60)?),
        event,
    })
}

pub fn run(admin: &str, options: &Options) -> Result<(), String> {
    let plan = Arc::new(planned(options)?);
    let admin = pg(admin, 1)?;
    runtime(options.number("threads", 2)?)?.block_on(async move {
        ready(&plan).await?;
        let stop = Arc::new(AtomicBool::new(false));
        let sampler = spawn(sample(Arc::clone(&admin), Arc::clone(&stop), plan.sample));
        if plan.route {
            route(&plan).await;
            spawn(routing(Arc::clone(&plan), Arc::clone(&stop)));
        }
        let started = Instant::now();
        let until = started + Duration::from_secs(plan.seconds);
        let event = spawn(happen(Arc::clone(&plan), started));
        let workers: Vec<_> = (0..plan.concurrency)
            .map(|index| spawn(work(Arc::clone(&plan), index, started, until)))
            .collect();
        let mut tally = Tally::default();
        for worker in workers {
            merge(&mut tally, worker.await.map_err(|error| format!("worker: {error}"))?);
        }
        let elapsed = started.elapsed();
        stop.store(true, Ordering::Relaxed);
        let sampled = sampler.await.map_err(|error| format!("sampler: {error}"))?;
        let happened = event.await.map_err(|error| format!("event: {error}"))??;
        let alive: Vec<String> = plan
            .replicas
            .iter()
            .filter(|replica| !matches!(&plan.event, Some(Event::Kill { victim, .. }) if victim == *replica))
            .cloned()
            .collect();
        let verified = verify(&plan, &tally.acks, &alive).await;
        let stats = stats(&alive, plan.retry).await;
        println!("{}", report(&plan, &tally, elapsed, &sampled, happened.as_ref(), &verified, &stats));
        Ok(())
    })
}

fn merge(total: &mut Tally, tally: Tally) {
    total.appends += tally.appends;
    total.reads += tally.reads;
    total.latencies_us.extend(tally.latencies_us);
    total.acks.extend(tally.acks);
    for (kind, count) in tally.errors {
        *total.errors.entry(kind).or_default() += count;
    }
    for (kind, count) in tally.retries {
        *total.retries.entry(kind).or_default() += count;
    }
}

async fn get(address: &str, path: &str, bound: Duration) -> Result<Value, String> {
    let answer = timeout(bound, async {
        let mut conn = Conn::connect(address).await?;
        conn.call("GET", path, false, b"").await
    })
    .await
    .map_err(|_| format!("{address}{path}: no answer within {bound:?}"))?;
    let response = answer.map_err(|error| format!("{address}: {error}"))?;
    serde_json::from_slice(&response.body).map_err(|error| format!("{address}{path}: {error}"))
}

fn held(value: &Value) -> Vec<u32> {
    value["held"]
        .as_array()
        .map(|held| {
            held.iter()
                .filter_map(|pair| pair[0].as_u64())
                .filter_map(|part| u32::try_from(part).ok())
                .collect()
        })
        .unwrap_or_default()
}

async fn route(plan: &Plan) {
    let answers = join_all(
        plan.replicas
            .iter()
            .map(|replica| get(replica, "/admin/held", plan.retry)),
    )
    .await;
    let mut owners = vec![usize::MAX; plan.owners.len()];
    for (index, answer) in answers.iter().enumerate() {
        let Ok(answer) = answer else { continue };
        if answer["draining"].as_bool() == Some(true) {
            continue;
        }
        for part in held(answer) {
            if let Some(owner) = owners.get_mut(part as usize) {
                *owner = index;
            }
        }
    }
    for (slot, owner) in plan.owners.iter().zip(owners) {
        slot.store(owner, Ordering::Relaxed);
    }
}

async fn routing(plan: Arc<Plan>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        sleep(plan.route_every).await;
        route(&plan).await;
    }
}

async fn ready(plan: &Plan) -> Result<(), String> {
    let deadline = Instant::now() + plan.ready;
    let (mut last, mut stable, mut why) = (Vec::new(), 0, String::new());
    while Instant::now() < deadline {
        let answers = join_all(
            plan.replicas
                .iter()
                .map(|replica| get(replica, "/admin/held", plan.retry)),
        )
        .await;
        let mut seen: Vec<(u32, usize)> = Vec::new();
        for (index, answer) in answers.iter().enumerate() {
            if let Ok(answer) = answer {
                seen.extend(held(answer).into_iter().map(|part| (part, index)));
            }
        }
        seen.sort_unstable();
        let covered = seen.len() == plan.parts.get() as usize
            && seen
                .iter()
                .map(|(part, _)| *part)
                .collect::<BTreeSet<_>>()
                .len()
                == seen.len()
            && answers.iter().all(Result::is_ok);
        why = answers
            .iter()
            .map(|answer| match answer {
                Ok(answer) => format!("{} held {}", answer["node"], held(answer).len()),
                Err(error) => error.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        stable = if covered && seen == last {
            stable + 1
        } else {
            0
        };
        if stable >= STABLE_POLLS {
            return Ok(());
        }
        last = seen;
        sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "partitions not settled within {:?}: {why}",
        plan.ready
    ))
}

async fn sample(admin: Arc<PgSql>, stop: Arc<AtomicBool>, every: Duration) -> Sampled {
    let mut sampled = Sampled::default();
    while !stop.load(Ordering::Relaxed) {
        let answer = admin
            .transaction(
                "scoped_api_sample",
                &[
                    SqlStatement {
                        sql: LOCKS,
                        params: Vec::new(),
                    },
                    SqlStatement {
                        sql: WAITING,
                        params: Vec::new(),
                    },
                ],
            )
            .await
            .unwrap_or_default();
        for cells in answer.get(1).into_iter().flatten() {
            if let (Some(Some(query)), Some(Some(count))) = (cells.first(), cells.get(1)) {
                *sampled.waiting.entry(query.clone()).or_default() +=
                    count.parse::<u64>().unwrap_or_default();
            }
        }
        let cells = answer
            .into_iter()
            .next()
            .and_then(|rows| rows.into_iter().next());
        let number = |cells: &[Option<String>], index: usize| {
            cells
                .get(index)
                .and_then(Option::as_deref)
                .and_then(|text| text.parse::<u64>().ok())
        };
        match cells
            .as_deref()
            .map(|cells| (number(cells, 0), number(cells, 1)))
        {
            Some((Some(waits), Some(ungranted))) => {
                sampled.samples += 1;
                sampled.lock_waits += waits;
                sampled.lock_waits_max = sampled.lock_waits_max.max(waits);
                sampled.ungranted += ungranted;
                sampled.ungranted_max = sampled.ungranted_max.max(ungranted);
            }
            _ => sampled.failed += 1,
        }
        sleep(every).await;
    }
    sampled
}

async fn happen(plan: Arc<Plan>, started: Instant) -> Result<Option<Happened>, String> {
    let Some(event) = &plan.event else {
        return Ok(None);
    };
    let (victim, offset) = match event {
        Event::Kill { victim, at, .. } | Event::Handoff { victim, at } => (victim.clone(), *at),
    };
    nanocode_executor::sleep_until(started + offset).await;
    let moved = held(&get(&victim, "/admin/held", plan.retry).await?);
    let at_us = micros(started.elapsed());
    let (kind, answer) = match event {
        Event::Kill { pid, .. } => {
            let status = std::process::Command::new("kill")
                .args(["-9", pid])
                .status()
                .map_err(|error| format!("kill {pid}: {error}"))?;
            ("kill", json!({"killed": status.success()}))
        }
        Event::Handoff { .. } => {
            let mut conn = Conn::connect(&victim)
                .await
                .map_err(|error| format!("{victim}: {error}"))?;
            let response = conn
                .call("POST", "/admin/drain", false, b"")
                .await
                .map_err(|error| format!("drain {victim}: {error}"))?;
            (
                "handoff",
                serde_json::from_slice(&response.body)
                    .map_err(|error| format!("drain answer: {error}"))?,
            )
        }
    };
    Ok(Some(Happened {
        kind,
        at_us,
        moved,
        answer,
    }))
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn kind(response: &Response) -> String {
    serde_json::from_slice::<Value>(&response.body)
        .ok()
        .and_then(|body| body["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("http_{}", response.status))
}

async fn call(
    conn: &mut Option<Conn>,
    plan: &Plan,
    target: &mut usize,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Response, String> {
    let replicas = plan.replicas.len();
    if conn.is_none() {
        match Conn::connect(&plan.replicas[*target]).await {
            Ok(connected) => *conn = Some(connected),
            Err(_) => {
                *target = (*target + 1) % replicas;
                return Err("connect".to_owned());
            }
        }
    }
    let Some(open) = conn.as_mut() else {
        return Err("connect".to_owned());
    };
    match timeout(plan.retry, open.call(method, path, false, body)).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => {
            *conn = None;
            *target = (*target + 1) % replicas;
            Err("io".to_owned())
        }
        Err(_) => {
            *conn = None;
            Err("timeout".to_owned())
        }
    }
}

async fn attempt(
    conn: &mut Option<Conn>,
    plan: &Plan,
    target: &mut usize,
    tally: &mut Tally,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Response, String> {
    let begun = Instant::now();
    loop {
        let refused = match call(conn, plan, target, method, path, body).await {
            Ok(response) if response.status == 200 => return Ok(response),
            Ok(response) => kind(&response),
            Err(kind) if kind == "connect" => kind,
            Err(kind) => return Err(kind),
        };
        if !(RETRYABLE.contains(&refused.as_str()) || refused == "connect")
            || begun.elapsed() >= plan.retry
        {
            return Err(refused);
        }
        *tally.retries.entry(refused).or_default() += 1;
        sleep(plan.pause).await;
    }
}

fn body(key: u64, id: u64) -> String {
    format!("{{\"k\":\"k{key}\",\"v\":\"{id}\"}}")
}

fn reroute(
    plan: &Plan,
    scope: u64,
    target: &mut usize,
    conn: &mut Option<Conn>,
    conns: &mut [Option<Conn>],
) {
    let part = place(&format!("s{scope}"), plan.parts).0 as usize;
    let owner = plan.owners[part].load(Ordering::Relaxed);
    if owner != usize::MAX && owner != *target {
        conns[*target] = conn.take();
        *target = owner;
        *conn = conns[*target].take();
    }
}

async fn work(plan: Arc<Plan>, index: u64, started: Instant, until: Instant) -> Tally {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (index + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9));
    let mut target = usize::try_from(index).unwrap_or_default() % plan.replicas.len();
    let (mut conn, mut tally, mut made) = (None, Tally::default(), 0);
    let mut conns: Vec<Option<Conn>> = plan.replicas.iter().map(|_| None).collect();
    while Instant::now() < until {
        let scope = rng.below(plan.scopes);
        if plan.route {
            reroute(&plan, scope, &mut target, &mut conn, &mut conns);
        }
        let write = rng.below(100) < plan.writes;
        let begun = Instant::now();
        if write {
            let (key, id) = (rng.below(KEYS), (index << 40) | made);
            made += 1;
            let path = format!("/scope/s{scope}/append");
            match attempt(
                &mut conn,
                &plan,
                &mut target,
                &mut tally,
                "POST",
                &path,
                body(key, id).as_bytes(),
            )
            .await
            {
                Ok(response) => {
                    let seq = serde_json::from_slice::<Value>(&response.body)
                        .ok()
                        .and_then(|body| body["seq"].as_u64());
                    let Some(seq) = seq else {
                        *tally.errors.entry("bad_ack".to_owned()).or_default() += 1;
                        continue;
                    };
                    tally.appends += 1;
                    tally.latencies_us.push(micros(begun.elapsed()));
                    tally.acks.push(Ack {
                        scope,
                        key,
                        id,
                        seq,
                        at_us: micros(started.elapsed()),
                    });
                }
                Err(kind) => *tally.errors.entry(kind).or_default() += 1,
            }
        } else {
            match attempt(
                &mut conn,
                &plan,
                &mut target,
                &mut tally,
                "GET",
                &format!("/scope/s{scope}"),
                b"",
            )
            .await
            {
                Ok(_) => {
                    tally.reads += 1;
                    tally.latencies_us.push(micros(begun.elapsed()));
                }
                Err(kind) => *tally.errors.entry(kind).or_default() += 1,
            }
        }
    }
    tally
}

#[derive(Default)]
struct Verified {
    acked: u64,
    scopes: u64,
    missing: u64,
    mismatched: u64,
    gaps: u64,
    duplicates: u64,
    unreadable: u64,
}

impl Verified {
    const fn add(&mut self, other: &Self) {
        self.scopes += other.scopes;
        self.missing += other.missing;
        self.mismatched += other.mismatched;
        self.gaps += other.gaps;
        self.unreadable += other.unreadable;
    }
}

async fn verify(plan: &Plan, acks: &[Ack], alive: &[String]) -> Verified {
    let mut by_scope: BTreeMap<u64, Vec<Ack>> = BTreeMap::new();
    let mut verified = Verified {
        acked: acks.len() as u64,
        ..Verified::default()
    };
    let mut seen = BTreeSet::new();
    for ack in acks {
        if !seen.insert((ack.scope, ack.seq)) {
            verified.duplicates += 1;
        }
        by_scope.entry(ack.scope).or_default().push(*ack);
    }
    let scopes: Vec<(u64, Vec<Ack>)> = by_scope.into_iter().collect();
    let lanes = usize::try_from(plan.concurrency).unwrap_or(1);
    let chunk = scopes.len().div_ceil(lanes).max(1);
    let checked = join_all(
        scopes
            .chunks(chunk)
            .enumerate()
            .map(|(lane, chunk)| check(plan, alive, lane, chunk)),
    )
    .await;
    for part in &checked {
        verified.add(part);
    }
    verified
}

async fn check(plan: &Plan, alive: &[String], lane: usize, scopes: &[(u64, Vec<Ack>)]) -> Verified {
    let replicas = Plan {
        replicas: alive.to_vec(),
        route: false,
        owners: Vec::new(),
        route_every: plan.route_every,
        parts: plan.parts,
        concurrency: 1,
        scopes: 0,
        seconds: 0,
        writes: 0,
        retry: plan.retry,
        pause: plan.pause,
        sample: plan.sample,
        ready: plan.ready,
        event: None,
    };
    let (mut conn, mut target, mut tally) = (None, lane % alive.len().max(1), Tally::default());
    let mut verified = Verified::default();
    for (scope, acks) in scopes {
        verified.scopes += 1;
        let rows = match attempt(
            &mut conn,
            &replicas,
            &mut target,
            &mut tally,
            "GET",
            &format!("/scope/s{scope}/log"),
            b"",
        )
        .await
        {
            Ok(response) => serde_json::from_slice::<Vec<(u64, String)>>(&response.body).ok(),
            Err(_) => None,
        };
        let Some(rows) = rows else {
            verified.unreadable += 1;
            continue;
        };
        verified.gaps += rows
            .iter()
            .enumerate()
            .filter(|(index, (seq, _))| *seq != *index as u64 + 1)
            .count() as u64;
        let bodies: BTreeMap<u64, &str> = rows
            .iter()
            .map(|(seq, body)| (*seq, body.as_str()))
            .collect();
        for ack in acks {
            match bodies.get(&ack.seq) {
                None => verified.missing += 1,
                Some(found) if *found != body(ack.key, ack.id) => verified.mismatched += 1,
                Some(_) => {}
            }
        }
    }
    verified
}

async fn stats(alive: &[String], bound: Duration) -> Value {
    let answers = join_all(
        alive
            .iter()
            .map(|replica| get(replica, "/admin/stats", bound)),
    )
    .await;
    let mut total: BTreeMap<String, u64> = BTreeMap::new();
    for answer in answers.into_iter().flatten() {
        if let Some(fields) = answer.as_object() {
            for (key, value) in fields {
                *total.entry(key.clone()).or_default() += value.as_u64().unwrap_or_default();
            }
        }
    }
    json!(total)
}

fn percentile(sorted: &[u64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index] as f64 / 1000.0
}

fn stalls(plan: &Plan, acks: &[Ack], happened: &Happened, end_us: u64) -> (Vec<f64>, f64) {
    let moved: BTreeSet<u32> = happened.moved.iter().copied().collect();
    let mut times: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for ack in acks.iter().filter(|ack| ack.at_us >= happened.at_us) {
        let part = place(&format!("s{}", ack.scope), plan.parts).0;
        times.entry(part).or_default().push(ack.at_us);
    }
    let mut moved_ms = Vec::new();
    let mut unmoved_ms: f64 = 0.0;
    for part in 0..plan.parts.get() {
        let mut points = times.remove(&part).unwrap_or_default();
        points.sort_unstable();
        let mut previous = happened.at_us;
        let mut gap = 0;
        for point in points.iter().copied().chain(std::iter::once(end_us)) {
            gap = gap.max(point.saturating_sub(previous));
            previous = point;
        }
        let gap_ms = gap as f64 / 1000.0;
        if moved.contains(&part) {
            moved_ms.push(gap_ms);
        } else {
            unmoved_ms = unmoved_ms.max(gap_ms);
        }
    }
    moved_ms.sort_by(f64::total_cmp);
    (moved_ms, unmoved_ms)
}

fn report(
    plan: &Plan,
    tally: &Tally,
    elapsed: Duration,
    sampled: &Sampled,
    happened: Option<&Happened>,
    verified: &Verified,
    stats: &Value,
) -> Value {
    let mut latencies = tally.latencies_us.clone();
    latencies.sort_unstable();
    let seconds = elapsed.as_secs_f64();
    let event = happened.map(|happened| {
        let (moved_ms, unmoved_ms) = stalls(plan, &tally.acks, happened, micros(elapsed));
        json!({
            "kind": happened.kind,
            "at_s": happened.at_us as f64 / 1e6,
            "moved_parts": happened.moved.len(),
            "stall_ms_max": moved_ms.last().copied().unwrap_or_default(),
            "stall_ms_p50": moved_ms.get(moved_ms.len() / 2).copied().unwrap_or_default(),
            "unmoved_gap_ms_max": unmoved_ms,
            "answer": happened.answer,
        })
    });
    json!({
        "replicas": plan.replicas.len(),
        "concurrency": plan.concurrency,
        "parts": plan.parts.get(),
        "scopes": plan.scopes,
        "writes_pct": plan.writes,
        "seconds": seconds,
        "appends": tally.appends,
        "reads": tally.reads,
        "ops_per_s": (tally.appends + tally.reads) as f64 / seconds,
        "p50_ms": percentile(&latencies, 0.50),
        "p99_ms": percentile(&latencies, 0.99),
        "max_ms": percentile(&latencies, 1.0),
        "errors": tally.errors,
        "retries": tally.retries,
        "acked": verified.acked,
        "acked_scopes": verified.scopes,
        "acked_missing": verified.missing,
        "acked_mismatched": verified.mismatched,
        "acked_duplicates": verified.duplicates,
        "log_gaps": verified.gaps,
        "unreadable_scopes": verified.unreadable,
        "samples": sampled.samples,
        "samples_failed": sampled.failed,
        "lock_wait_sampled": sampled.lock_waits,
        "lock_wait_max": sampled.lock_waits_max,
        "ungranted_sampled": sampled.ungranted,
        "ungranted_max": sampled.ungranted_max,
        "lock_waiting_queries": sampled.waiting,
        "replica_stats": stats,
        "event": event,
    })
}
