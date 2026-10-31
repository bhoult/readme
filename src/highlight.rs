//! Syntax highlighting with syntect, using bat's syntax definitions.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use ratatui::style::{Color, Modifier, Style};
use syntect::{
    easy::HighlightLines,
    highlighting::{self, FontStyle, HighlightIterator, HighlightState, Highlighter, Theme},
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
};
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

pub struct Highlighting {
    syntaxes: SyntaxSet,
}

static LOADED: OnceLock<Highlighting> = OnceLock::new();

/// The theme the colors come from, and how many times it's been changed, which tells
/// colors worked out with an earlier one apart.
struct Chosen {
    name: EmbeddedThemeName,
    theme: Arc<Theme>,
    changes: u64,
}

impl Chosen {
    fn new(name: EmbeddedThemeName, changes: u64) -> Self {
        Self { name, theme: Arc::new(two_face::theme::extra().get(name).clone()), changes }
    }

    /// README_THEME's theme, or else ansi, which draws with the terminal's own palette
    /// and so suits light and dark terminals alike.
    fn first() -> Self {
        let wanted = std::env::var("README_THEME").ok().and_then(|name| named(&name));
        Self::new(wanted.unwrap_or(EmbeddedThemeName::Ansi), 0)
    }
}

#[cfg(not(test))]
static CHOSEN: Mutex<Option<Chosen>> = Mutex::new(None);

#[cfg(not(test))]
fn chosen<R>(f: impl FnOnce(&mut Chosen) -> R) -> R {
    f(CHOSEN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Chosen::first))
}

// Each test has a theme of its own, as it has a thread of its own, so one that changes
// it leaves the colors in the others alone
#[cfg(test)]
thread_local! {
    static CHOSEN: std::cell::RefCell<Option<Chosen>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn chosen<R>(f: impl FnOnce(&mut Chosen) -> R) -> R {
    CHOSEN.with(|chosen| f(chosen.borrow_mut().get_or_insert_with(Chosen::first)))
}

/// The theme called `name`, in any case.
fn named(name: &str) -> Option<EmbeddedThemeName> {
    EmbeddedLazyThemeSet::theme_names().iter().copied().find(|n| n.as_name().eq_ignore_ascii_case(name.trim()))
}

/// The themes there are, by name, ansi (the terminal's own palette) first.
pub fn themes() -> impl Iterator<Item = &'static str> {
    EmbeddedLazyThemeSet::theme_names().iter().map(|n| n.as_name())
}

/// The name of the theme the colors come from.
pub fn theme_name() -> &'static str {
    chosen(|chosen| chosen.name.as_name())
}

/// Takes the colors from the theme called `name` (in any case) from now on; false if
/// there's no such theme.
pub fn set_theme(name: &str) -> bool {
    let Some(name) = named(name) else { return false };
    chosen(|chosen| {
        if chosen.name != name {
            *chosen = Chosen::new(name, chosen.changes + 1);
        }
    });
    true
}

fn theme() -> Arc<Theme> {
    chosen(|chosen| chosen.theme.clone())
}

/// What colors are worked out with: 0 until highlighting has loaded, and a new number
/// after each change of theme.
pub fn version() -> u64 {
    if LOADED.get().is_some() { chosen(|chosen| chosen.changes) + 1 } else { 0 }
}

/// Loading the syntax definitions takes a moment, so it happens in the background;
/// until it's done, files show without colors.
pub fn preload() {
    std::thread::spawn(load);
}

pub fn get() -> Option<&'static Highlighting> {
    LOADED.get()
}

/// Lines of a file a syntax is built from
const SAMPLE: usize = 64;

/// Whether `syntax` is ready to highlight with. Its patterns are built the first time
/// they're used, which for some (TypeScript) takes a third of a second, so the first
/// time a syntax is asked for they're built in the background, from `text`'s first lines,
/// and until then the file shows without colors, as it does while highlighting loads.
fn built(hl: &'static Highlighting, syntax: &'static SyntaxReference, text: &[String]) -> bool {
    // Each syntax asked for, by name, and whether it's built yet
    static BUILT: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let built = BUILT.get_or_init(Mutex::default);
    let mut asked = built.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&done) = asked.get(&syntax.name) {
        return done;
    }
    asked.insert(syntax.name.clone(), false);
    let sample: Vec<String> = text.iter().take(SAMPLE).filter(|l| l.len() <= MAX_LINE).cloned().collect();
    let theme = theme();
    let started = std::thread::Builder::new().name("syntax".into()).spawn(move || {
        // Done even if building fails, so the file isn't left waiting: what's needed is
        // then built as it's highlighted
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut lines = HighlightLines::new(syntax, &theme);
            for line in &sample {
                let _ = lines.highlight_line(&format!("{line}\n"), &hl.syntaxes);
            }
        }));
        built.lock().unwrap_or_else(|e| e.into_inner()).insert(syntax.name.clone(), true);
    });
    // With no thread, it's built as it's highlighted
    if started.is_err() {
        asked.insert(syntax.name.clone(), true);
    }
    started.is_err()
}

