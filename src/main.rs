//! readme: a terminal file viewer and basic editor. A directory tree on the left,
//! the file on the right, with the mouse working throughout.

mod app;
mod archive;
mod buffer;
mod clipboard;
mod external;
mod fileops;
mod gallery;
mod git;
mod grep;
mod help;
mod highlight;
mod markdown;
mod picture;
mod table;
mod text;
mod tree;
mod video;

use std::{
    env,
    io::{self, Stdout},
    panic,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::Result;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        cursor::{SetCursorStyle, Show},
        event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event},
        execute,
        terminal::{self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen},
    },
};

use app::App;
use picture::LateAnswers;

/// What a panic on another thread said, for the screen to show
static BACKGROUND_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// What tmux's allow-passthrough was for this pane before readme started, empty if the
/// pane didn't set it. ratatui-image turns it on whenever it's set up inside tmux and
/// leaves it on, which would let anything run in the pane later write escape sequences
/// straight to the terminal outside, so it's put back on the way out.
static PASSTHROUGH: OnceLock<String> = OnceLock::new();

fn save_passthrough() {
    let tmux =
        env::var("TERM").is_ok_and(|t| t.starts_with("tmux")) || env::var("TERM_PROGRAM").is_ok_and(|p| p == "tmux");
    if !tmux {
        return;
    }
    let shown = Command::new("tmux")
        .args(["show-options", "-p", "-v", "allow-passthrough"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    // A tmux too old to have the option can't have had it turned on either
    if let Some(out) = shown.ok().filter(|out| out.status.success()) {
        let _ = PASSTHROUGH.set(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
}

fn restore_passthrough() {
    let Some(was) = PASSTHROUGH.get() else { return };
    let mut tmux = Command::new("tmux");
    if was.is_empty() {
        tmux.args(["set-option", "-p", "-u", "allow-passthrough"]);
    } else {
        tmux.args(["set-option", "-p", "allow-passthrough", was]);
    }
    let _ = tmux.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

const USAGE: &str = "\
readme: view and edit files in the terminal

Usage: readme [PATH]

PATH is a directory to browse (default: the current one; its README opens
if it has one) or a file to open.

Keys
  Tab          switch between the tree and the file
  Tree         ↑↓/jk move · Enter open · ←→/hl fold · / filter by name
               Backspace or .. up a folder
               s sort: by name, biggest, oldest or newest first
               F find text in all the files (Esc back to the tree)
               . hidden files · r refresh · H git history · B branches
               m mark (Ctrl+click, Shift+↑↓ for a range) · Del to trash
               F2 rename · drag marked files onto a folder to move them
  File         ↑↓ PgUp PgDn Home End scroll · ← back to the tree
               / search, n/N next/prev
               e edit · w word wrap · m raw/formatted (Markdown, CSV)
               y copy selection
  Editing      Esc stop · Ctrl+S save · Ctrl+Z/Ctrl+Y undo/redo
               Ctrl+C/X/V copy/cut/paste · Ctrl+A select all · Ctrl+F search
               Alt+Z word wrap
  Images       wheel or +/- zoom · 0 fit · drag or arrows to move around
  Videos       Space play/pause (in the tree too) · ←→ 5 s · ↑↓ 1 min
               Home start · m sound · +/- volume · Esc back to the tree
               Enter or o the video player, pausing it here
  History      H on a file or folder · ↑↓ commits · Enter its files
               d diff or file · a all files · ← back, or a wider history
               B its branches · Esc leave
  Branches     B on a file or folder: every branch, with its owner and when it
               was last updated, those that changed it first
               ↑↓ branches · Enter the files it changed · d diff or file
               a all the files it changed · ← back, or wider · H its history
               s newest first, by name or by owner
               f all, local or remote branches (fetching the remote's)
               Esc leave
  Anywhere     ? help, with the version (F1 while typing)
               Ctrl+Q or Ctrl+C quit (q when not editing; while editing,
               Ctrl+C copies the selected text if there is some)

Mouse
  Click a directory or archive to fold it, a file to open it; double-click a
  file to focus it. Wheel scrolls (Shift+wheel sideways), and over a video
  turns its sound up or down. Drag across text to select and copy it.
  Double-click text to start editing there. Drag the divider to resize the
  tree.

Also: CSV/TSV files show as tables, images show in the terminal, zip and tar
archives (.tar.gz, .tar.bz2, .tar.xz, .tar.zst) open like directories, a
folder with images in it shows them as thumbnails (+/- to resize, Enter or a
click to open one, Esc to go back), MP4 and MOV videos with H.264 play in the
pane with their sound, and Enter on an HTML file or a video opens it in the
browser or the video player; a click never does.

Environment
  README_THEME   syntax theme from bat's set (default: ansi, the terminal's colors)
  README_IMAGES  halfblocks, kitty, sixel, iterm2 or auto. Inside tmux or zellij
                 the default is halfblocks, since they claim graphics support the
                 terminal around them may not have. Inside herdr, images use real
                 pixels when every herdr window runs in Ghostty, WezTerm or kitty.
";

/// Raw mode, the alternate screen and mouse capture, undone however the program ends.
struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    fn start() -> Result<Self> {
        let previous = panic::take_hook();
        // Only a panic on this thread ends the program. One on another thread ends just
        // that thread, so the screen stays as it is and says so in its status line,
        // rather than have the message written over it. Thumbnail decoders say it
        // themselves, by showing "can't read", and a video says its decoder stopped.
        let main = std::thread::current().id();
        let quiet = [gallery::WORKER, video::PICTURES, video::SOUND];
        panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            if thread.id() == main {
                restore();
                previous(info);
            } else if !thread.name().is_some_and(|name| quiet.contains(&name)) {
                let at = info.location().map(|l| format!(" ({}:{})", l.file(), l.line())).unwrap_or_default();
                let payload = info.payload();
                let message =
                    payload.downcast_ref::<&str>().copied().or(payload.downcast_ref::<String>().map(String::as_str));
                let what = format!("{}{at}", message.unwrap_or("it stopped"));
                if let Ok(mut last) = BACKGROUND_PANIC.lock() {
                    *last = Some(what);
                }
            }
        }));
        terminal::enable_raw_mode()?;
        let setup = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)
            .map_err(anyhow::Error::from)
            .and_then(|()| Ok(Terminal::new(CrosstermBackend::new(io::stdout()))?));
        match setup {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                restore();
                Err(e)
            }
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    restore_passthrough();
    let _ = execute!(
        io::stdout(),
        EndSynchronizedUpdate,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        SetCursorStyle::DefaultUserShape,
        Show
    );
    let _ = terminal::disable_raw_mode();
}

/// How many threads the machine can run at once.
pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

fn main() -> Result<()> {
    let target = match std::env::args_os().nth(1) {
        Some(arg) if arg == "-h" || arg == "--help" => {
            print!("{USAGE}");
            return Ok(());
        }
        Some(arg) if arg == "-V" || arg == "--version" => {
            println!("{}", help::version());
            return Ok(());
        }
        Some(arg) => PathBuf::from(arg),
        None => PathBuf::from("."),
    };
    save_passthrough();
    highlight::preload();
    let mut app = App::new(&target)?;
    let mut tui = Tui::start()?;
    // Asking the terminal about graphics needs raw mode, and has to happen before any input is read
    let (picker, late) = picture::picker();
    app.set_picker(picker);
    run(&mut tui.terminal, &mut app, late.then(LateAnswers::new))
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App, mut late: Option<LateAnswers>) -> Result<()> {
    let mut handle = |app: &mut App, event: Event| {
        // The terminal's answer to the graphics question, come too late, isn't typing
        if !late.as_mut().is_some_and(|late| late.swallows(&event)) {
            app.handle(event);
        }
    };
    let mut bar_cursor = false;
    let mut last_tick = Instant::now();
    while !app.quit {
        let started = Instant::now();
        // Terminals that can hold each frame back until all of it is there, so a video's
        // picture never shows half of one and half of the next
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        terminal.draw(|f| app.draw(f))?;
        execute!(io::stdout(), EndSynchronizedUpdate)?;
        app.drew(started.elapsed());
        if app.editing() != bar_cursor {
            bar_cursor = app.editing();
            let style = if bar_cursor { SetCursorStyle::BlinkingBar } else { SetCursorStyle::DefaultUserShape };
            execute!(io::stdout(), style)?;
        }
        // Straight back to drawing while there's more to color in, and back for a video's
        // next picture when it's due, unless a key comes first
        let idle = Duration::from_millis(250);
        let wait = if app.busy() { Duration::ZERO } else { app.due().map_or(idle, |due| due.min(idle)) };
        if event::poll(wait)? {
            handle(app, event::read()?);
            // Take whatever else is queued before redrawing, so fast scrolling keeps up
            while !app.quit && event::poll(Duration::ZERO)? {
                handle(app, event::read()?);
            }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            app.tick();
            last_tick = Instant::now();
        }
        if let Some(what) = BACKGROUND_PANIC.lock().ok().and_then(|mut last| last.take()) {
            app.say(format!("Something went wrong in the background: {what}"));
        }
    }
    Ok(())
}
