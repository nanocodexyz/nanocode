use super::{
    capacity::{Acquire, Capacity, TurnCapacity, Waiter},
    model::{
        AgentContext, AgentId, AgentMessage, MessageDisposition, MessageId, MessagePriority,
        MessagePurpose, MessageSender,
    },
    platform::{self, Task, TaskError},
    runtime::{
        DelegationChange, Hibernated, Registry, completion_instructions, completion_reminder,
        follow_up_reminder,
    },
};
use nanocode_agent_core::input::Prompt;
use nanocode_agent_core::{
    Agent, NanocodeError, Result as AgentResult, SessionCheckpoint, TurnControl, TurnResult,
    TurnUsage,
};
use nanocode_sync::{mpsc, oneshot};
use nanocode_trace::Instrument;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};

mod turns;

const COMMAND_CAPACITY: usize = 8;

const SUBMITTED_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone)]
pub(super) struct HarnessHandle {
    commands: mpsc::Sender<Box<HarnessCommand>>,
    deferred: mpsc::UnboundedSender<Box<DeliveryCommand>>,
    urgent: mpsc::UnboundedSender<Box<DeliveryCommand>>,
    side_questions: Arc<AtomicUsize>,
}

pub(super) struct SideQuestionLease(Arc<AtomicUsize>);

impl Drop for SideQuestionLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct DeliveryCommand {
    message: AgentMessage,
    committed: Option<oneshot::Receiver<()>>,
    restored: bool,
    response: oneshot::Sender<std::io::Result<MessageDisposition>>,
}

impl DeliveryCommand {
    async fn wait_for_commit(&mut self) -> bool {
        let Some(committed) = self.committed.take() else {
            return true;
        };
        committed.await.is_ok()
    }
}

pub(super) struct EnqueuedDelivery {
    committed: oneshot::Sender<()>,
    response: oneshot::Receiver<std::io::Result<MessageDisposition>>,
}

impl EnqueuedDelivery {
    pub(super) async fn release(self) -> std::io::Result<MessageDisposition> {
        self.committed
            .send(())
            .map_err(|_| std::io::Error::other("subagent harness stopped before delivery"))?;
        self.response
            .await
            .map_err(|_| std::io::Error::other("subagent harness stopped before responding"))?
    }
}

enum HarnessCommand {
    Snapshot {
        response: oneshot::Sender<std::io::Result<SessionCheckpoint>>,
    },
    Agent {
        response: oneshot::Sender<std::io::Result<Agent>>,
    },
    Start {
        prompt: String,
        capacity: TurnCapacity,
        response: oneshot::Sender<std::io::Result<()>>,
    },
    Interrupt {
        response: oneshot::Sender<std::io::Result<()>>,
    },
    Submitted,
    Close {
        response: oneshot::Sender<std::io::Result<()>>,
    },
    Resume {
        hibernated: Hibernated,
        capacity: TurnCapacity,
        interrupted: bool,
        response: oneshot::Sender<std::io::Result<()>>,
    },
}

