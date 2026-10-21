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
mod highlight;
mod markdown;
mod picture;
mod table;
mod text;
mod tree;

use std::{
    io::{self, Stdout},
    panic,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        cursor::{SetCursorStyle, Show},
        event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
        execute,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    },
};

use app::App;

const USAGE: &str = "\
readme: view and edit files in the terminal

Usage: readme [PATH]

PATH is a directory to browse (default: the current one; its README opens
if it has one) or a file to open.

Keys
  Tab          switch between the tree and the file
  Tree         ↑↓/jk move · Enter open · ←→/hl fold · / filter by name
               F find text in all the files (Esc back to the tree)
               . hidden files · r refresh · H git history
               Space mark (Ctrl+click, Shift+↑↓ for a range) · Del to trash
               F2 rename · drag marked files onto a folder to move them
  File         ↑↓ PgUp PgDn Home End scroll · ← back to the tree
               / search, n/N next/prev
               e edit · w word wrap · m raw/formatted (Markdown, CSV)
               y copy selection
  Editing      Esc stop · Ctrl+S save · Ctrl+Z/Ctrl+Y undo/redo
               Ctrl+C/X/V copy/cut/paste · Ctrl+A select all · Ctrl+F search
               Alt+Z word wrap
  Images       wheel or +/- zoom · 0 fit · drag or arrows to move around
  History      H on a file or folder · ↑↓ commits · Enter its files
               d diff or file · a all files · ← back, or a wider history
               Esc leave
  Anywhere     Ctrl+Q quit (q when not editing)

Mouse
  Click a directory or archive to fold it, a file to open it; double-click a
  file to focus it. Wheel scrolls (Shift+wheel sideways). Drag across text to
  select and copy it. Double-click text to start editing there. Drag the
  divider to resize the tree.

Also: CSV/TSV files show as tables, images show in the terminal, zip and tar
archives (.tar.gz, .tar.bz2, .tar.xz, .tar.zst) open like directories, a
folder with images in it shows them as thumbnails (+/- to resize, Enter or a
click to open one, Esc to go back), and clicking an HTML file or a video (or
Enter on it) opens it in the browser or the video player.

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
        // Only a panic on this thread ends the program. One on another thread (an image
        // decoder on a broken file, say) ends just that thread, so the screen stays as it
        // is, without the message written over it.
        let main = std::thread::current().id();
        panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() != main {
                return;
            }
            restore();
            previous(info);
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
    let _ = execute!(
        io::stdout(),
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
            println!("readme {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(arg) => PathBuf::from(arg),
        None => PathBuf::from("."),
    };
    highlight::preload();
    let mut app = App::new(&target)?;
    let mut tui = Tui::start()?;
    // Asking the terminal about graphics needs raw mode, and has to happen before any input is read
    app.set_picker(picture::picker());
    run(&mut tui.terminal, &mut app)
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    let mut bar_cursor = false;
    let mut last_tick = Instant::now();
    while !app.quit {
        terminal.draw(|f| app.draw(f))?;
        if app.editing() != bar_cursor {
            bar_cursor = app.editing();
            let style = if bar_cursor { SetCursorStyle::BlinkingBar } else { SetCursorStyle::DefaultUserShape };
            execute!(io::stdout(), style)?;
        }
        if event::poll(Duration::from_millis(250))? {
            app.handle(event::read()?);
            // Take whatever else is queued before redrawing, so fast scrolling keeps up
            while !app.quit && event::poll(Duration::ZERO)? {
                app.handle(event::read()?);
            }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            app.tick();
            last_tick = Instant::now();
        }
    }
    Ok(())
}
