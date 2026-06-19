//! Autodesk 3ds Max `.max` (and other OLE-compound docs) embedded thumbnail.
//!
//! `.max` is an OLE2 compound file. The viewport thumbnail lives in the
//! `\x05SummaryInformation` property set, property `PIDSI_THUMBNAIL` (PID `0x11`,
//! type `VT_CF`). 3ds Max writes a CUSTOM payload (clipboard tag `0xFFFFFFFF`):
//! a small header then top-down 24-bit RGB pixels at offset 98. Legacy Office /
//! Visio / Publisher instead write a standard `CF_DIB` (tag `8`) — we handle that
//! too, so this one extractor covers the whole OLE family. No SDK; uses the
//! pure-Rust [`super::ole`] reader. Verified against a real 3ds Max scene.

use image::{DynamicImage, RgbImage};

use super::ole;
use super::util::{dib_to_bmp, le16, le32};
use super::CoverOut;

const PIDSI_THUMBNAIL: u32 = 0x11;
const VT_CF: u32 = 0x0047;
const CF_METAFILEPICT: u32 = 3;
const CF_DIB: u32 = 8;
const CF_ENHMETAFILE: u32 = 14;
/// 3ds Max / Visio / Publisher all write the outer clipboard tag as 0xFFFFFFFF and
/// encode the real format inside the payload — see [`sentinel_payload`].
const SENTINEL: u32 = 0xFFFF_FFFF;
const MAX_DIM: u32 = 8192;

pub fn looks_like_max(head: &[u8]) -> bool {
    ole::looks_like_ole(head)
}

/// Extract the embedded thumbnail from an OLE compound file, or None.
pub fn extract(bytes: &[u8]) -> Option<CoverOut> {
    let s = ole::read_stream(bytes, "\u{5}SummaryInformation")?;

    // PropertySet: first section's byte offset is at +44 (after the 28-byte stream
    // header + the first 16-byte FMTID).
    let section = le32(&s, 44)? as usize;
    let num_props = le32(&s, section.checked_add(4)?)? as usize;
    let mut value_off = None;
    for i in 0..num_props.min(256) {
        let pair = section.checked_add(8)?.checked_add(i.checked_mul(8)?)?;
        let pid = le32(&s, pair)?;
        let poff = le32(&s, pair.checked_add(4)?)? as usize;
        if pid == PIDSI_THUMBNAIL {
            value_off = Some(section.checked_add(poff)?);
            break;
        }
    }
    let v = value_off?;
    if le32(&s, v)? != VT_CF {
        return None;
    }
    let cb = le32(&s, v.checked_add(4)?)? as usize; // size of tag + data
    let tag = le32(&s, v.checked_add(8)?)?;
    let data_len = cb.checked_sub(4)?;
    let data = s.get(v.checked_add(12)?..v.checked_add(12)?.checked_add(data_len)?)?;

    match tag {
        CF_DIB => super::util::decodable_image(dib_to_bmp(data)?).map(CoverOut::Bytes),
        // Some apps store CF_ENHMETAFILE directly: the data IS the EMF.
        CF_ENHMETAFILE => super::util::decodable_image(data.to_vec()).map(CoverOut::Bytes),
        SENTINEL => sentinel_payload(data),
        _ => None,
    }
}

/// The 0xFFFFFFFF clipboard "sentinel": the real format is nested in the payload.
/// Visio writes CF_ENHMETAFILE(14) + a complete EMF; Publisher writes
/// CF_METAFILEPICT(3) + an 8-byte METAFILEPICT + a standard WMF; 3ds Max writes its
/// own raw-RGB header (no nested clipboard id). All three are disambiguated by the
/// nested id plus a format signature, so the 3ds Max `u32(3)` first field can't be
/// mistaken for CF_METAFILEPICT.
fn sentinel_payload(data: &[u8]) -> Option<CoverOut> {
    let inner = le32(data, 0)?;
    // Visio: CF_ENHMETAFILE, then the EMF ("·EMF" signature at EMF offset 40).
    if inner == CF_ENHMETAFILE && data.get(44..48)? == b" EMF" {
        return super::util::decodable_image(data.get(4..)?.to_vec()).map(CoverOut::Bytes);
    }
    // Publisher: CF_METAFILEPICT + 8-byte METAFILEPICT + a standard WMF (METAHEADER
    // mtType=1, mtHeaderSize=9 at the start).
    if inner == CF_METAFILEPICT && data.get(12..16)? == [0x01, 0x00, 0x09, 0x00] {
        return super::util::decodable_image(data.get(12..)?.to_vec()).map(CoverOut::Bytes);
    }
    // Otherwise: 3ds Max's custom payload — u32(3), u16(1), u16 W, u16 H, … then
    // top-down 24-bit RGB at offset 98 (exactly `RgbImage`'s layout: no flip/swap).
    let w = le16(data, 6)? as u32;
    let h = le16(data, 8)? as u32;
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
        return None;
    }
    let need = (w as usize).checked_mul(h as usize)?.checked_mul(3)?;
    let px = data.get(98..98usize.checked_add(need)?)?;
    RgbImage::from_raw(w, h, px.to_vec()).map(|img| CoverOut::Image(DynamicImage::ImageRgb8(img)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_ole() {
        assert!(!looks_like_max(b"not an ole file"));
        assert!(extract(b"not an ole file").is_none());
    }

    // A full CFB round-trip is covered by the live regression against the real
    // `Logo3D.max` sample; the property-set/payload math here is unit-tested via
    // that path (a hand-built valid CFB in a unit test would be ~as much code as
    // the reader itself).
}
