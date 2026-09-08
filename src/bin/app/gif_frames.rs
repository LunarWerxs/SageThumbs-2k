//! Shared incremental animated-frame decode loop for this EXE's two decode-bomb guards: the
//! Quick preview viewer (`preview::anim`, GIF/APNG/animated-WebP, frames kept at native size)
//! and the sponsor-banner loader (`sponsors`, GIF only, each frame resized to the banner's
//! fixed control size as it lands). Both used to carry their own copy of "cap the frame
//! count, cap the cumulative decoded-byte total, and check both INCREMENTALLY (never
//! decode-everything-then-check)" with different numbers; this hoists the loop itself so
//! there is one decoder, with the caps as parameters so each caller keeps its own budget.

use image::Frame;

/// One decoded frame at its native (pre-resize) size, plus its un-converted delay. Each
/// caller applies its own delay-to-milliseconds rule (the viewer floors every frame at
/// ~50 fps; the banner reads only the first frame's delay and applies it to the whole
/// animation), so that one-line conversion stays out of the shared loop rather than forcing
/// both callers onto the same rounding.
pub(crate) struct RawFrame {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) rgba: Vec<u8>,
    pub(crate) delay: image::Delay,
}

/// What the loop does once a frame fails to decode or the byte budget is exceeded.
pub(crate) enum OverBudget {
    /// Reject the whole animation — the caller falls back to a static image.
    RejectAll,
    /// Stop decoding and keep the frames already collected (a partial animation is fine).
    KeepPartial,
}

/// Drain `frames` under a frame-count cap (`max_frames`), a per-frame canvas cap
/// (`max_dim`), and a cumulative RGBA-byte cap (`max_total_bytes`, counted at each frame's
/// native decoded size). A frame past `max_dim` (including a degenerate 0×0) always rejects
/// the whole animation regardless of `over_budget` — that is a decode-bomb guard, not a
/// policy choice a caller can opt out of. Returns `None` if fewer than 2 frames came out
/// (not an animation).
pub(crate) fn collect_capped(
    frames: image::Frames,
    max_frames: usize,
    max_dim: u32,
    max_total_bytes: u64,
    over_budget: OverBudget,
) -> Option<Vec<RawFrame>> {
    let mut out = Vec::new();
    let mut total: u64 = 0;
    for fr in frames.take(max_frames) {
        let fr: Frame = match fr {
            Ok(fr) => fr,
            Err(_) => {
                return match over_budget {
                    OverBudget::RejectAll => None,
                    OverBudget::KeepPartial => finish(out),
                };
            }
        };
        let delay = fr.delay();
        let buf = fr.into_buffer();
        let (w, h) = (buf.width(), buf.height());
        if w == 0 || h == 0 || w > max_dim || h > max_dim {
            return None;
        }
        let frame_bytes = (w as u64)
            .checked_mul(h as u64)
            .and_then(|p| p.checked_mul(4))?;
        let new_total = total.saturating_add(frame_bytes);
        if new_total > max_total_bytes {
            return match over_budget {
                OverBudget::RejectAll => None,
                OverBudget::KeepPartial => finish(out),
            };
        }
        total = new_total;
        out.push(RawFrame {
            w,
            h,
            rgba: buf.into_raw(),
            delay,
        });
    }
    finish(out)
}

fn finish(out: Vec<RawFrame>) -> Option<Vec<RawFrame>> {
    if out.len() < 2 {
        None
    } else {
        Some(out)
    }
}

