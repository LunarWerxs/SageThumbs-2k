//! Loading + decode dispatch, window sizing/placement, follow-selection poll.


use windows::Win32::Foundation::{
    HWND, LPARAM, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::content::{self, RenderData};
use super::infocard;
use super::window::{ViewerState, ContentKind, state, is_pdf, image_dims, letterbox_bg, set_title, CAPTION_H, MIN_W, MIN_H, LOADING_W, LOADING_H, CARD_W, CARD_H, TEXT_W, TEXT_H, VIDEO_W, VIDEO_H, SCRUB_H, SHOW_TIMER_ID, SCRUB_TIMER_ID, ANIM_TIMER_ID, TOC_TIMER_ID, WM_APP_SWITCH};
#[cfg(feature = "html-preview")]
use super::window::content_rect;
use super::transport::video_rect; use super::toolbar::update_tooltips;

/// Switch the viewer to preview `path` (async decode). Resets the open grace window.
pub(super) unsafe fn load(hwnd: HWND, path: &str) {
    let st = &*state(hwnd);
    *st.path.borrow_mut() = Some(path.to_string());
    st.born.set(GetTickCount64());
    let gen = st.decode_gen.get() + 1;
    st.decode_gen.set(gen);
    *st.render.borrow_mut() = None;
    *st.card.borrow_mut() = None;
    *st.text.borrow_mut() = None;
    *st.video.borrow_mut() = None; // stop + tear down any previous video player
    #[cfg(feature = "html-preview")]
    {
        *st.webview.borrow_mut() = None; // close any previous WebView2 host
    }
    let _ = KillTimer(Some(hwnd), SCRUB_TIMER_ID);
    st.frames.borrow_mut().clear(); // drop any previous animation frames (frees their HBITMAPs)
    st.frame_delays.borrow_mut().clear();
    st.cur_frame.set(0);
    let _ = KillTimer(Some(hwnd), ANIM_TIMER_ID);
    st.pdf_page.set(0);
    st.pdf_pages.set(0);
    st.zoom.set(1.0); // reset zoom/pan/scroll for the new file
    st.pan.set((0, 0));
    st.text_scroll.set(0);
    // Clear the selection but NOT `sel_drag` — like `drag`/`scrub_drag`, the in-progress flag
    // must survive a mid-drag reload (←/→ nav, daemon push) so WM_LBUTTONUP still releases the
    // mouse capture. The drag continues harmlessly: with `sel` None it has nothing to extend.
    st.sel.set(None);
    st.line_starts.borrow_mut().clear(); // rebuilt lazily on the first hit-test
    st.md_hits.borrow_mut().clear(); // rebuilt by the next Markdown paint
    st.md_links.borrow_mut().clear(); // no stale link/outline/image state from the previous document
    st.md_toc.borrow_mut().clear();
    st.toc_hits.borrow_mut().clear();
    st.md_imgs.borrow_mut().clear(); // frees the previous document's image DIBs
    st.md_has_headings.set(false);
    st.toc_sel.set(None);
    st.toc_anim.set(None); // settle any mid-slide sidebar instantly for the new document
    let _ = KillTimer(Some(hwnd), TOC_TIMER_ID);
    // NOTE: `src_view` is deliberately NOT reset here — it's a sticky viewing mode for the window,
    // so flipping through a folder of .md files with ←/→ keeps showing source.
    st.src_capable.set(source_capable(&ext_of(path)));

    // "View source" is on and this file has a rendered view to toggle away from → show the raw
    // text instead. A failed/binary read falls through to the normal rendered path.
    if st.src_capable.get() && st.src_view.get() && show_source(hwnd, path) {
        return;
    }

    // Archives (zip/7z/rar-family with no cover): show a file listing in the text pane. Falls
    // through to normal classification if it isn't actually a recognized archive.
    if content::is_archive_ext(&ext_of(path)) {
        if let Some(listing) = content::archive_listing(path) {
            *st.text.borrow_mut() = Some(listing);
            st.kind.set(ContentKind::Text);
            ensure_shown(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
            set_title(hwnd);
            update_tooltips(hwnd, st.tip.get());
            return;
        }
    }
    // Font files: render a specimen (name + pangram + glyph sheet) as an image.
    if super::font::is_font_ext(&ext_of(path)) && render_font_to_state(&*state(hwnd), path) {
        ensure_shown(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
        set_title(hwnd);
        update_tooltips(hwnd, st.tip.get());
        return;
    }
    // HTML / .url: WebView2-hosted render (feature `html-preview`, gated behind Settings toggles).
    #[cfg(feature = "html-preview")]
    if try_load_web(hwnd, path) {
        return;
    }

    let kind = content::classify(path);
    match kind {
        ContentKind::Image => {
            st.kind.set(ContentKind::Loading);
            if is_pdf(path) {
                // PDF: render page 0 via the OS renderer + fetch the page count (for nav).
                content::spawn_decode_pdf(hwnd, path.to_string(), 0, gen);
            } else {
                content::spawn_decode(hwnd, path.to_string(), gen);
            }
            if st.shown.get() {
                let _ = InvalidateRect(Some(hwnd), None, false); // show "Loading" in the current window
            }
        }
        ContentKind::Video => {
            // Set the kind first (so client_size uses the video size), show the window (gives the
            // render child a parent + rect), then start playback into a child over the content.
            st.kind.set(ContentKind::Video);
            ensure_shown(hwnd);
            let cr = video_rect(hwnd); // render child leaves room for the scrub strip
            match super::video::create(hwnd, hwnd, &cr, st.hinst, path) {
                Some(p) => {
                    *st.video.borrow_mut() = Some(p);
                    SetTimer(Some(hwnd), SCRUB_TIMER_ID, 250, None); // repaint the scrub position
                }
                None => {
                    // Playback unavailable (codec/engine) → fall back to a still frame.
                    st.kind.set(ContentKind::Loading);
                    content::spawn_decode(hwnd, path.to_string(), gen);
                }
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        ContentKind::Text | ContentKind::Markdown => {
            // Text/markdown read is fast + small (5 MB cap), so resolve it synchronously.
            // Structured docs (CSV/TSV/ipynb) read UNtruncated so their parse sees the whole file.
            let read = if sagethumbs2k_core::formats::is_preview_doc(&ext_of(path)) {
                content::read_doc(path)
            } else {
                content::read_text(path)
            };
            match read {
                Some(mut t) => {
                    if kind == ContentKind::Markdown {
                        // CSV/TSV/ipynb convert to synthesized markdown first (see `docconv`),
                        // then one full parse at load — the paint path reads the cached flag.
                        if let Some(conv) = super::docconv::to_markdown(&ext_of(path), &t) {
                            t = conv.md;
                            seed_md_attachments(st, conv.attachments);
                        }
                        st.md_has_headings.set(super::markdown::has_headings(&t));
                        st.md_remote_ok.set(sagethumbs2k_core::settings::preview_md_remote_img());
                    }
                    *st.text.borrow_mut() = Some(t);
                    st.kind.set(kind);
                }
                None => {
                    // Unreadable / turned out binary → the calm card.
                    *st.card.borrow_mut() = Some(infocard::gather(path));
                    st.kind.set(ContentKind::InfoCard);
                }
            }
            ensure_shown(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        _ => {
            *st.card.borrow_mut() = Some(infocard::gather(path));
            st.kind.set(ContentKind::InfoCard);
            ensure_shown(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    set_title(hwnd);
    // The PDF pager may have just vanished (pdf_pages reset above), re-packing the buttons —
    // re-point the tooltip rects so they can't linger over the wrong button.
    update_tooltips(hwnd, st.tip.get());
}

/// Synchronous load for the headless shot: decode on this thread, size, place off-screen,
/// and show (invisible) so `PrintWindow` can capture it.
pub(super) unsafe fn load_sync(hwnd: HWND, path: Option<&str>, opts: &super::ShotOpts) {
    let st = &*state(hwnd);
    if let Some(path) = path {
        *st.path.borrow_mut() = Some(path.to_string());
        let cls = content::classify(path);

        if opts.play && matches!(cls, ContentKind::Video) {
            // Live engine so the transport strip renders (the video surface is a swap chain
            // PrintWindow can't read, so it stays black; the strip is parent GDI and captures).
            st.kind.set(ContentKind::Video);
            let (cw, ch) = client_size(hwnd);
            place(hwnd, cw, ch, Some((-32000, -32000)));
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            st.shown.set(true);
            if let Some(p) = super::video::create(hwnd, hwnd, &video_rect(hwnd), st.hinst, path) {
                *st.video.borrow_mut() = Some(p);
            }
            set_title(hwnd);
            if let Some(h) = opts.hot {
                st.hot.set(Some(h));
            }
            return; // already sized/shown
        } else if is_pdf(path) {
            // Render the requested page + the page count so the ◀ ▶ pager + "N / M" show.
            let pg = opts.pdf_page.unwrap_or(0);
            let done = std::fs::read(path)
                .ok()
                .and_then(|b| sagethumbs2k_core::pdf::render_page_counted(&b, pg, 1600))
                .and_then(|(png, count)| image::load_from_memory(&png).ok().map(|img| (img, count)))
                .map(|(img, count)| {
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    if let Some(hbmp) = content::make_dib(w, h, &rgba.into_raw(), letterbox_bg(st)) {
                        *st.render.borrow_mut() = Some(RenderData { hbmp, iw: w, ih: h });
                        st.kind.set(ContentKind::Image);
                        st.pdf_page.set(pg.min(count.saturating_sub(1)));
                        st.pdf_pages.set(count.min(1_000_000));
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if !done {
                load_static(st, path, ContentKind::Image);
            }
        } else if let (Some(fr), true) = (opts.frame, is_animatable(path)) {
            // Decode the animation and show a specific frame.
            let frames = std::fs::read(path)
                .ok()
                .and_then(|b| super::anim::decode_animation(&b, &ext_of(path)));
            if let Some(frames) = frames {
                let bg = letterbox_bg(st);
                let mut rds: Vec<RenderData> = Vec::new();
                for (d, _) in frames {
                    if let Some(hbmp) = content::make_dib(d.w, d.h, &d.rgba, bg) {
                        rds.push(RenderData { hbmp, iw: d.w, ih: d.h });
                    }
                }
                if !rds.is_empty() {
                    st.cur_frame.set(fr.min(rds.len() - 1));
                    *st.frames.borrow_mut() = rds;
                    st.kind.set(ContentKind::Image);
                }
            }
            if st.frames.borrow().is_empty() {
                load_static(st, path, ContentKind::Image); // not actually animated
            }
        } else {
            // Video (no --play) falls back to its still frame-grab (the Image path).
            let kind = match cls {
                ContentKind::Video => ContentKind::Image,
                k => k,
            };
            load_static(st, path, kind);
        }
    }
    set_title(hwnd);
    if let Some(h) = opts.hot {
        st.hot.set(Some(h));
    }
    let (cw, ch) = client_size(hwnd);
    // Off-screen so no flash; realized (SW_SHOWNOACTIVATE) so PrintWindow renders it.
    place(hwnd, cw, ch, Some((-32000, -32000)));
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    st.shown.set(true);
}

/// Synchronous still decode for the headless shot: image → DIB, text/markdown → read, else card.
pub(super) unsafe fn load_static(st: &ViewerState, path: &str, kind: ContentKind) {
    // "View source" (`--source`) — same gating as the async `load` path, minus the window ops.
    st.src_capable.set(source_capable(&ext_of(path)));
    if st.src_capable.get() && st.src_view.get() {
        if let Some(text) = content::read_text(path) {
            *st.text.borrow_mut() = Some(text);
            st.kind.set(ContentKind::Text);
            return;
        }
    }
    // Archive listing (zip/7z/rar-family) — shown in the text pane, same as the async `load` path.
    if content::is_archive_ext(&ext_of(path)) {
        if let Some(listing) = content::archive_listing(path) {
            *st.text.borrow_mut() = Some(listing);
            st.kind.set(ContentKind::Text);
            return;
        }
    }
    // Font specimen (same render as the async `load` path).
    if super::font::is_font_ext(&ext_of(path)) && render_font_to_state(st, path) {
        return;
    }
    match kind {
        ContentKind::Image => match content::decode_sync(path) {
            Some(d) => {
                if let Some(hbmp) = content::make_dib(d.w, d.h, &d.rgba, letterbox_bg(st)) {
                    *st.render.borrow_mut() = Some(RenderData { hbmp, iw: d.w, ih: d.h });
                    st.kind.set(ContentKind::Image);
                } else {
                    *st.card.borrow_mut() = Some(infocard::gather(path));
                    st.kind.set(ContentKind::InfoCard);
                }
            }
            None => {
                *st.card.borrow_mut() = Some(infocard::gather(path));
                st.kind.set(ContentKind::InfoCard);
            }
        },
        ContentKind::Text | ContentKind::Markdown => match if sagethumbs2k_core::formats::is_preview_doc(&ext_of(path)) {
            content::read_doc(path)
        } else {
            content::read_text(path)
        } {
            Some(mut t) => {
                if kind == ContentKind::Markdown {
                    if let Some(conv) = super::docconv::to_markdown(&ext_of(path), &t) {
                        t = conv.md;
                        seed_md_attachments(st, conv.attachments);
                    }
                    st.md_has_headings.set(super::markdown::has_headings(&t));
                    st.md_remote_ok.set(sagethumbs2k_core::settings::preview_md_remote_img());
                }
                *st.text.borrow_mut() = Some(t);
                st.kind.set(kind);
            }
            None => {
                *st.card.borrow_mut() = Some(infocard::gather(path));
                st.kind.set(ContentKind::InfoCard);
            }
        },
        _ => {
            *st.card.borrow_mut() = Some(infocard::gather(path));
            st.kind.set(ContentKind::InfoCard);
        }
    }
}

/// Decode a converted document's inline attachments (notebook `attachment:` images — bytes that
/// live inside the file) and pre-seed them into the image cache under their rewritten keys, so
/// the markdown paint finds them ready. Runs at load, off the paint path; failures just leave the
/// key absent (renders as an alt-text pill). The cache was cleared earlier this load.
unsafe fn seed_md_attachments(st: &ViewerState, attachments: Vec<(String, Vec<u8>)>) {
    if attachments.is_empty() {
        return;
    }
    let bg = crate::dark::SURFACE().0; // markdown content background
    let mut imgs = st.md_imgs.borrow_mut();
    for (key, bytes) in attachments {
        if let Some(rd) = super::markdown::decode_bytes_to_dib(&bytes, bg) {
            imgs.insert(key, super::markdown::ImgSlot::Ready(rd));
        }
    }
}

/// Render a font specimen for `path` and install it as the Image content (no window ops — the
/// caller sizes/shows). Returns false if the font can't be loaded/rendered.
unsafe fn render_font_to_state(st: &ViewerState, path: &str) -> bool {
    let bg = windows::Win32::Foundation::COLORREF(letterbox_bg(st));
    let fg = crate::dark::DARK_TEXT();
    if let Some((rgba, w, h)) = super::font::render_specimen(path, bg, fg) {
        if let Some(hbmp) = content::make_dib(w, h, &rgba, letterbox_bg(st)) {
            *st.render.borrow_mut() = Some(RenderData { hbmp, iw: w, ih: h });
            st.kind.set(ContentKind::Image);
            return true;
        }
    }
    false
}

/// Build an HTML/`.url` WebView2 preview when the ext + Settings toggle allow it. Returns true if
/// handled (webview created, a card shown, or the `.url` target shown as text). Falls through
/// (false) to show HTML source as text when the toggle is off or it isn't a web file.
#[cfg(feature = "html-preview")]
unsafe fn try_load_web(hwnd: HWND, path: &str) -> bool {
    let st = &*state(hwnd);
    match ext_of(path).as_str() {
        "html" | "htm" | "xhtml" => {
            if !sagethumbs2k_core::settings::preview_html() {
                return false; // show source as text instead
            }
            create_web(hwnd, &file_uri(path), super::webview::Mode::Local)
        }
        "url" | "webloc" => {
            let Some(target) = parse_url_shortcut(path) else { return false };
            if sagethumbs2k_core::settings::preview_url_live() {
                return create_web(hwnd, &target, super::webview::Mode::Live);
            }
            // Text-first (the safe default): show the parsed target; never auto-load.
            *st.text.borrow_mut() = Some(format!(
                "Web shortcut\n\n{target}\n\n(Turn on \"Live .url preview\" in Settings > Quick preview to load it.)"
            ));
            st.kind.set(ContentKind::Text);
            ensure_shown(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
            set_title(hwnd);
            update_tooltips(hwnd, st.tip.get());
            true
        }
        _ => false,
    }
}

/// Create the WebView2 host over the content area; on failure show a calm card. Always returns
/// true (the web file is "handled" either way).
///
/// SAFETY-CRITICAL: `webview::create` synchronously PUMPS the message loop while WebView2's async
/// environment/controller initialise, so the wndproc can re-enter during it. We set `busy` first so
/// close/switch requests are DEFERRED (see `request_close`/`request_load`), and after the pump we
/// RE-VALIDATE the window (it may have been destroyed) and re-fetch state before touching it — the
/// `st` from before the pump could be dangling.
#[cfg(feature = "html-preview")]
unsafe fn create_web(hwnd: HWND, url: &str, mode: super::webview::Mode) -> bool {
    {
        let st = &*state(hwnd);
        st.kind.set(ContentKind::Html);
        st.busy.set(true);
    }
    ensure_shown(hwnd); // realise the window so the child has a parent + size
    let cr = content_rect(hwnd);
    let host = super::webview::create(hwnd, &cr, url, mode); // PUMPS the message loop

    // The pump may have destroyed the window (close-while-loading) — never touch freed state.
    if !windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)).as_bool() {
        return true; // `host` drops here, closing the controller
    }
    let st = &*state(hwnd);
    st.busy.set(false);
    // Apply anything deferred during the pump. A close wins; a newer file-switch means our host is
    // stale (drop it and load the newer path instead of clobbering it).
    if st.pending_close.take() {
        drop(host);
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        return true;
    }
    // Take into a `let` FIRST: an `if let` scrutinee's `RefMut` lives for the whole block on
    // edition 2021, and `load` below can re-enter `create_web`, which pumps the message loop —
    // a switch request arriving during THAT pump writes `pending_path` (see `request_load`) and
    // would hit a BorrowMutError, which `panic=abort` turns into a dead viewer.
    let pending = st.pending_path.borrow_mut().take();
    if let Some(p) = pending {
        drop(host);
        load(hwnd, &p);
        return true;
    }
    match host {
        Some(h) => {
            *st.webview.borrow_mut() = Some(h);
            set_title(hwnd);
            update_tooltips(hwnd, st.tip.get());
        }
        None => {
            // Runtime missing / async failed → fall back to a calm card.
            let p = st.path.borrow().clone().unwrap_or_else(|| url.to_string());
            *st.card.borrow_mut() = Some(infocard::gather(&p));
            st.kind.set(ContentKind::InfoCard);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    true
}

/// Turn a local path into a `file:///` URI (forward slashes, minimal escaping of space/#/?).
#[cfg(feature = "html-preview")]
fn file_uri(path: &str) -> String {
    let esc = path.replace('\\', "/").replace(' ', "%20").replace('#', "%23").replace('?', "%3F");
    if esc.starts_with('/') {
        format!("file://{esc}")
    } else {
        format!("file:///{esc}")
    }
}

/// Parse a `.url`/`.webloc` shortcut for its target. `.url` is an INI (`URL=` under
/// `[InternetShortcut]`); `.webloc` is a plist with a `<string>` URL. `None` unless the scheme is
/// http(s) — so WebView2 never gets a `file:`/`javascript:` target from a shortcut.
#[cfg(feature = "html-preview")]
fn parse_url_shortcut(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let url = if path.to_ascii_lowercase().ends_with(".webloc") {
        let a = text.find("<string>")? + 8;
        let b = text[a..].find("</string>")? + a;
        text[a..b].trim().to_string()
    } else {
        text.lines()
            .find_map(|l| {
                let t = l.trim();
                t.strip_prefix("URL=").or_else(|| t.strip_prefix("url="))
            })?
            .trim()
            .to_string()
    };
    let low = url.to_ascii_lowercase();
    (low.starts_with("http://") || low.starts_with("https://")).then_some(url)
}

/// Whether `ext` names a file the viewer RENDERS from readable source — i.e. one with two
/// meaningful views, so the caption's `</>` toggle has something to switch between.
///
/// This mirrors the render gating in [`content::classify`] / [`try_load_web`] on purpose: with
/// "Render Markdown" off a `.md` is ALREADY shown as source, so offering a source toggle there
/// would be a button that visibly does nothing. Formats whose only view is source (`.rs`, `.json`)
/// and whose only view is rendered (a PNG, a video) are both excluded.
pub(super) fn source_capable(ext: &str) -> bool {
    use sagethumbs2k_core::{formats, settings};
    if formats::is_preview_markdown(ext) {
        return settings::preview_markdown();
    }
    if formats::is_preview_doc(ext) {
        // Same split `classify` uses: a notebook is a markdown document, CSV/TSV are text files.
        return if ext.eq_ignore_ascii_case("ipynb") {
            settings::preview_markdown()
        } else {
            settings::preview_text()
        };
    }
    // HTML only renders in the WebView2 build with the toggle on; otherwise it's already source.
    #[cfg(feature = "html-preview")]
    if matches!(ext, "html" | "htm" | "xhtml") {
        return settings::preview_html();
    }
    // SVG renders as an image (resvg) but is plain XML underneath.
    ext == "svg"
}

/// Show `path` as raw text (the "view source" branch of [`load`]). Returns false if the file
/// can't be read as text, so the caller can fall through to the rendered path rather than
/// stranding the viewer on an empty pane.
pub(super) unsafe fn show_source(hwnd: HWND, path: &str) -> bool {
    let st = &*state(hwnd);
    let Some(text) = content::read_text(path) else {
        return false;
    };
    *st.text.borrow_mut() = Some(text);
    st.kind.set(ContentKind::Text);
    ensure_shown(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
    set_title(hwnd);
    update_tooltips(hwnd, st.tip.get()); // the outline / PDF pager just went away
    true
}

/// Lowercase extension of `path` (no dot).
pub(super) fn ext_of(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Whether `path` is a frame-animatable format (GIF/APNG/animated WebP).
pub(super) fn is_animatable(path: &str) -> bool {
    matches!(ext_of(path).as_str(), "gif" | "png" | "apng" | "webp")
}

/// Show the window at the right size (first time) or resize to fit the current content
/// (subsequent switches keep the current position, per QuickLook's keep-anchored rule).
pub(super) unsafe fn ensure_shown(hwnd: HWND) {
    let st = &*state(hwnd);
    if st.shot {
        return;
    }
    // While full-screen (F11), a content switch must NOT resize the window back to fit-size — that
    // would leave a small borderless window at the old full-screen spot with the `fullscreen` flag
    // still set (desynced). Keep the full-screen geometry; the new content just repaints into it.
    if st.fullscreen.get().is_some() {
        let _ = InvalidateRect(Some(hwnd), None, false);
        return;
    }
    let (cw, ch) = client_size(hwnd);
    if st.shown.get() {
        place(hwnd, cw, ch, None); // keep position, just resize
    } else {
        let _ = KillTimer(Some(hwnd), SHOW_TIMER_ID);
        place(hwnd, cw, ch, center_on_cursor_monitor(cw, ch));
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); // never steals focus (plan §3)
        // Bring the window to the front of the z-order WITHOUT activating it — Explorer stays the
        // foreground window so its arrow-key selection keeps driving the follow-poll.
        //   * pinned (toolbar pin): genuinely always-on-top.
        //   * open-front (default): a plain HWND_TOP from this *background* process does NOT reliably
        //     beat Explorer's foreground window (it opened BEHIND it), so "bounce" through TOPMOST —
        //     which forces us above everything even from the background — then immediately drop back
        //     to non-topmost so the window can still be covered when you click elsewhere.
        //   * both off: leave it wherever it naturally landed.
        if st.pinned.get() {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        } else if st.open_front.get() {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        st.shown.set(true);
        // Follow the Explorer selection (arrows / clicks) — daemon mode only. A manual
        // `--preview <path>` shows that exact file and must not be hijacked by the selection.
        if !st.manual && !st.poll_started.get() {
            st.poll_started.set(true);
            start_poll(hwnd);
        }
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// The follow-selection poll: a dedicated thread (NEVER a `WM_TIMER` on the UI thread — the
/// `IShellWindows` automation marshals into explorer.exe and can stall) that re-resolves the
/// foreground selection every 500 ms and posts a switch when it changes. Exits when the viewer
/// window is gone. Mirrors QuickLook's `FocusMonitor`.
pub(super) fn start_poll(hwnd: HWND) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || unsafe {
        let mut last: Option<String> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            if !IsWindow(Some(hwnd)).as_bool() {
                break; // viewer closed — stop polling
            }
            if let Some(path) = crate::explorer_selection::preview_target() {
                // inits its own COM STA; post only when the selection actually changed
                if last.as_deref() != Some(path.as_str()) {
                    last = Some(path.clone());
                    let boxed = Box::into_raw(Box::new(path));
                    if PostMessageW(Some(hwnd), WM_APP_SWITCH, WPARAM(0), LPARAM(boxed as isize))
                        .is_err()
                    {
                        drop(Box::from_raw(boxed)); // window vanished mid-post — don't leak
                        break;
                    }
                }
            }
        }
    });
}

/// Compute the desired CLIENT size (device px) for the current content.
pub(super) unsafe fn client_size(hwnd: HWND) -> (i32, i32) {
    let st = &*state(hwnd);
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let cap = sc(CAPTION_H);
    match st.kind.get() {
        ContentKind::Image => {
            if let Some((rdw, rdh)) = image_dims(st) {
                let (_dpi, work) = crate::win::cursor_monitor_metrics();
                let cap_w = (work.right - work.left) * 80 / 100;
                let cap_h = (work.bottom - work.top) * 80 / 100 - cap;
                let mut scale = f64::min(cap_w as f64 / rdw as f64, cap_h as f64 / rdh as f64);
                if scale > 1.0 {
                    scale = 1.0; // never upscale past 100%
                }
                let iw = ((rdw as f64 * scale).round() as i32).max(1);
                let ih = ((rdh as f64 * scale).round() as i32).max(1);
                ((iw).max(sc(MIN_W)), (ih + cap).max(sc(MIN_H)))
            } else {
                (sc(LOADING_W), sc(LOADING_H))
            }
        }
        ContentKind::InfoCard => (sc(CARD_W), sc(CARD_H) + cap),
        ContentKind::Text | ContentKind::Markdown => (sc(TEXT_W), sc(TEXT_H)),
        ContentKind::Video => (sc(VIDEO_W), sc(VIDEO_H) + cap + sc(SCRUB_H)),
        ContentKind::Html => (sc(VIDEO_W), sc(VIDEO_H) + cap), // browser-ish default

        ContentKind::Loading => (sc(LOADING_W), sc(LOADING_H)),
    }
}

/// Resize (and optionally move) the window so its CLIENT area is `cw`×`ch`. `pos` = top-left
/// window position, or `None` to keep the current position.
pub(super) unsafe fn place(hwnd: HWND, cw: i32, ch: i32, pos: Option<(i32, i32)>) {
    let mut rc = RECT { left: 0, top: 0, right: cw, bottom: ch };
    let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
    let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
    let _ = AdjustWindowRectEx(&mut rc, style, false, ex);
    let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
    match pos {
        Some((x, y)) => {
            let _ = SetWindowPos(hwnd, None, x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        None => {
            let _ = SetWindowPos(hwnd, None, 0, 0, ww, wh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

/// Top-left position that centers a `cw`×`ch` client window on the cursor's monitor work area.
pub(super) unsafe fn center_on_cursor_monitor(cw: i32, ch: i32) -> Option<(i32, i32)> {
    let (_dpi, work) = crate::win::cursor_monitor_metrics();
    let x = work.left + (work.right - work.left - cw) / 2;
    let y = work.top + (work.bottom - work.top - ch) / 2;
    Some((x.max(work.left), y.max(work.top)))
}
