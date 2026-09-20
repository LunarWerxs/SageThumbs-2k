//! TIFF / EXIF block surgery: read an IFD tree, keep what carries over, rebuild the block, reset orientation, drop the IFD1 thumbnail.

use super::*;

/// TIFF tags with a home of their own in [`Carried`]: the XMP packet, the IPTC record
/// and the ICC profile.
pub(super) const TAG_XMP: u16 = 0x02BC;

pub(super) const TAG_IPTC: u16 = 0x83BB;

pub(super) const TAG_ICC: u16 = 0x8773;

/// MakerNote (0x927C): dropped from a rebuilt block, because its contents hold offsets
/// relative to the original block that do not survive a rebuild.
pub(super) const TAG_MAKER_NOTE: u16 = 0x927C;

/// The Exif, GPS and Interoperability sub-IFD pointers.
pub(super) const TAG_EXIF_IFD: u16 = 0x8769;

pub(super) const TAG_GPS_IFD: u16 = 0x8825;

pub(super) const TAG_INTEROP_IFD: u16 = 0xA005;

/// The IFD0 entries a TIFF file's rebuilt EXIF block keeps: the TIFF Rev. 6.0 attribute
/// set the EXIF spec lists (description, camera, orientation, resolution, software,
/// date, artist, colour characteristics, copyright) plus the Exif and GPS pointers.
/// Everything that describes the pixel strips and tiles stays behind, so the block built
/// from these references no image data.
pub(super) const TIFF_IFD0_KEEP: &[u16] = &[
    0x010E,
    0x010F,
    0x0110,
    0x0112,
    0x011A,
    0x011B,
    0x0128,
    0x0131,
    0x0132,
    0x013B,
    0x013E,
    0x013F,
    0x0211,
    0x0213,
    0x0214,
    0x8298,
    TAG_EXIF_IFD,
    TAG_GPS_IFD,
];

/// Largest single value copied out of a TIFF directory; a bigger one (a preview image
/// stored as a tag, say) is left behind.
pub(super) const TIFF_VALUE_MAX: usize = 1024 * 1024;

/// Entries read from one directory at most.
pub(super) const TIFF_IFD_MAX_ENTRIES: usize = 512;

/// One TIFF directory entry with its value bytes in hand, plus the directory a sub-IFD
/// pointer leads to.
pub(super) struct TiffEntry {
    pub(super) tag: u16,
    pub(super) typ: u16,
    pub(super) count: u32,
    /// The raw value bytes, in the block's own byte order.
    pub(super) data: Vec<u8>,
    /// For an Exif, GPS or Interoperability pointer: the directory it points at.
    pub(super) sub: Option<Vec<TiffEntry>>,
}

/// The entries of the directory at `off`, values read in, sub-IFDs not followed. An
/// entry of an unknown type, an oversized value, or one whose value lies outside the
/// block is skipped; a directory whose entry table itself is cut off reads as `None`.
pub(super) fn tiff_read_ifd(tiff: &[u8], le: bool, off: usize) -> Option<Vec<TiffEntry>> {
    let count = tiff_u16(tiff, le, off)? as usize;
    if count > TIFF_IFD_MAX_ENTRIES {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let entry = off.checked_add(2 + i * 12)?;
        if let Some(e) = tiff_read_entry(tiff, le, entry)? {
            out.push(e);
        }
    }
    Some(out)
}

/// Read one directory entry's tag/type/count and value bytes from `tiff`. The outer
/// `None` when the entry table is cut off (a hard failure the caller propagates);
/// the inner `None` when the entry is one this walk skips (an unknown type, an
/// oversized or out-of-range value).
pub(super) fn tiff_read_entry(tiff: &[u8], le: bool, entry: usize) -> Option<Option<TiffEntry>> {
    let tag = tiff_u16(tiff, le, entry)?;
    let typ = tiff_u16(tiff, le, entry + 2)?;
    let count = tiff_u32(tiff, le, entry + 4)?;
    let Some(size) = tiff_type_size(typ) else {
        return Some(None);
    };
    let Some(total) = size.checked_mul(count as usize) else {
        return Some(None);
    };
    if total > TIFF_VALUE_MAX {
        return Some(None);
    }
    let data = if total <= 4 {
        tiff.get(entry + 8..entry + 8 + total)?.to_vec()
    } else {
        let o = tiff_u32(tiff, le, entry + 8)? as usize;
        match o.checked_add(total).and_then(|end| tiff.get(o..end)) {
            Some(v) => v.to_vec(),
            None => return Some(None),
        }
    };
    Some(Some(TiffEntry {
        tag,
        typ,
        count,
        data,
        sub: None,
    }))
}

