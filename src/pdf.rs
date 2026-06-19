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
//! regardless of the caller, and isolates COM init/uninit.

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
pub fn render_first_page(bytes: &[u8], max_dim: u32) -> Option<Vec<u8>> {
    let owned = bytes.to_vec();
    // Own MTA thread (see module docs); join blocks the caller for the result.
    let worker = std::thread::spawn(move || {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = render(&owned, max_dim).ok();
        if inited {
            unsafe { CoUninitialize() };
        }
        out
    });
    match worker.join() {
        Ok(out) => out,
        Err(_) => {
            // A panic in the WinRT worker would otherwise vanish as a plain None.
            crate::safety::log_debug("pdf: render worker thread panicked");
            None
        }
    }
}

fn render(bytes: &[u8], max_dim: u32) -> Result<Vec<u8>> {
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

    // Load the document and grab page 0.
    let doc = block_op(&PdfDocument::LoadFromStreamAsync(&stream)?)?;
    if doc.PageCount()? == 0 {
        return Err(E_FAIL.into());
    }
    let page = doc.GetPage(0)?;

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
    Ok(buf)
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
