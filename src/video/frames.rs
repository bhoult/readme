//! Decoding a video track's pictures on a thread of their own. Each is scaled to the
//! size it's shown at and turned into RGB, and they're handed over in the order
//! they're shown, a few ahead of time.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, VecDeque},
    fs::File,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, TryRecvError, sync_channel},
    },
};

use rust_h264::{
    decoder::{Decoder, Frame as Yuv},
    nal::{NalUnitType, parse_annex_b, parse_avcc},
};

use super::mp4::{Codec, Movie, Track};

/// The name of the decoding threads, which the panic hook knows to leave be
pub const WORKER: &str = "video";

/// Pictures decoded ahead of the one on screen
const AHEAD: usize = 3;
/// The most pictures a second worth making: terminals don't show more, and each is
/// megabytes for them to read
const MOST_A_SECOND: f64 = 60.0;

/// A picture ready to draw.
pub struct Frame {
    /// When it's shown, in seconds from the start
    pub at: f64,
    pub width: u32,
    pub height: u32,
    /// Three bytes a pixel, row by row
    pub rgb: Vec<u8>,
}

pub enum Decoded {
    Frame(Frame),
    /// The last picture has been sent
    End,
    /// Decoding stopped, saying why
    Failed(String),
}

/// The size pictures are made at, which the thread reads for each one, so a pane that
/// changes size gets pictures to fit without decoding starting over.
#[derive(Clone, Default)]
pub struct Size(Arc<AtomicU64>);

impl Size {
    pub fn set(&self, width: u32, height: u32) {
        self.0.store(u64::from(width) << 32 | u64::from(height), Ordering::Relaxed);
    }

    pub fn get(&self) -> (u32, u32) {
        let both = self.0.load(Ordering::Relaxed);
        ((both >> 32) as u32, both as u32)
    }
}

/// Where playing has got to, in microseconds, for the thread to skip pictures it's too
/// late for.
#[derive(Clone, Default)]
pub struct Late(Arc<AtomicU64>);

impl Late {
    pub fn set(&self, seconds: f64) {
        self.0.store((seconds.max(0.0) * 1e6) as u64, Ordering::Relaxed);
    }

    fn get(&self) -> f64 {
        self.0.load(Ordering::Relaxed) as f64 / 1e6
    }
}

/// Pictures being decoded on a thread, from some point in the video on.
pub struct Frames {
    rx: Receiver<Decoded>,
    stop: Arc<AtomicBool>,
}

impl Frames {
    /// Starts decoding the video at `path` so the first picture handed over is the one
    /// shown at `from` seconds.
    pub fn start(path: &Path, movie: Arc<Movie>, from: f64, size: Size, late: Late) -> Self {
        let (tx, rx) = sync_channel(AHEAD);
        let stop = Arc::new(AtomicBool::new(false));
        let path = path.to_path_buf();
        let stopped = stop.clone();
        let spawned = std::thread::Builder::new().name(WORKER.into()).spawn(move || {
            let mut send = |frame| tx.send(Decoded::Frame(frame)).is_ok();
            let result = decode(&path, &movie, from, &size, &late, &mut send, &stopped);
            let _ = tx.send(match result {
                Ok(()) => Decoded::End,
                Err(why) => Decoded::Failed(why),
            });
        });
        if let Err(e) = spawned {
            // No thread, so no pictures; say so the way the thread would have
            let (tx, failed) = sync_channel(1);
            let _ = tx.send(Decoded::Failed(e.to_string()));
            return Self { rx: failed, stop };
        }
        Self { rx, stop }
    }

    /// The next picture, if it's ready.
    pub fn next(&self) -> Option<Decoded> {
        match self.rx.try_recv() {
            Ok(decoded) => Some(decoded),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Decoded::Failed("the decoder stopped".into())),
        }
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The picture on screen `at` seconds into the video at `path`, or its last if it's
/// shorter, made as big as fits in `room` keeping its shape: decoded on this thread, for
/// a thumbnail.
pub fn still(path: &Path, at: f64, room: (u32, u32)) -> Option<Frame> {
    let movie = super::mp4::open(path).ok()?;
    let track = movie.video.as_ref()?;
    let (w, h) = (f64::from(track.width.max(1)), f64::from(track.height.max(1)));
    let k = (f64::from(room.0) / w).min(f64::from(room.1) / h).min(1.0);
    let size = Size::default();
    size.set(((w * k).round() as u32).max(1), ((h * k).round() as u32).max(1));
    // The first picture handed over is the one on screen at `at`, and that's all
    let mut first = None;
    let mut take = |frame| {
        first = Some(frame);
        false
    };
    decode(path, &movie, at, &size, &Late::default(), &mut take, &AtomicBool::new(false)).ok()?;
    first
}

/// A decoded picture waiting its turn to be shown, in YUV until it's sent.
struct Waiting {
    time: i64,
    yuv: Yuv,
}

impl PartialEq for Waiting {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time
    }
}

