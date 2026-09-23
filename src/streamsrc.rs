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
use windows::Win32::Foundation::{E_FAIL, E_OUTOFMEMORY};
use windows::Win32::System::Com::{
    CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATSTG, STREAM_SEEK, STREAM_SEEK_CUR,
    STREAM_SEEK_END, STREAM_SEEK_SET,
};

use crate::settings::ThumbSettings;
use crate::{decode, safety};

mod archive;
mod headprev;
mod headwin;
mod mp4remux;
mod rawsniff;

// Parent-hub imports: the children are glob-imported PRIVATELY so the cascade below
// still reads as one flat namespace (and so each child's own `use super::*` sees the
// shared IStream helpers). Nothing here is part of the crate's public surface except
// `stream_source` / `StreamSource`, which stay in this file.
use archive::*;
use headprev::*;
use headwin::*;
use mp4remux::*;
use rawsniff::*;
mod istream;
use istream::*;
mod videosrc;
pub(crate) use istream::{read_full, stream_extension, StreamHead};
use videosrc::*;
// The decode hub needs one of the sniffs directly: a TIFF whose IFD0 is only a
// reduced-resolution copy must not be answered by the `image` tier. See its doc comment.
pub(crate) use rawsniff::tiff_ifd0_is_reduced;

// The whole-file read ceiling, shared with the path-reading verbs via
// `decode::limits::MAX_INPUT_BYTES` (one DoS budget, not two copies).
const MAX_BYTES: usize = decode::limits::MAX_INPUT_BYTES as usize;

/// Read-ahead for the `std::io` readers handed to the zip and lofty parsers. Both seek to
/// a small index and then read it in small pieces; over a marshaled `IStream` each piece
/// would otherwise be one cross-process `Read` round trip.
const READ_AHEAD_BYTES: usize = 256 * 1024;

/// What [`stream_source`] hands back: a video frame Media Foundation already
/// decoded (no bytes to re-decode), the file's own picture decoded off the stream,
/// bounded raw bytes for the caller's tiered byte decoder, or a generic archive's
/// picked cover images for the contact-sheet compositor
/// (`decode::thumbnail_from_covers`).
pub enum StreamSource {
    Frame(image::DynamicImage),
    /// The file's own picture, read straight off the stream at or below its real size
    /// (a streamed EXR, a Photoshop composite, a GIMP flatten, the WIC rescue). A tile
    /// never enlarges it, exactly as the buffered path never enlarges a small file's own
    /// picture (`decode::thumbnail_from_own_picture`).
    Picture(image::DynamicImage),
    Bytes(Vec<u8>),
    /// A picture that STANDS IN for the file (an app's icon, an album's art, a document's
    /// baked thumbnail, a comic's cover, a rendered page), encoded. It fills the tile the way
    /// the same cover does when the buffered path finds it inside the whole file: the rule
    /// that never enlarges a file's own small picture is about the file, and a 72 px icon
    /// inside a 300 MB package is not the package's picture (`decode::decode_stand_in_thumbnail`).
    Cover(Vec<u8>),
    Covers(Vec<Vec<u8>>),
}

