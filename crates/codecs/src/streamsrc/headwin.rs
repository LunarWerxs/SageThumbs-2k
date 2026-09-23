//! The last generic rescue for a file too big to buffer: the ordinary decode tiers, run on
//! the file's head, and believed only when the head holds the whole picture.
//!
//! Past the input ceiling every decoder that wants the file in memory used to be skipped, so a
//! 300 MB Rhino model, SketchUp scene, Cinema 4D file, DICOM series or e-book got the stock
//! icon although its picture sits in its first few kilobytes (the big-file gate, 2026-09-23:
//! 137 formats, every surface). The picture is usually there: a baked preview written ahead of
//! the model, a cover ahead of the text, the first frame ahead of the rest.
//!
//! Usually is not always, and a decoder handed a cut-off file can draw the part it got: the top
//! rows of a genuinely huge raster, the first triangles of a mesh. So the head is decoded twice,
//! at 16 and at 32 MiB, and the picture is taken only when both agree to the byte. A picture the
//! first 16 MiB hold whole cannot change when another 16 MiB arrive; one that runs past the cut
//! does, and is refused - the file keeps the stock icon it had rather than a wrong picture.

use super::*;

/// How much of an oversized file's head [`head_window`] reads. The agreement check decodes the
/// first half too, so a picture is accepted only when it lies within the first 16 MiB.
const HEAD_WINDOW_BYTES: usize = 32 << 20;

