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

use core::ops::ControlFlow;

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

/// Walk one box level of an in-memory `buf` (the `&[u8]` shape; the `IStream`-seeking walkers
/// keep their own loop): each box's type, its body after the 8- or 16-byte header, and the
/// whole box go to `visit`, which returns `Break` to stop with a value. A `size32 == 1` box
/// reads its 64-bit size from the extended field and is stepped over like any other, so the
/// metadata boxes AFTER a large `mdat` are still reached. The walk ends at the first box whose
/// size does not fit `buf`, exactly where every hand-written copy of this loop ended.
pub(crate) fn for_each_box<B>(
    buf: &[u8],
    mut visit: impl FnMut(&[u8], &[u8], &[u8]) -> ControlFlow<B>,
) -> Option<B> {
    let mut p = 0usize;
    while p + 8 <= buf.len() {
        let size32 = u32::from_be_bytes(buf[p..p + 4].try_into().ok()?);
        let typ = &buf[p + 4..p + 8];
        let extended = if size32 == 1 {
            Some(u64::from_be_bytes(buf.get(p + 8..p + 16)?.try_into().ok()?))
        } else {
            None
        };
        let (full, hdr) = decode_box_size(size32, extended, p as u64, buf.len() as u64)?;
        let (full, hdr) = (full as usize, hdr as usize);
        let end = p + full;
        if let ControlFlow::Break(found) = visit(typ, &buf[p + hdr..end], &buf[p..end]) {
            return Some(found);
        }
        p = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn the_walk_hands_each_box_its_type_body_and_whole() {
        let mut buf = boxed(b"ftyp", b"avif");
        buf.extend(boxed(b"meta", &[0, 0, 0, 0, 1, 2, 3]));
        let mut seen = Vec::new();
        let stopped = for_each_box(&buf, |typ, body, whole| {
            seen.push((typ.to_vec(), body.len(), whole.len()));
            ControlFlow::<()>::Continue(())
        });
        assert_eq!(stopped, None);
        assert_eq!(
            seen,
            vec![(b"ftyp".to_vec(), 4, 12), (b"meta".to_vec(), 7, 15)]
        );
    }

    #[test]
    fn a_break_stops_the_walk_with_its_value() {
        let mut buf = boxed(b"aaaa", b"");
        buf.extend(boxed(b"pitm", &[0, 0, 0, 0, 0, 7]));
        buf.extend(boxed(b"zzzz", b""));
        let mut visited = 0;
        let found = for_each_box(&buf, |typ, body, _| {
            visited += 1;
            if typ == b"pitm" {
                ControlFlow::Break(body[5])
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(found, Some(7));
        assert_eq!(visited, 2, "the box after the break is never visited");
    }

    #[test]
    fn a_64_bit_size_is_stepped_over_and_the_box_after_it_is_reached() {
        let mut buf = 1u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&20u64.to_be_bytes()); // 16-byte header + 4 payload bytes
        buf.extend_from_slice(&[9, 9, 9, 9]);
        buf.extend(boxed(b"colr", b"nclx"));
        let mut types = Vec::new();
        for_each_box(&buf, |typ, body, _| {
            types.push((typ.to_vec(), body.to_vec()));
            ControlFlow::<()>::Continue(())
        });
        assert_eq!(
            types,
            vec![
                (b"mdat".to_vec(), vec![9, 9, 9, 9]),
                (b"colr".to_vec(), b"nclx".to_vec())
            ]
        );
    }

    #[test]
    fn a_size_that_overruns_the_buffer_ends_the_walk_without_a_panic() {
        let mut buf = boxed(b"aaaa", b"");
        buf.extend_from_slice(&500u32.to_be_bytes());
        buf.extend_from_slice(b"huge");
        buf.extend_from_slice(&[1, 2, 3, 4]);
        let mut types = Vec::new();
        let r = for_each_box(&buf, |typ, _, _| {
            types.push(typ.to_vec());
            ControlFlow::<()>::Continue(())
        });
        assert_eq!(r, None);
        assert_eq!(types, vec![b"aaaa".to_vec()]);
    }

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
