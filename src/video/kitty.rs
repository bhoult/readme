//! Pictures in Kitty's graphics protocol, placed with Unicode placeholders: cells of
//! U+10EEEE whose foreground colour names the image and whose combining marks give
//! the row and column of it they show. The terminal draws the image in them, scaled
//! to fill them, wherever the text goes, inside herdr or tmux as much as outside.

use std::{collections::VecDeque, fmt::Write as _, io, path::PathBuf};

use base64::Engine as _;
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

const PLACEHOLDER: char = '\u{10EEEE}';

/// The combining marks for rows and columns 0, 1, 2…, from Kitty's
/// rowcolumn-diacritics.txt (as ratatui-image has them)
const DIACRITICS: [u32; 297] = [
    0x305, 0x30D, 0x30E, 0x310, 0x312, 0x33D, 0x33E, 0x33F, 0x346, 0x34A, 0x34B, 0x34C, 0x350, 0x351, 0x352, 0x357,
    0x35B, 0x363, 0x364, 0x365, 0x366, 0x367, 0x368, 0x369, 0x36A, 0x36B, 0x36C, 0x36D, 0x36E, 0x36F, 0x483, 0x484,
    0x485, 0x486, 0x487, 0x592, 0x593, 0x594, 0x595, 0x597, 0x598, 0x599, 0x59C, 0x59D, 0x59E, 0x59F, 0x5A0, 0x5A1,
    0x5A8, 0x5A9, 0x5AB, 0x5AC, 0x5AF, 0x5C4, 0x610, 0x611, 0x612, 0x613, 0x614, 0x615, 0x616, 0x617, 0x657, 0x658,
    0x659, 0x65A, 0x65B, 0x65D, 0x65E, 0x6D6, 0x6D7, 0x6D8, 0x6D9, 0x6DA, 0x6DB, 0x6DC, 0x6DF, 0x6E0, 0x6E1, 0x6E2,
    0x6E4, 0x6E7, 0x6E8, 0x6EB, 0x6EC, 0x730, 0x732, 0x733, 0x735, 0x736, 0x73A, 0x73D, 0x73F, 0x740, 0x741, 0x743,
    0x745, 0x747, 0x749, 0x74A, 0x7EB, 0x7EC, 0x7ED, 0x7EE, 0x7EF, 0x7F0, 0x7F1, 0x7F3, 0x816, 0x817, 0x818, 0x819,
    0x81B, 0x81C, 0x81D, 0x81E, 0x81F, 0x820, 0x821, 0x822, 0x823, 0x825, 0x826, 0x827, 0x829, 0x82A, 0x82B, 0x82C,
    0x82D, 0x951, 0x953, 0x954, 0xF82, 0xF83, 0xF86, 0xF87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75,
    0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71, 0x1B72,
    0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4, 0x1DC5, 0x1DC6, 0x1DC7,
    0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5, 0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA,
    0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0,
    0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1,
    0x2DE0, 0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED,
    0x2DEE, 0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA, 0x2DFB,
    0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1, 0xA8E2, 0xA8E3, 0xA8E4,
    0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED, 0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0,
    0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26,
    0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242,
    0x1D243, 0x1D244,
];

fn diacritic(n: u32) -> char {
    char::from_u32(DIACRITICS[(n as usize).min(DIACRITICS.len() - 1)]).unwrap_or('\u{305}')
}

/// The most rows or columns the marks can number.
pub const MOST: u16 = DIACRITICS.len() as u16;

/// Transmits an RGB picture as image `id`, replacing whatever it was, placed to fill
/// `cols` × `rows` cells of placeholders.
pub fn transmit(id: u32, rgb: &[u8], (width, height): (u32, u32), (cols, rows): (u16, u16)) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(rgb);
    // At most 4096 bytes of base64 a chunk
    let chunks: Vec<&str> = data.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap_or("")).collect();
    let mut out = String::with_capacity(data.len() + chunks.len() * 16 + 80);
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            let _ = write!(
                out,
                "\x1b_Ga=T,U=1,q=2,i={id},p=1,f=24,s={width},v={height},c={cols},r={rows},m={more};{chunk}\x1b\\"
            );
        } else {
            let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
        }
    }
    out
}

/// Transmits the RGB picture in the file at `path` as image `id`, as [`transmit`] does
/// with the picture itself. The terminal deletes the file once it has read it.
pub fn transmit_file(id: u32, path: &std::path::Path, (width, height): (u32, u32), (cols, rows): (u16, u16)) -> String {
    let path = base64::engine::general_purpose::STANDARD.encode(path.as_os_str().as_encoded_bytes());
    format!("\x1b_Ga=T,U=1,q=2,i={id},p=1,f=24,s={width},v={height},c={cols},r={rows},t=t;{path}\x1b\\")
}

/// Pictures handed over in files the terminal reads and deletes (Kitty's `t=t`), so
/// they needn't go through it as text: a frame of video is megabytes of that. Ghostty
/// and kitty take them from /tmp, named with `tty-graphics-protocol`.
pub struct Files {
    /// The latest files written, oldest first, for any the terminal leaves behind
    written: VecDeque<PathBuf>,
}

