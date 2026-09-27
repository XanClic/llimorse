# llimorse-chat

## Purpose

Framework for chatbot-style LLM agent applications built on the `llimorse` crate. It provides the
application shell (`App`) that runs an `llimorse::Agent` in a background thread against a pluggable
UI, UI notification contracts, a user-approval tool gate, subagent UI wiring, and JSONL session logs
with resume support. The chat history itself is a display concern: it flows to the UI as
`AgentUpdate` notifications, which `term-ui` renders. The consumer apps `work-buddy` and `lemon`
build their binaries on it, and `term-ui` provides the UI implementation it is designed for.

## Files

- `src/lib.rs` — `App<I: UiState>`: the application state. `App::new` and `App::new_with_history`
  create it (the latter pre-feeds the UI with the given `&[ChatMessage]` history by emitting
  `AgentUpdate` notifications, and errors on a tool result referencing an unknown call ID),
  `App::run` drives the main loop multiplexing UI events, a 500 ms redraw tick, and UI
  notifications; the private `push_history` converts messages into `AgentUpdate`s (system messages
  are skipped, tool results classified by their `TOOL CALL …` content prefix); `Drop` exits the
  agent thread.
- `src/agent.rs` — `ChatAgent` (crate-private): the agent-thread loop. Consumes the internal
  `Notification` enum (`Exit`, `QueuePrompt`, `ForceSubmitQueued`, `Continue`), submits queued user
  prompts, streams LLM chunks, executes pending tool calls, and mirrors everything to the UI as
  `ui::Notification` updates (`AgentUpdate` content, `Update`, `AwaitingPrompt`, `Exit`). The
  `Incoming` helper owns the prompt queue and `wait_for_abort`.
- `src/tools.rs` — `UserToolGate` (implements `llimorse_tools::ToolGate` by round-tripping
  `ui::Notification::RequestPermission` over a oneshot channel) and `SubagentNotifier` /
  `SubagentConnector` (implement the `llimorse_tools::subagent` traits so subagent chunks and tool
  calls are mirrored to the UI as `AgentUpdate`s under their `AgentId`). Tests cover the approval
  round trip and UI-drop rejection.
- `src/log.rs` — `SessionLog` (implements `llimorse::ChatListener`; appends one JSON line per
  message, `new`/`load`/`null`), `Resume` (`Fresh`/`Newest`/`File`, plus a clap value parser under
  the `clap` feature), and `SessionManager` (creates the timestamped log file for this run,
  resolves the resume target, and force-resolves incomplete tool calls in a loaded history). Tests
  cover log selection and resume-path resolution.
- `src/ui.rs` — the UI contract: `UiState` trait (the only UI extension point), `Event` (UI to app:
  `Exit`, `Input`, `ForceSubmitQueued`, `Continue`), `Notification` (app to UI: `Update`,
  `AgentUpdate`, prompt lifecycle, `AwaitingPrompt`, `RequestPermission`,
  `SubagentCreated`/`SubagentDropped`), `AgentUpdate` (history content deltas: `User`, `Reasoning`,
  `Content`, `ToolCallEx`, `ToolResultEx`), `AgentId`/`SubagentId`, and `NotificationChannel`
  (creatable before `App`, consumed by it).

## Key APIs

- `App` (`src/lib.rs`) — the main entry point. Consumers build an `llimorse::Agent`, a
  `ui::NotificationChannel`, and a closure `(&Agent) -> Result<UiState>`, then call `app.run()`.
- `ui::UiState` trait (`src/ui.rs`) — the extension point every chat UI implements; `term-ui`
  implements it for `TermUi`.
- `ui::{Event, Notification, AgentUpdate, NotificationChannel, AgentId, SubagentId}` (`src/ui.rs`)
  — the message contract between the app loop, the agent thread, and the UI.
- `UserToolGate` (`src/tools.rs`, re-exported at crate root) — pass it to gated tools from
  `llimorse-tools` before creating the `App`.
