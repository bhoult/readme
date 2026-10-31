//! The backgrounds `c` chooses from, each with a text color that reads on it.

use std::{env, sync::OnceLock};

use ratatui::{
    buffer::Buffer,
    style::{Color, Style},
};

/// A background, and whether it's light, so the text on it is dark.
pub struct Background {
    pub name: &'static str,
    /// None leaves the background and the text to the terminal
    rgb: Option<(u8, u8, u8)>,
    light: bool,
}

const fn dark(name: &'static str, rgb: (u8, u8, u8)) -> Background {
    Background { name, rgb: Some(rgb), light: false }
}

const fn light(name: &'static str, rgb: (u8, u8, u8)) -> Background {
    Background { name, rgb: Some(rgb), light: true }
}

/// The choices, the terminal's own first. The rest are 24-bit colors, since the 256 have
/// nothing between black and the likes of #00005f, a blue that the terminal's own blue
/// text is lost on. The dark ones stay well below the gray of the title bars.
pub static BACKGROUNDS: [Background; 12] = [
    Background { name: "OS default", rgb: None, light: false },
    dark("Black", (0x00, 0x00, 0x00)),
    dark("Charcoal", (0x1c, 0x1c, 0x1c)),
    dark("Midnight", (0x1a, 0x1b, 0x26)),
    dark("Navy", (0x10, 0x1c, 0x36)),
    dark("Teal", (0x00, 0x2b, 0x36)),
    dark("Forest", (0x10, 0x2a, 0x1e)),
    dark("Plum", (0x26, 0x1a, 0x33)),
    dark("Wine", (0x2e, 0x14, 0x19)),
    dark("Espresso", (0x2b, 0x21, 0x19)),
    light("Paper", (0xfd, 0xf6, 0xe3)),
    light("White", (0xff, 0xff, 0xff)),
];

impl Background {
    /// Its color and the text's, or None for the terminal's own.
    pub fn style(&self) -> Option<Style> {
        let (r, g, b) = self.rgb?;
        // The terminal's own text color may not read on it, so it brings one, a gray of
        // the 256 colors
        let text = if self.light { Color::Indexed(234) } else { Color::Indexed(254) };
        Some(Style::new().fg(text).bg(color(r, g, b)))
    }

    /// The color the cursor takes on it, as OSC 12 has it, where the terminal's own might
    /// not show: on a light one, which the terminal's theme most likely wasn't made for.
    pub fn cursor(&self) -> Option<&'static str> {
        self.light.then_some("rgb:1c/1c/1c")
    }

    /// On a light background, text in the terminal's 16 colors, which its theme most
    /// likely made for a dark one, takes darker shades of them that read on it.
    pub fn recolor(&self, buffer: &mut Buffer) {
        if self.light {
            for cell in &mut buffer.content {
                if let Some(darker) = on_light(cell.fg) {
                    cell.fg = darker;
                }
            }
        }
    }
}

/// One of the terminal's 16 colors as a darker shade of it, for text on a light background.
fn on_light(fg: Color) -> Option<Color> {
    let (r, g, b) = match fg {
        Color::Black | Color::Indexed(0) => (0x1c, 0x1c, 0x1c),
        Color::Red | Color::LightRed | Color::Indexed(1 | 9) => (0xb3, 0x26, 0x1e),
        Color::Green | Color::LightGreen | Color::Indexed(2 | 10) => (0x2e, 0x7d, 0x32),
        Color::Yellow | Color::LightYellow | Color::Indexed(3 | 11) => (0x8a, 0x61, 0x00),
        Color::Blue | Color::LightBlue | Color::Indexed(4 | 12) => (0x1f, 0x5f, 0xbf),
        Color::Magenta | Color::LightMagenta | Color::Indexed(5 | 13) => (0x9c, 0x27, 0xb0),
        Color::Cyan | Color::LightCyan | Color::Indexed(6 | 14) => (0x00, 0x79, 0x6b),
        Color::Gray | Color::Indexed(7) => (0x5f, 0x5f, 0x5f),
        Color::DarkGray | Color::Indexed(8) => (0x76, 0x76, 0x76),
        Color::White | Color::Indexed(15) => (0x3a, 0x3a, 0x3a),
        _ => return None,
    };
    Some(color(r, g, b))
}