/// Numbers the files, across all of them this process writes.
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many files may be out at once before the oldest is taken back.
const OUT: usize = 8;

impl Files {
    pub fn new() -> Self {
        Self { written: VecDeque::new() }
    }

    /// `data` in a new file, readable only by this user.
    pub fn write(&mut self, data: &[u8]) -> io::Result<PathBuf> {
        let path = file_path(&NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_string());
        write_new(&path, data)?;
        self.written.push_back(path.clone());
        // One the terminal hasn't taken by now, it isn't going to
        while self.written.len() > OUT {
            if let Some(old) = self.written.pop_front() {
                let _ = std::fs::remove_file(old);
            }
        }
        Ok(path)
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        for path in self.written.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// /tmp/tty-graphics-protocol-readme-PID-`name`
pub fn file_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/tty-graphics-protocol-readme-{}-{name}", std::process::id()))
}

/// Writes `data` to a file that mustn't be there yet, readable only by this user.
pub fn write_new(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(data)
}

/// Places image `id`, already transmitted, to fill `cols` × `rows` cells.
pub fn place(id: u32, (cols, rows): (u16, u16)) -> String {
    format!("\x1b_Ga=p,U=1,q=2,i={id},p=1,c={cols},r={rows}\x1b\\")
}

/// Deletes image `id` and frees its memory in the terminal.
pub fn delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,q=2,i={id}\x1b\\")
}

/// Fills `area` with placeholders for image `id`, with `first` (a transmission or
/// placement) written ahead of them.
pub fn placeholders(buf: &mut Buffer, area: Rect, id: u32, first: Option<&str>) {
    let [extra, r, g, b] = id.to_be_bytes();
    let mut first = first;
    let mut symbol = String::new();
    for y in 0..area.height.min(MOST) {
        for x in 0..area.width.min(MOST) {
            let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) else { continue };
            symbol.clear();
            if let Some(first) = first.take() {
                symbol.push_str(first);
            }
            symbol.push(PLACEHOLDER);
            // The first of a row says which row of which image; the rest follow on
            if x == 0 {
                symbol.push(diacritic(u32::from(y)));
                symbol.push(diacritic(0));
                symbol.push(diacritic(u32::from(extra)));
            }
            cell.set_symbol(&symbol)
                .set_fg(Color::Rgb(r, g, b))
                .set_diff_option(ratatui::buffer::CellDiffOption::ForcedWidth(std::num::NonZeroU16::MIN));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_private_and_left_ones_are_taken_back() {
        let mut files = Files::new();
        let paths: Vec<PathBuf> = (0..OUT + 3).map(|i| files.write(&[i as u8; 6]).unwrap()).collect();
        assert!(paths[0].to_string_lossy().contains("tty-graphics-protocol"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(paths.last().unwrap()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The terminal took one; the oldest it didn't were taken back
        std::fs::remove_file(&paths[5]).unwrap();
        assert!(!paths[0].exists() && !paths[2].exists());
        assert!(paths[3].exists() && paths[OUT + 2].exists());
        drop(files);
        assert!(paths.iter().all(|p| !p.exists()));
    }

    #[test]
    fn transmits_in_chunks_and_places_in_cells() {
        let rgb = vec![7u8; 3 * 40 * 30];
        let sent = transmit(0x0102_0304, &rgb, (40, 30), (8, 3));
        assert!(sent.starts_with("\x1b_Ga=T,U=1,q=2,i=16909060,p=1,f=24,s=40,v=30,c=8,r=3,m=1;"));
        // 3600 bytes are 4800 of base64: a chunk of 4096 and one of the rest
        let chunks: Vec<&str> = sent.split("\x1b\\").filter(|c| !c.is_empty()).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].len(), "\x1b_Gm=0;".len() + 704);
        assert!(chunks[1].starts_with("\x1b_Gm=0;BwcH"));
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 4));
        placeholders(&mut buf, Rect::new(1, 1, 8, 3), 0x0102_0304, Some("<first>"));
        let cell = &buf[(1, 1)];
        assert!(cell.symbol().starts_with("<first>\u{10EEEE}"));
        assert_eq!(cell.fg, Color::Rgb(2, 3, 4));
        assert_eq!(buf[(2, 1)].symbol(), "\u{10EEEE}");
        // Row 2 marks itself as the image's third row
        assert_eq!(buf[(1, 3)].symbol().chars().nth(1), Some(diacritic(2)));
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(place(5, (8, 3)), "\x1b_Ga=p,U=1,q=2,i=5,p=1,c=8,r=3\x1b\\");
        // A file is named by its path in base64
        let sent = transmit_file(5, std::path::Path::new("/tmp/x"), (40, 30), (8, 3));
        assert_eq!(sent, "\x1b_Ga=T,U=1,q=2,i=5,p=1,f=24,s=40,v=30,c=8,r=3,t=t;L3RtcC94\x1b\\");
        assert_eq!(delete(5), "\x1b_Ga=d,d=I,q=2,i=5\x1b\\");
    }
}
