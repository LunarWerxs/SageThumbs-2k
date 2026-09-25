//! Reading a file's facts for the Info verb and `st2k info`: dimensions, EXIF, GPS, capture data, audio tags, and the verbose report.

use super::*;

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
pub(super) fn head_prefix(path: &str) -> Option<Vec<u8>> {
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
pub(super) const EXIF_SCAN_CAP: u64 = 32 * 1024 * 1024;

/// A `Read + Seek` view of the first `cap` bytes of `inner`: reads at or past the cap
/// return end-of-file, seeks are passed through. `exif::Reader::read_from_container`
/// needs both traits, which a plain `Take` does not provide.
pub(super) struct CappedReader<R> {
    pub(super) inner: R,
    pub(super) pos: u64,
    pub(super) cap: u64,
}

impl<R: std::io::Read> std::io::Read for CappedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let room = self.cap.saturating_sub(self.pos);
        let want = (buf.len() as u64).min(room) as usize;
        let window = &mut buf[..want];
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

pub(super) fn read_info_impl(path: &str, bounded: bool) -> ImageInfo {
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
pub(super) fn resolve_image_dimensions(path: &str, bounded: bool) -> ImageInfo {
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
pub(super) fn resolve_dims_via_full_decode(path: &str, info: &mut ImageInfo) {
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
            st2k_base::formats::category(&ext),
            st2k_base::formats::Category::Video
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
        st2k_base::safety::log_debugf!("read_info: could not determine dimensions for {path}");
    }
}

/// The first ASCII string of primary-IFD `tag`, trimmed of surrounding whitespace
/// and of the trailing NUL padding cameras write. None when the tag is absent,
/// empty, or not an ASCII field.
///
/// Raw ASCII rather than `display_value`: the latter renders an ASCII field wrapped
/// in literal double quotes, so Explorer's "Camera maker" column showed `"Canon"`
/// rather than `Canon`.
fn exif_ascii(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    use exif::{In, Value};
    match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Ascii(v) => {
            let s = String::from_utf8_lossy(v.first()?);
            let s = s.trim().trim_end_matches('\0').trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        _ => None,
    }
}

/// Fill in make/model/capture-time/DPI/GPS from a decoded EXIF container.
pub(super) fn apply_exif_metadata(info: &mut ImageInfo, exif: &exif::Exif) {
    use exif::{In, Tag, Value};
    let txt = |t: Tag| {
        exif.get_field(t, In::PRIMARY)
            .map(|f| f.display_value().with_unit(exif).to_string())
    };
    info.make = exif_ascii(exif, Tag::Make);
    info.model = exif_ascii(exif, Tag::Model);
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
pub(super) fn gps_dms(
    exif: &exif::Exif,
    coord: exif::Tag,
    refr: exif::Tag,
    neg_ref: u8,
) -> Option<f64> {
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

pub(super) fn write_file_section(s: &mut String, path: &str, p: &Path) {
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
        let _ = writeln!(s, "Type: .{lc}  ({:?})", st2k_base::formats::category(&lc));
    }
    let _ = writeln!(s);
}

pub(super) fn write_image_section(s: &mut String, path: &str) {
    use std::fmt::Write as _;
    let _ = writeln!(s, "── Image ──");
    let (mut w, mut h) = decode_image_meta(s, path);
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

/// Reads image format and decoder metadata, returning the decoded dimensions.
fn decode_image_meta(s: &mut String, path: &str) -> (u32, u32) {
    use image::ImageDecoder;
    use std::fmt::Write as _;
    let mut dims = (0u32, 0u32);
    if let Ok(rdr) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
        if let Some(fmt) = rdr.format() {
            let _ = writeln!(s, "Format: {fmt:?}");
        }
        if let Ok(dec) = rdr.into_decoder() {
            dims = dec.dimensions();
            let ct = dec.color_type();
            let _ = writeln!(
                s,
                "Color: {ct:?}  ({}-bit, {} channel(s))",
                ct.bits_per_pixel(),
                ct.channel_count()
            );
        }
    }
    dims
}

/// Returns whether an EXIF container was actually found and read.
pub(super) fn write_exif_section(s: &mut String, path: &str) -> bool {
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
pub(super) fn write_extra_facts_section(s: &mut String, path: &str) -> bool {
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
    use exif::{Reader, Tag};
    let mut out = CaptureMeta::default();

    let Ok(file) = std::fs::File::open(path) else {
        return out;
    };
    let mut buf = std::io::BufReader::new(file);
    let Ok(exif) = Reader::new().read_from_container(&mut buf) else {
        return out;
    };

    out.time = exif_ascii(&exif, Tag::DateTimeOriginal)
        .or_else(|| exif_ascii(&exif, Tag::DateTime))
        .and_then(|s| format_exif_datetime(&s));
    // Model is usually the useful one ("Canon EOS R5"); fall back to Make.
    out.camera = exif_ascii(&exif, Tag::Model).or_else(|| exif_ascii(&exif, Tag::Make));
    out
}

/// The tags `read_audio_tags` returns; defined next to the ASF reader that fills the same
/// struct directly (one type, not a lofty copy and an ASF copy with the same eight fields).
pub use crate::container::AudioTags;

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
        return t;
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

/// Split an EXIF-style `"DATE TIME"` stamp (`"YYYY:MM:DD HH:MM:SS"`) into its three date and
/// at-least-three time components. `:` is the EXIF date separator, but `-`/`/` are tolerated in
/// case a tool rewrote the stamp; a trailing sub-seconds field keeps `t` at four elements.
/// Returns `None` unless both halves have that shape. The components themselves are NOT
/// validated here — digits-only and never-set-clock checks stay in the callers
/// (`format_exif_datetime` here and the property handler's `DateTaken`).
pub fn split_exif_datetime(s: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    let (date, time) = s.split_once(' ')?;
    let d: Vec<&str> = date.split([':', '-', '/']).collect();
    let t: Vec<&str> = time.split([':', '.']).collect();
    if d.len() != 3 || t.len() < 3 {
        return None;
    }
    Some((d, t))
}

/// Reshape an EXIF `DateTime` (`"YYYY:MM:DD HH:MM:SS"`) into a filename-safe
/// `"YYYY-MM-DD HH.MM.SS"`. Returns None for a malformed or all-zero stamp (some
/// cameras write `"0000:00:00 00:00:00"` when the clock was never set).
pub(super) fn format_exif_datetime(s: &str) -> Option<String> {
    let (d, t) = split_exif_datetime(s)?;
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

    fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("st2k-info-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// The head sniff reads at most 64 bytes and refuses a file too short to hold any of the
    /// signatures it is used for (26 bytes), rather than handing back a stub to misread.
    #[test]
    fn head_prefix_caps_at_64_bytes_and_refuses_a_stub() {
        let long = temp_file("long.bin", &[7u8; 200]);
        assert_eq!(
            head_prefix(long.to_str().unwrap()).map(|b| b.len()),
            Some(64)
        );
        let short = temp_file("short.bin", &[7u8; 25]);
        assert_eq!(head_prefix(short.to_str().unwrap()), None);
        let exact = temp_file("exact.bin", &[7u8; 26]);
        assert_eq!(
            head_prefix(exact.to_str().unwrap()).map(|b| b.len()),
            Some(26)
        );
        for p in [long, short, exact] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn head_prefix_is_none_for_a_missing_file() {
        let missing =
            std::env::temp_dir().join(format!("st2k-info-{}-missing.bin", std::process::id()));
        assert_eq!(head_prefix(missing.to_str().unwrap()), None);
    }

    /// A never-set camera clock writes zeros; any one zero date field is enough to refuse it,
    /// and a non-digit time field is refused too, so no filename is built from a bogus stamp.
    #[test]
    fn a_date_with_any_zero_field_or_a_non_digit_time_is_refused() {
        assert_eq!(format_exif_datetime("2023:00:05 10:00:00"), None);
        assert_eq!(format_exif_datetime("2023:05:00 10:00:00"), None);
        assert_eq!(format_exif_datetime("0000:05:05 10:00:00"), None);
        assert_eq!(format_exif_datetime("2023:05:05 1a:00:00"), None);
        assert_eq!(
            format_exif_datetime("2023:05:05 10:20:30").as_deref(),
            Some("2023-05-05 10.20.30")
        );
    }
}
