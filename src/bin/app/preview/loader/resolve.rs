//! The asynchronous load: resolve on a worker, post the result, apply it on the UI thread.

use super::*;

/// The outcome of [`resolve_load`], carried from the worker thread to the UI thread in a
/// `WM_APP_LOAD_RESOLVED` payload. `Send`: every field is owned data, never a GDI handle or a
/// `ViewerState` reference. GDI object creation stays on the UI thread, as it already does for
/// an image decode (`content::spawn_decode` posts raw RGBA, never an HBITMAP).
pub(in super::super) enum Resolved {
    SourceText(String),
    Archive(String),
    DbMarkdown(String),
    MailMarkdown(String),
    /// A hex dump of a file `classify` could not otherwise place — see
    /// [`resolve_hex_or_card`]. Also markdown under the hood (a fenced code block), same as
    /// the two variants above; kept as its own variant rather than reusing `DbMarkdown`
    /// because "why this exists" is a completely different story than "SQLite schema", and
    /// conflating them would make a future reader of this enum guess which one a given call
    /// site actually meant.
    HexDump(String),
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
pub(in super::super) fn is_load_current(gen: u64, current: u64) -> bool {
    gen == current
}

/// `resolve_load`'s view-source branch: read `path` as text if view-source is active for it.
/// `None` means "not source view, or the read failed": either way the caller falls through to
/// the normal rendered path, exactly like the old `show_source` returning `false` did.
pub(super) fn resolve_source_text(path: &str, view_source_active: bool) -> Option<Resolved> {
    if !view_source_active {
        return None;
    }
    content::read_text(path).map(Resolved::SourceText)
}

/// `resolve_load`'s tail: classify `path`, then read text/markdown content on the SAME worker
/// so the UI thread applies the whole result in one step (image/PDF/video need no read here,
/// they already decode asynchronously once dispatched).
pub(super) fn resolve_by_content_kind(path: &str) -> Resolved {
    match content::classify(path) {
        kind @ (ContentKind::Text | ContentKind::Markdown) => resolve_text_or_markdown(path, kind),
        ContentKind::InfoCard => resolve_hex_or_card(path),
        kind => Resolved::Dispatch(kind),
    }
}

/// `classify`'s `InfoCard` verdict is not necessarily final: try a hex dump before giving up
/// completely. This is the one case that verdict is allowed to be reconsidered from —
/// `classify` itself stays untouched, per this repo's standing rule for the DB/mail hooks
/// (CLAUDE.md, preview/dbdoc.rs's own header comment). `hex_markdown` returning `None` (Text
/// toggle off, a directory, an unreadable/empty file — see `hexview::to_markdown`) is the
/// ordinary info card, completely unchanged from before this hook existed.
pub(super) fn resolve_hex_or_card(path: &str) -> Resolved {
    match hex_markdown(path) {
        Some(md) => Resolved::HexDump(md),
        None => Resolved::Dispatch(ContentKind::InfoCard),
    }
}

/// The `Text`/`Markdown` arm of [`resolve_by_content_kind`]. `kind` is `classify`'s verdict
/// (already gated on the Text/Markdown Settings toggles, not re-derived here). Structured docs
/// (CSV/TSV/ipynb) read UNtruncated so their parse sees the whole file. A read failure
/// (unreadable / turned out binary) falls back to the info card, exactly like the old
/// synchronous dispatch did.
pub(super) fn resolve_text_or_markdown(path: &str, kind: ContentKind) -> Resolved {
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
        if let Some(conv) = super::super::docconv::to_markdown(&ext, &t) {
            t = conv.md;
            attachments = conv.attachments;
        }
        md_flags = Some((
            super::super::markdown::has_headings(&t),
            super::super::markdown::has_remote_images(&t),
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
pub(super) fn resolve_load(path: &str, view_source_active: bool) -> Resolved {
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
pub(super) static PENDING_PREPARE: std::sync::Mutex<
    Option<sagethumbs2k_core::safety::AbandonTicket>,
> = std::sync::Mutex::new(None);

/// Mark any still-outstanding prepare worker as abandoned. Called at the start of every new
/// load ([`reset_viewer_state`]), right next to `content::begin_generation`'s equivalent step,
/// and from `window::on_destroy` (closing the viewer is a cancel too, audit E02 2026-09-07).
///
/// Logs (rate-limited via `window::log_abandoned_worker`) exactly when this actually gives up a
/// LIVE ticket, asked directly via `AbandonTicket::is_counted` rather than a before/after read
/// of the shared process-wide count, which races any other ticket's concurrent activity. A
/// worker that had already finished by the time this runs must not be reported as newly
/// abandoned.
pub(in super::super) fn abandon_pending_prepare() {
    let prev = PENDING_PREPARE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(ticket) = prev {
        ticket.caller_gave_up();
        if ticket.is_counted() {
            super::super::window::log_abandoned_worker("prepare");
        }
    }
}

/// Post a `resolve_load` result to the UI thread, reclaiming the box if the window died first.
pub(super) unsafe fn post_resolved(hwnd: HWND, gen: u64, resolved: Resolved) {
    let payload: Box<(u64, Resolved)> = Box::new((gen, resolved));
    let raw = Box::into_raw(payload);
    if PostMessageW(
        Some(hwnd),
        super::super::window::WM_APP_LOAD_RESOLVED,
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
pub(super) unsafe fn spawn_prepare_load(
    hwnd: HWND,
    path: String,
    gen: u64,
    view_source_active: bool,
) {
    if sagethumbs2k_core::safety::abandoned_budget_exhausted() {
        sagethumbs2k_core::safety::log_debugf!(
            "preview load: too many workers still running past their budget; leaving {path} \
             in its Loading state"
        );
        show_load_refused(hwnd, &path);
        return;
    }
    let ticket = sagethumbs2k_core::safety::AbandonTicket::new();
    *PENDING_PREPARE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ticket.clone());
    let hwnd_raw = hwnd.0 as isize;
    let worker_path = path.clone(); // keep `path` for the fallback below if `spawn` itself fails
    let spawned = std::thread::Builder::new()
        .name("st2k-preview-load".to_string())
        .spawn(move || {
            let stage_start = std::time::Instant::now();
            let resolved = resolve_load(&worker_path, view_source_active);
            if let Some(line) = sagethumbs2k_core::safety::stage_stall_report(
                "prepare",
                stage_start.elapsed(),
                sagethumbs2k_core::safety::PREVIEW_DECODE_BUDGET,
                gen,
                &worker_path,
            ) {
                sagethumbs2k_core::safety::log_debug(&line);
            }
            ticket.worker_finished();
            let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
            post_resolved(hwnd, gen, resolved);
        });
    if spawned.is_err() {
        // `Builder::spawn` refused to create the OS thread: nothing was started (and the
        // closure, with it the ticket, was dropped unstarted). Clear the slot too: a ticket
        // left RUNNING with no worker would be counted as an abandoned worker by every
        // later load's `abandon_pending_prepare`.
        *PENDING_PREPARE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        sagethumbs2k_core::safety::log_debug("preview load: failed to start the prepare worker");
        show_load_refused(hwnd, &path);
    }
}

/// Fall through to the fallback info card when the prepare worker could not even be started:
/// the abandoned-worker budget is exhausted, or the OS refused to create the thread (2026-09-05
/// audit, F10 adversarial review). Without this the window was left in its `Loading` state (an
/// endless spinner, indistinguishable from "still working") for that file, with no way for the
/// user to tell "waiting" from "will never finish". Same fall-through an unsupported/unreadable
/// file already gets via [`apply_resolved_dispatch`]'s `_ => dispatch_fallback_kind` arm.
pub(super) unsafe fn show_load_refused(hwnd: HWND, path: &str) {
    let st = &*state(hwnd);
    dispatch_fallback_kind(hwnd, st, path);
    set_title(hwnd);
    super::super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
}

/// Install a resolved `Text`/`Markdown` read. `md_flags` is `Some` only for Markdown, mirrors
/// the old synchronous dispatch, which only ever computed/consulted these for that kind.
pub(super) unsafe fn apply_text_or_markdown(
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
/// `classify`'s verdict, the same tail `dispatch_by_content_kind` used to run synchronously.
/// The `try_load_web` call here is a defensive fallback, not the primary route any more
/// (2026-09-05 audit, F10 adversarial review fix): `load()`'s `decide_load_route` now decides
/// the web route on the UI thread BEFORE `spawn_prepare_load` ever runs, so an html/`.url`/
/// `.webloc` path never reaches `resolve_load`'s worker-side `classify` in the first place and
/// this arm sees only the extensions `try_load_web` already declines (it returns `false` for
/// them). Kept here rather than removed so a future caller of `apply_resolved_dispatch` outside
/// `load()`'s gate can't silently regress back to showing HTML/`.url` as raw source.
pub(super) unsafe fn apply_resolved_dispatch(
    hwnd: HWND,
    st: &ViewerState,
    path: &str,
    kind: ContentKind,
) {
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
    super::super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
}

/// Apply a `WM_APP_LOAD_RESOLVED` result. The caller (`window::on_app_load_resolved`) has
/// already dropped a stale generation, so everything here is for the CURRENT load.
pub(in super::super) unsafe fn apply_resolved(hwnd: HWND, st: &ViewerState, resolved: Resolved) {
    let path = st.path.borrow().clone().unwrap_or_default();
    match resolved {
        Resolved::SourceText(text) => {
            *st.text.borrow_mut() = Some(text);
            st.kind.set(ContentKind::Text);
        }
        Resolved::Archive(listing) => set_archive_listing_state(st, listing),
        Resolved::DbMarkdown(md) | Resolved::MailMarkdown(md) | Resolved::HexDump(md) => {
            set_markdown_doc_state(st, md)
        }
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
    super::super::find::refresh(hwnd); // the new document exists now, so an open search re-runs on IT
}
