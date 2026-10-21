//! CSV and TSV files shown as a table: aligned columns, the first row as a header
//! that stays on screen, numbers right-aligned, and row numbers down the side.

use ratatui::{
    style::{Modifier, Style},
    text::Line,
};

use crate::{
    markdown::Rendered,
    text::{push_span, split_at_width, str_width},
};

/// Cells wider than this are cut short with "…"
const MAX_CELL: usize = 40;

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
    let rows: Vec<(usize, Vec<String>)> = reader
        .records()
        .filter_map(Result::ok)
        .map(|record| {
            let line = record.position().map_or(0, |p| p.line().saturating_sub(1) as usize);
            let cells =
                record.iter().map(|c| c.replace("\r\n", "↵").replace(['\n', '\r'], "↵").replace('\t', " ")).collect();
            (line, cells)
        })
        .collect();
    if rows.is_empty() {
        return Rendered { lines: vec![Line::styled("(empty)", dim())], src: vec![0], pinned: 0, gutter: 0, width: 0 };
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
    let mut out = Rendered { lines: Vec::new(), src: Vec::new(), pinned: 0, gutter, width };
    for (r, (line, cells)) in rows.iter().enumerate() {
        let mut spans = Vec::new();
        let number = if r == 0 { String::new() } else { r.to_string() };
        push_span(&mut spans, &format!("{number:>digits$} "), dim());
        for (c, &width) in widths.iter().enumerate() {
            if c > 0 {
                push_span(&mut spans, " │ ", dim());
            }
            let style = if r == 0 { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
            let cell = cells.get(c).map_or("", String::as_str);
            push_span(&mut spans, &fit(cell, width, numeric[c] && r > 0), style);
        }
        out.lines.push(Line::from(spans));
        out.src.push(*line);
        if r == 0 && rows.len() > 1 {
            let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            out.lines.push(Line::from(vec![
                ratatui::text::Span::styled(" ".repeat(gutter), dim()),
                ratatui::text::Span::styled(rule.join("─┼─"), dim()),
            ]));
            out.src.push(*line);
            out.pinned = 2;
        }
    }
    out
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
        ((0..r.lines.len()).map(|i| r.text(i)).collect(), r)
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
    fn multiline_cells_stay_on_one_row() {
        let (lines, r) = plain("a,b\n\"one\ntwo\",3\nx,4\n", b',');
        assert!(lines[2].contains("one↵two"));
        assert_eq!(r.src[3], 3);
    }
}
