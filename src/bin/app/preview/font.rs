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

/// Write `sfnt` to a fresh, exclusively-claimed `%TEMP%\st2k_woff_<pid>_<n>.ttf` and return its
/// path, or `None` if every attempt collided. `n` is a process-local counter (not the byte
/// length the old name used), so two same-size WOFFs previewed back to back can never fight
/// over one name.
///
/// `create_new`, never a plain `std::fs::write`: Windows' create-and-truncate follows hard
/// links and reparse points, so a pre-planted name in the shared, world-writable `%TEMP%`
/// directory would have the rebuilt font bytes written straight THROUGH it into whatever it
/// really points at. The name is predictable — the pid is public and the counter restarts at 0
/// each process — so refusing an existing name is the actual guard here, not the name's
/// obscurity. Mirrors `sagethumbs2k_core::decode::magick`'s `NamedTemp`, which solved the same
/// problem for its own coder staging; that struct is private to its crate/module so this is a
/// second, matching implementation rather than a shared one.
fn claim_temp_ttf(sfnt: &[u8]) -> Option<std::path::PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    const MAX_ATTEMPTS: u32 = 8;
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    for _ in 0..MAX_ATTEMPTS {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("st2k_woff_{pid}_{n}.ttf"));
        let Ok(mut file) = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)
        else {
            continue; // name already taken (or claimed by a concurrent attempt) — try the next
        };
        if file.write_all(sfnt).is_ok() {
            drop(file); // close before AddFontResourceExW opens it by name
            return Some(path);
        }
        drop(file);
        let _ = std::fs::remove_file(&path); // partial write (e.g. a full disk) — don't leak it
    }
    None
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

/// Locate the sfnt `name` table's offset, handling a `.ttc` collection by reading the
/// first font's table directory. `None` if the directory can't be read or has no `name`
/// table.
fn find_name_table(bytes: &[u8]) -> Option<usize> {
    let sfnt = if bytes.get(0..4) == Some(b"ttcf") {
        be32(bytes, 12)? as usize
    } else {
        0
    };
    let num_tables = be16(bytes, sfnt + 4)?;
    for i in 0..num_tables as usize {
        let e = sfnt + 12 + i * 16;
        if bytes.get(e..e + 4) == Some(b"name") {
            return Some(be32(bytes, e + 8)? as usize);
        }
    }
    None
}

/// Rank of a `name` record: lower is better. Full name (id 4) on the Windows/Unicode
/// platform (3) wins; family (id 1) on the same platform is next; any other platform's
/// full name, then anything else.
fn name_record_rank(name_id: u16, plat: u16) -> u8 {
    match (name_id, plat) {
        (4, 3) => 0,
        (1, 3) => 1,
        (4, _) => 2,
        _ => 3,
    }
}

/// One `name` record's read outcome: a malformed record aborts the WHOLE scan (matching
/// the original per-record `?` early return out of the enclosing function, since a name
/// table this corrupt can't be trusted further), an off-topic name id (not family/full
/// name) is skipped, and a good record yields `(platform, name id, decoded + trimmed
/// string)`.
enum NameRecordOutcome {
    Malformed,
    Skip,
    Value(u16, u16, String),
}

/// Read + decode one `name` record at byte offset `r`: UTF-16BE for the Unicode
/// platforms (3, 0), everything else as UTF-8-ish bytes.
fn read_name_record(bytes: &[u8], str_base: usize, r: usize) -> NameRecordOutcome {
    let Some(plat) = be16(bytes, r) else {
        return NameRecordOutcome::Malformed;
    };
    let Some(name_id) = be16(bytes, r + 6) else {
        return NameRecordOutcome::Malformed;
    };
    if name_id != 1 && name_id != 4 {
        return NameRecordOutcome::Skip;
    }
    let (Some(len), Some(off)) = (be16(bytes, r + 8), be16(bytes, r + 10)) else {
        return NameRecordOutcome::Malformed;
    };
    let (len, off) = (len as usize, off as usize);
    let Some(data) = bytes.get(str_base + off..str_base + off + len) else {
        return NameRecordOutcome::Malformed;
    };
    let s = if plat == 3 || plat == 0 {
        let u16s: Vec<u16> = data
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(data).into_owned()
    };
    NameRecordOutcome::Value(plat, name_id, s.trim().to_string())
}

/// Walk the `name` table at `nt` and keep the best-ranked family/full-name record.
fn best_name(bytes: &[u8], nt: usize) -> Option<String> {
    let count = be16(bytes, nt + 2)?;
    let str_base = nt + be16(bytes, nt + 4)? as usize;
    let mut best: Option<(u8, String)> = None; // (rank, name); lower rank = better
    for i in 0..count as usize {
        let r = nt + 6 + i * 12;
        let (plat, name_id, s) = match read_name_record(bytes, str_base, r) {
            NameRecordOutcome::Malformed => return None,
            NameRecordOutcome::Skip => continue,
            NameRecordOutcome::Value(plat, name_id, s) => (plat, name_id, s),
        };
        if s.is_empty() {
            continue;
        }
        let rank = name_record_rank(name_id, plat);
        if best.as_ref().is_none_or(|(br, _)| rank < *br) {
            best = Some((rank, s));
        }
    }
    best.map(|(_, s)| s)
}

/// Parse the sfnt `name` table for a human display name — full name (id 4), else family (id 1),
/// preferring the Windows/Unicode (platform 3, UTF-16BE) record. Handles a `.ttc` collection by
/// reading the first font. `None` if it can't be parsed.
fn display_name(bytes: &[u8]) -> Option<String> {
    let nt = find_name_table(bytes)?;
    best_name(bytes, nt)
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
            let tmp = claim_temp_ttf(&sfnt)?;
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

    /// The bug: `std::fs::write` truncates through an existing name (hard link or reparse
    /// point) instead of refusing it. `claim_temp_ttf` must claim a name EXCLUSIVELY — proven
    /// here by pre-creating the very first name it would try and confirming it lands on a
    /// different one instead of writing through the pre-existing file.
    #[test]
    fn claim_temp_ttf_never_writes_through_a_pre_existing_name() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        // The counter is a shared static across the whole test binary, so pin down the very
        // next name this call will try by pre-creating names 0..4 and confirming the claim
        // still succeeds on a fresh one within MAX_ATTEMPTS, never reusing any of them.
        let blocked: Vec<_> = (0..4)
            .map(|n| dir.join(format!("st2k_woff_{pid}_{n}.ttf")))
            .collect();
        for p in &blocked {
            if !p.exists() {
                std::fs::write(p, b"PRE-EXISTING, MUST NOT BE OVERWRITTEN").unwrap();
            }
        }

        let got = claim_temp_ttf(b"fake sfnt bytes").expect("a free name must be claimable");
        assert!(
            !blocked.contains(&got),
            "must not have claimed one of the pre-existing names: {got:?}"
        );
        assert_eq!(std::fs::read(&got).unwrap(), b"fake sfnt bytes");
        for p in &blocked {
            let content = std::fs::read_to_string(p).unwrap_or_default();
            assert_eq!(
                content, "PRE-EXISTING, MUST NOT BE OVERWRITTEN",
                "a pre-existing name must survive untouched: {p:?}"
            );
        }

        let _ = std::fs::remove_file(&got);
        for p in &blocked {
            let _ = std::fs::remove_file(p);
        }
    }
}
