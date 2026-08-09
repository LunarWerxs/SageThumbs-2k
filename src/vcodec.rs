//! Which video codec a container holds, and whether THIS Windows can decode it — the
//! diagnosis behind "video thumbnail not showing" (the top uninstall reason we can actually
//! see). Frames decode via the OS Media Foundation codecs (`crate::video`), and Windows does
//! not ship every codec: HEVC and AV1 are Microsoft Store add-ons, so an .mkv full of
//! HEVC rips fails every decode tier while the file, the container parser, and our
//! registration are all perfectly healthy. Without naming the codec, that failure is
//! indistinguishable from a bug in us.
//!
//! Identification is pure Rust over the container's own metadata — the Matroska `CodecID`
//! (`crate::mkv`) or the ISO-BMFF `stsd` sample-entry fourcc (`crate::mp4`) — no MF calls,
//! no decode attempt. The presence probe then asks MF's own registry (`MFTEnumEx`) whether
//! ANY decoder claims the codec's subtype, which answers "is it installed" without touching
//! the file. Both are `doctor`-only paths, never on the thumbnail hot path.

use std::io::{Read, Seek};

use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::{
    MFMediaType_Video, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_MJPG, MFVideoFormat_MP4V, MFVideoFormat_MPEG2, MFVideoFormat_Theora,
    MFVideoFormat_VP80, MFVideoFormat_VP90, MFVideoFormat_WMV3, MFT_CATEGORY_VIDEO_DECODER,
    MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO,
};

/// What we learned about the video codec inside a container.
pub struct CodecInfo {
    /// The container's own identifier — a Matroska CodecID ("V_MPEGH/ISO/HEVC") or an
    /// ISO-BMFF sample-entry fourcc ("hvc1"). Shown raw so a report is greppable.
    pub raw: String,
    /// Human name ("HEVC (H.265)"), or the raw id again for codecs we don't map.
    pub name: String,
    /// The MF subtype to probe with [`decoder_installed`]. `None` when the codec has no
    /// standard MF mapping (Theora, ProRes, VfW passthrough) — no Windows decoder exists,
    /// so there is nothing to probe and nothing to install.
    pub subtype: Option<GUID>,
    /// Whether the id is one we recognize. Distinguishes "known codec with no Windows
    /// decoder" (a firm finding) from "id we've never seen" (only a shrug) in reports.
    pub known: bool,
    /// What to install when the probe says no decoder is present. `None` for codecs whose
    /// decoder ships with Windows (its absence then means an "N" edition, which the Media
    /// Foundation presence check reports separately).
    pub install_hint: Option<&'static str>,
}

const HINT_HEVC: &str = "install 'HEVC Video Extensions' from the Microsoft Store (Microsoft \
                         sells it separately; Windows does not ship an HEVC decoder)";
const HINT_AV1: &str = "install the free 'AV1 Video Extension' from the Microsoft Store";
const HINT_VP9: &str = "install the free 'VP9 Video Extensions' from the Microsoft Store";
const HINT_MPEG2: &str = "install the free 'MPEG-2 Video Extension' from the Microsoft Store";
const HINT_THEORA: &str = "install the free 'Web Media Extensions' from the Microsoft Store";

/// Identify the video codec of `r` — a Matroska/WebM or ISO-BMFF (MP4/MOV) source. Each
/// container parser self-gates on its magic, so trying both in turn is cheap. `None`:
/// some other container (AVI/WMV/…) or no video track found.
pub fn identify<R: Read + Seek>(r: &mut R) -> Option<CodecInfo> {
    if let Some(id) = crate::mkv::video_codec_id(r) {
        return Some(from_mkv_codec_id(&id));
    }
    if let Some(fourcc) = crate::mp4::video_codec_fourcc(r) {
        return Some(from_mp4_fourcc(fourcc));
    }
    None
}

/// The embedded cover image of a video file, whichever container it is: a Matroska
/// attachment ([`crate::mkv::attached_cover`]) or an MP4/MOV `covr` item
/// ([`crate::mp4::cover_art`]). `None` when there is none, or for a container with no
/// cover concept (AVI, WMV, MPEG-TS).
///
/// Why this matters beyond convenience: it is the only thumbnail a file can have when the
/// OS lacks its codec, which for HEVC means "unless the user buys the Store extension".
/// A poster is also frequently a BETTER tile than a frame for a film, which is why it is
/// offered as a preference (`settings::prefer_cover_art`) and not only as a fallback.
pub fn cover_art<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    crate::mkv::attached_cover(r).or_else(|| crate::mp4::cover_art(r))
}

