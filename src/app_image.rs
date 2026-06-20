//! EXE-only artwork/HBITMAP helpers for the companion app (Options/About/banner).
//!
//! These build premultiplied 32-bpp DIB-section `HBITMAP`s from app artwork and
//! remote-downloaded images for the Win32 EXEs (About box logo, Options banner,
//! ad/banner image). They live out of the crate root because the DLL never uses
//! them (LTO dead-strips them from the cdylib); the EXEs link them from the rlib
//! as `sagethumbs2k_core::app_image::*`. Pure relocation — see `crate::dib` for the
//! shared DIB builder.

use core::ffi::c_void;

use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP};

/// Reject attacker-influenced banner/cover art whose declared dimensions would
/// blow up the premultiplied-DIB allocation (each pixel is 4 bytes). The upstream
/// byte cap (4 MiB) bounds the *compressed* size, but a tiny payload can still
/// declare an enormous canvas, so probe dimensions before decoding. Reuses the
/// decode pipeline's single bomb-guard ceilings (`decode::limits`) so all paths
/// share one budget.
const REMOTE_ART_MAX_DIM: u32 = crate::decode::limits::MAX_DIM;
const REMOTE_ART_MAX_ALLOC: u64 = crate::decode::limits::MAX_ALLOC;

/// Cheaply read an image's declared dimensions without decoding pixels, and
/// reject anything past the bomb-guard limits. `Some(())` means "safe to decode".
fn remote_art_dims_ok(bytes: &[u8]) -> Option<()> {
    use std::io::Cursor;
    let (w, h) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if w > REMOTE_ART_MAX_DIM
        || h > REMOTE_ART_MAX_DIM
        || (w as u64 * h as u64 * 4) > REMOTE_ART_MAX_ALLOC
    {
        return None;
    }
    Some(())
}

/// RAII wrapper around a raw GDI `HBITMAP` handle so a fallible decode loop never
/// leaks the handles it created before a mid-loop failure. Call [`into_raw`] to
/// surrender ownership at the point of building a successful return value.
///
/// [`into_raw`]: OwnedHbitmap::into_raw
pub struct OwnedHbitmap(isize);

impl OwnedHbitmap {
    /// Surrender ownership, returning the raw handle. The caller (or the shell)
    /// is now responsible for `DeleteObject`; `Drop` will NOT run.
    pub fn into_raw(self) -> isize {
        let raw = self.0;
        core::mem::forget(self);
        raw
    }
}

impl Drop for OwnedHbitmap {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                let _ = DeleteObject(HBITMAP(self.0 as *mut c_void).into());
            }
        }
    }
}

/// Raw straight-RGBA pixels (top row first) → premultiplied 32-bpp DIB-section
/// HBITMAP handle. For app artwork the caller composites itself (e.g. the About
/// box's light-mode logo chip). None on failure or size mismatch.
pub fn rgba_to_hbitmap(w: u32, h: u32, rgba: &[u8]) -> Option<isize> {
    if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let hbmp = unsafe { crate::dib::create_premultiplied_dib(w as i32, h as i32, rgba) }.ok()?;
    Some(hbmp.0 as isize)
}

/// Decode image bytes (via the `image` crate — PNG/JPEG/GIF/…, not the full
/// thumbnail pipeline) resized to exactly `w`x`h`, into a premultiplied 32-bpp
/// DIB-section HBITMAP returned as a raw handle. For the fixed-size logo / banner
/// controls; also decodes a remote-downloaded image. The first frame is used for
/// animated formats (GIF). None on failure or a bomb-guard rejection.
pub fn image_to_hbitmap_sized(bytes: &[u8], w: u32, h: u32) -> Option<isize> {
    if w == 0 || h == 0 {
        return None;
    }
    // Bomb guard: probe the source canvas before `load_from_memory` allocates it.
    remote_art_dims_ok(bytes)?;
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.resize_exact(w, h, image::imageops::FilterType::Lanczos3).to_rgba8();
    let hbmp = unsafe { crate::dib::create_premultiplied_dib(w as i32, h as i32, rgba.as_raw()) }.ok()?;
    Some(OwnedHbitmap(hbmp.0 as isize).into_raw())
}

/// Decode an animated GIF into one `w`x`h` HBITMAP per frame plus the inter-frame
/// delay in ms (so the Options banner can animate it). Returns None if `bytes`
/// isn't a multi-frame GIF — the caller then uses the single-image path.
pub fn decode_gif_frames_sized(bytes: &[u8], w: u32, h: u32) -> Option<(Vec<isize>, u32)> {
    use image::codecs::gif::GifDecoder;
    use image::AnimationDecoder;
    use std::io::Cursor;

    if w == 0 || h == 0 {
        return None;
    }
    // Bomb guard: a tiny GIF can declare an enormous logical-screen / frame canvas;
    // probe dimensions before the decoder allocates a frame buffer.
    remote_art_dims_ok(bytes)?;
    // Hostile-input guard: a small GIF can still declare a huge frame count, and
    // each frame becomes a w×h premultiplied DIB. Take frames lazily and stop at a
    // cap rather than `collect_frames()` (which decodes them all up front).
    const MAX_FRAMES: usize = 256;
    let decoder = GifDecoder::new(Cursor::new(bytes)).ok()?;
    let mut frame_iter = decoder.into_frames();
    let mut frames = Vec::new();
    while frames.len() < MAX_FRAMES {
        match frame_iter.next() {
            Some(Ok(f)) => frames.push(f),
            _ => break, // end of stream or a decode error → stop here
        }
    }
    if frames.len() < 2 {
        return None; // single frame — not animated
    }
    let (n, d) = frames[0].delay().numer_denom_ms();
    let delay_ms = n.checked_div(d).map_or(100, |q| q.clamp(20, 1000));

    // Collect owned handles so an early `?` (a failed DIB) drops every HBITMAP
    // already created this loop instead of leaking it. Ownership is surrendered
    // to the caller only once all frames succeed.
    let mut handles: Vec<OwnedHbitmap> = Vec::with_capacity(frames.len());
    for frame in &frames {
        let img = image::DynamicImage::ImageRgba8(frame.buffer().clone());
        let rgba = img.resize_exact(w, h, image::imageops::FilterType::Triangle).to_rgba8();
        let hbmp = unsafe { crate::dib::create_premultiplied_dib(w as i32, h as i32, rgba.as_raw()) }.ok()?;
        handles.push(OwnedHbitmap(hbmp.0 as isize));
    }
    let raw: Vec<isize> = handles.into_iter().map(OwnedHbitmap::into_raw).collect();
    Some((raw, delay_ms))
}
