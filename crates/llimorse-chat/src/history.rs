//! Chat log implementation.

use anyhow::anyhow;
use llimorse::line_format::{
    AssistantMessage, ChatMessage, ToolCallParams, ToolResult, UserMessage,
};
use llimorse::{Agent, ChatListener};
use std::collections::HashMap;

/// Type of a chat history entry (for formatting)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryEntryType {
    /// Nothing at all (separator)
    Empty,

    /// User message
    User,

    /// Assistant reply (actual content)
    Content,

    /// Assistant reasoning
    Reasoning,

    /// Tool call (with parameters)
    ToolCall,

    /// Tool call results (on success)
    ToolResultOk,

    /// Tool call error
    ToolResultErr,
}

/// Chat history data
#[derive(Debug, Default)]
pub struct ChatHistory {
    /// Full chat history (split into lines, but not broken by terminal width)
    lines: Vec<(String, HistoryEntryType)>,

    /// Token usage as last reported by the LLM
    token_usage: (usize, usize),

    /// Not-yet-resolved tool calls (for [`Self::push_raw()`])
    open_tool_calls: HashMap<String, ToolCallParams>,
}

impl ChatHistory {
    /// Push and format a raw [`ChatMessage`] into the history.
    pub fn push_raw(&mut self, agent: &Agent<impl ChatListener>, msg: &ChatMessage) {
        match msg {
            // System message are not shown in chat
            ChatMessage::System(_) => (),

            ChatMessage::User(UserMessage { content }) => {
                self.push_lines(content, HistoryEntryType::User, true);
            }

            ChatMessage::Assistant(AssistantMessage {
                reasoning_content,
                content,
                tool_calls,
            }) => {
                if let Some(reasoning) = reasoning_content {
                    self.push_lines(reasoning, HistoryEntryType::Reasoning, true);
                }
                if let Some(content) = content {
                    self.push_lines(content, HistoryEntryType::Content, true);
                }
                if let Some(tool_calls) = tool_calls {
                    for call in tool_calls {
                        self.open_tool_calls
                            .insert(call.id.clone(), call.call.clone());

                        self.push_lines(
                            &format!("[{}] {}\n", call.id, agent.display_call(&call.call)),
                            HistoryEntryType::ToolCall,
                            true,
                        );
                    }
                }
            }

            ChatMessage::Tool(ToolResult {
                tool_call_id,
                content,
            }) => {
                let call = self.open_tool_calls.remove(tool_call_id);
                let name = match &call {
                    Some(ToolCallParams::Function { function }) => &function.name,
                    Some(ToolCallParams::Custom { custom }) => &custom.name,
                    _ => "(unmatched call)",
                };

                match content
                    .strip_prefix("TOOL CALL FAILED: ")
                    .or_else(|| content.strip_prefix("TOOL CALL REJECTED: "))
                {
                    Some(error) => self.push_lines(
                        &format!("=[{name}/{tool_call_id}]=> {error}\n"),
                        HistoryEntryType::ToolResultErr,
                        true,
                    ),
                    None => {
                        let line = if let Some(call) = &call {
                            format!(
                                "=[{name}/{tool_call_id}]=> {}\n",
                                agent.display_call_result(call, content)
                            )
                        } else {
                            format!("=[{name}/{tool_call_id}]=> {content}\n")
                        };

                        self.push_lines(&line, HistoryEntryType::ToolResultOk, true);
                    }
                }
            }
        }
    }

    /// Return the chat history.
    pub fn lines(&self) -> &[(String, HistoryEntryType)] {
        &self.lines
    }

    /// Return the last-reported token usage.
    pub fn token_usage(&self) -> &(usize, usize) {
        &self.token_usage
    }

    /// Report the token usage.
    pub fn set_token_usage(&mut self, token_usage: (usize, usize)) {
        self.token_usage = token_usage;
    }

    /// Append the given string of type `ct` to the history.
    ///
    /// If `force_new_line` is false and the last line is of the same type, append to it; if it is
    /// true, always create a new line.
    pub fn push(&mut self, string: &str, kind: HistoryEntryType, force_new_line: bool) {
        if let Some(last) = self.lines.last_mut() {
            if last.1 == kind && !force_new_line {
                last.0.push_str(string);
                return;
            } else if last.1 != kind {
                // Convert empty lines of different type to type `Empty`, otherwise append `Empty`
                if last.0.is_empty() {
                    last.1 = HistoryEntryType::Empty;
                } else {
                    self.lines.push((String::new(), HistoryEntryType::Empty));
                }
            }
        }

        self.lines.push((string.to_string(), kind));
    }