/// Is this a picture WIC reads by streaming, so that a big file of this kind is most likely a
/// big picture rather than a small one with a long tail? JPEG, PNG, classic TIFF (a camera RAW
/// is not: its preview sits in its head, and WIC would decode the sensor data instead; BigTIFF
/// is, though WIC cannot open it and `tiffscale` reads it), BMP, GIF, JPEG XR, WebP and the
/// ISO-BMFF stills that are not a camera RAW. The WIC rescue answers these before the head
/// window, which would otherwise spend two failed decodes on every big scan.
pub(super) fn wic_reads_the_whole_picture(head: &StreamHead) -> bool {
    let b = head.bytes();
    b.starts_with(&[0xFF, 0xD8, 0xFF])
        || b.starts_with(b"\x89PNG\r\n\x1a\n")
        || plain_tiff(head)
        || b.starts_with(b"BM")
        || b.starts_with(b"GIF8")
        || b.starts_with(b"II\xBC\x01")
        || (b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"))
        || (b.get(4..8) == Some(b"ftyp") && !raw(head))
}

/// A camera RAW (a Canon CR3 is ISO-BMFF too): the buffered path draws its embedded preview,
/// which the head holds, and WIC would decode the sensor data instead.
fn raw(head: &StreamHead) -> bool {
    looks_like_raw_container(head.bytes(), head.extension_is(is_raw_extension))
}

/// A TIFF, classic or BigTIFF, that is not a camera RAW.
pub(super) fn plain_tiff(head: &StreamHead) -> bool {
    let b = head.bytes();
    let tiff = [b"II*\0", b"MM\0*", b"II+\0", b"MM\0+"]
        .iter()
        .any(|m| b.starts_with(*m));
    tiff && !raw(head)
}

/// Decode `bytes` the way the shell's buffered path would, naming the extension so a coder
/// ImageMagick picks by name is reachable.
fn decode_window(bytes: &[u8], target_edge: u32, name: &str) -> Option<image::DynamicImage> {
    decode::decode_preview_capped_for_path(bytes, target_edge, name).ok()
}

/// A CALS raster: its header gives no data length, the Group 4 image simply runs to the end of
/// the file, and ImageMagick's reader spends its whole CPU budget on a window of it (20 s, twice,
/// measured on a 300 MB file). A CALS that big is a big raster, which no window holds, so none
/// is tried.
fn reads_to_the_end(head: &StreamHead) -> bool {
    let b = head.bytes();
    head.extension_is(|e| e == "cal" || e == "cals")
        || [&b"srcdocid:"[..], b"rorient:", b"version: MIL-STD-1840"]
            .iter()
            .any(|m| b.starts_with(m))
}

/// A MATLAB v5 file's header text.
const MAT_V5: &[u8] = b"MATLAB 5.0 MAT-file";

/// Where a MATLAB v5 file's first variable ends, the one ImageMagick draws (its reader walks on
/// into whatever follows, and a long tail costs its whole CPU budget), when that is within the
/// window.
fn mat_picture_end(b: &[u8]) -> Option<u64> {
    let word = |at: usize| -> Option<u32> {
        let four: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        match b.get(126..128)? {
            b"IM" => Some(u32::from_le_bytes(four)),
            b"MI" => Some(u32::from_be_bytes(four)),
            _ => None,
        }
    };
    let (kind, size) = (word(128)?, u64::from(word(132)?));
    // A small data element (type and size packed in one word) holds no picture.
    if kind >> 16 != 0 {
        return None;
    }
    // miMATRIX is padded to eight bytes; a compressed element is exactly its size.
    let size = if kind == 14 {
        size.div_ceil(8) * 8
    } else {
        size
    };
    let end = 136 + size;
    (end <= HEAD_WINDOW_BYTES as u64).then_some(end)
}

/// Do two decodes show the same picture, pixel for pixel?
fn same_picture(a: &image::DynamicImage, b: &image::DynamicImage) -> bool {
    (a.width(), a.height()) == (b.width(), b.height())
        && a.color() == b.color()
        && a.as_bytes() == b.as_bytes()
}

/// The picture the ordinary tiers draw from the first 16 MiB of an oversized stream, when they
/// draw the same one from the first 32 MiB (see the module comment); `None` when either decode
/// fails or the two differ. The agreeing decode is handed back rather than the bytes, so a slow
/// decoder (ImageMagick on a HEIF sequence, ~1 s a time) is not run a third time. Rewinds the
/// stream.
pub(super) unsafe fn head_window(
    stream: &IStream,
    head: &StreamHead,
    target_edge: u32,
    who: &str,
) -> Option<image::DynamicImage> {
    let name = format!("head.{}", head.ext.as_deref().unwrap_or("bin"));
    if reads_to_the_end(head) {
        return None;
    }
    if head.bytes().starts_with(MAT_V5) {
        // The header says where the picture's bytes stop, so one exact decode does; past the
        // window the first variable is itself too big to draw.
        let end = mat_picture_end(head.bytes())?;
        let exact = stream_prefix(stream, head.size, usize::try_from(end).ok()?)?;
        let img = decode_window(&exact, target_edge, &name)?;
        safety::log_debugf!("{who}: picture read from its first {end} bytes");
        return Some(img);
    }
    let window = stream_prefix(stream, head.size, HEAD_WINDOW_BYTES)?;
    let half = window.len() / 2;
    let short = decode_window(&window[..half], target_edge, &name)?;
    let long = decode_window(&window, target_edge, &name)?;
    if !same_picture(&short, &long) {
        safety::log_debugf!("{who}: the picture runs past the head window; not drawn");
        return None;
    }
    safety::log_debugf!(
        "{who}: picture held whole by the first {half} bytes ({}x{})",
        short.width(),
        short.height()
    );
    Some(short)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first variable ends where the tag after the 128-byte header says, in either byte
    /// order; a small element or a variable past the window gives no end.
    #[test]
    fn a_matlab_files_first_variable_ends_where_its_tag_says() {
        let header = |order: &[u8]| {
            let mut h = MAT_V5.to_vec();
            h.resize(126, b' ');
            h.extend(order);
            h
        };
        let mut le = header(b"IM");
        le.extend(15u32.to_le_bytes());
        le.extend(1000u32.to_le_bytes());
        assert_eq!(mat_picture_end(&le), Some(1136));
        let mut be = header(b"MI");
        be.extend(14u32.to_be_bytes());
        be.extend(1001u32.to_be_bytes());
        assert_eq!(
            mat_picture_end(&be),
            Some(136 + 1008),
            "a matrix is padded to 8"
        );
        let mut small = header(b"IM");
        small.extend((4u32 << 16 | 5).to_le_bytes());
        small.extend(0u32.to_le_bytes());
        assert_eq!(mat_picture_end(&small), None);
        let mut huge = header(b"IM");
        huge.extend(15u32.to_le_bytes());
        huge.extend(u32::MAX.to_le_bytes());
        assert_eq!(mat_picture_end(&huge), None);
        match st2k_base::testcorpus::read("real.mat") {
            Some(real) => assert_eq!(
                mat_picture_end(&real),
                Some(real.len() as u64),
                "real.mat is one compressed variable"
            ),
            None => eprintln!("NOT MEASURED: real.mat absent"),
        }
    }
}
