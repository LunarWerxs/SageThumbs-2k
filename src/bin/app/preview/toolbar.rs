//! Caption toolbar: button rects, tooltips, and button hit-testing.

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::Controls::{
    TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW, TTM_SETMAXTIPWIDTH, TTM_UPDATETIPTEXTW,
    TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::transport::TBTNS;
use super::window::{btn_visible, state, Btn, BTNS, BTN_W, CAPTION_H, MIN_BTN_W, PAD};

/// Toolbar button rects (device px, in client coords), right-aligned in the caption. Hidden
/// buttons (see [`btn_visible`]) are omitted, so the visible set stays right-packed.
///
/// **Cells NARROW rather than overflowing when the visible set is wider than the caption.** The
/// most crowded case is a Markdown document that has headings, references a web image, and has a
/// source view, which shows twelve buttons at once; dragged to the 400 px minimum width that is
/// wider than the window, and the old fixed-width layout simply ran the leftmost buttons off the
/// left edge, where they were invisible and unclickable. Shrinking the CELL keeps every button
/// reachable and costs only padding, since the glyph is drawn centred and is ~14 px inside a
/// 38 px cell. When there is room the arithmetic picks [`BTN_W`] unchanged, so the normal window
/// lays out exactly as before.
pub(super) unsafe fn button_rects(hwnd: HWND) -> Vec<(Btn, RECT)> {
    let st = state(hwnd);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let cap = sc(CAPTION_H);
    let visible: Vec<Btn> = BTNS
        .iter()
        .rev()
        .copied()
        .filter(|&b| st.is_null() || btn_visible(&*st, b))
        .collect();
    let bw = cell_width(sc(BTN_W), sc(MIN_BTN_W), rc.right - sc(PAD), visible.len());
    let mut right = rc.right - sc(PAD);
    let mut out = Vec::with_capacity(visible.len());
    // Laid out right-to-left so Close sits at the far right.
    for b in visible {
        out.push((
            b,
            RECT {
                left: right - bw,
                top: 0,
                right,
                bottom: cap,
            },
        ));
        right -= bw;
    }
    out
}

/// How wide each toolbar cell should be: `full` when the `visible` buttons fit inside `avail`,
/// otherwise the widest that does fit, never below `min`.
///
/// Its own function so the rule is testable without a window, and so the "when there is room,
/// nothing changes" property is asserted rather than assumed — that is what keeps this from
/// quietly re-laying-out every normal preview.
pub(super) fn cell_width(full: i32, min: i32, avail: i32, visible: usize) -> i32 {
    match i32::try_from(visible) {
        Ok(n) if n > 0 => full.min(avail.max(0) / n).max(min),
        _ => full,
    }
}

/// Localized tooltip label for a toolbar button, for the state the button is CURRENTLY in.
///
/// **A toggle's tip names what the click WILL DO, never the state you are already in.** That is
/// the same convention [`super::paint::btn_glyph`] draws by, and the two have to agree or the
/// button says one thing and shows another. `pinned` / `src_view` are passed in rather than read
/// off the window so this stays a pure function the tests can drive through every combination —
/// which is the only way BOTH strings of a two-state button ever get checked.
pub(super) fn btn_tip(b: Btn, pinned: bool, src_view: bool) -> &'static str {
    crate::win::t(match b {
        // "Outline" NAMES the panel the button opens, the way `Settings` names a dialog — it is
        // not an imperative that goes stale in the other state, so it stays one string.
        Btn::Toc => "preview_tip_toc",
        // Reuses the string the Settings checkbox used before this moved into the
        // window, so all 36 translations carried straight over. Its text already describes
        // BOTH states ("Off: … On: …"), so it needs no second key either.
        Btn::MdImages => "tip_preview_md_remote",
        Btn::Source if src_view => "preview_tip_source_rendered",
        Btn::Source => "preview_tip_source",
        Btn::PdfPrev => "preview_tip_prev",
        Btn::PdfNext => "preview_tip_next",
        // One button, two meanings — it flips, so the tip has to say which way it goes
        // or it describes the state you are already in.
        Btn::Theme if crate::dark::is_dark() => "preview_tip_theme_light",
        Btn::Theme => "preview_tip_theme_dark",
        Btn::Settings => "preview_tip_settings",
        Btn::Pin if pinned => "preview_tip_unpin",
        Btn::Pin => "preview_tip_pin",
        Btn::Copy => "preview_tip_copy",
        Btn::SavePage => "preview_tip_savepage",
        Btn::Ocr => "preview_tip_ocr",
        Btn::Info => "preview_tip_info",
        Btn::Upload => "preview_tip_upload",
        Btn::OpenWith => "preview_tip_openwith",
        Btn::Open => "preview_tip_open",
        Btn::Print => "preview_tip_print",
        Btn::Close => "preview_tip_close",
    })
}

