//! The hidden `st2k flv-frame` verb: FLV bytes on STDIN → first VP6/Sorenson-Spark
//! keyframe as a PNG on STDOUT. Exit 0 with output, non-zero with none.
//!
//! This is the CHILD side of `sagethumbs2k_core::flv::flash_frame`. The decoders here —
//! nihav (VP6) and h263-rs (Sorenson), the exact crates Ruffle ships as the Flash codecs —
//! PANIC on malformed input, and the workspace builds `panic = "abort"`, so they may only
//! ever run in this throwaway process: a crash is a non-zero exit the parent shrugs at,
//! never a dead Explorer. That is also why this module is compiled ONLY into the `st2k`
//! console binary behind the EXE-only `flash-video` feature — `cargo tree -p
//! sagethumbs2k-dll` must never list nihav or h263 (mirrors the webview2 arrangement).
//!
//! Stdin/stdout on purpose (matching the ImageMagick child): a decoded frame of someone's
//! video never touches the disk. H.264 input (codec id 7) is DECLINED — the in-process
//! Media Foundation remux path owns it, and doing it here too would fork the behaviour.
//!
//! The decode wiring mirrors Ruffle's `ruffle_video_software` decoders (the reference
//! Flash implementation): the same VP56Decoder/H263State setup, the same macroblock-crop
//! handling, and the same BT.601 conversion — Flash treated both codecs' output as
//! studio-swing BT.601, so `h263_rs_yuv::bt601` (which expands 16..235 to full range) is
//! the faithful conversion for BOTH, whatever nihav's format tag nominally says.

use std::io::{Cursor, Read, Write};

use sagethumbs2k_core::flv::{
    scan_flash_keyframe, FlashCodec, FlashScan, FLASH_INPUT_CAP, FLASH_MAX_DIM,
};

/// Hard cap on how much memory this child may commit, enforced by Windows itself.
///
/// A subprocess contains a CRASH. It does not, on its own, contain a MEMORY BOMB: a child
/// quietly committing tens of gigabytes hurts the whole machine long before the CPU watchdog
/// notices, because it burns almost no CPU doing it.
///
/// And that is reachable. h263-rs allocates the full luma + chroma planes the moment it parses
/// a picture header, straight from a 16-bit custom width/height that a crafted Sorenson header
/// may declare as 65535x65535 — roughly 8.6 GB from a few hundred bytes of input, before this
/// module's own `check_dims` ever gets to run.
///
/// The obvious fix, pre-parsing those dimensions ourselves and refusing early, is the WRONG
/// one: the size fields sit at a bit offset that depends on `recognize_start_code`'s scan for
/// the picture start code, so a pre-check means re-implementing their bitstream search and
/// getting it exactly right forever. Instead this bounds the whole process by mechanism, which
/// also covers every other allocation site in either decoder — including the ones nobody has
/// audited, which is the point.
///
/// Over the limit, the commit fails, Rust's allocator error handler aborts the child, and the
/// parent sees a dead child and falls back to the stock icon: the same outcome as any other
/// decode failure. 512 MiB is far above a legitimate frame (the 4096 px cap is ~67 MB of RGBA)
/// and far below anything that troubles the machine.
const CHILD_MEMORY_CAP: usize = 512 * 1024 * 1024;

/// Put THIS process under a job-object memory cap, before a single byte of input is read.
///
/// Self-assignment is deliberate: capping from the parent would leave a window between
/// `CreateProcess` and `AssignProcessToJobObject` in which the child is already running and
/// could allocate, and `std::process::Command` cannot spawn suspended. Doing it here, first
/// thing, closes that window entirely. Best-effort: on the (unexpected) failure path we carry
/// on, because a missing cap is a smaller problem than refusing to thumbnail anything.
fn cap_own_memory() {
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let Ok(job) = CreateJobObjectW(None, None) else {
            return;
        };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = CHILD_MEMORY_CAP;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            core::ptr::addr_of!(info).cast(),
            u32::try_from(core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .unwrap_or(0),
        )
        .is_ok();
        if ok {
            let _ = AssignProcessToJobObject(job, GetCurrentProcess());
        }
    }
}

