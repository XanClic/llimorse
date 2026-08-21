//! Handle the agent-running part.

use super::history::{ChatHistory, HistoryEntryType};
use anyhow::Result;
use futures::StreamExt;
use llimo::StreamingChunk;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};

/// State of the agent-running part of WorkBuddy
pub(super) struct WorkBuddyAgent {
    /// The chat history as shared with the agent
    chat_history: Arc<Mutex<ChatHistory>>,

    /// User messages to be submitted to the LLM
    user_message_submit: mpsc::UnboundedReceiver<String>,

    /// Notify the UI to redraw
    update_ui: Arc<Notify>,

    /// Set once we are supposed to exit
    exit: Arc<AtomicBool>,
}

impl WorkBuddyAgent {
    /// Create a new instance.
    pub fn new(
        chat_history: Arc<Mutex<ChatHistory>>,
        user_message_submit: mpsc::UnboundedReceiver<String>,
        update_ui: Arc<Notify>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        WorkBuddyAgent {
            chat_history,
            user_message_submit,
            update_ui,
            exit,
        }
    }

    /// Run agent requests in a loop until the exit flag is set (or an error occurs).
    ///
    /// Will itself set the exit flag before returning.
    pub async fn run(&mut self, agent: llimo::Agent) -> Result<()> {
        let result = self.do_run(agent).await;
        self.exit.store(true, Ordering::Relaxed);
        result
    }

    /// Run agent requests in a loop until the exit flag is set (or an error occurs).
    async fn do_run(&mut self, mut agent: llimo::Agent) -> Result<()> {
        while let Some(message) = self.user_message_submit.recv().await {
            // The main loop will push an empty message to remind us to check the exit flag, so do
            // that here
            if self.exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            self.push_history(&message, HistoryEntryType::User);
            agent.push_user(message);
            while let Ok(message) = self.user_message_submit.try_recv() {
                self.push_history(&message, HistoryEntryType::User);
                agent.push_user(message);
            }

            loop {
                let mut result = agent.submit().await?;

                while let Some(chunk) = result.next().await {
                    self.process_chunk(chunk?);
                    if self.exit.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                }

                drop(result);

                self.chat_history
                    .lock()
                    .unwrap()
                    .set_token_usage(agent.token_usage());

                let tool_results = agent
                    .execute_pending_calls(
                        |agent, call| {
                            self.push_history(
                                &format!("[{}] {}\n", call.id, agent.display_call(&call.call)),
                                HistoryEntryType::ToolCall,
                            );
                            Ok(())
                        },
                        |agent, call, result| {
                            let name = match &call.call {
                                llimo::line_format::ToolCallParams::Function { function } => {
                                    &function.name
                                }
                                llimo::line_format::ToolCallParams::Custom { custom } => {
                                    &custom.name
                                }
                            };
                            match result {
                                Ok(result) => self.push_history(
                                    &format!(
                                        "=[{name}/{}]=> {}\n",
                                        call.id,
                                        agent.display_call_result(&call.call, result)
                                    ),
                                    HistoryEntryType::ToolResultOk,
                                ),
                                Err(err) => self.push_history(
                                    &format!("=[{name}/{}]=> {err}\n", call.id),
                                    HistoryEntryType::ToolResultErr,
                                ),
                            }
                            Ok(())
                        },
                    )
                    .await;

                let mut pending = !tool_results.is_empty();
                while let Ok(message) = self.user_message_submit.try_recv() {
                    self.push_history(&message, HistoryEntryType::User);
                    agent.push_user(message);
                    pending = true;
                }

                if !pending {
                    break;
                }
            }
        }

        Ok(())
    }

    /// Process the incoming `chunk` from the LLM (i.e. append it to the history).
    fn process_chunk(&self, chunk: StreamingChunk) {
        let (string, kind) = match chunk {
            StreamingChunk::Content(content) => (content, HistoryEntryType::Content),
            StreamingChunk::Reasoning(content) => (content, HistoryEntryType::Reasoning),
        };

        self.push_history(&string, kind);
    }

    /// Push the given string into the chat history, processing newlines
    fn push_history(&self, string: &str, kind: HistoryEntryType) {
        let mut history = self.chat_history.lock().unwrap();
        let mut force_new_line = false;
        for line in string.split('\n') {
            history.push(line, kind, force_new_line);
            force_new_line = true;
        }
        drop(history);

        self.update_ui.notify_one();
    }
}
