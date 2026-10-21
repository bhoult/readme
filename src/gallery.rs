//! A folder's images as a grid of thumbnails, decoded in the background as they come
//! into view.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

use image::DynamicImage;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::Paragraph,
};
use ratatui_image::{
    FilterType, Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};

use crate::{picture, text::str_width};

/// Columns between thumbnails
const GAP: u16 = 2;
/// Thumbnails are decoded this big at most, so resizing them doesn't decode again
const DECODED: (u32, u32) = (400, 800);
/// Portrait images are cut off at this many times their width
const TALLEST: f32 = 2.0;
/// Files bigger than this aren't decoded for a thumbnail
const MAX_BYTES: u64 = 50 << 20;
pub const WIDTHS: (u32, u32) = (50, 400);

/// The images directly in `dir`, by name.
pub fn images_in(dir: &Path) -> Vec<PathBuf> {
    let mut images: Vec<PathBuf> =
        fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| picture::is_image(p)).collect();
    images.retain(|p| p.is_file());
    images.sort_by_cached_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    images
}

enum Thumb {
    Waiting,
    Loading,
    Failed,
    Ready {
        protocol: Box<StatefulProtocol>,
        /// Height over width
        aspect: f32,
    },
}

pub struct Gallery {
    pub dir: PathBuf,
    pub images: Vec<PathBuf>,
    thumbs: Vec<Thumb>,
    pub selected: usize,
    /// First row of the grid on screen
    top: usize,
    /// Scroll so the selection shows; off after the wheel scrolls
    follow: bool,
    /// Thumbnail width in pixels
    pub width_px: u32,
    jobs: mpsc::Sender<(usize, PathBuf)>,
    done: mpsc::Receiver<(usize, Option<DynamicImage>)>,
    /// Where each thumbnail and its name were drawn, for the mouse
    hits: Vec<(Rect, usize)>,
    /// Thumbnails across, and rows of them on screen, at the last draw
    pub per_row: usize,
    pub rows_shown: usize,
}

impl Gallery {
    pub fn new(dir: PathBuf, images: Vec<PathBuf>) -> Self {
        let (jobs, queue) = mpsc::channel::<(usize, PathBuf)>();
        let (finished, done) = mpsc::channel();
        let queue = Arc::new(Mutex::new(queue));
        // Every core but the one the screen needs
        for _ in 0..crate::cores().saturating_sub(1).max(1) {
            let (queue, finished) = (queue.clone(), finished.clone());
            // They end when the gallery goes away and takes its sender with it
            thread::spawn(move || {
                loop {
                    let job = match queue.lock() {
                        Ok(queue) => queue.recv().ok(),
                        Err(_) => None,
                    };
                    let Some((i, path)) = job else { break };
                    if finished.send((i, decode(&path))).is_err() {
                        break;
                    }
                }
            });
        }
        let thumbs = images.iter().map(|_| Thumb::Waiting).collect();
        Self {
            dir,
            images,
            thumbs,
            selected: 0,
            top: 0,
            follow: true,
            width_px: 100,
            jobs,
            done,
            hits: Vec::new(),
            per_row: 1,
            rows_shown: 1,
        }
    }

    /// Moves the selection, keeping it on screen.
    pub fn select(&mut self, delta: isize) {
        if !self.images.is_empty() {
            self.selected = self.selected.saturating_add_signed(delta).min(self.images.len() - 1);
            self.follow = true;
        }
    }

    pub fn scroll(&mut self, rows: isize) {
        let last = self.images.len().saturating_sub(1) / self.per_row.max(1);
        self.top = self.top.saturating_add_signed(rows).min(last);
        self.follow = false;
    }

    pub fn resize(&mut self, step: i32) {
        self.width_px = self.width_px.saturating_add_signed(step).clamp(WIDTHS.0, WIDTHS.1);
        self.follow = true;
    }

    /// The image under a screen cell.
    pub fn at(&self, x: u16, y: u16) -> Option<usize> {
        self.hits.iter().find(|(r, _)| x >= r.x && x < r.right() && y >= r.y && y < r.bottom()).map(|(_, i)| *i)
    }

    fn take_finished(&mut self, picker: &Picker) {
        while let Ok((i, image)) = self.done.try_recv() {
            self.thumbs[i] = match image {
                Some(image) => Thumb::Ready {
                    aspect: image.height() as f32 / image.width().max(1) as f32,
                    protocol: Box::new(picker.new_resize_protocol(image)),
                },
                None => Thumb::Failed,
            };
        }
    }

    fn request(&mut self, i: usize) {
        if let Some(Thumb::Waiting) = self.thumbs.get(i) {
            let _ = self.jobs.send((i, self.images[i].clone()));
            self.thumbs[i] = Thumb::Loading;
        }
    }

