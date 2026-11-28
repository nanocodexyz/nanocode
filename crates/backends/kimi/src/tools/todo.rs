use super::{Builtins, Outcome, input};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Todo {
    pub title: String,
    pub status: String,
}

#[derive(Deserialize)]
struct Input {
    todos: Option<Vec<Todo>>,
}

pub fn call(builtins: &Builtins, args: &Value) -> Outcome {
    let Input { todos } = match input("TodoList", args) {
        Ok(input) => input,
        Err(error) => return error,
    };
    let Ok(mut stored) = builtins.todos.lock() else {
        return Outcome::error("todo list is unavailable");
    };
    let Some(todos) = todos else {
        return Outcome::ok(render(&stored));
    };
    *stored = todos;
    if stored.is_empty() {
        return Outcome::ok("Todo list cleared.");
    }
    let updated = format!("Todo list updated.\n{}", render(&stored));
    Outcome::ok(match hint(&stored) {
        Some(hint) => format!("{updated}\n\n{hint}"),
        None => updated,
    })
}

fn render(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "Todo list is empty.".to_owned();
    }
    std::iter::once("Current todo list:".to_owned())
        .chain(
            todos
                .iter()
                .map(|todo| format!("  [{}] {}", todo.status, todo.title)),
        )
        .collect::<Vec<_>>()
        .join("\n")
}

fn hint(todos: &[Todo]) -> Option<String> {
    let open = todos.iter().filter(|todo| todo.status != "done").count();
    if open == 0 {
        return None;
    }
    let active = todos
        .iter()
        .filter(|todo| todo.status == "in_progress")
        .collect::<Vec<_>>();
    Some(match active.as_slice() {
        [] => "No item is in_progress. Mark the one you are working on as in_progress.".to_owned(),
        [_, _, ..] => {
            "Several items are in_progress. Keep only the one you are working on now in_progress."
                .to_owned()
        }
        [current] if open == 1 => format!(
            "\"{}\" is the last open item. Once it is finished, mark it done before you write your final message; if it stays blocked or unfinished, keep it in_progress and say what remains in your reply.",
            current.title
        ),
        [current] => format!(
            "When \"{}\" is finished, mark it done and set the next item in_progress; that update can go in the same response as your next tool call.",
            current.title
        ),
    })
}

pub const REMINDER_PREFIX: &str = "The todo list has not been updated in the last ";
const STEPS_SINCE_WRITE: usize = 10;
const STEPS_BETWEEN_REMINDERS: usize = 10;

/// Kimi Code's stale todo-list reminder text, given the todos and how many assistant steps passed since
/// the last TodoList write and since the last such reminder; `None` when no reminder is due.
pub fn reminder(
    todos: &[(String, String)],
    since_write: usize,
    since_reminder: usize,
) -> Option<String> {
    let done = todos.iter().filter(|(_, status)| status == "done").count();
    if done == todos.len()
        || since_write < STEPS_SINCE_WRITE
        || since_reminder < STEPS_BETWEEN_REMINDERS
    {
        return None;
    }
    let progress = format!("{done}/{} done", todos.len());
    let status = match todos.iter().find(|(_, status)| status == "in_progress") {
        Some((title, _)) => format!("{progress}, \"{title}\" still in_progress"),
        None => format!("{progress}, none in_progress"),
    };
    let items = todos
        .iter()
        .enumerate()
        .map(|(index, (title, status))| format!("{}. [{status}] {title}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "{REMINDER_PREFIX}{since_write} steps ({status}). If an item is finished, mark it done; the TodoList call can go in the same response as your next tool call. If the list no longer matches the current work, rewrite or clear it. If the list still matches your progress, no update is needed; this is just a gentle reminder. Never mention this reminder to the user.\n\nCurrent todo list:\n{items}"
    ))
}
