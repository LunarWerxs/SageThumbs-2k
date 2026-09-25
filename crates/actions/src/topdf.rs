//! Combine images into one PDF — a hand-rolled minimal PDF embedding each image
//! as a baseline JPEG via the `/DCTDecode` filter (one image per page). Zero new
//! dependencies; the output was verified to load in the OS `Windows.Data.Pdf`
//! engine (the same one our thumbnailer uses).
//!
//! A searchable combine ([`combine_to_pdf_searchable`]) also OCRs each page with the in-box
//! Windows engine and lays the words over the picture as invisible text (see `textlayer`).

mod textlayer;

use st2k_codecs::decode::read_full_fidelity_capped;
use st2k_codecs::ocr::WordBox;
use std::io::Write;
use std::path::Path;

use image::codecs::jpeg::JpegEncoder;
use image::RgbImage;
use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;

use crate::verbs::{
    flatten_onto_white, partition, refusal, write_atomic, Combined, OmitCause, Omitted, OnOmit,
};
use st2k_base::settings::PdfPage;
use st2k_codecs::decode;

/// The page flattened onto white → baseline-JPEG bytes (3-component DeviceRGB).
/// An `RgbImage` from `.to_rgb8()` (NOT `encode_image` on a `DynamicImage`, whose view
/// pixel is RGBA in image 0.25) guarantees a JPEG-valid 3-channel stream.
fn rgb_to_baseline_jpeg(rgb: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, quality)
        .encode_image(rgb)
        .map_err(|e| Error::new(E_FAIL, format!("jpeg encode for pdf: {e}")))?;
    Ok(buf)
}

/// One decoded page: baseline JPEG bytes, pixel width and height, and the recognised words
/// (`None` for a plain combine, or when recognition failed on this page).
struct Page {
    jpeg: Vec<u8>,
    w: u32,
    h: u32,
    words: Option<Vec<Vec<WordBox>>>,
}

/// A writer that counts the bytes passed through it, so the xref table's object
/// offsets are known while the PDF is streamed rather than read off a `Vec`'s length.
struct Counted<W: Write> {
    inner: W,
    pos: usize,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos = self.pos.saturating_add(n);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Stream the PDF for `pages` into `w`. Consumes `pages`, so each page's compressed
/// bytes are freed as soon as they have been written: the decode stage kept only JPEG
/// bytes to bound memory, and holding a second full copy of all of them while the
/// document is assembled undid that on a hundreds-of-pages combine.
///
/// `searchable` appends the shared glyphless font after the pages and names it in every
/// page's resources, so each page's recognised words can be drawn as invisible text.
fn write_pdf<W: Write>(
    w: &mut Counted<W>,
    pages: Vec<Page>,
    page: PdfPage,
    searchable: bool,
) -> std::io::Result<()> {
    let n = pages.len();
    let font = searchable.then_some(3 + n * 3);
    // 1=Catalog, 2=Pages, then page/content/image per image, then the font's objects.
    let total = 2 + n * 3 + font.map_or(0, |_| textlayer::FONT_OBJS);
    let mut off = vec![0usize; total + 1];

    w.write_all(b"%PDF-1.7\n")?;
    w.write_all(&[b'%', 0xE2, 0xE3, 0xCF, 0xD3, b'\n'])?; // binary marker

    mark(&mut off, 1, w.pos);
    write!(w, "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n")?;

    mark(&mut off, 2, w.pos);
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + i * 3)).collect();
    write!(
        w,
        "2 0 obj\n<< /Type /Pages /Count {} /Kids [{}] >>\nendobj\n",
        n,
        kids.join(" ")
    )?;

    for (i, p) in pages.into_iter().enumerate() {
        write_page(w, &mut off, i, &p, page, font)?;
        // `p` drops here: one page's compressed bytes at a time.
    }
    if let Some(first) = font {
        textlayer::write_font(w, &mut off, first)?;
    }

    write_xref(w, &off, total)
}

/// Record object `i`'s byte offset for the xref table.
fn mark(off: &mut [usize], i: usize, pos: usize) {
    if let Some(o) = off.get_mut(i) {
        *o = pos;
    }
}

