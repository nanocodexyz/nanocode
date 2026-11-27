# Rules

Uphill writes these rules from each metric's `description`. Write code that meets them. `uphill` scores each metric against its target, and `uphill --compare HEAD --compact` lists what a change made worse.

## Commits

- Start each commit header with a conventional type, such as `feat:` or `fix:`, and a subject (commitlint `type-enum`, `type-case`, `type-empty`, and `subject-empty`).
- Start each subject in lowercase (commitlint `subject-case`).
- End each subject without a period (commitlint `subject-full-stop`).
- Keep each commit header to 100 characters or fewer (commitlint `header-max-length`).
- Put a blank line before the body and the footer, and wrap them at 100 characters (commitlint `body-leading-blank`, `body-max-line-length`, and the matching footer rules).
- State the change in the subject, not the reason. Leave out "because", "so that", and "in order to".
- Open each subject with a verb in its base form, such as "add", not "adds" or "added".
- Do not commit build output, installed packages, or `.env` files.

## Documentation

- Keep each sentence in a paragraph to 25 words or fewer.
- Keep each paragraph to about 40 words, and never more than 50.
- Keep each paragraph to six sentences or fewer.
- Use the active voice. Avoid passive phrases such as "is processed" (write-good `passive`).
- Use the short word instead of a wordy phrase, such as "to" for "in order to" (write-good `tooWordy`).
- Avoid weasel words such as "very" (write-good `weasel`).
- Write about the code, not the reader. Avoid "you", "your", and "yourself".
- Use precise verbs such as "returns" or "reports", not get, give, hand, know, say, see, or tell.
- Use the imperative, simple present, simple past, or simple future. Do not use a progressive or perfect form.
- Write "for example" and "that is" instead of "e.g.", "i.e.", and "etc.".
- Do not use semicolons in prose. Split the sentence, or use a comma, colon, or parenthesis.
- Do not use em dashes in prose. Split the sentence, or use a comma, colon, or parenthesis.
- Do not use emoji in prose.
- Keep every heading to fewer than four words.

## Correctness

- Fix every finding of clippy's default and pedantic lints.
- Finish each function. Leave no `todo!` or `unimplemented!` in the code.
- Encode invariants in types instead of `unwrap`, `expect`, slicing, or `panic!`. Use `get`, pattern matching, and `let` chains.
- Suppress a lint with `#[expect(lint, reason = "...")]`, never `#[allow]`.
- Write no `unsafe` code. If an exception is necessary, document each block with `// SAFETY:`.
- Mark `#[inline(always)]` only with a measured reason in an `#[expect(clippy::inline_always, reason = "...")]`.
- Never iterate a hash map to produce output. Use a `Vec`, a `BTreeMap`, or the declared order.
- Do not build `Rc<RefCell<_>>` graphs or wrap mutexes and buffers in `Rc`.
- Set clippy's `all` and `pedantic` groups and the restriction lints the conventions list in `[workspace.lints]`.
- Allow only `module_name_repetitions`, `must_use_candidate`, `similar_names`, and `too_many_lines` workspace-wide.
- Deny `unsafe_code` in the workspace lints.
- Keep the original error. Do not discard it with `map_err(|_| ...)`.
- Do not clone a value that is not used again (clippy `redundant_clone`). Move it instead.
- Return errors as typed values, not `String`, `&str`, or `Box<dyn Error>`.
- Give each channel a capacity.
- Keep each file parseable without syntax recovery.
- Do not write a function that calls itself. Use a loop with a bound that a reader can check.
- Use explicitly sized integers in Rust and Zig, such as `u32`, not `usize`.
- Assert preconditions, postconditions, and invariants, with at least two assertions per function on average. At least 2.
- Check one condition in each assertion: `assert(a); assert(b);`, not `assert(a and b)`.

## Style

- Format each Rust file with rustfmt (`cargo fmt`).
- Write output through `std::io::Write`, not `println!`, `eprintln!`, or `dbg!`.
- Do not write decorative divider comments, such as `// -----`.
- Name the unit in each `Duration::from_*` argument, such as `timeout_secs`.
- Keep the item's own name in `use` declarations. Do not rename it with `as`.
- Record values in log macros as fields, not interpolated into the message.
- Use a struct instead of a tuple of three or more types.
- Keep each function to 70 physical lines or fewer, including its signature.
- Split long functions. Each line removed from a function shows in comparisons.
- Keep each line to 100 columns or fewer.
- Write each comment as a sentence that starts with a capital letter and ends with a full stop or a colon.
- Indent code with 4 spaces.
- Name files in `snake_case`.
- Leave no trailing whitespace, blank lines at the end, or conflict markers in a tracked text file (`git diff --check`).
- Start each GitHub workflow with `name` and `on`, then list its other top-level keys in alphabetical order.

## Architecture

- Mark items that another crate cannot reach `pub(crate)` (rustc `unreachable_pub`).
- Make sure a workspace target, compiler input, or rustfmt traversal reaches each Rust file. Delete files that nothing reaches.
- Do not let sibling modules use each other in a circle.
- Do not let the types of two modules know each other in a circle. Keep each circle of types to seven types or fewer.
- Keep a type's fields private to its module. Code outside may read a value, but not write, index, or call a method on a field.
- Give each module a narrow interface. Show seven functions or fewer, or hide more than the module shows.

## Configuration

- Use edition 2024 in each package.
- Inherit the workspace's lints in each workspace member (`lints.workspace = true`).
- Take dependency versions from `[workspace.dependencies]` in each workspace member.
- Pin a version in rust-toolchain.toml, not a channel such as `stable`.
- Declare `rust-version` in the workspace.
- Set `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, and `strip = "symbols"` in the release profile.
- Add a dependency only with a stated reason. The baseline records the count without a target, so each change shows in review.
