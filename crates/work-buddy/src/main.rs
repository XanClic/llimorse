//! A very nice work buddy, taking care of your tickets for you. More or less.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

use anyhow::{Result, anyhow};
use clap::{CommandFactory, FromArgMatches, Parser};
use futures::StreamExt;
use llimo::StreamingChunk;
use std::borrow::Cow;
use std::num::Saturating;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{fs, io};

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
    /// The agent (connection to the LLM plus tools)
    ///
    /// Is `None` if currently processing a request.
    agent: Option<llimo::Agent>,

    /// Full chat history (split into lines, but not broken by terminal width)
    chat_history: Mutex<Vec<(String, HistoryEntryType)>>,

    /// First line of the chat history to show (`usize::MAX` to follow the tail)
    history_scroll: Saturating<usize>,

    /// How many lines (elements of `chat_history`) are visible on screen right now
    history_lines_on_screen: Mutex<usize>,

    /// User message input widget
    input_area: ratatui_textarea::TextArea<'static>,

    /// User message to send as a request to the LLM
    ///
    /// Filled once `input_area` is submitted via the Enter key.
    input: Option<String>,

    /// Token usage as last reported by the LLM
    token_usage: (usize, usize),

    /// Set once we are supposed to exit
    exit: bool,
}

impl WorkBuddy {
    /// Create a new application state around `agent`.
    fn new(agent: llimo::Agent) -> Self {
        let mut input_area: ratatui_textarea::TextArea<'static> = Default::default();
        input_area.set_cursor_line_style(Default::default());
        input_area.set_block(ratatui::widgets::Block::bordered().title("Input"));
        input_area.set_wrap_mode(ratatui_textarea::WrapMode::Word);

        WorkBuddy {
            agent: Some(agent),
            chat_history: Mutex::new(Vec::new()),
            history_scroll: Saturating(0),
            history_lines_on_screen: Mutex::new(0),
            input_area,
            input: None,
            token_usage: (0, 0),
            exit: false,
        }
    }

    /// Run the application until it finds it should exit.
    async fn run(mut self, mut terminal: ratatui::DefaultTerminal) -> Result<()> {
        let tick_rate = Duration::from_secs_f32(0.25);
        let mut last_refresh = Instant::now();

        loop {
            let user_message = loop {
                terminal.draw(|frame| self.render(frame))?;

                if self.exit {
                    return Ok(());
                }

                if let Some(input) = self.input.take() {
                    break input;
                }

                let timeout = tick_rate.saturating_sub(last_refresh.elapsed());
                last_refresh = Instant::now();

                self.handle_term_input(timeout)?;
            };

            self.agent.as_mut().unwrap().push_user(user_message);
            self.agent_loop(&mut terminal).await?;
        }
    }

    /// Handle input on the terminal, with the given `poll_timeout`.
    fn handle_term_input(&mut self, poll_timeout: Duration) -> Result<()> {
        if crossterm::event::poll(poll_timeout)? {
            use crossterm::event::Event;

            match crossterm::event::read()? {
                Event::Key(key) => self.handle_key_event(key)?,
                Event::Mouse(mouse) => self.handle_mouse_event(mouse)?,
                _ => (),
            }
        }

        Ok(())
    }

    /// Run a request on the agent to completion.
    async fn agent_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let mut agent = self.agent.take().unwrap();

        self.history_scroll.0 = usize::MAX;

        loop {
            {
                let mut result = agent.submit().await?;

                while let Some(chunk) = result.next().await {
                    self.process_chunk(chunk?).await?;
                    self.handle_term_input(Duration::from_secs(0))?;
                    if self.exit {
                        return Ok(());
                    }
                    terminal.draw(|frame| self.render(frame))?;
                }
            }

            self.token_usage = agent.token_usage();
            terminal.draw(|frame| self.render(frame))?;

            let tool_results = agent
                .execute_pending_calls(
                    |agent, call| {
                        self.append_to_history(
                            &format!("[{}] {}", call.id, agent.display_call(&call.call)),
                            HistoryEntryType::ToolCall,
                            true,
                        );
                        terminal.draw(|frame| self.render(frame))?;
                        Ok(())
                    },
                    |agent, call, result| {
                        let name = match &call.call {
                            llimo::line_format::ToolCallParams::Function { function } => {
                                &function.name
                            }
                            llimo::line_format::ToolCallParams::Custom { custom } => &custom.name,
                        };
                        match result {
                            Ok(result) => self.append_to_history(
                                &format!(
                                    "=[{name}/{}]=> {}",
                                    call.id,
                                    agent.display_call_result(&call.call, result)
                                ),
                                HistoryEntryType::ToolResultOk,
                                true,
                            ),
                            Err(err) => self.append_to_history(
                                &format!("=[{name}/{}]=> {err}", call.id),
                                HistoryEntryType::ToolResultErr,
                                true,
                            ),
                        }
                        Ok(())
                    },
                )
                .await;

            if tool_results.is_empty() {
                break;
            }
        }

