//! Animated GIF / APNG / animated-WebP frame extraction for the viewer's image path. Uses the
//! `image` crate's `AnimationDecoder` (the `gif`/`png`/`webp` features are already enabled), so
//! ZERO new dependencies. Bin-only (never linked into the DLL). Frame compositing (GIF disposal,
//! APNG blend) is handled inside the decoders, so each frame is a full pre-composited RGBA image.

use std::io::Cursor;
use std::time::Duration;

use image::AnimationDecoder;

use super::content::DecodedRgba;

/// Hard caps (a mischievous file can't stall the decode budget or blow memory). Enforced
/// INCREMENTALLY while decoding (never decode-everything-then-check): the frame count is capped
/// by `take`, and the cumulative RGBA byte budget is checked per frame so a many-huge-frame bomb
/// bails before it can exhaust memory (`panic=abort` would kill the viewer on a failed alloc).
const MAX_FRAMES: usize = 512;
const MAX_DIM: u32 = 8192;
const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024; // 512 MiB of frames, matches decode.rs's MAX_ALLOC

/// Decode an animated GIF/APNG/animated-WebP to `(rgba frame, delay ms)` pairs. Returns `None`
/// for non-animated / single-frame / unsupported / over-budget input, so the caller falls back
/// to the normal single-frame static path.
pub(super) fn decode_animation(bytes: &[u8], ext: &str) -> Option<Vec<(DecodedRgba, u32)>> {
    match ext {
        "gif" => {
            let d = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).ok()?;
            collect_capped(d.into_frames())
        }
        "png" | "apng" => {
            let d = image::codecs::png::PngDecoder::new(Cursor::new(bytes)).ok()?;
            if !d.is_apng().ok()? {
                return None; // ordinary single-frame PNG -> static path
            }
            collect_capped(d.apng().ok()?.into_frames())
        }
        "webp" => {
            let d = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
            if !d.has_animation() {
                return None; // still WebP -> static path
            }
            collect_capped(d.into_frames())
        }
        _ => None,
    }
}

/// Lazily drain a frame iterator under the caps above. A >512-frame animation plays its first
/// 512 frames; a per-frame or cumulative size violation rejects the whole animation (static
/// fallback) rather than risking the allocator.
fn collect_capped(frames: image::Frames) -> Option<Vec<(DecodedRgba, u32)>> {
    let mut out = Vec::new();
    let mut total: usize = 0;
    for fr in frames.take(MAX_FRAMES) {
        let fr = fr.ok()?;
        let ms = (Duration::from(fr.delay()).as_millis() as u32).max(20); // floor at ~50 fps
        let buf = fr.into_buffer();
        let (w, h) = (buf.width(), buf.height());
        if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
            return None;
        }
        total = total.checked_add((w as usize).checked_mul(h as usize)?.checked_mul(4)?)?;
        if total > MAX_TOTAL_BYTES {
            return None;
        }
        out.push((DecodedRgba { w: w as i32, h: h as i32, rgba: buf.into_raw() }, ms));
    }
    if out.len() < 2 {
        None
    } else {
        Some(out)
    }
}
