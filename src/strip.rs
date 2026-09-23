//! Lossless metadata strip for JPEG, PNG and WebP — a segment/chunk rewrite, NO
//! pixel re-encode (so a photo never loses quality). Removes EXIF / IPTC / XMP /
//! comments, and **C2PA "Content Credentials"** (see [`jumbf`]), which is neither
//! of those and therefore survives every EXIF-only scrubber.
//! Plus `read_info`, an EXIF reader for the "Image info" verb (reuses the
//! already-present `kamadak-exif` + `image` — no new deps for that part).
//!
//! The ICC color profile (JPEG APP2 / PNG iCCP) is deliberately KEPT — stripping
//! it shifts colors on wide-gamut displays.

use core::ffi::c_void;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use img_parts::jpeg::{markers, Jpeg};
use img_parts::png::Png;
use img_parts::Bytes;
use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Storage::FileSystem::{
    ReplaceFileW, REPLACEFILE_IGNORE_ACL_ERRORS, REPLACEFILE_IGNORE_MERGE_ERRORS,
    REPLACE_FILE_FLAGS,
};
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_UPDATEITEM, SHCNF_PATHW};

use crate::decode::read_full_fidelity_capped;

mod ddsinfo;
// `pub(crate)`: the decode tier reuses this hardened item parser to locate the primary
// AV1 payload of a BT.601 AVIF (decode/avifmf.rs) — same bounds discipline, one parser.
mod info;
pub(crate) mod isobmff;
mod jumbf;
mod svgmeta;
mod webpmeta;
mod xmpinfo;
#[cfg(test)]
use info::format_exif_datetime;
pub use info::{
    read_audio_tags, read_capture, read_info, read_info_bounded, read_info_verbose,
    split_exif_datetime, AudioTags, ImageInfo,
};

// Direct fuzz entry points into the (private) parsers above — see its own doc comment for why
// it lives here rather than in `crate::fuzz`.
#[cfg(test)]
pub(crate) mod fuzzseed;

use crate::isobmff::has_gain_map;
pub use jumbf::has_content_credentials;

/// JPEG markers we drop: Exif + XMP (both APP1), Photoshop/IPTC (APP13), and the
/// free-text comment (COM). APP2 (ICC) is intentionally omitted.
///
/// APP11 is NOT in this list because it is marker-ambiguous: JPEG XT uses it for
/// HDR extension layers. It is filtered per-segment instead, in [`jumbf`].
const STRIP_APP_MARKERS: &[u8] = &[markers::APP1, markers::APP13, markers::COM];

/// APP11 packet identity: `(box instance, packet sequence)`, per the JUMBF/CIPA layout
/// `JP`(2) + box instance(2, BE `u16`) + packet sequence(4, BE `u32`). The FIRST packet
/// of a box (sequence 1) carries the `LBox`/`TBox` header [`jumbf::is_jumbf_app11`]
/// matches on; a LATER packet in the same box instance (sequence > 1) carries none - raw
/// continuation payload only. Two independent boxes (a JUMBF manifest and, say, an
/// unrelated JPEG XT HDR layer) can legally reuse the same instance number since they
/// are never interleaved, and both are then "first of their own box" (sequence 1) - so
/// grouping keys on `sequence > 1` too, not the instance number alone, or an unrelated
/// same-instance first packet would be mistaken for this box's continuation.
fn app11_identity(contents: &[u8]) -> Option<(u16, u32)> {
    if contents.len() < 8 || !contents.starts_with(b"JP") {
        return None;
    }
    let instance = u16::from_be_bytes([contents[2], contents[3]]);
    let sequence = u32::from_be_bytes([contents[4], contents[5], contents[6], contents[7]]);
    Some((instance, sequence))
}

/// Inflate a `.svgz` gzip stream with a hard output cap (decompression-bomb guard) — a
/// thin wrapper over the shared [`decode::svg::gunzip_bounded`](crate::decode::svg)
/// (C5), passing this module's own, larger ceiling rather than `decode::svg`'s (that
/// one is sized for a thumbnail-sized SVG/EMF; this one is sized for the same
/// full-fidelity input every other in-place rewrite in this file accepts). `None` on any
/// inflate error or empty output.
fn gunzip_bounded(bytes: &[u8]) -> Option<Vec<u8>> {
    crate::decode::svg::gunzip_bounded(bytes, crate::decode::limits::MAX_INPUT_BYTES)
}

