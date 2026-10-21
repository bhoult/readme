//! The application: what's open, input handling, and drawing.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use ratatui::{
    Frame,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_image::picker::Picker;

use crate::{
    archive,
    buffer::{Buffer, Loaded, Pos},
    clipboard, external, fileops,
    gallery::{self, Gallery},
    git,
    grep::Grep,
    highlight::{self, LineCache},
    markdown::{self, Rendered},
    picture::{self, Picture},
    table,
    text::{
        char_at_col, display_col, find_all, line_spans, push_span, row_of, search, slice_line, str_width, wrap_points,
    },
    tree::Tree,
};

const MESSAGE_FOR: Duration = Duration::from_secs(4);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const TREE_REFRESH: Duration = Duration::from_secs(2);
const WHEEL: isize = 3;
/// Columns an arrow key scrolls sideways
const SIDEWAYS: isize = 8;
/// Markdown bigger than this opens as plain text
const MAX_RENDERED: usize = 5 << 20;

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn match_style() -> Style {
    Style::new().fg(Color::Black).bg(Color::Yellow)
}

fn selection_style() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Tree,
    Content,
}

/// How a file can be shown besides its plain text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Markdown,
    /// CSV or TSV, with its delimiter
    Table(u8),
}

/// Where what's shown came from. Only files on disk get edited or reloaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Disk,
    Archive,
    History,
}

/// An open text file and how it's being shown.
struct Doc {
    path: PathBuf,
    source: Source,
    /// Title in place of the path (for a file from history, say)
    label: Option<String>,
    buf: Buffer,
    colors: LineCache,
    kind: Kind,
    /// The plain text rather than the formatted view (Markdown, tables)
    raw: bool,
    editing: bool,
    wrap: bool,
    /// The source view's first line on screen, which of its wrapped rows is at the
    /// top, and (when not wrapping) the first screen column
    top: usize,
    top_row: usize,
    left: usize,
    /// Column within a wrapped row, kept while moving up and down
    wrap_x: Option<usize>,
    /// The formatted view, with the buffer version, width and highlighting it was made for
    rendered: Option<((u64, u16, bool, bool), Rendered)>,
    /// First formatted line below the pinned ones, and how far it's scrolled sideways
    fmt_top: usize,
    fmt_left: usize,
    /// Source line to put at the top of the formatted view once it's rendered
    fmt_sync: Option<usize>,
    last_match: Option<Pos>,
    /// Modification time and size when last read or saved
    disk: (Option<SystemTime>, u64),
    /// Changed on disk while it had unsaved changes here
    disk_changed: bool,
    /// The longest line's width, roughly, for the buffer version it was measured on
    widest: Option<(u64, usize)>,
    /// Whether the view's cut off anything to the left, and to the right
    more: (bool, bool),
}

fn disk_stamp(path: &Path) -> (Option<SystemTime>, u64) {
    fs::metadata(path).map_or((None, 0), |m| (m.modified().ok(), m.len()))
}

impl Doc {
    fn new(path: &Path, buf: Buffer, wrap: bool, source: Source, label: Option<String>) -> Self {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let kind = match ext.as_str() {
            "md" | "markdown" | "mdx" => Kind::Markdown,
            ext => table::delimiter(ext).map_or(Kind::Text, Kind::Table),
        };
        let mut doc = Self {
            path: path.into(),
            source,
            label,
            colors: LineCache::new(path, &buf.lines),
            buf,
            kind,
            raw: false,
            editing: false,
            wrap,
            top: 0,
            top_row: 0,
            left: 0,
            wrap_x: None,
            rendered: None,
            widest: None,
            more: (false, false),
            fmt_top: 0,
            fmt_left: 0,
            fmt_sync: None,
            last_match: None,
            disk: disk_stamp(path),
            disk_changed: false,
        };
        doc.raw = doc.too_big_to_render();
        doc
    }

    /// Markdown too big to render, which shows as text: rendering takes many times the
    /// text's size.
    fn too_big_to_render(&self) -> bool {
        self.kind == Kind::Markdown && self.buf.lines.iter().map(|l| l.len() + 1).sum::<usize>() > MAX_RENDERED
    }

    /// Back to the text if the Markdown has grown too big to render (reloaded, or edited),
    /// as it would show if it were opened now.
    fn keep_renderable(&mut self) {
        if self.raw || !self.too_big_to_render() {
            return;
        }
        if self.formatted() {
            (self.top, self.top_row) = (self.formatted_top_src(), 0);
        }
        self.raw = true;
        self.rendered = None;
    }

    /// Showing the formatted view rather than the text.
    fn formatted(&self) -> bool {
        self.kind != Kind::Text && !self.raw && !self.editing
    }

    fn render(&mut self, width: u16) -> &Rendered {
        // Tables don't depend on the width, so resizing doesn't redo them
        let width = if let Kind::Table(_) = self.kind { 0 } else { width };
        let key = (self.buf.version, width, highlight::get().is_some(), self.wrap);
        if self.rendered.as_ref().is_none_or(|(k, _)| *k != key) {
            let text = self.buf.lines.join("\n");
            let rendered = match self.kind {
                Kind::Table(delimiter) => table::render(&text, delimiter),
                _ => markdown::render(&text, width, self.wrap),
            };
            self.rendered = Some((key, rendered));
        }
        let rendered = &self.rendered.as_ref().unwrap().1;
        if let Some(line) = self.fmt_sync.take() {
            self.fmt_top = rendered.line_for_src(line).max(rendered.pinned);
        }
        rendered
    }

    fn pinned(&self) -> usize {
        self.rendered.as_ref().map_or(0, |(_, r)| r.pinned)
    }

    /// The source line shown at the top of the formatted view.
    fn formatted_top_src(&self) -> usize {
        self.rendered.as_ref().and_then(|(_, r)| r.src.get(self.fmt_top).copied()).unwrap_or(self.top)
    }

    /// Where each screen row of `line` starts: one row unless wrapping.
    fn rows(&self, line: usize, width: usize) -> Vec<usize> {
        if self.wrap { wrap_points(&self.buf.lines[line], width) } else { vec![0] }
    }

    /// `delta` screen rows on from (line, row), stopping at either end of the file.
    fn step_rows(&self, mut at: (usize, usize), delta: isize, width: usize) -> (usize, usize) {
        let last = self.buf.lines.len() - 1;
        // A kept place can be out of date: the file got shorter, or a wider pane wraps a
        // line into fewer rows
        at.0 = at.0.min(last);
        at.1 = at.1.min(self.rows(at.0, width).len() - 1);
        for _ in 0..delta.unsigned_abs() {
            if delta > 0 {
                if at.1 + 1 < self.rows(at.0, width).len() {
                    at.1 += 1;
                } else if at.0 < last {
                    at = (at.0 + 1, 0);
                } else {
                    break;
                }
            } else if at.1 > 0 {
                at.1 -= 1;
            } else if at.0 > 0 {
                at.0 -= 1;
                at.1 = self.rows(at.0, width).len() - 1;
            } else {
                break;
            }
        }
        at
    }

    /// The top that puts the end of the file at the bottom of the screen.
    fn last_top(&self, height: usize, width: usize) -> (usize, usize) {
        let mut left = height.max(1);
        let mut line = self.buf.lines.len() - 1;
        loop {
            let rows = self.rows(line, width).len();
            if rows >= left {
                return (line, rows - left);
            }
            left -= rows;
            if line == 0 {
                return (0, 0);
            }
            line -= 1;
        }
    }

    fn scroll(&mut self, delta: isize, height: usize, width: usize) {
        if self.formatted() {
            let pinned = self.pinned();
            let len = self.rendered.as_ref().map_or(0, |(_, r)| r.lines.len());
            let max = len.saturating_sub(height.saturating_sub(pinned)).max(pinned);
            self.fmt_top = self.fmt_top.max(pinned).saturating_add_signed(delta).clamp(pinned, max);
        } else if self.wrap {
            let at = self.step_rows((self.top, self.top_row), delta, width);
            (self.top, self.top_row) = at.min(self.last_top(height, width));
        } else {
            let max = self.buf.lines.len().saturating_sub(height);
            self.top = self.top.saturating_add_signed(delta).min(max);
        }
    }

    /// To the start or the end of the file.
    fn scroll_to(&mut self, end: bool, height: usize, width: usize) {
        if self.formatted() {
            self.fmt_top = if end { usize::MAX } else { 0 };
            // Clamped when drawn
            self.scroll(0, height, width);
        } else if !end {
            (self.top, self.top_row) = (0, 0);
        } else if self.wrap {
            (self.top, self.top_row) = self.last_top(height, width);
        } else {
            self.top = self.buf.lines.len().saturating_sub(height);
        }
    }

    fn scrolled_sideways(&self) -> bool {
        if self.formatted() { self.fmt_left > 0 } else { !self.wrap && self.left > 0 }
    }

    fn scroll_sideways(&mut self, delta: isize) {
        if self.formatted() {
            self.fmt_left = self.fmt_left.saturating_add_signed(delta);
        } else if !self.wrap {
            self.left = self.left.saturating_add_signed(delta);
        }
    }

    /// Scrolls so the cursor is on screen.
    fn follow_cursor(&mut self, height: usize, width: usize) {
        let cursor = self.buf.cursor;
        if !self.wrap {
            if cursor.line < self.top {
                self.top = cursor.line;
            } else if height > 0 && cursor.line >= self.top + height {
                self.top = cursor.line + 1 - height;
            }
            let col = display_col(&self.buf.lines[cursor.line], cursor.col);
            if col < self.left {
                self.left = col;
            } else if width > 0 && col >= self.left + width {
                self.left = col + 1 - width;
            }
            return;
        }
        self.left = 0;
        let at = (cursor.line, row_of(&self.rows(cursor.line, width), cursor.col));
        if at < (self.top, self.top_row) {
            (self.top, self.top_row) = at;
        } else if cursor.line >= self.top + height
            || self.step_rows((self.top, self.top_row), height as isize - 1, width) < at
        {
            // Put the cursor's row at the bottom
            (self.top, self.top_row) = self.step_rows(at, -(height as isize - 1), width);
        }
    }

    /// The buffer position under a screen cell of the source view.
    fn pos_at(&self, area: Rect, gutter: u16, x: u16, y: u16, width: usize) -> Pos {
        let dy = y.saturating_sub(area.y) as isize;
        let (line, row) = if self.wrap {
            self.step_rows((self.top, self.top_row), dy, width)
        } else {
            ((self.top + dy as usize).min(self.buf.lines.len() - 1), 0)
        };
        let text = &self.buf.lines[line];
        let starts = self.rows(line, width);
        let first = if self.wrap { display_col(text, starts[row]) } else { self.left };
        let mut col = char_at_col(text, first + x.saturating_sub(area.x + gutter) as usize);
        // Past the end of a wrapped row is its last character, not the next row
        if let Some(&next) = starts.get(row + 1) {
            col = col.min(next - 1);
        }
        Pos::new(line, col)
    }

    /// Up or down by screen rows when wrapping, by lines otherwise.
    fn move_rows(&mut self, delta: isize, select: bool, width: usize) {
        if !self.wrap {
            return self.buf.vertical(delta, select);
        }
        let cursor = self.buf.cursor;
        let starts = self.rows(cursor.line, width);
        let row = row_of(&starts, cursor.col);
        let text = &self.buf.lines[cursor.line];
        let x = self.wrap_x.unwrap_or_else(|| display_col(text, cursor.col) - display_col(text, starts[row]));
        let (line, row) = self.step_rows((cursor.line, row), delta, width);
        let starts = self.rows(line, width);
        let text = &self.buf.lines[line];
        let mut col = char_at_col(text, display_col(text, starts[row]) + x);
        if let Some(&next) = starts.get(row + 1) {
            col = col.min(next - 1);
        }
        self.buf.move_to(Pos::new(line, col), select);
        self.wrap_x = Some(x);
    }

    fn toggle_raw(&mut self) {
        // A match's place is in formatted lines or source lines, not both
        self.last_match = None;
        if self.raw {
            self.raw = false;
            self.fmt_sync = Some(self.top);
        } else {
            (self.top, self.top_row) = (self.formatted_top_src(), 0);
            self.raw = true;
        }
    }

    fn stop_editing(&mut self) {
        // While still editing, so the place kept is the text's
        self.keep_renderable();
        self.editing = false;
        self.buf.anchor = None;
        if self.kind != Kind::Text && !self.raw {
            self.last_match = None;
            self.fmt_sync = Some(self.top);
        }
    }

    /// Re-reads the file after it changed on disk, keeping the place.
    fn reload(&mut self) -> bool {
        let Ok(Loaded::Text(mut buf)) = Buffer::load(&self.path) else { return false };
        buf.cursor = buf.clamp(self.buf.cursor);
        if buf.read_only.is_some() {
            // start_editing is the only check, so leave edit mode rather than slip past it
            self.editing = false;
        }
        // Usually something was added at the end, so the colors above it stand
        let same = self.buf.lines.iter().zip(&buf.lines).take_while(|(a, b)| a == b).count();
        self.colors.replaced(&buf.lines, same);
        self.buf = buf;
        self.keep_renderable();
        self.top = self.top.min(self.buf.lines.len() - 1);
        self.top_row = 0;
        self.last_match = None;
        true
    }
}

enum Shown {
    Nothing,
    Note(PathBuf, String),
    Doc(Box<Doc>),
    Image(Box<ImageShown>),
    /// A folder's images as thumbnails
    Gallery(Box<Gallery>),
}

struct ImageShown {
    path: PathBuf,
    source: Source,
    label: Option<String>,
    bytes: u64,
    picture: Picture,
}

/// The left pane's history view: the commits that touched the folder, and the files
/// of one of them once it's opened.
struct History {
    repo: PathBuf,
    /// The file or folder whose history this is
    scope: PathBuf,
    /// For a file, its path in the repository
    file: Option<String>,
    commits: Vec<git::Commit>,
    selected: usize,
    scroll: usize,
    opened: Option<Opened>,
    /// Files show as diffs rather than as they were
    diff: bool,
}

struct Opened {
    commit: usize,
    /// Status letter (M, A, D, R; a space when listing all files) and path in the repo
    files: Vec<(char, String)>,
    /// Every file at the commit, not just the ones it changed
    all: bool,
    selected: usize,
    scroll: usize,
}

enum Prompt {
    Unsaved(After),
    Search(String),
    /// Yes or no to moving or deleting files
    Confirm(Change),
    /// A file's new name, being typed
    Rename(PathBuf, String),
    /// What to find in all the files, being typed
    Grep(String),
}

/// A change to files from the tree, waiting for a yes.
enum Change {
    Trash(Vec<PathBuf>),
    Move(Vec<PathBuf>, PathBuf),
}

/// Where dragged tree rows would land.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Row(usize),
    /// Below the last row: the folder at the top
    Root,
}

/// What to do once unsaved changes are dealt with.
enum After {
    Quit,
    Open(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    None,
    Divider,
    Select,
    /// A tree row pressed: a click if it's let go there, or else a move
    Tree {
        from: usize,
        double: bool,
        to: Option<Target>,
    },
    /// An image being moved around, last at this cell
    Pan(u16, u16),
}

pub struct App {
    tree: Tree,
    shown: Shown,
    pub focus: Focus,
    /// Keys go to the tree's filter box
    filtering: bool,
    /// Word wrap, for files opened from now on as well
    wrap: bool,
    /// The left pane shows git history instead of the files
    history: Option<History>,
    picker: Picker,
    /// The thumbnails an image was opened from, to go back to
    gallery_stash: Option<Box<Gallery>>,
    /// The left pane shows the matches of a search through all the files
    grep: Option<Grep>,
    /// The last thing searched for in all the files, to start from next time
    last_grep: String,
    tree_width: u16,
    prompt: Option<Prompt>,
    message: Option<(String, Instant)>,
    search: Option<String>,
    drag: Drag,
    last_click: Option<(Instant, u16, u16)>,
    // Where things were drawn last, for the mouse
    filter_area: Rect,
    tree_area: Rect,
    content_area: Rect,
    divider_x: u16,
    gutter: u16,
    last_refresh: Instant,
    pub quit: bool,
}

impl App {
    /// Opens a directory, showing its README if it has one, or a file in its directory.
    pub fn new(target: &Path) -> Result<Self> {
        let target = target.canonicalize().with_context(|| format!("can't open {}", target.display()))?;
        let (root, file) = if target.is_dir() {
            (target.clone(), find_readme(&target))
        } else {
            (target.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf), Some(target.clone()))
        };
        let mut app = Self {
            tree: Tree::new(root),
            shown: Shown::Nothing,
            focus: Focus::Tree,
            filtering: false,
            wrap: false,
            history: None,
            picker: Picker::halfblocks(),
            gallery_stash: None,
            grep: None,
            last_grep: String::new(),
            tree_width: 0,
            prompt: None,
            message: None,
            search: None,
            drag: Drag::None,
            last_click: None,
            filter_area: Rect::default(),
            tree_area: Rect::default(),
            content_area: Rect::default(),
            divider_x: 0,
            gutter: 0,
            last_refresh: Instant::now(),
            quit: false,
        };
        if let Some(file) = file {
            app.tree.reveal(&file);
            app.open(&file);
            if !target.is_dir() {
                app.focus = Focus::Content;
            }
        } else {
            // No README: a folder of images shows them
            app.show_gallery(&target);
        }
        Ok(app)
    }