/// The one called `name`, in any case, as the settings file has it.
pub fn named(name: &str) -> Option<usize> {
    BACKGROUNDS.iter().position(|b| b.name.eq_ignore_ascii_case(name.trim()))
}

/// A 24-bit color where the terminal says it shows them (in COLORTERM), or else the
/// nearest of the 256.
fn color(r: u8, g: u8, b: u8) -> Color {
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    let truecolor = *TRUECOLOR.get_or_init(|| env::var("COLORTERM").is_ok_and(|c| c == "truecolor" || c == "24bit"));
    if truecolor { Color::Rgb(r, g, b) } else { Color::Indexed(nearest_256(r, g, b)) }
}

/// The nearest of the 256 colors' 6×6×6 cube and 24 grays. The first 16 are left out: the
/// terminal's theme decides those.
fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let level = |c: i32| (0..6).min_by_key(|&i| (LEVELS[i] - c).abs()).unwrap_or(0);
    let (lr, lg, lb) = (level(r), level(g), level(b));
    let off = |(x, y, z): (i32, i32, i32)| (x - r).pow(2) + (y - g).pow(2) + (z - b).pow(2);
    // The grays go from 8 to 238, 10 apart
    let gray = ((r + g + b) / 3 - 3).clamp(0, 230) / 10;
    let v = 8 + 10 * gray;
    if off((v, v, v)) < off((LEVELS[lr], LEVELS[lg], LEVELS[lb])) {
        232 + gray as u8
    } else {
        (16 + 36 * lr + 6 * lg + lb) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_go_to_the_nearest_of_the_256() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(0x1c, 0x1c, 0x1c), 234);
        assert_eq!(nearest_256(0x1a, 0x1b, 0x26), 234);
        assert_eq!(nearest_256(0, 0, 0x5f), 17);
        assert_eq!(nearest_256(0xfd, 0xf6, 0xe3), 230);
        assert_eq!(nearest_256(0xff, 0xff, 0xff), 231);
    }

    #[test]
    fn on_a_light_background_the_terminals_colors_go_darker() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 3, 1));
        buffer.content[0].fg = Color::Yellow;
        buffer.content[1].fg = Color::Indexed(4);
        buffer.content[2].fg = Color::Rgb(250, 250, 0);
        let before = buffer.clone();
        BACKGROUNDS[named("Navy").unwrap()].recolor(&mut buffer);
        assert_eq!(buffer, before);
        BACKGROUNDS[named("Paper").unwrap()].recolor(&mut buffer);
        let fgs: Vec<Color> = buffer.content.iter().map(|c| c.fg).collect();
        assert_eq!(fgs, [color(0x8a, 0x61, 0x00), color(0x1f, 0x5f, 0xbf), Color::Rgb(250, 250, 0)]);
    }

    #[test]
    fn names_are_found_in_any_case() {
        assert_eq!(named("navy"), Some(4));
        assert_eq!(named(" os DEFAULT "), Some(0));
        assert_eq!(named("puce"), None);
        assert_eq!(BACKGROUNDS[0].style(), None);
    }

    #[test]
    fn light_ones_get_dark_text_and_dark_ones_stay_below_the_title_bars() {
        for background in &BACKGROUNDS[1..] {
            let (r, g, b) = background.rgb.unwrap();
            let luma = (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000;
            // The unfocused title bar is gray 237, #3a3a3a
            assert!(if background.light { luma > 200 } else { luma < 40 }, "{} at {luma}", background.name);
        }
    }
}
