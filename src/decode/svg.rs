//! The SVG tier (resvg) plus the gzip unwrapping `.svgz`/`.emz` need.
//!
//! Separate from the raster tiers because it is a RENDERER, not a decoder: it has its
//! own size floor/ceiling and its own wall-clock budget, since resvg runs in-process
//! with no child to kill.

use super::*;

/// The SVG/EMF gzip inflate cap this module's own callers use: an SVG/EMF that large is
/// already pathological for a thumbnail, and it bounds a hostile highly-compressible
/// payload. [`strip`](crate::strip)'s copy of [`gunzip_bounded`] passes its own, larger
/// cap instead (C5) — see that function's doc comment.
pub(crate) const GUNZIP_MAX: u64 = 64 * 1024 * 1024;

/// Inflate a gzip stream with a hard output cap `cap` (decompression-bomb guard),
/// shared by this module's `.svgz`/`.emz` callers (which pass [`GUNZIP_MAX`]) and by
/// [`strip::gunzip_bounded`](crate::strip) (C5), which passes its own cap rather than
/// duplicating this read loop. `flate2` (rust_backend / miniz_oxide) is already in the
/// tree for `zip`, so this adds no dependency and stays pure-Rust. Returns `None` on any
/// inflate error or empty output; a truncated-at-cap inflate just fails to parse
/// downstream and falls back to the default icon.
pub(crate) fn gunzip_bounded(bytes: &[u8], cap: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(cap)
        .read_to_end(&mut out)
        .ok()?;
    (!out.is_empty()).then_some(out)
}

/// Cap the SVG raster size; a vector at ≤2048px is ample for a thumbnail or a
/// reasonable convert, and bounds memory for SVGs that declare huge dimensions.
pub(super) const SVG_MAX_DIM: f32 = 2048.0;

/// Floor the SVG raster size: small-viewBox SVGs (24px/48px icons, logos) would otherwise
/// rasterize at their tiny intrinsic size, so a right-click "Convert into PNG" produced a
/// 24×24 image. A vector has no native resolution, so rendering it UP to this longest-edge
/// minimum is free (crisp, no interpolation) and gives a usable convert — and crisper
/// thumbnails, since the provider downscales a 512px render instead of upscaling a 24px one.
pub(super) const SVG_MIN_DIM: f32 = 512.0;

/// Hard wall-clock cap on a single SVG parse+render. resvg runs in-process (no
/// child to kill), so a pathological/hostile SVG — deeply nested groups, huge
/// filter chains — could otherwise spin a thumbnail-host thread indefinitely.
pub(super) const SVG_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    head.windows(4).any(|w| w.eq_ignore_ascii_case(b"<svg"))
}

/// Does the SVG define CSS keyframe animations? Cheap case-insensitive `@keyframes` scan of the
/// first 64 KB (SVGs are small; the `<style>` block is near the top). Used to enable the
/// reduced-motion render fallback in [`render_svg`] ONLY for animated SVGs.
pub(super) fn has_css_animation(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(64 * 1024)];
    head.windows(10)
        .any(|w| w.eq_ignore_ascii_case(b"@keyframes"))
}

/// Rasterize an SVG to straight (non-premultiplied) RGBA via resvg/tiny-skia.
///
/// Parse+render run on a dedicated worker thread joined with a deadline
/// ([`SVG_TIMEOUT`]), mirroring `pdf.rs`: resvg has no internal timeout and runs
/// in-process inside Explorer's thumbnail host, so an unbounded run is a DoS
/// vector. On timeout we return E_FAIL and let the worker finish on its own — a
/// leaked thread in a disposable host is acceptable (same trade-off as pdf.rs).
pub(super) fn decode_svg(bytes: &[u8]) -> Result<DynamicImage> {
    let owned = bytes.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Pin the DLL for this detached worker's lifetime — on timeout it outlives this call
        // and `DllCanUnloadNow` ignores it, so the in-process thumbnail/preview host could
        // unload the DLL mid-render and crash. Mirrors run_action_detached.
        #[allow(clippy::default_constructed_unit_structs)]
        let _module = crate::ModuleRef::default();
        let _ = tx.send(render_svg(&owned));
    });
    match rx.recv_timeout(SVG_TIMEOUT) {
        Ok(r) => r,
        Err(_) => {
            crate::safety::log_debug("SVG render exceeded the wall-clock deadline");
            Err(Error::from(E_FAIL))
        }
    }
}

