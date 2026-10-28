//! A folder's images and videos as a grid of thumbnails, decoded in the background as
//! they come into view. A video's is its picture a second in, with a badge.

use std::{
    fs,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use image::{DynamicImage, RgbImage};
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

use crate::{external, picture, text::str_width, video};

/// Columns between thumbnails
const GAP: u16 = 2;
/// Thumbnails are decoded this big at most, so resizing them doesn't decode again
const DECODED: (u32, u32) = (400, 800);
/// Portrait images are cut off at this many times their width
const TALLEST: f32 = 2.0;
/// Files bigger than this aren't decoded for a thumbnail
const MAX_BYTES: u64 = 50 << 20;
/// Nor are images that need more memory than this to decode
const LARGEST: u64 = 512 << 20;
/// What all the decoders may hold at once, in every gallery; a decode waits until it fits,
/// unless it's the only one. A photo of 12 megapixels takes 96 MB, so two of those go at
/// once, or more smaller ones.
const BUDGET: u64 = 256 << 20;
/// The most decoding threads a gallery has: more only wait on the budget for big photos,
/// and fight over the cores for small ones
const WORKERS: usize = 6;
/// Thumbnails further than this many screens from the view are let go
const KEEP_SCREENS: usize = 3;
/// A video's thumbnail is its picture this many seconds in
const STILL_AT: f64 = 1.0;
pub const WIDTHS: (u32, u32) = (50, 400);
/// The decoding threads' name: a panic in one fails just its thumbnail
pub const WORKER: &str = "thumbnails";

/// The images and videos directly in `dir`, by name. Hidden ones are left out, as the
/// tree leaves them out: macOS's `._IMG_0001.JPG` beside every photo on a shared drive, say.
pub fn images_in(dir: &Path) -> Vec<PathBuf> {
    let hidden = |p: &Path| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'));
    let mut images: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| (picture::is_image(p) || external::is_video(p)) && !hidden(p))
        .collect();
    images.retain(|p| p.is_file());
    images.sort_by_cached_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    images
}

/// Memory taken by decodes in progress.
struct Budget {
    used: Mutex<u64>,
    freed: Condvar,
}

/// One for the whole program: a gallery kept to go back to, or one left behind, may
/// still have decodes going
static DECODING: Budget = Budget { used: Mutex::new(0), freed: Condvar::new() };

impl Budget {
    /// Waits until `need` more fits, unless nothing else is decoding.
    fn take(&self, need: u64) -> Held<'_> {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used > 0 && *used + need > BUDGET {
            used = self.freed.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += need;
        Held { budget: self, need }
    }
}

struct Held<'a> {
    budget: &'a Budget,
    need: u64,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        *self.budget.used.lock().unwrap_or_else(|e| e.into_inner()) -= self.need;
        self.budget.freed.notify_all();
    }
}

enum Thumb {
    Waiting,
    Loading,
    Failed,
    Ready(Box<StatefulProtocol>),
}

/// What a worker made of a thumbnail it was asked for.
enum Decoded {
    Image(DynamicImage),
    Failed,
    /// Scrolled far out of view before its turn came; asked for again if it comes back
    Skipped,
}

