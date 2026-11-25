use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::sim::{Mutation, mutated};
use crate::{Change, Entry, EventId, Fold, Op, Phase, TaskId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retain {
    pub settled: usize,
    pub entries: usize,
    #[serde(default)]
    pub only: Option<(String, String)>,
    /// Events each task keeps (0: every one): past it the oldest of a task's events leave its dedupe
    /// record (seen), its replies and its said thread, so an open task that keeps hearing events
    /// holds a bounded number of them rather than every one since it was posted. A replay of an
    /// event trimmed here folds again; boards whose writers append each event once (a dense log)
    /// lose nothing by it.
    #[serde(default)]
    pub events: usize,
    /// Evicted tasks whose ids the fold keeps refusing a re-post of (0: every one): past it the earliest evicted
    /// leave the gone set once their post left the entries too, so a board that evicts a task per job holds at most
    /// this many plus the entries' rather than every id it ever evicted. A re-post of an id past it folds as a new
    /// task; a writer that must refuse one keeps its own record.
    #[serde(default)]
    pub gone: usize,
    /// Tasks labelled key = value keep at most this many events (fewer than events, or in place of
    /// it when that is 0), open or settled, as events does: a kind whose tasks hear many events no
    /// reader needs once folded (a lease's renewals) holds a few of them rather than up to events
    /// each. 0 keeps events' bound.
    #[serde(default)]
    pub label_events: Option<(String, String, usize)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    policy: Option<Retain>,
    base: usize,
    evicted: u64,
    #[serde(with = "crate::pairs")]
    settled: BTreeMap<TaskId, u64>,
    order: BTreeSet<(u64, TaskId)>,
    #[serde(with = "crate::pairs")]
    events: BTreeMap<TaskId, Vec<EventId>>,
    #[serde(with = "crate::pairs")]
    minted: BTreeMap<String, u64>,
    #[serde(with = "crate::pairs")]
    posted: BTreeMap<TaskId, usize>,
    gone: BTreeSet<TaskId>,
    #[serde(default)]
    gone_order: VecDeque<(usize, TaskId)>,
    /// Label keys each evicted task keeps (Fold::keep_labels), with its parent, while its id stays gone.
    #[serde(default)]
    keep: Vec<String>,
    #[serde(default, with = "crate::pairs")]
    kept: BTreeMap<TaskId, Evicted>,
}

/// What a fold keeps of a task it evicted, with Fold::keep_labels: the kept labels it had and its parent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evicted {
    pub labels: BTreeMap<String, String>,
    pub parent: Option<TaskId>,
}

impl Retention {
    pub const fn new() -> Self {
        Self {
            policy: None,
            base: 0,
            evicted: 0,
            settled: BTreeMap::new(),
            order: BTreeSet::new(),
            events: BTreeMap::new(),
            minted: BTreeMap::new(),
            posted: BTreeMap::new(),
            gone: BTreeSet::new(),
            gone_order: VecDeque::new(),
            keep: Vec::new(),
            kept: BTreeMap::new(),
        }
    }

    pub(crate) fn gone(&self, task: &TaskId) -> bool {
        self.gone.contains(task)
    }

    pub(crate) fn minted(&self, writer: &str) -> u64 {
        self.minted.get(writer).copied().unwrap_or(0)
    }

    pub(crate) const fn base(&self) -> usize {
        self.base
    }

    pub(crate) const fn trimmed_posts(&self) -> &BTreeMap<TaskId, usize> {
        &self.posted
    }
}

impl Fold {
    pub fn retaining(policy: Retain) -> Self {
        let mut fold = Self::new();
        fold.retention.policy = Some(policy);
        fold
    }

