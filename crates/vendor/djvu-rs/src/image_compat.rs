//! `image::ImageDecoder` integration for DjVu pages.
//!
//! This module provides [`DjVuDecoder`], which implements the
//! [`image::ImageDecoder`] and [`image::ImageDecoderRect`] traits from the
//! `image` crate, making djvu-rs a first-class image format usable anywhere
//! image-rs pipelines are used.
//!
//! ## Key public types
//!
//! - [`DjVuDecoder`] — implements `image::ImageDecoder` and `image::ImageDecoderRect`
//!   for a single DjVu page (compatible with `image` 0.25+)
//! - [`ImageCompatError`] — typed errors from this module
//!
//! ## Usage
//!
//! ```no_run
//! use djvu_rs::{DjVuDocument, image_compat::DjVuDecoder};
//! use image::ImageDecoder;
//!
//! let data = std::fs::read("file.djvu").unwrap();
//! let doc = DjVuDocument::parse(&data).unwrap();
//! let page = doc.page(0).unwrap();
//!
//! let decoder = DjVuDecoder::new(&page).unwrap();
//! let (w, h) = decoder.dimensions();
//! let mut buf = vec![0u8; decoder.total_bytes() as usize];
//! decoder.read_image(&mut buf).unwrap();
//! ```

use image::{
    ColorType, ImageDecoder, ImageDecoderRect, ImageResult,
    error::{DecodingError, ImageError, ImageFormatHint},
};

use crate::djvu_document::DjVuPage;
use crate::djvu_render::{RenderError, RenderOptions};

// ---- Error ------------------------------------------------------------------

/// Errors from the image-rs integration layer.
#[derive(Debug, thiserror::Error)]
pub enum ImageCompatError {
    /// The underlying render pipeline failed.
    #[error("render error: {0}")]
    Render(#[from] RenderError),
}

impl From<ImageCompatError> for ImageError {
    fn from(e: ImageCompatError) -> Self {
        ImageError::Decoding(DecodingError::new(
            ImageFormatHint::Name("DjVu".to_string()),
            e,
        ))
    }
}

// ---- DjVuDecoder ------------------------------------------------------------

/// An `image::ImageDecoder` and `image::ImageDecoderRect` for a single DjVu page.
///
/// By default renders at the native page resolution. Use [`DjVuDecoder::with_size`]
/// to override the output dimensions.
pub struct DjVuDecoder<'a> {
    page: &'a DjVuPage,
    width: u32,
    height: u32,
}

impl<'a> DjVuDecoder<'a> {
    /// Construct a decoder from a [`DjVuPage`] reference.
    ///
    /// The output dimensions default to the native page size from the INFO chunk.
    pub fn new(page: &'a DjVuPage) -> Result<Self, ImageCompatError> {
        Ok(Self {
            width: page.width() as u32,
            height: page.height() as u32,
            page,
        })
    }

    /// Override the output dimensions.
    ///
    /// The rendered image will be scaled to `width × height` using bilinear
    /// interpolation via [`RenderOptions`].
    #[must_use]
    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Render the full page into an RGBA byte buffer.
    fn render_to_vec(&self) -> Result<Vec<u8>, ImageCompatError> {
        let opts = RenderOptions {
            width: self.width,
            height: self.height,
            ..RenderOptions::default()
        };
        let size = (self.width as usize)
            .saturating_mul(self.height as usize)
            .saturating_mul(4);
        let mut buf = vec![0u8; size];
        self.page.render_into(&opts, &mut buf)?;
        Ok(buf)
    }
}

// ---- ImageDecoder impl ------------------------------------------------------

