use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nanocode_http::StatusCode;
use nanocode_http::header::HeaderMap;
use nanocode_sync::{Atom, Slot, mpsc, watch};
use serde_json::Value;

use super::requests::{HandRequests, PendingRequest};
use crate::Tool;
use crate::embedded::{
    CodeModeExecution, CodeModeHostError, CodeModeObserver, HandCalls, HostFuture,
};
use crate::remote::{
    HAND_RESTARTED, NOT_RECEIVED, PEER_CLOSED, REQUEST_IN_USE, RemoteCatalog, RemoteClient,
    RemoteContext, RemoteDelivery, RemoteFrame, RemoteLedger, RemoteLink, RemoteOperation,
    RemoteReply, RemoteRequest, RemoteTransport,
};

#[derive(Clone, Debug)]
pub struct AcceptorOptions {
    pub path: String,
    pub frame_bytes_max: usize,
    pub head_timeout: Duration,
    pub catalog_timeout: Duration,
    pub ping: Duration,
    pub heartbeat_timeout: Duration,
    pub operation_timeout: Option<Duration>,
    pub reconnect_grace: Duration,
    pub runtimes_max: usize,
    pub attaches_queued_max: usize,
}

pub struct Binding {
    pub key: String,
}

pub struct Refusal {
    pub status: StatusCode,
    pub body: String,
}

type Authorize = Box<dyn Fn(&str, &HeaderMap) -> Result<Binding, Refusal> + Send + Sync>;

pub type ResolveKey = Arc<dyn Fn() -> HostFuture<'static, Option<String>> + Send + Sync>;

pub const NOT_CONNECTED: &str =
    "the session's hand has no live socket; the operation was not dispatched";
pub const NOT_RECORDED: &str = "no request was recorded for this call, so it was never dispatched";
pub const NOT_ANSWERED: &str =
    "the hand did not answer the recorded request; its outcome is unknown";

struct Changes {
    count: AtomicU64,
    sender: watch::Sender<u64>,
}

impl Changes {
    fn bump(&self) {
        let count = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        self.sender.send_replace(count);
    }
}

struct Wire {
    link: Box<dyn RemoteLink>,
    greeting: Slot<RemoteFrame>,
    ended: watch::Sender<bool>,
}

impl Wire {
    fn end(&self) {
        self.ended.send_replace(true);
    }
}

struct Redial {
    wire: watch::Sender<Option<Arc<Wire>>>,
    grace: Duration,
}

impl Redial {
    fn current(&self) -> Option<Arc<Wire>> {
        self.wire.borrow().clone()
    }

    fn swap(&self, next: Arc<Wire>) -> bool {
        let mut old = None;
        let swapped = self.wire.send_if_modified(|wire| match wire.as_mut() {
            Some(wire) => {
                old = Some(std::mem::replace(wire, Arc::clone(&next)));
                true
            }
            None => false,
        });
        if let Some(old) = old {
            old.end();
        }
        swapped
    }

    fn close(&self) {
        if let Some(wire) = self.wire.send_replace(None) {
            wire.end();
        }
    }

    fn give_up(&self, failed: &Arc<Wire>) -> bool {
        self.wire.send_if_modified(|wire| {
            let same = wire.as_ref().is_some_and(|wire| Arc::ptr_eq(wire, failed));
            if same {
                *wire = None;
            }
            same
        })
    }

    async fn receive(&self) -> Result<Vec<RemoteFrame>, CodeModeHostError> {
        loop {
            let Some(wire) = self.current() else {
                return Err(CodeModeHostError::new(PEER_CLOSED));
            };
            if let Some(greeting) = wire.greeting.take() {
                return Ok(vec![greeting]);
            }
            let mut ended = wire.ended.subscribe();
            let received = nanocode_sync::select! {
                received = wire.link.receive() => received,
                _ = ended.wait_for(|ended| *ended) => Err(CodeModeHostError::new(PEER_CLOSED)),
            };
            let error = match received {
                Ok(frames) => return Ok(frames),
                Err(error) => error,
            };
            wire.end();
            let mut wires = self.wire.subscribe();
            let moved =
                |now: &Option<Arc<Wire>>| now.as_ref().is_none_or(|now| !Arc::ptr_eq(now, &wire));
            let next = nanocode_platform::timeout(self.grace, wires.wait_for(moved)).await;
            if !matches!(next, Ok(Ok(_))) && self.give_up(&wire) {
                return Err(error);
            }
        }
    }

