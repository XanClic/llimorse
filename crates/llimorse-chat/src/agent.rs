//! Handle the agent-running part.

use super::history::{ChatHistory, HistoryEntryType};
use super::ui;
use anyhow::Result;
use futures::{FutureExt, StreamExt};
use llimorse::{ChatListener, StreamingChunk};
use std::collections::VecDeque;
use std::mem;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// State of the agent-running part of the llimorse-based chat application
pub(super) struct ChatAgent {
    /// The chat history as shared with the agent
    chat_history: Arc<Mutex<ChatHistory>>,

    /// Notifications from the application
    notifications: mpsc::UnboundedReceiver<Notification>,

    /// User messages queued
    queued_messages: VecDeque<String>,

    /// Notify the UI to redraw
    ui_notifications: Arc<mpsc::UnboundedSender<ui::Notification>>,
}

/// Notifications to be sent to the agent
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Notification {
    /// Notify of an exit event
    Exit,

    /// Queue the given prompt until the next cycle where we can naturally submit it
    QueuePrompt(String),

    /// Force submitting all queued prompts *right now*
    ForceSubmitQueued,
}

impl ChatAgent {
    /// Create a new instance.
    pub fn new(
        chat_history: Arc<Mutex<ChatHistory>>,
        notifications: mpsc::UnboundedReceiver<Notification>,
        ui_notifications: Arc<mpsc::UnboundedSender<ui::Notification>>,
    ) -> Self {
        ChatAgent {
            chat_history,
            notifications,
            queued_messages: VecDeque::new(),
            ui_notifications,
        }
    }

    /// Run agent requests in a loop until the exit flag is set (or an error occurs).
    ///
    /// Will itself set the exit flag before returning.
    pub async fn run(&mut self, agent: llimorse::Agent<impl ChatListener>) -> Result<()> {
        let result = self.do_run(agent).await;
        let _ = self.ui_notifications.send(ui::Notification::Exit);
        result
    }

    /// Process incoming notifications until finding a user or exit message.
    ///
    /// Return `false` if the channel is closed (or an exit event is received).
    ///
    /// The caller **must** submit `self.queued_messages` immediately (because this function
    /// ignores `ForceSubmitQueued`, assuming the caller will do so).
    async fn process_notifications(&mut self) -> bool {
        while let Some(notification) = self.notifications.recv().await {
            match notification {
                Notification::Exit => return false,

                Notification::QueuePrompt(p) => {
                    self.queued_messages.push_back(p);
                    return true;
                }

                // Ignore this, the caller will submit them all right now anyway.
                Notification::ForceSubmitQueued => (),
            }
        }

        false
    }

    /// Process all available incoming notifications (non-blocking)
    ///
    /// Return `false` if an exit event is received.
    ///
    /// The caller **must** submit `self.queued_messages` immediately (because this function
    /// ignores `ForceSubmitQueued`, assuming the caller will do so).
    fn process_available_notifications(&mut self) -> bool {
        while let Ok(notification) = self.notifications.try_recv() {
            match notification {
                Notification::Exit => return false,

                Notification::QueuePrompt(p) => self.queued_messages.push_back(p),

                // Ignore this, the caller will submit them all right now anyway.
                Notification::ForceSubmitQueued => (),
            }
        }

        true
    }

    /// Push all messages currently in `self.queued_messages` onto the agent/chat history.
    ///
    /// This does not yet submit a request to the agent.
    fn submit_queued_user_messages(&mut self, agent: &mut llimorse::Agent<impl ChatListener>) {
        let messages = mem::take(&mut self.queued_messages);
        for message in messages {
            let _ = self
                .ui_notifications
                .send(ui::Notification::PromptSubmitted);
            self.push_history(&message, HistoryEntryType::User);
            agent.push_user(message);
        }
    }

    /// Run agent requests in a loop until the exit flag is set (or an error occurs).
    async fn do_run(&mut self, mut agent: llimorse::Agent<impl ChatListener>) -> Result<()> {
        while self.process_notifications().await && self.process_available_notifications() {
            self.submit_queued_user_messages(&mut agent);

            loop {
                let mut streaming = agent.submit().await?;

                loop {
                    futures::select! {
                        chunk = streaming.next() => {
                            if let Some(chunk) = chunk {
                                self.process_chunk(chunk?);
                            } else {
                                break;
                            }
                        }

                        notification = self.notifications.recv().fuse() => {
                            if let Some(notification) = notification {
                                 match notification {
                                     Notification::Exit => return Ok(()),
                                     Notification::QueuePrompt(p) => {
                                         self.queued_messages.push_back(p);
                                     }
                                     Notification::ForceSubmitQueued => {
                                        streaming.force_finalize();
                                        break;
                                     }
                                 }
                            }
                        }
                    }
                }

                drop(streaming);

                let mut pending = agent
                    .execute_pending_calls(
                        |agent, call| {
                            self.push_history(
                                &format!("[{}] {}\n", call.id, agent.display_call(&call.call)),
                                HistoryEntryType::ToolCall,
                            );
                            Ok(())
                        },
                        |agent, call, result| {
                            let name = call.call.name();
                            let id = &call.id;
                            match result {
                                Ok(result) => self.push_history(
                                    &format!(
                                        "=[{name}/{id}]=> {}\n",
                                        agent.display_call_result(&call.call, result)
                                    ),
                                    HistoryEntryType::ToolResultOk,
                                ),
                                Err(err) => self.push_history(
                                    &format!("=[{name}/{id}]=> {err}\n"),
                                    HistoryEntryType::ToolResultErr,
                                ),
                            }
                            Ok(())
                        },
                    )
                    .await;

                if !self.process_available_notifications() {
                    return Ok(());
                }
                if !self.queued_messages.is_empty() {
                    self.submit_queued_user_messages(&mut agent);
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
        // Put user messages on a new line, always, as they can never be streamed content
        history.push_lines(string, kind, kind == HistoryEntryType::User);
        drop(history);

        let _ = self.ui_notifications.send(ui::Notification::Update);
    }
}
