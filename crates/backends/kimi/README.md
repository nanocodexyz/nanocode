# nanocode-backend-kimi

A nanocode harness that reproduces the engine of Kimi Code CLI (`@moonshot-ai/kimi-code` 2.7.3) for Kimi models: its
system prompt, its reminders, its repeated-call guard, its compaction, its Chat Completions request shape and its
built-in tools: Read, Write, Edit, Glob, Grep, Bash, ReadMediaFile, FetchURL, TaskList, TaskOutput, TaskStop, WaitFor
and TodoList. They follow the tool selection the way Codex's built-ins do: no selection means `Tools::default()`; the
workspace switch (`ToolsBuilder::workspace`, off with `without_defaults`) brings them, the web-search switch brings
FetchURL, and their names are reserved against registered tools. An explicit workspace source or a Code Mode host brings
its own workspace tools instead. They run on the local machine, or over a host's files and shell
(`KimiHarness::host`, a `FileHost` and a `ForegroundExecutor`), which is how a wasm agent gets them, as Codex's
and Claude's `host_tools` do; wasm without a host has none. Over a host, Glob and Grep run `rg` through its shell, task
logs, spilled results and media are written through its files under `/tmp`, and outputs match the local ones. The rest of the selection reaches the model beside them, in
Code Mode or directly (`KimiHarness::direct_tools`). Subagents, Code Mode and MCP stay nanocode's own.

## Selecting it

```rust
use nanocode::kimi::{KimiBuilder, KimiHarness};

let runtime = nanocode::Nanocode::builder()
    .kimi(KimiHarness::new())
    .harness_model(KimiHarness::selection("k3"))
    .build_async()
    .await?;
```

`KimiHarness::new()` posts to Kimi Code's API (`https://api.kimi.com/coding/v1`) with `KIMI_API_KEY`;
`KimiHarness::endpoint` sets another base URL, key and user agent, and `KimiHarness::sender` routes requests through a
host (on wasm, it is required). The model's context window comes from the endpoint's `/models` catalog
(`context_length`) unless `KimiHarness::context_window` gives it. Thinking maps to K3's efforts: `low`, `high` (the
default) or `max`; K3 always reasons, so other levels are refused.

## What it reproduces

| Kimi Code | Here |
| --- | --- |
| System prompt (`system.md`, `coding.md`, `delivering-work.md`, `project-information.md`) | Pinned in `prompts/` with their source revision and hashes in `prompts/upstream.json`; host instructions fill `${role_additional}` |
| Environment: OS, shell, working directory, its two-level listing, `AGENTS.md` (`~/.kimi-code`, `~/.agents`, then the project from its git root to the working directory) | The same, or what the host passes in `Environment` |
| Request: `thinking: {type: enabled, effort, keep: all}`, `max_completion_tokens` = window − estimated request, `prompt_cache_key` = session, `stream_options.include_usage` | The same |
| Tool schemas: local `$ref`s inlined, a `type` inferred for every child schema from its `enum`, `const` or structure | The same (`normalize_tool_schema`) |
| History: assistant `reasoning_content` always replayed, no `content` beside tool calls when it is blank, tool call IDs sanitized and made unique | The same |
| Built-in tools: the declarations Kimi Code sends (`prompts/tools.json`, verbatim), their outputs, notes, errors and path guards, Bash's background tasks and their completion notifications, TodoList's guidance and stale-list reminder | The same, checked call by call against the release CLI; Glob and Grep run `rg` from `PATH`. FetchURL fetches locally (the API-key path; Readability text differs from Kimi's linkedom DOM on a few pages) and is native only. ReadMediaFile matches PNG bytes; JPEG re-encodes differ slightly. Images go up to `/files` before each request and are sent as `ms://` ids, as Kimi Code sends them: identical images upload once per agent (Kimi Code also caches them on disk across sessions), a 401 or 403 fails the turn, and any other failure sends the image inline and retries the upload on the next request, except an endpoint without a `/files` route (404, 405 or 501, as a proxy such as Cloudflare's AI Gateway answers), which keeps images inline for the rest of the agent where Kimi Code would retry on every request. Calls in a step start together and are recorded in call order, with Bash, Glob, Grep, Write and Edit taking turns in call order, as Kimi Code's process lease makes them; a Write or Edit is applied before later calls of its step run, where Kimi Code's file-history capture often lets them run first. Not ported: WebSearch, Agent, AskUserQuestion, plan mode, goals, cron, skills |
| Tool calls: arguments parsed, unknown tools refused, then validated as AJV does (`allErrors`, every error joined with `; `) for every tool, nanocode's included | The same; a malformed-JSON message uses serde's wording except at end of input |
| Tool results: `<system>` markers for failed and empty outputs, a note as a second text part, results over 50 000 characters saved to a file with a head and tail preview (on the host when the built-in tools run there; a preview alone when they cannot be saved, as on wasm without a host), a `Wall time` header for Bash, Glob, Grep and MCP tools in compaction requests | The same; results are saved under the system temp directory instead of Kimi Code's session directory |
| Reminders: the date at the start and when it changes, auto permission mode, both again after a compaction | The same |
| Repeated-call guard: reminders on the 3rd, 5th and 8th identical call in a row, a separate one at the 12th, the turn stopped after the 13th | The same |
| Compaction at 85% of the window (or within 50k tokens of it): the whole history plus the pinned instruction, then the user's messages (20k tokens, head and tail), the summary and a continuation reminder | The same, without Kimi Code's pointer to its on-disk event log |
| Retries: 10 attempts, 0.5 s doubling to 32 s with 25% jitter, `Retry-After`, on timeouts, dropped connections and streams, 408/409/429/5xx/529 except quota exhaustion | The same |
