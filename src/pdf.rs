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

/// Pixel width a page is rasterized at when it is going to be READ rather than looked at.
///
/// `Windows.Data.Pdf` exposes no text layer at all, so the Quick preview's Ctrl+F gets a PDF's
/// text by rendering each page and running [`crate::ocr`] over it. This width is what decides
/// whether that works.
///
/// **Measured, not assumed.** `tests::what_the_recognizer_reads_at_each_render_width` prints the
/// table below: one US Letter page carrying a 48 pt heading and a line of 14 pt body text.
///
/// ```text
///   width   14pt is   heading   body      elapsed
///     200     4.6 px  yes       NO         199 ms
///     300     6.9 px  yes       NO          22 ms
///     400     9.2 px  NO        yes         28 ms
///     600    13.7 px  yes       yes         29 ms
///     800    18.3 px  yes       yes         45 ms
///    1000    22.9 px  yes       yes         34 ms
///    2000    45.8 px  yes       yes         99 ms
///    3200    73.2 px  yes       yes        255 ms
/// ```
///
/// So 600 px would already read THIS page, and at a third of the cost. The width is nonetheless
/// 2000, for two reasons the fixture cannot show:
///
/// 1. **14 pt is not what documents are set in.** Ordinary body text is 9 to 11 pt. At 2000 px
///    that lands at 29 to 36 px, which is past the ~26 px `ocr`'s own measurements call clean. At
///    1000 px the same text is 15 to 18 px, which is the range the engine returns the EMPTY
///    STRING for - indistinguishable, to a reader, from a document with no text in it.
/// 2. **Below ~900 px `ocr` enlarges the bitmap itself**, so a small render is not actually the
///    saving it looks like; it moves the work rather than removing it, and it is the reason the
///    400 px row can read the body text while failing on the heading.
///
/// The remaining cost is ~100 ms a page of background work, which buys a margin over the whole
/// range of type sizes a real document might use. That is the right side to be wrong on: this
/// runs behind a search box, and a search that finds nothing is worse than one that is slow.
pub const OCR_RENDER_WIDTH: u32 = 2000;

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

    /// Build a valid multi-page PDF where page `i` carries `headings[i]` in 48 pt and the same
    /// line of ordinary 14 pt body text on every page.
    ///
    /// The body line is the point of the fixture, not decoration. A 48 pt heading is legible at
    /// almost any render width (it survives even at 200 px, see the table on
    /// [`super::OCR_RENDER_WIDTH`]), so a heading-only fixture would keep passing while the width
    /// fell far enough to stop reading real documents. The 14 pt line fails from 300 px down,
    /// which is what makes this fixture able to fail at all.
    ///
    /// It does NOT pin the width to its current value: 600 px reads this page perfectly well.
    /// The argument for 2000 is about type sizes smaller than any fixture can prove from the
    /// inside, and it lives on the constant.
    ///
    /// Same hand-written-to-the-spec approach as [`solid_colour_pdf`], and the same reason: a
    /// fixture produced by our own PDF writer would share its assumptions.
    pub(super) fn text_pdf(headings: &[&str]) -> Vec<u8> {
        assert!(!headings.is_empty(), "a PDF needs at least one page");
        let n = headings.len();
        let mut out: Vec<u8> = Vec::new();
        let mut offsets: Vec<usize> = Vec::new();
        // 1 = catalog, 2 = page tree, 3 = the font, then a (page, contents) pair each. Written in
        // ascending object order so the xref built from `offsets` lines up by position.
        let page_obj = |i: usize| 4 + i * 2;
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
        // Helvetica is one of the 14 standard PDF fonts, so nothing has to be embedded and the
        // fixture stays a few kilobytes.
        obj(
            &mut out,
            &mut offsets,
            "3 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n".into(),
        );
        for (i, heading) in headings.iter().enumerate() {
            let (po, co) = (page_obj(i), page_obj(i) + 1);
            obj(
                &mut out,
                &mut offsets,
                format!(
                    "{po} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                     /Contents {co} 0 R /Resources << /Font << /F1 3 0 R >> >> >>\nendobj\n"
                ),
            );
            let stream = format!(
                "BT /F1 48 Tf 60 640 Td ({heading}) Tj ET\n\
                 BT /F1 14 Tf 60 560 Td ({BODY_LINE}) Tj ET\n"
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
        let total = offsets.len() + 1;
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

    /// The 14 pt line every page of [`text_pdf`] carries. A pangram, so the recognizer is asked
    /// for a spread of letter shapes rather than one lucky word.
    const BODY_LINE: &str = "the quick brown fox jumps over the lazy dog";

    /// The whole Ctrl+F chain, end to end: an open session rasterizes a page, the in-box
    /// recognizer reads it, and what comes back is that page's own words.
    ///
    /// This is the test that would catch the feature rotting silently. Every part of the search
    /// above this - the offsets, the page map, the progress note - is pure and unit-tested, and
    /// all of it would keep passing perfectly while returning nothing at all, because the thing
    /// that actually produces the text is two OS APIs talking to each other.
    #[test]
    fn body_text_survives_the_trip_through_a_rendered_page() {
        let headings = ["ALPHA", "BRAVO", "CHARLIE"];
        let pdf = text_pdf(&headings);
        let s = super::PdfSession::open(&pdf).expect("session opens");
        assert_eq!(s.page_count(), headings.len());

        let png = s
            .render_to_width(1, super::OCR_RENDER_WIDTH)
            .expect("page renders");
        let text = match crate::ocr::recognize_bytes(png) {
            Ok(t) => t,
            Err(e) => {
                // No OCR language pack on this machine. Say so loudly rather than passing
                // quietly: a green run that gated nothing is worse than a red one.
                eprintln!(
                    "SKIPPED body_text_survives_the_trip_through_a_rendered_page: this machine \
                     has no usable OCR engine ({e:?}), so PDF text search cannot be gated here"
                );
                return;
            }
        };
        assert!(
            !text.trim().is_empty(),
            "the recognizer works but read nothing off a page of plain Helvetica"
        );
        let up = text.to_uppercase();
        assert!(
            up.contains("BRAVO"),
            "page 2's heading did not survive the round trip: {text:?}"
        );
        assert!(
            up.contains("QUICK") && up.contains("BROWN"),
            "14 pt body text did not survive at {} px, which is the width every PDF search \
             renders at. Anything at or below 300 px fails this line (see the table on \
             OCR_RENDER_WIDTH), so a width that dropped that far would silently return no \
             results for every real document: {text:?}",
            super::OCR_RENDER_WIDTH
        );

        // And every page must read as ITSELF. A search hit is turned into a page number, so a
        // page that reads as its neighbour scrolls the reader somewhere the word is not.
        for (i, want) in headings.iter().enumerate() {
            let png = s
                .render_to_width(i, super::OCR_RENDER_WIDTH)
                .unwrap_or_else(|| panic!("page {i} renders"));
            let got = crate::ocr::recognize_bytes(png)
                .unwrap_or_default()
                .to_uppercase();
            assert!(
                got.contains(want),
                "page {i} should carry {want}, read {got:?}"
            );
            for other in headings.iter().filter(|o| *o != want) {
                assert!(
                    !got.contains(other),
                    "page {i} also read as {other}, so pages are not distinguishable"
                );
            }
        }
    }

    /// Prints what the recognizer actually reads off a page at each render width, which is where
    /// [`super::OCR_RENDER_WIDTH`]'s value comes from. Run by hand:
    ///
    ///   cargo test --lib what_the_recognizer_reads_at_each_render_width -- --ignored --nocapture
    #[test]
    #[ignore = "prints a measurement table"]
    fn what_the_recognizer_reads_at_each_render_width() {
        let pdf = text_pdf(&["ALPHA"]);
        let s = super::PdfSession::open(&pdf).expect("session opens");
        eprintln!("  width   body px   heading   body      elapsed");
        for width in [200u32, 300, 400, 600, 800, 1000, 1600, 2000, 2400, 3200] {
            let t0 = std::time::Instant::now();
            let png = s.render_to_width(0, width).expect("renders");
            let text = crate::ocr::recognize_bytes(png).unwrap_or_default();
            let ms = t0.elapsed().as_millis();
            let up = text.to_uppercase();
            // 14 pt on a 612 pt page: how tall the body glyphs land once rendered at this width.
            let body_px = 14.0 * f64::from(width) / 612.0;
            eprintln!(
                "  {width:>5}   {body_px:>7.1}   {:<7}   {:<7}   {ms:>4} ms",
                if up.contains("ALPHA") { "yes" } else { "NO" },
                if up.contains("QUICK") && up.contains("BROWN") {
                    "yes"
                } else {
                    "NO"
                },
            );
        }
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

    /// Writes a searchable multi-page PDF for driving the viewer headlessly, so a `--shot` of
    /// Ctrl+F over a PDF is reproducible rather than depending on whatever file was to hand:
    ///
    ///   ST2K_FIXTURE_OUT=D:\tmp\searchable.pdf cargo test --lib write_searchable_pdf_fixture \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore = "writes a searchable PDF fixture on demand"]
    fn write_searchable_pdf_fixture() {
        let out =
            std::env::var("ST2K_FIXTURE_OUT").expect("set ST2K_FIXTURE_OUT to the path to write");
        // ST2K_FIXTURE_PAGES makes a document longer than the search's own page cap, which is
        // the only way to see the "read this many of that many pages" note stay up.
        let n: usize = std::env::var("ST2K_FIXTURE_PAGES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4);
        const NAMES: [&str; 8] = [
            "ALPHA", "BRAVO", "CHARLIE", "DELTA", "ECHO", "FOXTROT", "GOLF", "HOTEL",
        ];
        let owned: Vec<String> = (0..n.max(1))
            .map(|i| {
                if n <= NAMES.len() {
                    NAMES[i].to_string()
                } else {
                    format!("{} {}", NAMES[i % NAMES.len()], i + 1)
                }
            })
            .collect();
        let headings: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pdf = text_pdf(&headings);
        std::fs::write(&out, &pdf).unwrap();
        eprintln!(
            "wrote {out} ({} bytes, {} pages)",
            pdf.len(),
            headings.len()
        );
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
