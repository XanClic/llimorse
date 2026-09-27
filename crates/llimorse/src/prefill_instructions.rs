//! Prefill the chat context by user-defined instructions

use crate::Agent;
use crate::line_format::ToolCall;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Prefill the chat context
#[allow(async_fn_in_trait)]
pub trait Prefill: Sized {
    /// Consume this object to prefill the chat context from it.
    async fn execute(self, agent: &mut Agent) -> Result<()>;
}

/// Instructions on how to prefill the chat context e.g. with system information read from files
#[derive(Debug, Deserialize, Serialize)]
pub struct PrefillInstructions(Vec<Instruction>);

/// An instruction for getting something into the chat context
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
enum Instruction {
    /// Push a message into the context
    Message(Message),

    /// Execute a tool call
    ToolCall(ToolCall),
}

/// Simple message
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct Message {
    /// What role to give to the result
    role: MessageTarget,

    /// Where to get the data from
    source: MessageSource,
}

/// How to represent message data
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum MessageTarget {
    /// System/admin message
    System,

    /// User message
    User,
}

/// Where to get message data from
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum MessageSource {
    /// Data is defined in-line as a string
    Inline(String),

    /// Data is to be read from the given file
    File(PathBuf),
}

impl PrefillInstructions {
    /// Load the instructions from the given `path`.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let insns = serde_json::from_str(&content)?;
        Ok(insns)
    }
}

impl Prefill for PrefillInstructions {
    async fn execute(self, agent: &mut Agent) -> Result<()> {
        for insn in self.0 {
            insn.execute(agent).await?;
        }
        Ok(())
    }
}

impl Prefill for Instruction {
    async fn execute(self, agent: &mut Agent) -> Result<()> {
        match self {
            Instruction::Message(msg) => msg.execute(agent).await,
            Instruction::ToolCall(call) => call.execute(agent).await,
        }
    }
}

impl Prefill for Message {
    async fn execute(self, agent: &mut Agent) -> Result<()> {
        let data = match self.source {
            MessageSource::Inline(s) => s,
            MessageSource::File(ref path) => {
                fs::read_to_string(path).map_err(|err| anyhow!("{self:?}: {err}"))?
            }
        };

        match self.role {
            MessageTarget::System => agent.push_system(data),
            MessageTarget::User => agent.push_user(data),
        }

        Ok(())
    }
}

impl Prefill for ToolCall {
    async fn execute(self, agent: &mut Agent) -> Result<()> {
        agent.push_tool_call(self);

        let mut error = None::<anyhow::Error>;
        agent
            .execute_pending_calls(
                |_, _| Ok(()),
                |_, _, result| {
                    if let Err(err) = result {
                        // One call, one result only
                        assert!(error.is_none());
                        error = Some(anyhow!("{err}"));
                    }
                    Ok(())
                },
            )
            .await;

        if let Some(err) = error {
            Err(err)
        } else {
            Ok(())
        }
    }
}
