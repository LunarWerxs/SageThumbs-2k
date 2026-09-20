#![cfg(test)]

use super::*;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

/// Wraps a `Cursor` and counts `seek()` calls, so a test can prove a walk
/// stopped early instead of just checking its (possibly coincidentally
/// identical) final answer.
struct CountingReader<'a> {
    inner: Cursor<&'a [u8]>,
    seeks: usize,
}
impl Read for CountingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}
impl Seek for CountingReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.seeks += 1;
        self.inner.seek(pos)
    }
}

/// Validate the box-navigation primitives on a hand-built nested structure.
#[test]
fn box_walk_finds_nested_children() {
    let leaf = fbx(b"stss", 0, 0, &concat32(&[1, 7]));
    let stbl = container(b"stbl", &[&leaf]);
    let minf = container(b"minf", &[&stbl]);
    let found_stbl = find(box_body(&minf), b"stbl").unwrap();
    let found_stss = find(box_body(found_stbl), b"stss").unwrap();
    // full_box_body skips ver/flags → entry_count then the sample number.
    assert_eq!(g32(full_box_body(found_stss), 0), Some(1));
    assert_eq!(g32(full_box_body(found_stss), 4), Some(7));
}

/// A hostile 64-bit box size must END the walk, not wrap the cursor into a panic.
///
/// `size32 == 1` means "the real size is the u64 that follows", and that u64 is entirely
/// file-controlled. With `u64::MAX` the old `pos + full` wrapped (release builds carry no
/// overflow checks) to `pos - 1`, so `pos + full > buf.len()` was FALSE and the walk fell
/// through to `&buf[pos..pos - 1]` — a start-after-end slice panic, i.e. an abort of
/// whatever host was parsing, `explorer.exe` included.
///
/// The second box is where it has to be tested: at `pos == 0` the wrap lands on
/// `usize::MAX`, which the bounds check happens to catch. Only a non-zero cursor exposes it.
#[test]
fn a_wrapping_64bit_box_size_ends_the_walk_instead_of_panicking() {
    let mut buf = bx(b"ftyp", &[0u8; 8]); // a normal first box → cursor lands past 0
    let start = buf.len();
    assert!(start > 0);
    buf.extend_from_slice(&1u32.to_be_bytes()); // size32 == 1 → 64-bit extended form
    buf.extend_from_slice(b"moov");
    buf.extend_from_slice(&u64::MAX.to_be_bytes()); // the hostile length
    buf.extend_from_slice(&[0u8; 16]); // some payload, so the slice would be in-range-ish

    // Wrapping arithmetic is what the fix removed: prove the trap is still armed, so this
    // test cannot quietly stop measuring anything if the constant is ever changed.
    assert!(
        start.wrapping_add(u64::MAX as usize) < buf.len(),
        "the wrapped end must look in-bounds, or this fixture proves nothing"
    );

    let walked: Vec<[u8; 4]> = boxes(&buf).map(|(t, _)| t).collect();
    assert_eq!(
        walked,
        vec![*b"ftyp"],
        "the walk must stop AT the hostile box"
    );
    assert!(find(&buf, b"moov").is_none());
}

/// A `size32 == 1` box declares a 16-byte header (8 fixed + 8 extended), so its own claimed
/// size can never legally be smaller than 16. The walker used to check the claimed size
/// against a hardcoded `8` regardless of which header length was actually in play, so a box
/// like this one — extended size 10, which only covers the *fixed* header — was silently
/// accepted and yielded a box slice narrower than its own header (`box_body` would then read
/// past the end of a 10-byte slice looking for the 16-byte header offset and get an empty
/// body). The shared `decode_box_size` checks against the REAL header length (16 here), so
/// this must now be rejected outright instead of walked past.
#[test]
fn a_size_one_box_shorter_than_its_own_16_byte_header_is_rejected() {
    let mut buf = bx(b"ftyp", &[0u8; 8]); // a normal first box
    buf.extend_from_slice(&1u32.to_be_bytes()); // size32 == 1 → extended form
    buf.extend_from_slice(b"moov");
    buf.extend_from_slice(&10u64.to_be_bytes()); // claimed size: shorter than the header

    let walked: Vec<[u8; 4]> = boxes(&buf).map(|(t, _)| t).collect();
    assert_eq!(
        walked,
        vec![*b"ftyp"],
        "a box claiming to be shorter than its own header must be rejected, not accepted"
    );
}