/// Follow the Exif, GPS and Interoperability pointers in `entries`, attaching the
/// directory each leads to; a pointer whose directory cannot be read is dropped, as is
/// every MakerNote. `depth` bounds the walk to IFD0, Exif and Interoperability.
pub(super) fn tiff_attach_sub_ifds(tiff: &[u8], le: bool, entries: &mut Vec<TiffEntry>, depth: u8) {
    entries.retain(|e| e.tag != TAG_MAKER_NOTE);
    if depth >= 3 {
        entries.retain(|e| !SUB_IFD_TAGS.contains(&e.tag));
        return;
    }
    entries.retain_mut(|e| {
        if !SUB_IFD_TAGS.contains(&e.tag) {
            return true;
        }
        if e.typ != 4 || e.count != 1 {
            return false;
        }
        let Some(off) = e.data.first_chunk::<4>().map(|b| {
            if le {
                u32::from_le_bytes(*b)
            } else {
                u32::from_be_bytes(*b)
            }
        }) else {
            return false;
        };
        let Some(mut sub) = tiff_read_ifd(tiff, le, off as usize) else {
            return false;
        };
        tiff_attach_sub_ifds(tiff, le, &mut sub, depth + 1);
        if sub.is_empty() {
            return false;
        }
        e.sub = Some(sub);
        true
    });
}

/// Append the directory `entries` to `out` at its current (even) length, with the
/// out-of-line values and sub-directories after it, patching each entry's offset field
/// as they land. Entries go out in ascending tag order, as TIFF requires.
pub(super) fn tiff_write_ifd(out: &mut Vec<u8>, le: bool, entries: &mut [TiffEntry]) -> Option<()> {
    entries.sort_by_key(|e| e.tag);
    let n = entries.len();
    let base = out.len();
    out.resize(base + 2 + n * 12 + 4, 0); // count, entries, next-IFD pointer (none)
    tiff_put16(out, base, le, u16::try_from(n).ok()?)?;
    for (i, e) in entries.iter_mut().enumerate() {
        let at = base + 2 + i * 12;
        tiff_write_entry(out, le, at, e)?;
    }
    Some(())
}

/// Write one directory entry's tag/type/count at `at`, then its value: inline when it
/// fits in the 4-byte slot, otherwise appended out-of-line (or, for a sub-IFD, the
/// whole sub-directory written out-of-line) with the slot patched to its offset.
pub(super) fn tiff_write_entry(
    out: &mut Vec<u8>,
    le: bool,
    at: usize,
    e: &mut TiffEntry,
) -> Option<()> {
    tiff_put16(out, at, le, e.tag)?;
    tiff_put16(out, at + 2, le, e.typ)?;
    tiff_put32(out, at + 4, le, e.count)?;
    tiff_write_entry_value(out, le, at, e)
}

/// Write one entry's value field at `at + 8`: the whole sub-directory when the entry
/// is a sub-IFD pointer, the bytes inline when they fit the 4-byte slot, or appended
/// out-of-line with the slot patched to their offset.
pub(super) fn tiff_write_entry_value(
    out: &mut Vec<u8>,
    le: bool,
    at: usize,
    e: &mut TiffEntry,
) -> Option<()> {
    if let Some(sub) = e.sub.as_mut() {
        let off = tiff_pad_to_even(out);
        tiff_write_ifd(out, le, sub)?;
        tiff_put32(out, at + 8, le, u32::try_from(off).ok()?)
    } else if e.data.len() <= 4 {
        out.get_mut(at + 8..at + 8 + e.data.len())?
            .copy_from_slice(&e.data);
        Some(())
    } else {
        let off = tiff_pad_to_even(out);
        out.extend_from_slice(&e.data);
        tiff_put32(out, at + 8, le, u32::try_from(off).ok()?)
    }
}

