//! UI connector for llimo-chat UIs

/// UI state for interacting with the llimo-chat application
#[allow(async_fn_in_trait)]
pub trait UiState {
    /// Error type for the implementing `struct`.
    type Error: Into<anyhow::Error> + Send + Sync + 'static;

    /// Await an event on the UI.
    ///
    /// Notably, the future is dropped whenever the agent has an update, and then later re-called,
    /// so if the UI needs redrawn on updates, it may be done in this function.
    async fn get_event(&mut self) -> Result<Event, Self::Error>;
}

/// Application state level events that can come from the UI
pub enum Event {
    /// Exit requested
    Exit,

    /// User submitted a message as input
    Input(String),
}