/// A trak with no `mdia` is one bad track, not the end of the moov. Editors emit
/// hint/metadata/placeholder traks; when one led the file, the `?` here abandoned the
/// whole search and a perfectly good video track that came second was reported absent
/// (silent index-tier downgrade, plus doctor claiming the codec was unidentifiable).
/// Issue #32: the display-matrix rotation, pinned to the numbers it was MEASURED from.
///
/// The direction here is the whole risk. FFmpeg, the ISO spec and various players
/// describe this transform with opposite sign conventions, so a plausible reading of the
/// spec gets it backwards - and backwards rotates every phone video the wrong way, which
/// looks exactly like the bug being fixed rather than like a new one. These four rows were
/// therefore read out of real files written by the issue's own command
/// (`ffmpeg -display_rotation N -i in.mp4 -c copy out.mp4`), and the intended picture for
/// each was taken from `ffmpeg -frames:v 1` (autorotate on by default) and matched against
/// rotations of the unrotated original at a mean pixel difference of 0.00.
#[test]
fn rotation_matches_the_measured_ground_truth() {
    const ONE: i32 = FIXED_ONE;
    // (a, b, c, d) as written by ffmpeg          -> clockwise degrees to apply
    assert_eq!(rotation_from_matrix(0, -ONE, ONE, 0), Some(270)); // -display_rotation 90
    assert_eq!(rotation_from_matrix(-ONE, 0, 0, -ONE), Some(180)); // -display_rotation 180
    assert_eq!(rotation_from_matrix(0, ONE, -ONE, 0), Some(90)); // -display_rotation 270

    // The two 90-degree cases must NOT be interchangeable, or the assertions above would
    // pass just as happily with the direction inverted.
    assert_ne!(
        rotation_from_matrix(0, -ONE, ONE, 0),
        rotation_from_matrix(0, ONE, -ONE, 0),
        "the two quarter turns must map to different angles"
    );
}

/// An upright video, and everything that is not an exact right angle, must be left alone.
/// A thumbnail cannot honour a scale, a shear or a 37-degree tilt faithfully, and
/// half-honouring one would be worse than ignoring it.
#[test]
fn only_exact_right_angles_rotate_anything() {
    const ONE: i32 = FIXED_ONE;
    assert_eq!(rotation_from_matrix(ONE, 0, 0, ONE), None, "identity");
    assert_eq!(rotation_from_matrix(0, 0, 0, 0), None, "degenerate");
    assert_eq!(
        rotation_from_matrix(-ONE, 0, 0, ONE),
        None,
        "horizontal flip"
    );
    assert_eq!(rotation_from_matrix(ONE, 0, 0, -ONE), None, "vertical flip");
    assert_eq!(rotation_from_matrix(2 * ONE, 0, 0, 2 * ONE), None, "scale");
    assert_eq!(
        rotation_from_matrix(46341, -46341, 46341, 46341),
        None,
        "45 degrees"
    );
    // Near-misses are misses: a matrix one fixed-point unit off a right angle is not one.
    assert_eq!(
        rotation_from_matrix(0, -ONE + 1, ONE, 0),
        None,
        "off by one unit"
    );
}

