use super::*;

/// The current image render installed in the window (the DIB + its natural dims + the bg
/// it was composited over). Sole owner of `hbmp`; freed when replaced or on window destroy.
pub(crate) struct RenderData {
    pub hbmp: HBITMAP,
    /// The NATIVE dimensions of the image — what the file actually contains.
    ///
    /// Everything user-visible keys off these and always has: the aspect-fit geometry, what
    /// "100%" means to the zoom, the size the window opens at, and the dimensions in the
    /// caption. They stay native even when `hbmp` is a smaller codec-scaled decode, which is
    /// what makes holding fewer pixels invisible.
    pub iw: i32,
    pub ih: i32,
    /// The dimensions of `hbmp` ITSELF, which is a different question. Equal to `(iw, ih)` for
    /// a full-resolution decode; smaller when the codec was asked for a display-sized picture
    /// and [`paint_image`] stretches the last little bit.
    bw: i32,
    bh: i32,
    /// The bitmap holds PREMULTIPLIED alpha and must be composed with `AlphaBlend`, not blitted.
    /// Only ever true for the main image pane (see [`make_render`]); cover art and inline Markdown
    /// images stay flattened, so they keep the plain blit and want no checkerboard.
    pub alpha: bool,
    /// The premultiplied pixels of `hbmp`, which is a DIB SECTION, so this is its own backing
    /// memory rather than a second copy. Valid exactly as long as `hbmp`. Null unless `alpha`.
    src: *const u8,
    /// Last box-filtered downscale, keyed by the size it was built for. See [`scaled_for`] for
    /// why the resampling is done here rather than left to GDI.
    scaled: RefCell<Option<(i32, i32, HBITMAP)>>,
}

impl RenderData {
    /// A fully opaque render: plain `StretchBlt`, no checkerboard, no resampling cache.
    pub(crate) fn opaque(hbmp: HBITMAP, iw: i32, ih: i32) -> Self {
        Self {
            hbmp,
            iw,
            ih,
            bw: iw,
            bh: ih,
            alpha: false,
            src: core::ptr::null(),
            scaled: RefCell::new(None),
        }
    }

    /// Re-label a render whose bitmap is a codec-scaled decode of a larger image, so the
    /// geometry keeps answering about the real thing. See the `iw`/`bw` split above.
    fn with_native(mut self, nat: (i32, i32)) -> Self {
        self.bw = self.iw;
        self.bh = self.ih;
        self.iw = nat.0;
        self.ih = nat.1;
        self
    }
}

impl Drop for RenderData {
    fn drop(&mut self) {
        unsafe {
            if let Some((_, _, s)) = self.scaled.borrow_mut().take() {
                let _ = DeleteObject(s.into());
            }
            let _ = DeleteObject(self.hbmp.into());
        }
    }
}

/// Build a top-down 32bpp DIB of `rgba` composited over the opaque `bg` (`COLORREF`
/// 0x00BBGGRR), so painting is a plain `StretchBlt`. `None` on a malformed size /
/// allocation failure (never panics on attacker-controlled dims). Verbatim port of
/// `previewhandler::make_dib`.
pub(crate) unsafe fn make_dib(iw: i32, ih: i32, rgba: &[u8], bg: u32) -> Option<HBITMAP> {
    make_dib_hinted(iw, ih, rgba, bg, None)
}

/// True when every pixel is fully opaque — the common case for a photo, and the one that lets
/// [`make_dib_hinted`] skip the compositing arithmetic entirely.
fn all_opaque(rgba: &[u8], px: usize) -> bool {
    (0..px).all(|i| rgba[i * 4 + 3] == 255)
}

/// Source-over composite of one channel: `s` at coverage `a` laid onto `d`. Mirrors the private
/// `comp` closure inside `sagethumbs2k_core::safety::composite_rgba_over_bg` (the now-shared
/// implementation `make_dib_hinted` below delegates to) — kept here, `#[cfg(test)]`-only, so the
/// `a == 255` / `a == 0` reductions that fast path relies on stay a property a test can assert
/// against, rather than just a claim in a comment. Not production code any more (nothing here
/// calls it outside `render_size_tests`), hence the test-only gate.
#[cfg(test)]
fn composite_channel(s: u32, d: u32, a: u32) -> u8 {
    (((s * a) + (d * (255 - a)) + 127) / 255) as u8
}

