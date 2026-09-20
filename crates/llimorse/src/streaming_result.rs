//! Helper object to manage streaming result

use super::client::TokenUsage;
use super::line_format::{AssistantMessage, CustomCall, FunctionCall, ToolCall, ToolCallParams};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::Stream;
use futures::stream::FusedStream;
use pin_project::pin_project;
use serde::Deserialize;
use std::collections::VecDeque;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

/// Streamable result for a chat completion request
#[pin_project(project = StreamingResultProjection)]
pub struct StreamingResult<S: Stream<Item = reqwest::Result<bytes::Bytes>>> {
    /// Source stream
    #[pin]
    stream: NewlineSplit<S>,

    /// Not-yet returned stream chunks
    chunks: VecDeque<StreamingChunk>,

    /// Set once `[DONE]` has been received
    done: bool,

    /// The full message, as it is being constructed
    constructing: StreamingAssistantMessage,

    /// The actual full message after `[DONE]`
    full_message: Option<AssistantMessage>,

    /// Token usage for the whole request
    token_usage: Arc<TokenUsage>,
}

/// In-construction message from the assistant to the user or system
///
/// Message is currently being streamed, so everything is recursively optional.
#[derive(Clone, Debug, Default, Deserialize)]
struct StreamingAssistantMessage {
    /// Message content
    #[serde(default)]
    content: Option<String>,

    /// Private reasoning (“thinking”), as separated out by llama-server.
    #[serde(default)]
    reasoning_content: Option<String>,

    /// Tool calls to be performed
    #[serde(default)]
    tool_calls: Option<Vec<StreamingToolCall>>,
}

/// Request a tool call (in construction)
///
/// Message is currently being streamed, so everything is recursively optional.
#[derive(Clone, Debug, Deserialize)]
struct StreamingToolCall {
    /// Which index in the tool call vec this applies to
    index: usize,

    /// In-construction ID to reference in the result
    #[serde(default)]
    id: Option<String>,

    /// Which tool to call, and how
    #[serde(default, flatten)]
    call: Option<StreamingToolCallParams>,
}

/// In-construction tool call name and parameters
///
/// Message is currently being streamed, so everything is recursively optional.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StreamingToolCallParams {
    /// Function tool call
    Function(StreamingFunctionCall),

    /// Custom tool call
    Custom(StreamingCustomCall),
}

/// Request a function tool call (in-construction)
///
/// Message is currently being streamed, so everything is recursively optional.
#[derive(Clone, Debug, Default, Deserialize)]
struct StreamingFunctionCall {
    /// In-construction function name
    #[serde(default)]
    name: Option<String>,

    /// In-construction arguments in JSON format
    #[serde(default)]
    arguments: Option<String>,
}

/// Request a custom tool call
///
/// Message is currently being streamed, so everything is recursively optional.
#[derive(Clone, Debug, Default, Deserialize)]
struct StreamingCustomCall {
    /// In-construction custom tool name
    #[serde(default)]
    name: Option<String>,

    /// In-construction input to feed into the tool
    #[serde(default)]
    input: Option<String>,
}

/// Single chunk of a streamed chat completion request
#[derive(Clone, Debug)]
pub enum StreamingChunk {
    /// Normal text output
    Content(String),

    /// Internal reasoning text (“thinking”)
    Reasoning(String),
}

/// SSE stream chunk shapes (OpenAI delta format)
#[derive(Clone, Debug, Deserialize)]
struct StreamChunk {
    /// Streamed assistant message
    #[serde(default)]
    choices: Vec<StreamChoice>,

    /// Token usage (before `[DONE]`)
    #[serde(default)]
    usage: Option<UsagePayload>,
}

/// Token usage as reported by the LLM
#[derive(Clone, Debug, Deserialize)]
struct UsagePayload {
    /// Tokens in the prompt
    #[serde(default)]
    prompt_tokens: u32,

    /// New tokens produced
    #[serde(default)]
    completion_tokens: u32,
}