        self.agent = Some(agent);
        Ok(())
    }

    /// Append the given string of type `ct` to the history.
    ///
    /// If `force_new_line` is true, append it to the prior line if the type matches; if it is
    /// false, always create a new line.
    fn append_to_history(&self, string: &str, ct: HistoryEntryType, force_new_line: bool) {
        let mut chat_history = self.chat_history.lock().unwrap();

        if let Some(last) = chat_history.last_mut() {
            if last.1 == ct && !force_new_line {
                last.0.push_str(string);
                return;
            } else if last.1 != ct {
                chat_history.push((String::new(), HistoryEntryType::Empty));
            }
        }
        chat_history.push((string.to_string(), ct));
    }

    /// Process the incoming `chunk` from the LLM (i.e. append it to the history).
    async fn process_chunk(&mut self, chunk: StreamingChunk) -> Result<()> {
        let (string, kind) = match chunk {
            StreamingChunk::Content(content) => (content, HistoryEntryType::Content),
            StreamingChunk::Reasoning(content) => (content, HistoryEntryType::Reasoning),
        };

        let mut force_new_line = false;
        for line in string.split('\n') {
            self.append_to_history(line, kind, force_new_line);
            force_new_line = true;
        }

        Ok(())
    }

    /// Handle the given keyboard event.
    fn handle_key_event(&mut self, event: crossterm::event::KeyEvent) -> Result<()> {
        if event.kind == crossterm::event::KeyEventKind::Press {
            if event.code == crossterm::event::KeyCode::Enter && event.modifiers.is_empty() {
                if !self.input_area.is_empty() {
                    let input = self.input_area.lines().join("\n");
                    // Join plus split because otherwise the borrow checker is mad about `self` use
                    for line in input.split("\n") {
                        self.append_to_history(line, HistoryEntryType::User, true);
                    }
                    self.input = Some(input);
                    self.input_area.clear();
                    return Ok(());
                }
            } else if event.code == crossterm::event::KeyCode::Esc {
                self.exit = true;
                return Ok(());
            }
        }

        self.input_area.input(event);
        Ok(())
    }

    /// Handle the given mouse event.
    fn handle_mouse_event(&mut self, event: crossterm::event::MouseEvent) -> Result<()> {
        match event.kind {
            crossterm::event::MouseEventKind::ScrollDown => {
                let history_len = self.chat_history.lock().unwrap().len();
                let lines_on_screen = *self.history_lines_on_screen.lock().unwrap();

                self.history_scroll += 1;
                if self.history_scroll.0 >= history_len.saturating_sub(lines_on_screen) {
                    self.history_scroll.0 = usize::MAX;
                }
            }

            crossterm::event::MouseEventKind::ScrollUp => {
                let history_len = self.chat_history.lock().unwrap().len();
                let lines_on_screen = *self.history_lines_on_screen.lock().unwrap();

                if self.history_scroll.0 == usize::MAX {
                    self.history_scroll.0 = history_len.saturating_sub(lines_on_screen + 1);
                } else {
                    self.history_scroll -= 1;
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

        let mut history_lines_on_screen = self.history_lines_on_screen.lock().unwrap();
        *history_lines_on_screen = 0;

        let history_lines = if self.history_scroll.0 == usize::MAX {
            let mut history_lines = Self::into_ratatui_lines(
                chat_history
                    .iter()
                    .rev()
                    .flat_map(|line| {
                        *history_lines_on_screen += 1; // diabolical
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
                    .iter()
                    .skip(self.history_scroll.0)
                    .flat_map(|line| {
                        *history_lines_on_screen += 1; // diabolical
                        textwrap::wrap(&line.0, history_width)
                            .into_iter()
                            .map(|l| (l, line.1))
                    })
                    .take(history_line_count),
            )
        };

        let chat_history = ratatui::text::Text {
            alignment: None,
            style: Default::default(),
            lines: history_lines,
        };

        let paragraph = ratatui::widgets::Paragraph::new(chat_history).block(
            ratatui::widgets::Block::bordered().title(format!(
                "Chat: {:.1}k+{:.1}k",
                self.token_usage.0 as f32 * 1.0e-3,
                self.token_usage.1 as f32 * 1.0e-3
            )),
        );

        frame.render_widget(paragraph, history_cell);
        frame.render_widget(&self.input_area, input_cell);
    }
}
