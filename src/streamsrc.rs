//! Shared shell-`IStream` source acquisition for the thumbnail + preview handlers.
//!
//! Both handlers receive the same kind of shell `IStream` and need the same
//! "get me something decodable WITHOUT buffering a multi-GB file" cascade:
//! video frame-grab tiers, seek-only audio album art, streamed archive covers,
//! the head-preview prefix rescue, and the bounded whole-file read. This module
//! owns that cascade ([`stream_source`]) plus the low-level `IStream` helpers
//! it is built from, so the two handlers can't drift apart. Everything here
//! runs on the CALLING (COM apartment) thread — the marshaled stream is
//! apartment-bound and must not be touched from a worker.

use core::ffi::c_void;

use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Com::{
    CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATFLAG_NONAME, STATSTG, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};

use crate::{decode, safety};

mod archive;
mod headprev;
mod mp4remux;
mod rawsniff;

// Parent-hub imports: the children are glob-imported PRIVATELY so the cascade below
// still reads as one flat namespace (and so each child's own `use super::*` sees the
// shared IStream helpers). Nothing here is part of the crate's public surface except
// `stream_source` / `StreamSource`, which stay in this file.
use archive::*;
use headprev::*;
use mp4remux::*;
use rawsniff::*;

// The whole-file read ceiling, shared with the path-reading verbs via
// `decode::limits::MAX_INPUT_BYTES` (one DoS budget, not two copies).
const MAX_BYTES: usize = decode::limits::MAX_INPUT_BYTES as usize;

/// What [`stream_source`] hands back: a video frame Media Foundation already
/// decoded (no bytes to re-decode), bounded raw bytes for the caller's tiered
/// byte decoder, or a generic archive's picked cover images for the
/// contact-sheet compositor (`decode::thumbnail_from_covers`).
pub enum StreamSource {
    Frame(image::DynamicImage),
    Bytes(Vec<u8>),
    Covers(Vec<Vec<u8>>),
}

/// Turn the shell's `IStream` into a decodable source without ever buffering an
/// unbounded file. `who` prefixes the debug-log lines ("GetThumbnail" /
/// "DoPreview"); `max_file_bytes` is the user's MaxSize cap. Purpose-built
/// previews may sidestep it when their cost is inherently small (one video frame,
/// album art, or a comic's declared cover); generic ZIP/RAR/7z contact sheets
/// honor it because discovering pictures in an arbitrary project archive can
/// itself be expensive.
///
/// The cascade, in order:
/// 1. VIDEO — an MP4/MOV/M4V video shares the ISO-BMFF `ftyp` box with M4A/M4B
///    audio, so the audio probe below would otherwise claim it and, finding no
///    cover art, bail before the frame-grab ever ran (every .mp4 got a blank
///    icon, then the shell cached that failure forever). The `is_video_magic`
///    sniff keys off the container brand, so it tells a video file from audio /
///    HEIC. If it IS video but no tier decodes a frame, stop (default icon /
///    blank pane) rather than buffering the whole file only to fail decoding it
///    as an image — EXCEPT ambiguous OggS, which falls through to the audio path.
/// 2. AUDIO — the album art lives in the metadata, so seek straight to it and
///    read ONLY the art (not the whole file). Sidesteps the size cap AND avoids
///    buffering; artless audio stops here (raw audio bytes are not a decodable
///    image, a full read + decode would just burn time and fail).
/// 3. OPENEXR — decoded straight off the stream at the target edge, never
///    buffered (a 12K render pass is hundreds of MB of input and gigabytes of
///    float pixels; both caps refuse it, so it used to get the stock icon).
/// 4. OVERSIZED (past the cap) — streamed container cover (CBZ central
///    directory + one entry; Clip Studio `.clip` tail database) or the
///    head-preview prefix rescue (.blend/PSD-PSB baked previews sit in the
///    first bytes); otherwise skip.
/// 5. Everything else: bounded whole-file read.
///
/// `target_edge` is the caller's requested output edge; only the streaming EXR
/// tier consumes it (a smaller target lets it skip more of the file).
pub unsafe fn stream_source(
    stream: &IStream,
    max_file_bytes: u64,
    target_edge: u32,
    who: &str,
) -> Result<StreamSource> {
    stream_source_with_caps(
        stream,
        max_file_bytes,
        decode::limits::MAX_INPUT_BYTES,
        target_edge,
        who,
    )
}

