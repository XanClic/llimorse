# llimorse-tools

## Purpose

The standard tool set for agent harnesses written with `llimorse`: file access (`View`,
`Write`, `Edit`), command execution (`Bash`), web search against a SearXNG instance
(`WebSearch`), and task delegation to subagents (`Subagent`). It also defines the
`ToolGate` permission abstraction that lets a harness demand user approval before a
tool call executes. Consumers are the `lemon` harness (all tools), `work-buddy`
(web search only), and `llimorse-chat`, which supplies the `ToolGate` and subagent UI
implementations the tools are built to accept.

## Files

- `src/lib.rs` — crate root; module declarations and re-exports (`Bash`, `Edit`, `View`,
  `Write`, `Subagent`, `ToolFactory`, `WebSearch`). Defines the `ToolGate` trait, the
  blanket-implemented `GateableToolParams` pseudo-trait (`Any + Debug + Display`), and
  the `AutoApprove` gate.
- `src/file.rs` — the file tools, each declared with `llimorse::tool!`: `View`
  (`ViewParams`/`ViewResult`; reads a file whole or an inclusive, 1-based line range),
  `Write` (`WriteParams`/`WriteResult`; truncates and rewrites the whole file), `Edit`
  (`EditParams`/`EditResult`; replaces the first `old_content` match, fails on zero or
  multiple matches). All are generic over `ToolGate` and consult the gate before
  touching the file.
- `src/bash.rs` — the `Bash` tool (`BashParams`/`BashResult`): runs `command_line`
  through `bash -c`. `shell_command` builds a decidedly non-interactive child: new
  session via `setsid` on unix, null stdin, `GIT_TERMINAL_PROMPT=0`,
  `SSH_ASKPASS_REQUIRE=never`, `kill_on_drop(true)`. `BashResult.exit_code` is the
  negated signal number when the child dies by a signal (unix).
- `src/web_search.rs` — the `WebSearch` tool (`WebSearchParams`/`WebSearchResults`):
  queries a SearXNG `/search?format=json&categories=general` endpoint and maps the
  response into `title`/`url`/`snippet` triples (the element type `WebSearchResult`
  is private). Not gated.
- `src/subagent.rs` — the `Subagent` tool (`SubagentParams`/`SubagentResult`) and its
  support traits: `ToolFactory` (equips each subagent with a tool set),
  `SubagentNotifier` (creates a connector per subagent), `SubagentConnector` (receives
  streamed chunks and tool call/result events). `Subagent::execute` spawns a fresh
  `llimorse::Client` and `Agent`, optionally forks the parent's history, then loops
  `submit`/`execute_pending_calls` until the subagent produces a final response.

## Key APIs

- `View::new`, `Write::new`, `Edit::new`, `Bash::new` (`src/file.rs`, `src/bash.rs`) —
  each takes a `ToolGate` and returns the state object to pass to `Agent::add_tool`;
  `AutoApprove` is the no-approval gate.
- `WebSearch::new(searxng_url)` (`src/web_search.rs`) — the SearXNG base URL; no gate.
- `Subagent::new(system_prompt, llama_url, model_name, tool_factory, notifier,
  max_parallel)` (`src/subagent.rs`) — `max_parallel: Option<usize>` bounds how many
  subagents run concurrently (`None` = unlimited).
- `ToolGate` (`src/lib.rs`) — `async fn permitted(&self, params: &dyn GateableToolParams)
  -> Result<(), String>`; rejection is signaled by returning `Err`. The `Display` impl
  of a tool's params is the text a user sees in the approval prompt.
- `AutoApprove` (`src/lib.rs`) — the `ToolGate` that permits everything.
- `ToolFactory` (`src/subagent.rs`, re-exported at the crate root) —
  `fn add_tools(&self, agent: &mut Agent)`.