/// Reject a declared canvas that would blow the decode-time allocation before any pixels are
/// decoded — a cheap header-only probe, for a caller (the banner loader) that wants to refuse
/// a hostile LSD/IHDR-declared size up front, on top of the incremental per-frame check
/// `collect_capped` already does after each frame is decoded. `bytes` may be any
/// `image`-crate-supported format; unreadable/unrecognised input is treated as unsafe.
pub(crate) fn declared_canvas_ok(bytes: &[u8], max_dim: u32, max_alloc: u64) -> bool {
    use std::io::Cursor;
    let Ok(reader) = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format() else {
        return false;
    };
    let Ok((w, h)) = reader.into_dimensions() else {
        return false;
    };
    let canvas_bytes = (w as u64).saturating_mul(h as u64).saturating_mul(4);
    w <= max_dim && h <= max_dim && canvas_bytes <= max_alloc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32) -> image::ImageResult<Frame> {
        Ok(Frame::new(image::RgbaImage::new(w, h)))
    }

    fn frames_of(n: usize, side: u32) -> image::Frames<'static> {
        image::Frames::new(Box::new((0..n).map(move |_| frame(side, side))))
    }

    #[test]
    fn a_single_frame_is_not_an_animation() {
        assert!(
            collect_capped(frames_of(1, 4), 512, 8192, u64::MAX, OverBudget::RejectAll).is_none()
        );
    }

    #[test]
    fn an_oversized_frame_rejects_the_whole_animation_even_when_partial_is_allowed() {
        // 3 frames of a 100x100 canvas against a 50px cap: the bomb guard always wins,
        // regardless of the over-budget policy passed in.
        assert!(collect_capped(
            frames_of(3, 100),
            512,
            50,
            u64::MAX,
            OverBudget::KeepPartial
        )
        .is_none());
    }

    #[test]
    fn reject_all_drops_everything_once_the_byte_cap_is_exceeded() {
        // 16 KiB/frame; a 48 KiB cap allows exactly 3.
        let side = 64; // 64*64*4 = 16384 bytes
        assert!(collect_capped(
            frames_of(4, side),
            512,
            8192,
            3 * 16384,
            OverBudget::RejectAll
        )
        .is_none());
    }

    #[test]
    fn keep_partial_returns_what_was_collected_before_the_cap() {
        let side = 64; // 16 KiB/frame
        let out = collect_capped(
            frames_of(4, side),
            512,
            8192,
            3 * 16384,
            OverBudget::KeepPartial,
        )
        .expect("3 frames fit under the cap before the 4th trips it");
        assert_eq!(out.len(), 3);
    }

    /// The two production caller shapes (the viewer's 512 frames / 512 MiB and the banner's
    /// smaller 256 frames / 256 MiB) run through this ONE function. Scaled down 8192x for a
    /// fast, low-memory test, but the ratio (banner cap = half the viewer's) is preserved:
    /// each cap set refuses a stream that exceeds ITS OWN byte budget, via the identical
    /// shared code path.
    #[test]
    fn both_caller_cap_sets_refuse_past_their_own_byte_cap_via_the_same_function() {
        const VIEWER_LIKE_BYTES: u64 = 512 * 1024; // stand-in for 512 MiB
        const BANNER_LIKE_BYTES: u64 = 256 * 1024; // stand-in for 256 MiB
        let side = 64; // 16 KiB/frame

        // Exactly at each cap: fits.
        assert!(collect_capped(
            frames_of(32, side),
            512,
            8192,
            VIEWER_LIKE_BYTES,
            OverBudget::RejectAll
        )
        .is_some());
        assert!(collect_capped(
            frames_of(16, side),
            256,
            8192,
            BANNER_LIKE_BYTES,
            OverBudget::RejectAll
        )
        .is_some());

        // One frame past each cap: both refuse, through the same collect_capped call.
        assert!(collect_capped(
            frames_of(33, side),
            512,
            8192,
            VIEWER_LIKE_BYTES,
            OverBudget::RejectAll
        )
        .is_none());
        assert!(collect_capped(
            frames_of(17, side),
            256,
            8192,
            BANNER_LIKE_BYTES,
            OverBudget::RejectAll
        )
        .is_none());
    }

    #[test]
    fn declared_canvas_ok_rejects_garbage_and_respects_both_caps() {
        assert!(!declared_canvas_ok(b"not an image", 8192, u64::MAX));

        // Encode a real 4x4 RGBA PNG (rather than a hand-typed byte literal) so the probe
        // exercises a genuine header parse.
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(4, 4))
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .expect("encoding a tiny PNG for the test");

        assert!(declared_canvas_ok(&png_bytes, 8192, 512 * 1024 * 1024));
        assert!(!declared_canvas_ok(&png_bytes, 2, 512 * 1024 * 1024)); // 4 > 2px cap
        assert!(!declared_canvas_ok(&png_bytes, 8192, 10)); // 4*4*4=64 bytes > 10-byte cap
    }
}
