//! CSV and TSV files shown as a table: aligned columns, the first row as a header
//! that stays on screen, numbers right-aligned, and row numbers down the side.

use ratatui::{
    style::{Modifier, Style},
    text::Line,
};

use crate::{
    markdown::Rendered,
    text::{printable, push_span, split_at_width, str_width},
};

/// Cells wider than this are cut short with "…"
const MAX_CELL: usize = 40;
/// Only this many rows are laid out as a table; the raw text has the rest
const MAX_ROWS: usize = 100_000;
/// Roughly the most memory the laid-out table may take. Every cell is padded to its
/// column's width and kept as text of its own, so a file of many short or empty cells
/// takes a hundred times its size or more: a wide, sparse export could take gigabytes.
const MAX_TABLE_BYTES: usize = 256 << 20;

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// The delimiter for a file, from its extension; None if it isn't a table.
pub fn delimiter(ext: &str) -> Option<u8> {
    match ext {
        "csv" => Some(b','),
        "tsv" | "tab" => Some(b'\t'),
        _ => None,
    }
}

pub fn render(src: &str, delimiter: u8) -> Rendered {
    let mut reader =
        csv::ReaderBuilder::new().delimiter(delimiter).has_headers(false).flexible(true).from_reader(src.as_bytes());
    // Each record with the (0-based) source line it starts on
    let mut rows: Vec<(usize, Vec<String>)> = reader
        .records()
        .filter_map(Result::ok)
        .take(MAX_ROWS + 1)
        .map(|record| {
            let line = record.position().map_or(0, |p| p.line().saturating_sub(1) as usize);
            let cells = record
                .iter()
                .map(|c| printable(&c.replace("\r\n", "↵").replace(['\n', '\r'], "↵").replace('\t', " ")).into_owned())
                .collect();
            (line, cells)
        })
        .collect();
    let cut = rows.len() > MAX_ROWS;
    rows.truncate(MAX_ROWS);
    if rows.is_empty() {
        return Rendered::note(Line::styled("(empty)", dim()));
    }

    let cols = rows.iter().map(|(_, cells)| cells.len()).max().unwrap_or(0);
    let mut widths = vec![1; cols];
    for (_, cells) in &rows {
        for (i, cell) in cells.iter().enumerate() {
            widths[i] = widths[i].max(str_width(cell).min(MAX_CELL));
        }
    }
    // A column whose values are all numbers lines up on the right
    let numeric: Vec<bool> = (0..cols)
        .map(|i| {
            let mut values =
                rows[1..].iter().filter_map(|(_, cells)| cells.get(i)).filter(|c| !c.trim().is_empty()).peekable();
            values.peek().is_some() && values.all(|c| c.trim().replace(',', "").parse::<f64>().is_ok())
        })
        .collect();

    let digits = (rows.len() - 1).max(1).to_string().len();
    let gutter = digits + 1;
    let width = gutter + widths.iter().sum::<usize>() + 3 * (cols - 1);
    // Each cell and the bar after it are a span apiece, each with text of its own on the heap
    let row_bytes = width + cols * 2 * (size_of::<ratatui::text::Span>() + 32);
    if rows.len().saturating_mul(row_bytes) > MAX_TABLE_BYTES {
        let size = format!("{} rows by {cols} columns", rows.len());
        let note = format!("(too big to lay out as a table, {size}; m shows the text)");
        return Rendered::note(Line::styled(note, dim()));
    }
    let rule = rows.len() > 1;
    let mut table = Rows { widths, numeric, digits, rule, cut, ..Rows::default() };
    let mut src = Vec::with_capacity(rows.len() + 2);
    for (r, (line, cells)) in rows.iter().enumerate() {
        for cell in cells {
            table.cells.push_str(cell);
            table.ends.push(table.cells.len());
        }
        table.starts.push(table.ends.len());
        src.push(*line);
        // The rule under the header goes with it
        if r == 0 && rule {
            src.push(*line);
        }
    }
    if cut {
        src.push(rows.last().map_or(0, |r| r.0));
    }
    let pinned = if rule { 2 } else { 0 };
    Rendered::table(table, src, pinned, gutter, width)
}