/// Pad `out` to an even length (TIFF values are word-aligned) and return the offset
/// the next value written will land at.
pub(super) fn tiff_pad_to_even(out: &mut Vec<u8>) -> usize {
    if out.len() % 2 == 1 {
        out.push(0);
    }
    out.len()
}

/// Define a function writing an integer's fixed-width bytes at `at` in the file's byte
/// order, or `None` when `out` has no room for them.
macro_rules! tiff_put_int {
    ($name:ident, $t:ty, $n:expr) => {
        pub(super) fn $name(out: &mut [u8], at: usize, le: bool, v: $t) -> Option<()> {
            let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
            out.get_mut(at..at + $n)?.copy_from_slice(&b);
            Some(())
        }
    };
}

tiff_put_int!(tiff_put16, u16, 2);

tiff_put_int!(tiff_put32, u32, 4);

/// Wrap a raw IPTC-IIM record as the Photoshop image-resource block a JPEG APP13
/// segment carries: the `Photoshop 3.0` signature, one `8BIM` resource of id 0x0404
/// with an empty name, then the record, padded to an even length.
pub(super) fn iptc_as_photoshop_irb(iptc: &[u8]) -> Vec<u8> {
    let mut v = b"Photoshop 3.0\0".to_vec();
    v.extend_from_slice(b"8BIM");
    v.extend_from_slice(&0x0404u16.to_be_bytes());
    v.extend_from_slice(&[0, 0]); // empty Pascal name, padded to even
    v.extend_from_slice(&(iptc.len() as u32).to_be_bytes());
    v.extend_from_slice(iptc);
    if iptc.len() % 2 == 1 {
        v.push(0);
    }
    v
}

/// The metadata of a TIFF file. Its own IFD0 is the EXIF block, so the attribute
/// entries (camera, dates, orientation, the Exif and GPS directories) are copied into a
/// fresh block that references no pixel data, and the XMP, IPTC and ICC tags come out
/// as the packets they hold. The block keeps the file's byte order, so every value is
/// copied byte-for-byte.
pub(super) fn read_tiff(bytes: &[u8], out: &mut Carried) {
    let Some(le) = tiff_is_le(bytes) else {
        return;
    };
    if tiff_u16(bytes, le, 2) != Some(42) {
        return; // BigTIFF (43) has 8-byte offsets this walk does not read
    }
    let Some(ifd0) = tiff_u32(bytes, le, 4) else {
        return;
    };
    let Some(mut entries) = tiff_read_ifd(bytes, le, ifd0 as usize) else {
        return;
    };
    tiff_take_packets(&entries, out);
    entries.retain(|e| TIFF_IFD0_KEEP.contains(&e.tag));
    tiff_attach_sub_ifds(bytes, le, &mut entries, 0);
    if entries.is_empty() {
        return;
    }
    if let Some(block) = tiff_rebuild_block(le, &mut entries) {
        out.exif = Some(block);
    }
}

/// Lift the XMP, ICC and IPTC packets out of a TIFF file's IFD0 `entries` into `out`.
pub(super) fn tiff_take_packets(entries: &[TiffEntry], out: &mut Carried) {
    for e in entries {
        match e.tag {
            TAG_XMP if matches!(e.typ, 1 | 7) => out.xmp = Some(e.data.clone()),
            TAG_ICC if matches!(e.typ, 1 | 7) => out.icc = Some(e.data.clone()),
            TAG_IPTC if matches!(e.typ, 1 | 4 | 7) => {
                out.iptc = Some(iptc_as_photoshop_irb(&e.data));
            }
            _ => {}
        }
    }
}

/// A fresh TIFF block for the attribute `entries`: the byte-order magic, the IFD0
/// offset (8) and the directory itself. `None` when the directory could not be
/// written, so the caller keeps whatever block it had.
pub(super) fn tiff_rebuild_block(le: bool, entries: &mut [TiffEntry]) -> Option<Vec<u8>> {
    let mut block = if le {
        b"II*\0".to_vec()
    } else {
        b"MM\0*".to_vec()
    };
    block.extend_from_slice(&if le {
        8u32.to_le_bytes()
    } else {
        8u32.to_be_bytes()
    });
    tiff_write_ifd(&mut block, le, entries)
        .is_some()
        .then_some(block)
}

