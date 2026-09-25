//! The synchronous loads the headless --shot harness drives.

use super::*;

/// Synchronous load for the headless shot: decode on this thread, size, place off-screen,
/// and show (invisible) so `PrintWindow` can capture it.
pub(in super::super) unsafe fn load_sync(
    hwnd: HWND,
    path: Option<&str>,
    opts: &super::super::ShotOpts,
) {
    let st = &*state(hwnd);
    if let Some(path) = path {
        if dispatch_sync_path(hwnd, st, path, opts) {
            return;
        }
    }
    set_title(hwnd);
    if let Some(h) = opts.hot {
        st.hot.set(Some(h));
    }
    show_offscreen(hwnd, st);
}

/// Load path content synchronously into viewer state; returns true if already sized/shown.
unsafe fn dispatch_sync_path(
    hwnd: HWND,
    st: &ViewerState,
    path: &str,
    opts: &super::super::ShotOpts,
) -> bool {
    *st.path.borrow_mut() = Some(path.to_string());
    let cls = content::classify(path);

    if opts.play && matches!(cls, ContentKind::Video) {
        load_sync_play_video(hwnd, st, path, opts);
        return true; // already sized/shown
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
    false
}

/// `load_sync`'s `--play` branch: a live video engine so the transport strip renders (the video
/// surface is a swap chain `PrintWindow` can't read, so it stays black; the strip is parent GDI
/// and captures). Sizes/shows the window itself (unlike the other branches, which fall through
/// to `load_sync`'s common tail) because the video child needs a parented, realized window.
pub(super) unsafe fn load_sync_play_video(
    hwnd: HWND,
    st: &ViewerState,
    path: &str,
    opts: &super::super::ShotOpts,
) {
    st.kind.set(ContentKind::Video);
    show_offscreen(hwnd, st);
    if let Some(p) = super::super::video::create(
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
pub(super) unsafe fn load_sync_pdf(
    hwnd: HWND,
    st: &ViewerState,
    path: &str,
    opts: &super::super::ShotOpts,
) {
    // The re-show path gets the continuous view too, so a viewer that was already open
    // scrolls exactly like one opened fresh. Asynchronous, so a headless shot that does
    // not pump captures the single-page render below and is byte-identical to before;
    // `--wait-ms` is what lets a shot wait for the scrolling view on purpose.
    super::super::pdfview::spawn_open(hwnd, path.to_string(), st.decode_gen.get());
    let pg = opts.pdf_page.unwrap_or(0);
    let done = st2k_codecs::pdf::render_page_counted_path(path, pg, 1600)
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
pub(super) unsafe fn load_sync_frame(st: &ViewerState, path: &str, fr: usize) {
    let frames = st2k_codecs::decode::read_preview_capped(path)
        .ok()
        .and_then(|b| super::super::anim::decode_animation(&b, &ext_of(path)));
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
pub(super) unsafe fn try_static_source_view(st: &ViewerState, path: &str) -> bool {
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
pub(super) unsafe fn try_static_archive_listing(st: &ViewerState, path: &str) -> bool {
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
pub(super) unsafe fn try_static_db_markdown(st: &ViewerState, path: &str) -> bool {
    let Some(md) = db_markdown(path) else {
        return false;
    };
    set_markdown_doc_state(st, md);
    true
}

/// `load_static`'s email-view hook, same render as the async `load` path.
pub(super) unsafe fn try_static_mail_markdown(st: &ViewerState, path: &str) -> bool {
    let Some(md) = mail_markdown(path) else {
        return false;
    };
    set_markdown_doc_state(st, md);
    true
}

/// `load_static`'s `ContentKind::Image` arm: decode, or fall back to the info card.
pub(super) unsafe fn apply_static_image(st: &ViewerState, path: &str) {
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
pub(super) unsafe fn apply_static_text_or_markdown(
    st: &ViewerState,
    path: &str,
    kind: ContentKind,
) {
    let read = if st2k_base::formats::is_preview_doc(&ext_of(path)) {
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
        if let Some(conv) = super::super::docconv::to_markdown(&ext_of(path), &t) {
            t = conv.md;
            seed_md_attachments(st, conv.attachments);
        }
        st.md_has_headings
            .set(super::super::markdown::has_headings(&t));
        st.md_has_remote
            .set(super::super::markdown::has_remote_images(&t));
        st.md_remote_ok
            .set(st2k_base::settings::preview_md_remote_img());
    }
    *st.text.borrow_mut() = Some(t);
    st.kind.set(kind);
}

/// `load_static`'s final by-kind dispatch, once none of the content-specific hooks took over.
pub(super) unsafe fn apply_static_content_kind(st: &ViewerState, path: &str, kind: ContentKind) {
    match kind {
        ContentKind::Image => apply_static_image(st, path),
        ContentKind::Text | ContentKind::Markdown => apply_static_text_or_markdown(st, path, kind),
        ContentKind::InfoCard => apply_static_hex_or_card(st, path),
        _ => {
            *st.card.borrow_mut() = Some(infocard::gather(path));
            st.kind.set(ContentKind::InfoCard);
        }
    }
}

/// `load_static`'s hex-dump hook — the headless twin of [`resolve_hex_or_card`]. Same
/// fall-through as every other hook in this file: `hex_markdown` returning `None` leaves the
/// ordinary info card exactly as it was before this hook existed.
pub(super) unsafe fn apply_static_hex_or_card(st: &ViewerState, path: &str) {
    match hex_markdown(path) {
        Some(md) => set_markdown_doc_state(st, md),
        None => {
            *st.card.borrow_mut() = Some(infocard::gather(path));
            st.kind.set(ContentKind::InfoCard);
        }
    }
}

/// Synchronous still decode for the headless shot: image → DIB, text/markdown → read, else card.
pub(in super::super) unsafe fn load_static(st: &ViewerState, path: &str, kind: ContentKind) {
    // Same blocklist gate as the async `load` path, checked first for the same reason: a
    // blocked extension must never reach a decoder, headless shot included.
    if is_load_blocked(path) {
        *st.card.borrow_mut() = Some(infocard::gather(path));
        st.kind.set(ContentKind::InfoCard);
        return;
    }
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
    if super::super::font::is_font_ext(&ext_of(path)) && render_font_to_state(st, path) {
        return;
    }
    apply_static_content_kind(st, path, kind);
}

/// Decode a converted document's inline attachments (notebook `attachment:` images — bytes that
/// live inside the file) and pre-seed them into the image cache under their rewritten keys, so
/// the markdown paint finds them ready. Runs at load, off the paint path; failures just leave the
/// key absent (renders as an alt-text pill). The cache was cleared earlier this load.
pub(super) unsafe fn seed_md_attachments(st: &ViewerState, attachments: Vec<(String, Vec<u8>)>) {
    if attachments.is_empty() {
        return;
    }
    let bg = st2k_appkit::dark::SURFACE().0; // markdown content background
    let mut imgs = st.md_imgs.borrow_mut();
    for (key, bytes) in attachments {
        if let Some(rd) = super::super::markdown::decode_bytes_to_dib(&bytes, bg) {
            imgs.insert(key, super::super::markdown::ImgSlot::Ready(rd));
        }
    }
}