/// Streamed assistant message
#[derive(Clone, Debug, Deserialize)]
struct StreamChoice {
    /// Delta to apply to the assistant message
    #[serde(default)]
    delta: StreamingAssistantMessage,
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> StreamingResult<S> {
    /// Read and parse the given input stream.
    pub fn from_stream(stream: S, token_usage: &Arc<TokenUsage>) -> Self {
        StreamingResult {
            stream: stream.into(),
            chunks: VecDeque::new(),
            done: false,
            constructing: Default::default(),
            full_message: None,
            token_usage: Arc::clone(token_usage),
        }
    }

    /// Return the full message after streaming is done.
    ///
    /// Will only return `Some(_)` once streaming is done ([`Self::is_terminated()`] returns true)
    /// and only if there was no error.
    ///
    /// `.take()`s the full message, so will return it only once.
    pub(crate) fn full_message_pinned(self: Pin<&mut Self>) -> Option<AssistantMessage> {
        self.project().full_message.take()
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> StreamingResultProjection<'_, S> {
    /// Implementation for [`StreamingResult::poll_next()`].
    ///
    /// Implemented separately so the actual function can check for completion and call
    /// [Self::terminate()`] when necessary.
    fn do_poll_next(&mut self, ctx: &mut Context<'_>) -> Poll<Option<Result<StreamingChunk>>> {
        if let Some(chunk) = self.chunks.pop_front() {
            return Poll::Ready(Some(Ok(chunk)));
        }

        if *self.done {
            let result = match mem::take(self.constructing).finalize() {
                Ok(message) => {
                    *self.full_message = Some(message);
                    None
                }
                Err(err) => Some(Err(err.context("constructing finalized message"))),
            };

            return Poll::Ready(result);
        }

        let Poll::Ready(line) = self.stream.as_mut().poll_next(ctx) else {
            return Poll::Pending;
        };

        let line = match line {
            Some(Ok(line)) => line,
            Some(Err(err)) => {
                return Poll::Ready(Some(Err(err.context("reading completion stream"))));
            }
            None => {
                return Poll::Ready(Some(Err(anyhow!(
                    "completion stream ended before being done"
                ))));
            }
        };

        let Some(data) = line.trim().strip_prefix("data:") else {
            return self.do_poll_next(ctx);
        };
        let data = data.trim_start();

        if data == "[DONE]" {
            self.stream.as_mut().project().terminate();
            *self.done = true;
        } else {
            let parsed = match serde_json::from_str::<StreamChunk>(data) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let err: anyhow::Error = err.into();
                    return Poll::Ready(Some(Err(err.context("completion stream invalid"))));
                }
            };

            if let Err(err) = self.apply_chunk(parsed) {
                return Poll::Ready(Some(Err(err)));
            }
        }

        self.do_poll_next(ctx)
    }

