//! Painting the content area, one arm per content kind.

use super::*;

/// The single `WM_PAINT` content arm, one branch per [`ContentKind`]. Split out of
/// [`paint_into`] purely to keep that coordinator thin; each arm's own drawing lives in its own
/// named helper below.
pub(super) unsafe fn paint_content(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    content_rc: &RECT,
    content_bg: u32,
    text: u32,
    subtle: u32,
) {
    match st.kind.get() {
        ContentKind::Image => paint_content_image(hwnd, hdc, st, content_rc, content_bg, subtle),
        ContentKind::InfoCard => {
            paint_content_infocard(hwnd, hdc, st, content_rc, content_bg, text, subtle)
        }
        ContentKind::Text => {
            paint_content_text(hwnd, hdc, st, content_rc, content_bg, text, subtle)
        }
        ContentKind::Markdown => {
            paint_content_markdown(hwnd, hdc, st, content_rc, content_bg, text, subtle)
        }
        ContentKind::Video => paint_content_video(hwnd, hdc, st, text, subtle),
        // Localized (audit F29 follow-up): the guard test added with that finding flagged this
        // as a user-visible literal that never became a key, so a non-English preview showed an
        // English placeholder for the whole decode.
        ContentKind::Loading => paint_message(
            hwnd,
            hdc,
            content_rc,
            content_bg,
            subtle,
            crate::win::t("preview_loading"),
        ),
        // The WebView2 child window renders over the content area; just fill behind it.
        ContentKind::Html => fill(hdc, content_rc, content_bg),
    }
}

/// `ContentKind::Image`: the fit-zoom bitmap, or a scrolled multi-page PDF's pages + strip.
pub(super) unsafe fn paint_content_image(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    content_rc: &RECT,
    content_bg: u32,
    subtle: u32,
) {
    // Checkerboard behind a transparent image, from the SAME setting the classic menu tile
    // uses, so a white-on-transparent logo doesn't read as an empty pane. `paint_image`
    // ignores it for an opaque bitmap, so this is a no-op for photos. The cell is
    // DPI-scaled and larger than the menu tile's, because this pane is full size.
    let checker =
        sagethumbs2k_core::settings::preview_checker().then(|| crate::win::dpi_scale(hwnd, 12));
    // The fit view is drawn from a codec-scaled decode. If this zoom (or a resize) has
    // outgrown it, ask for the real pixels; they arrive asynchronously and repaint.
    super::super::window::ensure_full_for_zoom(hwnd, content_rc);
    // A multi-page PDF whose session opened scrolls continuously instead of showing
    // one page at a time. Returns false for everything else (and for a PDF whose
    // session never landed), which falls through to the single-image path below,
    // unchanged. The caption is painted by the shared code after this match either
    // way, so the page indicator and buttons behave identically in both modes.
    let scrolled_pdf =
        super::super::pdfview::paint(hwnd, hdc, content_rc, content_bg, crate::dark::BTN_FACE().0);
    // The page-thumbnail strip lives in the slice `content_rect` gave up, so it paints
    // after the pages and can never overlap them.
    if scrolled_pdf {
        super::super::pdfview::paint_strip(
            hwnd,
            hdc,
            crate::dark::DARK_BG().0,
            crate::dark::BTN_FACE().0,
        );
    }
    let frames = st.frames.borrow();
    if scrolled_pdf {
        // already drawn
    } else if let Some(rd) = frames.get(st.cur_frame.get()) {
        content::paint_image(
            hdc,
            content_rc,
            rd,
            content_bg,
            st.zoom.get(),
            st.pan.get(),
            checker,
        );
    } else if let Some(rd) = st.render.borrow().as_ref() {
        content::paint_image(
            hdc,
            content_rc,
            rd,
            content_bg,
            st.zoom.get(),
            st.pan.get(),
            checker,
        );
    } else {
        // Same localized placeholder as the `ContentKind::Loading` arm above: this is the
        // still-decoding branch of the image path, and it must not disagree with it.
        paint_message(
            hwnd,
            hdc,
            content_rc,
            content_bg,
            subtle,
            crate::win::t("preview_loading"),
        );
    }
}

/// `ContentKind::InfoCard`: the metadata card for a file with no visual preview.
pub(super) unsafe fn paint_content_infocard(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    content_rc: &RECT,
    content_bg: u32,
    text: u32,
    subtle: u32,
) {
    if let Some(card) = st.card.borrow().as_ref() {
        infocard::paint(hwnd, hdc, content_rc, card, content_bg, text, subtle);
    } else {
        paint_message(hwnd, hdc, content_rc, content_bg, subtle, "");
    }
}