/// [`stream_source`] with the HARD buffering ceiling as a parameter, so tests can drive the
/// oversized branch without staging a multi-hundred-megabyte fixture. Mirrors the by-path
/// twin's `read_preview_capped_at`. Production always passes
/// [`decode::limits::MAX_INPUT_BYTES`].
pub(crate) unsafe fn stream_source_with_caps(
    stream: &IStream,
    max_file_bytes: u64,
    hard_cap: u64,
    target_edge: u32,
    who: &str,
) -> Result<StreamSource> {
    if peek_is_video(stream) {
        // OPTION: prefer the embedded poster over a frame from the film (`VideoCoverArt`,
        // off by default). Read BEFORE the frame tiers so a film library shows its covers
        // without paying for a decode first; a file with no cover falls straight through to
        // the normal cascade, having cost one bounded metadata read.
        //
        // `tried_cover_art` remembers whether this pass ran: if it did and found nothing,
        // the fallback rescue below (after every frame tier also fails) must not call
        // `vcodec::cover_art` a second time — the stream hasn't changed, so it would just
        // re-scan the same moov to the same null answer. That third full scan (cover_art,
        // keyframe_mini_mp4, cover_art again) was the actual A032 cost on a HEVC-with-no-
        // cover-and-no-OS-codec file.
        let mut tried_cover_art = false;
        if crate::settings::prefer_cover_art() {
            tried_cover_art = true;
            if let Some(cover) = crate::vcodec::cover_art(&mut IStreamReader {
                stream: stream.clone(),
            }) {
                safety::log_debug(&format!(
                    "{who}: cover art preferred over a frame ({} bytes)",
                    cover.len()
                ));
                return Ok(StreamSource::Bytes(cover));
            }
        }
        // Never stream the multi-GB original through the shell IStream: Media Foundation
        // reading a whole movie that way is catastrophically slow (30 s+, a pegged core, past
        // Explorer's timeout). Every tier below is a TARGETED read instead: find the one
        // keyframe worth showing and touch only the bytes around it.
        //
        // There used to be a tier 0 here, "decode by FILE PATH when we can recover it". It
        // was REMOVED (2026-08-12) because it never ran: both handlers are initialised with
        // an `IStream`, and a shell stream reports only a bare leaf NAME, so the path lookup
        // it depended on always returned nothing. A documented optimisation that silently
        // does not exist is worse than none, because it makes the tiers below look like a
        // fallback nobody needs to keep fast. They are the whole story. Measured after
        // removal, through the real provider on a 163 MB 1080p clip: 1.2 s with the index at
        // the END and 1.7 s with it at the front, in a DEBUG build.
        // WHERE in the video: the user's `VideoOffset` (30 % unless changed — see
        // `settings::video_offset_frac`). Read ONCE here so every tier below seeks to the same
        // mark; a per-tier read could disagree if the setting changed mid-decode, and the
        // fallbacks would then show a different frame from the tier that was meant to run.
        let at = crate::settings::video_offset_frac();
        // Tiers, each fast or a fast miss:
        //   1. SMART TARGETED READ (MP4/MOV): parse the moov index, build a tiny
        //      one-keyframe MP4 for the sync sample nearest the mark, decode that —
        //      single-digit MB (index + one keyframe), a representative frame, and
        //      it works regardless of moov position (faststart or moov-at-end);
        //   2. SMART TARGETED READ (Matroska/WebM): the EBML analog — read the Cues
        //      index, build a tiny one-cluster MKV for the keyframe nearest the mark;
        //  2b. FLV (H.264 only): walk the head tags for the AVC config + first keyframe
        //      and remux them into a mini-MP4 — MF has no FLV demuxer, so no later tier
        //      can open the container at all (it has no index, so `at` can't be honoured);
        //  2c. FLV (VP6 / Sorenson Spark): decode the first keyframe OUT OF PROCESS via
        //      the sibling st2k.exe — Windows has no decoder for these at all;
        //   3. GENERAL targeted read (AVI/WMV/… + any unmapped MP4/MKV): let MF's own
        //      demuxer seek the real index over a block-caching IStream that
        //      coalesces its reads (no per-format parser, any container MF decodes);
        //   4. a faststart MP4 / small / unindexed video decodes from its head prefix;
        //   5. a big *non*-faststart MP4 (moov at the very end) is remuxed —
        //      head frames + tail moov stitched into a small valid MP4.
        // Tiers 4–5 stay as fallbacks for anything tier 3's demuxer can't seek. They read a
        // bounded head prefix, so they CANNOT honour a late offset — there are no bytes there
        // to seek into. That is a property of the fallback, not a bug to fix here.
        let frame = crate::mp4::keyframe_mini_mp4(
            &mut IStreamReader {
                stream: stream.clone(),
            },
            at,
        )
        .and_then(crate::video::frame_from_owned_bytes)
        .or_else(|| {
            crate::mkv::keyframe_mini_mkv(
                &mut IStreamReader {
                    stream: stream.clone(),
                },
                at,
            )
            .and_then(crate::video::frame_from_owned_bytes)
        })
        .or_else(|| {
            crate::flv::keyframe_mini_mp4(&mut IStreamReader {
                stream: stream.clone(),
            })
            .and_then(crate::video::frame_from_owned_bytes)
        })
        .or_else(|| {
            // 2c. FLV, VP6/Sorenson (issue #26): no Windows decoder exists, so the frame is
            //     decoded out of process by the sibling st2k.exe (`flv::flash_frame` — the
            //     pure-Rust Flash decoders panic on hostile input and must never run inside
            //     the shell host). Self-gated on the FLV magic + codec id; bounded head read.
            crate::flv::flash_frame(&mut IStreamReader {
                stream: stream.clone(),
            })
        })
        .or_else(|| {
            // MF demuxes AVI/WMV/etc. directly; the block-caching stream makes its
            // seek reads cheap (the old shell-IStream meltdown was thousands
            // of tiny marshaled reads — here they coalesce into a few big ones).
            stream_size(stream)
                .and_then(|size| crate::video::frame_from_block_stream(stream, size, at))
        })
        .or_else(|| video_prefix(stream).and_then(crate::video::frame_from_owned_bytes))
        .or_else(|| mp4_remux_moov(stream).and_then(crate::video::frame_from_owned_bytes))
        .or_else(|| {
            // 6. VP9 Profile 2/3 (10/12-bit HDR, issue #26): MF's VP9 decoder stops at
            //    Profile 0/1 even with the Store extension installed, so when every tier
            //    above came back empty AND the container says V_VP9, the keyframe (located
            //    via the same Cues read as tier 2) is decoded out of process by the sibling
            //    st2k.exe (`crate::vp9`). Deliberately LAST: Profile 0 must keep hitting the
            //    hardware-accelerated in-process MF path, and only otherwise-blank tiles pay
            //    for a process spawn. Self-gated on the codec id; bounded targeted reads.
            crate::vp9::vp9_frame(
                &mut IStreamReader {
                    stream: stream.clone(),
                },
                at,
            )
        });
        if let Some(frame) = frame {
            safety::log_debug(&format!(
                "{who}: video frame {}x{}",
                frame.width(),
                frame.height()
            ));
            return Ok(StreamSource::Frame(frame));
        }
        // No decodable frame. OggS is ambiguous — an audio-only .ogg/.opus matches
        // the video magic too, so fall THROUGH to the album-art path below instead of
        // failing. A genuine video container the OS can't decode stops here — after one
        // last rescue: Matroska attached cover art. The usual reason NO tier decoded is
        // a missing OS codec (HEVC/AV1 are Store add-ons, not inbox), and library rips
        // routinely attach a poster, so show the film instead of a blank tile. Bounded:
        // the attachment element is read via the container's own index, never the stream.
        if !peek_is_ogg(stream) {
            if needs_fallback_cover_art(tried_cover_art) {
                if let Some(cover) = crate::vcodec::cover_art(&mut IStreamReader {
                    stream: stream.clone(),
                }) {
                    safety::log_debug(&format!(
                        "{who}: video frame undecodable — using attached cover art ({} bytes)",
                        cover.len()
                    ));
                    return Ok(StreamSource::Bytes(cover));
                }
            }
            safety::log_debug(&format!("{who}: video with no decodable frame"));
            return Err(Error::from(E_FAIL));
        }
        safety::log_debug(&format!("{who}: OggS not video — trying album art"));
    }

    match audio_art(stream) {
        AudioArt::Art(art) => return Ok(StreamSource::Bytes(art)),
        AudioArt::NoArt => {
            safety::log_debug(&format!("{who}: audio file has no embedded art"));
            return Err(Error::from(E_FAIL));
        }
        AudioArt::NotAudio => {}
    }

    // OPENEXR — decode scaled straight off the (seekable) stream. A 12K VFX render
    // pass is hundreds of MB on disk, so the bounded whole-file read below refuses
    // it outright and the file gets the stock icon; and even well under the cap,
    // the `image` tier's full-resolution float decode costs orders of magnitude
    // more memory and time than the tile we were asked for. Files the scaled
    // decoder declines (deep, chroma-subsampled, non-RGB channel names) fall
    // through to the unchanged cascade below.
    if peek_is_exr(stream) {
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);
        let source = IStreamReader {
            stream: stream.clone(),
        };
        match decode::exr_scaled_from_reader(source, target_edge) {
            Ok(img) => {
                safety::log_debug(&format!(
                    "{who}: scaled EXR {}x{}",
                    img.width(),
                    img.height()
                ));
                return Ok(StreamSource::Frame(img));
            }
            Err(e) => {
                safety::log_debug(&format!("{who}: scaled EXR decode failed ({e})"));
                let _ = stream.Seek(0, STREAM_SEEK_SET, None);
            }
        }
    }

    // GENERIC archive (a registered .zip/.rar/.7z — NOT the cbz/epub/office/… zips,
    // which keep their dedicated cover paths): identify the image entries from the
    // archive's file LIST (central directory / headers — never a full decompress),
    // then pull only those. Gated on the Stat-recovered file extension so the
    // magic alone can't reroute a comic, and on MaxSize before any archive parser
    // runs so a huge project backup remains a cheap stock icon.
    match generic_archive(stream, max_file_bytes, who) {
        ArchiveProbe::NotGeneric => {}
        ArchiveProbe::NoCover => {
            // A recognized generic archive with no readable image: fail now so
            // Explorer shows the stock zip icon — buffering the whole file just to
            // fail the image tiers on raw archive bytes would prove nothing.
            safety::log_debug(&format!("{who}: generic archive with no image entries"));
            return Err(Error::from(E_FAIL));
        }
        ArchiveProbe::Found(src) => return Ok(src),
    }

    // HEAD-PREVIEW fast path (opaque PSD/PSB, plain .blend): the baked preview
    // lives in the file's head, so reading the whole (possibly ~100 MB) document
    // through the marshaled IStream just to slice out a ~160px JPEG is the
    // dominant per-thumbnail cost in a big PSD folder — Explorer extracts
    // serially, so every file pays it in turn. Read a bounded prefix (exact
    // resources-section end for PSD) and commit to it ONLY when the same
    // extractor the decode tier runs actually finds a preview in it; otherwise
    // fall through to the normal paths (a PSD with no baked thumbnail still
    // renders via the full tiers exactly as before). Runs for ANY size — the win
    // is under-cap files, which used to pay the whole-file read; an oversized
    // hit just gets the exact prefix instead of the blanket rescue below.
    // Transparent PSDs skip this (preview_prefix_len bows out) — their composite
    // needs the full bytes.
    if let Some(prefix) = head_preview_fast(stream) {
        safety::log_debug(&format!(
            "{who}: head-preview fast path ({} bytes)",
            prefix.len()
        ));
        return Ok(StreamSource::Bytes(prefix));
    }

    // CAMERA-RAW embedded-preview fast path.  A number of RAW families put a
    // display JPEG near the front of the file, followed by tens or hundreds of
    // MiB of sensor data. Do not turn every TIFF into this path: it needs a RAW
    // extension or RAW-specific container metadata, plus a structurally valid
    // preview. On a miss (including a preview beyond this bounded prefix) the
    // old whole-file path remains the correctness backstop.
    if let Some(raw) = raw_preview_fast(stream, max_file_bytes) {
        match raw {
            RawFastSource::Preview(preview) => {
                safety::log_debug(&format!(
                    "{who}: RAW embedded-preview fast path ({} bytes)",
                    preview.len()
                ));
                return Ok(StreamSource::Bytes(preview));
            }
            RawFastSource::Prefix(prefix, size) => {
                // No early JPEG: reuse the bytes already fetched while probing and
                // read only the remaining tail. This preserves the old full-decode
                // fallback without rereading the first 16 MiB.
                stream.Seek(prefix.len() as i64, STREAM_SEEK_SET, None)?;
                return Ok(StreamSource::Bytes(read_all_append(
                    stream,
                    decode::effective_input_cap(max_file_bytes) as usize,
                    Some(size),
                    prefix,
                )?));
            }
        }
    }

    // Not audio, not video: skip oversized files cheaply via the stream length
    // before reading into memory. The effective cap is the user's MaxSize but
    // never above the hard MAX_BYTES ceiling ("0 = unlimited" means "up to
    // MAX_BYTES").
    let max = max_file_bytes.min(hard_cap);
    let size = stream_size(stream);
    match size {
        // Oversized: the whole-file read is a DoS risk, so we skip it —
        // EXCEPT a seek-streamable container: a giant ZIP comic archive
        // (CBZ) reads only its central directory + one cover entry
        // over the IStream, and a Clip Studio .clip seeks to the SQLite
        // database at its tail and reads only that. (CBR can't — `rars`
        // needs the full buffer — so a huge .cbr still gets the default
        // icon.) Head-preview containers (.blend / PSD-PSB) get a second
        // rescue: their baked thumbnail sits in the first bytes, so a
        // bounded prefix read suffices no matter the file size (issue #1).
        Some(size) if size > max => {
            if let Some(cover) = archive_cover_streamed(stream) {
                safety::log_debug(&format!("{who}: streamed cover from {size}-byte archive"));
                return Ok(StreamSource::Bytes(cover));
            }
            if let Some(prefix) = head_preview_prefix(stream) {
                safety::log_debug(&format!(
                    "{who}: head-preview prefix ({} bytes) of {size}-byte file",
                    prefix.len()
                ));
                return Ok(StreamSource::Bytes(prefix));
            }
            // GIMP `.xcf`. Every rescue around this one needs something a GIMP file does not
            // have: a baked-in preview near the front (it has none at all) or an OS codec for
            // the WIC pass below (Windows has none). So a large `.xcf` reached no decoder on
            // any version ever shipped, and that is not a small class of file: XCF stores
            // layers, and layered work is exactly what gets big. Its decoder walks absolute
            // file offsets and reads one tile at a time, so it needs no buffer and no cap.
            //
            // Placed BEFORE the MaxSize test below on purpose: nothing here is buffered, so
            // the reason that test exists does not apply. The user's own MaxSize is still
            // honoured — this branch is only reached when `size > min(MaxSize, hard cap)`, and
            // the settings-driven half of that is checked again by the caller.
            if size <= max_file_bytes {
                let mut reader = IStreamReader {
                    stream: stream.clone(),
                };
                if let Some(img) = crate::container::xcf_from_reader(&mut reader) {
                    safety::log_debug(&format!(
                        "{who}: streamed XCF decode of {size}-byte file -> {}x{}",
                        img.width(),
                        img.height()
                    ));
                    return Ok(StreamSource::Frame(img));
                }
                let _ = stream.Seek(0, STREAM_SEEK_SET, None);
            }
            // LAST RESCUE: hand the FILE to the OS codecs and let them scale during decode,
            // so a huge scan/panorama/RAW gets a real thumbnail instead of the stock icon.
            // Needs a real path — the shell usually exposes one (same recovery the video
            // tier already relies on); a sandboxed or virtual item has none and falls
            // through to the refusal below exactly as before. Nothing is buffered: WIC
            // reads the file itself and `target_edge` bounds what we copy out.
            // LAST RESCUE: let the OS codecs read THIS STREAM and scale during decode, so a
            // huge scan/panorama/RAW gets a real thumbnail instead of the stock icon.
            //
            // Deliberately stream-based, not path-based. The shell gives a thumbnail provider
            // no path (its stream reports only a leaf name), so an earlier by-path version of
            // this rescue could never fire here. WIC reads a stream lazily, which is all the
            // rescue ever actually needed: nothing buffers the document, and `target_edge`
            // bounds what gets copied out.
            //
            // ONLY when OUR OWN buffering ceiling is what refused the file. If the USER set a
            // smaller MaxSize, they asked us to skip files this big and the rescue must not
            // quietly overrule that -- "too big to hold in memory" is our problem to route
            // around, "don't spend effort on files over N MB" is their decision to keep.
            if size <= max_file_bytes {
                let head = read_prefix(stream, decode::COLOR_HEAD_BYTES);
                if let Some(img) = decode::wic_scaled_from_stream(stream, target_edge, &head) {
                    safety::log_debug(&format!(
                        "{who}: oversized WIC rescue of {size}-byte stream -> {}x{}",
                        img.width(),
                        img.height()
                    ));
                    return Ok(StreamSource::Frame(img));
                }
                let _ = stream.Seek(0, STREAM_SEEK_SET, None);
            }
            safety::log_debug(&format!("{who}: skip, {size} bytes over limit"));
            Err(Error::from(E_FAIL))
        }
        None if peek_is_7z(stream) => {
            // A provider stream with neither a recoverable name nor a Stat size
            // cannot prove this is a bounded CB7 rather than a huge project 7z.
            // Do not pull up to hundreds of MiB over a marshaled/network stream
            // merely to discover that after the fact.
            safety::log_debug(&format!(
                "{who}: refusing name-less 7z with unavailable stream size"
            ));
            Err(Error::from(E_FAIL))
        }
        _ => {
            let _ = stream.Seek(0, STREAM_SEEK_SET, None);
            Ok(StreamSource::Bytes(read_all(stream, max as usize, size)?))
        }
    }
}

