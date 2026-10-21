//! Markdown rendered for reading: styled, wrapped to the pane, and each line tagged
//! with the source line it came from, so switching to the raw text keeps the place.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::Line,
};

use crate::{
    highlight,
    text::{find_all, push_span, split_at_width, str_width},
};

pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// The source line each rendered line came from
    pub src: Vec<usize>,
    /// Lines at the top that stay on screen while the rest scrolls (a table header)
    pub pinned: usize,
    /// Columns on the left that stay put while the rest scrolls sideways
    pub gutter: usize,
    /// The widest line, when lines can be wider than the screen
    pub width: usize,
}

impl Rendered {
    /// The first rendered line from source line `line` on.
    pub fn line_for_src(&self, line: usize) -> usize {
        self.src.iter().position(|&s| s >= line).unwrap_or(self.src.len().saturating_sub(1))
    }

    pub fn text(&self, i: usize) -> String {
        self.lines[i].spans.iter().map(|s| s.content.as_ref()).collect()
    }
}

type Frag = (String, Style);

/// Table cells wider than this are cut short with "…"
const MAX_CELL: usize = 60;

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn heading_style(level: HeadingLevel) -> Style {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 => bold.fg(Color::Magenta),
        HeadingLevel::H2 => bold.fg(Color::Cyan),
        HeadingLevel::H3 => bold.fg(Color::Green),
        _ => bold.fg(Color::Yellow),
    }
}

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Frag>>>,
    row: Vec<Vec<Frag>>,
}

struct Renderer {
    width: usize,
    line_starts: Vec<usize>,
    out: Rendered,
    blank: Vec<bool>,
    /// Text of the paragraph, heading, list item or table cell being built
    inline: Vec<Frag>,
    styles: Vec<Style>,
    /// Enclosing lists; Some(n) for ordered ones, n being the next number
    lists: Vec<Option<u64>>,
    /// Widths of the markers of the list items we're in
    items: Vec<usize>,
    /// Bullet waiting for the first line of its item
    marker: Option<String>,
    quote: usize,
    code: Option<(String, String)>,
    table: Option<Table>,
    /// Where each open link's text starts in `inline`, and its target
    links: Vec<(usize, String)>,
    /// Source line of the block being rendered
    src: usize,
    /// Wrap long code lines; otherwise they run on, to scroll to sideways
    wrap_code: bool,
}

/// Markdown laid out for `width` columns. Text wraps to fit; tables keep their widths
/// and code its lines (unless `wrap_code`), and the view scrolls sideways to them.
pub fn render(src: &str, width: u16, wrap_code: bool) -> Rendered {
    let mut line_starts = vec![0];
    line_starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
    let mut r = Renderer {
        width: (width as usize).max(20),
        line_starts,
        out: Rendered { lines: Vec::new(), src: Vec::new(), pinned: 0, gutter: 0, width: 0 },
        blank: Vec::new(),
        inline: Vec::new(),
        styles: Vec::new(),
        lists: Vec::new(),
        items: Vec::new(),
        marker: None,
        quote: 0,
        code: None,
        table: None,
        links: Vec::new(),
        src: 0,
        wrap_code,
    };
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    for (event, range) in Parser::new_ext(src, options).into_offset_iter() {
        r.event(event, range.start);
    }
    r.flush();
    r.out.width = r.out.lines.iter().map(Line::width).max().unwrap_or(0);
    r.out
}

impl Renderer {
    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    fn indent(&self) -> usize {
        self.items.iter().sum()
    }

    /// Room for text after the quote bars and list indentation.
    fn room(&self, extra: usize) -> usize {
        self.width.saturating_sub(self.indent() + self.quote * 2 + extra).max(10)
    }

    fn prefix(&self) -> Vec<Frag> {
        let mut frags: Vec<Frag> = (0..self.quote).map(|_| ("▌ ".into(), Style::new().fg(Color::DarkGray))).collect();
        frags.push((" ".repeat(self.indent()), Style::new()));
        frags
    }

    fn emit(&mut self, frags: Vec<Frag>, blank: bool) {
        let mut spans = Vec::new();
        for (text, style) in frags {
            if !text.is_empty() {
                push_span(&mut spans, &text, style);
            }
        }
        self.out.lines.push(Line::from(spans));
        self.out.src.push(self.src);
        self.blank.push(blank);
    }

    /// A blank line between blocks.
    fn gap(&mut self) {
        if self.blank.last() == Some(&false) {
            let mut frags = self.prefix();
            frags.truncate(self.quote);
            self.emit(frags, true);
        }
    }

    fn push(&mut self, text: &str, style: Style) {
        let style = self.styles.last().copied().unwrap_or_default().patch(style);
        self.inline.push((text.to_string(), style));
    }