/// [`make_dib`] with a caller-supplied opacity answer. `None` means "work it out", which costs a
/// full pass over the alpha bytes; callers that already know (because they had to ask the same
/// question to choose a DIB builder at all) pass `Some` and save it.
///
/// A thin wrapper over [`sagethumbs2k_core::safety::composite_rgba_over_bg`] — the actual
/// compositing loop is shared with `previewhandler::make_dib` (the Explorer preview-pane host),
/// which used to carry its own hand-copied duplicate that never got this opacity-hint fast
/// path. `all_opaque` above stays local: `make_render`'s premultiplied path below still needs it
/// directly, and that path is NOT part of this shared loop (different output — premultiplied
/// BGRA for `AlphaBlend`, not composited-over-bg).
unsafe fn make_dib_hinted(
    iw: i32,
    ih: i32,
    rgba: &[u8],
    bg: u32,
    opaque: Option<bool>,
) -> Option<HBITMAP> {
    sagethumbs2k_core::safety::composite_rgba_over_bg(iw, ih, rgba, bg, opaque)
}

/// The same DIB, but for the MAIN image pane, where transparency has to survive to paint time so a
/// checkerboard can show through it. Returns `(bitmap, has_alpha)`.
///
/// When the image is fully opaque this produces byte-identical output to [`make_dib`] and the
/// caller keeps using the plain `StretchBlt` path, so the overwhelmingly common case (a photo) is
/// completely unchanged, HALFTONE downscaling included. Only when some pixel is actually
/// translucent does it emit PREMULTIPLIED BGRA for `AlphaBlend`; premultiplied data is also what
/// makes the intermediate `StretchBlt` in [`paint_image`] filter correctly, since premultiplied
/// channels are linearly interpolatable and straight ones are not.
pub(crate) unsafe fn make_render(iw: i32, ih: i32, rgba: &[u8], bg: u32) -> Option<RenderData> {
    if iw <= 0 || ih <= 0 {
        return None;
    }
    let px = (iw as usize).checked_mul(ih as usize)?;
    if rgba.len() < px.checked_mul(4)? {
        return None;
    }
    let has_alpha = !all_opaque(rgba, px);
    if !has_alpha {
        // The opacity question is already answered — hand it down rather than let `make_dib`
        // walk all 12 million alpha bytes a second time to reach the same conclusion.
        return make_dib_hinted(iw, ih, rgba, bg, Some(true))
            .map(|h| RenderData::opaque(h, iw, ih));
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = iw;
    bmi.bmiHeader.biHeight = -ih; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        return None;
    }
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    for i in 0..px {
        let a = rgba[i * 4 + 3] as u32;
        let pm = |s: u8| (((s as u32 * a) + 127) / 255) as u8;
        dst[i * 4] = pm(rgba[i * 4 + 2]); // B
        dst[i * 4 + 1] = pm(rgba[i * 4 + 1]); // G
        dst[i * 4 + 2] = pm(rgba[i * 4]); // R
        dst[i * 4 + 3] = a as u8;
    }
    Some(RenderData {
        hbmp,
        iw,
        ih,
        bw: iw,
        bh: ih,
        alpha: true,
        src: bits as *const u8,
        scaled: RefCell::new(None),
    })
}

/// Build the window's image render from a finished decode, whether that decode is the whole
/// image or a codec-scaled stand-in for it.
///
/// The one place that knows how to keep `DecodedRgba::nat` and `RenderData::iw` in step, so no
/// caller has to remember that the pixels and the image can be different sizes.
pub(crate) unsafe fn make_render_for(d: &DecodedRgba, bg: u32) -> Option<RenderData> {
    let rd = make_render(d.w, d.h, &d.rgba, bg)?;
    Some(if d.is_full() {
        rd
    } else {
        rd.with_native(d.nat)
    })
}

