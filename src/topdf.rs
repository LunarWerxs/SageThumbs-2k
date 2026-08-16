//! Combine images into one PDF — a hand-rolled minimal PDF embedding each image
//! as a baseline JPEG via the `/DCTDecode` filter (one image per page). Zero new
//! dependencies; the output was verified to load in the OS `Windows.Data.Pdf`
//! engine (the same one our thumbnailer uses).

use std::path::{Path, PathBuf};

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, RgbImage};
use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::UI::Shell::StrCmpLogicalW;

use crate::decode;
use crate::fsutil::rename_retrying;
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
///
/// Returns `(out_path, dropped)` — `dropped` is how many of `paths` were undecodable and so
/// silently excluded from the PDF (see [`combine_to_pdf_paged`]). Every current caller only
/// checks success/failure via `?`/`map_err` and never binds the `Ok` payload's exact shape, so
/// widening it from `PathBuf` to `(PathBuf, usize)` here needed no changes at those call sites.
pub fn combine_to_pdf(paths: &[String], out: &Path, quality: u8) -> Result<(PathBuf, usize)> {
    combine_to_pdf_paged(paths, out, quality, crate::settings::pdf_page())
}

/// A page's file name as a NUL-terminated UTF-16 buffer for [`StrCmpLogicalW`].
fn logical_key(p: &str) -> Vec<u16> {
    let fname = Path::new(p)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(p);
    fname.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Natural-sort `paths` by file name (page2 before page10), matching Explorer and
/// the CBZ combiner's page order (`verbs::fileops::combine_to_cbz`). Combine-to-PDF
/// used to keep raw click order while its CBZ sibling natural-sorted, so the same
/// "combine N images" action silently produced a different page order depending on
/// which output format you picked.
fn natural_sort_paths(paths: &[String]) -> Vec<String> {
    let mut keyed: Vec<(Vec<u16>, &String)> = paths.iter().map(|p| (logical_key(p), p)).collect();
    keyed.sort_by(|a, b| {
        unsafe { StrCmpLogicalW(PCWSTR(a.0.as_ptr()), PCWSTR(b.0.as_ptr())) }.cmp(&0)
    });
    keyed.into_iter().map(|(_, p)| p.clone()).collect()
}

/// [`combine_to_pdf`] with the layout passed in rather than read from settings -
/// the entry point for tests, which must not depend on whatever this machine's
/// registry happens to say.
///
/// Returns `(out_path, dropped)`: `dropped` counts how many of `paths` never made it into the
/// PDF (unreadable file, or a format `decode::decode_full`/the JPEG re-encode couldn't handle).
/// Silently excluding them used to be invisible to the caller — `ActionReport::applied(1, 1)`
/// on any `Ok(_)` claimed full success even when, say, 3 of 10 inputs were dropped — so the
/// count is threaded back out here for `verbs::actions` to surface as a note.
pub fn combine_to_pdf_paged(
    paths: &[String],
    out: &Path,
    quality: u8,
    page: PdfPage,
) -> Result<(PathBuf, usize)> {
    let paths = natural_sort_paths(paths);

    // Decode AND JPEG-encode every page inside the parallel worker, so only the
    // compressed bytes (not the decoded DynamicImage) survive past this call -
    // holding every full-fidelity DynamicImage until a later sequential encoding
    // pass would peak at N x decoded-image size on a hundreds-of-pages comic
    // combine. Per-worker COM init + the global magick cap are handled inside the
    // pool / decoder.
    let attempts: Vec<Option<(Vec<u8>, u32, u32)>> = crate::parallel::map(&paths, |_, p| {
        let img = read_capped(p)
            .ok()
            .and_then(|b| decode::decode_full(&b).ok())?;
        image_to_baseline_jpeg(&img, quality).ok()
    });
    // Undecodable inputs drop out (the `flatten` below), exactly as the old sequential
    // `filter_map` did — but now counted first, rather than silently discarded, so a partial
    // combine can be reported as partial instead of a bare, misleadingly-total success.
    let dropped = attempts.iter().filter(|a| a.is_none()).count();
    let pages: Vec<(Vec<u8>, u32, u32)> = attempts.into_iter().flatten().collect();
    if pages.is_empty() {
        return Err(Error::from(E_FAIL));
    }

    let mut pdf: Vec<u8> = Vec::new();
    let n = pages.len();
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

    for (i, (jpeg, w, h)) in pages.iter().enumerate() {
        let (w, h) = (*w, *h);
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
        pdf.extend_from_slice(jpeg); // raw JPEG bytes — never string-formatted
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
    // A transient Explorer/thumbnail-cache lock on `out` (Windows os error 5/32)
    // can make a fresh rename fail even though nothing is really wrong; retry past
    // it like every other atomic writer in the codebase instead of failing once.
    rename_retrying(&tmp, out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok((out.to_path_buf(), dropped))
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

    /// `combine_to_pdf_paged` used to swallow undecodable inputs with no count kept anywhere
    /// (a plain `.flatten()`), so a caller combining 10 files where 1 was garbage had no way to
    /// know it got a 9-page PDF instead of 10. This pins that the dropped count comes back
    /// accurately, alongside a real PDF built from the ones that DID decode.
    #[test]
    fn combine_to_pdf_paged_reports_how_many_inputs_it_had_to_drop() {
        let dir = std::env::temp_dir().join(format!("st2k_topdf_drop_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let good: Vec<String> = (0..2)
            .map(|i| {
                let p = dir.join(format!("good{i}.png"));
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
        // Not an image at all — `decode::decode_full` must reject it, exactly the
        // "undecodable input" case the finding describes.
        let garbage = dir.join("garbage.png");
        std::fs::write(&garbage, b"not a png").unwrap();

        let mut paths = good.clone();
        paths.push(garbage.to_str().unwrap().to_string());

        let out = dir.join("partial.pdf");
        let (out_path, dropped) = combine_to_pdf(&paths, &out, 85).expect("2 good pages remain");
        assert_eq!(dropped, 1, "exactly the one garbage input must be dropped");
        assert_eq!(out_path, out);

        // The PDF that DOES get built must contain only the pages that decoded — 2, not 3 (and
        // not silently empty either).
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(
            bytes.windows(9).filter(|w| *w == b"DCTDecode").count(),
            2,
            "only the 2 decodable pages should have been embedded"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pages must land in natural (Explorer-logical) filename order, matching the
    /// CBZ combiner, even when the caller's click order was the opposite. Two
    /// distinctly-sized pages, submitted in reverse-of-natural order, prove it by
    /// checking which page's MediaBox appears first in the output bytes.
    #[test]
    fn combine_to_pdf_orders_pages_by_natural_filename_not_click_order() {
        let dir = std::env::temp_dir().join(format!("st2k_topdf_sort_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let make = |name: &str, w: u32, h: u32| -> String {
            let p = dir.join(name);
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                w,
                h,
                image::Rgb([10, 20, 30]),
            ))
            .save(&p)
            .unwrap();
            p.to_str().unwrap().to_string()
        };
        // "b_page.png" naturally sorts AFTER "a_page.png"; pass them in the
        // opposite (click) order so a naive "keep input order" implementation
        // would put the 80-wide page first.
        let b = make("b_page.png", 80, 8);
        let a = make("a_page.png", 50, 8);
        let paths = vec![b, a];

        let out = dir.join("sorted.pdf");
        combine_to_pdf(&paths, &out, 85).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let text = String::from_utf8_lossy(&bytes);

        let first_a = text.find("/MediaBox [0 0 50 8]");
        let first_b = text.find("/MediaBox [0 0 80 8]");
        let (first_a, first_b) = (
            first_a.expect("a_page's page object must be present"),
            first_b.expect("b_page's page object must be present"),
        );
        assert!(
            first_a < first_b,
            "a_page.png (naturally first) must precede b_page.png in the PDF even though it was submitted second"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