    async fn send(
        &self,
        frames: Vec<(RemoteFrame, Option<String>)>,
        delivery: RemoteDelivery,
    ) -> Result<(), CodeModeHostError> {
        let Some(wire) = self.current() else {
            return Err(CodeModeHostError::unsent(PEER_CLOSED));
        };
        match wire.link.send_encoded(frames, delivery).await {
            Err(error) if !error.is_unsent() => {
                wire.end();
                Ok(())
            }
            sent => sent,
        }
    }
}

struct Shared(Arc<Redial>);

impl RemoteLink for Shared {
    fn send(
        &self,
        frames: Vec<RemoteFrame>,
        delivery: RemoteDelivery,
    ) -> HostFuture<'_, Result<(), CodeModeHostError>> {
        Box::pin(self.0.send(
            frames.into_iter().map(|frame| (frame, None)).collect(),
            delivery,
        ))
    }

    fn send_encoded(
        &self,
        frames: Vec<(RemoteFrame, Option<String>)>,
        delivery: RemoteDelivery,
    ) -> HostFuture<'_, Result<(), CodeModeHostError>> {
        Box::pin(self.0.send(frames, delivery))
    }

    fn receive(&self) -> HostFuture<'_, Result<Vec<RemoteFrame>, CodeModeHostError>> {
        Box::pin(self.0.receive())
    }

    fn close<'a>(&'a self, _: &'a str) -> HostFuture<'a, ()> {
        Box::pin(async {})
    }

    fn max_frame_bytes(&self) -> Option<usize> {
        self.0
            .current()
            .and_then(|wire| wire.link.max_frame_bytes())
    }

    fn resumes(&self) -> Option<String> {
        self.0.current().and_then(|wire| wire.link.resumes())
    }
}

struct Ledger {
    requests: Arc<dyn HandRequests>,
    key: String,
    runtime: String,
    changes: Arc<Changes>,
}

impl RemoteLedger for Ledger {
    fn sent(&self, request: RemoteRequest) -> HostFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let record = PendingRequest {
                id: request.id,
                key: self.key.clone(),
                runtime: self.runtime.clone(),
                session_id: request.session_id,
                call_id: request.call_id,
                turn_id: request.turn_id,
                reply: None,
            };
            self.requests.sent(&record).await
        })
    }

    fn replied<'a>(
        &'a self,
        id: u64,
        reply: &'a RemoteReply,
    ) -> HostFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.requests
                .replied(&self.key, &self.runtime, id, reply)
                .await?;
            self.changes.bump();
            Ok(())
        })
    }
}

#[derive(Clone)]
struct Runtime {
    generation: u64,
    client: RemoteClient,
    catalog: Arc<RemoteCatalog>,
    resumes: Option<String>,
    recorded: String,
    redial: Arc<Redial>,
    socket: Option<u64>,
}

pub struct Acceptor {
    pub(super) options: AcceptorOptions,
    pub(super) authorize: Authorize,
    requests: Arc<dyn HandRequests>,
    runtimes: Atom<HashMap<String, Runtime>>,
    sockets: AtomicU64,
    changes: Arc<Changes>,
    attached: mpsc::Sender<String>,
}

pub struct Attached {
    acceptor: Arc<Acceptor>,
    key: String,
    socket: u64,
    wire: Arc<Wire>,
}

impl Attached {
    pub async fn ended(&self) {
        let mut ended = self.wire.ended.subscribe();
        drop(ended.wait_for(|ended| *ended).await);
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.wire.end();
        self.acceptor.dropped(&self.key, self.socket);
    }
}

