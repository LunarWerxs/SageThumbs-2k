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
mod clip;
mod djvu;
// DjVu's decode stack: `zp` (arithmetic coder) feeds `iw44` (wavelet background)
// and `jb2` (bilevel text mask); `djvu` composites them.
mod zp;
mod iw44;
mod jb2;
mod eps;
mod epub;
mod gcode;
mod fb2;
mod mobi;
mod office;
mod project;
mod psd;
mod select;
mod sevenz;
mod tarfmt;
mod zipfmt;
#[cfg(feature = "rar")]
mod rar;

/// A cover: either raw image-file bytes (re-decoded by the image tiers) or
/// already-decoded pixels (DjVu, which is not a standalone image file).
pub enum CoverOut {
    Bytes(Vec<u8>),
    Image(DynamicImage),
}

/// Max bytes we'll read for one cover entry (DarkThumbs' CBXMEM cap, 32 MiB).
pub(crate) const MAX_COVER: u64 = 32 * 1024 * 1024;

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
    // RAR 4.x ("Rar!\x1A\x07\x00") and 5.x ("Rar!\x1A\x07\x01\x00"): CBR.
    #[cfg(feature = "rar")]
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

/// Does `name` (a path inside an archive) have a raster-image extension we accept
/// as a cover? Mirrors DarkThumbs' IsImage set (common.cpp) exactly — including
/// ICO, the camera-RAW types, JPEG-XR and HEIF that our decoder can still read.
pub(crate) fn is_image_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "bmp" | "ico" | "gif" | "jpg" | "jpe" | "jfif" | "jpeg" | "png" | "tif" | "tiff"
            | "svg" | "webp" | "jxr" | "nrw" | "nef" | "dng" | "cr2" | "heif" | "heic"
            | "avif" | "jxl"
    )
}
