use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_partitions::{Partition, Row};
use nanocode_sync::{Atom, OrdMap, OrdSet, Slot, Vector, oneshot, watch};
use nanocode_turnlog::{LogHost, SCAN_END, Stamps};

use crate::{
    Adopted, Carry, Committed, Flushed, Part, Policy, Resident, ScopeError,
    checkpoint::{Checkpoints, Listing, Manifest, digest},
    lease::Leases,
    meter::{Logs, Meter, Tally, Usage},
    owner::{Ask, Chain, Checkpointed, Core, Life, Owner, Placed, Turn, Why},
    route::Route,
    sim::{self, Mutation},
};

pub type ClaimHook =
    Arc<dyn Fn(Arc<str>) -> StoreFuture<'static, Result<(), StoreError>> + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub residents: u64,
    pub resident_bytes: u64,
    pub loads: u64,
    pub load_rows: u64,
    pub load_rows_max: u64,
    pub load_parts: u64,
    pub commits: u64,
    pub rows: u64,
    pub checkpoints: u64,
    pub checkpoint_parts: u64,
    pub checkpoint_parts_kept: u64,
    pub checkpoint_part_bytes: u64,
    pub checkpoint_listing_bytes: u64,
    pub checkpoint_listings_whole: u64,
    pub checkpoint_failures: u64,
    pub adoptions: u64,
    pub adoption_resumes: u64,
    pub adoption_rows: u64,
    pub adoption_bytes: u64,
    pub evictions: u64,
    pub over_budget: u64,
    pub releases: u64,
    pub lost: u64,
    pub forwards: u64,
    pub forward_ms_total: u64,
    pub forward_ms_max: u64,
    pub forwards_busy: u64,
    pub forwards_unreachable: u64,
    pub usage: Usage,
    pub admissions: u64,
    pub admissions_busy: u64,
    pub release_drops: u64,
    pub unadopted: u64,
    pub backfill_reads: u64,
    pub backfill_failures: u64,
    pub serving_queued: u64,
    pub backfill_lanes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Backfilled {
    pub adopted: u64,
    pub resumed: u64,
    pub current: u64,
    pub dropped: u64,
    pub failed: Vec<(String, ScopeError)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Due {
    part: Partition,
    epoch: u64,
}

struct Quiet {
    tally: Arc<Tally>,
    seen: u64,
}

enum Backfill {
    Adopted,
    Resumed,
    Current,
    Dropped,
}

pub struct Wiring<F: Resident> {
    pub fold: Arc<F>,
    pub log: Arc<dyn LogHost>,
    pub leases: Arc<dyn Leases>,
    pub checkpoints: Arc<dyn Checkpoints>,
    pub route: Arc<dyn Route<F>>,
    pub policy: Policy,
    pub clock: Clock,
    pub claimed: Option<ClaimHook>,
}

type Waiter<V> = Arc<Slot<oneshot::Sender<Result<V, ScopeError>>>>;

struct Loaded<S> {
    state: S,
    next: u64,
    checkpointed: Checkpointed,
    parts: u64,
    tail: Vec<(u64, String)>,
    adopted: Option<(u64, u64)>,
    raced: bool,
}

struct Flights<V>(Atom<OrdMap<Arc<str>, Vector<Waiter<V>>>>);

enum Flight<V> {
    Lead,
    Wait(oneshot::Receiver<Result<V, ScopeError>>),
}

struct Flying<'a, V: Clone> {
    flights: &'a Flights<V>,
    key: Arc<str>,
}

impl<V: Clone> Drop for Flying<'_, V> {
    fn drop(&mut self) {
        self.flights.0.update(|flights| flights.remove(&self.key));
    }
}

impl<V: Clone + Send + 'static> Flights<V> {
    fn new() -> Self {
        Self(Atom::default())
    }

    async fn run<W: Future<Output = Result<V, ScopeError>>>(
        &self,
        key: &str,
        work: impl FnOnce() -> W,
    ) -> Result<V, ScopeError> {
        let key: Arc<str> = Arc::from(key);
        let (sender, receiver) = oneshot::channel();
        let waiter: Waiter<V> = Arc::new(Slot::new(Some(sender)));
        let lead = self.0.update(|flights| match flights.get(&key) {
            Some(waiting) => {
                let mut waiting = waiting.clone();
                waiting.push_back(Arc::clone(&waiter));
                flights.insert(Arc::clone(&key), waiting);
                false
            }
            None => {
                flights.insert(Arc::clone(&key), Vector::new());
                true
            }
        });
        let flight = if lead {
            Flight::Lead
        } else {
            Flight::Wait(receiver)
        };
        match flight {
            Flight::Wait(receiver) => receiver.await.unwrap_or(Err(ScopeError::Moved)),
            Flight::Lead => {
                let flying = Flying {
                    flights: self,
                    key: Arc::clone(&key),
                };
                let answer = work().await;
                let waiting = self
                    .0
                    .update(|flights| flights.remove(&key))
                    .unwrap_or_default();
                std::mem::forget(flying);
                for waiter in &waiting {
                    if let Some(sender) = waiter.take() {
                        drop(sender.send(answer.clone()));
                    }
                }
                answer
            }
        }
    }
}