/// Does the post-frame-tiers cover-art rescue in [`stream_source_with_caps`] need
/// to call `vcodec::cover_art` at all, or did the prefer-cover-art pass already
/// call it (and find nothing) for this exact stream? A second call on an
/// untouched stream can only repeat the same answer — a redundant full moov
/// scan this predicate exists to skip. Kept as a standalone, argument-driven
/// function (rather than inlined) so the decision itself — not the registry
/// read or the IStream plumbing around it — is what a test pins down.
fn needs_fallback_cover_art(already_tried_cover_art: bool) -> bool {
    !already_tried_cover_art
}

/// First `max` bytes of `stream`, rewinding afterwards. Short reads are fine: the only
/// consumer is the ISOBMFF colour-box probe, which simply finds nothing and lets WIC's own
/// colour context answer instead.
unsafe fn read_prefix(stream: &IStream, max: usize) -> Vec<u8> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut buf = vec![0u8; max];
    let mut got: u32 = 0;
    let hr = stream.Read(buf.as_mut_ptr() as *mut c_void, max as u32, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    if hr.is_err() {
        return Vec::new();
    }
    // Never trust the reported count past the buffer we supplied (same clamp the sibling
    // reads use, for the same hostile-stream reason).
    buf.truncate((got as usize).min(max));
    buf
}

