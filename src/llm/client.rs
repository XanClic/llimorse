//! Client for llama-server’s (llama.cpp) OpenAI-compatible chat completions API, using native
//! template tool-calling (--jinja) and SSE streaming.

use super::line_format::{ChatCompletion, ChatMessage, StreamOptions, ToolChoice, ToolDefinition};
use super::streaming_result::StreamingResult;
use anyhow::{Result, bail};
use futures::Stream;
use std::time;
use tracing::{debug, warn};

/// Connection to llama-server
pub struct Client {
    /// HTTP client, handling the actual network connection
    http: reqwest::Client,

    /// Chat endpoint URL
    url: String,
}

impl Client {
    pub fn new(base_url: &str) -> Self {
        Client {
            http: reqwest::Client::builder()
                .timeout(time::Duration::from_secs(600))
                .build()
                .expect("building http client"),
            url: format!("{}/v1/chat/completions", base_url.trim_end_matches('/')),
        }
    }

    /// Submit the given chat history, providing the given tools.
    ///
    /// Note on the retun type: The `impl Stream` actually uses none of the lifetimes and types
    /// here, but Rust currently requires specifying all types in `use<>` anyway.
    pub async fn chat_stream<'tc, T: Into<ToolChoice<'tc>>>(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tool_choice: T,
    ) -> Result<StreamingResult<impl Stream<Item = reqwest::Result<bytes::Bytes>> + use<T>>> {
        let request = ChatCompletion {
            model: "default",
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

        Ok(response.bytes_stream().into())
    }

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
}
