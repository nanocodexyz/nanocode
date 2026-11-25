use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use nanocode_board::delivery::Plan;
pub use nanocode_board::mirror::{
    Action, Author, Control, Delivery, Join, Kind, Mode, Settle, SyncOp, Synced, Target,
};
use nanocode_board::{
    Board, BoardError, EventId, NewTask, Op, Rejection, TaskId, mirror::Refusal, tasks::Terms,
};

pub mod fake;
pub mod github;
pub mod linear;
pub mod sim;
pub mod slack;

use sim::{Mutation, mutated};

#[cfg(not(target_family = "wasm"))]
pub type PlatformFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(target_family = "wasm")]
pub type PlatformFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlatformError {
    Failed(String),
    RateLimited { retry_after_ms: u64 },
}

pub(crate) fn seconds(header: &str) -> Option<u64> {
    header
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| seconds * 1000)
}

pub(crate) fn throttled(retry_after_ms: Option<u64>) -> PlatformError {
    retry_after_ms.map_or_else(
        || PlatformError::Failed("rate limited without a readable retry deadline".to_owned()),
        |retry_after_ms| PlatformError::RateLimited { retry_after_ms },
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posted {
    pub external: String,
    pub thread: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub blocks: Option<serde_json::Value>,
}

pub struct Call<'a> {
    pub to: &'a Target,
    pub sub: Option<&'a str>,
    pub marker: &'a EventId,
    pub rendered: &'a Rendered,
}

pub trait Platform: Send + Sync {
    fn post<'a>(&'a self, call: Call<'a>) -> PlatformFuture<'a, Result<Posted, PlatformError>>;

    fn edit<'a>(
        &'a self,
        call: Call<'a>,
        external: &'a str,
    ) -> PlatformFuture<'a, Result<(), PlatformError>>;

    fn delete<'a>(
        &'a self,
        to: &'a Target,
        external: &'a str,
    ) -> PlatformFuture<'a, Result<(), PlatformError>>;

    fn find<'a>(
        &'a self,
        to: &'a Target,
        marker: &'a EventId,
    ) -> PlatformFuture<'a, Result<Option<Posted>, PlatformError>>;
}

pub trait Renderer {
    fn render(&self, delivery: &Delivery) -> Rendered;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Start { task: NewTask, mode: Mode },
    Wait,
    Ignore,
}

pub trait Router {
    fn route(&self, thread: &Target, author: &Author, body: &str) -> Route;

    fn mirror(&self, task: &NewTask) -> Vec<(Target, Mode)>;
}

