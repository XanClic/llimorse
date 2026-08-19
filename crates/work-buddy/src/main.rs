//! A very nice work buddy, taking care of your tickets for you. More or less.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

use anyhow::{Result, anyhow};
use clap::{CommandFactory, FromArgMatches, Parser};
use futures::{FutureExt, StreamExt};
use llimo::StreamingChunk;
use std::borrow::Cow;
use std::num::Saturating;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{fs, io};
use tokio::sync::{Mutex as TokioMutex, Notify, mpsc};

/// Command-line arguments for WorkBuddy
#[derive(Parser)]
struct Args {
    /// llama.cpp server base URL
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    llama_url: String,

    /// Base URL of a SearXNG instance for the web_search tool
    #[arg(long, default_value = "http://127.0.0.1:8888")]
    searxng_url: String,

    /// Path to a file containing the system prompt
    #[arg(long)]
    system: Option<PathBuf>,

    /// Enable debug-level logging
    #[arg(long)]
    debug: bool,
}

/// Return a random “witty” tag line for --help
fn tagline() -> &'static str {
    const TAGLINES: [&str; 4] = [
        "because someone has to care about your tickets, and it won’t be you",
        "turning existential dread into well-formatted Jira tickets since 2026",
        "proof of work for proof of employment",
        "comprehensive documentation for the comprehensively unmotivated",
    ];

    TAGLINES[fastrand::usize(..TAGLINES.len())]
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::from_arg_matches(
        &Args::command()
            .about(format!("WorkBuddy! …{}", tagline()))
            .get_matches(),
    )
    .unwrap();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                if args.debug {
                    "work_buddy=debug"
                } else {
                    "work_buddy=info"
                }
                .into()
            }),
        )
        .init();

    let system_prompt = args.system.map(fs::read_to_string).transpose()?;

    let llm = llimo::Client::new(&args.llama_url);
    let mut agent = llimo::Agent::new(llm);

    agent.add_tool(llimo::tools::WebSearch::new(&args.searxng_url));

    if let Some(system_prompt) = system_prompt {
        agent.push_system(system_prompt);
    }

    color_eyre::install().map_err(|err| anyhow!("Failed to install crash handler: {err}"))?;
    let term = ratatui::init();
    crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;
    let result = WorkBuddy::new(agent).run(term).await;
    crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture)?;
    ratatui::restore();
    result
}

/// Type of a chat history entry (for formatting)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryEntryType {
    /// Nothing at all (separator)
    Empty,

    /// User message
    User,

    /// Assistant reply (actual content)
    Content,

    /// Assistant reasoning
    Reasoning,

    /// Tool call (with parameters)
    ToolCall,

    /// Tool call results (on success)
    ToolResultOk,

    /// Tool call error
    ToolResultErr,
}

impl From<HistoryEntryType> for ratatui::style::Style {
    fn from(ct: HistoryEntryType) -> Self {
        match ct {
            HistoryEntryType::Empty => ratatui::style::Style::default(),
            HistoryEntryType::User => ratatui::style::Style::default().bold().magenta(),
            HistoryEntryType::Content => ratatui::style::Style::default().bold(),
            HistoryEntryType::Reasoning => ratatui::style::Style::default().italic(),
            HistoryEntryType::ToolCall => ratatui::style::Style::default().blue(),
            HistoryEntryType::ToolResultOk => ratatui::style::Style::default().green(),
            HistoryEntryType::ToolResultErr => ratatui::style::Style::default().bold().red(),
        }
    }
}

impl From<HistoryEntryType> for ratatui::layout::Alignment {
    fn from(ct: HistoryEntryType) -> Self {
        match ct {
            HistoryEntryType::Empty => ratatui::layout::Alignment::Left,
            HistoryEntryType::User => ratatui::layout::Alignment::Right,
            HistoryEntryType::Content => ratatui::layout::Alignment::Left,
            HistoryEntryType::Reasoning => ratatui::layout::Alignment::Left,
            HistoryEntryType::ToolCall => ratatui::layout::Alignment::Left,
            HistoryEntryType::ToolResultOk => ratatui::layout::Alignment::Left,
            HistoryEntryType::ToolResultErr => ratatui::layout::Alignment::Left,
        }
    }
}

/// The application state
struct WorkBuddy {
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

/// State of the agent-running part of WorkBuddy
struct WorkBuddyAgent {
    /// The chat history as shared with the agent
    chat_history: Arc<Mutex<ChatHistory>>,

    /// User messages to be submitted to the LLM
    user_message_submit: mpsc::UnboundedReceiver<String>,

    /// Notify the UI to redraw
    update_ui: Arc<Notify>,

    /// Set once we are supposed to exit
    exit: Arc<AtomicBool>,
}

/// Chat history data
struct ChatHistory {
    /// Full chat history (split into lines, but not broken by terminal width)
    lines: Vec<(String, HistoryEntryType)>,

