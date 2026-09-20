//! FLV (Flash Video) → one-keyframe mini-MP4, for H.264 content only.
//!
//! Windows Media Foundation has no FLV demuxer, so an `.flv` never even opens through the
//! normal video tiers — but the H.264 *codec* inside most FLVs is the same inbox decoder
//! every MP4 uses. The bridge is small because FLV stores AVC exactly the way MP4 does:
//! the "AVC sequence header" video tag's payload IS an `AVCDecoderConfigurationRecord`
//! (byte-identical to an `avcC` box body), and each NALU video tag's payload is already
//! length-prefixed AVCC sample data. So: walk the tags, take the config + the first
//! keyframe, wrap the config in an `avc1` sample entry, and hand both to the pure muxer
//! [`crate::mp4::build_mini_mp4`] — the result decodes through
//! [`crate::video::frame_from_bytes`] like any other mini-MP4.
//!
//! Codec ids 2 (Sorenson Spark) and 4 (VP6) have no Windows decoder, but pure-Rust ones
//! exist (the Ruffle Flash codecs). Those decoders PANIC on malformed input and this crate
//! builds `panic = "abort"`, so they must never run inside Explorer: [`flash_frame`] spawns
//! the sibling `st2k.exe` with the FLV bytes on stdin and reads a PNG back — the crash, if
//! any, dies in a throwaway child (`src/bin/vdec`). Other ids still return `None` cleanly.
//!
//! This parses UNTRUSTED bytes in-process (`panic = "abort"` inside explorer.exe), hence:
//! no indexing, no unwraps, checked arithmetic on every file-supplied length, a strictly
//! forward tag walk with tag-count and byte caps, and capped allocations throughout.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;
mod sps;
pub use sps::Bits;
use sps::*;

use crate::mp4::{build_mini_mp4, bx, fbx, read_exact_at};

/// Give up after this many tags. A real pre-keyframe run is a metadata tag plus a handful
/// of audio tags — even a generous 10 s audio preroll is only a few hundred — so this stays
/// small on purpose: `keyframe_mini_mp4` issues at least one `read_exact_at` per tag over a
/// caller stream that may be a marshaled shell `IStream`, where each read is a synchronous
/// COM round trip. The old 200,000 let a tiny-tag-heavy FLV force on the order of 200k-400k
/// of those before the cap fired — not infinite, but a real hang inside the thumbnail host.
const MAX_TAGS: u32 = 4_096;
/// Never walk past this absolute offset looking for the config + first keyframe. Both live
/// near the head of any real FLV (the sequence header is the first video tag by spec).
const WALK_MAX: u64 = 256 * 1024 * 1024;
/// Sanity cap on the AVCDecoderConfigurationRecord (typically well under 100 bytes).
const CONFIG_MAX: usize = 64 * 1024;
/// Sanity cap on the keyframe sample. An FLV tag's DataSize field is 24-bit so the format
/// can't exceed 16 MiB anyway; this is belt-and-braces against a parsing slip.
const KEYFRAME_MAX: usize = 16 * 1024 * 1024;
/// Largest plausible FLV header DataOffset (spec value is 9; some writers pad slightly).
const HEADER_MAX: u64 = 4096;

/// Validates the FLV signature and reads `DataOffset`. `None` for anything that isn't a
/// well-formed FLV header, or a file too small to hold one plus a tag.
fn read_flv_header<R: Read + Seek>(r: &mut R, total: u64) -> Option<u64> {
    // Header (9) + PreviousTagSize0 (4) + one tag header (11) is the smallest useful file.
    if total < 24 {
        return None;
    }
    let mut hdr = [0u8; 9];
    read_exact_at(r, 0, &mut hdr)?;
    if &hdr[0..3] != b"FLV" {
        return None;
    }
    let data_offset = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as u64;
    if !(9..=HEADER_MAX).contains(&data_offset) {
        return None;
    }
    Some(data_offset)
}

/// What processing one tag inside [`keyframe_mini_mp4`]'s walk should do next.
enum TagOutcome {
    /// Not a usable video tag yet (or this one was the config, now captured) — keep walking.
    Continue,
    /// A keyframe tag was found: this is the whole function's answer, ready to return
    /// (`mux` itself can still fail, hence the inner `Option`).
    Return(Option<Vec<u8>>),
}