impl Eq for Waiting {}

impl PartialOrd for Waiting {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Waiting {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.time.cmp(&other.time)
    }
}

fn decode(
    path: &Path,
    movie: &Movie,
    from: f64,
    size: &Size,
    late: &Late,
    out: &mut dyn FnMut(Frame) -> bool,
    stop: &AtomicBool,
) -> Result<(), String> {
    let Some(track) = &movie.video else { return Err("there's no video track".into()) };
    let Codec::H264(avc) = &track.codec else { return Err("it isn't H.264".into()) };
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut decoder = Decoder::new();
    let mut params = Vec::new();
    for set in avc.sps.iter().chain(&avc.pps) {
        params.extend_from_slice(&[0, 0, 0, 1]);
        params.extend_from_slice(set);
    }
    for nal in parse_annex_b(&params) {
        decoder.decode_nal(&nal).map_err(|e| format!("its H.264 settings can't be read ({e})"))?;
    }

    let target = track.ticks(from);
    let mut sender = Sender { track, out, size, target, before: None, sent_any: false, beat: Beat::default() };
    // Decoded in decoding order, so each picture's time is that of the oldest sample fed
    // in that hasn't given one yet
    let mut times: VecDeque<i64> = VecDeque::new();
    let mut waiting: BinaryHeap<Reverse<Waiting>> = BinaryHeap::new();
    let mut buf = Vec::new();
    let mut errors = 0;
    let mut decoded_any = false;
    // How long a picture is on screen, which takes looking through all the samples
    let frame_ticks = track.frame_ticks();
    for i in track.start_for(from)..track.samples.len() {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let sample = track.samples[i];
        if sample.sync {
            // Whatever was in hand before a keyframe is done
            if let Some(yuv) = decoder.flush() {
                let time = times.pop_front().unwrap_or(sample.time);
                waiting.push(Reverse(Waiting { time, yuv }));
            }
            times.clear();
        }
        track.read(&mut file, i, &mut buf).map_err(|e| format!("it can't be read ({e})"))?;
        let nals = parse_avcc(&buf, avc.length_size);
        // A picture nothing refers to can be left out when it's before where showing
        // starts, or too late to show
        let unreferenced = nals.iter().all(|nal| nal.nal_ref_idc == 0 || !is_slice(nal.nal_unit_type));
        let shown_before = sample.time + frame_ticks <= target || track.seconds(sample.time) + 0.1 < late.get();
        let picture = nals.iter().any(|nal| is_slice(nal.nal_unit_type));
        if unreferenced && shown_before && picture {
            continue;
        }
        // Only a sample with a picture in it gives one back
        if picture {
            times.push_back(sample.time);
        }
        for nal in &nals {
            match decoder.decode_nal(nal) {
                Ok(Some(yuv)) => {
                    decoded_any = true;
                    let time = times.pop_front().unwrap_or(sample.time);
                    waiting.push(Reverse(Waiting { time, yuv }));
                }
                Ok(None) => {}
                Err(e) => {
                    errors += 1;
                    // A stream that can't be read at all says why; one broken now and
                    // then shows what it can
                    if !decoded_any && errors >= 8 {
                        return Err(format!("its H.264 can't be decoded ({e})"));
                    }
                }
            }
        }
        while waiting.len() > track.reorder {
            let Reverse(next) = waiting.pop().expect("more than none waiting");
            if !sender.send(next) {
                return Ok(());
            }
        }
    }
    if let Some(yuv) = decoder.flush() {
        let time = times.pop_front().unwrap_or(i64::MAX);
        waiting.push(Reverse(Waiting { time, yuv }));
    }
    while let Some(Reverse(next)) = waiting.pop() {
        if !sender.send(next) {
            return Ok(());
        }
    }
    sender.finish();
    if !sender.sent_any {
        return Err("no pictures came out of it".into());
    }
    Ok(())
}

