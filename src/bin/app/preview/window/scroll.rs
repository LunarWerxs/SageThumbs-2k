//! Text and Markdown scrolling, including the owner-drawn scrollbar.

use super::*;

/// Shared text/Markdown scroll range. Every scroll path (wheel, keyboard selection, outline
/// jumps, resize clamping, and the custom scrollbar) uses this exact viewport/margin math.
#[derive(Clone, Copy)]
pub(in crate::preview) struct TextScrollMetrics {
    pub(in crate::preview) content: RECT,
    pub(in crate::preview) visible: i32,
    pub(in crate::preview) max_scroll: i32,
}

pub(in crate::preview) fn text_scroll_limits(
    content_h: i32,
    margin: i32,
    text_h: i32,
) -> (i32, i32) {
    let visible = content_h.saturating_sub(2 * margin).max(1);
    (visible, text_h.saturating_sub(visible).max(0))
}

pub(in crate::preview) unsafe fn text_scroll_metrics(hwnd: HWND) -> TextScrollMetrics {
    let content = content_rect(hwnd);
    let (visible, max_scroll) = text_scroll_limits(
        content.bottom - content.top,
        crate::win::dpi_scale(hwnd, 12),
        (*state(hwnd)).text_h.get(),
    );
    TextScrollMetrics {
        content,
        visible,
        max_scroll,
    }
}

/// Set an absolute text scroll position using the shared clamp. Returns whether it changed and
/// invalidates the content when it did; callers decide whether the move clears an outline choice.
pub(in crate::preview) unsafe fn set_text_scroll(hwnd: HWND, desired: i32) -> bool {
    let metrics = text_scroll_metrics(hwnd);
    let st = &*state(hwnd);
    let new = desired.clamp(0, metrics.max_scroll);
    if new == st.text_scroll.get() {
        return false;
    }
    st.text_scroll.set(new);
    let _ = InvalidateRect(Some(hwnd), Some(&metrics.content), false);
    true
}

/// Re-clamp the current scroll after a viewport/content-height change.
pub(in crate::preview) unsafe fn clamp_text_scroll(hwnd: HWND) -> bool {
    let st = &*state(hwnd);
    if !matches!(st.kind.get(), ContentKind::Text | ContentKind::Markdown) {
        return false;
    }
    set_text_scroll(hwnd, st.text_scroll.get())
}

/// Geometry for the owner-drawn text/Markdown scrollbar. Painting and pointer hit-testing both
/// consume this object, so the visual and interactive thumb cannot drift apart.
pub(in crate::preview) struct TextScrollbar {
    pub(in crate::preview) thumb: RECT,
    track: RECT,
    max_scroll: i32,
    page: i32,
}

pub(in crate::preview) fn scroll_thumb_geometry(
    track_h: i32,
    visible: i32,
    max_scroll: i32,
    scroll: i32,
    min_thumb: i32,
) -> Option<(i32, i32)> {
    let track_h = track_h.max(1);
    let visible = visible.max(1);
    if max_scroll <= 0 {
        return None;
    }
    let min_thumb = min_thumb.clamp(1, track_h);
    let text_h = visible as i64 + max_scroll as i64;
    let proportional = ((visible as i64 * track_h as i64) / text_h) as i32;
    let thumb_h = proportional.clamp(min_thumb, track_h);
    let travel = track_h - thumb_h;
    let thumb_offset =
        ((scroll.clamp(0, max_scroll) as i64 * travel as i64) / max_scroll as i64) as i32;
    Some((thumb_h, thumb_offset))
}

pub(in crate::preview) fn scroll_from_thumb_offset(
    thumb_offset: i32,
    travel: i32,
    max_scroll: i32,
) -> i32 {
    if travel <= 0 {
        return 0;
    }
    let thumb_offset = thumb_offset.clamp(0, travel);
    ((thumb_offset as i64 * max_scroll as i64 + travel as i64 / 2) / travel as i64) as i32
}

pub(in crate::preview) unsafe fn text_scrollbar(hwnd: HWND) -> Option<TextScrollbar> {
    let st = &*state(hwnd);
    if !matches!(st.kind.get(), ContentKind::Text | ContentKind::Markdown) {
        return None;
    }
    let metrics = text_scroll_metrics(hwnd);
    let track = metrics.content;
    let track_h = (track.bottom - track.top).max(1);
    let min_thumb = crate::win::dpi_scale(hwnd, 32);
    let (thumb_h, thumb_offset) = scroll_thumb_geometry(
        track_h,
        metrics.visible,
        metrics.max_scroll,
        st.text_scroll.get(),
        min_thumb,
    )?;
    let tw = crate::win::dpi_scale(hwnd, 4);
    let pad = crate::win::dpi_scale(hwnd, 3);
    let top = track.top + thumb_offset;
    Some(TextScrollbar {
        thumb: RECT {
            left: track.right - pad - tw,
            top,
            right: track.right - pad,
            bottom: top + thumb_h,
        },
        track,
        max_scroll: metrics.max_scroll,
        page: metrics.visible,
    })
}

pub(in crate::preview) enum TextScrollHit {
    Thumb(i32),
    Page(i32),
}

/// Hit-test the entire forgiving scrollbar lane. The thumb begins a capture drag; the track
/// above/below it pages by one viewport, matching a native scrollbar.
pub(in crate::preview) unsafe fn hit_text_scrollbar(
    hwnd: HWND,
    x: i32,
    y: i32,
) -> Option<TextScrollHit> {
    let sb = text_scrollbar(hwnd)?;
    let lane = RECT {
        left: sb.thumb.left - crate::win::dpi_scale(hwnd, 6),
        top: sb.track.top,
        right: sb.track.right,
        bottom: sb.track.bottom,
    };
    if x < lane.left || x >= lane.right || y < lane.top || y >= lane.bottom {
        return None;
    }
    if y < sb.thumb.top {
        Some(TextScrollHit::Page(-sb.page))
    } else if y >= sb.thumb.bottom {
        Some(TextScrollHit::Page(sb.page))
    } else {
        Some(TextScrollHit::Thumb(y - sb.thumb.top))
    }
}

pub(in crate::preview) unsafe fn invalidate_text_scrollbar(hwnd: HWND) {
    let content = content_rect(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&content), false);
}

pub(in crate::preview) unsafe fn set_scroll_hot(hwnd: HWND, hot: bool) -> bool {
    let st = &*state(hwnd);
    if st.scroll_hot.replace(hot) == hot {
        return false;
    }
    invalidate_text_scrollbar(hwnd);
    true
}

pub(in crate::preview) unsafe fn scroll_text_by(hwnd: HWND, dy: i32) -> bool {
    let st = &*state(hwnd);
    let desired = st.text_scroll.get().saturating_add(dy);
    if !set_text_scroll(hwnd, desired) {
        return false;
    }
    st.toc_sel.set(None);
    true
}

pub(in crate::preview) unsafe fn drag_text_scroll_thumb(hwnd: HWND, y: i32, grab_y: i32) {
    let Some(sb) = text_scrollbar(hwnd) else {
        return;
    };
    let thumb_h = sb.thumb.bottom - sb.thumb.top;
    let travel = (sb.track.bottom - sb.track.top - thumb_h).max(0);
    let thumb_offset = y - grab_y - sb.track.top;
    let new_scroll = scroll_from_thumb_offset(thumb_offset, travel, sb.max_scroll);
    let st = &*state(hwnd);
    if set_text_scroll(hwnd, new_scroll) {
        st.toc_sel.set(None);
    }
}