/// Turn the shell's `IStream` into a decodable source without ever buffering an
/// unbounded file. `who` prefixes the debug-log lines ("GetThumbnail" /
/// "DoPreview"); `cfg` is the caller's settings snapshot, whose `max_file_bytes`
/// is the user's MaxSize cap (one registry read per request, shared by every tier
/// here instead of each tier reading its own value). Purpose-built
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
/// 3. OPENEXR and FITS — decoded straight off the stream at the target edge, never
///    buffered (a 12K render pass is hundreds of MB of input and gigabytes of
///    float pixels; both caps refuse it, so it used to get the stock icon).
/// 4. OVERSIZED (past the cap) — streamed container cover (CBZ central
///    directory + one entry; Clip Studio `.clip` tail database) or the
///    head-preview prefix rescue (.blend/PSD-PSB baked previews sit in the
///    first bytes); otherwise skip.
/// 5. Everything else: bounded whole-file read.
///
/// `target_edge` is the caller's requested output edge; the streaming EXR tier
/// consumes it, as do the head-preview fast path, the streamed XCF flatten and
/// the oversized WIC rescue (a smaller target lets each skip more of the file).
pub unsafe fn stream_source(
    stream: &IStream,
    cfg: &ThumbSettings,
    target_edge: u32,
    who: &str,
) -> Result<StreamSource> {
    stream_source_with_caps(
        stream,
        cfg,
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
    cfg: &ThumbSettings,
    hard_cap: u64,
    target_edge: u32,
    who: &str,
) -> Result<StreamSource> {
    let max_file_bytes = cfg.max_file_bytes;
    // One head read and one `Stat`, shared by every probe below instead of each probe
    // seeking, reading and rewinding its own copy of the same first bytes and calling
    // `Stat` again for the same size and name.
    let head = stream_head(stream);

    if let Some(resolved) = try_video_source(stream, &head, cfg, who) {
        return resolved;
    }

    match audio_art(stream, &head) {
        AudioArt::Art(art) => return Ok(StreamSource::Cover(art)),
        AudioArt::NoArt => {
            safety::log_debugf!("{who}: audio file has no embedded art");
            return Err(Error::from(E_FAIL));
        }
        AudioArt::NotAudio => {}
    }

    if let Some(src) = try_exr_source(stream, &head, who, target_edge) {
        return Ok(src);
    }
    if let Some(src) = try_fits_source(stream, &head, who, target_edge) {
        return Ok(src);
    }

    // GENERIC archive (a registered .zip/.rar/.7z — NOT the cbz/epub/office/… zips,
    // which keep their dedicated cover paths): identify the image entries from the
    // archive's file LIST (central directory / headers — never a full decompress),
    // then pull only those. Gated on the Stat-recovered file extension so the
    // magic alone can't reroute a comic, and on MaxSize before any archive parser
    // runs so a huge project backup remains a cheap stock icon.
    match generic_archive(stream, &head, cfg, who) {
        ArchiveProbe::NotGeneric => {}
        ArchiveProbe::NoCover => {
            // A recognized generic archive with no readable image: fail now so
            // Explorer shows the stock zip icon — buffering the whole file just to
            // fail the image tiers on raw archive bytes would prove nothing.
            safety::log_debugf!("{who}: generic archive with no image entries");
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
    // needs the full bytes. So does a PSD whose ~160px baked preview is too small
    // for `target_edge` (issue #33): the prefix would otherwise be the only bytes
    // the decoder ever sees, and the composite unreachable however big the request.
    if let Some(prefix) = head_preview_fast(stream, &head, target_edge) {
        safety::log_debugf!("{who}: head-preview fast path ({} bytes)", prefix.len());
        return Ok(StreamSource::Bytes(prefix));
    }

    // CAMERA-RAW embedded-preview fast path. A number of RAW families put a
    // display JPEG near the front of the file, followed by tens or hundreds of
    // MiB of sensor data. Do not turn every TIFF into this path: it needs a RAW
    // extension or RAW-specific container metadata, plus a structurally valid
    // preview. On a miss (including a preview beyond this bounded prefix) the
    // old whole-file path remains the correctness backstop.
    if let Some(src) = try_raw_preview_fast(stream, &head, max_file_bytes, who)? {
        return Ok(src);
    }

    finish_bounded_read(stream, &head, max_file_bytes, hard_cap, target_edge, who)
}

/// Not audio, not video: the size-gated tail — oversized streamed cover / head-preview rescue,
/// name-less-7z refusal, otherwise the bounded whole-file read.
unsafe fn finish_bounded_read(
    stream: &IStream,
    head: &StreamHead,
    max_file_bytes: u64,
    hard_cap: u64,
    target_edge: u32,
    who: &str,
) -> Result<StreamSource> {
    // Not audio, not video: skip oversized files cheaply via the stream length
    // before reading into memory. The effective cap is the user's MaxSize but
    // never above the hard MAX_BYTES ceiling ("0 = unlimited" means "up to
    // MAX_BYTES").
    let max = max_file_bytes.min(hard_cap);
    match head.size {
        // Oversized: the whole-file read is a DoS risk, so we skip it —
        // EXCEPT a seek-streamable container: a giant ZIP comic archive
        // (CBZ) reads only its central directory + one cover entry
        // over the IStream, a Clip Studio .clip seeks to the SQLite
        // database at its tail and reads only that, and a CBR's block
        // headers are walked to the cover's entry (`rar::covers_seek`).
        // Head-preview containers (.blend / PSD-PSB) get a second
        // rescue: their baked thumbnail sits in the first bytes, so a
        // bounded prefix read suffices no matter the file size (issue #1).
        Some(size) if size > max => {
            oversized_rescue(stream, head, size, max_file_bytes, target_edge, who)
        }
        None if head.is_7z() => {
            // A provider stream with neither a recoverable name nor a Stat size
            // cannot prove this is a bounded CB7 rather than a huge project 7z.
            // Do not pull up to hundreds of MiB over a marshaled/network stream
            // merely to discover that after the fact.
            safety::log_debugf!("{who}: refusing name-less 7z with unavailable stream size");
            Err(Error::from(E_FAIL))
        }
        _ => {
            let _ = stream.Seek(0, STREAM_SEEK_SET, None);
            Ok(StreamSource::Bytes(read_all(
                stream,
                max as usize,
                head.size,
            )?))
        }
    }
}

/// Tiers 1-2 of the cascade (MP4/MKV keyframe-index parse) plus the issue #35
/// Media-Foundation-refusal gate that must run on their bytes before any later tier hands
/// the stream to MF. Returns the winning mini-clip bytes (if either container mapped),
/// whether a container parse ran at all, the display rotation it already parsed out (so
/// the caller never re-reads the container for it), and `mf` downgraded to `false` when
/// issue #35 fires.
struct ContainerProbe {
    clip_bytes: Option<Vec<u8>>,
    container_ran: bool,
    container_rotation: Option<u32>,
    mf: bool,
}

/// The two container facts the MP4/MKV keyframe tiers yield besides their clip bytes:
/// whether either container mapped a mini-clip at all, and the display rotation such a
/// parse already read out of its own moov/Tracks (so the caller never re-reads the
/// container for it). Shared with the by-bytes twin in `decode::pdf_tier`.
pub(crate) fn container_facts(
    mp4_clip: Option<&(Vec<u8>, Option<u32>)>,
    mkv_clip: Option<&(Vec<u8>, Option<u32>)>,
) -> (bool, Option<u32>) {
    let container_ran = mp4_clip.is_some() || mkv_clip.is_some();
    let container_rotation = mp4_clip
        .and_then(|(_, r)| *r)
        .or_else(|| mkv_clip.and_then(|(_, r)| *r));
    (container_ran, container_rotation)
}

unsafe fn probe_container_tiers(stream: &IStream, mf: bool, at: f64, who: &str) -> ContainerProbe {
    // The MP4/MKV keyframe tiers hand back the display rotation they already parsed out of
    // the same moov/Tracks they read for the mini-clip - captured here, once,
    // so the rotation decision below (after the tier chain converges) never re-reads the
    // container a second time. Gated on `mf` like every in-process tier: without Media
    // Foundation neither clip could ever decode to a frame. The MKV parse only runs when the
    // MP4 one didn't even locate a container to parse - a file the MP4 tier mapped is not
    // Matroska, so trying `keyframe_mini_mkv` on it would just be a second failing read.
    let mp4_clip = if mf {
        crate::mp4::keyframe_mini_mp4(
            &mut IStreamReader {
                stream: stream.clone(),
            },
            at,
        )
    } else {
        None
    };
    let mkv_clip = if mf && mp4_clip.is_none() {
        crate::mkv::keyframe_mini_mkv(
            &mut IStreamReader {
                stream: stream.clone(),
            },
            at,
        )
    } else {
        None
    };
    let (container_ran, container_rotation) = container_facts(mp4_clip.as_ref(), mkv_clip.as_ref());
    // ISSUE #35: decide from the container's OWN bytes whether Windows can decode this track
    // at all, BEFORE any tier hands Media Foundation the stream. Windows' H.264 decoder does
    // Baseline/Main/High 8-bit 4:2:0 only; Windows 11 refuses a 4:4:4 file at once, but the
    // reporter's Windows 10 22H2 wedged inside `ReadSample` on BOTH the mini-clip and the
    // block-stream tier, and the second of those ran inline on this thread, so Explorer's
    // whole thumbnail pipeline hung behind one file until a reboot. The mini-clip is already
    // in RAM and carries the `stsd` / `TrackEntry` verbatim, so this costs a few KB of box
    // walking and no further read of the stream. A shrug (any other codec, nothing parseable)
    // blocks nothing. Twin of the gate in `decode::try_video_tier`.
    let mf_refused = mp4_clip
        .as_ref()
        .map(|(b, _)| b.as_slice())
        .or_else(|| mkv_clip.as_ref().map(|(b, _)| b.as_slice()))
        .and_then(|mini| crate::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(mini)));
    if let Some(reason) = &mf_refused {
        safety::log(&format!(
            "{who}: {reason}; every Media Foundation tier skipped (issue #35)"
        ));
    }
    let mf = mf && mf_refused.is_none();
    let clip_bytes = mp4_clip
        .map(|(b, _)| b)
        .or_else(|| mkv_clip.map(|(b, _)| b));
    ContainerProbe {
        clip_bytes,
        container_ran,
        container_rotation,
        mf,
    }
}

/// Tiers 2b through 6: the tier-1/2 mini-clip (if any) decoded, then FLV remux, the
/// out-of-process Flash decode, MF's own demuxer over a block-caching stream, the
/// head-prefix and tail-remux fallbacks, and finally out-of-process VP9 profile 2/3.
/// `clip_bytes` is [`ContainerProbe::clip_bytes`] and `mf` is its (possibly downgraded)
/// `mf` flag - see `probe_container_tiers` and the tier comments in `try_video_source`.
unsafe fn mp4_mkv_or_else_tiers(
    stream: &IStream,
    head: &StreamHead,
    clip_bytes: Option<Vec<u8>>,
    mf: bool,
    within_max: bool,
    at: f64,
) -> Option<image::DynamicImage> {
    clip_bytes
        .filter(|_| mf)
        .and_then(crate::video::frame_from_owned_bytes)
        .or_else(|| {
            tier_if(mf, || {
                crate::flv::keyframe_mini_mp4(&mut IStreamReader {
                    stream: stream.clone(),
                })
                .and_then(crate::video::frame_from_owned_bytes)
            })
        })
        .or_else(|| {
            // 2c. FLV, VP6/Sorenson (issue #26): no Windows decoder exists, so the frame is
            //     decoded out of process by the sibling st2k.exe (`flv::flash_frame` - the
            //     pure-Rust Flash decoders panic on hostile input and must never run inside
            //     the shell host). Self-gated on the FLV magic + codec id; bounded head read.
            crate::flv::flash_frame(&mut IStreamReader {
                stream: stream.clone(),
            })
        })
        .or_else(|| {
            // MF demuxes AVI/WMV/etc. directly; the block-caching stream makes its
            // seek reads cheap (the old shell-IStream meltdown was thousands
            // of tiny marshaled reads - here they coalesce into a few big ones).
            tier_if(mf, || {
                head.size
                    .and_then(|size| crate::video::frame_from_block_stream(stream, size, at))
            })
        })
        .or_else(|| {
            tier_if(mf && within_max, || {
                video_prefix(stream, head.size).and_then(crate::video::frame_from_owned_bytes)
            })
        })
        .or_else(|| {
            tier_if(mf && within_max, || {
                head.size
                    .and_then(|total| mp4_remux_moov(stream, total))
                    .and_then(crate::video::frame_from_owned_bytes)
            })
        })
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
        })
        .or_else(|| {
            // 7. MPEG-1 system streams, bare MPEG-1/2 elementary streams, MPEG-2 program
            //    streams without the Store extension: Media Foundation has no source for the
            //    first two on any Windows, so when every tier above came back empty AND the
            //    head is one of the two MPEG magics, our own bounded demux (a window around
            //    the mark, coalesced block reads) cuts one intra picture and the sibling
            //    st2k.exe decodes it out of process (`crate::mpeg12`). Deliberately LAST,
            //    like VP9: a `.vob` on a machine with the Store extension keeps hitting the
            //    hardware-accelerated in-process MF path.
            crate::mpeg12::mpeg_frame(
                &mut IStreamReader {
                    stream: stream.clone(),
                },
                at,
            )
        })
}

