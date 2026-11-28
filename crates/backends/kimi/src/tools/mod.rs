//! Kimi Code's built-in tools (`@moonshot-ai/kimi-code` 2.7.3): the exact contracts the CLI declares, and
//! handlers that reproduce its outputs. They run beside nanocode's tools, always as direct tools, the way
//! Kimi Code exposes them.

mod files;
mod io;
mod media;
mod search;
mod shell;
pub mod spill;
mod todo;
mod web;

use serde_json::Value;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

pub const CONTRACTS: &str = include_str!("../../prompts/tools.json");

pub use io::Host;
pub use todo::{REMINDER_PREFIX as TODO_REMINDER_PREFIX, reminder as todo_reminder};

/// What a tool hands back to the model: its output, whether it failed, the `note` Kimi Code sends as
/// a second text part of the tool message, and the media parts that follow the output's first text part
/// (`Message::Tool::media`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub output: String,
    pub failed: bool,
    pub note: Option<String>,
    pub media: Vec<Value>,
}

impl Outcome {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            ..Self::default()
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            failed: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

const NAMES: [&str; 13] = [
    "Bash",
    "Edit",
    "FetchURL",
    "Glob",
    "Grep",
    "Read",
    "ReadMediaFile",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TodoList",
    "WaitFor",
    "Write",
];

pub const APPLIED: &str =
    "<system>The change is applied; there is no need to Read the file back to check it.</system>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Search,
}

pub struct Builtins {
    cwd: PathBuf,
    home: Option<PathBuf>,
    /// The session's media store (Kimi Code's `<session>/media`), where ReadMediaFile saves what it cannot
    /// send inline.
    media: Option<PathBuf>,
    /// Whether FetchURL is on: it follows the selection's web-search switch, the way Codex's web tool does.
    web: bool,
    /// The host the tools reach files and processes through; the local machine when `None`.
    host: Option<Host>,
    todos: Mutex<Vec<todo::Todo>>,
    shell: shell::Shell,
}

impl Builtins {
    pub fn new(
        cwd: PathBuf,
        home: Option<PathBuf>,
        media: Option<PathBuf>,
        web: bool,
        host: Option<Host>,
    ) -> Self {
        Self {
            shell: shell::Shell::new(&cwd, host.clone()),
            host,
            cwd,
            home,
            media,
            web,
            todos: Mutex::default(),
        }
    }

    /// The declarations Kimi Code sends for these tools, verbatim.
    pub fn declarations() -> Vec<Value> {
        serde_json::from_str::<Vec<Value>>(CONTRACTS)
            .expect("verified upstream Kimi tool contracts")
    }

