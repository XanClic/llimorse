//! Handle the agent-running part.

use super::ui::{self, AgentId, AgentUpdate};
use anyhow::Result;
use futures::{FutureExt, StreamExt};
use llimorse::line_format::{ChatMessage, ToolCall};
use llimorse::{Agent, StreamingChunk};
use std::collections::VecDeque;
use std::mem;
use std::sync::Arc;
use tokio::sync::mpsc;

/// State of the agent-running part of the llimorse-based chat application
pub(super) struct ChatAgent {
    /// Notify the UI to redraw
    ui_notifications: Arc<mpsc::UnboundedSender<ui::Notification>>,

    /// Notifications from the application
    ///
    /// Generally `Some(_)`, but we need to `.take()` it during tool call execution so we can use
    /// this mutably while the tool call executor has `&self` borrowed.
    incoming: Option<Incoming>,
}

/// Incoming notification processing
struct Incoming {
    /// Notifications from the application
    notifications: mpsc::UnboundedReceiver<Notification>,

    /// User messages queued
    queued_messages: VecDeque<String>,
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

    /// Submit the current state without a new user message (e.g. continue a resumed
    /// session)
    Continue,
}

impl ChatAgent {
    /// Create a new instance.
    pub fn new(
        notifications: mpsc::UnboundedReceiver<Notification>,
        ui_notifications: Arc<mpsc::UnboundedSender<ui::Notification>>,
    ) -> Self {
        ChatAgent {
            incoming: Some(Incoming {
                notifications,
                queued_messages: VecDeque::new(),
            }),
            ui_notifications,
        }
    }

    /// Run agent requests in a loop until the exit flag is set (or an error occurs).
    ///
    /// Will itself set the exit flag before returning.
    pub async fn run(&mut self, agent: llimorse::Agent) -> Result<()> {
        let result = self.do_run(agent).await;
        let _ = self.ui_notifications.send(ui::Notification::Exit);
        result
    }