fn greeting(catalog: &RemoteCatalog) -> RemoteFrame {
    RemoteFrame::Catalog {
        definitions: catalog.definitions.clone(),
        name: catalog.name.clone(),
        workspace: catalog.workspace.clone(),
        runtime_id: catalog.runtime_id.clone(),
        max_frame_bytes: catalog.max_frame_bytes,
        resumable: catalog.resumable,
        tool_selection: catalog.tool_selection,
        cancels_calls: catalog.cancels_calls,
        caches_local_tools: catalog.caches_local_tools,
        environment: catalog.environment.clone(),
        capabilities: catalog.capabilities.clone(),
        tools_only: catalog.tools_only,
        version: catalog.version.clone(),
        host: catalog.host.clone(),
    }
}

fn from_hand(message: String) -> CodeModeHostError {
    if message == NOT_RECEIVED || message == REQUEST_IN_USE {
        CodeModeHostError::unsent(message)
    } else {
        CodeModeHostError::new(message)
    }
}

impl Acceptor {
    pub fn new(
        options: AcceptorOptions,
        requests: Arc<dyn HandRequests>,
        authorize: impl Fn(&str, &HeaderMap) -> Result<Binding, Refusal> + Send + Sync + 'static,
    ) -> (Arc<Self>, mpsc::Receiver<String>) {
        let (attached, attaches) = mpsc::channel(options.attaches_queued_max);
        let changes = Arc::new(Changes {
            count: AtomicU64::new(0),
            sender: watch::channel(0).0,
        });
        let acceptor = Self {
            options,
            authorize: Box::new(authorize),
            requests,
            runtimes: Atom::default(),
            sockets: AtomicU64::new(0),
            changes,
            attached,
        };
        (Arc::new(acceptor), attaches)
    }

    fn live(&self, key: &str) -> Option<(RemoteClient, Arc<RemoteCatalog>)> {
        self.runtimes.read(|runtimes| {
            let runtime = runtimes
                .get(key)
                .filter(|runtime| runtime.socket.is_some())?;
            Some((runtime.client.clone(), Arc::clone(&runtime.catalog)))
        })
    }

    #[must_use]
    pub fn connected(&self, key: &str) -> bool {
        self.live(key).is_some()
    }

    #[must_use]
    pub fn catalog(&self, key: &str) -> Option<Arc<RemoteCatalog>> {
        self.live(key).map(|(_, catalog)| catalog)
    }

