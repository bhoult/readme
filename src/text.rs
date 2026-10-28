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

/// Whether the byte at `at` is a printable ASCII character with nothing joined onto it,
/// so a grapheme of its own a column wide, known without Unicode's rules: no rule joins
/// two ASCII characters but CR LF, and what does join onto one is never ASCII.
fn printable_alone(bytes: &[u8], at: usize) -> bool {
    (0x20..0x7f).contains(&bytes[at]) && bytes.get(at + 1).is_none_or(|&next| next < 0x80)
}

/// The graphemes of `s` with their byte offsets, as `grapheme_indices` gives them, but
/// several times faster through the printable ASCII most text is mostly made of.
/// Unicode's rules are asked only for the rest, from a grapheme's start, where they
/// give what they'd give reading from the start of `s`.
pub fn graphemes(s: &str) -> impl Iterator<Item = (usize, &str)> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        let start = at;
        if start >= s.len() {
            return None;
        }
        at += if printable_alone(s.as_bytes(), start) { 1 } else { s[start..].graphemes(true).next()?.len() };
        Some((start, &s[start..at]))
    })
}

/// Each char of `s` with its byte offset and the columns it takes, from column `col`
/// on. A grapheme's columns all go to its first char; what joins onto it takes none.
pub fn cells(s: &str, col: usize) -> impl Iterator<Item = (usize, char, usize)> + '_ {
    Cells { s, at: 0, col, rest: None }
}

struct Cells<'a> {
    s: &'a str,
    at: usize,
    col: usize,
    /// The chars after the first of the grapheme being gone through, and where it starts
    rest: Option<(usize, std::str::CharIndices<'a>)>,
}

impl Iterator for Cells<'_> {
    type Item = (usize, char, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((start, chars)) = &mut self.rest {
            if let Some((i, c)) = chars.next() {
                return Some((*start + i, c, 0));
            }
            self.rest = None;
        }
        let start = self.at;
        let &byte = self.s.as_bytes().get(start)?;
        if printable_alone(self.s.as_bytes(), start) {
            self.at += 1;
            self.col += 1;
            return Some((start, char::from(byte), 1));
        }
        let g = self.s[start..].graphemes(true).next()?;
        let width = grapheme_cols(g, self.col);
        self.at += g.len();
        self.col += width;
        let mut chars = g.char_indices();
        let (_, first) = chars.next()?;
        self.rest = Some((start, chars));
        Some((start, first, width))
    }
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
    // Printable ASCII is a column a byte
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return s.len();
    }
    cells(s, 0).map(|(_, _, w)| w).sum()
}

/// Whether `s` takes more than `cols` columns, found out without measuring all of a long line.
pub fn wider_than(s: &str, cols: usize) -> bool {
    wider_than_from(s, Place::default(), cols)
}