/// Inspect one FLV tag. VideoData: `FrameType`(4 bits) `CodecID`(4 bits), then for AVC:
/// `AVCPacketType`(1) `CompositionTime`(3), then the body. Non-video or too-short tags are
/// [`TagOutcome::Continue`]; a non-AVC codec aborts the whole parse (`None`) — Sorenson
/// Spark / VP6 get their frame via [`flash_frame`] instead, never remuxed here.
/// Capture `avc_config` from an AVC sequence-header tag's payload (the caller has already
/// confirmed this tag IS one: `packet_type == 0`, no config yet, non-empty body). `None` aborts
/// the whole parse: an oversized or malformed (`configurationVersion != 1`) record.
fn capture_avc_config<R: Read + Seek>(
    r: &mut R,
    payload_pos: u64,
    body_len: usize,
    avc_config: &mut Option<Vec<u8>>,
) -> Option<TagOutcome> {
    // AVC sequence header: the payload is an AVCDecoderConfigurationRecord.
    if body_len > CONFIG_MAX {
        return None;
    }
    let mut cfg = vec![0u8; body_len];
    read_exact_at(r, payload_pos.checked_add(5)?, &mut cfg)?;
    if cfg.first() != Some(&1) || cfg.len() < 7 {
        return None; // configurationVersion must be 1
    }
    *avc_config = Some(cfg);
    Some(TagOutcome::Continue)
}

/// Read and mux a keyframe NALU tag's payload (the caller has already confirmed this tag IS
/// one: config seen, AVC packet type 1, frame type 1). `None` aborts the whole parse (an
/// oversized sample or a read failure).
fn mux_keyframe_tag<R: Read + Seek>(
    r: &mut R,
    payload_pos: u64,
    body_len: usize,
    cfg: &[u8],
) -> Option<TagOutcome> {
    if body_len > KEYFRAME_MAX {
        return None;
    }
    let mut keyframe = vec![0u8; body_len];
    read_exact_at(r, payload_pos.checked_add(5)?, &mut keyframe)?;
    Some(TagOutcome::Return(mux(cfg, &keyframe)))
}

fn handle_video_tag<R: Read + Seek>(
    r: &mut R,
    tag_type: u8,
    data_size: u64,
    payload_pos: u64,
    avc_config: &mut Option<Vec<u8>>,
) -> Option<TagOutcome> {
    if tag_type != 9 || data_size < 2 {
        return Some(TagOutcome::Continue);
    }
    let mut vh = [0u8; 2];
    read_exact_at(r, payload_pos, &mut vh)?;
    let frame_type = vh[0] >> 4;
    let codec_id = vh[0] & 0x0F;
    if codec_id != 7 {
        return None;
    }
    let packet_type = vh[1];
    if data_size < 5 {
        return Some(TagOutcome::Continue);
    }
    let body_len = (data_size - 5) as usize;
    if packet_type == 0 && avc_config.is_none() && body_len > 0 {
        return capture_avc_config(r, payload_pos, body_len, avc_config);
    }
    if packet_type == 1 && frame_type == 1 && body_len > 0 {
        // A keyframe NALU tag — usable once the config has been seen.
        if let Some(cfg) = avc_config.as_ref() {
            return mux_keyframe_tag(r, payload_pos, body_len, cfg);
        }
    }
    Some(TagOutcome::Continue)
}

/// Build a one-keyframe mini-MP4 from an FLV whose video codec is H.264. Returns the
/// mini-MP4 bytes for [`crate::video::frame_from_bytes`], or `None` for non-FLV input,
/// non-AVC codecs, or any malformed/truncated structure (caller falls through).
///
/// Unlike the MP4/MKV twins this takes no time fraction: FLV has no sample index to seek
/// (`onMetaData` keyframe tables are optional and untrustworthy), so the representative
/// frame is the FIRST keyframe after the sequence header — a targeted read of the file
/// head, never a whole-file scan.
pub fn keyframe_mini_mp4<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let total = r.seek(SeekFrom::End(0)).ok()?;
    let data_offset = read_flv_header(r, total)?;
    // `walk_avc_tags` returns `None` for a broken walk and `Some(v)` once a keyframe tag was
    // seen, where `v` is the mux result itself (`None` if muxing failed).
    walk_avc_tags(r, total, data_offset.checked_add(4)?)?
}