    pub fn set_retain(&mut self, policy: Option<Retain>) {
        let had = self.retention.policy.is_some();
        let some = policy.is_some();
        self.retention.policy = policy;
        if !some || had {
            return;
        }
        let mut events: BTreeMap<TaskId, Vec<EventId>> = BTreeMap::new();
        let mut settled = BTreeMap::new();
        for entry in &self.entries {
            let list = events.entry(entry.task.clone()).or_default();
            if list.last() != Some(&entry.event) {
                list.push(entry.event.clone());
            }
            if entry.change.terminal() {
                settled.insert(entry.task.clone(), entry.seq);
            }
        }
        self.retention.events = events;
        self.retention.settled.clear();
        self.retention.order.clear();
        for (id, seq) in settled {
            if self.tracked(&id) {
                self.retention.order.insert((seq, id.clone()));
                self.retention.settled.insert(id, seq);
            }
        }
    }

    pub const fn retain(&self) -> Option<&Retain> {
        self.retention.policy.as_ref()
    }

    /// Keeps, for each task this fold evicts from now on, its labels named in keys and its parent, for as long as
    /// its id stays gone: a reader can still tell what an evicted task was (whose, and under what) for a few dozen
    /// bytes rather than the whole task.
    pub fn keep_labels(&mut self, keys: &[&str]) {
        self.retention.keep = keys.iter().map(|key| (*key).to_owned()).collect();
    }

    /// The label keys keep_labels set.
    pub fn kept_labels(&self) -> &[String] {
        &self.retention.keep
    }

    /// Whether the fold evicted task and still refuses a re-post of it (Retain::gone).
    pub fn gone(&self, task: &TaskId) -> bool {
        self.retention.gone(task)
    }