struct Harness {
    root_session_id: String,
    id: AgentId,
    context: Option<AgentContext>,
    agent: Option<Agent>,
    active: Option<ActiveTurn>,
    commands: mpsc::Receiver<Box<HarnessCommand>>,
    deferred: mpsc::UnboundedReceiver<Box<DeliveryCommand>>,
    urgent: mpsc::UnboundedReceiver<Box<DeliveryCommand>>,
    pending_deferred: VecDeque<AgentMessage>,
    pending_urgent: VecDeque<AgentMessage>,
    output_schema: String,
    contract: Contract,
    capacity: Capacity,
    waiting: Option<Acquire>,
    registry: Weak<Registry>,
    parked: u64,
    submitted: Option<nanocode_platform::Instant>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Started {
    Prompt,
    RecoveryOnly,
}

enum Contract {
    Pending { assignment: Option<String> },
    Delivered,
}

struct ActiveTurn {
    control: TurnControl,
    result: Task<AgentResult<TurnResult>>,
    _capacity: TurnCapacity,
}

enum HarnessEvent {
    Command(Option<Box<HarnessCommand>>),
    Deferred(Option<Box<DeliveryCommand>>),
    Urgent(Option<Box<DeliveryCommand>>),
    CapacityAcquired(TurnCapacity),
    TurnFinished(Result<AgentResult<TurnResult>, TaskError>),
    Parked(u64),
    SubmittedGrace,
}

impl HarnessHandle {
    pub(super) async fn snapshot(&self) -> std::io::Result<SessionCheckpoint> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Box::new(HarnessCommand::Snapshot { response }))
            .await
            .map_err(|_| std::io::Error::other("subagent harness is closed"))?;
        result
            .await
            .map_err(|_| std::io::Error::other("subagent snapshot interrupted"))?
    }

    pub(super) fn lease_side_question(&self) -> SideQuestionLease {
        self.side_questions.fetch_add(1, Ordering::SeqCst);
        SideQuestionLease(Arc::clone(&self.side_questions))
    }

    pub(super) fn answering_side_questions(&self) -> bool {
        self.side_questions.load(Ordering::SeqCst) > 0
    }

    pub(super) async fn agent(&self) -> std::io::Result<Agent> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Box::new(HarnessCommand::Agent { response }))
            .await
            .map_err(|_| std::io::Error::other("subagent harness is closed"))?;
        result
            .await
            .map_err(|_| std::io::Error::other("subagent harness stopped before responding"))?
    }

    pub(super) async fn start(
        &self,
        prompt: String,
        capacity: TurnCapacity,
    ) -> std::io::Result<()> {
        self.request(|response| HarnessCommand::Start {
            prompt,
            capacity,
            response,
        })
        .await
    }

    pub(super) fn submitted(&self) {
        drop(self.commands.try_send(Box::new(HarnessCommand::Submitted)));
    }

    pub(super) async fn interrupt(&self) -> std::io::Result<()> {
        self.request(|response| HarnessCommand::Interrupt { response })
            .await
    }

    pub(super) async fn close(&self) -> std::io::Result<()> {
        self.request(|response| HarnessCommand::Close { response })
            .await
    }

    pub(super) async fn resume(
        &self,
        hibernated: Hibernated,
        capacity: TurnCapacity,
        interrupted: bool,
    ) -> std::io::Result<()> {
        self.request(|response| HarnessCommand::Resume {
            hibernated,
            capacity,
            interrupted,
            response,
        })
        .await
    }

    pub(super) fn enqueue_delivery(
        &self,
        message: AgentMessage,
    ) -> std::io::Result<EnqueuedDelivery> {
        let (response, result) = oneshot::channel();
        let (committed, wait_for_commit) = oneshot::channel();
        self.send_delivery(DeliveryCommand {
            message,
            committed: Some(wait_for_commit),
            restored: false,
            response,
        })?;
        Ok(EnqueuedDelivery {
            committed,
            response: result,
        })
    }

    pub(super) async fn redeliver(
        &self,
        message: AgentMessage,
    ) -> std::io::Result<MessageDisposition> {
        let (response, result) = oneshot::channel();
        self.send_delivery(DeliveryCommand {
            message,
            committed: None,
            restored: true,
            response,
        })?;
        result
            .await
            .map_err(|_| std::io::Error::other("subagent harness stopped before responding"))?
    }

    fn send_delivery(&self, command: DeliveryCommand) -> std::io::Result<()> {
        let sender = match command.message.priority {
            MessagePriority::Deferred => &self.deferred,
            MessagePriority::Urgent => &self.urgent,
        };
        sender
            .send(Box::new(command))
            .map_err(|_| std::io::Error::other("subagent harness is closed"))
    }

    async fn request(
        &self,
        command: impl FnOnce(oneshot::Sender<std::io::Result<()>>) -> HarnessCommand,
    ) -> std::io::Result<()> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Box::new(command(response)))
            .await
            .map_err(|_| std::io::Error::other("subagent harness is closed"))?;
        result
            .await
            .map_err(|_| std::io::Error::other("subagent harness stopped before responding"))?
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn(
    root_session_id: String,
    id: AgentId,
    context: Option<AgentContext>,
    agent: Agent,
    capacity: Capacity,
    registry: Weak<Registry>,
    output_schema: String,
    rehydrated_assignment: Option<String>,
) -> (HarnessHandle, Task<()>) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (deferred, deferred_receiver) = mpsc::unbounded_channel();
    let (urgent, urgent_receiver) = mpsc::unbounded_channel();
    let handle = HarnessHandle {
        commands,
        deferred,
        urgent,
        side_questions: Arc::new(AtomicUsize::new(0)),
    };
    let span = nanocode_trace::info_span!(
        "subagent.harness",
        session.root_id = root_session_id.as_str(),
        session.id = agent.session_id()
    );
    let task = platform::spawn(
        Harness {
            root_session_id,
            id,
            context,
            agent: Some(agent),
            active: None,
            commands: receiver,
            deferred: deferred_receiver,
            urgent: urgent_receiver,
            pending_deferred: VecDeque::new(),
            pending_urgent: VecDeque::new(),
            output_schema,
            contract: Contract::Pending {
                assignment: rehydrated_assignment,
            },
            capacity,
            waiting: None,
            registry,
            parked: 0,
            submitted: None,
        }
        .run()
        .instrument(span),
    );
    (handle, task)
}