    fn doc(&mut self) -> Option<&mut Doc> {
        match &mut self.shown {
            Shown::Doc(doc) => Some(doc),
            _ => None,
        }
    }

    fn dirty(&self) -> bool {
        matches!(&self.shown, Shown::Doc(doc) if doc.buf.dirty)
    }

    fn shown_path(&self) -> Option<&Path> {
        match &self.shown {
            Shown::Doc(doc) => Some(&doc.path),
            Shown::Image(image) => Some(&image.path),
            Shown::Gallery(gallery) => Some(&gallery.dir),
            Shown::Note(path, _) => Some(path),
            Shown::Nothing => None,
        }
    }

    /// Showing `path` as it is now, not a version of it from history.
    fn showing(&self, path: &Path) -> bool {
        let source = match &self.shown {
            Shown::Doc(doc) => Some(doc.source),
            Shown::Image(image) => Some(image.source),
            _ => None,
        };
        self.shown_path() == Some(path) && source != Some(Source::History)
    }

    /// How images are drawn, once the terminal has been asked. The image on screen, if
    /// any, is drawn again the new way.
    pub fn set_picker(&mut self, picker: Picker) {
        self.picker = picker;
        match &self.shown {
            Shown::Image(image) if image.source != Source::History => {
                let path = image.path.clone();
                self.open(&path);
            }
            Shown::Gallery(gallery) => {
                let dir = gallery.dir.clone();
                self.shown = Shown::Nothing;
                self.show_gallery(&dir);
            }
            _ => {}
        }
    }

    /// Typing goes into the file.
    pub fn editing(&self) -> bool {
        self.focus == Focus::Content && matches!(&self.shown, Shown::Doc(doc) if doc.editing)
    }