pub(crate) struct Inner<F: Resident> {
    pub(crate) fold: Arc<F>,
    pub(crate) logs: Logs,
    pub(crate) leases: Arc<dyn Leases>,
    pub(crate) meter: Arc<Meter>,
    route: Arc<dyn Route<F>>,
    pub(crate) policy: Policy,
    pub(crate) clock: Clock,
    claimed: Option<ClaimHook>,
    residents: Atom<OrdMap<Arc<str>, Arc<Owner<F>>>>,
    loads: Flights<Arc<Owner<F>>>,
    claims: Flights<u64>,
    backfills: Atom<OrdMap<Arc<str>, Due>>,
    quiet: Atom<OrdMap<Arc<str>, Arc<Quiet>>>,
    running: Atom<OrdSet<Arc<str>>>,
    unreachable: Atom<BTreeSet<String>>,
    tick: AtomicU64,
    pub(crate) stats: Atom<Stats>,
}

pub struct Scopes<F: Resident> {
    inner: Arc<Inner<F>>,
}

impl<F: Resident> Clone for Scopes<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub struct Scope<F: Resident> {
    inner: Arc<Inner<F>>,
    owner: Arc<Owner<F>>,
}

impl<F: Resident> Clone for Scope<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            owner: Arc::clone(&self.owner),
        }
    }
}

pub struct Routed<F: Resident> {
    inner: Arc<Inner<F>>,
    scope: Arc<str>,
    holder: String,
}

impl<F: Resident> Clone for Routed<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            scope: Arc::clone(&self.scope),
            holder: self.holder.clone(),
        }
    }
}

pub enum Reached<F: Resident> {
    Local(Scope<F>),
    Routed(Routed<F>),
}

impl<F: Resident> Reached<F> {
    pub async fn send(&self, command: F::Command) -> Result<Committed<F::Reply>, ScopeError> {
        match self {
            Self::Local(scope) => scope.send(command).await,
            Self::Routed(routed) => routed.send(command).await,
        }
    }

    pub async fn stage(&self, command: F::Command) -> Result<Committed<F::Reply>, ScopeError> {
        match self {
            Self::Local(scope) => scope.stage(command).await,
            Self::Routed(routed) => routed.send(command).await,
        }
    }

    pub async fn flush(&self, carry: Option<Carry>) -> Result<Flushed, ScopeError> {
        match self {
            Self::Local(scope) => scope.flush(carry).await,
            Self::Routed(_) => Ok(Flushed {
                next: None,
                carried: false,
            }),
        }
    }
}

impl<F: Resident> Scope<F> {
    pub fn name(&self) -> &str {
        &self.owner.scope
    }

    pub fn epoch(&self) -> u64 {
        self.owner.epoch
    }

    pub fn retired(&self) -> bool {
        matches!(self.owner.life(), Life::Lost | Life::Released { .. })
    }

    pub fn next(&self) -> u64 {
        self.owner.next()
    }

    pub fn state(&self) -> Arc<F::State> {
        #[cfg(not(target_family = "wasm"))]
        if sim::mutated(Mutation::ReadThroughWriter) {
            return match nanocode_sync::block_on(self.owner.ask(Ask::Read, &self.inner)) {
                Turn::Read(state) => state,
                _ => self.owner.state(),
            };
        }
        self.owner.state()
    }

    pub fn watch(&self) -> watch::Receiver<Arc<F::State>> {
        self.owner.watch()
    }

    pub fn usage(&self) -> Usage {
        self.owner.tally.usage()
    }

    pub async fn send(&self, command: F::Command) -> Result<Committed<F::Reply>, ScopeError> {
        self.committed(command, Ask::Send).await
    }

    pub async fn stage(&self, command: F::Command) -> Result<Committed<F::Reply>, ScopeError> {
        self.committed(command, Ask::Stage).await
    }

    pub async fn flush(&self, carry: Option<Carry>) -> Result<Flushed, ScopeError> {
        self.owner.used.store(self.inner.touch(), Ordering::Relaxed);
        match self.owner.ask(Ask::Flush(carry), &self.inner).await {
            Turn::Flushed(flushed) => flushed,
            Turn::Committed(_)
            | Turn::Moved(_)
            | Turn::Read(_)
            | Turn::Checkpointed(_)
            | Turn::Drained(_) => {
                panic!("a flush of scope {} is answered as a flush", self.owner.scope)
            }
        }
    }

    async fn committed(
        &self,
        command: F::Command,
        ask: fn(F::Command) -> Ask<F>,
    ) -> Result<Committed<F::Reply>, ScopeError> {
        self.owner.used.store(self.inner.touch(), Ordering::Relaxed);
        match self.owner.ask(ask(command), &self.inner).await {
            Turn::Committed(committed) => committed,
            Turn::Moved(Some(command)) => match self.inner.get(&self.owner.scope).await? {
                Reached::Local(scope) => match scope.owner.ask(ask(command), &self.inner).await {
                    Turn::Committed(committed) => committed,
                    _ => Err(ScopeError::Moved),
                },
                Reached::Routed(routed) => routed.send(command).await,
            },
            Turn::Moved(None)
            | Turn::Read(_)
            | Turn::Flushed(_)
            | Turn::Checkpointed(_)
            | Turn::Drained(_) => {
                panic!("a send to scope {} is answered as a send", self.owner.scope)
            }
        }
    }
}