/// OPENEXR - decode scaled straight off the (seekable) stream. A 12K VFX render
/// pass is hundreds of MB on disk, so the bounded whole-file read below refuses it
/// outright and the file gets the stock icon; and even well under the cap, the
/// `image` tier's full-resolution float decode costs orders of magnitude more
/// memory and time than the tile we were asked for. Files the scaled decoder
/// declines (deep, chroma-subsampled, non-RGB channel names) fall through to the
/// unchanged cascade below (`None`).
unsafe fn try_exr_source(
    stream: &IStream,
    head: &StreamHead,
    who: &str,
    target_edge: u32,
) -> Option<StreamSource> {
    if !head.is_exr() {
        return None;
    }
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let source = IStreamReader {
        stream: stream.clone(),
    };
    match decode::exr_scaled_from_reader(source, target_edge) {
        Ok(img) => {
            safety::log_debugf!("{who}: scaled EXR {}x{}", img.width(), img.height());
            Some(own_picture(head.bytes(), img))
        }
        Err(e) => {
            safety::log_debugf!("{who}: scaled EXR decode failed ({e})");
            let _ = stream.Seek(0, STREAM_SEEK_SET, None);
            None
        }
    }
}

/// FITS - the file's first image read off the stream at the target edge, whatever the file's
/// size (see `decode::fits`): the same reader the buffered tiers use, so a big file draws what
/// a small one does. A file it does not read falls through to the cascade below.
unsafe fn try_fits_source(
    stream: &IStream,
    head: &StreamHead,
    who: &str,
    target_edge: u32,
) -> Option<StreamSource> {
    if !decode::is_fits(head.bytes()) {
        return None;
    }
    let source = std::io::BufReader::with_capacity(
        READ_AHEAD_BYTES,
        IStreamReader {
            stream: stream.clone(),
        },
    );
    let img = decode::fits_scaled_from_reader(source, target_edge);
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let img = img?;
    safety::log_debugf!("{who}: FITS {}x{}", img.width(), img.height());
    Some(own_picture(head.bytes(), img))
}