/// Every registered tool's tooltip TEXT, in the same tool-id order as [`tool_rects`].
///
/// Reads the two toggle states the caption's tips depend on (pin, view-source) and the two the
/// transport strip's do (mute, repeat) out of the window, so one call describes the whole bar as
/// it stands right now. [`update_tooltips`] compares this against what is registered.
pub(super) unsafe fn tool_texts(hwnd: HWND) -> Vec<&'static str> {
    let st = state(hwnd);
    let (pinned, src_view) = if st.is_null() {
        (false, false)
    } else {
        ((*st).pinned.get(), (*st).src_view.get())
    };
    // The transport strip's own toggles live on the video engine, which is absent for anything
    // that is not a playing video/track — and then its rects are empty and its tips unreachable,
    // so the "off" wording is the right answer rather than merely a harmless one.
    //
    // `try_borrow`, not `borrow`. This runs at the top of every `paint`, and the four places that
    // take `video.borrow_mut()` all DROP the previous engine, whose `Drop` destroys a child
    // window. A panic here would take the viewer out. Unlike the `strip_width` case that taught
    // this repo to distrust try_borrow fallbacks (see the memory: a fallback that changed
    // GEOMETRY laid PDFs out at the wrong width), a wrong answer here only affects tooltip TEXT
    // and is self-correcting — the next paint diffs against what we stored and re-sends it.
    let (muted, looping) = if st.is_null() {
        (false, false)
    } else {
        (*st).video.try_borrow().map_or((false, false), |v| {
            v.as_ref()
                .map_or((false, false), |v| (v.muted(), v.looping()))
        })
    };
    let mut out = Vec::with_capacity(BTNS.len() + TBTNS.len());
    for &b in BTNS.iter() {
        out.push(btn_tip(b, pinned, src_view));
    }
    for &t in TBTNS.iter() {
        out.push(super::transport::tbtn_tip(t, muted, looping));
    }
    out
}

/// Create the caption toolbar's tooltip control: one RECT tool per button, `TTF_SUBCLASS` so the
/// tip auto-tracks the mouse over the parent (the buttons are custom-drawn, not child HWNDs).
/// Returns `HWND::default()` on failure. Rects are refreshed on resize via [`update_tooltips`].
pub(super) unsafe fn create_tooltips(hwnd: HWND, hinst: HINSTANCE) -> HWND {
    let Ok(tip) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("tooltips_class32"),
        PCWSTR::null(),
        WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
        0,
        0,
        0,
        0,
        Some(hwnd),
        None,
        Some(hinst),
        None,
    ) else {
        return HWND::default();
    };
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, Some(WPARAM(0)), Some(LPARAM(320)));
    // One tool per BTNS entry (uId = BTNS index), then one per transport control (uId continues
    // past BTNS.len()). Hidden buttons and a hidden strip get an EMPTY rect so their tip can never
    // trigger; [`update_tooltips`] re-points every rect when the layout changes.
    let rects = tool_rects(hwnd);
    let texts = tool_texts(hwnd);
    for (idx, text) in texts.iter().enumerate() {
        add_tool(tip, hwnd, idx, rects[idx], text);
    }
    let st = state(hwnd);
    if !st.is_null() {
        *(*st).tip_rects.borrow_mut() = rects;
        *(*st).tip_texts.borrow_mut() = texts;
    }
    tip
}

/// Every registered tool's rect, in tool-id order: one per [`BTNS`] entry (hidden buttons get an
/// empty rect, which can never be hit), then one per transport control. This is THE layout both
/// the tooltip control and the painter answer to — [`button_rects`] is the single source, so a
/// tip cannot describe a button that is no longer under it.
pub(super) unsafe fn tool_rects(hwnd: HWND) -> Vec<RECT> {
    let rects = button_rects(hwnd);
    let mut out = Vec::with_capacity(BTNS.len() + TBTNS.len());
    for &b in BTNS.iter() {
        out.push(
            rects
                .iter()
                .find(|(bb, _)| *bb == b)
                .map(|(_, r)| *r)
                .unwrap_or_default(),
        );
    }
    for (_, r) in super::transport::transport_rects(hwnd) {
        out.push(r);
    }
    out
}

