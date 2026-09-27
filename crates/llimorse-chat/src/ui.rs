//! UI connector for llimorse-chat UIs

use crate::ChatHistory;
use llimorse::client::{ClientInfo, ClientState};
use parking_lot::RwLock;
use std::fmt;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// UI state for interacting with the llimorse-chat application
#[allow(async_fn_in_trait)]
pub trait UiState {
    /// Error type for the implementing `struct`.
    type Error: Into<anyhow::Error> + Send + Sync + 'static;

    /// Await an event on the UI.
    async fn get_event(&mut self) -> Result<Event, Self::Error>;

    /// Notify the UI about something.
    fn notify(&mut self, notification: Notification) -> Result<(), Self::Error>;
}

/// Application state level events that can come from the UI
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// Exit requested
    Exit,

    /// User submitted a message as input
    Input(String),

    /// Force submitting all queued prompts *right now*
    ForceSubmitQueued,
}

/// ID of a subagent, unique within one application
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct SubagentId(
    /// The raw ID
    usize,
);

impl SubagentId {
    /// Create a new subagent ID from the given integer.
    pub const fn new(id: usize) -> Self {
        Self(id)
    }
}

impl fmt::Display for SubagentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// ID of an agent: the main agent or one of its subagents
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub enum AgentId {
    /// The main agent
    Main,

    /// One of the main agent’s subagents
    Subagent(SubagentId),
}

/// Notifications to the UI
#[derive(Debug)]
pub enum Notification {
    /// Exit requested
    Exit,

    /// Update the interface
    Update,

    /// The given agent’s history has been updated
    UpdateAgent {
        /// The ID of the agent whose history was updated
        agent_id: AgentId,
    },

    /// User message queued to be submitted to the LLM
    PromptQueued(String),

    /// User message has been submitted to the LLM
    PromptSubmitted,

    /// The current prompt iteration is complete; the agent is now awaiting a new prompt
    AwaitingPrompt {
        /// Output of the just-completed prompt iteration (if any)
        response: Option<String>,
    },

    /// A tool call is requesting permission from the user
    RequestPermission {
        /// The prompt displayed to the user
        prompt: String,

        /// The user's decision, to be sent back across this channel
        approval: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    },

    /// A new subagent has been created
    SubagentCreated {
        /// Unique ID by which the subagent can be identified
        subagent_id: SubagentId,

        /// Prompt for the subagent
        prompt: String,

        /// The immutable information of the client to which the subagent is connected
        client_info: ClientInfo,

        /// The state of the client to which the subagent is connected
        client_state: Arc<RwLock<ClientState>>,

        /// Subagent’s chat history
        chat_history: Arc<Mutex<ChatHistory>>,
    },

    /// A subagent is done and has been dropped
    SubagentDropped {
        /// The subagent’s ID
        subagent_id: SubagentId,
    },
}

/// A handle to the UI-notification channel, which can be created before [`App`].
///
/// The sender side is cloneable, so it can be shared with objects that are created before the
/// application, such as [`UserToolGate`]. [`App::new()`] consumes this object, taking the
/// receiver side into itself.
pub struct NotificationChannel {
    /// The sender side (cloneable)
    sender: mpsc::UnboundedSender<Notification>,

    /// The receiver side (consumed by [`App`])
    receiver: mpsc::UnboundedReceiver<Notification>,
}

impl Default for NotificationChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationChannel {
    /// Create a new UI-notification channel.
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self { sender, receiver }
    }

    /// Clone the sender, for objects that need to notify the UI.
    pub fn sender(&self) -> mpsc::UnboundedSender<Notification> {
        self.sender.clone()
    }

    /// For [`App::new()`]: Consume and get the receiving end
    pub(crate) fn into_receiver(self) -> mpsc::UnboundedReceiver<Notification> {
        self.receiver
    }
}