/// Re-gzip stripped SVG source for the `.svgz` output path, so the file's own
/// extension stays truthful (a plain-XML rewrite of a `.svgz` would silently become an
/// uncompressed file wearing a compressed-format extension).
fn regzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(bytes)
        .map_err(|e| Error::new(E_FAIL, format!("gzip: {e}")))?;
    gz.finish()
        .map_err(|e| Error::new(E_FAIL, format!("gzip finish: {e}")))
}

/// Strip metadata from `path` in place (JPEG / PNG / WebP). Re-parses the rewritten
/// bytes before swapping, so a malformed rewrite can never clobber the original.
pub fn strip_metadata(path: &str) -> Result<()> {
    let input = Bytes::from(read_full_fidelity_capped(path)?);
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let out_bytes: Vec<u8> = strip_by_extension(&ext, input)?;

    atomic_overwrite(Path::new(path), &out_bytes)
}

/// Rewrite `input` (JPEG / PNG / WebP / SVG / `.svgz` / HEIC), refusing a format this
/// cannot losslessly strip instead of lossy-converting it.
fn strip_by_extension(ext: &str, input: Bytes) -> Result<Vec<u8>> {
    match ext {
        "jpg" | "jpeg" | "jpe" | "jfif" => strip_jpeg(input),
        "png" => strip_png(input),
        "webp" => webpmeta::strip(input),
        "svg" => svgmeta::strip(&input),
        // .svgz is gzip-compressed SVG (Illustrator/Inkscape's "compressed" save option). The
        // old match arm here (`"svg" | "svgz" if ext == "svg"`) guarded the WHOLE or-pattern on
        // `ext == "svg"`, so it could only ever fire for "svg" and every real .svgz file fell
        // through to the unsupported case below. Inflate bounded by the same input ceiling as
        // every other decode path (a compression bomb here would otherwise expand a KB-sized
        // file-controlled payload without limit), strip the decompressed XML, then re-gzip so
        // the file's own ".svgz" extension stays truthful.
        "svgz" => {
            let inflated = gunzip_bounded(&input)
                .ok_or_else(|| Error::new(E_FAIL, "svgz: not a gzip stream, or empty"))?;
            let stripped = svgmeta::strip(&inflated)?;
            regzip(&stripped)
        }
        // HEIC/AVIF items are rewritten in place (see `isobmff`); `None` means the
        // layout was not one we can touch without risking the picture.
        "heic" | "heif" | "hif" | "avif" => isobmff::strip(&input).ok_or_else(|| {
            Error::new(
                E_FAIL,
                "heif: no strippable item, or a layout not rewritten in place",
            )
        }),
        // Unsupported: refuse, never lossy-convert.
        _ => {
            let why = format!("strip: .{ext} is not a format this can rewrite");
            Err(Error::new(E_FAIL, why))
        }
    }
}

/// The APP2 payload prefix of a Multi-Picture Format index (CIPA DC-007).
const MPF_PREFIX: &[u8] = b"MPF\0";

/// The smallest EXIF that carries Orientation and nothing else: a little-endian TIFF header
/// and one IFD0 entry (tag 0x0112, SHORT, count 1, value left-justified), 26 bytes.
fn tiff_orientation_only(orientation: u32) -> Vec<u8> {
    let mut t = vec![b'I', b'I', 0x2A, 0x00, 8, 0, 0, 0, 1, 0];
    t.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 1, 0, 0, 0]);
    t.extend_from_slice(&(orientation as u16).to_le_bytes());
    t.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    t
}

/// The Orientation this file displays with, when it is anything but the identity. Orientation
/// is rendering-critical, not metadata ABOUT the picture: a photo tagged 6 displays portrait,
/// and dropping the tag turned it landscape in an operation sold as lossless metadata removal
/// (2026-09-19 audit F04). Every stripper puts this one tag back, in a fresh EXIF that holds
/// nothing else - no make, no date, no GPS, no thumbnail.
fn kept_orientation(bytes: &[u8]) -> Option<u32> {
    crate::decode::exif_orientation(bytes).filter(|o| (2..=8).contains(o))
}

