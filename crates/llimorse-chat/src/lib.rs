//! Framework for a llimorse-based chat application

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

pub mod agent;
pub mod history;
pub mod log;
pub mod ui;

use agent::ChatAgent;
use anyhow::Result;
use futures::FutureExt;
pub use history::ChatHistory;
use llimorse::line_format::ChatMessage;
use llimorse::{Agent, ChatListener};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
pub use ui::UiState;

/// The application state
pub struct App<I: UiState> {
    /// Agent thread running concurrently
    agent_thread: Option<JoinHandle<()>>,

    /// UI state
    ui: I,

    /// Notifications to the agent
    agent_notifications: mpsc::UnboundedSender<agent::Notification>,

    /// Notification to the UI
    ui_notifications: mpsc::UnboundedReceiver<ui::Notification>,
}

impl<I: UiState> App<I> {
    /// Create a new application state around `agent`, pre-feeding the chat log with `history`.
    pub fn new_with_history<
        L: ChatListener + Send + 'static,
        F: FnOnce(&Agent<L>, Arc<Mutex<ChatHistory>>) -> Result<I>,
    >(
        mut agent: Agent<L>,
        history: &[ChatMessage],
        create_ui: F,
    ) -> Result<Self> {
        let mut chat_history = ChatHistory::for_agent(&agent);
        for message in history {
            chat_history.push_raw(&agent, message);
        }
        chat_history.force_resolve_unresolved_tool_calls(&mut agent);

        let chat_history = Arc::new(Mutex::new(chat_history));

        let ui = create_ui(&agent, Arc::clone(&chat_history))?;

        let (agent_notifications, recv_agent_notifications) = mpsc::unbounded_channel();
        let (send_ui_notifications, ui_notifications) = mpsc::unbounded_channel();

        let agent_thread = thread::spawn({
            move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move {
                        let mut wba = ChatAgent::new(
                            chat_history,
                            recv_agent_notifications,
                            Arc::new(send_ui_notifications),
                        );
                        if let Err(err) = wba.run(agent).await {
                            panic!("Agent error: {err}");
                        }
                    })
            }
        });

        Ok(App {
            agent_thread: Some(agent_thread),

            ui,

            agent_notifications,
            ui_notifications,
        })
    }

    /// Create a new application state around `agent`.
    pub fn new<
        L: ChatListener + Send + 'static,
        F: FnOnce(&Agent<L>, Arc<Mutex<ChatHistory>>) -> Result<I>,
    >(
        agent: Agent<L>,
        create_ui: F,
    ) -> Result<Self> {
        Self::new_with_history(agent, &[], create_ui)
    }

    /// Run the application until it finds it should exit.
    pub async fn run(&mut self) -> Result<()> {
        // Draw once at start
        self.ui
            .notify(ui::Notification::Update)
            .map_err(Into::into)?;

        let redraw_min_time = Duration::from_millis(500);

        loop {
            let event_result = futures::select! {
                result = self.ui.get_event().fuse() => result.map(Some).map_err(Into::into),
                notification = time::timeout(redraw_min_time, self.ui_notifications.recv()).fuse() => {
                    if let Ok(Some(notification)) = notification {
                        let exit = notification == ui::Notification::Exit;
                        self.ui.notify(notification).map_err(Into::into)?;
                        if exit {
                            // TODO: Fix this, it's a bit of a hack here
                            return Ok(());
                        }
                    } else {
                        self.ui.notify(ui::Notification::Update).map_err(Into::into)?;
                    }
                    Ok(None)
                }
            };

            if let Some(event) = event_result? {
                match event {
                    ui::Event::Exit => {
                        let _ = self.agent_notifications.send(agent::Notification::Exit);
                        return Ok(());
                    }
                    ui::Event::Input(message) => {
                        self.ui
                            .notify(ui::Notification::PromptQueued(message.clone()))
                            .map_err(Into::into)?;

                        let _ = self
                            .agent_notifications
                            .send(agent::Notification::QueuePrompt(message));
                    }
                    ui::Event::ForceSubmitQueued => {
                        let _ = self
                            .agent_notifications
                            .send(agent::Notification::ForceSubmitQueued);
                    }
                }
            }
        }
    }
}

impl<I: UiState> Drop for App<I> {
    fn drop(&mut self) {
        if let Some(agent_thread) = self.agent_thread.take() {
            let _ = self.agent_notifications.send(agent::Notification::Exit);
            let _ = agent_thread.join();
        }
    }
}