/// The version-dependent offset walk, on both `tkhd` layouts. v0 packs the times as
/// 32-bit and v1 as 64-bit, so a parser that assumes one reads the matrix out of the
/// middle of some other field on the other - and would then usually find no right angle
/// and silently do nothing, which is indistinguishable from an upright video.
#[test]
fn the_matrix_is_found_in_both_tkhd_versions() {
    const ONE: i32 = FIXED_ONE;
    for version in [0u8, 1u8] {
        let tkhd = tkhd_with_matrix(version, 0, -ONE, ONE, 0);
        assert_eq!(
            rotation_from_tkhd(&tkhd),
            Some(270),
            "tkhd v{version} matrix must be read at the right offset"
        );
    }
    // An unknown version is declined rather than guessed at.
    assert_eq!(
        rotation_from_tkhd(&tkhd_with_matrix(2, 0, -ONE, ONE, 0)),
        None
    );
    // A tkhd cut off before the end of the matrix is a clean miss, never a panic - this
    // runs under `panic = "abort"`, so an out-of-bounds read here would take the shell
    // host down rather than decline a thumbnail.
    //
    // The boundary is stated exactly rather than as "some small numbers", because the two
    // sides mean different things - and it is the boundary of what THIS parser reads, not
    // of the matrix. Only a, b, c and d carry rotation, and d ends 68 bytes into a v0 box
    // (8 header + 4 version and flags + 20 times + 16 reserved/layer/volume, then a@48
    // b@52 u@56 c@60 d@64). The remaining matrix entries, the width and the height are all
    // surplus here, so a box cut anywhere after 68 still answers.
    const ROTATION_FIELDS_END: usize = 8 + 4 + 20 + 16 + 20;
    let short = tkhd_with_matrix(0, 0, -ONE, ONE, 0);
    assert!(
        short.len() > ROTATION_FIELDS_END,
        "the fixture must have a tail to cut"
    );
    for cut in [0usize, 8, 12, 20, 40, ROTATION_FIELDS_END - 1] {
        assert_eq!(
            rotation_from_tkhd(&short[..cut]),
            None,
            "truncated at {cut}"
        );
    }
    assert_eq!(
        rotation_from_tkhd(&short[..ROTATION_FIELDS_END]),
        Some(270),
        "everything past the d entry is surplus to this parser"
    );
}

/// End to end over a whole file: the rotation must be found through the real box walk,
/// on the VIDEO track, past a decoy track that claims a different rotation.
#[test]
fn display_rotation_reads_the_video_tracks_matrix() {
    const ONE: i32 = FIXED_ONE;
    // A sound track that (nonsensically, but this is the point) carries a 180 matrix.
    let sound = container(
        b"trak",
        &[
            &tkhd_with_matrix(0, -ONE, 0, 0, -ONE),
            &container(b"mdia", &[&hdlr_of(b"soun")]),
        ],
    );
    let video = container(
        b"trak",
        &[
            &tkhd_with_matrix(0, 0, ONE, -ONE, 0),
            &container(b"mdia", &[&hdlr_of(b"vide")]),
        ],
    );
    let moov = container(b"moov", &[&sound, &video]);
    let mut file = default_ftyp();
    file.extend_from_slice(&moov);

    assert_eq!(
        display_rotation(&mut Cursor::new(&file)),
        Some(90),
        "the VIDEO track decides, not whichever trak comes first"
    );

    // Not an ISO-BMFF at all, and an upright video: both are a quiet None.
    assert_eq!(
        display_rotation(&mut Cursor::new(b"not a video".to_vec())),
        None
    );
    let upright = container(
        b"trak",
        &[
            &tkhd_with_matrix(0, ONE, 0, 0, ONE),
            &container(b"mdia", &[&hdlr_of(b"vide")]),
        ],
    );
    let mut plain = default_ftyp();
    plain.extend_from_slice(&container(b"moov", &[&upright]));
    assert_eq!(display_rotation(&mut Cursor::new(&plain)), None);
}

/// A `tkhd` of the given version whose 2x2 matrix part is (a, b, c, d), everything else
/// zero. Built to the real layout rather than to the parser's expectations.
fn tkhd_with_matrix(version: u8, a: i32, b: i32, c: i32, d: i32) -> Vec<u8> {
    let times = if version == 0 { 20 } else { 32 };
    let mut body = vec![0u8; times + 16]; // through volume + reserved
    for v in [a, b, 0, c, d, 0, 0, 0, 1 << 30] {
        body.extend_from_slice(&(v as u32).to_be_bytes());
    }
    body.extend_from_slice(&[0u8; 8]); // width + height, 16.16
    fbx(b"tkhd", version, 0x0000_0007, &body)
}

