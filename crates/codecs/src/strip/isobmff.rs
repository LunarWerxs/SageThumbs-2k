//! HEIC / AVIF metadata: locating the EXIF and XMP items, neutralising them, and
//! spotting an HDR gain map.
//!
//! # Why this doesn't remove boxes
//!
//! An ISOBMFF metadata item is described in `iinf` (what it is) and located by
//! `iloc` (where its bytes are, as an **absolute file offset**). Actually deleting
//! an item means rewriting `iinf`, `iloc`, `iref` and `ipma`, and shifting every
//! later offset in the file - one arithmetic slip and the picture is destroyed,
//! silently, in place.
//!
//! So the item stays and its PAYLOAD is overwritten with a valid EMPTY one of the
//! **exact same length**: an EXIF item becomes a well-formed TIFF block with zero
//! entries, an XMP item becomes an empty xpacket padded with spaces. Not one
//! offset in the file moves, no box header changes, and the camera, GPS and edit
//! history are genuinely gone rather than merely unreferenced. A reader that
//! looks for the item still finds one; it just says nothing.
//!
//! Anything unexpected - a construction method other than "file offset", an
//! `iloc` version we don't know, an extent that runs past the end of the file -
//! makes the whole operation refuse. Corrupting someone's photo library is a much
//! worse outcome than declining to strip one file.

use crate::isobmff::{boxes, items, Item};

/// Overwrite the EXIF and XMP item payloads in place with valid empty ones.
///
/// Returns `None` when there is nothing to strip, or when any part of the layout
/// is not something we can rewrite without moving a byte (see the module docs).
pub(super) fn strip(bytes: &[u8]) -> Option<Vec<u8>> {
    let found = items(bytes);
    let targets: Vec<&Item> = found.iter().filter(|i| is_target(i)).collect();
    if targets.is_empty() || !targets_replaceable(&targets) || !targets_disjoint(bytes, &found) {
        return None;
    }
    let mut out = bytes.to_vec();
    for it in targets {
        let (off, len) = it.extent?;
        let slot = out.get_mut(off..off + len)?;
        if &it.kind == b"Exif" {
            write_empty_exif(slot);
        } else {
            write_empty_xmp(slot);
        }
    }
    Some(out)
}

/// An item [`strip`] rewrites: the EXIF item, or the XMP `mime` item.
fn is_target(i: &Item) -> bool {
    &i.kind == b"Exif" || (&i.kind == b"mime" && i.is_xmp)
}

/// Can every target be overwritten in place with a valid empty payload? Every target
/// must be locatable (or we refuse the whole file rather than half-strip it and report
/// success); an EXIF item too small to hold a valid empty TIFF cannot be replaced with
/// anything a reader will accept; and the same refusal holds for a too-short XMP item:
/// `write_empty_xmp` only writes its xpacket header when the slot is at least
/// `EMPTY.len()` bytes, so without this guard a short XMP item would be blanked to bare
/// spaces (no valid xpacket at all) while `strip` still reported success - exactly the
/// "half-fix" this module refuses to do.
fn targets_replaceable(targets: &[&Item]) -> bool {
    targets.iter().all(|i| {
        let min = if &i.kind == b"Exif" {
            MIN_EXIF_ITEM
        } else {
            MIN_XMP_ITEM
        };
        i.extent.is_some_and(|(_, l)| l >= min)
    })
}