/// JPEG arm of [`strip_metadata`]: drop EXIF/IPTC/XMP/COM (APP1/APP13/COM), plus any C2PA
/// "Content Credentials" JUMBF box (APP11), see [`jumbf`]. ICC (APP2) is deliberately kept.
///
/// A Multi-Picture Format file (an APP2 `MPF\0` index: iPhone HDR/Portrait, Pixel and
/// Samsung Ultra HDR) is refused whole. The index records this image's byte length and
/// the offsets of the pictures stored after its EOI; removing segments ahead of the scan
/// moves every byte it points at while the index itself would be kept verbatim, and the
/// result is written over the original. Same all-or-nothing rule as [`isobmff::strip`].
fn strip_jpeg(input: Bytes) -> Result<Vec<u8>> {
    let orientation = kept_orientation(&input);
    let mut jpeg =
        Jpeg::from_bytes(input).map_err(|e| Error::new(E_FAIL, format!("jpeg parse: {e}")))?;
    if jpeg
        .segments()
        .iter()
        .any(|s| s.marker() == markers::APP2 && s.contents().starts_with(MPF_PREFIX))
    {
        let why = "multi-picture (MPF) JPEG: its index would no longer match the file";
        st2k_base::safety::log(&format!("strip refused: {why}"));
        return Err(Error::new(E_FAIL, why));
    }
    // C2PA / Content Credentials: a JUMBF box spread over APP11 segments. Only the
    // FIRST packet of a box carries the LBox/TBox header `is_jumbf_app11` looks for;
    // once a manifest exceeds ~64KB it continues in more APP11 segments that share the
    // same box-instance number but have no TBox of their own to match on. Find every
    // C2PA box instance from whichever segment announces it, then drop every APP11
    // segment in that instance - not just the one that matched - so a multi-segment
    // manifest doesn't leave its continuation packets behind (which would otherwise let
    // `has_content_credentials` report `false` while manifest fragments still survive).
    let c2pa_instances: std::collections::HashSet<u16> = jpeg
        .segments()
        .iter()
        .filter(|s| s.marker() == markers::APP11 && jumbf::is_jumbf_app11(s.contents()))
        .filter_map(|s| app11_identity(s.contents()).map(|(inst, _)| inst))
        .collect();
    jpeg.segments_mut()
        .retain(|s| !is_stripped_segment(s, &c2pa_instances));
    if let Some(o) = orientation {
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff_orientation_only(o));
        jpeg.segments_mut().insert(
            0,
            img_parts::jpeg::JpegSegment::new_with_contents(markers::APP1, Bytes::from(app1)),
        );
    }
    let bytes = jpeg.encoder().bytes();
    // Sanity re-parse.
    Jpeg::from_bytes(bytes.clone())
        .map_err(|e| Error::new(E_FAIL, format!("jpeg re-parse: {e}")))?;
    Ok(bytes.to_vec())
}

/// Whether a JPEG segment is strip-worthy metadata: an APP1/APP13/COM marker, a JUMBF
/// box-defining APP11 packet, or a continuation packet of a flagged C2PA box instance.
fn is_stripped_segment(
    s: &img_parts::jpeg::JpegSegment,
    c2pa_instances: &std::collections::HashSet<u16>,
) -> bool {
    if STRIP_APP_MARKERS.contains(&s.marker()) {
        return true;
    }
    if s.marker() == markers::APP11 {
        if jumbf::is_jumbf_app11(s.contents()) {
            return true; // the box-defining packet itself
        }
        // A JPEG XT HDR layer wears the same marker and must survive - only a
        // genuine CONTINUATION packet (sequence > 1) of a flagged box instance is
        // dropped, never an unrelated first-of-its-own-box packet that happens to
        // reuse the same instance number.
        if let Some((inst, seq)) = app11_identity(s.contents()) {
            if seq > 1 && c2pa_instances.contains(&inst) {
                return true;
            }
        }
    }
    false
}

/// PNG arm of [`strip_metadata`]: drop EXIF/text/time chunks plus any C2PA chunk. iCCP (color
/// profile) is intentionally NOT removed, stripping it shifts colors on wide-gamut displays.
fn strip_png(input: Bytes) -> Result<Vec<u8>> {
    let orientation = kept_orientation(&input);
    let mut png =
        Png::from_bytes(input).map_err(|e| Error::new(E_FAIL, format!("png parse: {e}")))?;
    for k in [b"eXIf", b"tEXt", b"iTXt", b"zTXt", b"tIME"] {
        png.remove_chunks_by_type(*k);
    }
    png.remove_chunks_by_type(jumbf::PNG_C2PA_CHUNK);
    if let Some(o) = orientation {
        // eXIf sits after IHDR (chunk 0) and before the image data, per the PNG spec.
        let at = png.chunks().len().min(1);
        png.chunks_mut().insert(
            at,
            img_parts::png::PngChunk::new(*b"eXIf", Bytes::from(tiff_orientation_only(o))),
        );
    }
    let bytes = png.encoder().bytes();
    Png::from_bytes(bytes.clone()).map_err(|e| Error::new(E_FAIL, format!("png re-parse: {e}")))?;
    Ok(bytes.to_vec())
}

