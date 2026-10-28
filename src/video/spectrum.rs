//! The sound's spectrum, for the bars under a video's bar: the sound at the moment
//! that's playing, decoded there and then and split into bands, low notes on the left
//! and high ones on the right, which leap up with the sound and fall back slowly.

use std::{
    fs::File,
    path::Path,
    sync::{Arc, OnceLock},
    time::Instant,
};

use symphonia_core::dsp::{complex::Complex, fft::Fft};

use super::{mp4::Movie, sound::Aac};

/// Samples looked at for each moment: about 46 ms of sound at 44.1 kHz
const WINDOW: usize = 2048;
/// The bands span these, in hertz
const LOWEST: f32 = 50.0;
const HIGHEST: f32 = 16_000.0;
/// Loudness drawn as nothing, and as a whole bar, in decibels
const FLOOR: f32 = -66.0;
const CEILING: f32 = -6.0;
/// How fast a bar falls, in whole heights a second
const FALL: f32 = 1.6;

pub struct Spectrum {
    movie: Arc<Movie>,
    file: File,
    aac: Aac,
    /// Where the sound ends, in its track's ticks: past it there's nothing to hear
    end: i64,
    /// The next sound packet to decode
    next: usize,
    /// The latest samples decoded, mixed to one channel: WINDOW of them, or fewer near
    /// the start, and none past the end
    recent: Vec<f32>,
    fft: Fft,
    /// The bars' heights, 0 to 1, and when they were last brought up to date
    bars: Vec<f32>,
    updated: Instant,
    /// Paused, the packet the bars were worked out up to and how many there are: the
    /// same again draws them as they are
    still: Option<(usize, usize)>,
}

impl Spectrum {
    /// The spectrum of the sound `aac` decodes, of the video at `path`.
    pub fn new(path: &Path, movie: Arc<Movie>, aac: Aac) -> Option<Self> {
        let track = movie.audio.as_ref()?;
        let end = track.samples.last()?.time + track.frame_ticks();
        let file = File::open(path).ok()?;
        Some(Self {
            movie,
            file,
            aac,
            end,
            next: usize::MAX,
            recent: Vec::with_capacity(WINDOW * 2),
            fft: Fft::new(WINDOW),
            bars: Vec::new(),
            updated: Instant::now(),
            still: None,
        })
    }

    /// `count` bars for the sound at `at` seconds, 0 to 1. While it plays they leap up
    /// with the sound and fall back slowly; paused, they show just that moment.
    pub fn bars(&mut self, at: f64, count: usize, playing: bool) -> &[f32] {
        self.listen(at);
        let heard = (self.next, count);
        if !playing && self.still == Some(heard) {
            return &self.bars;
        }
        self.still = (!playing).then_some(heard);
        let levels = levels(&self.recent, self.aac.rate, count, &mut self.fft);
        let fall = if playing { self.updated.elapsed().as_secs_f32().min(0.25) * FALL } else { 1.0 };
        self.updated = Instant::now();
        if self.bars.len() != count {
            self.bars = vec![0.0; count];
        }
        for (bar, level) in self.bars.iter_mut().zip(levels) {
            *bar = level.max(*bar - fall);
        }
        &self.bars
    }

    /// Decodes the sound up to `at` seconds, keeping the latest WINDOW samples.
    fn listen(&mut self, at: f64) {
        let Some(track) = &self.movie.audio else { return };
        let ticks = track.ticks(at);
        // Past the end of the sound, as where a video's sound stops before its pictures do,
        // there's nothing, and the bars fall
        if ticks >= self.end {
            self.recent.clear();
            self.next = usize::MAX;
            return;
        }
        let packet = track.samples.partition_point(|s| s.time <= ticks).saturating_sub(1);
        // Gone back, or a long way on: start again a few packets before, for the
        // decoder to settle and the window to fill
        if packet + 1 < self.next || packet > self.next.saturating_add(8) {
            self.aac.reset();
            self.next = packet.saturating_sub(3);
            self.recent.clear();
        }
        let channels = self.aac.channels.max(1);
        let (mut buf, mut out) = (Vec::new(), Vec::new());
        while self.next <= packet && self.next < track.samples.len() {
            out.clear();
            if track.read(&mut self.file, self.next, &mut buf).is_ok() {
                self.aac.decode(&buf, &mut out);
            }
            let scale = channels as f32 * 32768.0;
            self.recent
                .extend(out.chunks(channels).map(|frame| frame.iter().map(|&s| f32::from(s)).sum::<f32>() / scale));
            self.next += 1;
        }
        let old = self.recent.len().saturating_sub(WINDOW);
        self.recent.drain(..old);
    }
}