impl Harness {
    async fn run(mut self) {
        loop {
            match self.next_event().await {
                HarnessEvent::Command(Some(command)) => {
                    if self.handle(*command).await {
                        return;
                    }
                }
                HarnessEvent::Command(None) => {
                    self.fail_pending("subagent harness stopped").await;
                    drop(self.close().await);
                    return;
                }
                HarnessEvent::Deferred(Some(command)) => {
                    self.accept_delivery(*command, MessagePriority::Deferred)
                        .await;
                }
                HarnessEvent::Urgent(Some(command)) => {
                    self.accept_delivery(*command, MessagePriority::Urgent)
                        .await;
                }
                HarnessEvent::Deferred(None) | HarnessEvent::Urgent(None) => {}
                HarnessEvent::CapacityAcquired(capacity) => {
                    self.waiting = None;
                    self.start_pending(Some(capacity)).await;
                }
                HarnessEvent::TurnFinished(result) => self.turn_finished(result).await,
                HarnessEvent::SubmittedGrace => self.end_submitted_turn().await,
                HarnessEvent::Parked(entries) => {
                    self.parked = entries;
                    if self.hibernate().await {
                        return;
                    }
                }
            }
        }
    }

    async fn hibernate(&mut self) -> bool {
        if !self.deferred.is_empty() || !self.urgent.is_empty() || !self.commands.is_empty() {
            return false;
        }
        let (Some(registry), Some(agent)) = (self.registry.upgrade(), self.agent.clone()) else {
            return false;
        };
        let Some(lock) = registry
            .begin_hibernation(&self.root_session_id, self.id)
            .await
        else {
            return false;
        };
        if !self.deferred.is_empty() || !self.urgent.is_empty() || !self.commands.is_empty() {
            return false;
        }
        let Some(stored) = registry.hibernation_runtime(&self.root_session_id, self.id) else {
            return false;
        };
        let Ok(Some(call)) = agent.park().await else {
            return false;
        };
        let Some(mut active) = self.active.take() else {
            return false;
        };
        drop((&mut active.result).await);
        drop(active);
        self.agent = None;
        drop(agent);
        registry
            .hibernated(
                &self.root_session_id,
                self.id,
                stored,
                Hibernated {
                    call,
                    capacity: None,
                    urgent: std::mem::take(&mut self.pending_urgent),
                    deferred: std::mem::take(&mut self.pending_deferred),
                },
                lock,
            )
            .await;
        true
    }

