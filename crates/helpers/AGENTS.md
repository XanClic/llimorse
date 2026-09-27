# helpers

## Purpose

A shared, dependency-light utility crate for the llimorse workspace: merging a config file
under command-line arguments, a serde helper for system-prompt file lists, and string
truncation for display. It is used by the harness apps `work-buddy` and `lemon` and by the
display-facing crates `term-ui` and `llimorse-tools`; the core `llimorse` and `llimorse-chat`
crates do not depend on it.

## Files

- `src/lib.rs` — crate root. Declares the three public modules and re-exports `Mergeable`
  and `TruncatedDisplay` at the crate root. Enables `missing_docs` warnings, including for
  private items.
- `src/macros.rs` — the `Mergeable` trait (`merge`, `merge_weak`) with built-in impls for
  `Option<T>`, `bool`, and `Vec<T>`; the `derive_merge!` macro that implements `Mergeable`
  field-by-field for a struct.
- `src/system_files.rs` — `deserialize`, a serde deserializer that reads a single file path
  (legacy config form) or a list of paths into a `Vec<PathBuf>`.
- `src/truncated_display.rs` — the `TruncatedDisplay` trait for `&str` and `String` with
  `truncated_display(max_length) -> impl Debug + Display`; the private `TruncatedDisplayStr`
  struct implements both `Debug` and `Display` with the same logic.

## Key APIs

- `helpers::Mergeable` (`src/macros.rs`) — `merge(&mut self, other)` lets `other` take
  precedence; `merge_weak(&mut self, other)` lets `self` take precedence. Built-in impls:
  `Option<T>` (takes `other` only if `Some` / takes `other` only if `None`), `bool` (both
  methods OR), `Vec<T>` (append: `merge` appends `other` to `self`, `merge_weak` puts `other`
  first and appends `self`; an empty list appends nothing).
- `helpers::derive_merge!` (`src/macros.rs`) — implements `Mergeable` for a struct whose
  fields all implement it. Each `merge`/`merge_weak` call delegates to the corresponding
  field method.
- `helpers::system_files::deserialize` (`src/system_files.rs`) — use with
  `#[serde(default, deserialize_with = ...)]` on a `Vec<PathBuf>` field so that both
  `system = "foo.md"` (legacy) and `system = ["foo.md", "bar.md"]` deserialize.
- `helpers::TruncatedDisplay` (`src/truncated_display.rs`) — `truncated_display(max_length)`
  on `&str`/`String` returns an object implementing both `Debug` and `Display`. Output is at
  most `max_length` chars, counting the trailing `…` when truncating; `max_length = 0` prints
  nothing.

## Fit in the workspace

- Depends on: `serde` only, via the workspace dependency table (`crates/helpers/Cargo.toml`).
- `crates/work-buddy` — `derive_merge!` on its `Args` struct, with `args.merge_weak(cfg_args)`
  overlaying a deserialized config file under the CLI arguments; `system_files::deserialize`
  on the `system` field; `TruncatedDisplay` in the `Debug` impls of its tools (tasks,
  worklog, knowledge, write_md).
- `crates/lemon` — the same `derive_merge!`/`merge_weak` config pattern on `Args`
  (`args.merge_weak(cfg)`); `system_files::deserialize` on both `system` and `subagent_system`;
  no `TruncatedDisplay` use.
- `crates/term-ui` — `TruncatedDisplay` for chat titles/bodies (200 chars) and subagent task
  names (40 chars) in the TUI.
- `crates/llimorse-tools` — `TruncatedDisplay` in the `Debug` impls of the file tools (View,
  Write, Edit) and of `Bash` results (stdout/stderr).
- `crates/llimorse` and `crates/llimorse-chat` — no dependency on this crate.

## Invariants

- Keep the `merge`/`merge_weak` precedence contract: `merge` = other wins, `merge_weak` =
  self wins. work-buddy and lemon call `merge_weak` with the config-file values, so CLI
  arguments must keep overriding the config file.
- `derive_merge!` is `#[macro_export]` (so it lives at the crate root) and is also re-exported
  from `macros.rs`; both `helpers::derive_merge!` and `helpers::macros::derive_merge!` are
  used by consumers and must keep working. The generated impls reference
  `$crate::macros::Mergeable`, so the `macros` module must stay public.
- The macro's input grammar (bare struct, attributes, trailing comma on the last field) must
  stay compatible with how work-buddy and lemon write their `Args` structs.
- `Vec<T>::merge`/`merge_weak` concatenate, never replace: in `merge_weak` (the
  config-file pattern) the config values come first and the CLI values are
  appended after, so higher-precedence prompts land later in the list. An empty
  list appends nothing, so a config file can never clobber a CLI value. Do not
  "fix" this to replacement.
- `system_files::deserialize` must keep accepting the single-string legacy form as well as
  the list form; existing config files rely on both.
- Truncation counts `char`s (not bytes), the trailing `…` counts toward `max_length`, and
  `Debug` and `Display` for `TruncatedDisplayStr` must stay in sync; `max_length = 0` must
  yield empty output in both.
- New items must have doc comments (`missing_docs` warnings, including private items), and
  the crate should keep `serde` as its only runtime dependency.

## Maintenance

- If you change the `Mergeable` trait (methods, impls, or semantics): check the `Args`
  structs and `merge_weak` calls in `work-buddy` and `lemon`.
- If you change the `derive_merge!` syntax or behavior: check both `Args` derive sites in
  `work-buddy` and `lemon`.
- If you change `system_files::deserialize`: check the `system`/`subagent_system` fields and
  the documented config-file format in `work-buddy` and `lemon`.
- If you change truncation behavior or the `TruncatedDisplay` trait: check `term-ui`,
  `llimorse-tools`, and the tool `Debug` impls in `work-buddy`.
- If you add a new module or public item: update the re-exports in `lib.rs` and the Files /
  Key APIs sections of this file.
