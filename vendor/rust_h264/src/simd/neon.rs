//! Hand-written NEON (aarch64) motion-compensation kernels.
//!
//! Covers the luma 6-tap half-pel filters, which dominate MC time. Kernels
//! not implemented here re-export the scalar reference. All kernels process
//! 8 pixels per iteration with a scalar tail, and are bit-exact with
//! `scalar.rs` (verified by the golden corpus digest test in `mod.rs`).

use core::arch::aarch64::*;

use crate::inter_pred::{avg, clip_u8, fir6};

pub use super::scalar::{
    bi_pred_avg, chroma_bilinear_block, row_avg_h_hv, row_avg_h_int, row_avg_int_h, row_avg_v_hv,
    row_half_pel_hv, weighted_bi, weighted_bi_implicit, weighted_uni,
};

/// 6-tap FIR of six u8x8 vectors, rounded and clipped to u8x8:
/// `clip((s0 - 5*s1 + 20*s2 + 20*s3 - 5*s4 + s5 + 16) >> 5)`.
#[inline(always)]
unsafe fn fir6_round_clip(
    s0: uint8x8_t,
    s1: uint8x8_t,
    s2: uint8x8_t,
    s3: uint8x8_t,
    s4: uint8x8_t,
    s5: uint8x8_t,
) -> uint8x8_t {
    let sum_pos1 = vaddl_u8(s0, s5);
    let sum_20 = vaddl_u8(s2, s3);
    let sum_neg5 = vaddl_u8(s1, s4);
    let mut acc = vreinterpretq_s16_u16(sum_pos1);
    acc = vmlaq_n_s16(acc, vreinterpretq_s16_u16(sum_20), 20);
    acc = vmlsq_n_s16(acc, vreinterpretq_s16_u16(sum_neg5), 5);
    acc = vaddq_s16(acc, vdupq_n_s16(16));
    vqmovun_s16(vshrq_n_s16(acc, 5))
}

/// Horizontal half-pel for 8 pixels starting at `src[i]` (`src` must have
/// `i + 13` accessible bytes).
#[inline(always)]
unsafe fn hfir6_8(src: &[u8], i: usize) -> uint8x8_t {
    let p = src.as_ptr().add(i);
    fir6_round_clip(
        vld1_u8(p),
        vld1_u8(p.add(1)),
        vld1_u8(p.add(2)),
        vld1_u8(p.add(3)),
        vld1_u8(p.add(4)),
        vld1_u8(p.add(5)),
    )
}

/// Vertical half-pel for 8 pixels at column `i` across six rows.
#[inline(always)]
unsafe fn vfir6_8(rows: &[&[u8]; 6], i: usize) -> uint8x8_t {
    fir6_round_clip(
        vld1_u8(rows[0].as_ptr().add(i)),
        vld1_u8(rows[1].as_ptr().add(i)),
        vld1_u8(rows[2].as_ptr().add(i)),
        vld1_u8(rows[3].as_ptr().add(i)),
        vld1_u8(rows[4].as_ptr().add(i)),
        vld1_u8(rows[5].as_ptr().add(i)),
    )
}

#[inline(always)]
fn scalar_vfir6(rows: &[&[u8]; 6], i: usize) -> u8 {
    let val =
        rows[0][i] as i32 - 5 * rows[1][i] as i32 + 20 * rows[2][i] as i32 + 20 * rows[3][i] as i32
            - 5 * rows[4][i] as i32
            + rows[5][i] as i32;
    clip_u8((val + 16) >> 5)
}

/// Row-based horizontal half-pel filter for in-bounds blocks.
/// Reads `w` output pixels from row at `src` (which must have `w + 5` accessible bytes).
#[inline(never)]
pub fn row_half_pel_h(src: &[u8], out: &mut [u8], w: usize) {
    debug_assert!(src.len() >= w + 5 && out.len() >= w);
    let mut i = 0;
    while i + 8 <= w {
        // SAFETY: src has w + 5 bytes, so src[i..i + 13] is in bounds for
        // i + 8 <= w; out has at least w bytes.
        unsafe {
            vst1_u8(out.as_mut_ptr().add(i), hfir6_8(src, i));
        }
        i += 8;
    }
    while i < w {
        out[i] = clip_u8((fir6(src, i) + 16) >> 5);
        i += 1;
    }
}

/// Row-based vertical half-pel filter for in-bounds blocks.
/// `rows` contains 6 row slices (y-2..y+3), each at least `w` bytes.
#[inline(never)]
pub fn row_half_pel_v(rows: [&[u8]; 6], out: &mut [u8], w: usize) {
    debug_assert!(rows.iter().all(|r| r.len() >= w) && out.len() >= w);
    let mut i = 0;
    while i + 8 <= w {
        // SAFETY: every row has at least w bytes, so rows[k][i..i + 8] is in
        // bounds for i + 8 <= w; out has at least w bytes.
        unsafe {
            vst1_u8(out.as_mut_ptr().add(i), vfir6_8(&rows, i));
        }
        i += 8;
    }
    while i < w {
        out[i] = scalar_vfir6(&rows, i);
        i += 1;
    }
}

/// Average an integer row with a vertical half-pel row.
#[inline(never)]
pub fn row_avg_int_v(int_row: &[u8], rows: [&[u8]; 6], out: &mut [u8], w: usize) {
    debug_assert!(int_row.len() >= w && rows.iter().all(|r| r.len() >= w) && out.len() >= w);
    let mut i = 0;
    while i + 8 <= w {
        // SAFETY: as in row_half_pel_v, plus int_row has at least w bytes.
        unsafe {
            let int_val = vld1_u8(int_row.as_ptr().add(i));
            let hp = vfir6_8(&rows, i);
            vst1_u8(out.as_mut_ptr().add(i), vrhadd_u8(int_val, hp));
        }
        i += 8;
    }
    while i < w {
        out[i] = avg(int_row[i], scalar_vfir6(&rows, i));
        i += 1;
    }
}

/// Average horizontal and vertical half-pel rows.
#[inline(never)]
pub fn row_avg_h_v(src_h: &[u8], rows_v: [&[u8]; 6], out: &mut [u8], w: usize) {
    debug_assert!(src_h.len() >= w + 5 && rows_v.iter().all(|r| r.len() >= w) && out.len() >= w);
    let mut i = 0;
    while i + 8 <= w {
        // SAFETY: src_h has w + 5 bytes (see row_half_pel_h) and every row
        // in rows_v has at least w bytes (see row_half_pel_v).
        unsafe {
            let h_val = hfir6_8(src_h, i);
            let v_val = vfir6_8(&rows_v, i);
            vst1_u8(out.as_mut_ptr().add(i), vrhadd_u8(h_val, v_val));
        }
        i += 8;
    }
    while i < w {
        let h_val = clip_u8((fir6(src_h, i) + 16) >> 5);
        out[i] = avg(h_val, scalar_vfir6(&rows_v, i));
        i += 1;
    }
}