/// Whether the tooltip control's registered rects still describe `now`.
///
/// Split out as a plain function so the cache it guards is testable without a window. `RECT` is a
/// plain POD here; comparing the four edges avoids depending on whether the `windows` crate
/// derives `PartialEq` for it in a given version.
pub(super) fn tooltip_layout_changed(cached: &[RECT], now: &[RECT]) -> bool {
    cached.len() != now.len()
        || cached.iter().zip(now).any(|(a, b)| {
            a.left != b.left || a.top != b.top || a.right != b.right || a.bottom != b.bottom
        })
}

/// Build the `TTTOOLINFOW` naming tool `id` on `hwnd`, with `lpszText` pointing at the caller-owned
/// `text` buffer, which must outlive the `SendMessageW` that consumes the struct. `rect` is left
/// default; the callers that carry one fill it in over the result.
fn tool_info(hwnd: HWND, id: usize, text: &[u16]) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_SUBCLASS,
        hwnd,
        uId: id,
        lpszText: PWSTR(text.as_ptr() as *mut u16),
        ..Default::default()
    }
}

/// Register one rect tool. comctl32 copies the text on add, so the wide temporary is fine.
unsafe fn add_tool(tip: HWND, hwnd: HWND, id: usize, rect: RECT, text: &str) {
    let text = crate::win::wide(text);
    let mut ti = TTTOOLINFOW {
        rect,
        ..tool_info(hwnd, id, &text)
    };
    SendMessageW(
        tip,
        TTM_ADDTOOLW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut ti as *mut _ as isize)),
    );
}

/// Whether the tooltip control's registered TEXTS still describe `now`.
///
/// The twin of [`tooltip_layout_changed`], and the reason it exists is a shipped bug: the tips
/// were registered ONCE at window creation and never re-sent, while three of them are chosen from
/// runtime state. Clicking the light/dark button flipped the glyph on the next paint and left the
/// tooltip describing the theme you had just LEFT — reported by a user as "the moon says light
/// background", which it did, permanently, from the first click onward.
pub(super) fn tooltip_text_changed(cached: &[&str], now: &[&str]) -> bool {
    cached.len() != now.len() || cached.iter().zip(now).any(|(a, b)| a != b)
}

/// Re-point every tooltip tool at its control's current rect, and re-send any tip whose TEXT the
/// window's state has changed, if either moved since the last call. No-op if the tip control
/// wasn't created (the headless shot never makes one).
///
/// **Called from the PAINT path, and that is the fix, not an optimisation.** A caption button
/// appears or disappears from several places — a decode landing (`Btn::Ocr` needs
/// `ContentKind::Image`, which only becomes true when the worker's bitmap arrives), a PDF page
/// count, a Markdown document turning out to have headings, a resize — and the previous design
/// asked each of those to remember to call this. `on_render` did not, so on every image the two
/// leftmost tips sat one button to the right of where they belonged: hovering Copy said "Keep on
/// top" and the pin had no tooltip at all. Any state change that alters the buttons MUST repaint
/// the caption or the drawn toolbar itself would be wrong, so the paint is the one place that
/// cannot be forgotten. The comparison against the last-synced rects keeps the common repaint
/// (a scroll notch, a hover) down to a `Vec` compare with no window messages at all.
pub(super) unsafe fn update_tooltips(hwnd: HWND, tip: HWND) {
    if tip.is_invalid() {
        return;
    }
    let st = state(hwnd);
    if st.is_null() {
        return;
    }
    // Rects and texts move for DIFFERENT reasons — a resize moves every rect and no text, a
    // theme click changes one text and no rect — so they are compared and sent independently.
    // Folding them into one guard would make either change re-send the other for nothing.
    let rects = tool_rects(hwnd);
    if tooltip_layout_changed(&(*st).tip_rects.borrow(), &rects) {
        for (idx, r) in rects.iter().enumerate() {
            move_tool(tip, hwnd, idx, *r);
        }
        *(*st).tip_rects.borrow_mut() = rects;
    }
    let texts = tool_texts(hwnd);
    if tooltip_text_changed(&(*st).tip_texts.borrow(), &texts) {
        for (idx, text) in texts.iter().enumerate() {
            set_tool_text(tip, hwnd, idx, text);
        }
        *(*st).tip_texts.borrow_mut() = texts;
    }
}

