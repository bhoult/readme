//! Column arithmetic for terminal text (tabs, wide characters), search, and
//! turning a source line into styled spans.

use std::ops::Range;

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

pub const TAB_WIDTH: usize = 4;

/// Screen columns `c` takes when it starts at column `col`; a tab runs to the next tab stop.
pub fn char_cols(c: char, col: usize) -> usize {
    if c == '\t' {
        TAB_WIDTH - col % TAB_WIDTH
    } else if c.is_control() {
        1
    } else {
        c.width().unwrap_or(0)
    }
}

/// The screen column where char `idx` of `line` starts.
pub fn display_col(line: &str, idx: usize) -> usize {
    line.chars().take(idx).fold(0, |col, c| col + char_cols(c, col))
}

/// The char of `line` under screen column `target`, or the end of the line if it's shorter.
pub fn char_at_col(line: &str, target: usize) -> usize {
    let mut col = 0;
    for (i, c) in line.chars().enumerate() {
        let w = char_cols(c, col);
        if w > 0 && target < col + w {
            return i;
        }
        col += w;
    }
    line.chars().count()
}

pub fn str_width(s: &str) -> usize {
    display_col(s, usize::MAX)
}

/// Splits `s` after at most `width` columns.
pub fn split_at_width(s: &str, width: usize) -> (&str, &str) {
    let mut col = 0;
    for (i, c) in s.char_indices() {
        col += char_cols(c, col);
        if col > width {
            return s.split_at(i);
        }
    }
    (s, "")
}

/// The chars where each screen row of `line` starts when it's wrapped to `width`
/// columns, breaking after a space where there is one.
pub fn wrap_points(line: &str, width: usize) -> Vec<usize> {
    let width = width.max(1);
    let chars: Vec<char> = line.chars().collect();
    // Start column of each char, then the end of the line
    let mut cols = Vec::with_capacity(chars.len() + 1);
    let mut col = 0;
    for &c in &chars {
        cols.push(col);
        col += char_cols(c, col);
    }
    cols.push(col);
    let mut starts = vec![0];
    let mut row = 0;
    let mut after_space = None;
    for i in 0..chars.len() {
        // Spaces never start a row; they hang off the end of the one before
        if chars[i] == ' ' || chars[i] == '\t' {
            after_space = Some(i + 1);
            continue;
        }
        while cols[i + 1] - cols[row] > width && i > row {
            let at = after_space.filter(|&s| s > row && s <= i).unwrap_or(i);
            starts.push(at);
            row = at;
            after_space = None;
        }
    }
    starts
}

/// Which row (from `wrap_points`) char `col` falls in.
pub fn row_of(starts: &[usize], col: usize) -> usize {
    starts.partition_point(|&s| s <= col).saturating_sub(1)
}

/// A styled line scrolled `left` columns sideways, except for its first `keep`
/// columns, which stay put.
pub fn slice_line(line: &Line<'static>, keep: usize, left: usize) -> Line<'static> {
    if left == 0 {
        return line.clone();
    }
    let mut spans = Vec::new();
    let mut col = 0;
    for span in &line.spans {
        for c in span.content.chars() {
            let start = col;
            col += char_cols(c, col);
            if start < keep || start >= keep + left {
                push_span(&mut spans, c.encode_utf8(&mut [0; 4]), span.style);
            } else if col > keep + left {
                // A wide character cut by the edge
                push_span(&mut spans, " ", span.style);
            }
        }
    }
    Line::from(spans)
}

/// A char in lower case, as searches compare them: one char for one, so a folded text
/// lines up with the original (unlike str::to_lowercase, which makes İ two chars and
/// a final Σ a ς).
pub fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

pub fn fold_case(text: &str) -> String {
    text.chars().map(fold_char).collect()
}

/// Char ranges where `query` occurs in `line`, ignoring case unless the query has a capital.
pub fn find_all(line: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold = !query.chars().any(char::is_uppercase);
    let norm = |c: char| if fold { fold_char(c) } else { c };
    let hay: Vec<char> = line.chars().map(norm).collect();
    let needle: Vec<char> = query.chars().map(norm).collect();
    let mut hits = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == needle[..] {
            hits.push((i, i + needle.len()));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    hits
}

/// The next (or previous) match of `query` in `lines` from `from` (line, char), wrapping
/// around the end. A match starting exactly at `from` counts only when `inclusive`.
/// Returns (line, start char, end char).
pub fn search<S: AsRef<str>>(
    lines: &[S],
    query: &str,
    from: (usize, usize),
    forward: bool,
    inclusive: bool,
) -> Option<(usize, usize, usize)> {
    let n = lines.len();
    if n == 0 || query.is_empty() {
        return None;
    }
    let (line0, col0) = (from.0.min(n - 1), from.1);
    for step in 0..=n {
        let line = if forward { (line0 + step) % n } else { (line0 + n - step % n) % n };
        // The starting line is searched twice: past `from` first, the rest after wrapping
        let ok = |s: usize| match (step, forward) {
            (0, true) => s > col0 || (inclusive && s == col0),
            (0, false) => s < col0 || (inclusive && s == col0),
            (_, true) if step == n => s <= col0,
            (_, false) if step == n => s >= col0,
            _ => true,
        };
        let hits = find_all(lines[line].as_ref(), query);
        let hit = if forward { hits.into_iter().find(|h| ok(h.0)) } else { hits.into_iter().rev().find(|h| ok(h.0)) };
        if let Some((s, e)) = hit {
            return Some((line, s, e));
        }
    }
    None
}

/// Appends `text` to `spans`, merging it into the last span when the style matches.
pub fn push_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match spans.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => spans.push(Span::styled(text.to_string(), style)),
    }
}

