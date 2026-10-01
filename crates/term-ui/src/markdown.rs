//! Very simple markdown-y “parser”.
//!
//! The parser turns markdown-flavored text into rendered text (syntax removed) plus a style
//! register: a list of `(offset, style)` pairs, where `offset` is a byte offset into the rendered
//! text and `style` is the set of markdown effects in effect from that offset until the next one
//! (or the end of the text). Text before the first entry has no markdown styling. How the effects
//! are represented is a theme decision made at render time.

/// A markdown effect that can be active in a region of text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Effect {
    /// `**text**`
    Bold,
    /// `*text*`
    Italic,
    /// `` `text` ``
    Code,
}

/// The set of markdown effects active at a point in the text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MarkdownStyle(u8);

impl MarkdownStyle {
    /// The bit of the given effect in the bitmask.
    const fn bit(effect: Effect) -> u8 {
        1 << effect as u8
    }

    /// Whether the given effect is active.
    pub const fn contains(self, effect: Effect) -> bool {
        self.0 & Self::bit(effect) != 0
    }

    /// The style with the given effect enabled.
    pub const fn with(self, effect: Effect) -> Self {
        Self(self.0 | Self::bit(effect))
    }

    /// The style with the given effect disabled.
    pub const fn without(self, effect: Effect) -> Self {
        Self(self.0 & !Self::bit(effect))
    }

    /// The style with the given effect flipped.
    pub const fn toggle(self, effect: Effect) -> Self {
        Self(self.0 ^ Self::bit(effect))
    }
}

/// Parses markdown-flavored text line by line, carrying region state across lines.
#[derive(Debug, Default)]
pub struct Parser {
    /// The style of the region currently being emitted (persists across lines)
    current: MarkdownStyle,

    /// The next byte is escaped and dropped
    escaped: bool,
    /// A `*` region is open
    asterisk: bool,
    /// Position of the `*` that opened the current `*`/`**` region (a byte offset into the last
    /// pushed line), if any.
    asterisk_at: Option<usize>,
}

impl Parser {
    /// Parse one line of (possibly partial) markdown, continuing from the state of previous calls.
    ///
    /// Return the rendered text (syntax removed) and a style register of byte offsets into it at
    /// which the style changes; each entry’s style lasts until the next offset (or the end of the
    /// text).
    pub fn push(&mut self, block: &str) -> (String, Vec<(usize, MarkdownStyle)>) {
        let mut output = Output::new(block);
        let bytes = block.as_bytes();
        let mut i = 0;

        while i < bytes.len() {
            if self.escaped {
                self.escaped = false;
                i += 1;
                continue;
            }

            let c = bytes[i];

            if !self.current.contains(Effect::Code) && c == b'*' {
                if self.asterisk {
                    // Close: an opener on the previous byte means `**` (bold), a lone `*`
                    // means italic. The `**` close both ends the bold region it closes and
                    // starts the one it opens, so Bold is toggled.
                    if i >= 1 && self.asterisk_at == Some(i - 1) {
                        output.emit(self.current, i - 1, 2);
                        self.current = self.current.without(Effect::Italic).toggle(Effect::Bold);
                    } else {
                        output.emit(self.current, i, 1);
                        self.current = self.current.without(Effect::Italic);
                    }
                    self.asterisk = false;
                    self.asterisk_at = None;
                } else {
                    output.emit(self.current, i, 1);
                    // Tentatively italic; a `**` open is corrected on the next byte
                    self.current = self.current.with(Effect::Italic);
                    self.asterisk = true;
                    self.asterisk_at = Some(i);
                }
                i += 1;
                continue;
            }

            if c == b'\\' {
                self.escaped = true;
                i += 1;
                continue;
            }

            if c == b'`' {
                let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                if run >= 3 {
                    // A code fence: ignore it and keep the backticks as plain text
                    output.emit(self.current, i + run, 0);
                    i += run;
                } else {
                    output.emit(self.current, i, 1);
                    self.current = self.current.toggle(Effect::Code);
                    i += 1;
                }
            } else {
                i += 1;
            }
        }

        // Flush the trailing run
        output.emit(self.current, block.len(), 0);
        (output.text, output.styles)
    }
}