/// Does no target overlap ANY other item's bytes, nor the `meta` box itself? `iloc` is
/// attacker-controlled: a crafted file can point an Exif item at another item's payload
/// (or at the box structure), and overwriting it in place would silently destroy data we
/// were never asked to touch. Both extents fit inside the file, so the bounds check alone
/// does not catch this.
///
/// The target itself is excluded by its INDEX in `found`, never by extent value: a crafted
/// Exif extent equal byte-for-byte to the image item's extent would otherwise exclude that
/// image item from the comparison too, and the overwrite would land on the picture. A
/// target with no extent answers false (`targets_replaceable` already refused it).
fn targets_disjoint(bytes: &[u8], found: &[Item]) -> bool {
    // Every top-level box, HEADER INCLUDED: pointing a target at any of them (ftyp, the
    // meta box itself, ...) would overwrite the box structure. Spans are reconstructed
    // from the body offsets `boxes` returns plus box contiguity; `mdat`'s BODY (where real
    // payloads legitimately live) is exempt, its header is not.
    let mut prev_end = 0usize;
    let structural: Vec<(usize, usize)> = boxes(bytes, 0)
        .into_iter()
        .map(|(typ, o, l)| {
            let start = prev_end;
            prev_end = o + l;
            let end = if &typ == b"mdat" { o } else { o + l };
            (start, end.saturating_sub(start))
        })
        .collect();
    let overlaps = |a: (usize, usize), b: (usize, usize)| a.0 < b.0 + b.1 && b.0 < a.0 + a.1;
    found
        .iter()
        .enumerate()
        .filter(|(_, t)| is_target(t))
        .all(|(ti, t)| {
            let Some(te) = t.extent else {
                return false;
            };
            let clear_of_items = !found
                .iter()
                .enumerate()
                .filter(|&(oi, _)| oi != ti)
                .filter_map(|(_, o)| o.extent)
                .any(|e| overlaps(te, e));
            clear_of_items && !structural.iter().any(|&s| overlaps(te, s))
        })
}

/// The 18 bytes an empty HEIF EXIF item needs: a 4-byte TIFF-header offset plus a
/// minimal zero-entry IFD0. An item smaller than this cannot be replaced with
/// anything well-formed, so [`strip`] refuses the file instead of leaving a blob
/// of zeroes behind and calling it success.
const MIN_EXIF_ITEM: usize = 18;

/// The 48 bytes an empty XMP item needs: exactly [`write_empty_xmp`]'s `EMPTY` xpacket,
/// which is what the slot gets replaced with. An item smaller than this cannot hold a
/// valid xpacket, so [`strip`] refuses the file instead of leaving pure whitespace where
/// a reader expects one (mirrors the `MIN_EXIF_ITEM` guard above).
const MIN_XMP_ITEM: usize = 48;

/// A HEIF EXIF item is a 4-byte TIFF-header offset followed by the TIFF block.
/// Replace it with a well-formed IFD0 that has zero entries, then pad.
fn write_empty_exif(slot: &mut [u8]) {
    slot.fill(0);
    const EMPTY: [u8; 18] = [
        0, 0, 0, 0, // tiff_header_offset = 0
        b'I', b'I', 42, 0, // little-endian TIFF magic
        8, 0, 0, 0, // IFD0 at offset 8
        0, 0, // zero entries
        0, 0, 0, 0, // no next IFD
    ];
    if slot.len() >= EMPTY.len() {
        slot[..EMPTY.len()].copy_from_slice(&EMPTY);
    }
}