/// Walk the FLV from `start` (past PreviousTagSize0) for the AVC config + first keyframe.
/// `Some(v)` once a keyframe tag arrives (`v` is the mux result), `None` when the walk ends
/// or a cap/read/bound fails.
fn walk_avc_tags<R: Read + Seek>(r: &mut R, total: u64, start: u64) -> Option<Option<Vec<u8>>> {
    let mut avc_config: Option<Vec<u8>> = None;
    let mut pos = start;
    let mut tags = 0u32;
    // Tag layout: type(1) DataSize(3) Timestamp(3) TimestampExt(1) StreamID(3) then
    // Data[DataSize] then PreviousTagSize(4). The advance is 11 + DataSize + 4 ≥ 15, so the
    // walk is STRUCTURALLY forward-only — a hostile DataSize can overshoot (caught by the
    // bounds check) but never stall or rewind. The caps bound a long crafted crawl anyway.
    while pos.checked_add(11)? <= total {
        tags = tags.checked_add(1)?;
        if tags > MAX_TAGS || pos > WALK_MAX {
            return None;
        }
        let (tag_type, payload_pos, data_size) = read_stream_tag_header(r, pos, total)?;
        match handle_video_tag(r, tag_type, data_size, payload_pos, &mut avc_config)? {
            TagOutcome::Continue => {}
            TagOutcome::Return(v) => return Some(v),
        }
        pos = payload_pos.checked_add(data_size)?.checked_add(4)?;
    }
    None
}

/// Read one tag's fixed 11-byte header from the stream at `pos`: `(tag_type, payload offset,
/// payload length)`, or `None` for a read failure or a payload that runs past `total`
/// (truncated mid-tag).
fn read_stream_tag_header<R: Read + Seek>(
    r: &mut R,
    pos: u64,
    total: u64,
) -> Option<(u8, u64, u64)> {
    let mut th = [0u8; 11];
    read_exact_at(r, pos, &mut th)?;
    let tag_type = th[0] & 0x1F; // top bits: reserved + encryption filter
    let data_size = u32::from_be_bytes([0, th[1], th[2], th[3]]) as u64;
    let payload_pos = pos.checked_add(11)?;
    if payload_pos.checked_add(data_size)? > total {
        return None; // truncated mid-tag
    }
    Some((tag_type, payload_pos, data_size))
}

/// Wrap the AVC config + keyframe sample in a mini-MP4. The `stsd` is synthesized (an
/// `avc1` visual sample entry carrying the config verbatim as its `avcC` box); dimensions
/// come from the SPS inside the config. FLV timestamps are milliseconds, so timescale 1000
/// with a nominal 40 ms (25 fps) frame duration — the value only shapes the mvhd/stts of a
/// one-frame movie, it does not affect decoding.
fn mux(avc_config: &[u8], keyframe: &[u8]) -> Option<Vec<u8>> {
    let (width, height) = sps_dims(avc_config)?;
    let stsd = build_stsd(avc_config, width, height);
    Some(build_mini_mp4(
        None, &stsd, 1, 40, 1000, width, height, keyframe,
    ))
}

/// An MP4 `stsd` full box holding one `avc1` VisualSampleEntry whose `avcC` payload is the
/// FLV's AVCDecoderConfigurationRecord, verbatim.
fn build_stsd(avc_config: &[u8], width: u16, height: u16) -> Vec<u8> {
    let avcc = bx(b"avcC", avc_config);
    // VisualSampleEntry (ISO 14496-12 §12.1.3): reserved(6) data_ref_index(2) pre_defined(2)
    // reserved(2) pre_defined(12) width(2) height(2) hres(4) vres(4) reserved(4)
    // frame_count(2) compressorname(32) depth(2) pre_defined(2), then the codec box.
    let mut vse = Vec::with_capacity(78 + avcc.len());
    vse.extend_from_slice(&[0u8; 6]);
    vse.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    vse.extend_from_slice(&[0u8; 16]); // pre_defined + reserved + pre_defined[3]
    vse.extend_from_slice(&width.to_be_bytes());
    vse.extend_from_slice(&height.to_be_bytes());
    vse.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi horizontal
    vse.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi vertical
    vse.extend_from_slice(&[0u8; 4]); // reserved
    vse.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    vse.extend_from_slice(&[0u8; 32]); // compressorname
    vse.extend_from_slice(&24u16.to_be_bytes()); // depth
    vse.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
    vse.extend_from_slice(&avcc);
    let avc1 = bx(b"avc1", &vse);
    let mut body = 1u32.to_be_bytes().to_vec(); // entry_count
    body.extend_from_slice(&avc1);
    fbx(b"stsd", 0, 0, &body)
}