/// CAMERA-RAW embedded-preview fast path (see the callsite comment in
/// [`stream_source_with_caps`]). `Ok(None)` means "no fast-path preview here,
/// continue the cascade below" - a miss, not an error.
unsafe fn try_raw_preview_fast(
    stream: &IStream,
    head: &StreamHead,
    max_file_bytes: u64,
    who: &str,
) -> Result<Option<StreamSource>> {
    let Some(raw) = raw_preview_fast(stream, head, max_file_bytes) else {
        return Ok(None);
    };
    match raw {
        RawFastSource::Preview(preview) => {
            safety::log_debugf!(
                "{who}: RAW embedded-preview fast path ({} bytes)",
                preview.len()
            );
            Ok(Some(StreamSource::Bytes(preview)))
        }
        RawFastSource::Prefix(prefix, size) => {
            // No early JPEG: reuse the bytes already fetched while probing and
            // read only the remaining tail. This preserves the old full-decode
            // fallback without rereading the first 16 MiB.
            stream.Seek(prefix.len() as i64, STREAM_SEEK_SET, None)?;
            Ok(Some(StreamSource::Bytes(read_all_append(
                stream,
                decode::effective_input_cap(max_file_bytes) as usize,
                Some(size),
                prefix,
            )?)))
        }
    }
}

