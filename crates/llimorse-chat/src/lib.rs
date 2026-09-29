//! Framework for a llimorse-based chat application

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

pub mod agent;
pub mod log;
pub mod tools;
pub mod ui;

use agent::ChatAgent;
use anyhow::{Result, anyhow};
use futures::FutureExt;
use llimorse::Agent;
use llimorse::line_format::{ChatMessage, ToolCall};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
pub use tools::{SubagentNotifier, UserToolGate};
use ui::AgentUpdate;
pub use ui::{AgentId, UiState};

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
    pub fn new_with_history<F: FnOnce(&Agent) -> Result<I>>(
        agent: Agent,
        history: &[ChatMessage],
        ui_notifications: ui::NotificationChannel,
        create_ui: F,
    ) -> Result<Self> {
        let ui = create_ui(&agent)?;

        let (agent_notifications, recv_agent_notifications) = mpsc::unbounded_channel();
        let mut ui_notification_sender = ui_notifications.sender();

        if !history.is_empty() {
            Self::push_history(&mut ui_notification_sender, history, &agent)
                .map_err(|err| anyhow!("Failed to load history: {err}"))?;
        }

        let agent_thread = thread::spawn({
            move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async move {
                        let mut wba = ChatAgent::new(
                            recv_agent_notifications,
                            Arc::new(ui_notification_sender),
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
            ui_notifications: ui_notifications.into_receiver(),
        })
    }

    /// Create a new application state around `agent`.
    pub fn new<F: FnOnce(&Agent) -> Result<I>>(
        agent: Agent,
        ui_notifications: ui::NotificationChannel,
        create_ui: F,
    ) -> Result<Self> {
        Self::new_with_history(agent, &[], ui_notifications, create_ui)
    }

    /// Run the application until it finds it should exit.
    pub async fn run(&mut self) -> Result<()> {
        // Redraw at a steady cadence, independent of how busy the notification
        // channel is (a hot channel must not starve the idle tick, which e.g.
        // animates the spinner). The first tick is immediate, so this also
        // covers the initial draw.
        let mut redraw_interval = time::interval(Duration::from_millis(500));

        loop {
            let event_result = futures::select! {
                result = self.ui.get_event().fuse() => result.map(Some).map_err(Into::into),
                _ = redraw_interval.tick().fuse() => {
                    self.ui.notify(ui::Notification::Update).map_err(Into::into)?;
                    Ok(None)
                }
                notification = self.ui_notifications.recv().fuse() => {
                    if let Some(notification) = notification {
                        let exit = matches!(notification, ui::Notification::Exit);
                        self.ui.notify(notification).map_err(Into::into)?;
                        if exit {
                            // TODO: Fix this, it's a bit of a hack here
                            return Ok(());
                        }
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
                    ui::Event::Continue => {
                        let _ = self.agent_notifications.send(agent::Notification::Continue);
                    }
                }
            }
        }
    }

    /// Push everything in `history` onto `sender`, thus displaying it
    fn push_history(
        sender: &mut mpsc::UnboundedSender<ui::Notification>,
        history: &[ChatMessage],
        agent: &Agent,
    ) -> Result<()> {
        let agent_update = |update: AgentUpdate| {
            let _ = sender.send(ui::Notification::AgentUpdate {
                agent_id: AgentId::Main,
                content: update,
            });
        };

        let mut open_calls = HashMap::<String, ToolCall>::new();

        for message in history {
            match message {
                ChatMessage::System(_) => (), // ignored
                ChatMessage::User(msg) => {
                    agent_update(AgentUpdate::User {
                        prompt: msg.content.clone(),
                    });
                }
                ChatMessage::Assistant(msg) => {
                    if let Some(reasoning) = &msg.reasoning_content {
                        agent_update(AgentUpdate::Reasoning {
                            append: reasoning.clone(),
                        });
                    }
                    if let Some(content) = &msg.content {
                        agent_update(AgentUpdate::Content {
                            append: content.clone(),
                        });
                    }
                    if let Some(calls) = &msg.tool_calls {
                        for call in calls {
                            agent_update(AgentUpdate::ToolCallEx {
                                call: call.clone(),
                                display: agent.display_call(&call.call).to_string(),
                            });
                            open_calls.insert(call.id.clone(), call.clone());
                        }
                    }
                }
                ChatMessage::Tool(result) => {
                    let call = open_calls.remove(&result.tool_call_id).ok_or_else(|| {
                        anyhow!(
                            "Tool call result references unknown call ID {}",
                            result.tool_call_id
                        )
                    })?;

                    let result = &result.content;
                    if let Some(error) = result
                        .strip_prefix("TOOL CALL FAILED: ")
                        .or_else(|| result.strip_prefix("TOOL CALL REJECTED: "))
                    {
                        agent_update(AgentUpdate::ToolResultEx {
                            call,
                            display: Err(error.to_string()),
                        })
                    } else {
                        let display = agent.display_call_result(&call.call, result).to_string();
                        agent_update(AgentUpdate::ToolResultEx {
                            call,
                            display: Ok(display),
                        });
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
            let _ = self.agent_notifications.send(agent::Notification::Exit);
            let _ = agent_thread.join();
        }
    }
}