    /// The built-in tools this agent has: every one Kimi Code runs locally, less FetchURL when web access
    /// is off.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        NAMES
            .into_iter()
            .filter(|name| self.web || *name != "FetchURL")
    }

    pub const fn host(&self) -> Option<&Host> {
        self.host.as_ref()
    }

    pub fn handles(&self, name: &str) -> bool {
        self.names().any(|known| known == name)
    }

    /// Runs a built-in tool on arguments that already passed schema validation; `id` is the tool call's.
    pub async fn call(&self, id: &str, name: &str, args: &Value) -> Outcome {
        match name {
            "Read" => files::read(self, args).await,
            "Write" => files::write(self, args).await,
            "Edit" => files::edit(self, args).await,
            "Glob" => search::glob(self, args).await,
            "Grep" => search::grep(self, args).await,
            "Bash" => self.shell.bash(self, id, args).await,
            "TaskList" => self.shell.list(args),
            "TaskOutput" => self.shell.output(args).await,
            "TaskStop" => self.shell.stop(args).await,
            "WaitFor" => self.shell.wait(args).await,
            "TodoList" => todo::call(self, args),
            "FetchURL" => web::fetch(self, args).await,
            "ReadMediaFile" => media::read(self, args).await,
            other => Outcome::error(format!("Tool \"{other}\" not found")),
        }
    }

    /// Completion notices for background tasks that finished since the last call, rendered as the user
    /// messages Kimi Code injects before its next model request.
    pub fn notifications(&self) -> Vec<String> {
        self.shell.notifications()
    }

    /// A new turn began (a prompt); Kimi Code counts repeated WaitFor calls per turn.
    pub fn turn(&self) {
        self.shell.turn();
    }

    /// Stops every background task; called when the agent shuts down.
    pub async fn shutdown(&self) {
        self.shell.shutdown().await;
    }

    pub fn todos(&self) -> Value {
        let todos = self
            .todos
            .lock()
            .map(|todos| todos.clone())
            .unwrap_or_default();
        serde_json::to_value(todos).unwrap_or(Value::Null)
    }

    pub fn restore_todos(&self, value: &Value) {
        if let (Ok(todos), Ok(mut current)) = (
            serde_json::from_value::<Vec<todo::Todo>>(value.clone()),
            self.todos.lock(),
        ) {
            *current = todos;
        }
    }

    pub fn open_todos(&self) -> Vec<(String, String)> {
        self.todos
            .lock()
            .map(|todos| {
                todos
                    .iter()
                    .map(|todo| (todo.title.clone(), todo.status.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Kimi Code's `resolvePathAccessPath`: expands `~`, resolves against the working directory,
    /// normalizes lexically, refuses well-known secret files, and requires an absolute path to reach
    /// outside the working directory. Errors are the resolve-phase failure Kimi Code reports for `tool`.
    /// Glob and Grep (`Access::Search`) skip the secret check, as Kimi Code's `checkSensitive: false`.
    pub fn resolve(&self, tool: &str, path: &str, access: Access) -> Result<PathBuf, Outcome> {
        let fail = |message: String| {
            Outcome::error(format!(
                "Tool \"{tool}\" failed to resolve execution: {message}"
            ))
        };
        if path.is_empty() {
            return Err(fail("Path cannot be empty".to_owned()));
        }
        let expanded = match (&self.home, path) {
            (Some(home), "~") => home.clone(),
            (Some(home), _) if path.starts_with("~/") => home.join(&path[2..]),
            _ => PathBuf::from(path),
        };
        let absolute = expanded.is_absolute();
        let canonical = normalize(&match absolute {
            true => expanded,
            false => self.cwd.join(expanded),
        });
        if access != Access::Search && sensitive(&canonical) {
            return Err(fail(format!(
                "\"{path}\" matches a sensitive-file pattern (env / credential / SSH key). Access is blocked to protect secrets."
            )));
        }
        if !canonical.starts_with(&self.cwd) && !absolute {
            let purpose = match access {
                Access::Write => "write or edit a file",
                Access::Search => "search",
                Access::Read => "read a file",
            };
            return Err(fail(format!(
                "\"{path}\" is not an absolute path. You must provide an absolute path to {purpose} outside the working directory."
            )));
        }
        Ok(canonical)
    }
}

/// Node's `path.normalize` on an absolute path: drops `.`, folds `..`, keeps symlinks unresolved.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Kimi Code's `isSensitiveFile`.
pub fn sensitive(path: &Path) -> bool {
    const BASENAMES: [&str; 5] = [".env", "id_rsa", "id_ed25519", "id_ecdsa", "credentials"];
    const PREFIXES: [&str; 4] = ["id_rsa", "id_ed25519", "id_ecdsa", "credentials"];
    const EXEMPT: [&str; 3] = [".env.example", ".env.sample", ".env.template"];
    const PUBLIC: [&str; 3] = ["id_rsa.pub", "id_ed25519.pub", "id_ecdsa.pub"];
    const VARIANTS: [&str; 10] = [
        ".bak",
        ".backup",
        ".copy",
        ".disabled",
        ".key",
        ".old",
        ".orig",
        ".pem",
        ".save",
        ".tmp",
    ];
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let whole = path.to_string_lossy().to_lowercase();
    if EXEMPT.contains(&name.as_str()) || PUBLIC.contains(&name.as_str()) {
        return false;
    }
    if BASENAMES.contains(&name.as_str()) || name.starts_with(".env.") {
        return true;
    }
    for prefix in PREFIXES {
        if name == prefix {
            return true;
        }
        if let Some(suffix) = name.strip_prefix(prefix) {
            match suffix.chars().next() {
                Some('-' | '_') => return true,
                Some('.') if VARIANTS.contains(&suffix) => return true,
                _ => {}
            }
        }
    }
    [".aws/credentials", ".gcp/credentials"]
        .iter()
        .any(|suffix| {
            whole.ends_with(&format!("/{suffix}")) || whole.contains(&format!("/{suffix}/"))
        })
}

/// Deserializes validated arguments into a tool's input struct.
pub fn input<T: serde::de::DeserializeOwned>(tool: &str, args: &Value) -> Result<T, Outcome> {
    serde_json::from_value(args.clone())
        .map_err(|error| Outcome::error(format!("Invalid args for tool \"{tool}\": {error}")))
}
