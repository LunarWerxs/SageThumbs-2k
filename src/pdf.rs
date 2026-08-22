//! PDF first-page thumbnails via the OS PDF rasterizer (`Windows.Data.Pdf`).
//!
//! Windows 10+ ships a PDF renderer (the engine Edge uses) behind the WinRT
//! `Windows.Data.Pdf` API. Rendering through it means PDF thumbnails cost ZERO
//! bundled bytes — no `pdfium.dll`, no Ghostscript, no extra installer weight.
//! We rasterize page 0 to a PNG byte stream and hand it back to the normal
//! image tiers (`decode::decode_image`), exactly like an ebook cover.
//!
//! The work runs on a dedicated MTA thread: WinRT's blocking waits can deadlock
//! inside a single-threaded apartment, and we can't assume which apartment the
//! shell's thumbnail host thread is in. A fresh MTA thread makes the wait safe
//! regardless of the caller, and isolates COM init/uninit. The caller `recv_timeout`s
//! that worker under a HOST-SIDE budget ([`PDF_TIMEOUT`]) — the four internal async ops
//! are each capped at ~30 s, so a malformed/encrypted PDF could otherwise park the
//! in-process shell thumbnail thread for ~120 s. The worker holds a [`crate::ModuleRef`]
//! so that a render which outlives the budget can't let the DLL unload mid-run.

use std::time::Duration;

use windows::core::{Result, RuntimeType};
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows_future::{AsyncStatus, IAsyncAction, IAsyncOperation};

/// Render the first page of a PDF to PNG bytes, scaled so its long edge is
/// ~`max_dim` px. Returns `None` on any failure (encrypted, malformed, the API
/// unavailable on this OS, …) so the shell falls back to the default icon.
/// Host-side wall-clock budget for the whole PDF render, enforced by the CALLER (not the
/// worker). Without it, a pathological PDF could park the in-process shell thumbnail thread
/// for ~120 s (four serial 30 s [`WAIT_BUDGET`] async ops). On expiry we return `None` and
/// let the worker finish + exit on its own (a leaked thread in a disposable host is the
/// accepted trade-off, same as `decode_svg`).
const PDF_TIMEOUT: Duration = Duration::from_secs(30);

pub fn render_first_page(bytes: &[u8], max_dim: u32) -> Option<Vec<u8>> {
    render_page_counted(bytes, 0, max_dim).map(|(png, _count)| png)
}

/// Render page `page_index` (0-based, clamped to the last page) of a PDF to PNG bytes (long
/// edge ~`max_dim`), AND return the document's total page count. Loads the document once. This
/// powers the Quick preview viewer's page navigation; thumbnail/preview-pane callers use the
/// page-0 [`render_first_page`] wrapper (whose behaviour is UNCHANGED). `None` on any failure.
pub fn render_page_counted(bytes: &[u8], page_index: u32, max_dim: u32) -> Option<(Vec<u8>, u32)> {
    let owned = bytes.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    // Dedicated MTA thread (see module docs). We `recv_timeout` instead of `join()` so a
    // malformed/encrypted PDF can never park the in-process shell thumbnail thread past
    // PDF_TIMEOUT. The worker holds a ModuleRef so that, if it outlives the budget, the
    // in-process host can't unload the DLL mid-render and access-violate (mirrors decode_svg).
    std::thread::spawn(move || {
        #[allow(clippy::default_constructed_unit_structs)]
        let _module = crate::ModuleRef::default();
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = render(&owned, page_index, max_dim).ok();
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(out);
    });
    match rx.recv_timeout(PDF_TIMEOUT) {
        Ok(out) => out,
        Err(_) => {
            crate::safety::log_debug("pdf: render exceeded the wall-clock deadline");
            None
        }
    }
}

/// One page's declared size in DIPs (1/96 inch), which is what `PdfPage::Size` reports.
///
/// NOT PDF points. A PDF's own user space is 1/72 inch, so US Letter is 612x792 there and
/// 816x1056 here. Only the ratio matters to layout, but the absolute numbers matter the moment
/// anyone converts to a physical size, so the unit is named rather than implied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSize {
    pub w: f32,
    pub h: f32,
}