pub fn load() -> &'static Highlighting {
    LOADED.get_or_init(|| Highlighting { syntaxes: two_face::syntax::extra_newlines() })
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
/// Most time one call spends highlighting, so a jump to the end of a big file doesn't
/// hold up the screen: the next frames do the rest
const SLICE: Duration = Duration::from_millis(25);

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
    /// The syntax is being built in the background, so there are no colors yet
    building: bool,
    /// The theme's changes when these colors were worked out
    changes: u64,
}

impl LineCache {
    pub fn new(path: &Path, lines: &[String]) -> Self {
        let disabled = too_slow(lines);
        let (lines, checkpoints, changes) = (Vec::new(), Vec::new(), chosen(|chosen| chosen.changes));
        Self { path: path.into(), syntax: None, disabled, lines, checkpoints, state: None, building: false, changes }
    }

    /// The text was replaced by `lines`, the same as before up to line `from`: what's
    /// highlighted before that stays, unless the first line (which can decide the
    /// syntax) changed, or whether to highlight at all.
    pub fn replaced(&mut self, lines: &[String], from: usize) {
        if from == 0 || self.disabled != too_slow(lines) {
            let path = std::mem::take(&mut self.path);
            *self = Self::new(&path, lines);
        } else {
            self.forget(from);
        }
    }

    /// The text was edited from line `from` on. A first line typed in (`#!/bin/sh`) can
    /// set the syntax, and a long line pasted in turns highlighting off (syntect takes
    /// seconds over a minified line) until it's gone, as if the file were opened now.
    pub fn invalidate(&mut self, lines: &[String], from: usize) {
        if from == 0 || self.disabled != too_slow(lines) {
            self.replaced(lines, 0);
        } else {
            self.forget(from);
        }
    }

    /// Forgets everything from line `from` on.
    fn forget(&mut self, from: usize) {
        if from >= self.lines.len() {
            return;
        }
        let cp = from / CHECKPOINT;
        self.lines.truncate(cp * CHECKPOINT);
        self.checkpoints.truncate(cp + 1);
        self.state = self.checkpoints.last().cloned();
    }