/// A picture decoded straight off the stream, marked the way the buffered path treats the same
/// decode: the file's own picture, never enlarged, when the header declares a size the buffered
/// path can read (`decode::declared_dimensions`); otherwise a picture that fills the tile, as a
/// FITS or GIMP file under the input ceiling does. A small picture past the ceiling then draws
/// at the size it draws under it.
fn own_picture(head: &[u8], img: image::DynamicImage) -> StreamSource {
    if decode::declared_dimensions(head).is_some() {
        StreamSource::Picture(img)
    } else {
        StreamSource::Frame(img)
    }
}

/// Log a rescued picture and hand it back through [`own_picture`], or rewind `stream`
/// so the next rescue reads it from the start. The oversized-file rescues in
/// [`oversized_rescue`] end that way and differ only in their decode call and debug line;
/// the format string stays a literal at the call site so the lines read exactly as they
/// did before.
macro_rules! frame_or_rewind {
    ($stream:expr, $head:expr, $decode:expr, $fmt:literal) => {
        if let Some(img) = $decode {
            safety::log_debugf!($fmt, img.width(), img.height());
            return Ok(own_picture($head, img));
        }
        let _ = $stream.Seek(0, STREAM_SEEK_SET, None);
    };
}

/// The oversized-file branch of the tail size match in [`stream_source_with_caps`]:
/// a streamed archive cover, a head-preview prefix, a streamed XCF flatten, and
/// finally the OS-codec (WIC) rescue reading straight off the stream - each tried
/// in turn before giving up on a file too big to buffer.
unsafe fn oversized_rescue(
    stream: &IStream,
    head: &StreamHead,
    size: u64,
    max_file_bytes: u64,
    target_edge: u32,
    who: &str,
) -> Result<StreamSource> {
    if let Some(cover) = archive_cover_streamed(stream, head) {
        safety::log_debugf!("{who}: streamed cover from {size}-byte archive");
        return Ok(StreamSource::Cover(cover));
    }
    // A Photoshop document too big to buffer whose baked preview does not serve the request
    // (the head-preview fast path above takes every one whose preview does). Its stored
    // composite is read by offset, only the rows this tile needs, so the size costs nothing
    // (issue #46: past the ceiling the tile used to be the ~160 px preview blown up). Behind
    // MaxSize like the XCF walk below, for the same reason.
    if size <= max_file_bytes && head.bytes.starts_with(b"8BPS") {
        let reader = IStreamReader {
            stream: stream.clone(),
        };
        frame_or_rewind!(
            stream,
            head.bytes(),
            crate::container::psd_merged_from_reader(reader, target_edge),
            "{who}: stored PSD composite of {size}-byte file -> {}x{}"
        );
    }
    if let Some(prefix) = head_preview_prefix(stream, head) {
        safety::log_debugf!(
            "{who}: head-preview prefix ({} bytes) of {size}-byte file",
            prefix.len()
        );
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
    // honoured - this branch is only reached when `size > min(MaxSize, hard cap)`, and
    // the settings-driven half of that is checked again by the caller.
    if size <= max_file_bytes {
        let mut reader = IStreamReader {
            stream: stream.clone(),
        };
        // The caller's target goes IN, so the flatten happens on a reduced grid. A
        // big layered XCF is precisely the file that reaches this branch, and it is
        // also the one that used to spend seconds building a full-resolution canvas
        // nobody would look at.
        frame_or_rewind!(
            stream,
            head.bytes(),
            crate::container::xcf_from_reader(&mut reader, Some(target_edge)),
            "{who}: streamed XCF decode of {size}-byte file -> {}x{}"
        );
    }
    // ONLY when OUR OWN buffering ceiling is what refused the file. If the USER set a
    // smaller MaxSize, they asked us to skip files this big and the last rescues must not
    // quietly overrule that -- "too big to hold in memory" is our problem to route
    // around, "don't spend effort on files over N MB" is their decision to keep.
    if size <= max_file_bytes {
        if let Some(src) = last_rescues(stream, head, target_edge, who) {
            return Ok(src);
        }
    }
    safety::log_debugf!("{who}: skip, {size} bytes over limit");
    Err(Error::from(E_FAIL))
}

/// The last rescues, in the order that suits the file: a preview found by offset (a compound
/// file's streams, a DOS EPS, a DXF) or a PDF's first page read through its index, both exact;
/// then the OS codecs reading the stream (a big scan or panorama is a big picture), and the
/// ordinary tiers on the file's head (a model, a document or an e-book is usually a small
/// picture in front of a lot of data; see `headwin`).
unsafe fn last_rescues(
    stream: &IStream,
    head: &StreamHead,
    target_edge: u32,
    who: &str,
) -> Option<StreamSource> {
    if let Some(src) = offset_cover(stream, head, who) {
        return Some(src);
    }
    if crate::container::ole::looks_like_ole(head.bytes()) {
        // A compound file with no thumbnail stream: what draws the small one is the last
        // resort, the largest JPEG inside it, and in a big one that can sit anywhere.
        return embedded_jpeg(stream, who);
    }
    if head.bytes().starts_with(b"%PDF-") {
        // A PDF is its page renderer's alone: no head window or codec reads one, and a file the
        // renderer declines (an Illustrator placeholder, a document past 2 GiB) has no picture.
        return pdf_page(stream, head, target_edge, who);
    }
    if let Some(src) = mesh_stream(stream, head, who) {
        return Some(src);
    }
    if let Some(src) = raw_raster(stream, head, target_edge, who) {
        return Some(src);
    }
    let wic_first = wic_reads_the_whole_picture(head);
    if !wic_first {
        if let Some(img) = head_window(stream, head, target_edge, who) {
            return Some(own_picture(head.bytes(), img));
        }
    }
    if let Some(src) = wic_rescue(stream, target_edge, who) {
        return Some(src);
    }
    if let Some(src) = tiff_strips(stream, head, target_edge, who) {
        return Some(src);
    }
    if wic_first {
        return head_window(stream, head, target_edge, who)
            .map(|img| own_picture(head.bytes(), img));
    }
    None
}

/// A preview the container readers find by OFFSET, read through a block cache so a scattered
/// walk (a compound file's FAT) costs a few big reads rather than thousands of tiny ones.
/// Rewinds the stream.
unsafe fn offset_cover(stream: &IStream, head: &StreamHead, who: &str) -> Option<StreamSource> {
    let size = head.size?;
    let deadline = std::time::Instant::now() + OFFSET_COVER_BUDGET;
    let cached: IStream =
        crate::vstream::BlockCacheStream::new(stream.clone(), size, deadline).into();
    let cover = crate::container::seek_cover(IStreamReader { stream: cached }, head.bytes());
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    match cover? {
        crate::container::CoverOut::Bytes(bytes) => {
            safety::log_debugf!("{who}: preview found by offset ({} bytes)", bytes.len());
            Some(StreamSource::Cover(bytes))
        }
        crate::container::CoverOut::Image(img) => Some(StreamSource::Frame(img)),
    }
}

/// A 3D mesh (STL, OBJ, PLY) read whole off the stream and rendered, a huge one sampled down
/// to the render's triangle budget (see `decode::mesh`): a big model is all triangles, so no
/// head holds its shape.
unsafe fn mesh_stream(stream: &IStream, head: &StreamHead, who: &str) -> Option<StreamSource> {
    let size = head.size?;
    let sniff = stream_prefix(stream, Some(size), decode::MESH_SNIFF_BYTES)?;
    decode::mesh_kind(&sniff, size)?;
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let reader = std::io::BufReader::with_capacity(
        1 << 20,
        IStreamReader {
            stream: stream.clone(),
        },
    );
    let img = decode::mesh_from_reader(reader, &sniff, size);
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let img = img?;
    safety::log_debugf!(
        "{who}: mesh read off the stream -> {}x{}",
        img.width(),
        img.height()
    );
    Some(StreamSource::Frame(img))
}

/// A simple raster (binary PNM, PAM, PFM, farbfeld, TGA) too big to hold: the rows the tile
/// takes, read off the stream (see `decode::rawraster`). A big one of these is all pixels, so
/// no head holds it.
unsafe fn raw_raster(
    stream: &IStream,
    head: &StreamHead,
    target_edge: u32,
    who: &str,
) -> Option<StreamSource> {
    if !decode::is_raw_raster(head.bytes()) {
        return None;
    }
    // Unbuffered: each sampled row is one exact read at its offset (a read-ahead would be
    // thrown away by the next seek), and run-length TGA buffers its own front-to-back pass.
    let reader = IStreamReader {
        stream: stream.clone(),
    };
    let img = decode::raw_raster_scaled_from_reader(reader, target_edge);
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let img = img?;
    safety::log_debugf!(
        "{who}: raster rows read off the stream -> {}x{}",
        img.width(),
        img.height()
    );
    Some(own_picture(head.bytes(), img))
}

/// A TIFF the OS codecs would not open (BigTIFF, above all), read a strip or tile at a time
/// (see `decode::tiffscale`).
unsafe fn tiff_strips(
    stream: &IStream,
    head: &StreamHead,
    target_edge: u32,
    who: &str,
) -> Option<StreamSource> {
    if !plain_tiff(head) {
        return None;
    }
    let reader = IStreamReader {
        stream: stream.clone(),
    };
    let img = decode::tiff_scaled_from_reader(reader, target_edge);
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let img = img?;
    safety::log_debugf!(
        "{who}: TIFF strips read off the stream -> {}x{}",
        img.width(),
        img.height()
    );
    Some(own_picture(head.bytes(), img))
}

/// The largest JPEG embedded anywhere in the file (`decode::largest_embedded_jpeg_from`), read
/// front to back once within [`OFFSET_COVER_BUDGET`]; the buffered path's last resort. Rewinds.
unsafe fn embedded_jpeg(stream: &IStream, who: &str) -> Option<StreamSource> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let reader = UntilDeadline {
        inner: IStreamReader {
            stream: stream.clone(),
        },
        deadline: std::time::Instant::now() + OFFSET_COVER_BUDGET,
    };
    let jpeg = decode::largest_embedded_jpeg_from(
        std::io::BufReader::with_capacity(READ_AHEAD_BYTES, reader),
        decode::LENIENT_RAW_PREVIEW,
    );
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let jpeg = jpeg?;
    safety::log_debugf!("{who}: largest embedded JPEG ({} bytes)", jpeg.len());
    Some(StreamSource::Cover(jpeg))
}