/// The byte order of a TIFF block, from its `II`/`MM` magic: `Some(true)` for
/// little-endian. `None` for anything that is not a TIFF header.
pub(super) fn tiff_is_le(tiff: &[u8]) -> Option<bool> {
    match tiff.first_chunk::<2>() {
        Some(b"II") => Some(true),
        Some(b"MM") => Some(false),
        _ => None,
    }
}

pub(super) fn tiff_u16(tiff: &[u8], le: bool, o: usize) -> Option<u16> {
    let v = tiff.get(o..o.checked_add(2)?)?.first_chunk::<2>()?;
    Some(if le {
        u16::from_le_bytes(*v)
    } else {
        u16::from_be_bytes(*v)
    })
}

pub(super) fn tiff_u32(tiff: &[u8], le: bool, o: usize) -> Option<u32> {
    let v = tiff.get(o..o.checked_add(4)?)?.first_chunk::<4>()?;
    Some(if le {
        u32::from_le_bytes(*v)
    } else {
        u32::from_be_bytes(*v)
    })
}

/// Rewrite IFD0's Orientation entry (tag 0x0112) to 1 ("normal"), in place.
///
/// The value is a single SHORT, which TIFF stores inline in the entry's own
/// 4-byte value field, so this never changes the block's length or any offset.
/// Best-effort: any parse surprise (bad byte-order marker, out-of-range offsets,
/// an entry shaped unlike a plain SHORT/count-1) leaves `tiff` untouched rather
/// than guess at a layout we do not recognise.
///
/// The one implementation for every caller: the carried block here, and the
/// lossless-rotate output in `encode.rs` (which keeps the source's own segment).
pub(in super::super) fn reset_orientation_to_1(tiff: &mut [u8]) {
    let Some(le) = tiff_is_le(tiff) else {
        return;
    };
    let Some(ifd0) = tiff_u32(tiff, le, 4).map(|v| v as usize) else {
        return;
    };
    let Some(count) = tiff_u16(tiff, le, ifd0) else {
        return;
    };
    for i in 0..count as usize {
        let entry = ifd0 + 2 + i * 12;
        if tiff_u16(tiff, le, entry) != Some(TAG_ORIENTATION) {
            continue;
        }
        // Type 3 (SHORT), count 1 - anything else is malformed; leave it alone
        // rather than guess at a layout we do not recognise.
        if tiff_u16(tiff, le, entry + 2) != Some(3) || tiff_u32(tiff, le, entry + 4) != Some(1) {
            return;
        }
        let one: [u8; 2] = if le {
            1u16.to_le_bytes()
        } else {
            1u16.to_be_bytes()
        };
        if let Some(slot) = tiff.get_mut(entry + 8..entry + 10) {
            slot.copy_from_slice(&one);
        }
        return;
    }
}

/// Byte size of one value of a TIFF field type (1..=12), or `None` for a type
/// this does not know, which makes the caller keep its hands off the block.
pub(super) fn tiff_type_size(t: u16) -> Option<usize> {
    Some(match t {
        1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,         // SHORT, SSHORT
        4 | 9 | 11 => 4,    // LONG, SLONG, FLOAT
        5 | 10 | 12 => 8,   // RATIONAL, SRATIONAL, DOUBLE
        _ => return None,
    })
}

/// Sub-IFD pointer tags reachable from IFD0: Exif (0x8769), GPS (0x8825) and, from
/// inside the Exif IFD, Interoperability (0xA005).
pub(super) const SUB_IFD_TAGS: [u16; 3] = [TAG_EXIF_IFD, TAG_GPS_IFD, TAG_INTEROP_IFD];

/// Drop the IFD1 thumbnail from a TIFF block, in place.
///
/// Clears IFD0's next-IFD pointer, so no reader finds IFD1, and then truncates the
/// block at IFD1's offset when every byte IFD0 and its sub-IFDs (Exif, GPS,
/// Interoperability) reference lies before it, which is where cameras and every
/// mainstream writer put the thumbnail. When anything referenced lies at or past
/// that offset the bytes are kept (unreachable but harmless) rather than risk
/// cutting a MakerNote or GPS value in half. Any parse surprise leaves the block
/// as it was.
pub(in super::super) fn drop_ifd1_thumbnail(tiff: &mut Vec<u8>) {
    let Some((le, next_ptr_at, ifd0, ifd1)) = tiff_ifd1_pointer(tiff) else {
        return;
    };
    let Some(slot) = tiff.get_mut(next_ptr_at..next_ptr_at + 4) else {
        return;
    };
    slot.fill(0);

    // Highest byte any reachable IFD entry touches. `None` means an entry was of a shape
    // this does not model, in which case the pointer reset above is all that happens.
    let Some(max_end) = tiff_reachable_end(tiff, le, ifd0, next_ptr_at + 4) else {
        return;
    };
    if ifd1 >= max_end && ifd1 <= tiff.len() {
        tiff.truncate(ifd1);
    }
}

