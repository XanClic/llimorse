//! UI part of the WorkBuddy application

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod wrap;

use anyhow::Result;
use crossterm::event as ct;
use futures::StreamExt;
use helpers::TruncatedDisplay;
use llimorse::Agent;
use llimorse::agent::AgentStage;
use llimorse::client::{ClientInfo, ClientState};
use llimorse_chat::ui::{self, AgentId, AgentUpdate, SubagentId};
use parking_lot::RwLock;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::num::NonZero;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use std::{env, fmt, io};
use tokio::sync::oneshot;

/// Counts users of the ratatui terminal (honestly only should be one or none...)
static TERM_SET_UP: AtomicUsize = AtomicUsize::new(0);

/// UI state for WorkBuddy
pub struct TermUi {
    /// ratatui terminal object
    ///
    /// Always set, except during rendering, because for some reason ratatui needs ownership access
    /// to this. (Well, “for some reason” is that it found it clever to have a `term.do(|x| ...)`
    /// pattern for rendering, so if we want access to `self` in that callback, we cannot have
    /// `term` stored here.)
    term: Option<DefaultTerminal>,

    /// Produces terminal events, asynchronously
    events: ct::EventStream,

    /// All agents currently running
    agents: UiAgents,

    /// User message input widget
    input_area: ratatui_textarea::TextArea<'static>,

    /// Messages that are queued for sending
    queued_prompts: VecDeque<String>,

    /// Whether a prompt iteration is currently being processed (from `PromptSubmitted`
    /// until `AwaitingPrompt`); the input title is not bolded while processing
    processing: bool,

    /// Tool calls awaiting a permission decision from the user, oldest first.
    ///
    /// Only the first one is shown; the rest wait their turn.
    pending_permissions: VecDeque<(String, oneshot::Sender<std::result::Result<(), String>>)>,

    /// The application name to use e.g. for notifications
    app_name: String,

    /// When the object was created, purely for visual purposes
    creation: Instant,
}

/// Data for the agents currently running
struct UiAgents {
    /// State of the main agent and subagents
    state: Vec<AgentState>,

    /// Which agent is being viewed (0 = main agent)
    active_agent: usize,
}

/// Overall state for an agent (main agent or subagent), including its view
struct AgentState {
    /// Immutable client information
    client_info: ClientInfo,

    /// The current general state of the agent’s client
    client_state: Arc<RwLock<ClientState>>,

    /// The chat history
    history: ChatHistory,

    /// If a subagent: Additional information about it
    subagent_state: Option<SubagentState>,
}

/// Additional information about subagents
struct SubagentState {
    /// The ID by which the subagent is identified in UI notifications
    id: SubagentId,

    /// The task given to this agent
    task: String,
}

/// The scroll position of an agent’s view
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollPosition {
    /// Follow the tail of the history
    Follow,

    /// The character at the given logical line and column starts the top row
    /// of the view
    Anchor {
        /// The index of the logical (unwrapped) line
        line: usize,

        /// The offset of the character within that line
        col: usize,
    },
}

/// The wrapped-line counts of a chat history, cached for a width
#[derive(Debug, Default)]
struct WrappedCache {
    /// The width the counts were computed for
    width: usize,

    /// The number of wrapped lines each logical line produces
    line_lengths: Vec<usize>,

    /// The total number of wrapped lines
    total: usize,
}

impl WrappedCache {
    /// (Re)compute the wrapped-line counts of `history` for `width`, reusing the previous counts
    /// where the lines have not changed.
    ///
    /// The history is append-only, and only its last logical line can change content (streaming),
    /// so only the last existing line and any new lines need to be (re)wrapped.
    fn update(&mut self, history: &ChatHistoryContent, width: usize) {
        if self.width != width {
            self.width = width;
            self.line_lengths = history
                .logical_lines()
                .map(|(line, _)| textwrap::wrap(line, width).len())
                .collect();
            self.total = self.line_lengths.iter().sum();
            return;
        }

        let count = history.logical_line_count();
        if count == 0 {
            self.line_lengths.clear();
            self.total = 0;
            return;
        }

        if count < self.line_lengths.len() {
            // The history shrank, which cannot happen; drop the stale counts
            self.line_lengths.truncate(count);
        }

        // The last line may have grown (a streaming chunk)
        let last = count - 1;
        if last < self.line_lengths.len() {
            let line = history.logical_line(last).expect("line exists");
            self.line_lengths[last] = textwrap::wrap(line, width).len();
        }

        for i in self.line_lengths.len()..count {
            let line = history.logical_line(i).expect("line exists");
            self.line_lengths.push(textwrap::wrap(line, width).len());
        }

        self.total = self.line_lengths.iter().sum();
    }

    /// Return the index of the wrapped line that contains the character at the given logical
    /// `line` and `col`.
    fn row_of(&self, history: &ChatHistoryContent, line: usize, col: usize) -> usize {
        let line = line.min(self.line_lengths.len().saturating_sub(1));

        let mut row = 0;
        for &len in &self.line_lengths[..line] {
            row += len;
        }

        let logical = history.logical_line(line).expect("line exists");
        let starts = fragment_starts(logical, &textwrap::wrap(logical, self.width));
        let frag = starts.iter().rposition(|&start| start <= col).unwrap_or(0);
        row + frag
    }

    /// Return the scroll position that puts the top row of the view at wrapped line `row`.
    fn anchor_at_row(&self, history: &ChatHistoryContent, row: usize) -> ScrollPosition {
        if self.total == 0 {
            return ScrollPosition::Follow;
        }

        let row = row.min(self.total - 1);
        let mut acc = 0;
        for (line, &len) in self.line_lengths.iter().enumerate() {
            if row < acc + len {
                let logical = history.logical_line(line).expect("line exists");
                let starts = fragment_starts(logical, &textwrap::wrap(logical, self.width));
                let col = starts[row - acc];
                return ScrollPosition::Anchor { line, col };
            }
            acc += len;
        }

        // Unreachable: row < total
        ScrollPosition::Follow
    }
}