/// A reader that ends at a deadline: a scan of a slow or remote file stops there with what it
/// has read.
struct UntilDeadline<R> {
    inner: R,
    deadline: std::time::Instant,
}

impl<R: std::io::Read> std::io::Read for UntilDeadline<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if std::time::Instant::now() >= self.deadline {
            return Ok(0);
        }
        self.inner.read(buf)
    }
}

/// How long [`offset_cover`] may spend reading.
const OFFSET_COVER_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

/// How much of a big PDF's head is read to tell an Illustrator file and find its private data:
/// Illustrator writes that among its first objects (at ~40-110 KB in the corpus's files).
const PDF_FRONT_BYTES: usize = 16 << 20;

/// A PDF's first page, or an Illustrator file's artboards by Illustrator's rules (see
/// `decode::pdf_tier::illustrator_answer`), rendered by the OS rasterizer reading the stream
/// itself (`pdf::render_pages_from_stream`), at the edge the PDF tier renders a small file at.
unsafe fn pdf_page(
    stream: &IStream,
    head: &StreamHead,
    target_edge: u32,
    who: &str,
) -> Option<StreamSource> {
    use crate::decode::pdf_tier;
    use crate::pdf::PageFit;
    let size = head.size?;
    // The viewer's whole-picture request is no thumbnail size: a page renders at the edge the
    // buffered preview gives it.
    let cx = (target_edge < decode::OVERSIZED_VIEW_EDGE).then_some(target_edge);
    let edge = pdf_tier::pdf_raster_edge(cx);
    let front = stream_prefix(stream, Some(size), PDF_FRONT_BYTES)?;
    let illustrator = crate::container::ai::is_illustrator(&front);
    let (fit, want) = if illustrator {
        (PageFit::Width(edge), pdf_tier::AI_SHEET_PAGES as u32)
    } else {
        (PageFit::LongSide(edge), 1)
    };
    let pages = crate::pdf::render_pages_from_stream(stream, size, fit, want);
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let (pngs, count) = pages?;
    safety::log_debugf!(
        "{who}: {} of {count} PDF page(s) rendered off the stream",
        pngs.len()
    );
    if !illustrator {
        return pngs.into_iter().next().map(StreamSource::Cover);
    }
    let rendered = pngs
        .iter()
        .map_while(|png| image::load_from_memory(png).ok())
        .collect();
    match pdf_tier::illustrator_answer(rendered, count as usize, &front, edge)? {
        Ok(img) => Some(StreamSource::Frame(img)),
        Err(e) => {
            safety::log_debugf!("{who}: {}", e.message());
            None
        }
    }
}