    /// The gone tasks this fold keeps labels of (keep_labels), by id.
    pub fn evicted_tasks(&self) -> impl Iterator<Item = (&TaskId, &Evicted)> + '_ {
        self.retention.kept.iter()
    }

    pub const fn entries_base(&self) -> usize {
        self.retention.base
    }

    pub const fn entry_count(&self) -> usize {
        self.retention.base + self.entries.len()
    }

    pub fn entries_since(&self, count: usize) -> &[Entry] {
        let from = count.saturating_sub(self.retention.base);
        self.entries.get(from..).unwrap_or(&[])
    }

    pub const fn evicted(&self) -> u64 {
        self.retention.evicted
    }

    pub fn settled_count(&self) -> usize {
        self.retention.settled.len()
    }

    pub(crate) fn heard(&mut self, event: &EventId, op: &Op) {
        if self.retention.policy.is_none() {
            return;
        }
        let Some(task) = op.task() else {
            return;
        };
        if self.tasks.contains_key(task) {
            let list = self.retention.events.entry(task.clone()).or_default();
            if list.last() != Some(event) {
                list.push(event.clone());
            }
        }
    }

    pub(crate) fn retained(&mut self, from: usize, seq: u64) {
        if self.retention.policy.is_none() {
            return;
        }
        let touched = self
            .entries_since(from)
            .iter()
            .map(|entry| (entry.task.clone(), entry.event.clone()))
            .collect::<Vec<_>>();
        for (id, event) in touched {
            if self.tasks.contains_key(&id) {
                let list = self.retention.events.entry(id.clone()).or_default();
                if list.last() != Some(&event) {
                    list.push(event);
                }
            }
            self.settle(seq, &id);
            self.cap(&id);
        }
        self.evict(seq);
    }

    /// Keep task's newest policy.events events: the older ones leave seen (raising their writer's
    /// minted floor, as an eviction does), and its replies and said thread keep as many.
    fn cap(&mut self, id: &TaskId) {
        let keep = self.events_kept(id);
        if keep == 0 || mutated(Mutation::EventsUncapped) {
            return;
        }
        let Some(list) = self.retention.events.get_mut(id) else {
            return;
        };
        let over = list.len().saturating_sub(keep);
        if over == 0 {
            return;
        }
        for event in list.drain(..over).collect::<Vec<_>>() {
            self.seen.remove(&event);
            if let Some((writer, count)) = event.0.rsplit_once('.')
                && let Ok(count) = count.parse::<u64>()
            {
                let floor = self.retention.minted.entry(writer.to_owned()).or_default();
                *floor = (*floor).max(count);
            }
        }
        if let Some(task) = self.tasks.get_mut(id) {
            let over = task.replies.len().saturating_sub(keep);
            task.replies.drain(..over);
        }
        if !self.mirror.holds(id) {
            self.said.trim(&id.0, keep);
        }
    }

    /// How many events task id keeps (0: every one): policy.events, or fewer for a task with its
    /// label_events label.
    fn events_kept(&self, id: &TaskId) -> usize {
        let Some(policy) = self.retention.policy.as_ref() else {
            return 0;
        };
        let few = policy.label_events.as_ref().and_then(|(key, value, few)| {
            let labelled = self
                .tasks
                .get(id)
                .is_some_and(|task| task.spec.labels.get(key) == Some(value));
            (labelled && *few > 0).then_some(*few)
        });
        match (policy.events, few) {
            (0, Some(few)) => few,
            (events, Some(few)) => events.min(few),
            (events, None) => events,
        }
    }

    fn settle(&mut self, seq: u64, id: &TaskId) {
        let terminal = self.tracked(id);
        match (terminal, self.retention.settled.get(id).copied()) {
            (true, None) => {
                self.retention.settled.insert(id.clone(), seq);
                self.retention.order.insert((seq, id.clone()));
            }
            (false, Some(at)) => {
                self.retention.settled.remove(id);
                self.retention.order.remove(&(at, id.clone()));
            }
            _ => {}
        }
    }

    fn tracked(&self, id: &TaskId) -> bool {
        let only = self
            .retention
            .policy
            .as_ref()
            .and_then(|policy| policy.only.as_ref());
        self.tasks.get(id).is_some_and(|task| {
            task.phase.terminal()
                && only.is_none_or(|(key, value)| task.spec.labels.get(key) == Some(value))
        })
    }

    fn evictable(&self, id: &TaskId) -> bool {
        let Some(task) = self.tasks.get(id) else {
            return false;
        };
        let goal_parent = task.spec.parent.as_ref().is_some_and(|parent| {
            self.intents.holds_goal(parent)
                && self.tasks.get(parent).is_some_and(|p| !p.phase.terminal())
        });
        task.phase.terminal()
            && task.spec.terms.recur.is_none()
            && !goal_parent
            && self.children.get(id).is_none_or(BTreeSet::is_empty)
            && self.dependents.get(id).is_none_or(BTreeSet::is_empty)
            && !self.mirror.holds(id)
            && !self.marks.lanes_hold(id)
            && !self.steers.holds(id)
    }

    fn evict(&mut self, _seq: u64) {
        let Some(policy) = self.retention.policy.clone() else {
            return;
        };
        let over = self.retention.settled.len().saturating_sub(policy.settled);
        if over > 0 {
            let doomed = self
                .retention
                .order
                .iter()
                .filter(|(_, id)| self.evictable(id))
                .take(over)
                .map(|(_, id)| id.clone())
                .collect::<Vec<_>>();
            for id in doomed {
                self.forget(&id);
            }
        }
        self.trim(policy.entries);
    }

    fn forget(&mut self, id: &TaskId) {
        let Some(task) = self.tasks.remove(id) else {
            return;
        };
        let post = self
            .posted
            .get(id)
            .or_else(|| self.retention.posted.get(id))
            .copied()
            .unwrap_or(0);
        for (key, value) in &task.spec.labels {
            if let Some(values) = self.labelled.get_mut(key) {
                if let Some(ids) = values.get_mut(value) {
                    ids.remove(id);
                    if ids.is_empty() {
                        values.remove(value);
                    }
                }
                if values.is_empty() {
                    self.labelled.remove(key);
                }
            }
        }
        self.live.remove(id);
        if let Some(parent) = &task.spec.parent
            && let Some(below) = self.children.get_mut(parent)
        {
            below.remove(id);
            if below.is_empty() {
                self.children.remove(parent);
            }
        }
        self.children.remove(id);
        for need in &task.spec.needs {
            if let Some(above) = self.dependents.get_mut(need) {
                above.remove(id);
                if above.is_empty() {
                    self.dependents.remove(need);
                }
            }
        }
        self.dependents.remove(id);
        self.posted.remove(id);
        self.retention.posted.remove(id);
        self.access.forget(id);
        self.mirror.forget(id);
        self.said.forget(&id.0);
        self.intents.forget(id, task.spec.parent.as_ref());
        if let Some(at) = self.retention.settled.remove(id) {
            self.retention.order.remove(&(at, id.clone()));
        }
        for event in self.retention.events.remove(id).unwrap_or_default() {
            self.seen.remove(&event);
            if let Some((writer, count)) = event.0.rsplit_once('.')
                && let Ok(count) = count.parse::<u64>()
            {
                let floor = self.retention.minted.entry(writer.to_owned()).or_default();
                *floor = (*floor).max(count);
            }
        }
        self.retention.evicted += 1;
        if !self.retention.keep.is_empty() {
            let labels = (self.retention.keep.iter())
                .filter_map(|key| task.spec.labels.get_key_value(key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let parent = task.spec.parent.clone();
            let evicted = Evicted { labels, parent };
            self.retention.kept.insert(id.clone(), evicted);
        }
        let keep = self
            .retention
            .policy
            .as_ref()
            .map_or(0, |policy| policy.gone);
        if keep == 0 || mutated(Mutation::GoneUnbounded) {
            self.retention.gone.insert(id.clone());
            return;
        }
        if self.retention.gone_order.len() < self.retention.gone.len() {
            self.retention.gone_order = self
                .retention
                .gone
                .iter()
                .map(|id| (0, id.clone()))
                .collect();
        }
        if self.retention.gone.insert(id.clone()) {
            self.retention.gone_order.push_back((post, id.clone()));
        }
        self.shed_gone();
    }

    fn shed_gone(&mut self) {
        let keep = self
            .retention
            .policy
            .as_ref()
            .map_or(0, |policy| policy.gone);
        if keep == 0 || mutated(Mutation::GoneUnbounded) {
            return;
        }
        while self.retention.gone_order.len() > keep
            && self
                .retention
                .gone_order
                .front()
                .is_some_and(|(post, _)| *post < self.retention.base)
        {
            if let Some((_, old)) = self.retention.gone_order.pop_front() {
                self.retention.gone.remove(&old);
                self.retention.kept.remove(&old);
            }
        }
    }

    fn trim(&mut self, max: usize) {
        let over = self.entries.len().saturating_sub(max);
        if over == 0 {
            return;
        }
        let base = self.retention.base;
        for (index, entry) in self.entries[..over].iter().enumerate() {
            if entry.change == Change::Posted
                && self.posted.get(&entry.task) == Some(&(base + index))
            {
                self.retention
                    .posted
                    .insert(entry.task.clone(), base + index);
            }
        }
        self.entries.drain(..over);
        self.retention.base += over;
        self.shed_gone();
    }

    pub(crate) fn restored_posts(&self) -> BTreeMap<TaskId, usize> {
        let base = self.retention.base;
        let mut posted = self.retention.trimmed_posts().clone();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.change == Change::Posted && self.tasks.contains_key(&entry.task) {
                posted.entry(entry.task.clone()).or_insert(base + index);
            }
        }
        posted
    }

    pub(crate) fn restore_retention(&mut self) {
        if mutated(Mutation::EvictionDiffersByReplica) && !self.retention.order.is_empty() {
            let (at, id) = self
                .retention
                .order
                .iter()
                .next_back()
                .cloned()
                .expect("non-empty");
            self.retention.order.remove(&(at, id.clone()));
            self.retention.order.insert((0, id.clone()));
            self.retention.settled.insert(id, 0);
        }
    }
}