    /// Process incoming notifications until finding a prompt, a continue, or an exit.
    ///
    /// Return `false` if the channel is closed (or an exit event is received).
    ///
    /// The caller **must** submit `self.queued_messages` immediately (because this function
    /// ignores `ForceSubmitQueued`, assuming the caller will do so).
    async fn process_notifications(&mut self) -> bool {
        let incoming = self.incoming.as_mut().expect("Notifications object taken");

        while let Some(notification) = incoming.notifications.recv().await {
            match notification {
                Notification::Exit => return false,

                Notification::QueuePrompt(p) => {
                    incoming.queued_messages.push_back(p);
                    return true;
                }

                Notification::Continue => return true,

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
        let incoming = self.incoming.as_mut().expect("Notifications object taken");

        while let Ok(notification) = incoming.notifications.try_recv() {
            match notification {
                Notification::Exit => return false,

                Notification::QueuePrompt(p) => incoming.queued_messages.push_back(p),

                // Ignore this, the caller will submit them all right now anyway.
                Notification::ForceSubmitQueued => (),

                Notification::Continue => (),
            }
        }

        true
    }

    /// Push all messages currently in `self.queued_messages` onto the agent/chat history.
    ///
    /// Return if there were any messages.
    ///
    /// This does not yet submit a request to the agent.
    fn submit_queued_user_messages(&mut self, agent: &mut llimorse::Agent) -> bool {
        let incoming = self.incoming.as_mut().expect("Notifications object taken");

        let messages = mem::take(&mut incoming.queued_messages);
        let any_messages = !messages.is_empty();
        for message in messages {
            self.send_update(AgentUpdate::User {
                prompt: message.clone(),
            });
            agent.push_user(message);
        }

        any_messages
    }

    /// Whether a history can be submitted without a new user message: it must top out on a tool
    /// result (e.g. an interrupted session) or a user message the model has not answered yet.
    fn can_continue(history: &[ChatMessage]) -> bool {
        matches!(
            history.last(),
            Some(ChatMessage::Tool(_)) | Some(ChatMessage::User(_))
        )
    }

    /// Run agent requests in a loop until the exit flag is set (or an error occurs).
    async fn do_run(&mut self, mut agent: llimorse::Agent) -> Result<()> {
        while self.process_notifications().await && self.process_available_notifications() {
            // If there were no queued user messages, we received `Continue` notification; honor
            // that only if we can actually continue, i.e. if there is anything to submit.
            if !self.submit_queued_user_messages(&mut agent) && !Self::can_continue(agent.history())
            {
                continue;
            }

            let response = loop {
                let mut streaming = agent.submit().await?;

                loop {
                    let incoming = self.incoming.as_mut().expect("Notifications object taken");

                    futures::select! {
                        chunk = streaming.next() => {
                            if let Some(chunk) = chunk {
                                self.process_chunk(chunk?);
                            } else {
                                break;
                            }
                        }

                        exit = incoming.wait_for_abort().fuse() => if exit {
                            return Ok(())
                        } else {
                            streaming.force_finalize();
                            break;
                        }
                    }
                }

                drop(streaming);

                let mut incoming = self.incoming.take().expect("Notifications object taken");

                let check_calls = |agent: &Agent, call: &ToolCall| {
                    self.send_update(AgentUpdate::ToolCallEx {
                        call: call.clone(),
                        display: agent.display_call(&call.call).to_string(),
                    });
                    Ok(())
                };

                let check_results = |agent: &Agent, call: &ToolCall, result: &Result<String>| {
                    match result {
                        Ok(result) => self.send_update(AgentUpdate::ToolResultEx {
                            call: call.clone(),
                            display: Ok(agent.display_call_result(&call.call, result).to_string()),
                        }),
                        Err(err) => self.send_update(AgentUpdate::ToolResultEx {
                            call: call.clone(),
                            display: Err(err.to_string()),
                        }),
                    }
                    Ok(())
                };

                let mut pending = futures::select! {
                    pending = agent.execute_pending_calls(check_calls, check_results).fuse() => pending,
                    exit = incoming.wait_for_abort().fuse() => if exit {
                        return Ok(());
                    } else {
                        false // not pending by itself, unless there are actually user messages
                    }
                };
                self.incoming = Some(incoming);

                if !self.process_available_notifications() {
                    return Ok(());
                }
                if self.submit_queued_user_messages(&mut agent) {
                    pending = true;
                }

                if !pending {
                    break agent.history().last().and_then(|cm| {
                        if let ChatMessage::Assistant(msg) = cm
                            && let Some(response) = &msg.content
                            && !response.is_empty()
                        {
                            Some(response.clone())
                        } else {
                            None
                        }
                    });
                }
            };

            // The loop can only exit when there are no pending tool calls and no queued
            // messages: the prompt iteration is complete, and the agent will wait for the
            // user’s next prompt.
            let _ = self
                .ui_notifications
                .send(ui::Notification::AwaitingPrompt { response });
        }

        Ok(())
    }

    /// Process the incoming `chunk` from the LLM (i.e. append it to the history).
    fn process_chunk(&self, chunk: StreamingChunk) {
        let update = match chunk {
            StreamingChunk::Content(content) => AgentUpdate::Content { append: content },
            StreamingChunk::Reasoning(content) => AgentUpdate::Reasoning { append: content },
        };
        self.send_update(update);
    }

    /// Submit a chat update to the UI
    fn send_update(&self, update: AgentUpdate) {
        let _ = self.ui_notifications.send(ui::Notification::AgentUpdate {
            agent_id: AgentId::Main,
            content: update,
        });
    }
}

impl Incoming {
    /// Process all available incoming notifications, until any abort notification is received
    ///
    /// Abort notifications are `Exit` and `ForceSubmitQueued`, or the channel being closed.
    ///
    /// Return `true` if we need to exit, `false` if we just need to abort and submit everything
    /// that’s queued.
    async fn wait_for_abort(&mut self) -> bool {
        while let Some(notification) = self.notifications.recv().await {
            match notification {
                Notification::Exit => return true,
                Notification::ForceSubmitQueued => return false,

                Notification::Continue => (),

                Notification::QueuePrompt(p) => self.queued_messages.push_back(p),
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::ChatAgent;
    use llimorse::line_format::{AssistantMessage, ChatMessage, ToolResult, UserMessage};

    #[test]
    fn can_continue_on_tool_result() {
        let result = ToolResult::new("call-1".to_string(), Ok("done".to_string()));
        let history = vec![ChatMessage::Tool(result)];
        assert!(ChatAgent::can_continue(&history));
    }

    #[test]
    fn can_continue_on_unanswered_user_message() {
        let history = vec![ChatMessage::User(UserMessage {
            content: "hello".to_string(),
        })];
        assert!(ChatAgent::can_continue(&history));
    }

    #[test]
    fn cannot_continue_after_assistant_message() {
        let history = vec![ChatMessage::Assistant(AssistantMessage {
            reasoning_content: None,
            content: Some("hi there".to_string()),
            tool_calls: None,
        })];
        assert!(!ChatAgent::can_continue(&history));
    }

    #[test]
    fn cannot_continue_on_empty_history() {
        assert!(!ChatAgent::can_continue(&[]));
    }
}