/// A minimal `hdlr` declaring `kind` ("vide" / "soun").
fn hdlr_of(kind: &[u8; 4]) -> Vec<u8> {
    let mut body = vec![0u8; 4]; // pre_defined
    body.extend_from_slice(kind);
    body.extend_from_slice(&[0u8; 12]);
    fbx(b"hdlr", 0, 0, &body)
}

/// An `stsd` with one `avc1` entry whose `avcC` declares `profile`. Built to the real
/// VisualSampleEntry layout (78 fixed bytes after the box header, then the children),
/// not to the parser's expectations.
fn stsd_avc1(profile: u8) -> Vec<u8> {
    let mut fields = vec![0u8; 6]; // reserved
    fields.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    fields.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
    fields.extend_from_slice(&320u16.to_be_bytes()); // width
    fields.extend_from_slice(&240u16.to_be_bytes()); // height
    fields.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution 72 dpi
    fields.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution
    fields.extend_from_slice(&[0u8; 4]); // reserved
    fields.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    fields.extend_from_slice(&[0u8; 32]); // compressorname
    fields.extend_from_slice(&24u16.to_be_bytes()); // depth
    fields.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
    assert_eq!(fields.len(), 78);
    // AVCDecoderConfigurationRecord: version 1, profile, compatibility, level 3.1,
    // 4-byte NAL lengths, no SPS, no PPS - the shape, not a decodable stream.
    fields.extend_from_slice(&bx(b"avcC", &[1, profile, 0x00, 31, 0xFF, 0xE0, 0x00]));
    let entry = bx(b"avc1", &fields);
    let mut body = 1u32.to_be_bytes().to_vec(); // entry_count
    body.extend_from_slice(&entry);
    fbx(b"stsd", 0, 0, &body)
}

/// A parseable ISO-BMFF whose video trak carries `stsd` and nothing else: enough for the
/// codec probes, deliberately not enough for a keyframe.
fn file_with_stsd(stsd: &[u8]) -> Vec<u8> {
    let stbl = container(b"stbl", &[stsd]);
    let minf = container(b"minf", &[&stbl]);
    let mdia = container(b"mdia", &[&hdlr_of(b"vide"), &minf]);
    let trak = container(b"trak", &[&mdia]);
    let mut file = default_ftyp();
    file.extend_from_slice(&container(b"moov", &[&trak]));
    file
}

#[test]
fn h264_profile_idc_reads_the_avcc_profile_indication() {
    for profile in [66u8, 77, 100, 110, 122, 244] {
        let file = file_with_stsd(&stsd_avc1(profile));
        assert_eq!(h264_profile_idc(&mut Cursor::new(&file)), Some(profile));
    }
    // An HEVC entry is not H.264, whatever its children say.
    let mut hevc = stsd_avc1(244);
    let at = hevc.windows(4).position(|w| w == b"avc1").unwrap();
    hevc[at..at + 4].copy_from_slice(b"hvc1");
    assert_eq!(
        h264_profile_idc(&mut Cursor::new(&file_with_stsd(&hevc))),
        None
    );
    assert_eq!(
        h264_profile_idc(&mut Cursor::new(b"not a video".to_vec())),
        None
    );
}

