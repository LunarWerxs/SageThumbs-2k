//! A first frame out of an in-memory stream, as NV12 or as RGBA.

use super::*;

/// A decoded frame in the codec's own NV12 layout, colour conversion NOT applied.
pub struct Nv12Frame {
    /// The full contiguous NV12 buffer: `height` rows of Y, then `height/2` rows of
    /// interleaved UV, each row `stride` bytes.
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Decode the first frame and hand back the RAW NV12 pixels — no video processor, no
/// RGB conversion. For callers that must own the YUV→RGB matrix themselves.
///
/// Exists because of the 8-bit BT.601 AVIF path (`decode/avifmf.rs`): both the WIC HEIF
/// glue AND Media Foundation's video processor convert those with BT.709 coefficients
/// (measured: identical wrong numbers from each, worst channel 39/255), so the ONLY
/// component that can be trusted with the matrix is us, fed by the decoder's untouched
/// output. The source reader is created WITHOUT `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING`
/// on purpose: the AV1 decoder MFT emits NV12 natively, and enabling the processor would
/// put the component this function exists to bypass back into the chain.
pub fn nv12_frame_from_owned_bytes(owned: Vec<u8>) -> Option<Nv12Frame> {
    if !mf_usable() {
        return None;
    }
    grab_budgeted(move || unsafe {
        let _session = MfSession::start()?;
        let (reader, stream_index, w, h, stride) = nv12_reader_for_bytes(&owned)?;
        read_first_nv12_sample(&reader, stream_index, w, h, stride)
    })
}

/// Set up a source reader over `owned`, negotiate NV12 as the media type (no video
/// processing — the AV1 decoder MFT emits NV12 natively, and enabling the processor would
/// put the component this function exists to bypass back into the chain), and read back the
/// negotiated frame dimensions and row stride. Returns `(reader, stream_index, w, h, stride)`.
pub(super) unsafe fn nv12_reader_for_bytes(
    owned: &[u8],
) -> Option<(IMFSourceReader, u32, u32, u32, u32)> {
    let stream = SHCreateMemStream(Some(owned))?;
    let bs = MFCreateMFByteStreamOnStream(&stream).ok()?;
    let reader = MFCreateSourceReaderFromByteStream(&bs, None).ok()?;
    let first = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    let want = MFCreateMediaType().ok()?;
    want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
    want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok()?;
    reader.SetCurrentMediaType(first, None, &want).ok()?;

    let cur = reader.GetCurrentMediaType(first).ok()?;
    let frame_size = cur.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
    let (w, h) = ((frame_size >> 32) as u32, frame_size as u32);
    if w == 0 || h == 0 || (w as u64) * (h as u64) > crate::decode::limits::MAX_PIXELS {
        return None;
    }
    let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(w);
    // A negative default stride means bottom-up, which NV12 never is; treat as w.
    let stride = if (stride as i32) < 0 {
        w
    } else {
        stride.max(w)
    };
    Some((reader, first, w, h, stride))
}

/// Read the first real sample from `reader` (bounded like `grab_reader`'s read loop — a
/// one-sample mini-MP4 has nothing to skip, and this path's callers build exactly that) and
/// copy out its NV12 bytes: `h` rows of Y then `h/2` rows of UV, `stride` bytes each.
pub(super) unsafe fn read_first_nv12_sample(
    reader: &IMFSourceReader,
    stream_index: u32,
    w: u32,
    h: u32,
    stride: u32,
) -> Option<Nv12Frame> {
    for _ in 0..16 {
        let mut flags: u32 = 0;
        let mut smp: Option<IMFSample> = None;
        if reader
            .ReadSample(
                stream_index,
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut smp),
            )
            .is_err()
        {
            return None;
        }
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return None;
        }
        let Some(sample) = smp else { continue };
        let Ok(buf) = sample.ConvertToContiguousBuffer() else {
            continue;
        };
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        if buf.Lock(&mut ptr, None, Some(&mut len)).is_err() {
            continue;
        }
        // NV12: `h` rows of Y then `h/2` rows of UV, `stride` bytes each. A short
        // buffer means the stride guess is wrong — refuse rather than misread.
        let need = (stride as usize) * (h as usize) * 3 / 2;
        let data = if (len as usize) >= need {
            Some(std::slice::from_raw_parts(ptr, need).to_vec())
        } else {
            None
        };
        let _ = buf.Unlock();
        return data.map(|data| Nv12Frame {
            data,
            width: w,
            height: h,
            stride,
        });
    }
    None
}

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
