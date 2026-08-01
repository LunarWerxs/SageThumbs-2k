//! Lossless metadata strip for JPEG, PNG and WebP — a segment/chunk rewrite, NO
//! pixel re-encode (so a photo never loses quality). Removes EXIF / IPTC / XMP /
//! comments, and **C2PA "Content Credentials"** (see [`jumbf`]), which is neither
//! of those and therefore survives every EXIF-only scrubber.
//! Plus `read_info`, an EXIF reader for the "Image info" verb (reuses the
//! already-present `kamadak-exif` + `image` — no new deps for that part).
//!
//! The ICC color profile (JPEG APP2 / PNG iCCP) is deliberately KEPT — stripping
//! it shifts colors on wide-gamut displays.

use std::path::{Path, PathBuf};

use img_parts::jpeg::{markers, Jpeg};
use img_parts::png::Png;
use img_parts::Bytes;
use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;

use crate::verbs::read_capped;

mod ddsinfo;
mod isobmff;
mod jumbf;
mod svgmeta;
mod webpmeta;
mod xmpinfo;

pub use isobmff::has_gain_map;
pub use jumbf::has_content_credentials;

/// JPEG markers we drop: Exif + XMP (both APP1), Photoshop/IPTC (APP13), and the
/// free-text comment (COM). APP2 (ICC) is intentionally omitted.
///
/// APP11 is NOT in this list because it is marker-ambiguous: JPEG XT uses it for
/// HDR extension layers. It is filtered per-segment instead, in [`jumbf`].
const STRIP_APP_MARKERS: &[u8] = &[markers::APP1, markers::APP13, markers::COM];

/// Strip metadata from `path` in place (JPEG / PNG / WebP). Re-parses the rewritten
/// bytes before swapping, so a malformed rewrite can never clobber the original.
pub fn strip_metadata(path: &str) -> Result<()> {
    let input = Bytes::from(read_capped(path)?);
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let out_bytes: Vec<u8> = match ext.as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => {
            let mut jpeg = Jpeg::from_bytes(input).map_err(|_| Error::from(E_FAIL))?;
            jpeg.segments_mut().retain(|s| {
                if STRIP_APP_MARKERS.contains(&s.marker()) {
                    return false;
                }
                // C2PA / Content Credentials: a JUMBF box spread over APP11
                // segments. Only the `jumb` ones go - a JPEG XT HDR layer wears
                // the same marker and must survive.
                !(s.marker() == markers::APP11 && jumbf::is_jumbf_app11(s.contents()))
            });
            let bytes = jpeg.encoder().bytes();
            Jpeg::from_bytes(bytes.clone()).map_err(|_| Error::from(E_FAIL))?; // sanity re-parse
            bytes.to_vec()
        }
        "png" => {
            let mut png = Png::from_bytes(input).map_err(|_| Error::from(E_FAIL))?;
            // iCCP (color profile) intentionally NOT removed.
            for k in [b"eXIf", b"tEXt", b"iTXt", b"zTXt", b"tIME"] {
                png.remove_chunks_by_type(*k);
            }
            png.remove_chunks_by_type(jumbf::PNG_C2PA_CHUNK);
            let bytes = png.encoder().bytes();
            Png::from_bytes(bytes.clone()).map_err(|_| Error::from(E_FAIL))?;
            bytes.to_vec()
        }
        "webp" => webpmeta::strip(input)?,
        "svg" | "svgz" if ext == "svg" => svgmeta::strip(&input)?,
        // HEIC/AVIF items are rewritten in place (see `isobmff`); `None` means the
        // layout was not one we can touch without risking the picture.
        "heic" | "heif" | "hif" | "avif" => {
            isobmff::strip(&input).ok_or_else(|| Error::from(E_FAIL))?
        }
        _ => return Err(Error::from(E_FAIL)), // unsupported: refuse, never lossy-convert
    };

    atomic_overwrite(Path::new(path), &out_bytes)
}

/// In-place overwrite via a same-volume temp + rename, with a short retry so a
/// transient Explorer/thumbnail-cache lock (os error 5/32) doesn't fail it.
fn atomic_overwrite(dst: &Path, data: &[u8]) -> Result<()> {
    let tmp: PathBuf = {
        let mut s = dst.to_path_buf().into_os_string();
        s.push(".st2ktmp");
        PathBuf::from(s)
    };
    std::fs::write(&tmp, data).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    crate::fsutil::rename_retrying(&tmp, dst).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })
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

fn read_info_impl(path: &str, bounded: bool) -> ImageInfo {
    use exif::{In, Reader, Tag, Value};
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
    // Formats the image crate cannot probe (PSD, EPS, HEIC/RAW, containers) get a small
    // container-header probe next. Explicit callers may then use the full-fidelity fallback;
    // property-handler callers intentionally stop after headers, so a Details-pane request can
    // never materialize the entire file or start an ImageMagick/WIC decode.
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
        let bytes = std::fs::read(path).ok();
        if let Some(bytes) = bytes {
            if let Some((w, h)) = crate::container::real_dims(&bytes).or_else(|| {
                crate::decode::decode_full(&bytes)
                    .ok()
                    .map(|i| (i.width(), i.height()))
            }) {
                info.width = w;
                info.height = h;
            }
        }
        // VIDEO last resort for explicit callers only — `frame_from_path` can spawn a long-lived
        // Media Foundation worker, so it is never part of the in-shell property path.
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

    let Ok(file) = std::fs::File::open(path) else {
        return info;
    };
    let mut buf = std::io::BufReader::new(file);
    let Ok(exif) = Reader::new().read_from_container(&mut buf) else {
        return info;
    };

    let txt = |t: Tag| {
        exif.get_field(t, In::PRIMARY)
            .map(|f| f.display_value().with_unit(&exif).to_string())
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

    let lat = gps_dms(&exif, Tag::GPSLatitude, Tag::GPSLatitudeRef, b'S');
    let lon = gps_dms(&exif, Tag::GPSLongitude, Tag::GPSLongitudeRef, b'W');
    if let (Some(la), Some(lo)) = (lat, lon) {
        info.gps = Some((la, lo));
    }
    info
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
    use exif::Reader;
    use image::ImageDecoder;
    use std::fmt::Write as _;

    let p = std::path::Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or(path);
    let mut s = String::new();
    let _ = writeln!(s, "{name}\n{path}\n");

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
            if let Some((cw, ch)) = crate::container::real_dims(&bytes).or_else(|| {
                crate::decode::decode_full(&bytes)
                    .ok()
                    .map(|i| (i.width(), i.height()))
            }) {
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
    // Facts EXIF has no field for. Each is best-effort: the file is read once,
    // and anything unrecognised simply contributes no row.
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
    if !extra.is_empty() {
        let _ = writeln!(s);
        for (label, value) in &extra {
            let _ = writeln!(s, "{label}: {value}");
        }
    }

    // Provenance metadata is neither EXIF nor XMP, so it belongs on its own row.
    // Presence only - we do not verify the signature or the claim behind it.
    let credentials = has_content_credentials(path);
    if credentials {
        let _ = writeln!(
            s,
            "\nContent Credentials (C2PA): present  (removable with Strip metadata)"
        );
    }
    if !had_exif && !credentials && extra.is_empty() {
        let _ = writeln!(s, "(none)");
    }
    s
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
    out.year = tag.year();
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