/// Re-send one registered tool's text. comctl32 copies it, same as on add, so the wide temporary
/// is fine — but it must OUTLIVE the `SendMessageW`, which is why it is a named local.
unsafe fn set_tool_text(tip: HWND, hwnd: HWND, id: usize, text: &str) {
    let text = crate::win::wide(text);
    let mut ti = tool_info(hwnd, id, &text);
    SendMessageW(
        tip,
        TTM_UPDATETIPTEXTW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut ti as *mut _ as isize)),
    );
}

/// Re-point one registered tool at a new rect.
unsafe fn move_tool(tip: HWND, hwnd: HWND, id: usize, rect: RECT) {
    let mut ti = TTTOOLINFOW {
        cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_SUBCLASS,
        hwnd,
        uId: id,
        rect,
        ..Default::default()
    };
    SendMessageW(
        tip,
        TTM_NEWTOOLRECTW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut ti as *mut _ as isize)),
    );
}

// ===== Keyboard focus model =====
//
// The caption toolbar and the video transport strip (`transport.rs`) are painted onto the
// client area and were mouse-only. This gives Tab a way in: Tab/Shift+Tab move through the
// two bars as one linear sequence, Left/Right move within whichever bar has focus, Up/Down
// jump straight to the other bar, Enter/Space activate exactly what a click would, and Escape
// leaves the bar without closing the window. Mirrors the conventions the screenshot editor's
// overlay focus model (`screenshot/overlay/input.rs`) already established: an index-based
// `FocusTarget`, a cheap `is_focus_key` bail, and "not mine" returns `None` so every other key
// cluster is unaffected until the user actually presses Tab.

/// Where keyboard focus sits among the toolbar's two bars. Stores an index into whichever bar
/// is CURRENTLY VISIBLE (the same list [`button_rects`] / [`TBTNS`] give the mouse), never a
/// `BTNS` index — visibility changes per document (`btn_visible`), and a stored `BTNS` index
/// would silently point at a different button, or none, the moment the visible set shifts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FocusTarget {
    /// Index into `button_rects(hwnd)` (the caption bar's visible buttons).
    Caption(usize),
    /// Index into `TBTNS` (the transport strip's controls).
    Transport(usize),
}

/// Whether a keypress could possibly matter to the toolbar focus model — a cheap bail so a key
/// that is never Tab/arrow/Enter/Space/Escape costs one comparison rather than laying the
/// toolbar out.
fn is_focus_key(vk: u16) -> bool {
    vk == VK_TAB.0
        || vk == VK_RETURN.0
        || vk == VK_SPACE.0
        || vk == VK_ESCAPE.0
        || vk == VK_LEFT.0
        || vk == VK_RIGHT.0
        || vk == VK_UP.0
        || vk == VK_DOWN.0
}

/// The focus to actually honour: `None` once the load that set it has moved on. Every load
/// bumps `ViewerState::decode_gen` — including the in-place video-fallback path in `window.rs`
/// that never goes through `loader::load` at all — so comparing generations catches both a
/// full file switch and a same-file content-kind change ("focus is cleared ... on a content
/// change") without any load path having to remember to clear focus itself.
pub(super) fn live_focus(
    focus: Option<FocusTarget>,
    focus_gen: u64,
    decode_gen: u64,
) -> Option<FocusTarget> {
    if focus_gen != decode_gen {
        None
    } else {
        focus
    }
}

/// Pull a maybe-stale focus back inside the CURRENT visible bars, or drop it. A shrunk bar —
/// fewer buttons than when focus was set, e.g. a mid-load content-kind change swapping which
/// buttons `btn_visible` allows — leaves a stored index out of bounds; rather than clamp it
/// onto a DIFFERENT button, this drops focus entirely: "focus resets ... when the bar's
/// contents change".
pub(super) fn repair_focus(
    focus: Option<FocusTarget>,
    caption_len: usize,
    transport_len: usize,
) -> Option<FocusTarget> {
    match focus {
        Some(FocusTarget::Caption(i)) if i < caption_len => focus,
        Some(FocusTarget::Transport(i)) if i < transport_len => focus,
        _ => None,
    }
}

