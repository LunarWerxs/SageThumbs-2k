//! Inline images: resolving a src to a local file, decoding it, caching the DIB, and
//! drawing it (or a clickable pill when it cannot be shown).
//!
//! LOCAL sources only by default - a remote src is never fetched unless the user opts
//! in, because a preview silently reaching the network is a tracking pixel.

use super::*;
use std::path::{Component, Prefix};

/// Draw one image block: local src -> decoded DIB (cached per document, synchronous — the
/// extension gate keeps it on the fast pure-Rust tiers); remote src (opt-in toggle) -> async
/// fetch worker, pill until the posted result lands; failed/blocked -> alt-text pill. Returns
/// the y after the block.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(super) unsafe fn draw_image(
    hwnd: HWND,
    hdc: HDC,
    clip: &RECT,
    ib: &ImgBlock,
    x0: i32,
    y: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    imgs: &mut ImgCache,
    doc_dir: Option<&Path>,
    gen: u64,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    const MAX_IMAGES: usize = 24; // bound decode/fetch work per document
    if !imgs.contains_key(&ib.src) {
        if imgs.len() >= MAX_IMAGES {
            return pill_fallback(hwnd, hdc, ib, x0, y, full_w, c, links);
        }
        if is_remote_src(&ib.src) {
            // Only reachable when the remote-images toggle is ON (Builder pills them
            // otherwise). Fetch + decode OFF the paint thread; repaint installs the result.
            imgs.insert(ib.src.clone(), ImgSlot::Pending);
            crate::preview::content::spawn_md_img(hwnd, ib.src.clone(), gen);
        } else {
            let slot = match load_img(&ib.src, doc_dir, c.bg) {
                Some(rd) => ImgSlot::Ready(rd),
                None => ImgSlot::Failed,
            };
            imgs.insert(ib.src.clone(), slot);
        }
    }
    let Some(ImgSlot::Ready(rd)) = imgs.get(&ib.src) else {
        return pill_fallback(hwnd, hdc, ib, x0, y, full_w, c, links);
    };
    let mut dw = match ib.width {
        ImgW::Natural => sc(rd.iw),
        ImgW::Px(p) => sc(p),
        ImgW::Pct(p) => (full_w as i64 * (p.min(100)) as i64 / 100) as i32,
    };
    dw = dw.clamp(1, full_w);
    let dh = ((dw as i64 * rd.ih as i64) / rd.iw.max(1) as i64).max(1) as i32;
    let x = if ib.center {
        x0 + (full_w - dw) / 2
    } else {
        x0
    };
    // Blit only when the destination intersects the pane (layout still advances offscreen).
    if y + dh >= clip.top && y <= clip.bottom {
        let memdc = CreateCompatibleDC(Some(hdc));
        let old = SelectObject(memdc, rd.hbmp.into());
        SetStretchBltMode(hdc, HALFTONE);
        let _ = StretchBlt(hdc, x, y, dw, dh, Some(memdc), 0, 0, rd.iw, rd.ih, SRCCOPY);
        SelectObject(memdc, old);
        let _ = DeleteDC(memdc);
    }
    if let Some(url) = &ib.link {
        links.push(LinkHit {
            rect: RECT {
                left: x,
                top: y,
                right: x + dw,
                bottom: y + dh,
            },
            url: url.clone(),
        });
    }
    y + dh + sc(12)
}

/// Alt-text pill for an image we won't/can't decode (remote, failed, over caps).
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(super) unsafe fn pill_fallback(
    hwnd: HWND,
    hdc: HDC,
    ib: &ImgBlock,
    x0: i32,
    y: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let label = if ib.alt.trim().is_empty() {
        "image"
    } else {
        ib.alt.trim()
    };
    let label = label.replace(' ', "\u{00A0}"); // one unbroken pill token
    let runs = [Run {
        text: format!("\u{00A0}{label}\u{00A0}"),
        bold: false,
        italic: false,
        code: true,
        strike: false,
        link: ib.link.clone(),
    }];
    let fonts = Fonts::new(hwnd, 13, false, false);
    let ctx = ctx_for(hwnd, c, c.muted);
    // Synthesized label, not part of the document — no selection wiring.
    let (ny, _) = run_block(
        hdc,
        &runs,
        &fonts,
        x0,
        y,
        full_w,
        if ib.center { 1 } else { 0 },
        false,
        &ctx,
        links,
        None,
    );
    fonts.free();
    ny + sc(8)
}

