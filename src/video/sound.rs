//! A video's sound: its AAC decoded, and played through ALSA on a thread of its own.
//! ALSA's library is loaded when sound is first played, so readme neither needs it to
//! build nor to run; without it, videos play silently. Where the sound has got to is
//! what times the pictures, as it can't be sped up or held back.

use std::{
    fs::File,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use symphonia_codec_aac::AacDecoder;
use symphonia_core::{
    codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions, well_known::CODEC_ID_AAC},
    packet::PacketRef,
    units::{Duration, Timestamp},
};

use super::mp4::{Codec, Movie, Track};

/// The name of the sound thread, which the panic hook knows to leave be
pub const WORKER: &str = "sound";

/// Decodes a track's AAC, a packet at a time.
pub struct Aac {
    decoder: AacDecoder,
    pub rate: u32,
    pub channels: usize,
}

impl Aac {
    /// None for sound that isn't AAC this decoder plays: AAC-LC, mono or stereo.
    pub fn new(track: &Track) -> Option<Self> {
        let Codec::Aac(config) = &track.codec else { return None };
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC).with_extra_data(config.clone().into_boxed_slice());
        let decoder = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).ok()?;
        let rate = decoder.codec_params().sample_rate?;
        let channels = decoder.codec_params().channels.as_ref()?.count();
        Some(Self { decoder, rate, channels })
    }

    /// One packet's sound, as interleaved 16-bit samples added to `out`. A packet that
    /// can't be decoded is left out.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) {
        let Ok(buf) = self.decoder.decode_ref(&PacketRef::new(0, Timestamp::new(0), Duration::new(1024), packet))
        else {
            return;
        };
        let mut samples: Vec<f32> = Vec::new();
        buf.copy_to_vec_interleaved(&mut samples);
        out.extend(samples.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
    }
}

/// Sound playing from some point in a video.
pub struct Sound {
    stop: Arc<AtomicBool>,
    /// Seconds into the video that's been heard, as f64 bits; NaN until it's started
    heard: Arc<AtomicU64>,
    /// The thread's finished, played out or failed
    done: Arc<AtomicBool>,
}

impl Sound {
    /// Starts playing the sound of the video at `path` from `from` seconds. None when
    /// there's no sound this plays, or no ALSA.
    pub fn start(path: &Path, movie: Arc<Movie>, from: f64) -> Option<Self> {
        let track = movie.audio.as_ref()?;
        Aac::new(track)?;
        if cfg!(test) || alsa::get().is_none() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let heard = Arc::new(AtomicU64::new(f64::NAN.to_bits()));
        let done = Arc::new(AtomicBool::new(false));
        let (path, stopped, playing, finished) = (path.to_path_buf(), stop.clone(), heard.clone(), done.clone());
        std::thread::Builder::new()
            .name(WORKER.into())
            .spawn(move || {
                if let Some(track) = &movie.audio {
                    play(&path, track, from, &stopped, &playing);
                }
                finished.store(true, Ordering::Relaxed);
            })
            .ok()?;
        Some(Self { stop, heard, done })
    }

    /// How far into the video has been heard, once sound has started coming out.
    pub fn heard(&self) -> Option<f64> {
        Some(f64::from_bits(self.heard.load(Ordering::Relaxed))).filter(|t| !t.is_nan())
    }

