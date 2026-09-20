//! Rotate and flip: the dihedral group, the lossless JPEG path and the pixel path it falls back to.

use super::*;

/// Apply a [`Transform`] and write the result as a NEW file ("<name> (edited)")
/// next to the original — never overwrites the source (a JPEG would re-compress).
/// Keeps the source format. Returns the output path.
pub fn transform_file(path: &str, t: Transform) -> Result<PathBuf> {
    let bytes = read_full_fidelity_capped(path)?;
    let src = Path::new(path);
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();

    // LOSSLESS path for baseline JPEGs: rotate/flip the DCT coefficients directly
    // (no decode-to-pixels, no re-quantize → zero quality loss). Falls through to
    // the lossy re-encode below if the JPEG is outside the supported scope
    // (progressive, non-block-aligned dimensions, a multi-picture index, etc.).
    if matches!(ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif") {
        if let Some(out) = transform_file_lossless_jpeg(src, &bytes, t, &ext)? {
            return Ok(out);
        }
    }

    transform_file_pixels(path, &bytes, t, &ext, src)
}

/// The lossless jpegtran branch of [`transform_file`]: write the coefficient-rotated
/// bytes as a new "(edited)" sibling. `Ok(None)` when the file is outside `jpegtran`'s
/// scope, so the caller takes the pixel path.
pub(super) fn transform_file_lossless_jpeg(
    src: &Path,
    bytes: &[u8],
    t: Transform,
    ext: &str,
) -> Result<Option<PathBuf>> {
    let Some(out_bytes) = lossless_jpeg_transform(bytes, t) else {
        return Ok(None);
    };
    let slot = reserve_unique_suffix(src, "edited", ext);
    write_atomic(slot.path(), |tmp| {
        std::fs::write(tmp, &out_bytes)
            .map_err(|e| Error::new(E_FAIL, format!("write {}: {e}", tmp.display())))
    })?;
    preserve_src_time(src, slot.path());
    Ok(Some(slot.path().to_path_buf()))
}

/// The native writer for an already-truthful output extension (`edit_output_ext`'s
/// result), or `None` when that extension has no native encoder and magick has to
/// write it. A native-but-unknown extension falls back to PNG, the same honest
/// fallback [`edit_output_ext`] itself makes.
pub(super) fn native_writer_for(out_ext: &str) -> Option<ImageFormat> {
    if ext_needs_magick(out_ext) {
        None
    } else {
        Some(native_output_format(out_ext).unwrap_or(ImageFormat::Png))
    }
}

/// What each [`Transform`] means, applied to pixels — the one definition both the
/// pixel fallback below and the tests that predict its output go through.
pub(super) fn apply_transform(img: &DynamicImage, t: Transform) -> DynamicImage {
    match t {
        Transform::Right90 => img.rotate90(),
        Transform::Left90 => img.rotate270(),
        Transform::Rotate180 => img.rotate180(),
        Transform::FlipH => img.fliph(),
        Transform::FlipV => img.flipv(),
    }
}

