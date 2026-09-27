# llimorse

## Purpose

The central crate of the workspace: an LLM agent harness that talks to llama.cpp's
llama-server through its OpenAI-compatible chat completions API, using SSE streaming
and native (Jinja template) tool calling. It provides the HTTP client, the agent loop
(chat history, tool registration and execution), the `tool!` macro for defining tools,
and the shared observable state that UIs render from. Every other agent crate in this
workspace (llimorse-chat, llimorse-tools, term-ui, work-buddy, lemon) is built on it.

## Files

- `src/lib.rs` — crate root; public re-exports (`Agent`, `CallableTool`, `ChatListener`,
  `Client`, `StreamingChunk`) and the `tool!` macro (with the private `__tool_struct!`
  helper for re-emitting struct bodies).
- `src/client.rs` — `Client`: connects to llama-server, resolves the model via
  `/v1/models`, POSTs chat requests with bounded retry on transient errors;
  `ClientInfo`: immutable client information (model name, context size);
  `ClientState`: the `Arc<RwLock<…>>`-shared state (`TokenUsage`, `AgentStage`,
  including prefill progress) that UIs observe; `TokenUsage`.
- `src/agent.rs` — `Agent` (chat history, tool registry, pending tool calls); the
  `Tool`, `ToolState` and `CallableTool` traits; `ChatListener`; `AgentStage`;
  `AgentRunning`/`AgentResponse` (the streaming request futures); `DisplayCall` and
  `DisplayCallResult` for human-readable formatting of calls and results.
- `src/line_format.rs` — serde types for the wire format: `ChatMessage` (with
  `SystemMessage`, `UserMessage`, `AssistantMessage`, `ToolResult`), `ToolCall`,
  `ToolCallParams`, `FunctionCall`, `CustomCall`, `ToolDefinition`,
  `FunctionDefinition`, `ToolChoice`/`ToolChoiceMode`/`ToolChoiceSet`, the
  `ChatCompletion` request, and the `/v1/models` types (`Models`, `Model`).
- `src/prefill_instructions.rs` — `Prefill` trait and `PrefillInstructions`: loads a
  JSON file of instructions that seed the chat context (push inline or file-based
  messages, execute a tool call).