/// Return the offset of each wrapped fragment within the original line.
///
/// `fragments` is the output of `textwrap::wrap(line, width)`. The fragments
/// are in order and separated by the whitespace textwrap dropped at the wrap
/// points, so the offsets are recovered by walking the line.
fn fragment_starts(line: &str, fragments: &[Cow<'_, str>]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(fragments.len());
    let mut pos = 0;

    for fragment in fragments {
        while pos < line.len() {
            let c = line[pos..].chars().next().unwrap();
            if c.is_whitespace() {
                pos += c.len_utf8();
            } else {
                break;
            }
        }
        starts.push(pos);
        pos += fragment.len();
    }

    starts
}

/// Chat history data, and view
#[derive(Debug)]
struct ChatHistory {
    /// Full chat history
    content: ChatHistoryContent,

    /// The scroll position of the view
    scroll: ScrollPosition,

    /// The width in terminal character blocks (from this or the last render)
    pane_width: NonZero<usize>,

    /// The number of rows the history pane (from this or the last render)
    pane_height: NonZero<usize>,

    /// The wrapped-line counts of the history, for the last render’s width
    cache: WrappedCache,
}

/// Chat history data
#[derive(Debug, Default)]
struct ChatHistoryContent(Vec<ChatBlock>);

/// A block in the chat with a single semantic
#[derive(Debug)]
enum ChatBlock {
    /// A user prompt
    User(ChatBlockContent),

    /// Assistant reply (actual content)
    Content(ChatBlockContent),

    /// Reasoning output
    Reasoning(ChatBlockContent),

    /// A tool call
    ToolCall(ChatBlockContent),

    /// A tool call result (on success)
    ToolResultOk(ChatBlockContent),

    /// A tool call result (on error)
    ToolResultErr(ChatBlockContent),
}

/// The content of a block in the chat with some semantic
#[derive(Debug, Default)]
struct ChatBlockContent {
    /// The actual lines of content
    lines: Vec<String>,

    /// Trailing whitespace, tracked separately because we do not want to show it
    trailing_whitespace: String,
}

impl TermUi {
    /// Create the term state for `agent`, with the application name `app_name` (e.g. for
    /// notifications).
    pub fn new(app_name: &str, agent: &Agent) -> Self {
        let term = ratatui::init();
        set_up_term();

        let mut input_area: ratatui_textarea::TextArea<'static> = Default::default();
        input_area.set_cursor_line_style(Default::default());
        input_area.set_wrap_mode(ratatui_textarea::WrapMode::Word);

        TermUi {
            term: Some(term),
            events: ct::EventStream::new(),
            agents: UiAgents::new(agent.client_state_arc(), agent.client_info().clone()),
            input_area,
            queued_prompts: VecDeque::new(),
            processing: false,
            pending_permissions: VecDeque::new(),
            app_name: app_name.to_string(),
            creation: Instant::now(),
        }
    }

    /// Render the current state to screen
    pub fn draw(&mut self) -> Result<()> {
        // I really hate libraries/crates that thing they need to take ownership of everything via
        // callbacks or prescribing traits/interfaces
        let mut term = self.term.take().unwrap();
        term.draw(|frame| self.render(frame))?;
        self.term = Some(term);

        Ok(())
    }

    /// Scroll the active view up by the given number of screen rows.
    fn scroll_up(&mut self, lines: usize) {
        self.agents.active_mut().history.scroll_up(lines);
    }

    /// Scroll the active view down by the given number of screen rows.
    fn scroll_down(&mut self, lines: usize) {
        self.agents.active_mut().history.scroll_down(lines);
    }

    /// Handle the given keyboard event.
    fn handle_key_event(&mut self, event: ct::KeyEvent) -> Result<Option<ui::Event>> {
        // Ctrl-C is a hard exit, even while a permission request is on screen.
        if event.kind == ct::KeyEventKind::Press
            && event.code == ct::KeyCode::Char('c')
            && event.modifiers.contains(ct::KeyModifiers::CONTROL)
        {
            return Ok(Some(ui::Event::Exit));
        }

        if event.kind == ct::KeyEventKind::Press && !self.pending_permissions.is_empty() {
            // A permission request is on screen: it is modal. Enter approves, Esc denies, all
            // other keys are ignored.
            let decision: std::result::Result<(), String> = match event.code {
                ct::KeyCode::Enter if event.modifiers.is_empty() => Ok(()),
                ct::KeyCode::Esc => Err("denied by user".to_string()),
                _ => return Ok(None),
            };
            let (_, approval) = self.pending_permissions.pop_front().unwrap();
            let _ = approval.send(decision);
            return Ok(None);
        }

        if event.kind == ct::KeyEventKind::Press {
            match event.code {
                ct::KeyCode::Enter if event.modifiers.is_empty() => {
                    if self.agents.is_main() {
                        if !self.input_area.is_empty() {
                            let message = self.input_area.lines().join("\n");
                            self.input_area.clear();
                            return Ok(Some(ui::Event::Input(message)));
                        } else if !self.queued_prompts.is_empty() {
                            return Ok(Some(ui::Event::ForceSubmitQueued));
                        } else {
                            // Empty input: submit the current state. The agent thread ignores this
                            // unless the history tops out on a tool result or a user message.
                            return Ok(Some(ui::Event::Continue));
                        }
                    } else {
                        // Do not do anything unless we’re in the main agent view
                        return Ok(None);
                    }
                }

                ct::KeyCode::PageUp => {
                    if event.modifiers.contains(ct::KeyModifiers::SHIFT) {
                        self.agents.switch_prev();
                    } else {
                        let pane_height = self.agents.active().history.pane_height.get();
                        self.scroll_up(pane_height.div_ceil(2));
                    }
                    return Ok(None);
                }
                ct::KeyCode::PageDown => {
                    if event.modifiers.contains(ct::KeyModifiers::SHIFT) {
                        self.agents.switch_next();
                    } else {
                        let pane_height = self.agents.active().history.pane_height.get();
                        self.scroll_down(pane_height.div_ceil(2));
                    }
                    return Ok(None);
                }

                _ => (),
            }
        }

        if self.agents.is_main() {
            self.input_area.input(event);
        }
        Ok(None)
    }

    /// Handle the given mouse event.
    fn handle_mouse_event(&mut self, event: ct::MouseEvent) -> Result<Option<ui::Event>> {
        match event.kind {
            ct::MouseEventKind::ScrollDown => self.scroll_down(1),
            ct::MouseEventKind::ScrollUp => self.scroll_up(1),

            _ => (),
        }

        Ok(None)
    }

    /// Handle clipboard pasting.
    fn handle_paste_event(&mut self, text: String) -> Result<Option<ui::Event>> {
        // Ignore pasting in subagent views
        if self.agents.is_main() {
            // Normalize line endings: `ratatui_textarea::TextArea::insert_str()` does not handle
            // \r.
            let text = text.replace("\r\n", "\n").replace("\r", "\n");
            self.input_area.insert_str(&text);
        }

        Ok(None)
    }

    /// The input field’s block: the title is bolded while the agent awaits a prompt, and not
    /// bolded while a request is being processed.
    fn input_block(processing: bool) -> Block<'static> {
        let input_title_style = if processing {
            Style::default()
        } else {
            Style::default().bold()
        };
        Block::bordered()
            .title(Line::from(" Input ").style(input_title_style))
            .title_bottom(Line::from(" [Ctrl-C to quit] ").right_aligned())
    }

    /// Send a terminal (OSC 99) notification with the given title and optional body.
    ///
    /// Under tmux (detected via `$TERM`) the OSC 99 messages are wrapped in tmux’s
    /// passthrough envelope (which requires `allow-passthrough on`); otherwise the
    /// messages are sent bare.
    ///
    /// The title and body are trimmed, and newlines, tabs, semicolons and other ASCII
    /// control characters are replaced or dropped, since they would corrupt the OSC 99
    /// payload; both are truncated to 200 characters.
    fn send_osc99_notification(&self, title: &str, body: Option<&str>) {
        /// Strip characters that would corrupt an OSC 99 payload.
        fn sanitize(text: &str) -> String {
            text.trim()
                .chars()
                .filter_map(|c| match c {
                    '\n' | '\t' => Some(' '),
                    '\r' => None,
                    ';' => Some(','),
                    c if c.is_ascii_control() => Some('�'),
                    c => Some(c),
                })
                .collect()
        }

        // Without the leading ESC and the ST terminator: the bare path and the tmux
        // wrapper both supply them.
        let title_payload = format!(
            "]99;i=1:d=0;{}: {}",
            sanitize(&self.app_name),
            sanitize(title).truncated_display(200)
        );
        let body_payload = match body {
            Some(body) => {
                format!(
                    "]99;i=1:d=1:p=body;{}",
                    sanitize(body).truncated_display(200)
                )
            }
            None => String::from("]99;i=1:d=1:p=body;(No body)"),
        };

        let seq = if env::var("TERM").is_ok_and(|term| term.starts_with("tmux")) {
            // Each message wrapped in tmux’s DCS passthrough envelope: the OSC’s leading
            // ESC is doubled, its ST is spelled as `ESC ESC \`, and the envelope is closed
            // with `ESC \`.
            let title_env =
                format!("\u{1b}Ptmux;\u{1b}\u{1b}{title_payload}\u{1b}\u{1b}\\\u{1b}\\");
            let body_env = format!("\u{1b}Ptmux;\u{1b}\u{1b}{body_payload}\u{1b}\u{1b}\\\u{1b}\\");
            format!("{title_env}{body_env}")
        } else {
            format!("\u{1b}{title_payload}\u{1b}\\\u{1b}{body_payload}\u{1b}\\")
        };

        let _ = crossterm::execute!(io::stdout(), crossterm::style::Print(seq));
    }

    /// Raise a terminal notification that the response is ready and the agent is awaiting a
    /// new prompt.
    fn notify_prompt_done(&self, response: Option<String>) {
        let body = response.as_deref().unwrap_or("(Awaiting prompt.)");
        self.send_osc99_notification("Turn done, awaiting prompt", Some(body));
    }

    /// Render the current state onto the screen.
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let input_outer_height = if self.agents.is_main() {
            // Input height field: Number of lines, maximum 5. Note that `.lines()` is always
            // guaranteed to at least return one (empty) line, and `line_ranges` likewise always
            // yields at least one row per line.  The `TextArea` does not expose its on-screen row
            // count, so count rows with the same wrapping algorithm the widget renders with
            // (vendored in `wrap`).
            const MAX_HEIGHT: usize = 5;
            let input_inner_width = area.width.saturating_sub(2) as usize; // account for the border
            let input_inner_height = self
                .input_area
                .lines()
                .iter()
                .take(MAX_HEIGHT)
                .map(|line| {
                    wrap::wrapped_line_count(line, self.input_area.wrap_mode(), input_inner_width)
                })
                .sum::<usize>()
                .min(MAX_HEIGHT) as u16;
            Some(input_inner_height + 2) // account for the border
        } else {
            None
        };

        let mut layout = Vec::with_capacity(self.queued_prompts.len() + 2);
        layout.push(Constraint::Percentage(100));
        if let Some(input_outer_height) = input_outer_height {
            for _ in 0..self.queued_prompts.len() {
                layout.push(Constraint::Min(1));
            }
            layout.push(Constraint::Min(input_outer_height));
        }

        let layout = Layout::vertical(layout).split(area);

        let history_cell = layout[0];
        let input_cell = input_outer_height.map(|_| layout[self.queued_prompts.len() + 1]);

        let subagent_count = self.agents.subagent_count();
        let title_bottom = if let Some(subagent_i) = self.agents.subagent_index() {
            let subagent_no = subagent_i + 1;
            let title = if let Some(subagent_task) = self.agents.subagent_task() {
                format!(
                    " Subagent {subagent_no}/{subagent_count}: {} [Shift+PgDn/PgUp] ",
                    subagent_task.truncated_display(40),
                )
            } else {
                format!(" Subagent {subagent_no}/{subagent_count} [Shift+PgDn/PgUp] ")
            };
            Some(title)
        } else if subagent_count > 0 {
            Some(format!(" {subagent_count} subagents [Shift+PgDn/PgUp] "))
        } else {
            None
        };

        let agent = self.agents.active_mut();

        // Account for the border
        agent.history.set_layout(
            history_cell.width.saturating_sub(2) as usize,
            history_cell.height.saturating_sub(2) as usize,
        );
        let history_lines = agent.history.render();

        let paragraph_content = Text {
            alignment: None,
            style: Default::default(),
            lines: history_lines,
        };

        let mut chat_block = Block::bordered()
            .title_style(Style::new().bold())
            .title(format!(" {} ", agent.stats(self.creation)));
        if let Some(title_bottom) = title_bottom {
            chat_block = chat_block.title_bottom(Line::from(title_bottom).right_aligned());
        }
        let paragraph = Paragraph::new(paragraph_content).block(chat_block);

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let mut scrollbar_state = agent.history.scrollbar_state();

        frame.render_widget(paragraph, history_cell);
        frame.render_stateful_widget(
            scrollbar,
            history_cell.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
        if let Some(input_cell) = input_cell {
            for (i, p) in self.queued_prompts.iter().enumerate() {
                let line = Line {
                    style: Style::new().white().on_blue(),
                    alignment: None,
                    spans: vec![Span {
                        style: Default::default(),
                        content: p.into(),
                    }],
                };
                frame.render_widget(line, layout[i + 1]);
            }
            self.input_area
                .set_block(Self::input_block(self.processing));
            frame.render_widget(&self.input_area, input_cell);
        }

        // Render the permission popup last, so that it occludes the main layout.
        if let Some((prompt, _)) = self.pending_permissions.front() {
            TermUi::render_permission_popup(frame, area, prompt);
        }
    }

    /// Render the given permission request as a popup centered on the screen, on top of the main
    /// layout.
    ///
    /// The popup is 80% of the terminal’s width (at most) and as tall as its wrapped prompt
    /// requires (at most the terminal’s height), and clears the area it covers, so that nothing
    /// from the main layout shows through.
    fn render_permission_popup(frame: &mut Frame, area: Rect, prompt: &str) {
        let width = (area.width.saturating_mul(8) / 10).max(4).min(area.width);
        let inner_width = width.saturating_sub(2).max(1); // account for the border

        let block = Block::bordered()
            .border_style(Style::new().yellow())
            .border_type(BorderType::Thick)
            .title(" Tool permission requested ")
            .title_style(Style::new().white().bold())
            .title_bottom(Line::from(" [Enter: allow]  [Esc: deny] ").right_aligned())
            .padding(Padding::proportional(1));
        let paragraph = Paragraph::new(prompt)
            .wrap(Wrap { trim: true })
            .block(block);
        let height = paragraph
            .line_count(inner_width)
            .min(area.height as usize)
            .max(1) as u16;

        let popup_area = area.centered(Constraint::Length(width), Constraint::Length(height));
        frame.render_widget(Clear, popup_area); // clear what is underneath
        frame.render_widget(paragraph, popup_area);
    }
}

