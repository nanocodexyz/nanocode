use super::*;
use crate::{
    MessageResponse,
    execution::{BeginRequest, EncodedState, JournalWrite, Settlement},
};
use nanocode_durability::{EncodedPayload, Sequence, SequenceLog};
use serde_json::value::RawValue;
use std::collections::BTreeSet;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    provider: std::borrow::Cow<'static, str>,
    version: u32,
    pub(super) conversation: Conversation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) developer_notices: Vec<(String, String)>,
    pub(super) discovered: HashSet<String>,
    pub(super) tasks: Option<Value>,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) workspace: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) lineage: Option<Lineage>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) conversation_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) effort: Option<crate::Effort>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) thinking_disabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) fast_mode: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) context: Option<SystemContext>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct SystemContext {
    pub(super) date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) instructions: Option<String>,
    #[serde(default, skip_serializing)]
    pub(super) text: Option<Arc<str>>,
    pub(super) requested: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) build: Option<String>,
    #[serde(default, skip_serializing)]
    pub(super) template: Option<String>,
    #[serde(skip)]
    pub(super) prefix: Option<Arc<crate::durability::Prefix>>,
}

pub(super) fn instructions_key(instructions: Option<&str>) -> Option<String> {
    instructions.map(|text| EncodedPayload::from_json(text.into()).key().to_owned())
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            provider: std::borrow::Cow::Borrowed("claude"),
            version: 1,
            conversation: Conversation::default(),
            developer_notices: Vec::new(),
            discovered: HashSet::new(),
            tasks: None,
            model: None,
            workspace: None,
            lineage: None,
            conversation_id: None,
            effort: None,
            thinking_disabled: false,
            context: None,
            fast_mode: false,
        }
    }
}
impl Snapshot {
    pub(super) fn decode_paged(head: Value, pages: ContinuationPages) -> Result<Self> {
        let mut snapshot = serde_json::from_value::<Self>(head).map_err(provider_error)?;
        snapshot.conversation.messages = decode_stored(pages.messages)?;
        if let Some(code_calls) = pages.code_calls {
            snapshot.conversation.code_calls = decode_pages(code_calls)?;
        }
        if let Some(compacted) = pages.compacted {
            snapshot.conversation.compacted = decode_pages(compacted)?;
        }
        if let Some(sequence) = pages.compacted_pages {
            snapshot.conversation.compacted_pages = Some(sequence);
        }
        snapshot.validated()
    }

    pub(super) fn decode(value: Value) -> Result<Self> {
        serde_json::from_value::<Self>(value)
            .map_err(provider_error)?
            .validated()
    }
    pub(super) fn validated(self) -> Result<Self> {
        if self.provider != "claude" || self.version != 1 {
            return Err(NanocodeError::InvalidCheckpoint(
                "unsupported Claude checkpoint version/provider".into(),
            ));
        }
        Ok(self)
    }

    pub(super) fn session_context(&self, workspace: String) -> AgentSessionContext {
        use nanocode_types::responses::{ContentItem, MessageRole, ResponseItem};
        let mut history =
            self.conversation
                .packed_messages()
                .into_iter()
                .filter(|message| !message.is_effort_update())
                .map(|message| {
                    ResponseItem::message(
                        match message.role() {
                            Role::User => MessageRole::User,
                            Role::Assistant => MessageRole::Assistant,
                            Role::System => MessageRole::Developer,
                        },
                        message.into_message().content.into_iter().filter_map(
                            |block| match block {
                                ContentBlock::Text { text, .. } => Some(ContentItem::InputText {
                                    text: text.into_boxed_str(),
                                }),
                                _ => None,
                            },
                        ),
                    )
                })
                .collect::<Vec<_>>();
        history.extend(self.developer_notices.iter().map(|(_, text)| {
            ResponseItem::message(
                MessageRole::Developer,
                [ContentItem::InputText {
                    text: text.clone().into_boxed_str(),
                }],
            )
        }));
        AgentSessionContext::from_backend(workspace, history)
    }

    pub(super) const fn has_conversation(&self) -> bool {
        !self.conversation.messages.is_empty() || !self.conversation.summary.is_empty()
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    #[serde(skip)]
    pub(super) recovered_code_index: Option<u32>,
    pub(super) snapshot: Snapshot,
    pub(super) template: MessagesRequest,
    #[serde(default)]
    pub(super) lifecycle_turn_id: String,
    #[serde(default)]
    pub(super) stop_hook_active: bool,
    #[serde(default)]
    pub(super) finish_checked: bool,
    #[serde(default)]
    pub(super) fresh_attempts: u32,
    #[serde(default)]
    pub(super) persistence_nudged: bool,
    #[serde(default)]
    pub(super) instruction_revision: Option<u64>,

    #[serde(default)]
    pub(super) dynamic_tool_names: HashSet<String>,
    #[serde(default)]
    pub(super) wire_profile: Option<crate::FrozenWireProfile>,
    pub(super) threshold: u64,
    pub(super) parallel: bool,
    pub(super) tool_search: bool,
    pub(super) operation: Option<String>,
    pub(super) prepared: bool,
    #[serde(default)]
    pub(super) developer_notice_ids: HashSet<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) frozen_prompt: Option<Prompt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) pending_base: Option<u32>,
    pub(super) pending: Vec<HistoryMessage>,
    pub(super) usage: Usage,
    pub(super) index: u32,
    #[serde(default)]
    pub(super) steers: u32,

    #[serde(default)]
    pub(super) model_step_offset: u32,
    #[serde(default)]
    pub(super) model_receipt_start: Option<u32>,

    #[serde(default)]
    pub(super) context_recovery_attempted: bool,
    #[serde(default)]
    pub(super) output_continuations: u32,
    #[serde(default)]
    pub(super) thinking_only_nudged: bool,
    #[serde(default)]
    pub(super) refusal_recovered: bool,
    #[serde(default)]
    pub(super) refusal_continuations: u32,
}
impl Cursor {
    pub(super) fn host_context(&self, state: &State) -> Option<Arc<str>> {
        state
            .host_context
            .clone()
            .or_else(|| self.operation.as_deref().map(Arc::from))
    }
    pub(super) fn effect<'a>(&'a self, state: &'a State, step: &str) -> Option<Effect<'a>> {
        Some(Effect {
            policy: state.policy.as_deref()?,
            operation: self.operation.as_deref()?,
            step: step.to_owned(),
            model_call: step
                .strip_prefix("model-")
                .and_then(|index| index.parse::<u32>().ok())
                .zip(self.model_receipt_start)
                .is_some_and(|(index, start)| index >= start),
        })
    }
}
pub(super) struct Effect<'a> {
    policy: &'a dyn ClaudeExecutionPolicy,
    operation: &'a str,
    step: String,
    model_call: bool,
}
impl Effect<'_> {
    pub(super) fn key(&self) -> (&str, &str) {
        (self.operation, &self.step)
    }

    pub(super) fn scoped(&self, scope: &str) -> Self {
        Effect {
            policy: self.policy,
            operation: self.operation,
            step: format!("{scope}-{}", self.step),
            model_call: self.model_call,
        }
    }

    pub(super) async fn begin_model(
        &self,
        wire: &crate::PreparedWire,
        writes: Vec<JournalWrite>,
    ) -> Result<Step> {
        let request = BeginRequest {
            step_id: self.step.clone(),
            kind: if self.model_call {
                "model_call"
            } else {
                "model"
            }
            .to_owned(),
            input: wire.durable_input().map_err(provider_error)?,
            wire: Some(wire),
            recovery: nanocode_durability::StepRecovery::Rerun,
        };
        self.admit(writes, request).await
    }
    pub(super) async fn begin_tool(
        &self,
        input: Box<RawValue>,
        writes: Vec<JournalWrite>,
    ) -> Result<Step> {
        let request = BeginRequest {
            step_id: self.step.clone(),
            kind: "tool".to_owned(),
            input,
            wire: None,
            recovery: nanocode_durability::StepRecovery::AtMostOnce,
        };
        self.admit(writes, request).await
    }
    async fn admit(&self, writes: Vec<JournalWrite>, request: BeginRequest<'_>) -> Result<Step> {
        self.policy
            .commit_writes(self.operation.to_owned(), writes, Some(request))
            .await?
            .ok_or_else(|| recovery_error("the Claude step admission returned no outcome"))
    }
    pub(super) async fn complete(&self, output: Value) -> Result<()> {
        self.policy
            .complete_step(self.operation.to_owned(), self.step.clone(), output)
            .await
    }
    pub(super) async fn refuse(&self, error: &NanocodeError) -> Result<()> {
        self.complete(json!({"refused": error.to_string()})).await
    }
    pub(super) fn completion(&self, output: &impl Serialize) -> Result<JournalWrite> {
        Ok(JournalWrite::Complete {
            step_id: self.step.clone(),
            output: serde_json::value::to_raw_value(output).map_err(provider_error)?,
            records: Vec::new(),
        })
    }

    pub(super) fn model_completion(
        &self,
        response: &mut MessageResponse,
        paged: bool,
    ) -> Result<JournalWrite> {
        #[derive(Serialize)]
        struct Receipt<'a> {
            content_record: EncodedPayload,
            #[serde(flatten)]
            response: &'a MessageResponse,
        }
        if !paged {
            return self.completion(response);
        }
        let message = Message {
            role: Role::Assistant,
            content: std::mem::take(&mut response.content),
            output_config: None,
        };
        let record =
            serde_json::to_string(&message).map(|json| EncodedPayload::from_json(Arc::from(json)));
        let output = record.as_ref().map_err(provider_error).and_then(|record| {
            serde_json::value::to_raw_value(&Receipt {
                content_record: record.reference(),
                response,
            })
            .map_err(provider_error)
        });
        response.content = message.content;
        let record = record.map_err(provider_error)?;
        Ok(JournalWrite::Complete {
            step_id: self.step.clone(),
            output: output?,
            records: vec![record],
        })
    }
}