/// The actual resvg parse + render, run on the worker thread above.
pub(super) fn render_svg(bytes: &[u8]) -> Result<DynamicImage> {
    use resvg::{tiny_skia, usvg};

    let mut opt = usvg::Options::default();
    // SECURITY: usvg's default `<image href>` resolver opens any absolute or UNC path with
    // `std::fs::read` (usvg 0.48's `ImageHrefResolver::default_string_resolver` -> `get_abs_path`,
    // which with `resources_dir: None` returns the href verbatim). Reachable in-process inside
    // explorer.exe via `decode_menu_preview`, so a ~300-byte SVG in a browsed folder could make
    // the shell read an attacker-named file (a UNC href is an outbound SMB connect and NetNTLMv2
    // leak) and bypass `limits::MAX_INPUT_BYTES` entirely. resvg is built without `raster-images`
    // anyway, so the loaded bytes are never rendered — the read buys nothing. Refuse every
    // external href; only inline `data:` URIs resolve.
    opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
    // CSS-animated SVGs (`@keyframes`) commonly HIDE their content at rest (`opacity:0` on the
    // shapes) and REVEAL it through the animation. resvg is a STATIC rasterizer — it never runs
    // CSS animations — so it renders that hidden initial state and we get a blank image. Browsers
    // (and QuickLook, which renders SVG in one) show the animation; such SVGs also ship a
    // `@media (prefers-reduced-motion: reduce)` fallback for non-animating contexts. Mirror that
    // reduced-motion intent: disable animations and force the resting/visible state. GATED on the
    // presence of `@keyframes`, so ordinary static SVGs (which may use legitimate partial opacity)
    // are left exactly as before. Fixes the blank render on every surface (thumbnail, preview
    // pane, and the Quick preview viewer).
    if has_css_animation(bytes) {
        opt.style_sheet = Some("*{animation:none!important;opacity:1!important}".to_string());
    }
    // Keep the usvg cause: "this looked like SVG but won't parse" is the single
    // most common SVG triage question, and a bare E_FAIL discards the reason.
    let tree = usvg::Tree::from_data(bytes, &opt).map_err(|e| {
        crate::safety::log_debug(&format!("SVG parse failed: {e:?}"));
        Error::from(E_FAIL)
    })?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    // reject non-positive or NaN sizes (equivalent to the prior `!(longest > 0.0)` guard).
    if longest <= 0.0 || longest.is_nan() {
        return Err(Error::from(E_FAIL));
    }
    let scale = if longest > SVG_MAX_DIM {
        SVG_MAX_DIM / longest // clamp huge declared sizes down
    } else if longest < SVG_MIN_DIM {
        SVG_MIN_DIM / longest // render small icons/logos UP to a usable size (vector = crisp)
    } else {
        1.0
    };
    let w = (size.width() * scale).ceil().max(1.0) as u32;
    let h = (size.height() * scale).ceil().max(1.0) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or_else(|| Error::from(E_FAIL))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny-skia pixels are premultiplied RGBA; un-premultiply so they flow
    // through the same straight-RGBA path as every other decoder.
    let mut buf = pixmap.data().to_vec();
    for px in buf.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            let un = |c: u8| (((c as u32) * 255 + a / 2) / a).min(255) as u8;
            px[0] = un(px[0]);
            px[1] = un(px[1]);
            px[2] = un(px[2]);
        }
    }
    let img = image::RgbaImage::from_raw(w, h, buf).ok_or_else(|| Error::from(E_FAIL))?;
    Ok(DynamicImage::ImageRgba8(img))
}
