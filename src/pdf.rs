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
//! in-process shell thumbnail thread for ~120 s. The worker holds a [`crate::host::ModuleRef`]
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

/// Run `f` on the current thread inside a fresh MTA COM apartment, holding a [`crate::host::ModuleRef`]
/// DLL pin for the whole call. WinRT's blocking waits can deadlock in an STA and we can't assume
/// the caller's apartment (see the module docs), so every detached worker wraps its body in this.
/// The apartment is unbalanced again before returning, and only if this call initialized it.
/// Shared with `crate::video`'s Media Foundation workers.
pub(crate) fn with_mta_apartment<T>(f: impl FnOnce() -> T) -> T {
    #[allow(clippy::default_constructed_unit_structs)]
    let _module = crate::host::ModuleRef::default();
    // S_OK / S_FALSE both add a ref to balance; RPC_E_CHANGED_MODE does not.
    let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let out = f();
    if inited {
        unsafe { CoUninitialize() };
    }
    out
}

pub fn render_first_page(bytes: &[u8], max_dim: u32) -> Option<Vec<u8>> {
    render_page_counted(bytes, 0, max_dim).map(|(png, _count)| png)
}

/// Render page `page_index` (0-based, clamped to the last page) of a PDF to PNG bytes (long
/// edge ~`max_dim`), AND return the document's total page count. Loads the document once. This
/// powers the Quick preview viewer's page navigation; thumbnail/preview-pane callers use the
/// page-0 [`render_first_page`] wrapper (whose behaviour is UNCHANGED). `None` on any failure.
pub fn render_page_counted(bytes: &[u8], page_index: u32, max_dim: u32) -> Option<(Vec<u8>, u32)> {
    render_page_counted_from(Source::Bytes(bytes.to_vec()), page_index, max_dim)
}

/// [`render_page_counted`] for the file at `path`, read by the rasterizer as it needs it: no
/// copy of the document is held, so its size does not matter. What the Quick preview uses;
/// reading the whole file first refused any PDF past the 256 MiB input ceiling (the big-file
/// gate, 2026-09-23).
pub fn render_page_counted_path(
    path: &str,
    page_index: u32,
    max_dim: u32,
) -> Option<(Vec<u8>, u32)> {
    render_page_counted_from(Source::Path(path.to_string()), page_index, max_dim)
}

/// The biggest document handed to Windows' PDF engine. Past 2 GiB `Windows.Data.Pdf` takes the
/// whole PROCESS down with an access violation - measured 2026-09-23 on a 2.2 GB PDF, loaded by
/// `PdfDocument::LoadFromFileAsync` from plain PowerShell with none of our code involved - and
/// in Explorer that process is the thumbnail host. So a bigger PDF gets no page from us.
pub(crate) const MAX_ENGINE_BYTES: u64 = i32::MAX as u64;

/// May a document `len` bytes long be handed to Windows' PDF engine? See [`MAX_ENGINE_BYTES`].
pub(crate) fn engine_can_open(len: u64) -> bool {
    len <= MAX_ENGINE_BYTES
}

/// Where a document is read from: bytes in hand, or a file the rasterizer reads itself.
enum Source {
    Bytes(Vec<u8>),
    Path(String),
}

impl Source {
    /// The document as the stream `PdfDocument` loads from.
    fn open(&self) -> Result<windows::Storage::Streams::IRandomAccessStream> {
        use windows::core::Interface;
        let len = match self {
            Self::Bytes(bytes) => Some(bytes.len() as u64),
            Self::Path(path) => std::fs::metadata(path).ok().map(|m| m.len()),
        };
        if !len.is_some_and(engine_can_open) {
            return Err(E_FAIL.into());
        }
        match self {
            Self::Bytes(bytes) => stream_with_bytes(bytes)?.cast(),
            Self::Path(path) => unsafe {
                use windows::Win32::System::Com::STGM_READ;
                let file = windows::Win32::UI::Shell::SHCreateStreamOnFileEx(
                    &windows::core::HSTRING::from(path.as_str()),
                    STGM_READ.0,
                    0,
                    false,
                    None,
                )?;
                windows::Win32::System::WinRT::CreateRandomAccessStreamOverStream(
                    &file,
                    windows::Win32::System::WinRT::BSOS_DEFAULT,
                )
            },
        }
    }
}

