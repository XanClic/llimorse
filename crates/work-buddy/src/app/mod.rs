//! Implementation of the WorkBuddy application.

mod agent;
mod history;

use agent::WorkBuddyAgent;
use anyhow::{Result, anyhow};
use futures::{FutureExt, StreamExt};
use history::ChatHistory;
use std::io;
use std::num::Saturating;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as TokioMutex, Notify, mpsc};

/// The application state
pub struct WorkBuddy {
    /// Agent thread running concurrently
    agent_thread: Option<JoinHandle<()>>,

    /// Produces terminal events, asynchronously
    events: TokioMutex<crossterm::event::EventStream>,

    /// The chat history as shared with the agent
    chat_history: Arc<Mutex<ChatHistory>>,

    /// UI state
    ui: Mutex<TermState>,

    /// Submit user messages to the LLM
    user_message_submit: mpsc::UnboundedSender<String>,

    /// Notification from the agent to redraw the UI
    agent_update: Arc<Notify>,

    /// Set once we are supposed to exit
    exit: Arc<AtomicBool>,
}

/// UI state for WorkBuddy
struct TermState {
    /// First line of the chat history to show (`usize::MAX` to follow the tail)
    history_scroll: Saturating<usize>,

    /// How many lines (elements of `chat_history`) are visible on screen right now
    history_lines_on_screen: usize,

    /// User message input widget
    input_area: ratatui_textarea::TextArea<'static>,
}