/// A PDF held OPEN, so a caller can rasterize many pages without re-parsing the file each time.
///
/// [`render_page_counted`] loads the whole document per call, which is right for a thumbnail
/// (one page, then the process forgets the file) and wrong for the Quick preview's continuous
/// scroll, where a single flick through a document asks for page after page. A session parses
/// once and answers page requests off a channel.
///
/// The document lives on ONE dedicated MTA thread and never crosses one: WinRT's blocking waits
/// deadlock in a single-threaded apartment, `PdfDocument` is not ours to send anywhere, and the
/// callers here are the viewer's decode workers, which are not the same thread twice. Dropping
/// the session closes the channel, which ends the thread and releases the document.
pub struct PdfSession {
    jobs: std::sync::Mutex<std::sync::mpsc::Sender<Job>>,
    sizes: Vec<PageSize>,
}

/// One rasterize request: page index, target WIDTH in pixels, and where to put the PNG.
struct Job {
    page: u32,
    width: u32,
    reply: std::sync::mpsc::Sender<Option<Vec<u8>>>,
}

/// Refuse to enumerate more pages than this. `PdfPage::Size` is cheap but not free, and a
/// hostile file can claim an enormous page count; the viewer only ever scrolls through what a
/// person can scroll through. Past the cap the caller falls back to single-page paging, which
/// needs no layout at all.
pub const MAX_SESSION_PAGES: usize = 4096;

impl PdfSession {
    /// Open `bytes` and read every page's size. `None` if the document will not load (encrypted,
    /// malformed, the API missing) or has more than [`MAX_SESSION_PAGES`] pages.
    pub fn open(bytes: &[u8]) -> Option<Self> {
        let owned = bytes.to_vec();
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<Vec<PageSize>>>();
        std::thread::spawn(move || {
            #[allow(clippy::default_constructed_unit_structs)]
            let _module = crate::ModuleRef::default();
            let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            match open_document(&owned) {
                Ok((doc, sizes)) => {
                    // Announce success BEFORE serving, so `open` returns as soon as the layout
                    // is known rather than waiting on the first render.
                    let _ = ready_tx.send(Some(sizes));
                    // Ends when the session drops and the sender goes with it.
                    while let Ok(job) = job_rx.recv() {
                        let png = render_page_of(&doc, job.page, job.width).ok();
                        let _ = job.reply.send(png);
                    }
                }
                Err(_) => {
                    let _ = ready_tx.send(None);
                }
            }
            if inited {
                unsafe { CoUninitialize() };
            }
        });
        let sizes = ready_rx.recv_timeout(PDF_TIMEOUT).ok().flatten()?;
        Some(Self {
            jobs: std::sync::Mutex::new(job_tx),
            sizes,
        })
    }

    pub fn page_count(&self) -> usize {
        self.sizes.len()
    }

    /// Declared size of page `i`, clamped into range. Never panics: layout code asks about
    /// pages that a concurrent reload may already have invalidated.
    pub fn size(&self, i: usize) -> PageSize {
        self.sizes
            .get(i.min(self.sizes.len().saturating_sub(1)))
            .copied()
            .unwrap_or(PageSize { w: 612.0, h: 792.0 })
    }

    pub fn sizes(&self) -> &[PageSize] {
        &self.sizes
    }

    /// Rasterize page `i` to PNG bytes exactly `width` px wide (height follows the page's own
    /// aspect). Blocks the CALLING thread under [`PDF_TIMEOUT`], never the session thread.
    pub fn render_to_width(&self, i: usize, width: u32) -> Option<Vec<u8>> {
        if i >= self.sizes.len() || width == 0 {
            return None;
        }
        let (reply, rx) = std::sync::mpsc::channel();
        {
            let tx = self.jobs.lock().ok()?;
            tx.send(Job {
                page: i as u32,
                width,
                reply,
            })
            .ok()?;
        }
        match rx.recv_timeout(PDF_TIMEOUT) {
            Ok(png) => png,
            Err(_) => {
                crate::safety::log_debug("pdf: session render exceeded the wall-clock deadline");
                None
            }
        }
    }
}

