//! Ebook / comic-archive cover extraction — the native-Rust port of DarkThumbs.
//!
//! The shell hands us a byte stream with no extension, so we CONTENT-SNIFF the
//! container by its magic bytes and pull out the cover image. The cover bytes
//! then flow back through the normal tiered decoder (`decode::decode_full` ->
//! `decode_image`), so we add zero new image-decode code — only cover *finding*.
//!
//! Everything here runs in Explorer's thumbnail host under `panic = "abort"`, so
//! every parser works on `&[u8]` with checked slicing and bounded allocation; on
//! any malformed input we return `None` and the shell shows the default icon.

use image::DynamicImage;

mod affinity;
mod audio;
mod blend;
// Cinema 4D (.c4d) — carve the embedded document/scene preview JPEG.
mod c4d;
// CorelDRAW (.cdr/.cdt) / Corel Exchange (.cmx) — RIFF DISP preview DIB → BMP.
mod cdr;
mod clip;
// DjVu (.djvu) cover decode — via the maintained pure-Rust `djvu-rs` crate (see djvu.rs).
mod djvu;
mod dwg;
mod eps;
mod indd;
mod epub;
mod gcode;
mod fb2;
// Amiga / Deluxe Paint IFF ILBM (.iff/.ilbm/.lbm) — a real planar-bitmap decoder.
mod ilbm;
mod max;
mod mobi;
mod office;
mod ole;
mod project;
mod psd;
mod psp;
mod rar;
mod rhino;
mod select;
mod sevenz;
mod skp;
mod tarfmt;
mod util;
// Waveform thumbnails for raw-PCM audio (WAV/AIFF) with no embedded cover art.
mod waveform;
mod zipfmt;

/// A cover: either raw image-file bytes (re-decoded by the image tiers) or
/// already-decoded pixels (DjVu, which is not a standalone image file).
pub enum CoverOut {
    Bytes(Vec<u8>),
    Image(DynamicImage),
}

/// Max bytes we'll read for one cover entry (DarkThumbs' CBXMEM cap, 32 MiB).
pub(crate) const MAX_COVER: u64 = 32 * 1024 * 1024;

/// Do the leading bytes look like a raster image format our tiers can actually
/// render (JPEG / PNG / GIF / BMP / WebP)? Container extractors use this to reject
/// embedded previews we can't decode (e.g. EMF/WMF). Shared magic-byte predicate
/// for `office`, `project`, and `mobi` so the accept set stays in one place.
pub(crate) fn looks_like_raster(data: &[u8]) -> bool {
    data.starts_with(&[0xFF, 0xD8, 0xFF]) // JPEG
        || data.starts_with(&[0x89, b'P', b'N', b'G']) // PNG
        || data.starts_with(b"GIF8") // GIF
        || data.starts_with(b"BM") // BMP
        || (data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP") // WebP
        // Windows metafiles — decodable since we added emf/wmf (magick tier). Lets
        // container previews stored as EMF/WMF through (e.g. Visio docProps/thumbnail.emf).
        || (data.len() >= 44 && data[0..4] == [0x01, 0x00, 0x00, 0x00] && &data[40..44] == b" EMF") // EMF
        || data.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A]) // placeable WMF
        || data.starts_with(&[0x01, 0x00, 0x09, 0x00, 0x00, 0x03]) // standard (memory) WMF METAHEADER
}

/// Does `head` (the first bytes of a file) look like an audio container that may
/// carry embedded cover art? Lets the thumbnail provider take the memory-light
/// seek path instead of reading the whole (possibly huge) file.
pub fn looks_like_audio(head: &[u8]) -> bool {
    audio::looks_like_audio(head)
}

/// Album art from a seekable reader (the shell's IStream). lofty seeks to the
/// metadata, so we read only what's needed to reach the picture — no whole-file
/// read, hence no size cap on audio.
pub fn audio_art_from_reader<R: std::io::Read + std::io::Seek>(reader: R) -> Option<Vec<u8>> {
    audio::extract_reader(reader)
}

pub(crate) use audio::AsfTags;

/// Artist/album/title/track from an ASF/WMA file (lofty can't read ASF, so the
/// `strip::read_audio_tags` lofty path would return nothing). `None` for non-ASF
/// input → the caller falls back to lofty for every other audio format.
pub(crate) fn audio_asf_tags<R: std::io::Read + std::io::Seek>(reader: &mut R) -> Option<AsfTags> {
    audio::asf_tags(reader)
}