/// Page `i`'s three objects: the Page, its content stream (one `cm` + `Do`, then the
/// invisible text when `font` names the text-layer font), and the JPEG image XObject.
fn write_page<W: Write>(
    w: &mut Counted<W>,
    off: &mut [usize],
    i: usize,
    p: &Page,
    page: PdfPage,
    font: Option<usize>,
) -> std::io::Result<()> {
    let (jpeg, iw, ih) = (&p.jpeg, p.w, p.h);
    let (pw, ph, dx, dy, dw, dh) = place(page, iw as f64, ih as f64);
    let (pg, ct, im) = (3 + i * 3, 4 + i * 3, 5 + i * 3);
    let font_res = font.map_or(String::new(), |f| format!(" /Font << /F1 {f} 0 R >>"));

    mark(off, pg, w.pos);
    write!(w, "{pg} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw} {ph}] /Resources << /XObject << /Im0 {im} 0 R >>{font_res} >> /Contents {ct} 0 R >>\nendobj\n")?;

    // The `cm` matrix is scale-x, 0, 0, scale-y, translate-x, translate-y.
    let mut content = format!("q\n{dw} 0 0 {dh} {dx} {dy} cm\n/Im0 Do\nQ\n");
    if let (Some(_), Some(lines)) = (font, &p.words) {
        let (iw, ih) = (iw as f64, ih as f64);
        let frame = textlayer::Frame {
            dx,
            dy,
            dw,
            dh,
            iw,
            ih,
        };
        content.push_str(&textlayer::text_ops(lines, frame));
    }
    mark(off, ct, w.pos);
    write!(w, "{ct} 0 obj\n<< /Length {} >>\nstream\n", content.len())?;
    w.write_all(content.as_bytes())?;
    w.write_all(b"endstream\nendobj\n")?;

    mark(off, im, w.pos);
    write!(w, "{im} 0 obj\n<< /Type /XObject /Subtype /Image /Width {iw} /Height {ih} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n", jpeg.len())?;
    w.write_all(jpeg)?; // raw JPEG bytes — never string-formatted
    w.write_all(b"\nendstream\nendobj\n")
}

/// The xref table over `off` and the trailer.
fn write_xref<W: Write>(w: &mut Counted<W>, off: &[usize], total: usize) -> std::io::Result<()> {
    let xref = w.pos;
    write!(w, "xref\n0 {}\n", total + 1)?;
    w.write_all(b"0000000000 65535 f \n")?;
    for &o in off.iter().skip(1) {
        writeln!(w, "{:010} 00000 n ", o)?;
    }
    write!(
        w,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        total + 1,
        xref
    )
}