pub trait IdentityMap {
    fn member(&self, sync: &str, user: &str) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Opened {
        task: NewTask,
        thread: Target,
        message: String,
        mode: Mode,
    },
    Message {
        thread: Target,
        message: String,
        author: Author,
        body: String,
        context: Option<String>,
        sub: Option<String>,
        marker: Option<EventId>,
    },
    Edited {
        thread: Target,
        message: String,
        body: String,
        version: u64,
    },
    Deleted {
        thread: Target,
        message: String,
    },
    Reworded {
        thread: Target,
        author: Author,
        title: String,
        body: Option<String>,
        version: u64,
    },
    Reaction {
        thread: Target,
        message: String,
        author: Author,
        emoji: String,
    },
    Clicked {
        thread: Target,
        message: String,
        author: Author,
        control: String,
        values: BTreeMap<String, String>,
    },
    Labels {
        task: TaskId,
        set: BTreeMap<String, String>,
        unset: Vec<String>,
        own: Vec<String>,
        version: u64,
    },
    Closed {
        task: TaskId,
        outcome: Settle,
    },
    Unlinked {
        task: TaskId,
        thread: Target,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inbound {
    pub key: String,
    pub event: Event,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ingested {
    pub written: u64,
    pub skipped: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub posted: u64,
    pub edited: u64,
    pub deleted: u64,
    pub recovered: u64,
    pub failed: u64,
    pub dead: u64,
    pub unsettled: u64,
    pub limited: u64,
}

pub struct Engine<R, M, I> {
    pub renderer: R,
    pub router: M,
    pub identity: I,
    pub platforms: BTreeMap<String, Arc<dyn Platform>>,
    pub attempts: u64,
    errors: BTreeMap<(EventId, Target), String>,
    limits: BTreeMap<String, u64>,
    refunds: BTreeMap<String, SyncOp>,
}

const fn benign(rejection: &Rejection) -> bool {
    matches!(
        rejection,
        Rejection::Exists
            | Rejection::Terminal
            | Rejection::Sync(
                Refusal::Linked
                    | Refusal::Delivered
                    | Refusal::Stale
                    | Refusal::Echo
                    | Refusal::NoCopy
                    | Refusal::NotLinked(_)
            )
    )
}

impl<R: Renderer, M: Router, I: IdentityMap> Engine<R, M, I> {
    pub const fn new(
        renderer: R,
        router: M,
        identity: I,
        platforms: BTreeMap<String, Arc<dyn Platform>>,
        attempts: u64,
    ) -> Self {
        Self {
            renderer,
            router,
            identity,
            platforms,
            attempts,
            errors: BTreeMap::new(),
            limits: BTreeMap::new(),
            refunds: BTreeMap::new(),
        }
    }

    pub fn not_before(&self, sync: &str) -> Option<u64> {
        self.limits.get(sync).copied()
    }

    fn limit(&mut self, sync: &str, at_ms: u64, retry_after_ms: u64, drained: &mut Drained) {
        drained.limited += 1;
        let until = self.limits.entry(sync.to_owned()).or_default();
        *until = (*until).max(at_ms + retry_after_ms);
    }

    fn held(&self, sync: &str, at_ms: u64) -> bool {
        !mutated(Mutation::RetryAfterIgnored)
            && self.limits.get(sync).is_some_and(|until| at_ms < *until)
    }

    async fn refund(
        &mut self,
        board: &mut Board,
        key: String,
        at_ms: u64,
        op: SyncOp,
        drained: &mut Drained,
    ) -> Result<(), BoardError> {
        match board
            .submit_keyed(&key, at_ms, Op::Sync { sync: op.clone() })
            .await
        {
            Ok(_) | Err(BoardError::Rejected(_)) => Ok(()),
            Err(BoardError::Unsettled { .. } | BoardError::Store(_)) => {
                drained.unsettled += 1;
                self.refunds.insert(key, op);
                Ok(())
            }
            Err(error) => {
                self.refunds.insert(key, op);
                Err(error)
            }
        }
    }

    async fn write(
        board: &mut Board,
        key: &str,
        at_ms: u64,
        op: Op,
        done: &mut Ingested,
    ) -> Result<(), BoardError> {
        match board.submit_keyed(key, at_ms, op).await {
            Ok(_) => done.written += 1,
            Err(BoardError::Rejected(rejection)) if benign(&rejection) => done.skipped += 1,
            Err(error) => return Err(error),
        }
        Ok(())
    }

    async fn identify(
        &self,
        board: &mut Board,
        key: &str,
        at_ms: u64,
        sync: &str,
        author: &Author,
        done: &mut Ingested,
    ) -> Result<(), BoardError> {
        if author.bot {
            return Ok(());
        }
        let Some(member) = self.identity.member(sync, &author.user) else {
            return Ok(());
        };
        if board.fold().mirror().member(sync, &author.user) == Some(&member) {
            return Ok(());
        }
        let op = Op::Sync {
            sync: SyncOp::Account {
                member,
                sync: sync.to_owned(),
                user: author.user.clone(),
                linked: true,
            },
        };
        Self::write(board, &format!("{key}/account"), at_ms, op, done).await
    }

    pub async fn ingest(
        &self,
        board: &mut Board,
        at_ms: u64,
        inbound: &Inbound,
    ) -> Result<Ingested, BoardError> {
        let key = &inbound.key;
        let mut done = Ingested::default();
        match &inbound.event {
            Event::Opened {
                task,
                thread,
                message,
                mode,
            } => {
                let link = root_link(task.id.clone(), thread, message, *mode);
                Self::post_root(board, key, at_ms, task.clone(), link, &mut done).await?;
            }
            Event::Message {
                thread,
                message,
                author,
                body,
                ..
            } => match self.route(board, thread, author, body).await? {
                Route::Ignore => done.skipped += 1,
                Route::Start { task, mode } => {
                    self.identify(board, key, at_ms, &thread.sync, author, &mut done)
                        .await?;
                    let link = root_link(task.id.clone(), thread, message, mode);
                    Self::post_root(board, key, at_ms, task, link, &mut done).await?;
                }
                Route::Wait => {
                    self.identify(board, key, at_ms, &thread.sync, author, &mut done)
                        .await?;
                    let op = Op::Sync {
                        sync: echo(&inbound.event),
                    };
                    Self::write(board, key, at_ms, op, &mut done).await?;
                }
            },
            Event::Reaction { thread, author, .. } | Event::Clicked { thread, author, .. } => {
                self.identify(board, key, at_ms, &thread.sync, author, &mut done)
                    .await?;
                let op = Op::Sync {
                    sync: echo(&inbound.event),
                };
                Self::write(board, key, at_ms, op, &mut done).await?;
            }
            event => {
                let op = Op::Sync { sync: echo(event) };
                Self::write(board, key, at_ms, op, &mut done).await?;
            }
        }
        Ok(done)
    }

    async fn route(
        &self,
        board: &mut Board,
        thread: &Target,
        author: &Author,
        body: &str,
    ) -> Result<Route, BoardError> {
        if board.fold().mirror().routes(thread).is_none() && !author.bot {
            board.refresh().await?;
        }
        if board.fold().mirror().routes(thread).is_none() && !author.bot {
            return Ok(self.router.route(thread, author, body));
        }
        Ok(Route::Wait)
    }

    async fn post_root(
        board: &mut Board,
        key: &str,
        at_ms: u64,
        task: NewTask,
        link: SyncOp,
        done: &mut Ingested,
    ) -> Result<(), BoardError> {
        Self::write(
            board,
            &format!("{key}/post"),
            at_ms,
            Op::Post { task },
            done,
        )
        .await?;
        Self::write(
            board,
            &format!("{key}/link"),
            at_ms,
            Op::Sync { sync: link },
            done,
        )
        .await
    }

    pub async fn post(
        &self,
        board: &mut Board,
        key: &str,
        at_ms: u64,
        task: NewTask,
    ) -> Result<Ingested, BoardError> {
        let mut done = Ingested::default();
        let targets = self.router.mirror(&task);
        let id = task.id.clone();
        Self::write(
            board,
            &format!("{key}/post"),
            at_ms,
            Op::Post { task },
            &mut done,
        )
        .await?;
        for (target, mode) in targets {
            let link = SyncOp::Link {
                task: id.clone(),
                target: target.clone(),
                join: Join::Channel,
                mode,
            };
            Self::write(
                board,
                &format!("{key}/link/{target}"),
                at_ms,
                Op::Sync { sync: link },
                &mut done,
            )
            .await?;
        }
        Ok(done)
    }

    pub async fn apply(
        &self,
        board: &mut Board,
        key: &str,
        at_ms: u64,
        op: SyncOp,
    ) -> Result<Ingested, BoardError> {
        let mut done = Ingested::default();
        Self::write(board, key, at_ms, Op::Sync { sync: op }, &mut done).await?;
        Ok(done)
    }

    pub async fn drain(&mut self, board: &mut Board, at_ms: u64) -> Result<Drained, BoardError> {
        board.refresh().await?;
        let mut drained = Drained::default();
        for (key, op) in std::mem::take(&mut self.refunds) {
            self.refund(board, key, at_ms, op, &mut drained).await?;
        }
        for delivery in board.fold().outbox() {
            let Some(platform) = self.platforms.get(&delivery.target.sync).cloned() else {
                continue;
            };
            if self.held(&delivery.target.sync, at_ms) {
                continue;
            }
            self.deliver(board, at_ms, &*platform, &delivery, &mut drained)
                .await?;
        }
        Ok(drained)
    }

    async fn settle(
        board: &mut Board,
        key: String,
        at_ms: u64,
        op: SyncOp,
        drained: &mut Drained,
    ) -> Result<bool, BoardError> {
        match board.submit_keyed(&key, at_ms, Op::Sync { sync: op }).await {
            Ok(_) => Ok(true),
            Err(BoardError::Rejected(_)) => Ok(false),
            Err(BoardError::Unsettled { .. } | BoardError::Store(_)) => {
                drained.unsettled += 1;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    async fn deliver(
        &mut self,
        board: &mut Board,
        at_ms: u64,
        platform: &dyn Platform,
        delivery: &Delivery,
        drained: &mut Drained,
    ) -> Result<(), BoardError> {
        let lane = lane(delivery);
        let plan = delivery.plan(self.attempts);
        if plan == Plan::Orphan {
            let error = "root undelivered".to_owned();
            return Self::bury(board, &lane, at_ms, delivery, error, drained).await;
        }
        let find = match plan {
            Plan::Sweep => true,
            Plan::Retry { .. } => !mutated(Mutation::RetryGuardSkipped),
            Plan::Orphan | Plan::Exhausted | Plan::Send => false,
        };
        if find
            && self
                .recover(board, at_ms, platform, delivery, plan, drained)
                .await?
        {
            return Ok(());
        }
        if matches!(plan, Plan::Exhausted | Plan::Retry { exhausted: true }) {
            let slot = (delivery.message.clone(), delivery.target.clone());
            let error = self.errors.remove(&slot).unwrap_or_else(|| {
                format!(
                    "gave up after {} attempts",
                    delivery.attempts - delivery.throttled
                )
            });
            return Self::bury(board, &lane, at_ms, delivery, error, drained).await;
        }
        self.send(board, at_ms, platform, delivery, drained).await
    }

    async fn bury(
        board: &mut Board,
        lane: &str,
        at_ms: u64,
        delivery: &Delivery,
        error: String,
        drained: &mut Drained,
    ) -> Result<(), BoardError> {
        let dead = SyncOp::Undelivered {
            task: delivery.task.clone(),
            message: delivery.message.clone(),
            target: delivery.target.clone(),
            error,
        };
        if Self::settle(board, format!("dead:{lane}"), at_ms, dead, drained).await? {
            drained.dead += 1;
        }
        Ok(())
    }

    async fn recover(
        &mut self,
        board: &mut Board,
        at_ms: u64,
        platform: &dyn Platform,
        delivery: &Delivery,
        plan: Plan,
        drained: &mut Drained,
    ) -> Result<bool, BoardError> {
        let Delivery {
            task,
            message,
            target,
            ..
        } = delivery;
        let lane = lane(delivery);
        match platform.find(&delivery.to, message).await {
            Ok(Some(found)) => {
                let adopted = SyncOp::Synced {
                    task: task.clone(),
                    message: message.clone(),
                    target: target.clone(),
                    external: found.external,
                    revision: 0,
                    thread: found.thread,
                };
                if Self::settle(board, format!("found:{lane}"), at_ms, adopted, drained).await? {
                    drained.recovered += 1;
                }
                Ok(true)
            }
            Ok(None) if plan == Plan::Sweep => {
                let withdrawn = SyncOp::Undelivered {
                    task: task.clone(),
                    message: message.clone(),
                    target: target.clone(),
                    error: "withdrawn".to_owned(),
                };
                Self::settle(board, format!("dead:{lane}"), at_ms, withdrawn, drained).await?;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(PlatformError::RateLimited { retry_after_ms }) => {
                self.limit(&target.sync, at_ms, retry_after_ms, drained);
                Ok(true)
            }
            Err(PlatformError::Failed(error)) => {
                self.errors.insert((message.clone(), target.clone()), error);
                drained.failed += 1;
                Ok(true)
            }
        }
    }

    async fn send(
        &mut self,
        board: &mut Board,
        at_ms: u64,
        platform: &dyn Platform,
        delivery: &Delivery,
        drained: &mut Drained,
    ) -> Result<(), BoardError> {
        let attempt = SyncOp::Attempt {
            task: delivery.task.clone(),
            message: delivery.message.clone(),
            target: delivery.target.clone(),
            revision: delivery.revision,
        };
        let key = format!("try:{}:{}", lane(delivery), delivery.attempts);
        if !mutated(Mutation::AttemptAfterCall)
            && !Self::settle(board, key.clone(), at_ms, attempt.clone(), drained).await?
        {
            return Ok(());
        }
        let rendered = self.renderer.render(delivery);
        let call = Call {
            to: &delivery.to,
            sub: delivery.sub.as_deref(),
            marker: &delivery.message,
            rendered: &rendered,
        };
        let result = match &delivery.action {
            Action::Post { .. } => platform.post(call).await.map(Some),
            Action::Edit { external } => platform.edit(call, external).await.map(|()| None),
            Action::Delete { external } => {
                platform.delete(&delivery.to, external).await.map(|()| None)
            }
            Action::Sweep | Action::Orphan => return Ok(()),
        };
        if mutated(Mutation::AttemptAfterCall) {
            Self::settle(board, key, at_ms, attempt, drained).await?;
        }
        self.record(board, at_ms, delivery, result, drained).await
    }

    async fn record(
        &mut self,
        board: &mut Board,
        at_ms: u64,
        delivery: &Delivery,
        result: Result<Option<Posted>, PlatformError>,
        drained: &mut Drained,
    ) -> Result<(), BoardError> {
        let Delivery {
            task,
            message,
            target,
            revision,
            attempts,
            ..
        } = delivery;
        let lane = lane(delivery);
        let synced = |external: String, thread: Option<String>| SyncOp::Synced {
            task: task.clone(),
            message: message.clone(),
            target: target.clone(),
            external,
            revision: *revision,
            thread,
        };
        match (result, &delivery.action) {
            (Ok(Some(posted)), _) => {
                let ok = synced(posted.external, posted.thread);
                if Self::settle(board, format!("ok:{lane}"), at_ms, ok, drained).await? {
                    drained.posted += 1;
                }
            }
            (Ok(None), Action::Edit { external }) => {
                let ok = synced(external.clone(), None);
                if Self::settle(board, format!("ok:{lane}"), at_ms, ok, drained).await? {
                    drained.edited += 1;
                }
            }
            (Ok(None), _) => {
                let removed = SyncOp::Removed {
                    task: task.clone(),
                    message: message.clone(),
                    target: target.clone(),
                };
                if Self::settle(board, format!("gone:{lane}"), at_ms, removed, drained).await? {
                    drained.deleted += 1;
                }
            }
            (Err(PlatformError::RateLimited { retry_after_ms }), _) => {
                self.limit(&target.sync, at_ms, retry_after_ms, drained);
                if !mutated(Mutation::RateLimitCountsAttempt) {
                    let refund = SyncOp::Throttled {
                        task: task.clone(),
                        message: message.clone(),
                        target: target.clone(),
                        revision: *revision,
                    };
                    let key = format!("wait:{lane}:{attempts}");
                    self.refund(board, key, at_ms, refund, drained).await?;
                }
            }
            (Err(PlatformError::Failed(error)), _) => {
                self.errors.insert((message.clone(), target.clone()), error);
                drained.failed += 1;
            }
        }
        Ok(())
    }
}

fn lane(delivery: &Delivery) -> String {
    format!(
        "{}:{}:{}",
        delivery.message, delivery.target, delivery.revision
    )
}

fn root_link(task: TaskId, thread: &Target, message: &str, mode: Mode) -> SyncOp {
    SyncOp::Link {
        task,
        target: thread.clone(),
        join: Join::Root {
            message: message.to_owned(),
        },
        mode,
    }
}

fn echo(event: &Event) -> SyncOp {
    match event {
        Event::Message {
            thread,
            message,
            author,
            body,
            context,
            sub,
            marker,
        } => SyncOp::Import {
            thread: thread.clone(),
            message: message.clone(),
            author: author.clone(),
            body: body.clone(),
            context: context.clone(),
            sub: sub.clone(),
            marker: marker.clone(),
        },
        Event::Edited {
            thread,
            message,
            body,
            version,
        } => SyncOp::Edit {
            thread: thread.clone(),
            message: message.clone(),
            body: body.clone(),
            version: *version,
        },
        Event::Deleted { thread, message } => SyncOp::Delete {
            thread: thread.clone(),
            message: message.clone(),
        },
        Event::Reworded {
            thread,
            author,
            title,
            body,
            version,
        } => SyncOp::Reword {
            thread: thread.clone(),
            author: author.clone(),
            title: title.clone(),
            body: body.clone(),
            version: *version,
        },
        event => echo_act(event),
    }
}

fn echo_act(event: &Event) -> SyncOp {
    match event {
        Event::Reaction {
            thread,
            message,
            author,
            emoji,
        } => SyncOp::React {
            thread: thread.clone(),
            message: message.clone(),
            author: author.clone(),
            emoji: emoji.clone(),
        },
        Event::Clicked {
            thread,
            message,
            author,
            control,
            values,
        } => SyncOp::Act {
            thread: thread.clone(),
            message: message.clone(),
            author: author.clone(),
            control: control.clone(),
            values: values.clone(),
        },
        event => echo_task(event),
    }
}

fn echo_task(event: &Event) -> SyncOp {
    match event {
        Event::Labels {
            task,
            set,
            unset,
            own,
            version,
        } => SyncOp::Label {
            task: task.clone(),
            set: set.clone(),
            unset: unset.iter().cloned().collect(),
            own: own.clone(),
            version: *version,
        },
        Event::Closed { task, outcome } => SyncOp::Settle {
            task: task.clone(),
            outcome: outcome.clone(),
        },
        Event::Unlinked { task, thread } => SyncOp::Unlink {
            task: task.clone(),
            target: thread.clone(),
        },
        Event::Opened { .. }
        | Event::Message { .. }
        | Event::Edited { .. }
        | Event::Deleted { .. }
        | Event::Reworded { .. }
        | Event::Reaction { .. }
        | Event::Clicked { .. } => unreachable!("opened events post a task, not a sync op"),
    }
}

pub struct Plain;

fn mention(sync: &str, user: &str) -> String {
    match sync {
        "slack" => format!("<@{user}>"),
        _ => format!("@{user}"),
    }
}

impl Renderer for Plain {
    fn render(&self, delivery: &Delivery) -> Rendered {
        let mut text = delivery.body.clone();
        for (member, user) in &delivery.mentions {
            if let Some(user) = user {
                text = text.replace(&format!("@{member}"), &mention(&delivery.target.sync, user));
            }
        }
        let text = match delivery.kind {
            Kind::Status => format!("status: {text}"),
            Kind::Root | Kind::Reply | Kind::Final => format!("{}: {text}", delivery.author),
        };
        let blocks = (!delivery.controls.is_empty() && delivery.target.sync == "slack")
            .then(|| slack::blocks(&text, &delivery.controls));
        let text = if blocks.is_none() && !delivery.controls.is_empty() {
            let labels = delivery.controls.iter().map(|control| match control {
                Control::Button { label, .. } | Control::Form { label, .. } => format!("[{label}]"),
            });
            format!("{text}\n{}", labels.collect::<Vec<_>>().join(" "))
        } else {
            text
        };
        Rendered { text, blocks }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Accounts(pub BTreeMap<(String, String), String>);

impl IdentityMap for Accounts {
    fn member(&self, sync: &str, user: &str) -> Option<String> {
        self.0.get(&(sync.to_owned(), user.to_owned())).cloned()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Channels {
    pub mirrored: BTreeMap<Target, Mode>,
    pub bot: BTreeMap<String, String>,
    pub start: bool,
}

impl Router for Channels {
    fn route(&self, thread: &Target, author: &Author, body: &str) -> Route {
        let channel = thread
            .id
            .split_once(':')
            .map_or(thread.id.as_str(), |(channel, _)| channel);
        let channel = Target::new(&thread.sync, channel);
        let mentioned = self
            .bot
            .get(&thread.sync)
            .is_some_and(|bot| nanocode_board::mirror::mentions(body).contains(bot));
        match self.mirrored.get(&channel) {
            Some(mode) if self.start && channel == *thread || mentioned => Route::Start {
                task: NewTask {
                    id: TaskId(format!("{}/{}", thread.sync, thread.id)),
                    parent: None,
                    spec: body.to_owned(),
                    labels: BTreeMap::from([
                        ("source".to_owned(), thread.sync.clone()),
                        ("author".to_owned(), author.name.clone()),
                    ]),
                    needs: Vec::new(),
                    terms: Terms::default(),
                },
                mode: *mode,
            },
            Some(_) => Route::Wait,
            None if mentioned => Route::Start {
                task: NewTask {
                    id: TaskId(format!("{}/{}", thread.sync, thread.id)),
                    parent: None,
                    spec: body.to_owned(),
                    labels: BTreeMap::from([("source".to_owned(), thread.sync.clone())]),
                    needs: Vec::new(),
                    terms: Terms::default(),
                },
                mode: Mode::Status,
            },
            None => Route::Ignore,
        }
    }

    fn mirror(&self, _task: &NewTask) -> Vec<(Target, Mode)> {
        self.mirrored
            .iter()
            .map(|(target, mode)| (target.clone(), *mode))
            .collect()
    }
}
