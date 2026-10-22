use super::*;
use crate::djvu_document::DjVuDocument;

/// #815: a refused output pixmap surfaces as the render-output limit, not
/// as a blank page.
#[test]
fn pixmap_too_large_maps_to_render_output_limit() {
    let e = RenderError::from(crate::pixmap::PixmapError::TooLarge {
        width: 10000,
        height: 10000,
        pixels: 100_000_000,
        max: Pixmap::MAX_PIXELS,
    });
    match e {
        RenderError::ResourceLimit(x) => {
            assert_eq!(
                x.axis,
                crate::resource_limits::ResourceLimitAxis::RenderOutputPixels
            );
            assert_eq!((x.found, x.limit), (100_000_000, Pixmap::MAX_PIXELS as u64));
            assert_eq!((x.width, x.height), (Some(10000), Some(10000)));
        }
        other => panic!("expected ResourceLimit, got {other:?}"),
    }
}

#[test]
fn pixmap_overflow_maps_to_invalid_dimensions() {
    let e = RenderError::from(crate::pixmap::PixmapError::Overflow {
        width: u32::MAX,
        height: u32::MAX,
    });
    assert!(matches!(
        e,
        RenderError::InvalidDimensions {
            width: u32::MAX,
            height: u32::MAX
        }
    ));
}

pub(super) fn assets_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("references/djvujs/library/assets")
}

/// Helper that returns an owned document so tests can borrow pages from it.
pub(super) fn load_doc(filename: &str) -> DjVuDocument {
    let data = std::fs::read(assets_path().join(filename))
        .unwrap_or_else(|_| panic!("{filename} must exist"));
    DjVuDocument::parse(&data).unwrap_or_else(|e| panic!("parse failed: {e}"))
}

mod aa_zoom;
mod banded;
mod compositor;
mod jpeg;
mod lanczos;
mod options;
mod permissive;
mod pipeline;
mod region;
mod streaming;
mod tile_cache;

use compositor::*;
use permissive::*;
use pipeline::*;