fn is_slice(kind: NalUnitType) -> bool {
    matches!(kind, NalUnitType::Slice | NalUnitType::SliceIdr)
}

/// Hands pictures over in the order they're shown, from the one on screen at the
/// target time on.
struct Sender<'a> {
    track: &'a Track,
    /// Takes each picture, and says whether to go on
    out: &'a mut dyn FnMut(Frame) -> bool,
    size: &'a Size,
    target: i64,
    /// The latest picture shown before the target, held back until it's known
    /// whether it's the one on screen at the target
    before: Option<Waiting>,
    sent_any: bool,
    beat: Beat,
}

/// Which pictures to show of a video with more a second than [`MOST_A_SECOND`]: those on
/// a steady beat of that many, so 120 a second show as every other one.
#[derive(Default)]
struct Beat {
    /// When the next picture is due, in seconds; none before the first
    due: Option<f64>,
}

impl Beat {
    /// Whether the picture shown at `at` seconds is one to show.
    fn keep(&mut self, at: f64) -> bool {
        let step = 1.0 / MOST_A_SECOND;
        match self.due {
            // A little early still counts: times in ticks don't divide evenly
            Some(due) if at < due - step / 8.0 => false,
            // On the beat; or, after a gap, a new beat from here
            Some(due) if at < due + step => {
                self.due = Some(due + step);
                true
            }
            _ => {
                self.due = Some(at + step);
                true
            }
        }
    }
}

impl Sender<'_> {
    /// Sends `next`, in time order. False once nobody's taking them any more.
    fn send(&mut self, next: Waiting) -> bool {
        if next.time < self.target {
            self.before = Some(next);
            return true;
        }
        if let Some(before) = self.before.take()
            && next.time > self.target
            && !self.put(before)
        {
            return false;
        }
        self.put(next)
    }

    fn finish(&mut self) {
        if let Some(before) = self.before.take() {
            self.put(before);
        }
    }

    fn put(&mut self, waiting: Waiting) -> bool {
        let at = self.track.seconds(waiting.time).max(0.0);
        // Left out before it's made into RGB, which is most of the work of one
        if !self.beat.keep(at) {
            return true;
        }
        let (width, height) = self.size.get();
        let (width, height) =
            if width == 0 || height == 0 { (waiting.yuv.width, waiting.yuv.height) } else { (width, height) };
        let rgb = to_rgb(&waiting.yuv, width, height);
        self.sent_any = true;
        (self.out)(Frame { at, width, height, rgb })
    }
}

/// `yuv` scaled to `width` × `height` and turned into RGB. Shrinking averages the
/// pixels each one covers; growing repeats them. Bands of rows are done on threads of
/// their own, as a full HD picture is a lot of pixels for one.
pub fn to_rgb(yuv: &Yuv, width: u32, height: u32) -> Vec<u8> {
    let (sw, sh) = (yuv.width as usize, yuv.height as usize);
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    if sw == 0 || sh == 0 || yuv.y.len() < sw * sh {
        return vec![128; w * h * 3];
    }
    // The decoder's chroma planes are half the size, rounded down. A picture too small
    // to have them is gray.
    let neutral = [128u8];
    let (cw, ch) = (sw / 2, sh / 2);
    let (u, v, cw, ch) = if cw > 0 && ch > 0 && yuv.u.len() >= cw * ch && yuv.v.len() >= cw * ch {
        (&yuv.u[..], &yuv.v[..], cw, ch)
    } else {
        (&neutral[..], &neutral[..], 1, 1)
    };
    let planes = Planes { y: &yuv.y, u, v, sw, sh, cw, ch, hd: sh >= 720 };
    let mut rgb = vec![0; w * h * 3];
    let threads = (w * h / 200_000).clamp(1, 4);
    let band = h.div_ceil(threads);
    std::thread::scope(|scope| {
        for (i, out) in rgb.chunks_mut(band * w * 3).enumerate() {
            let planes = &planes;
            scope.spawn(move || planes.rows(out, i * band, (w, h)));
        }
    });
    rgb
}

