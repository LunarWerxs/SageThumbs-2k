//! The size of one page render, in both orientations.
//!
//! A caller asks for a size the reader sees: a DPI, a scale, a width to fit, a
//! box, or an exact size. The page's INFO chunk may rotate the page by 90°, and
//! the compositor works in the page's *native* (pre-rotation) orientation and
//! rotates last. So a request in display space must be turned into a native
//! buffer size for [`RenderOptions`], and a caller that declares the output
//! size (an image header, a canvas, a text overlay) needs the display size.
//!
//! [`RenderSize`] is the one place that does that translation, with one
//! rounding and clamping policy. Every render entry point, exporter, and
//! binding asks it instead of re-deriving `page.width() * scale`.

use crate::djvu_document::DjVuPage;
use crate::djvu_render::{RenderOptions, UserRotation, display_dimensions};
use crate::info::Rotation;

/// The size of one page render, resolved against the page's INFO rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderSize {
    /// Compositor buffer size, in the page's native (pre-rotation)
    /// orientation: the `width × height` of [`RenderOptions`].
    pub native: (u32, u32),
    /// Output size, after the INFO rotation: the size of the returned pixmap.
    pub display: (u32, u32),
}

impl RenderSize {
    /// The native page scaled by `scale`; each side is rounded and at least 1.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub fn at_scale(page: &DjVuPage, scale: f32) -> Self {
        let w = ((page.width() as f32 * scale).round() as u32).max(1);
        let h = ((page.height() as f32 * scale).round() as u32).max(1);
        Self::from_native(page, (w, h))
    }

    /// The page rendered at `target_dpi`. A `0`-DPI page counts as 1 DPI so
    /// the scale stays finite.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub fn at_dpi(page: &DjVuPage, target_dpi: f32) -> Self {
        Self::at_scale(page, target_dpi / page.dpi().max(1) as f32)
    }

    /// Display width `width`, height by the page's display aspect ratio.
    pub fn fit_width(page: &DjVuPage, width: u32) -> Self {
        let (dw, dh) = display_dimensions(page);
        let height = if dw == 0 {
            width
        } else {
            ((dh as f64 * width as f64) / dw as f64).round() as u32
        };
        Self::exact(page, width, height)
    }

    /// Display height `height`, width by the page's display aspect ratio.
    pub fn fit_height(page: &DjVuPage, height: u32) -> Self {
        let (dw, dh) = display_dimensions(page);
        let width = if dh == 0 {
            height
        } else {
            ((dw as f64 * height as f64) / dh as f64).round() as u32
        };
        Self::exact(page, width, height)
    }

    /// The largest display size within `max_width × max_height` that keeps
    /// the page's aspect ratio. A page with a zero side fills the box.
    pub fn fit_box(page: &DjVuPage, max_width: u32, max_height: u32) -> Self {
        let (dw, dh) = display_dimensions(page);
        if dw == 0 || dh == 0 {
            return Self::exact(page, max_width, max_height);
        }
        let scale = (max_width as f64 / dw as f64).min(max_height as f64 / dh as f64);
        Self::exact(
            page,
            (dw as f64 * scale).round() as u32,
            (dh as f64 * scale).round() as u32,
        )
    }

    /// Exactly `width × height` in display space (each side at least 1); the
    /// aspect ratio is the caller's choice.
    pub fn exact(page: &DjVuPage, width: u32, height: u32) -> Self {
        let display = (width.max(1), height.max(1));
        Self {
            native: swap_if_quarter_turn(page.rotation(), display),
            display,
        }
    }

    /// Render options for this size: only `width × height` are set, the rest
    /// are the defaults. The pipeline derives the decode scale from `width`.
    pub fn options(self) -> RenderOptions {
        RenderOptions {
            width: self.native.0,
            height: self.native.1,
            ..RenderOptions::default()
        }
    }

    /// Render options that leave the page in its native orientation: the
    /// INFO rotation is cancelled, so the pixmap is `native`-sized. For
    /// exporters that rotate the page themselves (PDF `/Rotate`) and keep
    /// masks, text, and links in native coordinates.
    #[cfg_attr(not(feature = "pdf"), allow(dead_code))]
    pub fn native_options(self, page: &DjVuPage) -> RenderOptions {
        RenderOptions {
            rotation: match page.rotation() {
                Rotation::None => UserRotation::None,
                Rotation::Cw90 => UserRotation::Ccw90,
                Rotation::Rot180 => UserRotation::Rot180,
                Rotation::Ccw90 => UserRotation::Cw90,
            },
            ..self.options()
        }
    }

    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    fn from_native(page: &DjVuPage, native: (u32, u32)) -> Self {
        Self {
            native,
            display: swap_if_quarter_turn(page.rotation(), native),
        }
    }
}

fn swap_if_quarter_turn(rotation: Rotation, (w, h): (u32, u32)) -> (u32, u32) {
    match rotation {
        Rotation::Cw90 | Rotation::Ccw90 => (h, w),
        Rotation::None | Rotation::Rot180 => (w, h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::djvu_document::DjVuDocument;
    use crate::djvu_render::render_pixmap;

    fn load(name: &str) -> DjVuDocument {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        DjVuDocument::parse(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Every request resolves to a buffer whose render comes out at exactly
    /// the promised display size, on an upright and a quarter-turned page.
    #[test]
    fn render_comes_out_at_display_size() {
        for name in ["boy_jb2.djvu", "boy_jb2_rotate90.djvu"] {
            let doc = load(name);
            let page = doc.page(0).unwrap();
            for size in [
                RenderSize::at_scale(page, 0.5),
                RenderSize::at_dpi(page, 75.0),
                RenderSize::fit_width(page, 120),
                RenderSize::fit_height(page, 90),
                RenderSize::fit_box(page, 100, 100),
                RenderSize::exact(page, 70, 40),
            ] {
                let pm = render_pixmap(page, &size.options()).unwrap();
                assert_eq!((pm.width, pm.height), size.display, "{name}: {size:?}");
            }
        }
    }

    #[test]
    fn fit_requests_are_in_display_space() {
        let doc = load("boy_jb2_rotate90.djvu");
        let page = doc.page(0).unwrap();
        let (dw, dh) = display_dimensions(page);
        assert_ne!(dw, dh, "fixture must be non-square");
        assert_eq!(RenderSize::fit_width(page, dw).display, (dw, dh));
        assert_eq!(RenderSize::fit_height(page, dh).display, (dw, dh));
        assert_eq!(RenderSize::fit_box(page, dw, dh).display, (dw, dh));
        assert_eq!(RenderSize::at_scale(page, 1.0).display, (dw, dh));
        assert_eq!(
            RenderSize::at_scale(page, 1.0).native,
            (page.width() as u32, page.height() as u32)
        );
    }

    #[test]
    fn native_options_cancel_info_rotation() {
        let upright = load("boy_jb2.djvu");
        let rotated = load("boy_jb2_rotate90.djvu");
        let page = rotated.page(0).unwrap();
        let size = RenderSize::at_scale(page, 1.0);
        let pm = render_pixmap(page, &size.native_options(page)).unwrap();
        assert_eq!((pm.width, pm.height), size.native);
        let want = crate::djvu_render::render_pixmap(
            upright.page(0).unwrap(),
            &RenderSize::at_scale(upright.page(0).unwrap(), 1.0).options(),
        )
        .unwrap();
        assert!(
            pm.data == want.data,
            "native render must match the upright page"
        );
    }

    #[test]
    fn zero_dpi_page_stays_finite() {
        let doc = load("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let size = RenderSize::at_dpi(page, 0.0);
        assert_eq!(size.native, (1, 1));
    }
}