/// The stream's total size in bytes via `IStream::Stat`, or None if the stream
/// doesn't support it (then the general read is bounded by the effective user +
/// hard cap, while expensive name-less 7z input fails closed).
unsafe fn stream_size(stream: &IStream) -> Option<u64> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
    Some(stat.cbSize)
}

/// Read just enough to recognize a 7z signature and rewind. Used only for the
/// unknown-size fail-closed gate; ZIP remains eligible for its deliberate
/// seek-only CBZ cover rescue.
unsafe fn peek_is_7z(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut signature = [0u8; 6];
    let mut got = 0u32;
    let result = stream.Read(
        signature.as_mut_ptr() as *mut c_void,
        signature.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    result.is_ok()
        && got as usize == signature.len()
        && signature == [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]
}

/// Sniff the stream head for a video container we can frame-grab (Matroska/WebM, MP4/MOV,
/// AVI, ASF/WMV, …). Rewinds to 0 either way so the subsequent MF / whole-file read starts
/// clean. HEIC/AVIF and M4A/M4B share MP4's `ftyp` box but are excluded by `is_video_magic`.
unsafe fn peek_is_video(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    // 208 bytes: enough to verify the MPEG-TS / M2TS sync-byte STRIDE (a second 0x47 at
    // offset 188 / 196) so we don't false-match any file that merely starts with 'G'.
    let mut head = [0u8; 208];
    let mut got: u32 = 0;
    let hr = stream.Read(
        head.as_mut_ptr() as *mut c_void,
        head.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    hr.is_ok() && crate::video::is_video_magic(&head[..got])
}

/// Is the stream head the OpenEXR magic? Rewinds either way so the scaled decode
/// (or, on a miss, the rest of the cascade) starts clean.
unsafe fn peek_is_exr(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 4];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, 4, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    hr.is_ok() && got == 4 && decode::is_exr_magic(&head)
}

/// Is the stream head the Ogg container magic (`OggS`)? Ogg carries both video (.ogv) and
/// audio (Vorbis/Opus/Speex), so a video frame-grab miss on an Ogg means it's audio-only —
/// the caller then falls back to the album-art path instead of failing.
unsafe fn peek_is_ogg(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 4];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, 4, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    hr.is_ok() && got == 4 && &head == b"OggS"
}

/// Recover the backing file path from the shell's `IStream` via `IStream::Stat`
/// (`STATFLAG_DEFAULT` fills `pwcsName` — file-backed shell streams report the full path).
/// Returned only when it names an existing file, so a stream with no / non-file name simply
/// falls back to streaming. `pwcsName` is a CoTaskMem allocation we own and must free.
///
/// TEST-ONLY, deliberately. Nothing in production may depend on this, because for the streams
/// the shell actually hands our handlers it always returns `None` (they report a bare leaf
/// name, and resolving a relative name against our own working directory would risk opening a
/// DIFFERENT file of the same name). It survives so tests can PIN that fact: see
/// `nameless_oversized_7z_is_not_streamed_past_max_size`, which asserts no path is
/// recoverable while the file's extension still is. Anything wanting a file TYPE wants
/// [`stream_extension`]; anything wanting to avoid buffering wants the stream itself.
#[cfg(test)]
unsafe fn stream_path(stream: &IStream) -> Option<String> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    let s = stat.pwcsName.to_string().ok();
    CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void));
    let s = s?;
    let p = std::path::Path::new(&s);
    // ABSOLUTE, then existing — in that order, and the absolute check is not cosmetic.
    // Streams routinely report only a LEAF NAME rather than a path: `SHCreateStreamOnFileEx`
    // does, and so does a shell item bound via `BHID_Stream` (both verified here). A bare
    // name reaching `is_file()` is resolved against OUR PROCESS'S working directory, so it
    // either fails — or, far worse, silently matches a DIFFERENT file that happens to share
    // the name, and we would then decode and cache that one as the user's thumbnail. Refusing
    // a relative name costs only a fast path we can retake from the stream itself.
    if p.is_absolute() && p.is_file() {
        Some(s)
    } else {
        safety::log_debug(&format!(
            "stream_path: {s:?} is not an absolute path to an existing file"
        ));
        None
    }
}

