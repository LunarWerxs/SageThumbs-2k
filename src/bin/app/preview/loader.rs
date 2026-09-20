//! Loading + decode dispatch, window sizing/placement, follow-selection poll.

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::WindowsAndMessaging::*;
mod resolve;
use resolve::*;
mod syncload;
use syncload::*;
mod placement;
#[cfg(feature = "html-preview")]
mod web;
#[cfg(test)]
pub(super) use placement::clamp_remembered_size;
pub(super) use placement::{
    client_size, ensure_shown, forget_size, place, remember_size, should_size_for_loading,
};
pub(super) use resolve::{abandon_pending_prepare, apply_resolved, is_load_current, Resolved};
pub(super) use syncload::load_sync;
#[cfg(feature = "html-preview")]
pub(super) use web::is_web_route_ext;
#[cfg(feature = "html-preview")]
use web::*;

use super::content::{self, RenderData};
use super::hexview;
use super::infocard;
use super::transport::video_rect;
#[cfg(feature = "html-preview")]
use super::window::content_rect;
use super::window::{
    image_dims, is_pdf, letterbox_bg, set_title, state, ContentKind, ViewerState, ANIM_TIMER_ID,
    CAPTION_H, CARD_H, CARD_W, LOADING_H, LOADING_W, MIN_H, MIN_W, SCRUB_H, SCRUB_TIMER_ID,
    SHOW_TIMER_ID, TEXT_H, TEXT_W, TOC_TIMER_ID, VIDEO_H, VIDEO_W, WM_APP_SWITCH,
};

/// Switch the viewer to preview `path` (async decode). Resets the open grace window.
///
/// Shows the window in its Loading state immediately, before anything on `path` is read
/// (2026-09-05 audit, F10): everything past that used to run synchronously here, the file
/// sniff, archive listing, DB/mail markdown and the text/markdown read, which blocked the UI
/// thread on slow/stalled storage before the window could even appear, let alone respond. That
/// work now runs on a worker thread via [`spawn_prepare_load`] and lands back through
/// `WM_APP_LOAD_RESOLVED` ([`apply_resolved`]), fenced by generation exactly like the image
/// decode path (`content::spawn_decode`) already was.
pub(super) unsafe fn load(hwnd: HWND, path: &str) {
    let st = &*state(hwnd);
    let gen = reset_viewer_state(hwnd, st, path);

    // The per-extension Quick-preview blocklist (`settings::preview_blocked`, a SEPARATE list
    // from the File-types page's per-format thumbnail toggle — see its doc comment) is checked
    // before anything else: a blocked extension must never reach a decoder, not even a
    // classify() sniff. Falls through to the ordinary "can't render this" card, exactly like an
    // unsupported format already does, so a user-blocked format degrades the same way.
    if is_load_blocked(path) {
        dispatch_fallback_kind(hwnd, st, path);
        set_title(hwnd);
        super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
        return;
    }

    st.kind.set(ContentKind::Loading);
    // An already-shown window (an arrow-key step through a folder) must never be resized down
    // to the Loading box only to snap back once content lands (2026-09-05 audit, F10 review;
    // `client_size` sizes by `kind`, and `ensure_shown` resizes an already-shown window to
    // whatever that is). Only the very-first-open case wants the full show-and-size call; an
    // already-visible window just repaints to show the Loading state, exactly the pattern
    // `dispatch_image_kind` already uses below. See [`should_size_for_loading`].
    if should_size_for_loading(st.shown.get()) {
        ensure_shown(hwnd);
    } else {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }

    // Font specimens stay synchronous: the extension check is free, and unlike the branches
    // below a font read/parse is not a case the audit evidence names, kept out of the async
    // path to hold the diff to the cited hot paths (see the finding report).
    if try_show_font_specimen(hwnd, st, path) {
        return;
    }

    let view_source_active = st.src_capable.get() && st.src_view.get();

    // HTML / .url / .webloc: the web route must be decided HERE, on the UI thread, before
    // anything is handed to the worker (2026-09-05 audit, F10 adversarial review). WebView2
    // needs the STA (its creation already pumps the message loop via `create_web`'s
    // busy-deferral), so it can never move to a background thread the way the rest of this
    // function's work did. The old synchronous `load()` ran this exact check before its
    // `classify` call for the same reason; leaving it to `resolve_load`'s worker-side
    // `classify` is wrong because "html" (and an unregistered `.url`/`.webloc`) both sniff as
    // plain text once the Text toggle is on, so `resolve_by_content_kind` returns
    // `Resolved::TextOrMarkdown` and `apply_resolved_dispatch` (which still carries the
    // `try_load_web` call for the `Resolved::Dispatch` arm) never runs at all; every release
    // build (html-preview ships in every release EXE) then previewed HTML as raw source and a
    // web shortcut as its raw INI/plist instead of the card or live view. With the feature off
    // `decide_load_route` doesn't compile at all, so this whole block does not exist and
    // behaviour is unchanged: straight through to the worker's classify-based text/card
    // fall-through, exactly like before this audit's change.
    #[cfg(feature = "html-preview")]
    if decide_load_route(&ext_of(path), view_source_active) == LoadRoute::Web
        && try_load_web(hwnd, path)
    {
        return; // sets its own title/find-refresh (or defers via busy/pending)
    }

    spawn_prepare_load(hwnd, path.to_string(), gen, view_source_active);
}