/// In-place overwrite via a same-volume temp + swap, with a short retry so a
/// transient Explorer/thumbnail-cache lock (os error 5/32) doesn't fail it.
fn atomic_overwrite(dst: &Path, data: &[u8]) -> Result<()> {
    atomic_overwrite_with(dst, data, notify_item_updated)
}

/// Atomically replace `dst`, then report the changed item to Explorer.
///
/// The swap goes through [`replace_retrying`], so the rewritten file keeps the
/// original's attributes, ACL, creation time and alternate data streams. Its
/// last-write time is put back too when the user keeps original file dates
/// (`settings::preserve_file_date`, the same switch every new-file verb honours).
///
/// Keeping the notification callback explicit lets the rewrite path be tested
/// without depending on a running Explorer shell.
fn atomic_overwrite_with(dst: &Path, data: &[u8], notify: impl FnOnce(&Path)) -> Result<()> {
    // A reserved, unique staging entry (`create_new`), never the bare `<dst>.st2ktmp` this used
    // to write into: a pre-existing entry at a predictable name - a hard link to the file
    // itself, say - was truncated by the write (2026-09-19 audit F03).
    let tmp: PathBuf = st2k_base::fsutil::create_staging(dst)
        .map_err(|e| Error::new(E_FAIL, format!("stage {}: {e}", dst.display())))?;
    let mtime = std::fs::metadata(dst).and_then(|m| m.modified()).ok();
    std::fs::write(&tmp, data).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("write {}: {e}", tmp.display()))
    })?;
    replace_retrying(&tmp, dst).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("replace {}: {e}", dst.display()))
    })?;
    if st2k_base::settings::preserve_file_date() {
        if let Some(m) = mtime {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(dst) {
                let _ = f.set_modified(m);
            }
        }
    }
    notify(dst);
    Ok(())
}

/// Retry count for [`replace_retrying`]; mirrors `fsutil::rename_retrying`'s.
const REPLACE_RETRIES: u32 = 5;

/// Swap `tmp` into `dst`'s place with `ReplaceFileW`. Unlike a rename, which gives
/// `dst`'s name to a brand-new file, `ReplaceFileW` keeps the replaced file's
/// attributes (hidden/system/read-only), DACL, creation time and alternate data
/// streams (the Zone.Identifier mark, for one). Retried past a transient lock with
/// the same backoff `fsutil::rename_retrying` uses. A `dst` that does not exist has
/// nothing to preserve and takes the plain rename.
fn replace_retrying(tmp: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        return st2k_base::fsutil::rename_retrying(tmp, dst);
    }
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(once(0)).collect() };
    let (replaced, replacement) = (wide(dst), wide(tmp));
    let flags =
        REPLACE_FILE_FLAGS(REPLACEFILE_IGNORE_MERGE_ERRORS.0 | REPLACEFILE_IGNORE_ACL_ERRORS.0);
    let mut last: std::io::Result<()> = Ok(());
    for _ in 0..REPLACE_RETRIES {
        let swapped = unsafe {
            ReplaceFileW(
                PCWSTR(replaced.as_ptr()),
                PCWSTR(replacement.as_ptr()),
                PCWSTR::null(),
                flags,
                None,
                None,
            )
        };
        match swapped {
            Ok(()) => return Ok(()),
            Err(e) => last = Err(std::io::Error::other(e)),
        }
        std::thread::sleep(st2k_base::fsutil::RENAME_BACKOFF);
    }
    last
}

/// Tell Explorer that one existing file was rewritten in place.
fn notify_item_updated(path: &Path) {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEITEM,
            SHCNF_PATHW,
            Some(wide.as_ptr() as *const c_void),
            None,
        );
    }
}

#[cfg(test)]
mod tests;
