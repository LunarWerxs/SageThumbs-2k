//! Loading + decode dispatch, window sizing/placement, follow-selection poll.

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::content::{self, RenderData};
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

    st.kind.set(ContentKind::Loading);
    ensure_shown(hwnd);

    // Font specimens stay synchronous: the extension check is free, and unlike the branches
    // below a font read/parse is not a case the audit evidence names, kept out of the async
    // path to hold the diff to the cited hot paths (see the finding report).
    if try_show_font_specimen(hwnd, st, path) {
        return;
    }

    let view_source_active = st.src_capable.get() && st.src_view.get();
    spawn_prepare_load(hwnd, path.to_string(), gen, view_source_active);
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

/// The outcome of [`resolve_load`], carried from the worker thread to the UI thread in a
/// `WM_APP_LOAD_RESOLVED` payload. `Send`: every field is owned data, never a GDI handle or a
/// `ViewerState` reference. GDI object creation stays on the UI thread, as it already does for
/// an image decode (`content::spawn_decode` posts raw RGBA, never an HBITMAP).
pub(super) enum Resolved {
    SourceText(String),
    Archive(String),
    DbMarkdown(String),
    MailMarkdown(String),
    TextOrMarkdown {
        kind: ContentKind,
        text: String,
        attachments: Vec<(String, Vec<u8>)>,
        /// `(has_headings, has_remote_images, remote_images_allowed)`, set only for Markdown.
        md_flags: Option<(bool, bool, bool)>,
    },
    /// Neither of the above matched: `classify`'s verdict, for the existing per-kind dispatch
    /// (which may still need the html-preview check, or the image/video decode worker).
    Dispatch(ContentKind),
}

/// Whether a `WM_APP_LOAD_RESOLVED` completion tagged `gen` should still be applied against the
/// window's CURRENT decode generation. `false` means the user already switched files/selections
/// while this completion was in flight, so it must never paint over whatever loaded after it
/// (2026-09-05 audit, F10 acceptance). Strict equality, not `gen >= current` or `gen <= current`:
/// `decode_gen` only ever increases, so a completion is current for exactly the one load it was
/// started for, matching the equivalent check `on_render` already uses for the image path.
pub(super) fn is_load_current(gen: u64, current: u64) -> bool {
    gen == current
}

/// `resolve_load`'s view-source branch: read `path` as text if view-source is active for it.
/// `None` means "not source view, or the read failed": either way the caller falls through to
/// the normal rendered path, exactly like the old `show_source` returning `false` did.
fn resolve_source_text(path: &str, view_source_active: bool) -> Option<Resolved> {
    if !view_source_active {
        return None;
    }
    content::read_text(path).map(Resolved::SourceText)
}

/// `resolve_load`'s tail: classify `path`, then read text/markdown content on the SAME worker
/// so the UI thread applies the whole result in one step (image/PDF/video need no read here,
/// they already decode asynchronously once dispatched).
fn resolve_by_content_kind(path: &str) -> Resolved {
    match content::classify(path) {
        kind @ (ContentKind::Text | ContentKind::Markdown) => resolve_text_or_markdown(path, kind),
        kind => Resolved::Dispatch(kind),
    }
}

/// The `Text`/`Markdown` arm of [`resolve_by_content_kind`]. `kind` is `classify`'s verdict
/// (already gated on the Text/Markdown Settings toggles, not re-derived here). Structured docs
/// (CSV/TSV/ipynb) read UNtruncated so their parse sees the whole file. A read failure
/// (unreadable / turned out binary) falls back to the info card, exactly like the old
/// synchronous dispatch did.
fn resolve_text_or_markdown(path: &str, kind: ContentKind) -> Resolved {
    let ext = ext_of(path);
    let read = if sagethumbs2k_core::formats::is_preview_doc(&ext) {
        content::read_doc(path)
    } else {
        content::read_text(path)
    };
    let Some(mut t) = read else {
        return Resolved::Dispatch(ContentKind::InfoCard);
    };
    let mut attachments = Vec::new();
    let mut md_flags = None;
    if kind == ContentKind::Markdown {
        // CSV/TSV/ipynb convert to synthesized markdown first (see `docconv`), then one full
        // parse here, the paint path reads the cached flags this computes.
        if let Some(conv) = super::docconv::to_markdown(&ext, &t) {
            t = conv.md;
            attachments = conv.attachments;
        }
        md_flags = Some((
            super::markdown::has_headings(&t),
            super::markdown::has_remote_images(&t),
            sagethumbs2k_core::settings::preview_md_remote_img(),
        ));
    }
    Resolved::TextOrMarkdown {
        kind,
        text: t,
        attachments,
        md_flags,
    }
}