/// The pixel fallback of [`transform_file`]: decode, apply `t`, encode with the native
/// writer (or magick for exotic targets), carrying the metadata through.
pub(super) fn transform_file_pixels(
    path: &str,
    bytes: &[u8],
    t: Transform,
    ext: &str,
    src: &Path,
) -> Result<PathBuf> {
    // Pixel fallback: keep the extension only when a real writer exists. Exotic
    // writable formats go through Magick; decoder-only/unknown inputs get an
    // honest PNG sibling instead of PNG bytes disguised by the source suffix.
    let img = decode::decode_full_for_path(bytes, path)?;
    let out_img = apply_transform(&img, t);
    let out_ext = edit_output_ext(ext);
    let native_format = native_writer_for(out_ext);
    let slot = reserve_unique_suffix(src, "edited", out_ext);
    // A104: this pixel fallback (progressive JPEG / PNG / TIFF / …) decodes-and-re-encodes,
    // which drops every metadata block on its own — `resize_file` below already carries EXIF/
    // XMP/IPTC through the same shape of pipeline; this branch was the one that didn't.
    let carried = carry::read(bytes, ext);
    write_reencoded(&out_img, out_ext, native_format, carried.as_ref(), &slot)?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// Encode `img` with the native writer for `native_format`, grafting the carried
/// metadata onto the written file, or through magick when there is no native writer.
pub(super) fn write_reencoded(
    img: &DynamicImage,
    out_ext: &str,
    native_format: Option<ImageFormat>,
    carried: Option<&carry::Carried>,
    slot: &OutSlot,
) -> Result<()> {
    write_atomic(slot.path(), |tmp| {
        write_reencoded_to(img, out_ext, native_format, carried, tmp)
    })
}

/// The encoder choice inside [`write_reencoded`]'s staging-file closure.
pub(super) fn write_reencoded_to(
    img: &DynamicImage,
    out_ext: &str,
    native_format: Option<ImageFormat>,
    carried: Option<&carry::Carried>,
    tmp: &Path,
) -> Result<()> {
    if let Some(format) = native_format {
        encode_to(img, format, out_ext, tmp)?;
        if let Some(m) = carried {
            carry::apply(m, tmp, out_ext)?;
        }
        Ok(())
    } else {
        encode_via_magick_carrying(img, carried, tmp, out_ext, None)
    }
}

/// One of the eight symmetries of a rectangle, written as "transpose, then flip
/// horizontally, then flip vertically" (each step optional, always in that order).
///
/// Both an EXIF Orientation and a menu [`Transform`] are members of this group, and
/// the lossless JPEG path needs their COMPOSITION: the stored pixels of an
/// `Orientation=6` phone photo lie on their side and the viewer rotates them, so a
/// "rotate right" request must act on what the viewer shows, not on the stored grid.
/// Composing the two picks the single [`crate::jpegtran::Op`] that turns the stored
/// grid into the requested result, after which the tag is reset to 1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Dihedral {
    pub(super) transpose: bool,
    pub(super) flip_h: bool,
    pub(super) flip_v: bool,
}

impl Dihedral {
    /// The operation a viewer applies to the stored pixels for EXIF Orientation `o`
    /// (1..=8 per the EXIF spec; anything else is treated as 1, "normal").
    pub(super) fn from_exif_orientation(o: u32) -> Self {
        let (transpose, flip_h, flip_v) = match o {
            2 => (false, true, false),
            3 => (false, true, true),
            4 => (false, false, true),
            5 => (true, false, false),
            6 => (true, true, false), // rotate 90° CW = transpose then flip-H
            7 => (true, true, true),
            8 => (true, false, true), // rotate 270° CW = transpose then flip-V
            _ => (false, false, false),
        };
        Self {
            transpose,
            flip_h,
            flip_v,
        }
    }

    pub(super) fn from_transform(t: Transform) -> Self {
        let (transpose, flip_h, flip_v) = match t {
            Transform::Right90 => (true, true, false),
            Transform::Left90 => (true, false, true),
            Transform::Rotate180 => (false, true, true),
            Transform::FlipH => (false, true, false),
            Transform::FlipV => (false, false, true),
        };
        Self {
            transpose,
            flip_h,
            flip_v,
        }
    }

    /// `self` first, then `next`.
    ///
    /// Moving `next`'s transpose in front of `self`'s flips swaps their axes, because
    /// `transpose(flip_h(x)) == flip_v(transpose(x))`; flips then combine by parity.
    pub(super) fn then(self, next: Self) -> Self {
        let (flip_h, flip_v) = if next.transpose {
            (self.flip_v, self.flip_h)
        } else {
            (self.flip_h, self.flip_v)
        };
        Self {
            transpose: self.transpose ^ next.transpose,
            flip_h: flip_h ^ next.flip_h,
            flip_v: flip_v ^ next.flip_v,
        }
    }

