//! Images, drawn with the terminal's graphics protocol where it has one, or with
//! half-block characters in true color where it doesn't.

use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use image::DynamicImage;
use ratatui::{Frame, layout::Rect};
use ratatui_image::{
    FilterType, Resize, StatefulImage,
    picker::{Picker, ProtocolType, cap_parser::QueryStdioOptions},
    protocol::StatefulProtocol,
};

const EXTENSIONS: [&str; 16] =
    ["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "tga", "qoi", "pnm", "ppm", "pgm", "pbm", "hdr"];

pub fn is_image(path: &Path) -> bool {
    path.extension().is_some_and(|e| EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str()))
}

/// How images get drawn, chosen once at startup. A terminal multiplexer answers
/// graphics queries itself even when the terminal around it can't show the result
/// (herdr does inside GNOME Terminal), so inside one this keeps to half-blocks. The
/// exception is herdr whose windows all run in a terminal it passes images on to.
/// README_IMAGES overrides all this: kitty, sixel, iterm2, halfblocks, or auto to ask
/// the terminal.
pub fn picker() -> Picker {
    let wanted = env::var("README_IMAGES").unwrap_or_default().to_lowercase();
    let multiplexed = ["HERDR_ENV", "TMUX", "ZELLIJ", "STY"].iter().any(|v| env::var_os(v).is_some());
    let herdr_shows_images = wanted.is_empty() && env::var_os("HERDR_ENV").is_some() && herdr_windows_show_images();
    let mut picker = match wanted.as_str() {
        "halfblocks" | "blocks" => Picker::halfblocks(),
        "" if multiplexed && !herdr_shows_images => Picker::halfblocks(),
        // A terminal that doesn't answer would otherwise hold up startup for two seconds
        _ => Picker::from_query_stdio_with_options(QueryStdioOptions {
            timeout: Duration::from_millis(400),
            ..Default::default()
        })
        .unwrap_or_else(|_| Picker::halfblocks()),
    };
    let forced = match wanted.as_str() {
        "kitty" => Some(ProtocolType::Kitty),
        "" if herdr_shows_images => Some(ProtocolType::Kitty),
        "sixel" => Some(ProtocolType::Sixel),
        "iterm2" | "iterm" => Some(ProtocolType::Iterm2),
        _ => None,
    };
    if let Some(protocol) = forced {
        picker.set_protocol_type(protocol);
    }
    picker
}

/// Whether every herdr window attached to this pane's session runs in a terminal herdr
/// passes Kitty graphics on to. This is herdr's own test, made on the windows' herdr
/// processes, since a pane can't ask its way past herdr.
#[cfg(target_os = "linux")]
fn herdr_windows_show_images() -> bool {
    // ~/.config/herdr/herdr.sock, or ~/.config/herdr/sessions/<name>/herdr.sock
    let socket = PathBuf::from(env::var_os("HERDR_SOCKET_PATH").unwrap_or_default());
    let dir = socket.parent();
    let session = dir
        .filter(|d| d.parent().and_then(Path::file_name).is_some_and(|n| n == "sessions"))
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned());
    let Ok(processes) = std::fs::read_dir("/proc") else { return false };
    let mut windows = 0;
    for process in processes.flatten() {
        let Ok(cmdline) = std::fs::read(process.path().join("cmdline")) else { continue };
        let args: Vec<String> = cmdline
            .split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let is_herdr = args.first().and_then(|a| Path::new(a).file_name()).is_some_and(|n| n == "herdr");
        if !is_herdr {
            continue;
        }
        // Windows attached to a session; not the server, other commands, or remote ones
        let attached = match &args[1..] {
            [] => None,
            [flag, name] if flag == "--session" => Some(name.clone()),
            [command, attach, name] if command == "session" && attach == "attach" => Some(name.clone()),
            _ => continue,
        };
        if attached != session {
            continue;
        }
        let Ok(environ) = std::fs::read(process.path().join("environ")) else { continue };
        let vars: Vec<(String, String)> = environ
            .split(|&b| b == 0)
            .filter_map(|v| String::from_utf8_lossy(v).split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect();
        let var = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        if !terminal_shows_kitty(var) {
            return false;
        }
        windows += 1;
    }
    windows > 0
}

#[cfg(not(target_os = "linux"))]
fn herdr_windows_show_images() -> bool {
    false
}

/// herdr 0.9's test for passing Kitty graphics to the terminal a window runs in.
fn terminal_shows_kitty<'a>(var: impl Fn(&str) -> Option<&'a str>) -> bool {
    let program = var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let capable = program == "ghostty"
        || program == "wezterm"
        || matches!(var("TERM"), Some("xterm-ghostty" | "xterm-kitty" | "xterm-wezterm"))
        || var("KITTY_WINDOW_ID").is_some();
    let relayed = ["SSH_CONNECTION", "SSH_TTY", "TMUX", "STY"].iter().any(|v| var(v).is_some());
    capable && !relayed
}

pub struct Picture {
    /// "1920×1080 PNG"
    pub info: String,
    protocol: StatefulProtocol,
    /// Half-blocks have about two pixels to a character, so images are scaled up to
    /// fill the pane; real graphics show them no bigger than they are
    fill: bool,
}

impl Picture {
    pub fn new(image: DynamicImage, format: &str, picker: &Picker) -> Self {
        let info = format!("{}×{} {}", image.width(), image.height(), format.to_uppercase());
        let fill = picker.protocol_type() == ProtocolType::Halfblocks;
        Self { info, protocol: picker.new_resize_protocol(image), fill }
    }

    /// Fitted into `area` and centered.
    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let fill = self.fill;
        let resize = || {
            if fill { Resize::Scale(Some(FilterType::Triangle)) } else { Resize::Fit(Some(FilterType::Triangle)) }
        };
        let size = self.protocol.size_for(resize(), area.as_size());
        let x = area.x + area.width.saturating_sub(size.width) / 2;
        let y = area.y + area.height.saturating_sub(size.height) / 2;
        let at = Rect::new(x, y, size.width.min(area.width), size.height.min(area.height));
        f.render_stateful_widget(StatefulImage::default().resize(resize()), at, &mut self.protocol);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shows(vars: &[(&'static str, &'static str)]) -> bool {
        terminal_shows_kitty(|name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| *v))
    }

    #[test]
    fn which_terminals_get_images() {
        assert!(shows(&[("TERM_PROGRAM", "ghostty"), ("TERM", "xterm-ghostty")]));
        assert!(shows(&[("TERM", "xterm-kitty")]));
        assert!(shows(&[("TERM_PROGRAM", "WezTerm")]));
        assert!(!shows(&[("TERM", "xterm-256color"), ("VTE_VERSION", "8400")]));
        assert!(!shows(&[("TERM_PROGRAM", "ghostty"), ("SSH_TTY", "/dev/pts/3")]));
    }
}
