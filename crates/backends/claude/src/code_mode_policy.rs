use std::sync::{Arc, LazyLock};

use nanocode_tool_host::{CodeModeToolPolicy, WorkspaceToolRouting};

pub const HAND_TOOLS: [&str; 6] = ["Bash", "Edit", "Read", "TaskOutput", "TaskStop", "Write"];

static POLICY: LazyLock<Arc<dyn CodeModeToolPolicy>> = LazyLock::new(|| Arc::new(ClaudeToolPolicy));

pub(crate) fn claude_tool_policy() -> Arc<dyn CodeModeToolPolicy> {
    Arc::clone(&POLICY)
}

struct ClaudeToolPolicy;

impl CodeModeToolPolicy for ClaudeToolPolicy {
    fn workspace_routing(&self, name: &str) -> Option<WorkspaceToolRouting> {
        if name == "Bash" {
            return Some(WorkspaceToolRouting::Shell);
        }
        HAND_TOOLS
            .contains(&name)
            .then_some(WorkspaceToolRouting::File)
    }

    fn exclusive(&self, name: &str) -> bool {
        ["Write", "Edit"].contains(&name)
    }
}