/// IFD0's next-IFD pointer, when it points somewhere (byte order, the pointer's own
/// offset, IFD0's offset, IFD1's offset). `None` when the block doesn't parse this far
/// or there is no IFD1 to drop.
pub(super) fn tiff_ifd1_pointer(tiff: &[u8]) -> Option<(bool, usize, usize, usize)> {
    let le = tiff_is_le(tiff)?;
    let ifd0 = tiff_u32(tiff, le, 4)? as usize;
    let count = tiff_u16(tiff, le, ifd0)?;
    let next_ptr_at = (count as usize)
        .checked_mul(12)
        .and_then(|n| ifd0.checked_add(2 + n))?;
    let ifd1 = tiff_u32(tiff, le, next_ptr_at)? as usize;
    (ifd1 != 0).then_some((le, next_ptr_at, ifd0, ifd1))
}

/// Highest byte any entry in `ifd0` or its Exif/GPS/Interoperability sub-IFDs
/// references, starting no lower than `seed`. `None` on an entry of a shape this walk
/// does not model, or a sub-IFD chain deep enough to look like a loop.
pub(super) fn tiff_reachable_end(tiff: &[u8], le: bool, ifd0: usize, seed: usize) -> Option<usize> {
    let mut max_end = seed;
    let mut pending = vec![ifd0];
    let mut seen = 0usize;
    while let Some(ifd) = pending.pop() {
        seen += 1;
        if seen > 4 {
            return None; // IFD0 + three sub-IFDs is the whole EXIF tree; anything more is a loop
        }
        max_end = max_end.max(tiff_ifd_reach(tiff, le, ifd, &mut pending)?);
    }
    Some(max_end)
}

/// The highest byte one directory's own entry table or out-of-line values touch, and
/// the offsets of any Exif/GPS/Interoperability sub-IFD it points at (pushed onto
/// `pending`). `None` on an entry of a shape this walk does not model.
pub(super) fn tiff_ifd_reach(
    tiff: &[u8],
    le: bool,
    ifd: usize,
    pending: &mut Vec<usize>,
) -> Option<usize> {
    let n = tiff_u16(tiff, le, ifd)?;
    let mut end = (n as usize)
        .checked_mul(12)
        .and_then(|e| ifd.checked_add(2 + e + 4))?;
    for i in 0..n as usize {
        let entry = ifd + 2 + i * 12;
        end = end.max(tiff_entry_reach(tiff, le, entry, pending)?);
    }
    Some(end)
}

/// The highest byte one directory entry touches (0 when its value is inline), and the
/// offset of the Exif/GPS/Interoperability sub-IFD it points at (pushed onto
/// `pending`). `None` on an entry of a shape this walk does not model.
pub(super) fn tiff_entry_reach(
    tiff: &[u8],
    le: bool,
    entry: usize,
    pending: &mut Vec<usize>,
) -> Option<usize> {
    let (Some(tag), Some(typ), Some(cnt)) = (
        tiff_u16(tiff, le, entry),
        tiff_u16(tiff, le, entry + 2),
        tiff_u32(tiff, le, entry + 4),
    ) else {
        return None;
    };
    let size = tiff_type_size(typ)?;
    let total = size.checked_mul(cnt as usize)?;
    let mut end = 0;
    if total > 4 {
        let off = tiff_u32(tiff, le, entry + 8)? as usize;
        end = off.checked_add(total)?;
    }
    if SUB_IFD_TAGS.contains(&tag) && typ == 4 && cnt == 1 {
        if let Some(sub) = tiff_u32(tiff, le, entry + 8) {
            pending.push(sub as usize);
        }
    }
    Some(end)
}
