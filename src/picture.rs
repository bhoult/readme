//! Images, drawn with the terminal's graphics protocol where it has one, or with
//! half-block characters in true color where it doesn't.

use std::{env, path::Path, time::Duration};

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
/// (herdr does inside GNOME Terminal), so inside one this keeps to half-blocks unless
/// README_IMAGES asks for kitty, sixel or iterm2 (or auto, to ask the terminal).
pub fn picker() -> Picker {
    let wanted = env::var("README_IMAGES").unwrap_or_default().to_lowercase();
    let multiplexed = ["HERDR_ENV", "TMUX", "ZELLIJ", "STY"].iter().any(|v| env::var_os(v).is_some());
    let mut picker = match wanted.as_str() {
        "halfblocks" | "blocks" => Picker::halfblocks(),
        "" if multiplexed => Picker::halfblocks(),
        // A terminal that doesn't answer would otherwise hold up startup for two seconds
        _ => Picker::from_query_stdio_with_options(QueryStdioOptions {
            timeout: Duration::from_millis(400),
            ..Default::default()
        })
        .unwrap_or_else(|_| Picker::halfblocks()),
    };
    let forced = match wanted.as_str() {
        "kitty" => Some(ProtocolType::Kitty),
        "sixel" => Some(ProtocolType::Sixel),
        "iterm2" | "iterm" => Some(ProtocolType::Iterm2),
        _ => None,
    };
    if let Some(protocol) = forced {
        picker.set_protocol_type(protocol);
    }
    picker
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
