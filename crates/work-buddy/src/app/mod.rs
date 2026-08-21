//! Implementation of the WorkBuddy application.

mod agent;
mod history;
mod ui;

use agent::WorkBuddyAgent;
use anyhow::Result;
use futures::FutureExt;
use history::ChatHistory;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use ui::TermState;

/// The application state
pub struct WorkBuddy {
    /// Agent thread running concurrently
    agent_thread: Option<JoinHandle<()>>,

    /// UI state
    ui: TermState,

    /// Submit user messages to the LLM
    user_message_submit: mpsc::UnboundedSender<String>,

    /// Notification from the agent to redraw the UI
    agent_update: Arc<Notify>,

    /// Set once we are supposed to exit
    exit: Arc<AtomicBool>,
}

impl WorkBuddy {
    /// Create a new application state around `agent`.
    pub fn new(agent: llimo::Agent) -> Self {
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

            ui: TermState::new(chat_history),

            user_message_submit: user_message_send,
            agent_update,

            exit,
        }
    }

    /// Run the application until it finds it should exit.
    pub async fn run(&mut self) -> Result<()> {
        let tick_rate = Duration::from_secs_f32(0.25);
        let mut last_refresh = Instant::now();

        while !self.exit.load(Ordering::Relaxed) {
            self.ui.draw()?;

            let timeout = tick_rate.saturating_sub(last_refresh.elapsed());
            last_refresh = Instant::now();

            let event_result = futures::select! {
                result = self.ui.handle_term_input(timeout).fuse() => result,
                _ = self.agent_update.notified().fuse() => Ok(None),
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

impl Drop for WorkBuddy {
    fn drop(&mut self) {
        if let Some(agent_thread) = self.agent_thread.take() {
            self.exit.store(true, Ordering::Relaxed);
            let _ = self.user_message_submit.send(String::new());
            let _ = agent_thread.join();
        }
    }
}
