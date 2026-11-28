mod engine;
mod files;
mod format;
mod schedule;
pub mod sim;
mod validate;

pub use engine::{Engine, Message, Outcome, WebSearch, WebSearchFuture};
pub use files::LocalFiles;
pub use format::{
    REMINDER, TRUNCATE_LIMIT, local_timestamp, system_message, timestamp, truncate_output,
    with_header,
};
pub use validate::validate;

pub const DECLARATIONS: &str = include_str!("tools.jsonl");
pub const READ_FILE_CLAUDE: &str = include_str!("read_file.claude.json");

pub fn declaration(name: &str) -> Option<&'static str> {
    DECLARATIONS.lines().find(|line| {
        line.strip_prefix("{\"name\":\"")
            .and_then(|rest| rest.split_once('"'))
            .is_some_and(|(found, _)| found == name)
    })
}

pub const NAMES: [&str; 8] = [
    "read_file",
    "run_command",
    "manage_task",
    "schedule",
    "write_file",
    "edit_file",
    "search_web",
    "ask_question",
];