impl ui::UiState for TermUi {
    type Error = anyhow::Error;

    /// Handle input on the terminal, and redraw
    async fn get_event(&mut self) -> Result<ui::Event> {
        while let Some(result) = self.events.next().await {
            let event = match result? {
                ct::Event::Paste(text) => self.handle_paste_event(text),
                ct::Event::Key(key) => self.handle_key_event(key),
                ct::Event::Mouse(mouse) => self.handle_mouse_event(mouse),
                _ => Ok(None),
            };

            self.draw()?;

            if let Some(event) = event? {
                return Ok(event);
            }
        }

        // Stream ended
        Ok(ui::Event::Exit)
    }

    fn notify(&mut self, notification: ui::Notification) -> Result<()> {
        match notification {
            ui::Notification::Exit => (), // To be handled by the parent
            ui::Notification::Update => (),
            ui::Notification::AgentUpdate { agent_id, content } => {
                if matches!(
                    (agent_id, &content),
                    (AgentId::Main, AgentUpdate::User { prompt: _ })
                ) {
                    self.queued_prompts.pop_front();
                    self.processing = true;
                }
                if let Some(agent) = self.agents.get_mut(&agent_id) {
                    agent.history.push(content);
                }
                // Skip the redraw if the updated agent is not the one on screen
                if agent_id != self.agents.active_agent_id() {
                    return Ok(());
                }
            }
            ui::Notification::PromptQueued(p) => {
                let sanitized = p
                    .chars()
                    .filter_map(|c| match c {
                        '\n' => Some('↵'),
                        '\t' => Some(' '),
                        '\r' => None,
                        c if c.is_ascii_control() => Some('�'),
                        c => Some(c),
                    })
                    .collect::<String>();
                self.queued_prompts.push_back(sanitized);
            }
            ui::Notification::AwaitingPrompt { response } => {
                self.processing = false;
                self.notify_prompt_done(response);
            }
            ui::Notification::RequestPermission { prompt, approval } => {
                // Alert the user out of band: the modal popup only reaches them while
                // they are viewing this agent.
                self.send_osc99_notification("Tool permission requested", Some(&prompt));
                self.pending_permissions.push_back((prompt, approval));
            }
            ui::Notification::SubagentCreated {
                subagent_id,
                prompt,
                client_info,
                client_state,
            } => {
                self.agents
                    .add_subagent(subagent_id, prompt, client_info, client_state);
            }
            ui::Notification::SubagentDropped { subagent_id } => {
                self.agents.remove_subagent(subagent_id);
            }
        }

        self.draw()
    }
}

