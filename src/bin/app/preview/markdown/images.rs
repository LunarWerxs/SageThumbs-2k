//! Inline images: resolving a src to a local file, decoding it, caching the DIB, and
//! drawing it (or a clickable pill when it cannot be shown).
//!
//! LOCAL sources only by default - a remote src is never fetched unless the user opts
//! in, because a preview silently reaching the network is a tracking pixel.

use super::*;

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
/// any `?query`/`#fragment` suffix.
pub(super) fn resolve_src(src: &str, dir: Option<&Path>) -> Option<PathBuf> {
    let s = src.split(['?', '#']).next().unwrap_or("");
    if s.is_empty() {
        return None;
    }
    let s = percent_decode(s);
    let s = s.strip_prefix("./").unwrap_or(&s);
    let p = Path::new(s);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        dir.map(|d| d.join(p))
    }
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
    if path.as_os_str().to_string_lossy().starts_with("\\\\") {
        return None; // a relative src must not join into a UNC target either
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
    let img = if img.width() > 2048 || img.height() > 4096 {
        img.thumbnail(2048, 4096)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    let hbmp = crate::preview::content::make_dib(w, h, rgba.as_raw(), bg)?;
    Some(RenderData::opaque(hbmp, w, h))
}

// ---- inline run layout -------------------------------------------------------------------