#[derive(Default)]
pub(super) struct Journal {
    operation: Option<String>,
    writes: Vec<JournalWrite>,
}
impl Journal {
    fn advance(&mut self, state: EncodedState) {
        self.writes
            .retain(|write| !matches!(write, JournalWrite::Advance(_)));
        self.writes.push(JournalWrite::Advance(state));
    }
    pub(super) fn complete(&mut self, write: JournalWrite) {
        self.writes.push(write);
    }
    pub(super) fn take(&mut self) -> Vec<JournalWrite> {
        std::mem::take(&mut self.writes)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CodeOnlyUpgrade {
    pub(super) code_only_tools: Vec<ClaudeToolSpec>,
    pub(super) notice: String,
}
impl CodeOnlyUpgrade {
    pub(super) fn new(code_only_tools: Vec<ClaudeToolSpec>, tools_disabled: bool) -> Self {
        Self {
            code_only_tools,
            notice: if tools_disabled {
                "Harness recovery notice: the interrupted summary request was reissued with the current tool catalog. Produce the requested text-only summary."
            } else {
                "Harness recovery notice: this operation was upgraded to Code Mode. The preceding direct-tool model request was retired with outcome unknown; provider-side effects may already have occurred. Do not automatically repeat them. Reconcile effects before continuing through exec and wait."
            }.into(),
        }
    }
}
pub(super) fn is_code_only_catalog(tools: &[ClaudeToolSpec]) -> bool {
    tools.len() == 2
        && ["exec", "wait"].iter().all(|name| {
            tools
                .iter()
                .any(|tool| matches!(tool, ClaudeToolSpec::Client(tool) if tool.name == *name))
        })
}
impl State {
    pub(super) fn code_only_tools(&self) -> Vec<ClaudeToolSpec> {
        self.available_tools().into_iter().filter(|tool| {
            matches!(tool, ClaudeToolSpec::Client(tool) if tool.name == "exec" || tool.name == "wait")
        }).collect()
    }
    pub(super) fn classify_code_only_tools(&self, cursor: &mut Cursor) {
        cursor.dynamic_tool_names = self
            .dynamic_catalog()
            .into_iter()
            .filter_map(|(definition, _)| {
                cursor.template.tools.iter().any(|tool| {
                    matches!(tool, ClaudeToolSpec::Client(admitted) if admitted == &definition)
                }).then_some(definition.name)
            })
            .collect();
    }
    #[cfg_attr(not(not(target_family = "wasm")), allow(clippy::missing_const_for_fn))]
    fn task_snapshot(&self) -> Result<Option<Value>> {
        #[cfg(not(target_family = "wasm"))]
        {
            self.task_board
                .as_ref()
                .map(|tasks| tasks.snapshot().map_err(provider_error))
                .transpose()
        }
        #[cfg(not(not(target_family = "wasm")))]
        {
            Ok(None)
        }
    }
    fn restore_tasks(&self, tasks: Option<Value>) -> Result<()> {
        if let Some(tasks) = tasks {
            #[cfg(not(target_family = "wasm"))]
            self.task_board
                .as_ref()
                .ok_or_else(|| unsupported("Claude task checkpoint requires a task board"))?
                .restore(tasks)
                .map_err(provider_error)?;
            #[cfg(not(not(target_family = "wasm")))]
            {
                let _ = tasks;
                return Err(unsupported(
                    "Claude task checkpoint restoration requires a native target with tools and a task board",
                ));
            }
        }
        Ok(())
    }
    pub(super) async fn snapshot(&self, conversation: &Conversation) -> Result<Snapshot> {
        Ok(Snapshot {
            conversation: conversation.clone(),
            developer_notices: Vec::clone(&self.developer_notices.get()),
            discovered: HashSet::clone(&self.discovered.get()),
            tasks: self.task_snapshot()?,
            model: Some(self.model()),
            workspace: Some(self.workspace()),
            lineage: Some(self.lineage.clone()),
            conversation_id: Some(self.conversation_id.clone()),
            effort: self.effort(),
            thinking_disabled: self.thinking_mode.get().disabled(),
            fast_mode: self.fast_mode.load(Ordering::SeqCst),
            context: self.kept_context().filter(|_| !self.holds_back_formats()),
            ..Snapshot::default()
        })
    }
    async fn restore_snapshot(
        &self,
        conversation: &mut Conversation,
        snapshot: Snapshot,
    ) -> Result<()> {
        self.restore_tasks(snapshot.tasks)?;
        self.discovered.set(snapshot.discovered);
        self.developer_notices.set(snapshot.developer_notices);
        *conversation = snapshot.conversation;
        Ok(())
    }
    async fn merge_developer_notices(
        &self,
        policy: &dyn ClaudeExecutionPolicy,
        operation: &str,
        selected: &mut HashSet<String>,
    ) -> Result<()> {
        let notices = policy.developer_notices(operation.to_owned()).await?;
        for notice in &notices {
            selected.remove(&notice.0);
        }
        self.developer_notices.update(|queued| {
            for notice in &notices {
                if !queued.iter().any(|(id, _)| id == &notice.0) {
                    queued.push(notice.clone());
                }
            }
        });
        Ok(())
    }

    pub(super) async fn cursor(
        &self,
        conversation: &mut Conversation,
        operation: Option<&str>,
        speed: Option<crate::Speed>,
        prompt: Option<&Prompt>,
        mut developer_notice_ids: HashSet<String>,
        journal: &mut Journal,
    ) -> Result<Cursor> {
        let update = self.persistence_update.enter().await;
        if let (Some(policy), Some(operation)) = (&self.policy, operation)
            && let Some(continuation) = policy.continuation_paged(operation.to_owned()).await?
        {
            let mut cursor: Cursor =
                serde_json::from_value(continuation.head).map_err(recovery_error)?;
            if let Some(pages) = continuation.pages {
                let conversation = &mut cursor.snapshot.conversation;
                conversation.messages = decode_stored(pages.messages)?;
                if let Some(code_calls) = pages.code_calls {
                    conversation.code_calls = decode_pages(code_calls)?;
                }
                if let Some(compacted) = pages.compacted {
                    conversation.compacted = decode_pages(compacted)?;
                }
                if let Some(sequence) = pages.compacted_pages {
                    conversation.compacted_pages = Some(sequence);
                }
            }
            if let Some(base) = cursor.pending_base.take() {
                let prefix = usize::try_from(base)
                    .ok()
                    .and_then(|base| cursor.snapshot.conversation.messages.get(..base))
                    .ok_or_else(|| {
                        recovery_error(
                            "Claude continuation pending prefix exceeds its conversation",
                        )
                    })?;
                cursor.pending = prefix
                    .iter()
                    .cloned()
                    .chain(std::mem::take(&mut cursor.pending))
                    .collect();
            }
            cursor.recovered_code_index = Some(cursor.index);

            if cursor.model_receipt_start.is_none() {
                cursor.model_receipt_start = Some(cursor.index.saturating_add(1));
            }
            if cursor.operation.as_deref() != Some(operation)
                || cursor.snapshot.provider != "claude"
                || cursor.snapshot.version != 1
            {
                return Err(recovery_error("invalid Claude execution continuation"));
            }

            self.restore_snapshot(conversation, std::mem::take(&mut cursor.snapshot))
                .await
                .map_err(recovery_error)?;
            self.merge_developer_notices(
                policy.as_ref(),
                operation,
                &mut cursor.developer_notice_ids,
            )
            .await?;
            journal.operation.clone_from(&cursor.operation);
            return Ok(cursor);
        }
        if let (Some(policy), Some(operation)) = (&self.policy, operation) {
            self.merge_developer_notices(policy.as_ref(), operation, &mut developer_notice_ids)
                .await?;
        }
        drop(update);
        self.outlived_cache();
        let mut template = self.request_template(speed, conversation.effort_pin);
        if self.lineage.depth > 0 {
            conversation.fallback_latch = FallbackLatch::Main;
        }
        if conversation.escalated
            && let Some(escalation) = &self.escalation
        {
            escalation.model.clone_into(&mut template.model);
        }
        if let Some(model) = conversation.fallback_latch.model() {
            model.clone_into(&mut template.model);
        }
        let kept = self.kept_prefix.get();
        let static_names = self
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .chain(kept.iter().flat_map(|kept| {
                kept.tools.iter().filter_map(|tool| match tool {
                    ClaudeToolSpec::Client(tool) => Some(tool.name.as_str()),
                    ClaudeToolSpec::Server(_) => None,
                })
            }))
            .collect::<HashSet<_>>();
        let dynamic_tool_names = template
            .tools
            .iter()
            .filter_map(|tool| match tool {
                ClaudeToolSpec::Client(tool) if !static_names.contains(tool.name.as_str()) => {
                    Some(tool.name.clone())
                }
                _ => None,
            })
            .collect();
        let wire_profile = self
            .client
            .freeze_wire_profile(template.cache_control.is_some());
        #[cfg_attr(target_family = "wasm", allow(unused_mut))]
        let mut cursor = Cursor {
            recovered_code_index: None,
            lifecycle_turn_id: candidate_id("lifecycle"),
            stop_hook_active: false,
            finish_checked: false,
            fresh_attempts: 0,
            persistence_nudged: false,
            instruction_revision: None,
            snapshot: Snapshot::default(),
            template,
            dynamic_tool_names,
            wire_profile: Some(wire_profile),
            threshold: self.compaction_threshold(),
            parallel: self.parallel_tools,
            tool_search: self.client_tool_search,
            operation: operation.map(str::to_owned),
            prepared: false,
            developer_notice_ids,
            frozen_prompt: prompt
                .filter(|prompt| {
                    matches!(
                        prompt.instruction,
                        nanocode_agent_core::input::PromptInput::Content(_)
                    )
                })
                .cloned(),
            pending_base: None,
            pending: Vec::new(),
            usage: Usage::default(),
            index: 0,
            steers: 0,
            model_step_offset: 1,
            model_receipt_start: Some(0),
            context_recovery_attempted: false,
            output_continuations: 0,
            thinking_only_nudged: false,
            refusal_recovered: false,
            refusal_continuations: 0,
        };

        #[cfg(not(target_family = "wasm"))]
        if self.policy.is_some() && self.task_board.is_some() {
            cursor.parallel = false;
        }
        Ok(cursor)
    }

    pub(super) async fn stage_cursor(
        &self,
        cursor: &mut Cursor,
        conversation: &mut Conversation,
        journal: &mut Journal,
    ) -> Result<()> {
        if journal.operation.is_none() {
            self.defer_cursor(cursor, conversation, journal).await?;
        }
        Ok(())
    }

    pub(super) fn persists(&self, cursor: &Cursor) -> bool {
        self.policy.is_some() && cursor.operation.is_some()
    }

    pub(super) fn holds_back_formats(&self) -> bool {
        self.policy
            .as_ref()
            .is_some_and(|policy| policy.holds_back_formats())
    }

    pub(super) fn pages_messages(&self) -> bool {
        self.policy
            .as_ref()
            .is_some_and(|policy| policy.pages_messages())
    }

    pub(super) fn retain_pending(&self, cursor: &mut Cursor, pending: &[HistoryMessage]) {
        if self.persists(cursor) {
            cursor.pending = pending.to_vec();
        }
    }
    pub(super) async fn defer_cursor(
        &self,
        cursor: &mut Cursor,
        conversation: &mut Conversation,
        journal: &mut Journal,
    ) -> Result<()> {
        let _update = self.persistence_update.enter().await;
        self.defer_cursor_locked(cursor, conversation, journal)
            .await
    }
    pub(super) async fn defer_cursor_locked(
        &self,
        cursor: &mut Cursor,
        conversation: &mut Conversation,
        journal: &mut Journal,
    ) -> Result<()> {
        if self.code_only && !is_code_only_catalog(&cursor.template.tools) {
            cursor.template.tools = self.code_only_tools();
            self.classify_code_only_tools(cursor);
            cursor.tool_search = false;
        }

        if self.persists(cursor) {
            cursor.snapshot = self.snapshot(&Conversation::default()).await?;
            cursor.snapshot.context = None;
            cursor.snapshot.conversation = std::mem::take(conversation);
            let mut paging = self.paging.take().unwrap_or_default();
            paging.stored(self.policy.as_deref());
            let pooled = self.pool_template(cursor, &mut paging).await;
            let encoded =
                pooled.and_then(|()| encode_cursor(cursor, self.pages_messages(), &mut paging));
            self.paging.put(paging);
            *conversation = std::mem::take(&mut cursor.snapshot).conversation;
            journal.operation.clone_from(&cursor.operation);
            journal.advance(encoded?);
        }
        Ok(())
    }
    async fn pool_template(&self, cursor: &Cursor, paging: &mut Paging) -> Result<()> {
        let Some(policy) = self
            .policy
            .as_ref()
            .filter(|policy| policy.pages_messages())
        else {
            return Ok(());
        };
        let prompt = TemplatePrompt {
            system: cursor.template.system.as_ref(),
            tools: &cursor.template.tools,
        };
        let record = template_record(&prompt, paging)?;
        if paging.pooled.as_deref() != Some(record.key()) && policy.pool(record.clone()).await? {
            paging.pooled = Some(record.key().to_owned());
        }
        Ok(())
    }
    pub(super) async fn flush_for_lifecycle(
        &self,
        event: &crate::ClaudeLifecycleEvent,
        cursor: &mut Cursor,
        conversation: &mut Conversation,
        journal: &mut Journal,
    ) -> Result<()> {
        if self
            .tool_hooks
            .iter()
            .any(|hook| hook.handles_lifecycle(event))
        {
            self.stage_cursor(cursor, conversation, journal).await?;
            self.flush_journal(journal).await?;
        }
        Ok(())
    }
    pub(super) async fn defer_journal(&self, journal: &mut Journal) -> Result<()> {
        let (Some(policy), Some(operation)) = (&self.policy, journal.operation.clone()) else {
            return Ok(());
        };
        let writes = journal.take();
        if writes.is_empty() {
            return Ok(());
        }
        let advanced = writes
            .iter()
            .any(|write| matches!(write, JournalWrite::Advance(_)));
        policy.defer_writes(operation, writes).await?;
        if advanced {
            self.persistence_ready.notify_waiters();
        }
        Ok(())
    }
    pub(super) async fn flush_journal(&self, journal: &mut Journal) -> Result<()> {
        let (Some(policy), Some(operation)) = (&self.policy, journal.operation.clone()) else {
            return Ok(());
        };
        let writes = journal.take();
        if writes.is_empty() {
            return Ok(());
        }
        let advanced = writes
            .iter()
            .any(|write| matches!(write, JournalWrite::Advance(_)));
        policy.commit_writes(operation, writes, None).await?;
        if advanced {
            self.committed();
            self.persistence_ready.notify_waiters();
        }
        Ok(())
    }
    fn committed(&self) {
        if let Some(mut paging) = self.paging.take() {
            paging.committed();
            self.paging.put(paging);
        }
    }
    pub(super) async fn settle(
        &self,
        conversation: &mut Conversation,
        request: &BackendPrompt,
        result: &Result<TurnResult>,
    ) -> Result<()> {
        let _update = self.persistence_update.enter().await;
        let (Some(policy), Some(operation)) = (&self.policy, &request.request_id) else {
            return Ok(());
        };

        if result
            .as_ref()
            .err()
            .is_some_and(|error| error.execution_policy_disposition().is_some())
        {
            self.life.stop();
            return Ok(());
        }
        if matches!(result, Err(NanocodeError::TurnCancelled)) && self.life.closing() {
            let released = policy.release(operation.clone()).await;
            if let Some(turn) = self.steering.acquire().await.get_mut(&request.key) {
                turn.operation = None;
            }
            self.persistence_ready.notify_waiters();
            return released;
        }
        let mut snapshot = self.snapshot(&Conversation::default()).await?;
        snapshot.conversation = std::mem::take(conversation);
        let mut paging = self.paging.take().unwrap_or_default();
        paging.stored(Some(&**policy));
        let checkpoint = encode_snapshot(&mut snapshot, policy.pages_messages(), &mut paging);
        self.paging.put(paging);
        *conversation = snapshot.conversation;
        let checkpoint = checkpoint?;
        let settlement = match result {
            Ok(result) => Settlement::Complete(turn_output(
                result,
                conversation
                    .messages
                    .last()
                    .filter(|_| policy.pages_messages()),
            )?),
            Err(NanocodeError::TurnCancelled) => Settlement::Cancel,
            Err(NanocodeError::Response(error)) if error.retryable() => {
                Settlement::FailAttempt(error.to_string())
            }
            Err(error) => Settlement::Fail(error.to_string()),
        };
        let settled = policy
            .settle_encoded(operation.clone(), checkpoint, settlement)
            .await;
        if settled.is_err() {
            self.life.stop();
        } else {
            self.committed();
        }
        if let Some(turn) = self.steering.acquire().await.get_mut(&request.key) {
            turn.operation = None;
        }
        self.persistence_ready.notify_waiters();
        settled
    }
    pub(super) async fn durable_tool(
        &self,
        control: (&Cursor, &Cancellation),
        call: (&str, &str, &Value),
        handler: Option<&Handler>,
        events: &AgentEventPublisher,
        journal: (Vec<JournalWrite>, nanocode_durability::ResultTiming),
    ) -> Result<(ContentBlock, Option<JournalWrite>)> {
        #[derive(Serialize)]
        struct Call<'a> {
            id: &'a str,
            input: &'a Value,
            name: &'a str,
        }
        #[derive(Serialize)]
        struct Completed<'a> {
            result: &'a ContentBlock,
            tasks: Option<Value>,
            discovered: &'a HashSet<String>,
        }
        let (cursor, cancel) = control;
        let (id, name, input) = call;
        let (writes, timing) = journal;
        let index = cursor.index;
        let step = format!("tool-{index}-{id}");
        let effect = cursor.effect(self, &step);
        let admitted = if let Some(effect) = &effect {
            effect
                .begin_tool(
                    serde_json::value::to_raw_value(&Call { id, input, name })
                        .map_err(provider_error)?,
                    writes,
                )
                .await?
        } else {
            Step::Execute
        };
        let interrupted = matches!(&admitted, Step::Interrupted);
        if let Step::Replay(value) = admitted {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Receipt {
                result: ContentBlock,
                tasks: Option<Value>,
                discovered: HashSet<String>,
            }
            let mut receipt: Receipt = serde_json::from_value(value).map_err(recovery_error)?;
            let content = match &mut receipt.result {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == id => content,
                _ => {
                    return Err(recovery_error(
                        "tool receipt does not match the admitted call",
                    ));
                }
            };
            super::images::prepare_tool_images(
                content,
                crate::prompt::ImageResolution::of(&cursor.template.model),
                &self.images,
            )
            .await?;
            self.restore_tasks(receipt.tasks).map_err(recovery_error)?;
            self.discovered
                .update(|discovered| discovered.extend(receipt.discovered.iter().cloned()));
            return Ok((receipt.result, None));
        }
        let unknown = || {
            ContentBlock::tool_result_content(id, ToolResultContent::Text("Tool execution interrupted; outcome unknown. Do not assume it did not run or automatically repeat it.".into()), true)
        };
        let recovered = match handler {
            Some(handler) if interrupted => {
                self.recover_tool(id, name, input, handler, events, (cursor, cancel))
                    .await?
            }
            _ => Recovered::Unknown,
        };
        let undispatched = matches!(recovered, Recovered::NotDispatched);
        let recovered = match recovered {
            Recovered::Reply(block) => Some(block),
            Recovered::NotDispatched | Recovered::Unknown => None,
        };
        let interrupted = interrupted && recovered.is_none() && !undispatched;
        let checked = handler
            .filter(|_| crate::upstream::tool_definition(name).is_some())
            .and_then(|_| {
                cursor.template.tools.iter().find_map(|tool| match tool {
                    ClaudeToolSpec::Client(tool) if tool.name == name => Some(
                        crate::tools::input::validate(name, tool.input_schema.as_value(), input),
                    ),
                    _ => None,
                })
            });
        let called = match &checked {
            Some(Ok(Some(coerced))) => coerced,
            _ => input,
        };
        let result = if let Some(recovered) = recovered {
            recovered
        } else if interrupted && let Some(settled) = self.settled_fact(id).await? {
            settled
        } else if interrupted {
            ContentBlock::tool_result_content(id, ToolResultContent::Text("Tool execution interrupted by a host crash; outcome unknown. Do not assume it did not run or automatically repeat it.".into()), true)
        } else if cancel.is_cancelled() {
            unknown()
        } else if handler.is_some()
            && let Some(unparsed) = input
                .as_object()
                .filter(|input| input.len() == 1)
                .and_then(|input| input.get("__unparsedToolInput"))
            && let (Some(raw), Some(len)) = (
                unparsed.get("raw").and_then(Value::as_str),
                unparsed.get("len").and_then(Value::as_u64),
            )
        {
            let mut units = 0;
            let shown: String = raw
                .chars()
                .take_while(|c| {
                    units += c.len_utf16();
                    units <= 200
                })
                .collect();
            ContentBlock::tool_result_content(
                id,
                ToolResultContent::Text(format!(
                    "<tool_use_error>InputValidationError: {name} was called with input that could not be parsed as JSON.\nYou sent (first {} of {len} bytes): {shown}\nCommon causes: unescaped backslashes in file paths (use / or \\\\), unescaped control characters, or truncated output. Retry with valid JSON.</tool_use_error>",
                    shown.encode_utf16().count()
                )),
                true,
            )
        } else if let Some(Err(error)) = &checked {
            ContentBlock::tool_result_content(
                id,
                ToolResultContent::Text(format!(
                    "<tool_use_error>InputValidationError: {error}</tool_use_error>"
                )),
                true,
            )
        } else if !undispatched
            && self.code_only
            && cursor.recovered_code_index == Some(index)
            && name == "exec"
        {
            ContentBlock::tool_result_content(id, ToolResultContent::Text(
                "Code Mode admission was lost during recovery; prior effects may have outcome unknown. No code was executed in this attempt. Reconcile those effects before using a fresh cell from a new model request.".into()
            ), true)
        } else if let Some(handler) = handler
            && matches!(name, "exec" | "wait")
        {
            self.call_tool(id, name, called, handler, events, (cursor, cancel, None))
                .await?
        } else if let Some(handler) = handler {
            nanocode_sync::select! {
                result = self.call_tool(id, name, called, handler, events, (cursor, cancel, None)) => result?,
                () = cancel.cancelled() => match self.abandoned(id).await? {
                    Some(settled) => settled,
                    None => unknown(),
                },
            }
        } else if self.code_only
            && name != "exec"
            && name != "wait"
            && cursor
                .template
                .tools
                .iter()
                .any(|tool| matches!(tool, ClaudeToolSpec::Client(tool) if tool.name == name))
        {
            ContentBlock::tool_result_content(
                id,
                ToolResultContent::Text(format!(
                    "Direct tool {name} was retired during Code Mode recovery. Its prior outcome is unknown; no handler was invoked in this attempt. Reconcile effects before repeating through exec."
                )),
                true,
            )
        } else if self.code_only
            && self
                .nested_tools
                .as_ref()
                .is_some_and(|nested| nested(name))
        {
            let call = if name.contains('-') || name.starts_with(|c: char| c.is_ascii_digit()) {
                format!("use the `tools` entry listed as {name} in `ALL_TOOLS`")
            } else {
                format!("`await tools.{name}(...)`")
            };
            ContentBlock::tool_result_content(
                id,
                ToolResultContent::Text(format!(
                    "{name} is not a top-level tool; no handler was invoked. Only exec and wait are top-level. Call {name} from JavaScript inside exec through the global tools object: {call}."
                )),
                true,
            )
        } else {
            ContentBlock::tool_result_content(
                id,
                ToolResultContent::Text(unavailable(name, &cursor.template.tools)),
                true,
            )
        };
        let mut deferred = None;
        if let Some(effect) = &effect {
            let completion = effect.completion(&Completed {
                result: &result,
                tasks: self.task_snapshot()?,
                discovered: &self.discovered.get(),
            })?;
            match timing {
                nanocode_durability::ResultTiming::NextCommit => deferred = Some(completion),
                nanocode_durability::ResultTiming::Now => {
                    self.policy_writes(effect, vec![completion]).await?;
                }
            }
        }
        if interrupted {
            self.emit(
                events,
                AgentEventKind::ToolResult,
                json!({
                    "call_id": id,
                    "tool": name,
                    "status": "unknown",
                    "result": "Tool execution interrupted by a host crash; outcome unknown. Do not assume it did not run or automatically repeat it.",
                    "outcome_unknown": true,
                    "duration_ns": 0,
                    "started_after_ns": null,
                }),
            );
        }
        Ok((result, deferred))
    }
    pub(super) async fn policy_writes(
        &self,
        effect: &Effect<'_>,
        writes: Vec<JournalWrite>,
    ) -> Result<()> {
        effect
            .policy
            .commit_writes(effect.operation.to_owned(), writes, None)
            .await
            .map(drop)
    }
}

