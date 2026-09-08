//! TH44 thumbnail generation for multi-page DjVu bundles.
//!
//! Encodes a page image (color [`Pixmap`] or bilevel [`Bitmap`]) as a small
//! IW44-encoded grayscale or color thumbnail, returning the raw TH44 chunk
//! payloads (one `Vec<u8>` per IW44 sub-chunk).
//!
//! # Placement convention
//!
//! Thumbnails are embedded as `TH44` chunks **inside the page's own
//! `FORM:DJVU`** component — one or more consecutive `TH44` sub-chunks form
//! a single IW44 stream, just like BG44 chunks. This is the layout the
//! per-page `thumbnail()` reader in `djvu_document.rs` expects: it collects
//! all `TH44` chunks from `self.chunks` and feeds them to the IW44 decoder.
//!
//! The alternative — a separate `FORM:THUM` component in the DIRM — is an
//! older convention used by some DjVuLibre tools but is not required by the
//! spec and is not what this repo's reader looks for. Per-page TH44 is
//! simpler, widely supported, and is what the reader already handles.
//!
//! # Thumbnail size
//!
//! The long side is scaled to at most [`THUMBNAIL_MAX_SIDE`] pixels (default
//! 128), with the short side scaled proportionally.  A minimum of 1×1 is
//! always returned (even for degenerate inputs).

use crate::iw44_encode::{Iw44EncodeOptions, encode_iw44_color, encode_iw44_gray};
use crate::pixmap::{GrayPixmap, Pixmap};

/// Maximum side length (pixels) of an embedded TH44 thumbnail.
pub const THUMBNAIL_MAX_SIDE: u32 = 128;

/// Encode a color [`Pixmap`] as TH44 thumbnail payloads.
///
/// The image is first Lanczos-3 downscaled so that its long side is at most
/// [`THUMBNAIL_MAX_SIDE`] pixels, then IW44-encoded (color, single chunk).
/// Returns the raw chunk payload bytes (to be wrapped in `TH44` IFF chunks
/// by the caller).
pub fn encode_th44_color(src: &Pixmap) -> Vec<Vec<u8>> {
    let (tw, th) = thumbnail_dimensions(src.width, src.height);
    let thumb = crate::pixmap::scale_lanczos3(src, tw, th);
    let opts = Iw44EncodeOptions {
        // One chunk is enough for a small thumbnail; the default 10×10-slice
        // chunking would produce identical quality but is overkill here.
        slices_per_chunk: 100,
        total_slices: 100,
        chroma_delay: 0,
        chroma_half: false,
        ..Iw44EncodeOptions::default()
    };
    encode_iw44_color(&thumb, &opts)
}

/// Encode a bilevel [`crate::Bitmap`] as TH44 thumbnail payloads.
///
/// The bitmap is first converted to a grayscale image (black → 0, white →
/// 255), downscaled to thumbnail size, then IW44-encoded as grayscale.
pub fn encode_th44_gray_from_bitmap(src: &crate::Bitmap) -> Vec<Vec<u8>> {
    // Convert bilevel bitmap to GrayPixmap: black pixel → 0, white pixel → 255.
    let w = src.width;
    let h = src.height;
    let mut data = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            data.push(if src.get(x, y) { 0u8 } else { 255u8 });
        }
    }
    let gray = GrayPixmap {
        width: w,
        height: h,
        data,
    };
    let (tw, th) = thumbnail_dimensions(w, h);
    let scaled = scale_gray_bilinear(&gray, tw, th);
    let opts = Iw44EncodeOptions {
        slices_per_chunk: 100,
        total_slices: 100,
        chroma_delay: 0,
        chroma_half: false,
        ..Iw44EncodeOptions::default()
    };
    encode_iw44_gray(&scaled, &opts)
}

/// Compute the thumbnail dimensions for an image of size `(w, h)` so the
/// long side is at most [`THUMBNAIL_MAX_SIDE`].  Returns `(1, 1)` for
/// degenerate zero-dimension inputs.
pub(crate) fn thumbnail_dimensions(w: u32, h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    let max = THUMBNAIL_MAX_SIDE;
    if w <= max && h <= max {
        return (w.max(1), h.max(1));
    }
    if w >= h {
        let tw = max;
        let th = ((h as u64 * max as u64) / w as u64).max(1) as u32;
        (tw, th)
    } else {
        let th = max;
        let tw = ((w as u64 * max as u64) / h as u64).max(1) as u32;
        (tw, th)
    }
}