// ---------------------------------------------------------------------------------------------
// VP6 / Sorenson Spark (codec ids 4 / 2): decoded OUT OF PROCESS by the sibling st2k.exe
// ---------------------------------------------------------------------------------------------

/// Cap on the FLV bytes handed to the `st2k flv-frame` child (and on what that child will
/// accept from stdin — the two ends share this constant). The config-free Flash codecs put
/// their first keyframe at the head of the file, so a prefix this large is generous.
pub const FLASH_INPUT_CAP: usize = 32 * 1024 * 1024;
/// Largest frame edge the child will decode (and the parent will accept back). FLV predates
/// HD; VP6's coded size is 8-bit macroblock counts (≤4080 px) and Sorenson's custom format
/// is 16-bit, so anything past this is a crafted file, not a video.
pub const FLASH_MAX_DIM: usize = 4096;
/// CPU budget for one child decode (one keyframe of a ≤4096px pre-2010 codec is far under a
/// second; this is pure headroom), plus the elapsed backstop for a child that hangs without
/// burning CPU on a loaded machine — the same split the ImageMagick watchdog uses.
const FLASH_CPU_BUDGET: Duration = Duration::from_secs(10);
const FLASH_WALL_CEILING: Duration = Duration::from_secs(30);
/// Cap on the PNG read back from the child. A 4096×4096 RGBA frame is ~64 MiB raw, and PNG
/// only shrinks that; more means a broken or hostile child.
const FLASH_PNG_CAP: usize = 64 * 1024 * 1024;

/// The two Flash-era FLV codecs the `st2k flv-frame` child can decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlashCodec {
    /// Codec id 2 — Sorenson Spark, the Flash flavour of H.263.
    Sorenson,
    /// Codec id 4 — On2 VP6.
    Vp6,
}

/// Outcome of scanning an in-memory FLV for its first Flash-codec keyframe. This is the
/// ONE walk both sides of the process boundary use: the `st2k flv-frame` child extracts the
/// keyframe payload with it, and the tests pin its behaviour, so the parent's spawn
/// decision and the child's parse can't drift apart.
pub enum FlashScan<'a> {
    /// A codec 2/4 keyframe: the payload with the 1-byte FrameType/CodecID header stripped
    /// (for VP6 the leading adjustment byte is still present — the decoder consumes it).
    Keyframe(FlashCodec, &'a [u8]),
    /// The first video tag uses some other codec (7 = H.264, which the MF remux path owns).
    OtherCodec(u8),
    /// Not an FLV, truncated, or no usable video keyframe within the caps.
    NoVideo,
}

/// Walk an in-memory FLV for the first VP6/Sorenson KEYFRAME tag. Same tag layout and caps
/// as [`keyframe_mini_mp4`]'s walk, over a slice because the child holds the whole (capped)
/// input in memory. The first video tag decides the codec — a file that switches codecs
/// mid-stream is not a real FLV.
pub fn scan_flash_keyframe(flv: &[u8]) -> FlashScan<'_> {
    let Some(codec) = slice_walk(flv, &mut |frame_type, codec_id, payload| {
        let codec = match codec_id {
            2 => FlashCodec::Sorenson,
            4 => FlashCodec::Vp6,
            other => return SliceWalk::Stop(FlashScan::OtherCodec(other)),
        };
        if frame_type == 1 && !payload.is_empty() {
            SliceWalk::Stop(FlashScan::Keyframe(codec, payload))
        } else {
            SliceWalk::Continue // an inter frame before the first keyframe — keep looking
        }
    }) else {
        return FlashScan::NoVideo;
    };
    codec
}