/// Replace an XMP packet with an empty one, space-padded to the original length
/// (trailing whitespace inside an xpacket is legal padding, which is exactly what
/// the format's own writers use).
fn write_empty_xmp(slot: &mut [u8]) {
    const EMPTY: &[u8] = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"></x:xmpmeta>";
    slot.fill(b' ');
    if slot.len() >= EMPTY.len() {
        slot[..EMPTY.len()].copy_from_slice(EMPTY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isobmff::testutil::{infe, synth};

    #[test]
    fn strip_erases_payloads_without_moving_a_byte() {
        // The XMP payload must be at least MIN_XMP_ITEM (48) bytes so this exercises the
        // real successful-strip path rather than the new too-short refusal below.
        let (file, _) = synth(
            &[
                (1, b"II*\0secret-camera-data"),
                (
                    2,
                    b"<x:xmpmeta>gps-coordinates-secret-location-data</x:xmpmeta>",
                ),
            ],
            &[],
        );
        let out = strip(&file).expect("strip refused");
        assert_eq!(out.len(), file.len(), "file length changed");
        assert!(
            !out.windows(11).any(|w| w == b"secret-came"),
            "EXIF payload survived"
        );
        assert!(!out.windows(3).any(|w| w == b"gps"), "XMP payload survived");
        // The boxes themselves are untouched, so the items are still discoverable.
        assert_eq!(items(&out).len(), 2);
    }

    #[test]
    fn refuses_when_xmp_item_is_too_short_for_a_valid_xpacket() {
        // A 14-byte XMP payload is well under MIN_XMP_ITEM (48). Before this guard,
        // write_empty_xmp would silently blank it to bare spaces (no xpacket header at
        // all) and strip() still reported success — the "half-fix" the module's own docs
        // say never to do.
        let (file, _) = synth(
            &[(1, b"II*\0secret-camera-data"), (2, b"<x:xmpmeta>gps")],
            &[],
        );
        assert!(strip(&file).is_none());
    }

    #[test]
    fn refuses_when_an_item_cannot_be_located() {
        // Declares Exif + XMP in iinf but gives iloc only ONE of them: a partial
        // strip that reported success would be a silent metadata leak.
        let (file, _) = synth(&[(1, b"II*\0secret")], &[]);
        assert!(strip(&file).is_none());
    }

    /// A crafted Exif extent IDENTICAL to the image item's extent used to slip past the
    /// overlap guard: the "not myself" filter compared extents by value, so the image
    /// item was excluded from the comparison along with the target, and the empty EXIF
    /// block landed on the picture bytes. The target is excluded by index now.
    #[test]
    fn refuses_when_a_target_extent_equals_another_items_extent() {
        let xmp = b"<x:xmpmeta>gps-coordinates-secret-location-data</x:xmpmeta>";
        let (file, spots) = synth(
            &[
                (1, b"II*\0secret-camera-data"),
                (2, xmp),
                (9, b"av01-primary-image-payload"),
            ],
            &[infe(9, b"av01", None)],
        );
        // Sanity: the honest layout strips.
        assert!(strip(&file).is_some(), "the untampered file must strip");

        // Rewrite item 1's iloc extent to item 9's (offset, length). iloc v1 entries here
        // are 16 bytes each: id(2) method(2) dref(2) extents(2) offset(4) length(4), in
        // payload order after a 6-byte FullBox header + size byte pair + 2-byte count.
        let iloc_at = file
            .windows(4)
            .position(|w| w == b"iloc")
            .expect("iloc box")
            + 4;
        let entries = iloc_at + 6 + 2;
        let exif_entry = entries;
        let image_entry = entries + 2 * 16;
        let mut crafted = file.clone();
        let (image_off, image_len) = (
            u32::from_be_bytes(
                crafted[image_entry + 8..image_entry + 12]
                    .try_into()
                    .unwrap(),
            ),
            u32::from_be_bytes(
                crafted[image_entry + 12..image_entry + 16]
                    .try_into()
                    .unwrap(),
            ),
        );
        assert_eq!(image_off as usize, spots[2].1, "entry layout assumption");
        crafted[exif_entry + 8..exif_entry + 12].copy_from_slice(&image_off.to_be_bytes());
        crafted[exif_entry + 12..exif_entry + 16].copy_from_slice(&image_len.to_be_bytes());
        let items_now = items(&crafted);
        assert_eq!(
            items_now
                .iter()
                .find(|i| &i.kind == b"Exif")
                .unwrap()
                .extent,
            items_now
                .iter()
                .find(|i| &i.kind == b"av01")
                .unwrap()
                .extent,
            "the crafted Exif extent must equal the image item's"
        );
        assert!(
            strip(&crafted).is_none(),
            "an Exif item aimed at the image payload must refuse the whole file"
        );
        assert!(
            crafted.windows(12).any(|w| w == b"av01-primary"),
            "strip must not have touched the input"
        );
    }

    #[test]
    fn non_isobmff_input_is_ignored() {
        assert!(strip(b"not a file at all").is_none());
    }
}