/// The whole issue #35 gate, over a synthetic file AND over the mini-clip the cascades
/// actually ask about: `build_mini_mp4` copies the stsd verbatim, so the profile survives
/// into the clip and the refusal costs no second read of the original.
#[test]
fn mf_undecodable_reason_names_the_444_profile_and_clears_the_420_one() {
    let stsd = stsd_avc1(244);
    let reason = crate::vcodec::mf_undecodable_reason(&mut Cursor::new(&file_with_stsd(&stsd)))
        .expect("High 4:4:4 Predictive must be refused");
    assert!(
        reason.contains("High 4:4:4 Predictive") && reason.contains("244"),
        "{reason}"
    );
    let mini = build_mini_mp4(None, &stsd, 1, 512, 15360, 320, 240, &[0u8; 64]);
    assert!(
        crate::vcodec::mf_undecodable_reason(&mut Cursor::new(&mini)).is_some(),
        "the mini-clip must carry the refusal"
    );
    let plain = build_mini_mp4(None, &stsd_avc1(100), 1, 512, 15360, 320, 240, &[0u8; 64]);
    assert_eq!(
        crate::vcodec::mf_undecodable_reason(&mut Cursor::new(&plain)),
        None,
        "High 8-bit 4:2:0 is exactly what the decoder implements"
    );
}

#[test]
fn video_track_found_after_a_trak_with_no_mdia() {
    let mut hdlr_body = vec![0u8; 4]; // version+flags
    hdlr_body.extend_from_slice(&[0u8; 4]); // pre_defined
    hdlr_body.extend_from_slice(b"vide"); // handler_type
    hdlr_body.extend_from_slice(&[0u8; 12]);
    let hdlr = bx(b"hdlr", &hdlr_body);
    let mdia = container(b"mdia", &[&hdlr]);
    let good = container(b"trak", &[&mdia]);
    // A leading trak carrying only a tkhd — no mdia at all.
    let broken = container(b"trak", &[&bx(b"tkhd", &[0u8; 12])]);
    let moov = container(b"moov", &[&broken, &good]);

    assert!(
        video_mdia(box_body(&moov)).is_some(),
        "the video trak after an mdia-less trak must still be found"
    );
}

/// iTunes-style cover art: moov ▸ udta ▸ meta ▸ ilst ▸ covr ▸ data, with the 8-byte
/// type-indicator/locale header stripped. `meta` is a FULL box here.
#[test]
fn cover_art_read_from_the_covr_atom() {
    const JPEG: &[u8] = b"\xFF\xD8\xFFrest-of-the-jpeg";
    let mut data_body = vec![0u8; 8]; // type indicator (13 = JPEG) + locale
    data_body[3] = 13;
    data_body.extend_from_slice(JPEG);
    let data = bx(b"data", &data_body);
    let covr = container(b"covr", &[&data]);
    let ilst = container(b"ilst", &[&covr]);
    // meta as a full box: version+flags then children.
    let meta = fbx(b"meta", 0, 0, &ilst);
    let udta = container(b"udta", &[&meta]);
    let moov = container(b"moov", &[&udta]);
    let mut file = bx(b"ftyp", b"isomisom");
    file.extend_from_slice(&moov);

    assert_eq!(
        cover_art(&mut Cursor::new(&file)).as_deref(),
        Some(JPEG),
        "the covr payload past the 8-byte data header"
    );

    // A QuickTime-style `meta` with NO version/flags must still resolve.
    let meta_plain = container(b"meta", &[&ilst]);
    let udta2 = container(b"udta", &[&meta_plain]);
    let moov2 = container(b"moov", &[&udta2]);
    let mut file2 = bx(b"ftyp", b"isomisom");
    file2.extend_from_slice(&moov2);
    assert_eq!(cover_art(&mut Cursor::new(&file2)).as_deref(), Some(JPEG));
}

/// A `covr` whose payload is not actually an image must be declined, so the caller
/// falls through to the real decode tiers instead of being handed junk as "the cover".
#[test]
fn cover_art_declines_a_non_image_payload() {
    let mut data_body = vec![0u8; 8];
    data_body.extend_from_slice(b"this is not an image at all");
    let file = {
        let data = bx(b"data", &data_body);
        let covr = container(b"covr", &[&data]);
        let ilst = container(b"ilst", &[&covr]);
        let meta = fbx(b"meta", 0, 0, &ilst);
        let moov = container(b"moov", &[&container(b"udta", &[&meta])]);
        let mut f = bx(b"ftyp", b"isomisom");
        f.extend_from_slice(&moov);
        f
    };
    assert_eq!(cover_art(&mut Cursor::new(&file)), None);
    // And a file with no udta at all is simply None, not a panic.
    let bare = {
        let mut f = bx(b"ftyp", b"isomisom");
        f.extend_from_slice(&container(b"moov", &[&bx(b"mvhd", &[0u8; 32])]));
        f
    };
    assert_eq!(cover_art(&mut Cursor::new(&bare)), None);
}

