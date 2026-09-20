//! Client for llama-server’s (llama.cpp) OpenAI-compatible chat completions API, using native
//! template tool-calling (--jinja) and SSE streaming.

use super::line_format::{
    ChatCompletion, ChatMessage, Models, StreamOptions, ToolChoice, ToolDefinition,
};
use super::streaming_result::StreamingResult;
use anyhow::{Result, anyhow, bail};
use futures::Stream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time;
use tracing::{debug, warn};

/// Connection to llama-server
#[derive(Debug)]
pub struct Client {
    /// HTTP client, handling the actual network connection
    http: reqwest::Client,

    /// Chat endpoint URL
    url: String,

    /// Model to use
    model: String,

    /// Context size in tokens
    context_size: Option<u64>,

    /// Token usage (how much of the context is used)
    token_usage: Arc<TokenUsage>,
}

/// Token counts from a completed chat request
#[derive(Debug, Default)]
pub struct TokenUsage {
    /// Tokens in the prompt
    pub prompt_tokens: AtomicUsize,

    /// New tokens produced
    pub completion_tokens: AtomicUsize,

    /// Tokens being streamed
    pub streamed_tokens: AtomicUsize,
}

impl Client {
    /// Connect to the given `base_url` llama-server instance.
    ///
    /// If `model_name` is given, select the given model from what is available on the server. If
    /// `None`, and there is only a single model, select that; if there are more, select the
    /// `"default"` model.
    pub async fn new(base_url: &str, model_name: Option<&str>) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Per-read, not total: a total timeout counts the whole SSE stream against the
            // deadline, so it kills long generations (at ~19 t/s, 600 s cut off at ~11k tokens)
            // instead of detecting a stalled server.  This resets on every chunk, so it only fires
            // when nothing arrives at all.  Generous, because it also covers prompt processing
            // before the first token.
            .read_timeout(time::Duration::from_secs(300))
            .build()
            .expect("building http client");

        let base_url = base_url.trim_end_matches('/');
        let models_url = format!("{base_url}/v1/models");
        let models_info = http
            .get(&models_url)
            .send()
            .await
            .map_err(|err| anyhow!("Failed to query model info on {models_url}: {err}"))?;
        let status = models_info.status();
        let models_info = models_info
            .text()
            .await
            .map_err(|err| anyhow!("Failed to query model info on {models_url}: {err}"))?;
        if !status.is_success() {
            bail!("Failed to query model info on {models_url}: {status}: {models_info}")
        }

        let models_info: Models = serde_json::from_str(&models_info).map_err(|err| {
            anyhow!("Failed to parse model info from {models_url}: {models_info}: {err}")
        })?;

        let model = if model_name.is_none() && models_info.data.len() == 1 {
            &models_info.data[0]
        } else {
            let model_name = model_name.unwrap_or("default");
            models_info
                .data
                .iter()
                .find(|model| {
                    model.id == model_name || model.aliases.iter().any(|alias| alias == model_name)
                })
                .ok_or_else(|| {
                    let models = models_info
                        .data
                        .iter()
                        .map(|model| {
                            let aliases = model
                                .aliases
                                .iter()
                                .map(|a| format!("\"{a}\")"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("- {} (aliases: {aliases})", model.id)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    anyhow!("Model \"{model_name}\" not found, available models:\n{models}")
                })?
        };

        Ok(Client {
            http,
            url: format!("{base_url}/v1/chat/completions"),
            model: model.id.clone(),
            context_size: model.meta.as_ref().and_then(|m| m.n_ctx),
            token_usage: Default::default(),
        })
    }

    /// Submit the given chat history, providing the given tools.
    ///
    /// Note on the return type: The `impl Stream` actually uses none of the lifetimes and types
    /// here, but Rust currently requires specifying all types in `use<>` anyway.
    pub async fn chat_stream<'tc, T: Into<ToolChoice<'tc>>>(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tool_choice: T,
    ) -> Result<StreamingResult<impl Stream<Item = reqwest::Result<bytes::Bytes>> + use<T>>> {
        let request = ChatCompletion {
            model: &self.model,
            messages,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
                ..Default::default()
            },
            tools,
            tool_choice: tool_choice.into(),
        };

        let response = self.submit_request(request, 3).await?;

        Ok(StreamingResult::from_stream(
            response.bytes_stream(),
            &self.token_usage,
        ))
    }

    /// Request completion of the given `request` from the LLM.
    ///
    /// Retry benign errors `max_attempts` times.
    async fn submit_request(
        &self,
        request: ChatCompletion<'_>,
        max_attempts: usize,
    ) -> Result<reqwest::Response> {
        debug!(
            "LLM request: {} messages, last message: {:?}",
            request.messages.len(),
            request.messages.last(),
        );

        debug!("Full JSON: {}", serde_json::to_string(&request).unwrap());

        let mut attempt = 0;
        let response = loop {
            attempt += 1;

            match self.http.post(&self.url).json(&request).send().await {
                Ok(res) => break res,
                Err(e) => {
                    let is_transient = e.is_connect() || e.is_timeout() || e.is_request();
                    if !is_transient || attempt >= max_attempts {
                        bail!(
                            "sending chat request to {} (attempt {}): {}",
                            self.url,
                            attempt,
                            e
                        )
                    }

                    let delay = time::Duration::from_millis(500 * attempt as u64);
                    warn!(
                        attempt,
                        max_attempts,
                        delay_ms = delay.as_millis(),
                        error = %e,
                        "chat request failed, retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("llama-server returned {status}: {body}");
        }

        Ok(response)
    }

    /// Return the token usage stat object
    pub fn token_usage(&self) -> &Arc<TokenUsage> {
        &self.token_usage
    }

    /// Return the name of the model in use
    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Return the number of tokens that fit into the context
    pub fn context_size(&self) -> Option<u64> {
        self.context_size
    }
}

impl TokenUsage {
    /// The full sum of all tokens in the context
    pub fn sum(&self) -> usize {
        self.prompt_tokens.load(Ordering::Relaxed)
            + self.completion_tokens.load(Ordering::Relaxed)
            + self.streamed_tokens.load(Ordering::Relaxed)
    }
}