struct Planes<'a> {
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
    sw: usize,
    sh: usize,
    cw: usize,
    ch: usize,
    /// BT.709 rather than BT.601
    hd: bool,
}

/// A pixel's 0–255 from its sum in 1/1024ths, rounding already added.
#[inline(always)]
fn clamp(n: i32) -> u8 {
    (n >> 10).clamp(0, 255) as u8
}

/// What to multiply a sum of `count` samples by, then shift down 40 bits, to divide it
/// by `count`: exact for any such sum while `count` is under 2^16, which keeps sum times
/// count under 2^40. Zero where it wouldn't be exact, for dividing instead.
fn reciprocal(count: usize) -> u64 {
    if count < 1 << 16 { (1u64 << 40).div_ceil(count.max(1) as u64) } else { 0 }
}

/// `sum` over `count` samples, by `recip`, its [`reciprocal`].
#[inline(always)]
fn average(sum: u64, count: usize, recip: u64) -> i32 {
    if recip != 0 { ((sum * recip) >> 40) as i32 } else { (sum / count as u64) as i32 }
}

/// Each output column's or row's span of source pixels, for `n` of `out` from `from` on,
/// made from `src`: the pixels it covers, the ones at its edges whole.
fn spans(out: usize, src: usize, from: usize, n: usize) -> Vec<(usize, usize)> {
    (from..from + n)
        .map(|i| {
            let start = (i * src / out).min(src - 1);
            (start, ((i + 1) * src).div_ceil(out).clamp(start + 1, src))
        })
        .collect()
}

/// `sums` set to the running sums along a row of `plane`'s columns, each summed down
/// rows `y0..y1`: `sums[c]` is the sum of the columns before `c`, so any span's is a
/// subtraction. `down` is room for the columns' sums.
fn running_sums(sums: &mut [u64], down: &mut [u32], plane: &[u8], stride: usize, (y0, y1): (usize, usize)) {
    let width = down.len();
    for (d, &p) in down.iter_mut().zip(&plane[y0 * stride..][..width]) {
        *d = u32::from(p);
    }
    for y in y0 + 1..y1 {
        for (d, &p) in down.iter_mut().zip(&plane[y * stride..][..width]) {
            *d += u32::from(p);
        }
    }
    let mut total = 0;
    sums[0] = 0;
    for (s, &d) in sums[1..].iter_mut().zip(down.iter()) {
        total += u64::from(d);
        *s = total;
    }
}

