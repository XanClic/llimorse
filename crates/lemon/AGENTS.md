# lemon

## Purpose

lemon is a single-binary agent harness for llama.cpp's llama-server: an interactive
TUI chat app equipped with file tools (view/write/edit), a bash tool, web search,
and a subagent tool, with session logging and resuming. It is the workspace's
"just works" harness for local model development. It is driven by humans via its
CLI and, primarily, by the root `lemonade.sh` script, which runs it inside a
disposable Podman container. No other crate in this workspace depends on it.

## Files

- `src/main.rs` — the entire crate (binary only; no library target).
  - `Args` — clap `Parser` + serde `Deserialize` struct holding every CLI flag;
    `kebab-case` serde rename with `deny_unknown_fields`, so config-file keys are
    exactly the kebab-case flag names. Declared with `helpers::derive_merge!`
    so a TOML `--config` file supplies weak (CLI-wins) base values. `config` and
    `resume` are `#[serde(skip)]` (CLI-only).
  - `LemonToolFactory` — implements `llimorse_tools::ToolFactory` for subagents;
    mirrors the main agent's tool set (see Invariants) using the gate the main
    agent created.
  - `display_model_name` — strips a trailing alphanumeric file extension (e.g.
    `.gguf`) from a model id for display to the LLM; keeps dots inside the name.
  - `read_system_files` — concatenates one or more prompt files with a blank
    line between them, trimming trailing newlines for a uniform join.
  - `main` — parses args, merges the TOML config, rejects
    `--subagent-max-parallel 0`, opens the log file, installs the tracing
    subscriber into it, builds `llimorse::Client` and `Agent` (with the
    `SessionManager` log as listener), wires tools and the subagent tool, and
    runs `llimorse_chat::App` with `TermUi`.
  - Constants: `LLAMA_URL_DEFAULT` (`http://127.0.0.1:8080`), `LOG_FILE_DEFAULT`
    (`lemon.log` in `$TMPDIR`).
  - A built-in subagent system prompt string, used when `--subagent-system` is
    not given.
  - `tests` — unit tests for `display_model_name`.

## Key APIs

lemon is a binary: it exposes no library surface. Its load-bearing interface is
the CLI contract that `lemonade.sh` (and humans) rely on, defined by `Args`:
`--config`, `--llama-url`, `--searxng-url`, `--model`, `--system` (repeatable),
`--subagent-system` (repeatable), `--subagent-max-parallel`, `--log-file`,
`--session-logs`, `--resume` (optionally valueless, resumes the newest session
log), and `--zesty` (auto-approve all tool calls). Config-file semantics come
from the serde mapping on `Args`; session resuming is delegated to
`llimorse_chat::log::SessionManager`/`Resume`.

## Fit in the workspace

Dependencies (all workspace crates unless noted):

- `llimorse` — core: `Client::new` and `Agent` (`new_with_listener`,
  `push_system`, `push_history`, `add_tool`, `client_state().model_name`).
- `llimorse-chat` (with its `clap` feature) — app framework: `App::new_with_history`
  + `run`, `log::{SessionManager, Resume}`, `UserToolGate`, `SubagentNotifier`,
  `ui::NotificationChannel`.
- `llimorse-tools` — the basic tool set for llimorse harnesses: `View`, `Write`,
  `Edit`, `Bash`, `WebSearch`, `AutoApprove`, and `Subagent`/`ToolFactory`.
- `term-ui` — the TUI: `TermUi::new("Lemon", agent)`, the UI state for `App`.
- `helpers` — `macros::{Mergeable, derive_merge}` for config merging and
  `system_files::deserialize` (single path or list) for the prompt-file flags.
- External: `anyhow`, `chrono`, `clap`, `schemars`, `serde`, `serde_json`,
  `tokio`, `toml`, `tracing-subscriber`.

Consumers:

- None in the workspace. The external driver is the root `lemonade.sh`, which
  runs the host-built `lemon` binary (found via `LEMON_BIN`/`PATH`) inside a
  Podman container with `-w /work` as `lemon --zesty --llama-url ...
  --searxng-url ...`, appending `--system /system-prompt-env.md` (always),
  `--system /system-prompt.md` (from an explicit `--system`, or the first of
  `AGENTS.md` / `CLAUDE.md` found in the working directory, with `LEMON.md`
  appended as a refinement when present),
  `--subagent-system /subagent-system-prompt.md` (explicit flag or a
  `LEMON.SUBAGENT.md` in the working directory), and `--config /lemonade.toml`
  (the host's `~/.lemonade.toml`) when present.
- The workspace's agent-documentation conventions (this multi-level AGENTS.md
  setup) are defined in the root `/work/AGENTS.md`. In the lemonade flow, lemon
  picks up `AGENTS.md` / `CLAUDE.md` (the first that exists) from the working
  directory as its system prompt, with `LEMON.md` appended as a refinement when
  present, and `LEMON.SUBAGENT.md` for subagents — so a crate's AGENTS.md can
  reach the agent through this mechanism.

## Invariants

- One system message only. The system prompt and the date/model note are folded
  into a single leading system message; several chat templates mishandle
  multiple system messages. Never push a second system message.
- On resume (`SessionManager` returns non-empty history), the system prompt and
  date note are not re-injected; only a fresh session gets them. Keep that
  split.
- Tool wiring: `View` is always auto-approved; `Write`/`Edit`/`Bash` are gated
  by the shared `UserToolGate` unless `--zesty`, which swaps in
  `AutoApprove`. `LemonToolFactory` must mirror the main agent's wiring (same
  tools, same gate) — the subagent inherits the main agent's tool gate by
  design, so the gate is created before the agent's tools are added.
- `--subagent-max-parallel 0` is an error (minimum 1); `None` means unlimited.
- The terminal is taken by the TUI, so all tracing output goes to the log file
  (`--log-file`, default `$TMPDIR/lemon.log`), created fresh (truncated) at
  start, with ANSI disabled.
- `Args` serde shape is the config-file contract: `deny_unknown_fields` plus
  `kebab-case` renames, and `system_files::deserialize` must keep accepting both
  a single path (legacy) and a list. New flags should follow the same pattern.
- The built-in subagent prompt string is the fallback when no
  `--subagent-system` is given; it is read/validated at startup so a bad prompt
  path fails before anything connects.

## Maintenance

- If you add, remove, or rename a flag in `Args`, update the Files section and
  the Key APIs flag list here, and check `lemonade.sh`, which forwards and
  intercepts several of these flags.
- If you change the built-in subagent prompt, the system-prompt assembly (the
  date/model note), or the tool-wiring rules, update Invariants.
- If you change `LemonToolFactory`, keep it in sync with the main agent's tool
  wiring in `main`.
- If the crate gains more source files, add a line per file to the Files section.
