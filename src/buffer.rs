//! The text of an open file, with its cursor, selection and undo history.

use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, bail};

use crate::text::{char_at_col, display_col};

/// A place in the buffer; `col` counts chars, not bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

pub enum Loaded {
    Text(Buffer),
    Binary(u64),
    TooBig(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit {
    Insert,
    Delete,
    Other,
}

struct Snapshot {
    lines: Vec<String>,
    cursor: Pos,
    /// Roughly how much text `lines` holds
    bytes: usize,
}

impl Snapshot {
    fn new(lines: Vec<String>, cursor: Pos) -> Self {
        let bytes = lines.iter().map(|l| l.len() + 1).sum();
        Self { lines, cursor, bytes }
    }
}

const MAX_FILE: u64 = 50 << 20;
const MAX_UNDO: usize = 200;
/// Every undo step is a whole copy of the text, so big files keep fewer of them
const MAX_UNDO_BYTES: usize = 256 << 20;
/// How much of a file is checked for NUL bytes to tell binary from text
const SNIFF: u64 = 8000;

/// A version no buffer has had before, so a new buffer never matches what was drawn
/// from an old one.
fn next_version() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Pos,
    /// The other end of the selection, while there is one
    pub anchor: Option<Pos>,
    pub dirty: bool,
    pub read_only: Option<&'static str>,
    /// New on every change, and never one another buffer had, so anything drawn from the
    /// text knows to redo it
    pub version: u64,
    /// The first line changed since the highlighter last caught up
    pub changed_from: Option<usize>,
    /// Screen column kept while moving up and down through shorter lines
    want_col: Option<usize>,
    crlf: bool,
    final_newline: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: Option<Edit>,
}

fn byte_at(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Buffer {
    pub fn load(path: &Path) -> Result<Loaded> {
        // Checked before opening: opening a FIFO blocks, and reading /dev/zero never ends
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            bail!("not a regular file");
        }
        let size = meta.len();
        if size > MAX_FILE {
            return Ok(Loaded::TooBig(size));
        }
        let mut file = File::open(path)?;
        let mut bytes = Vec::with_capacity(size as usize);
        (&mut file).take(SNIFF).read_to_end(&mut bytes)?;
        if bytes.contains(&0) {
            return Ok(Loaded::Binary(size));
        }
        file.take(MAX_FILE + 1 - bytes.len() as u64).read_to_end(&mut bytes)?;
        Ok(Self::from_bytes(bytes))
    }

    /// Text from bytes read some other way (from an archive, or an old commit).
    pub fn from_bytes(bytes: Vec<u8>) -> Loaded {
        let size = bytes.len() as u64;
        if size > MAX_FILE {
            return Loaded::TooBig(size);
        }
        if bytes[..bytes.len().min(SNIFF as usize)].contains(&0) {
            return Loaded::Binary(size);
        }
        Loaded::Text(match String::from_utf8(bytes) {
            Ok(text) => Self::from_text(&text),
            Err(e) => {
                let mut buf = Self::from_text(&String::from_utf8_lossy(e.as_bytes()));
                buf.read_only = Some("not UTF-8");
                buf
            }
        })
    }

    pub fn from_text(text: &str) -> Self {
        // Only when every line ends in CRLF: in a file that mixes the two, the lines keep
        // their \r so saving writes back each line's own ending
        let crlf = text.contains("\r\n") && text.matches("\r\n").count() == text.matches('\n').count();
        let body = text.strip_suffix('\n').unwrap_or(text);
        let lines =
            body.split('\n').map(|l| if crlf { l.strip_suffix('\r').unwrap_or(l) } else { l }.to_string()).collect();
        Self {
            lines,
            cursor: Pos::default(),
            anchor: None,
            dirty: false,
            read_only: None,
            version: next_version(),
            changed_from: None,
            want_col: None,
            crlf,
            final_newline: text.is_empty() || text.ends_with('\n'),
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
        }
    }

    /// The file contents, with the original line endings.
    pub fn text(&self) -> String {
        let nl = if self.crlf { "\r\n" } else { "\n" };
        let mut text = self.lines.join(nl);
        if self.final_newline && !text.is_empty() {
            text.push_str(nl);
        }
        text
    }

    pub fn save(&mut self, path: &Path) -> Result<()> {
        fs::write(path, self.text())?;
        self.dirty = false;
        Ok(())
    }

    pub fn line_len(&self, line: usize) -> usize {
        self.lines[line].chars().count()
    }

    pub fn end(&self) -> Pos {
        let line = self.lines.len() - 1;
        Pos::new(line, self.line_len(line))
    }

    pub fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.lines.len() - 1);
        Pos::new(line, pos.col.min(self.line_len(line)))
    }

    // Selection

    /// The selected range, start first.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        match anchor.cmp(&self.cursor) {
            std::cmp::Ordering::Less => Some((anchor, self.cursor)),
            std::cmp::Ordering::Greater => Some((self.cursor, anchor)),
            std::cmp::Ordering::Equal => None,
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        let part = |line: usize, from: usize, to: usize| -> String {
            self.lines[line].chars().skip(from).take(to.saturating_sub(from)).collect()
        };
        if s.line == e.line {
            return Some(part(s.line, s.col, e.col));
        }
        let mut text = part(s.line, s.col, usize::MAX);
        for line in &self.lines[s.line + 1..e.line] {
            text.push('\n');
            text.push_str(line);
        }
        text.push('\n');
        text.push_str(&part(e.line, 0, e.col));
        Some(text)
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(Pos::default());
        self.cursor = self.end();
    }

    pub fn select_word(&mut self, at: Pos) {
        let at = self.clamp(at);
        let chars: Vec<char> = self.lines[at.line].chars().collect();
        let (mut start, mut end) = (at.col, at.col);
        while start > 0 && is_word(chars[start - 1]) {
            start -= 1;
        }
        while end < chars.len() && is_word(chars[end]) {
            end += 1;
        }
        self.anchor = Some(Pos::new(at.line, start));
        self.cursor = Pos::new(at.line, end);
    }

    // Movement

    /// Moves the cursor; with `select` the selection grows from where the cursor was.
    pub fn move_to(&mut self, pos: Pos, select: bool) {
        if !select {
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
        self.cursor = self.clamp(pos);
        self.want_col = None;
        self.last_edit = None;
    }

    pub fn left(&mut self, select: bool, word: bool) {
        let Pos { line, col } = self.cursor;
        let to = if col == 0 {
            if line == 0 { self.cursor } else { Pos::new(line - 1, self.line_len(line - 1)) }
        } else if word {
            let chars: Vec<char> = self.lines[line].chars().take(col).collect();
            let mut i = col;
            while i > 0 && !is_word(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word(chars[i - 1]) {
                i -= 1;
            }
            Pos::new(line, i)
        } else {
            Pos::new(line, col - 1)
        };
        self.move_to(to, select);
    }

    pub fn right(&mut self, select: bool, word: bool) {
        let Pos { line, col } = self.cursor;
        let len = self.line_len(line);
        let to = if col >= len {
            if line + 1 < self.lines.len() { Pos::new(line + 1, 0) } else { self.cursor }
        } else if word {
            let chars: Vec<char> = self.lines[line].chars().collect();
            let mut i = col;
            while i < len && !is_word(chars[i]) {
                i += 1;
            }
            while i < len && is_word(chars[i]) {
                i += 1;
            }
            Pos::new(line, i)
        } else {
            Pos::new(line, col + 1)
        };
        self.move_to(to, select);
    }

    /// Up or down by `lines`, staying in the same screen column where the lines allow.
    pub fn vertical(&mut self, lines: isize, select: bool) {
        let want = self.want_col.unwrap_or_else(|| display_col(&self.lines[self.cursor.line], self.cursor.col));
        let line = self.cursor.line.saturating_add_signed(lines).min(self.lines.len() - 1);
        let col = char_at_col(&self.lines[line], want);
        self.move_to(Pos::new(line, col), select);
        self.want_col = Some(want);
    }

    /// To the first non-blank character, or to column 0 if already there.
    pub fn home(&mut self, select: bool) {
        let line = self.cursor.line;
        let indent = self.lines[line].chars().take_while(|c| c.is_whitespace()).count();
        let col = if self.cursor.col == indent { 0 } else { indent };
        self.move_to(Pos::new(line, col), select);
    }

    pub fn line_end(&mut self, select: bool) {
        let line = self.cursor.line;
        self.move_to(Pos::new(line, self.line_len(line)), select);
    }

    // Editing

    /// Types or pastes `text` over the selection.
    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let typing = text.chars().count() == 1 && text != "\n" && self.selection().is_none();
        self.begin(if typing { Edit::Insert } else { Edit::Other });
        self.delete_selection();
        self.insert_raw(&text);
    }

    /// Breaks the line, carrying its indentation over.
    pub fn newline(&mut self) {
        self.begin(Edit::Other);
        self.delete_selection();
        let indent: String = self.lines[self.cursor.line]
            .chars()
            .take(self.cursor.col)
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        self.insert_raw(&format!("\n{indent}"));
    }

    pub fn backspace(&mut self) {
        if self.selection().is_some() {
            self.begin(Edit::Other);
            self.delete_selection();
            return;
        }
        // An empty selection (a click leaves one) would become a real one once the cursor moves
        self.anchor = None;
        let Pos { line, col } = self.cursor;
        if col == 0 && line == 0 {
            return;
        }
        self.begin(Edit::Delete);
        if col > 0 {
            let at = byte_at(&self.lines[line], col - 1);
            self.lines[line].remove(at);
            self.cursor.col -= 1;
        } else {
            let joined = self.lines.remove(line);
            self.cursor = Pos::new(line - 1, self.line_len(line - 1));
            self.lines[line - 1].push_str(&joined);
        }
        self.changed(self.cursor.line);
    }

    pub fn delete(&mut self) {
        if self.selection().is_some() {
            self.begin(Edit::Other);
            self.delete_selection();
            return;
        }
        self.anchor = None;
        let Pos { line, col } = self.cursor;
        let at_end = col >= self.line_len(line);
        if at_end && line + 1 >= self.lines.len() {
            return;
        }
        self.begin(Edit::Delete);
        if at_end {
            let joined = self.lines.remove(line + 1);
            self.lines[line].push_str(&joined);
        } else {
            let at = byte_at(&self.lines[line], col);
            self.lines[line].remove(at);
        }
        self.changed(line);
    }

    /// Removes the selection and returns its text.
    pub fn cut(&mut self) -> Option<String> {
        let text = self.selected_text()?;
        self.begin(Edit::Other);
        self.delete_selection();
        Some(text)
    }

    pub fn undo(&mut self) -> bool {
        self.step_history(true)
    }

    pub fn redo(&mut self) -> bool {
        self.step_history(false)
    }

    fn step_history(&mut self, undo: bool) -> bool {
        let (from, to) = if undo { (&mut self.undo, &mut self.redo) } else { (&mut self.redo, &mut self.undo) };
        let Some(snap) = from.pop() else { return false };
        to.push(Snapshot::new(std::mem::replace(&mut self.lines, snap.lines), self.cursor));
        self.cursor = snap.cursor;
        self.anchor = None;
        self.last_edit = None;
        self.changed(0);
        true
    }

    /// Records an undo step, except that a run of typing (or of deleting) undoes as one.
    fn begin(&mut self, kind: Edit) {
        if kind == Edit::Other || self.last_edit != Some(kind) {
            self.undo.push(Snapshot::new(self.lines.clone(), self.cursor));
            let mut bytes: usize = self.undo.iter().map(|s| s.bytes).sum();
            while self.undo.len() > MAX_UNDO || (self.undo.len() > 1 && bytes > MAX_UNDO_BYTES) {
                bytes -= self.undo.remove(0).bytes;
            }
        }
        self.redo.clear();
        self.last_edit = Some(kind);
    }

    fn changed(&mut self, from: usize) {
        self.dirty = true;
        self.version = next_version();
        self.want_col = None;
        self.changed_from = Some(self.changed_from.map_or(from, |l| l.min(from)));
    }

    fn delete_selection(&mut self) {
        let Some((s, e)) = self.selection() else {
            self.anchor = None;
            return;
        };
        let tail = {
            let line = &self.lines[e.line];
            line[byte_at(line, e.col)..].to_string()
        };
        let head = &mut self.lines[s.line];
        head.truncate(byte_at(head, s.col));
        head.push_str(&tail);
        self.lines.drain(s.line + 1..=e.line);
        self.cursor = s;
        self.anchor = None;
        self.changed(s.line);
    }

    fn insert_raw(&mut self, text: &str) {
        let Pos { line, col } = self.cursor;
        let at = byte_at(&self.lines[line], col);
        let rest = self.lines[line].split_off(at);
        let mut parts = text.split('\n');
        self.lines[line].push_str(parts.next().unwrap_or(""));
        let added: Vec<String> = parts.map(String::from).collect();
        let last = line + added.len();
        self.lines.splice(line + 1..line + 1, added);
        self.cursor = Pos::new(last, self.line_len(last));
        self.lines[last].push_str(&rest);
        self.changed(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &str) -> Buffer {
        Buffer::from_text(text)
    }

    #[test]
    fn keeps_line_endings() {
        assert_eq!(buf("a\nb\n").text(), "a\nb\n");
        assert_eq!(buf("a\nb").text(), "a\nb");
        assert_eq!(buf("a\r\nb\r\n").text(), "a\r\nb\r\n");
        assert_eq!(buf("a\nb\r\nc\n").text(), "a\nb\r\nc\n");
        assert_eq!(buf("").lines, vec![""]);
    }

    #[test]
    fn typing_newlines_and_deleting() {
        let mut b = buf("    fn x() {}\n");
        b.move_to(Pos::new(0, 12), false);
        b.newline();
        assert_eq!(b.lines, vec!["    fn x() {", "    }"]);
        assert_eq!(b.cursor, Pos::new(1, 4));
        b.insert("ab");
        b.backspace();
        assert_eq!(b.lines[1], "    a}");
        b.move_to(Pos::new(0, 12), false);
        b.delete();
        assert_eq!(b.lines, vec!["    fn x() {    a}"]);
    }

    #[test]
    fn selection_paste_and_undo() {
        let mut b = buf("one two\nthree\n");
        b.move_to(Pos::new(0, 4), false);
        b.move_to(Pos::new(1, 2), true);
        assert_eq!(b.selected_text().as_deref(), Some("two\nth"));
        b.insert("X\nY");
        assert_eq!(b.lines, vec!["one X", "Yree"]);
        assert!(b.undo());
        assert_eq!(b.lines, vec!["one two", "three"]);
        assert!(b.redo());
        assert_eq!(b.lines, vec!["one X", "Yree"]);
    }

    #[test]
    fn a_run_of_typing_undoes_at_once() {
        let mut b = buf("");
        for c in "hello".chars() {
            b.insert(&c.to_string());
        }
        b.undo();
        assert_eq!(b.lines, vec![""]);
    }

    #[test]
    fn deleting_after_a_click_deletes_one_character_at_a_time() {
        // A click leaves the selection's other end where the cursor is
        let mut b = buf("hello world\n");
        b.move_to(Pos::new(0, 5), false);
        b.anchor = Some(b.cursor);
        b.backspace();
        b.backspace();
        assert_eq!(b.lines, vec!["hel world"]);
        // Joining lines doesn't leave it on a line that's gone
        let mut b = buf("one\ntwo\n");
        b.move_to(Pos::new(1, 0), false);
        b.anchor = Some(b.cursor);
        b.backspace();
        b.backspace();
        b.insert("e");
        assert_eq!(b.lines, vec!["onetwo"]);
    }

    #[test]
    fn vertical_moves_keep_the_column() {
        let mut b = buf("long line\nab\nanother long\n");
        b.move_to(Pos::new(0, 7), false);
        b.vertical(1, false);
        assert_eq!(b.cursor, Pos::new(1, 2));
        b.vertical(1, false);
        assert_eq!(b.cursor, Pos::new(2, 7));
    }
}
