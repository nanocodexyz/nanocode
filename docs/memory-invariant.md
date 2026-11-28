# Memory invariant: one owner per byte

## Why

A busy multi-tenant server (one process, ~190 concurrent turns, 32 GiB limit) grew its heap by ~150 MiB per running turn. Its heap profile showed the same committed message bytes held three to five times per session: a parsed `Message` tree, a cached JSON string, a durable record, a payload memo copy and a wire cache. Each copy is reasonable alone; together they made retained heap a multiple of history.

## The invariant

1. **One owner per byte.** A live session holds each committed message's canonical serialized bytes exactly once. Every other view (request body, durable record, log entry, cache, parsed value tree) borrows those bytes (a shared `Arc<str>` or a slice of it) or is derived on demand and dropped. Retained heap per session is at most `K x history bytes + M x messages + C`.
2. **Bounded transients.** Building a request, loading or replaying a session, or committing a record allocates at most `T_BODY x request body + T_MSG x largest message + C_T` beyond what it retains: never O(history) more per step, except the one pass that writes the request body.
3. **Prefix-stable wire bytes.** Rebuilding the request from the single owner yields byte-identical message prefixes across rounds and across a durable reload, so prompt caching is unaffected. Deliberate context edits (Claude's tool-result clearing) are the only allowed breaks.

| Constant | Value | Code |
| --- | --- | --- |
| `K` | 1.05 | `RETAINED_PER_HISTORY_BYTE` |
| `M` | 1 KiB | `RETAINED_PER_MESSAGE_BYTES` |
| `C` | 2 MiB | `RETAINED_CONSTANT_BYTES` |
| `T_BODY` | 1 | `TRANSIENT_PER_REQUEST_BODY` |
| `T_MSG` | 4 | `TRANSIENT_PER_LARGEST_MESSAGE` |
| `C_T` | 4 MiB | `TRANSIENT_CONSTANT_BYTES` |

All six live in `bin/nanocode-sim/src/memory.rs`.

The retained bound has two slopes because a session keeps two kinds of memory. Bytes: the owner JSON of each message is about 1.03x its wire size, so `K = 1.05`. Bookkeeping: every message has a fixed cost whatever its size: the history entry and its `Arc`, the durable record and its key, the paged sequence log's reference, the wire cache's id, digest and layout prefix, and allocator rounding. That cost is O(messages), not O(bytes), so it has its own term instead of hiding in `K`, where the bound would depend on the average message size. `M = 1 KiB` is the measured per-message cost rounded up. A parsed tree kept beside every message's JSON costs several KiB per message and fails `M`; a second serialized copy of every message fails `K`.

**History bytes** are measured on the wire: for each position in the request's message array (`messages` for Claude, `input` for Codex), the largest serialized size it has had. Positions Claude later clears keep their original size. **Messages** is the number of positions.

## Who owns the bytes

The owner is the committed message's canonical serialized JSON, one `Arc<str>` per message, created once: by the backend when the message is committed, or by the load path when a stored record is read.

| Layer | Rule |
| --- | --- |
| Backend history (Claude `HistoryMessage`, Codex `ResponseItem` list) | Holds the owner. Claude keeps one entry per message: the record (the owner bytes) and, until its durable commit, the typed message it was built from; after the commit, typed views are parsed from the bytes on demand and dropped. Facts read every request (image count, tool-result token sizes, wire flags) are small fields, not copies. |
| Cleared copies (Claude tool-result clearing) | Derived per request from the owner and encoded straight into the body; never cached. The edit is deterministic, so the bytes are the same every round. |
| Wire / request builder | Writes the body by copying borrowed message bytes into one buffer per request; keeps no per-message serialized copy. The kept request body for cache reuse counts as a borrower only if it is the only copy of its bytes beyond the owner. |
| Durable records, `SequenceLog`, staged writes | Build records from the owner `Arc` (`EncodedPayload::from_json(Arc::clone(..))`); after a durable commit, a record may become a reference and drop its bytes. Rarely set record fields (a staged closure's pending records and children) live behind one optional pointer. |
| Payload memo (standby / handoff load) | Shares the loaded bytes with history (`EncodedPayload::shared_json()`), never a second `Arc<str>` of the same record. |
| Record cache | Budgeted, and holds record text a live session does not already own. |
| Load / reload / replay | One streaming pass: each record's bytes become the owner directly; no full parse tree of history is kept beside it. |

## The check

```sh
SHIPPY_REMOTE_BUILD=0 cargo build --release --locked --offline -p nanocode-sim
target/release/nanocode-sim --memory                          # report only; exit 1 on a broken prefix
target/release/nanocode-sim --memory --memory-gate-retained   # also exit 1 on a retained breach
target/release/nanocode-sim --memory --memory-gate            # exit 1 on any breach
target/release/nanocode-sim --seeds 0..8 --scenario memory --mutation step_transient_alloc
target/release/nanocode-sim --memory --only claude/reload --turns 150 --rounds 24 --size-profile tools --verbose
bin/nanocode-sim/memory-mutations.sh                          # the mutations the check catches
```

`nc-build check` (`scripts/check`) runs the combos listed in `bin/nanocode-sim/memory-gate.txt` with `--memory --memory-gate` beside the simulator's `--tidy` and `--machines` walks, so a breach in a listed combo fails the check. A combo is listed once it holds the invariant on master. A `combo=retained` line (also accepted by `--only`) gates retained and prefix only, for a combo whose load transient is still open: memo, reload and replay hydrate the whole checkpoint as one `Value` on open. `--memory-gate-retained` does the same for every combo.

A gated line also carries `peak_bytes=N margin_bytes=M`. A step's peak is the largest live heap the counting allocator saw during it, with no baseline subtracted. A step whose peak passes `N + M` breaches `PEAK`.

`peak_bytes` is the largest step peak of the combo on master, under the defaults, in the order nc-build runs the combos. `margin_bytes` comes from the spread of repeated runs, and the file's header records how. Measure again and update the line when a change moves a combo's peak.

The step transient subtracts the live heap after the turn's prompt, so a transient below that level hides from it. The peak gate still catches it. Mutation `step_transient_alloc` allocates the combo's whole peak limit in each step. `--scenario memory` runs one gated combo per seed, and the canary needs all eight caught.

Each combination of a backend and a durability mode runs one session against an in-process mock model: `--turns` turns (default 60) of `--rounds` tool rounds (default 16), each tool result a seeded log-uniform size between `--min-bytes` and `--max-bytes`. `--size-profile` picks a mix: `mixed` (the default, 200 B to 64 KiB, about 3.5 KiB per message), `tools` (1 KiB to 64 KiB, coding-agent tool output) and `chat` (64 B to 2 KiB, where bookkeeping dominates and `M` is what the check tests). The durable store keeps its records on disk, so they do not count as session heap. A counting global allocator (`nanocode_sys::alloc::Counted`) gives live and peak bytes; after each turn the check records retained (live minus the baseline before the first turn) and transient (peak during the turn minus the larger of start and end). The report's `B/msg` column is retained minus history, per message. The mock keeps only one hash per message, and checks every request's message prefix against the previous request's.

Backends (`BACKENDS`): `claude`, `codex`. Modes (`MODES`):

| Mode | What it drives |
| --- | --- |
| `paged` | One live durable session for every turn |
| `memo` | Store with `with_payload_memo()`; each turn reopens with `store.load` + `open_loaded_owned` (the standby / handoff path) |
| `reload` | Each turn reopens the session with `open_session` (durable reload mid-session) |
| `replay` | After each turn, reopen and resubmit the same request id: the completed step replays without a model call |

There is no unpaged mode: every runtime has a durable store (the default is a home JSONL store), and Claude durable policies always page messages.

## Adding a backend or a mode

A backend is one `Backend` entry: its name, the request array that holds messages, top-level request keys that change every request by design (Codex `client_metadata`), edit markers that may replace an item (Claude's cleared tool result), and two functions that build a runtime over the mock and spawn the session. A mode is one `Mode` entry: whether the store keeps a payload memo and how the session reopens between turns. No check code changes.