/// Which route `load()` takes for a file whose extension is `ext`, decided BEFORE anything is
/// read (2026-09-05 audit, F10 adversarial review). Pure (extension + `view_source_active`
/// only, no settings/IO), so the ordering fix is unit-testable without a live window or a
/// WebView2 runtime. `Web` means the caller must evaluate `try_load_web` right now, on the UI
/// thread; `Worker` means hand off to the async prepare worker exactly as before. View-source
/// mode always wins over the web route (matches the pre-audit ordering, where `show_source` was
/// checked ahead of `try_load_web`), so the `{ }` toggle still shows raw HTML source rather than
/// the live render. `Web` is a NECESSARY step, not a guarantee of the final content kind:
/// `try_load_web` still falls through to the worker itself when the relevant Settings toggle
/// (`preview_html`/`preview_url_live`) is off, same as before this audit's change.
#[cfg(feature = "html-preview")]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LoadRoute {
    Web,
    Worker,
}

#[cfg(feature = "html-preview")]
pub(super) fn decide_load_route(ext: &str, view_source_active: bool) -> LoadRoute {
    if !view_source_active && matches!(ext, "html" | "htm" | "xhtml" | "url" | "webloc") {
        LoadRoute::Web
    } else {
        LoadRoute::Worker
    }
}

/// Reset all per-document viewer state ahead of loading `path`. Returns the new decode
/// generation id.
unsafe fn reset_viewer_state(hwnd: HWND, st: &ViewerState, path: &str) -> u64 {
    *st.path.borrow_mut() = Some(path.to_string());
    st.born.set(GetTickCount64());
    let gen = st.decode_gen.get() + 1;
    st.decode_gen.set(gen);
    // Tell any worker still running for an earlier file that nobody is waiting for it now.
    super::content::begin_generation(gen);
    abandon_pending_prepare();
    *st.render.borrow_mut() = None;
    *st.art.borrow_mut() = None; // drops the previous track's cover-art DIB
    *st.card.borrow_mut() = None;
    *st.text.borrow_mut() = None;
    *st.video.borrow_mut() = None; // stop + tear down any previous video player
    st.video_dims.set(None); // the next clip re-reports its own size at LOADEDMETADATA
    st.arrow_nav
        .set(sagethumbs2k_core::settings::preview_arrow_nav());
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
    // Dropping the old document also ends its session thread and releases the file, so a
    // reload never leaves a previous PDF parsed in the background.
    *st.pdf_doc.borrow_mut() = None;
    st.zoom.set(1.0); // reset zoom/pan/scroll for the new file
    st.full_pending.set(false); // any full-resolution request was for the PREVIOUS file
    st.pan.set((0, 0));
    st.text_scroll.set(0);
    st.scroll_hot.set(false);
    st.wheel_remainder.set(0);
    // Clear the selection but NOT any captured-input flag (`sel_drag`, `scroll_drag`,
    // `scroll_page_press`, etc.). A mid-drag reload (←/→ nav, daemon push) must leave capture
    // owned until WM_LBUTTONUP; each interaction continues harmlessly while content is unavailable.
    st.sel.set(None);
    super::find::on_document_changed(hwnd); // drop the cached search haystack + hits
    st.line_starts.borrow_mut().clear(); // rebuilt lazily on the first hit-test
    st.md_hits.borrow_mut().clear(); // rebuilt by the next Markdown paint
    st.md_links.borrow_mut().clear(); // no stale link/outline/image state from the previous document
    st.md_toc.borrow_mut().clear();
    st.toc_hits.borrow_mut().clear();
    st.md_imgs.borrow_mut().clear(); // frees the previous document's image DIBs
    st.md_has_headings.set(false);
    st.md_has_remote.set(false);
    st.toc_sel.set(None);
    st.toc_anim.set(None); // settle any mid-slide sidebar instantly for the new document
    let _ = KillTimer(Some(hwnd), TOC_TIMER_ID);
    // NOTE: `src_view` is deliberately NOT reset here — it's a sticky viewing mode for the window,
    // so flipping through a folder of .md files with ←/→ keeps showing source.
    st.src_capable.set(source_capable(&ext_of(path)));
    gen
}

