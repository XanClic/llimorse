//! UI part of the WorkBuddy application

use super::history::ChatHistory;
use anyhow::Result;
use crossterm::event as ct;
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout, Margin};
use ratatui::text::Text;
use ratatui::widgets::{Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::{DefaultTerminal, Frame};
use std::num::Saturating;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{cmp, io};

/// Counts users of the ratatui terminal (honestly only should be one or none...)
static TERM_SET_UP: AtomicUsize = AtomicUsize::new(0);

/// UI state for WorkBuddy
pub struct TermState {
    /// ratatui terminal object
    ///
    /// Always set, except during rendering, because for some reason ratatui needs ownership access
    /// to this. (Well, “for some reason” is that it found it clever to have a `term.do(|x| ...)`
    /// pattern for rendering, so if we want access to `self` in that callback, we cannot have
    /// `term` stored here.)
    term: Option<DefaultTerminal>,

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
}

/// Application state level events that can come from the UI
pub enum Event {
    /// Exit requested
    Exit,

    /// User submitted a message as input
    Input(String),
}

impl TermState {
    /// Create the term state with `chat_history`
    pub fn new(chat_history: Arc<Mutex<ChatHistory>>) -> Self {
        let term = ratatui::init();
        set_up_term();

        let mut input_area: ratatui_textarea::TextArea<'static> = Default::default();
        input_area.set_cursor_line_style(Default::default());
        input_area.set_block(Block::bordered().title("Input"));
        input_area.set_wrap_mode(ratatui_textarea::WrapMode::Word);

        TermState {
            term: Some(term),
            events: ct::EventStream::new(),
            chat_history,
            history_scroll: Saturating(usize::MAX),
            history_lines_on_screen: 0,
            input_area,
        }
    }

    /// Handle input on the terminal, with the given `poll_timeout`.
    pub async fn handle_term_input(&mut self, poll_timeout: Duration) -> Result<Option<Event>> {
        let result = tokio::time::timeout(poll_timeout, self.events.next()).await;
        let Ok(result) = result else {
            // Timeout means no event available, which is fine
            return Ok(None);
        };

        let Some(result) = result else {
            // Stream ended
            return Ok(Some(Event::Exit));
        };

        match result? {
            ct::Event::Key(key) => self.handle_key_event(key),
            ct::Event::Mouse(mouse) => self.handle_mouse_event(mouse),
            _ => Ok(None),
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
    fn handle_key_event(&mut self, event: ct::KeyEvent) -> Result<Option<Event>> {
        if event.kind == ct::KeyEventKind::Press {
            match event.code {
                ct::KeyCode::Enter if event.modifiers.is_empty() && !self.input_area.is_empty() => {
                    let message = self.input_area.lines().join("\n");
                    self.input_area.clear();
                    return Ok(Some(Event::Input(message)));
                }

                ct::KeyCode::Esc => return Ok(Some(Event::Exit)),

                ct::KeyCode::PageUp => {
                    self.scroll_up((self.history_lines_on_screen + 1) / 2);
                    return Ok(None);
                }
                ct::KeyCode::PageDown => {
                    self.scroll_down((self.history_lines_on_screen + 1) / 2);
                    return Ok(None);
                }

                _ => (),
            }
        }

        self.input_area.input(event);
        Ok(None)
    }

    /// Handle the given mouse event.
    fn handle_mouse_event(&mut self, event: ct::MouseEvent) -> Result<Option<Event>> {
        match event.kind {
            ct::MouseEventKind::ScrollDown => self.scroll_down(1),
            ct::MouseEventKind::ScrollUp => self.scroll_up(1),

            _ => (),
        }

        Ok(None)
    }

    /// Render the current state onto the screen.
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let layout =
            Layout::vertical([Constraint::Percentage(100), Constraint::Min(5)]).split(area);

        let history_cell = layout[0];
        let input_cell = layout[1];

        let chat_history = self.chat_history.lock().unwrap();
        let history_line_count = history_cell.height.saturating_sub(2) as usize;
        let history_width = history_cell.width.saturating_sub(2) as usize;

        let mut history_lines_on_screen = 0;
        let scroll = self.history_scroll.0;

        let history_lines = if scroll == usize::MAX {
            let mut history_lines = ChatHistory::into_ratatui_lines(
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
            ChatHistory::into_ratatui_lines(
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

        let paragraph = Paragraph::new(paragraph_content).block(Block::bordered().title(format!(
            "Chat: {:.1}k+{:.1}k",
            chat_history.token_usage().0 as f32 * 1.0e-3,
            chat_history.token_usage().1 as f32 * 1.0e-3
        )));

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
        frame.render_widget(&self.input_area, input_cell);
    }
}

impl Drop for TermState {
    fn drop(&mut self) {
        tear_down_term();
    }
}

/// Basic terminal set-up
fn set_up_term() {
    if TERM_SET_UP.fetch_add(1, Ordering::Relaxed) == 0 {
        color_eyre::install().expect("Failed to install crash handler");
        let _ = crossterm::execute!(io::stdout(), ct::EnableMouseCapture);
    }
}

/// Basic terminal tear-down
fn tear_down_term() {
    if TERM_SET_UP.fetch_sub(1, Ordering::Relaxed) == 1 {
        let _ = crossterm::execute!(io::stdout(), ct::DisableMouseCapture);
        ratatui::restore();
    }
}
