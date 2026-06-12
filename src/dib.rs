//! Build the 32bpp top-down premultiplied-BGRA DIB section the shell wants.
//!
//! `IThumbnailProvider::GetThumbnail` requires the returned `HBITMAP` to be a
//! DIB *section* (CreateDIBSection), 32bpp. Memory order is B,G,R,A; for
//! `WTSAT_ARGB` the color channels must be premultiplied by alpha. A negative
//! `biHeight` makes the bitmap top-down (positive would render upside-down).

use core::ffi::c_void;
use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP,
};

/// `rgba` is straight (non-premultiplied) RGBA8, row-major, top row first,
/// `width * height * 4` bytes. Returns an owned `HBITMAP`; on the success path
/// the shell takes ownership and `DeleteObject`s it.
pub unsafe fn create_premultiplied_dib(width: i32, height: i32, rgba: &[u8]) -> Result<HBITMAP> {
    if width <= 0 || height <= 0 {
        return Err(Error::from(E_FAIL));
    }
    let px = (width as usize) * (height as usize);
    if rgba.len() < px * 4 {
        return Err(Error::from(E_FAIL));
    }

    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = width;
    bmi.bmiHeader.biHeight = -height; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        return Err(Error::from(E_FAIL));
    }

    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    for i in 0..px {
        let r = rgba[i * 4];
        let g = rgba[i * 4 + 1];
        let b = rgba[i * 4 + 2];
        let a = rgba[i * 4 + 3];
        let m = |c: u8| (((c as u16) * (a as u16) + 127) / 255) as u8;
        dst[i * 4] = m(b);
        dst[i * 4 + 1] = m(g);
        dst[i * 4 + 2] = m(r);
        dst[i * 4 + 3] = a;
    }

    Ok(hbmp)
}
