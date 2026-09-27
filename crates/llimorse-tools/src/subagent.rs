//! Subagent tool: delegate tasks to a secondary agent.

use anyhow::{Result, anyhow};
use futures::StreamExt;
use llimorse::line_format::{ChatMessage, ToolCall};
use llimorse::{Agent, CallableTool, StreamingChunk};
use std::fmt;

/// Factory for creating the tool set that subagents receive.
pub trait ToolFactory: fmt::Debug + Send + Sync {
    /// Add a fresh set of tools to a new subagent.
    fn add_tools(&self, agent: &mut Agent);
}

#[allow(async_fn_in_trait)]
pub trait SubagentNotifier: fmt::Debug + Send {
    type Error: Into<anyhow::Error> + Send + Sync + 'static;
    type Subagent: SubagentConnector;

    async fn created(
        &self,
        agent: &Agent,
        prompt: &str,
    ) -> std::result::Result<Self::Subagent, Self::Error>;
}

#[allow(async_fn_in_trait)]
pub trait SubagentConnector {
    type Error: Into<anyhow::Error>;

    async fn push_chunk(&self, chunk: StreamingChunk);
    fn tool_call(&self, agent: &Agent, call: &ToolCall) -> std::result::Result<(), Self::Error>;
    fn tool_result(
        &self,
        agent: &Agent,
        call: &ToolCall,
        result: &Result<String, anyhow::Error>,
    ) -> std::result::Result<(), Self::Error>;
}

llimorse::tool! {
    'name: "subagent";

    /// Delegate a task to a subagent to explore questions or execute self-contained tasks without
    /// polluting your context with unrelated tool call results. The subagent will return its final
    /// response.
    #[derive(Debug)]
    'params: pub struct SubagentParams {
        /// The prompt to give to the subagent, which is all it will receive from you
        prompt: String,

        /// If true, the subagent inherits the parent agent's full conversation history, giving it
        /// complete context. If false (default), the subagent starts with a clean context
        /// containing only the prompt.
        #[serde(default)]
        fork: bool,
    }

    /// Result of running a subagent
    'result: pub struct SubagentResult {
        /// The subagent's final response
        response: String,
    }

    /// Delegate tasks to subagents.
    #[derive(Debug)]
    'state: pub struct Subagent<F: ToolFactory, N: SubagentNotifier> {
        /// System prompt pushed into every subagent conversation.
        system_prompt: String,

        /// llama-server base URL (e.g. "http://127.0.0.1:8080").
        llama_url: String,

        /// Model name to use (from the parent's [`ClientState`]).
        model_name: String,

        /// Factory producing the tool set for each subagent.
        tool_factory: F,

        /// Notify e.g. a UI of changes in subagents
        notifier: N,
    }
}

impl<F: ToolFactory, N: SubagentNotifier> Subagent<F, N> {
    /// Create a new subagent tool.
    pub fn new(
        system_prompt: &str,
        llama_url: &str,
        model_name: &str,
        tool_factory: F,
        notifier: N,
    ) -> Self {
        Subagent {
            system_prompt: system_prompt.to_string(),
            llama_url: llama_url.to_string(),
            model_name: model_name.to_string(),
            tool_factory,
            notifier,
        }
    }
}

impl fmt::Display for SubagentParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.fork {
            write!(f, "[fork] {}", self.prompt)
        } else {
            write!(f, "{}", self.prompt)
        }
    }
}

impl fmt::Display for SubagentResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.response)
    }
}

impl<F: ToolFactory, N: SubagentNotifier> CallableTool for Subagent<F, N> {
    async fn execute(&self, parent: &Agent, params: SubagentParams) -> Result<SubagentResult> {
        let client = llimorse::Client::new(&self.llama_url, Some(&self.model_name))
            .await
            .map_err(|err| {
                anyhow!(
                    "Failed to connect to {} to launch a subagent: {err}",
                    self.llama_url
                )
            })?;
        let mut agent = llimorse::Agent::new(client);

        if params.fork {
            // The parent's last message contains this very tool call (and possibly others); drop
            // the calls, so the history does not end in unanswered calls, and drop the whole
            // message if nothing remains
            let mut history = parent.history().to_vec();
            if let Some(ChatMessage::Assistant(msg)) = history.last_mut() {
                msg.tool_calls = None;
            }
            if history.last().is_some_and(ChatMessage::is_empty) {
                history.pop();
            }
            agent.push_history(history);

            // Not all chat templates support system messages in the middle of the conversation,
            // so pass the instructions as part of the user message
            agent.push_user(format!("{}\n\n{}", self.system_prompt, params.prompt));
        } else {
            agent.push_system(&self.system_prompt);
            agent.push_user(&params.prompt);
        }

        self.tool_factory.add_tools(&mut agent);

        let connector = self
            .notifier
            .created(&agent, &params.prompt)
            .await
            .map_err(Into::into)?;

        loop {
            let mut stream = agent.submit().await?;
            while let Some(chunk) = stream.next().await {
                connector.push_chunk(chunk?).await;
            }
            drop(stream);

            if !agent
                .execute_pending_calls(
                    |agent, call| connector.tool_call(agent, call).map_err(Into::into),
                    |agent, call, result| {
                        connector
                            .tool_result(agent, call, result)
                            .map_err(Into::into)
                    },
                )
                .await
            {
                break;
            }
        }

        Ok(SubagentResult {
            response: agent
                .last_result()
                .cloned()
                .ok_or_else(|| anyhow!("Subagent produced no response"))?,
        })
    }
}
