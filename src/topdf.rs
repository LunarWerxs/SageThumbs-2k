//! Combine images into one PDF — a hand-rolled minimal PDF embedding each image
//! as a baseline JPEG via the `/DCTDecode` filter (one image per page). Zero new
//! dependencies; the output was verified to load in the OS `Windows.Data.Pdf`
//! engine (the same one our thumbnailer uses).

use std::path::{Path, PathBuf};

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, RgbImage};
use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;

use crate::decode;
use crate::verbs::{flatten_onto_white, read_capped};

/// Decode → flatten onto white → baseline-JPEG bytes (3-component DeviceRGB).
/// `.to_rgb8()` (NOT `encode_image` on a `DynamicImage`, whose view pixel is
/// RGBA in image 0.25) guarantees a JPEG-valid 3-channel stream.
fn image_to_baseline_jpeg(img: &DynamicImage, quality: u8) -> Result<(Vec<u8>, u32, u32)> {
    let rgb: RgbImage = flatten_onto_white(img).to_rgb8();
    let (w, h) = (rgb.width(), rgb.height());
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, quality)
        .encode_image(&rgb)
        .map_err(|_| Error::from(E_FAIL))?;
    Ok((buf, w, h))
}

/// How each image is placed on its page.
///
/// PDF's unit is the point (1/72 inch) and our images go in at 72 dpi, so one
/// image pixel is one point and the arithmetic below needs no conversion.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum PdfPage {
    /// Page sized exactly to the image, image filling it. The original behaviour
    /// and still the default: it is what you want for scans and comics, where a
    /// border is just wasted paper.
    #[default]
    Tight,
    /// Page sized to the image plus a uniform margin, in points.
    Margin(f64),
    /// A fixed sheet. The image is centred and scaled DOWN to fit inside the
    /// margins, never UP - a small image keeps its own size instead of being
    /// blown up and blurred, the same rule Resize follows.
    Sheet { w: f64, h: f64, margin: f64 },
}

/// A4 and US Letter in points, for [`PdfPage::Sheet`].
pub const A4_PT: (f64, f64) = (595.276, 841.89);
pub const LETTER_PT: (f64, f64) = (612.0, 792.0);

impl PdfPage {
    /// `(page_w, page_h, draw_x, draw_y, draw_w, draw_h)` for a `w` x `h` image.
    fn place(self, w: f64, h: f64) -> (f64, f64, f64, f64, f64, f64) {
        match self {
            PdfPage::Tight => (w, h, 0.0, 0.0, w, h),
            PdfPage::Margin(m) => {
                let m = m.max(0.0);
                (w + 2.0 * m, h + 2.0 * m, m, m, w, h)
            }
            PdfPage::Sheet {
                w: sw,
                h: sh,
                margin,
            } => {
                let m = margin.max(0.0);
                let (aw, ah) = ((sw - 2.0 * m).max(1.0), (sh - 2.0 * m).max(1.0));
                let scale = (aw / w).min(ah / h).min(1.0);
                let (dw, dh) = (w * scale, h * scale);
                (sw, sh, (sw - dw) / 2.0, (sh - dh) / 2.0, dw, dh)
            }
        }
    }
}

/// Combine the decodable images in `paths` into one PDF at `out`, one per page,
/// laid out per the user's saved page setting. Atomic temp+rename.
pub fn combine_to_pdf(paths: &[String], out: &Path, quality: u8) -> Result<PathBuf> {
    combine_to_pdf_paged(paths, out, quality, crate::settings::pdf_page())
}