/// Let the OS codecs read THIS STREAM and scale during decode, so a huge scan, panorama or
/// RAW gets a real thumbnail instead of the stock icon. Rewinds the stream either way.
///
/// Deliberately stream-based, not path-based. The shell gives a thumbnail provider no path
/// (its stream reports only a leaf name), so an earlier by-path version of this rescue could
/// never fire here. WIC reads a stream lazily, which is all the rescue ever actually needed:
/// nothing buffers the document, and `target_edge` bounds what gets copied out.
unsafe fn wic_rescue(stream: &IStream, target_edge: u32, who: &str) -> Option<StreamSource> {
    let head = read_prefix(stream, decode::COLOR_HEAD_BYTES);
    let img = decode::wic_scaled_from_stream(stream, target_edge, &head);
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let img = img?;
    safety::log_debugf!(
        "{who}: oversized WIC rescue -> {}x{}",
        img.width(),
        img.height()
    );
    // A JPEG XR or HEIF the buffered path cannot size fills the tile there, so here too.
    Some(own_picture(&head, img))
}

/// Does the post-frame-tiers cover-art rescue in
/// [`video_undecodable_fallback`] (streamsrc/videosrc.rs) need to call
/// `vcodec::cover_art` at all, or did the prefer-cover-art pass already
/// call it (and find nothing) for this exact stream? A second call on an
/// untouched stream can only repeat the same answer — a redundant full moov
/// scan this predicate exists to skip. Kept as a standalone, argument-driven
/// function (rather than inlined) so the decision itself — not the registry
/// read or the IStream plumbing around it — is what a test pins down.
fn needs_fallback_cover_art(already_tried_cover_art: bool) -> bool {
    !already_tried_cover_art
}

