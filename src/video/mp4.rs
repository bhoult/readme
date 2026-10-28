//! Reading MP4 and QuickTime files: their first video and sound tracks, and where in
//! the file each sample (a frame, or a stretch of sound) is, so it can be read when
//! it's wanted. Fragmented files, as streamed, aren't read.
//!
//! Every count in the file is checked against the bytes there are for it before
//! anything is made that size, so a broken or hostile file can't ask for gigabytes.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// How much of the file the movie header may take, read whole into memory.
const MAX_MOOV: u64 = 256 << 20;
/// How many samples a track may have: a day of 48 kHz sound, or 46 hours of 30 fps video.
const MAX_SAMPLES: usize = 5_000_000;
/// The biggest single sample read: a frame of 8K video comes nowhere near.
pub const MAX_SAMPLE: u32 = 64 << 20;

#[derive(Debug)]
pub struct Movie {
    pub video: Option<Track>,
    pub audio: Option<Track>,
}

#[derive(Debug)]
pub struct Track {
    pub codec: Codec,
    /// Ticks per second for the times below
    pub timescale: u32,
    /// From the track header, or else the sample description
    pub width: u32,
    pub height: u32,
    /// In decoding order
    pub samples: Vec<Sample>,
    /// How many later-decoded samples are shown before an earlier one, at most: how
    /// many frames have to wait for the one shown before them
    pub reorder: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// When it's shown, in the track's ticks, counted from the start of the movie
    pub time: i64,
    /// Where decoding can start
    pub sync: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    H264(AvcConfig),
    /// AAC, with its AudioSpecificConfig
    Aac(Vec<u8>),
    /// Anything else, by its sample entry's four letters
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    /// Bytes in front of each NAL unit giving its length
    pub length_size: usize,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl Track {
    /// Seconds into the movie for a time in this track's ticks.
    pub fn seconds(&self, ticks: i64) -> f64 {
        ticks as f64 / f64::from(self.timescale.max(1))
    }

    /// From the start of the movie to the end of its last sample, in seconds.
    pub fn duration(&self) -> f64 {
        let Some(last) = self.samples.iter().map(|s| s.time).max() else { return 0.0 };
        // The last sample lasts about as long as the others
        self.seconds(last + self.frame_ticks()).max(0.0)
    }

    /// How long a sample lasts, on average, in ticks.
    pub fn frame_ticks(&self) -> i64 {
        let Some(last) = self.samples.iter().map(|s| s.time).max() else { return 1 };
        let first = self.samples.iter().map(|s| s.time).min().unwrap_or(0);
        ((last - first) / (self.samples.len() as i64 - 1).max(1)).max(1)
    }

    /// `seconds` in this track's ticks.
    pub fn ticks(&self, seconds: f64) -> i64 {
        (seconds * f64::from(self.timescale)).round() as i64
    }

    /// The sample to start decoding from to show the picture at `seconds`: the last
    /// sync sample shown no later than that.
    pub fn start_for(&self, seconds: f64) -> usize {
        let ticks = self.ticks(seconds);
        // The first sample shown after `seconds`, in decoding order, bounds it
        let mut start = 0;
        for (i, sample) in self.samples.iter().enumerate() {
            if sample.sync {
                if sample.time > ticks {
                    break;
                }
                start = i;
            }
        }
        start
    }

    /// Reads sample `i` into `buf`.
    pub fn read(&self, file: &mut File, i: usize, buf: &mut Vec<u8>) -> std::io::Result<()> {
        let sample = self.samples[i];
        buf.resize(sample.size as usize, 0);
        file.seek(SeekFrom::Start(sample.offset))?;
        file.read_exact(buf)
    }
}

/// Why a file can't be read as a movie.
#[derive(Debug)]
pub struct Unreadable(pub String);

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn bad<T>(why: &str) -> Result<T, Unreadable> {
    Err(Unreadable(why.to_string()))
}

/// Reads the movie header of the MP4 or QuickTime file at `path`.
pub fn open(path: &Path) -> Result<Movie, Unreadable> {
    let mut file = File::open(path).map_err(|e| Unreadable(e.to_string()))?;
    let len = file.metadata().map_err(|e| Unreadable(e.to_string()))?.len();
    let moov = find_moov(&mut file, len)?;
    parse_moov(&moov, len)
}

/// The movie header's contents, wherever it is among the top-level boxes.
fn find_moov(file: &mut File, len: u64) -> Result<Vec<u8>, Unreadable> {
    let mut at = 0u64;
    let mut seen_ftyp = false;
    while at + 8 <= len {
        let mut head = [0u8; 16];
        file.seek(SeekFrom::Start(at)).map_err(|e| Unreadable(e.to_string()))?;
        let got = read_up_to(file, &mut head);
        if got < 8 {
            break;
        }
        let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let kind = [head[4], head[5], head[6], head[7]];
        let (size, header) = match size32 {
            0 => (len - at, 8),
            1 if got >= 16 => (u64::from_be_bytes(head[8..16].try_into().unwrap()), 16),
            1 => return bad("it's cut short"),
            n => (u64::from(n), 8),
        };
        if size < header || at + size > len {
            return bad(if seen_ftyp { "it's cut short" } else { "it isn't an MP4 file" });
        }
        match &kind {
            b"ftyp" => seen_ftyp = true,
            b"moov" => {
                let body = size - header;
                if body > MAX_MOOV {
                    return bad("its index is too big");
                }
                let mut moov = vec![0; body as usize];
                file.seek(SeekFrom::Start(at + header)).map_err(|e| Unreadable(e.to_string()))?;
                file.read_exact(&mut moov).map_err(|e| Unreadable(e.to_string()))?;
                return Ok(moov);
            }
            b"moof" => return bad("it's a fragmented MP4"),
            _ if !seen_ftyp && at == 0 && !matches!(&kind, b"wide" | b"free" | b"skip" | b"mdat" | b"pnot") => {
                return bad("it isn't an MP4 file");
            }
            _ => {}
        }
        at += size;
    }
    bad(if seen_ftyp {
        "it has no movie header, maybe because it's still being written"
    } else {
        "it isn't an MP4 file"
    })
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> usize {
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    got
}

/// A cursor over a box's bytes that fails, rather than panics, at the end.
struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn left(&self) -> usize {
        self.data.len() - self.at
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Unreadable> {
        if n > self.left() {
            return bad("part of its index is cut short");
        }
        let bytes = &self.data[self.at..self.at + n];
        self.at += n;
        Ok(bytes)
    }

    fn skip(&mut self, n: usize) -> Result<(), Unreadable> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, Unreadable> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Unreadable> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, Unreadable> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, Unreadable> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// A count of entries `entry` bytes each, which there must be room for.
    fn count(&mut self, entry: usize) -> Result<usize, Unreadable> {
        let n = self.u32()? as usize;
        if n.saturating_mul(entry) > self.left() {
            return bad("part of its index is cut short");
        }
        Ok(n)
    }

    /// The boxes inside this one: their four letters and contents.
    fn boxes(mut self) -> impl Iterator<Item = Result<([u8; 4], &'a [u8]), Unreadable>> {
        std::iter::from_fn(move || {
            if self.left() == 0 {
                return None;
            }
            let item = (|| {
                let size = self.u32()? as u64;
                let kind: [u8; 4] = self.take(4)?.try_into().unwrap();
                let (size, header) = match size {
                    0 => (self.left() as u64 + 8, 8),
                    1 => (self.u64()?, 16),
                    n => (n, 8),
                };
                if size < header {
                    return bad("part of its index is broken");
                }
                Ok((kind, self.take((size - header) as usize)?))
            })();
            if item.is_err() {
                // Nothing more can be made of what's left
                self.at = self.data.len();
            }
            Some(item)
        })
    }
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>, Unreadable> {
    for item in Bytes::new(data).boxes() {
        let (k, body) = item?;
        if &k == kind {
            return Ok(Some(body));
        }
    }
    Ok(None)
}

fn parse_moov(moov: &[u8], file_len: u64) -> Result<Movie, Unreadable> {
    let mut movie_timescale = 1000;
    let mut movie = Movie { video: None, audio: None };
    let mut fragmented = false;
    for item in Bytes::new(moov).boxes() {
        let (kind, body) = item?;
        match &kind {
            b"mvhd" => {
                let mut b = Bytes::new(body);
                let version = b.u8()?;
                b.skip(3 + if version == 1 { 16 } else { 8 })?;
                movie_timescale = b.u32()?.max(1);
            }
            b"mvex" => fragmented = true,
            b"trak" => {
                let Some((handler, track)) = parse_trak(body, movie_timescale, file_len)? else { continue };
                match &handler {
                    b"vide" if movie.video.is_none() => movie.video = Some(track),
                    b"soun" if movie.audio.is_none() => movie.audio = Some(track),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if fragmented && movie.video.as_ref().is_none_or(|v| v.samples.is_empty()) {
        return bad("it's a fragmented MP4");
    }
    if movie.video.is_none() {
        return bad("it has no video track");
    }
    Ok(movie)
}

/// A track's handler ("vide", "soun") and the track, or None for one with no samples.
fn parse_trak(trak: &[u8], movie_timescale: u32, file_len: u64) -> Result<Option<([u8; 4], Track)>, Unreadable> {
    let (mut width, mut height) = (0, 0);
    if let Some(tkhd) = child(trak, b"tkhd")? {
        let mut b = Bytes::new(tkhd);
        let version = b.u8()?;
        // flags, times, id, reserved, duration, reserved, layer, group, volume,
        // reserved, matrix
        b.skip(3 + if version == 1 { 32 } else { 20 } + 8 + 8 + 36)?;
        width = b.u32()? >> 16;
        height = b.u32()? >> 16;
    }
    let edit = match child(trak, b"edts")?.map(|e| child(e, b"elst")).transpose()?.flatten() {
        Some(elst) => parse_elst(elst)?,
        None => Edit::default(),
    };
    let Some(mdia) = child(trak, b"mdia")? else { return Ok(None) };
    let Some(mdhd) = child(mdia, b"mdhd")? else { return Ok(None) };
    let mut b = Bytes::new(mdhd);
    let version = b.u8()?;
    b.skip(3 + if version == 1 { 16 } else { 8 })?;
    let timescale = b.u32()?.max(1);
    let Some(hdlr) = child(mdia, b"hdlr")? else { return Ok(None) };
    let mut b = Bytes::new(hdlr);
    b.skip(8)?;
    let handler: [u8; 4] = b.take(4)?.try_into().unwrap();
    if !matches!(&handler, b"vide" | b"soun") {
        return Ok(None);
    }
    let Some(stbl) = child(mdia, b"minf")?.map(|m| child(m, b"stbl")).transpose()?.flatten() else {
        return Ok(None);
    };
    let Some(stsd) = child(stbl, b"stsd")? else { return Ok(None) };
    let (codec, (entry_width, entry_height)) = parse_stsd(stsd)?;
    if width == 0 || height == 0 {
        (width, height) = (entry_width, entry_height);
    }
    // When the first sample is shown: after any empty edit, from the edit's start
    let shift = (edit.delay as f64 * f64::from(timescale) / f64::from(movie_timescale)) as i64 - edit.media_start;
    let (samples, reorder) = parse_samples(stbl, shift, file_len)?;
    if samples.is_empty() {
        return Ok(None);
    }
    Ok(Some((handler, Track { codec, timescale, width, height, samples, reorder })))
}

#[derive(Default)]
struct Edit {
    /// Empty time at the start, in the movie's ticks
    delay: i64,
    /// Where in the track the first edit starts, in its ticks
    media_start: i64,
}

fn parse_elst(elst: &[u8]) -> Result<Edit, Unreadable> {
    let mut b = Bytes::new(elst);
    let version = b.u8()?;
    b.skip(3)?;
    let entry = if version == 1 { 20 } else { 12 };
    let n = b.count(entry)?;
    let mut edit = Edit::default();
    for _ in 0..n {
        let (duration, media_time) = if version == 1 {
            (b.u64()? as i64, b.u64()? as i64)
        } else {
            (i64::from(b.u32()?), i64::from(b.u32()? as i32))
        };
        b.skip(4)?;
        if media_time == -1 {
            edit.delay += duration;
        } else {
            edit.media_start = media_time;
            break;
        }
    }
    Ok(edit)
}

/// The codec of the first sample description, and the size it gives for video.
fn parse_stsd(stsd: &[u8]) -> Result<(Codec, (u32, u32)), Unreadable> {
    let mut b = Bytes::new(stsd);
    b.skip(4)?;
    if b.u32()? == 0 {
        return bad("it has no sample description");
    }
    let Some(entry) = Bytes::new(&stsd[8..]).boxes().next() else { return bad("it has no sample description") };
    let (kind, body) = entry?;
    let name = String::from_utf8_lossy(&kind).trim().to_string();
    match &kind {
        b"avc1" | b"avc3" => {
            let mut b = Bytes::new(body);
            // reserved, data reference, pre-defined and reserved
            b.skip(6 + 2 + 16)?;
            let (width, height) = (u32::from(b.u16()?), u32::from(b.u16()?));
            // resolutions, reserved, frame count, compressor name, depth, pre-defined
            b.skip(4 + 4 + 4 + 2 + 32 + 2 + 2)?;
            let rest = &body[b.at..];
            let Some(avcc) = child(rest, b"avcC")? else { return bad("its H.264 settings are missing") };
            Ok((Codec::H264(parse_avcc(avcc)?), (width, height)))
        }
        b"mp4a" => {
            let mut b = Bytes::new(body);
            b.skip(6 + 2)?;
            // QuickTime's sound descriptions have more fields in later versions
            let version = b.u16()?;
            b.skip(6 + 2 + 2 + 4 + 4)?;
            b.skip(match version {
                1 => 16,
                2 => 36,
                _ => 0,
            })?;
            let rest = &body[b.at.min(body.len())..];
            let esds = match child(rest, b"esds")? {
                Some(esds) => Some(esds),
                None => child(rest, b"wave")?.map(|w| child(w, b"esds")).transpose()?.flatten(),
            };
            match esds.map(parse_esds).transpose()? {
                Some(Some(config)) => Ok((Codec::Aac(config), (0, 0))),
                _ => Ok((Codec::Other(name), (0, 0))),
            }
        }
        _ => Ok((Codec::Other(name), (0, 0))),
    }
}

fn parse_avcc(avcc: &[u8]) -> Result<AvcConfig, Unreadable> {
    let mut b = Bytes::new(avcc);
    b.skip(4)?;
    let length_size = usize::from(b.u8()? & 3) + 1;
    let sets = |b: &mut Bytes, mask: u8| -> Result<Vec<Vec<u8>>, Unreadable> {
        let n = b.u8()? & mask;
        (0..n)
            .map(|_| {
                let len = usize::from(b.u16()?);
                Ok(b.take(len)?.to_vec())
            })
            .collect()
    };
    let sps = sets(&mut b, 0x1f)?;
    let pps = sets(&mut b, 0xff)?;
    Ok(AvcConfig { length_size, sps, pps })
}

/// The AudioSpecificConfig in an ES descriptor, if it's AAC.
fn parse_esds(esds: &[u8]) -> Result<Option<Vec<u8>>, Unreadable> {
    let mut b = Bytes::new(esds);
    b.skip(4)?;
    // Descriptors: a tag, then a length seven bits at a time
    let descriptor = |b: &mut Bytes| -> Result<(u8, usize), Unreadable> {
        let tag = b.u8()?;
        let mut len = 0usize;
        for _ in 0..4 {
            let byte = b.u8()?;
            len = len << 7 | usize::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
        Ok((tag, len))
    };
    let (tag, _) = descriptor(&mut b)?;
    if tag != 3 {
        return Ok(None);
    }
    b.skip(2)?;
    let flags = b.u8()?;
    if flags & 0x80 != 0 {
        b.skip(2)?;
    }
    if flags & 0x40 != 0 {
        let len = usize::from(b.u8()?);
        b.skip(len)?;
    }
    if flags & 0x20 != 0 {
        b.skip(2)?;
    }
    let (tag, _) = descriptor(&mut b)?;
    // 0x40 is MPEG-4 audio; 0x66 to 0x68 the MPEG-2 AAC profiles
    let object_type = b.u8()?;
    if tag != 4 || !matches!(object_type, 0x40 | 0x66..=0x68) {
        return Ok(None);
    }
    b.skip(12)?;
    let (tag, len) = descriptor(&mut b)?;
    if tag != 5 || len < 2 {
        return Ok(None);
    }
    Ok(Some(b.take(len)?.to_vec()))
}

/// Every sample's place in the file and when it's shown, in decoding order, and how
/// far they're reordered for showing.
fn parse_samples(stbl: &[u8], shift: i64, file_len: u64) -> Result<(Vec<Sample>, usize), Unreadable> {
    let table = |kind: &[u8; 4]| child(stbl, kind);

    // Sizes
    let Some(stsz) = table(b"stsz")?.or(table(b"stz2")?) else { return bad("its sample sizes are missing") };
    let mut b = Bytes::new(stsz);
    b.skip(4)?;
    let sizes: Vec<u32> = if table(b"stsz")?.is_some() {
        let size = b.u32()?;
        if size != 0 {
            let n = b.u32()? as usize;
            if n > MAX_SAMPLES || u64::from(size).saturating_mul(n as u64) > file_len {
                return bad("it says it has more samples than it can");
            }
            vec![size; n]
        } else {
            let n = b.count(4)?;
            (0..n).map(|_| b.u32()).collect::<Result<_, _>>()?
        }
    } else {
        b.skip(3)?;
        let bits = b.u8()?;
        let n = b.u32()? as usize;
        if !matches!(bits, 4 | 8 | 16) || n.saturating_mul(usize::from(bits)) / 8 > b.left() {
            return bad("part of its index is cut short");
        }
        match bits {
            16 => (0..n).map(|_| b.u16().map(u32::from)).collect::<Result<_, _>>()?,
            8 => (0..n).map(|_| b.u8().map(u32::from)).collect::<Result<_, _>>()?,
            _ => {
                let bytes = b.take(n.div_ceil(2))?;
                (0..n).map(|i| u32::from(if i % 2 == 0 { bytes[i / 2] >> 4 } else { bytes[i / 2] & 15 })).collect()
            }
        }
    };
    let n = sizes.len();
    if n > MAX_SAMPLES {
        return bad("it has more samples than readme reads");
    }

    // Decoding times, then when each is shown
    let Some(stts) = table(b"stts")? else { return bad("its sample times are missing") };
    let mut b = Bytes::new(stts);
    b.skip(4)?;
    let entries = b.count(8)?;
    let mut times = Vec::with_capacity(n);
    let mut t = 0i64;
    for _ in 0..entries {
        let (count, delta) = (b.u32()? as usize, i64::from(b.u32()?));
        for _ in 0..count.min(n - times.len()) {
            times.push(t);
            t += delta;
        }
    }
    while times.len() < n {
        times.push(t);
    }
    if let Some(ctts) = table(b"ctts")? {
        let mut b = Bytes::new(ctts);
        let version = b.u8()?;
        b.skip(3)?;
        let entries = b.count(8)?;
        let mut i = 0;
        for _ in 0..entries {
            let count = b.u32()? as usize;
            let raw = b.u32()?;
            let offset = if version == 0 { i64::from(raw) } else { i64::from(raw as i32) };
            for _ in 0..count.min(n - i) {
                times[i] += offset;
                i += 1;
            }
        }
    }

    // Where: chunks in the file, and how many samples each holds
    let offsets: Vec<u64> = if let Some(stco) = table(b"stco")? {
        let mut b = Bytes::new(stco);
        b.skip(4)?;
        let entries = b.count(4)?;
        (0..entries).map(|_| b.u32().map(u64::from)).collect::<Result<_, _>>()?
    } else if let Some(co64) = table(b"co64")? {
        let mut b = Bytes::new(co64);
        b.skip(4)?;
        let entries = b.count(8)?;
        (0..entries).map(|_| b.u64()).collect::<Result<_, _>>()?
    } else {
        return bad("its chunk offsets are missing");
    };
    let Some(stsc) = table(b"stsc")? else { return bad("its chunk table is missing") };
    let mut b = Bytes::new(stsc);
    b.skip(4)?;
    let entries = b.count(12)?;
    let runs: Vec<(usize, usize)> = (0..entries)
        .map(|_| {
            let first = b.u32()? as usize;
            let per = b.u32()? as usize;
            b.skip(4)?;
            Ok((first.max(1) - 1, per))
        })
        .collect::<Result<_, Unreadable>>()?;
    let mut samples = Vec::with_capacity(n);
    for (r, &(first, per)) in runs.iter().enumerate() {
        let end = runs.get(r + 1).map_or(offsets.len(), |next| next.0.min(offsets.len()));
        for &chunk_offset in offsets.get(first..end).unwrap_or(&[]) {
            let mut at = chunk_offset;
            for _ in 0..per {
                let i = samples.len();
                if i == n {
                    break;
                }
                let size = sizes[i];
                if size > MAX_SAMPLE || at.saturating_add(u64::from(size)) > file_len {
                    return bad("it's cut short");
                }
                samples.push(Sample { offset: at, size, time: times[i] + shift, sync: true });
                at += u64::from(size);
            }
        }
    }
    if samples.len() < n {
        return bad("it's cut short");
    }

    // Sync samples: all of them unless a table lists some
    if let Some(stss) = table(b"stss")? {
        let mut b = Bytes::new(stss);
        b.skip(4)?;
        let entries = b.count(4)?;
        samples.iter_mut().for_each(|s| s.sync = false);
        for _ in 0..entries {
            if let Some(sample) = (b.u32()? as usize).checked_sub(1).and_then(|i| samples.get_mut(i)) {
                sample.sync = true;
            }
        }
        if let Some(first) = samples.first_mut() {
            first.sync = true;
        }
    }

    // How far showing is behind decoding: for each sample, how many decoded after it
    // are shown before it, looking a little way ahead
    let mut reorder = 0;
    for (i, s) in samples.iter().enumerate() {
        let ahead = samples[i + 1..].iter().take(32).filter(|later| later.time < s.time).count();
        reorder = reorder.max(ahead);
    }
    Ok((samples, reorder))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn clip() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/clip.mp4")
    }

    #[test]
    fn reads_the_video_and_sound_tracks() {
        let movie = open(&clip()).unwrap();
        let video = movie.video.unwrap();
        let Codec::H264(avc) = &video.codec else { panic!("{:?}", video.codec) };
        assert_eq!((avc.length_size, avc.sps.len(), avc.pps.len()), (4, 1, 1));
        assert_eq!((video.width, video.height), (72, 40));
        assert_eq!(video.samples.len(), 12);
        // Two keyframes, six frames apart
        let sync: Vec<usize> = video.samples.iter().enumerate().filter(|(_, s)| s.sync).map(|(i, _)| i).collect();
        assert_eq!(sync, [0, 6]);
        // Shown 10 a second from 0, after the edit list takes off the B-frames' delay
        let mut shown: Vec<i64> = video.samples.iter().map(|s| s.time).collect();
        shown.sort_unstable();
        let tenth = i64::from(video.timescale) / 10;
        assert_eq!(shown, (0..12).map(|i| i * tenth).collect::<Vec<_>>());
        assert!(video.samples[1].time > video.samples[2].time, "B-frames come after the P they're shown before");
        assert!((1..=3).contains(&video.reorder), "{}", video.reorder);
        assert!((video.duration() - 1.2).abs() < 0.01, "{}", video.duration());

        let audio = movie.audio.unwrap();
        // AAC-LC (2), 44.1 kHz (4), stereo (2), and saying there's no SBR
        let Codec::Aac(config) = &audio.codec else { panic!("{:?}", audio.codec) };
        assert_eq!(config[..2], [0x12, 0x10]);
        assert_eq!(audio.timescale, 44100);
        assert!(audio.samples.len() > 50);
        assert!(audio.samples.iter().all(|s| s.sync));
        // The encoder's priming samples come before the start
        assert_eq!(audio.samples[0].time, -1024);
    }

    #[test]
    fn starts_from_the_keyframe_before() {
        let video = open(&clip()).unwrap().video.unwrap();
        assert_eq!(video.start_for(0.0), 0);
        assert_eq!(video.start_for(0.55), 0);
        assert_eq!(video.start_for(0.6), 6);
        assert_eq!(video.start_for(5.0), 6);
    }

    #[test]
    fn names_a_codec_it_doesnt_read() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/other-codec.mp4");
        let movie = open(&path).unwrap();
        assert_eq!(movie.video.unwrap().codec, Codec::Other("mp4v".into()));
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_panic() {
        let whole = std::fs::read(clip()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.mp4");
        // Cut short anywhere
        for len in (0..whole.len()).step_by(97) {
            std::fs::write(&path, &whole[..len]).unwrap();
            assert!(open(&path).is_err(), "cut at {len}");
        }
        // Bytes changed anywhere in the index
        let moov = whole.windows(4).position(|w| w == b"moov").unwrap();
        let mut seed = 7u32;
        for _ in 0..3000 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let mut broken = whole.clone();
            let at = moov + (seed as usize >> 8) % (whole.len() - moov);
            broken[at] = (seed >> 3) as u8;
            std::fs::write(&path, &broken).unwrap();
            let _ = open(&path);
        }
    }

    #[test]
    fn other_files_are_not_movies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.mp4");
        std::fs::write(&path, "not a video at all").unwrap();
        assert_eq!(open(&path).unwrap_err().0, "it isn't an MP4 file");
        std::fs::write(&path, b"\0\0\0\x18ftypmp42").unwrap();
        assert!(open(&path).is_err());
    }
}