/// Tab's landing spot with nothing focused yet: the first visible caption button (there is
/// always at least Close), or the transport strip's first control if the caption bar is
/// somehow empty (defensive only).
fn first_focus(caption_len: usize, transport_len: usize) -> Option<FocusTarget> {
    if caption_len > 0 {
        Some(FocusTarget::Caption(0))
    } else if transport_len > 0 {
        Some(FocusTarget::Transport(0))
    } else {
        None
    }
}

/// One step of a wrapping `len`-item cycle from `from`, or `None` for an empty list.
fn wrap_index(len: usize, from: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let from = from.min(len - 1); // a stale index clamps rather than wraps oddly
    Some(if forward {
        (from + 1) % len
    } else {
        (from + len - 1) % len
    })
}

/// Left/Right: one step `forward`/back, WITHIN the current bar only, wrapping at both ends —
/// Left/Right never cross from the caption bar to the transport strip (that is Up/Down's job).
pub(super) fn arrow_step(
    from: FocusTarget,
    caption_len: usize,
    transport_len: usize,
    forward: bool,
) -> Option<FocusTarget> {
    match from {
        FocusTarget::Caption(i) => wrap_index(caption_len, i, forward).map(FocusTarget::Caption),
        FocusTarget::Transport(i) => {
            wrap_index(transport_len, i, forward).map(FocusTarget::Transport)
        }
    }
}

/// Down (`to_transport = true`) / Up (`false`): jump straight to the OTHER bar, landing on the
/// same index clamped to its length. `None` when there is nowhere to go — no transport strip to
/// move into, or already on the bar that direction would leave the key unhandled instead of
/// eaten for nothing.
pub(super) fn switch_bar(
    from: FocusTarget,
    caption_len: usize,
    transport_len: usize,
    to_transport: bool,
) -> Option<FocusTarget> {
    match (from, to_transport) {
        (FocusTarget::Caption(i), true) if transport_len > 0 => {
            Some(FocusTarget::Transport(i.min(transport_len - 1)))
        }
        (FocusTarget::Transport(i), false) if caption_len > 0 => {
            Some(FocusTarget::Caption(i.min(caption_len - 1)))
        }
        _ => None,
    }
}

/// Tab / Shift+Tab: the caption bar and the transport strip read as ONE linear sequence, so
/// stepping off either end of one bar lands on the start (or end) of the other — the "or Tab
/// again" half of the caption-to-transport transition; [`switch_bar`] (Down/Up) is the direct
/// jump between bars instead. Dispatches to one of four flat per-direction helpers below —
/// each keeps its own if/else-if chain but none of the "which bar, which direction" nesting
/// that used to wrap all four together.
pub(super) fn tab_step(
    from: FocusTarget,
    caption_len: usize,
    transport_len: usize,
    forward: bool,
) -> Option<FocusTarget> {
    match (from, forward) {
        (FocusTarget::Caption(i), true) => caption_tab_forward(i, caption_len, transport_len),
        (FocusTarget::Caption(i), false) => caption_tab_backward(i, caption_len, transport_len),
        (FocusTarget::Transport(i), true) => transport_tab_forward(i, caption_len, transport_len),
        (FocusTarget::Transport(i), false) => transport_tab_backward(i, caption_len, transport_len),
    }
}

/// Tab forward off the caption bar: next caption button, else the transport strip's first
/// control, else (defensively, no transport strip) wrap back to the caption bar's own start.
fn caption_tab_forward(i: usize, caption_len: usize, transport_len: usize) -> Option<FocusTarget> {
    if i + 1 < caption_len {
        Some(FocusTarget::Caption(i + 1))
    } else if transport_len > 0 {
        Some(FocusTarget::Transport(0))
    } else if caption_len > 0 {
        Some(FocusTarget::Caption(0))
    } else {
        None
    }
}

