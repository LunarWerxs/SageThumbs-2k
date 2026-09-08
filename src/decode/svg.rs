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
/// tree for `zip`, so this adds no dependency and stays pure-Rust.
///
/// Routed through [`read_bounded`] (2026-09-05 audit, F15): this used to `.take(cap)` (no
/// `+1`) and hand back whatever inflated, so an SVG/EMZ whose real decompressed size was
/// PAST `cap` silently got treated as a complete document consisting of its first `cap`
/// bytes, instead of being refused. `decode_svg_if_svg`'s caller already treats `None` as
/// "fall through to the raster tiers" (same as any other decode failure), so refusing
/// outright here changes nothing about the fallback path, only what triggers it. Returns
/// `None` on any inflate error, empty output, or output over `cap`: never a truncated
/// prefix.
pub(crate) fn gunzip_bounded(bytes: &[u8], cap: u64) -> Option<Vec<u8>> {
    let out = read_bounded(flate2::read::GzDecoder::new(bytes), cap).ok()?;
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
/// Parse+render run on a budgeted worker ([`crate::safety::spawn_budgeted`]) joined with a
/// deadline ([`SVG_TIMEOUT`]): resvg has no internal timeout and runs in-process inside
/// Explorer's thumbnail host and, through `decode_menu_preview`, inside explorer.exe itself,
/// so an unbounded run is a DoS vector. On timeout this returns E_FAIL and the worker
/// finishes on its own, pinning the DLL (the `ModuleRef` `spawn_budgeted` takes before it
/// spawns) and counted against [`crate::safety::MAX_ABANDONED_WORKERS`] like every other
/// late worker, so repeated hostile SVGs cannot pile render threads up past the process-wide
/// budget. This used to spawn a bare `std::thread`, which was the one detached-worker path
/// that budget did not cover (2026-09-05 audit, F01).
pub(super) fn decode_svg(bytes: &[u8]) -> Result<DynamicImage> {
    decode_svg_with(bytes, SVG_TIMEOUT, render_svg)
}

/// [`decode_svg`] with the render and the deadline as parameters, so a test can stand in a
/// render that blocks on command and prove the timeout, the abandoned accounting and the
/// recovery without a pathological SVG or a ten-second wait.
pub(super) fn decode_svg_with<F>(bytes: &[u8], timeout: Duration, render: F) -> Result<DynamicImage>
where
    F: FnOnce(&[u8]) -> Result<DynamicImage> + Send + 'static,
{
    let owned = bytes.to_vec();
    match crate::safety::spawn_budgeted("st2k-svg-render", timeout, move || render(&owned)) {
        Some(r) => r,
        None => {
            crate::safety::log_debug(
                "SVG render exceeded the wall-clock deadline, or the abandoned-worker budget \
                 refused to start it",
            );
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
        crate::safety::log_debugf!("SVG parse failed: {e:?}");
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

#[cfg(test)]
mod worker_tests {
    use super::*;

    /// A render that never returns must time out, be counted as abandoned while it runs, and
    /// be uncounted once it finishes, all through the budgeted path `decode_svg` now uses.
    /// A bare thread (the previous shape) satisfied the first and none of the rest. Relative
    /// assertions only: other tests in this binary run budgeted workers concurrently.
    #[test]
    fn a_blocking_render_times_out_and_is_accounted_for() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let r = decode_svg_with(b"<svg/>", Duration::from_millis(20), move |_| {
            let _ = release_rx.recv();
            let _ = done_tx.send(());
            Err(Error::from(E_FAIL))
        });
        assert!(r.is_err(), "a blocked render must time out to E_FAIL");
        // Our render is still blocked and counted, so the count is at least one whatever
        // other tests' workers do in the meantime.
        assert!(
            crate::safety::abandoned_workers() >= 1,
            "the still-blocked render must be counted as abandoned"
        );
        let _ = release_tx.send(());
        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the abandoned render must still run to completion on its own"
        );
        // Its release from the count is the handshake `safety::worker_tests` pins
        // deterministically; the shared count cannot be read exactly here while other tests
        // run budgeted workers beside this one.
    }

    /// A prompt render returns its result through the same path.
    #[test]
    fn a_prompt_render_returns_its_result() {
        let r = decode_svg_with(b"<svg/>", Duration::from_secs(10), |_| {
            Ok(DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2)))
        });
        assert_eq!(r.map(|i| (i.width(), i.height())).ok(), Some((2, 2)));
    }
}

#[cfg(test)]
mod gunzip_tests {
    use super::*;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(bytes).unwrap();
        gz.finish().unwrap()
    }

    /// F15 (2026-09-05 audit): `gunzip_bounded` used to `.take(cap)` (no `+1`) and hand back
    /// whatever inflated, so a stream whose real decompressed size is PAST `cap` silently
    /// came back as a `cap`-byte prefix instead of being refused. Reverting the fix (swap
    /// `read_bounded` back for the old bare `.take(cap).read_to_end(..)`) makes this assert
    /// fail: it would return `Some(_)` with exactly `cap` bytes instead of `None`.
    #[test]
    fn gunzip_bounded_refuses_output_over_the_cap() {
        let cap = 64u64;
        let inner = vec![b'x'; (cap + 1) as usize];
        let gz = gzip(&inner);
        assert!(
            gunzip_bounded(&gz, cap).is_none(),
            "output one byte over the cap must be refused, never truncated"
        );
    }

    /// The exact-cap boundary must still succeed (refuse OVER the cap, never AT it).
    #[test]
    fn gunzip_bounded_accepts_output_exactly_at_the_cap() {
        let cap = 64u64;
        let inner = vec![b'x'; cap as usize];
        let gz = gzip(&inner);
        assert_eq!(gunzip_bounded(&gz, cap), Some(inner));
    }

    /// A truncated / corrupt gzip stream must fail rather than returning a partial inflate.
    #[test]
    fn gunzip_bounded_rejects_a_truncated_gzip_stream() {
        let mut gz = gzip(&vec![b'x'; 4096]);
        gz.truncate(gz.len() / 2);
        assert!(
            gunzip_bounded(&gz, 4096).is_none(),
            "a truncated gzip member must fail, not return a partial inflate"
        );
    }
}
