//! The ICC profile tag out of a TIFF IFD.

use crate::container::util::{tiff_u16, tiff_u32};

/// The ICC profile a classic TIFF carries in IFD0's tag 34675, read off the bytes directly.
/// The `image` crate's TIFF decoder answers `None` for the profile a 16-bit TIFF written by
/// ImageMagick carries (measured 2026-09-11 on `tests/fixtures/tiff/scene-pq2020.tif`, tag
/// present, type UNDEFINED, 25116 bytes), which sent an HDR TIFF's PQ samples to the tone map
/// as if they were sRGB. Bounded: one IFD, entries checked against the buffer, no BigTIFF
/// (magic 43 is declined rather than guessed at), profiles past 4 MiB ignored.
pub(in super::super) fn tiff_icc(bytes: &[u8]) -> Option<Vec<u8>> {
    const ICC_PROFILE: u16 = 34675;
    let little = match bytes.get(0..4)? {
        b"II\x2a\x00" => true,
        b"MM\x00\x2a" => false,
        _ => return None,
    };
    let ifd = tiff_u32(bytes, little, 4)? as usize;
    let entries = usize::from(tiff_u16(bytes, little, ifd)?);
    for i in 0..entries.min(512) {
        let at = ifd.checked_add(2 + i * 12)?;
        if tiff_u16(bytes, little, at)? == ICC_PROFILE {
            return tiff_entry_bytes(bytes, little, at);
        }
    }
    None
}

/// The value of one BYTE/UNDEFINED IFD entry at `at`: inline when it fits the four value
/// bytes, at the entry's offset otherwise. Any other type is declined.
pub(super) fn tiff_entry_bytes(bytes: &[u8], little: bool, at: usize) -> Option<Vec<u8>> {
    if !matches!(tiff_u16(bytes, little, at + 2)?, 1 | 7) {
        return None;
    }
    let count = tiff_u32(bytes, little, at + 4)? as usize;
    if count == 0 || count > 4 * 1024 * 1024 {
        return None;
    }
    let start = if count <= 4 {
        at + 8
    } else {
        tiff_u32(bytes, little, at + 8)? as usize
    };
    Some(bytes.get(start..start.checked_add(count)?)?.to_vec())
}
