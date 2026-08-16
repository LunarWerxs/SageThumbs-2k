//! Shared box-header size arithmetic for the `[size(4)][type(4)][largesize(8)?][payload]`
//! shape that recurs across every ISO-BMFF-family parser in this codebase: the MP4/MOV
//! `moov`/`mdat` walkers, the HEIC/AVIF `colr`/`iprp`/HEVC-alpha probes, ImageMagick's `mini`
//! AVIF router, and the HEIC EXIF/XMP item locator. Every one of those independently
//! reimplemented this size arithmetic, and the guard strength had drifted between copies —
//! some checked a hostile size against the header length it actually needed (8 vs 16 bytes),
//! others checked it against a hardcoded 8 regardless. This is the one, correctly-guarded
//! version; callers keep their own iteration shape (recursive/depth-capped, IStream-seeking,
//! absolute-offset, …) and hand the decoded header fields here for the checked arithmetic.
//!
//! Deliberately pure and I/O-free: callers read the header bytes themselves (from a `&[u8]`
//! slice or from an `IStream`/`Read + Seek` source — the two shapes this codebase needs) and
//! only reach for the 8-byte extended-size field when `size32 == 1` actually requires it.

/// Decode one box header's size fields into `(full_size_including_header, header_len)`.
///
/// `size32` is the box's big-endian 4-byte size field, already read by the caller. `extended`
/// is the following 8 bytes (big-endian, as a `u64`) — pass `Some` only when the caller has
/// them available; `size32 == 1` with `extended: None` is treated as "this box's real size is
/// unknown" and rejected, which lets a caller that never bothers reading the extended field
/// (because its format never needs 64-bit boxes) simply always pass `None` and get the same
/// "decline on a 64-bit box" behaviour it already had. `pos`/`total` are this box's start
/// offset and the enclosing buffer/stream's total length.
///
/// - `size32 == 0` means "this box runs to the end of the enclosing container" (`total - pos`).
/// - `size32 == 1` means the real size is the 64-bit `extended` field that follows the 8-byte
///   header (a 16-byte header total) — required for boxes bigger than 4 GiB.
/// - Anything else is the literal size (an 8-byte header).
///
/// `None` covers every way a hostile or truncated size can misbehave: a missing `extended`
/// field when one is required, a `full` shorter than the header it must itself contain (a box
/// cannot be smaller than its own header), and — the one that matters under
/// `overflow-checks = off` in the release profile — `pos + full` overflowing rather than
/// landing past `total`, which `checked_add` catches instead of silently wrapping.
pub(crate) fn decode_box_size(
    size32: u32,
    extended: Option<u64>,
    pos: u64,
    total: u64,
) -> Option<(u64, u64)> {
    let (full, header_len): (u64, u64) = match size32 {
        0 => (total.checked_sub(pos)?, 8),
        1 => (extended?, 16),
        n => (n as u64, 8),
    };
    if full < header_len {
        return None;
    }
    let end = pos.checked_add(full)?;
    if end > total {
        return None;
    }
    Some((full, header_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_zero_extends_to_the_end_of_the_container() {
        assert_eq!(decode_box_size(0, None, 100, 500), Some((400, 8)));
    }

    #[test]
    fn size_one_uses_the_extended_64_bit_field() {
        assert_eq!(decode_box_size(1, Some(1000), 0, 1000), Some((1000, 16)));
    }

    #[test]
    fn size_one_without_an_extended_field_is_declined() {
        // A caller that never bothers reading the extended 8 bytes (because its format never
        // needs 64-bit boxes, e.g. `avif_wic_misreads_color`) always passes `None` here and
        // must get a clean decline, not a panic on an absent field.
        assert_eq!(decode_box_size(1, None, 0, 1000), None);
    }

    #[test]
    fn literal_size_is_used_as_is() {
        assert_eq!(decode_box_size(40, None, 0, 1000), Some((40, 8)));
    }

    #[test]
    fn rejects_a_box_shorter_than_its_own_header() {
        // size32 == 1 (needs a 16-byte header) but the extended field claims only 10 bytes total.
        assert_eq!(decode_box_size(1, Some(10), 0, 1000), None);
        // Ordinary 8-byte header, claimed total size of 4 — shorter than the header alone.
        assert_eq!(decode_box_size(4, None, 0, 1000), None);
    }

    #[test]
    fn rejects_a_box_that_would_end_past_the_container() {
        assert_eq!(decode_box_size(600, None, 100, 500), None);
    }

    #[test]
    fn checked_add_catches_a_hostile_size_that_would_wrap_pos_plus_full() {
        // Under overflow-checks-off in release, `pos + full` would silently wrap; this is the
        // exact shape a crafted 64-bit extended size can produce, and it must be rejected
        // rather than wrapping to a small `end` that then passes the `end > total` check.
        assert_eq!(decode_box_size(1, Some(u64::MAX), 10, 1000), None);
    }
}