- `SubagentNotifier` / `SubagentConnector` (`src/subagent.rs`; referenced as
  `llimorse_tools::subagent::…`, not re-exported at the crate root) — the hooks a
  harness implements to observe and render subagents.
- All `tool!`-generated params/result structs are public (e.g. `ViewParams`,
  `BashResult`); their field names, types, and doc comments are the LLM-facing JSON
  schema.

## Fit in the workspace

- Depends on `llimorse`: every tool is declared with `llimorse::tool!` and implements
  `CallableTool`; `Subagent` creates a `Client`/`Agent` per call, takes the model name
  from the parent's `ClientState`, and forwards `StreamingChunk`s; uses
  `line_format::{ChatMessage, ToolCall}` for forked history.
- Depends on `helpers`: `TruncatedDisplay` in the `Display` impls of the file and bash
  params/results (100-char cutoff).
- External deps: `anyhow`, `futures`, `reqwest`, `schemars`, `serde`, `serde_json`,
  `tokio`, `urlencoding`, `libc` (unix only).
- `lemon` — the main consumer: adds `View`/`Write`/`Edit`/`Bash` (gated with
  `AutoApprove` or `llimorse_chat::UserToolGate`), `WebSearch`, and `Subagent`;
  implements `ToolFactory` (`LemonToolFactory`) so subagents get the same tool set.
- `llimorse-chat` — implements `ToolGate` (`UserToolGate`, approval round-trip through
  the UI) and the `subagent` `SubagentNotifier`/`SubagentConnector` traits; its
  implementations are what `lemon` passes into this crate's tools.
- `work-buddy` — uses only `WebSearch::new`.

## Invariants

- Every tool is declared with `llimorse::tool!`: the macro owns the params/result/state
  structs and the `Tool`/`ToolState` impls; this crate owns the `CallableTool` impl and
  the `Display` impls. `'params` fields must stay named so the schema is a JSON object.
- The tool names (`"view"`, `"write"`, `"edit"`, `"bash"`, `"web_search"`,
  `"subagent"`) are the identifiers the LLM invokes; renaming them changes the
  model-facing interface.
- Gated tools must consult the gate before doing anything, and a rejection must surface
  as an `Err` from `execute` (wrapped as `"<tool> tool call rejected: …"`): `llimorse`
  turns tool-call errors into results the model can read.
- Params `Display` impls are user-facing approval-prompt text (rendered by
  `llimorse-chat`'s `UserToolGate`); keep them compact and truncate long content with
  `helpers::TruncatedDisplay`.
- The `Bash` child must stay non-interactive (new session, null stdin, prompt
  suppression, `kill_on_drop`): a prompt would hang the agent loop, and `kill_on_drop`
  is what lets an interrupted tool call kill the command.
- `View` ranges are 1-based and inclusive; `Edit` must fail on zero or multiple
  `old_content` matches.
- `Subagent` holds a semaphore permit for the whole subagent run, and in fork mode it
  strips tool calls from the parent's last assistant message and passes the system
  prompt inside a user message, because not all chat templates accept system messages
  mid-conversation.
- There are no tests and no `missing_docs` lint in this crate, but doc comments on
  public items are load-bearing: the `tool!` doc comments are the LLM-facing tool
  descriptions.

## Maintenance

- If you add a tool: declare it with `tool!`, implement `CallableTool` plus `Display`
  for params and result, and re-export the state type from `src/lib.rs`.
- If you change a tool's params fields: the LLM-facing JSON schema changes; check the
  harness system prompts and the approval-prompt text.
- If you change `ToolGate`/`GateableToolParams`: `llimorse-chat`'s `UserToolGate`
  implements them.
- If you change `ToolFactory`, `SubagentNotifier`, or `SubagentConnector`:
  `llimorse-chat` (subagent UI wiring) and `lemon` (tool factory) implement them.
- If you add, remove, or rename a public type: update the `src/lib.rs` re-exports and
  the Files/Key APIs sections of this file.