    /// Token usage as last reported by the LLM
    token_usage: (usize, usize),
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
    fn new(agent: llimo::Agent) -> Self {
        let mut input_area: ratatui_textarea::TextArea<'static> = Default::default();
        input_area.set_cursor_line_style(Default::default());
        input_area.set_block(ratatui::widgets::Block::bordered().title("Input"));
        input_area.set_wrap_mode(ratatui_textarea::WrapMode::Word);

        let chat_history = Arc::new(Mutex::new(ChatHistory {
            lines: Vec::new(),
            token_usage: (0, 0),
        }));
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
                        let mut wba = WorkBuddyAgent {
                            chat_history,
                            user_message_submit: user_message_recv,
                            update_ui,
                            exit,
                        };
                        if let Err(err) = wba.run(agent).await {
                            panic!("Agent error: {err}");
                        }
                        wba.exit.store(true, Ordering::Relaxed);
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
    async fn run(&mut self, mut terminal: ratatui::DefaultTerminal) -> Result<()> {
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
        let history_len = self.chat_history.lock().unwrap().lines.len();
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

    /// Helper function to convert the given iterator of `HistoryEntryType`-annotated lines into
    /// ratatui lines.
    fn into_ratatui_lines<'a, I: Iterator<Item = (Cow<'a, str>, HistoryEntryType)>>(
        iter: I,
    ) -> Vec<ratatui::text::Line<'a>> {
        iter.map(|line| ratatui::text::Line {
            style: line.1.into(),
            alignment: Some(line.1.into()),
            spans: vec![ratatui::text::Span {
                style: Default::default(),
                content: line.0,
            }],
        })
        .collect()
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
            let mut history_lines = Self::into_ratatui_lines(
                chat_history
                    .lines
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
            Self::into_ratatui_lines(
                chat_history
                    .lines
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
                chat_history.token_usage.0 as f32 * 1.0e-3,
                chat_history.token_usage.1 as f32 * 1.0e-3
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

impl WorkBuddyAgent {
    /// Run agent requests in a loop until the exit flag is set (or an error occurs).
    async fn run(&mut self, mut agent: llimo::Agent) -> Result<()> {
        while let Some(message) = self.user_message_submit.recv().await {
            self.push_history(&message, HistoryEntryType::User);
            agent.push_user(message);
            while let Ok(message) = self.user_message_submit.try_recv() {
                self.push_history(&message, HistoryEntryType::User);
                agent.push_user(message);
            }

            loop {
                let mut result = agent.submit().await?;

                while let Some(chunk) = result.next().await {
                    self.process_chunk(chunk?);
                    if self.exit.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                }

                drop(result);

                self.chat_history.lock().unwrap().token_usage = agent.token_usage();

                let tool_results = agent
                    .execute_pending_calls(
                        |agent, call| {
                            self.push_history(
                                &format!("[{}] {}\n", call.id, agent.display_call(&call.call)),
                                HistoryEntryType::ToolCall,
                            );
                            Ok(())
                        },
                        |agent, call, result| {
                            let name = match &call.call {
                                llimo::line_format::ToolCallParams::Function { function } => {
                                    &function.name
                                }
                                llimo::line_format::ToolCallParams::Custom { custom } => {
                                    &custom.name
                                }
                            };
                            match result {
                                Ok(result) => self.push_history(
                                    &format!(
                                        "=[{name}/{}]=> {}\n",
                                        call.id,
                                        agent.display_call_result(&call.call, result)
                                    ),
                                    HistoryEntryType::ToolResultOk,
                                ),
                                Err(err) => self.push_history(
                                    &format!("=[{name}/{}]=> {err}\n", call.id),
                                    HistoryEntryType::ToolResultErr,
                                ),
                            }
                            Ok(())
                        },
                    )
                    .await;

                let mut pending = !tool_results.is_empty();
                while let Ok(message) = self.user_message_submit.try_recv() {
                    self.push_history(&message, HistoryEntryType::User);
                    agent.push_user(message);
                    pending = true;
                }

                if !pending {
                    break;
                }
            }
        }

        Ok(())
    }

    /// Process the incoming `chunk` from the LLM (i.e. append it to the history).
    fn process_chunk(&self, chunk: StreamingChunk) {
        let (string, kind) = match chunk {
            StreamingChunk::Content(content) => (content, HistoryEntryType::Content),
            StreamingChunk::Reasoning(content) => (content, HistoryEntryType::Reasoning),
        };

        self.push_history(&string, kind);
    }

    /// Push the given string into the chat history, processing newlines
    fn push_history(&self, string: &str, kind: HistoryEntryType) {
        let mut history = self.chat_history.lock().unwrap();
        let mut force_new_line = false;
        for line in string.split('\n') {
            history.push(line, kind, force_new_line);
            force_new_line = true;
        }
        drop(history);

        self.update_ui.notify_one();
    }
}

impl ChatHistory {
    /// Append the given string of type `ct` to the history.
    ///
    /// If `force_new_line` is true, append it to the prior line if the type matches; if it is
    /// false, always create a new line.
    fn push(&mut self, string: &str, kind: HistoryEntryType, force_new_line: bool) {
        if let Some(last) = self.lines.last_mut() {
            if last.1 == kind && !force_new_line {
                last.0.push_str(string);
                return;
            } else if last.1 != kind {
                // Convert empty lines of different type to type `Empty`, otherwise append `Empty`
                if last.0.is_empty() {
                    last.1 = HistoryEntryType::Empty;
                } else {
                    self.lines.push((String::new(), HistoryEntryType::Empty));
                }
            }
        }

        self.lines.push((string.to_string(), kind));
    }
}
