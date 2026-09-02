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

use crate::verbs::read_full_fidelity_capped;

mod ddsinfo;
// `pub(crate)`: the decode tier reuses this hardened item parser to locate the primary
// AV1 payload of a BT.601 AVIF (decode/avifmf.rs) — same bounds discipline, one parser.
pub(crate) mod isobmff;
mod jumbf;
mod svgmeta;
mod webpmeta;
mod xmpinfo;

// Direct fuzz entry points into the (private) parsers above — see its own doc comment for why
// it lives here rather than in `crate::fuzz`.
#[cfg(test)]
pub(crate) mod fuzzseed;

pub use isobmff::has_gain_map;
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

    let out_bytes: Vec<u8> = match ext.as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => strip_jpeg(input)?,
        "png" => strip_png(input)?,
        "webp" => webpmeta::strip(input)?,
        "svg" => svgmeta::strip(&input)?,
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
            regzip(&stripped)?
        }
        // HEIC/AVIF items are rewritten in place (see `isobmff`); `None` means the
        // layout was not one we can touch without risking the picture.
        "heic" | "heif" | "hif" | "avif" => isobmff::strip(&input).ok_or_else(|| {
            Error::new(
                E_FAIL,
                "heif: no strippable item, or a layout not rewritten in place",
            )
        })?,
        // Unsupported: refuse, never lossy-convert.
        _ => {
            let why = format!("strip: .{ext} is not a format this can rewrite");
            return Err(Error::new(E_FAIL, why));
        }
    };

    atomic_overwrite(Path::new(path), &out_bytes)
}

/// The APP2 payload prefix of a Multi-Picture Format index (CIPA DC-007).
const MPF_PREFIX: &[u8] = b"MPF\0";

/// JPEG arm of [`strip_metadata`]: drop EXIF/IPTC/XMP/COM (APP1/APP13/COM), plus any C2PA
/// "Content Credentials" JUMBF box (APP11), see [`jumbf`]. ICC (APP2) is deliberately kept.
///
/// A Multi-Picture Format file (an APP2 `MPF\0` index: iPhone HDR/Portrait, Pixel and
/// Samsung Ultra HDR) is refused whole. The index records this image's byte length and
/// the offsets of the pictures stored after its EOI; removing segments ahead of the scan
/// moves every byte it points at while the index itself would be kept verbatim, and the
/// result is written over the original. Same all-or-nothing rule as [`isobmff::strip`].
fn strip_jpeg(input: Bytes) -> Result<Vec<u8>> {
    let mut jpeg =
        Jpeg::from_bytes(input).map_err(|e| Error::new(E_FAIL, format!("jpeg parse: {e}")))?;
    if jpeg
        .segments()
        .iter()
        .any(|s| s.marker() == markers::APP2 && s.contents().starts_with(MPF_PREFIX))
    {
        let why = "multi-picture (MPF) JPEG: its index would no longer match the file";
        crate::safety::log(&format!("strip refused: {why}"));
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
    jpeg.segments_mut().retain(|s| {
        if STRIP_APP_MARKERS.contains(&s.marker()) {
            return false;
        }
        if s.marker() == markers::APP11 {
            if jumbf::is_jumbf_app11(s.contents()) {
                return false; // the box-defining packet itself
            }
            // A JPEG XT HDR layer wears the same marker and must survive - only a
            // genuine CONTINUATION packet (sequence > 1) of a flagged box instance is
            // dropped, never an unrelated first-of-its-own-box packet that happens to
            // reuse the same instance number.
            if let Some((inst, seq)) = app11_identity(s.contents()) {
                if seq > 1 && c2pa_instances.contains(&inst) {
                    return false;
                }
            }
        }
        true
    });
    let bytes = jpeg.encoder().bytes();
    // Sanity re-parse.
    Jpeg::from_bytes(bytes.clone())
        .map_err(|e| Error::new(E_FAIL, format!("jpeg re-parse: {e}")))?;
    Ok(bytes.to_vec())
}

/// PNG arm of [`strip_metadata`]: drop EXIF/text/time chunks plus any C2PA chunk. iCCP (color
/// profile) is intentionally NOT removed, stripping it shifts colors on wide-gamut displays.
fn strip_png(input: Bytes) -> Result<Vec<u8>> {
    let mut png =
        Png::from_bytes(input).map_err(|e| Error::new(E_FAIL, format!("png parse: {e}")))?;
    for k in [b"eXIf", b"tEXt", b"iTXt", b"zTXt", b"tIME"] {
        png.remove_chunks_by_type(*k);
    }
    png.remove_chunks_by_type(jumbf::PNG_C2PA_CHUNK);
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
    let tmp: PathBuf = {
        let mut s = dst.to_path_buf().into_os_string();
        s.push(".st2ktmp");
        PathBuf::from(s)
    };
    let mtime = std::fs::metadata(dst).and_then(|m| m.modified()).ok();
    std::fs::write(&tmp, data).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("write {}: {e}", tmp.display()))
    })?;
    replace_retrying(&tmp, dst).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(E_FAIL, format!("replace {}: {e}", dst.display()))
    })?;
    if crate::settings::preserve_file_date() {
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
        return crate::fsutil::rename_retrying(tmp, dst);
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
        std::thread::sleep(crate::fsutil::RENAME_BACKOFF);
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

/// What "Image info" shows. Uses the existing `image` + `kamadak-exif` deps.
#[derive(Default)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub make: Option<String>,
    pub model: Option<String>,
    pub datetime: Option<String>,
    pub gps: Option<(f64, f64)>,
    /// Bits per PIXEL (e.g. 24 for RGB8, 32 for RGBA8); 0 = unknown. Surfaced as
    /// `System.Image.BitDepth` by the property handler.
    pub bit_depth: u32,
    /// Print resolution in pixels-per-inch from EXIF X/YResolution (cm values
    /// normalized to inches); 0.0 = absent. Surfaced as
    /// `System.Image.Horizontal/VerticalResolution`.
    pub dpi_x: f64,
    pub dpi_y: f64,
}

