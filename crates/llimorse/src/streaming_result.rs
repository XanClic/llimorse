//! Helper object to manage streaming result

use super::agent::AgentStage;
use super::client::{ClientState, TokenUsage};
use super::line_format::{AssistantMessage, CustomCall, FunctionCall, ToolCall, ToolCallParams};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::Stream;
use futures::stream::FusedStream;
use parking_lot::RwLock;
use pin_project::pin_project;
use serde::Deserialize;
use std::collections::VecDeque;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;
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

    /// State of the client (which we change)
    client_state: Arc<RwLock<ClientState>>,

    /// The current agent stage, mirroring `client_stage.operation_stage`
    ///
    /// Cached here so we only have to take the write lock on `client_stage` if the stage actually
    /// changes.
    ///
    /// `None` means unknown.
    stage: Option<AgentStage>,
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

    /// Prefill progress (llama.cpp extension; present on non-token chunks)
    #[serde(default)]
    prompt_progress: Option<PromptProgress>,
}

/// Prefill progress as reported by llama-server (`prompt_progress` chunk field)
#[derive(Clone, Debug, Deserialize)]
struct PromptProgress {
    /// Total tokens in the prompt (known before prefill starts)
    #[serde(default)]
    total: u32,

    /// Tokens reused from the KV cache (not re-processed)
    #[serde(default)]
    cache: u32,

    /// Prompt tokens taken in so far: cached prefix + computed/queued (absolute position in the
    /// prompt; starts at `cache`, ends at `total`)
    #[serde(default)]
    processed: u32,

    /// Milliseconds elapsed in prompt processing
    ///
    /// Part of the wire format; not used for the moment.
    #[serde(default)]
    #[allow(dead_code)]
    time_ms: u64,
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
    pub fn from_stream(stream: S, client_state: Arc<RwLock<ClientState>>) -> Self {
        StreamingResult {
            stream: stream.into(),
            chunks: VecDeque::new(),
            done: false,
            constructing: Default::default(),
            full_message: None,
            client_state,
            stage: None,
        }
    }

    /// Return the full message after streaming is done.
    ///
    /// Will only return `Some(_)` once streaming is done ([`Self::is_terminated()`] returns true)
    /// and only if there was no error; *or* after [`Self::force_finalize()`].
    ///
    /// `.take()`s the full message, so will return it only once.
    pub(crate) fn full_message_pinned(self: Pin<&mut Self>) -> Option<AssistantMessage> {
        self.project().full_message.take()
    }

    /// Return the full message after streaming is done.
    ///
    /// Will only return `Some(_)` once streaming is done ([`Self::is_terminated()`] returns true)
    /// and only if there was no error; *or* after [`Self::force_finalize()`].
    ///
    /// `.take()`s the full message, so will return it only once.
    pub(crate) fn full_message(&mut self) -> Option<AssistantMessage> {
        self.full_message.take()
    }

    /// Abort the incoming transmission, and treat it as finished
    pub fn force_finalize(&mut self) {
        self.full_message = Some(mem::take(&mut self.constructing).force_finalize());
        self.stream.terminate();
        mem::take(&mut self.chunks);
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

            self.enter_idle();
            return Poll::Ready(result);
        }

        let Poll::Ready(line) = self.stream.as_mut().poll_next(ctx) else {
            return Poll::Pending;
        };

        let line = match line {
            Some(Ok(line)) => line,
            Some(Err(err)) => {
                self.enter_idle();
                return Poll::Ready(Some(Err(err.context("reading completion stream"))));
            }
            None => {
                self.enter_idle();
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
                    self.enter_idle();
                    return Poll::Ready(Some(Err(err.context("completion stream invalid"))));
                }
            };

