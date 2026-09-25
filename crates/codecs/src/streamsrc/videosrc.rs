//! The video arm of the cascade: remux or decode a frame from the stream, and what to fall back to when no decoder answers.

use super::*;

/// Read the embedded cover art through a fresh `IStreamReader` and, on a hit, log `$fmt`
/// with the cover's byte count and hand it straight back as [`StreamSource::Cover`].
/// Both cover-art rescues in this module end that way and differ only in their debug
/// line; the format string stays a literal at the call site so the two lines read exactly
/// as they did before. `$stream` must be a simple expression (`clone` is called on it).
macro_rules! cover_art_source {
    ($stream:expr, $fmt:literal) => {
        if let Some(cover) = crate::vcodec::cover_art(&mut IStreamReader {
            stream: $stream.clone(),
        }) {
            safety::log_debugf!($fmt, cover.len());
            return Some(Ok(StreamSource::Cover(cover)));
        }
    };
}

/// The video tiers of the [`stream_source_with_caps`] cascade. `None` means "not a
/// video, or OggS ambiguously falling through" - the caller continues to the
/// audio-art path below exactly as before; `Some(result)` means the cascade is
/// already resolved (a decoded frame, a cover-art rescue, or a hard failure).
pub(super) unsafe fn try_video_source(
    stream: &IStream,
    head: &StreamHead,
    cfg: &ThumbSettings,
    who: &str,
) -> Option<Result<StreamSource>> {
    if !head.is_video() {
        return None;
    }
    // OPTION: prefer the embedded poster over a frame from the film (`VideoCoverArt`,
    // off by default). Read BEFORE the frame tiers so a film library shows its covers
    // without paying for a decode first; a file with no cover falls straight through to
    // the normal cascade, having cost one bounded metadata read.
    //
    // `tried_cover_art` remembers whether this pass ran: if it did and found nothing,
    // the fallback rescue below (after every frame tier also fails) must not call
    // `vcodec::cover_art` a second time - the stream hasn't changed, so it would just
    // re-scan the same moov to the same null answer. That third full scan (cover_art,
    // keyframe_mini_mp4, cover_art again) was the actual A032 cost on a HEVC-with-no-
    // cover-and-no-OS-codec file.
    let mut tried_cover_art = false;
    if cfg.prefer_cover_art {
        tried_cover_art = true;
        cover_art_source!(stream, "{who}: cover art preferred over a frame ({} bytes)");
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
    // WHERE in the video: the user's `VideoOffset` (30 % unless changed - see
    // `settings::video_offset_frac`). Taken from the caller's snapshot so every tier below
    // seeks to the same mark; a per-tier read could disagree if the setting changed
    // mid-decode, and the fallbacks would then show a different frame from the tier that
    // was meant to run.
    let at = cfg.video_offset_frac;
    // A transport stream padded past its content (a preallocated or half-downloaded
    // recording): every tier below sees the stream end at its last packet, because Media
    // Foundation's transport source refuses the padding outright.
    let trimmed;
    let head = match head.size.and_then(|total| {
        crate::mpeg12::ts_content_len(
            &mut IStreamReader {
                stream: stream.clone(),
            },
            &head.bytes,
            total,
        )
    }) {
        Some(content) => {
            trimmed = StreamHead {
                bytes: head.bytes.clone(),
                size: Some(content),
                ext: head.ext.clone(),
            };
            &trimmed
        }
        None => head,
    };
    // Media Foundation is delay-loaded and absent on the N/KN editions and Server Core.
    // Every in-process tier below hands its bytes to MF, so without it each can only fail,
    // after paying its reads (up to 64 MiB for the prefix tier, 128 + 96 MiB for the
    // remux). Checked once here; the two out-of-process tiers (Flash-era FLV, VP9 profile
    // 2/3) decode in the sibling st2k.exe and run either way.
    // `mf_usable`, not `media_foundation_available`: a host with a decode worker stuck
    // inside MF past its grace (issue #35) has MF but must not feed it another file.
    let mf = crate::video::mf_usable();
    if !mf {
        safety::log_debugf!(
            "{who}: Media Foundation unavailable or wedged, in-process frame tiers skipped"
        );
    }
    // The user's MaxSize gates the two non-targeted fallbacks (tiers 4 and 5): they read
    // a bounded head, or head plus tail, wherever the keyframe is, and a file the user has
    // excluded by size must not pay for that. A stream with no reported size stays
    // eligible; both reads are bounded on their own.
    let within_max = prefix_tiers_allowed(head.size, cfg.max_file_bytes);
    // Tiers, each fast or a fast miss:
    //   1. SMART TARGETED READ (MP4/MOV): parse the moov index, build a tiny
    //      one-keyframe MP4 for the sync sample nearest the mark, decode that -
    //      single-digit MB (index + one keyframe), a representative frame, and
    //      it works regardless of moov position (faststart or moov-at-end);
    //   2. SMART TARGETED READ (Matroska/WebM): the EBML analog - read the Cues
    //      index, build a tiny one-cluster MKV for the keyframe nearest the mark;
    //  2b. FLV (H.264 only): walk the head tags for the AVC config + first keyframe
    //      and remux them into a mini-MP4 - MF has no FLV demuxer, so no later tier
    //      can open the container at all (it has no index, so `at` can't be honoured);
    //  2c. FLV (VP6 / Sorenson Spark): decode the first keyframe OUT OF PROCESS via
    //      the sibling st2k.exe - Windows has no decoder for these at all;
    //   3. GENERAL targeted read (AVI/WMV/… + any unmapped MP4/MKV): let MF's own
    //      demuxer seek the real index over a block-caching IStream that
    //      coalesces its reads (no per-format parser, any container MF decodes);
    //   4. a faststart MP4 / small / unindexed video decodes from its head prefix;
    //   5. a big *non*-faststart MP4 (moov at the very end) is remuxed -
    //      head frames + tail moov stitched into a small valid MP4.
    // Tiers 4–5 stay as fallbacks for anything tier 3's demuxer can't seek. They read a
    // bounded head prefix, so they CANNOT honour a late offset - there are no bytes there
    // to seek into. That is a property of the fallback, not a bug to fix here.
    let probe = probe_container_tiers(stream, mf, at, who);
    let mf = probe.mf;
    let frame = mp4_mkv_or_else_tiers(stream, head, probe.clip_bytes, mf, within_max, at, who);
    if let Some(frame) = frame {
        return Some(Ok(resolve_decoded_frame(
            frame,
            probe.container_ran,
            probe.container_rotation,
            stream,
            who,
        )));
    }
    // No decodable frame. OggS is ambiguous - an audio-only .ogg/.opus matches
    // the video magic too, so fall THROUGH to the album-art path below instead of
    // failing. A genuine video container the OS can't decode stops here - after one
    // last rescue: Matroska attached cover art. The usual reason NO tier decoded is
    // a missing OS codec (HEVC/AV1 are Store add-ons, not inbox), and library rips
    // routinely attach a poster, so show the film instead of a blank tile. Bounded:
    // the attachment element is read via the container's own index, never the stream.
    video_undecodable_fallback(head, tried_cover_art, stream, who)
}

/// ISSUE #32, applied HERE because here is where every tier above converges. A clip
/// rotated losslessly (metadata only, no re-encode) must thumbnail the way it plays,
/// which is what Windows' own thumbnailer does; doing it per-tier would mean six
/// chances to forget. The MP4/MKV keyframe tiers already parsed this rotation
/// out of the same moov/Tracks they read for the mini-clip - only a
/// tier that did NOT parse the container needs this standalone probe to re-read it.
/// Not-an-MP4, or an upright one, costs at most one bounded moov read and changes
/// nothing.
pub(super) unsafe fn resolve_decoded_frame(
    frame: image::DynamicImage,
    container_ran: bool,
    container_rotation: Option<u32>,
    stream: &IStream,
    who: &str,
) -> StreamSource {
    let rotation = if container_ran {
        container_rotation
    } else {
        crate::mp4::display_rotation(&mut IStreamReader {
            stream: stream.clone(),
        })
        .or_else(|| {
            crate::mkv::display_rotation(&mut IStreamReader {
                stream: stream.clone(),
            })
        })
    };
    let frame = match rotation {
        Some(deg) => {
            safety::log_debugf!("{who}: display matrix asks for {deg} deg");
            crate::video::apply_display_rotation(frame, deg)
        }
        None => frame,
    };
    safety::log_debugf!("{who}: video frame {}x{}", frame.width(), frame.height());
    StreamSource::Frame(frame)
}

/// The tail of the cascade once every frame tier has come back empty: Ogg and ASF fall through
/// to the audio-art path (`None`, same as "not video" as far as the caller is concerned; both
/// containers carry audio alone as often as video, and a WMA's cover is in its tags),
/// a genuine undecodable video gets one last Matroska-attached-cover-art rescue, and
/// otherwise the file fails outright. Mirrors the tail of `try_video_source` exactly.
pub(super) unsafe fn video_undecodable_fallback(
    head: &StreamHead,
    tried_cover_art: bool,
    stream: &IStream,
    who: &str,
) -> Option<Result<StreamSource>> {
    if head.is_ogg() || head.is_asf() {
        // A WMA shares the ASF container with WMV, and with no frame it is audio: its cover
        // is in its tags (the big-file gate found every WMA without one in Explorer).
        safety::log_debugf!("{who}: Ogg/ASF with no video frame - trying album art");
        return None;
    }
    if needs_fallback_cover_art(tried_cover_art) {
        cover_art_source!(
            stream,
            "{who}: video frame undecodable - using attached cover art ({} bytes)"
        );
    }
    safety::log_debugf!("{who}: video with no decodable frame");
    Some(Err(Error::from(E_FAIL)))
}
