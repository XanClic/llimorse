//! Agent harness around an LLM client.

use super::client::Client;
use super::line_format::{
    ChatMessage, FunctionDefinition, SystemMessage, ToolCall, ToolCallParams, ToolChoiceMode,
    ToolDefinition, ToolResult, UserMessage,
};
use super::streaming_result::{StreamingChunk, StreamingResult, TokenUsage};
use anyhow::{Result, anyhow, bail};
use futures::stream::{FusedStream, FuturesUnordered};
use futures::{Stream, StreamExt};
use pin_project::pin_project;
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Agent harness around an LLM client.
pub struct Agent {
    /// LLM client
    client: Client,

    /// Current chat history
    history: Vec<ChatMessage>,

    /// Pending tool calls the LLM is waiting for
    pending_calls: Vec<ToolCall>,

    /// Current token usage
    token_usage: TokenUsage,
}

#[pin_project(project = AgentRunningProjection)]
pub struct AgentRunning<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>> {
    #[pin]
    streaming: StreamingResult<S>,
    agent: &'a mut Agent,
    terminated: bool,
}

#[derive(Deserialize, JsonSchema)]
struct HelloTool {
    /// Use this for an extra special tag line between friendly colleagues!
    #[allow(unused)]
    tagline: String,
}

impl Agent {
    pub fn new(client: Client) -> Self {
        Agent {
            client,
            history: Vec::new(),
            pending_calls: Vec::new(),
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
        let tool = ToolDefinition::Function {
            function: FunctionDefinition {
                name: "hello".into(),
                description: Some(
                    "Make the initial greeting to the user extra special! Use this to be super friendly.".into(),
                ),
                parameters: Some(schema_for!(HelloTool)),
                strict: true,
            },
        };

        let streaming = {
            self.client
                .chat_stream(&self.history, &[tool], ToolChoiceMode::Auto)
                .await?
        };

        Ok(AgentRunning {
            streaming,
            agent: self,
            terminated: false,
        })
    }

    /// Executes all pending tool calls requested by the LLM.
    ///
    /// Return whether any have been executed, in which case the results will need to be submitted
    /// to the LLM.
    pub async fn execute_pending_calls(&mut self) -> bool {
        let results = {
            let mut futs = FuturesUnordered::new();
            for tool_call in mem::take(&mut self.pending_calls) {
                futs.push(self.execute_call(tool_call));
            }

            let mut results = Vec::with_capacity(futs.len());
            while let Some(result) = futs.next().await {
                results.push(result.into());
            }
            results
        };

        if results.is_empty() {
            false
        } else {
            self.history.extend(results);
            true
        }
    }

    /// Executes the given tool call, creating a corresponding [`ToolResult`].
    async fn execute_call(&self, tool_call: ToolCall) -> ToolResult {
        let result = self.do_execute_call(tool_call.call).await;
        ToolResult::new(tool_call.id, result)
    }

    /// Performs the actual tool call, returning a `Result<_>`.
    ///
    /// To be usable by the LLM, this needs to be called by something that catches the errors and
    /// properly formats them for the LLM.
    async fn do_execute_call(&self, tool_call: ToolCallParams) -> Result<String> {
        match tool_call {
            ToolCallParams::Function { function } => match function.name.as_str() {
                "hello" => {
                    let params: HelloTool = serde_json::from_str(&function.arguments)?;
                    eprintln!(
                        "\n\x1b[31;1mA super special hello from the LLM: {}\x1b[0m\n",
                        params.tagline
                    );
                    Ok("Got it!".into())
                }

                _ => bail!("No such function: {}", function.name),
            },

            ToolCallParams::Custom { custom } => bail!("No such tool: {}", custom.name),
        }
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> AgentRunning<'_, S> {
    /// Same as [`Agent::execute_pending_calls()`].
    ///
    /// The problem is that [`AgentRunning`] retains a reference to [`Agent`] while it lives, so
    /// without dropping it, [`Agent::execute_pending_calls()`] cannot be run.  This function plugs
    /// that gap, doing both (dropping and executing the calls).
    pub async fn execute_pending_calls(self) -> bool {
        self.agent.execute_pending_calls().await
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

                // I think the tool calls need to remain in the history, so we cannot just
                // `.take()` them...?
                if let Some(ref tool_calls) = message.tool_calls {
                    this.agent.pending_calls.extend(tool_calls.iter().cloned());
                }

                // The reasoning, we might want to remove because most models won’t feed it back,
                // but some do, so... keep it.

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
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<()>> {
        if <Self as FusedStream>::is_terminated(&self) {
            // Technically not true if terminated because of error, but that’s your fault for
            // using both `poll_next()` and `poll()` then
            return Poll::Ready(Ok(()));
        }

        loop {
            match self.as_mut().poll_next(ctx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err)),
                Poll::Ready(Some(_)) => continue,
                Poll::Ready(None) => {
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}
