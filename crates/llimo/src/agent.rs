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
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, mem};

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

    /// Available tools
    tools: HashMap<String, Box<dyn Tool>>,

    /// Tool definition as passed to the LLM
    tool_definitions: Vec<ToolDefinition>,
}

/// Tool available to an agent.
///
/// This trait is what is needed by [`Agent`] to use a tool.
pub trait Tool {
    /// Tool name
    fn name(&self) -> String;

    /// Tool description, if any
    fn description(&self) -> Option<String>;

    /// Tool arguments JSON schema
    fn schema(&self) -> Schema;

    /// Execute this tool, arguments given in JSON format (unparsed)
    fn execute_unparsed<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + 'a>>;

    /// Format the tool call arguments for `Display`
    fn fmt_call_display(&self, f: &mut fmt::Formatter<'_>, arguments: &str) -> fmt::Result;

    /// Format the tool call result for `Display`
    fn fmt_call_result_display(&self, f: &mut fmt::Formatter<'_>, result: &str) -> fmt::Result;
}

/// Connects a tool to its parameter and result types.
///
/// This exists so [`CallableTool`] is forced to use the correct types (when using the
/// [`tool!`](super::tool) macro).  It is separate from [`CallableTool`] because the macro
/// implements this, and the user implements the latter.
pub trait ToolState {
    /// Associated parameter type.
    type ParamType: for<'a> Deserialize<'a> + fmt::Display + JsonSchema;

    /// Associated return type.
    type ResultType: for<'a> Deserialize<'a> + Serialize + fmt::Display;
}

/// User-defined trait for a tool.
#[allow(async_fn_in_trait)]
pub trait CallableTool: ToolState {
    /// Execute a tool call.
    async fn execute(
        &self,
        arguments: <Self as ToolState>::ParamType,
    ) -> Result<<Self as ToolState>::ResultType>;
}

/// Request currently being executed by the LLM.
#[pin_project(project = AgentRunningProjection)]
pub struct AgentRunning<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>> {
    /// The results as it is begin generated
    #[pin]
    streaming: StreamingResult<S>,

    /// Reference to the [`Agent`] object to push back the results
    agent: &'a mut Agent,

    /// Whether the request is done
    terminated: bool,
}

/// `Future` to await a full agent response without streaming
#[pin_project]
pub struct AgentResponse<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>> {
    /// Don’t tell anyone, but we actually do still stream, secretly.
    #[pin]
    stream: AgentRunning<'a, S>,
}

impl Agent {
    /// Create a new agent harness around the given [`Client`].
    pub fn new(client: Client) -> Self {
        Agent {
            client,
            history: Vec::new(),
            pending_calls: Vec::new(),
            token_usage: Default::default(),
            tools: HashMap::new(),
            tool_definitions: Vec::new(),
        }
    }

    /// Push the given message on top of the chat history.
    pub fn push(&mut self, message: impl Into<ChatMessage>) {
        self.history.push(message.into());
    }

    /// Push the given system-level message on top of the chat history.
    pub fn push_system(&mut self, message: impl Into<String>) {
        self.push(SystemMessage::from(message.into()))
    }

    /// Push the given user message on top of the chat history.
    pub fn push_user(&mut self, message: impl Into<String>) {
        self.push(UserMessage::from(message.into()))
    }

    /// Add the given tool, with the given state, to the agent.
    pub fn add_tool<T: Tool + 'static>(&mut self, state: T) {
        let definition = ToolDefinition::Function {
            function: FunctionDefinition {
                name: state.name(),
                description: state.description(),
                parameters: Some(state.schema()),
                strict: true,
            },
        };

        self.tools.insert(state.name(), Box::new(state));
        self.tool_definitions.push(definition);
    }

    /// Submit the current chat history.
    ///
    /// This includes pending tool call results (from [`Agent::execute_pending_calls()`]) and user
    /// messages.
    pub async fn submit(
        &mut self,
    ) -> Result<AgentRunning<'_, impl Stream<Item = reqwest::Result<bytes::Bytes>>>> {
        let streaming = {
            self.client
                .chat_stream(&self.history, &self.tool_definitions, ToolChoiceMode::Auto)
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
    pub async fn execute_pending_calls<
        F1: FnMut(&Agent, &ToolCall) -> Result<()>,
        F2: FnMut(&Agent, &ToolCall, &Result<String>) -> Result<()>,
    >(
        &mut self,
        mut tool_guard: F1,
        mut tool_result_guard: F2,
    ) -> &[ChatMessage] {
        let mut results = Vec::<ChatMessage>::with_capacity(self.pending_calls.len());
        let mut futs = FuturesUnordered::new();
        for call in mem::take(&mut self.pending_calls) {
            if let Err(err) = tool_guard(self, &call) {
                results.push(ToolResult::rejected(call.id, err).into());
            } else {
                futs.push(async { (self.execute_call(&call.call).await, call) })
            }
        }

        while let Some((result, call)) = futs.next().await {
            if let Err(err) = tool_result_guard(self, &call, &result) {
                results.push(ToolResult::rejected(call.id, err).into());
            } else {
                results.push(ToolResult::new(call.id, result).into());
            }
        }
        drop(futs);

        if results.is_empty() {
            &[]
        } else {
            let base_i = self.history.len();
            self.history.extend(results);
            &self.history[base_i..]
        }
    }

    /// Performs the actual tool call, returning a `Result<_>`.
    ///
    /// To be usable by the LLM, this needs to be called by something that catches the errors and
    /// properly formats them for the LLM.
    async fn execute_call(&self, tool_call: &ToolCallParams) -> Result<String> {
        match tool_call {
            ToolCallParams::Function { function } => {
                let state = self
                    .tools
                    .get(function.name.as_str())
                    .ok_or_else(|| anyhow!("No such function: {}", function.name))?;

                state.execute_unparsed(&function.arguments).await
            }

            ToolCallParams::Custom { custom } => bail!("No such tool: {}", custom.name),
        }
    }

    /// Returns an object that implements [`fmt::Display`] to properly format the call.
    pub fn display_call<'a>(&'a self, tool_call: &'a ToolCallParams) -> impl fmt::Display + 'a {
        DisplayCall {
            agent: self,
            call: tool_call,
        }
    }

    /// Returns an object that implements [`fmt::Display`] to properly format the result.
    pub fn display_call_result<'a>(
        &'a self,
        tool_call: &'a ToolCallParams,
        result: &'a String,
    ) -> impl fmt::Display + 'a {
        DisplayCallResult {
            agent: self,
            call: tool_call,
            result,
        }
    }

