//! Camera-RAW recognition + the bounded embedded-preview read.
//!
//! A RAW file puts a display JPEG near the front and then tens or hundreds of MiB of
//! sensor data, so the cascade reads only the head. Deciding that a stream really IS a
//! RAW (and not an ordinary TIFF, which must NOT take this shortcut) is the bulk of
//! this module: an Explorer stream often has no filename, so there is a structural
//! IFD parse behind the extension check.

use super::*;

pub(super) const RAW_PREFIX_BYTES: usize = 16 * 1024 * 1024;

pub(super) const RAW_SNIFF_BYTES: usize = 1024 * 1024;

pub(super) enum RawFastSource {
    Preview(Vec<u8>),
    Prefix(Vec<u8>, u64),
}

pub(super) fn raw_preview_size_allowed(size: u64, max_file_bytes: u64) -> bool {
    size > RAW_PREFIX_BYTES as u64 && size <= decode::effective_input_cap(max_file_bytes)
}

/// Read only a bounded RAW head and return its best complete embedded JPEG.
/// A RAW with no early preview retains the prefix so the existing bounded
/// whole-file fallback can append only the unread tail.
pub(super) unsafe fn raw_preview_fast(
    stream: &IStream,
    max_file_bytes: u64,
) -> Option<RawFastSource> {
    let raw_extension = stream_extension(stream).is_some_and(|ext| is_raw_extension(&ext));
    let size = stream_size(stream)?;
    if !raw_preview_size_allowed(size, max_file_bytes) {
        return None; // no I/O or allocation saving versus the normal bounded read
    }

    // Explorer commonly hands IInitializeWithStream an unnamed file stream. Prefer
    // the extension when its STATSTG exposes one, but retain a conservative content
    // fallback for those normal unnamed streams: RAW-specific signatures or
    // structurally parsed CFA/DNG IFD markers. A plain TIFF does not qualify.
    let sniff = stream_prefix(stream, RAW_SNIFF_BYTES)?;
    if !looks_like_raw_container(&sniff, raw_extension) {
        return None;
    }

    let prefix = stream_prefix(stream, RAW_PREFIX_BYTES)?;
    match decode::largest_embedded_jpeg(&prefix, decode::MIN_RAW_PREVIEW) {
        Some(jpeg) => Some(RawFastSource::Preview(jpeg.to_vec())),
        None => Some(RawFastSource::Prefix(prefix, size)),
    }
}

pub(super) fn is_raw_extension(ext: &str) -> bool {
    matches!(
        ext,
        "3fr"
            | "arw"
            | "bay"
            | "cap"
            | "cr2"
            | "cr3"
            | "crw"
            | "dcr"
            | "dcs"
            | "dng"
            | "drf"
            | "erf"
            | "fff"
            | "iiq"
            | "k25"
            | "kdc"
            | "mdc"
            | "mef"
            | "mos"
            | "mrw"
            | "nef"
            | "nrw"
            | "orf"
            | "ori"
            | "pef"
            | "ptx"
            | "pxn"
            | "raf"
            | "rw2"
            | "rwl"
            | "sr2"
            | "srf"
            | "srw"
            | "x3f"
    )
}

/// Common RAW signatures. Generic TIFF/BigTIFF magic is accepted only with a RAW
/// extension or RAW-specific metadata, because an Explorer stream often has no name.
pub(super) fn looks_like_raw_container(head: &[u8], raw_extension: bool) -> bool {
    let tiff = head.starts_with(b"II\x2A\0")
        || head.starts_with(b"MM\0\x2A")
        || head.starts_with(b"II\x2B\0")
        || head.starts_with(b"MM\0\x2B");
    (tiff && (raw_extension || tiff_has_raw_ifd_marker(head)))
        || head.starts_with(b"FUJIFILMCCD-RAW")
        || head.starts_with(b"FFF\0")
        || head.starts_with(b"FOVb")
        || head.starts_with(b"\0MRM")
        || head.starts_with(b"IIRO")
        || head.starts_with(b"MMOR")
        || head.starts_with(b"IIU\0")
        || (head.len() >= 12
            && &head[4..8] == b"ftyp"
            && (&head[8..12] == b"crx " || &head[8..12] == b"cr3 "))
}