/// Set `st`'s text + kind for an archive listing, the one state-setting body shared by the
/// async `load` path ([`apply_resolved`], via [`resolve_load`]) and the headless `load_static`
/// hook ([`try_static_archive_listing`]), which differ only in whether they also touch the window.
fn set_archive_listing_state(st: &ViewerState, listing: String) {
    *st.text.borrow_mut() = Some(listing);
    st.kind.set(ContentKind::Text);
}

/// Set `st`'s text + kind for a generated Markdown document (the DB schema view or the mail
/// headers/body view), the one state-setting body shared by both the async `load` path
/// ([`apply_resolved`], via [`resolve_load`]) and the headless `load_static` counterparts
/// ([`try_static_db_markdown`], [`try_static_mail_markdown`]). Both sources are generated text
/// with nothing remote in it, so `md_remote_ok` always stays off.
fn set_markdown_doc_state(st: &ViewerState, md: String) {
    *st.text.borrow_mut() = Some(md);
    st.md_has_headings.set(true);
    st.md_remote_ok.set(false);
    st.kind.set(ContentKind::Markdown);
}

/// Font files: render a specimen (name + pangram + glyph sheet) as an image.
unsafe fn try_show_font_specimen(hwnd: HWND, st: &ViewerState, path: &str) -> bool {
    if !(super::font::is_font_ext(&ext_of(path)) && render_font_to_state(st, path)) {
        return false;
    }
    ensure_shown(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
    set_title(hwnd);
    super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
    true
}

/// `ContentKind::Image` (including PDF): spawn the decode, then show the "Loading" state if
/// the window is already visible.
unsafe fn dispatch_image_kind(hwnd: HWND, st: &ViewerState, path: &str, gen: u64) {
    st.kind.set(ContentKind::Loading);
    if is_pdf(path) {
        // PDF: render page 0 via the OS renderer + fetch the page count (for nav).
        content::spawn_decode_pdf(hwnd, path.to_string(), 0, gen);
        // Second, and second on purpose: parsing the whole document for the
        // continuous view must never delay page one appearing. If it never lands
        // (encrypted, malformed, enormous) the viewer stays the single-page pager.
        super::pdfview::spawn_open(hwnd, path.to_string(), st.decode_gen.get());
    } else {
        content::spawn_decode(hwnd, path.to_string(), gen);
    }
    if st.shown.get() {
        let _ = InvalidateRect(Some(hwnd), None, false); // show "Loading" in the current window
    }
}

/// `ContentKind::Video`: show the window, then start playback into a child over the content
/// (falling back to a still frame when playback is unavailable).
unsafe fn dispatch_video_kind(hwnd: HWND, st: &ViewerState, path: &str, gen: u64) {
    // Set the kind first (so client_size uses the video size), show the window (gives the
    // render child a parent + rect), then start playback into a child over the content.
    st.kind.set(ContentKind::Video);
    ensure_shown(hwnd);
    let cr = video_rect(hwnd); // render child leaves room for the scrub strip
    match super::video::create(hwnd, hwnd, &cr, st.hinst, path, is_audio(path)) {
        Some(p) => {
            *st.video.borrow_mut() = Some(p);
            // Repaint the scrub position.
            SetTimer(Some(hwnd), SCRUB_TIMER_ID, 250, None);
            // Audio has no picture, so decode its embedded cover art for the backdrop.
            // The engine is already playing; this lands later and only repaints (see
            // `on_render`, which routes a decode arriving while the kind is still Video
            // into `art` rather than treating it as the content).
            if is_audio(path) {
                content::spawn_decode(hwnd, path.to_string(), gen);
            }
        }
        None => {
            // Playback unavailable (codec/engine) → fall back to a still frame.
            st.kind.set(ContentKind::Loading);
            content::spawn_decode(hwnd, path.to_string(), gen);
        }
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Show the fallback info card (unrecognized/unreadable content).
unsafe fn show_info_card(st: &ViewerState, path: &str) {
    *st.card.borrow_mut() = Some(infocard::gather(path));
    st.kind.set(ContentKind::InfoCard);
}

/// Show the "nothing to preview" card for a selection that resolved to
/// [`crate::explorer_selection::PreviewTarget::Virtual`] — Recycle Bin / This PC / any other
/// virtual-namespace item with no filesystem path behind it. `mod::run_preview` calls this
/// right after creating the window with NO initial path (so `load` never runs for this case;
/// there is no file to load), which is what makes this the one entry point outside `load`/
/// `load_static` that sets `st.card`/`st.kind` directly.
///
/// Reuses the SAME `ContentKind::InfoCard` state every other "can't render this" case shows,
/// so pressing Space on the Recycle Bin degrades exactly like the product already degrades
/// everywhere else, rather than a special-cased empty window (2026-09-08 QuickLook-parity
/// audit) — a real Recycle Bin/This PC browsable panel was rejected by the owner on 2026-08-07
/// and stays rejected; this is one card, not a panel.
///
/// OUT OF SCOPE (this file cannot construct an `InfoCard`; its fields are private to
/// `infocard.rs`, which is owned by another agent this session): `infocard.rs` needs a
/// `pub(super) fn virtual_item() -> InfoCard` that builds a card with no shell icon (there is
/// no real file to ask `SHGetFileInfoW` about) and locale-driven text — see the integrator
/// note left at this call site below.
pub(super) unsafe fn show_virtual_card(hwnd: HWND) {
    let st = &*state(hwnd);
    // NOTE for the integrator: replace this call once `infocard::virtual_item()` exists (see
    // the doc comment above). Suggested body:
    //     InfoCard {
    //         name: crate::i18n::t("ic_virtual_title"),
    //         detail: crate::i18n::t("ic_virtual_detail"),
    //         icon: None,
    //     }
    *st.card.borrow_mut() = Some(infocard::virtual_item());
    st.kind.set(ContentKind::InfoCard);
    ensure_shown(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
    set_title(hwnd);
    super::find::refresh(hwnd); // no text to search, but keeps this entry point symmetric
}

/// Any content kind with no dedicated view: the fallback info card.
unsafe fn dispatch_fallback_kind(hwnd: HWND, st: &ViewerState, path: &str) {
    show_info_card(st, path);
    ensure_shown(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

// ── async classify + read (2026-09-05 audit, F10) ──────────────────────────────────────────
//
// Everything below used to run synchronously in `load()`, on the UI thread, before the window
// could show or the message pump could turn: the archive listing, the DB/mail markdown, the
// view-source read, and `classify`'s own unknown-extension sniff, each capable of blocking on
// slow or stalled removable/network storage. `resolve_load` is the pure computation (touches
// only `path` + process-wide settings, never `ViewerState`/`HWND`) run on a worker thread by
// [`spawn_prepare_load`]; [`apply_resolved`] is the UI-thread half that used to be interleaved
// with the reads themselves. Image/Video/InfoCard carry no data of their own here, they
// already have their own async decode dispatch (`dispatch_image_kind`/`dispatch_video_kind`).

/// Off-screen so no flash; realized (SW_SHOWNOACTIVATE) so `PrintWindow` renders it.
unsafe fn show_offscreen(hwnd: HWND, st: &ViewerState) {
    let (cw, ch) = client_size(hwnd);
    place(hwnd, cw, ch, Some((-32000, -32000)));
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    st.shown.set(true);
}

/// Render a font specimen for `path` and install it as the Image content (no window ops — the
/// caller sizes/shows). Returns false if the font can't be loaded/rendered.
unsafe fn render_font_to_state(st: &ViewerState, path: &str) -> bool {
    let bg = windows::Win32::Foundation::COLORREF(letterbox_bg(st));
    let fg = crate::dark::DARK_TEXT();
    if let Some((rgba, w, h)) = super::font::render_specimen(path, bg, fg) {
        if let Some(hbmp) = content::make_dib(w, h, &rgba, letterbox_bg(st)) {
            // The font specimen is rendered onto the pane colour already, so it is opaque by
            // construction and wants the plain blit (and no checkerboard behind it).
            *st.render.borrow_mut() = Some(RenderData::opaque(hbmp, w, h));
            st.kind.set(ContentKind::Image);
            return true;
        }
    }
    false
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
    // .eml is genuine RFC-822/MIME plain text, gated the same as any other text preview. .msg's
    // raw bytes are an OLE compound file, not text, so it stays excluded — there is no source
    // view for it to show.
    if ext.eq_ignore_ascii_case("eml") {
        return settings::preview_text();
    }
    // SVG renders as an image (resvg) but is plain XML underneath.
    ext == "svg"
}

/// The database view for `path`, or `None` if it isn't a database file we preview (wrong
/// extension, the Text toggle is off, or the bytes aren't SQLite). One helper because both the
/// async `load` and the headless `load_static` must gate identically.
fn db_markdown(path: &str) -> Option<String> {
    if !super::dbdoc::is_db_ext(&ext_of(path)) || !sagethumbs2k_core::settings::preview_text() {
        return None;
    }
    super::dbdoc::to_markdown(path)
}

/// The email view for `path`, or `None` if it isn't mail we can parse (wrong extension,
/// Text toggle off, or the bytes aren't RFC-822/OLE) — same gate discipline as
/// [`db_markdown`], one helper so `load` and `load_static` cannot diverge.
fn mail_markdown(path: &str) -> Option<String> {
    if !super::mailmsg::is_mail_ext(&ext_of(path)) || !sagethumbs2k_core::settings::preview_text() {
        return None;
    }
    super::mailmsg::to_markdown(path)
}

/// The hex-dump view for `path`, or `None` if it isn't a case this module should touch — same
/// gate discipline as [`db_markdown`]/[`mail_markdown`]: the Text toggle must be on. Unlike
/// those two there is no extension test here: hex has no format of its own to recognize, it
/// only ever answers for whatever [`content::classify`] already gave up on (see
/// [`resolve_hex_or_card`] and its `load_static` twin, `apply_static_hex_or_card`).
fn hex_markdown(path: &str) -> Option<String> {
    if !sagethumbs2k_core::settings::preview_text() {
        return None;
    }
    hexview::to_markdown(path)
}

/// Whether `path` is refused outright by the Quick-preview extension blocklist
/// (`settings::preview_blocked`) — pulled out as its own function so `load`/`load_static`
/// can't diverge on how the extension is extracted, mirroring `db_markdown`/`mail_markdown`'s
/// shared-gate pattern above.
fn is_load_blocked(path: &str) -> bool {
    sagethumbs2k_core::settings::preview_blocked(&ext_of(path))
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

/// Whether `path` is an audio track. Audio and video share `ContentKind::Video` (one engine, one
/// transport strip), so this is what separates "has a picture of its own" from "needs the cover-art
/// backdrop". Reads the same `FORMATS` category table the rest of the app does, so a new audio
/// extension is covered the moment it is registered.
pub(super) fn is_audio(path: &str) -> bool {
    use sagethumbs2k_core::formats;
    matches!(formats::category(&ext_of(path)), formats::Category::Audio)
}

#[cfg(test)]
mod tests;