pub(super) fn decode_pages<T: serde::de::DeserializeOwned>(
    payloads: Vec<EncodedPayload>,
) -> Result<Vec<T>> {
    let mut decoded = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let value = payload.decode::<Value>().map_err(provider_error)?;
        drop(payload);
        decoded.push(serde_json::from_value(value).map_err(provider_error)?);
    }
    Ok(decoded)
}

fn decode_stored(payloads: Vec<EncodedPayload>) -> Result<Vec<HistoryMessage>> {
    let messages: Vec<HistoryMessage> = decode_pages(payloads)?;
    messages.iter().for_each(HistoryMessage::committed);
    Ok(messages)
}

pub(super) fn prepend_compacted(
    conversation: &mut Conversation,
    persisted: Vec<EncodedPayload>,
) -> Result<()> {
    let mut compacted: Vec<HistoryMessage> = decode_pages(persisted)?;
    compacted.append(&mut conversation.compacted);
    conversation.compacted = compacted;
    conversation.compacted_pages = None;
    Ok(())
}

pub(super) async fn load_paged(
    policy: &dyn ClaudeExecutionPolicy,
    messages: &[&HistoryMessage],
) -> Result<Vec<HistoryMessage>> {
    let records = messages
        .iter()
        .map(|message| {
            let original = message
                .paged_original()
                .expect("only paged messages are loaded");
            EncodedPayload::from_key(&original)
        })
        .collect::<Vec<_>>();
    let loaded = policy.load_records(records).await?;
    if loaded.len() != messages.len() {
        return Err(recovery_error(format!(
            "loading {} paged messages returned {}",
            messages.len(),
            loaded.len()
        )));
    }
    loaded
        .iter()
        .map(|record| {
            let json = record.json().map_err(provider_error)?;
            HistoryMessage::loaded(json).map_err(provider_error)
        })
        .collect()
}

