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
use llimorse_chat::history::HistoryEntryType;
use llimorse_chat::{ChatHistory, ui};
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
use std::num::Saturating;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{cmp, env, fmt, io};
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

    /// The chat history as shared with the agent
    history: Arc<Mutex<ChatHistory>>,

    /// If a subagent: Additional information about it
    subagent_state: Option<SubagentState>,

    /// First line of the chat history to show (`usize::MAX` to follow the tail)
    scroll: Saturating<usize>,

    /// How many lines (elements of `chat_history`) are visible on screen right now
    lines_on_screen: usize,
}

/// Additional information about subagents
struct SubagentState {
    /// The ID by which the subagent is identified in UI notifications
    id: usize,

    /// The task given to this agent
    task: String,
}

impl TermUi {
    /// Create the term state with `chat_history`, for `agent`, with the application name
    /// `app_name` (e.g. for notifications).
    pub fn new(app_name: &str, agent: &Agent, chat_history: Arc<Mutex<ChatHistory>>) -> Self {
        let term = ratatui::init();
        set_up_term();

        let mut input_area: ratatui_textarea::TextArea<'static> = Default::default();
        input_area.set_cursor_line_style(Default::default());
        input_area.set_wrap_mode(ratatui_textarea::WrapMode::Word);

        TermUi {
            term: Some(term),
            events: ct::EventStream::new(),
            agents: UiAgents::new(
                agent.client_state_arc(),
                agent.client_info().clone(),
                chat_history,
            ),
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

    /// Scroll the active view up by the given number of lines.
    fn scroll_up(&mut self, lines: usize) {
        let agent = self.agents.active_mut();
        let history_len = agent.history.lock().unwrap().lines().len();

        if agent.scroll.0 == usize::MAX {
            agent.scroll.0 = history_len.saturating_sub(agent.lines_on_screen);
        }
        agent.scroll -= lines;
    }

    /// Scroll the active view down by the given number of lines.
    fn scroll_down(&mut self, lines: usize) {
        let agent = self.agents.active_mut();
        let history_len = agent.history.lock().unwrap().lines().len();

        agent.scroll += lines;
        if agent.scroll.0 >= history_len.saturating_sub(agent.lines_on_screen) {
            agent.scroll.0 = usize::MAX;
        }
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
                            // Ignore Enter without modifier keys that does not mean a submit
                            // (i.e., when the field is empty, do not allow empty to create a
                            // newline, instead just ignore it)
                            return Ok(None);
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
                        self.scroll_up(self.agents.active().lines_on_screen.div_ceil(2));
                    }
                    return Ok(None);
                }
                ct::KeyCode::PageDown => {
                    if event.modifiers.contains(ct::KeyModifiers::SHIFT) {
                        self.agents.switch_next();
                    } else {
                        self.scroll_down(self.agents.active().lines_on_screen.div_ceil(2));
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

    /// Raise a terminal notification that the response is ready and the agent is awaiting a
    /// new prompt.
    ///
    /// Under tmux (detected via `$TERM`) the OSC 99 messages are wrapped in tmux’s
    /// passthrough envelope (which requires `allow-passthrough on`); otherwise the
    /// messages are sent bare.
    fn notify_prompt_done(&self, response: Option<String>) {
        // Without the leading ESC and the ST terminator: the bare path and the tmux
        // wrapper both supply them.
        let title_payload = format!("]99;i=1:d=0;{}: Turn done, awaiting prompt", self.app_name);
        let body_payload = if let Some(response) = response {
            let sanitized = response
                .trim()
                .chars()
                .filter_map(|c| match c {
                    '\n' | '\t' => Some(' '),
                    '\r' => None,
                    ';' => Some(','),
                    c if c.is_ascii_control() => Some('�'),
                    c => Some(c),
                })
                .collect::<String>();
            format!("]99;i=1:d=1:p=body;{}", sanitized.truncated_display(200))
        } else {
            String::from("]99;i=1:d=1:p=body;(Awaiting prompt.)")
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
        let chat_history = agent.history.lock().unwrap();
        let history_line_count = history_cell.height.saturating_sub(2) as usize;
        let history_width = history_cell.width.saturating_sub(2) as usize;

        let mut lines_on_screen = 0;

        let history_lines = if agent.scroll.0 == usize::MAX {
            let mut history_lines = history_into_ratatui_lines(
                chat_history
                    .lines()
                    .iter()
                    .rev()
                    .flat_map(|line| {
                        lines_on_screen += 1; // diabolical
                        textwrap::wrap(&line.0, history_width)
                            .into_iter()
                            .rev()
                            .map(|l| (l, line.1))
                    })
                    .take(history_line_count),
            );
            history_lines.reverse();
            history_lines
        } else {
            history_into_ratatui_lines(
                chat_history
                    .lines()
                    .iter()
                    .skip(agent.scroll.0)
                    .flat_map(|line| {
                        lines_on_screen += 1; // diabolical
                        textwrap::wrap(&line.0, history_width)
                            .into_iter()
                            .map(|l| (l, line.1))
                    })
                    .take(history_line_count),
            )
        };

        agent.lines_on_screen = lines_on_screen;

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
        let history_len = chat_history.lines().len();
        let scroll_len = history_len.saturating_sub(lines_on_screen);
        let mut scrollbar_state =
            ScrollbarState::new(scroll_len).position(cmp::min(agent.scroll.0, scroll_len));

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
            ui::Notification::PromptSubmitted => {
                self.queued_prompts.pop_front();
                self.processing = true;
            }
            ui::Notification::AwaitingPrompt { response } => {
                self.processing = false;
                self.notify_prompt_done(response);
            }
            ui::Notification::RequestPermission { prompt, approval } => {
                self.pending_permissions.push_back((prompt, approval));
            }
            ui::Notification::SubagentCreated {
                subagent_id,
                prompt,
                client_state,
                chat_history,
            } => {
                self.agents
                    .add_subagent(subagent_id, prompt, client_state, chat_history);
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

/// Helper function to convert the given iterator of `HistoryEntryType`-annotated lines into
/// ratatui lines.
fn history_into_ratatui_lines<'a, I: Iterator<Item = (Cow<'a, str>, HistoryEntryType)>>(
    iter: I,
) -> Vec<Line<'a>> {
    iter.map(|line| {
        let (style, alignment) = ratatui_style(line.1);

        Line {
            style,
            alignment: Some(alignment),
            spans: vec![Span {
                style: Default::default(),
                content: line.0,
            }],
        }
    })
    .collect()
}

/// Converts a `HistoryEntryType` into the corresponding ratatui styles
fn ratatui_style(het: HistoryEntryType) -> (Style, Alignment) {
    match het {
        HistoryEntryType::Empty => (Style::default(), Alignment::Left),
        HistoryEntryType::User => (Style::default().bold().magenta(), Alignment::Right),
        HistoryEntryType::Content => (Style::default().white(), Alignment::Left),
        HistoryEntryType::Reasoning => (Style::default().dim(), Alignment::Left),
        HistoryEntryType::ToolCall => (Style::default().blue(), Alignment::Left),
        HistoryEntryType::ToolResultOk => (Style::default().green(), Alignment::Left),
        HistoryEntryType::ToolResultErr => (Style::default().bold().red(), Alignment::Left),
    }
}

impl UiAgents {
    /// Create a new collection of agents, with a single main agent
    fn new(
        main_client_state: Arc<RwLock<ClientState>>,
        main_client_info: ClientInfo,
        main_history: Arc<Mutex<ChatHistory>>,
    ) -> Self {
        UiAgents {
            state: vec![AgentState {
                client_state: main_client_state,
                client_info: main_client_info,
                history: main_history,
                subagent_state: None,
                scroll: Saturating(usize::MAX),
                lines_on_screen: 0,
            }],
            active_agent: 0,
        }
    }

    /// Add a new subagent at the end of our list
    fn add_subagent(
        &mut self,
        subagent_id: usize,
        prompt: String,
        client_state: Arc<RwLock<ClientState>>,
        history: Arc<Mutex<ChatHistory>>,
    ) {
        self.state.push(AgentState {
            client_state,
            history,
            subagent_state: Some(SubagentState {
                id: subagent_id,
                task: prompt,
            }),
            scroll: Saturating(usize::MAX),
            lines_on_screen: 0,
        });
    }

    /// Remove a subagent by its ID
    ///
    /// If the subagent is currently active, change the active view to the main agent.
    fn remove_subagent(&mut self, subagent_id: usize) {
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
        self.state
            .get(self.active_agent)
            .or_else(|| self.state.first())
            .expect("No agents left")
    }

    /// Return the currently active view’s agent, mutably
    fn active_mut(&mut self) -> &mut AgentState {
        if let Some(state) = self.state.get_mut(self.active_agent) {
            state
        } else {
            self.state.first_mut().expect("No agents left")
        }
    }

    /// Return whether the currently active view is the main agent’s
    fn is_main(&self) -> bool {
        self.active_agent == 0
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

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
        // and proportional padding), at (6, 8) (the layout solver gives the leftover row to the top
        // when centering).
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
}