/// Read dimensions + camera/date/GPS EXIF (best-effort; missing fields stay None).
///
/// The UNBOUNDED flavour, for explicit user-initiated callers running in their OWN process —
/// the CLI `st2k info` and the right-click "Image info" dialog. When the cheap header probes
/// miss (PSD/EPS/HEIC/RAW/containers), it reads the whole file and runs the full
/// magick-capable decode to report the TRUE document size. For the in-process
/// [`IPropertyStore`](crate::propstore) handler — which the shell loads into Explorer,
/// SearchIndexer, AND a host app's file-open dialog — use [`read_info_bounded`] instead: an
/// unbounded whole-file read + up-to-20 s decode on that hot path froze the caller (selecting
/// a multi-GB upload in Chrome's file picker locked the whole browser — the 0.6.1
/// property-handler hang).
pub fn read_info(path: &str) -> ImageInfo {
    read_info_impl(path, false)
}

/// [`read_info`] for the in-process property handler. This is deliberately a metadata-only
/// probe: image-crate/container headers and EXIF are useful in the Details pane, but a fallback
/// whole-file read, WIC/ImageMagick decode, or embedded-preview extraction is not acceptable in
/// Explorer/SearchIndexer. Unsupported formats may therefore have no dimensions here; explicit
/// user actions use [`read_info`] and retain the full-fidelity fallback. `propstore` additionally
/// runs this cheap probe under a short wall-clock budget off the host thread.
pub fn read_info_bounded(path: &str) -> ImageInfo {
    read_info_impl(path, true)
}

/// First bytes of `path` (64 — ample for every `container::real_dims` header,
/// PSD needs 22), for the header-only dimension probe. None on I/O error or a
/// file too short to hold any such header.
fn head_prefix(path: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 64];
    let mut filled = 0usize;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    buf.truncate(filled);
    (buf.len() >= 26).then_some(buf)
}

/// How much of a file the in-process (`bounded`) EXIF probe may read. `exif::Reader`
/// reads a TIFF-magic file whole, and every camera RAW the property handler is hooked
/// for is a TIFF container, so the property handler's probe stops here rather than copy a
/// multi-GB file into Explorer or the indexer.
const EXIF_SCAN_CAP: u64 = 32 * 1024 * 1024;

/// A `Read + Seek` view of the first `cap` bytes of `inner`: reads at or past the cap
/// return end-of-file, seeks are passed through. `exif::Reader::read_from_container`
/// needs both traits, which a plain `Take` does not provide.
struct CappedReader<R> {
    inner: R,
    pos: u64,
    cap: u64,
}

impl<R: std::io::Read> std::io::Read for CappedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let room = self.cap.saturating_sub(self.pos);
        let want = (buf.len() as u64).min(room) as usize;
        let Some(window) = buf.get_mut(..want) else {
            return Ok(0);
        };
        if window.is_empty() {
            return Ok(0);
        }
        let n = self.inner.read(window)?;
        self.pos = self.pos.saturating_add(n as u64);
        Ok(n)
    }
}

impl<R: std::io::Seek> std::io::Seek for CappedReader<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

fn read_info_impl(path: &str, bounded: bool) -> ImageInfo {
    use exif::Reader;
    let mut info = resolve_image_dimensions(path, bounded);

    let Ok(file) = std::fs::File::open(path) else {
        return info;
    };
    let exif = if bounded {
        let mut buf = std::io::BufReader::new(CappedReader {
            inner: file,
            pos: 0,
            cap: EXIF_SCAN_CAP,
        });
        Reader::new().read_from_container(&mut buf)
    } else {
        let mut buf = std::io::BufReader::new(file);
        Reader::new().read_from_container(&mut buf)
    };
    let Ok(exif) = exif else {
        return info;
    };
    apply_exif_metadata(&mut info, &exif);
    info
}