pub(super) async fn unpage(
    conversation: &mut Conversation,
    policy: &dyn ClaudeExecutionPolicy,
) -> Result<()> {
    for messages in [&mut conversation.messages, &mut conversation.compacted] {
        let paged = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.paged_original().is_some())
            .map(|(at, _)| at)
            .collect::<Vec<_>>();
        if paged.is_empty() {
            continue;
        }
        let loaded = load_paged(
            policy,
            &paged.iter().map(|&at| &messages[at]).collect::<Vec<_>>(),
        )
        .await?;
        for (at, message) in paged.into_iter().zip(loaded) {
            messages[at] = message;
        }
    }
    Ok(())
}

const PAGED_VERSION: u32 = 4;

#[derive(Serialize)]
struct PagedConversation {
    messages: Sequence,
    code_calls: Sequence,
    #[serde(skip_serializing_if = "unskipped")]
    code_calls_skip: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    compacted: Option<Sequence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    paged: Option<Sequence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admitted_base: Option<EncodedPayload>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    admitted_tool_ids: Vec<Arc<str>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unadmitted_tool_ids: Vec<Arc<str>>,
}

#[derive(Serialize)]
struct PagedSnapshot<'a> {
    provider: &'a str,
    version: u32,
    #[serde(flatten)]
    conversation: PagedConversation,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<EncodedPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    template: Option<EncodedPayload>,
    snapshot: &'a RawValue,
}

#[derive(Serialize)]
struct PagedCursor<'a> {
    version: u32,
    #[serde(flatten)]
    conversation: PagedConversation,
    pending: Sequence,
    template: EncodedPayload,
    cursor: &'a RawValue,
}

#[derive(Serialize)]
struct AdmittedBase<'a> {
    ids: &'a BTreeSet<Arc<str>>,
}

const fn unskipped(skip: &usize) -> bool {
    *skip == 0
}

struct Detached {
    messages: Vec<HistoryMessage>,
    code_calls: Vec<history::CodeRound>,
    code_calls_omitted: u64,
    compacted: Vec<HistoryMessage>,
    compacted_pages: Option<Sequence>,
    admitted_tool_ids: history::AdmittedIds,
}

impl Detached {
    fn take(conversation: &mut Conversation) -> Self {
        Self {
            messages: std::mem::take(&mut conversation.messages),
            code_calls: std::mem::take(&mut conversation.code_calls),
            code_calls_omitted: conversation.code_calls_omitted_rounds,
            compacted: std::mem::take(&mut conversation.compacted),
            compacted_pages: conversation.compacted_pages.take(),
            admitted_tool_ids: std::mem::take(&mut conversation.admitted_tool_ids),
        }
    }

    fn restore(self, conversation: &mut Conversation) {
        conversation.messages = self.messages;
        conversation.code_calls = self.code_calls;
        conversation.compacted = self.compacted;
        conversation.compacted_pages = self.compacted_pages;
        conversation.admitted_tool_ids = self.admitted_tool_ids;
    }