impl Planes<'_> {
    /// Output rows from `first` on into `out`, for a picture `w` × `h`.
    fn rows(&self, out: &mut [u8], first: usize, (w, h): (usize, usize)) {
        // Limited range, in 1/1024ths
        let (rv, gu, gv, bu) = if self.hd { (1836, -218, -546, 2163) } else { (1634, -401, -832, 2066) };
        let rows = out.len() / (w * 3);
        let put = |px: &mut [u8], luma: i32, (dr, dg, db): (i32, i32, i32)| {
            let l = (luma - 16) * 1192 + 512;
            px[0] = clamp(l + dr);
            px[1] = clamp(l + dg);
            px[2] = clamp(l + db);
        };
        let chroma = |u: i32, v: i32| (rv * (v - 128), gu * (u - 128) + gv * (v - 128), bu * (u - 128));
        if (w, h) == (self.sw, self.sh) {
            // The same size: each pixel as it is, two at a time, as they share their chroma
            for r in 0..rows {
                let y = first + r;
                let luma = &self.y[y * self.sw..][..w];
                let cy = (y / 2).min(self.ch - 1) * self.cw;
                let (us, vs) = (&self.u[cy..][..self.cw], &self.v[cy..][..self.cw]);
                let line = &mut out[r * w * 3..][..w * 3];
                for (i, (pair, pixels)) in luma.chunks(2).zip(line.chunks_mut(6)).enumerate() {
                    let c = i.min(self.cw - 1);
                    let tint = chroma(i32::from(us[c]), i32::from(vs[c]));
                    for (&l, px) in pair.iter().zip(pixels.chunks_exact_mut(3)) {
                        put(px, i32::from(l), tint);
                    }
                }
            }
            return;
        }
        // Each output pixel is the average of the source pixels its spans cover: the
        // rows summed down each column, then running sums along them, so a span's sum
        // is a subtraction, and divided by multiplying
        let (cols, ccols) = (spans(w, self.sw, 0, w), spans(w, self.cw, 0, w));
        let (lines, clines) = (spans(h, self.sh, first, rows), spans(h, self.ch, first, rows));
        let (mut ys, mut us, mut vs) = (vec![0; self.sw + 1], vec![0; self.cw + 1], vec![0; self.cw + 1]);
        let (mut down, mut cdown) = (vec![0; self.sw], vec![0; self.cw]);
        let widest = cols.iter().chain(&ccols).map(|(a, b)| b - a).max().unwrap_or(1);
        let (mut recip, mut crecip) = (vec![0; widest + 1], vec![0; widest + 1]);
        for r in 0..rows {
            running_sums(&mut ys, &mut down, self.y, self.sw, lines[r]);
            running_sums(&mut us, &mut cdown, self.u, self.cw, clines[r]);
            running_sums(&mut vs, &mut cdown, self.v, self.cw, clines[r]);
            let (tall, ctall) = (lines[r].1 - lines[r].0, clines[r].1 - clines[r].0);
            for k in 1..=widest {
                (recip[k], crecip[k]) = (reciprocal(k * tall), reciprocal(k * ctall));
            }
            let line = &mut out[r * w * 3..][..w * 3];
            for ((px, &(x0, x1)), &(c0, c1)) in line.chunks_exact_mut(3).zip(&cols).zip(&ccols) {
                let (n, cn) = (x1 - x0, c1 - c0);
                let luma = average(ys[x1] - ys[x0], n * tall, recip[n]);
                let u = average(us[c1] - us[c0], cn * ctall, crecip[cn]);
                let v = average(vs[c1] - vs[c0], cn * ctall, crecip[cn]);
                put(px, luma, chroma(u, v));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::video::mp4;

    fn clip() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/clip.mp4")
    }

    /// Everything the thread hands over, from `from` on.
    fn all(path: &Path, from: f64, size: (u32, u32)) -> (Vec<Frame>, Option<String>) {
        let movie = Arc::new(mp4::open(path).unwrap());
        let target = Size::default();
        target.set(size.0, size.1);
        let frames = Frames::start(path, movie, from, target, Late::default());
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match frames.next() {
                Some(Decoded::Frame(frame)) => got.push(frame),
                Some(Decoded::End) => return (got, None),
                Some(Decoded::Failed(why)) => return (got, Some(why)),
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        panic!("the decoder didn't finish");
    }

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 3] {
        let at = ((y * frame.width + x) * 3) as usize;
        frame.rgb[at..at + 3].try_into().unwrap()
    }

    fn near(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= 3)
    }

    #[test]
    fn decodes_every_picture_in_the_order_theyre_shown() {
        let (frames, failed) = all(&clip(), 0.0, (72, 40));
        assert_eq!(failed, None);
        let times: Vec<f64> = frames.iter().map(|f| (f.at * 10.0).round() / 10.0).collect();
        assert_eq!(times, (0..12).map(|i| f64::from(i) / 10.0).collect::<Vec<_>>());
        // Red for the first half, blue after, with a white square moving right 5
        // pixels a picture, as ffmpeg makes them (within 2 of its colours, all through)
        for (i, frame) in frames.iter().enumerate() {
            let x = 4 + 5 * i as u32;
            let ground = if i < 6 { [207, 32, 32] } else { [32, 62, 223] };
            let white = pixel(frame, x + 6, 14).iter().all(|c| *c > 200);
            assert!(white, "{i}: {:?}", pixel(frame, x + 6, 14));
            assert!(near(pixel(frame, x + 6, 30), ground), "{i}: {:?}", pixel(frame, x + 6, 30));
            assert!(near(pixel(frame, x.saturating_sub(3), 14), ground), "{i}: {:?}", pixel(frame, x - 3, 14));
        }
    }

    #[test]
    fn starts_with_the_picture_on_screen_at_the_time_asked_for() {
        let (frames, _) = all(&clip(), 0.75, (36, 20));
        let times: Vec<f64> = frames.iter().map(|f| (f.at * 10.0).round() / 10.0).collect();
        assert_eq!(times, [0.7, 0.8, 0.9, 1.0, 1.1]);
        assert_eq!((frames[0].width, frames[0].height, frames[0].rgb.len()), (36, 20, 36 * 20 * 3));
        let (frames, _) = all(&clip(), 0.3, (72, 40));
        assert_eq!(frames.first().map(|f| (f.at * 10.0).round() / 10.0), Some(0.3));
    }

    #[test]
    fn a_damaged_video_stops_with_a_reason() {
        let whole = std::fs::read(clip()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("damaged.mp4");
        // The pictures scribbled over, the index left alone
        let movie = mp4::open(&clip()).unwrap();
        let video = movie.video.as_ref().unwrap();
        let mut broken = whole.clone();
        for sample in &video.samples {
            for b in &mut broken[sample.offset as usize + 4..(sample.offset + u64::from(sample.size)) as usize] {
                *b = b.wrapping_mul(31).wrapping_add(7);
            }
        }
        std::fs::write(&path, &broken).unwrap();
        let (_, failed) = all(&path, 0.0, (72, 40));
        assert!(failed.is_some());
    }

    #[test]
    fn decodes_x264s_hard_parts_exactly_as_ffmpeg_does() {
        // pyramid.mp4 has all that rust_h264 0.4.0 got wrong: a cropped width, two slices
        // a frame not deblocked across, reference B-frames, weighted P-frames listing a
        // picture twice, and MMCOs naming pictures after frame_num wraps. This is ffmpeg's
        // decode of it, FNV-1a over every frame's planes in the order they're shown.
        use rust_h264::decoder::OrderedDecoder;
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/pyramid.mp4");
        let movie = mp4::open(&path).unwrap();
        let track = movie.video.as_ref().unwrap();
        let Codec::H264(avc) = &track.codec else { panic!("{:?}", track.codec) };
        let mut decoder = OrderedDecoder::new();
        let mut params = Vec::new();
        for set in avc.sps.iter().chain(&avc.pps) {
            params.extend_from_slice(&[0, 0, 0, 1]);
            params.extend_from_slice(set);
        }
        for nal in parse_annex_b(&params) {
            decoder.decode_nal(&nal).unwrap();
        }
        let (mut hash, mut frames) = (0xcbf2_9ce4_8422_2325u64, 0);
        let mut take = |frame: Yuv| {
            for &b in frame.y.iter().chain(&frame.u).chain(&frame.v) {
                hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
            frames += 1;
        };
        let mut file = File::open(&path).unwrap();
        let mut buf = Vec::new();
        for i in 0..track.samples.len() {
            track.read(&mut file, i, &mut buf).unwrap();
            for nal in parse_avcc(&buf, avc.length_size) {
                decoder.decode_nal(&nal).unwrap().into_iter().for_each(&mut take);
            }
        }
        decoder.flush().into_iter().for_each(&mut take);
        assert_eq!((frames, hash), (80, 0x65ad_7e55_7595_3d57));
    }

    /// The conversion as it was: each output pixel's spans summed on their own, and
    /// divided.
    fn by_each_pixel(yuv: &Yuv, w: usize, h: usize) -> Vec<u8> {
        let (sw, sh, cw, ch) =
            (yuv.width as usize, yuv.height as usize, yuv.width as usize / 2, yuv.height as usize / 2);
        let (cols, ccols, lines, clines) =
            (spans(w, sw, 0, w), spans(w, cw, 0, w), spans(h, sh, 0, h), spans(h, ch, 0, h));
        let average = |plane: &[u8], stride: usize, (x0, x1): (usize, usize), (y0, y1): (usize, usize)| {
            let sum: u32 = (y0..y1).flat_map(|y| &plane[y * stride + x0..y * stride + x1]).map(|&v| u32::from(v)).sum();
            (sum / ((x1 - x0) * (y1 - y0)) as u32) as i32
        };
        let (rv, gu, gv, bu) = if sh >= 720 { (1836, -218, -546, 2163) } else { (1634, -401, -832, 2066) };
        let clamp = |n: i32| ((n + 512) >> 10).clamp(0, 255) as u8;
        let mut out = Vec::new();
        for r in 0..h {
            for x in 0..w {
                let (l, u, v) = if (w, h) == (sw, sh) {
                    let c = (r / 2).min(ch - 1) * cw + (x / 2).min(cw - 1);
                    (i32::from(yuv.y[r * sw + x]), i32::from(yuv.u[c]), i32::from(yuv.v[c]))
                } else {
                    let luma = average(&yuv.y, sw, cols[x], lines[r]);
                    (luma, average(&yuv.u, cw, ccols[x], clines[r]), average(&yuv.v, cw, ccols[x], clines[r]))
                };
                let (l, u, v) = ((l - 16) * 1192, u - 128, v - 128);
                out.extend([clamp(l + rv * v), clamp(l + gu * u + gv * v), clamp(l + bu * u)]);
            }
        }
        out
    }

    #[test]
    fn the_fast_conversion_makes_the_same_pixels() {
        let mut seed = 1u32;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (seed >> 16) as u8
                })
                .collect()
        };
        for (sw, sh) in [(64, 36), (33, 19), (512, 256), (1280, 720)] {
            let yuv = Yuv {
                width: sw,
                height: sh,
                y: noise((sw * sh) as usize),
                u: noise((sw / 2 * (sh / 2)) as usize),
                v: noise((sw / 2 * (sh / 2)) as usize),
                pic_order_cnt: 0,
            };
            // The same size, shrinking a little and a lot, odd sizes, a column, a
            // pixel (which divides rather than multiplies), and growing
            for (w, h) in [(sw, sh), (sw * 6 / 7, sh * 6 / 7), (sw / 3, sh / 5), (37, 21), (1, sh), (1, 1), (sw * 2, 7)]
            {
                let (w, h) = (w.max(1), h.max(1));
                assert!(to_rgb(&yuv, w, h) == by_each_pixel(&yuv, w as usize, h as usize), "{sw}x{sh} to {w}x{h}");
            }
        }
    }

    #[test]
    fn a_still_is_the_picture_on_screen_then() {
        // A second in, the eleventh picture: blue, the white square 50 pixels on
        let frame = still(&clip(), 1.0, (72, 40)).unwrap();
        assert_eq!((frame.width, frame.height, (frame.at * 10.0).round()), (72, 40, 10.0));
        assert!(pixel(&frame, 60, 14).iter().all(|c| *c > 200), "{:?}", pixel(&frame, 60, 14));
        assert!(near(pixel(&frame, 60, 30), [32, 62, 223]), "{:?}", pixel(&frame, 60, 30));
        // Smaller, keeping its shape; and past its end, its last picture
        let frame = still(&clip(), 5.0, (36, 36)).unwrap();
        assert_eq!((frame.width, frame.height, (frame.at * 10.0).round()), (36, 20, 11.0));
        // Not one readme plays
        let other = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/other-codec.mp4");
        assert!(still(&other, 1.0, (72, 40)).is_none());
    }

    #[test]
    fn no_more_than_sixty_pictures_a_second_are_made() {
        let kept = |fps: f64, seconds: f64| {
            let mut beat = Beat::default();
            (0..(fps * seconds) as usize).filter(|&i| beat.keep(i as f64 / fps)).count()
        };
        // All of them up to 60 a second, the times a little off as ticks make them
        assert_eq!(kept(30.0, 10.0), 300);
        assert_eq!(kept(59.94, 10.0), 599);
        assert_eq!(kept(60.0, 10.0), 600);
        // Above that, a steady 60 or so
        assert_eq!(kept(120.0, 10.0), 600);
        assert!((590..=610).contains(&kept(100.0, 10.0)), "{}", kept(100.0, 10.0));
        assert!((590..=610).contains(&kept(144.0, 10.0)), "{}", kept(144.0, 10.0));
        // After a gap, the beat starts again from the next picture
        let mut beat = Beat::default();
        assert!(beat.keep(0.0) && !beat.keep(0.005) && beat.keep(5.0) && !beat.keep(5.005) && beat.keep(5.017));
    }

    #[test]
    fn shrinking_averages_and_growing_repeats() {
        // 4×2 gray ramp, chroma neutral
        let yuv = Yuv {
            width: 4,
            height: 2,
            y: vec![16, 16, 235, 235, 16, 16, 235, 235],
            u: vec![128; 2],
            v: vec![128; 2],
            pic_order_cnt: 0,
        };
        assert_eq!(to_rgb(&yuv, 2, 1), [0, 0, 0, 255, 255, 255]);
        assert_eq!(to_rgb(&yuv, 8, 1)[..6], [0, 0, 0, 0, 0, 0]);
    }
}