/// A `dw`x`dh` copy of `rd`, box-filtered (a true area average), cached until the size changes.
/// `None` when it is not worth doing or could not be built, and the caller then lets GDI scale.
///
/// **Why this exists.** A translucent image cannot go through `StretchBlt`'s good `HALFTONE`
/// filter, because that mode treats the surface as plain RGB and destroys the alpha byte. The only
/// GDI call that respects alpha is `AlphaBlend`, and it ignores the stretch mode entirely, so it
/// point-samples when shrinking. Measured on a concentric-ring test pattern at a 3x downscale:
/// `AlphaBlend` produced 58% more spurious high-frequency energy than the true area average, versus
/// `HALFTONE`'s 21%, i.e. visible aliasing on fine detail. Averaging the pixels here fixes that and
/// is actually CLOSER to ground truth than `HALFTONE` is.
///
/// Only used when SHRINKING. Enlarging point-samples, which is what you want for inspecting pixels.
unsafe fn scaled_for(rd: &RenderData, dw: i32, dh: i32) -> Option<HBITMAP> {
    // Against the BITMAP's dims, not the image's: `hbmp` may already be a scaled decode, and
    // shrinking is only worth doing relative to what is actually there to shrink.
    if rd.src.is_null() || dw <= 0 || dh <= 0 || dw >= rd.bw || dh >= rd.bh {
        return None;
    }
    if let Some((cw, ch, h)) = *rd.scaled.borrow() {
        if cw == dw && ch == dh {
            return Some(h);
        }
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = dw;
    bmi.bmiHeader.biHeight = -dh;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0;
    let mut bits: *mut c_void = core::ptr::null_mut();
    let out = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(out.into());
        return None;
    }
    let (sw, sh) = (rd.bw as usize, rd.bh as usize);
    let src = core::slice::from_raw_parts(rd.src, sw * sh * 4);
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, (dw * dh) as usize * 4);
    // Source span of each destination row/column, precomputed so the inner loop stays tight.
    let xs: Vec<(usize, usize)> = (0..dw as usize)
        .map(|x| {
            let a = x * sw / dw as usize;
            let b = ((x + 1) * sw).div_ceil(dw as usize).min(sw);
            (a, b.max(a + 1))
        })
        .collect();
    for y in 0..dh as usize {
        let y0 = y * sh / dh as usize;
        let y1 = (((y + 1) * sh).div_ceil(dh as usize)).min(sh).max(y0 + 1);
        for (x, &(x0, x1)) in xs.iter().enumerate() {
            let (mut b, mut g, mut r, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                let row = sy * sw * 4;
                for sx in x0..x1 {
                    let i = row + sx * 4;
                    b += src[i] as u32;
                    g += src[i + 1] as u32;
                    r += src[i + 2] as u32;
                    a += src[i + 3] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * dw as usize + x) * 4;
            dst[o] = (b / n) as u8;
            dst[o + 1] = (g / n) as u8;
            dst[o + 2] = (r / n) as u8;
            dst[o + 3] = (a / n) as u8;
        }
    }
    if let Some((_, _, old)) = rd.scaled.borrow_mut().replace((dw, dh, out)) {
        let _ = DeleteObject(old.into());
    }
    Some(out)
}

/// Would drawing `rd` into `rc` at `zoom` magnify its bitmap, i.e. is the render a codec-scaled
/// stand-in that has run out of detail?
///
/// `false` for a full-resolution render (nothing sharper exists) and for any zoom the scaled
/// pixels still cover, which is the whole of ordinary fit-view browsing.
pub(crate) fn wants_full_resolution(rd: &RenderData, rc: &RECT, zoom: f64) -> bool {
    if rd.bw >= rd.iw && rd.bh >= rd.ih {
        return false; // already the whole image
    }
    let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
    let scale = fit_scale(rd.iw, rd.ih, cw, ch) * zoom;
    let need_w = rd.iw as f64 * scale;
    let need_h = rd.ih as f64 * scale;
    need_w > rd.bw as f64 * FIT_UPSCALE_TOLERANCE || need_h > rd.bh as f64 * FIT_UPSCALE_TOLERANCE
}

/// Aspect-fit scale (image px -> screen px) of `rd` inside `(cw, ch)`. Shared by the paint
/// and the zoom-at-cursor math so they never disagree.
pub(crate) fn fit_scale(iw: i32, ih: i32, cw: i32, ch: i32) -> f64 {
    if iw <= 0 || ih <= 0 || cw <= 0 || ch <= 0 {
        return 1.0;
    }
    f64::min(cw as f64 / iw as f64, ch as f64 / ih as f64)
}

/// Paint the image `rd` into `rc`, letterboxed with `bg`, at `zoom`x the aspect-fit scale and
/// offset by `pan` (device px). `zoom == 1.0`, `pan == (0,0)` is the plain aspect-fit centered
/// draw. Ported from `previewhandler::draw` (fill = letterbox, then `HALFTONE` `StretchBlt`).
/// Blit `rd` into EXACTLY `rc`, no fit, no zoom, no centring.
///
/// The continuous PDF view has already decided where every page goes and how big it is, so
/// `paint_image`'s aspect-fit would fight that layout rather than serve it. A PDF page is
/// always opaque, and the tile was rasterized at the width it is being drawn at, so this is a
/// 1:1 blit in the normal case and only stretches during the frame between a window resize and
/// the re-rendered tiles landing.
pub(crate) unsafe fn blit_exact(hdc: HDC, rc: &RECT, rd: &RenderData) {
    let (dw, dh) = (rc.right - rc.left, rc.bottom - rc.top);
    if dw <= 0 || dh <= 0 || rd.bw <= 0 || rd.bh <= 0 {
        return;
    }
    let memdc = CreateCompatibleDC(Some(hdc));
    let old = SelectObject(memdc, rd.hbmp.into());
    SetStretchBltMode(hdc, HALFTONE);
    let _ = StretchBlt(
        hdc,
        rc.left,
        rc.top,
        dw,
        dh,
        Some(memdc),
        0,
        0,
        rd.bw,
        rd.bh,
        SRCCOPY,
    );
    SelectObject(memdc, old);
    let _ = DeleteDC(memdc);
}

