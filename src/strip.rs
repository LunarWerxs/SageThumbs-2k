//! Lossless metadata strip (EXIF / IPTC / XMP / comments) for JPEG & PNG — a
//! segment/chunk rewrite, NO pixel re-encode (so a photo never loses quality).
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

/// JPEG markers we drop: Exif + XMP (both APP1), Photoshop/IPTC (APP13), and the
/// free-text comment (COM). APP2 (ICC) is intentionally omitted.
const STRIP_APP_MARKERS: &[u8] = &[markers::APP1, markers::APP13, markers::COM];

/// Strip metadata from `path` in place (JPEG/PNG only). Re-parses the rewritten
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
            jpeg.segments_mut().retain(|s| !STRIP_APP_MARKERS.contains(&s.marker()));
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
            let bytes = png.encoder().bytes();
            Png::from_bytes(bytes.clone()).map_err(|_| Error::from(E_FAIL))?;
            bytes.to_vec()
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

/// [`read_info`] for the in-process property handler. The dimension fallback reads at most
/// `decode::limits::MAX_INPUT_BYTES` via [`crate::decode::read_capped`] (which SKIPS a larger
/// file before allocating) instead of slurping an arbitrarily large one into the caller's
/// address space. A genuinely oversized media file then reports no dimensions — an image
/// decoder can't derive them anyway, and not freezing the host is worth more than a
/// Details-pane number. `propstore` additionally runs this under a wall-clock budget off the
/// host thread, so even a slow in-cap decode can't stall the shell.
pub fn read_info_bounded(path: &str) -> ImageInfo {
    read_info_impl(path, true)
}

fn read_info_impl(path: &str, bounded: bool) -> ImageInfo {
    use exif::{In, Reader, Tag};
    let mut info = ImageInfo::default();

    if let Ok(rdr) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
        if let Ok((w, h)) = rdr.into_dimensions() {
            info.width = w;
            info.height = h;
        }
    }
    // Formats the image crate can't probe (PSD, EPS, HEIC/RAW, containers): the
    // cheap container header probe first, then a full-fidelity decode — so
    // "Image info" / `st2k info` report the REAL document size, not 0×0 and not
    // the embedded preview's size. A `bounded` caller caps the read at the 256 MiB
    // input ceiling and skips anything larger (see `read_info_bounded`); the unbounded
    // caller slurps the whole file for the true size of an arbitrarily large document.
    if info.width == 0 && info.height == 0 {
        let bytes = if bounded {
            crate::decode::read_capped(path).ok()
        } else {
            std::fs::read(path).ok()
        };
        if let Some(bytes) = bytes {
            if let Some((w, h)) = crate::container::real_dims(&bytes)
                .or_else(|| crate::decode::decode_full(&bytes).ok().map(|i| (i.width(), i.height())))
            {
                info.width = w;
                info.height = h;
            }
        }
        if info.width == 0 && info.height == 0 {
            // All probes (image-crate header, container canvas, full decode) failed
            // — leave a breadcrumb so a "shows no dimensions" report is diagnosable
            // instead of silently surfacing the 0×0 sentinel.
            crate::safety::log_debug(&format!("read_info: could not determine dimensions for {path}"));
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
    info.make = txt(Tag::Make);
    info.model = txt(Tag::Model);
    info.datetime = txt(Tag::DateTimeOriginal).or_else(|| txt(Tag::DateTime));

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
            if let Some((cw, ch)) = crate::container::real_dims(&bytes)
                .or_else(|| crate::decode::decode_full(&bytes).ok().map(|i| (i.width(), i.height())))
            {
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
            let lat = gps_dms(&exif, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef, b'S');
            let lon = gps_dms(&exif, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef, b'W');
            if let (Some(la), Some(lo)) = (lat, lon) {
                let _ = writeln!(s, "\nGPS (decimal): {la:.6}, {lo:.6}");
                let _ = writeln!(s, "Map: https://maps.google.com/?q={la:.6},{lo:.6}");
            }
        }
    }
    if !had_exif {
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
}

/// Read an audio file's primary tag (artist/album/title/track). Empty/missing
/// fields stay None. Mirrors `container::audio`'s proven `Probe` read path.
pub fn read_audio_tags(path: &str) -> AudioTags {
    use lofty::file::TaggedFileExt;
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
        return out;
    }
    if file.seek(std::io::SeekFrom::Start(0)).is_err() {
        return out;
    }
    // Route through &mut dyn ReadSeek so lofty is monomorphized once across all callers
    // (see crate::container::ReadSeek), not separately for BufReader<File>.
    let mut br = std::io::BufReader::new(file);
    let Ok(probe) = Probe::new(&mut br as &mut dyn crate::container::ReadSeek).guess_file_type() else {
        return out;
    };
    let Ok(tagged) = probe.read() else {
        return out;
    };
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
    if !d.iter().chain(t.iter().take(3)).all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    // Reject the never-set clock (year/month/day all zero).
    if d[0].trim_start_matches('0').is_empty() || d[1] == "00" || d[2] == "00" {
        return None;
    }
    Some(format!("{}-{}-{} {}.{}.{}", d[0], d[1], d[2], t[0], t[1], t[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_jpeg_app1_exif_losslessly() {
        let dir = std::env::temp_dir().join("st2k_strip_exif");
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("e.jpg");

        // A baseline JPEG, then splice a fake APP1 "Exif" segment in after SOI.
        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 12, image::Rgb([40, 90, 160])))
            .write_to(&mut std::io::Cursor::new(&mut base), image::ImageFormat::Jpeg)
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
        assert!(with_exif.windows(4).any(|w| w == b"Exif"), "setup must contain Exif");

        strip_metadata(jpg.to_str().unwrap()).unwrap();

        let after = std::fs::read(&jpg).unwrap();
        assert!(!after.windows(4).any(|w| w == b"Exif"), "Exif should be stripped");
        let d = image::open(&jpg).unwrap();
        assert_eq!((d.width(), d.height()), (16, 12), "pixels must be untouched");
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
        let dir = std::env::temp_dir().join("st2k_info");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("i.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(33, 22)).save(&png).unwrap();
        let info = read_info(png.to_str().unwrap());
        assert_eq!((info.width, info.height), (33, 22));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