impl Drop for TermUi {
    fn drop(&mut self) {
        tear_down_term();
    }
}

/// Basic terminal set-up
fn set_up_term() {
    if TERM_SET_UP.fetch_add(1, Ordering::Relaxed) == 0 {
        color_eyre::install().expect("Failed to install crash handler");
        let _ = crossterm::execute!(
            io::stdout(),
            ct::EnableBracketedPaste,
            ct::EnableMouseCapture
        );
    }
}

/// Basic terminal tear-down
fn tear_down_term() {
    if TERM_SET_UP.fetch_sub(1, Ordering::Relaxed) == 1 {
        let _ = crossterm::execute!(
            io::stdout(),
            ct::DisableBracketedPaste,
            ct::DisableMouseCapture
        );
        ratatui::restore();
    }
}

impl UiAgents {
    /// Create a new collection of agents, with a single main agent
    fn new(main_client_state: Arc<RwLock<ClientState>>, main_client_info: ClientInfo) -> Self {
        UiAgents {
            state: vec![AgentState {
                client_info: main_client_info,
                client_state: main_client_state,
                history: ChatHistory::new(),
                subagent_state: None,
            }],
            active_agent: 0,
        }
    }

    /// Add a new subagent at the end of our list
    fn add_subagent(
        &mut self,
        subagent_id: SubagentId,
        prompt: String,
        client_info: ClientInfo,
        client_state: Arc<RwLock<ClientState>>,
    ) {
        self.state.push(AgentState {
            client_info,
            client_state,
            history: ChatHistory::new(),
            subagent_state: Some(SubagentState {
                id: subagent_id,
                task: prompt,
            }),
        });
    }

    /// Remove a subagent by its ID
    ///
    /// If the subagent is currently active, change the active view to the main agent.
    fn remove_subagent(&mut self, subagent_id: SubagentId) {
        let Some(index) = self.state.iter().position(|agent| {
            agent
                .subagent_state
                .as_ref()
                .is_some_and(|state| state.id == subagent_id)
        }) else {
            return;
        };

        if self.active_agent == index {
            self.active_agent = 0;
        } else if self.active_agent > index {
            self.active_agent -= 1;
        }
        self.state.remove(index);
    }

    /// Return the currently active view’s agent
    fn active(&self) -> &AgentState {
        self.state.get(self.active_agent).expect("No agents left")
    }

    /// Return the currently active view’s agent, mutably
    fn active_mut(&mut self) -> &mut AgentState {
        self.state
            .get_mut(self.active_agent)
            .expect("No agents left")
    }

    /// Return whether the currently active view is the main agent’s
    fn is_main(&self) -> bool {
        self.active_agent == 0
    }

    /// Return the ID of the agent in the currently active view
    fn active_agent_id(&self) -> AgentId {
        self.active()
            .subagent_state
            .as_ref()
            .map_or(AgentId::Main, |state| AgentId::Subagent(state.id))
    }