/// What a fold holds, broken down: its tasks by kind (the task's kind label, else its id's prefix) and stage
/// (a Claimed task past its lease counts as lapsed), its entries, dedupe outcomes and said messages (records sums
/// these, as Fold::records does), and its retention bookkeeping: the events it tracks per held task (they leave
/// with it), the settled order, the evicted ids it refuses a re-post of and its writers' minted floors.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Holding {
    pub tasks: BTreeMap<(String, &'static str), usize>,
    pub entries: usize,
    pub seen: usize,
    pub tracked: usize,
    pub said: usize,
    pub settled: usize,
    pub gone: usize,
    pub minted: usize,
}

impl Holding {
    pub fn records(&self) -> usize {
        self.tasks.values().sum::<usize>() + self.entries + self.seen + self.said
    }
}

impl Fold {
    pub fn holding(&self) -> Holding {
        let mut tasks: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
        for (id, task) in &self.tasks {
            let kind =
                task.spec.labels.get("kind").cloned().unwrap_or_else(|| {
                    id.0.split([':', '/']).next().unwrap_or_default().to_owned()
                });
            let stage = match &task.phase {
                Phase::Open => "open",
                Phase::Claimed { until_ms, .. } if *until_ms > self.now_ms => "held",
                Phase::Claimed { .. } => "lapsed",
                Phase::Done { .. } => "done",
                Phase::Failed { .. } => "failed",
                Phase::Cancelled { .. } => "cancelled",
                Phase::Blocked { .. } => "blocked",
            };
            *tasks.entry((kind, stage)).or_default() += 1;
        }
        let tracked = self.retention.events.values().map(Vec::len).sum();
        Holding {
            tasks,
            entries: self.entries.len(),
            seen: self.seen.len(),
            tracked,
            said: self.said.len(),
            settled: self.retention.settled.len(),
            gone: self.retention.gone.len(),
            minted: self.retention.minted.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::access::{Can, Grant, Who};
    use crate::tasks::Terms;
    use crate::{Event, EventId, Fold, NewTask, Op, Rejection, Retain, TaskId};

    fn write(fold: &mut Fold, minted: &mut u64, op: Op) -> Result<u64, Rejection> {
        *minted += 1;
        let id = EventId(format!("user:lead.{minted}"));
        let body = serde_json::to_string(&Event {
            id: id.clone(),
            by: "user:lead".to_owned(),
            at_ms: *minted,
            op,
        })
        .expect("an event encodes");
        fold.apply(fold.next(), &body).expect("the row applies");
        fold.outcome(&id).cloned().expect("an outcome")
    }

    fn post(id: &str, kind: &str) -> Op {
        Op::Post {
            task: NewTask {
                id: TaskId(id.to_owned()),
                parent: None,
                spec: id.to_owned(),
                labels: BTreeMap::from([("kind".to_owned(), kind.to_owned())]),
                needs: Vec::new(),
                terms: Terms::default(),
            },
        }
    }

    fn cancel(id: &str) -> Op {
        Op::Cancel {
            task: TaskId(id.to_owned()),
            reason: "done".to_owned(),
        }
    }

    fn board(policy: Retain, tasks: usize) -> Fold {
        let mut fold = Fold::retaining(policy);
        let mut minted = 0;
        assert!(write(&mut fold, &mut minted, Op::Charter {}).is_ok());
        let grant = Grant {
            who: vec![Who::Everyone],
            can: BTreeMap::from([("*".to_owned(), Can::ALL.to_vec())]),
        };
        assert!(write(&mut fold, &mut minted, Op::Grant { grant }).is_ok());
        for n in 0..tasks {
            let kind = if n % 2 == 0 { "land" } else { "thread" };
            let id = format!("t{n}");
            assert!(write(&mut fold, &mut minted, post(&id, kind)).is_ok());
            assert!(write(&mut fold, &mut minted, cancel(&id)).is_ok());
        }
        fold
    }

    #[test]
    fn an_open_task_keeps_its_newest_events_past_the_policy() {
        let policy = Retain {
            settled: 8,
            entries: 1_000,
            only: None,
            events: 4,
            gone: 0,
            label_events: None,
        };
        let mut fold = board(policy, 0);
        let mut minted = 2;
        assert!(write(&mut fold, &mut minted, post("open", "thread")).is_ok());
        let posted = EventId(format!("user:lead.{minted}"));
        for n in 0..50 {
            let reply = Op::Reply {
                task: TaskId("open".to_owned()),
                body: format!("line {n}"),
            };
            assert!(write(&mut fold, &mut minted, reply).is_ok());
        }
        let task = fold
            .task(&TaskId("open".to_owned()))
            .expect("the open task stays");
        assert!(!task.phase.terminal());
        let bodies: Vec<&str> = task.replies.iter().map(|(_, body)| body.as_str()).collect();
        assert_eq!(bodies, ["line 46", "line 47", "line 48", "line 49"]);
        assert!(fold.outcome(&posted).is_none(), "its post left seen");
        let last = EventId(format!("user:lead.{minted}"));
        assert!(fold.outcome(&last).is_some(), "its newest event stays");
        assert_eq!(fold.minted("user:lead"), minted, "the writer's floor holds");
        assert_eq!(fold.said.replies_to("open"), 4);
        let restored = Fold::restore(&fold.snapshot()).expect("a snapshot restores");
        assert_eq!(restored, fold);
    }

    #[test]
    fn a_labelled_task_keeps_fewer_events_open_and_settled() {
        let policy = Retain {
            settled: 8,
            entries: 1_000,
            only: None,
            events: 6,
            gone: 0,
            label_events: Some(("kind".to_owned(), "land".to_owned(), 3)),
        };
        let mut fold = board(policy, 0);
        let mut minted = 2;
        for id in ["job", "chat"] {
            let kind = if id == "job" { "land" } else { "thread" };
            assert!(write(&mut fold, &mut minted, post(id, kind)).is_ok());
            for n in 0..20 {
                let reply = Op::Reply {
                    task: TaskId(id.to_owned()),
                    body: format!("line {n}"),
                };
                assert!(write(&mut fold, &mut minted, reply).is_ok());
            }
        }
        let replies =
            |fold: &Fold, id: &str| fold.task(&TaskId(id.to_owned())).map(|t| t.replies.len());
        assert_eq!(
            replies(&fold, "job"),
            Some(3),
            "the labelled task keeps label_events"
        );
        assert_eq!(replies(&fold, "chat"), Some(6), "the rest keep events");
        assert_eq!(fold.holding().tracked, 3 + 6);
        assert!(write(&mut fold, &mut minted, cancel("job")).is_ok());
        let task = fold
            .task(&TaskId("job".to_owned()))
            .expect("settled, not evicted");
        assert!(task.phase.terminal());
        assert_eq!(fold.holding().tracked, 3 + 6, "settled it still keeps 3");
        assert_eq!(fold.minted("user:lead"), minted, "the writer's floor holds");
        let restored = Fold::restore(&fold.snapshot()).expect("a snapshot restores");
        assert_eq!(restored, fold);
    }

    #[test]
    fn evicts_the_oldest_settled_past_the_policy() {
        let policy = Retain {
            settled: 3,
            entries: 8,
            only: None,
            events: 0,
            gone: 0,
            label_events: None,
        };
        let mut fold = board(policy, 20);
        assert_eq!(
            fold.tasks().len(),
            3,
            "{:?}",
            fold.tasks().keys().collect::<Vec<_>>()
        );
        assert!(fold.task(&TaskId("t19".to_owned())).is_some());
        assert!(fold.task(&TaskId("t0".to_owned())).is_none());
        assert_eq!(fold.evicted(), 17);
        assert!(fold.entries().len() <= 8);
        assert_eq!(
            fold.entry_count(),
            fold.entries_base() + fold.entries().len()
        );
        let restored = Fold::restore(&fold.snapshot()).expect("restores");
        assert_eq!(restored, fold);
        let json = Fold::restore(fold.snapshot_json().as_bytes()).expect("restores");
        assert_eq!(json, fold);
        let mut minted = 1_000;
        assert_eq!(
            write(&mut fold, &mut minted, post("t0", "land")),
            Err(Rejection::Exists)
        );
    }

    #[test]
    fn only_evicts_tasks_with_its_label() {
        let only = Some(("kind".to_owned(), "land".to_owned()));
        let fold = board(
            Retain {
                settled: 2,
                entries: 1_000,
                only,
                events: 0,
                gone: 0,
                label_events: None,
            },
            20,
        );
        let land = fold
            .tasks()
            .values()
            .filter(|t| t.spec.labels["kind"] == "land")
            .count();
        assert_eq!(land, 2);
        assert_eq!(fold.tasks().len(), 12);
    }
}
