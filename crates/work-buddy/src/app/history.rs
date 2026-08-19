//! Chat log implementation.

use std::borrow::Cow;

/// Type of a chat history entry (for formatting)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HistoryEntryType {
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
pub(super) struct ChatHistory {
    /// Full chat history (split into lines, but not broken by terminal width)
    lines: Vec<(String, HistoryEntryType)>,

    /// Token usage as last reported by the LLM
    token_usage: (usize, usize),
}

impl From<HistoryEntryType> for ratatui::style::Style {
    fn from(ct: HistoryEntryType) -> Self {
        match ct {
            HistoryEntryType::Empty => ratatui::style::Style::default(),
            HistoryEntryType::User => ratatui::style::Style::default().bold().magenta(),
            HistoryEntryType::Content => ratatui::style::Style::default().bold(),
            HistoryEntryType::Reasoning => ratatui::style::Style::default().italic(),
            HistoryEntryType::ToolCall => ratatui::style::Style::default().blue(),
            HistoryEntryType::ToolResultOk => ratatui::style::Style::default().green(),
            HistoryEntryType::ToolResultErr => ratatui::style::Style::default().bold().red(),
        }
    }
}

impl From<HistoryEntryType> for ratatui::layout::Alignment {
    fn from(ct: HistoryEntryType) -> Self {
        match ct {
            HistoryEntryType::Empty => ratatui::layout::Alignment::Left,
            HistoryEntryType::User => ratatui::layout::Alignment::Right,
            HistoryEntryType::Content => ratatui::layout::Alignment::Left,
            HistoryEntryType::Reasoning => ratatui::layout::Alignment::Left,
            HistoryEntryType::ToolCall => ratatui::layout::Alignment::Left,
            HistoryEntryType::ToolResultOk => ratatui::layout::Alignment::Left,
            HistoryEntryType::ToolResultErr => ratatui::layout::Alignment::Left,
        }
    }
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

    /// Helper function to convert the given iterator of `HistoryEntryType`-annotated lines into
    /// ratatui lines.
    pub fn into_ratatui_lines<'a, I: Iterator<Item = (Cow<'a, str>, HistoryEntryType)>>(
        iter: I,
    ) -> Vec<ratatui::text::Line<'a>> {
        iter.map(|line| ratatui::text::Line {
            style: line.1.into(),
            alignment: Some(line.1.into()),
            spans: vec![ratatui::text::Span {
                style: Default::default(),
                content: line.0,
            }],
        })
        .collect()
    }
}
