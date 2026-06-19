//! DjVu (.djvu) cover extraction via the maintained pure-Rust `djvu-rs` crate.
//!
//! Replaced the hand-rolled `zp`/`iw44`/`jb2` decode stack (2026-06-14): djvu-rs is
//! MIT, C-free, and fuzzed, and handles multipage shared-dictionary (`INCL`→`Djbz`)
//! pages our hand-roll degraded to background-only. We decode page 1 to RGBA and
//! return it as a `DynamicImage` (the contract the rest of `container` expects).
//!
//! Uses the high-level (`std`) API on purpose: the crate's render pipeline requires
//! `std` at runtime — a `default-features=false` no_std build compiles but silently
//! fails to decode (see the Cargo.toml note). Runs in Explorer's thumbnail host under
//! `panic = "abort"`; djvu-rs is fuzzed, but we still bomb-guard + cap before allocating.

use image::DynamicImage;

/// The decode pipeline's single bomb-guard ceiling.
const MAX_DIM: u32 = crate::decode::limits::MAX_DIM;
/// Cap the rendered long edge: a full-res scan can be 5000×6600 (~130 MB RGBA),
/// pointless for a thumbnail the caller fit-to-box downscales anyway.
const RENDER_CAP: u32 = 1600;

pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    let doc = djvu_rs::Document::from_bytes(bytes.to_vec()).ok()?;
    let page = doc.page(0).ok()?;

    // Prefer the encoder's baked page thumbnail (TH44) when present — fast + tiny.
    // Otherwise render page 1, capping the long edge so a huge scan doesn't allocate
    // a ~130 MB buffer for a thumbnail.
    let pm = match page.thumbnail() {
        Ok(Some(thumb)) => thumb,
        _ => {
            let (dw, dh) = (page.display_width().max(1), page.display_height().max(1));
            let long = dw.max(dh);
            if long > RENDER_CAP {
                let s = RENDER_CAP as f32 / long as f32;
                let w = ((dw as f32 * s).round() as u32).max(1);
                let h = ((dh as f32 * s).round() as u32).max(1);
                page.render_to_size(w, h).ok()?
            } else {
                page.render().ok()?
            }
        }
    };

    if pm.width == 0 || pm.height == 0 || pm.width > MAX_DIM || pm.height > MAX_DIM {
        return None;
    }
    // pm.data is straight RGBA8 (4 B/px), top row first — same as our other decoders.
    image::RgbaImage::from_raw(pm.width, pm.height, pm.data).map(DynamicImage::ImageRgba8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_djvu() {
        assert!(extract(b"not a djvu file at all").is_none());
        assert!(extract(&[]).is_none());
    }
}
