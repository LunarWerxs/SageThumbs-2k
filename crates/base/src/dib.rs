//! Build the 32bpp top-down premultiplied-BGRA DIB section the shell wants.
//!
//! `IThumbnailProvider::GetThumbnail` requires the returned `HBITMAP` to be a
//! DIB *section* (CreateDIBSection), 32bpp. Memory order is B,G,R,A; for
//! `WTSAT_ARGB` the color channels must be premultiplied by alpha. A negative
//! `biHeight` makes the bitmap top-down (positive would render upside-down).

use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, GetDC, GetDIBits, GetObjectW, ReleaseDC, SelectObject,
    SetStretchBltMode, StretchBlt, BITMAP, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, SRCCOPY,
};

/// `rgba` is straight (non-premultiplied) RGBA8, row-major, top row first,
/// `width * height * 4` bytes. Returns an owned `HBITMAP`; on the success path
/// the shell takes ownership and `DeleteObject`s it.
///
/// # Safety
///
/// Must be called on a thread where GDI calls are legal, and the returned `HBITMAP` is an
/// OWNED handle: the caller either hands it to the shell (which deletes it) or `DeleteObject`s
/// it itself, exactly once. Every bounds check this needs is inside - a `rgba` shorter than
/// `width * height * 4` is rejected rather than read past - so the unsafety is the GDI handle
/// contract, not the slice.
pub unsafe fn create_premultiplied_dib(width: i32, height: i32, rgba: &[u8]) -> Result<HBITMAP> {
    if width <= 0 || height <= 0 {
        return Err(Error::from(E_FAIL));
    }
    // Checked, mirroring `safety::composite_rgba_over_bg`'s guard: `width`/`height` are i32 and
    // this crate only ever targets x64/arm64 (64-bit `usize`), so `px * 4` cannot actually
    // overflow for any positive i32 pair today — but the twin function next to this one
    // already pays for the check, and letting the two drift apart is how a future decoder
    // change (or a target retarget) would silently reintroduce the gap here first.
    let px = (width as usize)
        .checked_mul(height as usize)
        .ok_or(Error::from(E_FAIL))?;
    let total_bytes = px.checked_mul(4).ok_or(Error::from(E_FAIL))?;
    if rgba.len() < total_bytes {
        return Err(Error::from(E_FAIL));
    }

    let (hbmp, bits) = crate::safety::create_dib_section(width, height)?;

    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, total_bytes);
    for i in 0..px {
        let r = rgba[i * 4];
        let g = rgba[i * 4 + 1];
        let b = rgba[i * 4 + 2];
        let a = rgba[i * 4 + 3];
        // Hoist the two common cases (bit-exact with the divide below): a fully
        // opaque pixel premultiplies to itself, a fully transparent one to zero —
        // skipping three integer divides for the bulk of a typical thumbnail.
        if a == 255 {
            dst[i * 4] = b;
            dst[i * 4 + 1] = g;
            dst[i * 4 + 2] = r;
            dst[i * 4 + 3] = 255;
        } else if a == 0 {
            dst[i * 4] = 0;
            dst[i * 4 + 1] = 0;
            dst[i * 4 + 2] = 0;
            dst[i * 4 + 3] = 0;
        } else {
            let m = |c: u8| (((c as u16) * (a as u16) + 127) / 255) as u8;
            dst[i * 4] = m(b);
            dst[i * 4 + 1] = m(g);
            dst[i * 4 + 2] = m(r);
            dst[i * 4 + 3] = a;
        }
    }

    Ok(hbmp)
}

