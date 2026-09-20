//! Auto-truncate strings for display

use std::fmt::{self, Debug, Display};

/// Auto-truncate objects at a given length for displaying
pub trait TruncatedDisplay {
    /// Display `self` with at most `max_length` `char`s, including a trailing ‘…’ in case of
    /// truncation.
    ///
    /// The returned object implements both `Debug` and `Display`, depending on how you want to
    /// display.
    fn truncated_display(&self, max_length: usize) -> impl Debug + Display;
}

impl TruncatedDisplay for &str {
    fn truncated_display(&self, max_length: usize) -> impl Debug + Display {
        TruncatedDisplayStr {
            string: self,
            max_length,
        }
    }
}

impl TruncatedDisplay for String {
    fn truncated_display(&self, max_length: usize) -> impl Debug + Display {
        TruncatedDisplayStr {
            string: self,
            max_length,
        }
    }
}

/// Implement `Debug` and `Display` for truncating strings after a given maximum length
struct TruncatedDisplayStr<'a> {
    /// The string to display
    string: &'a str,
    /// The maximum number of `char`s to display (including trailing … in case of truncate)
    max_length: usize,
}

impl Debug for TruncatedDisplayStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.max_length == 0 {
            return Ok(());
        }

        let mut indices = self.string.char_indices().skip(self.max_length - 1);
        if let Some(cutoff) = indices.next()
            && indices.next().is_some()
        {
            write!(f, "{:?}…", &self.string[..cutoff.0])
        } else {
            write!(f, "{:?}", self.string)
        }
    }
}

impl Display for TruncatedDisplayStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.max_length == 0 {
            return Ok(());
        }

        let mut indices = self.string.char_indices().skip(self.max_length - 1);
        if let Some(cutoff) = indices.next()
            && indices.next().is_some()
        {
            write!(f, "{}…", &self.string[..cutoff.0])
        } else {
            write!(f, "{}", self.string)
        }
    }
}