    /// Highlights through line `upto` (exclusive) of `text`, or as far as it gets in
    /// SLICE; false if there's more to do.
    pub fn ensure(&mut self, text: &[String], upto: usize) -> bool {
        let Some(hl) = get() else { return true };
        if self.disabled {
            return true;
        }
        let syntax =
            *self.syntax.get_or_insert_with(|| hl.syntax_for(&self.path, text.first().map_or("", String::as_str)));
        let Some(syntax) = syntax else { return true };
        self.building = !built(hl, syntax, text);
        if self.building {
            return true;
        }
        // Colors from another theme, and the parser's states, which carry its styles,
        // are worked out again
        let changes = chosen(|chosen| chosen.changes);
        if self.changes != changes {
            (self.lines, self.checkpoints, self.state, self.changes) = (Vec::new(), Vec::new(), None, changes);
        }
        let started = Instant::now();
        let theme = theme();
        let highlighter = Highlighter::new(&theme);
        let state = self
            .state
            .get_or_insert_with(|| (ParseState::new(syntax), HighlightState::new(&highlighter, ScopeStack::new())));
        while self.lines.len() < upto.min(text.len()) {
            let n = self.lines.len();
            if n.is_multiple_of(CHECKPOINT) && started.elapsed() > SLICE {
                return false;
            }
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
        true
    }

    pub fn get(&self, line: usize) -> &[(Style, Range<usize>)] {
        self.lines.get(line).map_or(&[], Vec::as_slice)
    }

    /// Whether the colors wait on the syntax being built, so it's worth looking again soon.
    pub fn building(&self) -> bool {
        self.building
    }
}

/// Lines longer than this (minified code) would make syntect crawl.
const MAX_LINE: usize = 10_000;

/// Huge files and minified lines would make syntect crawl.
fn too_slow(lines: &[String]) -> bool {
    lines.len() > 100_000 || lines.iter().any(|l| l.len() > MAX_LINE)
}

/// Most code a fenced block may have to be highlighted. Rendering happens all at once,
/// and again for every new width, so a big block shows without colors.
const MAX_BLOCK: usize = 256 << 10;

/// Highlights a fenced code block. None if the language isn't known, highlighting
/// hasn't loaded yet, or the block is too big or has lines too long to do quickly.
pub fn block(lang: &str, code: &str) -> Option<Vec<Vec<(Style, String)>>> {
    if code.len() > MAX_BLOCK || code.lines().any(|l| l.len() > MAX_LINE) {
        return None;
    }
    let hl = get()?;
    let syntax = hl.syntaxes.find_syntax_by_token(lang)?;
    let theme = theme();
    let mut lines = HighlightLines::new(syntax, &theme);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_text_keeps_the_colors_above_the_change() {
        load();
        let text: Vec<String> = (0..200).map(|i| format!("let x{i} = {i};")).collect();
        let mut cache = LineCache::new(Path::new("a.rs"), &text);
        while !cache.ensure(&text, 200) || cache.building() {}
        assert_eq!(cache.lines.len(), 200);
        // A line added at the end, as a log or an agent's notes grow
        let mut more = text.clone();
        more.push("let y = 1;".into());
        cache.replaced(&more, 200);
        assert_eq!(cache.lines.len(), 200);
        // A change further up goes back to the checkpoint before it
        cache.replaced(&more, 130);
        assert_eq!(cache.lines.len(), 128);
        while !cache.ensure(&more, 201) || cache.building() {}
        assert_eq!(cache.get(200), LineCache::new(Path::new("a.rs"), &more).colors_of(&more, 200));
        // The first line can change the syntax (#!/bin/sh), so that starts again
        cache.replaced(&more, 0);
        assert!(cache.lines.is_empty());
    }

    #[test]
    fn edits_can_change_the_syntax_or_turn_colors_off() {
        load();
        let mut text: Vec<String> = vec!["echo hi".into(); 3];
        let mut cache = LineCache::new(Path::new("script"), &text);
        while !cache.ensure(&text, 3) || cache.building() {}
        assert!(cache.get(0).iter().all(|(s, _)| s.fg.is_none()), "no syntax yet");
        text.insert(0, "#!/bin/sh".into());
        cache.invalidate(&text, 0);
        while !cache.ensure(&text, 4) || cache.building() {}
        assert!(cache.syntax.flatten().is_some_and(|s| s.name.contains("Bourne") || s.name.contains("Shell")));
        // A minified line pasted in turns highlighting off, until it's gone
        text.push("x".repeat(20_000));
        cache.invalidate(&text, 4);
        assert!(cache.disabled);
        text.pop();
        cache.invalidate(&text, 4);
        assert!(!cache.disabled);
        assert!(block("js", &"x".repeat(20_000)).is_none());
    }

    #[test]
    fn a_new_theme_colors_what_was_colored_again() {
        load();
        let text: Vec<String> = vec!["fn main() {}".into(); 3];
        let mut cache = LineCache::new(Path::new("a.rs"), &text);
        while !cache.ensure(&text, 3) || cache.building() {}
        let before = cache.get(0).to_vec();
        let was = version();
        assert!(set_theme("nord") && theme_name() == "Nord" && version() > was);
        // Nothing's worked out again until it's asked for
        assert_eq!(cache.get(0), before.as_slice());
        while !cache.ensure(&text, 3) || cache.building() {}
        assert_ne!(cache.get(0), before.as_slice());
        assert!(cache.get(0).iter().any(|(s, _)| matches!(s.fg, Some(Color::Rgb(..)))), "{:?}", cache.get(0));
        assert!(!set_theme("no such theme") && theme_name() == "Nord");
        assert_eq!(themes().next(), Some("ansi"));
    }

    impl LineCache {
        fn colors_of(mut self, text: &[String], line: usize) -> Vec<(Style, Range<usize>)> {
            while !self.ensure(text, line + 1) || self.building() {}
            self.get(line).to_vec()
        }
    }
}