/// Shift+Tab backward off the caption bar: previous caption button, else the transport strip's
/// last control, else (defensively) wrap back to the caption bar's own end.
fn caption_tab_backward(i: usize, caption_len: usize, transport_len: usize) -> Option<FocusTarget> {
    if i > 0 {
        Some(FocusTarget::Caption(i - 1))
    } else if transport_len > 0 {
        Some(FocusTarget::Transport(transport_len - 1))
    } else if caption_len > 0 {
        Some(FocusTarget::Caption(caption_len - 1))
    } else {
        None
    }
}

/// Tab forward off the transport strip: next control, else wrap to the caption bar's start,
/// else (defensively, no caption buttons) wrap back to the transport strip's own start.
fn transport_tab_forward(
    i: usize,
    caption_len: usize,
    transport_len: usize,
) -> Option<FocusTarget> {
    if i + 1 < transport_len {
        Some(FocusTarget::Transport(i + 1))
    } else if caption_len > 0 {
        Some(FocusTarget::Caption(0))
    } else if transport_len > 0 {
        Some(FocusTarget::Transport(0))
    } else {
        None
    }
}

/// Shift+Tab backward off the transport strip: previous control, else wrap to the caption
/// bar's end. Unlike the other three directions this has NO self-wrap fallback (matches the
/// original behaviour exactly — the transport strip never wraps to its own end on Shift+Tab).
fn transport_tab_backward(
    i: usize,
    caption_len: usize,
    _transport_len: usize,
) -> Option<FocusTarget> {
    if i > 0 {
        Some(FocusTarget::Transport(i - 1))
    } else if caption_len > 0 {
        Some(FocusTarget::Caption(caption_len - 1))
    } else {
        None
    }
}

/// `WM_KEYDOWN`'s toolbar-keyboard-focus cluster: Tab/Shift+Tab, arrow movement, Enter/Space to
/// activate, Escape to leave the bar. `None` means "not mine" — the caller falls through to
/// every other key cluster exactly as before this model existed, so a user who never presses
/// Tab cannot observe any of it, not even a swallowed keystroke. Everything except Tab is
/// additionally gated on focus already being set: Space and Escape already mean "close the
/// window" in manual mode (`window::keydown_lifecycle`), and this must only steal them while a
/// toolbar button HAS focus, never on every keypress.
pub(super) unsafe fn keydown_toolbar_focus(
    hwnd: HWND,
    st: &super::window::ViewerState,
    vk: u16,
    shift: bool,
) -> Option<LRESULT> {
    if !is_focus_key(vk) || (st.focus.get().is_none() && vk != VK_TAB.0) {
        return None;
    }
    let buttons = button_rects(hwnd);
    let caption_len = buttons.len();
    let transport_len = if super::transport::transport_showing(hwnd) {
        TBTNS.len()
    } else {
        0
    };
    let live = live_focus(st.focus.get(), st.focus_gen.get(), st.decode_gen.get());
    let focus = repair_focus(live, caption_len, transport_len);
    st.focus.set(focus);

    let intent = focus_key_intent(vk, shift, focus, caption_len, transport_len);
    execute_focus_intent(hwnd, st, &buttons, intent)
}

/// What a handled toolbar-focus keypress should DO, computed with no side effects — kept
/// separate from [`execute_focus_intent`] so the "which key means what" logic (this function)
/// and the "how do we actually apply it" logic (the executor) are each independently readable.
enum FocusIntent {
    /// Set (Tab, arrow move, Escape-clears-to-`None`) or leave unchanged-but-consumed focus.
    SetFocus(Option<FocusTarget>),
    /// Activate whatever is currently focused (Enter / Space).
    Activate(FocusTarget),
    /// Not this cluster's key, or nowhere to go (e.g. Up from the caption bar) — the caller
    /// leaves the keystroke unhandled rather than eating a no-op press.
    Unhandled,
}

/// Decide the [`FocusIntent`] for one keydown, given the already-repaired `focus`. Mirrors the
/// original inline dispatch order exactly: Tab first (works with no focus set), then every
/// other key requires `focus` to already be `Some`.
fn focus_key_intent(
    vk: u16,
    shift: bool,
    focus: Option<FocusTarget>,
    caption_len: usize,
    transport_len: usize,
) -> FocusIntent {
    if vk == VK_TAB.0 {
        let next = match focus {
            None => first_focus(caption_len, transport_len),
            Some(f) => tab_step(f, caption_len, transport_len, !shift),
        };
        return FocusIntent::SetFocus(next);
    }
    let Some(focus) = focus else {
        return FocusIntent::Unhandled;
    };
    if vk == VK_ESCAPE.0 {
        return FocusIntent::SetFocus(None);
    }
    if vk == VK_RETURN.0 || vk == VK_SPACE.0 {
        return FocusIntent::Activate(focus);
    }
    arrow_focus_intent(vk, focus, caption_len, transport_len)
}