    /// Return the subagent index, if viewing a subagent
    fn subagent_index(&self) -> Option<usize> {
        self.active_agent.checked_sub(1)
    }

    /// Return the number of subagents
    fn subagent_count(&self) -> usize {
        self.state.len().checked_sub(1).expect("No agents left")
    }

    /// If the currently active view is a subagent, return its task prompt (if known)
    fn subagent_task(&self) -> Option<&str> {
        self.active()
            .subagent_state
            .as_ref()
            .map(|state| -> &str { &state.task })
    }

    /// Switch to the previous agent (wrapping)
    fn switch_prev(&mut self) {
        self.active_agent = self
            .active_agent
            .checked_sub(1)
            .or_else(|| self.state.len().checked_sub(1))
            .expect("No agents left");
    }

    /// Switch to the next agent (wrapping)
    fn switch_next(&mut self) {
        if self.state.is_empty() {
            panic!("No agents left");
        }

        self.active_agent = self.active_agent.checked_add(1).unwrap_or(0);
        if self.active_agent >= self.state.len() {
            self.active_agent = 0;
        }
    }

    /// Return a mutable reference to the agent with `agent_id`, if any
    fn get_mut(&mut self, agent_id: &AgentId) -> Option<&mut AgentState> {
        match agent_id {
            AgentId::Main => self.state.get_mut(0),
            AgentId::Subagent(id) => self
                .state
                .iter_mut()
                .find(|agent| agent.subagent_state.as_ref().is_some_and(|s| s.id == *id)),
        }
    }
}

impl AgentState {
    /// Format the client-state statistics that appear in the history pane title: model name,
    /// operation stage, and context usage (including live prefill progress).
    ///
    /// `time_ref` is an arbitrary (but fixed) point in time so we can animate spinners.
    fn stats(&self, time_ref: Instant) -> impl fmt::Display {
        AgentStatsDisplay {
            state: &self.client_state,
            info: &self.client_info,
            time_ref,
        }
    }
}

/// Helper struct to display agent/client stats
struct AgentStatsDisplay<'a> {
    /// Client/agent state to display
    state: &'a RwLock<ClientState>,
    /// Client/agent info to display (immutable state)
    info: &'a ClientInfo,
    /// An arbitrary fixed point in time to animate spinners
    time_ref: Instant,
}

impl AgentStatsDisplay<'_> {
    /// Return a representative emoji of the current stage, animated
    ///
    /// Specifically the stages where there is no visible text generation in the output window
    /// (i.e. all phases but reasoning and response generation) should have animated emoji, so the
    /// user knows we are not stuck. The exception of course is the idle (awaiting prompt) phase,
    /// where an animation would be distracting.
    fn animated_stage_emoji(&self, stage: AgentStage) -> &'static str {
        let animation_phase = self.time_ref.elapsed().as_secs();

        match stage {
            AgentStage::Idle => "🟢",
            AgentStage::Prefill => match animation_phase % 6 {
                0 | 2 | 4 => "⏳",
                1 => "🧪",
                3 => "⚗️",
                _ => "☕",
            },
            AgentStage::Reasoning => "🤔",
            AgentStage::ResponseGeneration => "🗣️",
            AgentStage::ToolCallGeneration => match animation_phase % 2 {
                0 => "🧨",
                _ => "💥",
            },
            AgentStage::ToolExecution => match animation_phase % 2 {
                0 => "🎆",
                _ => "✨",
            },
        }
    }
}

impl fmt::Display for AgentStatsDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.read();

        let stage = self.animated_stage_emoji(state.operation_stage);

        let tokens = state.token_usage.sum();
        let target_suffix = if let Some(prefill_target) = state.token_usage.prefill_target
            && state.operation_stage == AgentStage::Prefill
        {
            Cow::Owned(format!("… [{:.1}k]", prefill_target as f32 * 1.0e-3))
        } else if state.operation_stage.is_processing() {
            Cow::Borrowed("…")
        } else {
            Cow::Borrowed("")
        };

        if let Some(context_size) = self.info.context_size {
            write!(
                f,
                " {} {stage} {:.1}k{target_suffix} / {:.1}k ",
                self.info.model_name,
                tokens as f32 * 1.0e-3,
                context_size as f32 * 1.0e-3,
            )
        } else {
            write!(
                f,
                " {} {stage} {:.1}k{target_suffix} ",
                self.info.model_name,
                tokens as f32 * 1.0e-3,
            )
        }
    }
}

impl ChatHistory {
    /// Create an empty chat history.
    fn new() -> Self {
        ChatHistory {
            content: ChatHistoryContent::default(),
            scroll: ScrollPosition::Follow,
            pane_width: NonZero::new(usize::MAX).unwrap(),
            pane_height: NonZero::new(usize::MAX).unwrap(),
            cache: WrappedCache::default(),
        }
    }

    /// Push the given update into the history.
    fn push(&mut self, update: AgentUpdate) {
        self.content.push(update);
    }

    /// Set the history pane width and height (in characters).
    fn set_layout(&mut self, pane_width: usize, pane_height: usize) {
        if pane_width == 0 || pane_height == 0 {
            // Nothing is visible anyway, might as well go unlimited
            self.pane_width = NonZero::new(usize::MAX).unwrap();
            self.pane_height = NonZero::new(usize::MAX).unwrap();
        } else {
            // Checked to be non-zero
            self.pane_width = NonZero::new(pane_width).unwrap();
            self.pane_height = NonZero::new(pane_height).unwrap();
        }

        self.cache.update(&self.content, self.pane_width.get());
    }

    /// After rendering: Create a scrollbar state for this pane.
    fn scrollbar_state(&self) -> ScrollbarState {
        let scrollable = self.cache.total.saturating_sub(self.pane_height.get());
        let scroll_position = match &self.scroll {
            ScrollPosition::Follow => scrollable,
            ScrollPosition::Anchor { .. } => self.first_row().min(scrollable),
        };

        ScrollbarState::new(scrollable).position(scroll_position)
    }

    /// Render the history pane.
    fn render(&mut self) -> Vec<Line<'static>> {
        let (width, height) = (self.pane_width.get(), self.pane_height.get());

        self.cache.update(&self.content, width);

        let total = self.cache.total;
        let first = match self.scroll {
            ScrollPosition::Follow => total.saturating_sub(height),
            ScrollPosition::Anchor { line, col } => self.row_of(line, col),
        }
        .min(total);
        let visible = total.saturating_sub(first).min(height);