    #[must_use]
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.sender.subscribe()
    }

    pub async fn settle(&self, session_id: &str) -> Result<(), String> {
        self.requests.settle(session_id).await
    }

    #[must_use]
    pub fn route(self: &Arc<Self>, resolve: ResolveKey) -> AcceptorRoute {
        AcceptorRoute {
            acceptor: Arc::clone(self),
            resolve,
            last: Atom::default(),
            routed: Atom::default(),
        }
    }

    fn announce(&self, key: &str) {
        self.changes.bump();
        if self.attached.try_send(key.to_owned()).is_err() {
            nanocode_trace::warn!(key, "hand attach not queued: attaches_queued_max");
        }
    }

    fn resume(
        &self,
        key: &str,
        wire: &Arc<Wire>,
        catalog: &Arc<RemoteCatalog>,
        socket: u64,
    ) -> bool {
        let Some(resumes) = catalog.resumes() else {
            return false;
        };
        let redial = self.runtimes.read(|runtimes| {
            let runtime = runtimes.get(key)?;
            (runtime.resumes.as_deref() == Some(resumes)).then(|| Arc::clone(&runtime.redial))
        });
        let Some(redial) = redial else {
            return false;
        };
        if !redial.swap(Arc::clone(wire)) {
            return false;
        }
        self.runtimes.update(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(key)
                && Arc::ptr_eq(&runtime.redial, &redial)
            {
                runtime.socket = Some(socket);
                runtime.catalog = Arc::clone(catalog);
            }
        });
        self.announce(key);
        true
    }

    pub async fn attach(
        self: &Arc<Self>,
        key: String,
        catalog: RemoteCatalog,
        socket: impl RemoteLink,
    ) -> Result<Attached, String> {
        let catalog = Arc::new(catalog);
        let socket_id = self.sockets.fetch_add(1, Ordering::Relaxed) + 1;
        let wire = Arc::new(Wire {
            link: Box::new(socket),
            greeting: Slot::new(Some(greeting(&catalog))),
            ended: watch::channel(false).0,
        });
        let attached = |acceptor: &Arc<Self>, wire: Arc<Wire>, key: String| Attached {
            acceptor: Arc::clone(acceptor),
            key,
            socket: socket_id,
            wire,
        };
        if self.resume(&key, &wire, &catalog, socket_id) {
            return Ok(attached(self, wire, key));
        }
        let resumes = catalog.resumes().map(str::to_owned);
        let recorded = resumes
            .clone()
            .unwrap_or_else(|| nanocode_platform::uuid_v4().to_string());
        let pending = self.requests.pending(&key, &recorded).await?;
        if self.resume(&key, &wire, &catalog, socket_id) {
            return Ok(attached(self, wire, key));
        }
        drop(wire.greeting.take());
        let redial = Arc::new(Redial {
            wire: watch::channel(Some(Arc::clone(&wire))).0,
            grace: self.options.reconnect_grace,
        });
        let ledger = Ledger {
            requests: Arc::clone(&self.requests),
            key: key.clone(),
            runtime: recorded.clone(),
            changes: Arc::clone(&self.changes),
        };
        let (client, driver, resumed) = RemoteClient::durable(
            Shared(Arc::clone(&redial)),
            self.options.operation_timeout,
            pending,
            Arc::new(ledger),
        );
        let entry = Runtime {
            generation: socket_id,
            client,
            catalog,
            resumes,
            recorded,
            redial: Arc::clone(&redial),
            socket: Some(socket_id),
        };
        let max = self.options.runtimes_max;
        let inserted = self.runtimes.update(|runtimes| {
            if !runtimes.contains_key(&key) && runtimes.len() >= max {
                return Err(());
            }
            Ok(runtimes.insert(key.clone(), entry.clone()))
        });
        let Ok(replaced) = inserted else {
            redial.close();
            return Err(format!("hand {key} refused: runtimes_max ({max})"));
        };
        if let Some(old) = replaced {
            old.redial.close();
        }
        for (_, reply) in resumed {
            nanocode_platform::spawn(async move {
                drop(reply.await);
            });
        }
        let acceptor = Arc::clone(self);
        let ended = key.clone();
        nanocode_platform::spawn(async move {
            let why = driver.await;
            acceptor.detach(&ended, socket_id);
            nanocode_trace::info!(key = %ended, %why, "hand runtime ended");
        });
        self.announce(&key);
        Ok(attached(self, wire, key))
    }

    fn dropped(&self, key: &str, socket: u64) {
        self.runtimes.update(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(key)
                && runtime.socket == Some(socket)
            {
                runtime.socket = None;
            }
        });
        self.changes.bump();
    }

    fn detach(&self, key: &str, generation: u64) {
        let removed = self.runtimes.update(|runtimes| {
            if runtimes
                .get(key)
                .is_some_and(|runtime| runtime.generation == generation)
            {
                runtimes.remove(key)
            } else {
                None
            }
        });
        if let Some(runtime) = removed {
            runtime.redial.close();
        }
        self.changes.bump();
    }

    fn recorded_runtime(&self, key: &str) -> Option<String> {
        self.runtimes
            .read(|runtimes| runtimes.get(key).map(|runtime| runtime.recorded.clone()))
    }

    async fn recover(
        &self,
        session_id: &str,
        call_id: &str,
    ) -> Result<Option<Value>, CodeModeHostError> {
        let started = nanocode_platform::now();
        let mut changes = self.changes();
        let mut away = None;
        loop {
            let record = self
                .requests
                .find(session_id, call_id)
                .await
                .map_err(CodeModeHostError::new)?;
            let Some(record) = record else {
                return Err(CodeModeHostError::unsent(NOT_RECORDED));
            };
            if let Some(reply) = record.reply {
                return reply.map_err(from_hand);
            }
            let deadline = match self.recorded_runtime(&record.key) {
                Some(runtime) if runtime == record.runtime => {
                    away = None;
                    self.options
                        .operation_timeout
                        .map(|timeout| started + timeout)
                }
                Some(_) => return Err(CodeModeHostError::new(HAND_RESTARTED)),
                None => Some(
                    *away.get_or_insert_with(nanocode_platform::now) + self.options.reconnect_grace,
                ),
            };
            let Some(deadline) = deadline else {
                drop(changes.changed().await);
                continue;
            };
            let left = deadline.saturating_duration_since(nanocode_platform::now());
            if left.is_zero() {
                return Err(CodeModeHostError::new(NOT_ANSWERED));
            }
            drop(nanocode_platform::timeout(left, changes.changed()).await);
        }
    }
}