- `src/streaming_result.rs` — `StreamingResult`: parses the SSE byte stream (`data:`
  lines, `[DONE]`, the `usage` chunk, and llama.cpp's `prompt_progress` extension),
  accumulates deltas into a full `AssistantMessage`, and emits
  `StreamingChunk::Content`/`StreamingChunk::Reasoning`; contains the crate's unit
  tests.

## Key APIs

- `Client::new(base_url, model_name)` — the only constructor; picks the model by id
  or alias, or the single model, or `"default"`.
- `Agent` — `new` / `new_with_listener(listener)`; `push_user`, `push_system`,
  `push_tool_call`, `push_history`, `add_tool(state)`; `submit()` returning
  `AgentRunning`; `execute_pending_calls(call_guard, result_guard)`; `history()`,
  `last_result()`, `display_call()`, `display_call_result()`, `client_state()`.
- `AgentRunning` — a `Stream` of `Result<StreamingChunk>` that, on completion, pushes
  the final message into the history and queues any tool calls; `force_finalize()`
  (user abort), `execute_pending_calls()`, `full_response() -> AgentResponse`,
  `agent()`.
- `tool!` macro — generates the params/result/state structs and implements `Tool`
  and `ToolState` for the state struct; tool authors implement `CallableTool::execute`.
- `ChatListener` — every history mutation is logged through this (the hook for
  session logging); `()` implements it as a no-op.
- `AgentStage`, `ClientState`, `ClientInfo`, `TokenUsage` — the observable state
  for UIs (spinner stage, prefill progress, token counts, model info), shared as
  `Arc<parking_lot::RwLock<ClientState>>` and read from other threads.
- `line_format::ChatMessage` — the canonical message type; also the persistence
  format of session logs.

## Fit in the workspace

- Dependencies: no workspace crates; only external ones (reqwest, tokio, futures,
  serde, serde_json, schemars, bytemuck, parking_lot, bytes, pin-project, anyhow,
  tracing).
- llimorse-chat — runs the agent loop: `Agent::submit`, stream `StreamingChunk`s,
  `execute_pending_calls`; its session log implements `ChatListener`.
- llimorse-tools — defines the standard tools (`Bash`, `View`, `Write`, `Edit`,
  `WebSearch`, `Subagent`) via `llimorse::tool!` + `CallableTool`; the subagent tool
  spawns a fresh `Client`/`Agent` per call.
- term-ui — renders the agent state by reading `AgentStage`, `ClientState` and
  `TokenUsage` from the shared `Arc`.
- work-buddy — example app that builds a `Client` + `Agent::new_with_listener`,
  adds tools, pushes the system prompt, and hands the agent to llimorse-chat.
- lemon — the agent harness app; same construction pattern (plus a subagent
  `ToolFactory`).
- helpers — no relationship; it does not depend on llimorse.

## Invariants

- Every history change goes through the `ChatListener`; `ChatHistory` is the only
  route. Do not add a way to mutate history that bypasses it.
- `line_format` types are both the wire format and a persistence format: session
  logs are `ChatMessage` values serialized to JSONL. Renaming fields or changing
  serde tags breaks stored sessions; add fields with `#[serde(default)]`.
- Tool errors are reported back to the LLM as content strings (`TOOL CALL FAILED: …`,
  `TOOL CALL REJECTED: …`), and `execute_pending_calls` answers every `ToolCall` with
  a result so the history never ends in an unanswered call.
- `AgentRunning`/`AgentResponse` hold a mutable borrow of the `Agent`;
  `execute_pending_calls` on them panics unless the stream has terminated. Callers
  must drop the stream first (or use the wrapper method).
- `chat_stream` always sends `return_progress: true` and `include_usage: true`, and
  resets `streamed_tokens` per request; the streaming state machine relies on the
  `usage` chunk and `[DONE]` arriving.
- `ClientState` is shared as `Arc<parking_lot::RwLock<ClientState>>` (the
  immutable `ClientInfo` travels separately): mutate it under the write lock,
  read it under the read lock — UI threads access it concurrently.
- `AgentStage` transitions are a UI contract: `Prefill` at submit,
  `Reasoning`/`ResponseGeneration`/`ToolCallGeneration` while streaming,
  `ToolExecution` during calls, `Idle` after completion or any error.
- `tool!` contract: the `'params` struct must have a braced body so its schema is a
  JSON object; prefer `{}` over unit structs for empty params/results (unit structs
  serialize as `null`); the macro owns the `Tool`/`ToolState` impls, users own
  `CallableTool`.
- Aborting a request (`force_finalize`) discards in-flight tool calls entirely; a
  stream that ends without a complete message is an error, not partial data.
- The crate only supports llama-server. Do not assume other providers.
- `ChatMessage::is_empty` deliberately never treats tool results as empty.
- Docs are enforced: `missing_docs` warnings are on, including for private items.

## Maintenance

- If you change `line_format` types, check session-log compatibility (llimorse-chat
  serializes them) and the wire format.
- If you change `Agent`/`AgentRunning` methods or the `ChatListener` trait, update
  llimorse-chat (agent loop, log, history), llimorse-tools (subagent) and term-ui.
- If you change `ClientState`, `TokenUsage` or `AgentStage`, check term-ui's
  spinner and progress display.
- If you change `StreamingChunk` variants, check llimorse-chat and llimorse-tools.
- If you change the `tool!` macro's grammar, check all tools in llimorse-tools and
  work-buddy.
- If you add or remove `AgentStage` variants, update term-ui's emoji mapping.