impl<F: Resident> Routed<F> {
    pub fn holder(&self) -> &str {
        &self.holder
    }

    pub async fn send(&self, command: F::Command) -> Result<Committed<F::Reply>, ScopeError> {
        if !self.inner.fold.routable(&command) {
            return Err(ScopeError::Routed {
                holder: self.holder.clone(),
            });
        }
        self.inner.forward(&self.holder, &self.scope, command).await
    }
}

impl<F: Resident> Scopes<F> {
    pub fn new(wiring: Wiring<F>) -> Self {
        assert!(
            wiring.policy.checkpoint_rows > 0,
            "a scope checkpoints after at least one row"
        );
        let (meter, logs) = Meter::new(wiring.log, wiring.checkpoints, wiring.policy.admit_ms);
        Self {
            inner: Arc::new(Inner {
                fold: wiring.fold,
                logs,
                leases: wiring.leases,
                meter,
                route: wiring.route,
                policy: wiring.policy,
                clock: wiring.clock,
                claimed: wiring.claimed,
                residents: Atom::default(),
                loads: Flights::new(),
                claims: Flights::new(),
                backfills: Atom::default(),
                quiet: Atom::default(),
                running: Atom::default(),
                unreachable: Atom::default(),
                tick: AtomicU64::new(0),
                stats: Atom::default(),
            }),
        }
    }

    pub async fn get(&self, scope: &str) -> Result<Reached<F>, ScopeError> {
        self.inner.get(scope).await
    }

    pub async fn serve(
        &self,
        scope: &str,
        command: F::Command,
    ) -> Result<Committed<F::Reply>, ScopeError> {
        let part = self.inner.leases.partition(scope);
        if self.inner.leases.held(part).is_none() {
            return Err(ScopeError::Moved);
        }
        match self.inner.get(scope).await? {
            Reached::Local(scope) => scope.send(command).await,
            Reached::Routed(_) => Err(ScopeError::Moved),
        }
    }

    pub async fn hold(&self, scope: &str) -> Result<u64, ScopeError> {
        let part = self.inner.leases.partition(scope);
        if let Some(epoch) = self.inner.leases.held(part) {
            return Ok(epoch);
        }
        match self.inner.claim(scope, part).await? {
            Ok(epoch) => Ok(epoch),
            Err(holder) => Err(ScopeError::Routed { holder }),
        }
    }

    pub async fn release(&self, part: Partition) -> Result<(), ScopeError> {
        self.inner.release(part).await
    }

    pub async fn drain(&self, scope: &str) -> Result<u64, ScopeError> {
        let Some(owner) = self
            .inner
            .residents
            .read(|residents| residents.get(scope).cloned())
        else {
            return Err(ScopeError::Moved);
        };
        self.inner.drain(&owner, Why::Release).await
    }

    pub async fn renew(&self) -> Result<Vec<Partition>, ScopeError> {
        let lost = self.inner.leases.renew().await?;
        for part in &lost {
            self.inner.forget(*part);
        }
        Ok(lost)
    }

    pub async fn tick(&self) -> Vec<(String, ScopeError)> {
        let owners: Vec<Arc<Owner<F>>> = self
            .inner
            .residents
            .read(|residents| residents.values().cloned().collect());
        let mut failed = Vec::new();
        for owner in owners {
            if let Turn::Checkpointed(Err(error)) = owner.ask(Ask::Checkpoint, &self.inner).await {
                failed.push((owner.scope.as_ref().to_owned(), error));
            }
        }
        failed
    }

    pub fn residents(&self) -> Vec<(String, u64, u64)> {
        self.inner.residents.read(|residents| {
            residents
                .iter()
                .map(|(scope, owner)| (scope.as_ref().to_owned(), owner.epoch, owner.next()))
                .collect()
        })
    }

    pub fn stats(&self) -> Stats {
        let (residents, bytes) = self.inner.weighed();
        let admitted = self.inner.meter.admission().admitted_so_far();
        Stats {
            residents,
            resident_bytes: bytes,
            usage: self.inner.meter.usage(),
            admissions: admitted.admissions,
            admissions_busy: admitted.busy,
            backfill_reads: admitted.backfill_reads,
            serving_queued: admitted.queued,
            backfill_lanes: admitted.lanes,
            ..self.inner.stats.read(|stats| *stats)
        }
    }

    pub fn history(&self) -> Arc<dyn LogHost> {
        Arc::clone(&self.inner.logs.history)
    }

    pub fn legacy(&self) -> Arc<dyn LogHost> {
        Arc::clone(&self.inner.logs.legacy)
    }

    pub fn read_capacity(&self) -> u32 {
        self.inner.meter.admission().capacity()
    }

    pub fn adopting(&self, scope: &str) -> bool {
        self.inner
            .backfills
            .read(|backfills| backfills.contains_key(scope))
    }

    pub async fn backfill(&self) -> Backfilled {
        self.inner.backfill().await
    }
}