    /// Apply an incoming stream chunk.
    ///
    /// Populates all of:
    /// - [`StreamingResult::chunks`]
    /// - [`StreamingResult::constructing`]
    /// - [`StreamingResult::token_usage`] (if in the input)
    fn apply_chunk(&mut self, chunk: StreamChunk) -> Result<()> {
        for choice in chunk.choices {
            self.token_usage
                .streamed_tokens
                .fetch_add(1, Ordering::Relaxed);
            self.choice_received(choice)?;
        }

        if let Some(u) = chunk.usage {
            self.token_usage
                .prompt_tokens
                .store(u.prompt_tokens as usize, Ordering::Relaxed);
            self.token_usage
                .completion_tokens
                .store(u.completion_tokens as usize, Ordering::Relaxed);
            self.token_usage.streamed_tokens.store(0, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Apply `choice` to [`StreamingResult::constructing`].
    fn choice_received(&mut self, choice: StreamChoice) -> Result<()> {
        self.constructing.push(&choice.delta)?;

        if let Some(text) = choice.delta.content {
            self.chunks.push_back(StreamingChunk::Content(text));
        }

        if let Some(reasoning) = choice.delta.reasoning_content {
            self.chunks.push_back(StreamingChunk::Reasoning(reasoning));
        }

        Ok(())
    }

    /// Mark as terminated (in case of error).
    fn terminate(&mut self) {
        self.stream.as_mut().project().terminate();
        mem::take(self.chunks);
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> Stream for StreamingResult<S> {
    type Item = Result<StreamingChunk>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
    ) -> Poll<Option<Result<StreamingChunk>>> {
        if <Self as FusedStream>::is_terminated(&self) {
            return Poll::Ready(Some(Err(anyhow!("Stream is terminated"))));
        }

        let mut this = self.as_mut().project();
        let result = this.do_poll_next(ctx);

        if matches!(result, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            this.terminate();
        }

        result
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> FusedStream for StreamingResult<S> {
    fn is_terminated(&self) -> bool {
        self.chunks.is_empty() && self.stream.is_terminated()
    }
}

/// Stream that splits a byte stream by newline characters.
#[pin_project(project = NewlineSplitProjection)]
struct NewlineSplit<S: Stream<Item = reqwest::Result<bytes::Bytes>>> {
    /// Source stream
    #[pin]
    stream: S,

    /// End of source stream reached
    eof: bool,

    /// Buffer for data as it comes in, to split at newlines
    buffer: Vec<u8>,
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> NewlineSplitProjection<'_, S> {
    /// Split the first line off of [`NewlineSplit::buffer`].
    fn split_line_from_buffer(&mut self) -> Option<Result<String>> {
        let next_line_i = self.buffer.iter().position(|&b| b == b'\n')? + 1;
        let tail = self.buffer.split_off(next_line_i);
        let line = mem::replace(self.buffer, tail);

        Some(String::from_utf8(line).map_err(Into::into))
    }

    /// Mark as terminated (in case of error).
    fn terminate(&mut self) {
        *self.eof = true;
        mem::take(self.buffer);
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> From<S> for NewlineSplit<S> {
    /// Read and parse the given input stream.
    fn from(stream: S) -> Self {
        NewlineSplit {
            stream,
            eof: false,
            buffer: Vec::new(),
        }
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> Stream for NewlineSplit<S> {
    type Item = Result<String>;

    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Result<String>>> {
        let mut this = self.as_mut().project();

        if let Some(line) = this.split_line_from_buffer() {
            if line.is_err() {
                this.terminate();
            }
            return Poll::Ready(Some(line));
        }

        if *this.eof {
            return if this.buffer.is_empty() {
                Poll::Ready(None)
            } else {
                let string = String::from_utf8(mem::take(this.buffer));
                if string.is_err() {
                    this.terminate();
                }
                Poll::Ready(Some(string.map_err(Into::into)))
            };
        };

        let Poll::Ready(chunk) = this.stream.as_mut().poll_next(ctx) else {
            return Poll::Pending;
        };

        match chunk {
            Some(Ok(chunk)) => this.buffer.extend_from_slice(&chunk),
            Some(Err(err)) => {
                this.terminate();
                return Poll::Ready(Some(Err(err.into())));
            }
            None => *this.eof = true,
        }

        <Self as Stream>::poll_next(self, ctx)
    }
}

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> FusedStream for NewlineSplit<S> {
    fn is_terminated(&self) -> bool {
        self.buffer.is_empty() && self.eof
    }
}

/// An object that is the streaming version of something in [`crate::line_format`].
trait StreamingObject {
    /// The corresponding non-streaming type.
    type NonStreaming;

    /// Apply a delta to this object.
    ///
    /// Streaming means that deltas are sent over and over and fatten the object. This function
    /// here executes that fattening.
    fn push(&mut self, delta: &Self) -> Result<()>;

    /// Finalize the object by turning it into the non-streaming variant.
    fn finalize(self) -> Result<Self::NonStreaming>;
}

impl StreamingObject for StreamingAssistantMessage {
    type NonStreaming = AssistantMessage;

    fn push(&mut self, delta: &Self) -> Result<()> {
        if let Some(ref content) = delta.content {
            self.content.get_or_insert_default().push_str(content);
        }

        if let Some(ref reasoning) = delta.reasoning_content {
            self.reasoning_content
                .get_or_insert_default()
                .push_str(reasoning);
        }

        if let Some(ref tool_calls) = delta.tool_calls {
            for tc in tool_calls {
                let calls = self.tool_calls.get_or_insert_default();
                while calls.len() <= tc.index {
                    calls.push(StreamingToolCall {
                        index: calls.len(),
                        id: None,
                        call: None,
                    });
                }

                let index = tc.index;
                calls
                    .get_mut(index)
                    .unwrap()
                    .push(tc)
                    .with_context(|| format!("Tool call at index {index}"))?;
            }
        }

        Ok(())
    }

    fn finalize(self) -> Result<AssistantMessage> {
        let tool_calls = self
            .tool_calls
            .map(|tc| {
                tc.into_iter()
                    .map(StreamingObject::finalize)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;

        Ok(AssistantMessage {
            content: self.content,
            reasoning_content: self.reasoning_content,
            tool_calls,
        })
    }
}

impl StreamingObject for StreamingToolCall {
    type NonStreaming = ToolCall;

    fn push(&mut self, delta: &Self) -> Result<()> {
        if let Some(ref id) = delta.id {
            if let Some(ref old_id) = self.id {
                bail!("ID set twice: {old_id} -> {id}")
            }

            self.id = Some(id.clone());
        }

        if let Some(ref call_delta) = delta.call {
            if let Some(call) = self.call.as_mut() {
                call.push(call_delta)?;
            } else {
                self.call = Some(call_delta.clone())
            }
        }

        Ok(())
    }

    fn finalize(self) -> Result<ToolCall> {
        let id = self
            .id
            .ok_or_else(|| anyhow!("Tool call {} has no ID", self.index))?;

        let call = self
            .call
            .ok_or_else(|| anyhow!("Tool call {} is empty", self.index))?
            .finalize()
            .with_context(|| format!("Tool call {}", self.index))?;

        Ok(ToolCall { id, call })
    }
}

impl StreamingObject for StreamingToolCallParams {
    type NonStreaming = ToolCallParams;

    fn push(&mut self, delta: &Self) -> Result<()> {
        match delta {
            StreamingToolCallParams::Function(fc_delta) => {
                let StreamingToolCallParams::Function(fc) = self else {
                    bail!(
                        "Non-function tool call updated with function data: {self:?} -> {delta:?}"
                    )
                };

                fc.push(fc_delta)
            }

            StreamingToolCallParams::Custom(ctc_delta) => {
                let StreamingToolCallParams::Custom(ctc) = self else {
                    bail!(
                        "Non-custom tool call updated with custom tool data: {self:?} -> {delta:?}"
                    )
                };

                ctc.push(ctc_delta)
            }
        }
    }

    fn finalize(self) -> Result<ToolCallParams> {
        Ok(match self {
            StreamingToolCallParams::Function(sfc) => ToolCallParams::Function {
                function: sfc.finalize()?,
            },
            StreamingToolCallParams::Custom(sctc) => ToolCallParams::Custom {
                custom: sctc.finalize()?,
            },
        })
    }
}

impl StreamingObject for StreamingFunctionCall {
    type NonStreaming = FunctionCall;

    fn push(&mut self, delta: &Self) -> Result<()> {
        if let Some(ref name) = delta.name {
            self.name.get_or_insert_default().push_str(name);
        }

        if let Some(ref arguments) = delta.arguments {
            self.arguments.get_or_insert_default().push_str(arguments);
        }

        Ok(())
    }

    fn finalize(self) -> Result<FunctionCall> {
        let name = self.name.ok_or_else(|| anyhow!("Missing function name"))?;
        let arguments = self
            .arguments
            .ok_or_else(|| anyhow!("Missing function arguments"))?;

        Ok(FunctionCall { name, arguments })
    }
}

impl StreamingObject for StreamingCustomCall {
    type NonStreaming = CustomCall;

    fn push(&mut self, delta: &Self) -> Result<()> {
        if let Some(ref name) = delta.name {
            self.name.get_or_insert_default().push_str(name);
        }

        if let Some(ref input) = delta.input {
            self.input.get_or_insert_default().push_str(input);
        }

        Ok(())
    }

    fn finalize(self) -> Result<CustomCall> {
        let name = self.name.ok_or_else(|| anyhow!("Missing tool name"))?;
        let input = self.input.ok_or_else(|| anyhow!("Missing tool input"))?;

        Ok(CustomCall { name, input })
    }
}