pub struct Gallery {
    pub dir: PathBuf,
    /// The images and videos, and how many are videos
    pub images: Vec<PathBuf>,
    pub videos: usize,
    thumbs: Vec<Thumb>,
    /// Each image's height over its width, once known; kept when a thumbnail is let go
    aspects: Vec<Option<f32>>,
    pub selected: usize,
    /// First row of the grid on screen
    top: usize,
    /// Scroll so the selection shows; off after the wheel scrolls
    follow: bool,
    /// Thumbnail width in pixels
    pub width_px: u32,
    jobs: mpsc::Sender<(usize, PathBuf)>,
    done: mpsc::Receiver<(usize, Decoded)>,
    /// The thumbnails worth decoding, from the last draw: the workers pass over others
    wanted: Arc<[AtomicUsize; 2]>,
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
        let wanted = Arc::new([AtomicUsize::new(0), AtomicUsize::new(usize::MAX)]);
        // Every core but the one the screen needs, up to WORKERS
        for _ in 0..crate::cores().saturating_sub(1).clamp(1, WORKERS) {
            let (queue, finished, wanted) = (queue.clone(), finished.clone(), wanted.clone());
            // They end when the gallery goes away and takes its sender with it
            let _ = thread::Builder::new().name(WORKER.into()).spawn(move || {
                loop {
                    let job = match queue.lock() {
                        Ok(queue) => queue.recv().ok(),
                        Err(_) => None,
                    };
                    let Some((i, path)) = job else { break };
                    // Scrolling past a big folder asks for screens of thumbnails on the way;
                    // the ones on screen now shouldn't wait behind them
                    let decoded =
                        if !(wanted[0].load(Ordering::Relaxed)..wanted[1].load(Ordering::Relaxed)).contains(&i) {
                            Decoded::Skipped
                        } else {
                            // A decoder that panics on a broken file fails just that thumbnail
                            match panic::catch_unwind(AssertUnwindSafe(|| decode(&path, &DECODING))) {
                                Ok(Some(image)) => Decoded::Image(image),
                                _ => Decoded::Failed,
                            }
                        };
                    if finished.send((i, decoded)).is_err() {
                        break;
                    }
                }
            });
        }
        let thumbs = images.iter().map(|_| Thumb::Waiting).collect();
        let aspects = vec![None; images.len()];
        let videos = images.iter().filter(|p| external::is_video(p)).count();
        Self {
            dir,
            images,
            videos,
            thumbs,
            aspects,
            selected: 0,
            top: 0,
            follow: true,
            width_px: 100,
            jobs,
            done,
            wanted,
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
        while let Ok((i, decoded)) = self.done.try_recv() {
            self.thumbs[i] = match decoded {
                Decoded::Image(image) => {
                    self.aspects[i] = Some(image.height() as f32 / image.width().max(1) as f32);
                    Thumb::Ready(Box::new(picker.new_resize_protocol(image)))
                }
                Decoded::Failed => Thumb::Failed,
                Decoded::Skipped => Thumb::Waiting,
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
        let aspect = self.aspects[i].map_or(0.75, |a| a.min(TALLEST));
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
            // The highest top (no higher than now) that still shows the selected row: up from
            // that row while the rows above it fit, so a jump to the end costs one screenful
            let mut first = row;
            let mut used = usize::from(height(self, row));
            while first > self.top {
                let above = usize::from(height(self, first - 1));
                if used + above > usize::from(area.height) {
                    break;
                }
                used += above;
                first -= 1;
            }
            self.top = first;
        }

        // What's worth decoding, roughly, before anything's asked for: a worker that took a
        // job straight away would otherwise go by where the view was before
        let screen = per_row * self.rows_shown.max(1);
        let around = (self.top * per_row).saturating_sub(screen * KEEP_SCREENS);
        self.wanted[0].store(around, Ordering::Relaxed);
        self.wanted[1].store(self.top * per_row + screen * (KEEP_SCREENS + 2), Ordering::Relaxed);
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
                // A thumbnail cut off by the bottom edge waits for the next scroll, unless
                // it's bigger than the whole grid (made big in a short pane), and is drawn
                // smaller, to what there's room for
                let fits = picture.bottom() <= area.bottom() && picture.right() <= area.right();
                let never_fits = h > area.height || w > area.width;
                match &mut self.thumbs[i] {
                    Thumb::Ready(protocol) if fits || never_fits => {
                        let image = StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle)));
                        f.render_stateful_widget(image, picture.intersection(area), protocol.as_mut());
                    }
                    Thumb::Failed if external::is_video(&self.images[i]) => {
                        note(f, picture.intersection(area), "▶ no preview")
                    }
                    Thumb::Failed => note(f, picture.intersection(area), "can't read"),
                    Thumb::Ready(_) => {}
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
        let screen = per_row * self.rows_shown.max(1);
        for i in ahead..(ahead + screen).min(self.images.len()) {
            self.request(i);
        }
        // Let go of thumbnails far from the view, so a huge folder doesn't fill memory
        let keep = (self.top * per_row).saturating_sub(screen * KEEP_SCREENS)..ahead + screen * KEEP_SCREENS;
        self.wanted[0].store(keep.start, Ordering::Relaxed);
        self.wanted[1].store(keep.end, Ordering::Relaxed);
        for (i, thumb) in self.thumbs.iter_mut().enumerate() {
            if !keep.contains(&i) && matches!(thumb, Thumb::Ready(_)) {
                *thumb = Thumb::Waiting;
            }
        }
    }
}

fn decode(path: &Path, budget: &Budget) -> Option<DynamicImage> {
    if external::is_video(path) {
        return still(path, budget);
    }
    if fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    // The header gives the size, and so roughly what decoding will take: the picture,
    // up to four bytes a pixel, and as much again for a photo turned upright
    let (width, height) = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_dimensions().ok()?;
    let need = u64::from(width) * u64::from(height) * 4;
    if need > LARGEST {
        return None;
    }
    let _held = budget.take(need.max(1) * 2);
    let reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(LARGEST);
    let image = picture::decode_upright(reader, limits).ok()?;
    Some(image.thumbnail(DECODED.0, DECODED.1))
}