/// `(page_w, page_h, draw_x, draw_y, draw_w, draw_h)` for a `w` x `h` image on `page`.
fn place(page: PdfPage, w: f64, h: f64) -> (f64, f64, f64, f64, f64, f64) {
    match page {
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

/// Combine the decodable images in `paths` into one PDF at `out`, one per page,
/// laid out per the user's saved page setting. Atomic temp+rename.
///
/// Returns the [`Combined`] result: the output, how many inputs made it in, and every input
/// that was left out with its cause (see [`combine_to_pdf_paged`]).
pub fn combine_to_pdf(
    paths: &[String],
    out: &Path,
    quality: u8,
    on_omit: OnOmit,
) -> Result<Combined> {
    combine_to_pdf_paged(
        paths,
        out,
        quality,
        st2k_base::settings::pdf_page(),
        on_omit,
    )
}

/// A path's file name as a NUL-terminated UTF-16 buffer — the pre-encoded sort key for the
/// natural (logical) compares here and in the CBZ combiner (`verbs::fileops`), built once
/// per element so an O(n log n) sort doesn't re-encode UTF-16 on every comparison.
pub(crate) fn file_name_key(p: &str) -> Vec<u16> {
    let fname = Path::new(p)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(p);
    st2k_base::host::wide(fname)
}

/// Natural-sort `paths` by file name (page2 before page10), matching Explorer and
/// the CBZ combiner's page order (`verbs::fileops::combine_to_cbz`). Combine-to-PDF
/// used to keep raw click order while its CBZ sibling natural-sorted, so the same
/// "combine N images" action silently produced a different page order depending on
/// which output format you picked.
fn natural_sort_paths(paths: &[String]) -> Vec<String> {
    let mut keyed: Vec<(Vec<u16>, &String)> = paths.iter().map(|p| (file_name_key(p), p)).collect();
    keyed.sort_by(|a, b| st2k_codecs::container::select::cmp_logical_keys(&a.0, &b.0));
    keyed.into_iter().map(|(_, p)| p.clone()).collect()
}

/// Decode one input to a PDF page, or say exactly why it could not be. Each failure is also
/// logged with its path (`read_full_fidelity_capped` logs its own), so a dropped page can be
/// traced in the doctor log.
fn decode_page(p: &str, quality: u8, searchable: bool) -> std::result::Result<Page, Omitted> {
    let bytes =
        read_full_fidelity_capped(p).map_err(|e| Omitted::new(p, OmitCause::Unreadable, e))?;
    let img = decode::decode_full_for_output(&bytes).map_err(|e| {
        st2k_base::safety::log(&format!("pdf: cannot decode {p}: {e}"));
        Omitted::new(p, OmitCause::Undecodable, e)
    })?;
    drop(bytes);
    let rgb: RgbImage = flatten_onto_white(&img).to_rgb8();
    drop(img);
    let words = if searchable {
        recognize_page(p, &rgb)
    } else {
        None
    };
    let jpeg = rgb_to_baseline_jpeg(&rgb, quality).map_err(|e| {
        st2k_base::safety::log(&format!("pdf: cannot encode {p}: {e}"));
        Omitted::new(p, OmitCause::Unencodable, e)
    })?;
    Ok(Page {
        jpeg,
        w: rgb.width(),
        h: rgb.height(),
        words,
    })
}

/// OCR the exact pixels the page embeds (handed over as a PNG, so EXIF orientation or a
/// container quirk can never put the words somewhere other than the picture), or `None` when
/// recognition fails: the page still goes in, just without text, and the failure is logged.
fn recognize_page(p: &str, rgb: &RgbImage) -> Option<Vec<Vec<WordBox>>> {
    let mut png = Vec::new();
    if let Err(e) = rgb.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png) {
        st2k_base::safety::log(&format!("pdf: cannot prepare {p} for OCR: {e}"));
        return None;
    }
    match st2k_codecs::ocr::recognize_word_lines(png) {
        Ok(lines) => Some(lines),
        Err(e) => {
            st2k_base::safety::log(&format!("pdf: OCR failed on {p}: {e}"));
            None
        }
    }
}

/// The `E_FAIL` a combine aborts with: `headline` plus one line per left-out input. Shared by
/// the "nothing usable" and `OnOmit::Fail` refusals of the PDF and CBZ combiners, so the
/// refusal text is assembled one way. Consumes `headline` (the refusal copies it into the
/// message).
pub(crate) fn refuse(headline: String, omitted: &[Omitted]) -> Error {
    Error::new(E_FAIL, refusal(&headline, omitted))
}

/// [`combine_to_pdf`] with the layout passed in rather than read from settings -
/// the entry point for tests, which must not depend on whatever this machine's
/// registry happens to say.
///
/// Returns the [`Combined`] result: `omitted` names every input that never made it into the
/// PDF (unreadable file, or a format `decode::decode_full_for_output`, including its refusal of a
/// stand-in preview (issue #41), or the JPEG re-encode couldn't handle)
/// with its cause. Silently excluding them used to be invisible to the caller, and a bare count
/// was invisible past the Explorer verb (2026-09-05 audit, F31), so the list is threaded back
/// out here for every front end to surface. `OnOmit::Fail` writes nothing when the list would
/// be non-empty.
///
/// Refuses an `out` that is one of `paths` before reading anything (2026-09-05 audit, F30):
/// the write replaces the destination, so an alias would destroy a source.
pub fn combine_to_pdf_paged(
    paths: &[String],
    out: &Path,
    quality: u8,
    page: PdfPage,
    on_omit: OnOmit,
) -> Result<Combined> {
    combine(paths, out, quality, page, on_omit, false)
}

/// [`combine_to_pdf`], plus an invisible OCR text layer on every page, so the PDF can be
/// searched, selected and copied in any viewer while it still looks exactly like the images.
/// Recognition uses the in-box Windows engine (`Windows.Media.Ocr`, the user's profile
/// languages). A page it cannot read goes in without text; if it could read none of them
/// (typically no OCR language installed) the call fails and writes nothing, since the
/// caller asked for searchable output and would otherwise get a silently plain PDF.
pub fn combine_to_pdf_searchable(
    paths: &[String],
    out: &Path,
    quality: u8,
    on_omit: OnOmit,
) -> Result<Combined> {
    combine(
        paths,
        out,
        quality,
        st2k_base::settings::pdf_page(),
        on_omit,
        true,
    )
}

/// The shared body of [`combine_to_pdf_paged`] and [`combine_to_pdf_searchable`].
fn combine(
    paths: &[String],
    out: &Path,
    quality: u8,
    page: PdfPage,
    on_omit: OnOmit,
    searchable: bool,
) -> Result<Combined> {
    if let Some(alias) = st2k_base::fsutil::aliased_input(out, paths.iter().map(String::as_str)) {
        return Err(Error::new(
            E_FAIL,
            format!(
                "pdf: output {} is the same file as input {alias}; refusing to overwrite a source",
                out.display()
            ),
        ));
    }
    let paths = natural_sort_paths(paths);

    // Decode AND JPEG-encode every page inside the parallel worker, so only the
    // compressed bytes (not the decoded DynamicImage) survive past this call -
    // holding every full-fidelity DynamicImage until a later sequential encoding
    // pass would peak at N x decoded-image size on a hundreds-of-pages comic
    // combine. Per-worker COM init + the global magick cap are handled inside the
    // pool / decoder.
    let attempts = st2k_base::parallel::map(&paths, |_, p| decode_page(p, quality, searchable));
    let (pages, omitted) = partition(attempts);
    if pages.is_empty() {
        let headline = format!("pdf: none of the {} inputs could be decoded", paths.len());
        return Err(refuse(headline, &omitted));
    }
    if searchable && pages.iter().all(|p| p.words.is_none()) {
        return Err(Error::new(
            E_FAIL,
            "pdf: text recognition failed on every page (is a Windows OCR language installed?)",
        ));
    }
    if on_omit == OnOmit::Fail && !omitted.is_empty() {
        let headline = format!(
            "pdf: refusing to write a partial document (strict): {} of {} inputs cannot be used",
            omitted.len(),
            paths.len()
        );
        return Err(refuse(headline, &omitted));
    }
    let used = pages.len();
    // Pages OCR could not read still go in, just unsearchable: counted so the caller says so.
    let untexted = if searchable {
        pages.iter().filter(|p| p.words.is_none()).count()
    } else {
        0
    };

    // Streamed straight into the temp file through a BufWriter (no second in-memory copy
    // of every page), then renamed into place by the shared atomic writer, which owns
    // the temp naming, the on-error cleanup and the transient-lock rename retry.
    write_atomic(out, |tmp| {
        let file = std::fs::File::create(tmp)
            .map_err(|e| Error::new(E_FAIL, format!("create {}: {e}", tmp.display())))?;
        let mut w = Counted {
            inner: std::io::BufWriter::new(file),
            pos: 0,
        };
        write_pdf(&mut w, pages, page, searchable)
            .map_err(|e| Error::new(E_FAIL, format!("write {}: {e}", tmp.display())))?;
        // Explicit flush: BufWriter::drop discards flush errors, and a disk-full on the
        // final block must fail the write rather than rename a truncated PDF into place.
        w.flush()
            .map_err(|e| Error::new(E_FAIL, format!("flush {}: {e}", tmp.display())))
    })?;
    Ok(Combined {
        output: out.to_path_buf(),
        used,
        omitted,
        untexted,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn tight_pages_are_exactly_the_image() {
        let (pw, ph, dx, dy, dw, dh) = super::place(super::PdfPage::Tight, 800.0, 600.0);
        assert_eq!(
            (pw, ph, dx, dy, dw, dh),
            (800.0, 600.0, 0.0, 0.0, 800.0, 600.0)
        );
    }

    #[test]
    fn a_margin_grows_the_page_and_insets_the_image() {
        let (pw, ph, dx, dy, dw, dh) = super::place(super::PdfPage::Margin(36.0), 800.0, 600.0);
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
            w: st2k_base::settings::A4_PT.0,
            h: st2k_base::settings::A4_PT.1,
            margin: 36.0,
        };
        let (pw, ph, dx, dy, dw, dh) = super::place(page, 4000.0, 3000.0);
        assert_eq!((pw, ph), st2k_base::settings::A4_PT);
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
            w: st2k_base::settings::A4_PT.0,
            h: st2k_base::settings::A4_PT.1,
            margin: 36.0,
        };
        let (_, _, dx, dy, dw, dh) = super::place(page, 100.0, 50.0);
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
        combine_to_pdf(&paths, &out, 85, OnOmit::Report).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"%PDF-1.7"), "must be a PDF");
        assert!(
            bytes.windows(9).any(|w| w == b"DCTDecode"),
            "must embed JPEG via DCTDecode"
        );
        // End-to-end: the OS PDF engine (our own decode path) renders it.
        //
        // Retried, because this assertion is the one thing here that depends on getting CPU.
        // `pdf::render_page_counted` hands the work to a dedicated MTA thread and gives up
        // after a 30 s WALL CLOCK budget (`PDF_TIMEOUT`), so on a loaded machine the failure
        // mode is "the OS never got scheduled", not "the PDF is wrong". It went red exactly
        // once in CI, on a run where the lib suite took 442 s with the fuzzer saturating a
        // 4-core runner, and passed on an immediate re-run of the identical commit.
        //
        // The retry does not weaken WHAT is asserted, only how many chances the OS gets to
        // answer: a genuinely broken PDF fails all three attempts in milliseconds. It is the
        // same "confirm on a calm retry before crying regression" rule `regression.ps1`
        // already applies to the corpus sweep, and the sibling this codebase already paid
        // for is `video::tests::bounded_worker_returns_on_time_and_records_the_strand`
        // (the watchdog test it grew out of), whose first fix raced the watchdog thread
        // being scheduled at all.
        let rendered = (0..3).any(|attempt| {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            decode::decode_full_for_output(&bytes).is_ok()
        });
        assert!(
            rendered,
            "combined PDF should render via Windows.Data.Pdf (three attempts)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A text layer must not cost the page its picture or break the file: the glyphless font,
    /// the ToUnicode CMap and the `3 Tr` words ride along in a PDF the OS engine still
    /// renders, every object sits where the xref says (a miscounted font object would send a
    /// strict viewer to the wrong bytes), and the word is the UTF-16 the identity CMap maps
    /// back to text. Fixed word boxes, so no OCR language needs to be installed.
    #[test]
    fn a_text_layered_pdf_renders_and_its_xref_is_exact() {
        let rgb = image::RgbImage::from_pixel(60, 40, image::Rgb([200, 200, 200]));
        let words = vec![vec![WordBox {
            text: "Hello".into(),
            x: 5.0,
            y: 10.0,
            w: 40.0,
            h: 12.0,
        }]];
        let page = Page {
            jpeg: rgb_to_baseline_jpeg(&rgb, 85).unwrap(),
            w: 60,
            h: 40,
            words: Some(words),
        };
        let mut w = Counted {
            inner: Vec::new(),
            pos: 0,
        };
        write_pdf(&mut w, vec![page], PdfPage::Tight, true).unwrap();
        let bytes = w.inner;
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("3 Tr") && text.contains("<00480065006C006C006F> Tj"));
        assert!(
            text.contains("/Font << /F1 6 0 R >>"),
            "font follows the page's 3 objects"
        );
        assert!(text.contains("/FontFile2") && text.contains("/ToUnicode"));

        // Offsets are BYTE offsets: check them against `bytes`, never the lossy text. The table
        // is found through the trailer's `startxref` value, not by searching for "xref\n",
        // which also matches inside `startxref\n` and would land after the table.
        let start = text.rfind("startxref\n").unwrap() + "startxref\n".len();
        let xref: usize = text[start..].lines().next().unwrap().parse().unwrap();
        assert!(
            bytes[xref..].starts_with(b"xref\n"),
            "startxref must point at the table"
        );
        let table = std::str::from_utf8(&bytes[xref..]).unwrap();
        let rows: Vec<&str> = table.lines().skip(3).take(11).collect();
        assert_eq!(
            rows.len(),
            11,
            "catalog, pages, 3 page objects, 6 font objects"
        );
        for (k, row) in rows.iter().enumerate() {
            let at: usize = row[..10].parse().unwrap();
            let want = format!("{} 0 obj", k + 1);
            assert!(bytes[at..].starts_with(want.as_bytes()), "{want} misplaced");
        }

        // Same calm-retry rule as `combines_two_images_into_a_renderable_pdf`.
        let rendered = (0..3).any(|attempt| {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            decode::decode_full_for_output(&bytes).is_ok()
        });
        assert!(
            rendered,
            "a text-layered PDF should render via Windows.Data.Pdf"
        );
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
        let combined =
            combine_to_pdf(&paths, &out, 85, OnOmit::Report).expect("2 good pages remain");
        assert_eq!(
            combined.omitted.len(),
            1,
            "exactly the one garbage input must be dropped"
        );
        assert_eq!(combined.used, 2);
        assert_eq!(combined.requested(), 3);
        assert_eq!(combined.output, out);
        // 2026-09-05 audit, F31: the omission names the input and says WHY, not just how many.
        assert_eq!(combined.omitted[0].input, garbage.to_str().unwrap());
        assert_eq!(combined.omitted[0].cause, OmitCause::Undecodable);

        // The PDF that DOES get built must contain only the pages that decoded — 2, not 3 (and
        // not silently empty either).
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(
            bytes.windows(9).filter(|w| *w == b"DCTDecode").count(),
            2,
            "only the 2 decodable pages should have been embedded"
        );

        // 2026-09-05 audit, F31: the strict policy writes NOTHING when an input would be left
        // out, and its error carries the same per-input lines the partial report does.
        let strict_out = dir.join("strict.pdf");
        let err = combine_to_pdf(&paths, &strict_out, 85, OnOmit::Fail)
            .expect_err("strict must refuse a partial document");
        assert!(!strict_out.exists(), "strict must not write a partial PDF");
        let msg = err.message();
        assert!(msg.contains("strict"), "{msg}");
        assert!(
            msg.contains("omitted\t") && msg.contains("undecodable"),
            "{msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F30: an output that is one of the inputs (a PDF being re-combined
    /// over itself, the same-type alias the MCP suffix guard could not see) is refused before
    /// anything is read, and the source is byte-identical afterwards. Against the pre-fix code
    /// the call succeeds and the one-page PDF is replaced by a fresh render of itself.
    #[test]
    fn combine_to_pdf_refuses_an_output_that_is_one_of_its_inputs() {
        let dir = std::env::temp_dir().join(format!("st2k_topdf_alias_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("page.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            30,
            20,
            image::Rgb([10, 60, 60]),
        ))
        .save(&png)
        .unwrap();
        let doc = dir.join("doc.pdf");
        combine_to_pdf(
            &[png.to_str().unwrap().to_string()],
            &doc,
            85,
            OnOmit::Report,
        )
        .unwrap();
        let before = std::fs::read(&doc).unwrap();

        let upper = dir.join("DOC.PDF");
        let inputs = [
            png.to_str().unwrap().to_string(),
            doc.to_str().unwrap().to_string(),
        ];
        let err = combine_to_pdf(&inputs, &upper, 85, OnOmit::Report)
            .expect_err("the output aliases an input");
        assert!(err.message().contains("same file"), "{err}");
        assert_eq!(
            std::fs::read(&doc).unwrap(),
            before,
            "the source PDF was modified"
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
        combine_to_pdf(&paths, &out, 85, OnOmit::Report).unwrap();
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