/// Resolve a (non-remote) image src against the document folder, percent-decoding and dropping
/// any `?query`/`#fragment` suffix. `None` unless the result canonicalises to a real path that
/// is still under `dir` — a src with nowhere to be confined to (`dir` is `None`) never resolves.
///
/// A `p` that is rooted, or that carries a Windows path-prefix component, is rejected outright
/// rather than joined — per `PathBuf::push`'s own documented replace rules, ALL three shapes
/// make `Path::join` bypass `dir` instead of confining `p` under it:
/// - drive-absolute (`C:\other\x.png`, prefix + root) replaces `dir` entirely;
/// - drive-RELATIVE (`C:x.png`, prefix, no root) also replaces `dir` entirely;
/// - root-only (`\other\x.png` or a `\\`/`//` UNC prefix, either spelling — Rust's Windows path
///   parser recognises both) replaces everything but `dir`'s own prefix (its drive letter, if
///   any), landing outside `dir` all the same, and for a UNC prefix specifically would make the
///   eventual `fs::read` open an outbound SMB connection to an attacker-named host.
///
/// `..` components have no such shortcut through `join`, so they are instead caught by the
/// canonicalize-and-confine check below, which also closes a symlink-based escape.
pub(super) fn resolve_src(src: &str, dir: Option<&Path>) -> Option<PathBuf> {
    let s = src.split(['?', '#']).next().unwrap_or("");
    if s.is_empty() {
        return None;
    }
    let s = percent_decode(s);
    let s = s.strip_prefix("./").unwrap_or(&s);
    let p = Path::new(s);
    if p.has_root() || p.components().any(|c| matches!(c, Component::Prefix(_))) {
        return None;
    }
    let dir = dir?;
    let canon_dir = std::fs::canonicalize(dir).ok()?;
    let canon = std::fs::canonicalize(dir.join(p)).ok()?;
    if canon.starts_with(&canon_dir) {
        Some(canon)
    } else {
        None
    }
}

