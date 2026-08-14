//! Line-format objects for llama-server OpenAI-compatible communication.

#![allow(dead_code)]

use anyhow::Result;
use schemars::Schema;
use serde::{Deserialize, Serialize};

/// A basic chat message
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    /// System-level instructions
    System(SystemMessage),

    /// User message
    User(UserMessage),

    /// Assistant message
    Assistant(AssistantMessage),

    /// Tool call result
    Tool(ToolResult),
}

/// A system-level instruction to the assistant
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SystemMessage {
    /// Message content
    pub content: String,
}

/// Message from the user to the assistant
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UserMessage {
    /// Message content
    pub content: String,
}

/// Message from the assistant to the user or system
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AssistantMessage {
    /// Message content
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Private reasoning (“thinking”), as separated out by llama-server.
    ///
    /// Skipped on serialize, so reasoning is not replayed into later context.
    #[serde(default, skip_serializing)]
    pub reasoning_content: Option<String>,

    /// Tool calls to be performed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Request a tool call
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    /// ID to reference in the result
    pub id: String,

    /// Which tool to call, and how
    #[serde(flatten)]
    pub call: ToolCallParams,
}

/// Tool call name and parameters
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallParams {
    /// Function tool call
    Function { function: FunctionCall },

    /// Custom tool call
    Custom { custom: CustomCall },
}

/// Request a function tool call
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FunctionCall {
    /// Function name
    pub name: String,

    /// Arguments in JSON format
    pub arguments: String,
}

/// Request a custom tool call
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CustomCall {
    /// Custom tool name
    pub name: String,

    /// Input to feed into the tool
    pub input: String,
}

/// Tool call result from the system to the assistant
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    /// Reference to the tool call
    pub tool_call_id: Option<String>,

    /// Result
    pub content: String,
}

/// A chat to continue, as sent to the LLM
#[derive(Clone, Debug, Serialize)]
pub struct ChatCompletion<'a> {
    /// Which model to use (e.g. "default")
    pub model: &'a str,

    /// Chat history
    pub messages: &'a [ChatMessage],

    /// Whether to use streaming (i.e. send output as it is generated)
    pub stream: bool,

    /// Options for streaming
    pub stream_options: StreamOptions,

    /// Which tools are available
    #[serde(default, skip_serializing_if = "<[ToolDefinition]>::is_empty")]
    pub tools: &'a [ToolDefinition],

    /// How to use tools
    pub tool_choice: ToolChoice<'a>,
}

/// Option for streaming responses
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StreamOptions {
    /// Something about obfuscating against side-channel attacks
    #[serde(default, skip_serializing_if = "Clone::clone")]
    pub include_obfuscation: bool,

    /// Provide token usage information before `[DONE]`
    #[serde(default, skip_serializing_if = "Clone::clone")]
    pub include_usage: bool,
}

/// Tool description
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDefinition {
    /// A function tool generating responses
    Function { function: FunctionDefinition },

    /// A custom tool processing input
    Custom { custom: CustomToolDefinition },
}

/// Function definition
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FunctionDefinition {
    /// Function name
    pub name: String,

    /// Description of what this does
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Function parameters
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Schema>,

    /// Require strict adherence to the parameter definition
    #[serde(default, skip_serializing_if = "Clone::clone")]
    pub strict: bool,
}

/// Custom tool definition
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CustomToolDefinition {
    /// Tool name
    pub name: String,

    /// Description of what this does
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Input format for this tool
    #[serde(default)]
    pub format: CustomToolFormat,
}

/// Input format for custom tools
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CustomToolFormat {
    /// Free-form text
    #[default]
    Text,

    /// Specific grammar
    Grammar(CustomToolGrammar),
}

/// Input format grammar specification for custom tools
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "syntax", rename_all = "snake_case")]
pub enum CustomToolGrammar {
    /// Lark (EBNF-like) definition
    Lark {
        /// Grammar specification in Lark syntax
        definition: String,
    },

    /// Regex definition
    Regex {
        /// Grammar specification in Regex syntax
        definition: String,
    },
}

/// Prescribe tool use
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum ToolChoice<'a> {
    /// General choice for all tools
    Mode(ToolChoiceMode),

    /// Prescribe specific tools
    ToolChoice(ToolChoiceSet<'a>),
}

/// Whether to use tools in general
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    /// Suppress tool use
    None,

    /// Let the model decide
    Auto,

    /// Require use of at least one or more tools
    Required,
}

/// Prescribe specific tools to use
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "id", rename_all = "snake_case")]
pub enum ToolChoiceSet<'a> {
    /// Constrains the model to use specific tools
    AllowedTools {
        /// Whether the tools are allowed or required; must not be `None`
        mode: ToolChoiceMode,

        /// The list of tools
        tools: &'a [ToolReference<'a>],
    },

    /// Force the model to use this function
    Function(FunctionReference<'a>),

    /// Force the model to use this custom tool
    Custom(CustomReference<'a>),
}

/// References a function
#[derive(Clone, Debug, Serialize)]
pub struct FunctionReference<'a> {
    pub function: &'a str,
}

/// References a custom tool
#[derive(Clone, Debug, Serialize)]
pub struct CustomReference<'a> {
    pub custom: &'a str,
}

/// References any tool (function or custom)
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum ToolReference<'a> {
    Function(FunctionReference<'a>),
    Custom(CustomReference<'a>),
}

impl From<SystemMessage> for ChatMessage {
    fn from(system: SystemMessage) -> Self {
        ChatMessage::System(system)
    }
}

impl From<UserMessage> for ChatMessage {
    fn from(user: UserMessage) -> Self {
        ChatMessage::User(user)
    }
}

impl From<AssistantMessage> for ChatMessage {
    fn from(assistant: AssistantMessage) -> Self {
        ChatMessage::Assistant(assistant)
    }
}

impl From<ToolResult> for ChatMessage {
    fn from(result: ToolResult) -> Self {
        ChatMessage::Tool(result)
    }
}

impl From<String> for SystemMessage {
    fn from(message: String) -> Self {
        SystemMessage { content: message }
    }
}

impl From<String> for UserMessage {
    fn from(message: String) -> Self {
        UserMessage { content: message }
    }
}

impl ToolResult {
    pub fn new(call_id: String, result: Result<String>) -> Self {
        let content = match result {
            Ok(result) => result,
            Err(err) => format!("TOOL CALL FAILED: {err}"),
        };

        ToolResult {
            tool_call_id: Some(call_id),
            content,
        }
    }
}

impl From<ToolChoiceMode> for ToolChoice<'static> {
    fn from(mode: ToolChoiceMode) -> Self {
        ToolChoice::Mode(mode)
    }
}

impl<'a> From<ToolChoiceSet<'a>> for ToolChoice<'a> {
    fn from(set: ToolChoiceSet<'a>) -> Self {
        ToolChoice::ToolChoice(set)
    }
}
