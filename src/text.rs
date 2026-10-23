//! Column arithmetic for terminal text (tabs, wide characters), search, and
//! turning a source line into styled spans.

use std::{borrow::Cow, ops::Range};

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const TAB_WIDTH: usize = 4;

/// Screen columns `c` takes on its own when it starts at column `col`; a tab runs to
/// the next tab stop.
pub fn char_cols(c: char, col: usize) -> usize {
    if c == '\t' {
        TAB_WIDTH - col % TAB_WIDTH
    } else if c.is_control() {
        1
    } else {
        c.width().unwrap_or(0)
    }
}

/// Screen columns a grapheme takes from column `col`: a character with whatever joins
/// onto it (accents, an emoji's skin tone or presentation, a family joined by ZWJs),
/// measured whole, as ratatui lays it out.
fn grapheme_cols(g: &str, col: usize) -> usize {
    match g.chars().next() {
        Some(c) if c == '\t' || c.is_control() => char_cols(c, col),
        _ => g.width(),
    }
}

/// Each char of `s` with its byte offset and the columns it takes, from column `col`
/// on. A grapheme's columns all go to its first char; what joins onto it takes none.
pub fn cells(s: &str, mut col: usize) -> impl Iterator<Item = (usize, char, usize)> + '_ {
    s.grapheme_indices(true).flat_map(move |(byte, g)| {
        let width = grapheme_cols(g, col);
        col += width;
        g.char_indices().map(move |(i, c)| (byte + i, c, if i == 0 { width } else { 0 }))
    })
}

/// The screen column where char `idx` of `line` starts.
pub fn display_col(line: &str, idx: usize) -> usize {
    display_col_from(line, Place::default(), idx)
}

/// The screen column where char `idx` of `line` starts, counted from `from`, a place
/// at or before it.
pub fn display_col_from(line: &str, from: Place, idx: usize) -> usize {
    let rest = &line[from.byte.min(line.len())..];
    from.col + cells(rest, from.col).take(idx.saturating_sub(from.char)).map(|(_, _, w)| w).sum::<usize>()
}

/// The char of `line` under screen column `target`, or the end of the line if it's shorter.
pub fn char_at_col(line: &str, target: usize) -> usize {
    char_at_col_from(line, Place::default(), target)
}

/// The char of `line` under screen column `target`, looked for from `from`, a place at
/// or before it; the end of the line if it's shorter.
pub fn char_at_col_from(line: &str, from: Place, target: usize) -> usize {
    let (mut col, mut count) = (from.col, from.char);
    for (i, (_, _, w)) in cells(&line[from.byte.min(line.len())..], from.col).enumerate() {
        if w > 0 && target < col + w {
            return from.char + i;
        }
        col += w;
        count = from.char + i + 1;
    }
    count
}

pub fn str_width(s: &str) -> usize {
    cells(s, 0).map(|(_, _, w)| w).sum()
}

/// Whether `s` takes more than `cols` columns, found out without measuring all of a long line.
pub fn wider_than(s: &str, cols: usize) -> bool {
    let mut col = 0;
    cells(s, 0).any(|(_, _, w)| {
        col += w;
        col > cols
    })
}

/// The end of `s` that fits in `width` columns.
pub fn tail_width(s: &str, width: usize) -> &str {
    let (mut col, mut start) = (0, s.len());
    for (byte, g) in s.grapheme_indices(true).rev() {
        col += grapheme_cols(g, 0);
        if col > width {
            break;
        }
        start = byte;
    }
    &s[start..]
}

/// Splits `s` after at most `width` columns.
pub fn split_at_width(s: &str, width: usize) -> (&str, &str) {
    let mut col = 0;
    for (i, _, w) in cells(s, 0) {
        col += w;
        if col > width {
            return s.split_at(i);
        }
    }
    (s, "")
}

/// Text as it can be shown: a tab as spaces, and other control characters (which the
/// terminal would act on, and ratatui leaves out) as �.
pub fn printable(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|c| c.is_control() && c != '\n') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push_str(&" ".repeat(TAB_WIDTH)),
            '\n' => out.push(c),
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// A place in a line: a char, the byte it starts at, and the screen column it's in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Place {
    pub char: usize,
    pub byte: usize,
    pub col: usize,
}

