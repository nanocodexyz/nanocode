use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    ops::Range,
    process::ExitCode,
};

use nanocode_partitions::{
    Fenced, Member, Partition, Refused, Row, Token, assignment, live, partition_of, plan,
    sim::{self as partitions_sim, Mutation as Rule},
};

use crate::rng::{FNV_OFFSET, Rng, fnv};

pub(crate) const TICK_MS: u64 = 100;
pub(crate) const TTL_MS: u64 = 1_000;
pub(crate) const GRACE_MS: u64 = LAND_TICKS_MAX * TICK_MS;
const NODES_MIN: u64 = 3;
const NODES_MAX: u64 = 10;
const PARTITIONS_MIN: u64 = 1;
const PARTITIONS_MAX: u64 = 32;
const THREADS_MIN: u64 = 4;
const THREADS_MAX: u64 = 32;
const STEPS_MIN: u64 = 1;
const STEPS_MAX: u64 = 6;
const STEP_TICKS_MIN: u64 = 1;
const STEP_TICKS_MAX: u64 = 5;
const HANDS_MIN: u64 = 1;
const HANDS_MAX: u64 = 3;
const HAND_STEP_PPM: u64 = 300_000;
const LAND_TICKS_MAX: u64 = 2;
const FAULT_TICKS_MIN: u64 = 300;
const FAULT_TICKS_MAX: u64 = 600;
const FAULT_PPM: u64 = 50_000;
const FAULT_KINDS: u64 = 8;
const KILL_LANDS_PPM: u64 = 500_000;
/// Per tick while a hand runs a step: the hand moves off its host mid-step, the path
/// where the host must fail (not drop) its board claim.
const BUSY_HAND_MOVE_PPM: u64 = 250_000;
const PAUSE_MS_MIN: u64 = TTL_MS / 2;
const PAUSE_MS_MAX: u64 = TTL_MS * 3;
const CUT_TICKS_MIN: u64 = 2;
const CUT_TICKS_MAX: u64 = 30;
const GAP_MS_MIN: u64 = TICK_MS * 2;
const GAP_MS_MAX: u64 = TTL_MS * 3;
const STEP_SETTLE_TICKS: u64 = 2 * (LAND_TICKS_MAX + 1) + STEP_TICKS_MAX + 3;
const SETTLE_MS: u64 = 4 * TTL_MS + STEPS_MAX * STEP_SETTLE_TICKS * TICK_MS;
const PERCENTILE: usize = 99;
const PERCENT: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionsMutation {
    None,
    Unfenced,
    ClaimKeepsEpoch,
    WaitsForFree,
    KeepsMoved,
    AssignByIndex,
    ReplaysInFlight,
    DoubleClaim,
    LostClaim,
    UnboundedLoad,
}