/// The pixels of `hbmp` as straight RGBA8, top row first, with its width and height: the
/// inverse of [`create_premultiplied_dib`], for a bitmap another thumbnail provider returned.
/// `premultiplied` is how to read the alpha byte, from the provider's `WTS_ALPHATYPE`:
/// `Some(true)` for `WTSAT_ARGB` (colour premultiplied by it), `Some(false)` for `WTSAT_RGB`
/// (opaque whatever the byte holds), and `None` for `WTSAT_UNKNOWN`, which the shell detects
/// itself: premultiplied when any alpha byte is non-zero, opaque when every one is zero.
/// `None` when GDI cannot read the bitmap.
///
/// # Safety
///
/// `hbmp` must be a valid bitmap that is not selected into a device context. It is only read.
pub unsafe fn read_rgba(hbmp: HBITMAP, premultiplied: Option<bool>) -> Option<(Vec<u8>, u32, u32)> {
    let mut bm = BITMAP::default();
    let got = GetObjectW(
        hbmp.into(),
        core::mem::size_of::<BITMAP>() as i32,
        Some(core::ptr::addr_of_mut!(bm).cast()),
    );
    let (w, h) = (bm.bmWidth, bm.bmHeight.checked_abs()?);
    if got == 0 || w <= 0 || h <= 0 {
        return None;
    }
    let mut buf = vec![0u8; (w as usize).checked_mul(h as usize)?.checked_mul(4)?];
    let mut bmi = crate::safety::top_down_bmi(w, h);
    let screen = GetDC(None);
    let rows = GetDIBits(
        screen,
        hbmp,
        0,
        h as u32,
        Some(buf.as_mut_ptr().cast()),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, screen);
    if rows != h {
        return None;
    }
    let premultiplied =
        premultiplied.unwrap_or_else(|| buf.as_chunks::<4>().0.iter().any(|p| p[3] != 0));
    for px in buf.as_chunks_mut::<4>().0.iter_mut() {
        let a = if premultiplied { px[3] } else { 255 };
        let un = |c: u8| match a {
            0 | 255 => c,
            _ => ((c as u16 * 255 + a as u16 / 2) / a as u16).min(255) as u8,
        };
        *px = [un(px[2]), un(px[1]), un(px[0]), a];
    }
    Some((buf, w as u32, h as u32))
}

/// `HALFTONE`-scaled `SRCCOPY` blit of the whole `src` (`width`, `height`) of `hbmp` into the
/// `dst` rectangle (`x`, `y`, `width`, `height`) on `hdc`, through a scratch memory DC that is
/// selected back out and deleted afterwards. Shared by the Explorer preview pane and the app's
/// Quick preview, which draw the same way.
///
/// # Safety
/// `hdc` must be a valid device context and `hbmp` a valid bitmap not selected into another DC.
pub unsafe fn stretch_blit(hdc: HDC, dst: (i32, i32, i32, i32), hbmp: HBITMAP, src: (i32, i32)) {
    let (dx, dy, dw, dh) = dst;
    let memdc = CreateCompatibleDC(Some(hdc));
    let old = SelectObject(memdc, hbmp.into());
    SetStretchBltMode(hdc, HALFTONE);
    let _ = StretchBlt(
        hdc,
        dx,
        dy,
        dw,
        dh,
        Some(memdc),
        0,
        0,
        src.0,
        src.1,
        SRCCOPY,
    );
    SelectObject(memdc, old);
    let _ = DeleteDC(memdc);
}

/// Swap the red and blue channels of a 32bpp raster and force every pixel opaque, copying
/// `src` into `dst`.
///
/// GDI hands a DIB section's bits back as `B,G,R,A` and every `image::RgbaImage` in this repo
/// wants `R,G,B,A`, so BGRA -> RGBA and RGBA -> BGRA are the SAME operation - swap channels 0
/// and 2 - which is why one function serves both directions. Four call sites had pasted the
/// identical four-line loop: `safety::composite_rgba_over_bg`'s fully-opaque fast path,
/// `contextmenu::paint`'s menu-preview capture, the preview font specimen's read-back, and the
/// `previewhandler_shot` example.
///
/// ⚠ The `_opaque` in the name is load-bearing: this writes 255 into every alpha byte and
/// discards whatever was there. Anything that must honour transparency premultiplies instead
/// (`create_premultiplied_dib` above, `preview::content::dib`) and must NOT call this.
///
/// Converts `min(src.len(), dst.len()) / 4` pixels and returns that count, so a caller whose
/// two buffers disagree is truncated rather than panicking.
pub fn swap_rb_opaque(src: &[u8], dst: &mut [u8]) -> usize {
    let px = src.len().min(dst.len()) / 4;
    for i in 0..px {
        dst[i * 4] = src[i * 4 + 2];
        dst[i * 4 + 1] = src[i * 4 + 1];
        dst[i * 4 + 2] = src[i * 4];
        dst[i * 4 + 3] = 255;
    }
    px
}