pub struct AcceptorRoute {
    acceptor: Arc<Acceptor>,
    resolve: ResolveKey,
    last: Atom<Option<String>>,
    routed: Atom<Option<Arc<dyn HandCalls>>>,
}

impl AcceptorRoute {
    fn current(&self) -> Result<RemoteClient, CodeModeHostError> {
        let last = Option::clone(&self.last.get());
        last.and_then(|key| self.acceptor.live(&key))
            .map(|(client, _)| client)
            .ok_or_else(|| CodeModeHostError::unsent(NOT_CONNECTED))
    }

    async fn client(&self) -> Result<RemoteClient, CodeModeHostError> {
        let deadline = nanocode_platform::now() + self.acceptor.options.reconnect_grace;
        let mut changes = self.acceptor.changes();
        loop {
            if let Some(key) = (self.resolve)().await {
                let live = self.acceptor.live(&key);
                self.last.set(Some(key));
                if let Some((client, _)) = live {
                    if let Some(hands) = Option::clone(&self.routed.get()) {
                        client.route_hands(hands);
                    }
                    return Ok(client);
                }
            }
            let left = deadline.saturating_duration_since(nanocode_platform::now());
            if left.is_zero() {
                return Err(CodeModeHostError::unsent(NOT_CONNECTED));
            }
            drop(nanocode_platform::timeout(left, changes.changed()).await);
        }
    }
}

impl RemoteTransport for AcceptorRoute {
    fn supports_tool_selection(&self) -> bool {
        self.current()
            .is_ok_and(|client| client.supports_tool_selection())
    }

    fn request<'a>(
        &'a self,
        operation: RemoteOperation,
        local_tools: Vec<Arc<dyn Tool>>,
        observer: Option<&'a mut dyn CodeModeObserver>,
    ) -> HostFuture<'a, Result<Option<CodeModeExecution>, CodeModeHostError>> {
        Box::pin(async move {
            let client = self.client().await?;
            let observer = observer.map(|observer| observer as &mut dyn CodeModeObserver);
            client.request(operation, local_tools, observer).await
        })
    }

    fn call(&self, operation: RemoteOperation) -> HostFuture<'_, Result<Value, CodeModeHostError>> {
        Box::pin(async move { self.client().await?.call(operation).await })
    }

    fn recover<'a>(
        &'a self,
        context: &'a RemoteContext,
    ) -> Option<HostFuture<'a, Result<Option<Value>, CodeModeHostError>>> {
        Some(Box::pin(
            self.acceptor.recover(&context.session_id, &context.call_id),
        ))
    }

    fn adopt(&self, context: &RemoteContext, tools: Vec<Arc<dyn Tool>>) {
        if let Ok(client) = self.current() {
            client.adopt(context, tools);
        }
    }

    fn delivered(&self, context: &RemoteContext, execution: &CodeModeExecution) {
        if let Ok(client) = self.current() {
            client.delivered(context, execution);
        }
    }

    fn route_hands(&self, hands: Arc<dyn HandCalls>) {
        if let Ok(client) = self.current() {
            client.route_hands(Arc::clone(&hands));
        }
        self.routed.set(Some(hands));
    }

    fn notify(&self, operation: RemoteOperation) -> Result<(), CodeModeHostError> {
        self.current()?.notify(operation)
    }

    fn detach_turn(
        &self,
        session_id: &str,
        observer: &mut (dyn FnMut(&str) -> Box<dyn CodeModeObserver> + Send),
    ) -> Result<(), CodeModeHostError> {
        self.current()?.detach_turn(session_id, observer)
    }
}