    fn say(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now()));
    }

    fn page(&self) -> usize {
        self.content_area.height.max(1) as usize
    }

    fn text_width(&self) -> usize {
        self.content_area.width.saturating_sub(self.gutter).max(1) as usize
    }

    /// Width of the formatted view, which has a column of margin each side.
    fn formatted_width(&self) -> usize {
        self.content_area.width.saturating_sub(2).max(1) as usize
    }

    /// Opens a file on disk, or inside an archive.
    fn open(&mut self, path: &Path) {
        self.shown = if path.exists() {
            if external::is_video(path) && path.is_file() {
                let size = fs::metadata(path).map_or(0, |m| m.len());
                Shown::Note(path.into(), format!("Video, {}. Click it, or press Enter, to play it.", human_size(size)))
            } else if picture::is_image(path) {
                let bytes = match fs::metadata(path) {
                    Ok(m) if !m.is_file() => Err(anyhow::anyhow!("not a regular file")),
                    Ok(m) if m.len() > 50 << 20 => Err(anyhow::anyhow!("too large ({})", human_size(m.len()))),
                    _ => fs::read(path).map_err(Into::into),
                };
                self.show_bytes(path, bytes, Source::Disk, None)
            } else {
                match Buffer::load(path) {
                    Ok(Loaded::Text(buf)) => Shown::Doc(Box::new(Doc::new(path, buf, self.wrap, Source::Disk, None))),
                    Ok(Loaded::Binary(size)) => Shown::Note(path.into(), format!("Binary file, {}", human_size(size))),
                    Ok(Loaded::TooBig(size)) => {
                        Shown::Note(path.into(), format!("Too large to open ({})", human_size(size)))
                    }
                    Err(e) => Shown::Note(path.into(), format!("Can't open: {e}")),
                }
            }
        } else if let Some((archive, entry)) = archive::split(path) {
            self.show_bytes(path, archive::read(&archive, &entry), Source::Archive, None)
        } else {
            Shown::Note(path.into(), "Can't open: it isn't there any more".into())
        };
    }

    /// What to show for a file read some other way than from disk: text, an image,
    /// or a note saying why it can't be shown.
    fn show_bytes(&self, path: &Path, bytes: Result<Vec<u8>>, source: Source, label: Option<String>) -> Shown {
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(e) => return Shown::Note(path.into(), format!("Can't open: {e}")),
        };
        let size = bytes.len() as u64;
        if picture::is_image(path) {
            let format = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
            // The format by the file's name, unless its first bytes say otherwise (TGA has
            // no signature, only its name)
            let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes));
            if let Ok(format) = image::ImageFormat::from_path(path) {
                reader.set_format(format);
            }
            let decoded = reader.with_guessed_format().map_err(image::ImageError::IoError).and_then(|r| r.decode());
            return match decoded {
                Ok(image) => Shown::Image(Box::new(ImageShown {
                    path: path.into(),
                    source,
                    label,
                    bytes: size,
                    picture: Picture::new(image, &format, &self.picker),
                })),
                Err(e) => Shown::Note(path.into(), format!("Can't show this image: {e}")),
            };
        }
        match Buffer::from_bytes(bytes) {
            Loaded::Text(mut buf) => {
                buf.read_only = buf.read_only.or(match source {
                    Source::Disk => None,
                    Source::Archive => Some("in an archive"),
                    Source::History => Some("from history"),
                });
                Shown::Doc(Box::new(Doc::new(path, buf, self.wrap, source, label)))
            }
            Loaded::Binary(size) => Shown::Note(path.into(), format!("Binary file, {}", human_size(size))),
            Loaded::TooBig(size) => Shown::Note(path.into(), format!("Too large to open ({})", human_size(size))),
        }
    }

    /// Opens `path`, asking first what to do with unsaved changes.
    fn request_open(&mut self, path: PathBuf) {
        if self.showing(&path) {
            return;
        }
        if self.dirty() {
            self.prompt = Some(Prompt::Unsaved(After::Open(path)));
        } else {
            self.open(&path);
        }
    }

    fn request_quit(&mut self) {
        if self.dirty() {
            self.prompt = Some(Prompt::Unsaved(After::Quit));
        } else {
            self.quit = true;
        }
    }

    fn proceed(&mut self, after: After) {
        match after {
            After::Quit => self.quit = true,
            After::Open(path) => self.open(&path),
        }
    }

    fn save(&mut self) -> bool {
        let Shown::Doc(doc) = &mut self.shown else { return false };
        if !doc.buf.dirty {
            self.say("No changes to save");
            return true;
        }
        let result = doc.buf.save(&doc.path);
        let name = file_name(&doc.path);
        match result {
            Ok(()) => {
                doc.disk = disk_stamp(&doc.path);
                doc.disk_changed = false;
                self.say(format!("Saved {name}"));
                true
            }
            Err(e) => {
                self.say(format!("Couldn't save {name}: {e}"));
                false
            }
        }
    }

    fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        let (wrap, page, width) = (self.wrap, self.page(), self.text_width());
        if let Some(doc) = self.doc() {
            doc.wrap = wrap;
            (doc.top_row, doc.left, doc.wrap_x) = (0, 0, None);
            if doc.editing {
                doc.follow_cursor(page, width);
            }
        }
        self.say(if wrap { "Word wrap on" } else { "Word wrap off" });
    }

    /// Picks up files changed by something else, such as an agent in the next pane.
    pub fn tick(&mut self) {
        if let Shown::Doc(doc) = &mut self.shown
            && doc.source == Source::Disk
        {
            let stamp = disk_stamp(&doc.path);
            if stamp.0.is_some() && stamp != doc.disk {
                doc.disk = stamp;
                // Unsaved changes stay, as does what's shown when the new contents can't be
                // (binary, too big, unreadable); either way the header says it's out of date
                if !doc.buf.dirty && doc.reload() {
                    doc.disk_changed = false;
                    self.say("Reloaded: the file changed on disk");
                } else {
                    doc.disk_changed = true;
                }
            }
        }
        // Not while a row is held down: the press, and a drag from it, go by its place in the rows
        if self.last_refresh.elapsed() >= TREE_REFRESH && !matches!(self.drag, Drag::Tree { .. }) {
            self.tree.rebuild();
            self.last_refresh = Instant::now();
        }
    }

    // Input

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            Event::Mouse(mouse) => self.mouse(mouse),
            Event::Paste(text) => self.paste(&text),
            _ => {}
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if let Some(prompt) = self.prompt.take() {
            return self.prompt_key(prompt, key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('q') if ctrl => return self.request_quit(),
            KeyCode::Char('s') if ctrl => {
                self.save();
                return;
            }
            KeyCode::Char('z') if alt => return self.toggle_wrap(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.filtering = false;
                self.focus = if self.focus == Focus::Tree { Focus::Content } else { Focus::Tree };
                return;
            }
            _ => {}
        }
        match self.focus {
            Focus::Tree if self.grep.is_some() => self.grep_key(key),
            Focus::Tree if self.history.is_some() => self.history_key(key),
            Focus::Tree if self.filtering => self.filter_key(key),
            Focus::Tree => self.tree_key(key),
            Focus::Content if self.editing() => self.edit_key(key),
            Focus::Content => self.view_key(key),
        }
    }

    fn prompt_key(&mut self, prompt: Prompt, key: KeyEvent) {
        match prompt {
            Prompt::Unsaved(after) => match key.code {
                KeyCode::Char('s' | 'y') => {
                    if self.save() {
                        self.proceed(after);
                    }
                }
                KeyCode::Char('d' | 'n') => {
                    if let Some(doc) = self.doc() {
                        doc.buf.dirty = false;
                    }
                    self.proceed(after);
                }
                KeyCode::Esc | KeyCode::Char('c') => {
                    // Put the tree back on the file that's still open
                    if let Some(path) = self.shown_path().map(Path::to_path_buf) {
                        self.tree.reveal(&path);
                    }
                }
                _ => self.prompt = Some(Prompt::Unsaved(after)),
            },
            Prompt::Search(mut query) => match key.code {
                KeyCode::Esc => self.search = None,
                KeyCode::Enter => {
                    self.search = (!query.is_empty()).then_some(query);
                    if self.search.is_some() {
                        self.count_matches();
                        self.find(true, true);
                    }
                }
                KeyCode::Backspace => {
                    query.pop();
                    self.search = (!query.is_empty()).then(|| query.clone());
                    self.prompt = Some(Prompt::Search(query));
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(c);
                    self.search = Some(query.clone());
                    self.prompt = Some(Prompt::Search(query));
                }
                _ => self.prompt = Some(Prompt::Search(query)),
            },
            Prompt::Confirm(change) => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.perform(change),
                _ => self.say("Left as it was"),
            },
            Prompt::Grep(mut query) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.start_grep(query),
                KeyCode::Backspace => {
                    query.pop();
                    self.prompt = Some(Prompt::Grep(query));
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(c);
                    self.prompt = Some(Prompt::Grep(query));
                }
                _ => self.prompt = Some(Prompt::Grep(query)),
            },
            Prompt::Rename(path, mut name) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.rename(path, &name),
                KeyCode::Backspace => {
                    name.pop();
                    self.prompt = Some(Prompt::Rename(path, name));
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    name.push(c);
                    self.prompt = Some(Prompt::Rename(path, name));
                }
                _ => self.prompt = Some(Prompt::Rename(path, name)),
            },
        }
    }

    fn tree_key(&mut self, key: KeyEvent) {
        let page = self.tree_area.height.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if self.tree.marks.is_empty() {
                    self.tree.anchor = self.tree.rows.get(self.tree.selected).map(|r| r.path.clone());
                }
                self.tree_move(if key.code == KeyCode::Up { -1 } else { 1 });
                self.tree.mark_range();
            }
            KeyCode::Up | KeyCode::Char('k') => self.tree_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.tree_move(1),
            KeyCode::PageUp => self.tree_move(-page),
            KeyCode::PageDown => self.tree_move(page),
            KeyCode::Home | KeyCode::Char('g') => self.tree_move(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.tree_move(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.tree_activate(key.code == KeyCode::Enter),
            KeyCode::Left | KeyCode::Char('h') => self.tree.collapse_or_parent(),
            KeyCode::Char('/') => {
                if self.tree.filter.is_none() {
                    self.tree.set_filter(Some(String::new()));
                }
                self.filtering = true;
            }
            KeyCode::Char(' ') => {
                self.tree.toggle_mark(self.tree.selected);
                self.tree_move(1);
            }
            KeyCode::Delete => self.request_trash(),
            KeyCode::F(2) => self.request_rename(),
            KeyCode::Esc if !self.tree.marks.is_empty() => self.tree.marks.clear(),
            KeyCode::Esc if self.tree.filter.is_some() => self.tree.set_filter(None),
            KeyCode::Char('.') => {
                self.tree.show_hidden = !self.tree.show_hidden;
                self.tree.refresh();
                self.say(if self.tree.show_hidden {
                    "Showing hidden and ignored files"
                } else {
                    "Hiding hidden and ignored files"
                });
            }
            KeyCode::Char('r') | KeyCode::F(5) => self.tree.refresh(),
            KeyCode::Char('H') => self.toggle_history(),
            KeyCode::Char('F') => self.prompt = Some(Prompt::Grep(self.last_grep.clone())),
            KeyCode::Char('q') => self.request_quit(),
            _ => {}
        }
        self.tree.keep_visible(self.tree_area.height as usize);
    }

    // Finding text in all the files

    fn start_grep(&mut self, query: String) {
        if query.is_empty() {
            return;
        }
        self.last_grep = query.clone();
        self.history = None;
        self.filtering = false;
        self.grep = Some(Grep::start(&self.tree.root, &query, self.tree.show_hidden));
        self.focus = Focus::Tree;
    }

    fn grep_key(&mut self, key: KeyEvent) {
        let page = self.tree_area.height.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.grep_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.grep_move(1),
            KeyCode::PageUp => self.grep_move(-page),
            KeyCode::PageDown => self.grep_move(page),
            KeyCode::Home | KeyCode::Char('g') | KeyCode::End | KeyCode::Char('G') => {
                let end = matches!(key.code, KeyCode::End | KeyCode::Char('G'));
                if let Some(grep) = &mut self.grep {
                    let last = grep.files.len().saturating_sub(1);
                    let last_hit = grep.files.get(last).map_or(0, |f| f.hits.len().saturating_sub(1));
                    grep.selected = if end { (last, last_hit) } else { (0, 0) };
                }
                self.grep_move(0);
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                self.grep_preview();
                self.focus = Focus::Content;
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                self.grep = None;
                self.say("Back to the files");
            }
            KeyCode::Char('F') => self.prompt = Some(Prompt::Grep(self.last_grep.clone())),
            KeyCode::Char('q') => self.request_quit(),
            _ => {}
        }
    }

    fn grep_move(&mut self, delta: isize) {
        let height = self.tree_area.height.max(1) as usize;
        let Some(grep) = &mut self.grep else { return };
        grep.step(delta);
        let row = grep.selected_row();
        // Keep the file's own line in view with its first match
        if row <= grep.scroll {
            grep.scroll = row.saturating_sub(1);
        } else if row >= grep.scroll + height {
            grep.scroll = row + 1 - height;
        }
        self.grep_preview();
    }

    /// Shows the selected match: its file at that line, with the matches lit.
    fn grep_preview(&mut self) {
        let Some((path, line, col, query)) = self
            .grep
            .as_ref()
            .and_then(|g| g.current().map(|(p, h)| (p.to_path_buf(), h.line, h.col, g.query.clone())))
        else {
            return;
        };
        if let Some(grep) = &mut self.grep {
            grep.shown = true;
        }
        if !self.showing(&path) {
            if self.dirty() {
                return self.say("Save or discard your changes first, to see other files");
            }
            self.open(&path);
        }
        self.search = Some(query);
        let (page, width) = (self.page(), self.text_width());
        if let Some(doc) = self.doc() {
            // A match is on a line of the text, so formatted views show the text
            doc.raw = true;
            doc.last_match = Some(Pos::new(line, col));
            (doc.top, doc.top_row) = (line.saturating_sub(page / 3), 0);
            let at = display_col(doc.buf.lines.get(line).map_or("", String::as_str), col);
            doc.left = if doc.wrap || at < width { 0 } else { at.saturating_sub(width / 3) };
        }
    }

    fn grep_click(&mut self, y: u16, double: bool) {
        let Some(grep) = &mut self.grep else { return };
        let Some(row) = y.checked_sub(self.tree_area.y).map(|dy| grep.scroll + dy as usize) else { return };
        let Some(selected) = grep.at_row(row) else { return };
        grep.selected = selected;
        self.grep_preview();
        if double {
            self.focus = Focus::Content;
        }
    }

    // History

    fn toggle_history(&mut self) {
        if self.history.take().is_some() {
            return self.say("Back to the files");
        }
        // The history of what's selected in the tree, or of what's open
        let picked = match self.focus {
            Focus::Tree => self.tree.rows.get(self.tree.selected).map(|r| r.path.clone()),
            Focus::Content => self.shown_path().map(Path::to_path_buf),
        };
        let scope = picked.filter(|p| p.exists()).unwrap_or_else(|| self.tree.root.clone());
        self.show_history(scope);
    }

    /// The commits that touched a file or folder.
    fn show_history(&mut self, scope: PathBuf) {
        let dir = if scope.is_dir() { scope.clone() } else { scope.parent().unwrap_or(&scope).to_path_buf() };
        let Some(repo) = git::repo_root(&dir) else {
            return self.say(format!("{} isn't in a git repository", file_name(&scope)));
        };
        // git gives the repository's real path, which may differ from ours by symlinks
        let scope = scope.canonicalize().unwrap_or(scope);
        let file = scope.is_file().then(|| scope.strip_prefix(&repo).unwrap_or(&scope).to_string_lossy().into_owned());
        match git::log(&repo, &scope, file.is_some()) {
            Ok(commits) if commits.is_empty() => self.say(format!("No commits for {} yet", file_name(&scope))),
            Ok(commits) => {
                self.history =
                    Some(History { repo, scope, file, commits, selected: 0, scroll: 0, opened: None, diff: false });
                // It takes the left pane, which find-in-files' results would otherwise keep
                self.grep = None;
                self.focus = Focus::Tree;
                self.filtering = false;
                self.history_preview();
            }
            Err(e) => self.say(format!("Couldn't read the history: {e}")),
        }
    }

    /// Shows the selected commit, or the selected file of the opened commit, as it was
    /// or as a diff.
    fn history_preview(&mut self) {
        // The history still opens, but a file with unsaved changes stays on screen
        if self.dirty() {
            return self.say("Save or discard your changes first, to see other files");
        }
        let Some(h) = &self.history else { return };
        let repo = h.repo.clone();
        let (path, bytes, label) = match &h.opened {
            None => {
                let c = &h.commits[h.selected];
                match &h.file {
                    // A file's history shows the file at each commit, or what the commit did to it
                    Some(file) => {
                        let then = c.path.as_deref().unwrap_or(file);
                        if h.diff {
                            let label = format!(" {then} · diff in {}", c.short);
                            (repo.join(format!("{then}.diff")), git::file_diff(&repo, c, then), label)
                        } else {
                            match git::show(&repo, &c.hash, then) {
                                Ok(bytes) => {
                                    (repo.join(then), Ok(bytes), format!(" {then} @ {} {}", c.short, c.subject))
                                }
                                // Not there at that commit (it was deleted, say) or too big to show:
                                // what the commit did to it instead, shown as the diff it is
                                Err(_) => {
                                    let label = format!(" {then} · diff in {}", c.short);
                                    (repo.join(format!("{then}.diff")), git::file_diff(&repo, c, then), label)
                                }
                            }
                        }
                    }
                    None => {
                        let label = format!(" {} {}", c.short, c.subject);
                        (repo.join(format!("{}.diff", c.short)), git::commit_text(&repo, &c.hash), label)
                    }
                }
            }
            Some(opened) => {
                let Some((status, file)) = opened.files.get(opened.selected) else { return };
                let c = &h.commits[opened.commit];
                if h.diff {
                    let label = format!(" {file} · diff in {}", c.short);
                    (repo.join(format!("{file}.diff")), git::file_diff(&repo, c, file), label)
                } else if *status == 'D' {
                    // A file the commit deleted, as it was just before
                    let parent = c.parent.clone().unwrap_or_default();
                    let label = format!(" {file} · before {} deleted it", c.short);
                    (repo.join(file), git::show(&repo, &parent, file), label)
                } else {
                    (repo.join(file), git::show(&repo, &c.hash, file), format!(" {file} @ {}", c.short))
                }
            }
        };
        self.shown = self.show_bytes(&path, bytes, Source::History, Some(label));
    }

    fn history_key(&mut self, key: KeyEvent) {
        let page = self.tree_area.height.max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.history_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.history_move(1),
            KeyCode::PageUp => self.history_move(-page),
            KeyCode::PageDown => self.history_move(page),
            KeyCode::Home | KeyCode::Char('g') => self.history_move(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.history_move(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.history_enter(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => self.history_back(),
            KeyCode::Esc => self.history_leave(),
            KeyCode::Char('d') => {
                if let Some(h) = &mut self.history {
                    h.diff = !h.diff;
                    let diff = h.diff;
                    self.history_preview();
                    self.say(if diff { "Showing what each commit changed" } else { "Showing files as they were" });
                }
            }
            KeyCode::Char('a') => self.history_all_files(),
            KeyCode::Char('H') => self.toggle_history(),
            KeyCode::Char('q') => self.request_quit(),
            _ => {}
        }
    }

    fn history_move(&mut self, delta: isize) {
        let height = self.tree_area.height.max(1) as usize;
        let Some(h) = &mut self.history else { return };
        let (selected, scroll, len) = match &mut h.opened {
            Some(o) => (&mut o.selected, &mut o.scroll, o.files.len()),
            None => (&mut h.selected, &mut h.scroll, h.commits.len()),
        };
        if len == 0 {
            return;
        }
        *selected = selected.saturating_add_signed(delta).min(len - 1);
        if *selected < *scroll {
            *scroll = *selected;
        } else if *selected >= *scroll + height {
            *scroll = *selected + 1 - height;
        }
        self.history_preview();
    }

    /// Opens the selected commit's files, or moves into the selected file.
    fn history_enter(&mut self) {
        let Some(h) = &mut self.history else { return };
        if h.opened.is_some() {
            self.focus = Focus::Content;
            return;
        }
        match git::changes(&h.repo, &h.commits[h.selected]) {
            Ok(changes) if changes.is_empty() => self.say("This commit changed no files"),
            Ok(changes) => {
                let files: Vec<(char, String)> = changes.into_iter().map(|c| (c.status, c.path)).collect();
                // In a file's history, start on that file
                let then = h.commits[h.selected].path.clone().or_else(|| h.file.clone());
                let start = then.and_then(|f| files.iter().position(|(_, p)| *p == f)).unwrap_or(0);
                h.opened = Some(Opened { commit: h.selected, files, all: false, selected: start, scroll: 0 });
                self.history_preview();
            }
            Err(e) => self.say(format!("Couldn't read the commit: {e}")),
        }
    }

    /// Back from a commit's files to the commits; from the commits, out to the folder
    /// around (up to the whole repository), and from there back to the files.
    fn history_back(&mut self) {
        let Some(h) = &mut self.history else { return };
        if h.opened.take().is_some() {
            return self.history_preview();
        }
        if h.scope != h.repo
            && let Some(parent) = h.scope.parent().map(Path::to_path_buf)
        {
            return self.show_history(parent);
        }
        self.history_leave();
    }

    /// Out of a commit's files, or else out of the history.
    fn history_leave(&mut self) {
        let Some(h) = &mut self.history else { return };
        if h.opened.take().is_some() {
            return self.history_preview();
        }
        self.history = None;
        self.say("Back to the files");
    }

    /// Switches an opened commit between the files it changed and all of its files.
    fn history_all_files(&mut self) {
        let Some(h) = &mut self.history else { return };
        let Some(opened) = &mut h.opened else { return };
        let commit = &h.commits[opened.commit];
        let changes = match git::changes(&h.repo, commit) {
            Ok(changes) => changes,
            Err(e) => return self.say(format!("Couldn't read the commit: {e}")),
        };
        let current = opened.files.get(opened.selected).map(|f| f.1.clone());
        opened.all = !opened.all;
        opened.files = if opened.all {
            let status: std::collections::HashMap<&str, char> =
                changes.iter().map(|c| (c.path.as_str(), c.status)).collect();
            let mut files: Vec<(char, String)> = match git::files(&h.repo, &commit.hash) {
                Ok(files) => files.into_iter().map(|f| (status.get(f.as_str()).copied().unwrap_or(' '), f)).collect(),
                Err(e) => return self.say(format!("Couldn't list the files: {e}")),
            };
            files.extend(changes.iter().filter(|c| c.status == 'D').map(|c| ('D', c.path.clone())));
            files.sort_by(|a, b| a.1.cmp(&b.1));
            files
        } else {
            changes.into_iter().map(|c| (c.status, c.path)).collect()
        };
        opened.selected = current.and_then(|p| opened.files.iter().position(|f| f.1 == p)).unwrap_or(0);
        opened.scroll = opened.selected.saturating_sub(self.tree_area.height as usize / 2);
        let all = opened.all;
        self.history_preview();
        self.say(if all { "All files at this commit" } else { "Files this commit changed" });
    }

    /// Typing in the tree's filter box.
    fn filter_key(&mut self, key: KeyEvent) {
        let page = self.tree_area.height.max(1) as isize;
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.filtering = false;
                self.tree.set_filter(None);
            }
            KeyCode::Enter => {
                self.filtering = false;
                if self.tree.rows.get(self.tree.selected).is_some_and(|r| !r.is_dir || r.archive) {
                    self.tree_activate(true);
                }
            }
            KeyCode::Up => self.tree_move(-1),
            KeyCode::Down => self.tree_move(1),
            KeyCode::PageUp => self.tree_move(-page),
            KeyCode::PageDown => self.tree_move(page),
            KeyCode::Backspace | KeyCode::Char(_) => {
                let mut query = self.tree.filter.clone().unwrap_or_default();
                match key.code {
                    KeyCode::Char(c) if plain => query.push(c),
                    KeyCode::Backspace => {
                        query.pop();
                    }
                    _ => return,
                }
                self.tree.set_filter(Some(query));
                // The first file that matches, ready for Enter
                if let Some(i) = self.tree.rows.iter().position(|r| !r.is_dir) {
                    self.tree.selected = i;
                }
                self.tree.scroll = 0;
            }
            _ => {}
        }
        self.tree.keep_visible(self.tree_area.height as usize);
    }

    fn tree_move(&mut self, delta: isize) {
        if self.tree.rows.is_empty() {
            return;
        }
        self.tree.selected = self.tree.selected.saturating_add_signed(delta).min(self.tree.rows.len() - 1);
        self.preview();
    }

    /// Shows the selected file as the selection moves, unless that would drop unsaved changes.
    fn preview(&mut self) {
        let Some(row) = self.tree.rows.get(self.tree.selected) else { return };
        let (is_dir, path) = (row.is_dir, row.path.clone());
        if is_dir {
            // A folder of images shows its thumbnails
            return self.show_gallery(&path);
        }
        if !self.dirty() && !self.showing(&path) {
            self.open(&path);
        }
    }

    /// Shows the thumbnails of a folder's images, if it has any.
    fn show_gallery(&mut self, dir: &Path) {
        if self.dirty() || self.showing(dir) || !dir.is_dir() {
            return;
        }
        // Back in the folder an image was opened from: its thumbnails are still there
        if let Some(gallery) = self.gallery_stash.take_if(|g| g.dir == dir) {
            self.shown = Shown::Gallery(gallery);
            return;
        }
        let images = gallery::images_in(dir);
        if !images.is_empty() {
            self.shown = Shown::Gallery(Box::new(Gallery::new(dir.to_path_buf(), images)));
        }
    }

    /// Opens the selected thumbnail's image, keeping the thumbnails to go back to.
    fn open_from_gallery(&mut self) {
        if !matches!(self.shown, Shown::Gallery(_)) {
            return;
        }
        let Shown::Gallery(gallery) = std::mem::replace(&mut self.shown, Shown::Nothing) else { return };
        let path = gallery.images[gallery.selected].clone();
        self.gallery_stash = Some(gallery);
        self.tree.reveal(&path);
        self.open(&path);
        self.focus = Focus::Content;
    }

    /// From an image back to the thumbnails it was opened from.
    fn back_to_gallery(&mut self) -> bool {
        let Shown::Image(image) = &self.shown else { return false };
        let Some(dir) = image.path.parent().map(Path::to_path_buf) else { return false };
        let Some(gallery) = self.gallery_stash.take_if(|g| g.dir == dir) else { return false };
        self.tree.reveal(&dir);
        self.shown = Shown::Gallery(gallery);
        true
    }

    fn tree_activate(&mut self, enter: bool) {
        let Some(row) = self.tree.rows.get(self.tree.selected) else { return };
        let (is_dir, archive, open, path) = (row.is_dir, row.archive, self.tree.is_open(row), row.path.clone());
        if !is_dir {
            self.request_open(path.clone());
            self.focus = Focus::Content;
            self.hand_off(&path);
        } else if archive && self.tree.filter.is_some() {
            // The filter doesn't look inside archives, so open it in the full tree
            self.tree.set_filter(None);
            if let Some(i) = self.tree.rows.iter().position(|r| r.path == path) {
                self.tree.toggle(i);
            }
        } else if enter || !open {
            self.tree.toggle(self.tree.selected);
        } else {
            self.tree_move(1);
        }
    }

    fn view_key(&mut self, key: KeyEvent) {
        if let Shown::Gallery(gallery) = &mut self.shown {
            let (across, rows) = (gallery.per_row as isize, gallery.rows_shown.max(1) as isize);
            match key.code {
                // From the first of a row, back to the tree
                KeyCode::Left | KeyCode::Char('h') if gallery.selected % gallery.per_row.max(1) == 0 => {
                    self.focus = Focus::Tree;
                }
                KeyCode::Left | KeyCode::Char('h') => gallery.select(-1),
                KeyCode::Right | KeyCode::Char('l') => gallery.select(1),
                KeyCode::Up | KeyCode::Char('k') => gallery.select(-across),
                KeyCode::Down | KeyCode::Char('j') => gallery.select(across),
                KeyCode::PageUp => gallery.select(-across * rows),
                KeyCode::PageDown => gallery.select(across * rows),
                KeyCode::Home | KeyCode::Char('g') => gallery.select(isize::MIN / 2),
                KeyCode::End | KeyCode::Char('G') => gallery.select(isize::MAX / 2),
                KeyCode::Char('+' | '=') => gallery.resize(25),
                KeyCode::Char('-' | '_') => gallery.resize(-25),
                KeyCode::Enter => self.open_from_gallery(),
                KeyCode::Char('q') => self.request_quit(),
                _ => {}
            }
            return;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Backspace) && self.back_to_gallery() {
            return;
        }
        if let Shown::Image(image) = &mut self.shown {
            let picture = &mut image.picture;
            match key.code {
                KeyCode::Char('+' | '=') => picture.zoom_by(1.25, None),
                KeyCode::Char('-' | '_') => picture.zoom_by(0.8, None),
                KeyCode::Char('0') => picture.reset(),
                KeyCode::Left | KeyCode::Char('h') if picture.more_left() => picture.pan(-0.25, 0.0),
                KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Tree,
                KeyCode::Right | KeyCode::Char('l') => picture.pan(0.25, 0.0),
                KeyCode::Up | KeyCode::Char('k') => picture.pan(0.0, -0.25),
                KeyCode::Down | KeyCode::Char('j') => picture.pan(0.0, 0.25),
                KeyCode::Char('q') => self.request_quit(),
                _ => {}
            }
            return;
        }
        let (page, width) = (self.page(), self.text_width());
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => return self.request_quit(),
            KeyCode::Char('e' | 'i') | KeyCode::Enter => return self.start_editing(None),
            KeyCode::Char('/') => return self.prompt = Some(Prompt::Search(String::new())),
            KeyCode::Char('f') if ctrl => return self.prompt = Some(Prompt::Search(String::new())),
            KeyCode::Char('n') => return self.find(true, false),
            KeyCode::Char('N') => return self.find(false, false),
            KeyCode::Char('y') => return self.copy_selection(),
            KeyCode::Char('c') if ctrl => return self.copy_selection(),
            KeyCode::Char('w') => return self.toggle_wrap(),
            KeyCode::Char('H') => return self.toggle_history(),
            KeyCode::Char('F') => return self.prompt = Some(Prompt::Grep(self.last_grep.clone())),
            KeyCode::Esc => {
                self.search = None;
                if let Some(doc) = self.doc() {
                    doc.buf.anchor = None;
                }
                return;
            }
            _ => {}
        }
        // Nothing here uses ← itself, so it goes back to the tree
        if matches!(key.code, KeyCode::Left | KeyCode::Char('h')) && !matches!(self.shown, Shown::Doc(_)) {
            self.focus = Focus::Tree;
            return;
        }
        let Some(doc) = self.doc() else { return };
        let p = page as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => doc.scroll(-1, page, width),
            KeyCode::Down | KeyCode::Char('j') => doc.scroll(1, page, width),
            KeyCode::PageUp | KeyCode::Char('b') => doc.scroll(-p, page, width),
            KeyCode::PageDown | KeyCode::Char(' ') => doc.scroll(p, page, width),
            KeyCode::Home | KeyCode::Char('g') => doc.scroll_to(false, page, width),
            KeyCode::End | KeyCode::Char('G') => doc.scroll_to(true, page, width),
            KeyCode::Left | KeyCode::Char('h') if doc.scrolled_sideways() => doc.scroll_sideways(-SIDEWAYS),
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Tree,
            KeyCode::Right | KeyCode::Char('l') => doc.scroll_sideways(SIDEWAYS),
            KeyCode::Char('m') if doc.kind != Kind::Text => doc.toggle_raw(),
            _ => {}
        }
    }

    fn start_editing(&mut self, at: Option<Pos>) {
        let (page, width) = (self.page(), self.text_width());
        let Shown::Doc(doc) = &mut self.shown else { return };
        if let Some(why) = doc.buf.read_only {
            return self.say(format!("Can't edit this file: {why}"));
        }
        if doc.formatted() {
            (doc.top, doc.top_row) = (doc.formatted_top_src(), 0);
            doc.last_match = None;
        }
        doc.editing = true;
        let cursor = doc.buf.cursor;
        let on_screen = cursor.line >= doc.top && cursor.line < doc.top + page;
        let pos = at.unwrap_or(if on_screen { cursor } else { Pos::new(doc.top, 0) });
        doc.buf.move_to(pos, false);
        doc.follow_cursor(page, width);
        self.focus = Focus::Content;
    }

    fn edit_key(&mut self, key: KeyEvent) {
        let (page, width) = (self.page(), self.text_width());
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if let (true, KeyCode::Char(c)) = (ctrl, key.code) {
            return self.edit_command(c.to_ascii_lowercase(), shift);
        }
        let Some(doc) = self.doc() else { return };
        if !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown) {
            doc.wrap_x = None;
        }
        let rows = page as isize;
        match key.code {
            KeyCode::Esc => return doc.stop_editing(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                doc.buf.insert(c.encode_utf8(&mut [0; 4]))
            }
            KeyCode::Enter => doc.buf.newline(),
            KeyCode::Backspace => doc.buf.backspace(),
            KeyCode::Delete => doc.buf.delete(),
            KeyCode::Left => doc.buf.left(shift, ctrl),
            KeyCode::Right => doc.buf.right(shift, ctrl),
            KeyCode::Up => doc.move_rows(-1, shift, width),
            KeyCode::Down => doc.move_rows(1, shift, width),
            KeyCode::PageUp => doc.move_rows(-rows, shift, width),
            KeyCode::PageDown => doc.move_rows(rows, shift, width),
            KeyCode::Home if ctrl => doc.buf.move_to(Pos::default(), shift),
            KeyCode::End if ctrl => doc.buf.move_to(doc.buf.end(), shift),
            KeyCode::Home => doc.buf.home(shift),
            KeyCode::End => doc.buf.line_end(shift),
            _ => return,
        }
        doc.follow_cursor(page, width);
    }

    fn edit_command(&mut self, c: char, shift: bool) {
        let (page, width) = (self.page(), self.text_width());
        match c {
            'c' => self.copy_selection(),
            'x' => {
                let cut = self.doc().and_then(|doc| doc.buf.cut());
                match cut {
                    Some(text) => {
                        clipboard::copy(&text, false);
                        self.say(format!("Cut {} characters", text.chars().count()));
                    }
                    None => self.say("Nothing selected"),
                }
            }
            'v' => match clipboard::paste(false) {
                Some(text) => self.insert_text(&text),
                None => self.say("Nothing to paste"),
            },
            'f' => self.prompt = Some(Prompt::Search(String::new())),
            'z' | 'y' | 'a' => {
                let Some(doc) = self.doc() else { return };
                let done = match c {
                    'z' if !shift => doc.buf.undo(),
                    'z' | 'y' => doc.buf.redo(),
                    _ => {
                        doc.buf.select_all();
                        true
                    }
                };
                doc.follow_cursor(page, width);
                if !done {
                    self.say(if c == 'z' && !shift { "Nothing to undo" } else { "Nothing to redo" });
                }
            }
            _ => {}
        }
    }

    fn insert_text(&mut self, text: &str) {
        let (page, width) = (self.page(), self.text_width());
        if let Some(doc) = self.doc() {
            doc.buf.insert(text);
            doc.follow_cursor(page, width);
        }
    }

    /// Text pasted through the terminal (bracketed paste).
    fn paste(&mut self, text: &str) {
        let first_line = text.lines().next().unwrap_or("");
        match &mut self.prompt {
            // Into the search box, not the file behind it; a search is one line
            Some(Prompt::Search(query)) => {
                query.push_str(first_line);
                self.search = (!query.is_empty()).then(|| query.clone());
            }
            Some(Prompt::Rename(_, name) | Prompt::Grep(name)) => name.push_str(first_line),
            Some(Prompt::Unsaved(_) | Prompt::Confirm(_)) => {}
            None if self.focus == Focus::Tree && self.filtering => {
                let query = format!("{}{first_line}", self.tree.filter.clone().unwrap_or_default());
                self.tree.set_filter(Some(query));
            }
            None => {
                if self.editing() {
                    self.insert_text(text);
                } else if matches!(self.shown, Shown::Doc(_)) {
                    self.say("Press e to edit before pasting");
                }
            }
        }
    }

    fn copy_selection(&mut self) {
        let text = match &self.shown {
            Shown::Doc(doc) => doc.buf.selected_text(),
            _ => None,
        };
        match text {
            Some(text) => {
                clipboard::copy(&text, false);
                self.say(format!("Copied {} characters", text.chars().count()));
            }
            None => self.say("Nothing selected; drag across text to select it"),
        }
    }

    fn count_matches(&mut self) {
        let Some(query) = self.search.clone() else { return };
        let Some(doc) = self.doc() else { return };
        let count: usize = if doc.formatted() {
            let r = doc.rendered.as_ref().map(|(_, r)| r);
            r.map_or(0, |r| (0..r.lines.len()).map(|i| find_all(&r.text(i), &query).len()).sum())
        } else {
            doc.buf.lines.iter().map(|l| find_all(l, &query).len()).sum()
        };
        self.say(match count {
            0 => format!("No matches for “{query}”"),
            1 => "1 match".to_string(),
            n => format!("{n} matches · n next, N previous"),
        });
    }

    /// Jumps to the next (or previous) match of the search. A `fresh` search starts
    /// from the top of the screen.
    fn find(&mut self, forward: bool, fresh: bool) {
        let Some(query) = self.search.clone() else {
            return self.say("Press / to search");
        };
        let (page, width, fmt_width) = (self.page(), self.text_width(), self.formatted_width());
        let Some(doc) = self.doc() else { return };
        let formatted = doc.formatted();
        let top = if formatted { doc.fmt_top } else { doc.top };
        let last = doc.last_match.filter(|m| !fresh && m.line >= top && m.line < top + page);
        let (from, inclusive) = if doc.editing {
            let from = if forward { doc.buf.cursor } else { doc.buf.selection().map_or(doc.buf.cursor, |s| s.0) };
            // Forward from the end of the selected match, one starting right there is the next
            (from, forward || doc.buf.selection().is_none())
        } else {
            match last {
                Some(m) => (m, false),
                None => (Pos::new(if formatted { 0 } else { top }, 0), forward),
            }
        };
        let found = if formatted {
            let r = doc.rendered.as_ref().map(|(_, r)| r);
            let lines: Vec<String> = r.map_or(Vec::new(), |r| (0..r.lines.len()).map(|i| r.text(i)).collect());
            search(&lines, &query, (from.line.max(if last.is_none() { top } else { 0 }), from.col), forward, inclusive)
        } else {
            search(&doc.buf.lines, &query, (from.line, from.col), forward, inclusive)
        };
        let Some((line, start, end)) = found else {
            return self.say(format!("No matches for “{query}”"));
        };
        doc.last_match = Some(Pos::new(line, start));
        if formatted {
            let (pinned, gutter) = doc.rendered.as_ref().map_or((0, 0), |(_, r)| (r.pinned, r.gutter));
            let body = page.saturating_sub(pinned);
            if line >= pinned && (line < doc.fmt_top || line >= doc.fmt_top + body) {
                doc.fmt_top = line.saturating_sub(body / 3).max(pinned);
            }
            // Tables also scroll sideways to it
            let text = doc.rendered.as_ref().map(|(_, r)| r.text(line)).unwrap_or_default();
            let col = str_width(&text.chars().take(start).collect::<String>());
            let room = fmt_width.saturating_sub(gutter);
            if col >= gutter && (col < gutter + doc.fmt_left || col >= gutter + doc.fmt_left + room) {
                doc.fmt_left = (col - gutter).saturating_sub(room / 3);
            }
        } else if doc.editing {
            doc.buf.move_to(Pos::new(line, start), false);
            doc.buf.move_to(Pos::new(line, end), true);
            doc.follow_cursor(page, width);
        } else {
            if line < doc.top || line >= doc.top + page {
                (doc.top, doc.top_row) = (line.saturating_sub(page / 3), 0);
            }
            let col = display_col(&doc.buf.lines[line], start);
            if !doc.wrap && (col < doc.left || col >= doc.left + width) {
                doc.left = col.saturating_sub(width / 3);
            }
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);
        let at = Position::new(x, y);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.prompt.is_some() {
                    return;
                }
                let double =
                    self.last_click.is_some_and(|(t, cx, cy)| t.elapsed() < DOUBLE_CLICK && (cx, cy) == (x, y));
                self.last_click = Some((Instant::now(), x, y));
                if x == self.divider_x && y < self.content_area.bottom() {
                    self.drag = Drag::Divider;
                } else if self.grep.is_some() && x < self.divider_x && y < self.content_area.bottom() {
                    self.focus = Focus::Tree;
                    self.grep_click(y, double);
                } else if self.history.is_some() && x < self.divider_x && y < self.content_area.bottom() {
                    self.focus = Focus::Tree;
                    self.history_click(y, double);
                } else if self.filter_area.contains(at) {
                    self.focus = Focus::Tree;
                    self.filtering = true;
                } else if self.tree_area.contains(at) {
                    self.focus = Focus::Tree;
                    self.filtering = false;
                    self.tree_press(y, double, mouse.modifiers);
                } else if self.content_area.contains(at) {
                    self.focus = Focus::Content;
                    self.content_click(x, y, double, mouse.modifiers.contains(KeyModifiers::SHIFT));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.drag {
                Drag::Divider => self.tree_width = x.max(8),
                Drag::Select => self.drag_select(x, y),
                Drag::Tree { from, double, .. } => self.tree_drag(from, double, y),
                Drag::Pan(px, py) => {
                    if let Shown::Image(image) = &mut self.shown {
                        image.picture.drag(x as i32 - px as i32, y as i32 - py as i32);
                    }
                    self.drag = Drag::Pan(x, y);
                }
                Drag::None => {}
            },
            MouseEventKind::Up(MouseButton::Left) => match std::mem::replace(&mut self.drag, Drag::None) {
                Drag::Select => {
                    // Like herdr's copy_on_select: a mouse selection is copied right away
                    let text = match &self.shown {
                        Shown::Doc(doc) => doc.buf.selected_text(),
                        _ => None,
                    };
                    if let Some(text) = text {
                        clipboard::copy(&text, true);
                        self.say(format!("Copied {} characters", text.chars().count()));
                    }
                }
                Drag::Tree { from, double, to: None } => self.tree_click(from, double),
                Drag::Tree { from, to: Some(target), .. } => self.tree_drop(from, target),
                _ => {}
            },
            MouseEventKind::Down(MouseButton::Middle)
                if self.prompt.is_none() && self.editing() && self.content_area.contains(at) =>
            {
                let (area, gutter, width) = (self.content_area, self.gutter, self.text_width());
                if let Some(doc) = self.doc() {
                    let pos = doc.pos_at(area, gutter, x, y, width);
                    doc.buf.move_to(pos, false);
                }
                if let Some(text) = clipboard::paste(true) {
                    self.insert_text(&text);
                }
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                let sideways = matches!(mouse.kind, MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight)
                    || mouse.modifiers.contains(KeyModifiers::SHIFT);
                let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft) {
                    -WHEEL
                } else {
                    WHEEL
                };
                if self.tree_area.contains(at) && self.grep.is_some() {
                    let height = self.tree_area.height as usize;
                    if let Some(grep) = &mut self.grep {
                        grep.scroll = grep.scroll.saturating_add_signed(delta).min(grep.rows().saturating_sub(height));
                    }
                } else if self.tree_area.contains(at) && self.history.is_some() {
                    let height = self.tree_area.height as usize;
                    if let Some(h) = &mut self.history {
                        let (scroll, len) = match &mut h.opened {
                            Some(o) => (&mut o.scroll, o.files.len()),
                            None => (&mut h.scroll, h.commits.len()),
                        };
                        *scroll = scroll.saturating_add_signed(delta).min(len.saturating_sub(height));
                    }
                } else if self.tree_area.contains(at) {
                    let max = self.tree.rows.len().saturating_sub(self.tree_area.height as usize);
                    self.tree.scroll = self.tree.scroll.saturating_add_signed(delta).min(max);
                } else if self.content_area.contains(at) {
                    let (page, width) = (self.page(), self.text_width());
                    if let Shown::Gallery(gallery) = &mut self.shown {
                        if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                            gallery.resize(if delta < 0 { 25 } else { -25 });
                        } else {
                            gallery.scroll(delta.signum());
                        }
                    } else if let Shown::Image(image) = &mut self.shown {
                        // The wheel zooms, about the pointer
                        image.picture.zoom_by(if delta < 0 { 1.25 } else { 0.8 }, Some((x, y)));
                    } else if let Some(doc) = self.doc() {
                        if sideways {
                            doc.scroll_sideways(delta * 2);
                        } else {
                            doc.scroll(delta, page, width);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// A click in the history pane: its title goes back, a row selects (or with a
    /// double-click, opens).
    fn history_click(&mut self, y: u16, double: bool) {
        if y < self.tree_area.y {
            return self.history_back();
        }
        let Some(h) = &self.history else { return };
        let (scroll, selected) = match &h.opened {
            Some(o) => (o.scroll, o.selected),
            None => (h.scroll, h.selected),
        };
        let row = scroll + (y - self.tree_area.y) as usize;
        if row != selected {
            self.history_move(row as isize - selected as isize);
        }
        if double {
            self.history_enter();
        }
    }

    /// The tree row on screen line `y`, if there is one.
    fn row_at(&self, y: u16) -> Option<usize> {
        let i = self.tree.scroll + y.checked_sub(self.tree_area.y)? as usize;
        (y < self.tree_area.bottom() && i < self.tree.rows.len()).then_some(i)
    }

    /// A button going down on the tree. Ctrl marks the row and Shift marks a range;
    /// otherwise letting go decides between a click and a drag.
    fn tree_press(&mut self, y: u16, double: bool, modifiers: KeyModifiers) {
        let Some(i) = self.row_at(y) else {
            self.tree.marks.clear();
            return;
        };
        let before = self.tree.rows.get(self.tree.selected).map(|r| r.path.clone());
        self.tree.selected = i;
        if modifiers.contains(KeyModifiers::CONTROL) {
            self.tree.toggle_mark(i);
        } else if modifiers.contains(KeyModifiers::SHIFT) {
            // With nothing marked, the range starts from the row selected before, as with
            // Shift+↑↓, rather than from wherever an earlier range started
            if self.tree.marks.is_empty() {
                self.tree.anchor = before;
            }
            self.tree.mark_range();
        } else {
            self.drag = Drag::Tree { from: i, double, to: None };
        }
    }

    fn tree_drag(&mut self, from: usize, double: bool, y: u16) {
        // Near the edges the tree scrolls
        let (top, bottom) = (self.tree_area.y, self.tree_area.bottom());
        if y < top {
            self.tree.scroll = self.tree.scroll.saturating_sub(1);
        } else if y >= bottom {
            let max = self.tree.rows.len().saturating_sub(self.tree_area.height as usize);
            self.tree.scroll = (self.tree.scroll + 1).min(max);
        }
        let to = match self.row_at(y.clamp(top, bottom.saturating_sub(1))) {
            Some(i) if i == from => None,
            Some(i) => Some(Target::Row(i)),
            None => Some(Target::Root),
        };
        self.drag = Drag::Tree { from, double, to };
        if let Some(target) = to {
            let items = self.items_from(from);
            match self.drop_dir(target) {
                Some(dir) => self.say(format!("Drop to move {} into {}", describe(&items), self.dir_name(&dir))),
                None => self.say("Can't move things there"),
            }
        }
    }

    /// What dragging row `from` carries: the marked rows if it's one of them, or else it.
    fn items_from(&self, from: usize) -> Vec<PathBuf> {
        match self.tree.rows.get(from) {
            Some(row) if self.tree.marks.contains(&row.path) => self.tree.marked_or_selected(),
            Some(row) => vec![row.path.clone()],
            None => Vec::new(),
        }
    }

    /// The folder a drop lands in: a folder's row, a file's folder, or the top.
    fn drop_dir(&self, target: Target) -> Option<PathBuf> {
        let dir = match target {
            Target::Root => self.tree.root.clone(),
            Target::Row(i) => {
                let row = self.tree.rows.get(i)?;
                if row.is_dir && !row.archive { row.path.clone() } else { row.path.parent()?.to_path_buf() }
            }
        };
        // Folders inside archives aren't on disk
        dir.is_dir().then_some(dir)
    }

    fn tree_drop(&mut self, from: usize, target: Target) {
        let Some(dir) = self.drop_dir(target) else { return self.say("Can't move things there") };
        let items: Vec<PathBuf> =
            self.items_from(from).into_iter().filter(|p| p.parent() != Some(dir.as_path())).collect();
        if items.is_empty() {
            return self.say("Already there");
        }
        if let Some(problem) = self.unchangeable(&items) {
            return self.say(problem);
        }
        self.prompt = Some(Prompt::Confirm(Change::Move(items, dir)));
    }

    /// Why these can't be moved, renamed or deleted, if they can't.
    fn unchangeable(&self, items: &[PathBuf]) -> Option<String> {
        if items.iter().any(|p| p.symlink_metadata().is_err()) {
            return Some("Files inside archives can't be changed".into());
        }
        if items.contains(&self.tree.root) {
            return Some("The folder at the top can't be changed here".into());
        }
        if let Shown::Doc(doc) = &self.shown
            && doc.buf.dirty
            && items.iter().any(|p| doc.path.starts_with(p))
        {
            return Some(format!("Save or discard the changes to {} first", file_name(&doc.path)));
        }
        None
    }

    fn request_trash(&mut self) {
        let items = self.tree.marked_or_selected();
        if items.is_empty() {
            return;
        }
        match self.unchangeable(&items) {
            Some(problem) => self.say(problem),
            None => self.prompt = Some(Prompt::Confirm(Change::Trash(items))),
        }
    }

    fn request_rename(&mut self) {
        let Some(path) = self.tree.rows.get(self.tree.selected).map(|r| r.path.clone()) else { return };
        match self.unchangeable(std::slice::from_ref(&path)) {
            Some(problem) => self.say(problem),
            None => self.prompt = Some(Prompt::Rename(path.clone(), file_name(&path))),
        }
    }

    fn perform(&mut self, change: Change) {
        self.gallery_stash = None;
        match change {
            Change::Trash(items) => match fileops::trash(&items) {
                Ok(()) => {
                    let gone = self.shown_path().filter(|shown| items.iter().any(|p| shown.starts_with(p)));
                    if let Some(path) = gone.map(Path::to_path_buf) {
                        self.shown = Shown::Note(path, "Moved to the trash".into());
                    }
                    self.say(format!("Moved {} to the trash", describe(&items)));
                }
                Err(e) => self.say(format!("{e:#}")),
            },
            Change::Move(items, dir) => {
                let (moved, problem) = fileops::move_into(&items, &dir);
                for (from, to) in &moved {
                    self.tree.renamed(from, to);
                    self.follow(from, to);
                }
                match problem {
                    Some(e) if moved.is_empty() => self.say(format!("{e}")),
                    Some(e) => self.say(format!("Moved {}, then stopped: {e}", describe(&items[..moved.len()]))),
                    None => self.say(format!("Moved {} into {}", describe(&items), self.dir_name(&dir))),
                }
            }
        }
        self.tree.marks.clear();
        self.tree.refresh();
    }

    fn rename(&mut self, path: PathBuf, name: &str) {
        match fileops::rename(&path, name) {
            Ok(to) => {
                self.gallery_stash = None;
                self.tree.renamed(&path, &to);
                self.follow(&path, &to);
                self.tree.marks.clear();
                self.tree.refresh();
                self.tree.reveal(&to);
                self.say(format!("Renamed to {name}"));
            }
            Err(e) => self.say(format!("{e:#}")),
        }
    }

    /// Keeps showing what was shown after it (or a folder it's in) moved.
    fn follow(&mut self, from: &Path, to: &Path) {
        let Some(rest) = self.shown_path().and_then(|shown| shown.strip_prefix(from).ok()) else { return };
        let new = if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) };
        let from_history = matches!(&self.shown, Shown::Doc(doc) if doc.source != Source::Disk);
        if from_history {
            return;
        }
        if let Shown::Doc(doc) = &mut self.shown {
            doc.disk = disk_stamp(&new);
            doc.path = new;
        } else if let Shown::Gallery(_) = &self.shown {
            self.shown = Shown::Nothing;
            self.show_gallery(&new);
        } else {
            self.open(&new);
        }
    }

    fn dir_name(&self, dir: &Path) -> String {
        let rel = relative(dir, &self.tree.root);
        if rel.is_empty() { tilde(dir) } else { format!("{rel}/") }
    }

    fn tree_click(&mut self, i: usize, double: bool) {
        let Some(row) = self.tree.rows.get(i) else { return };
        let (is_dir, path) = (row.is_dir, row.path.clone());
        self.tree.selected = i;
        if is_dir {
            // The second click of a double-click would fold it straight back
            if !double {
                self.tree.toggle(i);
            }
            self.show_gallery(&path);
        } else {
            self.request_open(path.clone());
            if double {
                self.focus = Focus::Content;
            } else {
                self.hand_off(&path);
            }
        }
    }

    /// Opens a web page in the browser, or a video in the video player, as well as
    /// showing it here. Only on a click or Enter, not while the selection moves past,
    /// and only for files on disk.
    fn hand_off(&mut self, path: &Path) {
        let program = if external::is_web_page(path) {
            "the browser"
        } else if external::is_video(path) {
            "the video player"
        } else {
            return;
        };
        if !path.is_file() {
            return;
        }
        match external::open(path) {
            Ok(()) => self.say(format!("Opened {} in {program}", file_name(path))),
            Err(e) => self.say(format!("Couldn't open {}: {e}", file_name(path))),
        }
    }

    fn content_click(&mut self, x: u16, y: u16, double: bool, shift: bool) {
        if let Shown::Image(image) = &mut self.shown {
            // A double-click fits it back in; otherwise a drag moves it around
            if double {
                image.picture.reset();
            } else {
                self.drag = Drag::Pan(x, y);
            }
            return;
        }
        if let Shown::Gallery(gallery) = &mut self.shown {
            if let Some(i) = gallery.at(x, y) {
                gallery.selected = i;
                self.open_from_gallery();
            }
            return;
        }
        let (area, gutter, width) = (self.content_area, self.gutter, self.text_width());
        let Some(doc) = self.doc() else { return };
        if doc.formatted() {
            if double {
                let dy = (y - area.y) as usize;
                let pinned = doc.pinned();
                let row = if dy < pinned { dy } else { doc.fmt_top + dy - pinned };
                let line = doc.rendered.as_ref().and_then(|(_, r)| r.src.get(row).copied());
                if let Some(line) = line {
                    self.start_editing(Some(Pos::new(line, 0)));
                }
            }
            return;
        }
        doc.wrap_x = None;
        let pos = doc.pos_at(area, gutter, x, y, width);
        if double {
            if doc.editing {
                doc.buf.select_word(pos);
            } else {
                self.start_editing(Some(pos));
            }
            return;
        }
        doc.buf.move_to(pos, shift);
        if !shift {
            doc.buf.anchor = Some(pos);
        }
        self.drag = Drag::Select;
    }

    fn drag_select(&mut self, x: u16, y: u16) {
        let (area, gutter, page, width) = (self.content_area, self.gutter, self.page(), self.text_width());
        let Some(doc) = self.doc() else { return };
        if doc.formatted() || area.height == 0 {
            return;
        }
        // Dragging past the top or bottom edge scrolls
        if y < area.y {
            doc.scroll(-1, page, width);
        } else if y >= area.bottom() {
            doc.scroll(1, page, width);
        }
        let pos = doc.pos_at(area, gutter, x, y.clamp(area.y, area.bottom() - 1), width);
        doc.buf.cursor = pos;
    }

    // Drawing

    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if area.width < 30 || area.height < 5 {
            f.render_widget(Paragraph::new("Too small"), area);
            return;
        }
        if self.tree_width == 0 {
            self.tree_width = (area.width * 3 / 10).clamp(16, 40);
        }
        self.tree_width = self.tree_width.clamp(8, area.width - 20);
        let (tw, body) = (self.tree_width, area.height - 1);
        if let Some(grep) = &mut self.grep {
            grep.poll();
            // The first match shows as soon as it's found
            let first = !grep.shown && grep.current().is_some();
            if first {
                self.grep_preview();
            }
        }
        let filter = u16::from(self.tree.filter.is_some() && self.history.is_none() && self.grep.is_none());
        self.divider_x = area.x + tw;
        self.filter_area = Rect::new(area.x, area.y + 1, tw, filter);
        self.tree_area = Rect::new(area.x, area.y + 1 + filter, tw, body - 1 - filter);
        self.content_area = Rect::new(self.divider_x + 1, area.y + 1, area.width - tw - 1, body - 1);

        if self.grep.is_some() {
            self.draw_grep(f, Rect::new(area.x, area.y, tw, 1));
        } else if self.history.is_some() {
            self.draw_history(f, Rect::new(area.x, area.y, tw, 1));
        } else {
            self.draw_tree(f, Rect::new(area.x, area.y, tw, 1));
        }
        let divider = if self.drag == Drag::Divider { Style::new().add_modifier(Modifier::BOLD) } else { dim() };
        let bar: Vec<Line> = (0..body).map(|_| Line::styled("│", divider)).collect();
        f.render_widget(Paragraph::new(bar), Rect::new(self.divider_x, area.y, 1, body));
        self.draw_content(f, Rect::new(self.divider_x + 1, area.y, area.width - tw - 1, body));
        self.draw_status(f, Rect::new(area.x, area.y + body, area.width, 1));
    }

    fn draw_tree(&mut self, f: &mut Frame, header_area: Rect) {
        let focused = self.focus == Focus::Tree;
        let flag = if self.tree.show_hidden { "all " } else { "" };
        header(f, header_area, &format!(" {}", tilde(&self.tree.root)), flag, focused);

        let words: Vec<String> = match &self.tree.filter {
            Some(query) => {
                let typing = focused && self.filtering;
                let area = self.filter_area;
                let files = self.tree.rows.iter().filter(|r| !r.is_dir).count();
                let count = if self.tree.filtering() { format!("{files} ") } else { String::new() };
                let room = (area.width as usize).saturating_sub(3 + str_width(&count));
                let shown: String = query.chars().rev().take(room).collect::<Vec<_>>().into_iter().rev().collect();
                let pad = room.saturating_sub(str_width(&shown));
                let style = if typing { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
                let line = Line::from(vec![
                    Span::styled(" / ", Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(shown.clone(), style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(count, dim()),
                ]);
                f.render_widget(Paragraph::new(line), area);
                if typing {
                    f.set_cursor_position(Position::new(area.x + 3 + str_width(&shown) as u16, area.y));
                }
                query.split_whitespace().map(str::to_string).collect()
            }
            None => Vec::new(),
        };

        let list = self.tree_area;
        let height = list.height as usize;
        if std::mem::take(&mut self.tree.scroll_to_selected) {
            self.tree.keep_visible(height);
        }
        self.tree.scroll = self.tree.scroll.min(self.tree.rows.len().saturating_sub(height));
        let open = self.shown_path();
        let mut lines = Vec::new();
        for (i, row) in self.tree.rows.iter().enumerate().skip(self.tree.scroll).take(height) {
            let icon = match (row.is_dir, self.tree.is_open(row)) {
                (true, true) => "▾ ",
                (true, false) => "▸ ",
                _ => "  ",
            };
            let mut style = if row.archive {
                Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
            } else if row.is_dir {
                Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)
            } else if row.name.starts_with('.') {
                dim()
            } else {
                Style::new()
            };
            if open == Some(row.path.as_path()) {
                style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            let selected = i == self.tree.selected;
            let mut bar =
                if selected && focused { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            if selected {
                style = style.add_modifier(if focused { Modifier::REVERSED } else { Modifier::UNDERLINED });
            }
            // Where a drag would drop
            if matches!(self.drag, Drag::Tree { to: Some(Target::Row(t)), .. } if t == i) {
                style = Style::new().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD);
                bar = style;
            }
            let mark = if self.tree.marks.contains(&row.path) {
                Span::styled("●", Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD))
            } else {
                Span::styled(" ", bar)
            };
            let mut spans = vec![mark, Span::styled("  ".repeat(row.depth), bar)];
            push_span(&mut spans, icon, style);
            // Where the filter's words match, in the name
            let hits: Vec<(usize, usize)> = words.iter().flat_map(|w| find_all(&row.name, w)).collect();
            let hit = style.fg(Color::Magenta).add_modifier(Modifier::BOLD);
            for (idx, c) in row.name.chars().enumerate() {
                let matched = hits.iter().any(|&(s, e)| idx >= s && idx < e);
                push_span(&mut spans, c.encode_utf8(&mut [0; 4]), if matched { hit } else { style });
            }
            if row.repo {
                spans.push(Span::styled(" ⎇", style.remove_modifier(Modifier::BOLD).fg(Color::Magenta)));
            }
            let used: usize = spans.iter().map(|s| str_width(&s.content)).sum();
            spans.push(Span::styled(" ".repeat((list.width as usize).saturating_sub(used)), bar));
            lines.push(Line::from(spans));
        }
        if self.tree.rows.is_empty() {
            let note = if self.tree.filtering() { "  (no matches)" } else { "  (empty)" };
            lines.push(Line::styled(note, dim()));
        }
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_history(&mut self, f: &mut Frame, header_area: Rect) {
        let focused = self.focus == Focus::Tree;
        let list = self.tree_area;
        let (height, width) = (list.height as usize, list.width as usize);
        let Some(h) = &mut self.history else { return };
        let mut lines = Vec::new();
        let row_style = |selected: bool| match (selected, focused) {
            (true, true) => Style::new().add_modifier(Modifier::REVERSED),
            (true, false) => Style::new().add_modifier(Modifier::UNDERLINED),
            _ => Style::new(),
        };
        match &mut h.opened {
            None => {
                let what = match &h.file {
                    Some(file) => file.rsplit('/').next().unwrap_or(file).to_string(),
                    None if h.scope == h.repo => file_name(&h.repo),
                    None => format!("{}/", h.scope.strip_prefix(&h.repo).unwrap_or(&h.scope).display()),
                };
                let back = if h.scope == h.repo { " ⎇ " } else { " ◂ " };
                header(f, header_area, &format!("{back}{what} history"), &format!("{} ", h.commits.len()), focused);
                h.scroll = h.scroll.min(h.commits.len().saturating_sub(height));
                for (i, c) in h.commits.iter().enumerate().skip(h.scroll).take(height) {
                    let base = row_style(i == h.selected);
                    let age = git::age(c.time);
                    let room = width.saturating_sub(c.short.len() + age.len() + 3);
                    let subject: String = c.subject.chars().take(room).collect();
                    let pad = room.saturating_sub(str_width(&subject));
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {} ", c.short), base.fg(Color::Yellow)),
                        Span::styled(subject, base),
                        Span::styled(" ".repeat(pad + 1), base),
                        Span::styled(age, base.add_modifier(Modifier::DIM)),
                    ]));
                }
            }
            Some(o) => {
                let c = &h.commits[o.commit];
                let what = if o.all { "all files " } else { "" };
                // The subject gives way, not the hash
                let room = width.saturating_sub(c.short.len() + str_width(what) + 5);
                let subject = if str_width(&c.subject) > room {
                    format!("{}…", c.subject.chars().take(room.saturating_sub(1)).collect::<String>())
                } else {
                    c.subject.clone()
                };
                header(f, header_area, &format!(" ◂ {} {subject}", c.short), what, focused);
                o.scroll = o.scroll.min(o.files.len().saturating_sub(height));
                for (i, (status, path)) in o.files.iter().enumerate().skip(o.scroll).take(height) {
                    let base = row_style(i == o.selected);
                    let color = match status {
                        'A' => Color::Green,
                        'D' => Color::Red,
                        'R' | 'C' => Color::Cyan,
                        ' ' => Color::Reset,
                        _ => Color::Yellow,
                    };
                    let text = path.as_str();
                    let room = width.saturating_sub(4);
                    // Long paths keep their end
                    let skip = str_width(text).saturating_sub(room);
                    let shown: String = text.chars().skip(skip).collect();
                    let pad = room.saturating_sub(str_width(&shown));
                    let path_style = if *status == ' ' { base.add_modifier(Modifier::DIM) } else { base };
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {status} "), base.fg(color).add_modifier(Modifier::BOLD)),
                        Span::styled(shown, path_style),
                        Span::styled(" ".repeat(pad + 1), base),
                    ]));
                }
            }
        }
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_grep(&mut self, f: &mut Frame, header_area: Rect) {
        let focused = self.focus == Focus::Tree;
        let list = self.tree_area;
        let (height, width) = (list.height as usize, list.width as usize);
        let root = self.tree.root.clone();
        let Some(grep) = &mut self.grep else { return };
        let count = if !grep.done {
            format!("{} files… ", grep.searched.load(std::sync::atomic::Ordering::Relaxed))
        } else if grep.cut_short() {
            format!("first {} ", grep.matches())
        } else {
            format!("{} in {} ", grep.matches(), grep.files.len())
        };
        header(f, header_area, &format!(" ⌕ “{}”", grep.query), &count, focused);
        grep.scroll = grep.scroll.min(grep.rows().saturating_sub(height));

        let query = grep.query.clone();
        let hit_style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
        let mut lines = Vec::new();
        let mut row = 0;
        'files: for (f_i, file) in grep.files.iter().enumerate() {
            for h_i in 0..=file.hits.len() {
                if row >= grep.scroll + height {
                    break 'files;
                }
                if row >= grep.scroll {
                    if h_i == 0 {
                        let name = relative(&file.path, &root);
                        lines.push(Line::from(vec![
                            Span::styled(format!(" {name}"), Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                            Span::styled(format!(" {}", file.hits.len()), dim()),
                        ]));
                    } else {
                        let hit = &file.hits[h_i - 1];
                        let selected = grep.selected == (f_i, h_i - 1);
                        let base = match (selected, focused) {
                            (true, true) => Style::new().add_modifier(Modifier::REVERSED),
                            (true, false) => Style::new().add_modifier(Modifier::UNDERLINED),
                            _ => Style::new(),
                        };
                        let number = format!(" {:>4} ", hit.line + 1);
                        let room = width.saturating_sub(number.len());
                        // From the start of the text, or from a little before the match
                        let chars: Vec<char> = hit.text.chars().collect();
                        let lead = chars.iter().take_while(|c| c.is_whitespace()).count();
                        let start = lead.max(hit.col.saturating_sub(room / 3));
                        let hits = find_all(&hit.text, &query);
                        let mut spans = vec![Span::styled(number, base.add_modifier(Modifier::DIM))];
                        if start > lead {
                            push_span(&mut spans, "…", base.add_modifier(Modifier::DIM));
                        }
                        let mut used: usize = spans.iter().map(|s| str_width(&s.content)).sum();
                        for (i, c) in chars.iter().enumerate().skip(start) {
                            let c = if c.is_control() { ' ' } else { *c };
                            used += str_width(c.encode_utf8(&mut [0; 4]));
                            if used > width {
                                break;
                            }
                            let lit = hits.iter().any(|&(s, e)| i >= s && i < e);
                            push_span(
                                &mut spans,
                                c.encode_utf8(&mut [0; 4]),
                                if lit { base.patch(hit_style) } else { base },
                            );
                        }
                        let used: usize = spans.iter().map(|s| str_width(&s.content)).sum();
                        spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), base));
                        lines.push(Line::from(spans));
                    }
                }
                row += 1;
            }
        }
        if grep.files.is_empty() {
            let note = if grep.done { "  (no matches)" } else { "  searching…" };
            lines.push(Line::styled(note, dim()));
        }
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_content(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Content;
        let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        let root = self.tree.root.clone();
        let search = self.search.clone();
        match &mut self.shown {
            Shown::Nothing => {
                header(f, area, " readme", "", focused);
                f.render_widget(Paragraph::new(Line::styled("  Pick a file on the left.", dim())), body);
            }
            Shown::Note(path, note) => {
                header(f, area, &format!(" {}", relative(path, &root)), "", focused);
                f.render_widget(Paragraph::new(Line::styled(format!("  {note}"), dim())), body);
            }
            Shown::Image(image) => {
                let title = image.label.clone().unwrap_or_else(|| format!(" {}", relative(&image.path, &root)));
                header(f, area, &title, &format!("{} · {} ", image.picture.info, human_size(image.bytes)), focused);
                image.picture.draw(f, body);
            }
            Shown::Gallery(gallery) => {
                let rel = relative(&gallery.dir, &root);
                let title = if rel.is_empty() { format!(" {}", tilde(&gallery.dir)) } else { format!(" {rel}/") };
                let count = gallery.images.len();
                let plural = if count == 1 { "" } else { "s" };
                header(f, area, &title, &format!("{count} image{plural} · {} px ", gallery.width_px), focused);
                let grid =
                    Rect::new(body.x + 1, body.y + 1, body.width.saturating_sub(2), body.height.saturating_sub(1));
                gallery.draw(f, grid, &self.picker, focused);
            }
            Shown::Doc(doc) => {
                let mut flags = Vec::new();
                if doc.disk_changed {
                    flags.push("changed on disk");
                }
                if let Some(why) = doc.buf.read_only {
                    flags.push(why);
                }
                if doc.kind != Kind::Text && !doc.editing {
                    flags.push(match (doc.raw, doc.kind) {
                        (true, _) => "raw",
                        (false, Kind::Table(_)) => "table",
                        _ => "rendered",
                    });
                }
                if doc.wrap && !doc.formatted() {
                    flags.push("wrap");
                }
                let dirty = if doc.buf.dirty { " ●" } else { "" };
                let title = match &doc.label {
                    Some(label) => label.clone(),
                    None => format!(" {}{dirty}", relative(&doc.path, &root)),
                };
                if doc.formatted() {
                    draw_formatted(f, doc, body, search.as_deref());
                } else {
                    self.gutter = draw_source(f, doc, body, focused, search.as_deref());
                }
                let more = match doc.more {
                    (true, true) => Some("← more →"),
                    (true, false) => Some("← more"),
                    (false, true) => Some("more →"),
                    _ => None,
                };
                flags.extend(more);
                let flags = if flags.is_empty() { String::new() } else { format!("{} ", flags.join(" · ")) };
                header(f, area, &title, &flags, focused);
            }
        }
    }

    fn draw_status(&mut self, f: &mut Frame, area: Rect) {
        if let Some(prompt) = &self.prompt {
            let text = match prompt {
                Prompt::Unsaved(_) => {
                    let name = self.shown_path().map(file_name).unwrap_or_default();
                    format!(" Unsaved changes to {name}: s save · d discard · Esc cancel")
                }
                Prompt::Search(query) => format!(" /{query}"),
                Prompt::Confirm(Change::Trash(items)) => {
                    format!(" Move {} to the trash? y yes · n no", describe(items))
                }
                Prompt::Confirm(Change::Move(items, dir)) => {
                    format!(" Move {} into {}? y yes · n no", describe(items), self.dir_name(dir))
                }
                Prompt::Rename(_, name) => format!(" Rename to: {name}"),
                Prompt::Grep(query) => format!(" Find in files: {query}"),
            };
            let style = Style::new().add_modifier(Modifier::BOLD);
            if let Prompt::Search(_) | Prompt::Rename(..) | Prompt::Grep(_) = prompt {
                f.set_cursor_position(Position::new(area.x + str_width(&text) as u16, area.y));
            }
            f.render_widget(Paragraph::new(Line::styled(text, style)), area);
            return;
        }
        let (mode, color) = match self.focus {
            Focus::Tree if self.grep.is_some() => (" FIND ", Color::Yellow),
            Focus::Tree if self.history.is_some() => (" GIT ", Color::Magenta),
            Focus::Tree => (" TREE ", Color::Blue),
            Focus::Content if self.editing() => (" EDIT ", Color::Green),
            Focus::Content => (" VIEW ", Color::Cyan),
        };
        let message = self.message.as_ref().filter(|(_, at)| at.elapsed() < MESSAGE_FOR).map(|(m, _)| m.clone());
        let (text, text_style) = match message {
            Some(m) => (m, Style::new().add_modifier(Modifier::BOLD)),
            None => (self.hints().to_string(), dim()),
        };
        let position = self.position();
        let room = (area.width as usize).saturating_sub(mode.len() + position.len() + 3);
        let text: String = text.chars().take(room).collect();
        let pad = room.saturating_sub(str_width(&text));
        let line = Line::from(vec![
            Span::styled(mode, Style::new().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(text, text_style),
            Span::raw(" ".repeat(pad + 1)),
            Span::styled(position, dim()),
            Span::raw(" "),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn hints(&self) -> &'static str {
        match (&self.shown, self.focus) {
            (_, Focus::Tree) if self.grep.is_some() => {
                "↑↓ matches · ⏎ go to it · n/N within the file · F search again · Esc files"
            }
            (_, Focus::Tree) if self.history.as_ref().is_some_and(|h| h.opened.is_some()) => {
                "↑↓ files · d diff/file · a all files · ← commits · ⇥ view · q quit"
            }
            (_, Focus::Tree) if self.history.as_ref().is_some_and(|h| h.file.is_some()) => {
                "↑↓ commits · d diff/file · ⏎ its files · ← whole folder · Esc files · q quit"
            }
            (_, Focus::Tree) if self.history.is_some() => "↑↓ commits · ⏎ its files · ← wider · Esc files · q quit",
            (_, Focus::Tree) if self.filtering => "type to filter · ↑↓ choose · ⏎ open · Esc clear",
            (_, Focus::Tree) if !self.tree.marks.is_empty() => {
                "Space mark · drag onto a folder to move · Del trash · Esc unmark"
            }
            (_, Focus::Tree) => {
                "↑↓ move · ⏎ open · F find in files · H git history · / filter · Space mark · Del trash · F2 rename · q quit"
            }
            (Shown::Gallery(_), _) => "←→↑↓ choose · ⏎ or click to open · +/- size · ⇥ tree · q quit",
            (Shown::Image(_), _) if self.gallery_stash.is_some() => {
                "wheel or +/- zoom · 0 fit · drag to move · Esc back to the thumbnails · ← tree"
            }
            (Shown::Image(_), _) => "wheel or +/- zoom · 0 fit · drag or arrows to move · ← tree · q quit",
            (Shown::Doc(doc), _) if doc.editing => {
                "Esc done · ^S save · ^Z/^Y undo/redo · ^C ^X ^V copy/cut/paste · ^F find · Alt+Z wrap · ⇥ tree"
            }
            // Dragging selects only in the source view
            (Shown::Doc(doc), _) if doc.formatted() && doc.kind == Kind::Markdown => {
                "e edit · / find · m raw/rendered · ⇥ tree · q quit"
            }
            (Shown::Doc(doc), _) if doc.formatted() => "e edit · / find · ←→ scroll · m raw/table · ⇥ tree · q quit",
            (Shown::Doc(doc), _) if doc.kind != Kind::Text => {
                "e edit · / find · w wrap · m formatted · drag to copy · ⇥ tree · q quit"
            }
            _ => "e edit · / find · w wrap · drag to copy · ⇥ tree · q quit",
        }
    }

    fn position(&self) -> String {
        if let Shown::Gallery(gallery) = &self.shown {
            return format!("{}/{}", gallery.selected + 1, gallery.images.len());
        }
        let Shown::Doc(doc) = &self.shown else { return String::new() };
        if self.editing() {
            let c = doc.buf.cursor;
            return format!("Ln {}, Col {}", c.line + 1, display_col(&doc.buf.lines[c.line], c.col) + 1);
        }
        let (top, total) = if doc.formatted() {
            (
                doc.fmt_top - doc.pinned().min(doc.fmt_top),
                doc.rendered.as_ref().map_or(0, |(_, r)| r.lines.len() - r.pinned),
            )
        } else {
            (doc.top, doc.buf.lines.len())
        };
        let bottom = (top + self.page()).min(total);
        if top == 0 && bottom >= total { "All".into() } else { format!("{}%", bottom * 100 / total.max(1)) }
    }
}

/// Draws rendered Markdown or a table: any pinned header lines, then the rest from
/// `fmt_top`, scrolled sideways past the gutter.
fn draw_formatted(f: &mut Frame, doc: &mut Doc, area: Rect, search: Option<&str>) {
    let text = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), area.height);
    let height = area.height as usize;
    doc.render(text.width);
    let Some((_, rendered)) = &doc.rendered else { return };
    let pinned = rendered.pinned.min(height);
    let body = height - pinned;
    let max_top = rendered.lines.len().saturating_sub(body).max(rendered.pinned);
    doc.fmt_top = doc.fmt_top.clamp(rendered.pinned, max_top);
    doc.fmt_left = doc.fmt_left.min(rendered.width.saturating_sub(text.width as usize));
    doc.more = (doc.fmt_left > 0, doc.fmt_left + (text.width as usize) < rendered.width);
    let (top, left, gutter) = (doc.fmt_top, doc.fmt_left, rendered.gutter);
    let lines: Vec<Line> = rendered.lines[..pinned]
        .iter()
        .chain(rendered.lines.iter().skip(top).take(body))
        .map(|line| {
            let line = slice_line(line, gutter, left);
            match search {
                Some(q) => markdown::mark_matches(&line, q, match_style()),
                None => line,
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), text);
}

/// Draws the file with line numbers, wrapped or scrolled sideways; returns the gutter width.
fn draw_source(f: &mut Frame, doc: &mut Doc, area: Rect, focused: bool, search: Option<&str>) -> u16 {
    if let Some(line) = doc.buf.changed_from.take() {
        doc.colors.invalidate(line);
    }
    let total = doc.buf.lines.len();
    let height = area.height as usize;
    let digits = total.to_string().len();
    let gutter = digits as u16 + 2;
    let width = area.width.saturating_sub(gutter).max(1) as usize;
    doc.top = doc.top.min(total - 1);
    if !doc.wrap {
        doc.top_row = 0;
        // No scrolling past the end of the longest line (tabs count at their widest)
        let version = doc.buf.version;
        let widest = match doc.widest {
            Some((v, widest)) if v == version => widest,
            _ => {
                let widest =
                    doc.buf.lines.iter().map(|l| l.len() + 3 * l.bytes().filter(|&b| b == b'\t').count()).max();
                let widest = widest.unwrap_or(0);
                doc.widest = Some((version, widest));
                widest
            }
        };
        doc.left = doc.left.min((widest + 1).saturating_sub(width));
    }

    // The screen rows: (line, row within it, first char, first char of the next row)
    let mut rows = Vec::with_capacity(height);
    let (mut line, mut row) = (doc.top, doc.top_row);
    while rows.len() < height && line < total {
        let starts = doc.rows(line, width);
        for r in row.min(starts.len() - 1)..starts.len() {
            if rows.len() == height {
                break;
            }
            rows.push((line, r, starts[r], starts.get(r + 1).copied()));
        }
        line += 1;
        row = 0;
    }
    let bottom = rows.last().map_or(doc.top, |r| r.0 + 1);
    doc.colors.ensure(&doc.buf.lines, bottom);
    doc.more = if doc.wrap {
        (false, false)
    } else {
        let cut_off = rows.iter().any(|&(i, ..)| str_width(&doc.buf.lines[i]) > doc.left + width);
        (doc.left > 0, cut_off)
    };

    let selection = doc.buf.selection();
    let current = doc.last_match;
    let mut lines = Vec::with_capacity(height);
    for &(i, r, start, next) in &rows {
        let text = &doc.buf.lines[i];
        let number = if r > 0 { " ".repeat(digits + 2) } else { format!("{:>digits$}  ", i + 1) };
        let number_style = if doc.editing && i == doc.buf.cursor.line { Style::new().fg(Color::Yellow) } else { dim() };
        let mut marks = Vec::new();
        if let Some(q) = search {
            for (s, e) in find_all(text, q) {
                let style = if current == Some(Pos::new(i, s)) {
                    match_style().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    match_style()
                };
                marks.push((s, e, style));
            }
        }
        if let Some((s, e)) = selection.filter(|(s, e)| (s.line..=e.line).contains(&i)) {
            let from = if i == s.line { s.col } else { 0 };
            let to = if i == e.line { e.col } else { usize::MAX };
            marks.push((from, to, selection_style()));
        }
        // A wrapped row shows just its own characters; the last row of a line may
        // also show the selection running on past its end
        let (left, room) = if doc.wrap {
            let left = display_col(text, start);
            (left, next.map_or(width, |n| display_col(text, n) - left))
        } else {
            (doc.left, width)
        };
        let mut spans = vec![Span::styled(number, number_style)];
        spans.extend(line_spans(text, doc.colors.get(i), left, room, &marks));
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), area);

    if focused && doc.editing {
        let c = doc.buf.cursor;
        let text = &doc.buf.lines[c.line];
        let col = display_col(text, c.col);
        let on_row = rows
            .iter()
            .position(|&(i, _, start, next)| i == c.line && c.col >= start && next.is_none_or(|n| c.col < n));
        if let Some(y) = on_row {
            let left = if doc.wrap { display_col(text, rows[y].2) } else { doc.left };
            if col >= left && col - left < width {
                f.set_cursor_position(Position::new(area.x + gutter + (col - left) as u16, area.y + y as u16));
            }
        }
    }
    gutter
}