    /// Return the token usage from the last request: Prompt tokens, and completion tokens.
    pub fn token_usage(&self) -> (usize, usize) {
        (
            self.token_usage.prompt_tokens as usize,
            self.token_usage.completion_tokens as usize,
        )
    }
}

impl<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>> AgentRunning<'a, S> {
    /// Same as [`Agent::execute_pending_calls()`].
    ///
    /// The problem is that [`AgentRunning`] retains a reference to [`Agent`] while it lives, so
    /// without dropping it, [`Agent::execute_pending_calls()`] cannot be run.  This function plugs
    /// that gap, doing both (dropping and executing the calls).
    ///
    /// Must only be called after the request has run its course, with success.
    ///
    /// # Panics
    ///
    /// Panics if [`AgentRunning::terminated`] is false.
    pub async fn execute_pending_calls<
        F1: FnMut(&Agent, &ToolCall) -> Result<()>,
        F2: FnMut(&Agent, &ToolCall, &Result<String>) -> Result<()>,
    >(
        self,
        tool_guard: F1,
        tool_result_guard: F2,
    ) -> &'a [ChatMessage] {
        assert!(self.terminated);
        self.agent
            .execute_pending_calls(tool_guard, tool_result_guard)
            .await
    }

    /// Await the full response instead of a stream of parts.
    pub fn full_response(self) -> AgentResponse<'a, S> {
        AgentResponse { stream: self }
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> AgentRunningProjection<'_, '_, S> {
    /// Mark the stream as terminated.
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

                let Some(message) = this.streaming.as_mut().full_message_pinned() else {
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

                if let Some(token_usage) = this.streaming.token_usage_pinned() {
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

impl<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>> AgentResponse<'a, S> {
    /// Same as [`Agent::execute_pending_calls()`].
    ///
    /// The problem is that [`AgentResponse`] retains a reference to [`Agent`] while it lives, so
    /// without dropping it, [`Agent::execute_pending_calls()`] cannot be run.  This function plugs
    /// that gap, doing both (dropping and executing the calls).
    ///
    /// Must only be called after the request has run its course, with success.
    ///
    /// # Panics
    ///
    /// Panics if `self` has not been awaited yet.
    pub async fn execute_pending_calls<
        F1: FnMut(&Agent, &ToolCall) -> Result<()>,
        F2: FnMut(&Agent, &ToolCall, &Result<String>) -> Result<()>,
    >(
        self,
        tool_guard: F1,
        tool_result_guard: F2,
    ) -> &'a [ChatMessage] {
        assert!(self.stream.is_terminated());
        self.stream
            .agent
            .execute_pending_calls(tool_guard, tool_result_guard)
            .await
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> Future for AgentResponse<'_, S> {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Result<()>> {
        let mut this = self.project();

        if this.stream.is_terminated() {
            // Technically not true if terminated because of error, but that’s your fault for
            // using both `poll_next()` and `poll()` then
            return Poll::Ready(Ok(()));
        }

        loop {
            match this.stream.as_mut().poll_next(ctx) {
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

/// Helper struct for properly formatting call parameters for display.
pub struct DisplayCall<'a> {
    /// Agent; required to parse the call parameters
    agent: &'a Agent,

    /// Raw call parameters
    call: &'a ToolCallParams,
}

impl fmt::Display for DisplayCall<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.call {
            ToolCallParams::Function { function } => {
                let Some(state) = self.agent.tools.get(function.name.as_str()) else {
                    return write!(f, "[unknown function {}]", function.name);
                };

                write!(f, "[function call] {}(", function.name)?;
                state.fmt_call_display(f, &function.arguments)?;
                write!(f, ")")
            }

            ToolCallParams::Custom { custom } => write!(f, "[unknown tool {}]", custom.name),
        }
    }
}

/// Helper struct for properly formatting a call result for display.
pub struct DisplayCallResult<'a> {
    /// Agent; required to parse the call result
    agent: &'a Agent,

    /// Raw call parameters
    call: &'a ToolCallParams,

    /// Raw tool result
    result: &'a String,
}

impl fmt::Display for DisplayCallResult<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.call {
            ToolCallParams::Function { function } => {
                let Some(state) = self.agent.tools.get(function.name.as_str()) else {
                    return write!(f, "[unknown function {}]", function.name);
                };

                state.fmt_call_result_display(f, self.result)
            }

            ToolCallParams::Custom { custom } => write!(f, "[unknown tool {}]", custom.name),
        }
    }
}
