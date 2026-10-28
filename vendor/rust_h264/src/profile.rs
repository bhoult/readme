//! Coarse phase timers for the `profile` feature.
//!
//! Timers nest: `DecodeNal` wraps everything a `decode_nal` call does, so
//! the other phases should sum to roughly its total. The MB-level phases
//! (`MbDecode`, `MotionComp`) are inside `MbLoop`; the gap between `MbLoop`
//! and `MbDecode` is per-MB bookkeeping plus the timers' own overhead.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub enum Phase {
    /// Whole `Decoder::decode_nal` call (outermost; includes everything below).
    DecodeNal,
    /// `decode_slice` before the MB loop: slice header, reference list
    /// construction, `PictureState` allocation, entropy decoder init.
    SliceSetup,
    /// The whole MB loop of `decode_slice`.
    MbLoop,
    /// Per-MB entropy decode + reconstruction (inside `MbLoop`).
    MbDecode,
    /// Luma/chroma motion compensation (inside `MbDecode`). Timed per MC
    /// call, which can be as small as one 4x4 block, so the two clock reads
    /// per call inflate this phase noticeably; prefer the sampled
    /// flamegraph for its true share.
    MotionComp,
    /// `finalize_mb_info` after the MB loop.
    FinalizeMbInfo,
    /// Deblocking filter in `finalize_pending`.
    Deblock,
    /// MMCO + DPB insert in `finalize_pending`, including the plane copies.
    DpbFinalize,
    /// Crop to display size / field-pair interleave in `finalize_pending`.
    Output,
}

const PHASE_COUNT: usize = 9;

/// `(name, parent)`: parent names the phase this one is nested inside, so a
/// reporter can avoid double counting.
const PHASE_INFO: [(&str, Option<&str>); PHASE_COUNT] = [
    ("decode_nal_total", None),
    ("slice_setup", Some("decode_nal_total")),
    ("mb_loop", Some("decode_nal_total")),
    ("mb_decode", Some("mb_loop")),
    ("motion_comp", Some("mb_decode")),
    ("finalize_mb_info", Some("decode_nal_total")),
    ("deblock", Some("decode_nal_total")),
    ("dpb_finalize", Some("decode_nal_total")),
    ("output_crop", Some("decode_nal_total")),
];

#[derive(Clone, Copy)]
pub struct PhaseSample {
    pub name: &'static str,
    /// Name of the enclosing phase, if this one is nested.
    pub parent: Option<&'static str>,
    pub micros: u64,
}

// Accumulated in nanoseconds: per-MB intervals are well under a microsecond,
// so truncating each one to microseconds before summing would discard most
// of the MB-level time.
static TOTALS_NS: [AtomicU64; PHASE_COUNT] = [const { AtomicU64::new(0) }; PHASE_COUNT];
static ENABLED: AtomicBool = AtomicBool::new(false);

pub struct PhaseTimer {
    phase: Phase,
    start: Option<Instant>,
}

impl PhaseTimer {
    #[inline]
    pub fn start(phase: Phase) -> Self {
        Self {
            phase,
            start: ENABLED.load(Ordering::Relaxed).then(Instant::now),
        }
    }
}

impl Drop for PhaseTimer {
    #[inline]
    fn drop(&mut self) {
        if let Some(start) = self.start {
            add_duration(self.phase, start.elapsed());
        }
    }
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn reset() {
    for t in &TOTALS_NS {
        t.store(0, Ordering::Relaxed);
    }
}

pub fn snapshot() -> [PhaseSample; PHASE_COUNT] {
    let mut out = [PhaseSample {
        name: "",
        parent: None,
        micros: 0,
    }; PHASE_COUNT];
    for (i, (name, parent)) in PHASE_INFO.iter().enumerate() {
        out[i] = PhaseSample {
            name,
            parent: *parent,
            micros: TOTALS_NS[i].load(Ordering::Relaxed) / 1000,
        };
    }
    out
}

#[inline]
fn add_duration(phase: Phase, duration: Duration) {
    let nanos = duration.as_nanos().min(u128::from(u64::MAX)) as u64;
    TOTALS_NS[phase as usize].fetch_add(nanos, Ordering::Relaxed);
}
