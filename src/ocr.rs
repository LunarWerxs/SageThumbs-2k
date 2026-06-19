//! OCR an image → text → clipboard, via the in-box WinRT `Windows.Media.Ocr`
//! engine. Zero bundled bytes (Windows ships the recognizer). Decoding is done by
//! the OS `BitmapDecoder`, so any format Windows can open works.
//!
//! Runs on a dedicated MTA thread and blocks the WinRT async via `pdf::block_op`
//! (same pattern as the PDF thumbnailer — windows-future's `.join()` is private).

use windows::core::{Error, Result, HSTRING};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::pdf::block_op;
use crate::verbs::read_capped;

/// Recognize text in the image at `path` and put it on the clipboard. Errors
/// (no text found, no OCR language pack, unreadable image) leave the clipboard
/// untouched.
pub fn ocr_to_clipboard(path: &str) -> Result<()> {
    let bytes = read_capped(path)?;
    let text = recognize_bytes(&bytes)?;
    if text.trim().is_empty() {
        return Err(Error::from(E_FAIL));
    }
    unsafe { copy_text_to_clipboard(&text) }
}

/// Recognize text in image `bytes` (used by `ocr_to_clipboard` and the test hook).
/// Runs on a fresh MTA thread (blocking `.get()`-style waits can deadlock in an
/// STA; we can't assume the shell host's apartment).
pub(crate) fn recognize_bytes(bytes: &[u8]) -> Result<String> {
    let owned = bytes.to_vec();
    std::thread::spawn(move || {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = recognize(&owned);
        if inited {
            unsafe { CoUninitialize() };
        }
        out
    })
    .join()
    .map_err(|_| {
        // Don't let a panic in the WinRT OCR worker vanish into a generic failure.
        crate::safety::log_debug("ocr: recognize worker thread panicked");
        Error::from(E_FAIL)
    })?
}

fn recognize(bytes: &[u8]) -> Result<String> {
    // Load the bytes into a WinRT stream and decode to a SoftwareBitmap.
    let stream = InMemoryRandomAccessStream::new()?;
    {
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(bytes)?;
        block_op(&writer.StoreAsync()?)?;
        writer.DetachStream()?;
    }
    stream.Seek(0)?;
    let decoder = block_op(&BitmapDecoder::CreateAsync(&stream)?)?;
    let bmp = block_op(&decoder.GetSoftwareBitmapConvertedAsync(
        BitmapPixelFormat::Bgra8,
        BitmapAlphaMode::Premultiplied,
    )?)?;

    // Use the user's languages; fall back to English if no profile language has
    // an OCR pack. (windows-rs maps WinRT null → Err, so or_else fires.)
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .or_else(|_| OcrEngine::TryCreateFromLanguage(&Language::CreateLanguage(&HSTRING::from("en"))?))?;

    let max = OcrEngine::MaxImageDimension()?; // static associated fn
    if bmp.PixelWidth()? as u32 > max || bmp.PixelHeight()? as u32 > max {
        return Err(Error::from(E_FAIL));
    }
    let result = block_op(&engine.RecognizeAsync(&bmp)?)?;
    Ok(result.Text()?.to_string())
}

/// Put UTF-16 `text` on the clipboard as CF_UNICODETEXT, via the one shared,
/// audited clipboard writer in `crate::clipboard`.
unsafe fn copy_text_to_clipboard(text: &str) -> Result<()> {
    let bytes = crate::clipboard::utf16_nul_bytes(text);
    if crate::clipboard::set_clipboard(crate::clipboard::CF_UNICODETEXT, &bytes) {
        Ok(())
    } else {
        Err(Error::from(E_FAIL))
    }
}
