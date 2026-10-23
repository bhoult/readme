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
        // X11's tools when Wayland's can't do it: not installed (XWayland has the same
        // clipboard), or a stale WAYLAND_DISPLAY in a long-lived session
        let wayland = wayland() && {
            if primary {
                pipe("wl-copy", &["--primary"], text);
            }
            pipe("wl-copy", &[], text)
        };
        let copied = wayland
            || x11() && {
                if primary {
                    let _ = pipe("xclip", &["-selection", "primary"], text) || pipe("xsel", &["-ip"], text);
                }
                pipe("xclip", &["-selection", "clipboard"], text) || pipe("xsel", &["-ib"], text)
            };
        if !copied {
            osc52(text);
        }
    }
}

/// The clipboard (or primary selection) contents, where a tool can read them: None
/// when there's nothing there to paste, or why it couldn't be read.
pub fn paste(primary: bool) -> Result<Option<String>, &'static str> {
    if cfg!(test) {
        return Ok(None);
    }
    let mut output = None;
    if wayland() {
        let mut args = vec!["--no-newline"];
        if primary {
            args.push("--primary");
        }
        output = read("wl-paste", &args)?;
    }
    // As copy does: X11's tools when there's no wl-paste, or WAYLAND_DISPLAY is stale
    if output.is_none() && x11() {
        let (which, flag) = if primary { ("primary", "-p") } else { ("clipboard", "-b") };
        output = match read("xclip", &["-o", "-selection", which])? {
            Some(output) => Some(output),
            None => read("xsel", &["-o", flag])?,
        };
    }
    Ok(output.and_then(|o| String::from_utf8(o).ok()).filter(|s| !s.is_empty()))
}

/// What a clipboard tool prints (None if it fails), if it's done within a couple of
/// seconds. The text comes from whichever program holds the clipboard, and one that has
/// hung would hang the tool, and readme waiting on it, for good.
fn read(program: &str, args: &[&str]) -> Result<Option<Vec<u8>>, &'static str> {
    use std::{io::Read, sync::mpsc, time::Duration};

    let Ok(mut child) =
        Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
    else {
        return Ok(None);
    };
    let Some(mut stdout) = child.stdout.take() else { return Ok(None) };
    let (sender, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = sender.send(stdout.read_to_end(&mut out).map(|_| out));
    });
    match received.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(out)) if child.wait().is_ok_and(|status| status.success()) => Ok(Some(out)),
        Ok(_) => {
            let _ = child.wait();
            Ok(None)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err("The clipboard didn't answer: the program holding it may be stuck")
        }
    }
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
    let Some(mut stdin) = child.stdin.take() else { return false };
    // Written from another thread: a tool that never reads what it's given (xclip
    // waiting on a display that doesn't answer, say) would otherwise hold readme for good
    let text = text.to_owned();
    let writer = std::thread::spawn(move || stdin.write_all(text.as_bytes()).is_ok());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !writer.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !writer.is_finished() {
        // It isn't taking it, so it's stopped, and the text goes another way
        let _ = child.kill();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = writer.join();
        });
        return false;
    }
    let written = writer.join().unwrap_or(false);
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
