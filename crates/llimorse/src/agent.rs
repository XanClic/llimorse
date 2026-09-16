//! Agent harness around an LLM client.

use super::client::Client;
use super::line_format::{
    AssistantMessage, ChatMessage, FunctionDefinition, SystemMessage, ToolCall, ToolCallParams,
    ToolChoiceMode, ToolDefinition, ToolResult, UserMessage,
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
#[derive(Debug)]
pub struct Agent<L: ChatListener = ()> {
    /// LLM client
    client: Client,

    /// Current chat history
    history: ChatHistory<L>,

    /// Pending tool calls the LLM is waiting for
    pending_calls: Vec<ToolCall>,

    /// Current token usage
    token_usage: TokenUsage,

    /// Available tools
    tools: HashMap<String, Box<dyn Tool>>,

    /// Tool definition as passed to the LLM
    tool_definitions: Vec<ToolDefinition>,
}

/// Wrapper for the chat history to ensure that everything that is added goes through the
/// `listener` as well.
#[derive(Debug)]
struct ChatHistory<L> {
    /// The full current chat history
    history: Vec<ChatMessage>,

    /// Push every single new `ChatMessage` through here
    listener: L,
}

/// Trait for an object listening to chat updates
pub trait ChatListener: fmt::Debug {
    /// The given chat message is added to the chat history; log it.
    fn log_message(&mut self, message: &ChatMessage);

    /// Log more than a single message at once.
    ///
    /// The default implementation calls [`Self::log_message()`] for each.
    fn log_messages(&mut self, messages: &[ChatMessage]) {
        for message in messages {
            self.log_message(message)
        }
    }
}

impl ChatListener for () {
    fn log_message(&mut self, _message: &ChatMessage) {}
    fn log_messages(&mut self, _messages: &[ChatMessage]) {}
}

/// Tool available to an agent.
///
/// This trait is what is needed by [`Agent`] to use a tool.
pub trait Tool: fmt::Debug + Send {
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
pub struct AgentRunning<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>, L: ChatListener> {
    /// The results as it is begin generated
    #[pin]
    streaming: StreamingResult<S>,

    /// Reference to the [`Agent`] object to push back the results
    agent: &'a mut Agent<L>,

    /// Whether the request is done
    terminated: bool,
}

/// `Future` to await a full agent response without streaming
#[pin_project]
pub struct AgentResponse<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>, L: ChatListener> {
    /// Don’t tell anyone, but we actually do still stream, secretly.
    #[pin]
    stream: AgentRunning<'a, S, L>,
}

impl Agent {
    /// Create a new agent harness around the given [`Client`].
    pub fn new(client: Client) -> Self {
        Self::new_with_listener(client, ())
    }

    /// Return the entire chat history so far
    pub fn history(&self) -> &[ChatMessage] {
        self.history.history()
    }

    /// Return the last content returned by the LLM after the last input from our side.
    ///
    /// Input from our side are:
    /// - User messages
    /// - System messages
    /// - Tool call results
    pub fn last_result(&self) -> Option<&String> {
        self.history
            .history()
            .iter()
            .rev()
            .take_while(|msg| matches!(msg, ChatMessage::Assistant(_)))
            .find_map(|msg| {
                let ChatMessage::Assistant(msg) = msg else {
                    unreachable!() // checked in `take_while`
                };
                msg.content.as_ref()
            })
    }
}

impl<L: ChatListener> Agent<L> {
    /// Create a new agent harness around the given [`Client`] with `listener` receiving new chat
    /// messages.
    pub fn new_with_listener(client: Client, listener: L) -> Self {
        Agent {
            client,
            history: ChatHistory::with_listener(listener),
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

    /// Push a full history, e.g. when resuming.
    ///
    /// Note that each entry still goes through the [`ChatListener`].
    pub fn push_history(&mut self, messages: Vec<ChatMessage>) {
        self.history.push_vec(messages);
    }

    /// Push the given system-level message on top of the chat history.
    pub fn push_system(&mut self, message: impl Into<String>) {
        self.push(SystemMessage::from(message.into()))
    }

    /// Push the given user message on top of the chat history.
    pub fn push_user(&mut self, message: impl Into<String>) {
        self.push(UserMessage::from(message.into()))
    }

    /// Push the given tool call on top of the chat history, and push it into the pending calls
    /// list (to execute it via [`Self::execute_pending_calls()`]).
    pub fn push_tool_call(&mut self, call: ToolCall) {
        self.pending_calls.push(call.clone());
        self.push(AssistantMessage {
            content: None,
            reasoning_content: None,
            tool_calls: Some(vec![call]),
        });
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
    ) -> Result<AgentRunning<'_, impl Stream<Item = reqwest::Result<bytes::Bytes>>, L>> {
        let streaming = {
            self.client
                .chat_stream(
                    self.history.history(),
                    &self.tool_definitions,
                    ToolChoiceMode::Auto,
                )
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
        F1: FnMut(&Agent<L>, &ToolCall) -> Result<()>,
        F2: FnMut(&Agent<L>, &ToolCall, &Result<String>) -> Result<()>,
    >(
        &mut self,
        mut tool_guard: F1,
        mut tool_result_guard: F2,
    ) -> bool {
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
            false
        } else {
            self.history.push_vec(results);
            true
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

impl<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>, L: ChatListener> AgentRunning<'a, S, L> {
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
    /// Panics if [`AgentRunning::is_terminated()`] is false.
    pub async fn execute_pending_calls<
        F1: FnMut(&Agent<L>, &ToolCall) -> Result<()>,
        F2: FnMut(&Agent<L>, &ToolCall, &Result<String>) -> Result<()>,
    >(
        self,
        tool_guard: F1,
        tool_result_guard: F2,
    ) -> bool {
        assert!(self.terminated);
        self.agent
            .execute_pending_calls(tool_guard, tool_result_guard)
            .await
    }

    /// Await the full response instead of a stream of parts.
    pub fn full_response(self) -> AgentResponse<'a, S, L> {
        AgentResponse { stream: self }
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>, L: ChatListener>
    AgentRunningProjection<'_, '_, S, L>
{
    /// Mark the stream as terminated.
    fn terminate(&mut self) {
        *self.terminated = true;
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>, L: ChatListener> Stream
    for AgentRunning<'_, S, L>
{
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

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>, L: ChatListener> FusedStream
    for AgentRunning<'_, S, L>
{
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl<'a, S: Stream<Item = reqwest::Result<bytes::Bytes>>, L: ChatListener> AgentResponse<'a, S, L> {
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
        F1: FnMut(&Agent<L>, &ToolCall) -> Result<()>,
        F2: FnMut(&Agent<L>, &ToolCall, &Result<String>) -> Result<()>,
    >(
        self,
        tool_guard: F1,
        tool_result_guard: F2,
    ) -> bool {
        assert!(self.stream.is_terminated());
        self.stream
            .agent
            .execute_pending_calls(tool_guard, tool_result_guard)
            .await
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>, L: ChatListener> Future
    for AgentResponse<'_, S, L>
{
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
pub struct DisplayCall<'a, L: ChatListener> {
    /// Agent; required to parse the call parameters
    agent: &'a Agent<L>,

    /// Raw call parameters
    call: &'a ToolCallParams,
}

impl<L: ChatListener> fmt::Display for DisplayCall<'_, L> {
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
pub struct DisplayCallResult<'a, L: ChatListener> {
    /// Agent; required to parse the call result
    agent: &'a Agent<L>,

    /// Raw call parameters
    call: &'a ToolCallParams,

    /// Raw tool result
    result: &'a String,
}

impl<L: ChatListener> fmt::Display for DisplayCallResult<'_, L> {
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

impl<L: ChatListener> ChatHistory<L> {
    /// Create a new `ChatHistory` instance, notifying `listener` on additions.
    fn with_listener(listener: L) -> Self {
        ChatHistory {
            history: Vec::new(),
            listener,
        }
    }

    /// Return all chat messages in the history.
    fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    /// Append the given `message` to the history.
    fn push(&mut self, message: ChatMessage) {
        self.listener.log_message(&message);
        self.history.push(message);
    }

    /// Append the given `messages` to the history.
    fn push_vec(&mut self, mut messages: Vec<ChatMessage>) {
        self.listener.log_messages(&messages);
        self.history.append(&mut messages);
    }
}