        let mut lines = Vec::with_capacity(visible);
        let mut row = 0; // wrapped-line index of the next logical line
        for (line_idx, (logical, block)) in self.content.logical_lines().enumerate() {
            let line_len = *self.cache.line_lengths.get(line_idx).unwrap_or(&0);
            if row + line_len <= first {
                row += line_len;
                continue; // the whole line is above the view
            }
            if row >= first + visible {
                break; // the view is full
            }

            let from = first.saturating_sub(row);
            let to = (first + visible - row).min(line_len);
            if let Some(block) = block {
                for fragment in textwrap::wrap(logical, width)
                    .into_iter()
                    .skip(from)
                    .take(to - from)
                {
                    lines.push(block.style_fragment(fragment.into_owned()));
                }
            } else {
                for _ in from..to {
                    lines.push(Line::default());
                }
            }
            row += line_len;
        }

        lines.resize(height, Line::default());
        lines
    }

    /// Return the wrapped line that starts the current view.
    fn first_row(&self) -> usize {
        match self.scroll {
            ScrollPosition::Follow => self.cache.total.saturating_sub(self.pane_height.get()),
            ScrollPosition::Anchor { line, col } => self.row_of(line, col),
        }
    }

    /// Return the index of the wrapped line that contains the character at the given logical
    /// `line` and `col`.
    fn row_of(&self, line: usize, col: usize) -> usize {
        self.cache.row_of(&self.content, line, col)
    }

    /// Scroll the history in the pane up by the given number of screen rows.
    fn scroll_up(&mut self, lines: usize) {
        if self.cache.total == 0 {
            return;
        }

        let first = self.first_row();
        let new_first = first.saturating_sub(lines);
        self.scroll = self.anchor_at_row(new_first);
    }

    /// Scroll the history in the pane down by the given number of screen rows.
    fn scroll_down(&mut self, lines: usize) {
        if self.cache.total == 0 {
            return;
        }

        let first = self.first_row();
        let new_first = first + lines;
        if new_first + self.pane_height.get() >= self.cache.total {
            self.scroll = ScrollPosition::Follow;
        } else {
            self.scroll = self.anchor_at_row(new_first);
        }
    }

    /// Return the scroll position that puts the top row of the view at wrapped line `row`.
    fn anchor_at_row(&self, row: usize) -> ScrollPosition {
        self.cache.anchor_at_row(&self.content, row)
    }
}

/// Iterate over the chat history, inserting empty lines between blocks
struct BlockSepIterator<'a, B: Iterator<Item = &'a ChatBlock>> {
    /// Iterator over all blocks
    blocks: B,
    /// The current block and an iterator over its logical lines
    current_block: Option<(&'a ChatBlock, std::slice::Iter<'a, String>)>,
}

impl<'a, B: Iterator<Item = &'a ChatBlock>> Iterator for BlockSepIterator<'a, B> {
    type Item = (&'a str, Option<&'a ChatBlock>);

    fn next(&mut self) -> Option<(&'a str, Option<&'a ChatBlock>)> {
        let Some((block, block_iter)) = self.current_block.as_mut() else {
            // Can only happen at the start
            let block = self.blocks.next()?;
            self.current_block = Some((block, block.lines().iter()));
            return self.next(); // no empty line because we are at the start
        };

        let Some(next) = block_iter.next() else {
            let block = self.blocks.next()?;
            self.current_block = Some((block, block.lines().iter()));
            return Some(("", None)); // empty line separating blocks
        };

        Some((next as &str, Some(block)))
    }
}

impl ChatHistoryContent {
    /// Push the given update into the history.
    fn push(&mut self, update: AgentUpdate) {
        match update {
            AgentUpdate::User { prompt } => {
                self.0.push(ChatBlock::User(prompt.into()));
            }

            AgentUpdate::Content { append } => {
                if let Some(last) = self.0.last_mut()
                    && let ChatBlock::Content(block) = last
                {
                    block.append(&append);
                } else {
                    self.0.push(ChatBlock::Content(append.into()));
                }
            }

            AgentUpdate::Reasoning { append } => {
                if let Some(last) = self.0.last_mut()
                    && let ChatBlock::Reasoning(block) = last
                {
                    block.append(&append);
                } else {
                    self.0.push(ChatBlock::Reasoning(append.into()));
                }
            }

            AgentUpdate::ToolCallEx { call, display } => {
                self.0.push(ChatBlock::ToolCall(
                    format!("[{}] {display}", call.id).into(),
                ));
            }

            AgentUpdate::ToolResultEx { call, display } => {
                let name = call.call.name();
                let line = match &display {
                    Ok(display) => format!("=[{name}/{}]=> {display}", call.id),
                    Err(err) => format!("=[{name}/{}]=> {err}", call.id),
                };
                let block = match display {
                    Ok(_) => ChatBlock::ToolResultOk(line.into()),
                    Err(_) => ChatBlock::ToolResultErr(line.into()),
                };
                self.0.push(block);
            }
        }
    }

    /// Return the logical lines (unwrapped) of the history, in order: the lines of each block,
    /// with a blank line between consecutive blocks (marked by `None`). The blank space between
    /// blocks is a presentation decision of the render code; the data has no separator. Empty
    /// blocks contribute no lines, but a separator is still emitted at each block boundary.
    fn logical_lines(&self) -> impl Iterator<Item = (&str, Option<&ChatBlock>)> {
        BlockSepIterator {
            blocks: self.blocks(),
            current_block: None,
        }
    }

    /// Return the number of logical (unwrapped) lines in the history, including the blank lines
    /// between blocks.
    fn logical_line_count(&self) -> usize {
        let mut lines = 0;
        let mut blocks = 0usize;
        for block in self.blocks() {
            lines += block.lines().len();
            blocks += 1;
        }
        lines + blocks.saturating_sub(1)
    }

    /// Return the given logical (unwrapped) line, if it exists.
    fn logical_line(&self, index: usize) -> Option<&str> {
        self.logical_lines().nth(index).map(|(line, _)| line)
    }

    /// Return all the blocks of the history, in order.
    fn blocks(&self) -> impl Iterator<Item = &ChatBlock> {
        self.0.iter()
    }
}

impl ChatBlock {
    /// Return the logical (unwrapped) lines of this block.
    fn lines(&self) -> &[String] {
        match self {
            ChatBlock::User(b)
            | ChatBlock::Content(b)
            | ChatBlock::Reasoning(b)
            | ChatBlock::ToolCall(b)
            | ChatBlock::ToolResultOk(b)
            | ChatBlock::ToolResultErr(b) => &b.lines,
        }
    }