/// Whether `s` takes more than `cols` columns, measured on from `from`, a place in it.
pub fn wider_than_from(s: &str, from: Place, cols: usize) -> bool {
    let rest = &s[from.byte.min(s.len())..];
    // No char is wider than its bytes, but a tab, at most four columns for one
    if from.col + rest.len() <= cols && !rest.contains('\t') {
        return false;
    }
    let mut col = from.col;
    cells(rest, from.col).any(|(_, _, w)| {
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

/// Places along `line` about every `every` bytes, at the starts of graphemes, the first
/// at its start: something to measure a long line from, rather than from its start.
pub fn places(line: &str, every: usize) -> Vec<Place> {
    let mut found = vec![Place::default()];
    let (mut char, mut col, mut next) = (0, 0, every);
    for (byte, g) in graphemes(line) {
        if byte >= next {
            found.push(Place { char, byte, col });
            next = byte + every;
        }
        char += g.chars().count();
        col += grapheme_cols(g, col);
    }
    found
}

/// The last of `places` at or before screen column `col`.
pub fn place_before_col(places: &[Place], col: usize) -> Place {
    places.get(places.partition_point(|p| p.col <= col).saturating_sub(1)).copied().unwrap_or_default()
}

/// The first of `places` past screen column `col`, if there's one.
pub fn place_after_col(places: &[Place], col: usize) -> Option<Place> {
    places.get(places.partition_point(|p| p.col <= col)).copied()
}

/// The last of `places` at or before char `char`.
pub fn place_before_char(places: &[Place], char: usize) -> Place {
    places.get(places.partition_point(|p| p.char <= char).saturating_sub(1)).copied().unwrap_or_default()
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
/// columns, which stay put, and cut off after `width` columns in all: what's past them
/// wouldn't show, and a line can be megabytes long.
pub fn slice_line(line: &Line<'static>, keep: usize, left: usize, width: usize) -> Line<'static> {
    if left == 0 && !line_wider_than(line, width) {
        return line.clone();
    }
    let mut spans = Vec::new();
    let mut col = 0;
    'spans: for span in &line.spans {
        for (_, g) in graphemes(&span.content) {
            let start = col;
            col += grapheme_cols(g, col);
            if start >= width + left {
                break 'spans;
            }
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

/// Whether a styled line takes more than `cols` columns, found out without measuring
/// all of a long one.
pub fn line_wider_than(line: &Line, cols: usize) -> bool {
    let mut col = 0;
    for span in &line.spans {
        if wider_than(&span.content, cols - col) {
            return true;
        }
        col += str_width(&span.content);
    }
    false
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
/// Matches don't overlap: each is looked for after the one before.
pub fn find_all(line: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold = !query.chars().any(char::is_uppercase);
    let mut hits = Vec::new();
    // All ASCII, a char is a byte, and cases fold the same either way
    if line.is_ascii() && query.is_ascii() {
        let (hay, needle) = (line.as_bytes(), query.as_bytes());
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            let here = &hay[i..i + needle.len()];
            if if fold { here.eq_ignore_ascii_case(needle) } else { here == needle } {
                hits.push((i, i + needle.len()));
                i += needle.len();
            } else {
                i += 1;
            }
        }
        return hits;
    }
    let norm = |c: char| if fold { fold_char(c) } else { c };
    let needle: Vec<char> = query.chars().map(norm).collect();
    // The text from char `i` on, folded a char at a time as it's compared
    let (mut rest, mut i) = (line.chars(), 0);
    loop {
        let mut here = rest.clone().map(norm);
        if needle.iter().all(|&c| here.next() == Some(c)) {
            hits.push((i, i + needle.len()));
            rest.nth(needle.len() - 1);
            i += needle.len();
        } else if rest.next().is_some() {
            i += 1;
        } else {
            return hits;
        }
    }
}

/// Whether `query` occurs in `line`, as [`find_all`] would find it.
pub fn finds(line: &str, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let fold = !query.chars().any(char::is_uppercase);
    if line.is_ascii() && query.is_ascii() {
        let needle = query.as_bytes();
        return line
            .as_bytes()
            .windows(needle.len())
            .any(|here| if fold { here.eq_ignore_ascii_case(needle) } else { here == needle });
    }
    !find_all(line, query).is_empty()
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
    // The colors from `from` on: a long line can have hundreds of thousands
    let mut colors = colors[colors.partition_point(|(_, r)| r.end <= from.byte)..].iter().peekable();
    // The marks in the order they start, each with its place in `marks`; only those
    // reached and not yet over are looked at, as a long line can have thousands
    let mut order: Vec<(usize, &(usize, usize, Style))> = marks.iter().enumerate().collect();
    order.sort_by_key(|(_, m)| m.0);
    let (mut next, mut active) = (0, Vec::new());
    let (mut col, mut idx) = (from.col, from.char);
    let mut cut_off = false;
    for (offset, g) in graphemes(&line[from.byte.min(line.len())..]) {
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

    /// `cells` as it was, reading every grapheme by Unicode's rules.
    fn cells_by_the_rules(s: &str, mut col: usize) -> Vec<(usize, char, usize)> {
        let mut out = Vec::new();
        for (byte, g) in s.grapheme_indices(true) {
            let width = grapheme_cols(g, col);
            col += width;
            out.extend(g.char_indices().map(|(i, c)| (byte + i, c, if i == 0 { width } else { 0 })));
        }
        out
    }

    #[test]
    fn the_fast_walk_finds_the_graphemes_the_rules_do() {
        // ASCII with accents joined on, CR LF, flags in pairs, emoji joined by ZWJs, tabs
        // and control characters, wide characters, and runs of plain text between
        let pieces = [
            "a",
            "b",
            " ",
            ".",
            "\t",
            "\u{301}",
            "\r",
            "\n",
            "\u{1f1fa}",
            "\u{1f1f8}",
            "👍",
            "🏽",
            "\u{200d}",
            "日",
            "\u{fe0f}",
            "\u{1b}",
            "\u{7f}",
            "\u{915}",
            "\u{94d}",
            "é",
            "plain text",
        ];
        let mut seed = 7u64;
        let mut samples: Vec<String> = vec!["e\u{301}a cafe\u{301}!".into(), "👨\u{200d}👩\u{200d}👧 ok".into()];
        for _ in 0..500 {
            let mut s = String::new();
            for _ in 0..24 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                s.push_str(pieces[(seed >> 33) as usize % pieces.len()]);
            }
            samples.push(s);
        }
        for s in &samples {
            let fast: Vec<(usize, &str)> = graphemes(s).collect();
            assert_eq!(fast, s.grapheme_indices(true).collect::<Vec<_>>(), "{s:?}");
            let rules = cells_by_the_rules(s, 3);
            assert_eq!(cells(s, 3).collect::<Vec<_>>(), rules, "{s:?}");
            let width: usize = cells_by_the_rules(s, 0).iter().map(|c| c.2).sum();
            assert_eq!(str_width(s), width, "{s:?}");
            for cols in [0, 5, 20, width.saturating_sub(1), width] {
                assert_eq!(wider_than(s, cols), width > cols, "{s:?} {cols}");
            }
        }
    }

    #[test]
    fn measuring_from_a_place_along_a_line_is_measuring_from_its_start() {
        // A long line of all that's measured unusually: tabs, wide characters, emoji with
        // what joins onto them, accents written as their own marks
        let line: String =
            ["ab\tc", "日本", "👍🏽", "e\u{301}", " plain text"].iter().copied().cycle().take(3000).collect();
        let places = places(&line, 64);
        assert!(places.len() > 50, "{}", places.len());
        for p in &places {
            assert!(line.is_char_boundary(p.byte) && line[..p.byte].chars().count() == p.char, "{p:?}");
            assert_eq!(display_col(&line, p.char), p.col, "{p:?}");
        }
        let width = str_width(&line);
        for col in (0..width + 10).step_by(37) {
            let from = place_before_col(&places, col);
            assert!(from.col <= col && place_after_col(&places, col).is_none_or(|p| p.col > col));
            assert_eq!(char_at_col_from(&line, from, col), char_at_col(&line, col), "{col}");
            assert_eq!(wider_than_from(&line, from, col), wider_than(&line, col), "{col}");
        }
        for c in (0..line.chars().count()).step_by(41) {
            assert_eq!(display_col_from(&line, place_before_char(&places, c), c), display_col(&line, c), "{c}");
        }
        // Drawn from a place, the view is what it is drawn from the start
        let text = |spans: Vec<Span>| spans.iter().map(|s| s.content.to_string()).collect::<String>();
        for left in [0, 500, 4321, width - 5] {
            let from = place_before_col(&places, left);
            let drawn = text(line_spans(&line, &[], from, left, 30, &[]));
            assert_eq!(drawn, text(line_spans(&line, &[], Place::default(), left, 30, &[])), "{left}");
        }
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
        assert_eq!(text(slice_line(&line, 3, 2, 80)), "12 cdef");
        // Cut off past the width, which is counted with the columns scrolled past
        assert_eq!(text(slice_line(&line, 3, 2, 5)), "12 cd");
        assert_eq!(text(slice_line(&line, 3, 0, 5)), "12 ab");
        assert!(line_wider_than(&line, 8) && !line_wider_than(&line, 9));
    }

    /// `find_all` as it was: both sides folded into arrays of chars first.
    fn find_all_in_arrays(line: &str, query: &str) -> Vec<(usize, usize)> {
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

    #[test]
    fn finding_without_copying_finds_what_copying_did() {
        // Letters whose lower case is unusual (a dotted capital I, the Kelvin sign, sharp
        // s, final sigma), accents joined on, and text that repeats, so matches crowd
        let pieces = ["a", "A", "aa", "İ", "i", "\u{212a}", "k", "ß", "SS", "Σ", "ς", "σ", "e\u{301}", "é", "日", " "];
        let mut seed = 11u64;
        let mut next = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        for _ in 0..3000 {
            let line: String = (0..next(30)).map(|_| pieces[next(pieces.len())]).collect();
            let query: String = (0..1 + next(3)).map(|_| pieces[next(pieces.len())]).collect();
            assert_eq!(find_all(&line, &query), find_all_in_arrays(&line, &query), "{line:?} {query:?}");
            // And a query from the line itself
            let chars: Vec<char> = line.chars().collect();
            if !chars.is_empty() {
                let start = next(chars.len());
                let query: String = chars[start..(start + 1 + next(3)).min(chars.len())].iter().collect();
                assert_eq!(find_all(&line, &query), find_all_in_arrays(&line, &query), "{line:?} {query:?}");
            }
        }
        assert_eq!(find_all("aaaa", "aa"), vec![(0, 2), (2, 4)]);
        for (line, query) in
            [("src/App.rs", "app"), ("src/App.rs", "App"), ("src/app.rs", "App"), ("ünï", "Ü"), ("a", "")]
        {
            assert_eq!(finds(line, query), !query.is_empty() && !find_all(line, query).is_empty(), "{line} {query}");
        }
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