/// Does this TIFF's IFD0 declare itself a REDUCED-RESOLUTION copy of another image in
/// the file (`NewSubfileType` bit 0, tag 0xFE = 1)?
///
/// **This is the TIFF spec saying "I am not the main picture", and the `image` crate
/// always decodes IFD0.** Camera RAW is where it bites: a Hasselblad `.3fr`, Kodak
/// `.dcr`/`.kdc`, Epson `.erf`, Phase One `.fff` and Nikon `.nef` all put a small
/// preview in IFD0 and the sensor data in SubIFDs. `image` decodes IFD0 happily, and
/// because it is the FIRST tier nothing better ever ran: a 768x512 Kodak KDC
/// thumbnailed from a 96x64 stamp, and a Kodak DCS760C `.dcr` from a 380x252
/// placeholder that is **essentially black** — a black tile for a perfectly good photo,
/// with every gate in the repo green (found 2026-08-21 by cross-checking the corpus
/// against an independent decoder).
///
/// Deliberately NOT the same question as [`tiff_has_raw_ifd_marker`]: that one looks for
/// CFA/DNG tags in IFD0 to recognise a RAW from a nameless shell stream, and these files
/// keep their CFA tags in the SubIFDs, which is exactly why it did not catch them.
///
/// Value 2 (a page of a multi-page document) and 4 (transparency mask) are NOT reduced
/// copies and must not match, or a normal multi-page TIFF would lose its fast tier.
/// It walks IFD0 the same shape as its sibling below rather than sharing a helper: that
/// one is fuzzed, load-bearing for the shell's nameless-stream routing, and asks a
/// different question. Twenty checked lines cost less than restructuring it.
pub(crate) fn tiff_ifd0_is_reduced(head: &[u8]) -> bool {
    // NOT extended to Phase One `.iiq` (TIFF + `IIII` at offset 8) or to a `SubIFDs` tag,
    // though both would be easy and both look right on paper. Measured A/B on 2026-08-21:
    // neither changes a single pixel, because those two formats already render the camera's
    // own embedded preview and the tiers that would run instead cannot do better (WIC
    // refuses `.mef` outright and hands back the same small preview for `.iiq`). Adding a
    // routing rule that provably does nothing is how a decoder accretes risk for free.
    let little = match head.get(..4) {
        Some(b"II\x2A\0") => true,
        Some(b"MM\0\x2A") => false,
        _ => return false, // BigTIFF's IFD layout differs; not worth a second walker.
    };
    let num = |offset: usize, wide: bool| -> Option<u32> {
        let n = if wide { 4 } else { 2 };
        let raw = head.get(offset..offset.checked_add(n)?)?;
        Some(match (wide, little) {
            (true, true) => u32::from_le_bytes(raw.try_into().ok()?),
            (true, false) => u32::from_be_bytes(raw.try_into().ok()?),
            (false, true) => u16::from_le_bytes(raw.try_into().ok()?) as u32,
            (false, false) => u16::from_be_bytes(raw.try_into().ok()?) as u32,
        })
    };
    let Some(ifd) = num(4, true).map(|v| v as usize) else {
        return false;
    };
    let Some(count) = num(ifd, false).map(|v| (v as usize).min(4096)) else {
        return false;
    };
    for index in 0..count {
        let Some(entry) = index
            .checked_mul(12)
            .and_then(|off| ifd.checked_add(2)?.checked_add(off))
        else {
            return false;
        };
        if num(entry, false) != Some(0x00FE) {
            continue;
        }
        // NewSubfileType is LONG (type 4) by the spec, but SHORT (3) is written in the
        // wild; anything else is malformed and ignored. Bit 0 set = reduced-resolution.
        let Some(type_offset) = entry.checked_add(2) else {
            return false;
        };
        let wide = match num(type_offset, false) {
            Some(4) => true,
            Some(3) => false,
            _ => continue,
        };
        // A SHORT is left-justified in the 4-byte value field in BOTH endiannesses, so
        // reading the first two bytes is right either way (same as the sibling scanner).
        if let Some(v) = entry.checked_add(8).and_then(|o| num(o, wide)) {
            return v & 1 == 1;
        }
    }
    false
}