    /// Wrap the given logical line at `line_width` and return the styled
    /// display lines for it.
    #[cfg(test)]
    fn render_line(&self, line: &str, line_width: usize) -> Vec<Line<'static>> {
        textwrap::wrap(line, line_width)
            .into_iter()
            .map(|line| self.style_fragment(line.into_owned()))
            .collect()
    }

    /// Style the given wrapped fragment as a display line.
    fn style_fragment(&self, fragment: String) -> Line<'static> {
        let (style, alignment) = self.style_alignment();

        Line {
            style,
            alignment,
            spans: vec![Span {
                style: Default::default(),
                content: fragment.into(),
            }],
        }
    }

    /// Return the style and alignment of this block’s lines.
    fn style_alignment(&self) -> (Style, Option<Alignment>) {
        match self {
            ChatBlock::User(_) => (Style::default().bold().magenta(), Some(Alignment::Right)),
            ChatBlock::Content(_) => (Style::default().white(), None),
            ChatBlock::Reasoning(_) => (Style::default().dim(), None),
            ChatBlock::ToolCall(_) => (Style::default().blue(), None),
            ChatBlock::ToolResultOk(_) => (Style::default().green(), None),
            ChatBlock::ToolResultErr(_) => (Style::default().bold().red(), None),
        }
    }
}

impl ChatBlockContent {
    /// Append `string` to this block
    fn append(&mut self, string: &str) {
        let full_string = format!("{}{string}", self.trailing_whitespace);
        let (iter, trailing_ws) = Self::split_up(&full_string);

        if let Some(mut iter) = iter {
            if let Some(last) = self.lines.last_mut() {
                last.push_str(iter.next().expect("split_up() returned an empty iterator"));
            }
            self.lines.extend(iter.map(String::from));
        }
        self.trailing_whitespace = trailing_ws.to_string();
    }

    /// Internal helper: Split up string by lines, and return trailing whitespace
    fn split_up(string: &str) -> (Option<impl Iterator<Item = &str>>, &str) {
        let non_ws = string.trim_end();
        let ws = &string[non_ws.len()..];

        if non_ws.is_empty() {
            (None, ws)
        } else {
            let iterator = non_ws
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l));

            (Some(iterator), ws)
        }
    }
}

impl From<&str> for ChatBlockContent {
    /// Use `string` as a whole as block content
    fn from(string: &str) -> Self {
        let (iter, trailing_ws) = Self::split_up(string);
        let lines = if let Some(iter) = iter {
            iter.map(String::from).collect()
        } else {
            Vec::new()
        };
        ChatBlockContent {
            lines,
            trailing_whitespace: trailing_ws.to_string(),
        }
    }
}

impl From<String> for ChatBlockContent {
    /// Use `string` as a whole as block content
    fn from(string: String) -> Self {
        (&string as &str).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llimorse::client::TokenUsage;
    use llimorse::line_format::{FunctionCall, ToolCall, ToolCallParams};
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};

    /// Draw a busy background and the permission popup for the given prompt, and return the
    /// resulting screen as lines of symbols.
    fn screen_with_popup(width: u16, height: u16, prompt: &str) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let frame = terminal
            .draw(|frame| {
                let background = vec![Line::from("x".repeat(width as usize)); height as usize];
                frame.render_widget(Paragraph::new(background), frame.area());
                TermUi::render_permission_popup(frame, frame.area(), prompt);
            })
            .unwrap();
        frame
            .buffer
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    #[test]
    fn popup_is_centered_and_opaque() {
        // 60x20 terminal: the popup is 48 wide (80%) and 5 tall (one prompt line plus thick border
        // and proportional padding), at (6, 8) (the layout solver gives the leftover row to the
        // top when centering).
        let title = " Tool permission requested ";
        let hint = " [Enter: allow]  [Esc: deny] ";
        let mut expected = vec!["x".repeat(60); 20];
        expected[8] = format!(
            "{}┏{}{}┓{}",
            "x".repeat(6),
            title,
            "━".repeat(46 - 27),
            "x".repeat(6)
        );
        expected[9] = format!("{}┃{}┃{}", "x".repeat(6), " ".repeat(46), "x".repeat(6));
        expected[10] = format!(
            "{}┃  {}{}┃{}",
            "x".repeat(6),
            "echo hello",
            " ".repeat(46 - 12),
            "x".repeat(6)
        );
        expected[11] = format!("{}┃{}┃{}", "x".repeat(6), " ".repeat(46), "x".repeat(6));
        expected[12] = format!(
            "{}┗{}{}┛{}",
            "x".repeat(6),
            "━".repeat(46 - 29),
            hint,
            "x".repeat(6)
        );

        assert_eq!(screen_with_popup(60, 20, "echo hello"), expected);
    }

    /// Render the first (only) line of `block` and return its text and style.
    fn first_rendered_line(block: &ChatBlock, width: usize) -> (String, Style) {
        let line = block
            .render_line(&block.lines()[0], width)
            .into_iter()
            .next()
            .expect("block renders at least one line");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        (text, line.style)
    }

    /// Build a single content block with one logical line per entry.
    fn content_history(lines: &[&str]) -> ChatHistory {
        let mut history = ChatHistory::new();
        for line in lines {
            history.push(AgentUpdate::Content {
                append: format!("{line}\n"),
            });
        }
        history
    }