            if let Err(err) = self.apply_chunk(parsed) {
                self.enter_idle();
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
    /// - [`StreamingResult::client_state`] (if in the input)
    fn apply_chunk(&mut self, chunk: StreamChunk) -> Result<()> {
        if let Some(progress) = &chunk.prompt_progress {
            let mut client_state = self.client_state.write();
            client_state.token_usage = TokenUsage {
                prompt_tokens: progress.processed as usize,
                completion_tokens: None,
                prefill_target: Some(progress.total as usize),
                cached_tokens: progress.cache as usize,
                streamed_tokens: 0,
            };
        }

        for choice in chunk.choices {
            // Progress events carry an (empty) choice but are not tokens
            if chunk.prompt_progress.is_none() {
                self.client_state.write().token_usage.streamed_tokens += 1;
            }
            self.choice_received(choice)?;
        }

        if let Some(u) = chunk.usage {
            let mut client_state = self.client_state.write();
            let cached_tokens = client_state.token_usage.cached_tokens;
            client_state.token_usage = TokenUsage {
                prompt_tokens: u.prompt_tokens as usize,
                completion_tokens: Some(u.completion_tokens as usize),
                prefill_target: None,
                cached_tokens,
                streamed_tokens: 0,
            };
        }

        Ok(())
    }

    /// Apply `choice` to [`StreamingResult::constructing`].
    fn choice_received(&mut self, choice: StreamChoice) -> Result<()> {
        self.constructing.push(&choice.delta)?;

        if let Some(reasoning) = choice.delta.reasoning_content {
            self.set_stage(AgentStage::Reasoning);
            self.chunks.push_back(StreamingChunk::Reasoning(reasoning));
        }

        if let Some(text) = choice.delta.content {
            self.set_stage(AgentStage::ResponseGeneration);
            self.chunks.push_back(StreamingChunk::Content(text));
        }

        if choice.delta.tool_calls.is_some() {
            self.set_stage(AgentStage::ToolCallGeneration);
        }

        Ok(())
    }

    /// Mark as terminated (in case of error).
    fn terminate(&mut self) {
        self.stream.as_mut().project().terminate();
        mem::take(self.chunks);
    }

    /// Set the current agent stage
    pub fn set_stage(&mut self, stage: AgentStage) {
        if *self.stage != Some(stage) {
            self.client_state.write().operation_stage = stage;
            *self.stage = Some(stage);
        }
    }

    /// Request is done, return to idle
    pub fn enter_idle(&mut self) {
        self.set_stage(AgentStage::Idle);
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

impl<S: Stream<Item = reqwest::Result<bytes::Bytes>>> NewlineSplit<S> {
    /// Mark as terminated (in case of error).
    fn terminate(&mut self) {
        self.eof = true;
        mem::take(&mut self.buffer);
    }
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

    /// Force-finalize the object, turning it into the non-streaming variant, discarding fatally
    /// incomplete parts (e.g. tool calls).
    fn force_finalize(self) -> Self::NonStreaming;
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

    fn force_finalize(self) -> AssistantMessage {
        // We completely discard all tool calls because you never know if they are complete or not
        // (TODO: Find out how we can find out whether they are complete or not. It might be that
        // llama-server gives them strictly in order, so if we have *some* information about tool
        // call `i+1`, tool call `i` must be complete.)
        AssistantMessage {
            content: self.content,
            reasoning_content: self.reasoning_content,
            tool_calls: Default::default(),
        }
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

    fn force_finalize(self) -> ToolCall {
        todo!("Incomplete tool calls forbidden for the moment")
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

    fn force_finalize(self) -> ToolCallParams {
        todo!("Incomplete tool calls forbidden for the moment")
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

    fn force_finalize(self) -> FunctionCall {
        todo!("Incomplete tool calls forbidden for the moment")
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

    fn force_finalize(self) -> CustomCall {
        todo!("Incomplete tool calls forbidden for the moment")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// A stream that yields a fixed sequence of byte chunks, then ends.
    struct FakeStream(std::vec::IntoIter<bytes::Bytes>);

    impl Stream for FakeStream {
        type Item = reqwest::Result<bytes::Bytes>;

        fn poll_next(mut self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.next().map(Ok))
        }
    }

    /// A stream that yields a single chunk and then stalls, like a server that
    /// is still processing the prompt.
    struct StalledStream(Option<bytes::Bytes>);

    impl Stream for StalledStream {
        type Item = reqwest::Result<bytes::Bytes>;

        fn poll_next(mut self: Pin<&mut Self>, _ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.0.take() {
                Some(chunk) => Poll::Ready(Some(Ok(chunk))),
                None => Poll::Pending,
            }
        }
    }

    /// A fresh [`ClientState`] for test streams.
    fn test_state() -> Arc<RwLock<ClientState>> {
        Arc::new(RwLock::new(ClientState {
            token_usage: Default::default(),
            operation_stage: Default::default(),
        }))
    }

    /// Wrap an SSE payload in a `data:` frame.
    fn sse(payload: &str) -> bytes::Bytes {
        format!("data: {payload}\n").into()
    }

    /// A progress event must update the prefill state and count as no tokens.
    #[tokio::test]
    async fn progress_event_updates_prefill_state_without_counting_tokens() {
        let state = test_state();
        let stream = StalledStream(Some(sse(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}],"prompt_progress":{"total":1000,"cache":400,"processed":500,"time_ms":12}}"#,
        )));
        let mut result = Box::pin(StreamingResult::from_stream(stream, state.clone()));

        // The progress event emits no chunk; one poll consumes it and then
        // stalls waiting for the rest of the stream
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(result.as_mut().poll_next(&mut cx).is_pending());

        let state = state.read();
        // `processed` is the queued frontier: already includes the cached prefix
        assert_eq!(state.token_usage.prompt_tokens, 500);
        assert_eq!(state.token_usage.cached_tokens, 400);
        assert_eq!(state.token_usage.prefill_target, Some(1000));
        assert_eq!(state.token_usage.streamed_tokens, 0);
    }

    /// Progress events feed the client state, content chunks count as tokens,
    /// and the usage chunk stores the authoritative counts and clears the
    /// prefill fields.
    #[tokio::test]
    async fn prompt_progress_lifecycle() {
        let state = test_state();
        let stream = FakeStream(
            vec![
                sse(
                    r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}],"prompt_progress":{"total":1000,"cache":400,"processed":500,"time_ms":12}}"#,
                ),
                sse(r#"{"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#),
                sse(r#"{"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":7}}"#),
                sse("[DONE]"),
            ]
            .into_iter(),
        );
        let mut result = Box::pin(StreamingResult::from_stream(stream, state.clone()));

        // The progress event emits no chunk; the first chunk out is the content one
        let chunk = result.next().await.unwrap().unwrap();
        assert!(matches!(chunk, StreamingChunk::Content(ref text) if text == "Hi"));

        {
            let state = state.read();
            assert_eq!(state.token_usage.prompt_tokens, 500);
            assert_eq!(state.token_usage.cached_tokens, 400);
            assert_eq!(state.token_usage.prefill_target, Some(1000));
            assert_eq!(state.token_usage.streamed_tokens, 1);
            assert_eq!(state.operation_stage, AgentStage::ResponseGeneration);
        }

        // The usage event and [DONE] emit no chunks; both end the request
        assert!(result.next().await.is_none());

        let state = state.read();
        assert_eq!(state.token_usage.prompt_tokens, 1000);
        assert_eq!(state.token_usage.completion_tokens, Some(7));
        assert_eq!(state.token_usage.prefill_target, None);
        // The cache count is only observable during the prefill, so it must
        // survive the usage payload
        assert_eq!(state.token_usage.cached_tokens, 400);
        assert_eq!(state.token_usage.streamed_tokens, 0);
        assert_eq!(state.operation_stage, AgentStage::Idle);
    }
}
