//! Chat log implementation.

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
}

impl ChatHistory {
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
    /// If `force_new_line` is true, append it to the prior line if the type matches; if it is
    /// false, always create a new line.
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
}
