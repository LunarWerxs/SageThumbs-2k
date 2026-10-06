//! Byte-signature sniffers: which container or raster family a buffer's first bytes name.

use super::*;

/// Cheap magic test for the above, so a caller can route before it commits to a read.
pub(crate) fn looks_like_xcf(bytes: &[u8]) -> bool {
    xcf::looks_like_xcf(bytes)
}

/// Cheap magic test for the above (IFF85 "AT&TFORM"), so a caller can route before it commits.
pub(crate) fn looks_like_djvu(bytes: &[u8]) -> bool {
    bytes.starts_with(b"AT&TFORM")
}

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
        // Windows metafiles (EMF / placeable + memory WMF) — decodable via the magick
        // tier (e.g. Visio docProps/thumbnail.emf). Shares decode::looks_like_metafile
        // so the magic bytes live in exactly one place.
        || crate::decode::looks_like_metafile(data)
}

/// ZIP-family signature (local-file / central-dir / end-of-central-dir headers).
pub(super) fn is_zip(b: &[u8]) -> bool {
    b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06") || b.starts_with(b"PK\x07\x08")
}

/// 7-Zip signature.
pub(crate) fn is_7z(b: &[u8]) -> bool {
    b.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C])
}

/// A macOS AppleDouble companion: the `._name` file a Mac writes beside `name` on any volume
/// that cannot hold its Finder info and resource fork (a FAT/exFAT stick, a network share, a
/// zip made on a Mac). It carries the original's NAME, so a `._art.psd` registers as a
/// Photoshop file, but it is a few hundred bytes of metadata and never a picture. Magic
/// `00 05 16 07`, then version 1 or 2 (RFC 1740). AppleSingle (`00 05 16 00`), which carries
/// the data fork too, is not this.
pub(crate) fn is_apple_double(b: &[u8]) -> bool {
    b.starts_with(&[0x00, 0x05, 0x16, 0x07]) && matches!(b.get(4..8), Some([0, 1 | 2, 0, 0]))
}

/// RAR signature (RAR 1.5–4.x `Rar!\x1a\x07\x00` and RAR5 `Rar!\x1a\x07\x01\x00` share this prefix).
pub(super) fn is_rar(b: &[u8]) -> bool {
    b.starts_with(b"Rar!\x1a\x07")
}

/// Does `head` (the first bytes of a file) look like an audio container that may
/// carry embedded cover art? Lets the thumbnail provider take the memory-light
/// seek path instead of reading the whole (possibly huge) file.
pub fn looks_like_audio(head: &[u8]) -> bool {
    audio::looks_like_audio(head)
}

/// Is `head` the signature of a generic archive we thumbnail (.zip / .7z / .rar)?
/// The streamsrc archive branch uses this to decide the probe is worth a
/// `Stat`-name check at all. RAR is included here even though it can't stream —
/// its caller takes the bounded in-memory path instead.
pub fn is_generic_archive_magic(head: &[u8]) -> bool {
    is_zip(head) || is_7z(head) || is_rar(head)
}
