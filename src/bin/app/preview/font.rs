//! Font specimen preview. Fonts have no thumbnail/decode pipeline, so this is a self-contained
//! GDI render: parse the sfnt `name` table for a display name, load the file privately with
//! `AddFontResourceExW`, then draw the name + a pangram at several sizes + a glyph sheet into an
//! off-screen DIB (returned as RGBA for the Image path). Covers sfnt fonts (.ttf/.otf/.ttc/.otc)
//! and **.woff**, which is an sfnt with zlib-deflated tables and is unwrapped first (see
//! [`super::woff`]). WOFF2 is not covered: Brotli plus a `glyf`/`loca` transform is a font
//! library, not a header read.

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    AddFontResourceExW, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, FillRect, GdiFlush, GetDC, ReleaseDC, RemoveFontResourceExW,
    SelectObject, SetBkMode, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, FF_DONTCARE, FONT_QUALITY,
    FONT_RESOURCE_CHARACTERISTICS, FW_NORMAL, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_TT_PRECIS,
    TRANSPARENT,
};

/// Deletes the reconstructed-WOFF temp file however this function exits — including the
/// several `?` early returns between writing it and finishing the render.
struct TempFont(Option<std::path::PathBuf>);
impl Drop for TempFont {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// `FR_PRIVATE`: the font loads for THIS process only and is removed on `RemoveFontResourceExW`.
const FR_PRIVATE: FONT_RESOURCE_CHARACTERISTICS = FONT_RESOURCE_CHARACTERISTICS(0x10);

/// Extensions rendered as a font specimen.
pub(super) fn is_font_ext(ext: &str) -> bool {
    matches!(ext, "ttf" | "otf" | "ttc" | "otc" | "woff")
}

fn be16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]]))
}
fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parse the sfnt `name` table for a human display name — full name (id 4), else family (id 1),
/// preferring the Windows/Unicode (platform 3, UTF-16BE) record. Handles a `.ttc` collection by
/// reading the first font. `None` if it can't be parsed.
fn display_name(bytes: &[u8]) -> Option<String> {
    let sfnt = if bytes.get(0..4) == Some(b"ttcf") {
        be32(bytes, 12)? as usize
    } else {
        0
    };
    let num_tables = be16(bytes, sfnt + 4)?;
    let mut name_tbl = None;
    for i in 0..num_tables as usize {
        let e = sfnt + 12 + i * 16;
        if bytes.get(e..e + 4) == Some(b"name") {
            name_tbl = Some(be32(bytes, e + 8)? as usize);
            break;
        }
    }
    let nt = name_tbl?;
    let count = be16(bytes, nt + 2)?;
    let str_base = nt + be16(bytes, nt + 4)? as usize;
    let mut best: Option<(u8, String)> = None; // (rank, name); lower rank = better
    for i in 0..count as usize {
        let r = nt + 6 + i * 12;
        let plat = be16(bytes, r)?;
        let name_id = be16(bytes, r + 6)?;
        if name_id != 1 && name_id != 4 {
            continue;
        }
        let len = be16(bytes, r + 8)? as usize;
        let off = be16(bytes, r + 10)? as usize;
        let data = bytes.get(str_base + off..str_base + off + len)?;
        let s = if plat == 3 || plat == 0 {
            let u16s: Vec<u16> = data
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&u16s)
        } else {
            String::from_utf8_lossy(data).into_owned()
        };
        let s = s.trim().to_string();
        if s.is_empty() {
            continue;
        }
        let rank = match (name_id, plat) {
            (4, 3) => 0,
            (1, 3) => 1,
            (4, _) => 2,
            _ => 3,
        };
        if best.as_ref().is_none_or(|(br, _)| rank < *br) {
            best = Some((rank, s));
        }
    }
    best.map(|(_, s)| s)
}

/// Create a font at cap-height `px` in the given face.
unsafe fn face_font(face: &[u16], px: i32) -> HFONT {
    CreateFontW(
        -px,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_TT_PRECIS,
        Default::default(),
        FONT_QUALITY(CLEARTYPE_QUALITY.0),
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(face.as_ptr()),
    )
}

