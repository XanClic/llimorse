# term-ui

## Purpose

A ratatui-based terminal UI for llimorse-chat applications. `TermUi` implements
`llimorse_chat::ui::UiState` and renders the main agent and its subagents (each with its own chat
history, fed by `AgentUpdate` notifications, and rendered with lightweight markdown styling), the
message input area, and tool-permission popups. It is used by the two example apps, work-buddy and
lemon, which construct it via `TermUi::new(app_name, agent)`.

## Files

- `Cargo.toml` — dependencies: ratatui 0.30 (feature `unstable-rendered-line-info`),
  ratatui-textarea 0.9, crossterm 0.29 (feature `event-stream`), textwrap 0.16, unicode-width,
  unicode-segmentation, color-eyre, anyhow, futures, parking_lot, tokio (sync); workspace crates
  `helpers`, `llimorse`, `llimorse-chat`. No dev-dependencies.
- `src/lib.rs` — the whole crate: `TermUi` (implements `UiState`), `AgentStatsDisplay`, the
  private chat-history model (`ChatHistory`, `ChatHistoryContent`, `ChatBlockContent` with its
  finalized/active line split, `FinalLine`, `ActiveLine`, `Token`), the wrap cache
  (`WrappedCache`) and `ScrollPosition` for display-line scrolling, the private per-agent view
  state (`UiAgents`, `AgentState`, `SubagentState`), terminal set-up/tear-down, OSC 99
  notifications, and the tests.
- `src/markdown.rs` — `Parser`: an incremental (block-at-a-time) markdown parser that emits styled
  text fragments (`Effect`, `MarkdownStyle`) for the history pane.
- `src/wrap.rs` — `wrapped_line_count`: a simplified vendored copy of ratatui-textarea v0.9.2’s
  wrap algorithm (MIT), used to count the on-screen rows of the input `TextArea`, which the widget
  does not expose.

## Key APIs

- `TermUi` (lib.rs) — `new(app_name, agent)` and `draw()`; implements
  `llimorse_chat::ui::UiState`, the load-bearing integration point with `llimorse_chat::App`.
- `AgentStatsDisplay` (lib.rs) — `Display` impl for the history pane title: model name, animated
  stage emoji, and context usage including live prefill progress.

## Fit in the workspace

- `llimorse-chat` (dependency): implements `ui::UiState`; consumes `ui::{Event, Notification,
  AgentUpdate, AgentId, SubagentId}`.
- `llimorse` (dependency): uses `Agent`, `client::{ClientInfo, ClientState}`, and
  `agent::AgentStage` (`is_processing`; the stage emoji mapping is term-ui’s own).
- `helpers` (dependency): uses the `TruncatedDisplay` trait.
- work-buddy (consumer): builds `TermUi::new("WorkBuddy", agent)` in the create-UI closure of
  `llimorse_chat::App::new_with_history` in `src/main.rs`.
- lemon (consumer): same pattern, `TermUi::new("Lemon", agent)`.
- Not used by `llimorse`, `llimorse-chat`, `llimorse-tools`, or `helpers`.

## Invariants

- All rendering is inlined in `TermUi`: event handling, agent and scroll state, input editing,
  notifications, terminal set-up/tear-down, and OSC 99 notifications live there. There is no
  theme/plugin layer.
- The permission popup is rendered by `TermUi::render` after the main layout — it can never be
  hidden or omitted.
- Scrolling is by *display lines*: `WrappedCache` maps history lines to wrapped rows,
  `ScrollPosition::{Follow, Anchor}` tracks the view, PgUp/PgDn step by half the pane height in
  rows, and anchors survive terminal width changes (the tests pin this).
- Chat blocks are separated by blank display lines, and a block’s lines are split into finalized
  (stable, cacheable) and active (growing) at the last blank line. The history is append-only, so
  the wrap cache normally re-wraps only the last and new lines; a full re-wrap is flagged dirty
  when a finalization or a new block rewrites already-final lines, and on pane-width changes (the
  tests pin the split, the separation, and the dirty flag).
- `TermUi.term` is an `Option<DefaultTerminal>` because ratatui’s draw callback needs ownership;
  `draw()` takes it out and puts it back.
- The input `TextArea` is not replaceable: editing and key handling stay in `TermUi`.
- Input is disabled in subagent views: Enter is ignored (only Shift+PgUp/PgDn switch agents) and
  paste is dropped.
- Permission requests are modal: Enter approves, Esc denies, all other keys are ignored; Ctrl-C is
  a hard exit even while a popup is on screen.
- OSC 99 notifications sanitize their payload (newlines, tabs, semicolons, control characters) and
  truncate title and body to 200 characters; under tmux (detected via `$TERM`) the sequence is
  wrapped in tmux’s DCS passthrough envelope (requires `allow-passthrough on`).
- `wrap.rs` is a vendored copy of ratatui-textarea v0.9.2’s wrap algorithm; keep it in sync with
  the widget so `wrapped_line_count` matches what the `TextArea` actually renders. History lines
  are wrapped with the `textwrap` crate instead; both must agree with what is drawn.
- The crate warns on `missing_docs` and `clippy::missing_docs_in_private_items`: every item,
  private ones included, needs a doc comment.
- `TERM_SET_UP` refcounts terminal set-up; `TermUi::new` and `Drop` install/restore the
  color-eyre handler, bracketed paste, and mouse capture exactly once.

## Maintenance

- If you change the layout, styles, or the permission popup: the tests in `src/lib.rs`
  (`popup_is_centered_and_opaque`, `popup_does_not_panic_on_small_terminals`, and the
  history-rendering tests) assert exact screen content and must be updated.
- If you bump ratatui-textarea: re-verify the vendored `wrap.rs` against the new version and
  update its provenance note.
- If you change the signature of `TermUi::new`: update the create-UI closures in work-buddy and
  lemon’s `src/main.rs`.