    /// The size of a thumbnail on screen, in columns and rows.
    fn size(&self, i: usize, picker: &Picker) -> (u16, u16) {
        // Half-blocks show about two pixels to a character, so they get twice the room
        let scale = if picker.protocol_type() == ProtocolType::Halfblocks { 2.0 } else { 1.0 };
        let font = picker.font_size();
        let (cell_w, cell_h) = (font.width.max(1) as f32 / scale, font.height.max(1) as f32 / scale);
        let aspect = match &self.thumbs[i] {
            Thumb::Ready { aspect, .. } => aspect.min(TALLEST),
            _ => 0.75,
        };
        let width = self.width_px as f32;
        ((width / cell_w).ceil().max(2.0) as u16, (width * aspect / cell_h).ceil().max(1.0) as u16)
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, picker: &Picker, focused: bool) {
        self.take_finished(picker);
        self.hits.clear();
        if self.images.is_empty() || area.height == 0 {
            return;
        }
        let cols = self.size(0, picker).0;
        self.per_row = ((area.width + GAP) / (cols + GAP)).max(1) as usize;
        let per_row = self.per_row;
        let rows: Vec<std::ops::Range<usize>> =
            (0..self.images.len()).step_by(per_row).map(|s| s..(s + per_row).min(self.images.len())).collect();
        // Each row is as tall as its tallest thumbnail, plus a line for the names and a gap
        let height = |g: &Self, r: usize| rows[r].clone().map(|i| g.size(i, picker).1).max().unwrap_or(1) + 2;

        self.top = self.top.min(rows.len() - 1);
        if self.follow {
            let row = self.selected / per_row;
            if row < self.top {
                self.top = row;
            }
            while self.top < row && (self.top..=row).map(|r| height(self, r)).sum::<u16>() > area.height {
                self.top += 1;
            }
        }

        let mut y = area.y;
        let mut last = self.top;
        self.rows_shown = 0;
        for (r, row) in rows.iter().enumerate().skip(self.top) {
            if y >= area.bottom() {
                break;
            }
            let row_height = height(self, r);
            for (k, i) in row.clone().enumerate() {
                self.request(i);
                let x = area.x + k as u16 * (cols + GAP);
                let (w, h) = self.size(i, picker);
                let picture = Rect::new(x, y, w, h);
                // A thumbnail cut off by the bottom edge waits for the next scroll
                let fits = picture.bottom() <= area.bottom() && picture.right() <= area.right();
                match &mut self.thumbs[i] {
                    Thumb::Ready { protocol, .. } if fits => {
                        let image = StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle)));
                        f.render_stateful_widget(image, picture, protocol.as_mut());
                    }
                    Thumb::Failed => note(f, picture.intersection(area), "can't read"),
                    Thumb::Ready { .. } => {}
                    _ => note(f, picture.intersection(area), "…"),
                }
                let name_y = y + h;
                if name_y < area.bottom() {
                    let name = self.images[i].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let name = fit(&name, w as usize);
                    let style = match (i == self.selected, focused) {
                        (true, true) => Style::new().add_modifier(Modifier::REVERSED),
                        (true, false) => Style::new().add_modifier(Modifier::UNDERLINED),
                        _ => Style::new().add_modifier(Modifier::DIM),
                    };
                    f.render_widget(Paragraph::new(Line::styled(name, style)), Rect::new(x, name_y, w, 1));
                }
                self.hits.push((Rect::new(x, y, w, h + 1).intersection(area), i));
            }
            y = y.saturating_add(row_height);
            last = r;
            self.rows_shown += 1;
        }
        // Get the next screenful ready
        let ahead = (last + 1) * per_row;
        for i in ahead..(ahead + per_row * self.rows_shown.max(1)).min(self.images.len()) {
            self.request(i);
        }
    }
}

fn decode(path: &Path) -> Option<DynamicImage> {
    if fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let image = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?;
    Some(image.thumbnail(DECODED.0, DECODED.1))
}

fn note(f: &mut Frame, area: Rect, text: &str) {
    if area.width > 0 && area.height > 0 {
        let line = Line::styled(fit(text, area.width as usize), Style::new().add_modifier(Modifier::DIM));
        f.render_widget(Paragraph::new(line), Rect { y: area.y + area.height / 2, height: 1, ..area });
    }
}

/// `name` cut to `width` columns, with "…" where it was cut.
fn fit(name: &str, width: usize) -> String {
    if str_width(name) <= width {
        return name.to_string();
    }
    let mut out: String = String::new();
    for c in name.chars() {
        if str_width(&out) + 2 > width {
            break;
        }
        out.push(c);
    }
    out + "…"
}
