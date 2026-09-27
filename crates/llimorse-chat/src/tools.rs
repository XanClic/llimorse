//! Helpers for tools, for use with llimorse-chat

use crate::ui;
use llimorse_tools::{GateableToolParams, ToolGate};
use std::result;
use tokio::sync::{mpsc, oneshot};

/// A [`ToolGate`] that sends a permission request to the UI and waits for the user's decision.
///
/// Each call to [`ToolGate::permitted()`] sends a
/// [`ui::Notification::RequestPermission`] across the UI-notification channel and blocks until
/// the UI sends the user's decision back across the request's oneshot channel. If the UI is
/// dropped before a decision is made (e.g. the application exits while a prompt is pending),
/// the request is treated as a rejection.
#[derive(Debug)]
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
