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
    FilterType, FontSize, Resize, StatefulImage,
    picker::{Picker, ProtocolType},
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
        _ => ask_terminal().unwrap_or_else(Picker::halfblocks),
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

/// Asks the terminal whether it shows Kitty graphics or sixels, and how big its
/// character cells are. ratatui-image can ask too, but it waits on a thread that's
/// left reading the keyboard when a terminal never answers; this waits here, with a
/// deadline, so nothing is left behind.
#[cfg(unix)]
fn ask_terminal() -> Option<Picker> {
    use std::io::Write;
    let mut out = std::io::stdout();
    // Kitty graphics, the cell size, then the device attributes every terminal answers,
    // which come back last and end the wait
    out.write_all(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[c").ok()?;
    out.flush().ok()?;
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let mut answers = Vec::new();
    while answers_end(&answers).is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let mut stdin = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, then a read into a buffer of the length given
        let ready = unsafe { libc::poll(&mut stdin, 1, left.as_millis() as i32) };
        if ready <= 0 {
            return None;
        }
        let mut chunk = [0u8; 512];
        let read = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if read <= 0 {
            return None;
        }
        answers.extend_from_slice(&chunk[..read as usize]);
    }
    let (kitty, sixel, cell) = parse_answers(&answers);
    // Without the cell size in an answer, the window's size in pixels gives it
    let cell = cell.or_else(|| {
        let size = ratatui::crossterm::terminal::window_size().ok()?;
        (size.columns > 0 && size.rows > 0).then(|| (size.width / size.columns, size.height / size.rows))
    });
    let protocol = if kitty {
        ProtocolType::Kitty
    } else if sixel {
        ProtocolType::Sixel
    } else if env::var("TERM_PROGRAM").is_ok_and(|p| p == "iTerm.app") {
        ProtocolType::Iterm2
    } else {
        ProtocolType::Halfblocks
    };
    let (width, height) = cell.filter(|&c| sane_cell(c)).unwrap_or((10, 20));
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(FontSize::new(width, height));
    picker.set_protocol_type(protocol);
    Some(picker)
}

#[cfg(not(unix))]
fn ask_terminal() -> Option<Picker> {
    None
}

/// Where the device attributes answer (`ESC [ ? … c`) ends, once it has arrived.
fn answers_end(answers: &[u8]) -> Option<usize> {
    let start = answers.windows(3).position(|w| w == b"\x1b[?")?;
    answers[start + 3..].iter().position(|&b| b == b'c').map(|i| start + 3 + i)
}

/// From the terminal's answers: Kitty graphics, sixels, and the cell size in pixels.
fn parse_answers(answers: &[u8]) -> (bool, bool, Option<(u16, u16)>) {
    let text = String::from_utf8_lossy(answers);
    let kitty = text.contains("\x1b_Gi=31;OK");
    let sixel = text
        .split("\x1b[?")
        .nth(1)
        .and_then(|rest| rest.split('c').next())
        .is_some_and(|attrs| attrs.split(';').any(|a| a == "4"));
    // ESC [ 6 ; height ; width t
    let cell = text.split("\x1b[6;").nth(1).and_then(|rest| {
        let (numbers, _) = rest.split_once('t')?;
        let (height, width) = numbers.split_once(';')?;
        Some((width.parse().ok()?, height.parse().ok()?))
    });
    (kitty, sixel, cell.filter(|&c| sane_cell(c)))
}

/// A character cell's size in pixels that's worth believing. Terminals that don't know
/// answer 0×0, and the image code's arithmetic runs away with a zero (a test once had
/// it allocating until the machine ran out of memory).
fn sane_cell((width, height): (u16, u16)) -> bool {
    (2..=200).contains(&width) && (4..=400).contains(&height)
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
    image: DynamicImage,
    picker: Picker,
    /// Half-blocks have about two pixels to a character, so images are scaled up to
    /// fill the pane; real graphics show them no bigger than they are
    fill: bool,
    /// 1 is the image fitted to the pane; more is zoomed in
    pub zoom: f32,
    /// The point of the image at the middle of the view, as fractions of its size
    center: (f32, f32),
    /// The view drawn last, and its protocol, rebuilt only when the view changes
    view: Option<(View, StatefulProtocol)>,
}

/// The part of the image on screen (in image pixels), where it was drawn, and at how
/// many screen pixels to an image pixel.
#[derive(Clone, Copy, PartialEq)]
struct View {
    crop: (u32, u32, u32, u32),
    at: Rect,
    scale: f32,
}

const ZOOMS: (f32, f32) = (0.1, 50.0);

impl Picture {
    pub fn new(image: DynamicImage, format: &str, picker: &Picker) -> Self {
        let info = format!("{}×{} {}", image.width(), image.height(), format.to_uppercase());
        let fill = picker.protocol_type() == ProtocolType::Halfblocks;
        let font = picker.font_size();
        let picker = if sane_cell((font.width, font.height)) {
            picker.clone()
        } else {
            #[allow(deprecated)]
            let mut sane = Picker::from_fontsize(FontSize::new(10, 20));
            sane.set_protocol_type(picker.protocol_type());
            sane
        };
        Self { info, image, picker, fill, zoom: 1.0, center: (0.5, 0.5), view: None }
    }

    fn cell(&self) -> (f32, f32) {
        let font = self.picker.font_size();
        (font.width.max(1) as f32, font.height.max(1) as f32)
    }

    /// What of the image fits in `area` at the current zoom, centered there.
    fn layout(&mut self, area: Rect) -> View {
        let (cell_w, cell_h) = self.cell();
        let (width, height) = (self.image.width().max(1) as f32, self.image.height().max(1) as f32);
        let (room_w, room_h) = (area.width as f32 * cell_w, area.height as f32 * cell_h);
        let mut fit = (room_w / width).min(room_h / height);
        if !self.fill {
            fit = fit.min(1.0);
        }
        let scale = fit * self.zoom;
        let (view_w, view_h) = ((room_w / scale).min(width), (room_h / scale).min(height));
        let x = (self.center.0 * width - view_w / 2.0).clamp(0.0, width - view_w);
        let y = (self.center.1 * height - view_h / 2.0).clamp(0.0, height - view_h);
        // Panning stops at the edges
        self.center = ((x + view_w / 2.0) / width, (y + view_h / 2.0) / height);
        let cols = ((view_w * scale / cell_w).round() as u16).clamp(1, area.width);
        let rows = ((view_h * scale / cell_h).round() as u16).clamp(1, area.height);
        let at = Rect::new(area.x + (area.width - cols) / 2, area.y + (area.height - rows) / 2, cols, rows);
        let crop = (x as u32, y as u32, (view_w as u32).max(1), (view_h as u32).max(1));
        View { crop, at, scale }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let view = self.layout(area);
        if self.view.as_ref().is_none_or(|(v, _)| v.crop != view.crop || v.at.as_size() != view.at.as_size()) {
            // Scaled straight to the screen's size from the part on view, so a big image
            // isn't copied whole for every view of it
            let (x, y, w, h) = view.crop;
            let (cell_w, cell_h) = self.cell();
            let size = (
                ((view.at.width as f32 * cell_w) as u32).clamp(1, w.max(1) * 50),
                ((view.at.height as f32 * cell_h) as u32).clamp(1, h.max(1) * 50),
            );
            let part = image::imageops::crop_imm(&self.image, x, y, w, h);
            let screen = image::imageops::resize(&*part, size.0, size.1, FilterType::Triangle);
            self.view = Some((view, self.picker.new_resize_protocol(DynamicImage::ImageRgba8(screen))));
        }
        let Some((last, protocol)) = &mut self.view else { return };
        *last = view;
        f.render_stateful_widget(
            StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle))),
            view.at,
            protocol,
        );
    }

    /// Zooms by `factor`. With a screen cell, the part of the image under it stays put.
    pub fn zoom_by(&mut self, factor: f32, at: Option<(u16, u16)>) {
        let zoom = (self.zoom * factor).clamp(ZOOMS.0, ZOOMS.1);
        if let (Some((x, y)), Some((view, _))) = (at, &self.view) {
            let (cell_w, cell_h) = self.cell();
            let (width, height) = (self.image.width() as f32, self.image.height() as f32);
            // Screen pixels from the middle of the drawn image to the pointer
            let dx = (x as f32 + 0.5 - (view.at.x as f32 + view.at.width as f32 / 2.0)) * cell_w;
            let dy = (y as f32 + 0.5 - (view.at.y as f32 + view.at.height as f32 / 2.0)) * cell_h;
            let (crop_x, crop_y, crop_w, crop_h) = view.crop;
            let under = (
                crop_x as f32 + crop_w as f32 / 2.0 + dx / view.scale,
                crop_y as f32 + crop_h as f32 / 2.0 + dy / view.scale,
            );
            let scale = view.scale * zoom / self.zoom;
            self.center = ((under.0 - dx / scale) / width, (under.1 - dy / scale) / height);
        }
        self.zoom = zoom;
    }

    pub fn reset(&mut self) {
        (self.zoom, self.center) = (1.0, (0.5, 0.5));
    }

    /// Moves the view by a fraction of its own size.
    pub fn pan(&mut self, across: f32, down: f32) {
        let Some((view, _)) = &self.view else { return };
        let (_, _, w, h) = view.crop;
        self.center.0 += across * w as f32 / self.image.width().max(1) as f32;
        self.center.1 += down * h as f32 / self.image.height().max(1) as f32;
    }

    /// Moves the view with the mouse, by screen cells.
    pub fn drag(&mut self, cols: i32, rows: i32) {
        let Some((view, _)) = &self.view else { return };
        let (cell_w, cell_h) = self.cell();
        self.center.0 -= cols as f32 * cell_w / view.scale / self.image.width().max(1) as f32;
        self.center.1 -= rows as f32 * cell_h / view.scale / self.image.height().max(1) as f32;
    }

    /// Whether there's more of the image off to the left.
    pub fn more_left(&self) -> bool {
        self.view.as_ref().is_some_and(|(view, _)| view.crop.0 > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shows(vars: &[(&'static str, &'static str)]) -> bool {
        terminal_shows_kitty(|name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| *v))
    }

    #[test]
    fn a_zero_cell_size_is_not_believed() {
        assert_eq!(parse_answers(b"\x1b[6;0;0t\x1b[?62;22c").2, None);
        #[allow(deprecated)]
        let mut zero = Picker::from_fontsize(FontSize::new(0, 0));
        zero.set_protocol_type(ProtocolType::Halfblocks);
        let mut picture = Picture::new(DynamicImage::new_rgb8(64, 48), "png", &zero);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        term.draw(|f| picture.draw(f, Rect::new(0, 0, 60, 20))).unwrap();
    }

    #[test]
    fn reads_the_terminals_answers() {
        let ghostty = b"\x1b_Gi=31;OK\x1b\\\x1b[6;21;10t\x1b[?62;22;52c";
        assert_eq!(parse_answers(ghostty), (true, false, Some((10, 21))));
        assert!(answers_end(ghostty).is_some());
        let sixel = b"\x1b[?65;4;6;22c";
        assert_eq!(parse_answers(sixel), (false, true, None));
        assert!(answers_end(b"\x1b_Gi=31;OK\x1b\\").is_none(), "still waiting for the last answer");
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
