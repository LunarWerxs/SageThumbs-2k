//! Which of a media file's streams THIS Windows cannot decode, asked of Media Foundation itself:
//! the same source and decoders the Quick preview's media engine plays through, so the answer is
//! the engine's, not a guess from the container (issue #49).
//!
//! The case that prompted it: an MKV with DTS sound. Windows ships no DTS decoder at all, so the
//! Quick preview played it in silence and said nothing, and a codec pack cannot help, because
//! K-Lite and its kind plug into DirectShow, which Media Foundation never asks. The same silence
//! met HEVC or AV1 video without the Store extension: a still frame, no word why. Naming the
//! stream that has no decoder turns "SageThumbs is broken" into "this file needs X".
//!
//! Opens the file with a Source Reader and asks every stream for decoded output (float PCM for
//! sound, RGB32 for pictures, with the reader's own converters allowed); a stream that refuses
//! has no decoder on this machine. Media Foundation is never handed the stream data, so this is
//! a header read, not a decode. EXE-only in practice: the preview calls it off its UI thread.

use windows::core::{GUID, HSTRING};
use windows::Win32::Media::MediaFoundation::*;

/// One stream Media Foundation opened but has no decoder for.
#[derive(Debug, Clone, PartialEq)]
pub struct MissingDecoder {
    /// Sound (true) or picture (false).
    pub audio: bool,
    /// The codec's display name ("DTS", "HEVC"), or `None` for one we don't recognise.
    pub codec: Option<&'static str>,
    /// The Microsoft Store extension that adds the decoder, where one exists (proper noun,
    /// never translated).
    pub store_extension: Option<&'static str>,
}

/// Streams Media Foundation cannot decode on this machine, first stream first. Empty when every
/// stream decodes, when the file does not open as media, or when Media Foundation is absent or
/// wedged in this process (see `video::mf_usable`): a shrug, never a false alarm.
pub fn missing_decoders(path: &str) -> Vec<MissingDecoder> {
    if !crate::video::mf_usable() {
        return Vec::new();
    }
    unsafe { probe(path).unwrap_or_default() }
}

/// More streams than any real file carries; a hostile header cannot make the loop long.
const MAX_STREAMS: u32 = 64;

unsafe fn probe(path: &str) -> Option<Vec<MissingDecoder>> {
    let _session = crate::video::MfSession::start()?;
    let reader = open_reader(path)?;
    let mut out = Vec::new();
    for index in 0..MAX_STREAMS {
        let Ok(native) = reader.GetNativeMediaType(index, 0) else {
            break;
        };
        out.extend(stream_without_decoder(&reader, index, &native));
    }
    Some(out)
}

unsafe fn open_reader(path: &str) -> Option<IMFSourceReader> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1).ok()?;
    let attrs = attrs?;
    // The reader's own colour converter may bridge a decoder's output to RGB32, exactly as the
    // thumbnail path lets it: a stream only counts as undecodable when no decoder exists.
    attrs
        .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
        .ok()?;
    MFCreateSourceReaderFromURL(&HSTRING::from(path), &attrs).ok()
}

/// Stream `index` (native type `native`) as a [`MissingDecoder`] when it is sound or picture
/// that no decoder on this machine takes; `None` when it decodes, or plays no part (subtitles,
/// data, attachments).
unsafe fn stream_without_decoder(
    reader: &IMFSourceReader,
    index: u32,
    native: &IMFMediaType,
) -> Option<MissingDecoder> {
    let major = native.GetGUID(&MF_MT_MAJOR_TYPE).ok()?;
    let subtype = native.GetGUID(&MF_MT_SUBTYPE).ok()?;
    let decoded = decoded_subtype(major)?;
    let want = MFCreateMediaType().ok()?;
    want.SetGUID(&MF_MT_MAJOR_TYPE, &major).ok()?;
    want.SetGUID(&MF_MT_SUBTYPE, &decoded).ok()?;
    reader
        .SetCurrentMediaType(index, None, &want)
        .is_err()
        .then(|| describe(major == MFMediaType_Audio, subtype))
}

/// The uncompressed output asked of a stream of `major` type; `None` for a type nobody plays.
fn decoded_subtype(major: GUID) -> Option<GUID> {
    if major == MFMediaType_Audio {
        Some(MFAudioFormat_Float)
    } else if major == MFMediaType_Video {
        Some(MFVideoFormat_RGB32)
    } else {
        None
    }
}

/// Dolby TrueHD's subtype, which the Media Foundation headers do not name.
const TRUEHD: GUID = GUID::from_u128(0xeb27cec4_163e_4ca3_8b74_8e25f91b517e);

fn describe(audio: bool, subtype: GUID) -> MissingDecoder {
    let (codec, store_extension) = if audio {
        (audio_name(subtype), None)
    } else {
        video_name(subtype)
    };
    MissingDecoder {
        audio,
        codec,
        store_extension,
    }
}

fn audio_name(s: GUID) -> Option<&'static str> {
    let dts = [
        MFAudioFormat_DTS,
        MFAudioFormat_DTS_RAW,
        MFAudioFormat_DTS_HD,
        MFAudioFormat_DTS_XLL,
        MFAudioFormat_DTS_LBR,
        MFAudioFormat_DTS_UHD,
        MFAudioFormat_DTS_UHDY,
    ];
    let named = [
        (MFAudioFormat_Dolby_AC3, "Dolby Digital"),
        (MFAudioFormat_Dolby_AC3_SPDIF, "Dolby Digital"),
        (MFAudioFormat_Dolby_DDPlus, "Dolby Digital Plus"),
        (MFAudioFormat_Dolby_AC4, "Dolby AC-4"),
        (TRUEHD, "Dolby TrueHD"),
        (MFAudioFormat_FLAC, "FLAC"),
        (MFAudioFormat_Opus, "Opus"),
        (MFAudioFormat_ALAC, "ALAC"),
        (MFAudioFormat_AAC, "AAC"),
        (MFAudioFormat_MP3, "MP3"),
        (MFAudioFormat_MPEG, "MPEG audio"),
    ];
    if dts.contains(&s) {
        return Some("DTS");
    }
    named.iter().find(|(g, _)| *g == s).map(|(_, n)| *n)
}

fn video_name(s: GUID) -> (Option<&'static str>, Option<&'static str>) {
    let table = [
        (MFVideoFormat_HEVC, "HEVC", Some("HEVC Video Extensions")),
        (MFVideoFormat_AV1, "AV1", Some("AV1 Video Extension")),
        (MFVideoFormat_VP90, "VP9", Some("VP9 Video Extensions")),
        (
            MFVideoFormat_MPEG2,
            "MPEG-2",
            Some("MPEG-2 Video Extension"),
        ),
        (MFVideoFormat_Theora, "Theora", Some("Web Media Extensions")),
        (MFVideoFormat_H264, "H.264", None),
        (MFVideoFormat_VP80, "VP8", None),
        (MFVideoFormat_MP4V, "MPEG-4 Part 2", None),
        (MFVideoFormat_MJPG, "Motion JPEG", None),
        (MFVideoFormat_WMV3, "WMV 9", None),
    ];
    table
        .iter()
        .find(|(g, _, _)| *g == s)
        .map_or((None, None), |(_, n, e)| (Some(*n), *e))
}

#[cfg(test)]
mod tests;