    /// Return the texts of the given rendered lines.
    fn line_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn pane_shows_the_tail_with_wrapping() {
        // At width 5, "bbbbbbbb" wraps to two lines, so the history has four display lines for
        // three logical lines.
        let mut history = content_history(&["aaaaa", "bbbbbbbb", "cc"]);

        history.set_layout(5, 3);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["bbbbb", "bbb", "cc"]);
    }

    #[test]
    fn pane_anchors_at_display_lines() {
        // At width 5, "xxxx x" wraps to two display lines. The anchor of the third display line is
        // the start of "yyyy", so the view starts there, not at the second display line of the
        // first logical line.
        let mut history = content_history(&["xxxx x", "yyyy"]);

        history.set_layout(5, 2);
        history.scroll = history.anchor_at_row(2);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["yyyy", ""]);
    }

    #[test]
    fn pane_pads_short_histories() {
        // The empty logical line renders as an empty display line.
        let mut history = content_history(&["a", "", "b"]);

        history.set_layout(10, 5);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["a", "", "b", "", ""]);
    }

    #[test]
    fn scroll_steps_are_exact_display_lines() {
        // "aaaaaa" wraps to ["aaaaa", "a"] at width 5, so the history has
        // three display lines for two logical lines. The tail view’s top line
        // is the second fragment of the first logical line.
        let mut history = content_history(&["aaaaaa", "bbbb"]);

        history.set_layout(5, 2);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["a", "bbbb"]);

        // Scrolling up one display line moves the top to the first fragment
        // of the same logical line.
        let first = history.cache.total.saturating_sub(2);
        history.scroll = history.anchor_at_row(first.saturating_sub(1));
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["aaaaa", "a"]);
    }

    #[test]
    fn anchor_survives_a_width_change() {
        let mut history = content_history(&["aaaaaa", "bbbbbb", "cccc"]);

        // At width 5 the display lines are aaaaa, a, bbbbb, b, cccc; anchor
        // at the line starting with "bbbbb".
        history.set_layout(5, 5);
        history.scroll = history.anchor_at_row(2);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["bbbbb", "b", "cccc", "", ""]);

        // At width 10 the same anchor character starts "bbbbbb", so the view
        // stays on it after the reflow.
        history.set_layout(10, 5);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["bbbbbb", "cccc", "", "", ""]);
    }

    #[test]
    fn tool_calls_and_results_render_with_their_display_format() {
        let call = ToolCall {
            id: "call_1".into(),
            call: ToolCallParams::Function {
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            },
        };

        let mut history = ChatHistory::new();
        history.push(AgentUpdate::ToolCallEx {
            call: call.clone(),
            display: "bash: echo hi".into(),
        });
        history.push(AgentUpdate::ToolResultEx {
            call: call.clone(),
            display: Ok("hi".into()),
        });
        history.push(AgentUpdate::ToolResultEx {
            call,
            display: Err("command not found".into()),
        });

        // Three blocks of one line each, plus the blank line between each pair.
        assert_eq!(history.content.logical_line_count(), 5);

        let (text, style) = first_rendered_line(&history.content.0[0], 80);
        assert_eq!(text, "[call_1] bash: echo hi");
        assert_eq!(style.fg, Some(Color::Blue));

        let (text, style) = first_rendered_line(&history.content.0[1], 80);
        assert_eq!(text, "=[bash/call_1]=> hi");
        assert_eq!(style.fg, Some(Color::Green));

        let (text, style) = first_rendered_line(&history.content.0[2], 80);
        assert_eq!(text, "=[bash/call_1]=> command not found");
        assert_eq!(style.fg, Some(Color::Red));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn blocks_are_separated_by_blank_lines() {
        // The blank space between blocks is a presentation decision of the
        // render code: two blocks of one line each yield three display
        // lines, and the middle one is empty and unstyled.
        let mut history = ChatHistory::new();
        history.push(AgentUpdate::User {
            prompt: "hi".into(),
        });
        history.push(AgentUpdate::Content {
            append: "hello".into(),
        });

        assert_eq!(history.content.logical_line_count(), 3);

        history.set_layout(20, 5);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["hi", "", "hello", "", ""]);

        // The separator is a display line, so anchoring counts it: the
        // anchor of the third display line is the start of "hello".
        history.set_layout(20, 3);
        history.scroll = history.anchor_at_row(2);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["hello", "", ""]);
    }

    #[test]
    fn empty_blocks_take_no_lines_but_keep_their_separators() {
        // An empty block (e.g. from an empty streaming delta after a tool result) contributes
        // no lines of its own, but a separator is still emitted at each of its boundaries, so
        // the count must agree with the iterator.
        let call = ToolCall {
            id: "call_1".into(),
            call: ToolCallParams::Function {
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            },
        };

        let mut history = ChatHistory::new();
        history.push(AgentUpdate::ToolCallEx {
            call: call.clone(),
            display: "bash: echo hi".into(),
        });
        history.push(AgentUpdate::ToolResultEx {
            call: call.clone(),
            display: Ok("hi".into()),
        });
        history.push(AgentUpdate::Content {
            append: String::new(),
        });
        history.push(AgentUpdate::User {
            prompt: "hi".into(),
        });

        // Three non-empty lines and one empty block: 3 lines + 3 separators.
        assert_eq!(history.content.logical_line_count(), 6);
        assert_eq!(history.content.logical_lines().count(), 6);

        history.set_layout(80, 6);
        let lines = history.render();
        assert_eq!(
            line_texts(&lines),
            vec![
                "[call_1] bash: echo hi",
                "",
                "=[bash/call_1]=> hi",
                "",
                "",
                "hi"
            ]
        );

        // A lone empty block takes no space at all.
        let mut history = ChatHistory::new();
        history.push(AgentUpdate::Content {
            append: String::new(),
        });
        assert_eq!(history.content.logical_line_count(), 0);
        history.set_layout(20, 3);
        let lines = history.render();
        assert_eq!(line_texts(&lines), vec!["", "", ""]);
    }

    #[test]
    fn popup_does_not_panic_on_small_terminals() {
        // A prompt that would be far too large for the terminal must be clamped, not panic.
        let prompt = "x".repeat(1000);
        for (width, height) in [(12u16, 6), (4u16, 3), (1u16, 1)] {
            let screen = screen_with_popup(width, height, &prompt);
            assert_eq!(screen.len(), height as usize);
        }

        let screen = screen_with_popup(12, 6, &prompt);
        assert!(screen[0].contains('┏') && screen[0].contains('┓'));
        assert!(screen[5].contains('┗') && screen[5].contains('┛'));
    }

    /// Subagent IDs come from a monotonically increasing counter, while the state list is
    /// compacted when a subagent is dropped. Updates for surviving subagents must therefore be
    /// routed by their stored ID, not by index arithmetic.
    #[test]
    fn subagent_updates_survive_the_drop_of_another_subagent() {
        let client_state = || {
            Arc::new(RwLock::new(ClientState {
                token_usage: TokenUsage::default(),
                operation_stage: AgentStage::default(),
            }))
        };
        let client_info = || ClientInfo {
            model_name: "test".into(),
            context_size: None,
        };

        let mut agents = UiAgents::new(client_state(), client_info());
        agents.add_subagent(
            SubagentId::new(0),
            "first".into(),
            client_info(),
            client_state(),
        );
        agents.add_subagent(
            SubagentId::new(1),
            "second".into(),
            client_info(),
            client_state(),
        );

        // The first subagent finishes and is compacted out of the list.
        agents.remove_subagent(SubagentId::new(0));
        assert_eq!(agents.state.len(), 2);

        // The surviving subagent must still be routable, and the update must land in its history,
        // not be swallowed.
        let Some(agent) = agents.get_mut(&AgentId::Subagent(SubagentId::new(1))) else {
            panic!("surviving subagent was not found after compaction");
        };
        agent.history.push(AgentUpdate::Content {
            append: "hello".into(),
        });

        assert!(agents.state[0].history.content.0.is_empty());
        assert_eq!(
            agents.state[1].subagent_state.as_ref().unwrap().id,
            SubagentId::new(1)
        );
        assert!(!agents.state[1].history.content.0.is_empty());

        // A dropped subagent must not be routable.
        assert!(
            agents
                .get_mut(&AgentId::Subagent(SubagentId::new(0)))
                .is_none()
        );
    }
}