- `SubagentNotifier` (`src/tools.rs`, re-exported at crate root) — pass it to the
  `llimorse_tools::Subagent` tool.
- `log::{SessionLog, SessionManager, Resume}` (`src/log.rs`) — session persistence and `--resume`
  handling; `Resume::value_parser()` exists only with the `clap` feature enabled.

## Fit in the workspace

- Depends on `llimorse`: `Agent` (submission, streaming, tool display), `StreamingChunk`,
  `ChatListener` (for `SessionLog`), `line_format` message types, and
  `client::{ClientInfo, ClientState}` (both forwarded to the UI in `SubagentCreated`).
- Depends on `llimorse-tools`: implements its `ToolGate` (`UserToolGate`) and its `subagent`
  `SubagentNotifier`/`SubagentConnector` traits.
- Depends on `tokio` (mpsc/oneshot channels, timers), `futures`, `anyhow`, `chrono` (log file
  names), `parking_lot` (the `ClientState` handle), `serde`/`serde_json` (log format), `tracing`;
  `clap` is optional behind the `clap` feature.
- `work-buddy` — calls `App::new_with_history` with `term-ui`'s `TermUi` and uses
  `SessionManager`/`Resume` for `--session-logs`/`--resume`.
- `lemon` — same app pattern, plus `UserToolGate` for file/bash tools and `SubagentNotifier` for
  its subagent tool.
- `term-ui` — implements `ui::UiState`; keeps a private per-agent chat-history rendering state fed
  by `AgentUpdate` notifications, and tracks `AgentId`/`SubagentId` panes.

## Invariants

- The tool-call/tool-result display formats (``[id] …`` and ``=[name/id]=> …``) are produced in
  three places that must stay in sync: the live agent loop (`agent.rs`), `App::push_history`
  (`lib.rs`), and `SubagentConnector` (`tools.rs`). A resumed session must render identically to a
  live one.
- `App::push_history` classifies tool results as errors by the `TOOL CALL FAILED: ` /
  `TOOL CALL REJECTED: ` content prefixes, which are produced by `llimorse::line_format`. Keep the
  string match in step with that crate.
- The UI is shared with the agent thread only through the mpsc notification channels; all history
  content reaches the UI as `AgentUpdate` notifications. The agent runs in its own OS thread with
  its own current-thread tokio runtime; do not add cross-thread state to the app loop.
- `NotificationChannel` must stay constructible before `App` exists: `UserToolGate` and
  `SubagentNotifier` are built from it first, and `App::new` consumes it.
- `App::drop` sends `agent::Notification::Exit` and joins the agent thread; the agent loop must
  always terminate on `Exit`.
- A dropped/closed UI must read as a tool-permission denial (`UserToolGate::permitted` maps a
  broken oneshot to `Err`); `lemon` depends on that for exit safety.
- `SessionLog::new` opens with `create_new(true)` (never overwrites), and
  `SessionManager::new` resolves the resume target before creating the new log file, so the new
  file can never be picked as "newest".
- `App::push_history` deliberately skips system messages — they never appear in the chat display.
- The crate warns on missing docs, including for private items; new items need doc comments.

## Maintenance

- If you change `ui::Notification`, `ui::Event`, `ui::AgentUpdate`, `AgentId`, or `SubagentId`,
  update `term-ui` and re-check the `work-buddy`/`lemon` call sites, and update the Files/Key APIs
  sections here.
- If you change the tool-call display format, update all three producers (see Invariants) and the
  `term-ui` tests that pin the rendered format.
- If you change the session log file format, name scheme, or resume resolution, update `log.rs`
  tests and the usage in `work-buddy`/`lemon` (`Resume::value_parser`, `SessionManager::new`).
- If you add or remove a public type, update the Key APIs section and the crate-root re-exports in
  `src/lib.rs`.
- If you touch the agent loop in `agent.rs`, run the `tools.rs` and `log.rs` tests; the loop's
  queue/abort behavior is the trickiest part of the crate.