/// A one-line pane title, reversed when the pane has the focus.
fn header(f: &mut Frame, area: Rect, title: &str, right: &str, focused: bool) {
    let style = if focused {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::DIM)
    };
    let width = area.width as usize;
    let room = width.saturating_sub(str_width(right));
    // Long paths lose their beginning, not the file name
    let mut title = title.to_string();
    if str_width(&title) > room {
        let skip = str_width(&title) - room + 2;
        title = format!(" …{}", title.chars().skip(skip + 1).collect::<String>());
    }
    let pad = width.saturating_sub(str_width(&title) + str_width(right));
    let line = Line::from(vec![Span::raw(title), Span::raw(" ".repeat(pad)), Span::raw(right.to_string())]);
    f.render_widget(Paragraph::new(line).style(style), Rect::new(area.x, area.y, area.width, 1));
}

/// "notes.md", or "3 items".
fn describe(items: &[PathBuf]) -> String {
    match items {
        [one] => file_name(one),
        _ => format!("{} items", items.len()),
    }
}

fn find_readme(dir: &Path) -> Option<PathBuf> {
    let mut names: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && file_name(p).to_lowercase().starts_with("readme"))
        .collect();
    names.sort_by_key(|p| !file_name(p).eq_ignore_ascii_case("readme.md"));
    names.into_iter().next()
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) && path != home => {
            format!("~/{}", path.strip_prefix(&home).unwrap().display())
        }
        Some(home) if path == home => "~".into(),
        _ => path.display().to_string(),
    }
}

fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).map_or_else(|_| tilde(path), |p| p.display().to_string())
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} bytes"),
        b if b < 1 << 20 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyEventState};

    struct Harness {
        app: App,
        term: Terminal<TestBackend>,
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Harness {
        fn new(files: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            for (name, text) in files {
                let path = root.join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text).unwrap();
            }
            let app = App::new(&root).unwrap();
            let mut h = Self { app, term: Terminal::new(TestBackend::new(80, 20)).unwrap(), _dir: dir, root };
            h.draw();
            h
        }

        fn draw(&mut self) {
            self.term.draw(|f| self.app.draw(f)).unwrap();
        }

        fn send(&mut self, event: Event) {
            self.app.handle(event);
            self.draw();
        }

        fn key(&mut self, code: KeyCode) {
            self.key_with(code, KeyModifiers::NONE);
        }

        fn key_with(&mut self, code: KeyCode, modifiers: KeyModifiers) {
            self.send(Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, state: KeyEventState::NONE }));
        }

        fn typing(&mut self, text: &str) {
            for c in text.chars() {
                self.key(KeyCode::Char(c));
            }
        }

        fn mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) {
            self.mouse_with(kind, x, y, KeyModifiers::NONE);
        }

        fn mouse_with(&mut self, kind: MouseEventKind, x: u16, y: u16, modifiers: KeyModifiers) {
            self.send(Event::Mouse(MouseEvent { kind, column: x, row: y, modifiers }));
        }

        fn click(&mut self, x: u16, y: u16) {
            self.app.last_click = None;
            self.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
            self.mouse(MouseEventKind::Up(MouseButton::Left), x, y);
        }

        fn row(&self, y: u16) -> String {
            let buf = self.term.backend().buffer();
            (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>()
        }

        fn screen(&self) -> String {
            (0..self.term.backend().buffer().area.height).map(|y| self.row(y)).collect::<Vec<_>>().join("\n")
        }

        /// Row and column where `text` first appears on screen.
        fn find(&self, text: &str) -> (u16, u16) {
            for y in 0..self.term.backend().buffer().area.height {
                let row = self.row(y);
                if let Some(i) = row.find(text) {
                    return (row[..i].chars().count() as u16, y);
                }
            }
            panic!("{text:?} not on screen:\n{}", self.screen());
        }

        fn cursor(&mut self) -> Pos {
            self.app.doc().unwrap().buf.cursor
        }
    }

    #[test]
    fn opens_the_readme_rendered() {
        let h = Harness::new(&[("README.md", "# Hello\n\nSome **bold** words.\n"), ("src/main.rs", "fn main() {}\n")]);
        let screen = h.screen();
        assert!(screen.contains("Hello"), "{screen}");
        assert!(screen.contains("━━━━━"), "{screen}");
        assert!(screen.contains("Some bold words."), "{screen}");
        assert!(!screen.contains("**"), "{screen}");
        assert!(screen.contains("▸ src"), "{screen}");
    }

    #[test]
    fn tab_switches_panes() {
        let mut h = Harness::new(&[("a.txt", "a\n")]);
        assert_eq!(h.app.focus, Focus::Tree);
        h.key(KeyCode::Tab);
        assert_eq!(h.app.focus, Focus::Content);
        h.key(KeyCode::Tab);
        assert_eq!(h.app.focus, Focus::Tree);
    }

    #[test]
    fn clicking_opens_dirs_and_files() {
        let mut h = Harness::new(&[("src/lib.rs", "pub fn lib() {}\n"), ("notes.txt", "hello notes\n")]);
        let (x, y) = h.find("src");
        h.click(x, y);
        let (x, y) = h.find("lib.rs");
        h.click(x, y);
        assert!(h.screen().contains("pub fn lib() {}"), "{}", h.screen());
        let (x, y) = h.find("notes.txt");
        h.click(x, y);
        assert!(h.screen().contains("hello notes"), "{}", h.screen());
    }

    #[test]
    fn wheel_scrolls_the_file() {
        let text: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let mut h = Harness::new(&[("long.txt", &text)]);
        h.key(KeyCode::Enter);
        let (x, y) = h.find("line 1");
        h.mouse(MouseEventKind::ScrollDown, x, y);
        h.mouse(MouseEventKind::ScrollDown, x, y);
        assert!(h.row(1).contains("line 7"), "{}", h.screen());
    }

    #[test]
    fn edit_and_save() {
        let mut h = Harness::new(&[("a.txt", "hello\n")]);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('e'));
        h.key(KeyCode::End);
        h.typing(" world");
        h.key(KeyCode::Enter);
        h.typing("again");
        assert!(h.screen().contains("a.txt ●"), "{}", h.screen());
        h.key_with(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(fs::read_to_string(h.root.join("a.txt")).unwrap(), "hello world\nagain\n");
        assert!(!h.screen().contains("●"));
        h.key_with(KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert!(h.screen().contains("hello world"));
        assert!(!h.screen().contains("again"));
    }

    #[test]
    fn quitting_with_changes_asks_first() {
        let mut h = Harness::new(&[("a.txt", "hello\n")]);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('i'));
        h.typing("x");
        h.key(KeyCode::Esc);
        h.key(KeyCode::Char('q'));
        assert!(!h.app.quit);
        assert!(h.screen().contains("Unsaved changes to a.txt"), "{}", h.screen());
        h.key(KeyCode::Esc);
        assert!(!h.app.quit);
        h.key(KeyCode::Char('q'));
        h.key(KeyCode::Char('d'));
        assert!(h.app.quit);
        assert_eq!(fs::read_to_string(h.root.join("a.txt")).unwrap(), "hello\n");
    }

    #[test]
    fn dragging_selects_and_copies() {
        let mut h = Harness::new(&[("a.txt", "alpha beta gamma\n")]);
        h.key(KeyCode::Enter);
        let (x, y) = h.find("beta");
        h.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        h.mouse(MouseEventKind::Drag(MouseButton::Left), x + 4, y);
        h.mouse(MouseEventKind::Up(MouseButton::Left), x + 4, y);
        let copied = clipboard::COPIED.with(|c| c.borrow().last().cloned());
        assert_eq!(copied.as_deref(), Some("beta"));
    }

    #[test]
    fn clicking_in_edit_mode_moves_the_cursor() {
        let mut h = Harness::new(&[("a.txt", "alpha beta\n")]);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('e'));
        let (x, y) = h.find("beta");
        h.click(x, y);
        h.typing("new ");
        assert!(h.screen().contains("alpha new beta"), "{}", h.screen());
    }

    #[test]
    fn search_jumps_to_matches() {
        let text: String =
            (1..=100).map(|i| if i == 60 { "the needle\n".to_string() } else { format!("hay {i}\n") }).collect();
        let mut h = Harness::new(&[("hay.txt", &text)]);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('/'));
        h.typing("needle");
        h.key(KeyCode::Enter);
        assert!(h.screen().contains("the needle"), "{}", h.screen());
    }

    #[test]
    fn markdown_toggles_to_raw_and_back() {
        let mut h = Harness::new(&[("README.md", "# Title\n\ntext\n")]);
        h.key(KeyCode::Tab);
        h.key(KeyCode::Char('m'));
        assert!(h.screen().contains("# Title"), "{}", h.screen());
        h.key(KeyCode::Char('m'));
        assert!(!h.screen().contains("# Title"), "{}", h.screen());
    }

    #[test]
    fn reloads_when_the_file_changes() {
        let mut h = Harness::new(&[("a.txt", "before\n")]);
        h.key(KeyCode::Enter);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(h.root.join("a.txt"), "after, longer\n").unwrap();
        h.app.tick();
        h.draw();
        assert!(h.screen().contains("after, longer"), "{}", h.screen());
    }

    #[test]
    fn reloads_rendered_markdown_when_the_file_changes() {
        let mut h = Harness::new(&[("README.md", "# Before\n")]);
        assert!(h.screen().contains("Before"), "{}", h.screen());
        std::thread::sleep(Duration::from_millis(20));
        fs::write(h.root.join("README.md"), "# After, longer\n").unwrap();
        h.app.tick();
        h.draw();
        assert!(h.screen().contains("After, longer"), "{}", h.screen());
    }

    #[test]
    fn pasting_into_the_search_prompt_leaves_the_file_alone() {
        let mut h = Harness::new(&[("a.txt", "alpha needle\n")]);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('e'));
        h.key_with(KeyCode::Char('f'), KeyModifiers::CONTROL);
        h.send(Event::Paste("needle\nmore".into()));
        assert!(h.screen().contains("/needle"), "{}", h.screen());
        assert!(!h.app.dirty());
    }

    #[test]
    fn dragging_the_divider_resizes() {
        let mut h = Harness::new(&[("a.txt", "a\n")]);
        let x = h.app.divider_x;
        h.mouse(MouseEventKind::Down(MouseButton::Left), x, 3);
        h.mouse(MouseEventKind::Drag(MouseButton::Left), x + 10, 3);
        h.mouse(MouseEventKind::Up(MouseButton::Left), x + 10, 3);
        assert_eq!(h.app.divider_x, x + 10);
    }

    #[test]
    fn binary_files_are_not_shown() {
        let mut h = Harness::new(&[("blob.bin", "ab\0cd")]);
        h.key(KeyCode::Enter);
        assert!(h.screen().contains("Binary file"), "{}", h.screen());
    }

    const LONG: &str = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen\nshort\n";

    #[test]
    fn arrows_scroll_long_lines_sideways() {
        let line = format!("start {} END", "x".repeat(150));
        let mut h = Harness::new(&[("wide.txt", &format!("{line}\nshort\n"))]);
        h.key(KeyCode::Enter);
        assert!(h.screen().contains("more →"), "{}", h.screen());
        assert!(!h.screen().contains("END"));
        for _ in 0..30 {
            h.key(KeyCode::Right);
        }
        // It stops at the end of the longest line, and says there's more to the left
        let screen = h.screen();
        assert!(screen.contains("END") && screen.contains("← more"), "{screen}");
        assert!(!screen.contains("more →"), "{screen}");
        // ← scrolls back first, then returns to the tree
        for _ in 0..30 {
            h.key(KeyCode::Left);
        }
        assert!(h.screen().contains("start"), "{}", h.screen());
        assert_eq!(h.app.focus, Focus::Tree);
    }

    #[test]
    fn wide_markdown_tables_scroll_sideways() {
        let md = format!(
            "# T\n\n| {} |\n|---|\n| last |\n",
            "a heading that is far too wide for the pane ".repeat(2).trim()
        );
        let mut h = Harness::new(&[("README.md", &md)]);
        h.key(KeyCode::Tab);
        assert!(h.screen().contains("more →"), "{}", h.screen());
        assert!(!h.screen().contains('…'), "the table isn't squeezed:\n{}", h.screen());
        for _ in 0..10 {
            h.key(KeyCode::Right);
        }
        assert!(h.screen().contains("for the pane"), "{}", h.screen());
    }

    #[test]
    fn word_wrap_toggles() {
        let mut h = Harness::new(&[("a.txt", LONG)]);
        h.key(KeyCode::Enter);
        assert!(!h.screen().contains("eighteen"), "{}", h.screen());
        h.key(KeyCode::Char('w'));
        let screen = h.screen();
        assert!(screen.contains("eighteen"), "{screen}");
        assert!(screen.contains("wrap"), "{screen}");
        // The continuation has no line number, and the next line is numbered 2
        let (_, short) = h.find("short");
        assert!(h.row(short).contains("2  short"), "{screen}");
        assert!(!h.row(short - 1).contains(" 1 "), "{screen}");
        h.key(KeyCode::Char('w'));
        assert!(!h.screen().contains("eighteen"), "{}", h.screen());
    }

    #[test]
    fn editing_wrapped_lines_moves_by_screen_rows() {
        let mut h = Harness::new(&[("a.txt", LONG)]);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('w'));
        h.key(KeyCode::Char('e'));
        h.key(KeyCode::Down);
        let pos = h.cursor();
        assert_eq!(pos.line, 0, "still in the first line, one row down");
        assert!(pos.col > 0);
        // A click on the continuation row lands in the same line
        let (x, y) = h.find("eighteen");
        h.click(x, y);
        let pos = h.cursor();
        assert_eq!((pos.line, &LONG[pos.col..pos.col + 8]), (0, "eighteen"));
        h.typing("X");
        assert!(h.screen().contains("Xeighteen"), "{}", h.screen());
    }

    #[test]
    fn csv_shows_as_a_table_with_its_header_kept() {
        let mut text = String::from("name,amount,note\n");
        for i in 1..=60 {
            text.push_str(&format!("item {i},{},\"row {i}\"\n", i * 10));
        }
        let mut h = Harness::new(&[("data.csv", &text)]);
        h.key(KeyCode::Enter);
        let screen = h.screen();
        assert!(screen.contains("name    │ amount │ note"), "{screen}");
        assert!(screen.contains("item 1  │     10 │ row 1"), "{screen}");
        assert!(screen.contains("table"), "{screen}");
        for _ in 0..30 {
            h.key(KeyCode::Down);
        }
        let screen = h.screen();
        assert!(screen.contains("name    │ amount"), "header stays:\n{screen}");
        assert!(!screen.contains("item 1  │"), "{screen}");
        // The raw text opens at the same place
        h.key(KeyCode::Char('m'));
        let (_, y) = h.find("item 31,310,\"row 31\"");
        assert_eq!(y, 1, "{}", h.screen());
    }

    #[test]
    fn tree_filter_finds_files_anywhere() {
        let mut h = Harness::new(&[("src/deep/needle.rs", "found me\n"), ("src/other.rs", "\n"), ("notes.txt", "\n")]);
        h.key(KeyCode::Char('/'));
        h.typing("needle");
        let screen = h.screen();
        assert!(screen.contains("/ needle"), "{screen}");
        assert!(screen.contains("needle.rs"), "{screen}");
        assert!(!screen.contains("notes.txt"), "{screen}");
        assert!(!screen.contains("other.rs"), "{screen}");
        h.key(KeyCode::Enter);
        assert!(h.screen().contains("found me"), "{}", h.screen());
        assert_eq!(h.app.focus, Focus::Content);
        h.key(KeyCode::Tab);
        h.key(KeyCode::Esc);
        let screen = h.screen();
        assert!(screen.contains("notes.txt"), "{screen}");
        assert!(!screen.contains("/ needle"), "{screen}");
    }

    #[test]
    fn clicking_a_web_page_opens_the_browser() {
        let mut h = Harness::new(&[("a.txt", "a\n"), ("page.html", "<h1>Hi</h1>\n")]);
        // Arrowing past it only shows the source
        h.key(KeyCode::Down);
        h.key(KeyCode::Down);
        assert!(h.screen().contains("<h1>Hi</h1>"), "{}", h.screen());
        assert!(external::OPENED.with(|o| o.borrow().is_empty()));
        // The tree's row, not the file pane's title
        let (x, y) = h.find("  page.html");
        h.click(x + 2, y);
        let opened = external::OPENED.with(|o| o.borrow().clone());
        assert_eq!(opened, vec![h.root.join("page.html")]);
        assert!(h.screen().contains("Opened page.html in the browser"), "{}", h.screen());
    }

    fn write_png(path: &Path, width: u32, height: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut image = image::RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x * 20) as u8, (y * 20) as u8, 180]);
        }
        image.save(path).unwrap();
    }

    #[test]
    fn a_folder_of_images_shows_thumbnails() {
        let mut h = Harness::new(&[("notes.txt", "notes\n")]);
        for name in ["c.png", "a.png", "b.jpg"] {
            write_png(&h.root.join("pics").join(name), 12, 8);
        }
        h.app.tree.refresh();
        h.draw();
        // Arrowing onto the folder shows its images, by name
        h.key(KeyCode::Down);
        h.key(KeyCode::Up);
        assert!(h.screen().contains("3 images"), "{}", h.screen());
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.screen().contains('…') && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            h.draw();
        }
        let screen = h.screen();
        assert!(!screen.contains('…'), "all thumbnails decoded:\n{screen}");
        let (a, b, c) = (h.find("a.png"), h.find("b.jpg"), h.find("c.png"));
        // Two fit across this pane; the third starts the next row
        assert!(a.1 == b.1 && a.0 < b.0 && c.1 > a.1 && c.0 == a.0, "a grid, sorted by name:\n{screen}");

        // A click opens one; Esc goes back to the thumbnails
        h.click(b.0, b.1);
        assert!(h.screen().contains("12×8 JPG"), "{}", h.screen());
        assert!(h.screen().contains("pics/b.jpg"), "{}", h.screen());
        h.key(KeyCode::Esc);
        assert!(h.screen().contains("3 images"), "{}", h.screen());
        assert_eq!(h.app.position(), "2/3");

        // Keys move between them and open the selected one
        h.key(KeyCode::Right);
        h.key(KeyCode::Enter);
        assert!(h.screen().contains("pics/c.png"), "{}", h.screen());
    }

    #[test]
    fn clicking_a_video_plays_it() {
        let mut h = Harness::new(&[("a.txt", "a\n"), ("clip.mp4", "\0\0\0\x18ftypmp42")]);
        h.key(KeyCode::Down);
        h.key(KeyCode::Down);
        assert!(h.screen().contains("Video, "), "{}", h.screen());
        assert!(external::OPENED.with(|o| o.borrow().is_empty()));
        let (x, y) = h.find("  clip.mp4");
        h.click(x + 2, y);
        assert_eq!(external::OPENED.with(|o| o.borrow().clone()), vec![h.root.join("clip.mp4")]);
        assert!(h.screen().contains("Opened clip.mp4 in the video player"), "{}", h.screen());
    }

    #[test]
    fn history_of_a_repository_inside_the_tree() {
        let h0 = Harness::new(&[("notes.txt", "n\n"), ("proj/src/lib.rs", "one\n"), ("proj/README.md", "# P\n")]);
        let proj = h0.root.join("proj");
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&proj)
                .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "start"]);
        fs::write(proj.join("src/lib.rs"), "two\n").unwrap();
        git(&["commit", "-q", "-am", "change lib"]);
        let mut h = h0;
        h.app.tree.refresh();
        h.draw();
        // The folder at the top isn't a repository, but proj is, and says so
        assert!(h.screen().contains("proj ⎇"), "{}", h.screen());
        h.key(KeyCode::Down);
        h.key(KeyCode::Char('H'));
        assert!(h.screen().contains("notes.txt isn't in a git repository"), "{}", h.screen());
        let (x, y) = h.find("proj");
        h.click(x, y);
        h.key(KeyCode::Char('H'));
        let screen = h.screen();
        assert!(screen.contains("proj history") && screen.contains("change lib"), "{screen}");
        assert!(screen.contains("src/lib.rs | 2") && screen.contains("-one"), "the whole commit:\n{screen}");
        // ← leaves at the top of the repository
        h.key(KeyCode::Left);
        assert!(h.app.history.is_none());
    }

    #[test]
    fn find_in_files_lists_matches_and_shows_them() {
        let mut h = Harness::new(&[
            ("a.txt", "one\nthe needle here\n"),
            ("src/b.rs", "fn x() {}\n// needle\n"),
            ("c.txt", "nothing\n"),
        ]);
        h.key(KeyCode::Char('F'));
        assert!(h.screen().contains("Find in files:"), "{}", h.screen());
        h.typing("needle");
        h.key(KeyCode::Enter);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !h.app.grep.as_ref().unwrap().done && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            h.draw();
        }
        h.draw();
        let screen = h.screen();
        assert!(screen.contains("“needle”") && screen.contains("2 in 2"), "{screen}");
        assert!(screen.contains("a.txt") && screen.contains("src/b.rs") && !screen.contains("c.txt"), "{screen}");
        // The first match shows at its line
        assert!(screen.contains("2  the needle here"), "{screen}");
        h.key(KeyCode::Down);
        assert!(h.screen().contains("2  // needle"), "{}", h.screen());
        h.key(KeyCode::Enter);
        assert_eq!(h.app.focus, Focus::Content);
        h.key(KeyCode::Tab);
        h.key(KeyCode::Esc);
        assert!(h.app.grep.is_none());
        assert!(h.screen().contains("c.txt"), "the tree is back:\n{}", h.screen());
    }

    #[test]
    fn marked_files_go_to_the_trash() {
        let mut h = Harness::new(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        h.key(KeyCode::Char(' '));
        h.key(KeyCode::Char(' '));
        assert_eq!(h.screen().matches('●').count(), 2, "{}", h.screen());
        h.key(KeyCode::Delete);
        assert!(h.screen().contains("Move 2 items to the trash?"), "{}", h.screen());
        h.key(KeyCode::Char('y'));
        assert!(!h.root.join("a.txt").exists() && !h.root.join("b.txt").exists());
        assert!(h.root.join("c.txt").exists());
        // No leaves it alone
        h.key(KeyCode::Delete);
        assert!(h.screen().contains("Move c.txt to the trash?"), "{}", h.screen());
        h.key(KeyCode::Char('n'));
        assert!(h.root.join("c.txt").exists());
    }

    #[test]
    fn ctrl_click_marks_rows() {
        let mut h = Harness::new(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        for name in ["  a.txt", "  c.txt"] {
            let (x, y) = h.find(name);
            h.mouse_with(MouseEventKind::Down(MouseButton::Left), x + 2, y, KeyModifiers::CONTROL);
            h.mouse_with(MouseEventKind::Up(MouseButton::Left), x + 2, y, KeyModifiers::CONTROL);
        }
        h.key(KeyCode::Delete);
        assert!(h.screen().contains("Move 2 items to the trash?"), "{}", h.screen());
        h.key(KeyCode::Esc);
        assert!(h.root.join("a.txt").exists());
    }

    #[test]
    fn renaming_the_open_file_follows_it() {
        let mut h = Harness::new(&[("notes.txt", "hello\n")]);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Tab);
        h.key(KeyCode::F(2));
        assert!(h.screen().contains("Rename to: notes.txt"), "{}", h.screen());
        for _ in 0..3 {
            h.key(KeyCode::Backspace);
        }
        h.typing("md");
        h.key(KeyCode::Enter);
        assert!(h.root.join("notes.md").exists() && !h.root.join("notes.txt").exists());
        // Edits are saved under the new name
        h.key(KeyCode::Tab);
        h.key(KeyCode::Char('e'));
        h.typing("x");
        h.key_with(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(fs::read_to_string(h.root.join("notes.md")).unwrap(), "xhello\n");
        assert!(!h.root.join("notes.txt").exists());
    }

    #[test]
    fn dragging_onto_a_folder_moves_files() {
        let mut h = Harness::new(&[("sub/keep.txt", "k\n"), ("x.txt", "x\n")]);
        let (x, from) = h.find("  x.txt");
        let (_, to) = h.find("▸ sub");
        h.mouse(MouseEventKind::Down(MouseButton::Left), x + 2, from);
        h.mouse(MouseEventKind::Drag(MouseButton::Left), x + 2, to);
        assert!(h.screen().contains("Drop to move x.txt into sub/"), "{}", h.screen());
        h.mouse(MouseEventKind::Up(MouseButton::Left), x + 2, to);
        assert!(h.screen().contains("Move x.txt into sub/?"), "{}", h.screen());
        h.key(KeyCode::Char('y'));
        assert!(h.root.join("sub/x.txt").exists() && !h.root.join("x.txt").exists());
    }

    #[test]
    fn left_goes_back_to_the_tree_unless_editing() {
        let mut h = Harness::new(&[("a.txt", "abc\n")]);
        h.key(KeyCode::Enter);
        assert_eq!(h.app.focus, Focus::Content);
        h.key(KeyCode::Left);
        assert_eq!(h.app.focus, Focus::Tree);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('e'));
        h.key(KeyCode::End);
        h.key(KeyCode::Left);
        assert_eq!(h.app.focus, Focus::Content);
        assert_eq!(h.cursor().col, 2);
    }

    #[test]
    fn images_zoom_with_the_wheel_and_keys() {
        let h0 = Harness::new(&[]);
        write_png(&h0.root.join("big.png"), 64, 48);
        let mut h = Harness { app: App::new(&h0.root.join("big.png")).unwrap(), ..h0 };
        h.draw();
        let zoom = |h: &Harness| match &h.app.shown {
            Shown::Image(image) => image.picture.zoom,
            _ => panic!("no image shown"),
        };
        let area = h.app.content_area;
        h.mouse(MouseEventKind::ScrollUp, area.x + area.width / 2, area.y + area.height / 2);
        assert!(zoom(&h) > 1.0);
        h.key(KeyCode::Char('0'));
        assert_eq!(zoom(&h), 1.0);
        h.key(KeyCode::Char('-'));
        assert!(zoom(&h) < 1.0);
    }

    #[test]
    fn archives_open_like_directories() {
        use std::io::Write;
        let mut h = Harness::new(&[("other.txt", "\n")]);
        let mut zip = zip::ZipWriter::new(fs::File::create(h.root.join("bundle.zip")).unwrap());
        zip.start_file("docs/guide.md", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"# Guide\n\nInside the zip.\n").unwrap();
        zip.finish().unwrap();
        h.app.tree.refresh();
        h.draw();
        let (x, y) = h.find("bundle.zip");
        h.click(x, y);
        let (x, y) = h.find("docs");
        h.click(x, y);
        let (x, y) = h.find("guide.md");
        h.click(x, y);
        let screen = h.screen();
        assert!(screen.contains("Inside the zip."), "{screen}");
        assert!(screen.contains("bundle.zip/docs/guide.md"), "{screen}");
        // Read-only: e says why instead of editing
        h.key(KeyCode::Tab);
        h.key(KeyCode::Char('e'));
        assert!(!h.app.editing());
        assert!(h.screen().contains("in an archive"), "{}", h.screen());
    }

    #[test]
    fn images_show_in_half_blocks() {
        let h0 = Harness::new(&[]);
        let mut image = image::RgbImage::new(8, 8);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgb([x as u8 * 30, y as u8 * 30, 200]);
        }
        image.save(h0.root.join("dot.png")).unwrap();
        let mut h = Harness { app: App::new(&h0.root.join("dot.png")).unwrap(), ..h0 };
        h.draw();
        let screen = h.screen();
        assert!(screen.contains("8×8 PNG"), "{screen}");
        assert!(screen.contains('▀') || screen.contains('▄'), "{screen}");
        // TGA has no signature to know it by, only its name
        image.save(h.root.join("dot.tga")).unwrap();
        h.app.open(&h.root.join("dot.tga"));
        h.draw();
        assert!(h.screen().contains("8×8 TGA"), "{}", h.screen());
    }

    #[test]
    fn history_shows_commits_files_and_diffs() {
        let h0 = Harness::new(&[("a.txt", "one\n")]);
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&h0.root)
                .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first version"]);
        fs::write(h0.root.join("a.txt"), "two\n").unwrap();
        git(&["commit", "-q", "-am", "second version"]);
        let mut h = h0;

        // H on a file: its history, and the file as it was. From the file, with
        // find-in-files' results in the left pane, the history takes their place.
        h.key(KeyCode::Char('F'));
        h.typing("two");
        h.key(KeyCode::Enter);
        h.key(KeyCode::Tab);
        h.key(KeyCode::Char('H'));
        assert!(h.app.grep.is_none() && h.app.history.is_some());
        h.key(KeyCode::Char('H'));
        h.key(KeyCode::Char('H'));
        let screen = h.screen();
        assert!(screen.contains("second versi") && screen.contains("first versi"), "{screen}");
        assert!(screen.contains("a.txt history"), "{screen}");
        assert!(screen.contains("1  two"), "{screen}");

        h.key(KeyCode::Down);
        h.key(KeyCode::Enter);
        let screen = h.screen();
        assert!(screen.contains(" A a.txt"), "{screen}");
        assert!(screen.contains("a.txt @"), "{screen}");
        assert!(screen.contains("one"), "{screen}");

        h.key(KeyCode::Left);
        h.key(KeyCode::Up);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('d'));
        let screen = h.screen();
        assert!(screen.contains("-one") && screen.contains("+two"), "{screen}");

        h.key(KeyCode::Esc);
        h.key(KeyCode::Esc);
        assert!(h.app.history.is_none());
        // The tree's row, not the file pane's title
        let (x, y) = h.find("  a.txt");
        h.click(x + 2, y);
        assert!(h.screen().contains("1  two"), "the file as it is now:\n{}", h.screen());
    }

    #[test]
    fn history_leaves_unsaved_changes_on_screen() {
        let h0 = Harness::new(&[("a.txt", "one\n")]);
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&h0.root)
                .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first version"]);
        let mut h = h0;
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('e'));
        h.typing("new ");
        h.key(KeyCode::Esc);
        h.key(KeyCode::Char('H'));
        assert!(h.app.history.is_some(), "the history still opens");
        assert!(h.app.dirty(), "{}", h.screen());
        assert!(h.screen().contains("new one"), "{}", h.screen());
    }

    #[test]
    fn a_click_after_the_pane_widens_while_wrapping() {
        let mut h = Harness::new(&[("a.txt", &LONG.repeat(10))]);
        h.key(KeyCode::Enter);
        h.key(KeyCode::Char('w'));
        h.key(KeyCode::Down);
        h.key(KeyCode::Down);
        assert_eq!(h.app.doc().unwrap().top_row, 2, "the third row of the first line is at the top");
        // A wider pane wraps that line into two rows
        let x = h.app.divider_x;
        h.mouse(MouseEventKind::Down(MouseButton::Left), x, 3);
        h.mouse(MouseEventKind::Drag(MouseButton::Left), 8, 3);
        h.mouse(MouseEventKind::Up(MouseButton::Left), 8, 3);
        let area = h.app.content_area;
        h.click(area.x + 10, area.y);
        assert!(h.app.doc().is_some());
    }
}