/// File-name extension reported by a shell stream, without requiring that the
/// name be an absolute, currently-existing path.  Virtual shell sources often
/// report only a display name; that is still enough for a conservative format
/// gate, whereas [`stream_path`] intentionally rejects it for direct file I/O.
pub(crate) unsafe fn stream_extension(stream: &IStream) -> Option<String> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    let name = stat.pwcsName.to_string().ok();
    CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void));
    name.and_then(|name| {
        std::path::Path::new(&name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
    })
}

/// Read up to `max` bytes off the stream head in big sequential gulps, rewinding to 0
/// before and after. Shared by the video-prefix decode and the head-preview rescue —
/// the bounded read is the same, only the cap differs.
unsafe fn stream_prefix(stream: &IStream, max: usize) -> Option<Vec<u8>> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let cap = stream_size(stream).map_or(max, |sz| (sz as usize).min(max));
    let mut buf = vec![0u8; cap];
    let mut filled = 0usize;
    while filled < cap {
        let mut got: u32 = 0;
        let hr = stream.Read(
            buf[filled..].as_mut_ptr() as *mut c_void,
            (cap - filled) as u32,
            Some(&mut got),
        );
        if hr.is_err() || got == 0 {
            break;
        }
        filled += got as usize;
    }
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    buf.truncate(filled);
    (filled >= 64).then_some(buf)
}

/// Read exactly `buf.len()` bytes starting at the stream's current position (looping over
/// short reads). None if the stream ends early.
unsafe fn read_full(stream: &IStream, buf: &mut [u8]) -> Option<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let mut got: u32 = 0;
        let want = (buf.len() - filled).min(u32::MAX as usize) as u32;
        let hr = stream.Read(
            buf[filled..].as_mut_ptr() as *mut c_void,
            want,
            Some(&mut got),
        );
        if hr.is_err() || got == 0 {
            break;
        }
        filled += got as usize;
    }
    (filled == buf.len()).then_some(())
}

/// Result of the audio-art probe. The three cases are distinct so the caller can
/// tell "this isn't audio" (take the normal whole-file path) from "this IS audio
/// but carries no usable art" (stop — the raw audio bytes are not a decodable
/// image, so a full read + decode would just burn time and fail).
enum AudioArt {
    NotAudio,
    NoArt,
    Art(Vec<u8>),
}

/// Sniff the stream for audio and, if so, extract only the embedded art via a
/// seek-only read (lofty seeks to the metadata — we never buffer the whole file,
/// so even a multi-GB audiobook thumbnails). Rewinds the stream to 0 either way.
unsafe fn audio_art(stream: &IStream) -> AudioArt {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 16];
    let mut got: u32 = 0;
    let hr = stream.Read(
        head.as_mut_ptr() as *mut c_void,
        head.len() as u32,
        Some(&mut got),
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    // Never trust the IStream-reported count past the buffer it filled.
    let got = (got as usize).min(head.len());
    if hr.is_err() || got < 12 || !crate::container::looks_like_audio(&head[..got]) {
        return AudioArt::NotAudio;
    }
    match crate::container::audio_art_from_reader(IStreamReader {
        stream: stream.clone(),
    }) {
        Some(art) => AudioArt::Art(art),
        None => AudioArt::NoArt,
    }
}

/// `std::io` Read + Seek over a COM IStream, so lofty can parse tags by seeking
/// instead of us draining the file into memory.
struct IStreamReader {
    stream: IStream,
}

impl std::io::Read for IStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut got: u32 = 0;
        unsafe {
            self.stream.Read(
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                Some(&mut got),
            )
        }
        .ok()
        .map_err(std::io::Error::other)?;
        // Never trust the IStream-reported count past the buffer it filled (the
        // sibling reads at `audio_art`/`read_all` clamp the same way) — returning
        // more than `buf.len()` violates the `Read` contract on a hostile stream.
        Ok((got as usize).min(buf.len()))
    }
}

impl std::io::Seek for IStreamReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let (origin, off): (STREAM_SEEK, i64) = match pos {
            std::io::SeekFrom::Start(n) => (STREAM_SEEK_SET, n as i64),
            std::io::SeekFrom::Current(n) => (STREAM_SEEK_CUR, n),
            std::io::SeekFrom::End(n) => (STREAM_SEEK_END, n),
        };
        let mut newpos: u64 = 0;
        unsafe { self.stream.Seek(off, origin, Some(&mut newpos)) }
            .map_err(std::io::Error::other)?;
        Ok(newpos)
    }
}

/// Drain an IStream into a Vec, bounded by `max`.
unsafe fn read_all(stream: &IStream, max: usize, size_hint: Option<u64>) -> Result<Vec<u8>> {
    read_all_append(stream, max, size_hint, Vec::new())
}