/// If `bytes` is a recognized ebook/comic container, return its cover image.
pub fn extract_cover(bytes: &[u8]) -> Option<CoverOut> {
    // ZIP family: EPUB / CBZ / FBZ (and any zip of images).
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") || bytes.starts_with(b"PK\x07\x08") {
        return zipfmt::extract(bytes).map(CoverOut::Bytes);
    }
    // 7-Zip: CB7.
    if bytes.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        return sevenz::extract(bytes).map(CoverOut::Bytes);
    }
    // RAR 4.x ("Rar!\x1A\x07\x00") and 5.x ("Rar!\x1A\x07\x01\x00"): CBR. Pure-Rust
    // `rars` — always available now (no feature gate).
    if bytes.starts_with(b"Rar!\x1A\x07") {
        return rar::extract(bytes).map(CoverOut::Bytes);
    }
    // DjVu (IFF85 magic "AT&TFORM").
    if bytes.starts_with(b"AT&TFORM") {
        return djvu::extract(bytes).map(CoverOut::Image);
    }
    // Photoshop PSD/PSB: the baked-in JPEG thumbnail (resource 1036). Works with
    // no ImageMagick; on None we fall through so a full install can still render
    // the layers via the magick tier.
    if bytes.starts_with(b"8BPS") {
        if let Some(thumb) = psd::extract(bytes) {
            return Some(CoverOut::Bytes(thumb));
        }
    }
    // DOS-EPS: the baked-in TIFF screen preview (real PS rendering would need
    // Ghostscript). On None (WMF-only/bare PS), fall through to the magick tier.
    if bytes.starts_with(&[0xC5, 0xD0, 0xD3, 0xC6]) {
        if let Some(tiff) = eps::extract(bytes) {
            return Some(CoverOut::Bytes(tiff));
        }
    }
    // Blender: the RGBA thumbnail baked into the TEST file-block.
    if bytes.starts_with(b"BLENDER") {
        return blend::extract(bytes).map(CoverOut::Image);
    }
    // Affinity (Photo/Designer/Publisher): an embedded PNG preview.
    if affinity::looks_like_affinity(bytes) {
        return affinity::extract(bytes).map(CoverOut::Bytes);
    }
    // Paint Shop Pro (.pspimage/.psp): carve the JPEG preview from the file's
    // Composite Image Bank (present even when the pixel data is RLE/uncompressed).
    if psp::looks_like_psp(bytes) {
        return psp::extract(bytes).map(CoverOut::Bytes);
    }
    // Amiga / Deluxe Paint IFF ILBM (and DOS PBM): real planar-bitmap decode to
    // pixels. The `ILBM`/`PBM ` FORM type keeps this off AIFF audio (`FORM…AIFF`).
    if ilbm::looks_like_ilbm(bytes) {
        return ilbm::extract(bytes).map(CoverOut::Image);
    }
    // Cinema 4D (.c4d): carve the document/scene preview JPEG from the header slot
    // (material-swatch JPEGs deeper in the file are filtered out by size/offset).
    if c4d::looks_like_c4d(bytes) {
        return c4d::extract(bytes).map(CoverOut::Bytes);
    }
    // CorelDRAW .cdr/.cdt / Corel .cmx: RIFF files with an embedded DISP preview
    // DIB. The `CDR`/`CDT`/`CMX` form keeps this off WAV/other RIFF (`RIFF…WAVE`).
    if cdr::looks_like_cdr(bytes) {
        return cdr::extract(bytes).map(CoverOut::Bytes);
    }
    // Clip Studio Paint: read the preview PNG out of the embedded SQLite db.
    if bytes.starts_with(b"CSFCHUNK") {
        return clip::extract(bytes).map(CoverOut::Bytes);
    }
    // Kindle / Mobipocket: PalmDB type+creator "BOOKMOBI" at offset 60.
    if bytes.len() > 68 && &bytes[60..68] == b"BOOKMOBI" {
        return mobi::extract(bytes).map(CoverOut::Bytes);
    }
    // TAR-based comic (CBT): "ustar" magic at offset 257.
    if bytes.len() > 262 && &bytes[257..262] == b"ustar" {
        return tarfmt::extract(bytes).map(CoverOut::Bytes);
    }
    // FictionBook 2: XML containing "<FictionBook".
    if fb2::looks_like_fb2(bytes) {
        return fb2::extract(bytes).map(CoverOut::Bytes);
    }
    // SketchUp .skp: "SketchUp Model" header → carve the embedded thumbnail PNG.
    if skp::looks_like_skp(bytes) {
        return skp::extract(bytes).map(CoverOut::Bytes);
    }
    // AutoCAD .dwg: "AC10xx" header → preview section (PNG / DIB→BMP / WMF).
    if dwg::looks_like_dwg(bytes) {
        return dwg::extract(bytes).map(CoverOut::Bytes);
    }
    // Rhino .3dm: "3D Geometry File Format" → zlib-inflated DIB preview.
    if rhino::looks_like_3dm(bytes) {
        return rhino::extract(bytes).map(CoverOut::Bytes);
    }
    // Adobe InDesign .indd: master-GUID header → base64 JPEG in the XMP packet.
    if indd::looks_like_indd(bytes) {
        return indd::extract(bytes).map(CoverOut::Bytes);
    }
    // OLE2 compound file (3ds Max .max, legacy Office/Visio/Publisher): the
    // \x05SummaryInformation thumbnail. `extract` returns a CoverOut directly
    // (raw RGB → pixels, or a CF_DIB → BMP bytes).
    if max::looks_like_max(bytes) {
        return max::extract(bytes);
    }
    // Audio with embedded album art (MP3/FLAC/Ogg/Opus/M4A/WMA/APE/…).
    if audio::looks_like_audio(bytes) {
        return audio::extract(bytes).map(CoverOut::Bytes);
    }
    // 3D-printer G-code with an embedded base64 PNG preview (text scan; bails
    // fast on binary, so it's a cheap last resort).
    if let Some(png) = gcode::extract(bytes) {
        return Some(CoverOut::Bytes(png));
    }
    None
}