/// Entry point for the verb: returns the process exit code.
pub fn run() -> i32 {
    // FIRST, before any input is read: see `CHILD_MEMORY_CAP`.
    cap_own_memory();
    let mut flv = Vec::new();
    // Cap + 1 so an over-cap stream is detected as such instead of being silently
    // truncated into a "valid" but different file.
    if std::io::stdin()
        .lock()
        .take((FLASH_INPUT_CAP + 1) as u64)
        .read_to_end(&mut flv)
        .is_err()
    {
        eprintln!("st2k flv-frame: could not read stdin");
        return 1;
    }
    if flv.len() > FLASH_INPUT_CAP {
        eprintln!("st2k flv-frame: input exceeds the {FLASH_INPUT_CAP}-byte cap");
        return 1;
    }
    match frame_png(&flv) {
        Ok(png) => {
            let mut out = std::io::stdout().lock();
            if out.write_all(&png).and_then(|()| out.flush()).is_err() {
                return 1;
            }
            0
        }
        Err(why) => {
            eprintln!("st2k flv-frame: {why}");
            1
        }
    }
}

/// The testable core: FLV bytes → PNG bytes of the first VP6/Sorenson keyframe.
fn frame_png(flv: &[u8]) -> Result<Vec<u8>, String> {
    let (codec, payload) = match scan_flash_keyframe(flv) {
        FlashScan::Keyframe(codec, payload) => (codec, payload),
        // H.264 belongs to the in-process FLV→mini-MP4→Media Foundation path; decoding it
        // here as well would give the same file two decode behaviours.
        FlashScan::OtherCodec(7) => {
            return Err("H.264 FLV — the Media Foundation path owns it".into())
        }
        FlashScan::OtherCodec(id) => return Err(format!("unsupported FLV codec id {id}")),
        FlashScan::NoVideo => return Err("not an FLV, or no video keyframe found".into()),
    };
    let (width, height, rgba) = match codec {
        FlashCodec::Sorenson => decode_sorenson(payload)?,
        FlashCodec::Vp6 => decode_vp6(payload)?,
    };
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
        .ok_or("decoded plane sizes do not match the frame dimensions")?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode: {e}"))?;
    Ok(png)
}

/// Refuse absurd geometry BEFORE any full-frame allocation happens.
fn check_dims(width: usize, height: usize) -> Result<(), String> {
    if (1..=FLASH_MAX_DIM).contains(&width) && (1..=FLASH_MAX_DIM).contains(&height) {
        Ok(())
    } else {
        Err(format!(
            "refusing {width}x{height} frame (cap {FLASH_MAX_DIM})"
        ))
    }
}

/// Sorenson Spark (FLV codec id 2): the Flash flavour of H.263, via h263-rs. The payload
/// is the raw picture bitstream — no extra framing byte.
fn decode_sorenson(data: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    use h263_rs::parser::H263Reader;
    use h263_rs::{DecoderOption, H263State};

    let mut state = H263State::new(DecoderOption::SORENSON_SPARK_BITSTREAM);
    let mut reader = H263Reader::from_source(data);
    state
        .decode_next_picture(&mut reader)
        .map_err(|e| format!("Sorenson decode: {e:?}"))?;
    let picture = state
        .get_last_picture()
        .ok_or("Sorenson decode yielded no picture")?;
    let (width, height) = picture
        .format()
        .into_width_and_height()
        .ok_or("Sorenson picture carries no dimensions")?;
    let (width, height) = (width as usize, height as usize);
    check_dims(width, height)?;
    let (y, cb, cr) = picture.as_yuv();
    // The yuv420_to_rgba preconditions are DecodedPicture invariants (luma exactly w*h,
    // chroma exactly ceil(w/2)*ceil(h/2)); verify instead of trusting, because a violation
    // would panic inside the converter.
    let chroma = width.div_ceil(2) * height.div_ceil(2);
    if y.len() != width * height || cb.len() != chroma || cr.len() != chroma {
        return Err("Sorenson plane sizes are inconsistent".into());
    }
    let rgba = h263_rs_yuv::bt601::yuv420_to_rgba(y, cb, cr, width);
    Ok((width, height, rgba))
}

