//! Decoded Picture Buffer (H.264 spec 8.2.5).
//!
//! Stores decoded reference frames for use by P/B slice motion compensation.
//! Manages short-term reference marking via sliding window (spec 8.2.5.3).

use std::rc::Rc;

use crate::nal::NalUnitType;
use crate::slice::SliceHeader;
use crate::sps::Sps;

/// Reference status of a picture in the DPB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceStatus {
    /// Not used for reference.
    Unused,
    /// Short-term reference (identified by frame_num).
    ShortTerm,
    /// Long-term reference (identified by long_term_frame_idx).
    LongTerm(u32),
}

/// Immutable decoded picture data shared via Rc.
#[derive(Debug)]
pub struct DecodedPicture {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub frame_num: u32,
    pub pic_order_cnt: i32,
    /// Per-4x4-block L0 motion vectors (for temporal direct mode co-located access).
    pub mv_l0: Vec<[i16; 2]>,
    /// Per-4x4-block L0 reference indices.
    pub ref_idx_l0: Vec<i8>,
    /// Per-4x4-block POC of the L0 reference picture used (for temporal direct mode mapping).
    pub ref_poc_l0: Vec<i32>,
    /// Per-4x4-block L1 motion vectors (for spatial direct co-located L1 check).
    pub mv_l1: Vec<[i16; 2]>,
    /// Per-4x4-block L1 reference indices (for spatial direct co-located L1 check).
    pub ref_idx_l1: Vec<i8>,
    /// Width in macroblocks (for indexing into mv_l0/ref_idx_l0).
    pub mb_width: u32,
    /// Whether this picture is intra-only (all MBs are intra).
    pub is_intra: bool,
}

/// A DPB entry wrapping an Rc<DecodedPicture> with mutable status.
struct DpbEntry {
    pic: Rc<DecodedPicture>,
    reference: ReferenceStatus,
}

/// The Decoded Picture Buffer.
pub struct Dpb {
    max_ref_frames: usize,
    entries: Vec<DpbEntry>,
    // POC type 0 state
    prev_poc_msb: i32,
    prev_poc_lsb: u32,
}

impl Dpb {
    pub fn new(max_ref_frames: usize) -> Self {
        Self {
            max_ref_frames,
            entries: Vec::new(),
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
        }
    }

    /// Update capacity when a new SPS is parsed.
    pub fn set_max_ref_frames(&mut self, max_ref_frames: u32) {
        self.max_ref_frames = max_ref_frames as usize;
    }