impl<'a> ImageDecoder for DjVuDecoder<'a> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        ColorType::Rgba8
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()>
    where
        Self: Sized,
    {
        let data = self.render_to_vec().map_err(ImageError::from)?;
        if buf.len() != data.len() {
            return Err(ImageError::Decoding(DecodingError::new(
                ImageFormatHint::Name("DjVu".to_string()),
                format!(
                    "buffer size mismatch: expected {}, got {}",
                    data.len(),
                    buf.len()
                ),
            )));
        }
        buf.copy_from_slice(&data);
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

// ---- ImageDecoderRect impl --------------------------------------------------

impl<'a> ImageDecoderRect for DjVuDecoder<'a> {
    /// Decode a rectangular region of the page.
    ///
    /// DjVu does not natively support partial rendering; this implementation
    /// renders the full page and copies out the requested rectangle.
    /// `row_pitch` is the number of bytes between the start of consecutive
    /// rows in `buf` (must be ≥ `width * bytes_per_pixel`).
    fn read_rect(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        buf: &mut [u8],
        row_pitch: usize,
    ) -> ImageResult<()> {
        let bytes_per_pixel = self.color_type().bytes_per_pixel() as usize;
        let full_row_bytes = self.width as usize * bytes_per_pixel;
        let rect_row_bytes = width as usize * bytes_per_pixel;

        // Validate rectangle stays within image bounds.
        let x_end = x.checked_add(width).ok_or_else(|| {
            ImageError::Decoding(DecodingError::new(
                ImageFormatHint::Name("DjVu".to_string()),
                "rectangle x+width overflows u32",
            ))
        })?;
        let y_end = y.checked_add(height).ok_or_else(|| {
            ImageError::Decoding(DecodingError::new(
                ImageFormatHint::Name("DjVu".to_string()),
                "rectangle y+height overflows u32",
            ))
        })?;
        if x_end > self.width || y_end > self.height {
            return Err(ImageError::Decoding(DecodingError::new(
                ImageFormatHint::Name("DjVu".to_string()),
                format!(
                    "rectangle ({x},{y},{width},{height}) out of image bounds ({}×{})",
                    self.width, self.height
                ),
            )));
        }

        let full = self.render_to_vec().map_err(ImageError::from)?;

        for row in 0..height as usize {
            let src_y = y as usize + row;
            let src_start = src_y * full_row_bytes + x as usize * bytes_per_pixel;
            let src_end = src_start + rect_row_bytes;
            let dst_start = row * row_pitch;
            let dst_end = dst_start + rect_row_bytes;

            buf[dst_start..dst_end].copy_from_slice(&full[src_start..src_end]);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ColorType, ImageDecoder};

    fn assets_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets")
    }

    fn load_page(name: &str) -> crate::djvu_document::DjVuDocument {
        let data =
            std::fs::read(assets_path().join(name)).unwrap_or_else(|_| panic!("{name} must exist"));
        crate::djvu_document::DjVuDocument::parse(&data).unwrap_or_else(|e| panic!("parse: {e}"))
    }

    #[test]
    fn dimensions_match_page_size() {
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let decoder = DjVuDecoder::new(page).unwrap();
        let (w, h) = decoder.dimensions();
        assert_eq!(w, page.width() as u32);
        assert_eq!(h, page.height() as u32);
    }

    #[test]
    fn color_type_is_rgba8() {
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let decoder = DjVuDecoder::new(page).unwrap();
        assert_eq!(decoder.color_type(), ColorType::Rgba8);
    }

    #[test]
    fn total_bytes_equals_w_times_h_times_4() {
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let decoder = DjVuDecoder::new(page).unwrap();
        let (w, h) = decoder.dimensions();
        assert_eq!(decoder.total_bytes(), (w as u64) * (h as u64) * 4);
    }

    #[test]
    fn read_image_fills_buffer() {
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let decoder = DjVuDecoder::new(page).unwrap();
        let n = decoder.total_bytes() as usize;
        let mut buf = vec![0u8; n];
        decoder.read_image(&mut buf).unwrap();
        // At least some pixels must be non-zero for a real page
        assert!(
            buf.iter().any(|&b| b != 0),
            "rendered image must have non-zero pixels"
        );
    }

    #[test]
    fn read_image_wrong_buffer_size_returns_error() {
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let decoder = DjVuDecoder::new(page).unwrap();
        let mut buf = vec![0u8; 4]; // far too small
        let result = decoder.read_image(&mut buf);
        assert!(result.is_err(), "wrong buffer size must return error");
    }

    #[test]
    fn with_size_overrides_dimensions() {
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let decoder = DjVuDecoder::new(page).unwrap().with_size(64, 48);
        let (w, h) = decoder.dimensions();
        assert_eq!((w, h), (64, 48));
    }

    #[test]
    fn read_rect_full_page_matches_read_image() {
        use image::ImageDecoderRect;
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let decoder = DjVuDecoder::new(page).unwrap().with_size(32, 24);
        let (w, h) = decoder.dimensions();
        let n = (w as usize) * (h as usize) * 4;

        let mut full = vec![0u8; n];
        // Use a separate decoder for read_image (consumes self)
        let decoder2 = DjVuDecoder::new(page).unwrap().with_size(32, 24);
        decoder2.read_image(&mut full).unwrap();

        let mut rect_buf = vec![0u8; n];
        let mut decoder3 = DjVuDecoder::new(page).unwrap().with_size(32, 24);
        decoder3
            .read_rect(0, 0, w, h, &mut rect_buf, w as usize * 4)
            .unwrap();

        assert_eq!(
            full, rect_buf,
            "full read_image and read_rect(full page) must match"
        );
    }

    #[test]
    fn read_rect_out_of_bounds_returns_error() {
        use image::ImageDecoderRect;
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let mut decoder = DjVuDecoder::new(page).unwrap().with_size(32, 24);
        let mut buf = vec![0u8; 32 * 24 * 4];
        // x + width > image width
        let result = decoder.read_rect(10, 0, 30, 24, &mut buf, 30 * 4);
        assert!(result.is_err(), "out-of-bounds read_rect must return error");
    }

    #[test]
    fn from_image_compat_error_for_image_error() {
        use image::error::ImageError;
        let err = ImageCompatError::Render(crate::djvu_render::RenderError::InvalidDimensions {
            width: 0,
            height: 0,
        });
        let image_err: ImageError = err.into();
        let s = image_err.to_string();
        assert!(s.contains("DjVu"), "ImageError must mention DjVu: {s}");
    }

    #[test]
    fn read_image_boxed_works() {
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let decoder = Box::new(DjVuDecoder::new(page).unwrap().with_size(16, 12));
        let n = (16 * 12 * 4) as usize;
        let mut buf = vec![0u8; n];
        decoder.read_image_boxed(&mut buf).unwrap();
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    fn read_rect_x_plus_width_overflow_returns_error() {
        use image::ImageDecoderRect;
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let mut decoder = DjVuDecoder::new(page).unwrap().with_size(32, 24);
        let mut buf = vec![0u8; 4];
        // x = u32::MAX, width = 1 → overflow
        let result = decoder.read_rect(u32::MAX, 0, 1, 1, &mut buf, 4);
        assert!(result.is_err(), "x+width overflow must return error");
    }

    #[test]
    fn read_rect_y_plus_height_overflow_returns_error() {
        use image::ImageDecoderRect;
        let doc = load_page("chicken.djvu");
        let page = doc.page(0).unwrap();
        let mut decoder = DjVuDecoder::new(page).unwrap().with_size(32, 24);
        let mut buf = vec![0u8; 4];
        // y = u32::MAX, height = 1 → overflow
        let result = decoder.read_rect(0, u32::MAX, 1, 1, &mut buf, 4);
        assert!(result.is_err(), "y+height overflow must return error");
    }
}