/// The codec id of the first video tag (2 = Sorenson, 4 = VP6, 7 = H.264, …), or `None`
/// for a non-FLV / truncated / video-less input. Powers [`crate::vcodec::identify`]'s FLV
/// arm and [`flash_frame`]'s cheap pre-spawn gate.
pub(crate) fn video_codec_id<R: Read + Seek>(r: &mut R) -> Option<u8> {
    // A bounded head read is cheaper and simpler than a second Read+Seek walk: the first
    // video tag sits within the first few tags of any real FLV (metadata + audio headers).
    const PROBE_CAP: usize = 4 * 1024 * 1024;
    let total = r.seek(SeekFrom::End(0)).ok()?;
    r.seek(SeekFrom::Start(0)).ok()?;
    let take = total.min(PROBE_CAP as u64);
    let mut head = Vec::new();
    r.take(take).read_to_end(&mut head).ok()?;
    slice_walk(&head, &mut |_frame_type, codec_id, _payload| {
        SliceWalk::Stop(codec_id)
    })
}

/// What a [`slice_walk`] visitor wants next.
enum SliceWalk<T> {
    Stop(T),
    Continue,
}

/// Shared forward-only FLV tag walk over a slice: calls `visit` for every VIDEO tag with
/// `(frame_type, codec_id, payload-after-the-header-byte)` until it returns `Stop` or the
/// caps run out. Checked arithmetic throughout — this runs in-process on untrusted bytes.
fn slice_walk<'a, T>(
    flv: &'a [u8],
    visit: &mut dyn FnMut(u8, u8, &'a [u8]) -> SliceWalk<T>,
) -> Option<T> {
    let total = flv.len() as u64;
    let mut pos = flv_first_tag_pos(flv)?;
    let mut tags = 0u32;
    while pos.checked_add(11)? <= total {
        tags = tags.checked_add(1)?;
        // Same pair of caps `keyframe_mini_mp4` applies. Currently redundant in practice —
        // every caller already bounds `flv` well under WALK_MAX (32 MiB/4 MiB vs 256 MiB) —
        // but keeping both walks on the same two caps means a future caller can't silently
        // inherit only half the bound.
        if tags > MAX_TAGS || pos > WALK_MAX {
            return None;
        }
        let (tag_type, payload_pos, data_size) = read_tag_header(flv, pos, total)?;
        // Outer `None` aborts the walk (malformed payload), inner `Some` stops it.
        if let Some(out) = visit_video_tag(flv, tag_type, payload_pos, data_size, visit)? {
            return Some(out);
        }
        pos = payload_pos.checked_add(data_size)?.checked_add(4)?;
    }
    None
}

/// Hand one tag's video payload to `visit`. `Some(None)` for a non-video/too-short tag or a
/// visitor that kept going, `Some(Some(T))` when the visitor stopped, `None` if the payload
/// slice is out of range (which aborts the walk).
fn visit_video_tag<'a, T>(
    flv: &'a [u8],
    tag_type: u8,
    payload_pos: u64,
    data_size: u64,
    visit: &mut dyn FnMut(u8, u8, &'a [u8]) -> SliceWalk<T>,
) -> Option<Option<T>> {
    if tag_type != 9 || data_size < 2 {
        return Some(None);
    }
    let payload = flv.get(payload_pos as usize..(payload_pos + data_size) as usize)?;
    let (frame_type, codec_id) = (payload[0] >> 4, payload[0] & 0x0F);
    if let SliceWalk::Stop(out) = visit(frame_type, codec_id, &payload[1..]) {
        return Some(Some(out));
    }
    Some(None)
}

/// Validate the FLV header and return the byte offset of the first tag (past
/// PreviousTagSize0), or None for a non-FLV / malformed header.
fn flv_first_tag_pos(flv: &[u8]) -> Option<u64> {
    if flv.len() < 24 || &flv[0..3] != b"FLV" {
        return None;
    }
    let data_offset = u32::from_be_bytes([flv[5], flv[6], flv[7], flv[8]]) as u64;
    if !(9..=HEADER_MAX).contains(&data_offset) {
        return None;
    }
    data_offset.checked_add(4)
}

