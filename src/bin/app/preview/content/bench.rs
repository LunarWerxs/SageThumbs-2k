//! The --bench-preview entry points.

use super::*;

/// `--bench-preview` hook: decode `path` the way a COLD arrow-key step does — full read plus
/// full decode, no cache — then populate the cache exactly as `spawn_decode` does. Returns the
/// decoded size so the caller can tell a real decode from a miss.
pub(in super::super) fn bench_decode_uncached(path: &str) -> Option<(i32, i32)> {
    let d = read_and_decode(path)?;
    let dims = (d.w, d.h);
    cache_put(path, std::sync::Arc::new(d));
    Some(dims)
}

/// `--bench-preview` hook: the WARM path — what `spawn_decode` does on a revisit. Deliberately
/// goes through `cache_get`, copy included, so the number is the real cost of a cache hit and
/// not an idealised pointer lookup.
pub(in super::super) fn bench_decode_cached(path: &str) -> Option<(i32, i32)> {
    cache_get(path).map(|d| (d.w, d.h))
}

/// `--bench-preview` hook: what the codec-scaled decode costs, against the full decode in the
/// `cold` column.
///
/// This is the number that decides whether the full decode can become LAZY (issue 4/5): if
/// asking the codec for a display-sized picture is a fraction of decoding full size, then an
/// arrow step never needs the full one until the user zooms. If it is not, there is nothing
/// to win and the idea dies here. `None` for anything the fast path declines — a small source,
/// or a format the OS codecs will not open.
pub(in super::super) fn bench_scaled_decode(path: &str) -> Option<u128> {
    let t = std::time::Instant::now();
    let d = display_scaled_first_paint(path)?;
    let us = t.elapsed().as_micros();
    let _ = d;
    Some(us)
}

/// `--bench-preview` hook: the DISPLAY cost — turning decoded pixels into the premultiplied DIB
/// the window blits. Measured separately because on a cache hit it is the ONLY work left, and
/// the end-to-end arrow bench says a prefetched 12 MP photo still costs ~100 ms per step. If
/// that time is here, no decoder change can help it.
pub(in super::super) fn bench_make_render(path: &str) -> Option<u128> {
    let d = cache_get(path)?;
    let t = std::time::Instant::now();
    let rd = unsafe { make_render(d.w, d.h, &d.rgba, 0x0020_2020) };
    let us = t.elapsed().as_micros();
    drop(rd); // frees the HBITMAP; leaking one per benched file would skew later steps
    Some(us)
}

/// Synchronous decode for the headless `--shot` path (off the UI hot path, no worker).
///
/// Resolves the sharp composite inline: a still capture gets no second paint, so the upgrade
/// the live viewer receives asynchronously has to happen here for the shot to show it.
pub(in super::super) fn decode_sync(path: &str) -> Option<DecodedRgba> {
    let first = read_and_decode(path);
    let shown = first.as_ref().map_or((0, 0), |d| (d.w, d.h));
    if let Ok(head) = sagethumbs2k_core::decode::read_preview_capped(path) {
        // As `spawn_decode` does: a Photoshop document with no baked preview is chased from
        // nothing rather than left on the card (issue #46).
        if first.is_some() || head.starts_with(b"8BPS") {
            if let Some(sharp) = sharper_composite(path, &head, shown) {
                return Some(sharp);
            }
        }
    }
    first
}

/// Convert a decoded image to tight RGBA8 at full resolution: the pixels ARE the image.
pub(super) fn rgba8_full(img: image::DynamicImage) -> DecodedRgba {
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    DecodedRgba::full(w, h, rgba.into_raw())
}