/// Serialize the fixed `#[repr(C)]` BITMAPINFOHEADER prologue of a packed CF_DIB (40 bytes,
/// little-endian, no padding) onto the end of `dib`. `header` is the caller's already-computed
/// `size_of::<BITMAPINFOHEADER>()`, which it keeps for its own capacity and length checks.
/// Positive `h` = bottom-up DIB (the CF_DIB convention). Shared by the clipboard verb's
/// `build_dib` and the screenshot tool's copy.
pub fn push_cf_dib_header(dib: &mut Vec<u8>, w: i32, h: i32, header: usize) {
    dib.extend_from_slice(&(header as u32).to_le_bytes()); // biSize
    dib.extend_from_slice(&w.to_le_bytes()); // biWidth
    dib.extend_from_slice(&h.to_le_bytes()); // biHeight (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_rb_opaque_swaps_red_and_blue_and_forces_alpha() {
        let src = [1u8, 2, 3, 0, 10, 20, 30, 128];
        let mut dst = [0u8; 8];
        assert_eq!(swap_rb_opaque(&src, &mut dst), 2);
        assert_eq!(dst, [3, 2, 1, 255, 30, 20, 10, 255]);
        // Its own inverse: a second pass restores the colour channels.
        let mut back = [0u8; 8];
        assert_eq!(swap_rb_opaque(&dst, &mut back), 2);
        assert_eq!(&back[..3], &src[..3]);
    }

    #[test]
    fn swap_rb_opaque_truncates_to_the_shorter_buffer() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = [9u8; 4];
        assert_eq!(swap_rb_opaque(&src, &mut dst), 1);
        assert_eq!(dst, [3, 2, 1, 255]);
        let mut big = [9u8; 12];
        assert_eq!(swap_rb_opaque(&dst, &mut big), 1);
        assert_eq!(&big[4..], &[9u8; 8]);
    }

    /// Mirrors `safety::composite_rgba_over_bg`'s overflow-guard test (`safety/dib_tests.rs`):
    /// huge dims must be rejected via checked arithmetic, not silently wrap. On this crate's x64/arm64-only
    /// target `px * 4` cannot actually overflow for any positive `i32` pair (`i32::MAX`
    /// squared times 4 stays under `u64::MAX`), so the length check already caught this
    /// input before the `checked_mul` above existed - it is defense-in-depth, not a
    /// behavior change. This test locks in the shared contract regardless.
    /// A bitmap another provider handed the shell comes back as the picture it shows: channel
    /// order and the premultiplied alpha undone, so a badge drawn on it and written back with
    /// `create_premultiplied_dib` leaves the rest of the picture as it was (to within rounding).
    #[test]
    fn read_rgba_gives_back_what_create_premultiplied_dib_wrote() {
        let rgba = [
            200u8, 100, 50, 255, 200, 100, 50, 128, 10, 20, 30, 0, 0, 255, 0, 64,
        ];
        unsafe {
            let hbmp = create_premultiplied_dib(2, 2, &rgba).expect("dib");
            let as_argb = read_rgba(hbmp, Some(true));
            let as_opaque = read_rgba(hbmp, Some(false));
            // WTSAT_UNKNOWN: alpha bytes are set, so it reads as premultiplied, as WTSAT_ARGB.
            let as_unknown = read_rgba(hbmp, None);
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(hbmp.into());
            let zeros = create_premultiplied_dib(1, 1, &[10, 20, 30, 0]).expect("dib");
            let unknown_all_zero = read_rgba(zeros, None);
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(zeros.into());
            assert_eq!(as_unknown, as_argb, "WTSAT_UNKNOWN with alpha set");
            let (px, w, h) = as_argb.expect("read back");
            assert_eq!((w, h), (2, 2));
            let pairs = px.as_chunks::<4>().0.iter().zip(rgba.as_chunks::<4>().0);
            for (i, (got, want)) in pairs.enumerate() {
                assert_eq!(got[3], want[3], "alpha of pixel {i}");
                if want[3] != 0 {
                    for c in 0..3 {
                        assert!(
                            got[c].abs_diff(want[c]) <= 2,
                            "pixel {i}: {got:?} vs {want:?}"
                        );
                    }
                }
            }
            // An opaque provider's bitmap: the alpha byte is ignored, every pixel is solid.
            let opaque = as_opaque.expect("read back").0;
            assert!(opaque.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
            // WTSAT_UNKNOWN with every alpha byte zero: an opaque picture, not an invisible one.
            let zeros = unknown_all_zero.expect("read back").0;
            assert_eq!(zeros[3], 255, "WTSAT_UNKNOWN with no alpha");
        }
    }

    #[test]
    fn create_premultiplied_dib_rejects_bad_dims_without_panicking() {
        unsafe {
            assert!(create_premultiplied_dib(i32::MAX, i32::MAX, &[0u8; 4]).is_err());
            assert!(create_premultiplied_dib(0, 5, &[0u8; 4]).is_err());
            assert!(create_premultiplied_dib(5, 0, &[0u8; 4]).is_err());
        }
    }
}