/// The pure computation behind the async load: everything `load()` used to decide and read
/// directly, in the same order (view-source, archive, DB, mail, then classify + text read).
/// Touches only `path` and process-wide settings, never `ViewerState`/`HWND`, so it is safe
/// to run on a worker thread; [`apply_resolved`] applies the result back on the UI thread.
fn resolve_load(path: &str, view_source_active: bool) -> Resolved {
    if let Some(r) = resolve_source_text(path, view_source_active) {
        return r;
    }
    let ext = ext_of(path);
    if content::is_archive_ext(&ext) {
        if let Some(listing) = content::archive_listing(path) {
            return Resolved::Archive(listing);
        }
    }
    if let Some(md) = db_markdown(path) {
        return Resolved::DbMarkdown(md);
    }
    if let Some(md) = mail_markdown(path) {
        return Resolved::MailMarkdown(md);
    }
    resolve_by_content_kind(path)
}

/// The still-running prepare worker's ticket, if the load it was started for hasn't been
/// superseded yet. Mirrors `content::LIVE_GEN`'s "tell the old worker nobody is waiting"
/// bookkeeping, but for the process-wide abandoned-worker BUDGET rather than the paint fence:
/// this worker posts back through `PostMessageW`, never through a receiver the caller can time
/// out on, so `safety::AbandonTicket` is how the budget learns it might still be blocked in I/O
/// on a dead share (2026-09-05 audit, F10), same shape as `contextmenu::thumb`'s `MenuThumbJob`.
static PENDING_PREPARE: std::sync::Mutex<Option<sagethumbs2k_core::safety::AbandonTicket>> =
    std::sync::Mutex::new(None);

/// Mark any still-outstanding prepare worker as abandoned. Called at the start of every new
/// load ([`reset_viewer_state`]), right next to `content::begin_generation`'s equivalent step.
fn abandon_pending_prepare() {
    let prev = PENDING_PREPARE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(ticket) = prev {
        ticket.caller_gave_up();
    }
}

/// Post a `resolve_load` result to the UI thread, reclaiming the box if the window died first.
unsafe fn post_resolved(hwnd: HWND, gen: u64, resolved: Resolved) {
    let payload: Box<(u64, Resolved)> = Box::new((gen, resolved));
    let raw = Box::into_raw(payload);
    if PostMessageW(
        Some(hwnd),
        super::window::WM_APP_LOAD_RESOLVED,
        WPARAM(gen as usize),
        LPARAM(raw as isize),
    )
    .is_err()
    {
        drop(Box::from_raw(raw)); // window died between resolve and post, reclaim, don't leak
    }
}

/// Kick off the async classify/read step for `path` on a detached worker (2026-09-05 audit,
/// F10). Mirrors `content::spawn_decode`'s generation-fenced post-back, but for the work that
/// used to run synchronously in `load()` before the window could even show.
///
/// Bounded by the same process-wide abandoned-worker budget the DLL's detached decodes use
/// (`safety::abandoned_budget_exhausted`): a worker that never returns (a hung network share)
/// is tracked with an `AbandonTicket` so repeatedly opening files on the same dead share cannot
/// grow the viewer's thread count without limit. Past the budget this refuses to start another
/// worker and leaves the window in its Loading state instead of adding one more blocked thread.
unsafe fn spawn_prepare_load(hwnd: HWND, path: String, gen: u64, view_source_active: bool) {
    if sagethumbs2k_core::safety::abandoned_budget_exhausted() {
        sagethumbs2k_core::safety::log_debug(&format!(
            "preview load: too many workers still running past their budget; leaving {path} \
             in its Loading state"
        ));
        return;
    }
    let ticket = sagethumbs2k_core::safety::AbandonTicket::new();
    *PENDING_PREPARE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ticket.clone());
    let hwnd_raw = hwnd.0 as isize;
    let spawned = std::thread::Builder::new()
        .name("st2k-preview-load".to_string())
        .spawn(move || {
            let resolved = resolve_load(&path, view_source_active);
            ticket.worker_finished();
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            post_resolved(hwnd, gen, resolved);
        });
    if spawned.is_err() {
        // `Builder::spawn` refused to create the OS thread: nothing was started (and the
        // closure, with it the ticket, was dropped unstarted), so the window is simply
        // left in its Loading state, the same degraded outcome a budget refusal leaves it in.
        sagethumbs2k_core::safety::log_debug("preview load: failed to start the prepare worker");
    }
}

