//! UI part of the WorkBuddy application

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod wrap;

use anyhow::Result;
use crossterm::event as ct;
use futures::StreamExt;
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
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{cmp, io};
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

    /// Immutable client information
    client_info: ClientInfo,

    /// The current general state of the client
    client_state: Arc<RwLock<ClientState>>,

    /// Produces terminal events, asynchronously
    events: ct::EventStream,

    /// The chat history as shared with the agent
    chat_history: Arc<Mutex<ChatHistory>>,

    /// First line of the chat history to show (`usize::MAX` to follow the tail)
    history_scroll: Saturating<usize>,

    /// How many lines (elements of `chat_history`) are visible on screen right now
    history_lines_on_screen: usize,

    /// User message input widget
    input_area: ratatui_textarea::TextArea<'static>,

    /// Messages that are queued for sending
    queued_prompts: VecDeque<String>,

    /// Tool calls awaiting a permission decision from the user, oldest first.
    ///
    /// Only the first one is shown; the rest wait their turn.
    pending_permissions: VecDeque<(String, oneshot::Sender<std::result::Result<(), String>>)>,

    /// When the object was created, purely for visual purposes
    creation: Instant,
}

impl TermUi {
    /// Create the term state with `chat_history`
    pub fn new(agent: &Agent, chat_history: Arc<Mutex<ChatHistory>>) -> Self {
        let term = ratatui::init();
        set_up_term();

        let mut input_area: ratatui_textarea::TextArea<'static> = Default::default();
        input_area.set_cursor_line_style(Default::default());
        input_area.set_block(
            Block::bordered()
                .title(" Input ")
                .title_style(Style::default().bold()),
        );
        input_area.set_wrap_mode(ratatui_textarea::WrapMode::Word);

        TermUi {
            term: Some(term),
            client_info: agent.client_info().clone(),
            client_state: agent.client_state_arc(),
            events: ct::EventStream::new(),
            chat_history,
            history_scroll: Saturating(usize::MAX),
            history_lines_on_screen: 0,
            input_area,
            queued_prompts: VecDeque::new(),
            pending_permissions: VecDeque::new(),
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

    /// Scroll the chat history window up by the given number of lines.
    fn scroll_up(&mut self, lines: usize) {
        let history_len = self.chat_history.lock().unwrap().lines().len();

        if self.history_scroll.0 == usize::MAX {
            self.history_scroll.0 = history_len.saturating_sub(self.history_lines_on_screen);
        }
        self.history_scroll -= lines;
    }

    /// Scroll the chat history window down by the given number of lines.
    fn scroll_down(&mut self, lines: usize) {
        let history_len = self.chat_history.lock().unwrap().lines().len();

        self.history_scroll += lines;
        if self.history_scroll.0 >= history_len.saturating_sub(self.history_lines_on_screen) {
            self.history_scroll.0 = usize::MAX;
        }
    }

    /// Handle the given keyboard event.
    fn handle_key_event(&mut self, event: ct::KeyEvent) -> Result<Option<ui::Event>> {
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
                    if !self.input_area.is_empty() {
                        let message = self.input_area.lines().join("\n");
                        self.input_area.clear();
                        return Ok(Some(ui::Event::Input(message)));
                    } else if !self.queued_prompts.is_empty() {
                        return Ok(Some(ui::Event::ForceSubmitQueued));
                    } else {
                        // Ignore Enter without modifier keys that does not mean a submit (i.e.,
                        // when the field is empty, do not allow empty to create a newline, instead
                        // just ignore it)
                        return Ok(None);
                    }
                }

                ct::KeyCode::Esc => return Ok(Some(ui::Event::Exit)),

                ct::KeyCode::PageUp => {
                    self.scroll_up(self.history_lines_on_screen.div_ceil(2));
                    return Ok(None);
                }
                ct::KeyCode::PageDown => {
                    self.scroll_down(self.history_lines_on_screen.div_ceil(2));
                    return Ok(None);
                }

                _ => (),
            }
        }