/// [`combine_to_pdf`] with the layout passed in rather than read from settings -
/// the entry point for tests, which must not depend on whatever this machine's
/// registry happens to say.
pub fn combine_to_pdf_paged(
    paths: &[String],
    out: &Path,
    quality: u8,
    page: PdfPage,
) -> Result<PathBuf> {
    // Decode every page in parallel (the heavy, parallelizable cost), keeping input
    // order so pages stay in order; undecodable inputs drop out (the `flatten`),
    // exactly as the old sequential `filter_map` did. Per-worker COM init + the
    // global magick cap are handled inside the pool / decoder.
    let imgs: Vec<DynamicImage> = crate::parallel::map(paths, |_, p| {
        read_capped(p)
            .ok()
            .and_then(|b| decode::decode_full(&b).ok())
    })
    .into_iter()
    .flatten()
    .collect();
    if imgs.is_empty() {
        return Err(Error::from(E_FAIL));
    }

    let mut pdf: Vec<u8> = Vec::new();
    let n = imgs.len();
    let total = 2 + n * 3; // 1=Catalog, 2=Pages, then page/content/image per image
    let mut off = vec![0usize; total + 1];
    macro_rules! txt {
        ($($a:tt)*) => { pdf.extend_from_slice(format!($($a)*).as_bytes()); };
    }

    pdf.extend_from_slice(b"%PDF-1.7\n");
    pdf.extend_from_slice(&[b'%', 0xE2, 0xE3, 0xCF, 0xD3, b'\n']); // binary marker

    off[1] = pdf.len();
    txt!("1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    off[2] = pdf.len();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + i * 3)).collect();
    txt!(
        "2 0 obj\n<< /Type /Pages /Count {} /Kids [{}] >>\nendobj\n",
        n,
        kids.join(" ")
    );

    for (i, img) in imgs.iter().enumerate() {
        let (jpeg, w, h) = image_to_baseline_jpeg(img, quality)?;
        let (pw, ph, dx, dy, dw, dh) = page.place(w as f64, h as f64);
        let (pg, ct, im) = (3 + i * 3, 4 + i * 3, 5 + i * 3);

        off[pg] = pdf.len();
        txt!("{pg} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw} {ph}] /Resources << /XObject << /Im0 {im} 0 R >> >> /Contents {ct} 0 R >>\nendobj\n");

        // The `cm` matrix is scale-x, 0, 0, scale-y, translate-x, translate-y.
        let content = format!("q\n{dw} 0 0 {dh} {dx} {dy} cm\n/Im0 Do\nQ\n");
        off[ct] = pdf.len();
        txt!("{ct} 0 obj\n<< /Length {} >>\nstream\n", content.len());
        pdf.extend_from_slice(content.as_bytes());
        pdf.extend_from_slice(b"endstream\nendobj\n");

        off[im] = pdf.len();
        txt!("{im} 0 obj\n<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n", jpeg.len());
        pdf.extend_from_slice(&jpeg); // raw JPEG bytes — never string-formatted
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
    }

    let xref = pdf.len();
    txt!("xref\n0 {}\n", total + 1);
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for &o in &off[1..=total] {
        txt!("{:010} 00000 n \n", o);
    }
    txt!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        total + 1,
        xref
    );

    let tmp: PathBuf = {
        let mut s = out.to_path_buf().into_os_string();
        s.push(".st2ktmp");
        PathBuf::from(s)
    };
    std::fs::write(&tmp, &pdf).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    std::fs::rename(&tmp, out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(out.to_path_buf())
}

#[cfg(test)]
mod tests {
    #[test]
    fn tight_pages_are_exactly_the_image() {
        let (pw, ph, dx, dy, dw, dh) = super::PdfPage::Tight.place(800.0, 600.0);
        assert_eq!(
            (pw, ph, dx, dy, dw, dh),
            (800.0, 600.0, 0.0, 0.0, 800.0, 600.0)
        );
    }

    #[test]
    fn a_margin_grows_the_page_and_insets_the_image() {
        let (pw, ph, dx, dy, dw, dh) = super::PdfPage::Margin(36.0).place(800.0, 600.0);
        assert_eq!((pw, ph), (872.0, 672.0));
        assert_eq!((dx, dy), (36.0, 36.0));
        assert_eq!(
            (dw, dh),
            (800.0, 600.0),
            "the image itself must not be resized"
        );
    }

    /// A sheet scales an oversized image down to fit the printable area, and
    /// centres it. This is the case that would silently crop if the maths were
    /// wrong, so the assertion checks the image stays fully inside the margins.
    #[test]
    fn a_sheet_shrinks_an_oversized_image_and_centres_it() {
        let page = super::PdfPage::Sheet {
            w: super::A4_PT.0,
            h: super::A4_PT.1,
            margin: 36.0,
        };
        let (pw, ph, dx, dy, dw, dh) = page.place(4000.0, 3000.0);
        assert_eq!((pw, ph), super::A4_PT);
        assert!(
            dw <= pw - 72.0 + 0.01 && dh <= ph - 72.0 + 0.01,
            "{dw}x{dh} overflows the margins"
        );
        assert!(
            dx >= 36.0 - 0.01 && dy >= 36.0 - 0.01,
            "drawn outside the margin at {dx},{dy}"
        );
        assert!(
            ((dw / dh) - (4000.0 / 3000.0)).abs() < 1e-9,
            "aspect ratio changed"
        );
    }

    /// Never upscale: a business-card-sized image on A4 keeps its own size and
    /// just sits in the middle, rather than being blown up to fill the sheet.
    #[test]
    fn a_sheet_never_enlarges_a_small_image() {
        let page = super::PdfPage::Sheet {
            w: super::A4_PT.0,
            h: super::A4_PT.1,
            margin: 36.0,
        };
        let (_, _, dx, dy, dw, dh) = page.place(100.0, 50.0);
        assert_eq!((dw, dh), (100.0, 50.0));
        assert!(dx > 100.0 && dy > 100.0, "not centred: {dx},{dy}");
    }

    use super::*;

    #[test]
    fn combines_two_images_into_a_renderable_pdf() {
        let dir = std::env::temp_dir().join(format!("st2k_topdf_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<String> = (0..2)
            .map(|i| {
                let p = dir.join(format!("p{i}.png"));
                image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                    30,
                    20,
                    image::Rgb([i as u8 * 100, 60, 60]),
                ))
                .save(&p)
                .unwrap();
                p.to_str().unwrap().to_string()
            })
            .collect();

        let out = dir.join("c.pdf");
        combine_to_pdf(&paths, &out, 85).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"%PDF-1.7"), "must be a PDF");
        assert!(
            bytes.windows(9).any(|w| w == b"DCTDecode"),
            "must embed JPEG via DCTDecode"
        );
        // End-to-end: the OS PDF engine (our own decode path) renders it.
        assert!(
            decode::decode_full(&bytes).is_ok(),
            "combined PDF should render via Windows.Data.Pdf"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
