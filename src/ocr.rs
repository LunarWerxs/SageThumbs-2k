//! OCR an image → text → clipboard, via the in-box WinRT `Windows.Media.Ocr`
//! engine. Zero bundled bytes (Windows ships the recognizer). Decoding is done by
//! the OS `BitmapDecoder`, so any format Windows can open works.
//!
//! Runs on a dedicated MTA thread and blocks the WinRT async via `pdf::block_op`
//! (same pattern as the PDF thumbnailer — windows-future's `.join()` is private).

use std::time::Duration;

use windows::core::{Error, Result, HSTRING};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapInterpolationMode, BitmapPixelFormat, BitmapTransform,
    ColorManagementMode, ExifOrientationMode,
};
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
    let text = recognize_bytes(read_capped(path)?)?;
    if text.trim().is_empty() {
        return Err(Error::from(E_FAIL));
    }
    unsafe { copy_text_to_clipboard(&text) }
}

/// Host-side budget for the whole OCR run (decode + recognize). The internal `block_op`
/// waits are each capped at ~30 s but several stack, so bound the total here. OCR is reached
/// only via `run_action_detached` (already off the UI thread + DLL pinned), but we apply the
/// same recv_timeout + ModuleRef discipline as every other detached WinRT worker so the path
/// is self-contained, not reliant on the caller's structure.
const OCR_TIMEOUT: Duration = Duration::from_secs(30);

/// The error [`recognize_bytes`] returns when the image is bigger than the recognizer's own
/// `OcrEngine::MaxImageDimension()` — i.e. "this picture is too large", NOT "the engine is
/// broken". It is a DISTINCT code (WIC's `WINCODEC_ERR_IMAGESIZEOUTOFRANGE`, which says exactly
/// that) rather than the blanket `E_FAIL` every other failure returns, so a UI caller can tell
/// the two apart: the screen-OCR window otherwise told the user to go install a language pack,
/// which is simply false advice for an oversized selection.
pub const OCR_IMAGE_TOO_LARGE: windows::core::HRESULT =
    windows::core::HRESULT(0x8898_2F05u32 as i32);

/// Recognize text in image `bytes` (used by `ocr_to_clipboard`, the screen-capture OCR in the
/// companion app, the `st2k ocr` CLI, and the test hook). Runs on a fresh MTA thread — blocking
/// `.get()`-style waits can deadlock in an STA and we can't assume the caller's apartment.
///
/// Takes the buffer BY VALUE and moves it onto that worker: every caller already owns a `Vec`
/// it never touches again, so a `&[u8]` signature would force a second full copy of the image
/// for nothing (and this runs in-process in the shell, where "for nothing" is a second
/// image-sized allocation).
///
/// Built on [`crate::safety::spawn_budgeted`] (shared with `previewhandler.rs`'s preview decode
/// and `propstore.rs`'s property probe): it holds a `ModuleRef` for the worker's whole run —
/// unlike those two, this call site has no concurrency-limiting slot of its own, so it needs no
/// RAII guard beyond what `spawn_budgeted` already provides — and returns `None` uniformly for
/// "the OS refused a new thread" and "the wall-clock deadline passed", both of which used to
/// return `Err(E_FAIL)` here anyway (the difference was only whether a debug line got logged;
/// now both log it, which additionally fixes the previously-SILENT spawn-failure case).
pub fn recognize_bytes(bytes: Vec<u8>) -> Result<String> {
    // Fresh MTA thread (blocking WinRT waits can deadlock in an STA; we can't assume the
    // caller's apartment).
    let out = crate::safety::spawn_budgeted("st2k-ocr-recognize", OCR_TIMEOUT, move || {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = recognize(&bytes);
        if inited {
            unsafe { CoUninitialize() };
        }
        out
    });
    match out {
        Some(out) => out,
        None => {
            crate::safety::log_debug(
                "ocr: recognize exceeded the wall-clock deadline or failed to start",
            );
            Err(Error::from(E_FAIL))
        }
    }
}

/// How much to enlarge a `w`x`h` image before recognition, given the engine's `max` dimension.
///
/// Small screen text is the case the engine fails outright on (see the call site), so aim to get
/// the image up to roughly [`OCR_TARGET_MIN_DIM`] on its short side, capped so neither side can
/// exceed `max` — the engine rejects anything past that, and we'd rather recognize at 2x than
/// fail at 3x. Returns 1 when the input is already big enough (or can't be grown).
fn upscale_factor(w: u32, h: u32, max: u32) -> u32 {
    if w == 0 || h == 0 {
        return 1;
    }
    let short = w.min(h);
    // Ceiling-divide so a 300px capture aiming at 900 asks for 3x, not 2x.
    let wanted = OCR_TARGET_MIN_DIM
        .div_ceil(short.max(1))
        .clamp(1, OCR_MAX_UPSCALE);
    // Largest factor that keeps BOTH sides inside the engine's limit.
    let fits = (max / w.max(1)).min(max / h.max(1));
    wanted.min(fits).max(1)
}