/// How loud `count` bands of `samples` are, 0 to 1: bands evenly spaced in pitch from
/// LOWEST to HIGHEST, each as loud as the loudest frequency in it.
fn levels(samples: &[f32], rate: u32, count: usize, fft: &mut Fft) -> Vec<f32> {
    // The latest samples, through a Hann window, with silence before the first
    let mut x: Vec<Complex<f32>> = hann()
        .iter()
        .enumerate()
        .map(|(i, hann)| {
            let sample = (samples.len() + i).checked_sub(WINDOW).map_or(0.0, |j| samples[j]);
            Complex::new(sample * hann, 0.0)
        })
        .collect();
    fft.fft_inplace(&mut x);
    let bin = rate as f32 / WINDOW as f32;
    let top = HIGHEST.min(rate as f32 / 2.0);
    // A sine wave at full scale comes out of the window a quarter of WINDOW high
    let full = WINDOW as f32 / 4.0;
    let edge = |band: usize| LOWEST * (top / LOWEST).powf(band as f32 / count as f32);
    (0..count)
        .map(|band| {
            let (from, to) = (edge(band), edge(band + 1));
            let first = ((from / bin).round() as usize).clamp(1, WINDOW / 2 - 1);
            let last = ((to / bin).round() as usize).clamp(first + 1, WINDOW / 2);
            let loudest = x[first..last].iter().map(|c| c.norm()).fold(0.0, f32::max);
            // Most sound is quieter the higher it goes, so each octave above 1 kHz
            // counts for 3 dB more, and each below for 3 dB less
            let tilt = 3.0 * ((from * to).sqrt() / 1000.0).log2();
            let db = 20.0 * (loudest / full).max(1e-9).log10() + tilt;
            ((db - FLOOR) / (CEILING - FLOOR)).clamp(0.0, 1.0)
        })
        .collect()
}

/// The Hann window the samples go through, worked out once.
fn hann() -> &'static [f32] {
    static HANN: OnceLock<Vec<f32>> = OnceLock::new();
    HANN.get_or_init(|| {
        (0..WINDOW).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / WINDOW as f32).cos()).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::mp4;

    /// The band that's loudest, and the frequencies it spans.
    fn loudest(levels: &[f32], rate: u32) -> (usize, f32, f32) {
        let band = (0..levels.len()).max_by(|a, b| levels[*a].total_cmp(&levels[*b])).unwrap();
        let top = HIGHEST.min(rate as f32 / 2.0);
        let edge = |band: usize| LOWEST * (top / LOWEST).powf(band as f32 / levels.len() as f32);
        (band, edge(band), edge(band + 1))
    }

    #[test]
    fn a_tone_shows_in_its_band_and_silence_in_none() {
        let tone: Vec<f32> =
            (0..WINDOW).map(|i| 0.5 * (std::f32::consts::TAU * 440.0 * i as f32 / 44100.0).sin()).collect();
        let mut fft = Fft::new(WINDOW);
        let bands = levels(&tone, 44100, 24, &mut fft);
        let (band, from, to) = loudest(&bands, 44100);
        assert!(from <= 440.0 && 440.0 < to, "{band}: {from}..{to} {bands:?}");
        assert!(bands[band] > 0.7, "{bands:?}");
        // Far from it, next to nothing
        assert!(bands[20..].iter().all(|l| *l < 0.1), "{bands:?}");
        assert!(levels(&[0.0; WINDOW], 44100, 24, &mut fft).iter().all(|l| *l == 0.0));
        // Less sound than the window holds is padded with silence
        assert!(levels(&tone[..100], 44100, 24, &mut fft).iter().all(|l| *l < 0.7));
    }

    #[test]
    fn the_bars_follow_the_sound_where_it_plays() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/clip.mp4");
        let movie = Arc::new(mp4::open(&path).unwrap());
        let aac = Aac::new(movie.audio.as_ref().unwrap()).unwrap();
        let mut spectrum = Spectrum::new(&path, movie, aac).unwrap();
        // Its 440 Hz tone, half way through, and again after going back to the start and on
        for at in [0.6, 0.0, 0.9] {
            let levels = spectrum.bars(at, 24, false).to_vec();
            if at == 0.0 {
                continue;
            }
            let (band, from, to) = loudest(&levels, 44100);
            assert!(from <= 440.0 && 440.0 < to, "at {at}: {band}: {from}..{to} {levels:?}");
        }
        // Past the end of its sound there's nothing, and back in it the tone again
        assert!(spectrum.bars(5.0, 24, false).iter().all(|l| *l == 0.0));
        let (band, from, to) = loudest(spectrum.bars(0.6, 24, false), 44100);
        assert!(from <= 440.0 && 440.0 < to, "{band}: {from}..{to}");
    }
}