    /// Clear the entire DPB. Called on IDR.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.prev_poc_msb = 0;
        self.prev_poc_lsb = 0;
    }

    /// Insert a decoded picture into the DPB.
    /// Applies sliding window marking if needed (spec 8.2.5.3).
    pub fn insert(&mut self, pic: Rc<DecodedPicture>, reference: ReferenceStatus) {
        // Sliding window only applies to short-term references (spec 8.2.5.3)
        if reference == ReferenceStatus::ShortTerm {
            self.sliding_window_mark();
        }
        self.entries.push(DpbEntry { pic, reference });
        self.remove_unused();
    }

    /// Get the list of short-term reference pictures, sorted by descending PicNum.
    /// Used to build ref_pic_list_0 for P slices (spec 8.2.4.2.1).
    /// PicNum = FrameNumWrap: frame_num, less `max_frame_num` for pictures whose
    /// frame_num is above the current picture's (spec 8.2.4.1). This is decoding
    /// order, which isn't POC order once B-frames are used as references.
    pub fn short_term_ref_list(&self, frame_num: u32, max_frame_num: u32) -> Vec<Rc<DecodedPicture>> {
        let mut refs: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.reference == ReferenceStatus::ShortTerm)
            .map(|e| e.pic.clone())
            .collect();
        let frame_num_wrap = |p: &DecodedPicture| {
            if p.frame_num > frame_num {
                i64::from(p.frame_num) - i64::from(max_frame_num)
            } else {
                i64::from(p.frame_num)
            }
        };
        refs.sort_by_key(|p| std::cmp::Reverse(frame_num_wrap(p)));
        // Append long-term refs sorted by ascending long_term_frame_idx (spec 8.2.4.2.1)
        refs.extend(self.long_term_ref_list());
        refs
    }

    /// Every short-term reference picture, in no particular order.
    pub fn short_term_refs(&self) -> Vec<Rc<DecodedPicture>> {
        self.entries
            .iter()
            .filter(|e| e.reference == ReferenceStatus::ShortTerm)
            .map(|e| e.pic.clone())
            .collect()
    }

    /// Build ref_pic_list_0 for B slices (spec 8.2.4.2.3).
    /// Short-term refs with POC < current sorted by descending POC,
    /// then refs with POC > current sorted by ascending POC.
    pub fn ref_list_l0_b(&self, current_poc: i32) -> Vec<Rc<DecodedPicture>> {
        let short_term: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.reference == ReferenceStatus::ShortTerm)
            .map(|e| e.pic.clone())
            .collect();

        let mut before: Vec<_> = short_term
            .iter()
            .filter(|p| p.pic_order_cnt <= current_poc)
            .cloned()
            .collect();
        before.sort_by(|a, b| b.pic_order_cnt.cmp(&a.pic_order_cnt)); // descending

        let mut after: Vec<_> = short_term
            .iter()
            .filter(|p| p.pic_order_cnt > current_poc)
            .cloned()
            .collect();
        after.sort_by(|a, b| a.pic_order_cnt.cmp(&b.pic_order_cnt)); // ascending

        before.extend(after);
        // Append long-term refs (spec 8.2.4.2.3)
        before.extend(self.long_term_ref_list());
        before
    }

    /// Build ref_pic_list_1 for B slices (spec 8.2.4.2.4).
    /// Short-term refs with POC > current sorted by ascending POC,
    /// then refs with POC <= current sorted by descending POC.
    /// If L1 == L0 and has more than one entry, swap the first two.
    pub fn ref_list_l1_b(&self, current_poc: i32) -> Vec<Rc<DecodedPicture>> {
        let short_term: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.reference == ReferenceStatus::ShortTerm)
            .map(|e| e.pic.clone())
            .collect();

        let mut after: Vec<_> = short_term
            .iter()
            .filter(|p| p.pic_order_cnt > current_poc)
            .cloned()
            .collect();
        after.sort_by(|a, b| a.pic_order_cnt.cmp(&b.pic_order_cnt)); // ascending

        let mut before: Vec<_> = short_term
            .iter()
            .filter(|p| p.pic_order_cnt <= current_poc)
            .cloned()
            .collect();
        before.sort_by(|a, b| b.pic_order_cnt.cmp(&a.pic_order_cnt)); // descending

        after.extend(before);
        // Append long-term refs (spec 8.2.4.2.4)
        after.extend(self.long_term_ref_list());

        // Spec 8.2.4.2.4: if L1 == L0 and has more than one entry, swap first two
        let l0 = self.ref_list_l0_b(current_poc);
        if after.len() > 1
            && after.len() == l0.len()
            && after
                .iter()
                .zip(l0.iter())
                .all(|(a, b)| a.pic_order_cnt == b.pic_order_cnt)
        {
            after.swap(0, 1);
        }

        after
    }

    /// Apply ref_pic_list_modification to reorder a reference list (spec 8.2.4.3).
    /// `ops`: list of (modification_of_pic_nums_idc, abs_diff_pic_num_minus1)
    /// `frame_num`: current picture's frame_num
    /// `max_pic_num`: 1 << (log2_max_frame_num_minus4 + 4)
    /// `short_term`: every short-term reference picture. A command can name one
    /// the initial list had no room for, or that an earlier command pushed out.
    pub fn apply_ref_list_modification(
        ref_list: &mut Vec<Rc<DecodedPicture>>,
        ops: &[(u32, u32)],
        frame_num: u32,
        max_pic_num: u32,
        short_term: &[Rc<DecodedPicture>],
    ) {
        if ops.is_empty() || ref_list.is_empty() {
            return;
        }
        let num_active = ref_list.len();
        let mut pred_pic_num = frame_num;
        let mut ref_idx_lx = 0usize;

        for &(idc, val) in ops {
            // Per spec 8.2.4.3.1, ref_idx_lx must stay in [0, num_active - 1].
            // A malformed bitstream may encode more RPLM commands than there
            // are slots — bail out instead of indexing out of bounds.
            if ref_idx_lx >= num_active {
                return;
            }
            if idc == 2 {
                // Long-term ref reordering (spec 8.2.4.3.2)
                let long_term_pic_num = val; // long_term_pic_num directly
                if let Some(found_pos) = ref_list.iter().position(|p| {
                    // Match by frame_num used as long_term_frame_idx proxy
                    // In practice, the DPB entry's long_term_frame_idx is stored
                    // but DecodedPicture doesn't carry it. Match by position in list.
                    // Actually, long_term_pic_num == long_term_frame_idx for frames.
                    // We find LT refs appended at the end of the list.
                    p.frame_num == long_term_pic_num
                }) {
                    let pic = ref_list[found_pos].clone();
                    ref_list.push(ref_list.last().unwrap().clone());
                    let end = ref_list.len() - 1;
                    for c in (ref_idx_lx + 1..=end).rev() {
                        ref_list[c] = ref_list[c - 1].clone();
                    }
                    ref_list[ref_idx_lx] = pic.clone();
                    // Remove duplicate: entries after ref_idx_lx with same pic
                    let mut n = ref_idx_lx + 1;
                    for c in (ref_idx_lx + 1)..ref_list.len() {
                        if !Rc::ptr_eq(&ref_list[c], &pic) {
                            ref_list[n] = ref_list[c].clone();
                            n += 1;
                        }
                    }
                    ref_list.truncate(num_active);
                }
                ref_idx_lx += 1;
                continue;
            }
            if idc > 1 {
                continue; // idc=3 terminates the loop (handled by caller)
            }
            let abs_diff = val + 1;
            // Spec 8.2.4.3.1: modular arithmetic on pic_num. Use wrapping
            // ops to handle malformed streams where abs_diff exceeds the
            // valid range without panicking.
            let pic_num = if idc == 0 {
                if pred_pic_num >= abs_diff {
                    pred_pic_num - abs_diff
                } else {
                    pred_pic_num
                        .wrapping_add(max_pic_num)
                        .wrapping_sub(abs_diff)
                }
            } else {
                let sum = pred_pic_num.wrapping_add(abs_diff);
                if sum >= max_pic_num {
                    sum.wrapping_sub(max_pic_num)
                } else {
                    sum
                }
            };
            pred_pic_num = pic_num;

            // Spec 8.2.4.3.1: find the picture among all the short-term
            // references, shift entries right to make room, insert at
            // ref_idx_lx, then remove duplicates after ref_idx_lx.
            if let Some(pic) = short_term.iter().find(|p| p.frame_num == pic_num).cloned() {

                // Shift right: make room at ref_idx_lx
                // Temporarily grow the list by 1
                ref_list.push(ref_list.last().unwrap().clone());
                let end = ref_list.len() - 1;
                for c in (ref_idx_lx + 1..=end).rev() {
                    ref_list[c] = ref_list[c - 1].clone();
                }
                ref_list[ref_idx_lx] = pic.clone();

                // Remove duplicate: compact entries after ref_idx_lx that
                // have the same pic_num as the inserted picture
                let mut n = ref_idx_lx + 1;
                for c in (ref_idx_lx + 1)..ref_list.len() {
                    if ref_list[c].frame_num != pic_num {
                        ref_list[n] = ref_list[c].clone();
                        n += 1;
                    }
                }
                ref_list.truncate(num_active);
            }

            ref_idx_lx += 1;
        }
    }

    /// Compute Picture Order Count for the current picture (spec 8.2.1).
    pub fn compute_poc(
        &mut self,
        sps: &Sps,
        header: &SliceHeader,
        nal_unit_type: NalUnitType,
        nal_ref_idc: u8,
    ) -> i32 {
        match sps.pic_order_cnt_type {
            0 => self.compute_poc_type0(sps, header, nal_unit_type, nal_ref_idc),
            1 => self.compute_poc_type1(sps, header, nal_unit_type, nal_ref_idc),
            2 => Self::compute_poc_type2(header.frame_num, nal_unit_type, nal_ref_idc),
            _ => 0,
        }
    }

    /// POC type 0 (spec 8.2.1.1) — uses pic_order_cnt_lsb with MSB wrapping.
    fn compute_poc_type0(
        &mut self,
        sps: &Sps,
        header: &SliceHeader,
        nal_unit_type: NalUnitType,
        nal_ref_idc: u8,
    ) -> i32 {
        if nal_unit_type == NalUnitType::SliceIdr {
            self.prev_poc_msb = 0;
            self.prev_poc_lsb = 0;
            return 0;
        }

        let shift = (sps.log2_max_pic_order_cnt_lsb_minus4 + 4).min(31);
        let max_poc_lsb = 1u32 << shift;
        let poc_lsb = header.pic_order_cnt_lsb;

        let poc_msb = if poc_lsb < self.prev_poc_lsb
            && (self.prev_poc_lsb - poc_lsb) >= max_poc_lsb / 2
        {
            self.prev_poc_msb.wrapping_add(max_poc_lsb as i32)
        } else if poc_lsb > self.prev_poc_lsb && (poc_lsb - self.prev_poc_lsb) > max_poc_lsb / 2 {
            self.prev_poc_msb.wrapping_sub(max_poc_lsb as i32)
        } else {
            self.prev_poc_msb
        };

        let top_field_order_cnt = poc_msb.wrapping_add(poc_lsb as i32);
        // For frames: PicOrderCnt = min(TopFieldOrderCnt, BottomFieldOrderCnt)
        // BottomFieldOrderCnt = TopFieldOrderCnt + delta_pic_order_cnt_bottom
        let bottom_field_order_cnt =
            top_field_order_cnt.wrapping_add(header.delta_pic_order_cnt_bottom);
        let poc = top_field_order_cnt.min(bottom_field_order_cnt);

        if nal_ref_idc > 0 {
            self.prev_poc_msb = poc_msb;
            self.prev_poc_lsb = poc_lsb;
        }

        poc
    }

    /// POC type 1 (spec 8.2.1.2) — uses delta_pic_order_cnt with cycle offsets.
    fn compute_poc_type1(
        &self,
        sps: &Sps,
        header: &SliceHeader,
        nal_unit_type: NalUnitType,
        nal_ref_idc: u8,
    ) -> i32 {
        if nal_unit_type == NalUnitType::SliceIdr {
            return 0;
        }

        let num_ref_frames_in_cycle = sps.num_ref_frames_in_pic_order_cnt_cycle as usize;
        let expected_delta_per_cycle: i32 = sps.offset_for_ref_frame.iter().sum();

        let abs_frame_num = if num_ref_frames_in_cycle > 0 {
            header.frame_num as i32
        } else {
            0
        };

        let expected_poc = if nal_ref_idc == 0 && abs_frame_num > 0 {
            let cycle = (abs_frame_num - 1) / num_ref_frames_in_cycle.max(1) as i32;
            let idx = ((abs_frame_num - 1) % num_ref_frames_in_cycle.max(1) as i32) as usize;
            let partial: i32 = sps.offset_for_ref_frame[..=idx]
                .iter()
                .copied()
                .fold(0i32, |a, b| a.wrapping_add(b));
            cycle
                .wrapping_mul(expected_delta_per_cycle)
                .wrapping_add(partial)
                .wrapping_add(sps.offset_for_non_ref_pic)
        } else if abs_frame_num > 0 {
            let cycle = (abs_frame_num - 1) / num_ref_frames_in_cycle.max(1) as i32;
            let idx = ((abs_frame_num - 1) % num_ref_frames_in_cycle.max(1) as i32) as usize;
            let partial: i32 = sps.offset_for_ref_frame[..=idx]
                .iter()
                .copied()
                .fold(0i32, |a, b| a.wrapping_add(b));
            cycle
                .wrapping_mul(expected_delta_per_cycle)
                .wrapping_add(partial)
        } else {
            0
        };

        let top_field_order_cnt = expected_poc + header.delta_pic_order_cnt[0];
        // For frames: PicOrderCnt = min(TopFieldOrderCnt, BottomFieldOrderCnt)
        // BottomFieldOrderCnt = TopFieldOrderCnt + offset_for_top_to_bottom_field + delta[1]
        let bottom_field_order_cnt = top_field_order_cnt
            .wrapping_add(sps.offset_for_top_to_bottom_field)
            .wrapping_add(header.delta_pic_order_cnt[1]);
        top_field_order_cnt.min(bottom_field_order_cnt)
    }

    /// POC type 2 (spec 8.2.1.3) — derived directly from frame_num.
    fn compute_poc_type2(frame_num: u32, nal_unit_type: NalUnitType, nal_ref_idc: u8) -> i32 {
        if nal_unit_type == NalUnitType::SliceIdr {
            0
        } else if nal_ref_idc > 0 {
            2 * frame_num as i32
        } else {
            2 * frame_num as i32 - 1
        }
    }

    /// Sliding window reference marking (spec 8.2.5.3).
    /// While short-term ref count >= max_ref_frames, mark the oldest as unused.
    fn sliding_window_mark(&mut self) {
        let max = self.max_ref_frames.max(1);
        while self.max_ref_frames > 0 {
            // Total ref count includes both short-term and long-term (spec 8.2.5.3)
            let total_ref_count = self
                .entries
                .iter()
                .filter(|e| e.reference != ReferenceStatus::Unused)
                .count();
            if total_ref_count < max {
                break;
            }
            // Evict the oldest short-term reference (first in insertion order).
            // Using insertion order handles frame_num wraparound correctly
            // (spec 8.2.5.3: evict smallest FrameNumWrap, which is the oldest).
            if let Some(idx) = self
                .entries
                .iter()
                .position(|e| e.reference == ReferenceStatus::ShortTerm)
            {
                self.entries[idx].reference = ReferenceStatus::Unused;
            } else {
                break;
            }
        }
    }

    /// The frame_num of the frame a PicNum names (spec 8.2.4.1). PicNum counts back
    /// from the current frame_num, so it's negative for frames from before frame_num
    /// last wrapped, whose frame_num is it plus MaxFrameNum.
    pub fn frame_num_for_pic_num(pic_num: i32, max_frame_num: u32) -> u32 {
        pic_num.rem_euclid(max_frame_num.max(1) as i32) as u32
    }

    /// Mark a short-term reference as unused by frame_num (MMCO op=1).
    pub fn mark_short_term_unused(&mut self, frame_num: u32) {
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::ShortTerm && entry.pic.frame_num == frame_num {
                entry.reference = ReferenceStatus::Unused;
                break;
            }
        }
        self.remove_unused();
    }

    /// Mark a long-term reference as unused by long_term_pic_num (MMCO op=2).
    pub fn mark_long_term_unused(&mut self, long_term_pic_num: u32) {
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::LongTerm(long_term_pic_num) {
                entry.reference = ReferenceStatus::Unused;
                break;
            }
        }
        self.remove_unused();
    }

    /// Assign a short-term reference to long-term with given index (MMCO op=3).
    /// First marks any existing long-term with the same index as unused.
    pub fn assign_long_term(&mut self, frame_num: u32, long_term_frame_idx: u32) {
        // Remove any existing long-term with this index
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::LongTerm(long_term_frame_idx) {
                entry.reference = ReferenceStatus::Unused;
            }
        }
        // Convert the short-term ref to long-term
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::ShortTerm && entry.pic.frame_num == frame_num {
                entry.reference = ReferenceStatus::LongTerm(long_term_frame_idx);
                break;
            }
        }
        self.remove_unused();
    }

    /// Set max long-term frame index (MMCO op=4).
    /// All long-term refs with index > max are marked unused.
    /// max_long_term_frame_idx_plus1 = 0 means no long-term refs allowed.
    pub fn set_max_long_term_frame_idx(&mut self, max_long_term_frame_idx_plus1: u32) {
        if max_long_term_frame_idx_plus1 == 0 {
            // Mark ALL long-term refs as unused
            for entry in &mut self.entries {
                if matches!(entry.reference, ReferenceStatus::LongTerm(_)) {
                    entry.reference = ReferenceStatus::Unused;
                }
            }
        } else {
            let max_idx = max_long_term_frame_idx_plus1 - 1;
            for entry in &mut self.entries {
                if let ReferenceStatus::LongTerm(idx) = entry.reference {
                    if idx > max_idx {
                        entry.reference = ReferenceStatus::Unused;
                    }
                }
            }
        }
        self.remove_unused();
    }

    /// Clear all reference pictures (MMCO op=5).
    /// Marks all refs as unused and resets POC state.
    pub fn clear_all_refs(&mut self) {
        for entry in &mut self.entries {
            entry.reference = ReferenceStatus::Unused;
        }
        self.remove_unused();
        self.prev_poc_msb = 0;
        self.prev_poc_lsb = 0;
    }

    /// Insert current picture as long-term reference with given index (MMCO op=6).
    /// Called after the picture is decoded and inserted into DPB.
    /// The picture must already be in the DPB as ShortTerm.
    pub fn mark_current_as_long_term(&mut self, long_term_frame_idx: u32, frame_num: u32) {
        // Remove any existing long-term with this index
        for entry in &mut self.entries {
            if entry.reference == ReferenceStatus::LongTerm(long_term_frame_idx) {
                entry.reference = ReferenceStatus::Unused;
            }
        }
        // Find the just-inserted picture and change to long-term
        for entry in self.entries.iter_mut().rev() {
            if entry.reference == ReferenceStatus::ShortTerm && entry.pic.frame_num == frame_num {
                entry.reference = ReferenceStatus::LongTerm(long_term_frame_idx);
                break;
            }
        }
        self.remove_unused();
    }

    /// Get list of long-term reference pictures, sorted by ascending long_term_frame_idx.
    /// Used to append to reference lists after short-term refs (spec 8.2.4.2.1, 8.2.4.2.3).
    pub fn long_term_ref_list(&self) -> Vec<Rc<DecodedPicture>> {
        let mut refs: Vec<_> = self
            .entries
            .iter()
            .filter(|e| matches!(e.reference, ReferenceStatus::LongTerm(_)))
            .collect::<Vec<_>>();
        refs.sort_by_key(|e| match e.reference {
            ReferenceStatus::LongTerm(idx) => idx,
            _ => u32::MAX,
        });
        refs.iter().map(|e| e.pic.clone()).collect()
    }

    /// Remove entries that are unused for reference (freeing memory).
    fn remove_unused(&mut self) {
        self.entries
            .retain(|e| e.reference != ReferenceStatus::Unused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pic(frame_num: u32, poc: i32) -> Rc<DecodedPicture> {
        Rc::new(DecodedPicture {
            y: vec![],
            u: vec![],
            v: vec![],
            width: 16,
            height: 16,
            frame_num,
            pic_order_cnt: poc,
            mv_l0: vec![],
            ref_idx_l0: vec![],
            ref_poc_l0: vec![],
            mv_l1: vec![],
            ref_idx_l1: vec![],
            mb_width: 1,
            is_intra: false,
        })
    }

    #[test]
    fn test_dpb_insert_and_clear() {
        let mut dpb = Dpb::new(4);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        assert_eq!(dpb.short_term_ref_list(15, 16).len(), 2);
        dpb.clear();
        assert_eq!(dpb.short_term_ref_list(15, 16).len(), 0);
    }

    #[test]
    fn test_sliding_window() {
        let mut dpb = Dpb::new(2);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        assert_eq!(dpb.short_term_ref_list(15, 16).len(), 2);

        // Third insert triggers sliding window — oldest (frame_num=0) gets evicted
        dpb.insert(make_pic(2, 4), ReferenceStatus::ShortTerm);
        let refs = dpb.short_term_ref_list(15, 16);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].frame_num, 2); // newest first (descending)
        assert_eq!(refs[1].frame_num, 1);
    }

    #[test]
    fn test_poc_type2() {
        assert_eq!(Dpb::compute_poc_type2(0, NalUnitType::SliceIdr, 3), 0);
        assert_eq!(Dpb::compute_poc_type2(1, NalUnitType::Slice, 3), 2);
        assert_eq!(Dpb::compute_poc_type2(1, NalUnitType::Slice, 0), 1);
        assert_eq!(Dpb::compute_poc_type2(5, NalUnitType::Slice, 1), 10);
    }

    #[test]
    fn test_ref_list_sorted_descending() {
        let mut dpb = Dpb::new(4);
        dpb.insert(make_pic(3, 6), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(5, 10), ReferenceStatus::ShortTerm);

        let refs = dpb.short_term_ref_list(15, 16);
        assert_eq!(refs[0].frame_num, 5);
        assert_eq!(refs[1].frame_num, 3);
        assert_eq!(refs[2].frame_num, 1);
    }

    #[test]
    fn test_ref_list_in_decoding_order_not_display_order() {
        // P4 then a reference B2 shown before it: the B is the newer reference
        let mut dpb = Dpb::new(4);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 8), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(2, 4), ReferenceStatus::ShortTerm);
        let refs = dpb.short_term_ref_list(3, 16);
        let nums: Vec<u32> = refs.iter().map(|p| p.frame_num).collect();
        assert_eq!(nums, [2, 1, 0]);
    }

    #[test]
    fn test_ref_list_after_frame_num_wraps() {
        let mut dpb = Dpb::new(4);
        dpb.insert(make_pic(14, 28), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(15, 30), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(0, 32), ReferenceStatus::ShortTerm);
        let refs = dpb.short_term_ref_list(1, 16);
        let nums: Vec<u32> = refs.iter().map(|p| p.frame_num).collect();
        assert_eq!(nums, [0, 15, 14]);
    }

    #[test]
    fn test_mmco_after_frame_num_wraps_names_a_frame_from_before() {
        // x264's B-pyramid: the reference B-frame with frame_num 1 marks the one
        // two before it unused, PicNum -2, which is frame_num 14 (MaxFrameNum 16)
        let mut dpb = Dpb::new(5);
        for (frame_num, poc) in [(13, 48), (14, 44), (15, 50), (0, 58)] {
            dpb.insert(make_pic(frame_num, poc), ReferenceStatus::ShortTerm);
        }
        assert_eq!(Dpb::frame_num_for_pic_num(1 - 3, 16), 14);
        dpb.mark_short_term_unused(Dpb::frame_num_for_pic_num(1 - 3, 16));
        let mut left: Vec<u32> = dpb.short_term_refs().iter().map(|p| p.frame_num).collect();
        left.sort_unstable();
        assert_eq!(left, [0, 13, 15]);
        assert_eq!(Dpb::frame_num_for_pic_num(3, 16), 3);
        assert_eq!(Dpb::frame_num_for_pic_num(-16, 16), 0);
    }

    #[test]
    fn test_unused_not_in_ref_list() {
        let mut dpb = Dpb::new(4);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::Unused); // non-reference
        assert_eq!(dpb.short_term_ref_list(15, 16).len(), 1);
    }

    #[test]
    fn test_ref_list_l0_b() {
        let mut dpb = Dpb::new(5);
        // POCs: 0, 2, 4, 6, 8 — current POC is 5
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(2, 4), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(3, 6), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(4, 8), ReferenceStatus::ShortTerm);

        let l0 = dpb.ref_list_l0_b(5);
        // Before (POC <= 5): 4, 2, 0 (descending POC)
        // After (POC > 5): 6, 8 (ascending POC)
        assert_eq!(l0.len(), 5);
        assert_eq!(l0[0].pic_order_cnt, 4);
        assert_eq!(l0[1].pic_order_cnt, 2);
        assert_eq!(l0[2].pic_order_cnt, 0);
        assert_eq!(l0[3].pic_order_cnt, 6);
        assert_eq!(l0[4].pic_order_cnt, 8);
    }

    #[test]
    fn test_ref_list_l1_b() {
        let mut dpb = Dpb::new(5);
        dpb.insert(make_pic(0, 0), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(1, 2), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(2, 4), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(3, 6), ReferenceStatus::ShortTerm);
        dpb.insert(make_pic(4, 8), ReferenceStatus::ShortTerm);

        let l1 = dpb.ref_list_l1_b(5);
        // After (POC > 5): 6, 8 (ascending POC)
        // Before (POC <= 5): 4, 2, 0 (descending POC)
        assert_eq!(l1.len(), 5);
        assert_eq!(l1[0].pic_order_cnt, 6);
        assert_eq!(l1[1].pic_order_cnt, 8);
        assert_eq!(l1[2].pic_order_cnt, 4);
        assert_eq!(l1[3].pic_order_cnt, 2);
        assert_eq!(l1[4].pic_order_cnt, 0);
    }

    /// Regression test for a fuzz-discovered panic.
    ///
    /// `apply_ref_list_modification` panicked with "index out of bounds"
    /// when a malformed bitstream contained more RPLM commands than there
    /// were active reference slots. `ref_idx_lx` grew past `num_active`,
    /// causing an out-of-bounds index after the shift-right + push.
    ///
    /// Fix: bail out of the loop when `ref_idx_lx >= num_active`.
    #[test]
    fn test_fuzz_regression_rplm_too_many_ops_no_panic() {
        // Build a ref list with 2 entries
        let mut ref_list = vec![make_pic(0, 0), make_pic(1, 2)];
        // Feed 10 RPLM ops (far more than the 2 active slots)
        // idc=0 means "subtract abs_diff_pic_num from pred_pic_num"
        // With val=0 → abs_diff=1 → pic_num = pred - 1
        let ops: Vec<(u32, u32)> = (0..10).map(|_| (0u32, 0u32)).collect();
        // This must not panic
        let short_term = ref_list.clone();
        Dpb::apply_ref_list_modification(&mut ref_list, &ops, 5, 16, &short_term);
    }

    #[test]
    fn test_rplm_finds_a_reference_pushed_out_of_the_list() {
        // x264's weightp duplicates the newest reference at indices 0 and 1,
        // which pushes the last of four references out of the working list
        // before the final command puts it back (frame_num has wrapped at 16)
        let refs = [make_pic(1, 62), make_pic(0, 66), make_pic(15, 54), make_pic(14, 58)];
        let mut dpb = Dpb::new(4);
        for pic in &refs {
            dpb.insert(pic.clone(), ReferenceStatus::ShortTerm);
        }
        let mut ref_list = dpb.short_term_ref_list(2, 16);
        let ops = [(0, 1), (0, 15), (1, 0), (0, 2)];
        Dpb::apply_ref_list_modification(&mut ref_list, &ops, 2, 16, &dpb.short_term_refs());
        let nums: Vec<u32> = ref_list.iter().map(|p| p.frame_num).collect();
        assert_eq!(nums, [0, 0, 1, 14]);
    }
}