    fn page(
        &mut self,
        records: &mut Vec<EncodedPayload>,
        paging: &mut Paging,
        settled: bool,
    ) -> Result<PagedConversation> {
        let unchanged = paging.messages.unchanged(&self.messages, 0);
        let tools = &mut paging.tools;
        if tools.epoch != self.admitted_tool_ids.epoch()
            || tools.seen > self.admitted_tool_ids.len()
        {
            *tools = ToolIds::new(&self.admitted_tool_ids);
            for message in &paging.messages.items[..unchanged] {
                tools.list_message(message, &self.admitted_tool_ids);
            }
        }
        for message in &paging.messages.items[unchanged..] {
            tools.unlist_message(message, &self.admitted_tool_ids);
        }
        for message in &self.messages[unchanged..] {
            tools.list_message(message, &self.admitted_tool_ids);
        }
        tools.admit_new(&self.admitted_tool_ids);
        if settled {
            tools.settle(self.admitted_tool_ids.len());
        }
        let admitted_base = tools.base_record()?;
        if let Some(base) = &admitted_base {
            records.push(base.clone());
        }
        let (code_calls, code_calls_skip) = paging.code_calls.page(
            &self.code_calls,
            self.code_calls_omitted,
            records,
            paging.stored.as_ref(),
        )?;
        let groups = paged_groups(&self.messages);
        let paged = if groups.is_empty() {
            paging.paged = Pages::default();
            None
        } else {
            let unchanged = paging.paged.unchanged(&groups, 0);
            Some(
                paging
                    .paged
                    .page(&groups, 0, unchanged, records, paging.stored.as_ref())?,
            )
        };
        Ok(PagedConversation {
            messages: paging.messages.page(
                &self.messages,
                0,
                unchanged,
                records,
                paging.stored.as_ref(),
            )?,
            code_calls,
            code_calls_skip,
            compacted: paging.compacted.page(
                &mut self.compacted,
                &mut self.compacted_pages,
                records,
                paging.stored.as_ref(),
            )?,
            paged,
            admitted_base: admitted_base.as_ref().map(EncodedPayload::reference),
            admitted_tool_ids: tools.recent.iter().cloned().collect(),
            unadmitted_tool_ids: tools.unadmitted.iter().cloned().collect(),
        })
    }
}

#[derive(Default)]
struct ToolIds {
    epoch: u64,
    seen: usize,
    mark: usize,
    listed: HashMap<Arc<str>, u32>,
    recent: BTreeSet<Arc<str>>,
    base: BTreeSet<Arc<str>>,
    unadmitted: BTreeSet<Arc<str>>,
    base_record: Option<EncodedPayload>,
}

impl ToolIds {
    fn new(admitted: &history::AdmittedIds) -> Self {
        Self {
            epoch: admitted.epoch(),
            mark: admitted.len(),
            ..Self::default()
        }
    }

    fn tool_uses(message: &HistoryMessage) -> Vec<String> {
        if message.role() != Role::Assistant || !message.mentions("tool_use") {
            return Vec::new();
        }
        message
            .view()
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn unlisted(&mut self, id: &str, admitted: &history::AdmittedIds) {
        match admitted.position(id) {
            Some(position) if position < self.mark => {
                self.base.insert(id.into());
                self.base_record = None;
            }
            Some(_) => {
                self.recent.insert(id.into());
            }
            None => {
                self.unadmitted.remove(id);
            }
        }
    }

    fn list_message(&mut self, message: &HistoryMessage, admitted: &history::AdmittedIds) {
        for id in &Self::tool_uses(message) {
            let id = id.as_str();
            let count = self.listed.entry(id.into()).or_insert(0);
            *count += 1;
            if *count > 1 {
                continue;
            }
            if admitted.contains(id) {
                self.recent.remove(id);
                if self.base.remove(id) {
                    self.base_record = None;
                }
            } else {
                self.unadmitted.insert(id.into());
            }
        }
    }

    fn unlist_message(&mut self, message: &HistoryMessage, admitted: &history::AdmittedIds) {
        for id in &Self::tool_uses(message) {
            let id = id.as_str();
            let Some(count) = self.listed.get_mut(id) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.listed.remove(id);
                self.unlisted(id, admitted);
            }
        }
    }

    fn admit_new(&mut self, admitted: &history::AdmittedIds) {
        for id in admitted.since(self.seen) {
            if self.listed.contains_key(id) {
                self.unadmitted.remove(id);
            } else {
                self.unlisted(id, admitted);
            }
        }
        self.seen = admitted.len();
    }

    fn settle(&mut self, mark: usize) {
        self.mark = mark;
        if !self.recent.is_empty() {
            self.base.append(&mut self.recent);
            self.base_record = None;
        }
    }

    fn base_record(&mut self) -> Result<Option<EncodedPayload>> {
        if self.base.is_empty() {
            return Ok(None);
        }
        if self.base_record.is_none() {
            self.base_record = Some(
                EncodedPayload::encode(&AdmittedBase { ids: &self.base })
                    .map_err(provider_error)?,
            );
        }
        Ok(self.base_record.clone())
    }
}

#[derive(Serialize)]
struct TemplatePrompt<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a Arc<Value>>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [ClaudeToolSpec],
}

pub(super) trait Paged: Clone {
    fn same(&self, other: &Self) -> bool;
    fn record(&self) -> serde_json::Result<EncodedPayload>;
    fn committed(&self) {}
}

impl Paged for HistoryMessage {
    fn same(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }

    fn record(&self) -> serde_json::Result<EncodedPayload> {
        Self::record(self)
    }

    fn committed(&self) {
        Self::committed(self);
    }
}

const PAGED_GROUP_MESSAGES: usize = 16;

#[derive(Clone)]
pub(super) struct PagedGroup(Arc<PagedEntries>);

struct PagedEntries {
    entries: Vec<(usize, HistoryMessage)>,
    record: std::sync::OnceLock<EncodedPayload>,
}

type PagedEntry<'a> = (usize, &'a [(usize, u64)], &'a RawValue);

#[derive(Serialize)]
struct PagedRecord<'a> {
    entries: Vec<PagedEntry<'a>>,
}

fn paged_groups(messages: &[HistoryMessage]) -> Vec<PagedGroup> {
    let mut groups: Vec<Vec<(usize, HistoryMessage)>> = Vec::new();
    for (at, message) in messages.iter().enumerate() {
        if message.paged_parts().is_none() {
            continue;
        }
        match groups.last_mut() {
            Some(group)
                if group.first().map(|(first, _)| first / PAGED_GROUP_MESSAGES)
                    == Some(at / PAGED_GROUP_MESSAGES) =>
            {
                group.push((at, message.clone()));
            }
            _ => groups.push(vec![(at, message.clone())]),
        }
    }
    groups.pop();
    groups
        .into_iter()
        .map(|entries| {
            PagedGroup(Arc::new(PagedEntries {
                entries,
                record: std::sync::OnceLock::new(),
            }))
        })
        .collect()
}

impl Paged for PagedGroup {
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.0.entries.len() == other.0.entries.len()
                && self.0.entries.iter().zip(&other.0.entries).all(
                    |((at, message), (other_at, other))| at == other_at && message.ptr_eq(other),
                ))
    }

    fn record(&self) -> serde_json::Result<EncodedPayload> {
        if let Some(record) = self.0.record.get() {
            return Ok(record.clone());
        }
        let parts = self
            .0
            .entries
            .iter()
            .map(|(at, message)| {
                let (results, cleared) = message
                    .paged_parts()
                    .expect("a paged group holds paged messages");
                Ok((
                    *at,
                    results,
                    serde_json::from_str::<&RawValue>(&cleared)?.to_owned(),
                ))
            })
            .collect::<serde_json::Result<Vec<_>>>()?;
        let json = serde_json::to_string(&PagedRecord {
            entries: parts
                .iter()
                .map(|(at, results, cleared)| (*at, *results, &**cleared))
                .collect(),
        })?;
        let record = EncodedPayload::from_json(json.into());
        Ok(self.0.record.get_or_init(|| record).clone())
    }
}

impl Paged for history::CodeRound {
    fn same(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }

    fn record(&self) -> serde_json::Result<EncodedPayload> {
        Self::record(self)
    }
}

pub(super) struct Pages<T> {
    items: Vec<T>,
    log: SequenceLog,
    released: usize,
}

impl<T> Default for Pages<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            log: SequenceLog::default(),
            released: 0,
        }
    }
}

impl<T: Paged> Pages<T> {
    fn unchanged(&self, items: &[T], base: usize) -> usize {
        self.items
            .iter()
            .zip(items)
            .take_while(|(cached, item)| cached.same(item))
            .count()
            .min(self.log.len() - base)
    }

    fn page(
        &mut self,
        items: &[T],
        base: usize,
        unchanged: usize,
        records: &mut Vec<EncodedPayload>,
        stored: Option<&HashSet<String>>,
    ) -> Result<Sequence> {
        self.log
            .sync_known(
                base + unchanged,
                items[unchanged..].iter().map(|item| {
                    let record = item
                        .record()
                        .map_err(nanocode_durability::Error::InvalidPayload)?;
                    Ok(match stored {
                        Some(stored) if stored.contains(record.key()) => record.reference(),
                        _ => record,
                    })
                }),
                stored,
            )
            .map_err(provider_error)?;
        self.items.truncate(unchanged);
        self.released = self.released.min(unchanged);
        self.items.extend_from_slice(&items[unchanged..]);
        records.extend(self.log.frontier().cloned());
        Ok(self.log.sequence())
    }

    fn committed(&mut self) {
        self.items[self.released..].iter().for_each(T::committed);
        self.released = self.items.len();
        self.log.release();
    }
}

#[derive(Default)]
pub(super) struct RoundPages {
    pages: Pages<history::CodeRound>,
    omitted: u64,
    skip: usize,
}

impl RoundPages {
    fn page(
        &mut self,
        rounds: &[history::CodeRound],
        omitted: u64,
        records: &mut Vec<EncodedPayload>,
        stored: Option<&HashSet<String>>,
    ) -> Result<(Sequence, usize)> {
        let dropped = omitted
            .checked_sub(self.omitted)
            .and_then(|dropped| usize::try_from(dropped).ok())
            .filter(|dropped| *dropped <= self.pages.items.len());
        match dropped {
            Some(dropped) => {
                self.pages.items.drain(..dropped);
                self.pages.released = self.pages.released.saturating_sub(dropped);
                self.skip += dropped;
            }
            None => *self = Self::default(),
        }
        self.omitted = omitted;
        let unchanged = self.pages.unchanged(rounds, self.skip);
        if self.skip + unchanged < self.pages.log.floor() {
            *self = Self {
                omitted,
                ..Self::default()
            };
        }
        let unchanged = self.pages.unchanged(rounds, self.skip);
        let sequence = self
            .pages
            .page(rounds, self.skip, unchanged, records, stored)?;
        Ok((sequence, self.skip))
    }

    fn committed(&mut self) {
        self.pages.committed();
    }
}

#[derive(Default)]
pub(super) struct CompactedLog {
    log: SequenceLog,
}