impl<F: Resident> Inner<F> {
    fn touch(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed)
    }

    fn weighed(&self) -> (u64, u64) {
        self.residents.read(|residents| {
            let bytes: usize = residents.values().map(|owner| owner.weight()).sum();
            (residents.len() as u64, bytes as u64)
        })
    }

    fn resident(&self, scope: &str, part: Partition) -> Option<Arc<Owner<F>>> {
        let owner = self
            .residents
            .read(|residents| residents.get(scope).cloned())?;
        if owner.life() == Life::Serving && self.leases.held(part) == Some(owner.epoch) {
            return Some(owner);
        }
        if owner.life() != Life::Draining {
            self.evicted(&owner);
        }
        None
    }

    fn evicted(&self, owner: &Arc<Owner<F>>) {
        self.residents.update(|residents| {
            if residents
                .get(&owner.scope)
                .is_some_and(|kept| Arc::ptr_eq(kept, owner))
            {
                residents.remove(&owner.scope);
            }
        });
        self.meter.leave(&owner.scope, &owner.tally);
    }

    fn forget(&self, part: Partition) {
        let lost: Vec<Arc<Owner<F>>> = self.residents.read(|residents| {
            residents
                .values()
                .filter(|owner| owner.part == part)
                .cloned()
                .collect()
        });
        for owner in &lost {
            owner.lose();
            self.evicted(owner);
        }
        self.unqueue(part);
        self.stats.update(|stats| stats.lost += lost.len() as u64);
    }

    fn unqueue(&self, part: Partition) {
        self.backfills.update(|backfills| {
            let dropped: Vec<Arc<str>> = backfills
                .iter()
                .filter(|(_, due)| due.part == part)
                .map(|(scope, _)| Arc::clone(scope))
                .collect();
            for scope in &dropped {
                backfills.remove(scope);
            }
        });
        let unwatched: Vec<Arc<str>> = self.quiet.read(|quiet| {
            quiet
                .keys()
                .filter(|scope| {
                    !self
                        .backfills
                        .read(|backfills| backfills.contains_key(*scope))
                })
                .cloned()
                .collect()
        });
        for scope in &unwatched {
            self.unwatch(scope);
        }
    }

    fn watch(&self, scope: &str) {
        let name: Arc<str> = Arc::from(scope);
        let tally = self.meter.enter(&name);
        let quiet = Arc::new(Quiet { tally, seen: 0 });
        self.quiet
            .update(|watched| watched.insert(Arc::clone(&name), Arc::clone(&quiet)));
    }

    fn unwatch(&self, scope: &str) {
        if let Some(quiet) = self.quiet.update(|quiet| quiet.remove(scope)) {
            self.meter.leave(scope, &quiet.tally);
        }
    }

    fn quiet(&self, scope: &Arc<str>) -> bool {
        let Some(quiet) = self.quiet.read(|quiet| quiet.get(scope).cloned()) else {
            return true;
        };
        let now = quiet.tally.usage().legacy.calls;
        if now == quiet.seen {
            return true;
        }
        let seen = Arc::new(Quiet {
            tally: Arc::clone(&quiet.tally),
            seen: now,
        });
        self.quiet
            .update(|watched| watched.insert(Arc::clone(scope), Arc::clone(&seen)));
        false
    }

    async fn get(self: &Arc<Self>, scope: &str) -> Result<Reached<F>, ScopeError> {
        let part = self.leases.partition(scope);
        if let Some(owner) = self.resident(scope, part) {
            owner.used.store(self.touch(), Ordering::Relaxed);
            return Ok(self.local(owner));
        }
        let epoch = match self.leases.held(part) {
            Some(epoch) => epoch,
            None => match self.claim(scope, part).await? {
                Ok(epoch) => epoch,
                Err(holder) => {
                    return Ok(Reached::Routed(Routed {
                        inner: Arc::clone(self),
                        scope: Arc::from(scope),
                        holder,
                    }));
                }
            },
        };
        if self.due(scope, epoch) {
            self.stats.update(|stats| stats.unadopted += 1);
            return Err(ScopeError::Unadopted);
        }
        let owner = if sim::mutated(Mutation::ColdLoadDuplicated) {
            self.load(scope, part, epoch).await?
        } else {
            self.loads
                .run(scope, || self.load(scope, part, epoch))
                .await?
        };
        self.make_room(scope).await;
        Ok(self.local(owner))
    }

    fn local(self: &Arc<Self>, owner: Arc<Owner<F>>) -> Reached<F> {
        Reached::Local(Scope {
            inner: Arc::clone(self),
            owner,
        })
    }

    async fn claim(&self, scope: &str, part: Partition) -> Result<Result<u64, String>, ScopeError> {
        let row = self.leases.row(part).await?;
        let (other, live) = match &row {
            Row::Held {
                holder, expires_at, ..
            } if holder != self.leases.node() => (Some(holder.clone()), self.live(*expires_at)),
            Row::Held { .. } | Row::Released { .. } => (None, false),
        };
        if let Some(holder) = &other
            && live
            && !self.unreachable.read(|missed| missed.contains(holder))
        {
            return Ok(Err(holder.clone()));
        }
        let key = format!("{}", part.0);
        let claimed = self
            .claims
            .run(&key, || async {
                if let Some(epoch) = self.leases.held(part) {
                    return Ok(epoch);
                }
                self.leases.claim(part).await.map_err(ScopeError::from)
            })
            .await;
        match (claimed, other) {
            (Ok(epoch), _) => Ok(Ok(epoch)),
            (Err(ScopeError::Lost | ScopeError::Busy), Some(holder)) => Ok(Err(holder)),
            (Err(ScopeError::Lost), None) => Err(ScopeError::Busy),
            (Err(error), _) => {
                let _ = scope;
                Err(error)
            }
        }
    }

    fn live(&self, expires_at: u64) -> bool {
        sim::mutated(Mutation::RoutesPastExpiry) || (self.clock)() <= expires_at
    }

    async fn load(
        &self,
        scope: &str,
        part: Partition,
        epoch: u64,
    ) -> Result<Arc<Owner<F>>, ScopeError> {
        if let Some(owner) = self
            .residents
            .read(|residents| residents.get(scope).cloned())
            && owner.epoch == epoch
            && owner.life() == Life::Serving
        {
            return Ok(owner);
        }
        if let Some(hook) = &self.claimed {
            hook(Arc::from(scope)).await?;
        }
        let name: Arc<str> = Arc::from(scope);
        let tally = self.meter.enter(&name);
        let owner = match self
            .meter
            .admission()
            .admitted(self.read(scope, Due { part, epoch }))
            .await
        {
            Ok(loaded) => {
                let placed = Placed {
                    part,
                    epoch,
                    tally: Arc::clone(&tally),
                };
                self.resident_from(name, placed, loaded).await
            }
            Err(error) => Err(error),
        };
        if owner.is_err() {
            self.meter.leave(scope, &tally);
        }
        owner
    }

    async fn read(&self, scope: &str, due: Due) -> Result<Loaded<F::State>, ScopeError> {
        let manifest = if sim::mutated(Mutation::CheckpointIndexesOnly) {
            None
        } else {
            self.meter.latest(scope).await?
        };
        let now = (self.clock)();
        match manifest {
            Some(manifest) => {
                let (state, next, checkpointed, parts) = self.restore(scope, manifest, now).await?;
                let tail = self.logs.load.scan(scope, next, SCAN_END).await?;
                match tail.iter().find(|(_, row)| self.fold.foreign(row)) {
                    None => Ok(Loaded {
                        state,
                        next,
                        checkpointed,
                        parts,
                        tail,
                        adopted: None,
                        raced: false,
                    }),
                    Some((seq, _)) => self.unadopted(scope, due, now, Some(*seq)).await,
                }
            }
            None => self.unadopted(scope, due, now, None).await,
        }
    }

    fn due(&self, scope: &str, epoch: u64) -> bool {
        self.backfills
            .read(|backfills| backfills.get(scope).is_some_and(|due| due.epoch == epoch))
    }

    fn queue(&self, scope: &str, due: Due) -> ScopeError {
        let before = self
            .backfills
            .update(|backfills| backfills.insert(Arc::from(scope), due));
        if before.is_none() {
            self.watch(scope);
        }
        self.stats.update(|stats| stats.unadopted += 1);
        ScopeError::Unadopted
    }

    fn undue(&self, scope: &str, due: Due) {
        let removed = self.backfills.update(|backfills| {
            if backfills.get(scope) == Some(&due) {
                backfills.remove(scope);
                return true;
            }
            false
        });
        if removed {
            self.unwatch(scope);
        }
    }

    async fn unadopted(
        &self,
        scope: &str,
        due: Due,
        now: u64,
        foreign: Option<u64>,
    ) -> Result<Loaded<F::State>, ScopeError> {
        if sim::mutated(Mutation::AdoptInRequest) {
            if let Some(loaded) = self.adoption(scope, now, foreign, None).await? {
                return Ok(loaded);
            }
            return self.fresh(scope, due, now).await;
        }
        if foreign.is_some() || self.fold.adoption(&self.logs.load, scope).await? {
            return Err(self.queue(scope, due));
        }
        self.fresh(scope, due, now).await
    }

    async fn fresh(&self, scope: &str, due: Due, now: u64) -> Result<Loaded<F::State>, ScopeError> {
        let next = self.fold.first_row();
        let tail = self.logs.load.scan(scope, next, SCAN_END).await?;
        if tail.iter().any(|(_, row)| self.fold.foreign(row)) {
            return Err(self.queue(scope, due));
        }
        Ok(Loaded {
            state: self.fold.empty(),
            next,
            checkpointed: Checkpointed::empty(now),
            parts: 0,
            tail,
            adopted: None,
            raced: false,
        })
    }

    async fn backfill(&self) -> Backfilled {
        let mut tried: BTreeSet<Arc<str>> = BTreeSet::new();
        let mut backfilled = Backfilled::default();
        while let Some((scope, due)) = self.claim_due(&tried) {
            let outcome = self.backfill_one(&scope, due).await;
            self.running.update(|running| {
                running.remove(&scope);
            });
            match outcome {
                Ok(Backfill::Adopted) => backfilled.adopted += 1,
                Ok(Backfill::Resumed) => {
                    self.watch(&scope);
                    backfilled.resumed += 1;
                }
                Ok(Backfill::Current) => backfilled.current += 1,
                Ok(Backfill::Dropped) => backfilled.dropped += 1,
                Err(error) => {
                    if self.due(&scope, due.epoch) {
                        self.watch(&scope);
                    }
                    backfilled.failed.push((scope.as_ref().to_owned(), error));
                }
            }
            tried.insert(scope);
        }
        let failures = backfilled.failed.len() as u64;
        self.stats
            .update(|stats| stats.backfill_failures += failures);
        backfilled
    }

    fn claim_due(&self, tried: &BTreeSet<Arc<str>>) -> Option<(Arc<str>, Due)> {
        let capacity = usize::try_from(self.meter.admission().capacity())
            .expect("a read capacity fits in usize");
        let passes = usize::try_from(self.meter.admission().passes())
            .expect("a pass count fits in usize");
        let due: Vec<(Arc<str>, Due)> = self
            .backfills
            .read(|backfills| {
                backfills
                    .iter()
                    .filter(|(scope, _)| !tried.contains(*scope))
                    .map(|(scope, due)| (Arc::clone(scope), *due))
                    .collect::<Vec<_>>()
            })
            .into_iter()
            .filter(|(scope, _)| self.quiet(scope))
            .collect();
        let claimed = self.running.update(|running| {
            if running.len() >= passes {
                return None;
            }
            let next = due.iter().find(|(scope, _)| !running.contains(scope))?;
            running.insert(Arc::clone(&next.0));
            Some(next.clone())
        });
        assert!(
            self.running.read(OrdSet::len) <= capacity,
            "at most the read capacity of backfills run at once"
        );
        claimed
    }

    async fn backfill_one(&self, scope: &Arc<str>, due: Due) -> Result<Backfill, ScopeError> {
        if self.leases.held(due.part) != Some(due.epoch) {
            self.undue(scope, due);
            return Ok(Backfill::Dropped);
        }
        let tally = self.meter.enter(scope);
        let admission = self.meter.admission();
        let loaded = if sim::mutated(Mutation::BackfillTakesServingLanes) {
            admission.admitted(self.backfilled(scope)).await
        } else {
            self.backfilled(scope).await
        };
        let placed = Placed {
            part: due.part,
            epoch: due.epoch,
            tally: Arc::clone(&tally),
        };
        let landed = match loaded {
            Ok(Some(loaded)) if self.leases.held(due.part) == Some(due.epoch) && loaded.raced => {
                self.sealed(Arc::clone(scope), placed, loaded)
                    .await
                    .map(|()| Backfill::Resumed)
            }
            Ok(Some(loaded)) if self.leases.held(due.part) == Some(due.epoch) => self
                .resident_from(Arc::clone(scope), placed, loaded)
                .await
                .map(|_| Backfill::Adopted),
            Ok(Some(_)) => Ok(Backfill::Dropped),
            Ok(None) => Ok(Backfill::Current),
            Err(error) => Err(error),
        };
        if !matches!(landed, Ok(Backfill::Adopted)) {
            self.meter.leave(scope, &tally);
        }
        if matches!(
            landed,
            Ok(Backfill::Adopted | Backfill::Current | Backfill::Dropped)
        ) {
            self.undue(scope, due);
        }
        landed
    }

    async fn backfilled(&self, scope: &str) -> Result<Option<Loaded<F::State>>, ScopeError> {
        let admission = self.meter.admission();
        let now = (self.clock)();
        let Some(manifest) = admission.backfilled(self.meter.latest(scope)).await? else {
            return self.adoption(scope, now, None, None).await;
        };
        let tail = self.logs.adopt.scan(scope, manifest.seq, SCAN_END).await?;
        let Some((foreign, _)) = tail.iter().find(|(_, row)| self.fold.foreign(row)) else {
            return Ok(None);
        };
        let foreign = *foreign;
        drop(tail);
        if sim::mutated(Mutation::BackfillRereadsHistory) {
            return self.adoption(scope, now, Some(foreign), None).await;
        }
        let at = manifest.seq;
        let (checkpointed, next, _, _) = admission
            .backfilled(self.restore(scope, manifest, now))
            .await?;
        assert_eq!(
            next, at,
            "a restored checkpoint of {scope} resumes at its manifest"
        );
        assert!(
            foreign >= at,
            "a foreign row of {scope} lies past its checkpoint"
        );
        self.adoption(scope, now, Some(foreign), Some(checkpointed))
            .await
    }

    async fn sealed(
        &self,
        scope: Arc<str>,
        placed: Placed,
        loaded: Loaded<F::State>,
    ) -> Result<(), ScopeError> {
        assert!(
            loaded.raced,
            "only a raced adoption of {scope} is sealed aside"
        );
        assert!(
            loaded.tail.is_empty(),
            "a raced adoption of {scope} seals the state it read, without the rows past it"
        );
        let Some((rows, bytes)) = loaded.adopted else {
            panic!("a raced adoption of {scope} counts what it read");
        };
        let weight = self.fold.weight(&loaded.state);
        let core = Core {
            state: Arc::new(loaded.state),
            next: loaded.next,
            since: 0,
            checkpointed: loaded.checkpointed,
            staged: None,
            stamps: Stamps::new(placed.epoch),
        };
        let owner = Owner::new(scope, placed, core, weight, self.touch());
        owner.seal(self).await?;
        self.stats.update(|stats| {
            stats.adoption_resumes += 1;
            stats.adoption_rows += rows;
            stats.adoption_bytes += bytes;
        });
        Ok(())
    }

    async fn resident_from(
        &self,
        scope: Arc<str>,
        placed: Placed,
        loaded: Loaded<F::State>,
    ) -> Result<Arc<Owner<F>>, ScopeError> {
        let Loaded {
            mut state,
            next,
            checkpointed,
            parts,
            tail,
            adopted,
            raced,
        } = loaded;
        assert!(
            !raced,
            "a raced adoption of {scope} is sealed aside, never served"
        );
        let mut at = next;
        for (seq, row) in &tail {
            if *seq < at {
                return Err(ScopeError::Store(StoreError::Backend(format!(
                    "the log answered row {seq} of {scope} below {at}"
                ))));
            }
            self.fold.apply(&mut state, *seq, row)?;
            at = seq + 1;
        }
        self.stats.update(|stats| {
            stats.loads += 1;
            stats.load_rows += tail.len() as u64;
            stats.load_rows_max = stats.load_rows_max.max(tail.len() as u64);
            stats.load_parts += parts;
        });
        let weight = self.fold.weight(&state);
        let core = Core {
            state: Arc::new(state),
            next: at,
            since: tail.len(),
            checkpointed,
            staged: None,
            stamps: Stamps::new(placed.epoch),
        };
        let owner = Arc::new(Owner::new(scope, placed, core, weight, self.touch()));
        if let Some((rows, bytes)) = adopted {
            if !sim::mutated(Mutation::AdoptionSkipsCheckpoint) {
                owner.seal(self).await?;
            }
            self.stats.update(|stats| {
                stats.adoptions += 1;
                stats.adoption_rows += rows;
                stats.adoption_bytes += bytes;
            });
        }
        self.residents
            .update(|residents| residents.insert(Arc::clone(&owner.scope), Arc::clone(&owner)));
        Ok(owner)
    }

    async fn adoption(
        &self,
        scope: &str,
        now: u64,
        foreign: Option<u64>,
        checkpointed: Option<F::State>,
    ) -> Result<Option<Loaded<F::State>>, ScopeError> {
        let adopted = match checkpointed {
            Some(state) => self.fold.resume(&self.logs.adopt, scope, state).await?,
            None => self.fold.adopt(&self.logs.adopt, scope).await?,
        };
        let Adopted {
            state,
            next,
            rows,
            bytes,
        } = match (adopted, foreign) {
            (None, None) => return self.plain(scope, now).await.map(Some),
            (None, Some(seq)) => {
                return Err(ScopeError::Store(StoreError::Backend(format!(
                    "row {seq} of {scope} past its checkpoint was written outside the scope, and its log adopts nothing"
                ))));
            }
            (Some(adopted), _) => adopted,
        };
        assert!(next > 0, "an adopted history of {scope} ends past row 0");
        let tail = self.logs.adopt.scan(scope, next, SCAN_END).await?;
        let raced = tail.iter().any(|(_, row)| self.fold.foreign(row));
        if raced && sim::mutated(Mutation::BackfillRereadsHistory) {
            return Err(ScopeError::Unadopted);
        }
        Ok(Some(Loaded {
            state,
            next,
            checkpointed: Checkpointed::empty(now),
            parts: 0,
            tail: if raced { Vec::new() } else { tail },
            adopted: Some((rows, bytes)),
            raced,
        }))
    }

    async fn plain(&self, scope: &str, now: u64) -> Result<Loaded<F::State>, ScopeError> {
        let next = self.fold.first_row();
        let tail = self.logs.adopt.scan(scope, next, SCAN_END).await?;
        let bytes: usize = tail.iter().map(|(_, row)| row.len()).sum();
        Ok(Loaded {
            state: self.fold.empty(),
            next,
            checkpointed: Checkpointed::empty(now),
            parts: 0,
            adopted: Some((tail.len() as u64, bytes as u64)),
            tail,
            raced: false,
        })
    }

    async fn restore(
        &self,
        scope: &str,
        manifest: Manifest,
        now: u64,
    ) -> Result<(F::State, u64, Checkpointed, u64), ScopeError> {
        if manifest.scope != scope {
            return Err(ScopeError::Store(StoreError::Backend(format!(
                "the checkpoint store answered a checkpoint of {} for {scope}",
                manifest.scope
            ))));
        }
        let digests: BTreeSet<String> = manifest.parts.iter().map(|(_, at)| at.clone()).collect();
        let wanted: Vec<String> = digests.iter().cloned().collect();
        let found = self.meter.parts(scope, &wanted).await?;
        let mut bodies: OrdMap<String, String> = OrdMap::default();
        for (at, body) in found {
            if digest(&body) != at {
                return Err(ScopeError::Store(StoreError::Backend(format!(
                    "checkpoint part {at} of {scope} holds a body of another digest"
                ))));
            }
            bodies.insert(at, body);
        }
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for (name, at) in &manifest.parts {
            let Some(body) = bodies.get(at) else {
                return Err(ScopeError::Store(StoreError::Backend(format!(
                    "checkpoint of {scope} at {} names part {name} whose body {at} is missing",
                    manifest.seq
                ))));
            };
            parts.push(Part {
                name: name.clone(),
                body: body.clone(),
            });
        }
        let state = self.fold.decode(parts)?;
        let whole_bytes = Listing::Whole(manifest.parts.clone()).encoded().len();
        let checkpointed = Checkpointed {
            next: manifest.seq,
            at_ms: now,
            parts: manifest.parts.into_iter().collect(),
            chain: Some(Chain {
                base: manifest.seq,
                whole_bytes,
                changed_bytes: manifest.changed_bytes,
            }),
        };
        Ok((state, manifest.seq, checkpointed, wanted.len() as u64))
    }

    async fn drain(&self, owner: &Arc<Owner<F>>, why: Why) -> Result<u64, ScopeError> {
        let drained = match owner.ask(Ask::Drain(why), self).await {
            Turn::Drained(drained) => drained,
            Turn::Moved(_) => Err(ScopeError::Moved),
            Turn::Committed(_) | Turn::Read(_) | Turn::Flushed(_) | Turn::Checkpointed(_) => {
                panic!("a drain of scope {} is answered as a drain", owner.scope)
            }
        };
        if drained.is_ok() {
            self.evicted(owner);
        }
        drained
    }

    async fn release(&self, part: Partition) -> Result<(), ScopeError> {
        let owners: Vec<Arc<Owner<F>>> = self.residents.read(|residents| {
            residents
                .values()
                .filter(|owner| owner.part == part)
                .cloned()
                .collect()
        });
        let stops = sim::mutated(Mutation::ReleaseStopsAtDrain);
        let mut dropped = 0;
        for owner in &owners {
            let drained = if stops {
                self.drain(owner, Why::Release).await.map(drop)
            } else {
                self.released(owner, part).await
            };
            match drained {
                Ok(()) => {}
                Err(error) if stops => return Err(error),
                Err(_) => dropped += 1,
            }
        }
        self.unqueue(part);
        self.leases.release(part).await?;
        self.stats.update(|stats| {
            stats.releases += 1;
            stats.release_drops += dropped;
        });
        Ok(())
    }

    async fn released(&self, owner: &Arc<Owner<F>>, part: Partition) -> Result<(), ScopeError> {
        let owner = match owner.life() {
            Life::Serving | Life::Draining => Arc::clone(owner),
            Life::Released { .. } => {
                self.evicted(owner);
                return Ok(());
            }
            Life::Lost => {
                self.evicted(owner);
                if self.leases.held(part) != Some(owner.epoch) {
                    return Ok(());
                }
                self.load(&owner.scope, part, owner.epoch).await?
            }
        };
        if let Err(error) = self.drain(&owner, Why::Release).await {
            owner.lose();
            self.evicted(&owner);
            return Err(error);
        }
        Ok(())
    }

    async fn make_room(&self, keep: &str) {
        let count = self.residents.read(OrdMap::len);
        for _ in 0..count {
            let (bytes, victim) = self.residents.read(|residents| {
                let bytes: usize = residents.values().map(|owner| owner.weight()).sum();
                let victim = residents
                    .values()
                    .filter(|owner| {
                        owner.scope.as_ref() != keep
                            && owner.idle()
                            && owner.life() == Life::Serving
                    })
                    .min_by_key(|owner| owner.used.load(Ordering::Relaxed))
                    .cloned();
                (bytes, victim)
            });
            if bytes <= self.policy.resident_bytes_max {
                return;
            }
            let Some(victim) = victim else {
                self.stats.update(|stats| stats.over_budget += 1);
                return;
            };
            if self.drain(&victim, Why::Evict).await.is_err() {
                self.stats.update(|stats| stats.over_budget += 1);
                return;
            }
            self.stats.update(|stats| stats.evictions += 1);
        }
    }

    async fn forward(
        &self,
        holder: &str,
        scope: &str,
        command: F::Command,
    ) -> Result<Committed<F::Reply>, ScopeError> {
        let started = nanocode_platform::now();
        let forwarded = self.route.forward(holder, scope, command);
        let answer = if sim::mutated(Mutation::ForwardWaitsUnbounded) {
            forwarded.await
        } else {
            match nanocode_platform::timeout(
                Duration::from_millis(self.policy.forward_ms),
                forwarded,
            )
            .await
            {
                Ok(answer) => answer,
                Err(_) => Err(ScopeError::Unreachable {
                    holder: holder.to_owned(),
                }),
            }
        };
        let ms = u64::try_from(nanocode_platform::elapsed(started).as_millis()).unwrap_or(u64::MAX);
        let unreachable = matches!(answer, Err(ScopeError::Unreachable { .. }));
        self.unreachable.update(|missed| {
            if unreachable {
                missed.insert(holder.to_owned());
            } else {
                missed.remove(holder);
            }
        });
        self.stats.update(|stats| {
            stats.forwards += 1;
            stats.forward_ms_total += ms;
            stats.forward_ms_max = stats.forward_ms_max.max(ms);
            stats.forwards_busy += u64::from(matches!(answer, Err(ScopeError::Busy)));
            stats.forwards_unreachable += u64::from(unreachable);
        });
        answer
    }
}
