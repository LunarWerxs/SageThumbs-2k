//! Integration tests for `image::ImageDecoder` implementation.
//!
//! These tests require the `image` feature flag to be enabled.

#[cfg(feature = "image")]
mod image_tests {
    use std::path::PathBuf;

    use djvu_rs::{DjVuDocument, image_compat::DjVuDecoder};
    use image::{ImageDecoder, ImageDecoderRect};

    fn chicken_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets/chicken.djvu")
    }

    fn chicken_doc() -> DjVuDocument {
        let data = std::fs::read(chicken_path()).expect("chicken.djvu must exist");
        DjVuDocument::parse(&data).expect("parse must succeed")
    }

    #[test]
    fn decoder_dimensions_match_page_info() {
        let doc = chicken_doc();
        let page = doc.page(0).expect("page 0 must exist");
        let decoder = DjVuDecoder::new(page).expect("decoder should construct");

        let (w, h) = decoder.dimensions();
        assert_eq!(w, 181);
        assert_eq!(h, 240);
    }

    #[test]
    fn decoder_color_type_is_rgba8() {
        let doc = chicken_doc();
        let page = doc.page(0).expect("page 0 must exist");
        let decoder = DjVuDecoder::new(page).expect("decoder should construct");

        assert_eq!(decoder.color_type(), image::ColorType::Rgba8);
    }

    #[test]
    fn decoder_total_bytes() {
        let doc = chicken_doc();
        let page = doc.page(0).expect("page 0 must exist");
        let decoder = DjVuDecoder::new(page).expect("decoder should construct");

        let (w, h) = decoder.dimensions();
        let expected = u64::from(w) * u64::from(h) * 4; // 4 bytes per RGBA pixel
        assert_eq!(decoder.total_bytes(), expected);
    }

    #[test]
    fn read_image_produces_correct_pixel_count() {
        let doc = chicken_doc();
        let page = doc.page(0).expect("page 0 must exist");
        let decoder = DjVuDecoder::new(page).expect("decoder should construct");

        let total = decoder.total_bytes() as usize;
        let mut buf = vec![0u8; total];
        let decoder2 = DjVuDecoder::new(doc.page(0).expect("page 0")).expect("decoder2");
        decoder2
            .read_image(&mut buf)
            .expect("read_image should succeed");

        // Verify at least some non-zero pixels (not a blank image)
        assert!(
            buf.iter().any(|&b| b != 0),
            "image should have non-zero pixels"
        );
    }

    #[test]
    fn decoder_with_size_overrides_dimensions() {
        let doc = chicken_doc();
        let page = doc.page(0).expect("page 0 must exist");
        let decoder = DjVuDecoder::new(page).expect("decoder").with_size(90, 120);

        let (w, h) = decoder.dimensions();
        assert_eq!(w, 90);
        assert_eq!(h, 120);
    }

    #[test]
    fn read_image_pixel_match_with_render_pixmap() {
        use djvu_rs::djvu_render::RenderOptions;

        let data = std::fs::read(chicken_path()).expect("chicken.djvu must exist");
        let doc1 = DjVuDocument::parse(&data).expect("parse");
        let doc2 = DjVuDocument::parse(&data).expect("parse");

        let page1 = doc1.page(0).expect("page 0");
        let page2 = doc2.page(0).expect("page 0");

        // Render via render_into
        let opts = RenderOptions {
            width: page1.width() as u32,
            height: page1.height() as u32,
            ..RenderOptions::default()
        };
        let expected_bytes = (opts.width * opts.height * 4) as usize;
        let mut direct_buf = vec![0u8; expected_bytes];
        page1
            .render_into(&opts, &mut direct_buf)
            .expect("render_into");

        // Render via ImageDecoder
        let decoder = DjVuDecoder::new(page2).expect("decoder");
        let total = decoder.total_bytes() as usize;
        let mut decoder_buf = vec![0u8; total];
        decoder.read_image(&mut decoder_buf).expect("read_image");

        assert_eq!(
            direct_buf, decoder_buf,
            "ImageDecoder output must match direct render_into output"
        );
    }

    // ---- ImageDecoderRect tests ---------------------------------------------

    #[test]
    fn read_rect_matches_full_image_sub_region() {
        let doc = chicken_doc();
        let page = doc.page(0).expect("page 0 must exist");

        // Render full image via read_image
        let mut full_decoder = DjVuDecoder::new(page).expect("decoder");
        let total = full_decoder.total_bytes() as usize;
        let mut full_buf = vec![0u8; total];
        {
            let d = DjVuDecoder::new(doc.page(0).expect("page 0")).expect("decoder");
            d.read_image(&mut full_buf).expect("read_image");
        }

        // Read a sub-rectangle: top-left 40×30 region starting at (20, 10)
        let rx = 20u32;
        let ry = 10u32;
        let rw = 40u32;
        let rh = 30u32;
        let bytes_per_pixel = 4usize; // Rgba8
        let mut rect_buf = vec![0u8; (rw * rh) as usize * bytes_per_pixel];
        let row_pitch = rw as usize * bytes_per_pixel;
        full_decoder
            .read_rect(rx, ry, rw, rh, &mut rect_buf, row_pitch)
            .expect("read_rect should succeed");

        // Extract the same region from the full image manually
        let full_row_bytes = 181usize * bytes_per_pixel; // chicken.djvu is 181px wide
        let mut expected = vec![0u8; (rw * rh) as usize * bytes_per_pixel];
        for row in 0..rh as usize {
            let src_start = (ry as usize + row) * full_row_bytes + rx as usize * bytes_per_pixel;
            let dst_start = row * rw as usize * bytes_per_pixel;
            expected[dst_start..dst_start + rw as usize * bytes_per_pixel]
                .copy_from_slice(&full_buf[src_start..src_start + rw as usize * bytes_per_pixel]);
        }

        assert_eq!(
            rect_buf, expected,
            "read_rect must extract the correct sub-region"
        );
    }

    #[test]
    fn read_rect_out_of_bounds_returns_error() {
        let doc = chicken_doc();
        let page = doc.page(0).expect("page 0 must exist");
        let mut decoder = DjVuDecoder::new(page).expect("decoder");

        // Request a rect that goes beyond the page width (181px)
        let mut buf = vec![0u8; 100 * 100 * 4];
        let result = decoder.read_rect(150, 0, 100, 100, &mut buf, 100 * 4);
        assert!(
            result.is_err(),
            "read_rect out of bounds should return an error"
        );
    }
}