/// Width/height/bit-depth for the "Image info" dialog, tried cheapest-first: the `image` crate's
/// own header decode (which also gives bits-per-pixel for free), then a small container-header
/// probe for formats it can't read (PSD, EPS, HEIC/RAW), then — for explicit (non-bounded)
/// callers only — a full-file decode and finally a video-frame fallback. Property-handler
/// callers pass `bounded: true` and intentionally stop after headers, so a Details-pane request
/// can never materialize the entire file or start an ImageMagick/WIC decode.
fn resolve_image_dimensions(path: &str, bounded: bool) -> ImageInfo {
    use image::ImageDecoder;
    let mut info = ImageInfo::default();

    if let Ok(rdr) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
        // `into_decoder` (vs the old `into_dimensions`) also exposes the color type,
        // so we capture bits-per-pixel in the same cheap header read — no extra I/O.
        if let Ok(dec) = rdr.into_decoder() {
            let (w, h) = dec.dimensions();
            info.width = w;
            info.height = h;
            info.bit_depth = dec.color_type().bits_per_pixel() as u32;
        }
    }
    if info.width == 0 && info.height == 0 {
        // Header-only dims first: `real_dims` needs the PSD's fixed 26-byte header,
        // so probing a small head prefix answers a folder-of-big-PSDs Details pane
        // without the whole-file read below (Explorer runs this per file, serially,
        // right alongside the thumbnail extraction).
        if let Some((w, h)) = head_prefix(path).and_then(|head| crate::container::real_dims(&head))
        {
            info.width = w;
            info.height = h;
        }
    }
    if info.width == 0 && info.height == 0 && !bounded {
        resolve_dims_via_full_decode(path, &mut info);
    }
    info
}

/// The explicit-caller-only fallback tier: decode the whole file, then (for video) grab a
/// frame. `frame_from_path` can spawn a long-lived Media Foundation worker, so it is never part
/// of the in-shell property path — only reached here, past the `!bounded` gate.
fn resolve_dims_via_full_decode(path: &str, info: &mut ImageInfo) {
    if let Ok(bytes) = std::fs::read(path) {
        if let Some((w, h)) = crate::container::real_or_decoded_dims(&bytes) {
            info.width = w;
            info.height = h;
        }
    }
    if info.width == 0 && info.height == 0 {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(
            crate::formats::category(&ext),
            crate::formats::Category::Video
        ) {
            if let Some(img) = crate::video::frame_from_path(path) {
                info.width = img.width();
                info.height = img.height();
            }
        }
    }
    if info.width == 0 && info.height == 0 {
        // All probes (image-crate header, container canvas, full decode, video frame)
        // failed — leave a breadcrumb so a "shows no dimensions" report is diagnosable
        // instead of silently surfacing the 0×0 sentinel.
        crate::safety::log_debug(&format!(
            "read_info: could not determine dimensions for {path}"
        ));
    }
}

/// Fill in make/model/capture-time/DPI/GPS from a decoded EXIF container.
fn apply_exif_metadata(info: &mut ImageInfo, exif: &exif::Exif) {
    use exif::{In, Tag, Value};
    let txt = |t: Tag| {
        exif.get_field(t, In::PRIMARY)
            .map(|f| f.display_value().with_unit(exif).to_string())
    };
    // Make/Model must NOT go through `display_value`: it renders an ASCII field
    // wrapped in literal double quotes, so Explorer's "Camera maker" column showed
    // `"Canon"` rather than `Canon`. Read the raw ASCII the way `read_capture`
    // already does, trimmed of the trailing NUL padding cameras write.
    let ascii = |t: Tag| -> Option<String> {
        match &exif.get_field(t, In::PRIMARY)?.value {
            Value::Ascii(v) => {
                let s = String::from_utf8_lossy(v.first()?);
                let s = s.trim().trim_end_matches('\0').trim();
                (!s.is_empty()).then(|| s.to_string())
            }
            _ => None,
        }
    };
    info.make = ascii(Tag::Make);
    info.model = ascii(Tag::Model);
    // CAPTURE time only — NOT a fallback to Tag::DateTime (the file-modified stamp editors
    // write), because this feeds System.Photo.DateTaken. Showing an edit timestamp as "Date
    // taken" is wrong and inconsistent with Windows' own photo handler (which never falls back).
    info.datetime = txt(Tag::DateTimeOriginal);

    // Print resolution (DPI). ResolutionUnit: 2 = inches (the usual), 3 = cm — cm
    // values are normalized to inches so the property is always pixels-per-inch.
    let unit = exif
        .get_field(Tag::ResolutionUnit, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(2);
    let res = |t: Tag| -> Option<f64> {
        match &exif.get_field(t, In::PRIMARY)?.value {
            Value::Rational(r) => r.first().map(|x| x.to_f64()),
            _ => None,
        }
    };
    let to_dpi = |v: f64| if unit == 3 { v * 2.54 } else { v };
    if let Some(x) = res(Tag::XResolution) {
        info.dpi_x = to_dpi(x);
    }
    if let Some(y) = res(Tag::YResolution) {
        info.dpi_y = to_dpi(y);
    }

    let lat = gps_dms(exif, Tag::GPSLatitude, Tag::GPSLatitudeRef, b'S');
    let lon = gps_dms(exif, Tag::GPSLongitude, Tag::GPSLongitudeRef, b'W');
    if let (Some(la), Some(lo)) = (lat, lon) {
        info.gps = Some((la, lo));
    }
}

/// Decimal-degrees GPS from the DMS EXIF tags (module-level so the verbose reader can
/// share it). `neg_ref` is the ASCII ref byte that means a negative coordinate (S / W).
fn gps_dms(exif: &exif::Exif, coord: exif::Tag, refr: exif::Tag, neg_ref: u8) -> Option<f64> {
    use exif::{In, Value};
    let f = exif.get_field(coord, In::PRIMARY)?;
    let v = match &f.value {
        Value::Rational(r) if r.len() >= 3 => r,
        _ => return None,
    };
    let mut deg = v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0;
    if let Some(rf) = exif.get_field(refr, In::PRIMARY) {
        if let Value::Ascii(a) = &rf.value {
            if a.first().and_then(|s| s.first()) == Some(&neg_ref) {
                deg = -deg;
            }
        }
    }
    Some(deg)
}

/// Comprehensive metadata for the "Image info" dialog — file size/type, image
/// format/dimensions/colour, and EVERY EXIF tag (the verbose flavor; [`read_info`] is
/// the terse struct the CLI uses). Returns a ready-to-display multi-line string with LF
/// endings (the dialog converts to CRLF for the edit control).
pub fn read_info_verbose(path: &str) -> String {
    use std::fmt::Write as _;

    let p = std::path::Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or(path);
    let mut s = String::new();
    let _ = writeln!(s, "{name}\n{path}\n");

    write_file_section(&mut s, path, p);
    write_image_section(&mut s, path);
    let had_exif = write_exif_section(&mut s, path);
    let had_extra = write_extra_facts_section(&mut s, path);

    // Provenance metadata is neither EXIF nor XMP, so it belongs on its own row.
    // Presence only - we do not verify the signature or the claim behind it.
    let credentials = has_content_credentials(path);
    if credentials {
        let _ = writeln!(
            s,
            "\nContent Credentials (C2PA): present  (removable with Strip metadata)"
        );
    }
    if !had_exif && !credentials && !had_extra {
        let _ = writeln!(s, "(none)");
    }
    s
}

fn write_file_section(s: &mut String, path: &str, p: &Path) {
    use std::fmt::Write as _;
    let _ = writeln!(s, "── File ──");
    if let Ok(meta) = std::fs::metadata(path) {
        let len = meta.len();
        let _ = writeln!(
            s,
            "Size: {len} bytes  ({:.1} KB, {:.2} MB)",
            len as f64 / 1024.0,
            len as f64 / 1_048_576.0
        );
    }
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        let lc = ext.to_ascii_lowercase();
        let _ = writeln!(s, "Type: .{lc}  ({:?})", crate::formats::category(&lc));
    }
    let _ = writeln!(s);
}