    /// No more is coming: it's played out, or the output failed.
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

impl Drop for Sound {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The packet to start decoding from to hear `from` seconds, one early, as each
/// packet's sound overlaps the one before.
fn start_for(track: &Track, from: f64) -> usize {
    let ticks = track.ticks(from);
    track.samples.iter().rposition(|s| s.time <= ticks).unwrap_or(0).saturating_sub(1)
}

/// Decodes the whole track from `from` seconds, for the tests: samples, rate and channels.
#[cfg(test)]
pub fn decode_from(path: &Path, track: &Track, from: f64) -> (Vec<i16>, u32, usize) {
    let mut aac = Aac::new(track).unwrap();
    let mut file = File::open(path).unwrap();
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let first = start_for(track, from);
    for i in first..track.samples.len() {
        track.read(&mut file, i, &mut buf).unwrap();
        aac.decode(&buf, &mut out);
    }
    let skip = skip_frames(track, first, from, aac.rate) * aac.channels;
    (out.split_off(skip.min(out.len())), aac.rate, aac.channels)
}

/// How many frames of sound decoded from packet `first` come before `from` seconds.
fn skip_frames(track: &Track, first: usize, from: f64, rate: u32) -> usize {
    let start = track.seconds(track.samples[first].time);
    ((from - start).max(0.0) * f64::from(rate)).round() as usize
}

fn play(path: &Path, track: &Track, from: f64, stop: &AtomicBool, heard: &AtomicU64) {
    let Some(alsa) = alsa::get() else { return };
    let Some(mut aac) = Aac::new(track) else { return };
    let Ok(mut file) = File::open(path) else { return };
    let Some(pcm) = alsa.open(aac.rate, aac.channels) else { return };
    let first = start_for(track, from);
    let mut skip = skip_frames(track, first, from, aac.rate) * aac.channels;
    let mut buf = Vec::new();
    let mut out = Vec::new();
    // Frames handed to ALSA, counted from `from`
    let mut written = 0usize;
    let channels = aac.channels;
    let rate = f64::from(aac.rate);
    let tell = |written: usize, pcm: &alsa::Pcm| {
        let queued = pcm.delay().unwrap_or(0);
        let played = written.saturating_sub(queued) as f64;
        heard.store((from + played / rate).to_bits(), Ordering::Relaxed);
    };
    for i in first..track.samples.len() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if track.read(&mut file, i, &mut buf).is_err() {
            break;
        }
        out.clear();
        aac.decode(&buf, &mut out);
        let dropped = skip.min(out.len());
        skip -= dropped;
        let mut rest = &out[dropped..];
        while !rest.is_empty() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let Some(frames) = pcm.write(rest, channels) else { return };
            written += frames;
            rest = &rest[frames * channels..];
            tell(written, &pcm);
        }
    }
    // Played out to the end, still telling where it's got to
    while !stop.load(Ordering::Relaxed) && pcm.delay().is_some_and(|queued| queued > 0) {
        tell(written, &pcm);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The few calls to ALSA's library that playing takes, looked up when first wanted.
mod alsa {
    use std::{
        ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void},
        sync::OnceLock,
    };

    type PcmPtr = *mut c_void;
    type LocalHandler = unsafe extern "C" fn(*const c_char, c_int, *const c_char, c_int, *const c_char, *mut c_void);
    // The functions' types, as ALSA's headers give them
    type Open = unsafe extern "C" fn(*mut PcmPtr, *const c_char, c_int, c_int) -> c_int;
    type SetParams = unsafe extern "C" fn(PcmPtr, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int;
    type Writei = unsafe extern "C" fn(PcmPtr, *const c_void, c_ulong) -> c_long;
    type Recover = unsafe extern "C" fn(PcmPtr, c_int, c_int) -> c_int;
    type Delay = unsafe extern "C" fn(PcmPtr, *mut c_long) -> c_int;
    type Done = unsafe extern "C" fn(PcmPtr) -> c_int;
    type SetLocalHandler = unsafe extern "C" fn(Option<LocalHandler>) -> Option<LocalHandler>;

    pub struct Alsa {
        open: Open,
        set_params: SetParams,
        writei: Writei,
        recover: Recover,
        delay: Delay,
        drop: Done,
        close: Done,
        /// Since ALSA 1.1.6; where it's missing, ALSA's own complaints aren't quietened
        set_local_handler: Option<SetLocalHandler>,
    }

    // The library's functions are fine to call from any thread
    unsafe impl Send for Alsa {}
    unsafe impl Sync for Alsa {}

    const STREAM_PLAYBACK: c_int = 0;
    const FORMAT_S16_LE: c_int = 2;
    const ACCESS_RW_INTERLEAVED: c_int = 3;
    /// How much sound ALSA holds ahead, in microseconds
    const LATENCY: c_uint = 100_000;

    /// ALSA, if this system has it.
    #[cfg(target_os = "linux")]
    pub fn get() -> Option<&'static Alsa> {
        static ALSA: OnceLock<Option<Alsa>> = OnceLock::new();
        ALSA.get_or_init(|| {
            // SAFETY: the names are C strings; each symbol is cast to the type ALSA's
            // headers give it, and the library stays loaded for good
            unsafe {
                let lib = libc::dlopen(c"libasound.so.2".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
                if lib.is_null() {
                    return None;
                }
                let sym = |name: &std::ffi::CStr| {
                    let f = libc::dlsym(lib, name.as_ptr());
                    (!f.is_null()).then_some(f)
                };
                use std::mem::transmute;
                Some(Alsa {
                    open: transmute::<*mut c_void, Open>(sym(c"snd_pcm_open")?),
                    set_params: transmute::<*mut c_void, SetParams>(sym(c"snd_pcm_set_params")?),
                    writei: transmute::<*mut c_void, Writei>(sym(c"snd_pcm_writei")?),
                    recover: transmute::<*mut c_void, Recover>(sym(c"snd_pcm_recover")?),
                    delay: transmute::<*mut c_void, Delay>(sym(c"snd_pcm_delay")?),
                    drop: transmute::<*mut c_void, Done>(sym(c"snd_pcm_drop")?),
                    close: transmute::<*mut c_void, Done>(sym(c"snd_pcm_close")?),
                    set_local_handler: sym(c"snd_lib_error_set_local")
                        .map(|f| transmute::<*mut c_void, SetLocalHandler>(f)),
                })
            }
        })
        .as_ref()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn get() -> Option<&'static Alsa> {
        None
    }

    /// Where ALSA's complaints go on the sound thread, rather than over the screen.
    unsafe extern "C" fn quiet(
        _: *const c_char,
        _: c_int,
        _: *const c_char,
        _: c_int,
        _: *const c_char,
        _: *mut c_void,
    ) {
    }

    /// An open output, stopped and closed when dropped.
    pub struct Pcm {
        alsa: &'static Alsa,
        pcm: PcmPtr,
    }

    impl Alsa {
        /// The default output (or failing that, PipeWire's or PulseAudio's), set up for
        /// 16-bit sound at `rate`.
        pub fn open(&'static self, rate: u32, channels: usize) -> Option<Pcm> {
            let mut pcm: PcmPtr = std::ptr::null_mut();
            // SAFETY: calls as ALSA documents them, with a pointer for it to fill in
            unsafe {
                if let Some(set) = self.set_local_handler {
                    set(Some(quiet));
                }
                // The default output, or else PipeWire's or PulseAudio's: a default set to
                // a sound card PipeWire already has is busy
                let opened = [c"default", c"pipewire", c"pulse"]
                    .iter()
                    .any(|name| (self.open)(&raw mut pcm, name.as_ptr(), STREAM_PLAYBACK, 0) >= 0 && !pcm.is_null());
                if !opened {
                    return None;
                }
                let pcm = Pcm { alsa: self, pcm };
                let set = (self.set_params)(
                    pcm.pcm,
                    FORMAT_S16_LE,
                    ACCESS_RW_INTERLEAVED,
                    channels as c_uint,
                    rate,
                    1,
                    LATENCY,
                );
                (set >= 0).then_some(pcm)
            }
        }
    }

    impl Pcm {
        /// Writes what of `samples` there's room for, waiting for some, and says how
        /// many frames that was. None if the output's gone.
        pub fn write(&self, samples: &[i16], channels: usize) -> Option<usize> {
            let frames = samples.len() / channels.max(1);
            // SAFETY: `samples` holds `frames` whole frames
            let wrote = unsafe { (self.alsa.writei)(self.pcm, samples.as_ptr().cast(), frames as c_ulong) };
            if wrote >= 0 {
                return Some(wrote as usize);
            }
            // Run dry or suspended: carry on if ALSA can
            // SAFETY: as documented, with the error it gave
            let recovered = unsafe { (self.alsa.recover)(self.pcm, wrote as c_int, 1) };
            (recovered >= 0).then_some(0)
        }

        /// Frames written but not yet heard.
        pub fn delay(&self) -> Option<usize> {
            let mut delay: c_long = 0;
            // SAFETY: as documented, with a place for the answer
            let ok = unsafe { (self.alsa.delay)(self.pcm, &raw mut delay) } >= 0;
            ok.then_some(delay.max(0) as usize)
        }
    }

    impl Drop for Pcm {
        fn drop(&mut self) {
            // SAFETY: the handle came from snd_pcm_open and is closed only here
            unsafe {
                (self.alsa.drop)(self.pcm);
                (self.alsa.close)(self.pcm);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::mp4;

    #[test]
    fn decodes_the_sound_from_where_its_asked_for() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/clip.mp4");
        let movie = mp4::open(&path).unwrap();
        let track = movie.audio.as_ref().unwrap();
        let (all, rate, channels) = decode_from(&path, track, 0.0);
        assert_eq!((rate, channels), (44100, 2));
        // 1.2 seconds of it, give or take the encoder's padding at the end
        let seconds = all.len() as f64 / channels as f64 / f64::from(rate);
        assert!((1.15..1.3).contains(&seconds), "{seconds}");
        // A 440 Hz tone crosses zero 880 times a second
        let left: Vec<i16> = all.iter().step_by(2).copied().collect();
        let crossings = left.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count() as f64 / seconds;
        assert!((850.0..910.0).contains(&crossings), "{crossings}");
        // From half way, half as much
        let (half, _, _) = decode_from(&path, track, 0.6);
        let ratio = half.len() as f64 / all.len() as f64;
        assert!((0.45..0.55).contains(&ratio), "{ratio}");
    }
}
