# llimorse — agent orientation

Rust workspace for LLM agent harnesses, currently only for use with llama.cpp's
`llama-server`. This file is the root of a multi-level agent documentation setup:
it is a **router**, not a summary. Read only the detail files you need — each one
is written so you can work on its crate without reading all of its source.

## Crate map

| Crate | What it is | Details |
|---|---|---|
| `crates/llimorse` | Central crate: llama-server client (SSE streaming, native tool calling), agent loop with chat history and tool execution, the `tool!` macro, and the shared observable state UIs render from | `crates/llimorse/AGENTS.md` |
| `crates/llimorse-chat` | Framework for chatbot-style apps: the `App` shell, the UI notification contract (`UiState`, `AgentUpdate`), the user-approval tool gate, subagent UI wiring, JSONL session logs with resume | `crates/llimorse-chat/AGENTS.md` |
| `crates/llimorse-tools` | The standard tool set (View/Write/Edit/Bash/WebSearch/Subagent) plus the `ToolGate` permission abstraction | `crates/llimorse-tools/AGENTS.md` |
| `crates/term-ui` | ratatui TUI for llimorse-chat apps; `TermUi` implements `UiState` (per-agent chat histories with markdown, permission popups, input area) | `crates/term-ui/AGENTS.md` |
| `crates/work-buddy` | Example app (binary): a terminal chatbot where an LLM agent manages a task list and a work log | `crates/work-buddy/AGENTS.md` |
| `crates/lemon` | The agent harness (binary): a TUI chat app with file/bash/web tools, a subagent tool, and session resuming — driven by `lemonade.sh` in a container | `crates/lemon/AGENTS.md` |
| `crates/helpers` | Dependency-light shared utilities: config-file/CLI-arg merging (`Mergeable`, `derive_merge!`), system-prompt file deserialization, `TruncatedDisplay` | `crates/helpers/AGENTS.md` |

Dependency direction: `llimorse` → `llimorse-tools` → `llimorse-chat` → `term-ui`
→ the apps (`work-buddy`, `lemon`); `helpers` is used by `llimorse-tools`,
`term-ui`, and the two apps, not by `llimorse`/`llimorse-chat`.

## Routing

- LLM client, agent loop, streaming, `tool!`, wire/session-log format → `crates/llimorse/AGENTS.md`
- Chat loop, `App`, sessions/resume, tool approval, UI notifications → `crates/llimorse-chat/AGENTS.md`
- Tools (view/write/edit/bash/websearch/subagent), permissions → `crates/llimorse-tools/AGENTS.md`
- Terminal rendering, input handling, layout → `crates/term-ui/AGENTS.md`
- Config merging, small shared utilities → `crates/helpers/AGENTS.md`
- The harness app, its CLI flags, how lemon is launched → `crates/lemon/AGENTS.md` and `lemonade.DESIGN.md`
- Task / work-log example app → `crates/work-buddy/AGENTS.md`

## Things that are not in crates/

- `lemonade.sh` — entry-point script: builds/runs `lemon` inside a disposable
  Podman container with `-w /work`, injecting system prompts and config. Design:
  `lemonade.DESIGN.md`.
- `lemonade.DESIGN.md` — design decisions for `lemonade.sh`; read it before
  changing that script.
- `SYSTEM_PROMPT.md`, `REPORT_PROMPT.md` — the work-buddy app's system prompts
  (assistant persona; bi-weekly report generator).
- `.github/workflows/docs.yml` — on push to `main`, publishes crate docs
  (`cargo doc --no-deps --document-private-items`) to GitHub Pages.

## Workspace conventions

- Edition 2024, resolver 3. New external dependencies go in
  `[workspace.dependencies]` in the root `Cargo.toml`, referenced without versions.
- `rustfmt.toml`: `format_code_in_doc_comments`, `imports_granularity = "Module"`.
  (It sets `edition = "2021"` while the workspace is 2024 — verify before
  relying on it.)
- `missing_docs` warnings are on in several crates, including for private items:
  new items need doc comments.
- Build and test from the workspace root: `cargo build`, `cargo test`.

## Maintaining these files

- This file stays a router: one line per crate, details live in the crate's file.
- No line numbers in any AGENTS.md: file paths for "where", identifiers for "what".
- If you change a crate's structure, public API, invariants, or a workspace
  convention, update the matching AGENTS.md in the same change.
- If a crate file grows past ~150 lines, split it into per-topic files and
  update the routing table above.