/// Run a frame tier only when its precondition holds (Media Foundation present, the file
/// inside MaxSize). A false precondition is a miss like any other, so the chain continues
/// to the next tier without paying this one's reads.
fn tier_if<F>(enabled: bool, tier: F) -> Option<image::DynamicImage>
where
    F: FnOnce() -> Option<image::DynamicImage>,
{
    if enabled {
        tier()
    } else {
        None
    }
}

/// May the two non-targeted video fallbacks (the 64 MiB prefix and the head + tail remux)
/// run for a stream of `size`? Only inside the user's MaxSize, the same test
/// `oversized_rescue` applies; a stream with no reported size stays eligible because both
/// reads are bounded on their own.
fn prefix_tiers_allowed(size: Option<u64>, max_file_bytes: u64) -> bool {
    size.is_none_or(|size| size <= max_file_bytes)
}

/// First `max` bytes of `stream`, rewinding afterwards. Short reads are fine: the only
/// consumer is the ISOBMFF colour-box probe, which simply finds nothing and lets WIC's own
/// colour context answer instead.
unsafe fn read_prefix(stream: &IStream, max: usize) -> Vec<u8> {
    let mut buf = vec![0u8; max];
    let got = read_head(stream, &mut buf).unwrap_or(0);
    buf.truncate(got);
    buf
}

/// How much of the stream head [`stream_head`] reads up front for every probe in the
/// cascade. 4 KiB covers every magic sniff (the longest, the MPEG-TS / M2TS sync-byte
/// stride check, wants 197 bytes) and the IFD0 walk of a TIFF-shaped camera RAW.
const HEAD_BYTES: usize = 4096;

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
unsafe fn audio_art(stream: &IStream, head: &StreamHead) -> AudioArt {
    let head = head.first(16);
    if head.len() < 12 || !crate::container::looks_like_audio(head) {
        return AudioArt::NotAudio;
    }
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    // Buffered: lofty reads its tag blocks in small pieces, and each piece would otherwise
    // be one marshaled `IStream::Read` round trip.
    match crate::container::audio_art_from_reader(std::io::BufReader::with_capacity(
        READ_AHEAD_BYTES,
        IStreamReader {
            stream: stream.clone(),
        },
    )) {
        Some(art) => AudioArt::Art(art),
        None => AudioArt::NoArt,
    }
}

#[cfg(test)]
mod tests;