    /// Append the given multi-line string to the history.
    ///
    /// Splits `string` at newlines and appends each line as its own entry, so that entries in
    /// [`Self::lines()`] never span multiple physical lines.
    ///
    /// If `force_new_line` is false and the last line is of the same type, append to it; if it is
    /// true, always create a new line.
    pub fn push_lines(&mut self, string: &str, kind: HistoryEntryType, mut force_new_line: bool) {
        for line in string.split('\n') {
            self.push(line, kind, force_new_line);
            force_new_line = true; // just saw \n, so next line must be on a new line
        }
    }

    /// Force-resolve all open tool calls.
    ///
    /// Push an error for each tool call that is in the history that has not yet received a result.
    /// This is useful after loading history from an existing session state, which may be
    /// incomplete.
    pub fn force_resolve_unresolved_tool_calls(&mut self, agent: &mut Agent<impl ChatListener>) {
        while let Some(id) = self.open_tool_calls.keys().next() {
            let result = ToolResult::new(
                id.clone(),
                Err(anyhow!(
                    "Tool call aborted due to incomplete session state, please retry"
                )),
            );
            let message: ChatMessage = result.into();
            self.push_raw(agent, &message);
            agent.push(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llimorse::line_format::{FunctionCall, SystemMessage, ToolCall};
    use llimorse::{Agent, Client};

    /// Create a throwaway agent (constructing the client does no network access).
    fn test_agent() -> Agent<()> {
        Agent::new(Client::new("http://localhost"))
    }

    /// Assert the core invariant: no history entry spans multiple physical lines.
    fn assert_single_line_entries(history: &ChatHistory) {
        assert!(
            !history.lines().iter().any(|(line, _)| line.contains('\n')),
            "history entry spans multiple lines: {:#?}",
            history.lines()
        );
    }

    #[test]
    fn push_raw_splits_multiline_messages() {
        let agent = test_agent();
        let mut history = ChatHistory::default();

        history.push_raw(
            &agent,
            &ChatMessage::User(UserMessage {
                content: "line one\nline two".into(),
            }),
        );
        history.push_raw(
            &agent,
            &ChatMessage::Assistant(AssistantMessage {
                reasoning_content: Some("think one\nthink two".into()),
                content: Some("reply one\nreply two".into()),
                tool_calls: None,
            }),
        );

        assert_eq!(
            history.lines(),
            &[
                ("line one".to_owned(), HistoryEntryType::User),
                ("line two".to_owned(), HistoryEntryType::User),
                (String::new(), HistoryEntryType::Empty),
                ("think one".to_owned(), HistoryEntryType::Reasoning),
                ("think two".to_owned(), HistoryEntryType::Reasoning),
                (String::new(), HistoryEntryType::Empty),
                ("reply one".to_owned(), HistoryEntryType::Content),
                ("reply two".to_owned(), HistoryEntryType::Content),
            ]
        );
        assert_single_line_entries(&history);
    }

    #[test]
    fn push_raw_splits_tool_calls_and_results() {
        let agent = test_agent();
        let mut history = ChatHistory::default();

        let call = ToolCall {
            id: "call_1".into(),
            call: ToolCallParams::Function {
                function: FunctionCall {
                    name: "some_tool".into(),
                    arguments: "{}".into(),
                },
            },
        };
        history.push_raw(
            &agent,
            &ChatMessage::Assistant(AssistantMessage {
                reasoning_content: None,
                content: None,
                tool_calls: Some(vec![call]),
            }),
        );
        history.push_raw(
            &agent,
            &ChatMessage::Tool(ToolResult {
                tool_call_id: "call_1".into(),
                content: "result one\nresult two\n".into(),
            }),
        );

        // The tool is not registered with the test agent, so the display falls back to the
        // `[unknown ...]` forms
        assert_eq!(
            history.lines(),
            &[
                (
                    "[call_1] [unknown function some_tool]".to_owned(),
                    HistoryEntryType::ToolCall,
                ),
                (String::new(), HistoryEntryType::Empty),
                (
                    "=[some_tool/call_1]=> [unknown function some_tool]".to_owned(),
                    HistoryEntryType::ToolResultOk,
                ),
                (String::new(), HistoryEntryType::ToolResultOk),
            ]
        );
        assert_single_line_entries(&history);
    }

    #[test]
    fn push_raw_does_not_merge_consecutive_messages() {
        let agent = test_agent();
        let mut history = ChatHistory::default();

        history.push_raw(
            &agent,
            &ChatMessage::User(UserMessage {
                content: "first".into(),
            }),
        );
        history.push_raw(
            &agent,
            &ChatMessage::System(SystemMessage {
                content: "in between".into(),
            }),
        );
        history.push_raw(
            &agent,
            &ChatMessage::User(UserMessage {
                content: "second".into(),
            }),
        );

        // System messages are skipped, so the two user messages end up adjacent — but they must
        // stay separate entries, not be merged into one line
        assert_eq!(
            history.lines(),
            &[
                ("first".to_owned(), HistoryEntryType::User),
                ("second".to_owned(), HistoryEntryType::User),
            ]
        );
        assert_single_line_entries(&history);
    }
}