/// REAL pixel dimensions of the underlying document, for container formats whose
/// extracted cover is only a small baked-in preview (PSD/PSB today). Captions /
/// info displays show these instead of the preview's dimensions — a 4700×800 PSD
/// must not read "160 × 26 px" just because its thumbnail does.
pub fn real_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"8BPS") {
        return psd::header_dims(bytes);
    }
    None
}

/// Raster-image extensions we accept as an archive cover. A curated subset of the
/// formats our decoder can read (NOT all of `formats::FORMATS` — most FORMATS
/// entries, e.g. ebook/audio/document types, are not valid cover images). Mirrors
/// DarkThumbs' IsImage set (common.cpp) — including ICO, the camera-RAW types,
/// JPEG-XR and HEIF that our WIC tier reads.
///
/// Kept as a const (not inlined in a `match`) so the set is greppable and the
/// `cover_exts_are_known_formats` test can assert it against `FORMATS`. Every
/// entry must be in `FORMATS` except the documented [`COVER_ONLY_EXCEPTIONS`].
pub(crate) const COVER_IMAGE_EXTS: &[&str] = &[
    "bmp", "ico", "gif", "jpg", "jpe", "jfif", "jpeg", "png", "tif", "tiff", "svg", "webp",
    "jxr", "nrw", "nef", "dng", "cr2", "heif", "heic", "avif", "jxl",
];

/// Cover extensions we accept that are intentionally NOT standalone `FORMATS`
/// entries: WIC can decode them as an archive cover, but we don't hook the bare
/// file type in Explorer. Keep this list as small as possible — if one of these
/// later joins `FORMATS`, the test forces it out of here. (Consumed only by the
/// drift tests, hence `allow(dead_code)` in non-test builds.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const COVER_ONLY_EXCEPTIONS: &[&str] = &[
    // (Currently empty: `jxr` graduated into FORMATS as a hooked JPEG XR format, so it
    // now satisfies the cover-set check via FORMATS directly. Any future WIC-decodable
    // cover type that we deliberately DON'T hook as a standalone format goes here.)
];

/// Does `name` (a path inside an archive) have a raster-image extension we accept
/// as a cover? See [`COVER_IMAGE_EXTS`].
pub(crate) fn is_image_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    COVER_IMAGE_EXTS.contains(&ext.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cover extension must be a real `FORMATS` entry (so we never pick an
    /// archive member we can't actually decode) — except the documented WIC-only
    /// exceptions. Catches drift when `FORMATS` gains/loses a format but this
    /// hand-maintained cover set doesn't follow (the live `jxr` divergence the
    /// 2026-06 audit found).
    #[test]
    fn cover_exts_are_known_formats() {
        for &ext in COVER_IMAGE_EXTS {
            assert!(
                crate::formats::is_known(ext) || COVER_ONLY_EXCEPTIONS.contains(&ext),
                "is_image_name accepts `{ext}`, which is neither in FORMATS nor a documented \
                 cover-only exception — add it to FORMATS or to COVER_ONLY_EXCEPTIONS",
            );
        }
    }

    /// Each exception must genuinely be (a) absent from FORMATS and (b) still in
    /// the cover set — otherwise it is stale and should be removed, keeping the
    /// exception list honest.
    #[test]
    fn cover_exceptions_are_not_stale() {
        for &ext in COVER_ONLY_EXCEPTIONS {
            assert!(
                !crate::formats::is_known(ext),
                "`{ext}` is now in FORMATS — remove it from COVER_ONLY_EXCEPTIONS",
            );
            assert!(
                COVER_IMAGE_EXTS.contains(&ext),
                "`{ext}` is no longer a cover extension — remove it from COVER_ONLY_EXCEPTIONS",
            );
        }
    }
}