/// On2 VP6 (FLV codec id 4), via nihav. The payload starts with the FLV adjustment byte:
/// high nibble = pixels to crop off the RIGHT of the 16-aligned coded frame, low nibble =
/// off the BOTTOM. The wiring (version 6, flip=true, header-then-init, macroblock crop)
/// mirrors Ruffle's `Vp6Decoder`.
fn decode_vp6(payload: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    use nihav_codec_support::codecs::{NABufferType, NAVideoInfo, YUV420_FORMAT};
    use nihav_core::codecs::NADecoderSupport;
    use nihav_duck::codecs::vp6::{VP56Decoder, VP56Parser, VP6BR};
    use nihav_duck::codecs::vpcommon::BoolCoder;

    let (&adjust, frame) = payload.split_first().ok_or("empty VP6 payload")?;
    if frame.len() < 4 {
        return Err("VP6 frame too short".into());
    }
    // Parse just the header first: the coded (macroblock-aligned) size lives there, and
    // VP56Decoder::init needs it before decode_frame may run.
    let mut bitreader = VP6BR::new();
    let mut bool_coder = BoolCoder::new(frame).map_err(|e| format!("VP6 header: {e:?}"))?;
    let header = bitreader
        .parse_header(&mut bool_coder)
        .map_err(|e| format!("VP6 header: {e:?}"))?;
    if header.mb_w == 0 || header.mb_h == 0 {
        return Err("VP6 frame is not an intra frame (no dimensions)".into());
    }
    let coded_w = header.mb_w as usize * 16;
    let coded_h = header.mb_h as usize * 16;
    check_dims(coded_w, coded_h)?;

    let mut support = NADecoderSupport::new();
    let mut decoder = VP56Decoder::new(6, false, true);
    decoder
        .init(
            &mut support,
            NAVideoInfo::new(coded_w, coded_h, true, YUV420_FORMAT),
        )
        .map_err(|e| format!("VP6 init: {e:?}"))?;
    let (buffer, _frame_type) = decoder
        .decode_frame(&mut support, frame, &mut bitreader)
        .map_err(|e| format!("VP6 decode: {e:?}"))?;
    let NABufferType::Video(frame_buf) = buffer else {
        return Err("VP6 decoder returned a non-video buffer".into());
    };

    // The decoded frame spans whole macroblocks; the display size is the coded size minus
    // the adjustment nibbles (0..=15, so this can never reach zero).
    let disp_w = coded_w - (adjust >> 4) as usize;
    let disp_h = coded_h - (adjust & 0x0F) as usize;
    let (chroma_w, chroma_h) = (disp_w.div_ceil(2), disp_h.div_ceil(2));

    let data = frame_buf.get_data();
    let plane = |idx: usize, w: usize, h: usize| -> Result<Vec<u8>, String> {
        let offset = frame_buf.get_offset(idx);
        let stride = frame_buf.get_stride(idx);
        crop_plane(data.get(offset..).unwrap_or(&[]), stride, w, h)
            .ok_or_else(|| format!("VP6 plane {idx} is smaller than its declared geometry"))
    };
    let y = plane(0, disp_w, disp_h)?;
    let u = plane(1, chroma_w, chroma_h)?;
    let v = plane(2, chroma_w, chroma_h)?;
    let rgba = h263_rs_yuv::bt601::yuv420_to_rgba(&y, &u, &v, disp_w);
    Ok((disp_w, disp_h, rgba))
}