pub(super) fn tiff_has_raw_ifd_marker(head: &[u8]) -> bool {
    let little = match head.get(..4) {
        Some(b"II\x2A\0") => true,
        Some(b"MM\0\x2A") => false,
        _ => return false, // BigTIFF needs an extension; its IFD layout differs.
    };
    let u16_at = |offset: usize| -> Option<u16> {
        let end = offset.checked_add(2)?;
        let bytes: [u8; 2] = head.get(offset..end)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let u32_at = |offset: usize| -> Option<u32> {
        let end = offset.checked_add(4)?;
        let bytes: [u8; 4] = head.get(offset..end)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };

    let Some(ifd) = u32_at(4).map(|value| value as usize) else {
        return false;
    };
    let Some(count) = u16_at(ifd).map(|value| (value as usize).min(4096)) else {
        return false;
    };
    for index in 0..count {
        let Some(entry) = index
            .checked_mul(12)
            .and_then(|offset| ifd.checked_add(2)?.checked_add(offset))
        else {
            return false;
        };
        let Some(tag) = u16_at(entry) else {
            return false;
        };
        match raw_ifd_entry_marker(tag, entry, &u16_at, &u32_at) {
            std::ops::ControlFlow::Break(found) => return found,
            std::ops::ControlFlow::Continue(()) => {}
        }
    }
    false
}

/// Check one IFD0 entry for either RAW-marker shape this scanner recognizes: a CFA/DNG tag, or
/// PhotometricInterpretation (tag 0x0106, count 1) reading as TIFF/EP CFA (32803) or LinearRaw
/// (34892). `Break(true)` means the marker was found and the caller should return `true`
/// immediately; `Break(false)` means a truncated/malformed read, matching the caller's own
/// `?`-shaped early `return false`s; `Continue` means keep scanning the next entry.
fn raw_ifd_entry_marker(
    tag: u16,
    entry: usize,
    u16_at: &impl Fn(usize) -> Option<u16>,
    u32_at: &impl Fn(usize) -> Option<u32>,
) -> std::ops::ControlFlow<bool> {
    use std::ops::ControlFlow::{Break, Continue};
    // CFA/DNG tags are structurally parsed from IFD0, not searched as arbitrary byte strings.
    // This prevents a normal camera-authored TIFF's EXIF/XMP maker text from accidentally
    // rerouting it through the RAW shortcut.
    if matches!(
        tag,
        0x828D | 0x828E | 0xC612 | 0xC614 | 0xC616 | 0xC61A | 0xC627
    ) {
        return Break(true);
    }
    let Some(type_offset) = entry.checked_add(2) else {
        return Break(false);
    };
    let Some(count_offset) = entry.checked_add(4) else {
        return Break(false);
    };
    let Some(value_offset) = entry.checked_add(8) else {
        return Break(false);
    };
    if tag != 0x0106 || u32_at(count_offset) != Some(1) {
        return Continue(());
    }
    let Some(field_type) = u16_at(type_offset) else {
        return Break(false);
    };
    let value = match field_type {
        3 => match u16_at(value_offset) {
            Some(value) => value as u32,
            None => return Break(false),
        },
        4 => match u32_at(value_offset) {
            Some(value) => value,
            None => return Break(false),
        },
        _ => return Continue(()),
    };
    // TIFF/EP CFA and LinearRaw photometric interpretations.
    if matches!(value, 32_803 | 34_892) {
        Break(true)
    } else {
        Continue(())
    }
}