/// A table's rows, kept as their cells and laid out only as they're shown: laid out all
/// at once, every cell is text of its own padded to its column's width, which for a big
/// file takes dozens of times its size.
#[derive(Default)]
pub struct Rows {
    /// Every row's cells, one after another, where each cell ends, and where in `ends`
    /// each row's cells end
    cells: String,
    ends: Vec<usize>,
    starts: Vec<usize>,
    widths: Vec<usize>,
    /// Which columns are numbers, lined up on the right
    numeric: Vec<bool>,
    /// The width of the row numbers
    digits: usize,
    /// A rule under the header, and a note at the end that rows were left out
    rule: bool,
    cut: bool,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.starts.len() + usize::from(self.rule) + usize::from(self.cut)
    }

    /// Cell `c` of row `r`, or nothing for a row with fewer cells.
    fn cell(&self, r: usize, c: usize) -> &str {
        let first = r.checked_sub(1).map_or(0, |before| self.starts[before]);
        if first + c >= self.starts[r] {
            return "";
        }
        let start = (first + c).checked_sub(1).map_or(0, |before| self.ends[before]);
        &self.cells[start..self.ends[first + c]]
    }

    /// Line `i`: the header row, the rule under it, the other rows, and the note.
    pub fn line(&self, i: usize) -> Line<'static> {
        if self.rule && i == 1 {
            let rule: Vec<String> = self.widths.iter().map(|w| "─".repeat(*w)).collect();
            let gutter = ratatui::text::Span::styled(" ".repeat(self.digits + 1), dim());
            return Line::from(vec![gutter, ratatui::text::Span::styled(rule.join("─┼─"), dim())]);
        }
        let r = if self.rule && i > 1 { i - 1 } else { i };
        if r >= self.starts.len() {
            return Line::styled(format!("  … the first {MAX_ROWS} rows; m shows all of the text"), dim());
        }
        let mut spans = Vec::new();
        let (number, digits) = (if r == 0 { String::new() } else { r.to_string() }, self.digits);
        push_span(&mut spans, &format!("{number:>digits$} "), dim());
        for (c, &width) in self.widths.iter().enumerate() {
            if c > 0 {
                push_span(&mut spans, " │ ", dim());
            }
            let style = if r == 0 { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
            push_span(&mut spans, &fit(self.cell(r, c), width, self.numeric[c] && r > 0), style);
        }
        Line::from(spans)
    }
}

/// A cell cut or padded to exactly `width` columns.
fn fit(cell: &str, width: usize, right: bool) -> String {
    let text =
        if str_width(cell) > width { format!("{}…", split_at_width(cell, width - 1).0) } else { cell.to_string() };
    let pad = " ".repeat(width.saturating_sub(str_width(&text)));
    if right { format!("{pad}{text}") } else { format!("{text}{pad}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(src: &str, delimiter: u8) -> (Vec<String>, Rendered) {
        let r = render(src, delimiter);
        ((0..r.len()).map(|i| r.text(i)).collect(), r)
    }

    #[test]
    fn aligns_columns_and_numbers() {
        let (lines, r) = plain("name,qty\napple,3\n\"banana, ripe\",12\n", b',');
        assert_eq!(
            lines,
            vec!["  name         │ qty", "  ─────────────┼────", "1 apple        │   3", "2 banana, ripe │  12"]
        );
        assert_eq!((r.pinned, r.gutter), (2, 2));
        assert_eq!(r.src, vec![0, 0, 1, 2]);
    }

    #[test]
    fn tabs_ragged_rows_and_long_cells() {
        let long = "x".repeat(50);
        let (lines, _) = plain(&format!("a\tb\n1\n{long}\t2\n"), b'\t');
        assert_eq!(lines[2], format!("1 {:<40} │  ", "1"));
        assert!(lines[3].contains(&format!("{}…", "x".repeat(39))));
    }

    #[test]
    fn control_characters_and_huge_tables() {
        let (lines, _) = plain("a,b\nx\u{1b}[2J,y\n", b',');
        assert!(lines[2].contains("x\u{fffd}[2J"), "{lines:?}");
        // Many wide, empty columns: shown as a note rather than laid out
        let header: Vec<String> = (0..250).map(|i| format!("{:40}", i)).collect();
        let row = ",".repeat(249);
        let src = format!("{}\n{}", header.join(","), format!("{row}\n").repeat(20_000));
        let (lines, r) = plain(&src, b',');
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("too big to lay out"), "{lines:?}");
        assert_eq!(r.src, vec![0]);
    }

    #[test]
    fn multiline_cells_stay_on_one_row() {
        let (lines, r) = plain("a,b\n\"one\ntwo\",3\nx,4\n", b',');
        assert!(lines[2].contains("one↵two"));
        assert_eq!(r.src[3], 3);
    }
}
