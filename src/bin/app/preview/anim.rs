//! Animated GIF / APNG / animated-WebP frame extraction for the viewer's image path. Uses the
//! `image` crate's `AnimationDecoder` (the `gif`/`png`/`webp` features are already enabled), so
//! ZERO new dependencies. Bin-only (never linked into the DLL). Frame compositing (GIF disposal,
//! APNG blend) is handled inside the decoders, so each frame is a full pre-composited RGBA image.

use std::io::Cursor;
use std::time::Duration;

use image::AnimationDecoder;

use super::content::DecodedRgba;
use crate::gif_frames::{collect_capped, OverBudget};

/// Hard caps (a mischievous file can't stall the decode budget or blow memory). Enforced
/// INCREMENTALLY while decoding, by the shared `gif_frames::collect_capped` loop (never
/// decode-everything-then-check): the frame count is capped by `take`, and the cumulative
/// RGBA byte budget is checked per frame so a many-huge-frame bomb bails before it can
/// exhaust memory (`panic=abort` would kill the viewer on a failed alloc).
const MAX_FRAMES: usize = 512;
const MAX_DIM: u32 = 8192;
/// The decode pipeline's own allocation ceiling, so the viewer's animation budget can never
/// drift from the thumbnail budget.
const MAX_TOTAL_BYTES: u64 = sagethumbs2k_core::decode::limits::MAX_ALLOC;

/// Decode an animated GIF/APNG/animated-WebP to `(rgba frame, delay ms)` pairs. Returns `None`
/// for non-animated / single-frame / unsupported / over-budget input, so the caller falls back
/// to the normal single-frame static path.
pub(super) fn decode_animation(bytes: &[u8], ext: &str) -> Option<Vec<(DecodedRgba, u32)>> {
    match ext {
        "gif" => {
            let d = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).ok()?;
            to_decoded(d.into_frames())
        }
        "png" | "apng" => {
            let d = image::codecs::png::PngDecoder::new(Cursor::new(bytes)).ok()?;
            if !d.is_apng().ok()? {
                return None; // ordinary single-frame PNG -> static path
            }
            to_decoded(d.apng().ok()?.into_frames())
        }
        "webp" => {
            let d = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
            if !d.has_animation() {
                return None; // still WebP -> static path
            }
            to_decoded(d.into_frames())
        }
        _ => None,
    }
}

/// Run the shared, capped decode loop (`gif_frames::collect_capped`) and convert its raw
/// frames to this viewer's `DecodedRgba` + a per-frame delay in ms (floored at ~50 fps). A
/// 512-plus-frame animation plays its first 512 frames; a per-frame or cumulative size
/// violation rejects the whole animation (static fallback) rather than risking the
/// allocator — same caps and same early-stop semantics as before the loop was hoisted.
fn to_decoded(frames: image::Frames) -> Option<Vec<(DecodedRgba, u32)>> {
    let raw = collect_capped(
        frames,
        MAX_FRAMES,
        MAX_DIM,
        MAX_TOTAL_BYTES,
        OverBudget::RejectAll,
    )?;
    Some(
        raw.into_iter()
            .map(|f| {
                let ms = (Duration::from(f.delay).as_millis() as u32).max(20); // floor at ~50 fps
                (DecodedRgba::full(f.w as i32, f.h as i32, f.rgba), ms)
            })
            .collect(),
    )
}