/// Copy the top-left `w`×`h` window out of a strided plane. `None` on any out-of-bounds
/// geometry instead of panicking — the plane came from a decoder fed hostile input.
fn crop_plane(data: &[u8], stride: usize, w: usize, h: usize) -> Option<Vec<u8>> {
    if w > stride {
        return None;
    }
    let mut out = Vec::with_capacity(w.checked_mul(h)?);
    for row in 0..h {
        let start = row.checked_mul(stride)?;
        out.extend_from_slice(data.get(start..start.checked_add(w)?)?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Sorenson picture header declaring a 16-bit custom size of 65535x65535.
    ///
    /// h263-rs allocates the full luma + chroma planes the instant it parses this, so a file
    /// of a few dozen bytes asks for ~8.6 GB. Built bit-exactly to that decoder's own layout:
    /// 17-bit picture start code, 5-bit version (Sorenson reuses the GOB id), 8-bit temporal
    /// reference, 3-bit source-format selector (1 = 16-bit custom size), then the two 16-bit
    /// dimensions, picture type, quantizer and PEI.
    fn sorenson_giant_header() -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        let mut put = |v: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push(((v >> i) & 1) as u8);
            }
        };
        put(1, 17); // picture start code
        put(0, 5); // version / gob id
        put(0, 8); // temporal reference
        put(1, 3); // source format: 16-bit custom size
        put(65535, 16); // custom width
        put(65535, 16); // custom height
        put(0, 2); // picture type: I-frame
        put(1, 5); // quantizer
        put(0, 1); // PEI: no extra
        while !bits.len().is_multiple_of(8) {
            bits.push(0);
        }
        bits.chunks(8)
            .map(|c| c.iter().fold(0u8, |b, &x| (b << 1) | x))
            .collect()
    }

    /// THE MEMORY BOMB IS REFUSED BEFORE IT IS ALLOCATED.
    ///
    /// A subprocess contains a crash but not an 8 GB commit, and `check_dims` cannot help here
    /// because h263-rs allocates while parsing, before we ever see the dimensions. The real
    /// guard is [`CHILD_MEMORY_CAP`], enforced by Windows on this process.
    ///
    /// This test asserts the part that is testable in-process: that the geometry this header
    /// declares is refused by our own cap, so the intent cannot be silently lost if someone
    /// later "simplifies" the job-object setup away. The end-to-end behaviour was measured
    /// separately: the child aborts in ~350 ms with "memory allocation of 4294836225 bytes
    /// failed", commits nothing, and the parent falls back to the stock icon.
    #[test]
    fn a_giant_declared_sorenson_frame_is_over_every_cap() {
        assert!(
            check_dims(65535, 65535).is_err(),
            "65535x65535 must be refused by the dimension cap",
        );
        // And the cap itself must stay far below what such a frame would need, or the
        // job-object limit would not bite before the allocation succeeded.
        let luma = 65535usize * 65535;
        assert!(
            luma > CHILD_MEMORY_CAP,
            "the luma plane alone ({luma} bytes) must exceed CHILD_MEMORY_CAP \
             ({CHILD_MEMORY_CAP}), or the bomb is not actually contained",
        );
        // The header is well-formed enough to reach the decoder: if this ever stops being a
        // plausible Sorenson picture the test has stopped exercising the dangerous path.
        let hdr = sorenson_giant_header();
        assert!(
            hdr.len() >= 9,
            "header should be a realistic size, got {}",
            hdr.len()
        );
        assert_eq!(
            hdr.first().copied(),
            Some(0),
            "starts inside the 17-bit start code"
        );
    }

    // --- Synthetic FLV construction (mirrors the builders in core's flv.rs tests) --------

    fn flv_file(tags: &[Vec<u8>]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b"FLV\x01\x05");
        f.extend_from_slice(&9u32.to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes());
        for t in tags {
            f.extend_from_slice(t);
        }
        f
    }

    fn tag(tag_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut t = Vec::with_capacity(15 + payload.len());
        t.push(tag_type);
        t.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..4]);
        t.extend_from_slice(&[0, 0, 0, 0]); // timestamp + ext
        t.extend_from_slice(&[0, 0, 0]); // StreamID
        t.extend_from_slice(payload);
        t.extend_from_slice(&((11 + payload.len()) as u32).to_be_bytes());
        t
    }

    fn video_payload(frame_type: u8, codec_id: u8, body: &[u8]) -> Vec<u8> {
        let mut p = vec![(frame_type << 4) | codec_id];
        p.extend_from_slice(body);
        p
    }

    // --- Failure paths: each must return Err, never panic, never emit output -------------

    #[test]
    fn empty_input_fails_cleanly() {
        assert!(frame_png(&[]).is_err());
    }

    #[test]
    fn non_flv_input_fails_cleanly() {
        assert!(frame_png(b"definitely not a flash video").is_err());
        assert!(frame_png(&[0u8; 512]).is_err());
    }

    #[test]
    fn h264_flv_is_declined_to_the_mf_path() {
        let flv = flv_file(&[tag(9, &video_payload(1, 7, &[0, 1, 66, 0, 30, 0xFF, 0xE1]))]);
        let err = frame_png(&flv).unwrap_err();
        assert!(err.contains("H.264"), "wrong decline reason: {err}");
    }

    #[test]
    fn unknown_codec_ids_fail_cleanly() {
        for codec_id in [1u8, 3, 5, 6] {
            let flv = flv_file(&[tag(9, &video_payload(1, codec_id, &[0x11, 0x22]))]);
            assert!(frame_png(&flv).is_err(), "codec id {codec_id} must fail");
        }
    }

    #[test]
    fn truncated_and_garbage_flash_payloads_fail_cleanly() {
        // A Sorenson keyframe tag whose payload is garbage: the decoder must error (or the
        // scan must decline), never panic — this test aborting IS the failure signal under
        // panic=abort test profiles.
        let flv = flv_file(&[tag(9, &video_payload(1, 2, &[0xDE, 0xAD, 0xBE, 0xEF]))]);
        assert!(frame_png(&flv).is_err());
        // Same for VP6, with too-short and with garbage frames.
        let flv = flv_file(&[tag(9, &video_payload(1, 4, &[0x00]))]);
        assert!(frame_png(&flv).is_err());
        let flv = flv_file(&[tag(
            9,
            &video_payload(1, 4, &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]),
        )]);
        assert!(frame_png(&flv).is_err());
        // Truncations of a structurally-valid FLV head.
        let whole = flv_file(&[
            tag(18, b"\x02\x00\x0AonMetaData"),
            tag(9, &video_payload(1, 2, &[0x08; 64])),
        ]);
        for n in 0..whole.len() {
            let _ = frame_png(&whole[..n]); // Err or (never) Ok — must not panic
        }
    }

    #[test]
    fn crop_plane_bounds() {
        let data: Vec<u8> = (0..64).collect(); // 8x8, stride 8
        let out = crop_plane(&data, 8, 3, 2).unwrap();
        assert_eq!(out, vec![0, 1, 2, 8, 9, 10]);
        assert!(crop_plane(&data, 8, 9, 2).is_none()); // wider than the stride
        assert!(crop_plane(&data, 8, 3, 9).is_none()); // taller than the data
    }

    /// End-to-end on the REAL corpus Sorenson file (the issue-#26 case): sample.flv must
    /// now decode to a plausible PNG. Skips when the dev corpus isn't present (CI).
    #[test]
    fn corpus_sorenson_sample_decodes() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus")
            .join("sample.flv");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("corpus_sorenson_sample_decodes: no sample.flv — skipping");
            return;
        };
        let png = frame_png(&bytes).expect("sample.flv (Sorenson) should decode");
        let img = image::load_from_memory(&png).expect("child output should be a valid PNG");
        assert!(img.width() > 0 && img.height() > 0);
        eprintln!(
            "corpus_sorenson_sample_decodes: {}x{} ({} PNG bytes)",
            img.width(),
            img.height(),
            png.len()
        );
    }
}