/// Render a specimen for the font at `path` into an RGBA buffer `(rgba, w, h)`. Loads the font
/// privately, draws with it, then unloads. `None` if the file can't be read/loaded.
pub(super) unsafe fn render_specimen(
    path: &str,
    bg: COLORREF,
    fg: COLORREF,
) -> Option<(Vec<u8>, i32, i32)> {
    // Bounded read: an unadorned `std::fs::read` had no ceiling at all, so a hostile
    // multi-GB file dropped on the preview would buffer wholesale before any font
    // parsing even started. Shares the same DoS budget every other by-path decode uses.
    let bytes = sagethumbs2k_core::decode::read_capped(path).ok()?;
    // A WOFF is an sfnt with deflated tables. Windows' loader only takes a PATH,
    // so the rebuilt font goes to a temp file that is deleted before we return.
    let unwrapped = super::woff::is_woff(&bytes).then(|| super::woff::to_sfnt(&bytes));
    let (bytes, temp) = match unwrapped {
        Some(Some(sfnt)) => {
            let tmp = std::env::temp_dir().join(format!(
                "st2k_woff_{}_{}.ttf",
                std::process::id(),
                bytes.len()
            ));
            std::fs::write(&tmp, &sfnt).ok()?;
            (sfnt, Some(tmp))
        }
        Some(None) => return None, // a WOFF we could not rebuild — show the info card
        None => (bytes, None),
    };
    let load_path = temp
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let _cleanup = TempFont(temp);

    let name = display_name(&bytes).unwrap_or_else(|| {
        std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Font".into())
    });

    let wpath = crate::win::wide(&load_path);
    if AddFontResourceExW(PCWSTR(wpath.as_ptr()), FR_PRIVATE, None) == 0 {
        return None;
    }
    let face = crate::win::wide(&name);

    // Off-screen 32bpp top-down DIB canvas.
    let (w, h) = (1000i32, 720i32);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let screen = GetDC(None);
    let mut bits: *mut c_void = core::ptr::null_mut();
    let dib: HBITMAP =
        match CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) if !bits.is_null() => b,
            _ => {
                let _ = ReleaseDC(None, screen);
                let _ = RemoveFontResourceExW(PCWSTR(wpath.as_ptr()), FR_PRIVATE.0, None);
                return None;
            }
        };
    let mdc: HDC = CreateCompatibleDC(Some(screen));
    let old = SelectObject(mdc, HGDIOBJ(dib.0));

    // Background.
    let brush = CreateSolidBrush(bg);
    FillRect(
        mdc,
        &RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        },
        brush,
    );
    let _ = DeleteObject(HGDIOBJ(brush.0));

    SetBkMode(mdc, TRANSPARENT);
    SetTextColor(mdc, fg);

    let pangram = "The quick brown fox jumps over the lazy dog";
    let sheet_upper = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let sheet_lower = "abcdefghijklmnopqrstuvwxyz";
    let sheet_digits = "0123456789  &@#$%(){}[]/\\?!.,;:";

    // Draw a run of text at cap-height `px`, advancing the y cursor. The font is created + freed
    // per line (cheap, keeps ownership simple).
    let draw = |y: &mut i32, px: i32, text: &str| {
        let f = face_font(&face, px);
        let prev = SelectObject(mdc, HGDIOBJ(f.0));
        let w16: Vec<u16> = text.encode_utf16().collect();
        let _ = TextOutW(mdc, 40, *y, &w16);
        SelectObject(mdc, prev);
        let _ = DeleteObject(HGDIOBJ(f.0));
        *y += px + px / 3 + 10;
    };

    let mut y = 36;
    draw(&mut y, 44, &name); // the font's own name, set in itself
    y += 8;
    draw(&mut y, 40, pangram);
    draw(&mut y, 30, pangram);
    draw(&mut y, 22, pangram);
    draw(&mut y, 16, pangram);
    y += 12;
    draw(&mut y, 30, sheet_upper);
    draw(&mut y, 30, sheet_lower);
    draw(&mut y, 30, sheet_digits);

    let _ = GdiFlush();

    // Read the DIB (BGRA, top-down) back as RGBA.
    let px = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        rgba[i * 4] = px[i * 4 + 2]; // R <- B
        rgba[i * 4 + 1] = px[i * 4 + 1]; // G
        rgba[i * 4 + 2] = px[i * 4]; // B <- R
        rgba[i * 4 + 3] = 255; // opaque
    }

    SelectObject(mdc, old);
    let _ = DeleteObject(HGDIOBJ(dib.0));
    let _ = DeleteDC(mdc);
    let _ = ReleaseDC(None, screen);
    let _ = RemoveFontResourceExW(PCWSTR(wpath.as_ptr()), FR_PRIVATE.0, None);

    Some((rgba, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `render_specimen`'s file read must be BOUNDED — a bare `std::fs::read` had no
    /// ceiling at all, so a hostile multi-hundred-MB file would buffer wholesale before
    /// any font parsing even started. Uses a sparse file (`set_len`, no real bytes
    /// written) so the test stays fast while still exercising the real size check; the
    /// oversized read must be refused before any GDI call, so this needs no live display.
    #[test]
    fn render_specimen_refuses_a_file_past_the_input_cap() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_font_cap_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.ttf");
        let file = std::fs::File::create(&path).unwrap();
        // 300 MiB — safely past the documented 256 MiB `decode::limits::MAX_INPUT_BYTES`
        // ceiling `read_capped` enforces (that constant is crate-internal to
        // `sagethumbs2k_core`, so it isn't named directly from this crate).
        file.set_len(300 * 1024 * 1024).unwrap();
        drop(file);

        let got = unsafe { render_specimen(path.to_str().unwrap(), COLORREF(0), COLORREF(0)) };
        assert!(
            got.is_none(),
            "an oversized font file must be refused, not read wholesale"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