impl PartitionsMutation {
    const ALL: [Self; 10] = [
        Self::None,
        Self::Unfenced,
        Self::ClaimKeepsEpoch,
        Self::WaitsForFree,
        Self::KeepsMoved,
        Self::AssignByIndex,
        Self::ReplaysInFlight,
        Self::DoubleClaim,
        Self::LostClaim,
        Self::UnboundedLoad,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Unfenced => "unfenced",
            Self::ClaimKeepsEpoch => "claim_keeps_epoch",
            Self::WaitsForFree => "waits_for_free",
            Self::KeepsMoved => "keeps_moved",
            Self::AssignByIndex => "assign_by_index",
            Self::ReplaysInFlight => "replays_in_flight",
            Self::DoubleClaim => "double_claim",
            Self::LostClaim => "lost_claim",
            Self::UnboundedLoad => "unbounded_load",
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn rule(self) -> Rule {
        match self {
            Self::Unfenced => Rule::Unfenced,
            Self::ClaimKeepsEpoch => Rule::ClaimKeepsEpoch,
            Self::WaitsForFree => Rule::WaitsForFree,
            Self::KeepsMoved => Rule::KeepsMoved,
            Self::AssignByIndex => Rule::AssignByIndex,
            Self::UnboundedLoad => Rule::UnboundedLoad,
            Self::None | Self::ReplaysInFlight | Self::DoubleClaim | Self::LostClaim => Rule::None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PartitionsStats {
    pub seeds: u64,
    pub claims: u64,
    pub takeovers: u64,
    pub fenced: u64,
    pub renews_fenced: u64,
    pub releases_fenced: u64,
    pub done: u64,
    pub uncertain: u64,
    pub tasks: u64,
    pub hand_moves: u64,
    pub kills: u64,
    pub pauses: u64,
    pub cuts: u64,
    pub joins: u64,
    pub leaves: u64,
    pub gaps: u64,
    pub settle_ms_max: u64,
    pub takeover_ms: Vec<u64>,
    pub timed: Vec<Takeover>,
    pub claims_by_nodes: BTreeMap<u64, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gone {
    Killed,
    Paused,
    Cut,
}

/// One timed takeover, split into where its time went.
#[derive(Clone, Copy, Debug)]
pub struct Takeover {
    pub nodes: u64,
    pub cause: Gone,
    /// From the holder going away to the new holder's claim.
    pub total_ms: u64,
    /// Virtual clock jumps inside that window.
    pub gap_ms: u64,
    /// Until the holder's heartbeat aged past the TTL.
    pub detect_ms: u64,
    /// Until the holder's lease expired.
    pub lease_ms: u64,
    /// From the row first being claimable (holder dead or lease expired) to the claim.
    pub claim_lag_ms: u64,
    /// The new holder joined after the old one went: no node was left to take over.
    pub waited_for_join: bool,
    /// Another node was killed (within a TTL), paused or cut during the window.
    pub overlapping: bool,
    /// No other node killed (within a TTL), paused or cut during the window, no clock gap,
    /// and a node that could take over was already there.
    pub clean: bool,
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    sorted
        .len()
        .checked_mul(percent)
        .map(|scaled| scaled.div_ceil(PERCENT))
        .and_then(|rank| rank.checked_sub(1))
        .and_then(|at| sorted.get(at))
        .copied()
        .unwrap_or(0)
}

fn spread(values: impl Iterator<Item = u64>) -> String {
    let mut sorted: Vec<u64> = values.collect();
    sorted.sort_unstable();
    format!(
        "{}/{}/{}",
        percentile(&sorted, PERCENT / 2),
        percentile(&sorted, PERCENTILE),
        sorted.last().copied().unwrap_or(0)
    )
}

impl PartitionsStats {
    pub fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.claims += other.claims;
        self.takeovers += other.takeovers;
        self.fenced += other.fenced;
        self.renews_fenced += other.renews_fenced;
        self.releases_fenced += other.releases_fenced;
        self.done += other.done;
        self.uncertain += other.uncertain;
        self.tasks += other.tasks;
        self.hand_moves += other.hand_moves;
        self.kills += other.kills;
        self.pauses += other.pauses;
        self.cuts += other.cuts;
        self.joins += other.joins;
        self.leaves += other.leaves;
        self.gaps += other.gaps;
        self.settle_ms_max = self.settle_ms_max.max(other.settle_ms_max);
        self.takeover_ms.extend_from_slice(&other.takeover_ms);
        self.timed.extend_from_slice(&other.timed);
        for (nodes, claims) in &other.claims_by_nodes {
            *self.claims_by_nodes.entry(*nodes).or_insert(0) += claims;
        }
    }

    /// Takeover time split into its parts, by cause and by node count (p50/p99/max ms).
    pub fn breakdown(&self) -> String {
        let row = |label: String, of: &[&Takeover]| {
            format!(
                "{label} n={} total={} gap={} detect={} lease={} claim_lag={} total_minus_gap={}",
                of.len(),
                spread(of.iter().map(|t| t.total_ms)),
                spread(of.iter().map(|t| t.gap_ms)),
                spread(of.iter().map(|t| t.detect_ms)),
                spread(of.iter().map(|t| t.lease_ms)),
                spread(of.iter().map(|t| t.claim_lag_ms)),
                spread(of.iter().map(|t| t.total_ms - t.gap_ms)),
            )
        };
        let all: Vec<&Takeover> = self.timed.iter().collect();
        let clean: Vec<&Takeover> = self.timed.iter().filter(|t| t.clean).collect();
        let gapped: Vec<&Takeover> = self.timed.iter().filter(|t| t.gap_ms > 0).collect();
        let overlapped: Vec<&Takeover> = self
            .timed
            .iter()
            .filter(|t| t.gap_ms == 0 && t.overlapping)
            .collect();
        let joined: Vec<&Takeover> = self
            .timed
            .iter()
            .filter(|t| t.gap_ms == 0 && !t.overlapping && t.waited_for_join)
            .collect();
        let mut lines = vec![
            row("all".to_owned(), &all),
            row("clean".to_owned(), &clean),
            row("clock_gap".to_owned(), &gapped),
            row("overlapping_fault".to_owned(), &overlapped),
            row("waited_for_join".to_owned(), &joined),
        ];
        for cause in [Gone::Killed, Gone::Paused, Gone::Cut] {
            let of: Vec<&Takeover> = self.timed.iter().filter(|t| t.cause == cause).collect();
            lines.push(row(format!("{cause:?}"), &of));
            let clean: Vec<&Takeover> = of.into_iter().filter(|t| t.clean).collect();
            lines.push(row(format!("{cause:?}_clean"), &clean));
        }
        lines.join("\n")
    }

    fn summary(&self) -> String {
        let mut latencies = self.takeover_ms.clone();
        latencies.sort_unstable();
        let p99 = latencies
            .len()
            .checked_mul(PERCENTILE)
            .map(|scaled| scaled.div_ceil(PERCENT))
            .and_then(|rank| rank.checked_sub(1))
            .and_then(|at| latencies.get(at))
            .copied()
            .unwrap_or(0);
        format!(
            "seeds={} claims={} takeovers={} fenced_appends={} fenced_renews={} fenced_releases={} steps_done={} steps_uncertain={} hand_tasks={} hand_moves={} kills={} pauses={} cuts={} joins={} leaves={} clock_gaps={} takeover_ms_max={} takeover_ms_p99={} settle_ms_max={}",
            self.seeds,
            self.claims,
            self.takeovers,
            self.fenced,
            self.renews_fenced,
            self.releases_fenced,
            self.done,
            self.uncertain,
            self.tasks,
            self.hand_moves,
            self.kills,
            self.pauses,
            self.cuts,
            self.joins,
            self.leaves,
            self.gaps,
            latencies.last().copied().unwrap_or(0),
            p99,
            self.settle_ms_max
        ) + &self.by_nodes()
    }

    fn by_nodes(&self) -> String {
        (NODES_MIN..=NODES_MAX)
            .map(|nodes| {
                format!(
                    " nodes{nodes}=claims:{},takeover_ms_p50/p99/max:{}",
                    self.claims_by_nodes.get(&nodes).copied().unwrap_or(0),
                    spread(
                        self.timed
                            .iter()
                            .filter(|t| t.nodes == nodes)
                            .map(|t| t.total_ms)
                    )
                )
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId([u8; 4]);

impl NodeId {
    pub(crate) const fn new(index: u32) -> Self {
        Self(index.to_be_bytes())
    }

    pub(crate) const fn index(self) -> usize {
        u32::from_be_bytes(self.0) as usize
    }
}

impl AsRef<[u8]> for NodeId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

pub(crate) type StepKey = (u32, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    Started {
        thread: u32,
        step: u32,
        hand: Option<u32>,
    },
    Done {
        thread: u32,
        step: u32,
    },
    Uncertain {
        thread: u32,
        step: u32,
    },
}

impl Record {
    pub(crate) const fn key(self) -> StepKey {
        match self {
            Self::Started { thread, step, .. }
            | Self::Done { thread, step }
            | Self::Uncertain { thread, step } => (thread, step),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) epoch: u64,
    pub(crate) writer: NodeId,
    pub(crate) record: Record,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskState {
    Posted,
    Claimed { host: NodeId },
    Finished { host: NodeId },
    Failed { host: NodeId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Task {
    pub(crate) hand: u32,
    pub(crate) epoch: u64,
    pub(crate) state: TaskState,
}

#[derive(Debug)]
pub(crate) struct Store {
    pub(crate) rows: BTreeMap<Partition, Row<NodeId>>,
    pub(crate) members: BTreeMap<NodeId, u64>,
    pub(crate) logs: BTreeMap<Partition, Vec<Entry>>,
    pub(crate) tasks: BTreeMap<StepKey, Task>,
}

impl Store {
    pub(crate) fn new(count: NonZeroU32) -> Self {
        let partitions = (0..count.get()).map(Partition);
        Self {
            rows: partitions
                .clone()
                .map(|partition| (partition, Row::unclaimed()))
                .collect(),
            members: BTreeMap::new(),
            logs: partitions
                .map(|partition| (partition, Vec::new()))
                .collect(),
            tasks: BTreeMap::new(),
        }
    }

    pub(crate) fn heartbeat(&mut self, node: NodeId, now: u64) {
        self.members.insert(node, now);
    }

    pub(crate) fn remove_member(&mut self, node: NodeId) {
        self.members.remove(&node);
    }

    pub(crate) fn alive(&self, now: u64, ttl: u64) -> BTreeSet<NodeId> {
        let members: Vec<Member<NodeId>> = self
            .members
            .iter()
            .map(|(node, seen_at)| Member {
                node: *node,
                seen_at: *seen_at,
            })
            .collect();
        live(&members, now, ttl)
    }

    pub(crate) fn claim(
        &mut self,
        partition: Partition,
        node: NodeId,
        now: u64,
        ttl: u64,
        grace: u64,
    ) -> Result<(Option<NodeId>, u64), Refused<NodeId>> {
        let row = &self.rows[&partition];
        let previous = row.holder().copied();
        let (next, epoch) = row.claim(&node, now, ttl, grace)?;
        self.rows.insert(partition, next);
        Ok((previous, epoch))
    }

    pub(crate) fn renew(
        &mut self,
        partition: Partition,
        node: NodeId,
        epoch: u64,
        now: u64,
        ttl: u64,
    ) -> Result<(), Fenced> {
        let next = self.rows[&partition].renew(partition, &node, epoch, now, ttl)?;
        self.rows.insert(partition, next);
        Ok(())
    }

    pub(crate) fn release(
        &mut self,
        partition: Partition,
        node: NodeId,
        epoch: u64,
    ) -> Result<(), Fenced> {
        let cursor = self.logs.get(&partition).map_or(0, Vec::len) as u64;
        let next = self.rows[&partition].release(partition, &node, epoch, cursor)?;
        self.rows.insert(partition, next);
        Ok(())
    }

    pub(crate) fn append(
        &mut self,
        token: Token,
        writer: NodeId,
        record: Record,
    ) -> Result<(), Fenced> {
        self.rows[&token.partition].fence(token)?;
        self.logs.entry(token.partition).or_default().push(Entry {
            epoch: token.epoch,
            writer,
            record,
        });
        if let Record::Started {
            thread,
            step,
            hand: Some(hand),
        } = record
        {
            self.tasks.entry((thread, step)).or_insert(Task {
                hand,
                epoch: token.epoch,
                state: TaskState::Posted,
            });
        }
        Ok(())
    }

    pub(crate) fn claim_task(&mut self, key: StepKey, host: NodeId, over_claims: bool) -> bool {
        let Some(task) = self.tasks.get_mut(&key) else {
            return false;
        };
        let open = match task.state {
            TaskState::Posted => true,
            TaskState::Claimed { host: other } => over_claims && other != host,
            TaskState::Finished { .. } | TaskState::Failed { .. } => false,
        };
        if open {
            task.state = TaskState::Claimed { host };
        }
        open
    }

    pub(crate) fn finish_task(&mut self, key: StepKey, host: NodeId) -> bool {
        self.settle_task(key, host, TaskState::Finished { host })
    }

    pub(crate) fn fail_task(&mut self, key: StepKey, host: NodeId) -> bool {
        self.settle_task(key, host, TaskState::Failed { host })
    }

    pub(crate) fn repost_task(&mut self, key: StepKey, host: NodeId) -> bool {
        self.settle_task(key, host, TaskState::Posted)
    }

    fn settle_task(&mut self, key: StepKey, host: NodeId, state: TaskState) -> bool {
        match self.tasks.get_mut(&key) {
            Some(task) if task.state == (TaskState::Claimed { host }) => {
                task.state = state;
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    Up,
    Leaving,
    Killed,
    Left,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Starting { step: u32 },
    Running { step: u32, until: u64 },
    Waiting { step: u32 },
    Resolving { step: u32 },
}

#[derive(Clone, Copy, Debug)]
struct Lane {
    next: u32,
    phase: Phase,
}

#[derive(Debug)]
struct Owned {
    epoch: u64,
    lanes: BTreeMap<u32, Lane>,
}

#[derive(Clone, Copy, Debug)]
struct Send {
    token: Token,
    record: Record,
}

#[derive(Clone, Copy, Debug)]
struct Flight {
    from: NodeId,
    send: Send,
    land_at: u64,
}

#[derive(Clone, Copy, Debug)]
struct Hosted {
    hand: u32,
    until: u64,
}

#[derive(Debug)]
struct Node {
    id: NodeId,
    life: Life,
    paused_until: Option<u64>,
    cut_until: Option<u64>,
    owned: BTreeMap<Partition, Owned>,
    outbox: Vec<Send>,
    running: BTreeMap<StepKey, Hosted>,
}

impl Node {
    const fn new(id: NodeId) -> Self {
        Self {
            id,
            life: Life::Up,
            paused_until: None,
            cut_until: None,
            owned: BTreeMap::new(),
            outbox: Vec::new(),
            running: BTreeMap::new(),
        }
    }

    const fn member(&self) -> bool {
        matches!(self.life, Life::Up | Life::Leaving)
    }

    const fn stays(&self) -> bool {
        matches!(self.life, Life::Up)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Step {
    pub(crate) ready_at: u64,
    pub(crate) ticks: u64,
    pub(crate) hand: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct Thread {
    pub(crate) partition: Partition,
    pub(crate) steps: Vec<Step>,
}

#[derive(Clone, Copy, Debug)]
enum Change {
    Join(NodeId),
    Leave(NodeId),
}

pub(crate) fn thread_partition(thread: u32, count: NonZeroU32) -> Partition {
    partition_of(format!("thread-{thread}").as_bytes(), count)
}

struct World {
    rng: Rng,
    mutation: PartitionsMutation,
    now: u64,
    count: NonZeroU32,
    store: Store,
    nodes: Vec<Node>,
    threads: Vec<Thread>,
    by_partition: BTreeMap<Partition, Vec<u32>>,
    hands: Vec<Option<NodeId>>,
    wire: Vec<Flight>,
    members: BTreeSet<NodeId>,
    granted: BTreeMap<Partition, u64>,
    epochs: BTreeMap<Partition, u64>,
    started: BTreeSet<StepKey>,
    resolved: BTreeSet<StepKey>,
    runs: BTreeMap<StepKey, u32>,
    task_claims: BTreeMap<StepKey, u32>,
    gone_at: BTreeMap<NodeId, (u64, Gone)>,
    gaps: Vec<(u64, u64)>,
    faults: Vec<(NodeId, u64, u64)>,
    joined_at: BTreeMap<NodeId, u64>,
    nodes_at_start: u64,
    stats: PartitionsStats,
}

impl World {
    fn partitions(&self) -> impl Iterator<Item = Partition> + use<> {
        (0..self.count.get()).map(Partition)
    }

    fn step(&self, key: StepKey) -> Step {
        self.threads[key.0 as usize].steps[key.1 as usize]
    }

    fn membership(&mut self, change: Change) -> Result<(), String> {
        let before = assignment(self.count, &self.members);
        let nodes = match change {
            Change::Join(node) => {
                self.members.insert(node);
                self.members.len()
            }
            Change::Leave(node) => {
                let nodes = self.members.len();
                self.members.remove(&node);
                nodes
            }
        };
        let after = assignment(self.count, &self.members);
        let forced = match change {
            Change::Join(node) => after.values().filter(|owner| **owner == node).count(),
            Change::Leave(node) => before.values().filter(|owner| **owner == node).count(),
        };
        let moved = self
            .partitions()
            .filter(|partition| before.get(partition) != after.get(partition))
            .count();
        if moved > 2 * forced + nodes {
            return Err(format!(
                "invariant 6 (minimal movement): {change:?} moved {moved} partitions, {forced} of them forced, among {nodes} nodes"
            ));
        }
        self.balanced(&after)
    }

    fn balanced(&self, assigned: &BTreeMap<Partition, NodeId>) -> Result<(), String> {
        let loads: Vec<usize> = self
            .members
            .iter()
            .map(|member| assigned.values().filter(|owner| *owner == member).count())
            .collect();
        let (Some(most), Some(least)) = (loads.iter().max(), loads.iter().min()) else {
            return Ok(());
        };
        if most - least > 1 {
            return Err(format!(
                "invariant 7 (balance): {} partitions over {} nodes give {most} to one and {least} to another",
                self.count,
                self.members.len()
            ));
        }
        Ok(())
    }

    fn pick(&mut self, choose: impl Fn(&Node) -> bool) -> Option<usize> {
        let candidates: Vec<usize> = (0..self.nodes.len())
            .filter(|at| choose(&self.nodes[*at]))
            .collect();
        (!candidates.is_empty()).then(|| candidates[self.rng.pick(candidates.len())])
    }

    fn staying_without(&self, at: usize) -> usize {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(other, node)| *other != at && node.stays())
            .count()
    }

    fn detach(&mut self, id: NodeId) {
        for host in &mut self.hands {
            if *host == Some(id) {
                *host = None;
            }
        }
    }

    fn busy_hands(&self) -> Vec<usize> {
        let mut busy: Vec<usize> = self
            .nodes
            .iter()
            .flat_map(|node| {
                node.running
                    .values()
                    .filter(|hosted| self.hands[hosted.hand as usize] == Some(node.id))
                    .map(|hosted| hosted.hand as usize)
            })
            .collect();
        busy.sort_unstable();
        busy.dedup();
        busy
    }

    fn move_busy_hand(&mut self) {
        let busy = self.busy_hands();
        if busy.is_empty() || !self.rng.chance(BUSY_HAND_MOVE_PPM) {
            return;
        }
        let hand = busy[self.rng.pick(busy.len())];
        let host = self.hands[hand];
        match self.pick(|node| node.stays() && Some(node.id) != host) {
            Some(at) => self.attach(hand, at),
            None => self.hands[hand] = None,
        }
    }

    fn fault(&mut self) -> Result<(), String> {
        self.move_busy_hand();
        if !self.rng.chance(FAULT_PPM) {
            return Ok(());
        }
        match self.rng.below(FAULT_KINDS) {
            0 => self.kill(),
            1 => {
                if let Some(at) = self.pick(|node| node.member() && node.paused_until.is_none()) {
                    let until = self.now + self.rng.range(PAUSE_MS_MIN, PAUSE_MS_MAX);
                    self.flush(at);
                    self.nodes[at].paused_until = Some(until);
                    self.gone_at
                        .entry(self.nodes[at].id)
                        .or_insert((self.now, Gone::Paused));
                    self.faults.push((self.nodes[at].id, self.now, until));
                    self.stats.pauses += 1;
                }
                Ok(())
            }
            2 => {
                if let Some(at) = self.pick(|node| node.member() && node.cut_until.is_none()) {
                    let until = self.now + self.rng.range(CUT_TICKS_MIN, CUT_TICKS_MAX) * TICK_MS;
                    self.nodes[at].cut_until = Some(until);
                    self.gone_at
                        .entry(self.nodes[at].id)
                        .or_insert((self.now, Gone::Cut));
                    self.faults.push((self.nodes[at].id, self.now, until));
                    self.stats.cuts += 1;
                }
                Ok(())
            }
            3 => {
                if self.members.len() as u64 >= NODES_MAX {
                    return Ok(());
                }
                let id = NodeId::new(
                    u32::try_from(self.nodes.len()).map_err(|error| error.to_string())?,
                );
                self.nodes.push(Node::new(id));
                self.joined_at.insert(id, self.now);
                self.stats.joins += 1;
                self.membership(Change::Join(id))
            }
            4 => {
                let at = self.pick(|node| node.stays());
                if let Some(at) = at.filter(|at| self.staying_without(*at) > 0) {
                    self.nodes[at].life = Life::Leaving;
                }
                Ok(())
            }
            5 => {
                let gap = self.rng.range(GAP_MS_MIN, GAP_MS_MAX);
                self.gaps.push((self.now, gap));
                self.now += gap;
                self.stats.gaps += 1;
                Ok(())
            }
            6 => {
                let attached: Vec<usize> = (0..self.hands.len())
                    .filter(|hand| self.hands[*hand].is_some())
                    .collect();
                if !attached.is_empty() {
                    let hand = attached[self.rng.pick(attached.len())];
                    self.hands[hand] = None;
                }
                Ok(())
            }
            _ => {
                let hand = self.rng.pick(self.hands.len());
                if let Some(at) = self.pick(Node::stays) {
                    self.attach(hand, at);
                }
                Ok(())
            }
        }
    }

    fn attach(&mut self, hand: usize, at: usize) {
        let id = self.nodes[at].id;
        if self.hands[hand] != Some(id) {
            self.hands[hand] = Some(id);
            self.stats.hand_moves += 1;
        }
    }

    fn kill(&mut self) -> Result<(), String> {
        let at = self.pick(Node::member);
        let Some(at) = at.filter(|at| self.staying_without(*at) > 0) else {
            return Ok(());
        };
        let id = self.nodes[at].id;
        let node = &mut self.nodes[at];
        node.life = Life::Killed;
        node.paused_until = None;
        node.cut_until = None;
        node.owned.clear();
        node.outbox.clear();
        node.running.clear();
        let wire = std::mem::take(&mut self.wire);
        for flight in wire {
            if flight.from != id || self.rng.chance(KILL_LANDS_PPM) {
                self.wire.push(flight);
            }
        }
        self.gone_at.insert(id, (self.now, Gone::Killed));
        self.detach(id);
        self.faults.push((id, self.now, self.now + TTL_MS));
        self.stats.kills += 1;
        self.membership(Change::Leave(id))
    }

    fn wake(&mut self) {
        for node in &mut self.nodes {
            if node.paused_until.is_some_and(|until| until <= self.now) {
                node.paused_until = None;
            }
            if node.cut_until.is_some_and(|until| until <= self.now) {
                node.cut_until = None;
            }
            if node.member() && node.paused_until.is_none() && node.cut_until.is_none() {
                self.gone_at.remove(&node.id);
            }
        }
    }

    fn paused(&self, id: NodeId) -> bool {
        self.nodes[id.index()].paused_until.is_some()
    }

    fn land(&mut self) -> Result<(), String> {
        let wire = std::mem::take(&mut self.wire);
        let mut kept = Vec::new();
        let mut landing = Vec::new();
        for flight in wire {
            if flight.land_at > self.now || self.paused(flight.from) {
                kept.push(flight);
            } else {
                landing.push(flight);
            }
        }
        self.wire = kept;
        for flight in landing {
            self.commit(flight)?;
        }
        Ok(())
    }

    fn commit(&mut self, flight: Flight) -> Result<(), String> {
        let Send { token, record } = flight.send;
        let landed = self.store.append(token, flight.from, record).is_ok();
        if !landed {
            self.stats.fenced += 1;
            self.deliver(flight.from, flight.send, false)?;
            return Ok(());
        }
        let row = &self.store.rows[&token.partition];
        if row.epoch() != token.epoch || row.holder() != Some(&flight.from) {
            return Err(format!(
                "invariant 2 (fenced appends): {:?} committed {record:?} at epoch {} to partition {} while its row is {row:?}",
                flight.from, token.epoch, token.partition.0
            ));
        }
        let key = record.key();
        match record {
            Record::Started { .. } => {
                if !self.started.insert(key) {
                    return Err(format!(
                        "invariant 3 (no replay): thread {} step {} Started again by {:?} at epoch {}",
                        key.0, key.1, flight.from, token.epoch
                    ));
                }
                if self.step(key).hand.is_some() {
                    self.stats.tasks += 1;
                }
            }
            Record::Done { .. } | Record::Uncertain { .. } => {
                if !self.started.contains(&key) || !self.resolved.insert(key) {
                    return Err(format!(
                        "invariant 3 (one result): thread {} step {} resolved by {record:?} at epoch {} without one open start",
                        key.0, key.1, token.epoch
                    ));
                }
                if matches!(record, Record::Uncertain { .. }) {
                    self.stats.uncertain += 1;
                } else {
                    self.stats.done += 1;
                }
            }
        }
        self.deliver(flight.from, flight.send, true)
    }

    fn ran(&mut self, key: StepKey, by: NodeId) -> Result<(), String> {
        let runs = self.runs.entry(key).or_insert(0);
        *runs += 1;
        if *runs > 1 {
            return Err(format!(
                "invariant 3 (no replay): thread {} step {} ran {runs} times, the last on {by:?}",
                key.0, key.1
            ));
        }
        Ok(())
    }

    fn deliver(&mut self, from: NodeId, send: Send, landed: bool) -> Result<(), String> {
        let at = from.index();
        if !self.nodes[at].member() {
            return Ok(());
        }
        let partition = send.token.partition;
        let current = self.nodes[at]
            .owned
            .get(&partition)
            .map(|owned| owned.epoch);
        if current != Some(send.token.epoch) {
            return Ok(());
        }
        if !landed {
            self.nodes[at].owned.remove(&partition);
            return Ok(());
        }
        let key = send.record.key();
        let step = self.step(key);
        let phase = match send.record {
            Record::Started { hand: Some(_), .. } => Phase::Waiting { step: key.1 },
            Record::Started { hand: None, .. } => {
                self.ran(key, from)?;
                Phase::Running {
                    step: key.1,
                    until: self.now + step.ticks * TICK_MS,
                }
            }
            Record::Done { .. } | Record::Uncertain { .. } => Phase::Idle,
        };
        let lane = self.nodes[at]
            .owned
            .get_mut(&partition)
            .and_then(|owned| owned.lanes.get_mut(&key.0))
            .ok_or_else(|| format!("{from:?} has no lane for thread {}", key.0))?;
        let expected = match send.record {
            Record::Started { .. } => Phase::Starting { step: key.1 },
            Record::Done { .. } | Record::Uncertain { .. } => Phase::Resolving { step: key.1 },
        };
        if lane.phase != expected {
            return Err(format!(
                "{from:?} got {:?} for thread {} while its lane is {lane:?}",
                send.record, key.0
            ));
        }
        lane.phase = phase;
        if phase == Phase::Idle {
            lane.next = key.1 + 1;
        }
        Ok(())
    }

    fn flush(&mut self, at: usize) {
        let from = self.nodes[at].id;
        let outbox = std::mem::take(&mut self.nodes[at].outbox);
        for send in outbox {
            let land_at = self.now + self.rng.below(LAND_TICKS_MAX + 1) * TICK_MS;
            self.wire.push(Flight {
                from,
                send,
                land_at,
            });
        }
    }

    fn in_flight(&self, at: usize, partition: Option<Partition>) -> bool {
        let id = self.nodes[at].id;
        let matches =
            |send: &Send| partition.is_none_or(|partition| send.token.partition == partition);
        self.nodes[at].outbox.iter().any(matches)
            || self
                .wire
                .iter()
                .any(|flight| flight.from == id && matches(&flight.send))
    }

    fn drained(&self, at: usize, partition: Partition) -> bool {
        !self.in_flight(at, Some(partition))
            && self.nodes[at].owned.get(&partition).is_none_or(|owned| {
                owned
                    .lanes
                    .values()
                    .all(|lane| matches!(lane.phase, Phase::Idle | Phase::Waiting { .. }))
            })
    }

    fn tick(&mut self) -> Result<(), String> {
        self.wake();
        self.land()?;
        let mut order: Vec<usize> = (0..self.nodes.len()).collect();
        for at in (1..order.len()).rev() {
            let other = self.rng.pick(at + 1);
            order.swap(at, other);
        }
        for at in order {
            self.node_tick(at)?;
        }
        self.check_beliefs()
    }

    fn node_tick(&mut self, at: usize) -> Result<(), String> {
        let node = &self.nodes[at];
        let id = node.id;
        if !node.member() || node.paused_until.is_some() {
            return Ok(());
        }
        let leaving = node.life == Life::Leaving;
        if node.cut_until.is_some() {
            self.finish_effects(at);
            return Ok(());
        }
        self.flush(at);
        self.store.heartbeat(id, self.now);
        let alive = self.store.alive(self.now, TTL_MS);
        self.renew(at);
        let held: BTreeMap<Partition, u64> = self.nodes[at]
            .owned
            .iter()
            .map(|(partition, owned)| (*partition, owned.epoch))
            .collect();
        let planned = plan(
            &id,
            self.count,
            &alive,
            &held,
            &self.store.rows,
            self.now,
            GRACE_MS,
        );
        let releasing: BTreeSet<Partition> = if leaving {
            held.keys().copied().collect()
        } else {
            planned
                .release
                .iter()
                .map(|token| token.partition)
                .collect()
        };
        for partition in &releasing {
            self.work(at, *partition, &alive, false)?;
            if self.drained(at, *partition) {
                self.release(at, *partition);
            }
        }
        if !leaving {
            for partition in planned.claim {
                self.claim(at, partition)?;
            }
        }
        let working: Vec<Partition> = self.nodes[at]
            .owned
            .keys()
            .copied()
            .filter(|partition| !releasing.contains(partition))
            .collect();
        for partition in working {
            self.work(at, partition, &alive, true)?;
        }
        self.host(at, leaving)?;
        if leaving
            && self.nodes[at].owned.is_empty()
            && self.nodes[at].running.is_empty()
            && !self.in_flight(at, None)
        {
            self.store.remove_member(id);
            self.nodes[at].life = Life::Left;
            self.detach(id);
            self.stats.leaves += 1;
            self.membership(Change::Leave(id))?;
        }
        Ok(())
    }

    fn renew(&mut self, at: usize) {
        let id = self.nodes[at].id;
        let held: Vec<(Partition, u64)> = self.nodes[at]
            .owned
            .iter()
            .map(|(partition, owned)| (*partition, owned.epoch))
            .collect();
        for (partition, epoch) in held {
            if self
                .store
                .renew(partition, id, epoch, self.now, TTL_MS)
                .is_err()
            {
                self.stats.renews_fenced += 1;
                self.nodes[at].owned.remove(&partition);
            }
        }
    }

    fn release(&mut self, at: usize, partition: Partition) {
        let id = self.nodes[at].id;
        if let Some(owned) = self.nodes[at].owned.remove(&partition)
            && self.store.release(partition, id, owned.epoch).is_err()
        {
            self.stats.releases_fenced += 1;
        }
    }

    fn claim(&mut self, at: usize, partition: Partition) -> Result<(), String> {
        let id = self.nodes[at].id;
        let row = &self.store.rows[&partition];
        let before = row.epoch();
        let expires_at = match row {
            Row::Held { expires_at, .. } => *expires_at,
            Row::Released { .. } => self.now,
        };
        let Ok((previous, epoch)) = self.store.claim(partition, id, self.now, TTL_MS, GRACE_MS)
        else {
            return Ok(());
        };
        if epoch < before {
            return Err(format!(
                "invariant 1 (epochs): {id:?} claimed partition {} at epoch {epoch} below the row's {before}",
                partition.0
            ));
        }
        if let Some(last) = self.granted.get(&partition)
            && epoch <= *last
        {
            return Err(format!(
                "invariant 1 (one holder per epoch): {id:?} claimed partition {} from {previous:?} at epoch {epoch}, already granted up to {last}",
                partition.0
            ));
        }
        self.granted.insert(partition, epoch);
        self.stats.claims += 1;
        *self
            .stats
            .claims_by_nodes
            .entry(self.nodes_at_start)
            .or_insert(0) += 1;
        if let Some(previous) = previous
            && previous != id
        {
            self.stats.takeovers += 1;
            if let Some((gone_at, cause)) = self.gone_at.get(&previous).copied() {
                self.time_takeover(id, previous, gone_at, cause, expires_at);
            }
        }
        self.fold(at, partition, epoch);
        Ok(())
    }

    fn time_takeover(
        &mut self,
        by: NodeId,
        previous: NodeId,
        gone_at: u64,
        cause: Gone,
        expires_at: u64,
    ) {
        let total_ms = self.now - gone_at;
        let dead_at = self
            .store
            .members
            .get(&previous)
            .map_or(gone_at, |seen_at| seen_at + TTL_MS);
        let claimable_at = dead_at.min(expires_at).max(gone_at).min(self.now);
        let gap_ms = self
            .gaps
            .iter()
            .filter(|(at, _)| *at >= gone_at)
            .map(|(_, gap)| gap)
            .sum::<u64>()
            .min(total_ms);
        let overlapping = self
            .faults
            .iter()
            .any(|(node, from, until)| *node != previous && *from <= self.now && *until >= gone_at);
        let waited_for_join = self.joined_at.get(&by).is_some_and(|at| *at > gone_at);
        self.stats.takeover_ms.push(total_ms);
        self.stats.timed.push(Takeover {
            nodes: self.nodes_at_start,
            cause,
            total_ms,
            gap_ms,
            detect_ms: dead_at.saturating_sub(gone_at),
            lease_ms: expires_at.saturating_sub(gone_at),
            claim_lag_ms: self.now - claimable_at,
            waited_for_join,
            overlapping,
            clean: gap_ms == 0 && !overlapping && !waited_for_join,
        });
    }

    fn fold(&mut self, at: usize, partition: Partition, epoch: u64) {
        let token = Token { partition, epoch };
        let threads = self
            .by_partition
            .get(&partition)
            .cloned()
            .unwrap_or_default();
        let log = &self.store.logs[&partition];
        let mut lanes = BTreeMap::new();
        let mut sends = Vec::new();
        for thread in threads {
            let mut started = BTreeSet::new();
            let mut resolved = BTreeSet::new();
            for entry in log.iter().filter(|entry| entry.record.key().0 == thread) {
                match entry.record {
                    Record::Started { step, .. } => started.insert(step),
                    Record::Done { step, .. } | Record::Uncertain { step, .. } => {
                        resolved.insert(step)
                    }
                };
            }
            let next = resolved.iter().next_back().map_or(0, |step| step + 1);
            let open = started.difference(&resolved).next().copied();
            let lane = match open {
                Some(step) if self.mutation == PartitionsMutation::ReplaysInFlight => Lane {
                    next: step,
                    phase: Phase::Idle,
                },
                Some(step) => {
                    sends.push(Send {
                        token,
                        record: Record::Uncertain { thread, step },
                    });
                    Lane {
                        next: step,
                        phase: Phase::Resolving { step },
                    }
                }
                None => Lane {
                    next,
                    phase: Phase::Idle,
                },
            };
            lanes.insert(thread, lane);
        }
        let node = &mut self.nodes[at];
        node.owned.insert(partition, Owned { epoch, lanes });
        node.outbox.extend(sends);
    }

    fn finish_effects(&mut self, at: usize) {
        let now = self.now;
        let node = &mut self.nodes[at];
        for (partition, owned) in &mut node.owned {
            let token = Token {
                partition: *partition,
                epoch: owned.epoch,
            };
            for (thread, lane) in &mut owned.lanes {
                if let Phase::Running { step, until } = lane.phase
                    && until <= now
                {
                    node.outbox.push(Send {
                        token,
                        record: Record::Done {
                            thread: *thread,
                            step,
                        },
                    });
                    lane.phase = Phase::Resolving { step };
                }
            }
        }
    }

    fn work(
        &mut self,
        at: usize,
        partition: Partition,
        alive: &BTreeSet<NodeId>,
        start: bool,
    ) -> Result<(), String> {
        let Some(owned) = self.nodes[at].owned.get(&partition) else {
            return Ok(());
        };
        let token = Token {
            partition,
            epoch: owned.epoch,
        };
        let lanes: Vec<(u32, Lane)> = owned
            .lanes
            .iter()
            .map(|(thread, lane)| (*thread, *lane))
            .collect();
        for (thread, lane) in lanes {
            let steps = &self.threads[thread as usize].steps;
            let (record, phase) = match lane.phase {
                Phase::Idle if start => match steps.get(lane.next as usize) {
                    Some(step) if step.ready_at <= self.now => (
                        Record::Started {
                            thread,
                            step: lane.next,
                            hand: step.hand,
                        },
                        Phase::Starting { step: lane.next },
                    ),
                    _ => continue,
                },
                Phase::Running { step, until } if until <= self.now => {
                    (Record::Done { thread, step }, Phase::Resolving { step })
                }
                Phase::Waiting { step } => {
                    match self.store.tasks.get(&(thread, step)).map(|task| task.state) {
                        Some(TaskState::Finished { .. }) => {
                            (Record::Done { thread, step }, Phase::Resolving { step })
                        }
                        Some(TaskState::Failed { .. }) => (
                            Record::Uncertain { thread, step },
                            Phase::Resolving { step },
                        ),
                        Some(TaskState::Posted | TaskState::Claimed { .. }) => continue,
                        None => {
                            return Err(format!(
                                "thread {thread} step {step} waits on a task the store never posted"
                            ));
                        }
                    }
                }
                _ => continue,
            };
            self.nodes[at].outbox.push(Send { token, record });
            if let Some(lane) = self.nodes[at]
                .owned
                .get_mut(&partition)
                .and_then(|owned| owned.lanes.get_mut(&thread))
            {
                lane.phase = phase;
            }
        }
        self.sweep(partition, alive);
        Ok(())
    }

    fn sweep(&mut self, partition: Partition, alive: &BTreeSet<NodeId>) {
        let orphans: Vec<(StepKey, NodeId)> = self
            .store
            .tasks
            .iter()
            .filter(|(key, _)| self.threads[key.0 as usize].partition == partition)
            .filter_map(|(key, task)| match task.state {
                TaskState::Claimed { host } if !alive.contains(&host) => Some((*key, host)),
                _ => None,
            })
            .collect();
        for (key, host) in orphans {
            self.store.fail_task(key, host);
        }
    }

    fn host(&mut self, at: usize, leaving: bool) -> Result<(), String> {
        let id = self.nodes[at].id;
        let running: Vec<(StepKey, Hosted)> = self.nodes[at]
            .running
            .iter()
            .map(|(key, hosted)| (*key, *hosted))
            .collect();
        for (key, hosted) in running {
            if self.hands[hosted.hand as usize] != Some(id) {
                match self.mutation {
                    PartitionsMutation::LostClaim => {}
                    PartitionsMutation::DoubleClaim => {
                        self.store.repost_task(key, id);
                    }
                    _ => {
                        self.store.fail_task(key, id);
                    }
                }
                self.nodes[at].running.remove(&key);
            } else if hosted.until <= self.now {
                self.store.finish_task(key, id);
                self.nodes[at].running.remove(&key);
            }
        }
        if leaving {
            return Ok(());
        }
        let over_claims = self.mutation == PartitionsMutation::DoubleClaim;
        let open: Vec<(StepKey, u32)> = self
            .store
            .tasks
            .iter()
            .filter(|(key, task)| {
                self.hands[task.hand as usize] == Some(id)
                    && !self.nodes[at].running.contains_key(key)
                    && match task.state {
                        TaskState::Posted => true,
                        TaskState::Claimed { host } => over_claims && host != id,
                        TaskState::Finished { .. } | TaskState::Failed { .. } => false,
                    }
            })
            .map(|(key, task)| (*key, task.hand))
            .collect();
        for (key, hand) in open {
            if !self.store.claim_task(key, id, over_claims) {
                continue;
            }
            let claims = self.task_claims.entry(key).or_insert(0);
            *claims += 1;
            if *claims > 1 {
                return Err(format!(
                    "invariant 4 (one board claim): thread {} step {} claimed {claims} times, the last by {id:?}",
                    key.0, key.1
                ));
            }
            self.ran(key, id)?;
            let until = self.now + self.step(key).ticks * TICK_MS;
            self.nodes[at].running.insert(key, Hosted { hand, until });
        }
        Ok(())
    }

    fn check_beliefs(&mut self) -> Result<(), String> {
        for (key, task) in &self.store.tasks {
            if let TaskState::Claimed { host } = task.state {
                let node = &self.nodes[host.index()];
                if matches!(node.life, Life::Up | Life::Leaving) && !node.running.contains_key(key)
                {
                    return Err(format!(
                        "invariant 4 (no lost claim): thread {} step {} is claimed by {host:?}, which is up but no longer runs it",
                        key.0, key.1
                    ));
                }
            }
        }
        for partition in self.partitions() {
            let row = &self.store.rows[&partition];
            let epoch = row.epoch();
            let last = self.epochs.insert(partition, epoch).unwrap_or(0);
            if epoch < last {
                return Err(format!(
                    "invariant 1 (epochs): partition {} went back from epoch {last} to {epoch}",
                    partition.0
                ));
            }
            let believers: Vec<NodeId> = self
                .nodes
                .iter()
                .filter(|node| {
                    node.member()
                        && node
                            .owned
                            .get(&partition)
                            .is_some_and(|owned| owned.epoch == epoch)
                })
                .map(|node| node.id)
                .collect();
            if believers.len() > 1
                || believers
                    .first()
                    .is_some_and(|believer| row.holder() != Some(believer))
            {
                return Err(format!(
                    "invariant 1 (one holder per epoch): partition {} at epoch {epoch} is held by {believers:?} while its row is {row:?}",
                    partition.0
                ));
            }
        }
        Ok(())
    }

    fn quiesce(&mut self) {
        for node in &mut self.nodes {
            node.paused_until = None;
            node.cut_until = None;
            if node.member() {
                self.gone_at.remove(&node.id);
            }
        }
        for hand in 0..self.hands.len() {
            let settled = self.hands[hand].is_some_and(|host| self.nodes[host.index()].stays());
            if !settled && let Some(at) = self.pick(Node::stays) {
                self.attach(hand, at);
            }
        }
    }

    fn unsettled(&self) -> Option<String> {
        if let Some(node) = self.nodes.iter().find(|node| node.life == Life::Leaving) {
            return Some(format!("{:?} is still leaving", node.id));
        }
        for partition in self.partitions() {
            let expected = assignment(self.count, &self.members)
                .get(&partition)
                .copied();
            let row = &self.store.rows[&partition];
            let held = match (row, expected) {
                (Row::Held { holder, epoch, .. }, Some(expected)) => {
                    *holder == expected
                        && self.nodes[expected.index()]
                            .owned
                            .get(&partition)
                            .is_some_and(|owned| owned.epoch == *epoch)
                }
                _ => false,
            };
            if !held {
                return Some(format!(
                    "partition {} is {row:?}, its assignee {expected:?} among {:?}",
                    partition.0, self.members
                ));
            }
        }
        if let Some((key, task)) =
            self.store.tasks.iter().find(|(_, task)| {
                matches!(task.state, TaskState::Posted | TaskState::Claimed { .. })
            })
        {
            return Some(format!(
                "invariant 4 (no lost claim): thread {} step {} task is {:?} on hand {} hosted by {:?}",
                key.0, key.1, task.state, task.hand, self.hands[task.hand as usize]
            ));
        }
        for (thread, spec) in self.threads.iter().enumerate() {
            for step in 0..spec.steps.len() {
                let key = (thread as u32, step as u32);
                if !self.resolved.contains(&key) {
                    return Some(format!(
                        "thread {thread} step {step} is unresolved (started: {})",
                        self.started.contains(&key)
                    ));
                }
            }
        }
        None
    }

    fn digest(&self) -> u64 {
        let state = format!(
            "{:?}{:?}{:?}{}",
            self.store.rows,
            self.store.logs,
            self.store.tasks,
            self.stats.summary()
        );
        fnv(FNV_OFFSET, state.as_bytes())
    }
}

pub struct PartitionsOutcome {
    digest: u64,
    wrong: Option<String>,
    stats: PartitionsStats,
    shape: String,
}

fn build(seed: u64, mutation: PartitionsMutation) -> (World, u64, String) {
    let mut rng = Rng::derive(seed, "partitions");
    let nodes = rng.range(NODES_MIN, NODES_MAX);
    let count = NonZeroU32::new(rng.range(PARTITIONS_MIN, PARTITIONS_MAX) as u32)
        .expect("the partition range starts above zero");
    let threads = rng.range(THREADS_MIN, THREADS_MAX);
    let hands = rng.range(HANDS_MIN, HANDS_MAX);
    let fault_ticks = rng.range(FAULT_TICKS_MIN, FAULT_TICKS_MAX);
    let shape = format!(
        "nodes={nodes} partitions={} threads={threads} hands={hands} fault_ticks={fault_ticks}",
        count.get()
    );
    let mut specs = Vec::new();
    let mut by_partition: BTreeMap<Partition, Vec<u32>> = BTreeMap::new();
    for thread in 0..threads as u32 {
        let partition = thread_partition(thread, count);
        let mut ready: Vec<u64> = (0..rng.range(STEPS_MIN, STEPS_MAX))
            .map(|_| rng.below(fault_ticks) * TICK_MS)
            .collect();
        ready.sort_unstable();
        let steps = ready
            .into_iter()
            .map(|ready_at| Step {
                ready_at,
                ticks: rng.range(STEP_TICKS_MIN, STEP_TICKS_MAX),
                hand: rng.chance(HAND_STEP_PPM).then(|| rng.below(hands) as u32),
            })
            .collect();
        specs.push(Thread { partition, steps });
        by_partition.entry(partition).or_default().push(thread);
    }
    let ids: Vec<NodeId> = (0..nodes as u32).map(NodeId::new).collect();
    let hosts = (0..hands).map(|_| Some(ids[rng.pick(ids.len())])).collect();
    let world = World {
        rng,
        mutation,
        now: 0,
        count,
        store: Store::new(count),
        nodes: ids.iter().copied().map(Node::new).collect(),
        threads: specs,
        by_partition,
        hands: hosts,
        wire: Vec::new(),
        members: ids.iter().copied().collect(),
        granted: BTreeMap::new(),
        epochs: BTreeMap::new(),
        started: BTreeSet::new(),
        resolved: BTreeSet::new(),
        runs: BTreeMap::new(),
        task_claims: BTreeMap::new(),
        gone_at: BTreeMap::new(),
        gaps: Vec::new(),
        faults: Vec::new(),
        joined_at: BTreeMap::new(),
        nodes_at_start: nodes,
        stats: PartitionsStats {
            seeds: 1,
            ..PartitionsStats::default()
        },
    };
    (world, fault_ticks, shape)
}

fn run(world: &mut World, fault_ticks: u64) -> Result<(), String> {
    for _ in 0..fault_ticks {
        world.now += TICK_MS;
        world.fault()?;
        world.tick()?;
    }
    world.quiesce();
    let from = world.now;
    loop {
        let Some(problem) = world.unsettled() else {
            world.stats.settle_ms_max = world.now - from;
            return Ok(());
        };
        if world.now - from >= SETTLE_MS {
            return Err(format!(
                "invariant 5 (progress): {SETTLE_MS} virtual ms after faults stopped, {problem}"
            ));
        }
        world.now += TICK_MS;
        world.tick()?;
    }
}

fn simulate(seed: u64, mutation: PartitionsMutation) -> PartitionsOutcome {
    partitions_sim::mutate(mutation.rule());
    let (mut world, fault_ticks, shape) = build(seed, mutation);
    let wrong = run(&mut world, fault_ticks)
        .err()
        .map(|message| format!("at {} ms: {message}", world.now));
    partitions_sim::mutate(Rule::None);
    PartitionsOutcome {
        digest: world.digest(),
        wrong,
        stats: world.stats,
        shape,
    }
}

pub fn run_seed(seed: u64, mutation: PartitionsMutation) -> PartitionsOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        partitions_sim::mutate(Rule::None);
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        PartitionsOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: PartitionsStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: PartitionsMutation,
    twice: bool,
    verbose: bool,
    totals: &mut PartitionsStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={}",
            first.digest,
            first.shape,
            first.stats.summary()
        );
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: PartitionsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario partitions --seed {seed}");
    if mutation != PartitionsMutation::None {
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
    mutation: PartitionsMutation,
    totals: &PartitionsStats,
) -> ExitCode {
    for line in totals.breakdown().lines() {
        println!("TAKEOVER {line}");
    }
    println!(
        "RESULT seeds={}..{} scenario=partitions mutation={} passed={passed} failed={failed} twice={twice} stats={}",
        seeds.start,
        seeds.end,
        mutation.name(),
        totals.summary()
    );
    match mutation {
        PartitionsMutation::None if failed == 0 => ExitCode::SUCCESS,
        PartitionsMutation::None => ExitCode::FAILURE,
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