    async fn resume(
        &mut self,
        hibernated: Hibernated,
        capacity: TurnCapacity,
        interrupted: bool,
    ) -> std::io::Result<()> {
        let Hibernated {
            call,
            capacity: _,
            urgent,
            deferred,
        } = hibernated;
        self.pending_urgent = urgent;
        self.pending_deferred = deferred;
        let interrupted = match self.registry.upgrade() {
            Some(registry) => {
                interrupted
                    || registry
                        .turn_interrupted(&self.root_session_id, self.id)
                        .await
            }
            None => interrupted,
        };
        let resumed = self
            .resume_turn((!interrupted).then_some(call), capacity)
            .await;
        if let Err(error) = &resumed {
            self.publish_turn_result(Ok(Err(NanocodeError::InvalidRequest(format!(
                "agent {} could not wake: {error}",
                self.id
            )))))
            .await;
            self.start_pending(None).await;
            return resumed;
        }
        if interrupted {
            self.reject_waiting_deliveries("message rejected by agent interruption")
                .await;
            self.fail_pending("message cancelled by agent interruption")
                .await;
            return self.stop_active().await;
        }
        resumed
    }

    async fn resume_turn(
        &mut self,
        call: Option<Arc<dyn nanocode_agent_core::backend::ParkedCall>>,
        capacity: TurnCapacity,
    ) -> std::io::Result<()> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| std::io::Error::other("subagent runtime is closed"))?;
        let agent = self
            .agent
            .as_ref()
            .ok_or_else(|| std::io::Error::other(format!("agent {} is closed", self.id)))?;
        let (id, input) = registry
            .pending_turn(&self.root_session_id, self.id, agent.session_id())
            .await?
            .ok_or_else(|| std::io::Error::other("the hibernated turn is not pending"))?;
        let recovered = agent
            .decode_durable_prompt(input)
            .map_err(std::io::Error::other)?;
        if let Some(call) = call {
            agent.unpark(call);
        }
        let turn = agent
            .prompt(nanocode_agent_core::PromptRequest::new(recovered).request_id(id))
            .await
            .map_err(std::io::Error::other)?;
        self.contract = Contract::Delivered;
        self.active = Some(ActiveTurn {
            control: turn.control(),
            result: platform::spawn(turn.result()),
            _capacity: capacity,
        });
        Ok(())
    }

    async fn next_event(&mut self) -> HarnessEvent {
        let Some(active) = self.active.as_mut() else {
            self.submitted = None;
            if self.pending_deferred.is_empty() && self.pending_urgent.is_empty() {
                return nanocode_sync::select! {
                    command = self.commands.recv() => HarnessEvent::Command(command),
                    urgent = self.urgent.recv() => HarnessEvent::Urgent(urgent),
                    deferred = self.deferred.recv() => HarnessEvent::Deferred(deferred),
                };
            }
            let waiting = self
                .waiting
                .get_or_insert_with(|| self.capacity.acquire(Waiter::Resident));
            return nanocode_sync::select! {
                command = self.commands.recv() => HarnessEvent::Command(command),
                urgent = self.urgent.recv() => HarnessEvent::Urgent(urgent),
                deferred = self.deferred.recv() => HarnessEvent::Deferred(deferred),
                capacity = waiting => HarnessEvent::CapacityAcquired(capacity),
            };
        };
        let parked = match (&self.agent, self.registry.upgrade()) {
            (Some(agent), Some(registry)) if registry.hibernates() => {
                Some(agent.parked(self.parked))
            }
            _ => None,
        };
        let parked = async {
            match parked {
                Some(parked) => parked.await,
                None => std::future::pending().await,
            }
        };
        let submitted = self.submitted;
        let grace = async move {
            match submitted {
                Some(deadline) => nanocode_platform::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        nanocode_sync::select! {
            command = self.commands.recv() => HarnessEvent::Command(command),
            urgent = self.urgent.recv() => HarnessEvent::Urgent(urgent),

            result = &mut active.result => HarnessEvent::TurnFinished(result),
            deferred = self.deferred.recv() => HarnessEvent::Deferred(deferred),
            entries = parked => HarnessEvent::Parked(entries),
            () = grace => HarnessEvent::SubmittedGrace,
        }
    }

    async fn handle(&mut self, command: HarnessCommand) -> bool {
        match command {
            HarnessCommand::Snapshot { response } => {
                let result = match &self.agent {
                    Some(agent) => agent.checkpoint().await.map_err(std::io::Error::other),
                    None => Err(std::io::Error::other("subagent runtime is unloaded")),
                };
                let _ = response.send(result);
                false
            }
            HarnessCommand::Agent { response } => {
                let _ =
                    response.send(self.agent.clone().ok_or_else(|| {
                        std::io::Error::other(format!("agent {} is closed", self.id))
                    }));
                false
            }
            HarnessCommand::Start {
                prompt,
                capacity,
                response,
            } => {
                let started = self.start_turn(prompt, capacity, false, true).await;
                let _ = response.send(started.and_then(|started| match started {
                    Started::Prompt => Ok(()),
                    Started::RecoveryOnly => Err(std::io::Error::other(format!(
                        "agent {} resumed a recovered turn that does not take the prompt",
                        self.id
                    ))),
                }));
                false
            }
            HarnessCommand::Interrupt { response } => {
                {
                    self.reject_waiting_deliveries("message rejected by agent interruption")
                        .await;
                    self.fail_pending("message cancelled by agent interruption")
                        .await;
                }
                let _ = response.send(self.stop_active().await);
                false
            }
            HarnessCommand::Submitted => {
                if self.active.is_some() {
                    self.submitted
                        .get_or_insert_with(|| nanocode_platform::now() + SUBMITTED_GRACE);
                }
                false
            }
            HarnessCommand::Close { response } => {
                {
                    self.reject_waiting_deliveries("message rejected because the agent closed")
                        .await;
                    self.fail_pending("message cancelled because the agent closed")
                        .await;
                }
                let result = self.close().await;
                let _ = response.send(result);
                true
            }
            HarnessCommand::Resume {
                hibernated,
                capacity,
                interrupted,
                response,
            } => {
                let _ = response.send(self.resume(hibernated, capacity, interrupted).await);
                false
            }
        }
    }

    async fn accept_delivery(&mut self, command: DeliveryCommand, priority: MessagePriority) {
        turns::accept_delivery(self, command, priority).await
    }

    async fn queue_delivery(&mut self, command: DeliveryCommand, priority: MessagePriority) {
        let queue = match priority {
            MessagePriority::Deferred => &mut self.pending_deferred,
            MessagePriority::Urgent => &mut self.pending_urgent,
        };
        let id = command.message.id;
        queue.push_back(command.message);
        self.admit(id, command.response, MessageDisposition::Queued)
            .await;
    }

    async fn start_pending(&mut self, mut granted: Option<TurnCapacity>) {
        while self.active.is_none() {
            if self.pending_urgent.is_empty() && self.pending_deferred.is_empty() {
                return;
            }
            let capacity = match granted.take() {
                Some(capacity) => capacity,
                None => match self.capacity.reserve() {
                    Ok(capacity) => capacity,
                    Err(_) => return,
                },
            };
            self.waiting = None;
            let batch = self.take_pending_batch();
            let delegation = self.begin_delegation(batch[0].id).await;
            let prompt = batch
                .iter()
                .map(AgentMessage::prompt)
                .collect::<Vec<_>>()
                .join("\n\n");
            match self
                .start_turn(prompt, capacity, from_peers(&batch), false)
                .await
            {
                Ok(Started::RecoveryOnly) => {
                    self.rollback_delegation(delegation).await;
                    let (resumed, batch): (Vec<_>, Vec<_>) = batch.into_iter().partition(resumes);
                    if let Some(registry) = self.registry.upgrade() {
                        for message in &resumed {
                            registry
                                .message_delivered(
                                    &self.root_session_id,
                                    message.id,
                                    MessageDisposition::Started,
                                )
                                .await;
                        }
                    }
                    for message in batch.into_iter().rev() {
                        match message.priority {
                            MessagePriority::Deferred => self.pending_deferred.push_front(message),
                            MessagePriority::Urgent => self.pending_urgent.push_front(message),
                        }
                    }
                }
                Ok(Started::Prompt) => {
                    if let Some(registry) = self.registry.upgrade() {
                        for message in &batch {
                            registry
                                .message_delivered(
                                    &self.root_session_id,
                                    message.id,
                                    MessageDisposition::Started,
                                )
                                .await;
                        }
                    }
                }
                Err(error) => {
                    self.rollback_delegation(delegation).await;
                    for message in &batch {
                        self.publish_message_failure(message.id, error.to_string())
                            .await;
                    }
                }
            }
        }
    }

    fn take_pending_batch(&mut self) -> Vec<AgentMessage> {
        let mut batch = Vec::new();
        while let Some(message) = self
            .pending_urgent
            .front()
            .or_else(|| self.pending_deferred.front())
        {
            let delegates = message.purpose == MessagePurpose::Delegate;
            if delegates && !batch.is_empty() {
                break;
            }
            let message = if self.pending_urgent.is_empty() {
                self.pending_deferred.pop_front()
            } else {
                self.pending_urgent.pop_front()
            }
            .expect("a pending message should still exist");
            batch.push(message);
            if delegates {
                break;
            }
        }
        batch
    }

    async fn fail_pending(&mut self, reason: &str) {
        let pending = self
            .pending_urgent
            .drain(..)
            .chain(self.pending_deferred.drain(..))
            .map(|message| message.id)
            .collect::<Vec<_>>();
        for id in pending {
            self.publish_message_failure(id, reason.to_owned()).await;
        }
    }

    async fn reject_waiting_deliveries(&mut self, reason: &str) {
        while let Ok(command) = self.urgent.try_recv() {
            self.reject(*command, reason.to_owned()).await;
        }
        while let Ok(command) = self.deferred.try_recv() {
            self.reject(*command, reason.to_owned()).await;
        }
    }

    async fn reject(&self, mut command: DeliveryCommand, reason: String) {
        if !command.wait_for_commit().await {
            return;
        }
        if !command.restored
            && let Some(registry) = self.registry.upgrade()
        {
            registry
                .message_rejected(&self.root_session_id, command.message.id)
                .await;
        }
        let _ = command.response.send(Err(std::io::Error::other(reason)));
    }

    async fn publish_message_failure(&self, id: MessageId, error: String) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .message_failed(&self.root_session_id, id, error)
                .await;
        }
    }

    async fn admit(
        &self,
        id: MessageId,
        response: oneshot::Sender<std::io::Result<MessageDisposition>>,
        disposition: MessageDisposition,
    ) {
        let Some(registry) = self.registry.upgrade() else {
            let _ = response.send(Err(std::io::Error::other(
                "subagent runtime stopped before admitting the message",
            )));
            return;
        };
        registry
            .message_admitted(&self.root_session_id, id, disposition)
            .await;
        let _ = response.send(Ok(disposition));
    }

    async fn begin_delegation(&self, id: MessageId) -> Option<DelegationChange> {
        let registry = self.registry.upgrade()?;
        registry
            .begin_message_delivery(&self.root_session_id, id)
            .await
    }

    async fn rollback_delegation(&self, change: Option<DelegationChange>) {
        let (Some(registry), Some(change)) = (self.registry.upgrade(), change) else {
            return;
        };
        registry
            .rollback_message_delivery(&self.root_session_id, change)
            .await;
    }

    async fn start_turn(
        &mut self,
        prompt: String,
        capacity: TurnCapacity,
        from_peers: bool,
        initial: bool,
    ) -> std::io::Result<Started> {
        turns::start_turn(self, prompt, capacity, from_peers, initial).await
    }

    async fn stop_active(&mut self) -> std::io::Result<()> {
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        let cancellation = active.control.cancel().await;
        self.finish_active().await;
        match cancellation {
            Ok(()) | Err(NanocodeError::TurnNotCancellable) => Ok(()),
            Err(error) => Err(std::io::Error::other(format!(
                "could not stop agent {}: {error}",
                self.id
            ))),
        }
    }

    async fn finish_active(&mut self) {
        self.submitted = None;
        let Some(mut active) = self.active.take() else {
            return;
        };
        let result = (&mut active.result).await;
        self.publish_turn_result(result).await;
    }

    async fn end_submitted_turn(&mut self) {
        self.submitted = None;
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        if !registry
            .turn_submitted(&self.root_session_id, self.id)
            .await
        {
            return;
        }
        nanocode_trace::info!(id = %self.id, "ending a subagent turn whose result is already durable");
        if let Err(error) = self.stop_active().await {
            nanocode_trace::warn!(id = %self.id, %error, "a subagent turn with a durable result did not stop");
        }
        self.start_pending(None).await;
    }

    async fn turn_finished(&mut self, result: Result<AgentResult<TurnResult>, TaskError>) {
        self.submitted = None;
        let active = self.active.take();
        if let (Some(active), Ok(Ok(finished))) = (active, &result)
            && self.remind(active, finished.usage.clone()).await
        {
            return;
        }
        self.publish_turn_result(result).await;

        self.start_pending(None).await;
    }

    async fn remind(&mut self, active: ActiveTurn, usage: Option<TurnUsage>) -> bool {
        let (Some(registry), Some(agent)) = (self.registry.upgrade(), self.agent.as_ref()) else {
            return false;
        };
        let Some(revision) = registry
            .remind_resumed_turn(&self.root_session_id, self.id, usage)
            .await
        else {
            return false;
        };
        let prompt = Prompt::new(completion_reminder()).with_instruction_revision(revision);
        let prompt = match &self.context {
            Some(context) => context.prompt(prompt),
            None => prompt,
        };
        match agent
            .prompt(nanocode_agent_core::PromptRequest::new(prompt))
            .await
        {
            Ok(turn) => {
                self.active = Some(ActiveTurn {
                    control: turn.control(),
                    result: platform::spawn(turn.result()),
                    _capacity: active._capacity,
                });
                true
            }
            Err(error) => {
                nanocode_trace::warn!(id = %self.id, %error, "a resumed subagent turn could not be reminded to submit its result");
                false
            }
        }
    }

    async fn publish_turn_result(&self, result: Result<AgentResult<TurnResult>, TaskError>) {
        let result = result.unwrap_or_else(|error| {
            Err(NanocodeError::InvalidRequest(format!(
                "subagent turn task failed: {error}"
            )))
        });
        if let Some(registry) = self.registry.upgrade() {
            registry
                .harness_turn_finished(&self.root_session_id, self.id, result)
                .await;
        }
    }

    async fn close(&mut self) -> std::io::Result<()> {
        let shutdown_result = match self.agent.take() {
            Some(agent) => agent.shutdown().await.map_err(|error| {
                std::io::Error::other(format!("could not close agent {}: {error}", self.id))
            }),
            None => Ok(()),
        };
        self.finish_active().await;
        if let Some(registry) = self.registry.upgrade() {
            registry
                .harness_closed(&self.root_session_id, self.id)
                .await;
        }
        shutdown_result
    }
}

fn resumes(message: &AgentMessage) -> bool {
    matches!(message.from, MessageSender::Root)
        && message.body.starts_with(super::durable::RESUME_MESSAGE)
}

fn from_peers(messages: &[AgentMessage]) -> bool {
    messages.iter().all(|message| {
        matches!(message.from, MessageSender::Agent { .. })
            && message.purpose != MessagePurpose::Delegate
    })
}