/// 30 % of a uniform-cadence track should land on the sample nearest that time.
#[test]
fn stts_maps_fraction_to_sample() {
    // 100 samples, each 1000 ticks → total 100_000; 30 % → tick 30_000 → sample 30.
    let stts = fbx(b"stts", 0, 0, &concat32(&[1, 100, 1000]));
    let (sample, delta) = stts_target(full_box_body(&stts), 0.30).unwrap();
    assert_eq!(sample, 30);
    assert_eq!(delta, 1000);
}

/// nearest_sync takes the keyframe at or before the target; None stss ⇒ target itself.
#[test]
fn sync_sample_selection() {
    let stss = fbx(b"stss", 0, 0, &concat32(&[3, 1, 31, 61])); // sync at 1,31,61
    assert_eq!(nearest_sync(Some(&stss), 31), Some(31));
    assert_eq!(nearest_sync(Some(&stss), 45), Some(31)); // at-or-before
    assert_eq!(nearest_sync(Some(&stss), 70), Some(61));
    assert_eq!(nearest_sync(Some(&stss), 1), Some(1));
    assert_eq!(nearest_sync(None, 42), Some(42)); // all samples sync
}

/// stsc/stco offset resolution: 2 chunks × 2 samples, uniform 10-byte samples.
#[test]
fn sample_offset_resolution() {
    // chunk1 @ 1000, chunk2 @ 2000; each holds 2 samples of 10 bytes.
    let stsc = fbx(b"stsc", 0, 0, &concat32(&[1, 1, 2, 1])); // one run: first_chunk=1, spc=2, desc=1
    let stco = fbx(b"stco", 0, 0, &concat32(&[2, 1000, 2000]));
    let stsz = fbx(b"stsz", 0, 0, &concat32(&[10, 4])); // uniform 10 bytes, 4 samples
    let sizes = SampleSizes::Stsz(&stsz);
    // sample 0 → chunk1 + 0 = 1000; sample1 → 1010; sample2 → chunk2 = 2000; sample3 → 2010
    let cases = [(0u64, 1000u64), (1, 1010), (2, 2000), (3, 2010)];
    for (s, want) in cases {
        let (off, desc) = sample_location(full_box_body(&stsc), (&stco, false), &sizes, s).unwrap();
        assert_eq!(off, want, "sample {s}");
        assert_eq!(desc, 1);
    }
}

/// `locate_chunk_for_sample`'s arithmetic must decline crafted `stsc` tables
/// instead of panicking (debug/test, where overflow checks are on by default) or
/// wrapping to a bogus chunk (release). Two shapes: a run whose `next_first` is
/// smaller than `first_chunk` (an invalid decreasing table), and a `num_chunks`
/// large enough that `(next_first - first_chunk) * samples_per_chunk` overflows u64.
#[test]
fn locate_chunk_for_sample_declines_crafted_overflow() {
    // Entry 1: first_chunk=5, spc=1, desc=1. Entry 2 (the terminating one, so its
    // first_chunk is read as `next_first` for entry 1): first_chunk=2 — LESS than
    // entry 1's first_chunk=5, an invalid decreasing table.
    let stsc = fbx(b"stsc", 0, 0, &concat32(&[2, 5, 1, 1, 2, 1, 1]));
    assert_eq!(locate_chunk_for_sample(full_box_body(&stsc), 10, 0), None);

    // A single (i.e. terminating) run whose `next_first` comes straight from
    // `num_chunks` — a caller-controlled u64, not a u32-bounded stsc field — so a
    // hostile `num_chunks` alone can push `next_first - first_chunk` to ~u64::MAX;
    // multiplying that by a near-u32::MAX samples-per-chunk must decline, not wrap.
    let huge = fbx(b"stsc", 0, 0, &concat32(&[1, 1, u32::MAX, 1]));
    assert_eq!(
        locate_chunk_for_sample(full_box_body(&huge), u64::MAX - 1, 0),
        None
    );
}