fn write_image_section(s: &mut String, path: &str) {
    use image::ImageDecoder;
    use std::fmt::Write as _;
    let _ = writeln!(s, "── Image ──");
    let (mut w, mut h) = (0u32, 0u32);
    if let Ok(rdr) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
        if let Some(fmt) = rdr.format() {
            let _ = writeln!(s, "Format: {fmt:?}");
        }
        if let Ok(dec) = rdr.into_decoder() {
            let (dw, dh) = dec.dimensions();
            (w, h) = (dw, dh);
            let ct = dec.color_type();
            let _ = writeln!(
                s,
                "Color: {ct:?}  ({}-bit, {} channel(s))",
                ct.bits_per_pixel(),
                ct.channel_count()
            );
        }
    }
    if w == 0 && h == 0 {
        if let Ok(bytes) = std::fs::read(path) {
            if let Some((cw, ch)) = crate::container::real_or_decoded_dims(&bytes) {
                (w, h) = (cw, ch);
            }
        }
    }
    if w != 0 || h != 0 {
        let _ = writeln!(
            s,
            "Dimensions: {w} × {h} px  ({:.1} megapixels)",
            (w as f64 * h as f64) / 1_000_000.0
        );
    } else {
        let _ = writeln!(s, "Dimensions: unavailable");
    }
    let _ = writeln!(s);
}

/// Returns whether an EXIF container was actually found and read.
fn write_exif_section(s: &mut String, path: &str) -> bool {
    use exif::Reader;
    use std::fmt::Write as _;
    let _ = writeln!(s, "── EXIF / metadata ──");
    let mut had_exif = false;
    if let Ok(file) = std::fs::File::open(path) {
        let mut buf = std::io::BufReader::new(file);
        if let Ok(exif) = Reader::new().read_from_container(&mut buf) {
            had_exif = true;
            for f in exif.fields() {
                let _ = writeln!(s, "{}: {}", f.tag, f.display_value().with_unit(&exif));
            }
            let lat = gps_dms(
                &exif,
                exif::Tag::GPSLatitude,
                exif::Tag::GPSLatitudeRef,
                b'S',
            );
            let lon = gps_dms(
                &exif,
                exif::Tag::GPSLongitude,
                exif::Tag::GPSLongitudeRef,
                b'W',
            );
            if let (Some(la), Some(lo)) = (lat, lon) {
                let _ = writeln!(s, "\nGPS (decimal): {la:.6}, {lo:.6}");
                let _ = writeln!(s, "Map: https://maps.google.com/?q={la:.6},{lo:.6}");
            }
        }
    }
    had_exif
}