/// A video's thumbnail: its picture STILL_AT seconds in, badged as a video. None if
/// it isn't one readme plays.
fn still(path: &Path, budget: &Budget) -> Option<DynamicImage> {
    // The decoder keeps up to 16 pictures to go by, at a byte and a half a pixel
    let (width, height) = video::picture_size(path)?;
    let need = u64::from(width) * u64::from(height) * 24;
    if need > LARGEST {
        return None;
    }
    let _held = budget.take(need.max(1));
    let mut still = video::still(path, STILL_AT, DECODED)?;
    badge(&mut still);
    Some(DynamicImage::ImageRgb8(still))
}

/// Marks a picture as a video's: a play sign on a dark patch in its bottom left corner,
/// drawn into the picture itself, so it shows however the picture is drawn.
fn badge(picture: &mut RgbImage) {
    let (w, h) = (picture.width() as f32, picture.height() as f32);
    // About a quarter of the picture's height, but never too small to make out
    let tall = (w.min(h) * 0.24).max(10.0).min(h);
    let (wide, margin) = (tall * 1.4, (w.min(h) * 0.05).max(1.0));
    let (left, top) = (margin, h - margin - tall);
    let round = tall * 0.25;
    // The sign: a triangle pointing right, a little right of the middle so it looks
    // centered
    let (sign_h, sign_w) = (tall * 0.5, tall * 0.44);
    let (cx, cy) = (left + wide / 2.0 + tall * 0.03, top + tall / 2.0);
    let (x0, x1, y0, y1) = (cx - sign_w / 2.0, cx + sign_w / 2.0, cy - sign_h / 2.0, cy + sign_h / 2.0);
    let in_patch = |x: f32, y: f32| {
        let (dx, dy) =
            ((left + round - x).max(x - (left + wide - round)), (top + round - y).max(y - (top + tall - round)));
        x >= left && x < left + wide && y >= top && y < top + tall && (dx <= 0.0 || dy <= 0.0 || dx.hypot(dy) <= round)
    };
    // Right of the left edge, and inside the two slanted edges
    let in_sign = |x: f32, y: f32| x >= x0 && (y - cy).abs() <= (x1 - x) / (x1 - x0) * (y1 - y0) / 2.0;
    for py in top.floor().max(0.0) as u32..((top + tall).ceil() as u32).min(picture.height()) {
        for px in left.floor() as u32..((left + wide).ceil() as u32).min(picture.width()) {
            // How much of the pixel each covers, from 4 × 4 points in it
            let (mut patch, mut sign) = (0.0, 0.0);
            for k in 0..16 {
                let (x, y) = (px as f32 + (k % 4) as f32 / 4.0 + 0.125, py as f32 + (k / 4) as f32 / 4.0 + 0.125);
                if in_patch(x, y) {
                    patch += 1.0 / 16.0;
                    if in_sign(x, y) {
                        sign += 1.0 / 16.0;
                    }
                }
            }
            let pixel = picture.get_pixel_mut(px, py);
            for c in &mut pixel.0 {
                let dark = f32::from(*c) * (1.0 - 0.6 * patch);
                *c = (dark * (1.0 - sign) + 255.0 * sign).round() as u8;
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn videos_show_among_the_images_a_second_in_and_badged() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.png", "a.mp4", "notes.txt", ".hidden.mov"] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        let names: Vec<String> =
            images_in(dir.path()).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a.mp4", "b.png"]);
        // testdata's clip a second in is blue, with a white square up top
        let clip = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/clip.mp4");
        let Some(DynamicImage::ImageRgb8(picture)) = still(&clip, &DECODING) else { panic!("no still") };
        assert_eq!(picture.dimensions(), (72, 40));
        let blue = |p: &image::Rgb<u8>| p.0[2] > 200 && p.0[0] < 60;
        assert!(blue(picture.get_pixel(60, 30)) && picture.get_pixel(60, 14).0.iter().all(|c| *c > 200));
        // and the badge in the bottom left: a white sign on a darker patch
        assert!(picture.get_pixel(8, 33).0.iter().all(|c| *c > 200), "{:?}", picture.get_pixel(8, 33));
        let patch = picture.get_pixel(3, 36).0;
        assert!(patch[2] < 120 && patch[2] > 40, "{patch:?}");
        // Nothing for a video readme can't play
        let other = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/other-codec.mp4");
        assert!(still(&other, &DECODING).is_none());
    }
}