impl WorkBuddy {
    /// Create a new application state around `agent`.
    pub fn new(agent: llimo::Agent) -> Self {
        let mut input_area: ratatui_textarea::TextArea<'static> = Default::default();
        input_area.set_cursor_line_style(Default::default());
        input_area.set_block(ratatui::widgets::Block::bordered().title("Input"));
        input_area.set_wrap_mode(ratatui_textarea::WrapMode::Word);

        let chat_history = Arc::new(Mutex::new(ChatHistory::default()));
        let exit = Arc::new(AtomicBool::new(false));

        let (user_message_send, user_message_recv) = mpsc::unbounded_channel();
        let agent_update = Arc::new(Notify::new());

        let agent_thread = thread::spawn({
            let chat_history = Arc::clone(&chat_history);
            let update_ui = Arc::clone(&agent_update);
            let exit = Arc::clone(&exit);
            move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move {
                        let mut wba =
                            WorkBuddyAgent::new(chat_history, user_message_recv, update_ui, exit);
                        if let Err(err) = wba.run(agent).await {
                            panic!("Agent error: {err}");
                        }
                    })
            }
        });

        WorkBuddy {
            agent_thread: Some(agent_thread),
            events: TokioMutex::new(crossterm::event::EventStream::new()),

            chat_history,

            ui: Mutex::new(TermState {
                history_scroll: Saturating(usize::MAX),
                history_lines_on_screen: 0,
                input_area,
            }),

            user_message_submit: user_message_send,
            agent_update,

            exit,
        }
    }

    /// Run the application until it finds it should exit.
    ///
    /// Includes setting up the terminal and tearing it down.
    pub async fn run(&mut self) -> Result<()> {
        color_eyre::install().map_err(|err| anyhow!("Failed to install crash handler: {err}"))?;
        let term = ratatui::init();
        crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;
        let result = self.do_run(term).await;
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        ratatui::restore();
        result
    }

    /// Run the application until it finds it should exit.
    ///
    /// Requires the terminal to already be set up.
    async fn do_run(&mut self, mut terminal: ratatui::DefaultTerminal) -> Result<()> {
        let tick_rate = Duration::from_secs_f32(0.25);
        let mut last_refresh = Instant::now();

        loop {
            terminal.draw(|frame| self.render(frame))?;

            if self.exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            let timeout = tick_rate.saturating_sub(last_refresh.elapsed());
            last_refresh = Instant::now();

            let result = futures::select! {
                result = self.handle_term_input(timeout).fuse() => result,
                _ = self.agent_update.notified().fuse() => Ok(()),
            };
            result?;
        }
    }

    /// Handle input on the terminal, with the given `poll_timeout`.
    async fn handle_term_input(&self, poll_timeout: Duration) -> Result<()> {
        use crossterm::event::Event;

        let result = {
            let mut events = self.events.lock().await;
            tokio::time::timeout(poll_timeout, events.next()).await
        };
        let Ok(result) = result else {
            // Timeout means no event available, which is fine
            return Ok(());
        };

        let Some(result) = result else {
            // Stream ended
            self.exit.store(true, Ordering::Relaxed);
            return Ok(());
        };

        match result? {
            Event::Key(key) => self.handle_key_event(key)?,
            Event::Mouse(mouse) => self.handle_mouse_event(mouse)?,
            _ => (),
        }

        Ok(())
    }

    /// Handle the given keyboard event.
    fn handle_key_event(&self, event: crossterm::event::KeyEvent) -> Result<()> {
        let mut ui = self.ui.lock().unwrap();

        if event.kind == crossterm::event::KeyEventKind::Press {
            if event.code == crossterm::event::KeyCode::Enter && event.modifiers.is_empty() {
                if !ui.input_area.is_empty() {
                    let _ = self
                        .user_message_submit
                        .send(ui.input_area.lines().join("\n"));
                    ui.input_area.clear();
                    return Ok(());
                }
            } else if event.code == crossterm::event::KeyCode::Esc {
                self.exit.store(true, Ordering::Relaxed);
                return Ok(());
            }
        }

        ui.input_area.input(event);
        Ok(())
    }

    /// Handle the given mouse event.
    fn handle_mouse_event(&self, event: crossterm::event::MouseEvent) -> Result<()> {
        let history_len = self.chat_history.lock().unwrap().lines().len();
        let mut ui = self.ui.lock().unwrap();

        match event.kind {
            crossterm::event::MouseEventKind::ScrollDown => {
                ui.history_scroll += 1;
                if ui.history_scroll.0 >= history_len.saturating_sub(ui.history_lines_on_screen) {
                    ui.history_scroll.0 = usize::MAX;
                }
            }

            crossterm::event::MouseEventKind::ScrollUp => {
                if ui.history_scroll.0 == usize::MAX {
                    ui.history_scroll.0 =
                        history_len.saturating_sub(ui.history_lines_on_screen + 1);
                } else {
                    ui.history_scroll -= 1;
                }
            }

            _ => (),
        }

        Ok(())
    }

    /// Render the current state onto the screen.
    fn render(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let layout = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Percentage(100),
            ratatui::layout::Constraint::Min(5),
        ])
        .split(area);

        let history_cell = layout[0];
        let input_cell = layout[1];

        let chat_history = self.chat_history.lock().unwrap();
        let history_line_count = history_cell.height.saturating_sub(2) as usize;
        let history_width = history_cell.width.saturating_sub(2) as usize;

        let mut history_lines_on_screen = 0;
        let scroll = self.ui.lock().unwrap().history_scroll.0;

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

        self.ui.lock().unwrap().history_lines_on_screen = history_lines_on_screen;

        let paragraph_content = ratatui::text::Text {
            alignment: None,
            style: Default::default(),
            lines: history_lines,
        };

        let paragraph = ratatui::widgets::Paragraph::new(paragraph_content).block(
            ratatui::widgets::Block::bordered().title(format!(
                "Chat: {:.1}k+{:.1}k",
                chat_history.token_usage().0 as f32 * 1.0e-3,
                chat_history.token_usage().1 as f32 * 1.0e-3
            )),
        );

        let ui = self.ui.lock().unwrap();

        frame.render_widget(paragraph, history_cell);
        frame.render_widget(&ui.input_area, input_cell);
    }
}

impl Drop for WorkBuddy {
    fn drop(&mut self) {
        if let Some(agent_thread) = self.agent_thread.take() {
            self.exit.store(true, Ordering::Relaxed);
            let _ = self.user_message_submit.send(String::new());
            let _ = agent_thread.join();
        }
    }
}