/// Facts EXIF has no field for. Each is best-effort: the file is read once, and anything
/// unrecognised simply contributes no row. Returns whether any row was written.
fn write_extra_facts_section(s: &mut String, path: &str) -> bool {
    use std::fmt::Write as _;
    let mut extra: Vec<(String, String)> = Vec::new();
    if let Ok(bytes) = std::fs::read(path) {
        if has_gain_map(&bytes) {
            extra.push((
                "HDR gain map".into(),
                "present (the tone-map item every iPhone HDR photo carries)".into(),
            ));
        }
        if let Some((mips, fmt)) = ddsinfo::describe(&bytes) {
            extra.push(("Texture compression".into(), fmt));
            extra.push((
                "Mip levels".into(),
                if mips == 1 {
                    "1 (no mip chain)".into()
                } else {
                    mips.to_string()
                },
            ));
        }
        if let Some(pkt) = xmpinfo::packet(&bytes) {
            extra.extend(xmpinfo::facts(&pkt).into_iter().map(|(l, v)| (l.into(), v)));
        }
    }
    let had_extra = !extra.is_empty();
    if had_extra {
        let _ = writeln!(s);
        for (label, value) in &extra {
            let _ = writeln!(s, "{label}: {value}");
        }
    }
    had_extra
}

/// Capture metadata for the EXIF batch-rename verb: when the shot was taken and
/// which camera took it, both as filename-ready strings (or None when absent).
#[derive(Default)]
pub struct CaptureMeta {
    /// Capture time as a filename-safe `"YYYY-MM-DD HH.MM.SS"` (no colons).
    pub time: Option<String>,
    /// Camera model (or make, if model is missing), trimmed.
    pub camera: Option<String>,
}

/// Read the EXIF capture time + camera for batch-rename. Unlike [`read_info`]
/// (which formats for a *display* MessageBox), this reads the RAW ASCII values so
/// the strings are clean enough to put in a filename, and reshapes the EXIF
/// `"YYYY:MM:DD HH:MM:SS"` into a colon-free form Windows accepts.
pub fn read_capture(path: &str) -> CaptureMeta {
    use exif::{In, Reader, Tag, Value};
    let mut out = CaptureMeta::default();

    let Ok(file) = std::fs::File::open(path) else {
        return out;
    };
    let mut buf = std::io::BufReader::new(file);
    let Ok(exif) = Reader::new().read_from_container(&mut buf) else {
        return out;
    };

    // Pull the first ASCII string of a tag, trimmed of trailing NULs/space.
    let ascii = |t: Tag| -> Option<String> {
        match &exif.get_field(t, In::PRIMARY)?.value {
            Value::Ascii(v) => {
                let s = String::from_utf8_lossy(v.first()?);
                let s = s.trim().trim_end_matches('\0').trim();
                (!s.is_empty()).then(|| s.to_string())
            }
            _ => None,
        }
    };

    out.time = ascii(Tag::DateTimeOriginal)
        .or_else(|| ascii(Tag::DateTime))
        .and_then(|s| format_exif_datetime(&s));
    // Model is usually the useful one ("Canon EOS R5"); fall back to Make.
    out.camera = ascii(Tag::Model).or_else(|| ascii(Tag::Make));
    out
}

/// Audio tags for the "Rename by tag" verb (artist/title/album/track), read via
/// `lofty` — the same crate (and read path) the album-art extractor uses.
#[derive(Default)]
pub struct AudioTags {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub track: Option<u32>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    /// Playback length in milliseconds (0 = unknown). Surfaced as `System.Media.Duration`.
    pub duration_ms: u64,
    /// Overall bitrate in kbps (0 = unknown). Surfaced as `System.Audio.EncodingBitrate`.
    pub bitrate_kbps: u32,
}

/// Read an audio file's primary tag (artist/album/title/track). Empty/missing
/// fields stay None. Mirrors `container::audio`'s proven `Probe` read path.
pub fn read_audio_tags(path: &str) -> AudioTags {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::Accessor;
    use std::io::Seek;

    let mut out = AudioTags::default();
    let Ok(mut file) = std::fs::File::open(path) else {
        return out;
    };
    // ASF/WMA: lofty has no ASF support, so read the tags ourselves (mirrors the
    // album-art path). Non-ASF returns None → the lofty path below runs unchanged.
    if let Some(t) = crate::container::audio_asf_tags(&mut file) {
        out.artist = t.artist;
        out.album = t.album;
        out.title = t.title;
        out.track = t.track;
        out.genre = t.genre;
        out.year = t.year;
        out.duration_ms = t.duration_ms;
        out.bitrate_kbps = t.bitrate_kbps;
        return out;
    }
    if file.seek(std::io::SeekFrom::Start(0)).is_err() {
        return out;
    }
    // Route through &mut dyn ReadSeek so lofty is monomorphized once across all callers
    // (see crate::container::ReadSeek), not separately for BufReader<File>.
    let mut br = std::io::BufReader::new(file);
    let Ok(probe) = Probe::new(&mut br as &mut dyn crate::container::ReadSeek).guess_file_type()
    else {
        return out;
    };
    let Ok(tagged) = probe.read() else {
        return out;
    };
    // Audio PROPERTIES (duration/bitrate) come from the decoded stream, not a tag — so
    // read them BEFORE the tag check: a perfectly valid file can have a duration but no
    // tags, and we still want its length in the Details pane.
    let props = tagged.properties();
    out.duration_ms = props.duration().as_millis() as u64;
    out.bitrate_kbps = props.overall_bitrate().unwrap_or(0);

    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return out;
    };

    let clean = |c: std::borrow::Cow<str>| {
        let s = c.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    out.artist = tag.artist().and_then(clean);
    out.album = tag.album().and_then(clean);
    out.title = tag.title().and_then(clean);
    out.track = tag.track();
    out.genre = tag.genre().and_then(clean);
    // lofty 0.25 replaced `Accessor::year()` with `date() -> Option<Timestamp>`, which reads
    // the same underlying fields (`RecordingDate`, falling back to `Year`) and then parses
    // them. We only ever wanted the year, so take that component back off.
    out.year = tag.date().map(|d| u32::from(d.year));
    out
}