/// Load a document from bytes and read every page's declared size.
fn open_document(bytes: &[u8]) -> Result<(PdfDocument, Vec<PageSize>)> {
    let stream = InMemoryRandomAccessStream::new()?;
    {
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(bytes)?;
        block_op(&writer.StoreAsync()?)?;
        writer.DetachStream()?;
    }
    stream.Seek(0)?;
    let doc = block_op(&PdfDocument::LoadFromStreamAsync(&stream)?)?;
    let count = doc.PageCount()?;
    if count == 0 || count as usize > MAX_SESSION_PAGES {
        return Err(E_FAIL.into());
    }
    let mut sizes = Vec::with_capacity(count as usize);
    for i in 0..count {
        let s = doc.GetPage(i)?.Size()?;
        sizes.push(PageSize {
            w: s.Width.max(1.0),
            h: s.Height.max(1.0),
        });
    }
    Ok((doc, sizes))
}

/// Rasterize one page of an already-open document to an exact pixel WIDTH.
fn render_page_of(doc: &PdfDocument, page_index: u32, width: u32) -> Result<Vec<u8>> {
    let count = doc.PageCount()?;
    if count == 0 {
        return Err(E_FAIL.into());
    }
    let page = doc.GetPage(page_index.min(count - 1))?;
    let size = page.Size()?;
    let (pw, ph) = (size.Width.max(1.0), size.Height.max(1.0));
    let dw = width.max(1);
    let dh = ((ph / pw) * dw as f32).round().max(1.0) as u32;

    let out = InMemoryRandomAccessStream::new()?;
    let opts = PdfPageRenderOptions::new()?;
    opts.SetDestinationWidth(dw)?;
    opts.SetDestinationHeight(dh)?;
    block_action(&page.RenderWithOptionsToStreamAsync(&out, &opts)?)?;

    out.Seek(0)?;
    let len = out.Size()? as u32;
    let reader = DataReader::CreateDataReader(&out)?;
    block_op(&reader.LoadAsync(len)?)?;
    let mut buf = vec![0u8; len as usize];
    reader.ReadBytes(&mut buf)?;
    Ok(buf)
}

fn render(bytes: &[u8], page_index: u32, max_dim: u32) -> Result<(Vec<u8>, u32)> {
    // Copy the PDF into a WinRT in-memory stream.
    let stream = InMemoryRandomAccessStream::new()?;
    {
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(bytes)?;
        block_op(&writer.StoreAsync()?)?;
        // Detach so dropping the writer doesn't close `stream`.
        writer.DetachStream()?;
    }
    stream.Seek(0)?;

    // Load the document and grab the requested page (clamped into range).
    let doc = block_op(&PdfDocument::LoadFromStreamAsync(&stream)?)?;
    let count = doc.PageCount()?;
    if count == 0 {
        return Err(E_FAIL.into());
    }
    let page = doc.GetPage(page_index.min(count - 1))?;

    // Page size is in DIPs (96 dpi). Scale so the long edge is `max_dim`.
    let size = page.Size()?;
    let (pw, ph) = (size.Width.max(1.0), size.Height.max(1.0));
    let scale = max_dim as f32 / pw.max(ph);
    let dw = (pw * scale).round().clamp(1.0, max_dim as f32) as u32;
    let dh = (ph * scale).round().clamp(1.0, max_dim as f32) as u32;

    // Rasterize to a PNG stream (PdfPageRenderOptions defaults to PNG).
    let out = InMemoryRandomAccessStream::new()?;
    let opts = PdfPageRenderOptions::new()?;
    opts.SetDestinationWidth(dw)?;
    opts.SetDestinationHeight(dh)?;
    block_action(&page.RenderWithOptionsToStreamAsync(&out, &opts)?)?;

    // Read the PNG bytes back out.
    out.Seek(0)?;
    let len = out.Size()? as u32;
    let reader = DataReader::CreateDataReader(&out)?;
    block_op(&reader.LoadAsync(len)?)?;
    let mut buf = vec![0u8; len as usize];
    reader.ReadBytes(&mut buf)?;
    Ok((buf, count))
}