    /// The jpegtran operation with this effect; `None` for the identity.
    pub(super) fn to_op(self) -> Option<crate::jpegtran::Op> {
        use crate::jpegtran::Op;
        Some(match (self.transpose, self.flip_h, self.flip_v) {
            (false, false, false) => return None,
            (false, true, false) => Op::FlipH,
            (false, false, true) => Op::FlipV,
            (false, true, true) => Op::Rot180,
            (true, true, false) => Op::Rot90,
            (true, false, true) => Op::Rot270,
            (true, false, false) => Op::Transpose,
            (true, true, true) => Op::Transverse,
        })
    }
}

/// The source JPEG's EXIF Orientation tag, if it has one.
pub(super) fn exif_orientation(bytes: &[u8]) -> Option<u32> {
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
        .value
        .get_uint(0)
}

/// The lossless JPEG branch of [`transform_file`]: compose the source Orientation with
/// the request, transform the DCT grid with the resulting operation, and reset the
/// tag. `None` when the file is outside `jpegtran`'s scope, so the caller takes the
/// pixel path (which decodes with the orientation applied and so is correct by
/// construction).
pub(super) fn lossless_jpeg_transform(bytes: &[u8], t: Transform) -> Option<Vec<u8>> {
    let stored = Dihedral::from_exif_orientation(exif_orientation(bytes).unwrap_or(1));
    let out = match stored.then(Dihedral::from_transform(t)).to_op() {
        Some(op) => crate::jpegtran::transform(bytes, op)?,
        // The request exactly undoes the stored orientation: the stored grid already IS
        // the result, so the bytes are kept as they are and only the tag changes. A
        // multi-picture index is declined for the same reason `transform` declines it:
        // the EXIF rewrite below can change the segment's length.
        None => {
            if crate::jpegtran::has_multi_picture_index(bytes) {
                return None;
            }
            bytes.to_vec()
        }
    };
    Some(neutralize_lossless_jpeg_orientation(out))
}

/// A273: after a lossless rotate/flip, reset the EXIF Orientation tag to 1 and drop the
/// IFD1 thumbnail.
///
/// `crate::jpegtran::transform` keeps APPn/EXIF segments byte-for-byte verbatim while
/// physically transforming the DCT grid (that's the whole point — zero requantize loss), so
/// the tag still describes the SOURCE grid and the embedded thumbnail still shows the source
/// framing. This branch returns straight to the caller before `carry` is ever consulted
/// (there is no fresh re-encode here for `carry::apply` to graft onto), so the same two
/// rewrites `carry` applies to a lifted block are applied here to the output file's own
/// segment.
///
/// Best-effort: any parse surprise returns `bytes` unchanged rather than risk corrupting a
/// file whose pixel transform already succeeded. A file with no EXIF, or none of the shapes
/// this recognizes, is untouched — exactly today's behavior for those cases.
pub(super) fn neutralize_lossless_jpeg_orientation(bytes: Vec<u8>) -> Vec<u8> {
    use img_parts::jpeg::{markers, Jpeg, JpegSegment};
    use img_parts::Bytes;

    const EXIF_PREFIX: &[u8] = b"Exif\0\0";

    let Ok(mut jpeg) = Jpeg::from_bytes(Bytes::from(bytes.clone())) else {
        return bytes;
    };
    let segs = jpeg.segments_mut();
    let Some(idx) = segs
        .iter()
        .position(|s| s.marker() == markers::APP1 && s.contents().starts_with(EXIF_PREFIX))
    else {
        return bytes; // no EXIF segment — nothing to neutralize
    };
    let mut tiff = segs[idx].contents()[EXIF_PREFIX.len()..].to_vec();
    carry::reset_orientation_to_1(&mut tiff);
    carry::drop_ifd1_thumbnail(&mut tiff);
    let mut new_contents = EXIF_PREFIX.to_vec();
    new_contents.extend_from_slice(&tiff);
    segs[idx] = JpegSegment::new_with_contents(markers::APP1, Bytes::from(new_contents));

    let out = jpeg.encoder().bytes();
    // Sanity re-parse, mirroring carry::apply_jpeg — never hand back something we cannot
    // read again.
    if Jpeg::from_bytes(out.clone()).is_err() {
        return bytes;
    }
    out.to_vec()
}