        self.input_area.input(event);
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
        // Normalize line endings: `ratatui_textarea::TextArea::insert_str()` does not handle \r.
        let text = text.replace("\r\n", "\n").replace("\r", "\n");
        self.input_area.insert_str(&text);
        Ok(None)
    }

    /// Render the current state onto the screen.
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Input height field: Number of lines, maximum 5. Note that `.lines()` is always
        // guaranteed to at least return one (empty) line, and `line_ranges` likewise always
        // yields at least one row per line.
        // The `TextArea` does not expose its on-screen row count, so count rows with the same
        // wrapping algorithm the widget renders with (vendored in `wrap`).
        const MAX_HEIGHT: usize = 5;
        let input_inner_width = area.width.saturating_sub(2) as usize; // account for the border
        let input_outer_height = self
            .input_area
            .lines()
            .iter()
            .take(MAX_HEIGHT)
            .map(|line| {
                wrap::wrapped_line_count(line, self.input_area.wrap_mode(), input_inner_width)
            })
            .sum::<usize>()
            .min(MAX_HEIGHT) as u16
            + 2; // account for the border

        let mut layout = Vec::with_capacity(self.queued_prompts.len() + 2);
        layout.push(Constraint::Percentage(100));
        for _ in 0..self.queued_prompts.len() {
            layout.push(Constraint::Min(1));
        }
        layout.push(Constraint::Min(input_outer_height));

        let layout = Layout::vertical(layout).split(area);

        let history_cell = layout[0];
        let input_cell = layout[self.queued_prompts.len() + 1];

        let chat_history = self.chat_history.lock().unwrap();
        let history_line_count = history_cell.height.saturating_sub(2) as usize;
        let history_width = history_cell.width.saturating_sub(2) as usize;

        let mut history_lines_on_screen = 0;
        let scroll = self.history_scroll.0;

        let history_lines = if scroll == usize::MAX {
            let mut history_lines = history_into_ratatui_lines(
                chat_history
                    .lines()
                    .iter()
                    .rev()
                    .flat_map(|line| {
                        history_lines_on_screen += 1; // diabolical
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
                    .skip(scroll)
                    .flat_map(|line| {
                        history_lines_on_screen += 1; // diabolical
                        textwrap::wrap(&line.0, history_width)
                            .into_iter()
                            .map(|l| (l, line.1))
                    })
                    .take(history_line_count),
            )
        };

        self.history_lines_on_screen = history_lines_on_screen;

        let paragraph_content = Text {
            alignment: None,
            style: Default::default(),
            lines: history_lines,
        };

        let client_state = self.client_state();
        let tokens = client_state.token_usage.sum();
        let stage = self.animated_stage_emoji(client_state.operation_stage);
        let title = if let Some(context_size) = self.client_info.context_size {
            format!(
                " {} {stage} {:.1}k / {:.1}k ",
                self.client_info.model_name,
                tokens as f32 * 1.0e-3,
                context_size as f32 * 1.0e-3,
            )
        } else {
            format!(
                " {} {stage} {:.1}k ",
                self.client_info.model_name,
                tokens as f32 * 1.0e-3,
            )
        };
        let paragraph = Paragraph::new(paragraph_content).block(
            Block::bordered()
                .title(title)
                .title_style(Style::new().bold()),
        );

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let history_len = chat_history.lines().len();
        let scroll_len = history_len - history_lines_on_screen;
        let mut scrollbar_state =
            ScrollbarState::new(scroll_len).position(cmp::min(scroll, scroll_len));

        frame.render_widget(paragraph, history_cell);
        frame.render_stateful_widget(
            scrollbar,
            history_cell.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
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
        frame.render_widget(&self.input_area, input_cell);

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

    /// Return a representative emoji of the current stage, animated
    ///
    /// Specifically the stages where there is no visible text generation in the output window
    /// (i.e. all phases but reasoning and response generation) should have animated emoji, so the
    /// user knows we are not stuck. The exception of course is the idle (awaiting prompt) phase,
    /// where an animation would be distracting.
    fn animated_stage_emoji(&self, stage: AgentStage) -> &'static str {
        let animation_phase = self.creation.elapsed().as_secs();

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

    /// Return the current client state object
    fn client_state(&self) -> impl Deref<Target = ClientState> {
        self.client_state.read()
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
            ui::Notification::PromptQueued(p) => self.queued_prompts.push_back(p),
            ui::Notification::PromptSubmitted => {
                self.queued_prompts.pop_front();
            }
            ui::Notification::RequestPermission { prompt, approval } => {
                self.pending_permissions.push_back((prompt, approval));
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