/// Hard cap on a single async wait so a pathological PDF can't hang the thread.
const WAIT_BUDGET: u32 = 30_000; // ~30 s at 1 ms/poll

/// Block until a WinRT `IAsyncOperation<T>` finishes, then return its result.
/// (windows-future's event-based `.join()` lives on a private trait, so we poll
/// `Status()` — fine on our dedicated render thread.)
pub(crate) fn block_op<T: RuntimeType>(op: &IAsyncOperation<T>) -> Result<T> {
    for _ in 0..WAIT_BUDGET {
        if op.Status()? != AsyncStatus::Started {
            return op.GetResults();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(E_FAIL.into())
}

/// Block until a WinRT `IAsyncAction` finishes.
fn block_action(op: &IAsyncAction) -> Result<()> {
    for _ in 0..WAIT_BUDGET {
        if op.Status()? != AsyncStatus::Started {
            return op.GetResults();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(E_FAIL.into())
}

#[cfg(test)]
mod tests {
    use super::render_page_counted;

    /// Build a minimal, valid multi-page PDF where page `i` is a solid `colours[i]`.
    ///
    /// Hand-assembled rather than generated by ImageMagick, for the same reason
    /// `container::djvu` builds its own: a fixture the TEST can produce runs in CI, where the
    /// corpus is never checked out. A solid-colour page is also the only page whose rendered
    /// output can be asserted exactly, which is what makes "page 3 really is page 3" provable
    /// instead of eyeballed.
    ///
    /// Deliberately NOT built with our own [`crate::topdf`] writer, which could do it. A
    /// fixture written by the code under test shares its assumptions, so the pair would agree
    /// with each other while both being wrong about PDF. These bytes are written to the spec
    /// and read back by Windows' engine, so agreement means something.
    ///
    /// US Letter at 72 dpi (612x792), one uncompressed content stream per page. The xref
    /// offsets are computed as the body is written, because a PDF whose xref lies is a PDF
    /// some readers accept and others silently refuse, and a fixture must not be the variable.
    pub(super) fn solid_colour_pdf(colours: &[(u8, u8, u8)]) -> Vec<u8> {
        assert!(!colours.is_empty(), "a PDF needs at least one page");
        let n = colours.len();
        let mut out: Vec<u8> = Vec::new();
        let mut offsets: Vec<usize> = Vec::new();
        // Object numbering: 1 = catalog, 2 = page tree, then a (page, contents) pair each.
        let page_obj = |i: usize| 3 + i * 2;
        let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: String| {
            offsets.push(out.len());
            out.extend_from_slice(body.as_bytes());
        };

        out.extend_from_slice(b"%PDF-1.4\n");
        obj(
            &mut out,
            &mut offsets,
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".into(),
        );
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", page_obj(i))).collect();
        obj(
            &mut out,
            &mut offsets,
            format!(
                "2 0 obj\n<< /Type /Pages /Kids [{}] /Count {n} >>\nendobj\n",
                kids.join(" ")
            ),
        );
        for (i, &(r, g, b)) in colours.iter().enumerate() {
            let (po, co) = (page_obj(i), page_obj(i) + 1);
            obj(
                &mut out,
                &mut offsets,
                format!(
                    "{po} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                     /Contents {co} 0 R /Resources << >> >>\nendobj\n"
                ),
            );
            let stream = format!(
                "{:.5} {:.5} {:.5} rg\n0 0 612 792 re\nf\n",
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0
            );
            obj(
                &mut out,
                &mut offsets,
                format!(
                    "{co} 0 obj\n<< /Length {} >>\nstream\n{stream}endstream\nendobj\n",
                    stream.len()
                ),
            );
        }

        let xref_at = out.len();
        let total = offsets.len() + 1; // +1 for the free object 0
        out.extend_from_slice(format!("xref\n0 {total}\n0000000000 65535 f \n").as_bytes());
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {total} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n")
                .as_bytes(),
        );
        out
    }

    /// The four pages the corpus fixture and the unit tests both use. Page 0 is the corpus's
    /// standard "right answer" blue, so the existing thumbnail colour gate covers this file
    /// for free; the rest are far enough apart that no rounding can confuse them.
    pub(super) const PAGES: [(u8, u8, u8); 4] =
        [(30, 60, 210), (30, 170, 60), (230, 140, 20), (140, 50, 190)];

    /// Mean RGB of a decoded PNG, ignoring nothing. A solid page has no other content, so the
    /// mean IS the colour, and a wrong page shows up as a channel miles away rather than a
    /// subtle difference.
    fn mean_rgb(png: &[u8]) -> (u8, u8, u8) {
        let img = image::load_from_memory(png)
            .expect("render output is a PNG")
            .to_rgb8();
        let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
        for p in img.pixels() {
            r += u64::from(p[0]);
            g += u64::from(p[1]);
            b += u64::from(p[2]);
        }
        let n = u64::from(img.width()) * u64::from(img.height());
        assert!(n > 0, "rendered page is empty");
        ((r / n) as u8, (g / n) as u8, (b / n) as u8)
    }

    fn close(a: (u8, u8, u8), b: (u8, u8, u8)) -> bool {
        let d = |x: u8, y: u8| i32::from(x) - i32::from(y);
        d(a.0, b.0).abs() <= 8 && d(a.1, b.1).abs() <= 8 && d(a.2, b.2).abs() <= 8
    }

    /// The gap that let a multi-page PDF bug ship: the corpus's only ordinary PDF has ONE page,
    /// so nothing ever asked whether page N is really page N, or whether the page COUNT that
    /// drives the viewer's pager is the document's real one.
    #[test]
    fn every_page_of_a_multipage_pdf_renders_as_itself() {
        let pdf = solid_colour_pdf(&PAGES);
        for (i, &want) in PAGES.iter().enumerate() {
            let Some((png, count)) = render_page_counted(&pdf, i as u32, 256) else {
                panic!("page {i} of a {}-page PDF did not render", PAGES.len());
            };
            assert_eq!(
                count as usize,
                PAGES.len(),
                "page {i} reported {count} pages; the viewer's pager reads this number"
            );
            let got = mean_rgb(&png);
            assert!(
                close(got, want),
                "page {i} rendered {got:?}, expected {want:?} - it drew a DIFFERENT page"
            );
        }
    }

    /// Asking past the end must clamp to the last page, never fail and never wrap. The viewer
    /// relies on this: `goto_pdf_page` clamps too, but a restored session or a `--pdf-page`
    /// argument can hand in any number at all.
    #[test]
    fn a_page_index_past_the_end_clamps_to_the_last_page() {
        let pdf = solid_colour_pdf(&PAGES);
        let (png, count) = render_page_counted(&pdf, 9_999, 256).expect("clamped render");
        assert_eq!(count as usize, PAGES.len());
        assert!(
            close(mean_rgb(&png), PAGES[PAGES.len() - 1]),
            "an out-of-range page must clamp to the LAST page"
        );
    }

    /// A single-page PDF must still report exactly one page, or the viewer would show a pager
    /// for a document that has nothing to page through. This is the case the corpus already
    /// covered, kept here so the pair is visible together.
    #[test]
    fn a_single_page_pdf_reports_one_page() {
        let pdf = solid_colour_pdf(&PAGES[..1]);
        let (_png, count) = render_page_counted(&pdf, 0, 128).expect("single-page render");
        assert_eq!(count, 1);
    }

    /// The session is what continuous scrolling stands on: it must know every page's size
    /// BEFORE anything is rasterized, or the scroll bar would have to guess how tall the
    /// document is and then jump as pages arrived.
    #[test]
    fn a_session_reports_every_page_size_without_rendering_anything() {
        let pdf = solid_colour_pdf(&PAGES);
        let s = super::PdfSession::open(&pdf).expect("session opens");
        assert_eq!(s.page_count(), PAGES.len());
        for i in 0..s.page_count() {
            let sz = s.size(i);
            // US Letter is 612x792 PDF points, and a point is 1/72 inch while a DIP is 1/96,
            // so WinRT reports 816x1056. Asserted in DIPs deliberately: the first version of
            // this test expected the point figures, and the only reason that mistake did not
            // reach the layout code is that it failed here.
            assert!(
                (sz.w - 816.0).abs() < 0.5 && (sz.h - 1056.0).abs() < 0.5,
                "page {i} reported {sz:?}, expected US Letter in DIPs (816x1056)"
            );
        }
    }

    /// One open document, many pages, each still its own page. The whole point of the session
    /// is that the SECOND page does not re-parse the file, so a regression here would be
    /// silent: correct pictures, just slow. The content assertion is what keeps it honest.
    #[test]
    fn a_session_renders_each_page_at_the_width_it_was_asked_for() {
        let pdf = solid_colour_pdf(&PAGES);
        let s = super::PdfSession::open(&pdf).expect("session opens");
        for (i, &want) in PAGES.iter().enumerate() {
            let png = s
                .render_to_width(i, 300)
                .unwrap_or_else(|| panic!("page {i} rendered"));
            let img = image::load_from_memory(&png).expect("PNG").to_rgb8();
            assert_eq!(img.width(), 300, "page {i} came back the wrong width");
            // 612x792 at 300 px wide is 388 tall; allow the rounding either way.
            assert!(
                (img.height() as i32 - 388).abs() <= 1,
                "page {i} is {}px tall, expected the page's own aspect",
                img.height()
            );
            assert!(
                close(mean_rgb(&png), want),
                "page {i} rendered the wrong page"
            );
        }
    }

    /// A session must refuse rather than mislead. An out-of-range page is `None`, not a
    /// silently clamped neighbour: layout code asks by index and would otherwise stack the
    /// last page over and over past the end of the document.
    #[test]
    fn a_session_refuses_a_page_that_does_not_exist() {
        let pdf = solid_colour_pdf(&PAGES);
        let s = super::PdfSession::open(&pdf).expect("session opens");
        assert!(s.render_to_width(PAGES.len(), 200).is_none());
        assert!(s.render_to_width(9_999, 200).is_none());
        assert!(
            s.render_to_width(0, 0).is_none(),
            "zero width is not a render"
        );
    }

    /// Garbage in, `None` out, and no hung thread. The viewer calls this on any file the user
    /// presses Space on, so "not actually a PDF" is a normal input, not an exceptional one.
    #[test]
    fn a_session_declines_something_that_is_not_a_pdf() {
        assert!(super::PdfSession::open(b"this is not a PDF at all").is_none());
        assert!(super::PdfSession::open(&[]).is_none());
    }

    /// Writes the corpus fixture. Run by hand after changing `PAGES`; `build-corpus.ps1`
    /// documents the same command:
    ///
    ///   cargo test --release --lib write_pdf_corpus_fixture -- --ignored --nocapture
    #[test]
    #[ignore = "writes a corpus fixture on demand"]
    fn write_pdf_corpus_fixture() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus");
        assert!(corpus.is_dir(), "no test-corpus at {}", corpus.display());
        let pdf = solid_colour_pdf(&PAGES);
        let p = corpus.join("sample-multipage.pdf");
        std::fs::write(&p, &pdf).unwrap();
        eprintln!(
            "wrote {} ({} bytes, {} pages)",
            p.display(),
            pdf.len(),
            PAGES.len()
        );
    }
}