/// Reshape an EXIF `DateTime` (`"YYYY:MM:DD HH:MM:SS"`) into a filename-safe
/// `"YYYY-MM-DD HH.MM.SS"`. Returns None for a malformed or all-zero stamp (some
/// cameras write `"0000:00:00 00:00:00"` when the clock was never set).
fn format_exif_datetime(s: &str) -> Option<String> {
    let (date, time) = s.split_once(' ')?;
    // EXIF uses ':' date separators; accept '-'/'/' too in case a tool rewrote it.
    let d: Vec<&str> = date.split([':', '-', '/']).collect();
    let t: Vec<&str> = time.split([':', '.']).collect();
    if d.len() != 3 || t.len() < 3 {
        return None;
    }
    // Every component must be all-ASCII-digits and non-empty.
    if !d
        .iter()
        .chain(t.iter().take(3))
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    // Reject the never-set clock (year/month/day all zero).
    if d[0].trim_start_matches('0').is_empty() || d[1] == "00" || d[2] == "00" {
        return None;
    }
    Some(format!(
        "{}-{}-{} {}.{}.{}",
        d[0], d[1], d[2], t[0], t[1], t[2]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_overwrite_notifies_the_rewritten_item_only_after_success() {
        let dir = std::env::temp_dir().join(format!("st2k_strip_notify_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rewritten.jpg");
        std::fs::write(&path, b"old").unwrap();

        let mut notified = None;
        atomic_overwrite_with(&path, b"new", |updated| {
            notified = Some(updated.to_path_buf())
        })
        .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(notified.as_deref(), Some(path.as_path()));

        let missing = dir.join("missing").join("never-written.jpg");
        let mut failed_notify = false;
        assert!(atomic_overwrite_with(&missing, b"new", |_| failed_notify = true).is_err());
        assert!(!failed_notify, "a failed rewrite must not notify Explorer");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The in-place rewrite must keep the file's identity: a plain temp+rename gave the
    /// name to a brand-new file, which dropped the Hidden attribute and every alternate
    /// data stream (the Zone.Identifier mark-of-the-web among them). `ReplaceFileW` keeps
    /// both, and the content is still the new bytes.
    #[test]
    fn atomic_overwrite_keeps_attributes_and_alternate_streams() {
        use windows::Win32::Storage::FileSystem::{
            GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
        };
        let dir = std::env::temp_dir().join(format!("st2k_strip_attrs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("marked.jpg");
        std::fs::write(&path, b"old").unwrap();
        // An NTFS alternate data stream; a non-NTFS temp volume cannot hold one, in which
        // case only the attribute half is checked.
        let ads = format!("{}:Zone.Identifier", path.display());
        let has_ads = std::fs::write(&ads, b"[ZoneTransfer]\r\nZoneId=3\r\n").is_ok();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
        unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }.unwrap();

        atomic_overwrite_with(&path, b"new", |_| {}).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let attrs = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
        assert_ne!(
            attrs & FILE_ATTRIBUTE_HIDDEN.0,
            0,
            "the Hidden attribute was lost across the rewrite"
        );
        if has_ads {
            assert!(
                std::fs::read(&ads)
                    .map(|b| b.starts_with(b"[ZoneTransfer]"))
                    .unwrap_or(false),
                "the alternate data stream was lost across the rewrite"
            );
        }
        assert!(
            !path.with_extension("jpg.st2ktmp").exists(),
            "temp file left behind"
        );

        unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_NORMAL) }.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Multi-Picture Format JPEG (APP2 `MPF\0`) is refused whole: its index names byte
    /// offsets that stripping would move, and the result is written over the original.
    #[test]
    fn refuses_to_strip_a_multi_picture_mpf_jpeg() {
        let dir = std::env::temp_dir().join(format!("st2k_strip_mpf_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("hdr.jpg");

        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            12,
            image::Rgb([40, 90, 160]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        let mut out = base[0..2].to_vec(); // SOI
        for (marker, payload) in [
            (markers::APP1, &b"Exif\0\0sometagdata"[..]),
            (
                markers::APP2,
                &b"MPF\0II*\0\x08\0\0\0secondary-image-index"[..],
            ),
        ] {
            out.extend_from_slice(&[0xFF, marker]);
            out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(payload);
        }
        out.extend_from_slice(&base[2..]);
        out.extend_from_slice(b"\xFF\xD8appended-gain-map\xFF\xD9");
        std::fs::write(&jpg, &out).unwrap();

        assert!(
            strip_metadata(jpg.to_str().unwrap()).is_err(),
            "MPF must refuse"
        );
        assert_eq!(
            std::fs::read(&jpg).unwrap(),
            out,
            "a refused strip must leave the file byte-for-byte as it was"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minimal little-endian TIFF/EXIF block carrying a single `Make` tag.
    /// Layout: header(8) | entry count(2) | one 12-byte entry | next-IFD(4) |
    /// the ASCII value at offset 26.
    fn tiny_exif(make: &[u8; 6]) -> Vec<u8> {
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
        v.extend_from_slice(&1u16.to_le_bytes()); // one entry
        v.extend_from_slice(&0x010Fu16.to_le_bytes()); // Make
        v.extend_from_slice(&2u16.to_le_bytes()); // ASCII
        v.extend_from_slice(&6u32.to_le_bytes()); // count
        v.extend_from_slice(&26u32.to_le_bytes()); // value offset
        v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        v.extend_from_slice(make);
        v
    }

    /// PNG has carried real EXIF in an `eXIf` chunk since the 2017 spec change,
    /// and the competitor sweep flagged it as something we might be ignoring.
    /// We are not: `kamadak-exif` reads the chunk, so `read_info` fills in from a
    /// PNG exactly as it does from a JPEG. This test is what proves it stays true.
    #[test]
    fn reads_exif_from_a_png_exif_chunk() {
        let dir = std::env::temp_dir().join(format!("st2k_png_exif_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png_path = dir.join("e.png");

        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([9, 9, 9])))
            .write_to(
                &mut std::io::Cursor::new(&mut base),
                image::ImageFormat::Png,
            )
            .unwrap();

        let mut png = Png::from_bytes(Bytes::from(base)).unwrap();
        let chunk = img_parts::png::PngChunk::new(*b"eXIf", Bytes::from(tiny_exif(b"SageT\0")));
        png.chunks_mut().insert(1, chunk);
        std::fs::write(&png_path, png.encoder().bytes()).unwrap();

        let info = read_info(png_path.to_str().unwrap());
        assert_eq!(info.width, 4);
        assert_eq!(
            info.make.as_deref(),
            Some("SageT"),
            "PNG eXIf chunk was not read"
        );

        // ...and Strip removes it, which the eXIf entry in the PNG arm covers.
        strip_metadata(png_path.to_str().unwrap()).unwrap();
        let after = read_info(png_path.to_str().unwrap());
        assert_eq!(after.make, None, "PNG eXIf survived the strip");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole point of the APP11 work: a C2PA manifest goes, a JPEG XT layer
    /// wearing the same marker stays, and the pixels are untouched either way.
    #[test]
    fn strips_c2pa_app11_but_keeps_a_jpeg_xt_layer() {
        let dir = std::env::temp_dir().join(format!("st2k_c2pa_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("c.jpg");

        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            12,
            image::Rgb([10, 20, 30]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();

        // `JP` + box instance + packet sequence + LBox + TBox + payload.
        let app11 = |tbox: &[u8; 4], tail: &[u8]| {
            let mut v = b"JP".to_vec();
            v.extend_from_slice(&[0, 1, 0, 0, 0, 1]);
            v.extend_from_slice(&64u32.to_be_bytes());
            v.extend_from_slice(tbox);
            v.extend_from_slice(tail);
            v
        };
        let mut out = base[0..2].to_vec(); // SOI
        for payload in [
            app11(b"jumb", b"c2pa-manifest-store"),
            app11(b"xtld", b"jpegxt-hdr-layer"),
        ] {
            out.extend_from_slice(&[0xFF, markers::APP11]);
            out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(&payload);
        }
        out.extend_from_slice(&base[2..]);
        std::fs::write(&jpg, &out).unwrap();

        let path = jpg.to_str().unwrap();
        assert!(
            has_content_credentials(path),
            "setup must carry a C2PA manifest"
        );

        strip_metadata(path).unwrap();

        let after = std::fs::read(&jpg).unwrap();
        assert!(
            !after.windows(19).any(|w| w == b"c2pa-manifest-store"),
            "C2PA manifest survived the strip"
        );
        assert!(
            after.windows(16).any(|w| w == b"jpegxt-hdr-layer"),
            "the JPEG XT layer was collateral damage"
        );
        assert!(!has_content_credentials(path));
        let d = image::open(&jpg).unwrap();
        assert_eq!(
            (d.width(), d.height()),
            (16, 12),
            "pixels must be untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A C2PA manifest past ~64KB spans more than one APP11 segment: only the FIRST
    /// packet (sequence 1) carries the `LBox`/`TBox` header `is_jumbf_app11` matches on,
    /// so a per-segment-only filter left later packets (sequence > 1, same box instance)
    /// behind - the manifest fragment survived even though `has_content_credentials`
    /// reported `false`.
    #[test]
    fn strips_every_continuation_packet_of_a_multi_segment_c2pa_manifest() {
        let dir = std::env::temp_dir().join(format!("st2k_c2pa_multi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("c.jpg");

        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            12,
            image::Rgb([10, 20, 30]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();

        // Packet 1 of box instance 1: carries the LBox/TBox header, TBox == "jumb".
        let mut first = b"JP".to_vec();
        first.extend_from_slice(&[0, 1]); // box instance 1
        first.extend_from_slice(&[0, 0, 0, 1]); // sequence 1
        first.extend_from_slice(&64u32.to_be_bytes()); // LBox
        first.extend_from_slice(b"jumb"); // TBox
        first.extend_from_slice(b"manifest-part-one");
        // Packet 2 of the SAME box instance: sequence 2, no LBox/TBox of its own - a real
        // continuation packet, exactly what `is_jumbf_app11` can never match directly.
        let mut second = b"JP".to_vec();
        second.extend_from_slice(&[0, 1]); // same box instance
        second.extend_from_slice(&[0, 0, 0, 2]); // sequence 2
        second.extend_from_slice(b"manifest-part-two");

        let mut out = base[0..2].to_vec(); // SOI
        for payload in [first, second] {
            out.extend_from_slice(&[0xFF, markers::APP11]);
            out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(&payload);
        }
        out.extend_from_slice(&base[2..]);
        std::fs::write(&jpg, &out).unwrap();

        let path = jpg.to_str().unwrap();
        assert!(
            has_content_credentials(path),
            "setup must carry a C2PA manifest"
        );

        strip_metadata(path).unwrap();

        let after = std::fs::read(&jpg).unwrap();
        assert!(
            !after.windows(18).any(|w| w == b"manifest-part-one"),
            "the box-defining packet survived the strip"
        );
        assert!(
            !after.windows(18).any(|w| w == b"manifest-part-two"),
            "the continuation packet survived the strip"
        );
        assert!(!has_content_credentials(path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Before this fix, the match arm `"svg" | "svgz" if ext == "svg"` could only ever
    /// be true for `ext == "svg"`, so a real `.svgz` always fell through to the
    /// unsupported case and `strip_metadata` refused every compressed SVG.
    #[test]
    fn strips_metadata_from_a_gzip_compressed_svgz_file() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("st2k_svgz_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("logo.svgz");

        let svg = concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">\n",
            "  <title>Company logo FINAL v3</title>\n",
            "  <path d=\"M0 0h10v10z\"/>\n</svg>\n"
        );
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(svg.as_bytes()).unwrap();
        std::fs::write(&path, gz.finish().unwrap()).unwrap();

        strip_metadata(path.to_str().unwrap()).unwrap();

        let rewritten = std::fs::read(&path).unwrap();
        let inflated = gunzip_bounded(&rewritten).expect("output must still be valid gzip");
        let text = String::from_utf8(inflated).unwrap();
        assert!(!text.contains("Company logo"), "{text}");
        assert!(
            text.contains("<path d=\"M0 0h10v10z\"/>"),
            "art damaged: {text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strips_jpeg_app1_exif_losslessly() {
        let dir = std::env::temp_dir().join(format!("st2k_strip_exif_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("e.jpg");

        // A baseline JPEG, then splice a fake APP1 "Exif" segment in after SOI.
        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            12,
            image::Rgb([40, 90, 160]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        let payload = b"Exif\0\0sometagdata".to_vec();
        let len = (payload.len() + 2) as u16;
        let mut with_exif = Vec::new();
        with_exif.extend_from_slice(&base[0..2]); // SOI
        with_exif.extend_from_slice(&[0xFF, 0xE1]); // APP1
        with_exif.extend_from_slice(&len.to_be_bytes());
        with_exif.extend_from_slice(&payload);
        with_exif.extend_from_slice(&base[2..]);
        std::fs::write(&jpg, &with_exif).unwrap();
        assert!(
            with_exif.windows(4).any(|w| w == b"Exif"),
            "setup must contain Exif"
        );

        strip_metadata(jpg.to_str().unwrap()).unwrap();

        let after = std::fs::read(&jpg).unwrap();
        assert!(
            !after.windows(4).any(|w| w == b"Exif"),
            "Exif should be stripped"
        );
        let d = image::open(&jpg).unwrap();
        assert_eq!(
            (d.width(), d.height()),
            (16, 12),
            "pixels must be untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn formats_exif_datetime_filename_safe() {
        assert_eq!(
            format_exif_datetime("2023:05:01 14:30:09"),
            Some("2023-05-01 14.30.09".to_string())
        );
        // Subsecond/odd separators tolerated; reject the never-set clock + junk.
        assert_eq!(format_exif_datetime("0000:00:00 00:00:00"), None);
        assert_eq!(format_exif_datetime("not a date"), None);
        assert_eq!(format_exif_datetime("2023:05 14:30:00"), None);
    }

    #[test]
    fn read_info_returns_dimensions() {
        let dir = std::env::temp_dir().join(format!("st2k_info_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("i.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(33, 22))
            .save(&png)
            .unwrap();
        let info = read_info(png.to_str().unwrap());
        assert_eq!((info.width, info.height), (33, 22));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bounded_info_reads_psd_dimensions_from_the_header() {
        let dir = std::env::temp_dir().join(format!("st2k_bounded_info_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let psd = dir.join("large-looking.psd");

        // PSD dimensions live in the fixed 26-byte header. The deliberately large tail models
        // a document that must not be read or decoded merely to fill Explorer's Details pane.
        let mut bytes = Vec::with_capacity(8 * 1024 * 1024);
        bytes.extend_from_slice(b"8BPS");
        bytes.extend_from_slice(&[0, 1]);
        bytes.extend_from_slice(&[0; 6]);
        bytes.extend_from_slice(&3u16.to_be_bytes());
        bytes.extend_from_slice(&4321u32.to_be_bytes()); // height @ 14
        bytes.extend_from_slice(&8765u32.to_be_bytes()); // width @ 18
        bytes.extend_from_slice(&[0; 8]);
        bytes.resize(8 * 1024 * 1024, 0);
        std::fs::write(&psd, bytes).unwrap();

        let info = read_info_bounded(psd.to_str().unwrap());
        assert_eq!((info.width, info.height), (8765, 4321));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