/// Read one tag's fixed 11-byte header at `pos`: `(tag_type, payload offset, payload
/// length)`, or None for an out-of-range read or a payload that runs past `total`
/// (truncated mid-tag).
fn read_tag_header(flv: &[u8], pos: u64, total: u64) -> Option<(u8, u64, u64)> {
    let th = flv.get(pos as usize..pos as usize + 11)?;
    let tag_type = th[0] & 0x1F;
    let data_size = u32::from_be_bytes([0, th[1], th[2], th[3]]) as u64;
    let payload_pos = pos.checked_add(11)?;
    if payload_pos.checked_add(data_size)? > total {
        return None; // truncated mid-tag
    }
    Some((tag_type, payload_pos, data_size))
}

/// Decode the first VP6/Sorenson keyframe of an FLV to a frame — OUT OF PROCESS.
///
/// The pure-Rust decoders for these codecs (nihav / h263-rs, the Ruffle Flash codecs) panic
/// on malformed input, and under `panic = "abort"` a panic here would kill the user's
/// Explorer, so they are linked ONLY into `st2k.exe` (the EXE-only `flash-video` feature).
/// This spawns that sibling with the FLV bytes on stdin — no temp file ever holds a frame
/// of the user's video — and reads the PNG back, on the same CPU+wall watchdog and the same
/// cross-process concurrency gate the ImageMagick tier uses (a folder of 500 FLVs must not
/// spawn 500 children). Any failure — no sibling exe (DLL-only or feature-less build),
/// hostile input, a child crash/abort, over-budget — is a clean `None`, exactly the
/// pre-support behaviour.
pub(crate) fn flash_frame<R: Read + Seek>(r: &mut R) -> Option<image::DynamicImage> {
    if !matches!(video_codec_id(r), Some(2) | Some(4)) {
        return None; // not an FLV, or not a codec we self-decode — nothing to spawn for
    }
    let total = r.seek(SeekFrom::End(0)).ok()?;
    r.seek(SeekFrom::Start(0)).ok()?;
    let mut flv = Vec::new();
    r.take(total.min(FLASH_INPUT_CAP as u64))
        .read_to_end(&mut flv)
        .ok()?;
    let png = child_frame_png(
        "flv-frame",
        &flv,
        FLASH_CPU_BUDGET,
        FLASH_WALL_CEILING,
        FLASH_PNG_CAP,
    )?;
    // Bounded parse of OUR OWN child's output: the PNG is size-capped above and the child
    // caps its frame at FLASH_MAX_DIM², so this in-process decode is small by construction;
    // the dimension re-check makes that a verified property, not an assumption.
    let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).ok()?;
    if img.width() == 0
        || img.height() == 0
        || img.width() as usize > FLASH_MAX_DIM
        || img.height() as usize > FLASH_MAX_DIM
    {
        crate::safety::log_debug("flv flash decode: child returned out-of-bounds dimensions");
        return None;
    }
    Some(img)
}