/// `ContentKind::Text`: syntax-highlighted plain text, language resolved and cached once per load.
pub(super) unsafe fn paint_content_text(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    content_rc: &RECT,
    content_bg: u32,
    text: u32,
    subtle: u32,
) {
    if let Some(t) = st.text.borrow().as_ref() {
        let lang = cached_text_lang(hwnd, st.decode_gen.get(), || {
            let ext = st
                .path
                .borrow()
                .as_deref()
                .and_then(|p| std::path::Path::new(p).extension().and_then(|e| e.to_str()))
                .unwrap_or("")
                .to_ascii_lowercase();
            let mut lang = highlight::lang_from_ext(&ext);
            if matches!(lang, highlight::Lang::Plain) {
                // No usable extension (Makefile, Dockerfile, .bashrc, a bare shebang
                // script): fall back to the file NAME and the first line's `#!` before
                // giving up on colouring it. Only reached when the extension told us
                // nothing, so a real `.txt` is never second-guessed.
                let name = st
                    .path
                    .borrow()
                    .as_deref()
                    .and_then(|p| {
                        std::path::Path::new(p)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                lang = highlight::lang_from_name_or_shebang(&name, t);
            }
            lang
        });
        let th = paint_text(
            hwnd,
            hdc,
            content_rc,
            t,
            lang,
            content_bg,
            text,
            st.text_scroll.get(),
            sel_range(st),
        );
        st.text_h.set(th); // remember for the wheel handler's scroll clamp
        let _ = clamp_text_scroll(hwnd);
    } else {
        paint_message(hwnd, hdc, content_rc, content_bg, subtle, "");
    }
}

/// `ContentKind::Markdown`: the rendered document plus its optional outline (ToC) sidebar.
pub(super) unsafe fn paint_content_markdown(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    content_rc: &RECT,
    content_bg: u32,
    text: u32,
    subtle: u32,
) {
    let text_ref = st.text.borrow();
    let Some(t) = text_ref.as_ref() else {
        drop(text_ref);
        st.md_links.borrow_mut().clear();
        st.toc_hits.borrow_mut().clear();
        st.md_hits.borrow_mut().clear();
        paint_message(hwnd, hdc, content_rc, content_bg, subtle, "");
        return;
    };
    let cols = super::super::markdown::MdColors {
        bg: content_bg,
        fg: text,
        muted: subtle,
        accent: crate::dark::ACCENT_TEXT().0,
        code_bg: crate::dark::DARK_BG().0,
        border: crate::dark::BORDER().0,
        sel: crate::dark::SEL_BG().0,
    };
    // Outline (ToC) sidebar: reserve a left strip and shift the document right when the
    // sidebar is open AND the document actually has headings (flag cached at load).
    // While the open/close slide runs, `toc_anim` carries the mid-tween width.
    let w_full = crate::win::dpi_scale(hwnd, 220);
    let settled = if st.toc_open.get() { w_full } else { 0 };
    let sidebar_w = if st.md_has_headings.get() {
        st.toc_anim.get().unwrap_or(settled).clamp(0, w_full)
    } else {
        0
    };
    let show_toc = sidebar_w > 0;
    let md_rc = RECT {
        left: content_rc.left + sidebar_w,
        ..*content_rc
    };
    let scroll = st.text_scroll.get();
    // The markdown file's folder — local image srcs resolve against it.
    let doc_dir = st
        .path
        .borrow()
        .as_deref()
        .and_then(|p| std::path::Path::new(p).parent().map(|d| d.to_path_buf()));
    let mut links = st.md_links.borrow_mut();
    let mut toc = st.md_toc.borrow_mut();
    let mut imgs = st.md_imgs.borrow_mut();
    let mut layout = st.md_layout.borrow_mut();
    let mut hits = st.md_hits.borrow_mut();
    let mut sel = super::super::markdown::MdSel {
        range: sel_range(st),
        hits: &mut hits,
    };
    let th = super::super::markdown::render(
        hwnd,
        hdc,
        &md_rc,
        t,
        scroll,
        &cols,
        &mut links,
        &mut toc,
        &mut imgs,
        doc_dir.as_deref(),
        st.decode_gen.get(),
        st.md_remote_ok.get(),
        &mut layout,
        &mut sel,
    );
    drop(hits);
    drop(layout);
    st.text_h.set(th);
    let _ = clamp_text_scroll(hwnd);
    drop(links);
    drop(imgs);
    if show_toc {
        let side_rc = RECT {
            right: content_rc.left + sidebar_w,
            ..*content_rc
        };
        let mut hits = st.toc_hits.borrow_mut();
        paint_toc(
            hwnd,
            hdc,
            &side_rc,
            &toc,
            scroll,
            st.toc_sel.get(),
            &mut hits,
        );
    } else {
        st.toc_hits.borrow_mut().clear();
    }
}

/// `ContentKind::Video`: cover art (or black) behind the render child, plus the scrub strip.
pub(super) unsafe fn paint_content_video(
    hwnd: HWND,
    hdc: HDC,
    st: &ViewerState,
    text: u32,
    subtle: u32,
) {
    // For VIDEO the render child covers this area, so the black is only visible in the
    // brief pre-first-frame window. For AUDIO there is no picture and the child is hidden
    // (see `video::create`), so this IS the visible surface: paint the track's embedded
    // cover art aspect-fit on black, or plain black when it has none. Either way the
    // transport strip draws in the bottom band.
    let vr = video_rect(hwnd);
    match st.art.borrow().as_ref() {
        Some(art) => content::paint_image(hdc, &vr, art, 0x0000_0000, 1.0, (0, 0), None),
        None => fill(hdc, &vr, 0x0000_0000),
    }
    if let Some(v) = st.video.borrow().as_ref() {
        let focus_tb = match live_focus(st.focus.get(), st.focus_gen.get(), st.decode_gen.get()) {
            Some(FocusTarget::Transport(i)) => TBTNS.get(i).copied(),
            _ => None,
        };
        draw_scrub_strip(hwnd, hdc, &scrub_rect(hwnd), v, text, subtle, focus_tb);
    }
}
