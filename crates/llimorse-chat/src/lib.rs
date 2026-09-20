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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tokio::sync::{Notify, mpsc};
pub use ui::UiState;

/// The application state
pub struct App<I: UiState> {
    /// Agent thread running concurrently
    agent_thread: Option<JoinHandle<()>>,

    /// UI state
    ui: I,

    /// Submit user messages to the LLM
    user_message_submit: mpsc::UnboundedSender<String>,

    /// Notification from the agent to redraw the UI
    agent_update: Arc<Notify>,

    /// Set once we are supposed to exit
    exit: Arc<AtomicBool>,
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
        let exit = Arc::new(AtomicBool::new(false));

        let ui = create_ui(&agent, Arc::clone(&chat_history))?;

        let (user_message_send, user_message_recv) = mpsc::unbounded_channel();
        let agent_update = Arc::new(Notify::new());

        let agent_thread = thread::spawn({
            let update_ui = Arc::clone(&agent_update);
            let exit = Arc::clone(&exit);
            move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move {
                        let mut wba =
                            ChatAgent::new(chat_history, user_message_recv, update_ui, exit);
                        if let Err(err) = wba.run(agent).await {
                            panic!("Agent error: {err}");
                        }
                    })
            }
        });

        Ok(App {
            agent_thread: Some(agent_thread),

            ui,

            user_message_submit: user_message_send,
            agent_update,

            exit,
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
        while !self.exit.load(Ordering::Relaxed) {
            let event_result = futures::select! {
                result = self.ui.get_event().fuse() => result.map(Some).map_err(Into::into),
                _ = self.agent_update.notified().fuse() => Ok(None)
            };

            if let Some(event) = event_result? {
                match event {
                    ui::Event::Exit => self.exit.store(true, Ordering::Relaxed),
                    ui::Event::Input(message) => {
                        let _ = self.user_message_submit.send(message);
                    }
                }
            }
        }

        Ok(())
    }
}

impl<I: UiState> Drop for App<I> {
    fn drop(&mut self) {
        if let Some(agent_thread) = self.agent_thread.take() {
            self.exit.store(true, Ordering::Relaxed);
            let _ = self.user_message_submit.send(String::new());
            let _ = agent_thread.join();
        }
    }
}