    fn push_style(&mut self, style: Style) {
        let base = self.styles.last().copied().unwrap_or_default();
        self.styles.push(base.patch(style));
    }

    fn event(&mut self, event: Event, offset: usize) {
        match event {
            Event::Start(tag) => self.start(tag, offset),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) if self.code.is_some() => {
                if let Some((_, code)) = &mut self.code {
                    code.push_str(&text);
                }
            }
            Event::Text(text) => self.push(&text, Style::new()),
            Event::Code(text) => self.push(&text, Style::new().fg(Color::Yellow)),
            Event::Html(text) | Event::InlineHtml(text) => self.push(&text, dim()),
            Event::SoftBreak => self.push(" ", Style::new()),
            Event::HardBreak => self.push("\n", Style::new()),
            Event::Rule => {
                self.flush();
                self.gap();
                self.src = self.line_of(offset);
                let mut frags = self.prefix();
                frags.push(("─".repeat(self.room(0)), dim()));
                self.emit(frags, false);
            }
            Event::TaskListMarker(done) => {
                let width = self.items.last().copied().unwrap_or(2);
                let mut mark = if done { "☑" } else { "☐" }.to_string();
                while str_width(&mark) < width {
                    mark.push(' ');
                }
                self.marker = Some(mark);
            }
            Event::FootnoteReference(name) => self.push(&format!("[^{name}]"), dim()),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag, offset: usize) {
        let line = self.line_of(offset);
        match tag {
            Tag::Paragraph | Tag::Heading { .. } => {
                self.flush();
                self.gap();
                self.src = line;
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.gap();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.gap();
                // Fenced code starts on the line after the fence
                self.src = if matches!(kind, CodeBlockKind::Fenced(_)) { line + 1 } else { line };
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split([' ', ',', '{']).next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush();
                self.gap();
                self.src = line;
                self.code = Some(("html".into(), String::new()));
            }
            Tag::MetadataBlock(_) => {
                self.flush();
                self.src = line + 1;
                self.code = Some(("yaml".into(), String::new()));
            }
            Tag::List(start) => {
                self.flush();
                if self.items.is_empty() {
                    self.gap();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                self.src = line;
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => ["• ", "◦ ", "▪ "][depth % 3].to_string(),
                };
                self.items.push(str_width(&marker));
                self.marker = Some(marker);
            }
            Tag::Table(aligns) => {
                self.flush();
                self.gap();
                self.src = line;
                self.table = Some(Table { aligns, rows: Vec::new(), row: Vec::new() });
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.push_style(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push((self.inline.len(), dest_url.to_string()));
                self.push_style(Style::new().fg(Color::Blue).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { .. } => {
                self.push("[image: ", dim());
                self.push_style(dim());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
            }
            TagEnd::Heading(level) => {
                let style = heading_style(level);
                for frag in &mut self.inline {
                    frag.1 = style.patch(frag.1);
                }
                let width = self.flush();
                let rule = match level {
                    HeadingLevel::H1 => Some("━"),
                    HeadingLevel::H2 => Some("─"),
                    _ => None,
                };
                if let Some(rule) = rule {
                    let mut frags = self.prefix();
                    frags.push((rule.repeat(width), style.remove_modifier(Modifier::BOLD)));
                    self.emit(frags, false);
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock | TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) => {
                if let Some((lang, text)) = self.code.take() {
                    self.code_block(&lang, &text);
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                // An empty item still shows its bullet
                if self.marker.is_some() && self.inline.is_empty() {
                    self.inline.push((String::new(), Style::new()));
                }
                self.flush();
                self.items.pop();
                self.marker = None;
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.inline);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table(table);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((start, url)) = self.links.pop() {
                    let text: String =
                        self.inline[start.min(self.inline.len())..].iter().map(|f| f.0.as_str()).collect();
                    if !url.is_empty() && !url.starts_with('#') && text != url {
                        self.push(&format!(" ({url})"), dim());
                    }
                }
            }
            TagEnd::Image => {
                self.styles.pop();
                self.push("]", dim());
            }
            _ => {}
        }
    }

    /// Writes out the text built so far, wrapped. Returns the width of its widest line.
    fn flush(&mut self) -> usize {
        if self.inline.is_empty() {
            return 0;
        }
        let frags = std::mem::take(&mut self.inline);
        let marker = self.marker.take();
        let mut widest = 0;
        for (i, line) in wrap(&frags, self.room(0)).into_iter().enumerate() {
            let mut out = self.prefix();
            if let (0, Some(marker)) = (i, &marker) {
                let indent = out.pop().map_or(0, |(pad, _)| pad.len());
                out.push((" ".repeat(indent.saturating_sub(str_width(marker))), Style::new()));
                out.push((marker.clone(), Style::new().fg(Color::DarkGray)));
            }
            let width: usize = line.iter().map(|(t, _)| str_width(t)).sum();
            widest = widest.max(width);
            let blank = width == 0 && marker.is_none();
            out.extend(line);
            self.emit(out, blank);
        }
        widest
    }

    fn code_block(&mut self, lang: &str, code: &str) {
        let colors = highlight::block(lang, code);
        let bar = ("│ ".to_string(), dim());
        let room = self.room(2);
        let first = self.src;
        for (i, line) in code.lines().enumerate() {
            let frags: Vec<Frag> = match &colors {
                Some(colors) => colors[i].iter().map(|(s, t)| (t.replace('\t', "    "), *s)).collect(),
                None => vec![(line.replace('\t', "    "), Style::new().fg(Color::Yellow))],
            };
            self.src = first + i;
            let pieces = if self.wrap_code { hard_wrap(&frags, room) } else { vec![frags] };
            for piece in pieces {
                let mut out = self.prefix();
                out.push(bar.clone());
                out.extend(piece);
                self.emit(out, false);
            }
        }
        self.src = first;
    }

    fn table(&mut self, table: Table) {
        let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let cell_width = |cell: &Vec<Frag>| cell.iter().map(|(t, _)| str_width(t)).sum::<usize>();
        let mut widths = vec![1; cols];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell_width(cell));
            }
        }
        // A wide table runs off to the side rather than squeezing its cells; only
        // very long cells are cut short
        for width in &mut widths {
            *width = (*width).min(MAX_CELL);
        }
        for (r, row) in table.rows.iter().enumerate() {
            let mut out = self.prefix();
            for (c, &width) in widths.iter().enumerate() {
                if c > 0 {
                    out.push((" │ ".into(), dim()));
                }
                let mut cell = row.get(c).cloned().unwrap_or_default();
                if r == 0 {
                    for frag in &mut cell {
                        frag.1 = frag.1.add_modifier(Modifier::BOLD);
                    }
                }
                out.extend(fit(&cell, width, table.aligns.get(c).copied().unwrap_or(Alignment::None)));
            }
            self.emit(out, false);
            if r == 0 {
                let mut out = self.prefix();
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                out.push((rule.join("─┼─"), dim()));
                self.emit(out, false);
            }
        }
    }
}

/// Splits text into words, runs of spaces, and line breaks.
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let class = |c: char| {
        if c == '\n' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut prev = None;
    for (i, c) in text.char_indices() {
        let cl = class(c);
        if prev.is_some_and(|p| p != cl || cl == 0) {
            out.push(&text[start..i]);
            start = i;
        }
        prev = Some(cl);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn push_frag(line: &mut Vec<Frag>, text: &str, style: Style) {
    match line.last_mut() {
        Some((last, s)) if *s == style => last.push_str(text),
        _ => line.push((text.to_string(), style)),
    }
}

/// Greedy word wrap; "\n" forces a break. Always returns at least one line.
fn wrap(frags: &[Frag], width: usize) -> Vec<Vec<Frag>> {
    let mut lines: Vec<Vec<Frag>> = vec![Vec::new()];
    let mut col = 0;
    for (text, style) in frags {
        for token in tokens(text) {
            if token == "\n" {
                lines.push(Vec::new());
                col = 0;
                continue;
            }
            let w = str_width(token);
            if token.chars().all(char::is_whitespace) {
                if col > 0 && col + w <= width {
                    push_frag(lines.last_mut().unwrap(), token, *style);
                    col += w;
                }
                continue;
            }
            if col > 0 && col + w > width {
                if let Some((last, _)) = lines.last_mut().unwrap().last_mut() {
                    last.truncate(last.trim_end().len());
                }
                lines.push(Vec::new());
                col = 0;
            }
            // A word wider than the whole line gets split
            let mut rest = token;
            while col + str_width(rest) > width {
                let (mut head, mut tail) = split_at_width(rest, width - col);
                if head.is_empty() {
                    let first = rest.chars().next().map_or(0, char::len_utf8);
                    (head, tail) = rest.split_at(first);
                }
                push_frag(lines.last_mut().unwrap(), head, *style);
                lines.push(Vec::new());
                col = 0;
                rest = tail;
            }
            if !rest.is_empty() {
                push_frag(lines.last_mut().unwrap(), rest, *style);
                col += str_width(rest);
            }
        }
    }
    lines
}

/// Breaks code at exactly `width` columns.
fn hard_wrap(frags: &[Frag], width: usize) -> Vec<Vec<Frag>> {
    let mut lines: Vec<Vec<Frag>> = vec![Vec::new()];
    let mut col = 0;
    for (text, style) in frags {
        let mut rest = text.as_str();
        while !rest.is_empty() {
            if col >= width {
                lines.push(Vec::new());
                col = 0;
            }
            let (mut head, mut tail) = split_at_width(rest, width - col);
            if head.is_empty() && col == 0 {
                let first = rest.chars().next().map_or(0, char::len_utf8);
                (head, tail) = rest.split_at(first);
            }
            if head.is_empty() {
                col = width;
                continue;
            }
            push_frag(lines.last_mut().unwrap(), head, *style);
            col += str_width(head);
            rest = tail;
        }
    }
    lines
}

/// One table cell, cut or padded to `width`.
fn fit(cell: &[Frag], width: usize, align: Alignment) -> Vec<Frag> {
    let mut out = Vec::new();
    let mut used = 0;
    let total: usize = cell.iter().map(|(t, _)| str_width(t)).sum();
    let room = if total > width { width.saturating_sub(1) } else { width };
    for (text, style) in cell {
        let text = text.replace('\n', " ");
        let (head, _) = split_at_width(&text, room - used);
        used += str_width(head);
        out.push((head.to_string(), *style));
        if used >= room {
            break;
        }
    }
    if total > width {
        out.push(("…".into(), dim()));
        used += 1;
    }
    let pad = width.saturating_sub(used);
    let (before, after) = match align {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        _ => (0, pad),
    };
    out.insert(0, (" ".repeat(before), Style::new()));
    out.push((" ".repeat(after), Style::new()));
    out
}

/// Highlights matches of `query` in a rendered line.
pub fn mark_matches(line: &Line<'static>, query: &str, style: Style) -> Line<'static> {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let hits = find_all(&text, query);
    if hits.is_empty() {
        return line.clone();
    }
    let mut spans = Vec::new();
    let mut idx = 0;
    for span in &line.spans {
        for c in span.content.chars() {
            let hit = hits.iter().any(|&(s, e)| idx >= s && idx < e);
            push_span(&mut spans, c.encode_utf8(&mut [0; 4]), if hit { span.style.patch(style) } else { span.style });
            idx += 1;
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(md: &str, width: u16) -> Vec<String> {
        let r = render(md, width, true);
        (0..r.lines.len()).map(|i| r.text(i)).collect()
    }

    #[test]
    fn headings_paragraphs_and_lists() {
        let out = plain("# Title\n\nSome *text* here.\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n", 40);
        assert_eq!(
            out,
            vec![
                "Title",
                "━━━━━",
                "",
                "Some text here.",
                "",
                "• one",
                "• two",
                "  ◦ nested",
                "",
                "1. first",
                "2. second",
            ]
        );
    }

    #[test]
    fn wraps_and_keeps_list_indent() {
        let out = plain("- a long item that wraps around\n", 20);
        assert_eq!(out, vec!["• a long item that", "  wraps around"]);
    }

    #[test]
    fn code_blocks_tables_and_quotes() {
        let out = plain("```rust\nfn main() {}\n```\n\n| a | bb |\n|---|---:|\n| 1 | 2 |\n\n> quoted\n", 40);
        assert_eq!(out, vec!["│ fn main() {}", "", "a │ bb", "──┼───", "1 │  2", "", "▌ quoted"]);
    }

    #[test]
    fn source_lines_are_tracked() {
        let r = render("# A\n\npara\n\n```\nx\ny\n```\n", 40, true);
        let y = (0..r.lines.len()).find(|&i| r.text(i).contains('y')).unwrap();
        assert_eq!(r.src[y], 6);
        assert_eq!(r.line_for_src(2), 3);
    }

    #[test]
    fn wide_tables_and_code_run_off_to_the_side() {
        let md = "| a long heading | another long heading |\n|---|---|\n| 1 | 2 |\n\n```\nfn main() { let x = 1; let y = 2; }\n```\n";
        let r = render(md, 20, false);
        let lines: Vec<String> = (0..r.lines.len()).map(|i| r.text(i)).collect();
        assert!(lines[0].contains("a long heading │ another long heading"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("fn main() { let x = 1; let y = 2; }")), "{lines:?}");
        assert!(r.width > 20);
        // With word wrap on, code still wraps
        let wrapped = render(md, 20, true);
        assert!(!(0..wrapped.lines.len()).any(|i| wrapped.text(i).contains("fn main() { let x = 1; let y = 2; }")));
    }

    #[test]
    fn links_show_their_target() {
        assert_eq!(
            plain("[docs](https://x.dev) and <https://y.dev>\n", 60),
            vec!["docs (https://x.dev) and https://y.dev"]
        );
    }
}
