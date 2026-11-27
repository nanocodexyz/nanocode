# nanocode

Follow the [Uphill rules](.agents/uphill/rules.md), which `uphill rules --write` rewrites from each metric's description: @.agents/uphill/rules.md

A Rust agent SDK: one workspace of reusable crates that run the same agent natively and in WASM. Many agents work on it at once, so every rule here is about keeping master green while they do.

## Code

- Keep code simple and changes small. Match the surrounding code and preserve unrelated work.
- No code comments, doc comments, tests, test-only dependencies or Cargo features (`local-cells` on `nanocode-code-mode` is the one exception).
- `bin/nanocode-sim` is the deterministic simulator and the one exception to "no tests": checks live there and run by seed (`bin/nanocode-sim/sweep.sh`), and a new invariant comes with a mutation the simulator catches.
- No magic constants or arbitrary caps: a limit comes from a protocol, a measurement or caller policy. Surface invalid states; never hide them with silent truncation, clamping or fallbacks.
- Make invalid states unrepresentable: model a lifecycle as one enum whose variants carry only what that phase has.
- Lock-free: no Mutex, RwLock or Condvar. Use nanocode-sync (an owning task, Actor or Mailbox; Atom changed copy-on-write with side effects after the commit; Slot, Signal, atomics). Blocking I/O runs on an owning worker behind channels, never on Tokio workers.
- Every crate builds for native and WASM with parity; bindings, apps, services and wasm-bindgen live elsewhere. Follow [the crate layout](crates/README.md): contracts in `core/`, backend-generic orchestration in `runtime/`, each provider's wire client, tools and driver in `backends/<provider>`, host adapters in `integrations/`; `nanocode` only re-exports.
- Preserve public APIs. Use authentic, version-pinned upstream Claude and Codex prompts and tool shapes, keeping SDK additions separate, and share Code Mode and computer use across both backends.
- Inference, subagents and durability live in the brain; Code Mode runs on the user's machine. Never silently replay dispatched work: report uncertain outcomes.
- [TIGER_STYLE.md](TIGER_STYLE.md) is how we apply TigerStyle; this file wins where they differ.

## Checks

- Run `scripts/check`: it checks the crates your diff touches and their dependents (`cargo check --locked --lib --bins --examples` natively, so bin-only crates such as `nc-board` and `nanocode-fuzz` and crate examples are checked too, and `--lib --target wasm32-unknown-unknown` for the libraries among them), runs the simulator's `--tidy`, and runs the pass jobs and canaries of `bin/nanocode-sim/cfo.sh`'s table for every scenario that reaches them, printing one green or red line per check with its time. `scripts/check --full` is the release gate (whole workspace, every scenario and canary). `scripts/check-all [--full] <pyramid2> <pyramid2-web>` runs it here and in other clones at once. There is no CI: build, release and deploy by hand ([docs/RELEASE-CHAIN.md](docs/RELEASE-CHAIN.md)).
- Back performance changes with reproducible before/after measurements, interleaved, and put them in the commit message. [Uphill](https://github.com/wevm/uphill) runs the shared ones in `.agents/uphill/uphill.yaml` (latency, CPU counters, request bytes, sizes, memory, engines, build times; `uphill <category>` runs one): `uphill --compare origin/master --compact` for the comparison, and `uphill --update` to accept new sizes and counts into `.agents/uphill/uphill.baseline.yaml` in the same commit.
- Check behavior with harnesses in ignored `output/`, such as `output/hand-wiring`: a crate with its own `[workspace]` and path dependencies on these crates, one binary per check or benchmark, run against local mocks. Keep every artifact there; never commit them, credentials or personal data.
- Keep reference-build dependencies in the repository (`vendor/cargo`), separate online acquisition from builds, and verify local references with network access denied: `scripts/reference-cargo.sh metadata`, `scripts/reference-cargo.sh check nanocode-bytes`. Keep `SHIPPY_REMOTE_BUILD=0` for local checks, preserve compiler wrappers, and record external toolchains and system libraries explicitly.
- Use `cf --profile tempo` for Cloudflare operations, with `CLOUDFLARE_ACCOUNT_ID=0a39052b0a32ba8c8444345fe21b7595`. Use Wrangler only for a documented missing capability.

## Git and swarms

- Master only. Commit only your own changes and push each one straight to master: no pull requests and no other branches (feature, wip or snapshot), including for unfinished work, which lands in small commits that keep the checks passing.
- Many agents push at once. Take a write scope no other agent holds, rebase on origin/master before every push, rerun the checks your change touches, and never force-push or rewrite someone else's commit. On a conflict in another agent's change, keep theirs and redo yours on top.
- Nothing lives only on a machine: end every session with `git status` clean and nothing ahead of origin/master.
- The commit message is the record: `type(scope): summary`, then why, with the commands and measurements behind it. This repository is public: no internal links, people's private details or security findings in commits.
- No decision logs, status, plan or results documents. Docs describe what exists now and how to use it; delete one when it stops being true. History is in git (the old decision log is `DECISIONS.md` at 6a15d78).