/// Install a resolved `Text`/`Markdown` read. `md_flags` is `Some` only for Markdown, mirrors
/// the old synchronous dispatch, which only ever computed/consulted these for that kind.
unsafe fn apply_text_or_markdown(
    st: &ViewerState,
    kind: ContentKind,
    text: String,
    attachments: Vec<(String, Vec<u8>)>,
    md_flags: Option<(bool, bool, bool)>,
) {
    if let Some((has_headings, has_remote, remote_ok)) = md_flags {
        seed_md_attachments(st, attachments);
        st.md_has_headings.set(has_headings);
        st.md_has_remote.set(has_remote);
        st.md_remote_ok.set(remote_ok);
    }
    *st.text.borrow_mut() = Some(text);
    st.kind.set(kind);
}

/// `Resolved::Dispatch`: neither view-source, archive, DB nor mail matched, so fall back to
/// `classify`'s verdict, the same tail `dispatch_by_content_kind` used to run synchronously,
/// including the html-preview check (still synchronous: it needs the UI thread for WebView2).
unsafe fn apply_resolved_dispatch(hwnd: HWND, st: &ViewerState, path: &str, kind: ContentKind) {
    #[cfg(feature = "html-preview")]
    if try_load_web(hwnd, path) {
        return; // sets its own title/find-refresh (or defers via busy/pending)
    }
    let gen = st.decode_gen.get();
    match kind {
        ContentKind::Image => dispatch_image_kind(hwnd, st, path, gen),
        ContentKind::Video => dispatch_video_kind(hwnd, st, path, gen),
        _ => dispatch_fallback_kind(hwnd, st, path),
    }
    set_title(hwnd);
    super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
}

