# Changes from rust_h264 0.4.0

This is rust_h264 0.4.0 as published on crates.io, with five fixes. Without
them, ordinary MP4s from x264 and NVENC decoded with errors that grew until the
next keyframe. The fixes were checked against ffmpeg on real recordings and are
offered upstream in https://github.com/roticv/rust_h264/pull/2. readme can go back
to the crates.io release once they're in one.

1. **Deblocking stride** (`deblock.rs`). The filter stepped through rows at the
   cropped width (`frame.width`), but the frame is still at its coded width
   (a multiple of 16) when it's filtered. Any video whose width isn't a
   multiple of 16 was filtered in the wrong places, for example 2086 pixels
   wide, coded as 2096.

2. **P-slice reference list order** (`dpb.rs`, `short_term_ref_list`). The
   initial list was sorted by descending POC, used as a stand-in for decoding
   order. Spec 8.2.4.2.1 sorts by descending PicNum (FrameNumWrap). The two
   orders differ once B-frames are used as references, as NVENC's middle
   B-frames are, so P-frames predicted from the wrong pictures.

3. **Reference list modification** (`dpb.rs`, `apply_ref_list_modification`).
   A command can name any short-term reference (spec 8.2.4.3.1), including one
   the working list has no room for. x264's weighted prediction duplicates the
   newest reference, which pushes the last one out before a later command
   brings it back. The lookup was only made in the list, so the command was
   ignored.

4. **Per-slice deblocking** (`deblock.rs`, `decoder.rs`). Each macroblock's
   edges are filtered as its own slice's header says (spec 8.7), and with
   `disable_deblocking_filter_idc` 2 an edge shared with another slice isn't
   filtered at all. The decoder used the first slice's settings for the whole
   picture and never honoured 2, which x264 uses when it encodes with sliced
   threads. Cr also now uses `second_chroma_qp_index_offset`.

5. **Reference marking after frame_num wraps** (`decoder.rs`). MMCO commands name
   a picture by PicNum, counted back from the current frame_num, and once
   frame_num has wrapped that number is negative. It was cast to `u32` and
   matched nothing, so the picture stayed a reference and the sliding window
   dropped another one in its place. x264's B-pyramid sends these commands in
   every reference B-frame, so a few frames after each wrap, blocks predicted
   from that list index came from the wrong picture: visible as blocks of wrong
   color. The picture is now the one whose frame_num is PicNum modulo
   MaxFrameNum (spec 8.2.5.4.1).

With these fixes, x264 and NVENC recordings decode exactly as ffmpeg decodes them.
`testdata/pyramid.mp4` needs the fixes to match ffmpeg (upstream trunk without
them doesn't), but fix 5 doesn't change that clip, so a unit test in `dpb.rs`
covers it instead. The one exception: in one x264 film the color planes still
differ slightly in most frames, 60 dB at worst, which is far below what can be
seen. Its luma matches exactly.
