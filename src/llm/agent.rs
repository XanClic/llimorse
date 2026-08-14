//! Agent harness around an LLM client.

use super::client::Client;
use super::line_format::{ChatMessage, SystemMessage, ToolChoiceMode, UserMessage};
use super::streaming_result::{StreamingChunk, StreamingResult, TokenUsage};
use anyhow::{Result, anyhow};
use futures::Stream;
use futures::stream::FusedStream;
use pin_project::pin_project;
use std::ops::Range;
use std::pin::Pin;
use std::slice::SliceIndex;
use std::task::{Context, Poll};

/// Agent harness around an LLM client.
pub struct Agent {
    /// LLM client
    client: Client,

    /// Current chat history
    history: Vec<ChatMessage>,

    /// Current token usage
    token_usage: TokenUsage,
}

#[pin_project(project = AgentRunningProjection)]
pub struct AgentRunning<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>> {
    #[pin]
    streaming: StreamingResult<S>,
    agent: &'a mut Agent,
    history_len_before: usize,
    terminated: bool,
}

impl Agent {
    pub fn new(client: Client) -> Self {
        Agent {
            client,
            history: Vec::new(),
            token_usage: Default::default(),
        }
    }

    pub fn push(&mut self, message: impl Into<ChatMessage>) {
        self.history.push(message.into());
    }

    pub fn push_system(&mut self, message: impl Into<String>) {
        self.push(SystemMessage::from(message.into()))
    }

    pub fn push_user(&mut self, message: impl Into<String>) {
        self.push(UserMessage::from(message.into()))
    }

    pub async fn submit(
        &mut self,
    ) -> Result<AgentRunning<'_, impl Stream<Item = reqwest::Result<bytes::Bytes>>>> {
        let streaming = {
            self.client
                .chat_stream(&self.history, &[], ToolChoiceMode::Auto)
                .await?
        };

        let history_len_before = self.history.len();
        Ok(AgentRunning {
            streaming,
            agent: self,
            history_len_before,
            terminated: false,
        })
    }

    pub fn history(
        &self,
        range: impl SliceIndex<[ChatMessage], Output = [ChatMessage]>,
    ) -> &[ChatMessage] {
        &self.history[range]
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> AgentRunningProjection<'_, '_, S> {
    fn terminate(&mut self) {
        *self.terminated = true;
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> Stream for AgentRunning<'_, S> {
    type Item = Result<StreamingChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
    ) -> Poll<Option<Result<StreamingChunk>>> {
        let mut this = self.project();

        match this.streaming.as_mut().poll_next(ctx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Err(err))) => {
                this.terminate();
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(chunk))),
            Poll::Ready(None) => {
                this.terminate();

                // TODO: Utterly broken to use `poll()` here for this, but a proper method that
                // taeks `&mut` is really hard to do with the `Pin` stuff
                let result = this.streaming.poll(ctx);
                let Poll::Ready(Ok((message, token_usage))) = result else {
                    return Poll::Ready(Some(Err(anyhow!(
                        "Assistant did not generate a complete message"
                    ))));
                };

                this.agent.push(message);
                if let Some(token_usage) = token_usage {
                    this.agent.token_usage = token_usage;
                }

                Poll::Ready(None)
            }
        }
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> FusedStream for AgentRunning<'_, S> {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> Future for AgentRunning<'_, S> {
    type Output = Result<Range<usize>>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<Range<usize>>> {
        if <Self as FusedStream>::is_terminated(&self) {
            // Technically not true if terminated because of error, but that’s your fault for
            // using both `poll_next()` and `poll()` then
            return Poll::Ready(Ok(self.history_len_before..self.agent.history.len()));
        }

        loop {
            match self.as_mut().poll_next(ctx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err)),
                Poll::Ready(Some(_)) => continue,
                Poll::Ready(None) => {
                    return Poll::Ready(Ok(self.history_len_before..self.agent.history.len()));
                }
            }
        }
    }
}