pub(crate) unsafe fn paint_image(
    hdc: HDC,
    rc: &RECT,
    rd: &RenderData,
    bg: u32,
    zoom: f64,
    pan: (i32, i32),
    checker: Option<i32>,
) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());

    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    if cw <= 0 || ch <= 0 || rd.iw <= 0 || rd.ih <= 0 {
        return;
    }
    let scale = fit_scale(rd.iw, rd.ih, cw, ch) * zoom;
    let dw = ((rd.iw as f64 * scale).round() as i32).max(1);
    let dh = ((rd.ih as f64 * scale).round() as i32).max(1);
    let dx = rc.left + (cw - dw) / 2 + pan.0;
    let dy = rc.top + (ch - dh) / 2 + pan.1;

    let memdc = CreateCompatibleDC(Some(hdc));
    let old = SelectObject(memdc, rd.hbmp.into());
    SetStretchBltMode(hdc, HALFTONE);
    // The branch MUST key on `rd.alpha`, never on the checkerboard setting. A translucent bitmap
    // holds PREMULTIPLIED, un-composited pixels (see `make_render`), and `StretchBlt` ignores the
    // alpha byte entirely, so blitting one paints the premultiplied values as if they were opaque:
    // a 50%-alpha white pixel lands as mid-grey. Turning the checkerboard OFF must therefore still
    // take the blend path, just without the pattern under it (the flat `bg` fill above is what it
    // composites onto instead).
    match rd.alpha {
        // Translucent: optional checkerboard, then compose the premultiplied bitmap over it.
        //
        // `AlphaBlend` does its own scaling and ignores the stretch mode, and there is no way to
        // pre-scale through HALFTONE first: `StretchBlt` in HALFTONE mode treats the surface as
        // plain RGB and DESTROYS the alpha byte, so an intermediate scratch surface comes out
        // fully transparent and nothing draws at all (measured, not assumed). So the blend reads
        // straight from the source bitmap. Only translucent images take this path.
        true => {
            if let Some(cell) = checker {
                let (c0, c1) = sagethumbs2k_core::checker::checker_shades(bg);
                let cr = RECT {
                    left: dx,
                    top: dy,
                    right: dx + dw,
                    bottom: dy + dh,
                };
                sagethumbs2k_core::checker::fill_checker(hdc, &cr, c0, c1, cell);
            }
            let bf = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            // Shrinking: area-average it ourselves first and blend 1:1, because AlphaBlend's own
            // scaler aliases badly (see `scaled_for`). Enlarging, or if the scratch could not be
            // built, blend straight from the source.
            match scaled_for(rd, dw, dh) {
                Some(s) => {
                    let sdc = CreateCompatibleDC(Some(hdc));
                    let olds = SelectObject(sdc, s.into());
                    let _ = AlphaBlend(hdc, dx, dy, dw, dh, sdc, 0, 0, dw, dh, bf);
                    SelectObject(sdc, olds);
                    let _ = DeleteDC(sdc);
                }
                None => {
                    let _ = AlphaBlend(hdc, dx, dy, dw, dh, memdc, 0, 0, rd.bw, rd.bh, bf);
                }
            }
        }
        // Opaque: unchanged from before any of this existed. `make_render` produced byte-identical
        // output to the old `make_dib` for these, so photos take exactly the old HALFTONE path.
        false => {
            let _ = StretchBlt(
                hdc,
                dx,
                dy,
                dw,
                dh,
                Some(memdc),
                0,
                0,
                rd.bw,
                rd.bh,
                SRCCOPY,
            );
        }
    }
    SelectObject(memdc, old);
    let _ = DeleteDC(memdc);
}

#[cfg(test)]
mod render_size_tests {
    use super::*;

