//! Syntax highlighting with syntect, using bat's syntax definitions.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use ratatui::style::{Color, Modifier, Style};
use syntect::{
    easy::HighlightLines,
    highlighting::{self, FontStyle, HighlightIterator, HighlightState, Highlighter, Theme},
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
};
use two_face::theme::EmbeddedThemeName;

pub struct Highlighting {
    syntaxes: SyntaxSet,
    theme: Theme,
}

static LOADED: OnceLock<Highlighting> = OnceLock::new();

/// Loading the syntax definitions takes a moment, so it happens in the background;
/// until it's done, files show without colors.
pub fn preload() {
    std::thread::spawn(load);
}

pub fn get() -> Option<&'static Highlighting> {
    LOADED.get()
}

pub fn load() -> &'static Highlighting {
    LOADED.get_or_init(|| {
        // The "ansi" theme uses the terminal's own palette, so it suits light and dark
        // themes alike; README_THEME picks any other theme bat ships.
        let wanted = std::env::var("README_THEME").unwrap_or_default();
        let name = two_face::theme::EmbeddedLazyThemeSet::theme_names()
            .iter()
            .copied()
            .find(|n| n.as_name().eq_ignore_ascii_case(&wanted))
            .unwrap_or(EmbeddedThemeName::Ansi);
        Highlighting { syntaxes: two_face::syntax::extra_newlines(), theme: two_face::theme::extra().get(name).clone() }
    })
}

impl Highlighting {
    /// From the name alone (the text may come from an archive or an old commit, not
    /// the disk), then from a first line such as `#!/bin/sh`.
    fn syntax_for(&self, path: &Path, first_line: &str) -> Option<&SyntaxReference> {
        let part = |p: Option<&std::ffi::OsStr>| p.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        // Whole names first, for Makefile, Dockerfile, .bashrc and the like
        self.syntaxes
            .find_syntax_by_extension(&part(path.file_name()))
            .or_else(|| self.syntaxes.find_syntax_by_extension(&part(path.extension())))
            .or_else(|| self.syntaxes.find_syntax_by_first_line(first_line))
            .filter(|s| s.name != "Plain Text")
    }
}

/// Only foreground colors and font styles: the terminal keeps its own background.
fn to_style(style: highlighting::Style) -> Style {
    let c = style.foreground;
    let mut out = Style::new();
    out.fg = match c.a {
        // Terminal themes (ansi, base16) put a palette index in the red channel
        0 => Some(Color::Indexed(c.r)),
        // ...or mean the terminal's default color
        1 => None,
        _ => Some(Color::Rgb(c.r, c.g, c.b)),
    };
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Colors for one line, as byte ranges of the line.
pub type Colors = Vec<(Style, Range<usize>)>;

const CHECKPOINT: usize = 64;

/// Highlighted lines of a file, computed as far down as has been shown. Edits throw
/// away what follows them, back to the nearest saved parser state.
pub struct LineCache {
    path: PathBuf,
    /// None until highlighting has loaded and the file's syntax has been looked up
    syntax: Option<Option<&'static SyntaxReference>>,
    disabled: bool,
    lines: Vec<Colors>,
    /// Parser state at the start of every CHECKPOINT-th line
    checkpoints: Vec<(ParseState, HighlightState)>,
    /// Parser state at the start of line `lines.len()`
    state: Option<(ParseState, HighlightState)>,
}

impl LineCache {
    pub fn new(path: &Path, lines: &[String]) -> Self {
        // Huge files and minified lines would make syntect crawl
        let disabled = lines.len() > 100_000 || lines.iter().any(|l| l.len() > 10_000);
        Self { path: path.into(), syntax: None, disabled, lines: Vec::new(), checkpoints: Vec::new(), state: None }
    }

    /// Forgets everything from line `from` on.
    pub fn invalidate(&mut self, from: usize) {
        if from >= self.lines.len() {
            return;
        }
        let cp = from / CHECKPOINT;
        self.lines.truncate(cp * CHECKPOINT);
        self.checkpoints.truncate(cp + 1);
        self.state = self.checkpoints.last().cloned();
    }

    /// Highlights through line `upto` (exclusive) of `text`.
    pub fn ensure(&mut self, text: &[String], upto: usize) {
        let Some(hl) = get() else { return };
        if self.disabled {
            return;
        }
        let syntax =
            *self.syntax.get_or_insert_with(|| hl.syntax_for(&self.path, text.first().map_or("", String::as_str)));
        let Some(syntax) = syntax else { return };
        let highlighter = Highlighter::new(&hl.theme);
        let state = self
            .state
            .get_or_insert_with(|| (ParseState::new(syntax), HighlightState::new(&highlighter, ScopeStack::new())));
        while self.lines.len() < upto.min(text.len()) {
            let n = self.lines.len();
            if n.is_multiple_of(CHECKPOINT) && n / CHECKPOINT == self.checkpoints.len() {
                self.checkpoints.push(state.clone());
            }
            let line = format!("{}\n", text[n]);
            let ops = state.0.parse_line(&line, &hl.syntaxes).unwrap_or_default();
            let mut colors = Vec::new();
            let mut pos = 0;
            for (style, piece) in HighlightIterator::new(&mut state.1, &ops, &line, &highlighter) {
                let end = (pos + piece.len()).min(text[n].len());
                if end > pos {
                    colors.push((to_style(style), pos..end));
                }
                pos += piece.len();
            }
            self.lines.push(colors);
        }
    }

    pub fn get(&self, line: usize) -> &[(Style, Range<usize>)] {
        self.lines.get(line).map_or(&[], Vec::as_slice)
    }
}

/// Highlights a fenced code block. None if the language isn't known, or highlighting
/// hasn't loaded yet.
pub fn block(lang: &str, code: &str) -> Option<Vec<Vec<(Style, String)>>> {
    let hl = get()?;
    let syntax = hl.syntaxes.find_syntax_by_token(lang)?;
    let mut lines = HighlightLines::new(syntax, &hl.theme);
    Some(
        code.lines()
            .map(|line| {
                lines
                    .highlight_line(&format!("{line}\n"), &hl.syntaxes)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(style, text)| (to_style(style), text.trim_end_matches('\n').to_string()))
                    .filter(|(_, text)| !text.is_empty())
                    .collect()
            })
            .collect(),
    )
}