/// Accumulates the rendered text and style register of one line.
struct Output<'a> {
    /// The line being parsed
    input: &'a str,
    /// The rendered text so far
    text: String,
    /// The style register so far
    styles: Vec<(usize, MarkdownStyle)>,
    /// Start of the current (unflushed) run, in bytes of `input`
    start: usize,
}

impl<'a> Output<'a> {
    /// Create an empty accumulator for the given input line.
    fn new(input: &'a str) -> Self {
        Output {
            input,
            text: String::new(),
            styles: Vec::new(),
            start: 0,
        }
    }

    /// Record the run `[start..end)` with `style`, and advance past `end + skip` bytes of
    /// (dropped) syntax.
    fn emit(&mut self, style: MarkdownStyle, end: usize, skip: usize) {
        if end > self.start {
            let last = self.styles.last().map(|&(_, s)| s).unwrap_or_default();
            if last != style {
                self.styles.push((self.text.len(), style));
            }
            self.text.push_str(&self.input[self.start..end]);
        }
        self.start = end + skip;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lines: &[&str]) -> Vec<(String, Vec<(usize, MarkdownStyle)>)> {
        let mut parser = Parser::default();
        lines.iter().map(|line| parser.push(line)).collect()
    }

    fn register(
        text: &str,
        styles: &[(usize, MarkdownStyle)],
    ) -> (String, Vec<(usize, MarkdownStyle)>) {
        (text.to_string(), styles.to_vec())
    }

    fn effect(effect: Effect) -> MarkdownStyle {
        MarkdownStyle::default().with(effect)
    }

    #[test]
    fn plain_text_has_no_styling() {
        assert_eq!(parse(&["hello world"]), vec![register("hello world", &[])]);
    }

    #[test]
    fn single_asterisks_are_italic() {
        assert_eq!(
            parse(&["a *b* c"]),
            vec![register(
                "a b c",
                &[(2, effect(Effect::Italic)), (3, MarkdownStyle::default())]
            )]
        );
    }

    #[test]
    fn double_asterisks_are_bold() {
        assert_eq!(
            parse(&["a **b** c"]),
            vec![register(
                "a b c",
                &[(2, effect(Effect::Bold)), (3, MarkdownStyle::default())]
            )]
        );
    }

    #[test]
    fn backticks_are_code() {
        assert_eq!(
            parse(&["x `y` z"]),
            vec![register(
                "x y z",
                &[(2, effect(Effect::Code)), (3, MarkdownStyle::default())]
            )]
        );
    }

    #[test]
    fn code_fences_are_ignored() {
        // A run of three backticks is not code on/off/on: it is plain text
        assert_eq!(parse(&["x ``` y"]), vec![register("x ``` y", &[])]);
        assert_eq!(parse(&["x ```` y"]), vec![register("x ```` y", &[])]);
    }

    #[test]
    fn fenced_code_is_not_styled() {
        assert_eq!(
            parse(&["```rust", "fn main() {}", "```"]),
            vec![
                register("```rust", &[]),
                register("fn main() {}", &[]),
                register("```", &[]),
            ]
        );
    }

    #[test]
    fn text_after_a_syntax_event_is_kept() {
        // An unclosed `*` must not swallow the rest of the line; the region stays open, so the
        // rest is (tentatively) italic
        assert_eq!(
            parse(&["hello *world"]),
            vec![register("hello world", &[(6, effect(Effect::Italic))])]
        );
    }

    #[test]
    fn state_carries_over_lines() {
        assert_eq!(
            parse(&["a *b", "c* d"]),
            vec![
                register("a b", &[(2, effect(Effect::Italic))]),
                register(
                    "c d",
                    &[(0, effect(Effect::Italic)), (1, MarkdownStyle::default())]
                )
            ]
        );
    }
}