fn render_page_counted_from(
    source: Source,
    page_index: u32,
    max_dim: u32,
) -> Option<(Vec<u8>, u32)> {
    let owned = source;
    // Dedicated MTA thread (see module docs), waited on for at most PDF_TIMEOUT so a
    // malformed/encrypted PDF can never park the in-process shell thumbnail thread. A
    // budgeted worker: it holds a ModuleRef from before `spawn` until it has sent its result,
    // so a render that outlives the budget cannot have the DLL unloaded under it; a refused
    // thread is a `None`, never a panic; and abandoned renders count toward the process cap.
    let out = crate::safety::spawn_budgeted("st2k-pdf-render", PDF_TIMEOUT, move || {
        with_mta_apartment(|| render(&owned, page_index, max_dim).ok())
    });
    if out.is_none() {
        crate::safety::log_debug(
            "pdf: render exceeded the wall-clock deadline (or found no worker)",
        );
    }
    out.flatten()
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
        Self::open_from(Source::Bytes(bytes.to_vec()))
    }

    /// [`Self::open`] for the file at `path`, read by the rasterizer as it needs it (see
    /// [`render_page_counted_path`]).
    pub fn open_path(path: &str) -> Option<Self> {
        Self::open_from(Source::Path(path.to_string()))
    }

    fn open_from(owned: Source) -> Option<Self> {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<Vec<PageSize>>>();
        crate::safety::try_spawn("st2k-pdf-session", move || {
            with_mta_apartment(|| match open_document(&owned) {
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
            });
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
            // Unreachable today (`open_document` refuses a document with no pages), and in
            // DIPs like every `PageSize`: US Letter is 816 x 1056 DIPs (612 x 792 points).
            .unwrap_or(PageSize {
                w: 816.0,
                h: 1056.0,
            })
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

/// Load a document and read every page's declared size.
fn open_document(source: &Source) -> Result<(PdfDocument, Vec<PageSize>)> {
    let doc = block_op(&PdfDocument::LoadFromStreamAsync(&source.open()?)?)?;
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

/// Rasterize one page of an already-open document to an exact pixel WIDTH, unless the page is
/// so tall for its width that the height would pass `MAX_DIM`: the page size comes from the
/// file (a 1 x 14400 pt strip is a legal page), so both edges are then scaled down together.
fn render_page_of(doc: &PdfDocument, page_index: u32, width: u32) -> Result<Vec<u8>> {
    let count = doc.PageCount()?;
    if count == 0 {
        return Err(E_FAIL.into());
    }
    let page = doc.GetPage(page_index.min(count - 1))?;
    let size = page.Size()?;
    let (dw, dh) = width_fitted_dims(size.Width, size.Height, width);
    rasterize_page_to_png(&page, dw, dh)
}

/// The pixel size of a `pw` x `ph` page drawn `width` pixels wide, both edges capped at
/// `MAX_DIM` with the aspect kept.
fn width_fitted_dims(pw: f32, ph: f32, width: u32) -> (u32, u32) {
    let cap = crate::decode::limits::MAX_DIM as f32;
    let (pw, ph) = (pw.max(1.0), ph.max(1.0));
    let dw = (width.max(1) as f32).min(cap);
    let dh = (ph / pw) * dw;
    let shrink = if dh > cap { cap / dh } else { 1.0 };
    (
        (dw * shrink).round().clamp(1.0, cap) as u32,
        (dh * shrink).round().clamp(1.0, cap) as u32,
    )
}

fn render(source: &Source, page_index: u32, max_dim: u32) -> Result<(Vec<u8>, u32)> {
    render_doc(
        block_op(&PdfDocument::LoadFromStreamAsync(&source.open()?)?)?,
        page_index,
        max_dim,
    )
}

/// How a page is sized: by its long side, or, Illustrator's rule (`decode::pdf_tier`), by its
/// width.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PageFit {
    LongSide(u32),
    Width(u32),
}

/// The first `pages` pages of a PDF read straight off `shell`, a stream `size` bytes long, each
/// fitted by `fit`, as PNG bytes, with the document's page count: a file too big to hold. A big
/// PDF is its images and fonts, with the cross-reference that finds page one at its END, so no
/// bounded head read serves it (the big-file gate, 2026-09-23); the OS rasterizer reads what it
/// needs through a block cache instead. The worker gets the stream through the Global Interface
/// Table, so this is safe from the thumbnail host's thread whatever its apartment. A page that
/// fails to render ends the list there.
pub(crate) fn render_pages_from_stream(
    shell: &windows::Win32::System::Com::IStream,
    size: u64,
    fit: PageFit,
    pages: u32,
) -> Option<(Vec<Vec<u8>>, u32)> {
    use windows::Storage::Streams::IRandomAccessStream;
    use windows::Win32::System::WinRT::{CreateRandomAccessStreamOverStream, BSOS_DEFAULT};
    if !engine_can_open(size) {
        return None;
    }
    crate::video::with_stream_on_worker(
        shell,
        PDF_TIMEOUT,
        "pdf: the streamed render",
        move |inner| {
            let deadline = std::time::Instant::now() + PDF_TIMEOUT;
            let cached: windows::Win32::System::Com::IStream =
                crate::vstream::BlockCacheStream::new(inner, size, deadline).into();
            let ras: IRandomAccessStream =
                unsafe { CreateRandomAccessStreamOverStream(&cached, BSOS_DEFAULT) }.ok()?;
            let doc = block_op(&PdfDocument::LoadFromStreamAsync(&ras).ok()?).ok()?;
            let count = doc.PageCount().ok()?;
            let pngs: Vec<Vec<u8>> = (0..count.min(pages))
                .map_while(|i| render_fitted(&doc, i, fit).ok())
                .collect();
            Some((pngs, count))
        },
    )
}

/// Page `i` of an open document, sized by `fit`, as PNG bytes.
fn render_fitted(doc: &PdfDocument, i: u32, fit: PageFit) -> Result<Vec<u8>> {
    match fit {
        PageFit::LongSide(max_dim) => render_doc(doc.clone(), i, max_dim).map(|(png, _)| png),
        PageFit::Width(width) => render_page_of(doc, i, width),
    }
}

/// Page `page_index` (clamped into range) of an open document at long edge `max_dim`, as PNG
/// bytes, and the document's page count.
fn render_doc(doc: PdfDocument, page_index: u32, max_dim: u32) -> Result<(Vec<u8>, u32)> {
    let count = doc.PageCount()?;
    if count == 0 {
        return Err(E_FAIL.into());
    }
    let page = doc.GetPage(page_index.min(count - 1))?;

    let (dw, dh) = scaled_page_dims(&page, max_dim)?;
    let buf = rasterize_page_to_png(&page, dw, dh)?;
    Ok((buf, count))
}

/// Copy `bytes` into a fresh WinRT in-memory stream, rewound to the start. Shared with
/// `crate::ocr`, whose `BitmapDecoder` is fed from the very same kind of stream.
pub(crate) fn stream_with_bytes(bytes: &[u8]) -> Result<InMemoryRandomAccessStream> {
    let stream = InMemoryRandomAccessStream::new()?;
    {
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(bytes)?;
        block_op(&writer.StoreAsync()?)?;
        // Detach so dropping the writer doesn't close `stream`.
        writer.DetachStream()?;
    }
    stream.Seek(0)?;
    Ok(stream)
}

/// Page size is in DIPs (96 dpi). Scale so the long edge is `max_dim`.
fn scaled_page_dims(page: &windows::Data::Pdf::PdfPage, max_dim: u32) -> Result<(u32, u32)> {
    let size = page.Size()?;
    let (pw, ph) = (size.Width.max(1.0), size.Height.max(1.0));
    let scale = max_dim as f32 / pw.max(ph);
    let dw = (pw * scale).round().clamp(1.0, max_dim as f32) as u32;
    let dh = (ph * scale).round().clamp(1.0, max_dim as f32) as u32;
    Ok((dw, dh))
}

/// Rasterize `page` to a PNG byte stream (`PdfPageRenderOptions` defaults to PNG) and
/// read the encoded bytes back out.
fn rasterize_page_to_png(page: &windows::Data::Pdf::PdfPage, dw: u32, dh: u32) -> Result<Vec<u8>> {
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

/// Hard cap on a single async wait so a pathological PDF can't hang the thread.
const WAIT_BUDGET: u32 = 30_000; // ~30 s at 1 ms/poll

/// Block until a WinRT `IAsyncOperation<T>` finishes, then return its result.
/// (windows-future's event-based `.join()` lives on a private trait, so we poll
/// `Status()` — fine on our dedicated render thread.)
pub(crate) fn block_op<T: RuntimeType>(op: &IAsyncOperation<T>) -> Result<T> {
    wait_until_settled(|| op.Status())?;
    op.GetResults()
}

/// Block until a WinRT `IAsyncAction` finishes. Shared with the lock-screen verb, which
/// waits on `LockScreen::SetImageFileAsync` the same way.
pub(crate) fn block_action(op: &IAsyncAction) -> Result<()> {
    wait_until_settled(|| op.Status())?;
    op.GetResults()
}

/// Poll `status` once a millisecond until the operation has left `Started`, giving up after
/// [`WAIT_BUDGET`] polls so a pathological PDF cannot hang the thread.
fn wait_until_settled(status: impl Fn() -> Result<AsyncStatus>) -> Result<()> {
    for _ in 0..WAIT_BUDGET {
        if status()? != AsyncStatus::Started {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(E_FAIL.into())
}

#[cfg(test)]
pub(crate) mod tests;