/// stz2 compact sizes (8- and 16-bit fields).
#[test]
fn stz2_sizes() {
    let mut body8 = Vec::new();
    body8.push(0); // reserved
    body8.extend_from_slice(&[0, 0]); // reserved
    body8.push(8); // field_size
    body8.extend_from_slice(&3u32.to_be_bytes()); // sample_count
    body8.extend_from_slice(&[11, 22, 33]);
    let stz2 = fbx(b"stz2", 0, 0, &body8);
    let sizes = SampleSizes::Stz2(&stz2);
    assert_eq!(sizes.size_of(0), Some(11));
    assert_eq!(sizes.size_of(2), Some(33));
    assert_eq!(sizes.size_of(3), None);
}

/// `keyframe_mini_mp4` must hand back the rotation it already read off
/// the same `tkhd` it walked to find `mdia`, instead of making the caller re-scan the
/// moov a second time through [`display_rotation`]. A synthetic single-track, one-chunk
/// moov (stco offset 0 so the "keyframe" bytes are just whatever header bytes happen to
/// sit there — this test only cares about the rotation, not the decoded frame).
#[test]
fn keyframe_mini_mp4_returns_the_rotation_it_already_parsed() {
    const ONE: i32 = FIXED_ONE;
    let stsd = fbx(b"stsd", 0, 0, &concat32(&[0]));
    let stts = fbx(b"stts", 0, 0, &concat32(&[1, 10, 1000]));
    let stsc = fbx(b"stsc", 0, 0, &concat32(&[1, 1, 10, 1]));
    let stsz = fbx(b"stsz", 0, 0, &concat32(&[8, 10]));
    let stco = fbx(b"stco", 0, 0, &concat32(&[1, 0]));
    let stbl = container(b"stbl", &[&stsd, &stts, &stsc, &stsz, &stco]);
    let minf = container(b"minf", &[&stbl]);
    let mdia = container(b"mdia", &[&hdlr_of(b"vide"), &minf]);
    let trak = container(
        b"trak",
        &[&tkhd_with_matrix(0, 0, ONE, -ONE, 0), &mdia], // 90 deg clockwise
    );
    let moov = container(b"moov", &[&trak]);
    let mut file = default_ftyp();
    file.extend_from_slice(&moov);

    let (mini, rotation) = keyframe_mini_mp4(&mut Cursor::new(&file), 0.30)
        .expect("synthetic single-track moov should yield a mini-mp4");
    assert!(!mini.is_empty());
    assert_eq!(rotation, Some(90));

    // The upright twin: same shape, identity matrix, rotation must come back None.
    let trak_upright = container(b"trak", &[&tkhd_with_matrix(0, ONE, 0, 0, ONE), &mdia]);
    let moov_upright = container(b"moov", &[&trak_upright]);
    let mut file_upright = default_ftyp();
    file_upright.extend_from_slice(&moov_upright);
    let (_, rotation) = keyframe_mini_mp4(&mut Cursor::new(&file_upright), 0.30)
        .expect("upright synthetic moov should still yield a mini-mp4");
    assert_eq!(rotation, None);
}