/// Run a hidden `st2k <verb>` decode child with `input` on stdin, returning its PNG stdout.
/// Shared by the FLV Flash-codec tier here and the VP9 Profile 2/3 tier (`crate::vp9`), so
/// the two out-of-process decoders can't drift on the containment mechanics. Mirrors the
/// ImageMagick child harness in `decode/magick.rs`: stdin fed from its own thread, stdout
/// read on another, a CPU-budget watchdog with a wall-clock backstop, an unconditional
/// kill before the joins so a wedged child can't hang the (shell-hosted) caller, and the
/// shared cross-process gate bounding concurrent children.
pub(crate) fn child_frame_png(
    verb: &'static str,
    input: &[u8],
    cpu_budget: Duration,
    wall_ceiling: Duration,
    png_cap: usize,
) -> Option<Vec<u8>> {
    let exe = crate::sibling_of_dll(crate::CLI_EXE)?;
    let mut cmd = Command::new(exe);
    cmd.arg(verb)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // the child logs its own failures via the panic hook/log
        .creation_flags(crate::CREATE_NO_WINDOW);
    // Bound concurrent decode children (the ImageMagick gate is cross-process and named, so
    // st2k fan-outs and in-process decodes share the one cap).
    let _permit = crate::decode::magick_gate::acquire_for(crate::decode::Fidelity::Tile);
    let mut child = cmd.spawn().ok()?;
    // The one line `verify-installed-thumbnails-explorer.ps1` counts. It used to count
    // helpers by polling the process list every 15 ms from a PowerShell runspace, and a
    // helper that lived and died between two polls on a loaded box read as "expected 1,
    // saw 0 - the decode tier ordering changed" (2026-09-09, a false red that cost an hour).
    // A line written by the process that did the spawning cannot be missed by a scheduler.
    // Debug-gated, so the production path costs one cached registry flag read.
    crate::safety::log_debugf!("spawned helper pid {} for {verb}", child.id());

    // Feed stdin on its own thread so a full stdout pipe can't deadlock us.
    let mut stdin = child.stdin.take()?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        // drop(stdin) closes the pipe so the child sees EOF
    });
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::take(&mut stdout, (png_cap + 1) as u64).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let png = crate::decode::await_child_output(&mut child, &rx, cpu_budget, wall_ceiling);
    // Kill unconditionally (no-op if exited): a child that closed stdout but stopped
    // draining stdin would otherwise block writer.join() forever.
    let _ = child.kill();
    let _ = writer.join();
    let _ = reader.join();
    let status = child.wait().ok();
    match png {
        Ok(png) if !png.is_empty() && png.len() <= png_cap => Some(png),
        Ok(_) => {
            crate::safety::log_debugf!(
                "{verb} decode: child produced no/oversized output (status {status:?})"
            );
            None
        }
        Err(why) => {
            crate::safety::log_debugf!("{verb} decode: {why} (status {status:?})");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// SPS geometry
// ---------------------------------------------------------------------------------------------

/// Coded width/height from the first SPS in an AVCDecoderConfigurationRecord.
/// Record layout: version(1) profile(1) compat(1) level(1) 0b111111·lengthSizeMinusOne(1)
/// 0b111·numSPS(1), then per SPS: length(2) + the NAL (header byte + RBSP with emulation-
/// prevention bytes).
fn sps_dims(cfg: &[u8]) -> Option<(u16, u16)> {
    if cfg.first() != Some(&1) {
        return None;
    }
    let num_sps = cfg.get(5)? & 0x1F;
    if num_sps == 0 {
        return None;
    }
    let len = u16::from_be_bytes([*cfg.get(6)?, *cfg.get(7)?]) as usize;
    let sps = cfg.get(8..8usize.checked_add(len)?)?;
    let (nal_header, payload) = sps.split_first()?;
    if nal_header & 0x1F != 7 {
        return None; // not a seq_parameter_set NAL
    }
    let rbsp = strip_emulation_prevention(payload);
    parse_sps(&rbsp)
}

/// Direct fuzz entry points into the H.264 bitstream readers. Test-only.
///
/// The container walk above rejects a tag whose sizes stop adding up, so a mutation to an FLV
/// only reaches the SPS reader if it happened to leave every tag length intact — which filters
/// out most of the mutations worth trying against Exp-Golomb code. These hand the bit readers
/// their own bytes. Same argument as `crate::container::apk::fuzzapi`.
#[cfg(test)]
pub(crate) mod fuzzapi {
    /// The AVCDecoderConfigurationRecord geometry reader (the whole record).
    pub(crate) fn sps_dims(cfg: &[u8]) {
        let _ = super::sps_dims(cfg);
    }

    /// The SPS itself: unbounded-width Exp-Golomb over attacker-controlled bits, plus the
    /// scaling-list walk and the frame-cropping arithmetic behind it.
    pub(crate) fn parse_sps(rbsp: &[u8]) {
        let _ = super::parse_sps(rbsp);
    }

    /// The emulation-prevention unescaper, which sizes an allocation from the input.
    pub(crate) fn strip_emulation_prevention(b: &[u8]) {
        let _ = super::strip_emulation_prevention(b);
    }

    /// The same geometry read, RETURNING its answer, so the fuzz seed self-check can assert
    /// the `h264-avcc` seed still parses to its known dimensions instead of merely not
    /// panicking. (The target above discards, deliberately: it asserts robustness.)
    pub(crate) fn sps_dims_ret(cfg: &[u8]) -> Option<(u16, u16)> {
        super::sps_dims(cfg)
    }
}

#[cfg(test)]
mod tests;