/// Map a Matroska CodecID to a display name + MF subtype + install hint.
fn from_mkv_codec_id(id: &str) -> CodecInfo {
    let (name, subtype, hint): (&str, Option<GUID>, Option<&'static str>) = match id {
        "V_MPEG4/ISO/AVC" => ("H.264 (AVC)", Some(MFVideoFormat_H264), None),
        "V_MPEGH/ISO/HEVC" => ("HEVC (H.265)", Some(MFVideoFormat_HEVC), Some(HINT_HEVC)),
        "V_AV1" => ("AV1", Some(MFVideoFormat_AV1), Some(HINT_AV1)),
        "V_VP9" => ("VP9", Some(MFVideoFormat_VP90), Some(HINT_VP9)),
        "V_VP8" => ("VP8", Some(MFVideoFormat_VP80), None),
        "V_MPEG2" => ("MPEG-2", Some(MFVideoFormat_MPEG2), Some(HINT_MPEG2)),
        "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/AP" => {
            ("MPEG-4 Part 2", Some(MFVideoFormat_MP4V), None)
        }
        "V_MJPEG" => ("Motion JPEG", Some(MFVideoFormat_MJPG), None),
        "V_MS/VFW/FOURCC" => ("VfW-compatibility codec", None, None),
        "V_THEORA" => ("Theora", Some(MFVideoFormat_Theora), Some(HINT_THEORA)),
        "V_PRORES" => ("ProRes", None, None),
        other => {
            return CodecInfo {
                raw: other.to_string(),
                name: other.to_string(),
                subtype: None,
                known: false,
                install_hint: None,
            }
        }
    };
    CodecInfo {
        raw: id.to_string(),
        name: name.to_string(),
        subtype,
        known: true,
        install_hint: hint,
    }
}

/// Map an ISO-BMFF `stsd` sample-entry fourcc to a display name + MF subtype + install hint.
fn from_mp4_fourcc(fourcc: [u8; 4]) -> CodecInfo {
    let raw = String::from_utf8_lossy(&fourcc).to_string();
    let (name, subtype, hint): (&str, Option<GUID>, Option<&'static str>) = match &fourcc {
        b"avc1" | b"avc3" => ("H.264 (AVC)", Some(MFVideoFormat_H264), None),
        // dvh1/dvhe are Dolby Vision carried in HEVC sample entries — same decoder question.
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => {
            ("HEVC (H.265)", Some(MFVideoFormat_HEVC), Some(HINT_HEVC))
        }
        b"av01" => ("AV1", Some(MFVideoFormat_AV1), Some(HINT_AV1)),
        b"vp09" => ("VP9", Some(MFVideoFormat_VP90), Some(HINT_VP9)),
        b"vp08" => ("VP8", Some(MFVideoFormat_VP80), None),
        b"mp4v" => ("MPEG-4 Part 2", Some(MFVideoFormat_MP4V), None),
        b"mjpa" | b"mjpb" | b"jpeg" => ("Motion JPEG", Some(MFVideoFormat_MJPG), None),
        b"wmv3" => ("WMV 9", Some(MFVideoFormat_WMV3), None),
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => ("ProRes", None, None),
        _ => {
            return CodecInfo {
                raw: raw.clone(),
                name: raw,
                subtype: None,
                known: false,
                install_hint: None,
            }
        }
    };
    CodecInfo {
        raw,
        name: name.to_string(),
        subtype,
        known: true,
        install_hint: hint,
    }
}

/// Does this Windows have ANY registered Media Foundation decoder (software or hardware)
/// for `subtype`? `None` when Media Foundation itself is absent ("N"/"KN" editions — the
/// caller reports that as its own, bigger finding). Asks the MFT registry; never decodes.
pub fn decoder_installed(subtype: GUID) -> Option<bool> {
    if !crate::video::media_foundation_available() {
        return None;
    }
    unsafe {
        let _session = crate::video::MfSession::start()?;
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };
        let mut activates = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT
                | MFT_ENUM_FLAG_ASYNCMFT
                | MFT_ENUM_FLAG_HARDWARE
                | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut activates,
            &mut count,
        )
        .ok()?;
        // The out array is CoTaskMemAlloc'd COM refs: release each, then free the array.
        if !activates.is_null() {
            for i in 0..count as usize {
                drop((*activates.add(i)).take());
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(activates.cast()));
        }
        Some(count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mkv_codec_ids_map() {
        let hevc = from_mkv_codec_id("V_MPEGH/ISO/HEVC");
        assert_eq!(hevc.name, "HEVC (H.265)");
        assert!(hevc.subtype.is_some() && hevc.install_hint.is_some());
        let theora = from_mkv_codec_id("V_THEORA");
        assert!(theora.subtype.is_some() && theora.install_hint.is_some());
        let prores = from_mkv_codec_id("V_PRORES");
        assert!(prores.known && prores.subtype.is_none() && prores.install_hint.is_none());
        // Unmapped ids surface raw instead of vanishing.
        assert_eq!(from_mkv_codec_id("V_QUICKTIME").name, "V_QUICKTIME");
    }

    #[test]
    fn mp4_fourccs_map() {
        assert_eq!(from_mp4_fourcc(*b"hvc1").name, "HEVC (H.265)");
        assert_eq!(from_mp4_fourcc(*b"av01").name, "AV1");
        assert_eq!(from_mp4_fourcc(*b"zzzz").name, "zzzz");
    }

    /// The probe must ANSWER on any machine with MF — H.264's decoder ships inbox on
    /// consumer SKUs, so `Some(_)` is the strong claim we can make everywhere (Server
    /// Core lacks it, hence not asserting the bool).
    #[test]
    fn h264_probe_answers_when_mf_present() {
        if !crate::video::media_foundation_available() {
            eprintln!("h264_probe: Media Foundation absent — skipping");
            return;
        }
        assert!(decoder_installed(MFVideoFormat_H264).is_some());
    }
}