impl CompactedLog {
    fn page(
        &mut self,
        tail: &mut Vec<HistoryMessage>,
        pages: &mut Option<Sequence>,
        records: &mut Vec<EncodedPayload>,
        stored: Option<&HashSet<String>>,
    ) -> Result<Option<Sequence>> {
        if tail.is_empty() && pages.is_none() {
            return Ok(None);
        }
        let base = pages.take().unwrap_or_default();
        if self.log.sequence() != base {
            self.log = SequenceLog::resume(&base).ok_or_else(|| {
                provider_error("Claude compacted history pages are not resumable")
            })?;
        }
        let unchanged = self.log.len();
        self.log
            .sync_known(
                unchanged,
                tail.drain(..).map(|item| {
                    let record = item
                        .record()
                        .map_err(nanocode_durability::Error::InvalidPayload)?;
                    Ok(match stored {
                        Some(stored) if stored.contains(record.key()) => record.reference(),
                        _ => record,
                    })
                }),
                stored,
            )
            .map_err(provider_error)?;
        records.extend(self.log.frontier().cloned());
        let sequence = self.log.sequence();
        *pages = Some(sequence.clone());
        Ok(Some(sequence))
    }

    fn committed(&mut self) {
        self.log.release();
        if let Some(log) = self.log.forget_sealed() {
            self.log = log;
        }
    }
}

#[derive(Default)]
pub(super) struct Paging {
    paged: Pages<PagedGroup>,
    messages: Pages<HistoryMessage>,
    code_calls: RoundPages,
    compacted: CompactedLog,
    pending: Pages<HistoryMessage>,
    tools: ToolIds,
    template: Option<Arc<EncodedTemplate>>,
    pooled: Option<String>,
    stored: Option<HashSet<String>>,
}

impl Paging {
    fn stored(&mut self, policy: Option<&dyn ClaudeExecutionPolicy>) {
        if let Some(stored) = policy.and_then(ClaudeExecutionPolicy::take_stored_records) {
            self.stored = Some(stored);
        }
    }

    fn committed(&mut self) {
        self.stored = None;
        self.messages.committed();
        self.code_calls.committed();
        self.compacted.committed();
        self.pending.committed();
        self.paged.committed();
    }
}

struct EncodedTemplate {
    system: Option<Arc<Value>>,
    tools: Vec<ClaudeToolSpec>,
    record: EncodedPayload,
}

impl EncodedTemplate {
    fn encodes(&self, prompt: &TemplatePrompt<'_>) -> bool {
        let system = match (&self.system, prompt.system) {
            (Some(kept), Some(system)) => Arc::ptr_eq(kept, system) || kept == system,
            (kept, system) => kept.is_none() && system.is_none(),
        };
        system && self.tools.as_slice() == prompt.tools
    }
}

fn template_record(prompt: &TemplatePrompt<'_>, paging: &mut Paging) -> Result<EncodedPayload> {
    static LAST: std::sync::LazyLock<nanocode_sync::Atom<Option<Arc<EncodedTemplate>>>> =
        std::sync::LazyLock::new(nanocode_sync::Atom::default);
    if let Some(template) = &paging.template
        && template.encodes(prompt)
    {
        return Ok(template.record.clone());
    }
    let template = match LAST.read(Clone::clone).filter(|last| last.encodes(prompt)) {
        Some(last) => last,
        None => {
            let template = Arc::new(EncodedTemplate {
                system: prompt.system.cloned(),
                tools: prompt.tools.to_vec(),
                record: EncodedPayload::encode(prompt).map_err(provider_error)?,
            });
            LAST.update(|last| *last = Some(Arc::clone(&template)));
            template
        }
    };
    let record = template.record.clone();
    paging.template = Some(template);
    Ok(record)
}

fn encode_snapshot(
    snapshot: &mut Snapshot,
    paged: bool,
    paging: &mut Paging,
) -> Result<EncodedState> {
    if !paged {
        return serde_json::value::to_raw_value(&*snapshot)
            .map(|json| EncodedState {
                json,
                records: Vec::new(),
            })
            .map_err(provider_error);
    }
    let mut detached = Detached::take(&mut snapshot.conversation);
    let encoded = paged_snapshot(snapshot, &mut detached, paging);
    detached.restore(&mut snapshot.conversation);
    encoded
}

fn paged_snapshot(
    snapshot: &Snapshot,
    detached: &mut Detached,
    paging: &mut Paging,
) -> Result<EncodedState> {
    let mut records = Vec::new();
    let conversation = detached.page(&mut records, paging, true)?;
    let instructions = snapshot
        .context
        .as_ref()
        .filter(|_| {
            snapshot
                .lineage
                .as_ref()
                .is_none_or(|lineage| lineage.depth == 0)
        })
        .and_then(|context| context.text.as_deref())
        .map(EncodedPayload::encode)
        .transpose()
        .map_err(provider_error)?;
    let template = snapshot
        .context
        .as_ref()
        .and_then(|context| context.prefix.as_ref())
        .map(|prefix| {
            let template = template_record(
                &TemplatePrompt {
                    system: prefix.system.as_ref(),
                    tools: &prefix.tools,
                },
                paging,
            )?;
            crate::durability::keep_prefix(template.key(), prefix);
            Ok::<_, NanocodeError>(template)
        })
        .transpose()?;
    let inner = serde_json::value::to_raw_value(snapshot).map_err(provider_error)?;
    let json = serde_json::value::to_raw_value(&PagedSnapshot {
        provider: &snapshot.provider,
        version: PAGED_VERSION,
        conversation,
        instructions: instructions.as_ref().map(EncodedPayload::reference),
        template: template.as_ref().map(EncodedPayload::reference),
        snapshot: &inner,
    })
    .map_err(provider_error)?;
    records.extend(instructions);
    records.extend(template.filter(|template| paging.pooled.as_deref() != Some(template.key())));
    Ok(EncodedState { json, records })
}

fn paged_cursor(
    cursor: &Cursor,
    detached: &mut Detached,
    pending: &[HistoryMessage],
    prompt: &TemplatePrompt<'_>,
    paging: &mut Paging,
) -> Result<EncodedState> {
    let mut records = Vec::new();
    let conversation = detached.page(&mut records, paging, false)?;
    let unchanged = paging.pending.unchanged(pending, 0);
    let pending =
        paging
            .pending
            .page(pending, 0, unchanged, &mut records, paging.stored.as_ref())?;
    let template = template_record(prompt, paging)?;
    let inner = serde_json::value::to_raw_value(cursor).map_err(provider_error)?;
    let json = serde_json::value::to_raw_value(&PagedCursor {
        version: PAGED_VERSION,
        conversation,
        pending,
        template: template.reference(),
        cursor: &inner,
    })
    .map_err(provider_error)?;
    if paging.pooled.as_deref() != Some(template.key()) {
        records.push(template);
    }
    Ok(EncodedState { json, records })
}

fn encode_cursor(cursor: &mut Cursor, paged: bool, paging: &mut Paging) -> Result<EncodedState> {
    if paged {
        let mut detached = Detached::take(&mut cursor.snapshot.conversation);
        let pending = std::mem::take(&mut cursor.pending);
        let system = cursor.template.system.take();
        let tools = std::mem::take(&mut cursor.template.tools);
        let prompt = TemplatePrompt {
            system: system.as_ref(),
            tools: &tools,
        };
        let encoded = paged_cursor(cursor, &mut detached, &pending, &prompt, paging);
        detached.restore(&mut cursor.snapshot.conversation);
        cursor.pending = pending;
        cursor.template.system = system;
        cursor.template.tools = tools;
        return encoded;
    }
    let shared = cursor
        .snapshot
        .conversation
        .messages
        .iter()
        .zip(&cursor.pending)
        .take_while(|(committed, pending)| committed.ptr_eq(pending))
        .count();
    let base = u32::try_from(shared).ok().filter(|base| *base > 0);
    let pending = std::mem::take(&mut cursor.pending);
    cursor.pending = pending[base.map_or(0, |_| shared)..].to_vec();
    cursor.pending_base = base;
    let encoded = serde_json::value::to_raw_value(&*cursor);
    cursor.pending = pending;
    cursor.pending_base = None;
    encoded
        .map(|json| EncodedState {
            json,
            records: Vec::new(),
        })
        .map_err(provider_error)
}

fn turn_output(result: &TurnResult, last: Option<&HistoryMessage>) -> Result<Box<RawValue>> {
    let final_message = result.final_message();
    let record = last
        .filter(|message| {
            message.role() == Role::Assistant
                && !final_message.is_empty()
                && text_of(&message.view()) == final_message
        })
        .map(HistoryMessage::record)
        .transpose()
        .map_err(provider_error)?;
    #[derive(Serialize)]
    struct Output<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        final_message: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        final_record: Option<&'a str>,
        usage: Option<&'a TurnUsage>,
    }
    serde_json::value::to_raw_value(&Output {
        final_message: record.is_none().then_some(final_message),
        final_record: record.as_ref().map(EncodedPayload::key),
        usage: result.usage(),
    })
    .map_err(provider_error)
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn replay(operation: String, output: Value) -> Result<TurnResult> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Output {
        final_message: String,
        usage: Option<TurnUsage>,
    }
    let output: Output = serde_json::from_value(output).map_err(recovery_error)?;
    Ok(TurnResult::from_backend(
        Some(operation),
        output.final_message,
        output.usage,
        None,
    ))
}
pub(super) fn candidate_id(kind: &str) -> String {
    format!("claude-{kind}-{}", nanocode_platform::uuid_v4())
}

pub(super) fn recovery_error(error: impl std::fmt::Display) -> NanocodeError {
    NanocodeError::execution_policy_with_disposition(
        "Claude recovery",
        nanocode_agent_core::ExecutionPolicyDisposition::Reopen,
        provider_error(error),
    )
}

