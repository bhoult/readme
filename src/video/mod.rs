//! Playing videos in the file pane: MP4 and QuickTime files with H.264 pictures,
//! decoded in pure Rust, and AAC sound played through ALSA. Pictures are drawn in real
//! pixels where the terminal speaks Kitty's graphics protocol, and in half-blocks
//! elsewhere.

mod frames;
pub(crate) mod kitty;
pub mod mp4;
mod sound;
mod spectrum;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use ratatui::{
    Frame as Screen,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_image::picker::{Picker, ProtocolType};

use frames::{Decoded, Frame, Frames, Late, Size};
use mp4::{Codec, Movie};
use sound::Sound;
use spectrum::Spectrum;

pub use frames::WORKER as PICTURES;
pub use sound::WORKER as SOUND;

/// How far the arrow keys move
pub const STEP: f64 = 5.0;
pub const BIG_STEP: f64 = 60.0;

/// The volume control at the bar's end, " sound ▁▂▂▃▄▅▅▆▇█ ": its label, a step for each
/// tenth, and a space
const VOLUME_LABEL: u16 = 7;
const VOLUME_STEPS: [&str; 10] = ["▁", "▂", "▂", "▃", "▄", "▅", "▅", "▆", "▇", "█"];
const VOLUME_WIDTH: u16 = VOLUME_LABEL + VOLUME_STEPS.len() as u16 + 1;
/// How much each step is, in percent
const VOLUME_STEP: u8 = 100 / VOLUME_STEPS.len() as u8;
/// Eighths of a cell filled from the bottom, for the spectrum's bars
const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// What a click on the volume control does.
pub enum VolumeClick {
    /// On "sound" or "muted": turns the sound on or off
    Toggle,
    /// On the steps: sets the volume, in percent
    Set(u8),
}

/// Pixels a Kitty picture starts out with at most, before it's seen how fast the
/// terminal takes them, and the range it's kept in: every picture goes through the
/// terminal (and herdr or tmux) as text, a megapixel being 4 MB of it
const START_BUDGET: f64 = 250_000.0;
const LEAST_BUDGET: f64 = 30_000.0;
const MOST_BUDGET: f64 = 1_000_000.0;

pub struct Video {
    path: PathBuf,
    movie: Arc<Movie>,
    /// "1920×1080 H.264, stereo"
    pub info: String,
    pub duration: f64,
    size: Size,
    late: Late,
    frames: Frames,
    /// The next picture, come but not due yet
    queued: Option<Frame>,
    /// The picture on screen
    shown: Option<Frame>,
    /// No picture has come from the decoder since it was last started
    waiting: bool,
    /// The decoder has handed over its last picture
    ended: bool,
    /// Why the decoder stopped, if it did before the end
    pub failed: Option<String>,
    clock: Clock,
    sound: Option<(Sound, Instant)>,
    /// Videos start with their sound off
    pub muted: bool,
    /// How loud the sound plays, in percent: the same for every video, and followed by
    /// the sound's thread as it changes
    volume: Arc<AtomicU8>,
    /// Whether the video has sound this plays
    has_sound: bool,
    /// The sound's spectrum, for the bars under the bar
    spectrum: Option<Spectrum>,
    kitty: Option<KittyState>,
    /// Pixels per cell
    cell: (f64, f64),
    /// Where the picture and the bar under it were drawn last, for the mouse
    pub picture_area: Rect,
    pub bar: Rect,
    /// And the spectrum under the bar, for knowing when to draw again
    spectrum_area: Rect,
}

#[derive(Clone, Copy)]
enum Clock {
    Paused(f64),
    Playing { since: Instant, from: f64 },
}

struct KittyState {
    /// Two images, taking turns: each picture goes to the one not on screen, so the
    /// one on screen is never replaced while it's shown
    ids: [u32; 2],
    /// Which of them is on screen
    current: usize,
    /// The picture on screen has been sent to the terminal
    sent: bool,
    /// The cells the picture was placed in last
    placed: Option<(u16, u16)>,
    /// The most pixels a picture is sent with as text, kept to what the terminal keeps
    /// up with
    budget: f64,
    /// A picture went out in the last draw
    sending: bool,
    /// Where the terminal takes pictures from files, they go that way, whole
    files: Option<kitty::Files>,
}

fn codec_name(kind: &str) -> String {
    match kind {
        "hvc1" | "hev1" => "HEVC".into(),
        "vp09" => "VP9".into(),
        "av01" => "AV1".into(),
        "mp4v" => "MPEG-4 Part 2".into(),
        "apcn" | "apch" | "apcs" | "apco" | "ap4h" | "ap4x" => "ProRes".into(),
        "jpeg" | "mjpa" | "mjpb" => "Motion JPEG".into(),
        other => format!("“{other}”"),
    }
}

/// "1:05", or "1:02:05" for an hour or more.
pub fn clock_time(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

impl Video {
    /// Opens the video at `path`, paused at the start, its first picture on its way. Its
    /// sound plays at `volume` percent, as that changes.
    pub fn open(path: &Path, picker: &Picker, volume: Arc<AtomicU8>) -> Result<Self, String> {
        let movie = Arc::new(mp4::open(path).map_err(|e| e.0)?);
        let Some(track) = &movie.video else { return Err("it has no video track".into()) };
        match &track.codec {
            Codec::H264(_) => {}
            Codec::Other(kind) => return Err(format!("it's {}, and readme plays H.264", codec_name(kind))),
            Codec::Aac(_) => return Err("its video track holds sound".into()),
        }
        let aac = movie.audio.as_ref().and_then(sound::Aac::new);
        let has_sound = aac.is_some();
        let sound = match (&movie.audio, &aac) {
            (_, Some(aac)) if aac.channels == 1 => ", mono",
            (_, Some(_)) => ", stereo",
            (Some(_), None) => ", sound readme can't play",
            (None, None) => ", no sound",
        };
        let info = format!("{}×{} H.264{sound}", track.width, track.height);
        let duration = track.duration();
        let (size, late) = (Size::default(), Late::default());
        // The first picture is made before the first draw says the size: one no
        // bigger than a Kitty picture starts out, which half-blocks shrink from
        let k = (START_BUDGET / (f64::from(track.width.max(1)) * f64::from(track.height.max(1)))).sqrt().min(1.0);
        size.set(
            ((f64::from(track.width) * k).round() as u32).max(1),
            ((f64::from(track.height) * k).round() as u32).max(1),
        );
        let frames = Frames::start(path, movie.clone(), 0.0, size.clone(), late.clone());
        let spectrum = aac.and_then(|aac| Spectrum::new(path, movie.clone(), aac));
        Ok(Self {
            path: path.to_path_buf(),
            movie,
            info,
            duration,
            size,
            late,
            frames,
            queued: None,
            shown: None,
            waiting: true,
            ended: false,
            failed: None,
            clock: Clock::Paused(0.0),
            sound: None,
            muted: true,
            volume,
            has_sound,
            spectrum,
            kitty: kitty_for(picker),
            cell: cell_size(picker),
            picture_area: Rect::default(),
            bar: Rect::default(),
            spectrum_area: Rect::default(),
        })
    }

    /// Whether the video has sound readme plays, which the volume is for.
    pub fn has_sound(&self) -> bool {
        self.has_sound
    }

    /// From now on drawn the way `picker` draws.
    pub fn set_picker(&mut self, picker: &Picker) {
        (self.kitty, self.cell) = (kitty_for(picker), cell_size(picker));
    }

    pub fn playing(&self) -> bool {
        matches!(self.clock, Clock::Playing { .. })
    }

    /// Seconds into the video now.
    pub fn position(&self) -> f64 {
        let at = match self.clock {
            Clock::Paused(at) => at,
            // Held at the start until the first picture's in and the sound's going
            Clock::Playing { from, .. } if self.waiting => from,
            Clock::Playing { since, from } => match &self.sound {
                Some((sound, _)) => sound.heard().unwrap_or(from),
                None => from + since.elapsed().as_secs_f64(),
            },
        };
        at.min(self.duration)
    }

    pub fn toggle(&mut self) {
        if self.playing() { self.pause() } else { self.play() }
    }

    pub fn play(&mut self) {
        if self.playing() {
            return;
        }
        let mut at = self.position();
        // From the start again once it's played through
        if self.ended && self.queued.is_none() && at >= self.duration - 0.05 {
            self.seek(0.0);
            at = 0.0;
        }
        self.clock = Clock::Playing { since: Instant::now(), from: at };
        if !self.waiting {
            self.start_sound(at);
        }
    }

    pub fn pause(&mut self) {
        self.clock = Clock::Paused(self.position());
        self.sound = None;
    }

    /// Goes to `to` seconds in, playing on from there if it was playing.
    pub fn seek(&mut self, to: f64) {
        let to = to.clamp(0.0, self.duration.max(0.0));
        let playing = self.playing();
        self.sound = None;
        self.late.set(to);
        self.frames = Frames::start(&self.path, self.movie.clone(), to, self.size.clone(), self.late.clone());
        (self.queued, self.waiting, self.ended, self.failed) = (None, true, false, None);
        self.clock = if playing { Clock::Playing { since: Instant::now(), from: to } } else { Clock::Paused(to) };
    }

    pub fn seek_by(&mut self, seconds: f64) {
        let to = self.position() + seconds;
        self.seek(to);
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if self.playing() {
            // The clock takes over from the sound, or the sound from the clock, here
            let at = self.position();
            self.clock = Clock::Playing { since: Instant::now(), from: at };
            self.sound = None;
            if !self.waiting {
                self.start_sound(at);
            }
        }
    }

    fn start_sound(&mut self, at: f64) {
        self.sound = None;
        if !self.muted && self.has_sound {
            self.sound =
                Sound::start(&self.path, self.movie.clone(), at, self.volume.clone()).map(|s| (s, Instant::now()));
        }
    }

    /// Takes in pictures that have come, shows the one due, and keeps time.
    pub fn poll(&mut self) {
        // Sound that's stopped, never started, or runs ahead of time (an output that
        // swallows it whole) leaves the timing to the clock
        if let (Clock::Playing { since, from }, Some((sound, started))) = (self.clock, &self.sound) {
            let wall = from + since.elapsed().as_secs_f64();
            let heard = sound.heard();
            let stalled = heard.is_none() && started.elapsed() > Duration::from_secs(2);
            let racing = heard.is_some_and(|at| at > wall + 0.5);
            if stalled || racing || sound.finished() {
                let at = if racing { wall } else { heard.unwrap_or(from) };
                self.clock = Clock::Playing { since: Instant::now(), from: at };
                self.sound = None;
            }
        }
        let now = self.position();
        if self.playing() {
            self.late.set(now);
        }
        loop {
            // After its end, the decoder's thread is gone, which isn't a failure
            if self.queued.is_none() && !self.ended {
                match self.frames.next() {
                    Some(Decoded::Frame(frame)) => self.queued = Some(frame),
                    Some(Decoded::End) => self.ended = true,
                    Some(Decoded::Failed(why)) => {
                        self.ended = true;
                        self.failed = Some(why);
                    }
                    None => {}
                }
            }
            let Some(next) = &self.queued else { break };
            // The first picture since starting shows straight away, as it's the one
            // at the place started from
            if !self.waiting && next.at > now + 0.005 {
                break;
            }
            self.shown = self.queued.take();
            if let Some(kitty) = &mut self.kitty {
                kitty.sent = false;
            }
            if self.waiting {
                self.waiting = false;
                if let Clock::Playing { from, .. } = self.clock {
                    // The clock starts now the picture's here, and the sound with it
                    self.clock = Clock::Playing { since: Instant::now(), from };
                    self.start_sound(from);
                }
            }
        }
        // Played through: stop at the end, or where decoding stopped
        if self.playing() && self.ended && self.queued.is_none() && (self.waiting || now >= self.duration - 0.01) {
            self.clock = Clock::Paused(if self.failed.is_some() { now } else { self.duration });
            self.sound = None;
            self.waiting = false;
        }
    }

    /// How soon something will change on screen, for the main loop to wake then.
    pub fn next_due(&self) -> Option<Duration> {
        match (&self.clock, &self.queued) {
            (Clock::Playing { .. }, Some(next)) if !self.waiting => {
                let picture = Duration::from_secs_f64((next.at - self.position()).clamp(0.0, 1.0));
                // The spectrum moves on between pictures, about 30 times a second
                Some(if self.spectrum_area.height > 0 { picture.min(Duration::from_millis(33)) } else { picture })
            }
            // Waiting on the decoder, or the sound, to start
            (Clock::Playing { .. }, _) => Some(Duration::from_millis(10)),
            (Clock::Paused(_), _) if self.waiting && !self.ended => Some(Duration::from_millis(15)),
            _ => None,
        }
    }

    /// How long the last draw took to go out to the terminal: Kitty pictures are kept
    /// to a size it takes as fast as they come.
    pub fn drew(&mut self, took: Duration) {
        let Some(kitty) = &mut self.kitty else { return };
        if !std::mem::take(&mut kitty.sending) || kitty.files.is_some() {
            return;
        }
        // Room for the terminal to do more than take pictures
        let took = took.as_secs_f64();
        if took > 0.025 {
            kitty.budget = (kitty.budget * 0.8).max(LEAST_BUDGET);
        } else if took < 0.008 {
            kitty.budget = (kitty.budget * 1.1).min(MOST_BUDGET);
        }
    }

    pub fn draw(&mut self, f: &mut Screen, area: Rect) {
        self.poll();
        if area.width == 0 || area.height < 2 {
            return;
        }
        // The sound's spectrum goes under the bar, where there's room for it
        let bars = match area.height {
            _ if self.spectrum.is_none() => 0,
            40.. => 8,
            24.. => 6,
            12.. => 4,
            8.. => 2,
            _ => 0,
        };
        let room = Rect::new(area.x, area.y, area.width, area.height - 1 - bars);
        let mut at = self.fit(room);
        // The bar goes right under the picture, the spectrum under that, all centered
        // together
        at.y = area.y + (area.height - at.height - 1 - bars) / 2;
        self.bar = Rect::new(area.x, at.y + at.height, area.width, 1);
        self.spectrum_area = Rect::new(area.x, self.bar.y + 1, area.width, bars);
        self.picture_area = at;
        self.size_frames(at);
        match self.kitty.is_some() {
            true => self.draw_kitty(f, at),
            false => self.draw_blocks(f, at),
        }
        self.draw_bar(f);
        self.draw_spectrum(f);
    }

    /// Where the picture goes in `room`: as big as fits, keeping its shape, centered.
    fn fit(&self, room: Rect) -> Rect {
        let Some(track) = &self.movie.video else { return room };
        let (w, h) = (f64::from(track.width.max(1)), f64::from(track.height.max(1)));
        let (cw, ch) = self.cell;
        let (room_w, room_h) = (f64::from(room.width) * cw, f64::from(room.height) * ch);
        let k = (room_w / w).min(room_h / h);
        let most = if self.kitty.is_some() { kitty::MOST } else { u16::MAX };
        let cols = ((w * k / cw).round() as u16).clamp(1, room.width.min(most));
        let rows = ((h * k / ch).round() as u16).clamp(1, room.height.min(most));
        Rect::new(room.x + (room.width - cols) / 2, room.y + (room.height - rows) / 2, cols, rows)
    }

    /// Tells the decoder the size to make pictures for `at`.
    fn size_frames(&self, at: Rect) {
        let Some(track) = &self.movie.video else { return };
        let (width, height) = match &self.kitty {
            Some(kitty) => {
                // Real pixels: no more than the video has or the cells hold, and sent as
                // text, no more than the terminal keeps up with; it scales them to fill
                let (w, h) = (f64::from(track.width.max(1)), f64::from(track.height.max(1)));
                let cells = (f64::from(at.width) * self.cell.0, f64::from(at.height) * self.cell.1);
                let mut k = (cells.0 / w).min(cells.1 / h).min(1.0);
                if kitty.files.is_none() {
                    k = k.min((kitty.budget / (w * h)).sqrt());
                }
                (((w * k).round() as u32).max(1), ((h * k).round() as u32).max(1))
            }
            // Two pixels to a cell, one above the other
            None => (u32::from(at.width), u32::from(at.height) * 2),
        };
        self.size.set(width, height);
    }

    fn draw_kitty(&mut self, f: &mut Screen, at: Rect) {
        let (Some(kitty), Some(frame)) = (&mut self.kitty, &self.shown) else { return };
        let cells = (at.width, at.height);
        let first = if !kitty.sent {
            kitty.sent = true;
            kitty.sending = true;
            kitty.placed = Some(cells);
            kitty.current ^= 1;
            let (id, size) = (kitty.ids[kitty.current], (frame.width, frame.height));
            // In a file if the terminal takes them, or else as text
            let file = kitty.files.as_mut().and_then(|files| files.write(&frame.rgb).ok());
            Some(match file {
                Some(path) => kitty::transmit_file(id, &path, size, cells),
                None => kitty::transmit(id, &frame.rgb, size, cells),
            })
        } else if kitty.placed != Some(cells) {
            kitty.placed = Some(cells);
            Some(kitty::place(kitty.ids[kitty.current], cells))
        } else {
            None
        };
        kitty::placeholders(f.buffer_mut(), at, kitty.ids[kitty.current], first.as_deref());
    }

    fn draw_blocks(&mut self, f: &mut Screen, at: Rect) {
        let Some(frame) = &self.shown else { return };
        let (fw, fh) = (frame.width as usize, frame.height as usize);
        if fw == 0 || fh == 0 {
            return;
        }
        let rows = usize::from(at.height) * 2;
        let pixel = |x: usize, y: usize| -> Color {
            // The picture may still be the size the pane was before it changed
            let (sx, sy) = (x * fw / usize::from(at.width).max(1), y * fh / rows.max(1));
            let i = (sy.min(fh - 1) * fw + sx.min(fw - 1)) * 3;
            Color::Rgb(frame.rgb[i], frame.rgb[i + 1], frame.rgb[i + 2])
        };
        let buf = f.buffer_mut();
        for y in 0..at.height {
            for x in 0..at.width {
                if let Some(cell) = buf.cell_mut((at.x + x, at.y + y)) {
                    let (x, y) = (usize::from(x), usize::from(y) * 2);
                    cell.set_symbol("▀").set_fg(pixel(x, y)).set_bg(pixel(x, y + 1));
                }
            }
        }
    }

    /// What the bar says before its line, whether it's playing and the time, and whether
    /// the volume control fits at its end. The marks are text, as some terminals draw ⏸
    /// as a wide emoji.
    fn bar_label(&self) -> (String, bool) {
        let mark = if self.playing() { "▶" } else { "‖" };
        let label = format!(" {mark} {} / {} ", clock_time(self.position()), clock_time(self.duration));
        let width = unicode_width::UnicodeWidthStr::width(label.as_str());
        let volume = self.has_sound
            && self.failed.is_none()
            && usize::from(self.bar.width) >= width + usize::from(VOLUME_WIDTH) + 12;
        // Where the volume control doesn't fit, the label says the sound's off
        if self.muted && self.has_sound && !volume { (label + "muted ", false) } else { (label, volume) }
    }

    /// Where the bar's line is: the column it starts at, and how long it is.
    fn line(&self) -> (u16, u16) {
        let (label, volume) = self.bar_label();
        let left = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
        let right = if volume { VOLUME_WIDTH } else { 1 };
        (self.bar.x + left, self.bar.width.saturating_sub(left + right))
    }

    /// How much of a line `length` long has played.
    fn played(&self, length: u16) -> u16 {
        let done = if self.duration > 0.0 { self.position() / self.duration * f64::from(length) } else { 0.0 };
        (done.round() as u16).min(length)
    }

    fn draw_bar(&self, f: &mut Screen) {
        let (label, volume) = self.bar_label();
        let (_, length) = self.line();
        let mut spans = vec![Span::raw(label)];
        if let Some(why) = &self.failed {
            spans.push(Span::styled(format!("stopped: {why}"), Style::default().add_modifier(Modifier::DIM)));
        } else if length > 1 {
            let done = self.played(length);
            spans.push(Span::styled("━".repeat(usize::from(done)), Style::default().fg(Color::Cyan)));
            spans.push(Span::styled(
                "─".repeat(usize::from(length - done)),
                Style::default().add_modifier(Modifier::DIM),
            ));
            if volume {
                spans.extend(self.volume_control());
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), self.bar);
    }

    /// "sound" or "muted", and a step for each tenth of the volume, lit while the sound
    /// is on.
    fn volume_control(&self) -> Vec<Span<'static>> {
        let steps = usize::from(self.volume.load(Ordering::Relaxed).div_ceil(VOLUME_STEP));
        let mut spans = vec![Span::raw(if self.muted { " muted " } else { " sound " })];
        for (i, step) in VOLUME_STEPS.iter().enumerate() {
            spans.push(match (i < steps, self.muted) {
                (true, false) => Span::styled(*step, Style::default().fg(Color::Cyan)),
                (true, true) => Span::raw(*step),
                (false, _) => Span::styled("·", Style::default().add_modifier(Modifier::DIM)),
            });
        }
        spans.push(Span::raw(" "));
        spans
    }

    /// The time a click at column `x` of the bar goes to, if it's on the bar's line, or
    /// just past its end, which is the end of the video.
    pub fn bar_time(&self, x: u16) -> Option<f64> {
        let (start, length) = self.line();
        (length > 0 && (start..=start + length).contains(&x))
            .then(|| f64::from(x - start) / f64::from(length) * self.duration)
    }

    /// What a click at column `x` of the bar does to the sound, if it's on the volume
    /// control: on its label, the sound on or off; on its steps, the volume to that one.
    /// Where the control doesn't fit, a click on "muted" in the bar turns the sound on.
    pub fn volume_click(&self, x: u16) -> Option<VolumeClick> {
        let (label, fits) = self.bar_label();
        if !fits {
            let end = self.bar.x + unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
            let on_muted = label.ends_with("muted ") && (end - 6..end - 1).contains(&x);
            return on_muted.then_some(VolumeClick::Toggle);
        }
        // All of it is the control's, the spaces at its ends too: up to the label's end,
        // the sound on or off; from just left of the first step (none at all) to just past
        // the last (all of it), the volume
        let at = x.checked_sub(self.bar.x + self.bar.width - VOLUME_WIDTH)?;
        Some(match at.checked_sub(VOLUME_LABEL - 1) {
            None => VolumeClick::Toggle,
            Some(step) => VolumeClick::Set(step.min(VOLUME_STEPS.len() as u16) as u8 * VOLUME_STEP),
        })
    }

    /// The sound's spectrum under the bar: a bar for each band of pitches, low ones on
    /// the left, as high as they're loud, green at the bottom and red at the top.
    fn draw_spectrum(&mut self, f: &mut Screen) {
        let area = self.spectrum_area;
        if area.height == 0 || area.width < 8 || self.failed.is_some() {
            return;
        }
        let (at, playing) = (self.position(), self.playing());
        let Some(spectrum) = &mut self.spectrum else { return };
        // Two columns to a bar, and one between
        let count = usize::from((area.width + 1) / 3);
        let left = area.x + (area.width + 1 - count as u16 * 3) / 2;
        let rows = area.height;
        let buf = f.buffer_mut();
        for (i, level) in spectrum.bars(at, count, playing).iter().enumerate() {
            let eighths = (level * f32::from(rows * 8)).round() as u16;
            for row in 0..rows {
                let filled = usize::from(eighths.saturating_sub(row * 8).min(8));
                if filled == 0 {
                    break;
                }
                let style = Style::default().fg(bar_color(row, rows));
                for x in [left + i as u16 * 3, left + i as u16 * 3 + 1] {
                    if let Some(cell) = buf.cell_mut((x, area.y + rows - 1 - row)) {
                        cell.set_symbol(EIGHTHS[filled]).set_style(style);
                    }
                }
            }
        }
    }
}

/// The color of the spectrum's bars `row` rows up of `rows`: green, then yellow, then red
/// at the top.
fn bar_color(row: u16, rows: u16) -> Color {
    let t = f32::from(row) / f32::from(rows.saturating_sub(1).max(1));
    let (from, to, t) =
        if t < 0.5 { ((46, 204, 64), (240, 200, 40), t * 2.0) } else { ((240, 200, 40), (230, 60, 50), t * 2.0 - 1.0) };
    let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    Color::Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

impl Drop for Video {
    fn drop(&mut self) {
        // The terminal can let go of the last picture's memory
        #[cfg(not(test))]
        if let Some(kitty) = &self.kitty
            && kitty.placed.is_some()
        {
            use std::io::Write as _;
            for id in kitty.ids {
                let _ = std::io::stdout().write_all(kitty::delete(id).as_bytes());
            }
        }
    }
}

/// Kitty's state for a picker that draws with it.
fn kitty_for(picker: &Picker) -> Option<KittyState> {
    (picker.protocol_type() == ProtocolType::Kitty).then(|| KittyState {
        ids: two_image_ids(),
        current: 0,
        sent: false,
        placed: None,
        budget: START_BUDGET,
        sending: false,
        files: crate::picture::kitty_files().then(kitty::Files::new),
    })
}

/// Pixels per cell, or a usual size if the terminal didn't say.
fn cell_size(picker: &Picker) -> (f64, f64) {
    let font = picker.font_size();
    if font.width > 0 && font.height > 0 { (f64::from(font.width), f64::from(font.height)) } else { (10.0, 20.0) }
}

/// Two different image numbers.
fn two_image_ids() -> [u32; 2] {
    let first = image_id();
    let mut second = image_id();
    while second == first {
        second = image_id().rotate_left(8);
    }
    [first, second]
}

/// An image number for Kitty that's not likely to be another's: its three low bytes
/// ride in a colour, and none of its bytes is zero.
fn image_id() -> u32 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let mixed = (nanos ^ std::process::id().rotate_left(16)).wrapping_mul(2_654_435_761);
    let [a, b, c, d] = mixed.to_be_bytes();
    u32::from_be_bytes([a.max(1), b.max(1), c.max(1), d.max(1)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_like_a_clock() {
        assert_eq!(clock_time(0.0), "0:00");
        assert_eq!(clock_time(65.9), "1:05");
        assert_eq!(clock_time(3725.0), "1:02:05");
    }

    #[test]
    fn kitty_pictures_take_turns_between_two_images() {
        // Each picture goes to the image that isn't on screen, so the one shown is
        // never replaced while it's up: herdr shows nothing for a moment if it is
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(ratatui_image::FontSize::new(10, 20));
        picker.set_protocol_type(ProtocolType::Kitty);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/clip.mp4");
        let mut video = Video::open(&path, &picker, Arc::new(AtomicU8::new(100))).unwrap();
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        video.play();
        let mut sent = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.len() < 3 && Instant::now() < deadline {
            term.draw(|f| video.draw(f, f.area())).unwrap();
            let buf = term.backend().buffer();
            if let Some(cell) = buf.content.iter().find(|c| c.symbol().starts_with("\x1b_Ga=T")) {
                let id = cell.symbol().split(',').find_map(|kv| kv.strip_prefix("i=")).unwrap().to_string();
                sent.push(id);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_ne!(sent[0], sent[1]);
        assert_eq!(sent[0], sent[2]);
    }

    #[test]
    fn a_video_without_sound_has_no_spectrum() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/pyramid.mp4");
        let video = Video::open(&path, &Picker::halfblocks(), Arc::default()).unwrap();
        assert!(!video.has_sound() && video.spectrum.is_none());
    }

    #[test]
    fn image_ids_have_no_zero_bytes() {
        assert!(image_id().to_be_bytes().iter().all(|b| *b != 0));
    }
}