/// The arrow-key quarter of [`focus_key_intent`]: Left/Right step within a bar, Up/Down switch
/// bars, everything else is unhandled.
fn arrow_focus_intent(
    vk: u16,
    focus: FocusTarget,
    caption_len: usize,
    transport_len: usize,
) -> FocusIntent {
    let Some((forward, vertical)) = arrow_direction(vk) else {
        return FocusIntent::Unhandled;
    };
    let next = if vertical {
        switch_bar(focus, caption_len, transport_len, forward)
    } else {
        arrow_step(focus, caption_len, transport_len, forward)
    };
    match next {
        Some(n) => FocusIntent::SetFocus(Some(n)),
        None => FocusIntent::Unhandled,
    }
}

/// Left/Right/Up/Down as `(forward, vertical)`, or `None` for any other key.
fn arrow_direction(vk: u16) -> Option<(bool, bool)> {
    match vk {
        v if v == VK_LEFT.0 => Some((false, false)),
        v if v == VK_RIGHT.0 => Some((true, false)),
        v if v == VK_UP.0 => Some((false, true)),
        v if v == VK_DOWN.0 => Some((true, true)),
        _ => None,
    }
}

/// Apply a [`FocusIntent`]: the only part of this cluster that touches `hwnd`/`st`/the button
/// list, so it is the only part a Win32-side-effect bug can hide in.
unsafe fn execute_focus_intent(
    hwnd: HWND,
    st: &super::window::ViewerState,
    buttons: &[(Btn, RECT)],
    intent: FocusIntent,
) -> Option<LRESULT> {
    match intent {
        FocusIntent::Unhandled => None,
        FocusIntent::SetFocus(next) => {
            set_focus(hwnd, st, next);
            Some(LRESULT(0))
        }
        FocusIntent::Activate(focus) => {
            activate_focus(hwnd, buttons, focus);
            Some(LRESULT(0))
        }
    }
}

/// Enter/Space on whichever bar has focus: run the caption button's action, or trigger the
/// transport control, then repaint. A focus index whose target has since vanished is a no-op
/// (the keystroke was still consumed — see [`execute_focus_intent`]).
unsafe fn activate_focus(hwnd: HWND, buttons: &[(Btn, RECT)], focus: FocusTarget) {
    match focus {
        FocusTarget::Caption(i) => {
            if let Some((btn, _)) = buttons.get(i).copied() {
                super::window::do_action(hwnd, btn);
                // `do_action` can destroy `hwnd` (Close, or Open on a successful launch) —
                // never touch `st`/`hwnd` again once that has happened.
                if IsWindow(Some(hwnd)).as_bool() {
                    invalidate_focus_bars(hwnd);
                }
            }
        }
        FocusTarget::Transport(i) => {
            if let Some(&tb) = TBTNS.get(i) {
                super::transport::activate(hwnd, tb);
                invalidate_focus_bars(hwnd);
            }
        }
    }
}

/// Set (or clear) focus, recording the load generation it was set under, and repaint both bars.
pub(super) unsafe fn set_focus(
    hwnd: HWND,
    st: &super::window::ViewerState,
    focus: Option<FocusTarget>,
) {
    st.focus.set(focus);
    if focus.is_some() {
        st.focus_gen.set(st.decode_gen.get());
    }
    invalidate_focus_bars(hwnd);
}

/// Repaint both toolbar bars — the caption strip and (if showing) the transport strip — so a
/// focus-ring move, or a click that clears focus, is never left half-drawn on either.
pub(super) unsafe fn invalidate_focus_bars(hwnd: HWND) {
    super::window::invalidate_caption(hwnd);
    let sr = super::transport::scrub_rect(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
}

/// Which button (if any) contains the client-space point.
pub(super) unsafe fn hit_button(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    for (b, r) in button_rects(hwnd) {
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
            return BTNS.iter().position(|&bb| bb == b);
        }
    }
    None
}

#[cfg(test)]
mod tests;