/// Continue draining an IStream after an already-read prefix, bounded by `max`.
/// The caller positions the stream immediately after `out` before entering.
unsafe fn read_all_append(
    stream: &IStream,
    max: usize,
    size_hint: Option<u64>,
    mut out: Vec<u8>,
) -> Result<Vec<u8>> {
    if out.len() > max {
        return Err(Error::from(E_FAIL));
    }
    // Pre-size from the (already size-checked) stream length to skip the doubling
    // realloc churn on multi-MB images. Cap the upfront reservation so a stream that
    // lies about its size can't trick us into a giant allocation — the growth loop +
    // the `max` check below still bound the true read.
    let cap = size_hint.map_or(0, |h| (h as usize).min(max).min(64 << 20));
    if cap > out.len() {
        out.reserve(cap - out.len());
    }
    // 1 MiB chunks: the stream is marshaled (often cross-process), so per-Read
    // overhead is real — 64 KiB chunks cost a 100 MB file ~1,600 round trips.
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let mut got: u32 = 0;
        let hr = stream.Read(
            chunk.as_mut_ptr() as *mut c_void,
            chunk.len() as u32,
            Some(&mut got),
        );
        // S_OK and S_FALSE are both successes; a failing HRESULT is a real transport
        // error (network/cloud-placeholder stream), NOT end-of-stream — don't mistake
        // it for EOF and silently feed a truncated buffer to the decoder.
        hr.ok()?;
        if got == 0 {
            break; // success + 0 bytes == genuine EOF
        }
        let n = (got as usize).min(chunk.len()); // never trust got > buffer
        if n > max - out.len() {
            return Err(Error::from(E_FAIL));
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::psd_testutil::synthetic_psd;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, STGM_READ, STGM_SHARE_DENY_NONE,
    };
    use windows::Win32::UI::Shell::{SHCreateMemStream, SHCreateStreamOnFileEx};

    /// Run the full source cascade on `bytes` (100 MB cap, like the default
    /// MaxSize) and return the byte payload it hands the decode tiers.
    fn source_bytes(bytes: &[u8]) -> Vec<u8> {
        let stream = unsafe { SHCreateMemStream(Some(bytes)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, 100 << 20, 256, "test") } {
            Ok(StreamSource::Bytes(b)) => b,
            other => panic!(
                "expected StreamSource::Bytes, got {}",
                match other {
                    Ok(StreamSource::Frame(_)) => "Frame".into(),
                    Ok(StreamSource::Covers(_)) => "Covers".into(),
                    Ok(StreamSource::Bytes(_)) => unreachable!(),
                    Err(e) => format!("Err({e})"),
                }
            ),
        }
    }

    /// The oversized rescue, exercised through the REAL cascade without staging a 256 MB file.
    ///
    /// The trick is that "oversized" is relative: pass a tiny `max_file_bytes` and an ordinary
    /// image is already past every cap, taking the exact branch a half-gigabyte scan takes in
    /// production. What is being proven is that the branch can decode from the STREAM ALONE,
    /// with no path anywhere, which is the whole reason this rescue works inside Explorer
    /// where the shell hands over a stream that knows only a leaf file name.
    #[test]
    fn oversized_stream_is_rescued_without_any_path() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let jpeg = substantial_jpeg();
        let stream = unsafe { SHCreateMemStream(Some(&jpeg)) }.expect("SHCreateMemStream");

        // A memory stream has NO name at all, let alone a path -- if the rescue needed one it
        // could not possibly succeed here.
        assert!(
            unsafe { stream_path(&stream) }.is_none(),
            "fixture must have no recoverable path, or this proves nothing"
        );

        // `max_file_bytes` is the USER's allowance and is left generous; the small hard cap is
        // what refuses the buffered read, which is exactly the production shape.
        // Generous user allowance, tiny HARD cap: the file is refused by OUR buffering
        // ceiling, which is precisely the production shape for a half-gigabyte scan.
        let got = unsafe { stream_source_with_caps(&stream, u64::MAX, 1024, 64, "test") };
        match got {
            Ok(StreamSource::Frame(img)) => {
                // Scaled DURING decode, so the rescue never materialises the full image.
                assert!(
                    img.width().max(img.height()) <= 64,
                    "rescue must honour the target edge, got {}x{}",
                    img.width(),
                    img.height()
                );
            }
            other => panic!(
                "oversized stream should be rescued into a Frame, got {}",
                match other {
                    Ok(StreamSource::Bytes(_)) => "Bytes".into(),
                    Ok(StreamSource::Covers(_)) => "Covers".into(),
                    Ok(StreamSource::Frame(_)) => unreachable!(),
                    Err(e) => format!("Err({e})"),
                }
            ),
        }
        unsafe { CoUninitialize() };
    }

    /// The rescue must fire at the SHIPPED DEFAULT, not merely at "Unlimited".
    ///
    /// Its sibling above passes `u64::MAX` for the user allowance, and that is exactly how the
    /// bug hid for two releases: `u64::MAX` satisfies the rescue's `size <= max_file_bytes`
    /// gate for any file at all, so the test passed while proving it only for a configuration
    /// almost nobody runs. The rescue actually lives in the window
    /// `hard cap < size <= MaxSize`, and a default MaxSize EQUAL to the hard cap — which is
    /// what shipped — makes that window empty. Every user who never opened Settings got the
    /// stock icon on files this code exists to rescue. Driving the real cascade with the real
    /// constant is what makes that unrepresentable.
    #[test]
    fn oversized_stream_is_rescued_at_the_shipped_default_not_only_at_unlimited() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let jpeg = substantial_jpeg();
        let stream = unsafe { SHCreateMemStream(Some(&jpeg)) }.expect("SHCreateMemStream");
        let default_allowance = u64::from(crate::settings::DEFAULT_MAX_FILE_MB) * 1024 * 1024;
        assert!(
            default_allowance > decode::limits::MAX_INPUT_BYTES,
            "the default allowance must sit CLEAR of the buffering ceiling, or the rescue's \
             window is empty and it can never run at the shipped setting ({default_allowance} \
             vs {})",
            decode::limits::MAX_INPUT_BYTES
        );
        // Same shape as production: the user's allowance is the default, and the (scaled-down)
        // hard cap is what refuses the buffered read.
        let got = unsafe { stream_source_with_caps(&stream, default_allowance, 1024, 64, "test") };
        assert!(
            matches!(got, Ok(StreamSource::Frame(_))),
            "the default setting must still reach the oversized rescue"
        );
        unsafe { CoUninitialize() };
    }

    /// The user's own MaxSize still wins. "Too big to hold in memory" is ours to route around;
    /// "do not bother with files over N" is the user's decision and the rescue must not
    /// silently overrule it.
    #[test]
    fn rescue_does_not_overrule_the_users_max_size() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let jpeg = substantial_jpeg();
        let stream = unsafe { SHCreateMemStream(Some(&jpeg)) }.expect("SHCreateMemStream");
        // User allows only 1 KB; the fixture is bigger, so this is THEIR refusal, not ours.
        let got = unsafe { stream_source_with_caps(&stream, 1024, 1 << 30, 64, "test") };
        assert!(
            got.is_err(),
            "a file over the user's MaxSize must stay refused"
        );
        unsafe { CoUninitialize() };
    }

    /// The exact A032 fix: once the prefer-cover-art pass has already run
    /// `vcodec::cover_art` for this stream (whether or not it found anything),
    /// the fallback rescue after all frame tiers fail must NOT run it again —
    /// that was the third full moov scan. When that pass never ran (feature
    /// off), the fallback is still the only place `cover_art` gets called.
    #[test]
    fn fallback_cover_art_skipped_only_when_already_tried() {
        assert!(!needs_fallback_cover_art(true));
        assert!(needs_fallback_cover_art(false));
    }

    #[test]
    fn unlimited_setting_still_obeys_the_hard_archive_cap() {
        assert_eq!(decode::effective_input_cap(u64::MAX), MAX_BYTES as u64);
        assert_eq!(
            decode::effective_input_cap((MAX_BYTES as u64) + 1),
            MAX_BYTES as u64
        );
        assert_eq!(decode::effective_input_cap(1 << 20), 1 << 20);
    }

    fn substantial_jpeg() -> Vec<u8> {
        let image = image::RgbImage::from_fn(320, 240, |x, y| {
            // Deterministic high-detail pixels keep the encoded preview well
            // above the 16 KiB real-preview floor.
            image::Rgb([
                ((x * 37 + y * 13) & 255) as u8,
                ((x * 11 + y * 53) & 255) as u8,
                ((x * 71 + y * 19) & 255) as u8,
            ])
        });
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
            .encode_image(&image::DynamicImage::ImageRgb8(image))
            .expect("encode test JPEG");
        assert!(out.len() >= decode::MIN_RAW_PREVIEW);
        out
    }

    fn mark_synthetic_tiff_raw(bytes: &mut [u8]) {
        assert!(bytes.len() >= 26);
        bytes[..8].copy_from_slice(b"II\x2A\0\x08\0\0\0");
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[10..12].copy_from_slice(&0x0106u16.to_le_bytes()); // PhotometricInterpretation
        bytes[12..14].copy_from_slice(&3u16.to_le_bytes()); // SHORT
        bytes[14..18].copy_from_slice(&1u32.to_le_bytes());
        bytes[18..20].copy_from_slice(&32_803u16.to_le_bytes()); // CFA
    }

    #[test]
    fn raw_prefix_carver_returns_a_complete_early_preview() {
        let jpeg = substantial_jpeg();
        let mut raw = b"II\x2A\0raw-header".to_vec();
        raw.extend_from_slice(&[0x55; 4096]);
        raw.extend_from_slice(&jpeg);
        raw.extend_from_slice(&[0xA5; 4096]);

        let got = decode::largest_embedded_jpeg(&raw, decode::MIN_RAW_PREVIEW)
            .expect("early RAW preview");
        assert_eq!(got, jpeg.as_slice());
        assert!(
            image::load_from_memory(got).is_ok(),
            "must return a full JPEG"
        );
    }

    #[test]
    fn raw_fast_path_gate_rejects_plain_tiff_and_non_raw() {
        assert!(is_raw_extension("pef"));
        assert!(!is_raw_extension("tif"));
        assert!(looks_like_raw_container(b"II\x2A\0rest", true));
        assert!(!looks_like_raw_container(b"II\x2A\0rest", false));
        let mut raw_tiff = [0u8; 26];
        mark_synthetic_tiff_raw(&mut raw_tiff);
        assert!(looks_like_raw_container(&raw_tiff, false));
        assert!(looks_like_raw_container(b"FUJIFILMCCD-RAW", false));
        assert!(!looks_like_raw_container(b"not a camera raw", true));
    }

    #[test]
    fn raw_fast_path_respects_the_configured_input_cap() {
        let large_raw = (RAW_PREFIX_BYTES as u64) + 1;
        assert!(raw_preview_size_allowed(large_raw, u64::MAX));
        assert!(!raw_preview_size_allowed(large_raw, 1024 * 1024));
    }

    #[test]
    fn unnamed_raw_stream_returns_only_its_early_preview_and_honors_max_size() {
        let jpeg = substantial_jpeg();
        let mut raw = vec![0u8; RAW_PREFIX_BYTES + 1];
        mark_synthetic_tiff_raw(&mut raw);
        let jpeg_start = 4096;
        raw[jpeg_start..jpeg_start + jpeg.len()].copy_from_slice(&jpeg);

        let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, u64::MAX, 256, "test") } {
            Ok(StreamSource::Bytes(bytes)) => assert_eq!(bytes, jpeg),
            _ => panic!("unnamed RAW stream should use its embedded preview"),
        }

        let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
        assert!(
            unsafe { stream_source(&stream, 1024 * 1024, 256, "test") }.is_err(),
            "RAW fast path must not bypass the configured MaxSize"
        );
        drop(stream);

        raw[jpeg_start..jpeg_start + jpeg.len()].fill(0);
        *raw.last_mut().unwrap() = 0xA5;
        let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, u64::MAX, 256, "test") } {
            Ok(StreamSource::Bytes(bytes)) => {
                assert_eq!(bytes.len(), raw.len());
                assert_eq!(bytes.last(), Some(&0xA5));
                assert_eq!(&bytes[..26], &raw[..26]);
            }
            _ => panic!("RAW without an early preview should keep the full-read fallback"),
        }
    }

    #[test]
    fn real_large_pef_stream_uses_bounded_preview_when_corpus_is_available() {
        let Some(parent) = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent() else {
            return;
        };
        let path = parent.join("test-corpus-real").join("sample.pef");
        if !path.exists() {
            return;
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let stream = unsafe {
            SHCreateStreamOnFileEx(
                PCWSTR(wide.as_ptr()),
                (STGM_READ | STGM_SHARE_DENY_NONE).0,
                0,
                false,
                None,
            )
            .expect("SHCreateStreamOnFileEx")
        };
        match unsafe { stream_source(&stream, u64::MAX, 256, "test") } {
            Ok(StreamSource::Bytes(bytes)) => {
                assert!(bytes.len() >= decode::MIN_RAW_PREVIEW);
                assert!(
                    bytes.len() < RAW_PREFIX_BYTES,
                    "must return only the embedded preview, not the RAW prefix"
                );
                let thumb = decode::decode_thumbnail_opts(&bytes, 256, true)
                    .expect("embedded PEF preview should decode");
                assert_eq!(
                    (thumb.width, thumb.height),
                    (256, 171),
                    "shell fast path should match the corpus PEF thumbnail orientation"
                );
            }
            _ => panic!("large PEF should return its bounded embedded preview"),
        }
        drop(stream);
        if com {
            unsafe { CoUninitialize() };
        }
    }

    /// The whole point of the EXR tier: a render pass past the size cap must still
    /// produce a picture. `max_file_bytes = 1` puts this file hopelessly over the
    /// limit, so every other branch of the cascade would return the stock icon.
    #[test]
    fn oversized_exr_is_scaled_off_the_stream_instead_of_refused() {
        let exr = crate::decode::tests::ramp_exr_bytes(600, 400);
        let stream = unsafe { SHCreateMemStream(Some(&exr)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, 1, 64, "test") } {
            // step = floor(600 / 64) = 9 -> ceil(600/9) x ceil(400/9) = 67x45.
            Ok(StreamSource::Frame(img)) => {
                assert_eq!((img.width(), img.height()), (67, 45));
            }
            other => panic!(
                "oversized EXR must yield a scaled frame, got {}",
                match other {
                    Ok(StreamSource::Bytes(_)) => "Bytes".to_string(),
                    Ok(StreamSource::Covers(_)) => "Covers".to_string(),
                    Ok(StreamSource::Frame(_)) => unreachable!(),
                    Err(e) => format!("Err({e})"),
                }
            ),
        }
        drop(stream);

        // The requested edge really drives the decode (a smaller tile reads less).
        let stream = unsafe { SHCreateMemStream(Some(&exr)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, u64::MAX, 256, "test") } {
            // step = floor(600 / 256) = 2 -> 300x200.
            Ok(StreamSource::Frame(img)) => assert_eq!((img.width(), img.height()), (300, 200)),
            _ => panic!("EXR should scale to the requested edge"),
        }
    }

    #[test]
    fn generic_archive_requires_a_known_size_before_parse() {
        assert_eq!(checked_generic_archive_size(None, u64::MAX), None);
        assert_eq!(
            checked_generic_archive_size(Some(decode::limits::MAX_INPUT_BYTES + 1), u64::MAX),
            None
        );
        assert_eq!(
            checked_generic_archive_size(Some(4096), u64::MAX),
            Some(4096)
        );
        assert_eq!(checked_generic_archive_size(Some(4096), 1024), None);
    }

    #[test]
    fn sevenz_unknown_size_probe_is_signature_exact_and_rewinds() {
        const SIGNATURE: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
        let mut bytes = SIGNATURE.to_vec();
        bytes.extend_from_slice(b"payload");
        let stream = unsafe { SHCreateMemStream(Some(&bytes)) }.expect("SHCreateMemStream");

        assert!(unsafe { peek_is_7z(&stream) });
        let mut first = [0u8; 6];
        let mut got = 0u32;
        unsafe {
            stream
                .Read(
                    first.as_mut_ptr() as *mut c_void,
                    first.len() as u32,
                    Some(&mut got),
                )
                .unwrap();
        }
        assert_eq!(got as usize, first.len());
        assert_eq!(first, SIGNATURE);

        let not_7z =
            unsafe { SHCreateMemStream(Some(b"PK\x03\x04zip")) }.expect("SHCreateMemStream");
        assert!(!unsafe { peek_is_7z(&not_7z) });
    }

    #[test]
    fn under_cap_opaque_psd_reads_only_the_head_prefix() {
        // 6 MB of layer data behind the resources section: the fast path must
        // hand the decode tiers the exact head prefix, not the whole file.
        let (psd, head_len) = synthetic_psd(3, true, 6 << 20);
        let got = source_bytes(&psd);
        assert_eq!(
            got.len(),
            head_len,
            "fast path should stop at the resources section"
        );
        assert_eq!(&got[..], &psd[..head_len]);
        // And the prefix must actually decode to the baked thumbnail.
        assert!(crate::container::extract_cover(&got).is_some());
    }

    #[test]
    fn psd_without_baked_thumbnail_falls_back_to_the_whole_file() {
        let (psd, _) = synthetic_psd(3, false, 1 << 20);
        let got = source_bytes(&psd);
        assert_eq!(
            got.len(),
            psd.len(),
            "no baked preview -> the pre-fast-path whole read"
        );
    }

    #[test]
    fn under_cap_dwg_reads_only_the_preview_section() {
        // 4 MB of "object database" behind the preview records: the fast path
        // stops right after the PNG record's payload.
        let (dwg, head_len) = crate::container::dwg_testutil::synthetic_dwg(true, 4 << 20);
        let got = source_bytes(&dwg);
        assert_eq!(
            got.len(),
            head_len,
            "DWG fast path should stop after the record payload"
        );
        assert!(crate::container::extract_cover(&got).is_some());
    }

    #[test]
    fn dwg_without_a_preview_section_falls_back_to_the_whole_file() {
        // RASTERPREVIEW=0 / pre-R13: no sentinel, so no fast path. The whole-file
        // read then fails the decode tiers exactly as it did before this path.
        let (dwg, _) = crate::container::dwg_testutil::synthetic_dwg(false, 1 << 20);
        let stream = unsafe { SHCreateMemStream(Some(&dwg)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, 100 << 20, 256, "test") } {
            Ok(StreamSource::Bytes(b)) => assert_eq!(b.len(), dwg.len()),
            Ok(_) => panic!("expected Bytes"),
            Err(_) => panic!("expected the whole-file read, not a failure"),
        }
    }

    /// The fast path sits BEFORE the size-cap branch, so a preview-bearing DWG
    /// past the user's MaxSize now thumbnails off its exact prefix. Previously it
    /// fell to the oversized arm, which has no DWG rescue (`has_head_preview`
    /// covers only blend/PSD/gzip) and returned E_FAIL — the stock icon. This is a
    /// new capability, not just a speedup.
    #[test]
    fn oversized_dwg_now_thumbnails_via_the_exact_prefix() {
        let (dwg, head_len) = crate::container::dwg_testutil::synthetic_dwg(true, 4 << 20);
        let stream = unsafe { SHCreateMemStream(Some(&dwg)) }.expect("SHCreateMemStream");
        // A 1 MiB cap puts this 4 MB+ file firmly over the limit.
        match unsafe { stream_source(&stream, 1 << 20, 256, "test") } {
            Ok(StreamSource::Bytes(b)) => {
                assert_eq!(b.len(), head_len);
                assert!(crate::container::extract_cover(&b).is_some());
            }
            other => panic!(
                "oversized DWG should now yield its preview, got {}",
                other.is_ok()
            ),
        }
    }

    #[test]
    fn transparent_psd_falls_back_to_the_whole_file() {
        // 4 channels in RGB mode = alpha: the composite path needs every byte,
        // so the fast path must bow out even though a baked thumbnail exists.
        let (psd, _) = synthetic_psd(4, true, 1 << 20);
        let got = source_bytes(&psd);
        assert_eq!(got.len(), psd.len());
    }

    /// A shell stream may expose no filename, so the generic-extension gate
    /// cannot distinguish `.7z` from `.cb7`. Oversized 7z must still stop at
    /// MaxSize instead of falling into the old cap-bypassing CB7 rescue.
    #[test]
    fn nameless_oversized_7z_is_not_streamed_past_max_size() {
        const ARCHIVE: &[u8] = include_bytes!("../tests/fixtures/sevenz/solid_order.7z");
        let path = std::env::temp_dir().join(format!("st2k_generic_cap_{}.7z", std::process::id()));
        std::fs::write(&path, ARCHIVE).expect("write fixture");
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let open = || unsafe {
            SHCreateStreamOnFileEx(
                PCWSTR(wide.as_ptr()),
                (STGM_READ | STGM_SHARE_DENY_NONE).0,
                0,
                false,
                None,
            )
            .expect("SHCreateStreamOnFileEx")
        };

        let stream = open();
        let stat_path = unsafe { stream_path(&stream) };
        assert!(
            stat_path.is_none(),
            "fixture must exercise the name-less shell-stream path, got {stat_path:?}"
        );
        // The other half of the same fact, and the reason two probes were quietly broken:
        // the stream DOES report a usable file TYPE even though it reports no usable PATH.
        // Anything that only needs the extension must therefore ask `stream_extension`;
        // asking `stream_path` gets `None` and silently disables the feature in Explorer.
        assert_eq!(
            unsafe { stream_extension(&stream) }.as_deref(),
            Some("7z"),
            "extension must still be recoverable from a stream that exposes no path"
        );
        assert!(
            unsafe { stream_source(&stream, 1, 256, "test") }.is_err(),
            "over-MaxSize name-less 7z must keep the stock icon"
        );
        drop(stream);

        if com {
            unsafe { CoUninitialize() };
        }
        let _ = std::fs::remove_file(path);
    }
}