/// Apply a `WM_APP_LOAD_RESOLVED` result. The caller (`window::on_app_load_resolved`) has
/// already dropped a stale generation, so everything here is for the CURRENT load.
pub(super) unsafe fn apply_resolved(hwnd: HWND, st: &ViewerState, resolved: Resolved) {
    let path = st.path.borrow().clone().unwrap_or_default();
    match resolved {
        Resolved::SourceText(text) => {
            *st.text.borrow_mut() = Some(text);
            st.kind.set(ContentKind::Text);
        }
        Resolved::Archive(listing) => set_archive_listing_state(st, listing),
        Resolved::DbMarkdown(md) | Resolved::MailMarkdown(md) => set_markdown_doc_state(st, md),
        Resolved::TextOrMarkdown {
            kind,
            text,
            attachments,
            md_flags,
        } => apply_text_or_markdown(st, kind, text, attachments, md_flags),
        Resolved::Dispatch(kind) => {
            apply_resolved_dispatch(hwnd, st, &path, kind);
            return; // dispatch_*_kind/try_load_web already show/invalidate/title/refresh
        }
    }
    ensure_shown(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
    set_title(hwnd);
    super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
}

/// Synchronous load for the headless shot: decode on this thread, size, place off-screen,
/// and show (invisible) so `PrintWindow` can capture it.
pub(super) unsafe fn load_sync(hwnd: HWND, path: Option<&str>, opts: &super::ShotOpts) {
    let st = &*state(hwnd);
    if let Some(path) = path {
        *st.path.borrow_mut() = Some(path.to_string());
        let cls = content::classify(path);

        if opts.play && matches!(cls, ContentKind::Video) {
            load_sync_play_video(hwnd, st, path, opts);
            return; // already sized/shown
        } else if is_pdf(path) {
            load_sync_pdf(hwnd, st, path, opts);
        } else if let (Some(fr), true) = (opts.frame, is_animatable(path)) {
            load_sync_frame(st, path, fr);
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

/// `load_sync`'s `--play` branch: a live video engine so the transport strip renders (the video
/// surface is a swap chain `PrintWindow` can't read, so it stays black; the strip is parent GDI
/// and captures). Sizes/shows the window itself (unlike the other branches, which fall through
/// to `load_sync`'s common tail) because the video child needs a parented, realized window.
unsafe fn load_sync_play_video(hwnd: HWND, st: &ViewerState, path: &str, opts: &super::ShotOpts) {
    st.kind.set(ContentKind::Video);
    let (cw, ch) = client_size(hwnd);
    place(hwnd, cw, ch, Some((-32000, -32000)));
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    st.shown.set(true);
    if let Some(p) = super::video::create(
        hwnd,
        hwnd,
        &video_rect(hwnd),
        st.hinst,
        path,
        is_audio(path),
    ) {
        *st.video.borrow_mut() = Some(p);
        // Headless `--play --shot` of a track: decode its art synchronously so the capture
        // shows the same backdrop the live viewer paints (the async path never lands in a
        // shot window, which pumps only briefly).
        if is_audio(path) {
            if let Some(d) = content::decode_sync(path) {
                if let Some(hbmp) = content::make_dib(d.w, d.h, &d.rgba, 0x0000_0000) {
                    *st.art.borrow_mut() = Some(RenderData::opaque(hbmp, d.w, d.h));
                }
            }
        }
    }
    set_title(hwnd);
    if let Some(h) = opts.hot {
        st.hot.set(Some(h));
    }
}

/// `load_sync`'s PDF branch: render the requested page + page count synchronously so the ◀ ▶
/// pager + "N / M" indicator show, and spawn the continuous view opening in the background.
unsafe fn load_sync_pdf(hwnd: HWND, st: &ViewerState, path: &str, opts: &super::ShotOpts) {
    // The re-show path gets the continuous view too, so a viewer that was already open
    // scrolls exactly like one opened fresh. Asynchronous, so a headless shot that does
    // not pump captures the single-page render below and is byte-identical to before;
    // `--wait-ms` is what lets a shot wait for the scrolling view on purpose.
    super::pdfview::spawn_open(hwnd, path.to_string(), st.decode_gen.get());
    let pg = opts.pdf_page.unwrap_or(0);
    let done = sagethumbs2k_core::decode::read_capped(path)
        .ok()
        .and_then(|b| sagethumbs2k_core::pdf::render_page_counted(&b, pg, 1600))
        .and_then(|(png, count)| image::load_from_memory(&png).ok().map(|img| (img, count)))
        .map(|(img, count)| {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width() as i32, rgba.height() as i32);
            if let Some(rd) = content::make_render(w, h, &rgba.into_raw(), letterbox_bg(st)) {
                *st.render.borrow_mut() = Some(rd);
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
}

/// `load_sync`'s animated-frame branch: decode the animation and show frame `fr`, falling back
/// to the still-frame path if decoding fails or yields nothing (not actually animated).
unsafe fn load_sync_frame(st: &ViewerState, path: &str, fr: usize) {
    let frames = sagethumbs2k_core::decode::read_preview_capped(path)
        .ok()
        .and_then(|b| super::anim::decode_animation(&b, &ext_of(path)));
    if let Some(frames) = frames {
        let bg = letterbox_bg(st);
        let mut rds: Vec<RenderData> = Vec::new();
        for (d, _) in frames {
            if let Some(rd) = content::make_render(d.w, d.h, &d.rgba, bg) {
                rds.push(rd);
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
}

/// `load_static`'s "View source" hook, same gating as the async `load` path minus the window
/// ops. Returns true (and has already set text/kind) if source view took over.
unsafe fn try_static_source_view(st: &ViewerState, path: &str) -> bool {
    st.src_capable.set(source_capable(&ext_of(path)));
    if !(st.src_capable.get() && st.src_view.get()) {
        return false;
    }
    let Some(text) = content::read_text(path) else {
        return false;
    };
    *st.text.borrow_mut() = Some(text);
    st.kind.set(ContentKind::Text);
    true
}

/// `load_static`'s archive-listing hook (zip/7z/rar-family), same render as the async `load` path.
unsafe fn try_static_archive_listing(st: &ViewerState, path: &str) -> bool {
    if !content::is_archive_ext(&ext_of(path)) {
        return false;
    }
    let Some(listing) = content::archive_listing(path) else {
        return false;
    };
    set_archive_listing_state(st, listing);
    true
}

/// `load_static`'s database-view hook, same render as the async `load` path.
unsafe fn try_static_db_markdown(st: &ViewerState, path: &str) -> bool {
    let Some(md) = db_markdown(path) else {
        return false;
    };
    set_markdown_doc_state(st, md);
    true
}

/// `load_static`'s email-view hook, same render as the async `load` path.
unsafe fn try_static_mail_markdown(st: &ViewerState, path: &str) -> bool {
    let Some(md) = mail_markdown(path) else {
        return false;
    };
    set_markdown_doc_state(st, md);
    true
}

/// `load_static`'s `ContentKind::Image` arm: decode, or fall back to the info card.
unsafe fn apply_static_image(st: &ViewerState, path: &str) {
    match content::decode_sync(path) {
        Some(d) => {
            if let Some(rd) = content::make_render(d.w, d.h, &d.rgba, letterbox_bg(st)) {
                *st.render.borrow_mut() = Some(rd);
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
    }
}

/// `load_static`'s `ContentKind::Text`/`Markdown` arm: read (converting to markdown for the
/// Markdown case), or fall back to the info card.
unsafe fn apply_static_text_or_markdown(st: &ViewerState, path: &str, kind: ContentKind) {
    let read = if sagethumbs2k_core::formats::is_preview_doc(&ext_of(path)) {
        content::read_doc(path)
    } else {
        content::read_text(path)
    };
    let Some(mut t) = read else {
        *st.card.borrow_mut() = Some(infocard::gather(path));
        st.kind.set(ContentKind::InfoCard);
        return;
    };
    if kind == ContentKind::Markdown {
        if let Some(conv) = super::docconv::to_markdown(&ext_of(path), &t) {
            t = conv.md;
            seed_md_attachments(st, conv.attachments);
        }
        st.md_has_headings.set(super::markdown::has_headings(&t));
        st.md_has_remote.set(super::markdown::has_remote_images(&t));
        st.md_remote_ok
            .set(sagethumbs2k_core::settings::preview_md_remote_img());
    }
    *st.text.borrow_mut() = Some(t);
    st.kind.set(kind);
}

/// `load_static`'s final by-kind dispatch, once none of the content-specific hooks took over.
unsafe fn apply_static_content_kind(st: &ViewerState, path: &str, kind: ContentKind) {
    match kind {
        ContentKind::Image => apply_static_image(st, path),
        ContentKind::Text | ContentKind::Markdown => apply_static_text_or_markdown(st, path, kind),
        _ => {
            *st.card.borrow_mut() = Some(infocard::gather(path));
            st.kind.set(ContentKind::InfoCard);
        }
    }
}

/// Synchronous still decode for the headless shot: image → DIB, text/markdown → read, else card.
pub(super) unsafe fn load_static(st: &ViewerState, path: &str, kind: ContentKind) {
    if try_static_source_view(st, path) {
        return;
    }
    if try_static_archive_listing(st, path) {
        return;
    }
    if try_static_db_markdown(st, path) {
        return;
    }
    if try_static_mail_markdown(st, path) {
        return;
    }
    // Font specimen (same render as the async `load` path).
    if super::font::is_font_ext(&ext_of(path)) && render_font_to_state(st, path) {
        return;
    }
    apply_static_content_kind(st, path, kind);
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
            // The font specimen is rendered onto the pane colour already, so it is opaque by
            // construction and wants the plain blit (and no checkerboard behind it).
            *st.render.borrow_mut() = Some(RenderData::opaque(hbmp, w, h));
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
            let Some(target) = parse_url_shortcut(path) else {
                return false;
            };
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
            super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
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
            super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
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
    let esc = path
        .replace('\\', "/")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F");
    if esc.starts_with('/') {
        format!("file://{esc}")
    } else {
        format!("file:///{esc}")
    }
}

/// Parse a `.url`/`.webloc` shortcut for its target. `.url` is an INI (`URL=` under
/// `[InternetShortcut]`); `.webloc` is a plist with a `<string>` URL. `None` unless the scheme is
/// http(s) — so WebView2 never gets a `file:`/`javascript:` target from a shortcut.
/// Decode a `.url`/`.webloc` shortcut's raw bytes as text. Windows commonly writes `.url`
/// files with a non-ASCII target as UTF-16 (LE, with BOM) — a plain `read_to_string`
/// (UTF-8 only) silently failed on those, so the live-preview feature never engaged for
/// them. Sniff the BOM and decode accordingly; UTF-8 (the common case, and `.webloc`'s
/// plist encoding) falls through unchanged.
#[cfg(feature = "html-preview")]
fn decode_shortcut_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units).ok();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&units).ok();
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes); // UTF-8 BOM
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

#[cfg(feature = "html-preview")]
fn parse_url_shortcut(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = decode_shortcut_text(&bytes)?;
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
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        } else if st.open_front.get() {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
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

/// Clamp a REMEMBERED client size to something that actually fits: never below the window's
/// minimum, never larger than the monitor's work area (a size dragged out on a 4K screen must not
/// open off the edge of a laptop panel). Pure math, so it is unit-testable without a window.
pub(super) fn clamp_remembered_size(
    (w, h): (i32, i32),
    (min_w, min_h): (i32, i32),
    (work_w, work_h): (i32, i32),
) -> (i32, i32) {
    (
        w.clamp(min_w, work_w.max(min_w)),
        h.clamp(min_h, work_h.max(min_h)),
    )
}

/// Compute the desired CLIENT size (device px) for the current content.
///
/// A size the user dragged out beats the per-content default, for every content kind and every
/// file after it — that IS the "remember it" behaviour (`settings::preview_window_size`); a
/// caption double-click forgets it again. Two exceptions, in order:
///   * a resize drag that is still in progress wins over both, so a follow-selection switch
///     landing mid-drag can't yank the frame out from under the cursor;
///   * `--shot` ignores the remembered size entirely, so a headless capture never depends on
///     whatever size the developer happened to leave their own viewer at.
pub(super) unsafe fn client_size(hwnd: HWND) -> (i32, i32) {
    let st = &*state(hwnd);
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let cap = sc(CAPTION_H);
    if !st.shot {
        if st.user_sized.get() {
            let mut r = RECT::default();
            if GetClientRect(hwnd, &mut r).is_ok() {
                return (r.right - r.left, r.bottom - r.top);
            }
        }
        if let Some((w, h)) = sagethumbs2k_core::settings::preview_window_size() {
            let (_dpi, work) = crate::win::cursor_monitor_metrics();
            return clamp_remembered_size(
                (sc(w), sc(h)),
                (sc(MIN_W), sc(MIN_H)),
                (work.right - work.left, work.bottom - work.top),
            );
        }
    }
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
        ContentKind::Video => match st.video_dims.get() {
            // Real clip dimensions (rotation applied), known once MF has read the metadata. Fit
            // them the same way an image is fitted, then add the chrome. Without this every clip
            // opened into the same 16:9 shell and portrait phone video sat letterboxed inside it.
            Some((vw, vh)) if vw > 0 && vh > 0 => {
                let (_dpi, work) = crate::win::cursor_monitor_metrics();
                let chrome = cap + sc(SCRUB_H);
                let cap_w = (work.right - work.left) * 80 / 100;
                let cap_h = (work.bottom - work.top) * 80 / 100 - chrome;
                let mut scale = f64::min(cap_w as f64 / vw as f64, cap_h as f64 / vh as f64);
                if scale > 1.0 {
                    scale = 1.0; // never upscale a small clip past 100%
                }
                let w = ((vw as f64 * scale).round() as i32).max(1);
                let h = ((vh as f64 * scale).round() as i32).max(1);
                (w.max(sc(MIN_W)), (h + chrome).max(sc(MIN_H)))
            }
            // Audio, or metadata not in yet: the placeholder shell.
            _ => (sc(VIDEO_W), sc(VIDEO_H) + cap + sc(SCRUB_H)),
        },
        ContentKind::Html => (sc(VIDEO_W), sc(VIDEO_H) + cap), // browser-ish default

        ContentKind::Loading => (sc(LOADING_W), sc(LOADING_H)),
    }
}

/// Resize (and optionally move) the window so its CLIENT area is `cw`×`ch`. `pos` = top-left
/// window position, or `None` to keep the current position.
pub(super) unsafe fn place(hwnd: HWND, cw: i32, ch: i32, pos: Option<(i32, i32)>) {
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
    let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
    let _ = AdjustWindowRectEx(&mut rc, style, false, ex);
    let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
    match pos {
        Some((x, y)) => {
            let _ = SetWindowPos(hwnd, None, x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        None => {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                ww,
                wh,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
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

/// Persist the size the user just dragged the frame to, so the next file — and the next preview —
/// opens at it. Driven by `WM_EXITSIZEMOVE`, and only when `WM_SIZING` actually fired: that pair
/// is what separates a RESIZE from a plain window MOVE, which must not pin whatever size the
/// content happened to pick. Stored in logical px (see `settings::preview_window_size`).
pub(super) unsafe fn remember_size(hwnd: HWND) {
    let st = &*state(hwnd);
    // `replace` consumes the flag either way — a move that follows a resize starts clean.
    if !st.user_sized.replace(false) || st.shot || st.fullscreen.get().is_some() {
        return;
    }
    let mut r = RECT::default();
    if GetClientRect(hwnd, &mut r).is_err() {
        return;
    }
    let size = (
        crate::win::dpi_unscale(hwnd, r.right - r.left),
        crate::win::dpi_unscale(hwnd, r.bottom - r.top),
    );
    let _ = sagethumbs2k_core::settings::set_preview_window_size(Some(size));
}

/// Forget the remembered size and re-fit the window to the file it is showing — the caption
/// double-click. The escape hatch for "I dragged it out once and now everything opens that big".
pub(super) unsafe fn forget_size(hwnd: HWND) {
    let st = &*state(hwnd);
    st.user_sized.set(false);
    let _ = sagethumbs2k_core::settings::set_preview_window_size(None);
    if st.shot || st.fullscreen.get().is_some() {
        return;
    }
    let (cw, ch) = client_size(hwnd); // with nothing remembered, the content's own size again
    place(hwnd, cw, ch, None);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "html-preview")]
    use super::parse_url_shortcut;
    use super::{clamp_remembered_size, is_load_current, source_capable};

    /// 2026-09-05 audit, F10 acceptance: "switching selections repeatedly never displays an
    /// older selection's completion over the newest one". A completion is current for exactly
    /// the load it was started for, never for an older OR a hypothetical later generation,
    /// so a naive `gen >= current` (which would let a late completion win a race it lost) or
    /// `gen <= current` (which would accept a completion for a load that hasn't started yet)
    /// both fail this test; only strict equality passes.
    #[test]
    fn a_stale_generation_is_never_current() {
        // The load this completion was started for is still the one showing: apply it.
        assert!(is_load_current(5, 5));
        // The user already switched away (repeatedly, in the acceptance scenario) before this
        // slow completion landed: an OLDER generation must never paint over the current one.
        assert!(!is_load_current(3, 5));
        assert!(!is_load_current(1, 5));
        // Defensive: `decode_gen` only ever increases, so this can't happen in practice, but the
        // check must be exact equality, not merely "not older".
        assert!(!is_load_current(7, 5));
    }

    #[test]
    fn source_capable_reaches_eml_but_not_msg() {
        // .eml is genuine RFC-822 text (same gate as any other text preview); .msg's raw
        // bytes are an OLE compound file with no text view to show.
        assert_eq!(
            source_capable("eml"),
            sagethumbs2k_core::settings::preview_text()
        );
        assert!(!source_capable("msg"));
    }

    /// Windows commonly writes a `.url` shortcut with a non-ASCII target as UTF-16 LE
    /// with a BOM — a plain `read_to_string` (UTF-8 only) used to fail on these outright,
    /// so the live-preview feature never engaged for them.
    #[cfg(feature = "html-preview")]
    #[test]
    fn parse_url_shortcut_reads_a_utf16_le_target_with_bom() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_loader_url_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shortcut.url");

        let text = "[InternetShortcut]\r\nURL=https://example.com/caf\u{e9}\r\n";
        let mut bytes = vec![0xFFu8, 0xFE];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&path, &bytes).unwrap();

        let got = parse_url_shortcut(path.to_str().unwrap());
        assert_eq!(got.as_deref(), Some("https://example.com/caf\u{e9}"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remembered_size_is_kept_when_it_fits() {
        let got = clamp_remembered_size((1200, 800), (400, 200), (1920, 1040));
        assert_eq!(got, (1200, 800));
    }

    #[test]
    fn a_remembered_size_from_a_bigger_screen_shrinks_to_this_one() {
        let got = clamp_remembered_size((3800, 2000), (400, 200), (1920, 1040));
        assert_eq!(got, (1920, 1040));
    }

    #[test]
    fn a_remembered_size_never_drops_below_the_window_minimum() {
        // Even a work area smaller than the minimum must not produce a sub-minimum size —
        // `WM_GETMINMAXINFO` would refuse it and the two would disagree every resize.
        assert_eq!(
            clamp_remembered_size((50, 50), (400, 200), (1920, 1040)),
            (400, 200)
        );
        assert_eq!(
            clamp_remembered_size((900, 700), (400, 200), (320, 160)),
            (400, 200)
        );
    }
}
