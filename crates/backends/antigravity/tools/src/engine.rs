use crate::files;
use crate::format::{duration_label, go_json, timestamp};
use crate::schedule::Cron;
use nanocode_process_contracts::{ExecCommand, FileHost, OutputUnit, ShellExecutor, WriteStdin};
use nanocode_sync::Atom;
use serde_json::{Value, json};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[cfg(not(target_family = "wasm"))]
pub type WebSearchFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
#[cfg(target_family = "wasm")]
pub type WebSearchFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + 'a>>;

pub trait WebSearch: Send + Sync {
    fn search<'a>(&'a self, query: &'a str, domain: Option<&'a str>) -> WebSearchFuture<'a>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Output(String),
    Background(String),
    Error(String),
}

#[derive(Clone, Debug)]
pub struct Message {
    pub at: SystemTime,
    pub sender: String,
    pub high: bool,
    pub content: String,
    pub seen: bool,
}

impl Message {
    pub fn render(&self) -> String {
        let priority = match self.high {
            true => "MESSAGE_PRIORITY_HIGH",
            false => "MESSAGE_PRIORITY_LOW",
        };
        format!(
            "[Message] timestamp={} sender={} priority={priority} content={}",
            timestamp(self.at),
            self.sender,
            self.content
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Running,
    Done,
    Canceled,
}

#[derive(Clone)]
enum Kind {
    Process,
    Timer { condition: Option<String> },
    Cron,
}

#[derive(Clone)]
struct Task {
    id: String,
    tool: &'static str,
    summary: String,
    description: String,
    started: SystemTime,
    progress: SystemTime,
    log: PathBuf,
    daemon: bool,
    kind: Kind,
    state: State,
    output: String,
    input: Vec<String>,
}

struct Inner {
    shell: Arc<dyn ShellExecutor>,
    files: Arc<dyn FileHost>,
    workspace: PathBuf,
    brain: PathBuf,
    conversation: String,
    web: Option<Arc<dyn WebSearch>>,
    tasks: Atom<Vec<Arc<Task>>>,
    messages: Atom<Vec<Message>>,
    discard_steps: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

const UNLIMITED: usize = 1 << 30;

fn now() -> SystemTime {
    nanocode_platform::system_time()
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn integer(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_f64().map(|number| number as i64))
    })
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn command_body(code: i32, output: &str) -> String {
    match output.is_empty() {
        true => format!("Command exited with code {code}.\n\nNo output from the command."),
        false => format!("Command exited with code {code}.\n\nCommand output:\n{output}"),
    }
}

impl Engine {
    pub fn new(
        shell: Arc<dyn ShellExecutor>,
        files: Arc<dyn FileHost>,
        workspace: PathBuf,
        brain: PathBuf,
        conversation: String,
        web: Option<Arc<dyn WebSearch>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                shell,
                files,
                workspace,
                brain,
                conversation,
                web,
                tasks: Atom::default(),
                messages: Atom::default(),
                discard_steps: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    pub fn brain(&self) -> &Path {
        &self.inner.brain
    }

    pub fn conversation(&self) -> &str {
        &self.inner.conversation
    }

    pub fn step_file(&self, step: u64) -> PathBuf {
        self.inner
            .brain
            .join(".system_generated")
            .join("steps")
            .join(step.to_string())
            .join("output.txt")
    }

    fn task_log(&self, step: u64) -> PathBuf {
        self.inner
            .brain
            .join(".system_generated")
            .join("tasks")
            .join(format!("task-{step}.log"))
    }

    pub fn keep_step_files(&self, keep: bool) {
        self.inner
            .discard_steps
            .store(!keep, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn finish_output(&self, tool: &str, step: u64, full: &str) -> String {
        if tool == "read_file" {
            if !self
                .inner
                .discard_steps
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                let _ = self.inner.files.write(&self.step_file(step), full).await;
            }
            return full.to_owned();
        }
        if self
            .inner
            .discard_steps
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return crate::format::truncate(full, None);
        }
        let file = self.step_file(step);
        match self.inner.files.write(&file, full).await {
            Ok(()) => crate::format::truncate(full, Some(&file.to_string_lossy())),
            Err(_) => crate::format::truncate(full, None),
        }
    }

    pub async fn call(&self, name: &str, args: &Value, step: u64) -> Outcome {
        if let Some(declaration) = crate::declaration(name)
            && let Err(error) = crate::validate::validate(declaration, args)
        {
            return Outcome::Error(format!("Encountered error in tool validation: {error}"));
        }
        let result = match name {
            "read_file" => files::read_file(
                self.inner.files.as_ref(),
                text(args, "Path").unwrap_or_default(),
                integer(args, "StartLine"),
                integer(args, "EndLine"),
                integer(args, "ByteOffset"),
            )
            .await
            .map(Outcome::Output),
            "write_file" => files::write_file(
                self.inner.files.as_ref(),
                text(args, "Path").unwrap_or_default(),
                text(args, "Content").unwrap_or_default(),
                flag(args, "Overwrite"),
                flag(args, "Append"),
            )
            .await
            .map(Outcome::Output),
            "edit_file" => files::edit_file(
                self.inner.files.as_ref(),
                text(args, "Path").unwrap_or_default(),
                text(args, "Target").unwrap_or_default(),
                text(args, "Replacement").unwrap_or_default(),
            )
            .await
            .map(Outcome::Output),
            "run_command" => self.run_command(args, step).await,
            "manage_task" => self.manage_task(args),
            "schedule" => self.schedule(args, step).await,
            "search_web" => self.search_web(args).await,
            "ask_question" => Ok(Outcome::Output(ask_question(args))),
            _ => Err(format!("unknown tool {name:?}")),
        };
        result.unwrap_or_else(|error| {
            Outcome::Error(format!("Encountered error in tool execution: {error}"))
        })
    }

    async fn search_web(&self, args: &Value) -> Result<Outcome, String> {
        let web = self
            .inner
            .web
            .as_ref()
            .ok_or("no summary returned from GenerateContent")?;
        let query = text(args, "query").unwrap_or_default();
        web.search(
            query,
            text(args, "domain").filter(|domain| !domain.is_empty()),
        )
        .await
        .map(Outcome::Output)
    }

    async fn run_command(&self, args: &Value, step: u64) -> Result<Outcome, String> {
        let command = text(args, "CommandLine").unwrap_or_default().to_owned();
        let summary = text(args, "toolSummary").unwrap_or_default().to_owned();
        let wait = u64::try_from(
            integer(args, "WaitMsBeforeAsync")
                .unwrap_or(500)
                .clamp(500, 10_000),
        )
        .unwrap_or(500);
        let daemon = flag(args, "IsDaemon");
        let workspace = self.inner.workspace.clone();
        let started = nanocode_platform::now();
        let request = ExecCommand::new(
            command.clone(),
            Some(workspace.to_string_lossy().into_owned()),
            None,
            Some(false),
            true,
            Some(wait),
            Some(UNLIMITED),
        )
        .with_output_unit(OutputUnit::Chars);
        let mut result = self.inner.shell.execute(request, &workspace).await;
        let mut output = result.output.clone();
        loop {
            let elapsed =
                u64::try_from(nanocode_platform::elapsed(started).as_millis()).unwrap_or(u64::MAX);
            let (None, Some(session)) = (result.exit_code, result.session_id) else {
                break;
            };
            if elapsed >= wait {
                break;
            }
            result = self
                .inner
                .shell
                .write_stdin(
                    WriteStdin::new(
                        i32::try_from(session).unwrap_or_default(),
                        String::new(),
                        Some(wait - elapsed),
                        Some(UNLIMITED),
                    )
                    .with_output_unit(OutputUnit::Chars),
                )
                .await;
            output.push_str(&result.output);
        }
        match (result.exit_code, result.session_id) {
            (Some(code), _) => {
                if let Some(error) = result.error.filter(|_| output.is_empty()) {
                    return Err(error);
                }
                Ok(Outcome::Output(command_body(code, &output)))
            }
            (None, None) => Err(result
                .error
                .unwrap_or_else(|| "command failed to start".to_owned())),
            (None, Some(session)) => {
                let session = i32::try_from(session).unwrap_or_default();
                let id = format!("{}/task-{step}", self.inner.conversation);
                let log = self.task_log(step);
                let _ = self.inner.files.write(&log, &output).await;
                let at = now();
                self.add_task(Task {
                    id: id.clone(),
                    tool: "run_command",
                    summary,
                    description: command,
                    started: at,
                    progress: at,
                    log: log.clone(),
                    daemon,
                    kind: Kind::Process,
                    state: State::Running,
                    output: output.clone(),
                    input: Vec::new(),
                });
                let engine = self.clone();
                let task = id.clone();
                nanocode_platform::spawn(async move { engine.follow(task, session).await });
                let mut text = format!(
                    "Tool is running as a background task with task id: {id}\nTask logs are available at: file://{}",
                    log.display()
                );
                if !output.is_empty() {
                    text.push_str(&format!(
                        "\nLog output:\n{}",
                        crate::format::truncate(&output, None)
                    ));
                }
                Ok(Outcome::Background(text))
            }
        }
    }

    async fn follow(&self, id: String, session: i32) {
        loop {
            let pending = self
                .with_task(&id, |task| std::mem::take(&mut task.input).concat())
                .unwrap_or_default();
            let result = self
                .inner
                .shell
                .write_stdin(
                    WriteStdin::new(session, pending, Some(250), Some(UNLIMITED))
                        .with_output_unit(OutputUnit::Chars),
                )
                .await;
            let chunk = result.output.clone();
            let log = self.with_task(&id, |task| {
                if !chunk.is_empty() {
                    task.output.push_str(&chunk);
                    task.progress = now();
                }
                (task.log.clone(), task.output.clone())
            });
            if let Some((log, output)) = &log
                && !chunk.is_empty()
            {
                let _ = self.inner.files.write(log, output).await;
            }
            let finished = result.exit_code.is_some() || result.session_id.is_none();
            if !finished {
                continue;
            }
            let code = result.exit_code.unwrap_or(-1);
            let done = self.with_task(&id, |task| match task.state {
                State::Running => {
                    task.state = State::Done;
                    Some((task.output.clone(), task.log.clone()))
                }
                _ => None,
            });
            if let Some(Some((output, log))) = done {
                self.push_message(Message {
                    at: now(),
                    sender: id.clone(),
                    high: true,
                    content: format!(
                        "Task id \"{id}\" finished with result:\n{}\nLog: file://{}",
                        command_body(code, &crate::format::truncate(&output, None)),
                        log.display()
                    ),
                    seen: false,
                });
            }
            return;
        }
    }

    async fn schedule(&self, args: &Value, step: u64) -> Result<Outcome, String> {
        let prompt = text(args, "Prompt").unwrap_or_default().to_owned();
        let duration = integer(args, "DurationSeconds");
        let cron = text(args, "CronExpression").filter(|cron| !cron.trim().is_empty());
        let id = format!("{}/task-{step}", self.inner.conversation);
        let log = self.task_log(step);
        let _ = self.inner.files.write(&log, "").await;
        let at = now();
        let task = |summary: String, description: String, daemon: bool, kind: Kind| Task {
            id: id.clone(),
            tool: "schedule",
            summary,
            description,
            started: at,
            progress: at,
            log: log.clone(),
            daemon,
            kind,
            state: State::Running,
            output: String::new(),
            input: Vec::new(),
        };
        match (duration, cron) {
            (Some(seconds), None) => {
                let seconds = u64::try_from(seconds.max(0)).unwrap_or_default();
                let label = duration_label(seconds);
                let condition = text(args, "TimerCondition")
                    .filter(|condition| !condition.is_empty() && *condition != "never")
                    .map(str::to_owned);
                self.add_task(task(
                    format!("{label} timer"),
                    format!("Timer: {label}, Prompt: {prompt}"),
                    false,
                    Kind::Timer { condition },
                ));
                let engine = self.clone();
                let sender = id.clone();
                nanocode_platform::spawn(async move {
                    nanocode_platform::sleep(Duration::from_secs(seconds)).await;
                    engine.fire(&sender, prompt, true);
                });
            }
            (None, Some(expression)) => {
                let cron = Cron::parse(expression)?;
                let limit =
                    integer(args, "MaxIterations").and_then(|limit| u64::try_from(limit).ok());
                self.add_task(task(
                    format!("cron ({})", cron.label()),
                    format!("Cron: {expression}, Prompt: {prompt}"),
                    flag(args, "IsDaemon"),
                    Kind::Cron,
                ));
                let engine = self.clone();
                let sender = id.clone();
                nanocode_platform::spawn(
                    async move { engine.tick(&sender, cron, prompt, limit).await },
                );
            }
            _ => {
                return Err(
                    "exactly one of DurationSeconds or CronExpression must be set".to_owned(),
                );
            }
        }
        Ok(Outcome::Background(format!(
            "Tool is running as a background task with task id: {id}\nTask logs are available at: file://{}",
            log.display()
        )))
    }

    async fn tick(&self, id: &str, cron: Cron, prompt: String, limit: Option<u64>) {
        let mut iteration = 0;
        loop {
            let Some(wait) = cron.next_after(now()) else {
                return;
            };
            nanocode_platform::sleep(wait).await;
            if self.with_task(id, |task| task.state) != Some(State::Running) {
                return;
            }
            iteration += 1;
            self.fire(id, format!("{prompt} (iteration {iteration})"), false);
            if limit.is_some_and(|limit| iteration >= limit) {
                self.fire(
                    id,
                    format!("Cron schedule completed after reaching its maximum of {iteration} iterations."),
                    true,
                );
                return;
            }
        }
    }

    fn fire(&self, id: &str, content: String, last: bool) {
        let running = self.with_task(id, |task| {
            let running = task.state == State::Running;
            if running && last {
                task.state = State::Done;
            }
            running
        });
        if running == Some(true) {
            self.push_message(Message {
                at: now(),
                sender: id.to_owned(),
                high: true,
                content,
                seen: false,
            });
        }
    }

    fn manage_task(&self, args: &Value) -> Result<Outcome, String> {
        let action = text(args, "Action").unwrap_or_default();
        if action == "list" {
            return Ok(Outcome::Output(self.list()));
        }
        if !matches!(action, "status" | "kill" | "send_input") {
            return Err(format!("unknown action {action:?}"));
        }
        let id = text(args, "TaskId")
            .ok_or_else(|| format!("TaskId is required for action {action:?}"))?;
        let prefix = format!("{}/task-", self.inner.conversation);
        let valid = id
            .strip_prefix(&prefix)
            .is_some_and(|step| !step.is_empty() && step.bytes().all(|byte| byte.is_ascii_digit()));
        if !valid {
            return Err(format!("invalid task ID format: {id:?}"));
        }
        let task = self
            .tasks()
            .iter()
            .find(|task| task.id == id)
            .cloned()
            .ok_or_else(|| format!("task not found: {id:?}"))?;
        match action {
            "status" => {
                let status = match task.state {
                    State::Running => "RUNNING",
                    State::Done => "DONE",
                    State::Canceled => "CANCELLED",
                };
                let mut out = format!(
                    "Task: {id}\nStatus: {status}\nLog: file://{}\n",
                    task.log.display()
                );
                if !task.output.is_empty() {
                    let quiet = now()
                        .duration_since(task.progress)
                        .unwrap_or_default()
                        .as_secs();
                    out.push_str(&format!(
                        "Log output:\n{}\n\nLast progress: {quiet}s ago\n",
                        task.output
                    ));
                }
                Ok(Outcome::Output(out))
            }
            "send_input" => {
                let input =
                    text(args, "Input").ok_or("Input is required for action \"send_input\"")?;
                if task.state != State::Running || !matches!(task.kind, Kind::Process) {
                    return Err(format!("task {id:?} is not running"));
                }
                self.with_task(id, |task| task.input.push(input.to_owned()));
                Ok(Outcome::Output(format!("Input sent to task \"{id}\".")))
            }
            _ => {
                let canceled = self.with_task(id, |task| {
                    let running = task.state == State::Running;
                    if running {
                        task.state = State::Canceled;
                        if matches!(task.kind, Kind::Process) {
                            task.input.push("\u{3}".to_owned());
                        }
                    }
                    running
                });
                if canceled == Some(true) {
                    self.push_message(Message {
                        at: now(),
                        sender: id.to_owned(),
                        high: false,
                        content: format!(
                            "Task id \"{id}\" was canceled with result:\nTool execution was canceled\nLog: file://{}",
                            task.log.display()
                        ),
                        seen: false,
                    });
                }
                Ok(Outcome::Output(format!("Task \"{id}\" cancelled.")))
            }
        }
    }

    fn list(&self) -> String {
        let tasks = self.tasks();
        let running = tasks
            .iter()
            .filter(|task| task.state == State::Running)
            .map(|task| {
                let mut entry = serde_json::Map::new();
                entry.insert("taskId".into(), json!(task.id));
                entry.insert("toolName".into(), json!(task.tool));
                entry.insert("toolSummary".into(), json!(task.summary));
                entry.insert("description".into(), json!(task.description));
                entry.insert("startTime".into(), json!(timestamp(task.started)));
                entry.insert(
                    "logUri".into(),
                    json!(format!("file://{}", task.log.display())),
                );
                if task.daemon {
                    entry.insert("isDaemon".into(), json!(true));
                }
                ordered(&entry)
            })
            .collect::<Vec<_>>();
        match running.is_empty() {
            true => "No background tasks are currently running.".to_owned(),
            false => format!(
                "You have {} background task(s) currently running:\n[{}]",
                running.len(),
                running.join(",")
            ),
        }
    }

    pub fn snapshots(&self) -> Vec<String> {
        self.tasks()
            .iter()
            .filter(|task| task.state == State::Running)
            .map(|task| {
                format!(
                    "- {} ({}): {} [started {}, log file://{}]",
                    task.id,
                    task.tool,
                    task.description,
                    timestamp(task.started),
                    task.log.display()
                )
            })
            .collect()
    }

    fn tasks(&self) -> Arc<Vec<Arc<Task>>> {
        self.inner.tasks.get()
    }

    fn add_task(&self, task: Task) {
        let task = Arc::new(task);
        self.inner
            .tasks
            .update(|tasks| tasks.push(Arc::clone(&task)));
    }

    fn with_task<T>(&self, id: &str, mut update: impl FnMut(&mut Task) -> T) -> Option<T> {
        self.inner.tasks.try_update(|tasks| {
            let task = tasks.iter_mut().find(|task| task.id == id)?;
            Some(update(Arc::make_mut(task)))
        })
    }

    pub fn push_message(&self, message: Message) {
        self.inner.tasks.update(|tasks| {
            for task in tasks.iter_mut() {
                let Kind::Timer {
                    condition: Some(condition),
                } = &task.kind
                else {
                    continue;
                };
                let cancels = task.state == State::Running
                    && task.id != message.sender
                    && (condition == "any" || *condition == message.sender);
                if cancels {
                    Arc::make_mut(task).state = State::Canceled;
                }
            }
        });
        self.inner
            .messages
            .update(|messages| messages.push(message.clone()));
    }

    pub fn take_messages(&self) -> Vec<Message> {
        self.inner.messages.update(|queue| {
            let (ready, mut waiting): (Vec<_>, Vec<_>) = std::mem::take(queue)
                .into_iter()
                .partition(|message| message.high || message.seen);
            for message in &mut waiting {
                message.seen = true;
            }
            *queue = waiting;
            ready
        })
    }

    pub fn has_urgent(&self) -> bool {
        self.inner
            .messages
            .read(|messages| messages.iter().any(|message| message.high))
    }

    pub fn blocking(&self) -> bool {
        self.tasks()
            .iter()
            .any(|task| task.state == State::Running && !task.daemon)
    }

    pub async fn wait(&self) {
        while !self.has_urgent() && self.blocking() {
            nanocode_platform::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn shutdown(&self) {
        self.inner.tasks.update(|tasks| {
            for task in tasks.iter_mut() {
                if task.state == State::Running {
                    Arc::make_mut(task).state = State::Canceled;
                }
            }
        });
        self.inner.shell.terminate_all().await;
    }
}

fn ordered(entry: &serde_json::Map<String, Value>) -> String {
    let order = [
        "taskId",
        "toolName",
        "toolSummary",
        "description",
        "startTime",
        "logUri",
        "isDaemon",
    ];
    let fields = order
        .iter()
        .filter_map(|key| {
            entry
                .get(*key)
                .map(|value| format!("{}:{}", go_json(&json!(key)), go_json(value)))
        })
        .collect::<Vec<_>>();
    format!("{{{}}}", fields.join(","))
}

fn ask_question(args: &Value) -> String {
    let count = args
        .get("questions")
        .and_then(Value::as_array)
        .map_or(1, |questions| questions.len().max(1));
    (1..=count)
        .map(|index| format!("A{index}: "))
        .collect::<Vec<_>>()
        .join("\n")
}