pub fn rewind_checkpoint(
    source_session_id: &str,
    previous: Option<Value>,
    latest: Value,
) -> Result<Value> {
    let latest = Snapshot::decode(latest)?;

    let source_lineage = latest
        .lineage
        .clone()
        .unwrap_or_else(|| Lineage::root(source_session_id));
    if latest.conversation.pending_continuation {
        return Err(unsupported(
            "conversation rewind refuses a pending tool/provider continuation",
        ));
    }
    let mut selected = match previous {
        Some(value) => Snapshot::decode(value)?,
        None => Snapshot {
            model: latest.model.clone(),
            workspace: latest.workspace.clone(),
            effort: latest.effort,
            thinking_disabled: latest.thinking_disabled,
            fast_mode: latest.fast_mode,
            ..Snapshot::default()
        },
    };
    if selected.conversation.pending_continuation {
        return Err(unsupported(
            "selected checkpoint has a pending tool/provider continuation",
        ));
    }
    selected
        .conversation
        .admitted_tool_ids
        .extend(latest.conversation.admitted_tool_ids);
    for notice in latest.conversation.recovery_notices {
        if !selected.conversation.recovery_notices.contains(&notice) {
            selected.conversation.recovery_notices.push(notice);
        }
    }
    let notice = "This conversation was explicitly rewound into a new session. External effects from discarded turns may still exist; reconcile their current state before repeating any action. Historical tool calls must not be replayed.".to_owned();
    if !selected.conversation.recovery_notices.contains(&notice) {
        selected.conversation.recovery_notices.push(notice);
    }
    selected.conversation.lifecycle_started = false;
    selected.conversation.fallback_latch = FallbackLatch::Main;
    selected.conversation.container = None;
    selected.conversation.previous_message_id = None;

    selected.lineage = Some(Lineage::child_of(
        &source_lineage,
        source_session_id,
        Origin::Branch,
    ));
    selected.conversation_id = None;
    serde_json::to_value(selected).map_err(provider_error)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeCheckpointView {
    pub model: Option<ModelSelection>,

    pub workspace: Option<String>,

    pub lineage: Option<Lineage>,

    pub transcript: Vec<nanocode_agent_core::session::TranscriptItem>,
}

pub fn decode_checkpoint(checkpoint: Value) -> Result<ClaudeCheckpointView> {
    let snapshot: Snapshot = serde_json::from_value::<Snapshot>(checkpoint)
        .map_err(|error| NanocodeError::InvalidCheckpoint(error.to_string()))?
        .validated()?;
    snapshot.view()
}

/// The thinking level [`session_checkpoint`] gives `checkpoint`, read from
/// only the fields it derives it from (the provider, version, model, effort
/// and whether thinking is disabled) rather than decoding and re-encoding
/// the whole conversation: opening a stored session asks for it first, and
/// on a session of thousands of messages the full round trip took as long
/// as the rest of the open.
pub fn checkpoint_thinking(checkpoint: &Value) -> Result<Thinking> {
    let invalid = |message: String| NanocodeError::InvalidCheckpoint(message);
    if checkpoint.get("provider").and_then(Value::as_str) != Some("claude")
        || checkpoint.get("version").and_then(Value::as_u64) != Some(1)
    {
        return Err(invalid(
            "unsupported Claude checkpoint version/provider".into(),
        ));
    }
    let model = match checkpoint.get("model") {
        None | Some(Value::Null) => None,
        Some(model) => Some(
            model
                .as_str()
                .ok_or_else(|| invalid("checkpoint model must be a string".into()))?,
        ),
    }
    .filter(|model| !model.trim().is_empty())
    .ok_or_else(|| invalid("checkpoint records no Claude model".into()))?;
    let effort = match checkpoint.get("effort") {
        None | Some(Value::Null) => None,
        Some(effort) => {
            Some(crate::Effort::deserialize(effort).map_err(|error| invalid(error.to_string()))?)
        }
    };
    let thinking_disabled = match checkpoint.get("thinking_disabled") {
        None => false,
        Some(disabled) => {
            bool::deserialize(disabled).map_err(|error| invalid(error.to_string()))?
        }
    };
    Ok(if effort.is_none() && !thinking_disabled {
        ModelSelection::new(HarnessFamily::Claude, model.to_owned()).default_thinking()
    } else {
        effort_thinking(effort)
    })
}

pub fn session_checkpoint(
    session_id: &str,
    lineage: Lineage,
    checkpoint: Value,
) -> Result<SessionCheckpoint> {
    let mut snapshot = Snapshot::decode(checkpoint)?;
    let model = snapshot
        .model
        .clone()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            NanocodeError::InvalidCheckpoint("checkpoint records no Claude model".into())
        })?;
    let conversation_id = snapshot
        .conversation_id
        .clone()
        .unwrap_or_else(|| session_id.to_owned());
    snapshot.lineage = Some(lineage.clone());
    let policy = NativePolicy {
        context_window_tokens: default_context_window_tokens(&model),
        max_tokens: None,
        effort: snapshot.effort,
        adaptive_thinking: snapshot.effort.is_some(),
        thinking_disabled: snapshot.thinking_disabled,
        automatic_cache: false,
        cache_one_hour: false,
        keep_thinking: false,
        fast_mode: snapshot.fast_mode,
        message_diagnostics: false,
        auto_compact_window_tokens: None,
        model,
    };
    ClaudeBoundary {
        snapshot: Arc::new(snapshot),
        policy,
        session_id: session_id.to_owned(),
        lineage,
        conversation_id,
    }
    .checkpoint()
}

pub fn decode_session_checkpoint(checkpoint: &SessionCheckpoint) -> Result<ClaudeCheckpointView> {
    checkpoint.validate()?;
    checkpoint.require_family(HarnessFamily::Claude)?;
    let stored: NativeChildState = checkpoint
        .payload()
        .decode()
        .map_err(|error| NanocodeError::InvalidCheckpoint(error.to_string()))?;
    let mut view = stored.snapshot.validated()?.view()?;
    view.model = Some(checkpoint.model());
    view.lineage = Some(checkpoint.lineage().clone());
    Ok(view)
}

impl Snapshot {
    fn view(&self) -> Result<ClaudeCheckpointView> {
        let model = self
            .model
            .as_deref()
            .map(|model| {
                (!model.trim().is_empty())
                    .then(|| super::claude_selection(model))
                    .ok_or_else(|| {
                        NanocodeError::InvalidCheckpoint(
                            "checkpoint model is not a Claude model".into(),
                        )
                    })
            })
            .transpose()?;
        Ok(ClaudeCheckpointView {
            model,
            workspace: self
                .workspace
                .clone()
                .filter(|workspace| !workspace.is_empty()),
            lineage: self.lineage.clone(),
            transcript: self.transcript(),
        })
    }
    fn transcript(&self) -> Vec<nanocode_agent_core::session::TranscriptItem> {
        use nanocode_agent_core::session::TranscriptItem;
        let mut items = Vec::new();
        if !self.conversation.summary.is_empty() {
            items.push(TranscriptItem::Assistant(format!(
                "Retained conversation summary:\n{}",
                self.conversation.summary
            )));
        }
        for message in &self.conversation.messages {
            let view = message.view();
            for block in &view.content {
                match block {
                    ContentBlock::Text { text, .. } => items.push(match message.role() {
                        Role::Assistant => TranscriptItem::Assistant(text.clone()),
                        Role::User => TranscriptItem::User(text.clone()),
                        Role::System => continue,
                    }),
                    ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                        items.push(TranscriptItem::Reasoning(thinking.clone()));
                    }
                    ContentBlock::Compaction { content, .. } => {
                        items.push(TranscriptItem::Assistant(format!(
                            "Retained conversation summary:\n{content}"
                        )));
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    }
                    | ContentBlock::ServerToolUse {
                        id, name, input, ..
                    } => {
                        items.push(TranscriptItem::Tool {
                            call_id: id.clone(),
                            name: name.clone(),
                            arguments: input.to_string(),
                            parent_call_id: None,
                        });
                    }
                    ContentBlock::McpToolUse {
                        id,
                        name,
                        server_name,
                        input,
                        ..
                    } => items.push(TranscriptItem::Tool {
                        call_id: id.clone(),
                        name: format!("mcp__{server_name}__{name}"),
                        arguments: input.to_string(),
                        parent_call_id: None,
                    }),
                    _ => {}
                }
            }
        }
        items
    }
}

fn unavailable(name: &str, tools: &[ClaudeToolSpec]) -> String {
    let offered = tools
        .iter()
        .filter_map(|tool| match tool {
            ClaudeToolSpec::Client(tool) => Some(tool.name.as_str()),
            ClaudeToolSpec::Server(_) => None,
        })
        .collect::<Vec<_>>();
    let hint = match name {
        "Grep" | "Glob" if !offered.contains(&"Bash") => {
            format!(". {name} is disabled for this session.")
        }
        "Grep" => ". Grep is not available in this session \u{2014} search file contents with `grep` via the Bash tool instead.".to_owned(),
        "Glob" => ". Glob is not available in this session \u{2014} find files with `find` via the Bash tool instead.".to_owned(),
        _ => match offered
            .iter()
            .filter(|tool| tool.eq_ignore_ascii_case(name))
            .collect::<Vec<_>>()[..]
        {
            [tool] => format!(". Tool names are case-sensitive: call {tool} instead."),
            _ => String::new(),
        },
    };
    format!("<tool_use_error>Error: No such tool available: {name}{hint}</tool_use_error>")
}

#[cfg(test)]
mod thinking_tests {
    use super::*;

    fn stored(fields: Value) -> Value {
        let mut snapshot = serde_json::to_value(Snapshot::default()).unwrap();
        snapshot["model"] = json!("claude-opus-5-5");
        for (key, value) in fields.as_object().unwrap() {
            snapshot[key] = value.clone();
        }
        snapshot
    }

    #[test]
    fn checkpoint_thinking_matches_session_checkpoint() {
        for fields in [
            json!({}),
            json!({"effort": "low"}),
            json!({"effort": "max"}),
            json!({"effort": null}),
            json!({"thinking_disabled": true}),
            json!({"thinking_disabled": true, "effort": "high"}),
            json!({"model": "claude-sonnet-4-6"}),
            json!({"model": "claude-haiku-4-5", "effort": "medium"}),
        ] {
            let checkpoint = stored(fields.clone());
            let full = session_checkpoint("s", Lineage::root("s"), checkpoint.clone())
                .unwrap()
                .thinking();
            assert_eq!(checkpoint_thinking(&checkpoint).unwrap(), full, "{fields}");
        }
    }