/// True when a decoded image of `pw`x`ph` is safe to hand to the WinRT engine: within
/// `max_dim` on both sides AND within this repo's own MAX_ALLOC byte budget for the eventual
/// Bgra8 materialization. Two independent caps because they bound different things — dimension
/// alone doesn't stop a huge-but-under-the-ceiling image from blowing the allocation budget (see
/// the call site). Pure so the policy is unit-testable without a live WinRT bitmap.
fn ocr_size_within_budget(pw: u32, ph: u32, max_dim: u32) -> bool {
    if pw == 0 || ph == 0 || pw > max_dim || ph > max_dim {
        return false;
    }
    (pw as u64) * (ph as u64) * 4 <= crate::decode::limits::MAX_ALLOC
}

/// Short-side pixel target for the pre-recognition upscale. 900 lifts the 9-11 pt UI text the
/// engine chokes on into the range it reads reliably, without inflating an already-large capture.
const OCR_TARGET_MIN_DIM: u32 = 900;
/// Never enlarge more than this. Measured: 2x already recovers small text fully and 3x/4x add
/// nothing, so this is a safety rail against turning a thin strip into a huge allocation.
const OCR_MAX_UPSCALE: u32 = 4;

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

    // Bomb guard, and it MUST come before the pixel decode. The header alone declares the
    // dimensions, so a few-KB file can claim 60000×60000 — and `GetSoftwareBitmapConvertedAsync`
    // would faithfully materialize that as BGRA8 (~14 GiB) before anything got to reject it.
    // `read_capped` only bounds the file's bytes on disk, and this WinRT path doesn't go through
    // decode.rs's MAX_DIM/MAX_ALLOC guards, so the check has to live here. It matters because
    // this runs IN-PROCESS in explorer.exe (Tools ▸ Copy text (OCR) on an untrusted file, via
    // `run_action_detached`) and the worker isn't killed when the caller's timeout fires — the
    // allocation would keep going after we'd given up on it.
    //
    // The engine's own `MaxImageDimension` (~26,210px) is NOT the right threshold on its own:
    // it is looser than this repo's own MAX_DIM (16384) and, more importantly, says nothing
    // about total allocation — 16384x16384 alone is already ~1.07 GiB of Bgra8, over twice this
    // repo's MAX_ALLOC (512 MiB) standard applied to every other decode path. Clamp the
    // dimension to MAX_DIM and separately enforce the byte budget explicitly.
    let engine_max = OcrEngine::MaxImageDimension()?; // static associated fn
    let max = engine_max.min(crate::decode::limits::MAX_DIM);
    let (pw, ph) = (decoder.PixelWidth()?, decoder.PixelHeight()?);
    if !ocr_size_within_budget(pw, ph, max) {
        return Err(Error::from(OCR_IMAGE_TOO_LARGE));
    }

    // UPSCALE SMALL INPUT BEFORE RECOGNIZING. This is not a nicety — the in-box engine simply
    // gives up on small glyphs. Measured on this machine with one line of Segoe UI on a dark
    // background: at 8/9/10 pt it returns the EMPTY STRING (not bad text — nothing at all), 11 pt
    // is shaky, 13 pt is clean. Upscaling the 9 and 10 pt cases 2x turned them into essentially
    // perfect transcriptions, and 3x/4x were no better than 2x. That matters because the whole
    // point of screen OCR is grabbing UI text, which is 9-11 pt on a normal display: without this
    // the feature looks broken on exactly its main use case (reported: words run together, chunks
    // missing, from a chat-window screenshot).
    //
    // `Fant` is WIC's high-quality (windowed-sinc-ish) downscaler/upscaler; plain Linear leaves
    // the anti-aliased stems mushier. `max` (the engine ceiling clamped to MAX_DIM, above) caps
    // how far we can go, so the factor is whatever fits — a big capture stays at 1x, which is
    // fine because a big capture already has big glyphs.
    let scale = upscale_factor(pw, ph, max);
    let bmp = if scale > 1 {
        let transform = BitmapTransform::new()?;
        transform.SetScaledWidth(pw * scale)?;
        transform.SetScaledHeight(ph * scale)?;
        transform.SetInterpolationMode(BitmapInterpolationMode::Fant)?;
        block_op(&decoder.GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )?)?
    } else {
        block_op(&decoder.GetSoftwareBitmapConvertedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
        )?)?
    };

    // Use the user's languages; fall back to English if no profile language has
    // an OCR pack. (windows-rs maps WinRT null → Err, so or_else fires.)
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().or_else(|_| {
        OcrEngine::TryCreateFromLanguage(&Language::CreateLanguage(&HSTRING::from("en"))?)
    })?;
    let result = block_op(&engine.RecognizeAsync(&bmp)?)?;

    // `OcrResult::Text()` joins every recognized line with a SPACE, so a captured dialog,
    // paragraph or code block comes back as one run-on line. Rebuild it from `Lines` so the
    // layout survives to the clipboard/stdout; fall back to `Text()` if the view is empty or
    // unreadable (single-line images land on the same string either way).
    let mut out = String::new();
    if let Ok(lines) = result.Lines() {
        let n = lines.Size().unwrap_or(0);
        for i in 0..n {
            if let Ok(text) = lines.GetAt(i).and_then(|l| l.Text()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&text.to_string());
            }
        }
    }
    if out.is_empty() {
        out = result.Text()?.to_string();
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A typical engine ceiling, so the tests read like the real thing.
    const MAX: u32 = 10_000;

    /// The whole point: small captures MUST be enlarged, because the engine returns the empty
    /// string on 8-10 pt text (measured — see the call site) and screen OCR is mostly UI text.
    #[test]
    fn small_captures_get_enlarged() {
        assert!(
            upscale_factor(820, 130, MAX) > 1,
            "a small region must scale up"
        );
        assert!(
            upscale_factor(300, 30, MAX) > 1,
            "a thin text strip must scale up"
        );
    }

    /// An already-large capture is left alone: its glyphs are big, and growing it wastes a
    /// multi-hundred-MB allocation (and can push it past the engine's limit).
    #[test]
    fn large_captures_are_untouched() {
        assert_eq!(
            upscale_factor(3840, 2160, MAX),
            1,
            "a 4K capture must stay 1x"
        );
        assert_eq!(
            upscale_factor(1200, 900, MAX),
            1,
            "already at the target short side"
        );
    }

    /// The factor must never push either side past the engine's own maximum — that's the
    /// difference between recognizing at 2x and failing outright with IMAGESIZEOUTOFRANGE.
    #[test]
    fn never_exceeds_the_engine_limit() {
        for (w, h, max) in [
            (1900, 40, 4096),
            (900, 100, 1000),
            (5000, 20, 10_000),
            (7, 3, 20),
        ] {
            let f = upscale_factor(w, h, max);
            assert!(f >= 1, "factor must never be zero ({w}x{h}, max {max})");
            assert!(
                w * f <= max && h * f <= max,
                "{w}x{h} scaled {f}x exceeds the engine limit {max}"
            );
        }
    }

    /// Degenerate input must not divide by zero or return a zero factor (which would ask WIC
    /// for a 0-pixel bitmap).
    #[test]
    fn degenerate_dimensions_are_safe() {
        assert_eq!(upscale_factor(0, 0, MAX), 1);
        assert_eq!(upscale_factor(0, 500, MAX), 1);
        assert_eq!(upscale_factor(500, 0, MAX), 1);
        // An image already larger than the engine allows is rejected before this is called, but
        // the arithmetic still must not produce 0.
        assert_eq!(upscale_factor(20_000, 20_000, MAX), 1);
    }

    /// The engine's own ~26,210px ceiling alone would accept this; this repo's tighter MAX_DIM
    /// (16384, passed here as `max_dim` the way the real call site clamps it) must not.
    #[test]
    fn ocr_size_guard_rejects_past_max_dim() {
        assert!(!ocr_size_within_budget(
            20_000,
            20_000,
            crate::decode::limits::MAX_DIM
        ));
    }

    /// The dimension cap alone is NOT enough: 16384x16384 sits exactly at MAX_DIM but its Bgra8
    /// materialization is ~1.07 GiB, over twice this repo's MAX_ALLOC (512 MiB) standard. This is
    /// the defect the fix closes — the old guard checked dimension only and would have let this
    /// large-but-under-the-ceiling allocation straight through to WinRT.
    #[test]
    fn ocr_size_guard_enforces_the_byte_budget_even_under_max_dim() {
        assert!(!ocr_size_within_budget(
            crate::decode::limits::MAX_DIM,
            crate::decode::limits::MAX_DIM,
            crate::decode::limits::MAX_DIM,
        ));
        // A modest capture comfortably clears both caps.
        assert!(ocr_size_within_budget(
            1920,
            1080,
            crate::decode::limits::MAX_DIM
        ));
    }

    /// Degenerate dimensions must be rejected outright, not treated as "fits everything".
    #[test]
    fn ocr_size_guard_rejects_degenerate_dims() {
        assert!(!ocr_size_within_budget(0, 100, MAX));
        assert!(!ocr_size_within_budget(100, 0, MAX));
    }
}
