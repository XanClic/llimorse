//! Helpers for tools, for use with llimorse-chat

use crate::ChatHistory;
use crate::history::HistoryEntryType;
use crate::ui::{self, AgentId, SubagentId};
use anyhow::Result;
use llimorse::line_format::ToolCall;
use llimorse::{Agent, StreamingChunk};
use llimorse_tools::{GateableToolParams, ToolGate};
use std::result;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// A [`ToolGate`] that sends a permission request to the UI and waits for the user's decision.
///
/// Each call to [`ToolGate::permitted()`] sends a
/// [`ui::Notification::RequestPermission`] across the UI-notification channel and blocks until
/// the UI sends the user's decision back across the request's oneshot channel. If the UI is
/// dropped before a decision is made (e.g. the application exits while a prompt is pending),
/// the request is treated as a rejection.
#[derive(Debug, Clone)]
pub struct UserToolGate {
    /// The UI-notification channel to send permission requests across.
    notifications: mpsc::UnboundedSender<ui::Notification>,
}

impl UserToolGate {
    /// Create a new gate over the given UI-notification channel.
    pub fn new(notifications: &ui::NotificationChannel) -> Self {
        Self {
            notifications: notifications.sender(),
        }
    }
}

impl ToolGate for UserToolGate {
    /// Send a permission request to the UI and await the user's decision.
    async fn permitted(&self, params: &dyn GateableToolParams) -> result::Result<(), String> {
        let (approval, wait) = oneshot::channel();
        self.notifications
            .send(ui::Notification::RequestPermission {
                prompt: params.to_string(),
                approval,
            })
            .map_err(|_| "UI notifications channel is closed".to_string())?;

        wait.await
            .map_err(|_| "permission request cancelled (UI closed)".to_string())?
    }
}

/// Connects the subagent tool to a llimorse-chat app
///
/// New subagents registered with this will send the appropriate notifications to the UI and thus
/// register themselves with it.
#[derive(Debug)]
pub struct SubagentNotifier {
    /// A notification channel to inform the llimorse-chat app of notifications
    notifications: mpsc::UnboundedSender<ui::Notification>,

    /// A counter to auto-generate distinct subagent IDs
    subagent_id_counter: AtomicUsize,
}

/// Connects a subagent to a llimorse-chat app
#[derive(Debug)]
pub struct SubagentConnector {
    /// The auto-generated subagent ID
    id: SubagentId,

    /// The subagents chat history
    chat_history: Arc<Mutex<ChatHistory>>,

    /// A notification channel to inform the llimorse-chat app of notifications
    notifications: mpsc::UnboundedSender<ui::Notification>,
}

impl SubagentNotifier {
    /// Create a new subagent notifier object for a llimorse-chat app.
    ///
    /// New subagents registered with this will send the appropriate notifications over
    /// `notifications` and thus register themselves with the UI.
    pub fn new(notifications: &ui::NotificationChannel) -> Self {
        SubagentNotifier {
            notifications: notifications.sender(),
            subagent_id_counter: 0.into(),
        }
    }
}

impl llimorse_tools::subagent::SubagentNotifier for SubagentNotifier {
    type Error = anyhow::Error;
    type Subagent = SubagentConnector;

    async fn created(&self, agent: &Agent, prompt: &str) -> Result<SubagentConnector> {
        let subagent = SubagentConnector::new(
            SubagentId::new(self.subagent_id_counter.fetch_add(1, Ordering::Relaxed)),
            self.notifications.clone(),
        );

        subagent
            .chat_history
            .lock()
            .unwrap()
            .push_lines(prompt, HistoryEntryType::User, true);

        let _ = self.notifications.send(ui::Notification::SubagentCreated {
            subagent_id: subagent.id,
            prompt: prompt.to_string(),
            client_state: agent.client_state_arc(),
            chat_history: Arc::clone(&subagent.chat_history),
        });

        Ok(subagent)
    }
}

impl SubagentConnector {
    /// Create a new subagent connector for a llimorse-chat app.
    ///
    /// `id` is the generated subagent ID, `notifications` is a channel to inform the UI of
    /// changes.
    fn new(id: SubagentId, notifications: mpsc::UnboundedSender<ui::Notification>) -> Self {
        SubagentConnector {
            id,
            chat_history: Default::default(),
            notifications,
        }
    }

    /// Notify the UI that this subagent’s history has been updated
    fn notify_update(&self) {
        let _ = self.notifications.send(ui::Notification::UpdateAgent {
            agent_id: AgentId::Subagent(self.id),
        });
    }
}

impl Drop for SubagentConnector {
    fn drop(&mut self) {
        let _ = self.notifications.send(ui::Notification::SubagentDropped {
            subagent_id: self.id,
        });
    }
}

impl llimorse_tools::subagent::SubagentConnector for SubagentConnector {
    type Error = anyhow::Error;

    async fn push_chunk(&self, chunk: StreamingChunk) {
        let (string, kind) = match chunk {
            StreamingChunk::Content(c) => (c, HistoryEntryType::Content),
            StreamingChunk::Reasoning(r) => (r, HistoryEntryType::Reasoning),
        };
        self.chat_history
            .lock()
            .unwrap()
            .push_lines(&string, kind, false);
        self.notify_update();
    }

    fn tool_call(&self, agent: &Agent, call: &ToolCall) -> Result<()> {
        self.chat_history.lock().unwrap().push_lines(
            &format!("[{}] {}\n", call.id, agent.display_call(&call.call)),
            HistoryEntryType::ToolCall,
            true,
        );
        self.notify_update();
        Ok(())
    }

    fn tool_result(&self, agent: &Agent, call: &ToolCall, result: &Result<String>) -> Result<()> {
        let name = call.call.name();
        let id = &call.id;
        match result {
            Ok(result) => self.chat_history.lock().unwrap().push_lines(
                &format!(
                    "=[{name}/{id}]=> {}\n",
                    agent.display_call_result(&call.call, result)
                ),
                HistoryEntryType::ToolResultOk,
                true,
            ),
            Err(err) => self.chat_history.lock().unwrap().push_lines(
                &format!("=[{name}/{id}]=> {err}\n"),
                HistoryEntryType::ToolResultErr,
                true,
            ),
        }

        self.notify_update();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui;

    /// Test parameters displaying as a fixed command line.
    #[derive(Debug)]
    struct TestParams;

    impl std::fmt::Display for TestParams {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "echo hello")
        }
    }

    #[tokio::test]
    async fn approval_round_trip() {
        let notifications = ui::NotificationChannel::new();
        let gate = UserToolGate::new(&notifications);
        let mut receiver = notifications.into_receiver();
        let params = TestParams;

        let (prompt, decision) = futures::join!(
            async {
                let ui::Notification::RequestPermission { prompt, approval } =
                    receiver.recv().await.expect("channel closed")
                else {
                    panic!("not a permission request");
                };
                approval.send(Ok(())).unwrap();
                prompt
            },
            async { gate.permitted(&params).await },
        );

        assert_eq!(prompt, "echo hello");
        assert!(decision.is_ok());
    }

    #[tokio::test]
    async fn dropped_ui_cancels_the_request() {
        let notifications = ui::NotificationChannel::new();
        let gate = UserToolGate::new(&notifications);
        let params = TestParams;

        // Simulate the UI being dropped without a decision
        drop(notifications.into_receiver());

        let decision = gate.permitted(&params).await;
        assert!(decision.is_err());
    }
}