    #[test]
    fn checkpoint_thinking_refuses_what_session_checkpoint_refuses() {
        for fields in [
            json!({"model": null}),
            json!({"model": "  "}),
            json!({"provider": "codex"}),
            json!({"version": 2}),
            json!({"effort": "loud"}),
            json!({"thinking_disabled": "yes"}),
        ] {
            let checkpoint = stored(fields.clone());
            assert!(
                session_checkpoint("s", Lineage::root("s"), checkpoint.clone()).is_err(),
                "{fields}"
            );
            assert!(checkpoint_thinking(&checkpoint).is_err(), "{fields}");
        }
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    #[test]
    fn string_scan_matches_serialized_search() {
        let needle = "0193-abc-bash-";
        let blocks = [
            json!({"type": "text", "text": "nothing here"}),
            json!({"type": "text", "text": "running in background with ID: 0193-abc-bash-7"}),
            json!({"type": "tool_use", "id": "toolu_1", "name": "exec", "input": {"nested": [{"deep": "x 0193-abc-bash-2 y"}]}}),
            json!({"type": "tool_use", "id": "toolu_2", "name": "exec", "input": {"0193-abc-bash-key": 1}}),
            json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "<task-id>0193-abc-bash-7</task-id>"}]}),
            json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": "0193-abc-"}),
        ];
        for block in blocks {
            let block: ContentBlock = serde_json::from_value(block.clone()).unwrap();
            let json = serde_json::to_string(&block).unwrap();
            assert_eq!(
                serializes_str_containing(&block, needle),
                json.contains(needle),
                "{json}"
            );
        }
    }

    fn history(messages: Value) -> Vec<HistoryMessage> {
        serde_json::from_value(messages).unwrap()
    }

    #[test]
    fn stopped_background_notices_finds_unsettled_commands() {
        for session in ["0193-abc", "odd \"session\""] {
            let messages = history(json!([
                {"role": "user", "content": [{"type": "text", "text": "start"}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                    {"type": "text", "text": format!("Command running in background with ID: {session}-bash-1. Output is being written to /tmp/1")}
                ]}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2", "content": [
                    {"type": "text", "text": format!("Command running in background with ID: {session}-bash-2.")}
                ]}]},
                {"role": "user", "content": [{"type": "text", "text": format!("<task-notification><task-id>{session}-bash-2</task-id></task-notification>")}]},
                {"role": "assistant", "content": [{"type": "text", "text": "Command running in background with ID: other-bash-3"}]}
            ]));
            let ids = stopped_background_notices(session, &messages, &[])
                .into_iter()
                .map(|(notice, _)| notice)
                .collect::<Vec<_>>();
            let first = format!("{BACKGROUND_NOTICE_PREFIX}{session}-bash-1");
            if session.contains('"') {
                // A session ID JSON escapes takes the old path: every block
                // serialized, matched against its escaped form.
                assert!(ids.is_empty(), "{ids:?}");
            } else {
                assert_eq!(ids, vec![first], "{session}");
            }
            let queued = ids
                .iter()
                .map(|id| (id.clone(), String::new()))
                .collect::<Vec<_>>();
            assert!(stopped_background_notices(session, &messages, &queued).is_empty());
        }
    }
}

#[cfg(test)]
mod compacted_tests {
    use super::*;

    const KEY_BYTES: usize = 64;
    const TURNS: usize = 500;
    const PER_TURN: usize = 4;

    fn compacted_message(turn: usize, index: usize) -> HistoryMessage {
        HistoryMessage::from(Message::text(
            Role::User,
            format!("turn {turn} message {index} {}", "x".repeat(2048)),
        ))
    }

    fn held_compacted_bytes(snapshot: &Snapshot, paging: &Paging) -> usize {
        let tail = snapshot
            .conversation
            .compacted
            .iter()
            .map(|message| serde_json::to_string(message).unwrap().len())
            .sum::<usize>();
        tail + paging.compacted.log.held() * KEY_BYTES
    }

    #[test]
    fn a_running_conversation_holds_only_its_unpersisted_compacted_tail() {
        let mut paging = Paging::default();
        let mut snapshot = Snapshot::default();
        for turn in 0..TURNS {
            for index in 0..PER_TURN {
                snapshot
                    .conversation
                    .compacted
                    .push(compacted_message(turn, index));
            }
            encode_snapshot(&mut snapshot, true, &mut paging).unwrap();
            paging.committed();
            let held = held_compacted_bytes(&snapshot, &paging);
            assert!(
                held <= 64 * 1024,
                "turn {turn}: the conversation holds {held} bytes of compacted history"
            );
        }
    }

    #[test]
    fn paged_compacted_history_round_trips_without_decoding() {
        let mut paging = Paging::default();
        let mut snapshot = Snapshot::default();
        let mut staged = HashSet::new();
        let mut full = SequenceLog::default();
        let mut refs = Vec::new();
        let mut last = None;
        for turn in 0..TURNS {
            for index in 0..PER_TURN {
                let message = compacted_message(turn, index);
                let reference = message.record().unwrap().reference();
                full.push(reference.clone()).unwrap();
                refs.push(reference);
                snapshot.conversation.compacted.push(message);
            }
            let encoded = encode_snapshot(&mut snapshot, true, &mut paging).unwrap();
            let mut records: Vec<nanocode_durability::StoreRecord> = Vec::new();
            for mut record in encoded.records {
                record.stage(&mut records);
            }
            staged.extend(records.into_iter().map(|record| record.key));
            last = Some(encoded.json);
            paging.committed();
        }
        let sequence = snapshot.conversation.compacted_pages.clone().unwrap();
        assert!(snapshot.conversation.compacted.is_empty());
        assert_eq!(sequence.count(), Some(TURNS * PER_TURN));
        assert_eq!(
            serde_json::to_string(&sequence).unwrap(),
            serde_json::to_string(&full.sequence()).unwrap()
        );
        let missing = refs
            .iter()
            .filter(|reference| !staged.contains(reference.key()))
            .count();
        assert_eq!(missing, 0, "compacted messages never staged for the store");
        let stored: Value = serde_json::from_str(last.unwrap().get()).unwrap();
        assert_eq!(
            stored["compacted"],
            serde_json::to_value(&sequence).unwrap()
        );
        let decoded = Snapshot::decode_paged(
            stored["snapshot"].clone(),
            ContinuationPages {
                messages: Vec::new(),
                code_calls: None,
                compacted: None,
                compacted_pages: Some(sequence.clone()),
            },
        )
        .unwrap();
        assert!(decoded.conversation.compacted.is_empty());
        assert_eq!(
            decoded
                .conversation
                .compacted_pages
                .as_ref()
                .and_then(Sequence::count),
            Some(TURNS * PER_TURN)
        );
        let whole = Snapshot::decode(serde_json::to_value(&snapshot).unwrap()).unwrap();
        assert!(whole == snapshot);
    }

    fn page_turns(
        snapshot: &mut Snapshot,
        paging: &mut Paging,
        turns: std::ops::Range<usize>,
        paged: bool,
    ) -> Vec<HistoryMessage> {
        let mut added = Vec::new();
        for turn in turns {
            for index in 0..PER_TURN {
                let message = compacted_message(turn, index);
                added.push(message.clone());
                snapshot.conversation.compacted.push(message);
            }
            encode_snapshot(snapshot, paged, paging).unwrap();
            paging.committed();
        }
        added
    }

    fn full_sequence(messages: &[HistoryMessage]) -> String {
        let mut full = SequenceLog::default();
        for message in messages {
            full.push(message.record().unwrap().reference()).unwrap();
        }
        serde_json::to_string(&full.sequence()).unwrap()
    }

    #[test]
    fn restoring_an_earlier_snapshot_resumes_its_compacted_pages() {
        let mut paging = Paging::default();
        let mut snapshot = Snapshot::default();
        let mut kept = page_turns(&mut snapshot, &mut paging, 0..10, true);
        let earlier = snapshot.clone();
        page_turns(&mut snapshot, &mut paging, 10..20, true);
        snapshot = earlier;
        kept.extend(page_turns(&mut snapshot, &mut paging, 100..101, true));
        assert!(snapshot.conversation.compacted.is_empty());
        let sequence = snapshot.conversation.compacted_pages.clone().unwrap();
        assert_eq!(sequence.count(), Some(kept.len()));
        assert_eq!(
            serde_json::to_string(&sequence).unwrap(),
            full_sequence(&kept)
        );
    }

    #[test]
    fn a_non_paged_policy_keeps_every_compacted_message() {
        let mut paging = Paging::default();
        let mut snapshot = Snapshot::default();
        let added = page_turns(&mut snapshot, &mut paging, 0..10, false);
        assert!(snapshot.conversation.compacted_pages.is_none());
        assert_eq!(snapshot.conversation.compacted.len(), added.len());
        let encoded = encode_snapshot(&mut snapshot, false, &mut paging).unwrap();
        let stored: Value = serde_json::from_str(encoded.json.get()).unwrap();
        assert_eq!(
            stored["conversation"]["compacted"].as_array().map(Vec::len),
            Some(added.len())
        );
        assert!(stored["conversation"].get("compacted_pages").is_none());
    }

    #[test]
    fn a_fork_writes_the_whole_compacted_history_into_its_child() {
        let mut paging = Paging::default();
        let mut snapshot = Snapshot::default();
        let persisted = page_turns(&mut snapshot, &mut paging, 0..10, true);
        let tail = (0..PER_TURN)
            .map(|index| compacted_message(10, index))
            .collect::<Vec<_>>();
        snapshot.conversation.compacted.extend(tail.iter().cloned());
        let pages = snapshot.conversation.compacted_pages.clone().unwrap();
        assert_eq!(
            serde_json::to_string(&pages).unwrap(),
            full_sequence(&persisted)
        );
        let loaded = persisted
            .iter()
            .map(|message| message.record().unwrap())
            .collect();
        prepend_compacted(&mut snapshot.conversation, loaded).unwrap();
        let child = serde_json::to_value(&snapshot).unwrap();
        assert!(child["conversation"].get("compacted_pages").is_none());
        let expected = persisted
            .iter()
            .chain(&tail)
            .map(|message| serde_json::to_value(message).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(child["conversation"]["compacted"], Value::Array(expected));
    }
}