/// Simple bilinear downscaler for [`GrayPixmap`] (no external dependency).
///
/// For thumbnails, bilinear is sufficient and avoids pulling in the Lanczos
/// floating-point code which operates on [`Pixmap`] (RGBA) only.  When
/// `dst_w == src.width && dst_h == src.height` the source is cloned.
pub(crate) fn scale_gray_bilinear(src: &GrayPixmap, dst_w: u32, dst_h: u32) -> GrayPixmap {
    let sw = src.width;
    let sh = src.height;
    if sw == 0 || sh == 0 || dst_w == 0 || dst_h == 0 {
        return GrayPixmap {
            width: dst_w.max(1),
            height: dst_h.max(1),
            data: vec![255u8; (dst_w.max(1) * dst_h.max(1)) as usize],
        };
    }
    if sw == dst_w && sh == dst_h {
        return src.clone();
    }
    let mut data = Vec::with_capacity((dst_w * dst_h) as usize);
    for oy in 0..dst_h {
        // Map output row centre to source space.
        let sy_f = (oy as f32 + 0.5) * (sh as f32 / dst_h as f32) - 0.5;
        let sy0 = (sy_f.floor() as i64).clamp(0, (sh - 1) as i64) as u32;
        let sy1 = (sy0 + 1).min(sh - 1);
        let dy = (sy_f - sy_f.floor()).clamp(0.0, 1.0);
        for ox in 0..dst_w {
            let sx_f = (ox as f32 + 0.5) * (sw as f32 / dst_w as f32) - 0.5;
            let sx0 = (sx_f.floor() as i64).clamp(0, (sw - 1) as i64) as u32;
            let sx1 = (sx0 + 1).min(sw - 1);
            let dx = (sx_f - sx_f.floor()).clamp(0.0, 1.0);
            let p00 = src.get(sx0, sy0) as f32;
            let p10 = src.get(sx1, sy0) as f32;
            let p01 = src.get(sx0, sy1) as f32;
            let p11 = src.get(sx1, sy1) as f32;
            let val = p00 * (1.0 - dx) * (1.0 - dy)
                + p10 * dx * (1.0 - dy)
                + p01 * (1.0 - dx) * dy
                + p11 * dx * dy;
            data.push(val.round().clamp(0.0, 255.0) as u8);
        }
    }
    GrayPixmap {
        width: dst_w,
        height: dst_h,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_dimensions_long_side_capped() {
        // Landscape 800×600 → 128×96
        let (w, h) = thumbnail_dimensions(800, 600);
        assert_eq!(w, 128);
        assert_eq!(h, 96);
    }

    #[test]
    fn thumbnail_dimensions_portrait() {
        // Portrait 600×800 → 96×128
        let (w, h) = thumbnail_dimensions(600, 800);
        assert_eq!(w, 96);
        assert_eq!(h, 128);
    }

    #[test]
    fn thumbnail_dimensions_small_passthrough() {
        // Already small → unchanged
        let (w, h) = thumbnail_dimensions(64, 48);
        assert_eq!(w, 64);
        assert_eq!(h, 48);
    }

    #[test]
    fn thumbnail_dimensions_zero() {
        assert_eq!(thumbnail_dimensions(0, 100), (1, 1));
        assert_eq!(thumbnail_dimensions(100, 0), (1, 1));
    }

    #[test]
    fn scale_gray_bilinear_size() {
        let src = GrayPixmap {
            width: 64,
            height: 48,
            data: vec![128u8; 64 * 48],
        };
        let dst = scale_gray_bilinear(&src, 32, 24);
        assert_eq!(dst.width, 32);
        assert_eq!(dst.height, 24);
        assert_eq!(dst.data.len(), 32 * 24);
    }

    #[test]
    fn encode_th44_color_round_trips_dimensions() {
        use crate::iw44::Iw44Image;
        let src = Pixmap::white(200, 150);
        let chunks = encode_th44_color(&src);
        assert!(!chunks.is_empty());
        let mut img = Iw44Image::new();
        for c in &chunks {
            img.decode_chunk(c).expect("decode TH44 chunk");
        }
        let decoded = img.to_rgb().expect("to_rgb");
        let (tw, th) = thumbnail_dimensions(200, 150);
        assert_eq!(
            decoded.width, tw,
            "decoded width matches thumbnail_dimensions"
        );
        assert_eq!(
            decoded.height, th,
            "decoded height matches thumbnail_dimensions"
        );
    }

    #[test]
    fn encode_th44_gray_from_bitmap_round_trips_dimensions() {
        use crate::Bitmap;
        use crate::iw44::Iw44Image;
        let mut bm = Bitmap::new(200, 300);
        // Fill some pixels
        for x in 0..50 {
            for y in 0..50 {
                bm.set(x, y, true);
            }
        }
        let chunks = encode_th44_gray_from_bitmap(&bm);
        assert!(!chunks.is_empty());
        let mut img = Iw44Image::new();
        for c in &chunks {
            img.decode_chunk(c).expect("decode TH44 gray chunk");
        }
        let decoded = img.to_rgb().expect("to_rgb");
        let (tw, th) = thumbnail_dimensions(200, 300);
        assert_eq!(decoded.width, tw);
        assert_eq!(decoded.height, th);
    }
}
