//! UI connector for llimorse-chat UIs

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
pub enum Event {
    /// Exit requested
    Exit,

    /// User submitted a message as input
    Input(String),
}

/// Notifications to the UI
pub enum Notification {
    /// Update the interface
    Update,

    /// User message queued to be submitted to the LLM
    PromptQueued(String),

    /// User message has been submitted to the LLM
    PromptSubmitted,
}