/// End-to-end: parse a real MP4 (the corpus `sample.mp4`, or `ST2K_TEST_VIDEO` if set)
/// into a one-keyframe mini-MP4 and decode it through Media Foundation. Skipped where no
/// sample is available (e.g. CI), so `cargo test` stays green without committing a video
/// fixture.
#[test]
fn real_mp4_round_trips_through_mediafoundation() {
    let candidates = [
        std::env::var("ST2K_TEST_VIDEO").ok(),
        Some(
            crate::testcorpus::real_dir()
                .join("sample.mp4")
                .to_string_lossy()
                .into_owned(),
        ),
        Some(
            crate::testcorpus::dir()
                .join("sample.mp4")
                .to_string_lossy()
                .into_owned(),
        ),
    ];
    let Some(path) = candidates
        .into_iter()
        .flatten()
        .find(|p| Path::new(p).is_file())
    else {
        eprintln!("real_mp4_round_trips: no sample video found — skipping");
        return;
    };

    let bytes = std::fs::read(&path).expect("read sample video");
    let (mini, _rotation) =
        keyframe_mini_mp4(&mut Cursor::new(&bytes), 0.30).expect("build mini-mp4 from real sample");
    assert!(
        mini.len() < bytes.len().max(2 * 1024 * 1024),
        "mini-mp4 should be small ({} bytes from {} source)",
        mini.len(),
        bytes.len()
    );
    // The synthesized container must start ftyp…moov…mdat.
    assert_eq!(&mini[4..8], b"ftyp");
    assert!(mini.windows(4).any(|w| w == b"moov"));
    assert!(mini.windows(4).any(|w| w == b"mdat"));

    let frame = crate::video::frame_from_bytes(&mini)
        .expect("Media Foundation should decode the mini-mp4 keyframe");
    assert!(frame.width() > 0 && frame.height() > 0);
    eprintln!(
        "real_mp4_round_trips: {} → mini {} bytes → frame {}x{}",
        path,
        mini.len(),
        frame.width(),
        frame.height()
    );
}

/// A030: `scan_top_level`'s loop used to be bounded only by total file size, so a
/// file built from many tiny top-level boxes (no `moov` anywhere) drove one
/// Seek+Read pair per box. Build more boxes than `MAX_TOP_LEVEL_BOXES` and prove
/// the walk stops at the cap instead of visiting every one of them: checks the
/// seek COUNT, not just the final `None`, since "no moov" returns `None` either way.
#[test]
fn scan_top_level_bails_after_max_top_level_boxes() {
    let mut buf = bx(b"ftyp", &[0u8; 8]);
    let tiny_box_count = MAX_TOP_LEVEL_BOXES as usize + 900;
    for _ in 0..tiny_box_count {
        buf.extend_from_slice(&bx(b"free", &[]));
    }
    let mut r = CountingReader {
        inner: Cursor::new(buf.as_slice()),
        seeks: 0,
    };
    assert_eq!(
        scan_top_level(&mut r),
        None,
        "no moov present anywhere, so this must fail either way"
    );
    assert!(
        r.seeks <= MAX_TOP_LEVEL_BOXES as usize + 2,
        "must bail at the box cap instead of walking all {tiny_box_count} boxes (seeks = {})",
        r.seeks
    );
}

/// A233: `stss` PRESENT but with sample_count == 0 means there are NO sync
/// samples, which is a different claim than `stss` ABSENT (every sample is
/// sync). Both used to return `Some(target)`, silently building a mini-MP4
/// around a sample that may not be independently decodable.
#[test]
fn nearest_sync_declines_a_present_but_empty_stss() {
    let empty_stss = fbx(b"stss", 0, 0, &concat32(&[0])); // sample_count = 0
    assert_eq!(nearest_sync(Some(&empty_stss), 42), None);
    // The genuinely-absent case is unaffected: still "every sample is sync".
    assert_eq!(nearest_sync(None, 42), Some(42));
}

/// A234: the old scan trusted stss's claimed ascending order and broke at the
/// first overshoot, so an unsorted table (a buggy muxer) could silently return
/// the wrong sync sample instead of the true nearest one <= target.
#[test]
fn nearest_sync_finds_the_true_nearest_in_an_unsorted_stss() {
    // Deliberately out of order: 61 appears before 31, but 31 is the nearer
    // sync sample <= 45. A break-on-first-overshoot scan sees 61 > 45 first
    // and stops, missing 31 (and returning the wrong answer) entirely.
    let stss = fbx(b"stss", 0, 0, &concat32(&[3, 61, 31, 1]));
    assert_eq!(nearest_sync(Some(&stss), 45), Some(31));
}
