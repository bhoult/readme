//! Handing files to other programs: web pages to the browser, videos to the player.

use std::{io, path::Path};

#[cfg(test)]
thread_local! {
    pub static OPENED: std::cell::RefCell<Vec<std::path::PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn is_web_page(path: &Path) -> bool {
    path.extension().is_some_and(|e| matches!(e.to_string_lossy().to_lowercase().as_str(), "html" | "htm" | "xhtml"))
}

pub fn is_video(path: &Path) -> bool {
    path.extension().is_some_and(|e| {
        matches!(
            e.to_string_lossy().to_lowercase().as_str(),
            "mp4"
                | "m4v"
                | "mkv"
                | "webm"
                | "mov"
                | "avi"
                | "wmv"
                | "flv"
                | "mpg"
                | "mpeg"
                | "ogv"
                | "3gp"
                | "mts"
                | "m2ts"
        )
    })
}

/// Opens `path` in the desktop's default program for it, which for HTML is the browser.
#[cfg(not(test))]
pub fn open(path: &Path) -> io::Result<()> {
    use std::process::{Command, Stdio};

    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut child =
        Command::new(program).arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    // xdg-open hands the file to the browser and exits; reap it without waiting here
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
pub fn open(path: &Path) -> io::Result<()> {
    OPENED.with(|opened| opened.borrow_mut().push(path.to_path_buf()));
    Ok(())
}