    /// The opaque fast path in [`make_dib_hinted`] claims to be the compositing loop with the
    /// multiply and divide removed, not an approximation of it. That is only true if the
    /// arithmetic genuinely reduces to the identity at full alpha — so check every combination
    /// rather than trusting the algebra in the comment.
    #[test]
    fn composite_at_full_alpha_is_exactly_the_source() {
        for s in 0..=255u32 {
            for d in 0..=255u32 {
                assert_eq!(
                    composite_channel(s, d, 255),
                    s as u8,
                    "a fully opaque source must ignore the background (s={s}, d={d})"
                );
            }
        }
    }

    /// And the other end: at zero alpha the background must survive untouched, which is what
    /// makes the loop a real source-over rather than a lerp with rounding bias.
    #[test]
    fn composite_at_zero_alpha_is_exactly_the_background() {
        for d in 0..=255u32 {
            assert_eq!(composite_channel(200, d, 0), d as u8);
        }
    }

    /// The invariant the whole deferred-decode scheme rests on: a scaled decode holds SMALL
    /// pixels while the render still reports the REAL image size.
    ///
    /// If `iw`/`ih` ever came back as the bitmap's size instead, the failure would be quiet and
    /// everywhere — the caption would report the wrong dimensions, the window would open at the
    /// wrong size, "100%" would mean 100% of a downscale, and `wants_full_resolution` could
    /// never fire because the bitmap would always look big enough.
    #[test]
    fn a_scaled_render_reports_the_real_image_size_not_the_bitmaps() {
        let nat = (4000, 3000);
        let d = DecodedRgba::scaled(400, 300, vec![255u8; 400 * 300 * 4], nat);
        assert!(!d.is_full(), "a scaled decode is not the whole image");
        let rd = unsafe { make_render_for(&d, 0x0020_2020) }.expect("build");
        assert_eq!((rd.iw, rd.ih), nat, "reports the real image size");
        assert_eq!((rd.bw, rd.bh), (400, 300), "holds only the small pixels");

        // A full decode must be indistinguishable from before any of this existed.
        let f = DecodedRgba::full(400, 300, vec![255u8; 400 * 300 * 4]);
        assert!(f.is_full());
        let rd = unsafe { make_render_for(&f, 0x0020_2020) }.expect("build");
        assert_eq!((rd.iw, rd.ih), (400, 300));
        assert_eq!((rd.bw, rd.bh), (400, 300));
    }

    /// Zoom escalation has to fire exactly when the bitmap runs out of detail, and never for a
    /// full-resolution render (there is nothing sharper to fetch, and asking forever would
    /// spawn a decode per repaint).
    #[test]
    fn full_resolution_is_requested_only_once_the_zoom_outgrows_the_bitmap() {
        let pane = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let scaled = DecodedRgba::scaled(400, 300, vec![255u8; 400 * 300 * 4], (4000, 3000));
        let rd = unsafe { make_render_for(&scaled, 0) }.expect("build");
        // Aspect-fit puts 4000x3000 into 800x600, i.e. 800 px on screen against 400 held.
        assert!(
            wants_full_resolution(&rd, &pane, 1.0),
            "800 px of screen from a 400 px bitmap is a 2x stretch, well past tolerance"
        );
        // Half the pane: 400 px on screen, exactly what the bitmap holds.
        let half = RECT {
            right: 400,
            bottom: 300,
            ..pane
        };
        assert!(!wants_full_resolution(&rd, &half, 1.0), "exactly covered");
        // Inside the tolerance: a 20% stretch is not worth a decode (see
        // `FIT_UPSCALE_TOLERANCE` for why this band has to exist at all).
        assert!(
            !wants_full_resolution(&rd, &half, 1.2),
            "a stretch under tolerance must NOT trigger a fetch, or the fit view on a 4K              panel would demand a full decode on every navigation"
        );
        assert!(
            wants_full_resolution(&rd, &half, 1.35),
            "past tolerance, the real pixels are fetched"
        );
        assert!(
            wants_full_resolution(&rd, &half, 2.0),
            "zooming well past what it holds must ask"
        );

        let full = DecodedRgba::full(4000, 3000, vec![255u8; 16]);
        // `make_render_for` would need the real pixel buffer; check the predicate directly on a
        // full-resolution render built at its own size.
        let rd_full = unsafe { make_render(2, 2, &[255u8; 16], 0) }.expect("build");
        assert!(
            !wants_full_resolution(&rd_full, &pane, 8.0),
            "a full-resolution render must never ask, at any zoom"
        );
        assert!(full.is_full());
    }
}