/// Where each screen row of `line` starts when it's wrapped to `width` columns,
/// breaking after a space where there is one. Worked out in one pass, without a copy of
/// the line, since a line can be megabytes long.
pub fn wrap_points(line: &str, width: usize) -> Vec<Place> {
    let width = width.max(1);
    let mut starts = vec![Place::default()];
    let mut row = Place::default();
    let mut after_space: Option<Place> = None;
    let mut col = 0;
    for (i, (byte, c, w)) in cells(line, 0).enumerate() {
        let here = Place { char: i, byte, col };
        col += w;
        // Spaces never start a row; they hang off the end of the one before
        if c == ' ' || c == '\t' {
            after_space = Some(Place { char: i + 1, byte: byte + c.len_utf8(), col });
            continue;
        }
        // Nor does what joins onto a character (an emoji's skin tone, an accent): a
        // character too wide for the row goes on it whole, and the next starts the next
        if w == 0 {
            continue;
        }
        while col - row.col > width && i > row.char {
            let at = after_space.filter(|s| s.char > row.char && s.char <= i).unwrap_or(here);
            starts.push(at);
            row = at;
            after_space = None;
        }
    }
    starts
}

/// Which row (from `wrap_points`) char `col` falls in.
pub fn row_of(starts: &[Place], col: usize) -> usize {
    starts.partition_point(|s| s.char <= col).saturating_sub(1)
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
        for g in span.content.graphemes(true) {
            let start = col;
            col += grapheme_cols(g, col);
            if start < keep || start >= keep + left {
                push_span(&mut spans, g, span.style);
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
/// style, later ones on top) over them. The line is read from `from` on, a place at or
/// before the view, so a wrapped row doesn't go through the whole line before it. A mark
/// that ends at `usize::MAX` also covers one cell past the end of the line, so a
/// selection that runs on to the next line shows.
pub fn line_spans(
    line: &str,
    colors: &[(Style, Range<usize>)],
    from: Place,
    left: usize,
    width: usize,
    marks: &[(usize, usize, Style)],
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut colors = colors.iter().peekable();
    // The marks in the order they start, each with its place in `marks`; only those
    // reached and not yet over are looked at, as a long line can have thousands
    let mut order: Vec<(usize, &(usize, usize, Style))> = marks.iter().enumerate().collect();
    order.sort_by_key(|(_, m)| m.0);
    let (mut next, mut active) = (0, Vec::new());
    let (mut col, mut idx) = (from.col, from.char);
    let mut cut_off = false;
    for (offset, g) in line[from.byte.min(line.len())..].grapheme_indices(true) {
        if col >= left + width {
            cut_off = true;
            break;
        }
        let (byte, at) = (from.byte + offset, idx);
        idx += g.chars().count();
        let (start, end) = (col, col + grapheme_cols(g, col));
        col = end;
        if end <= left {
            continue;
        }
        while colors.peek().is_some_and(|(_, r)| r.end <= byte) {
            colors.next();
        }
        let base = colors.peek().filter(|(_, r)| r.start <= byte).map(|(s, _)| *s).unwrap_or_default();
        while next < order.len() && order[next].1.0 <= at {
            active.push(order[next]);
            next += 1;
        }
        active.retain(|(_, m)| m.1 > at);
        active.sort_by_key(|(n, _)| *n);
        let style = active.iter().fold(base, |style, (_, (_, _, mark))| style.patch(*mark));
        let visible = end.min(left + width).saturating_sub(start.max(left));
        let first = g.chars().next().unwrap_or(' ');
        if first == '\t' || start < left || end > left + width {
            push_span(&mut spans, &" ".repeat(visible), style);
        } else if first.is_control() {
            push_span(&mut spans, "\u{fffd}", style);
        } else {
            push_span(&mut spans, g, style);
        }
    }
    if !cut_off
        && col >= left
        && col < left + width
        && let Some((_, _, mark)) = marks.iter().find(|(s, e, _)| *e == usize::MAX && *s <= idx)
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
        let chars = |line: &str, width| wrap_points(line, width).iter().map(|p| p.char).collect::<Vec<_>>();
        assert_eq!(chars("hello world foo", 11), vec![0, 12]);
        assert_eq!(chars("hello world foo", 6), vec![0, 6, 12]);
        assert_eq!(chars("abcdefghij", 4), vec![0, 4, 8]);
        assert_eq!(chars("ab", 10), vec![0]);
        assert_eq!(chars("", 10), vec![0]);
        // Each row start knows its byte and column, for drawing from there
        assert_eq!(wrap_points("é é é", 2)[1], Place { char: 2, byte: 3, col: 2 });
        // Breaking after the tab, which counts as a space
        assert_eq!(wrap_points("\tab", 5)[1], Place { char: 1, byte: 1, col: 4 });
        // An emoji wider than the row stays whole, with its skin tone
        assert_eq!(chars("👍🏽x", 1), vec![0, 2]);
        let starts = wrap_points("hello world foo", 6);
        assert_eq!(row_of(&starts, 5), 0);
        assert_eq!(row_of(&starts, 6), 1);
        assert_eq!(row_of(&starts, 20), 2);
    }

    #[test]
    fn emoji_take_the_width_ratatui_gives_them() {
        // A warning sign made emoji, a family joined by ZWJs, a thumb with a skin tone,
        // and an accent written as its own mark
        for (text, cols) in [("\u{26a0}\u{fe0f}", 2), ("👨\u{200d}👩\u{200d}👧", 2), ("👍🏽", 2), ("e\u{301}", 1)]
        {
            assert_eq!(str_width(text), cols, "{text:?}");
            assert_eq!(str_width(text), Line::from(text).width(), "{text:?}");
        }
        let line = "\u{26a0}\u{fe0f} Note";
        assert_eq!(display_col(line, 2), 2);
        assert_eq!(char_at_col(line, 1), 0);
        assert_eq!(char_at_col(line, 2), 2);
        assert_eq!(split_at_width(line, 1).0, "");
        assert_eq!(split_at_width(line, 2).0, "\u{26a0}\u{fe0f}");
        assert!(wider_than(line, 6) && !wider_than(line, 7));
    }

    #[test]
    fn printable_text() {
        assert_eq!(printable("a\tb"), "a    b");
        assert_eq!(printable("x\u{1b}[31my\n"), "x\u{fffd}[31my\n");
        assert!(matches!(printable("plain"), Cow::Borrowed(_)));
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
        let start = Place::default();
        assert_eq!(text(line_spans("\tabc", &[], start, 0, 10, &[])), "    abc");
        assert_eq!(text(line_spans("\tabc", &[], start, 2, 10, &[])), "  abc");
        assert_eq!(text(line_spans("abcdef", &[], start, 2, 3, &[])), "cde");
        // From a row's start in the middle of the line
        let row = wrap_points("abc\tdef", 5)[1];
        assert_eq!(text(line_spans("abc\tdef", &[], row, row.col, 5, &[])), "def");
    }

    #[test]
    fn marks_go_on_top_in_order_and_only_where_they_are() {
        use ratatui::style::Color;
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().bg(Color::Blue);
        // Thousands of matches on a long line, and a selection over some of them
        let line = "ab".repeat(5000);
        let mut marks: Vec<(usize, usize, Style)> = (0..5000).map(|i| (2 * i, 2 * i + 1, red)).collect();
        marks.push((9990, usize::MAX, blue));
        let spans = line_spans(&line, &[], Place::default(), 9988, 20, &marks);
        let styles: Vec<(String, Style)> = spans.iter().map(|s| (s.content.to_string(), s.style)).collect();
        assert_eq!(styles[0], ("a".into(), red));
        assert_eq!(styles[1], ("b".into(), Style::default()));
        assert_eq!(styles[2], ("a".into(), red.patch(blue)));
        assert_eq!(styles[3], ("b".into(), blue));
        assert_eq!(styles[4], ("a".into(), red.patch(blue)));
        // The selection runs on past the end of the line
        let last = spans.last().unwrap();
        assert!(last.content.ends_with(' ') && last.style == blue, "{last:?}");
    }
}
