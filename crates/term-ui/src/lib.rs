//! UI part of the WorkBuddy application

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

mod wrap;

use anyhow::Result;
use crossterm::event as ct;
use futures::StreamExt;
use llimorse::{Agent, ChatListener};
use llimorse_chat::history::HistoryEntryType;
use llimorse_chat::{ChatHistory, ui};
use ratatui::layout::{Alignment, Constraint, Layout, Margin};
use ratatui::style::Style;
use ratatui::text::Text;
use ratatui::widgets::{Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::{DefaultTerminal, Frame};
use std::borrow::Cow;
use std::num::Saturating;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{cmp, io};

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

    /// The name of the model being run
    model_name: String,

    /// The maximum number of tokens that fit in the context
    context_size: Option<u64>,

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

impl TermUi {
    /// Create the term state with `chat_history`
    pub fn new(agent: &Agent<impl ChatListener>, chat_history: Arc<Mutex<ChatHistory>>) -> Self {
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
            model_name: agent.model_name().to_string(),
            context_size: agent.context_size(),
            events: ct::EventStream::new(),
            chat_history,
            history_scroll: Saturating(usize::MAX),
            history_lines_on_screen: 0,
            input_area,
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
        if event.kind == ct::KeyEventKind::Press {
            match event.code {
                ct::KeyCode::Enter if event.modifiers.is_empty() && !self.input_area.is_empty() => {
                    let message = self.input_area.lines().join("\n");
                    self.input_area.clear();
                    return Ok(Some(ui::Event::Input(message)));
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

        let layout = Layout::vertical([
            Constraint::Percentage(100),
            Constraint::Min(input_outer_height),
        ])
        .split(area);

        let history_cell = layout[0];
        let input_cell = layout[1];

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

        let tokens = chat_history.token_usage().sum();
        let title = if let Some(context_size) = self.context_size {
            format!(
                " {}: {:.1}k / {:.1}k ",
                self.model_name,
                tokens as f32 * 1.0e-3,
                context_size as f32 * 1.0e-3,
            )
        } else {
            format!(" {}: {:.1}k ", self.model_name, tokens as f32 * 1.0e-3,)
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
        frame.render_widget(&self.input_area, input_cell);
    }
}

impl ui::UiState for TermUi {
    type Error = anyhow::Error;

    /// Handle input on the terminal, and redraw
    async fn get_event(&mut self) -> Result<ui::Event> {
        self.draw()?;

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
) -> Vec<ratatui::text::Line<'a>> {
    iter.map(|line| {
        let (style, alignment) = ratatui_style(line.1);

        ratatui::text::Line {
            style,
            alignment: Some(alignment),
            spans: vec![ratatui::text::Span {
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
        HistoryEntryType::Content => (Style::default().bold(), Alignment::Left),
        HistoryEntryType::Reasoning => (Style::default().italic(), Alignment::Left),
        HistoryEntryType::ToolCall => (Style::default().blue(), Alignment::Left),
        HistoryEntryType::ToolResultOk => (Style::default().green(), Alignment::Left),
        HistoryEntryType::ToolResultErr => (Style::default().bold().red(), Alignment::Left),
    }
}
