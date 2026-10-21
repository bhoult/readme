//! The system clipboard: wl-copy/wl-paste on Wayland, xclip or xsel on X11, and the
//! OSC 52 escape sequence otherwise, which reaches the terminal's clipboard over ssh too.

#[cfg(not(test))]
use std::io::Write;
use std::process::{Command, Stdio};

#[cfg(not(test))]
use base64::Engine;

#[cfg(test)]
thread_local! {
    pub static COPIED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Copies `text`; with `primary` also to the primary selection, as herdr's
/// copy_on_select does, so a middle click pastes it.
pub fn copy(text: &str, primary: bool) {
    #[cfg(test)]
    {
        let _ = primary;
        COPIED.with(|c| c.borrow_mut().push(text.to_string()));
    }
    #[cfg(not(test))]
    {
        let copied = if wayland() {
            if primary {
                pipe("wl-copy", &["--primary"], text);
            }
            pipe("wl-copy", &[], text)
        } else if x11() {
            if primary {
                let _ = pipe("xclip", &["-selection", "primary"], text) || pipe("xsel", &["-ip"], text);
            }
            pipe("xclip", &["-selection", "clipboard"], text) || pipe("xsel", &["-ib"], text)
        } else {
            false
        };
        if !copied {
            osc52(text);
        }
    }
}

/// The clipboard (or primary selection) contents, where a tool can read them.
pub fn paste(primary: bool) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let run = |program: &str, args: &[&str]| {
        Command::new(program).args(args).stderr(Stdio::null()).output().ok().filter(|o| o.status.success())
    };
    let output = if wayland() {
        let mut args = vec!["--no-newline"];
        if primary {
            args.push("--primary");
        }
        run("wl-paste", &args)
    } else if x11() {
        // Either tool, as copy uses
        let (which, flag) = if primary { ("primary", "-p") } else { ("clipboard", "-b") };
        run("xclip", &["-o", "-selection", which]).or_else(|| run("xsel", &["-o", flag]))
    } else {
        None
    }?;
    String::from_utf8(output.stdout).ok().filter(|s| !s.is_empty())
}

fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn x11() -> bool {
    std::env::var_os("DISPLAY").is_some()
}

/// Feeds `text` to a clipboard tool. The tool may keep running to serve the
/// clipboard, so it's left to finish on its own.
#[cfg(not(test))]
fn pipe(program: &str, args: &[&str], text: &str) -> bool {
    use std::time::{Duration, Instant};

    let child =
        Command::new(program).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let Ok(mut child) = child else { return false };
    let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    // wl-copy and xclip fail straight away when they can't reach the display (a stale
    // WAYLAND_DISPLAY or DISPLAY in a long-lived session), and otherwise fork to serve the
    // clipboard. A failure has to count, or OSC 52 never gets its turn.
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return written && status.success(),
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break,
        }
    }
    std::thread::spawn(move || child.wait());
    written
}

#[cfg(not(test))]
fn osc52(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}