/// True when `path`'s file stem (its name with the extension stripped) is a reserved Windows
/// device name — `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`. Windows resolves
/// `NUL.png` to the NUL device regardless of the trailing extension, so a crafted README could
/// otherwise point an `<img>` at a device instead of a file on the synchronous paint path.
fn is_dos_device_stem(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Minimal %XX decoder (image paths with spaces). Byte-wise throughout — a `&str` slice here
/// would panic (=abort) on a multibyte char straddling the %XX window (e.g. `"%é"`).
pub(super) fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode a LOCAL image for inline display: known-fast formats only (the pure-Rust/resvg tiers —
/// never WIC/magick, this runs on the paint path), bounded size, downscaled to a display cap,
/// composited over the pane bg into a DIB. Returns `None` (-> pill) on any miss.
pub(super) unsafe fn load_img(src: &str, dir: Option<&Path>, bg: u32) -> Option<RenderData> {
    // Remote is NEVER fetched (privacy). This includes UNC paths (`\\server\…` / `//server/…`):
    // fs::read on one opens an SMB connection to an attacker-named host — an outbound network
    // hit (and NTLM handshake) triggered by merely previewing a hostile README.
    if src.starts_with("\\\\")
        || src.starts_with("//")
        || src.contains("://")
        || src.starts_with("data:")
    {
        return None;
    }
    // Notebook `attachment:` refs are served from the pre-seeded cache, never the filesystem —
    // reject the scheme so a decode-miss can't try `<dir>/…attachment:name` (an NTFS alternate
    // data stream on Windows).
    if src.contains("attachment:") {
        return None;
    }
    let path = resolve_src(src, dir)?;
    // Belt-and-suspenders alongside `resolve_src`'s own prefix rejection: check the resolved,
    // now-canonical path's actual `Prefix` component rather than a raw string spelling, since
    // `fs::canonicalize` renders every Windows path (local or UNC) in its `\\?\`-prefixed
    // verbatim form, where a naive `starts_with("\\\\")`/`starts_with("//")` would match a
    // perfectly normal LOCAL path too. `Prefix::kind()` tells local and network apart directly,
    // and it recognises a UNC prefix under either separator spelling.
    if matches!(
        path.components().next(),
        Some(Component::Prefix(p))
            if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    ) {
        return None; // a relative src must not join into a UNC target either
    }
    if is_dos_device_stem(&path) {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "jfif" | "gif" | "webp" | "bmp" | "svg" | "svgz" | "ico" | "apng"
    ) {
        return None;
    }
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 32 * 1024 * 1024 {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    decode_bytes_to_dib(&bytes, bg)
}

/// Decode already-in-memory image bytes (a notebook attachment / a fetched remote image) to a
/// display-capped DIB composited over `bg`. Shared by the local-file, remote-fetch, and
/// notebook-attachment paths. `None` on any decode/alloc failure.
pub(crate) unsafe fn decode_bytes_to_dib(bytes: &[u8], bg: u32) -> Option<RenderData> {
    let img = sagethumbs2k_core::decode::decode_preview(bytes).ok()?;
    // Bound the cached DIB (README art displays ≤ content width; 2048 keeps HiDPI crisp).
    // `reduce_to_fit` never enlarges, so it carries its own no-op case.
    let img = sagethumbs2k_core::decode::reduce_to_fit(img, 2048, 4096);
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    let hbmp = crate::preview::content::make_dib(w, h, rgba.as_raw(), bg)?;
    Some(RenderData::opaque(hbmp, w, h))
}

// ---- inline run layout -------------------------------------------------------------------

#[cfg(test)]
mod resolve_src_tests {
    use super::*;

    /// A throwaway `doc/` folder with one legitimate sibling image and one secret file one
    /// level up. Unique per call (process id plus a call counter — these tests run as
    /// concurrent threads in the same process, so the pid alone is not enough to keep them
    /// from colliding on the same directory).
    fn scratch_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("st2k_imgtest_{}_{n}", std::process::id()));
        let doc = root.join("doc");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::write(doc.join("ok.png"), b"x").unwrap();
        std::fs::write(root.join("secret.png"), b"x").unwrap();
        root
    }

    /// A src that stays inside the document folder resolves, canonicalised.
    #[test]
    fn resolve_src_allows_a_sibling_of_the_document() {
        let root = scratch_dir();
        let dir = root.join("doc");
        let got = resolve_src("ok.png", Some(&dir)).unwrap();
        assert_eq!(got, std::fs::canonicalize(dir.join("ok.png")).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `..` walks the src out of the document folder — refused, not silently resolved to the
    /// escaped file.
    #[test]
    fn resolve_src_refuses_a_traversal_out_of_the_document_folder() {
        let root = scratch_dir();
        let dir = root.join("doc");
        assert!(resolve_src("../secret.png", Some(&dir)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A drive-relative src (`C:x.png`, no root separator) would make `Path::join` silently
    /// REPLACE `dir` per its own documented semantics — refused before it ever reaches `join`.
    #[test]
    fn resolve_src_refuses_a_drive_relative_src() {
        let root = scratch_dir();
        let dir = root.join("doc");
        assert!(resolve_src("C:secret.png", Some(&dir)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A drive-absolute src points straight past `dir` — refused.
    #[test]
    fn resolve_src_refuses_a_drive_absolute_src() {
        let root = scratch_dir();
        let dir = root.join("doc");
        assert!(resolve_src("C:\\Windows\\x.png", Some(&dir)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A rooted-but-no-prefix src (`\windows\x.png`) would make `Path::join` replace
    /// everything but `dir`'s own drive prefix, per its documented semantics — refused.
    #[test]
    fn resolve_src_refuses_a_root_only_src() {
        let root = scratch_dir();
        let dir = root.join("doc");
        assert!(resolve_src("\\windows\\x.png", Some(&dir)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A UNC src, either slash spelling, is refused — the pre-decode string guard in
    /// `load_img` normally catches these first, but `resolve_src` itself must refuse them too.
    #[test]
    fn resolve_src_refuses_unc_srcs_both_spellings() {
        let root = scratch_dir();
        let dir = root.join("doc");
        assert!(resolve_src("\\\\attacker\\share\\x.png", Some(&dir)).is_none());
        assert!(resolve_src("//attacker/share/x.png", Some(&dir)).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With no document directory to confine a relative src to, nothing resolves — not even a
    /// drive-absolute src, which used to bypass `dir` entirely.
    #[test]
    fn resolve_src_resolves_nothing_without_a_document_dir() {
        assert!(resolve_src("ok.png", None).is_none());
        assert!(resolve_src("C:\\Windows\\x.png", None).is_none());
    }

    /// Reserved Windows device names are refused regardless of the extension pasted onto them.
    #[test]
    fn is_dos_device_stem_matches_case_insensitively_with_any_extension() {
        assert!(is_dos_device_stem(Path::new("NUL.png")));
        assert!(is_dos_device_stem(Path::new("con.jpg")));
        assert!(is_dos_device_stem(Path::new("COM3.gif")));
        assert!(is_dos_device_stem(Path::new("LPT9.bmp")));
        assert!(!is_dos_device_stem(Path::new("normal.png")));
        assert!(!is_dos_device_stem(Path::new("console.png")));
    }
}
