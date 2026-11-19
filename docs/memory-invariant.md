# Memory invariant: one owner per byte

Status: adopted as a report-only check (`nanocode-sim --memory`), 2026-11-19. Measured at d3b5744f8.

## Why

A busy multi-tenant server (one process, ~190 concurrent turns, 32 GiB limit) grows its heap by ~150 MiB per running turn. Its heap profile at a 24 GiB peak shows the same committed message bytes held three to five times per session: a parsed `Message` tree, a cached JSON string, a durable record, a payload memo copy and a wire cache. Each copy is reasonable alone; together they make retained heap a multiple of history.

## The invariant

1. **One owner per byte.** A live session holds each committed message's canonical serialized bytes exactly once. Every other view (request body, durable record, log entry, cache, parsed value tree) borrows those bytes (a shared `Arc<str>` or a slice of it) or is derived on demand and dropped. Retained heap per session is at most `K x history bytes + C`.
2. **Bounded transients.** Building a request, loading or replaying a session, or committing a record allocates at most `T_BODY x request body + T_MSG x largest message + C_T` beyond what it retains: never O(history) more per step, except the one pass that writes the request body.
3. **Prefix-stable wire bytes.** Rebuilding the request from the single owner yields byte-identical message prefixes across rounds and across a durable reload, so prompt caching is unaffected. Deliberate context edits (Claude's tool-result clearing) are the only allowed breaks.

| Constant | Value | Code |
| --- | --- | --- |
| `K` | 1.2 | `RETAINED_PER_HISTORY_BYTE` |
| `C` | 2 MiB | `RETAINED_CONSTANT_BYTES` |
| `T_BODY` | 1 | `TRANSIENT_PER_REQUEST_BODY` |
| `T_MSG` | 4 | `TRANSIENT_PER_LARGEST_MESSAGE` |
| `C_T` | 4 MiB | `TRANSIENT_CONSTANT_BYTES` |

All five live in `bin/nanocode-sim/src/memory.rs`. `K = 1.2` leaves 0.2 for per-message structure (offsets, ids, small parsed headers); a session that keeps a parsed tree of every message beside its JSON cannot meet it.

**History bytes** are measured on the wire: for each position in the request's message array (`messages` for Claude, `input` for Codex), the largest serialized size it has had. Positions Claude later clears keep their original size.

## Who owns the bytes

The owner is the committed message's canonical serialized JSON, one `Arc<str>` per message, created once: by the backend when the message is committed, or by the load path when a stored record is read.

| Layer | Rule |
| --- | --- |
| Backend history (Claude `HistoryMessage`, Codex `ResponseItem` list) | Holds the owner `Arc<str>`. A parsed form is either not kept or kept only for small fields; large strings (tool results, text) are not held twice. |
| Wire / request builder | Writes the body by copying borrowed message bytes into one buffer per request; keeps no per-message serialized copy (a "wire" `Box<str>` is a second owner). The kept request body for cache reuse counts as a borrower only if it is the only copy of its bytes beyond the owner. |
| Durable records, `SequenceLog`, staged writes | Build records from the owner `Arc` (`EncodedPayload::from_json(Arc::clone(..))`); after a durable commit, a record may become a reference and drop its bytes. |
| Payload memo (standby / handoff load) | Shares the loaded bytes with history (`EncodedPayload::shared_json()`), never a second `Arc<str>` of the same record. |
| Record cache | Budgeted, and holds record text a live session does not already own. |
| Load / reload / replay | One streaming pass: each record's bytes become the owner directly; no full parse tree of history is kept beside it. |

## The check

```sh
SHIPPY_REMOTE_BUILD=0 cargo build --release --locked --offline -p nanocode-sim
target/release/nanocode-sim --memory                     # report only, exit 0
target/release/nanocode-sim --memory --memory-gate       # exit 1 on any breach
target/release/nanocode-sim --memory --only claude/reload --turns 150 --rounds 24 --verbose
bin/nanocode-sim/memory-mutations.sh                     # the mutation the gate catches
```

Each combination of a backend and a durability mode runs one session against an in-process mock model: `--turns` turns (default 60) of `--rounds` tool rounds (default 16), each tool result a seeded log-uniform size between `--min-bytes` (200) and `--max-bytes` (64 KiB), about 10 MiB of history by default. The durable store keeps its records on disk, so they do not count as session heap. A counting global allocator (`nanocode_sys::alloc::Counted`) gives live and peak bytes; after each turn the check records retained (live minus the baseline before the first turn) and transient (peak during the turn minus the larger of start and end). The mock keeps only one hash per message, and checks every request's message prefix against the previous request's.

Backends (`BACKENDS`): `claude`, `codex`. Modes (`MODES`):

| Mode | What it drives |
| --- | --- |
| `paged` | One live durable session for every turn |
| `memo` | Store with `with_payload_memo()`; each turn reopens with `store.load` + `open_loaded_owned` (the standby / handoff path) |
| `reload` | Each turn reopens the session with `open_session` (durable reload mid-session) |
| `replay` | After each turn, reopen and resubmit the same request id: the completed step replays without a model call |

There is no unpaged mode: every runtime has a durable store (the default is a home JSONL store), and Claude durable policies always page messages.

## Baseline (d3b5744f8, default sizes, 2026-11-19)

| combo | history | retained | ratio max / final | transient (x body) | prefix | verdict |
| --- | --- | --- | --- | --- | --- | --- |
| claude/paged | 10.3 MiB | 27.2 MiB | 2.75 / 2.64 | 1.6 MiB (2.57) | stable | RETAINED |
| claude/memo | 10.3 MiB | 38.0 MiB | 3.72 / 3.69 | 10.7 MiB (17.06) | stable | RETAINED, TRANSIENT |
| claude/reload | 10.3 MiB | 26.9 MiB | 2.69 / 2.62 | 1.6 MiB (2.57) | stable | RETAINED |
| claude/replay | 10.3 MiB | 12.8 MiB | 1.29 / 1.24 | 25.7 MiB (40.88) | stable | TRANSIENT |
| codex/paged | 9.9 MiB | 21.7 MiB | 2.25 / 2.19 | 20.1 MiB (2.03) | stable | RETAINED, TRANSIENT |
| codex/memo | 9.9 MiB | 46.1 MiB | 4.64 / 4.64 | 30.8 MiB (3.10) | broken | RETAINED, TRANSIENT, PREFIX |
| codex/reload | 9.9 MiB | 21.7 MiB | 2.18 / 2.18 | 30.8 MiB (3.10) | broken | RETAINED, TRANSIENT, PREFIX |
| codex/replay | 9.9 MiB | 11.5 MiB | 1.87 / 1.16 | 41.0 MiB (4.12) | broken | RETAINED, TRANSIENT, PREFIX |

At production size (`--only claude/reload,claude/paged --turns 150 --rounds 24`): claude/reload holds 101.7 MiB for 39.2 MiB of history (2.59x), 0.68 MiB retained per turn for 0.26 MiB of history per turn; claude/paged 103.1 MiB (2.63x). The ratio is flat from 6 MiB to 39 MiB of history, so retained heap is linear in history at about 2.6x.

## Violations

Measured excess is the combination's retained bytes minus `K x history`. The production column is the matching stack's size in the 24 GiB profile above.

| # | Site (d3b5744f8) | Backend x mode | What | Sim excess | Production |
| --- | --- | --- | --- | --- | --- |
| V1 | `crates/backends/claude/src/agent/history.rs:137` (`json`) via `:145` (`record`) < `agent/durable.rs:1317`, `:1365` | claude x paged, reload, memo | A restored or new message keeps a fresh JSON `Arc<str>` and a record beside its parsed `Message`, for the life of the turn | about 1.2x history (est.: reload 2.62x minus replay 1.24x minus the 0.16x body); the mutation that copies the record adds exactly 1.0x, so record and JSON share one `Arc` today | 1338 + 394 + 202 + 188 MiB |
| V2 | `crates/core/durability/src/payload.rs:831` < `crates/runtime/durability/src/payload_memo.rs:65` | claude x memo, codex x memo | `load_many` copies each inline record into a new `Arc<str>` and the memo keeps it for the turn; history then serializes its own | claude +1.05x (11.1 MiB), codex +2.46x (24.4 MiB) over reload | 698 + 362 + 147 + 127 MiB |
| V3 | Claude `Message` parse trees (`ContentBlock` `Value` fields, flattened extras) | claude x all | A parsed tree of every message is kept beside its JSON | about 1.2x history: replay, which reopens and builds no request, retains 1.24x | ~1.6 GiB |
| V4 | `crates/backends/claude/src/agent/history.rs:227` < `:241` (`digests`) | claude, subscription wire | A second serialized copy per message in the `wire` `Box<str>` cache | not driven (the sim's client is an API-key client) | 330 MiB |
| V5 | `crates/backends/claude/src/durability/mod.rs:215` (`hydrated` `Slot<(String, Value, bool)>`) | claude x reload, memo | The hydrated cursor `Value` is kept and cloned | inside V1 / V3 | 400 MiB |
| V6 | `crates/backends/claude/client/src/lib.rs:3586` < `:2443` < `:2481` | claude x all | The kept request body; allowed once per session as a borrower, a violation when it is a third copy | ~0.16x (cleared tool results keep the Claude body small) | 670 MiB |
| V7 | `crates/backends/claude/src/agent/rounds.rs:1176` | claude x all | The token estimate forces the JSON cache on new tool results | inside V1 | 136 MiB |
| V8 | Codex history beside its loaded and request copies (`crates/backends/codex/src/durability/context.rs:177` shares loaded bytes; the live list and the request copy do not) | codex x paged, reload | Retained 2.2x history | 9.8 MiB over K at 9.9 MiB history | not profiled |
| V9 | Request build and load, both backends | claude x memo, replay; codex x all | A step allocates 2 to 4 request bodies (codex) or 2.5x history on load (claude replay) beyond what it keeps | codex 20 to 41 MiB per turn at 10 MiB history; claude/replay 25.7 MiB | ~475 MiB staged writes (`crates/core/durability/src/staged.rs:619`) |
| V10 | `crates/backends/codex/client/src/session/context.rs:105` (`truncate_tool_output`, `:808`) | codex x reload, memo, replay | On reload a tool output already cut to its token budget is cut again: the marker changes from `…2759 tokens truncated…` to `…7 tokens truncated…` and the cached prefix breaks at the first long output | prefix broken | prompt-cache misses after every reload |

## Adding a backend or a mode

A backend is one `Backend` entry: its name, the request array that holds messages, top-level request keys that change every request by design (Codex `client_metadata`), edit markers that may replace an item (Claude's cleared tool result), and two functions that build a runtime over the mock and spawn the session. A mode is one `Mode` entry: whether the store keeps a payload memo and how the session reopens between turns. No check code changes.
