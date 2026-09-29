//! A first frame out of an in-memory stream, as RGBA.

use super::*;

/// As [`frame_from_bytes`], but takes ownership of the buffer instead of cloning it —
/// `grab_budgeted`'s `'static` bound needs an owned buffer to move onto its worker thread
/// either way, so a caller that already has one should hand it over directly.
pub fn frame_from_owned_bytes(owned: Vec<u8>) -> Option<DynamicImage> {
    // Media Foundation is delay-loaded; calling into it when absent would raise a
    // structured exception under `panic = "abort"`. See `media_foundation_available`, and
    // `mf_usable` for the wedged-host half of the gate (issue #35).
    if !mf_usable() {
        return None;
    }
    grab_budgeted(move || unsafe {
        let stream = SHCreateMemStream(Some(&owned))?;
        let bs = MFCreateMFByteStreamOnStream(&stream).ok()?;
        // The buffer is either a bounded head prefix / remux (reach only EARLY frames — stay
        // near the head, 10% capped at 3s) or a one-keyframe mini-MP4 from `crate::mp4` (a
        // single sample, so the 10% seek of its ~one-frame duration is a harmless no-op and we
        // grab that keyframe directly). Both are served by the same near-the-head plan.
        grab(
            &bs,
            Seek {
                frac: 0.10,
                cap_hns: Some(MAX_SEEK_HNS),
            },
        )
    })
}

/// Grab a frame from a full in-memory buffer at the TRUE representative mark
/// ([`st2k_base::settings::video_offset_frac`], no depth cap). For callers that hold the WHOLE file
/// in RAM (the size-capped CLI read), so MF can seek freely via the container's own index —
/// unlike [`frame_from_bytes`], whose 3 s cap assumes a bounded head prefix. Used as the
/// CLI/preview fallback for non-MP4/MKV containers.
pub fn frame_from_bytes_repr(bytes: &[u8]) -> Option<DynamicImage> {
    // Media Foundation is delay-loaded; calling into it when absent would raise a
    // structured exception under `panic = "abort"`. See `media_foundation_available`, and
    // `mf_usable` for the wedged-host half of the gate (issue #35).
    if !mf_usable() {
        return None;
    }
    let owned = bytes.to_vec();
    grab_budgeted(move || unsafe {
        let stream = SHCreateMemStream(Some(&owned))?;
        let bs = MFCreateMFByteStreamOnStream(&stream).ok()?;
        grab(
            &bs,
            Seek {
                frac: st2k_base::settings::video_offset_frac(),
                cap_hns: None,
            },
        )
    })
}