/// The visible part of a source line, `width` columns from column `left`: syntax colors
/// from `colors` (byte ranges), tabs expanded, and `marks` (char ranges with an overlay
/// style) on top. A mark that ends at `usize::MAX` also covers one cell past the end of
/// the line, so a selection that runs on to the next line shows.
pub fn line_spans(
    line: &str,
    colors: &[(Style, Range<usize>)],
    left: usize,
    width: usize,
    marks: &[(usize, usize, Style)],
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut colors = colors.iter().peekable();
    let overlay = |idx: usize, base: Style| {
        marks.iter().filter(|(s, e, _)| idx >= *s && idx < *e).fold(base, |style, (_, _, mark)| style.patch(*mark))
    };
    let mut col = 0;
    let mut cut_off = false;
    for (idx, (byte, c)) in line.char_indices().enumerate() {
        if col >= left + width {
            cut_off = true;
            break;
        }
        while colors.peek().is_some_and(|(_, r)| r.end <= byte) {
            colors.next();
        }
        let base = colors.peek().filter(|(_, r)| r.start <= byte).map(|(s, _)| *s).unwrap_or_default();
        let style = overlay(idx, base);
        let (start, end) = (col, col + char_cols(c, col));
        col = end;
        if end <= left {
            continue;
        }
        let visible = end.min(left + width).saturating_sub(start.max(left));
        if c == '\t' || start < left || end > left + width {
            push_span(&mut spans, &" ".repeat(visible), style);
        } else if c.is_control() {
            push_span(&mut spans, "\u{fffd}", style);
        } else {
            push_span(&mut spans, c.encode_utf8(&mut [0; 4]), style);
        }
    }
    let len = line.chars().count();
    if !cut_off
        && col >= left
        && col < left + width
        && let Some((_, _, mark)) = marks.iter().find(|(s, e, _)| *e == usize::MAX && *s <= len)
    {
        push_span(&mut spans, " ", Style::default().patch(*mark));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_with_tabs_and_wide_chars() {
        assert_eq!(display_col("\tab", 1), 4);
        assert_eq!(display_col("a\tb", 2), 4);
        assert_eq!(display_col("日本", 1), 2);
        assert_eq!(char_at_col("\tab", 2), 0);
        assert_eq!(char_at_col("\tab", 4), 1);
        assert_eq!(char_at_col("日本x", 3), 1);
        assert_eq!(char_at_col("ab", 10), 2);
    }

    #[test]
    fn wraps_after_spaces() {
        assert_eq!(wrap_points("hello world foo", 11), vec![0, 12]);
        assert_eq!(wrap_points("hello world foo", 6), vec![0, 6, 12]);
        assert_eq!(wrap_points("abcdefghij", 4), vec![0, 4, 8]);
        assert_eq!(wrap_points("ab", 10), vec![0]);
        assert_eq!(wrap_points("", 10), vec![0]);
        assert_eq!(row_of(&[0, 6, 12], 5), 0);
        assert_eq!(row_of(&[0, 6, 12], 6), 1);
        assert_eq!(row_of(&[0, 6, 12], 20), 2);
    }

    #[test]
    fn slices_keep_the_gutter() {
        let line = Line::from("12 abcdef");
        let text = |l: Line| l.spans.iter().map(|s| s.content.to_string()).collect::<String>();
        assert_eq!(text(slice_line(&line, 3, 2)), "12 cdef");
    }

    #[test]
    fn smart_case_find() {
        assert_eq!(find_all("Foo foo FOO", "foo"), vec![(0, 3), (4, 7), (8, 11)]);
        assert_eq!(find_all("Foo foo FOO", "Foo"), vec![(0, 3)]);
    }

    #[test]
    fn search_wraps_both_ways() {
        let lines = ["alpha", "beta x", "x gamma"];
        assert_eq!(search(&lines, "x", (0, 0), true, true), Some((1, 5, 6)));
        assert_eq!(search(&lines, "x", (1, 5), true, false), Some((2, 0, 1)));
        assert_eq!(search(&lines, "x", (2, 0), true, false), Some((1, 5, 6)));
        assert_eq!(search(&lines, "x", (1, 5), false, false), Some((2, 0, 1)));
        assert_eq!(search(&lines, "nope", (0, 0), true, true), None);
    }

    #[test]
    fn spans_scroll_and_expand_tabs() {
        let text = |spans: Vec<Span>| spans.iter().map(|s| s.content.to_string()).collect::<String>();
        assert_eq!(text(line_spans("\tabc", &[], 0, 10, &[])), "    abc");
        assert_eq!(text(line_spans("\tabc", &[], 2, 10, &[])), "  abc");
        assert_eq!(text(line_spans("abcdef", &[], 2, 3, &[])), "cde");
    }
}
