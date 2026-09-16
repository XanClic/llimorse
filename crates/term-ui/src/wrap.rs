//! Row counting for the input `TextArea`.
//!
//! `ratatui_textarea::TextArea` does not expose how many on-screen rows its content occupies, so
//! counting them requires the very wrapping algorithm the widget renders with. The functions in
//! this module are a simplified copy of `wrap.rs` from `ratatui-textarea` v0.9.2 (MIT), dropping
//! the line-number gutter and tab-stop width logic, and the actual range handling (we only need a
//! number of lines as the result).

use ratatui_textarea::WrapMode;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// How many lines need to be rendered for the given `mode`, at least 1.
#[must_use]
pub(crate) fn wrapped_line_count(line: &str, mode: WrapMode, width: usize) -> usize {
    let width = width.max(1);
    match mode {
        WrapMode::None => 1,
        WrapMode::Glyph => {
            split_range_by_grapheme_width(line, 0, line.len(), width)
        }
        WrapMode::Word => wrap_word_chunks(line, width, false),
        WrapMode::WordOrGlyph => wrap_word_chunks(line, width, true),
    }.max(1)
}

/// Wraps the given line at word boundaries, falling back to grapheme splitting for words wider
/// than the width when enabled.
fn wrap_word_chunks(line: &str, width: usize, fallback_to_glyph: bool) -> usize {
    let mut line_count = 0;
    let mut seg_start = 0;
    let mut seg_end = 0;
    let mut seg_width = 0usize;

    for (start, text) in UnicodeSegmentation::split_word_bound_indices(line) {
        let end = start + text.len();

        if seg_end == seg_start {
            seg_start = start;
        }

        let chunk_width = display_width(text);
        if seg_width + chunk_width <= width {
            seg_end = end;
            seg_width += chunk_width;
            continue;
        }

        if seg_end > seg_start {
            line_count += 1;
            seg_start = start;
            seg_width = 0;

            if chunk_width <= width {
                seg_end = end;
                seg_width += chunk_width;
                continue;
            }
        }

        if fallback_to_glyph {
            line_count += split_range_by_grapheme_width(line, start, end, width);
        } else {
            line_count += 1;
        }

        seg_start = end;
        seg_end = end;
        seg_width = 0;
    }

    if seg_end > seg_start {
        line_count += 1;
    }

    line_count
}

/// Wraps the given byte range of a line at grapheme boundaries, accounting for wide characters.
fn split_range_by_grapheme_width(
    line: &str,
    start: usize,
    end: usize,
    width: usize,
) -> usize {
    let mut line_count = 0;

    let mut segment_start = start;
    while segment_start < end {
        let mut segment_end = segment_start;
        let mut segment_width = 0usize;

        for (offset, grapheme) in
            UnicodeSegmentation::grapheme_indices(&line[segment_start..end], true)
        {
            let grapheme_start = segment_start + offset;
            let grapheme_end = grapheme_start + grapheme.len();
            let grapheme_width = display_width(grapheme);

            if segment_end != segment_start && segment_width + grapheme_width > width {
                break;
            }

            segment_end = grapheme_end;
            segment_width += grapheme_width;
            if segment_width > width {
                break;
            }
        }

        if segment_end == segment_start {
            if let Some(ch) = line[segment_start..end].chars().next() {
                segment_end = segment_start + ch.len_utf8();
            } else {
                break;
            }
        }

        segment_start = segment_end;

        line_count += 1;
    }

    line_count
}

/// Total display width of the given text.
#[inline]
fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Segments of the given line after wrapping at the given width.
    fn segments(line: &str, mode: WrapMode, width: usize) -> Vec<&str> {
        line_ranges(line, mode, width)
            .into_iter()
            .map(|(s, e)| &line[s..e])
            .collect()
    }

    #[test]
    fn trailing_space_rolls_onto_its_own_row() {
        assert_eq!(segments("foo ", WrapMode::Word, 3), vec!["foo", " "]);
        assert_eq!(segments("foo ", WrapMode::WordOrGlyph, 3), vec!["foo", " "]);
        assert_eq!(segments("foo ", WrapMode::Glyph, 3), vec!["foo", " "]);
    }

    #[test]
    fn internal_space_runs_consume_width() {
        assert_eq!(segments("a  b", WrapMode::Word, 2), vec!["a", "  ", "b"]);
    }

    #[test]
    fn empty_line_is_one_row() {
        assert_eq!(segments("", WrapMode::Word, 10), vec![""]);
    }

    #[test]
    fn word_wrap_keeps_long_word() {
        assert_eq!(
            segments("helloworld", WrapMode::Word, 4),
            vec!["helloworld"]
        );
    }

    #[test]
    fn word_or_glyph_wrap_splits_long_word() {
        assert_eq!(
            segments("helloworld", WrapMode::WordOrGlyph, 4),
            vec!["hell", "owor", "ld"]
        );
    }

    #[test]
    fn glyph_wrap_handles_wide_chars() {
        assert_eq!(segments("ab犬猫", WrapMode::Glyph, 4), vec!["ab犬", "猫"]);
    }

    #[test]
    fn glyph_wrap_keeps_combining_grapheme_cluster() {
        assert_eq!(
            segments("e\u{301}x", WrapMode::Glyph, 1),
            vec!["e\u{301}", "x"]
        );
    }

    #[test]
    fn glyph_wrap_preserves_full_mixed_width_row_capacity() {
        assert_eq!(segments("a中bcde", WrapMode::Glyph, 4), vec!["a中b", "cde"]);
    }
}
