//! What a person SEES in a window, checked by machine: every visible control must sit inside
//! its parent, must not overlap a sibling, and must have room for its own text.
//!
//! Every headless `--shot` capture runs this when `ST2K_LAYOUT_AUDIT` names a file (see
//! [`super::capture_and_destroy`]), so each window the shot harness can build is audited at
//! whatever DPI (`--dpi`) and language (the `Lang` setting) the capture uses. It exists because
//! the tests checked that the code ran and never what the dialogs looked like: Rename with
//! pattern shipped with its Rename/Cancel row below the bottom edge (issue #48), and nothing in
//! the suite could have noticed.

use super::*;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    DrawTextW, GetDC, ReleaseDC, ScreenToClient, SelectObject, DRAW_TEXT_FORMAT, DT_CALCRECT,
    DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, HGDIOBJ,
};

/// One thing wrong with one control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutFinding {
    /// `clipped` (past its parent's client edge), `overlap` (covers a visible sibling) or
    /// `text` (its text needs more room than the control has).
    pub kind: &'static str,
    pub class: String,
    pub id: i32,
    pub text: String,
    /// The measurement behind the finding, in device px.
    pub detail: String,
}

/// Room a check box or radio button needs for its glyph and the gap after it, in design px.
const CHECK_GLYPH: i32 = 20;
/// Room a push button needs around its text, in design px (both sides together).
const BUTTON_PAD: i32 = 10;
/// Pixels of slack before a measurement counts, for rounding in the font metrics.
const SLACK: i32 = 1;

/// Audit every visible control under `hwnd`, recursively.
pub unsafe fn audit_layout(hwnd: HWND) -> Vec<LayoutFinding> {
    let mut out = Vec::new();
    audit_children(hwnd, &mut out);
    out
}

/// A visible child of `parent` with its rect in `parent`'s client coordinates.
struct Child {
    hwnd: HWND,
    rect: RECT,
}

/// `c`'s window rect in `parent`'s client coordinates, or `None` when it has no area. Shared
/// with `dialogs::controls_extent`, the fit that grows a dialog to its controls.
pub(super) unsafe fn rect_in_parent(parent: HWND, c: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    GetWindowRect(c, &mut r).ok()?;
    let mut tl = POINT {
        x: r.left,
        y: r.top,
    };
    let mut br = POINT {
        x: r.right,
        y: r.bottom,
    };
    let _ = ScreenToClient(parent, &mut tl);
    let _ = ScreenToClient(parent, &mut br);
    (br.x > tl.x && br.y > tl.y).then_some(RECT {
        left: tl.x,
        top: tl.y,
        right: br.x,
        bottom: br.y,
    })
}

/// `parent`'s direct children, in z-order.
pub(super) unsafe fn children(parent: HWND) -> Vec<HWND> {
    let mut out = Vec::new();
    let mut c = GetWindow(parent, GW_CHILD).ok();
    while let Some(h) = c.filter(|h| !h.is_invalid()) {
        out.push(h);
        c = GetWindow(h, GW_HWNDNEXT).ok();
    }
    out
}

unsafe fn visible_children(parent: HWND) -> Vec<Child> {
    children(parent)
        .into_iter()
        .filter(|&h| IsWindowVisible(h).as_bool())
        .filter_map(|h| rect_in_parent(parent, h).map(|rect| Child { hwnd: h, rect }))
        .collect()
}

unsafe fn audit_children(parent: HWND, out: &mut Vec<LayoutFinding>) {
    let mut client = RECT::default();
    if GetClientRect(parent, &mut client).is_err() {
        return;
    }
    let kids = visible_children(parent);
    for k in &kids {
        out.extend(clip_finding(k, client));
        if let Some(detail) = text_overflow(k.hwnd, k.rect) {
            out.push(finding("text", k.hwnd, detail));
        }
        audit_children(k.hwnd, out);
    }
    overlap_findings(&kids, out);
}

/// A `clipped` finding when `k` reaches past `client` on any side.
unsafe fn clip_finding(k: &Child, client: RECT) -> Option<LayoutFinding> {
    let r = k.rect;
    let past = r.left < -SLACK
        || r.top < -SLACK
        || r.right > client.right + SLACK
        || r.bottom > client.bottom + SLACK;
    past.then(|| {
        finding(
            "clipped",
            k.hwnd,
            format!(
                "control {} outside client {}x{}",
                fmt_rect(r),
                client.right,
                client.bottom
            ),
        )
    })
}

/// An `overlap` finding for every pair of `kids` that covers each other (see [`overlap_counts`]).
unsafe fn overlap_findings(kids: &[Child], out: &mut Vec<LayoutFinding>) {
    for (i, a) in kids.iter().enumerate() {
        for b in kids[i + 1..].iter().filter(|b| overlap_counts(a, b)) {
            let detail = format!(
                "{} covers {} `{}` {}",
                fmt_rect(a.rect),
                class_of(b.hwnd),
                text_of(b.hwnd),
                fmt_rect(b.rect)
            );
            out.push(finding("overlap", a.hwnd, detail));
        }
    }
}

/// Whether two visible siblings overlap by more than a hairline, leaving out the pairs that
/// overlap on purpose: a group box around its members, a container around its own children,
/// and an empty static used as a panel or divider behind other controls.
unsafe fn overlap_counts(a: &Child, b: &Child) -> bool {
    let w = a.rect.right.min(b.rect.right) - a.rect.left.max(b.rect.left);
    let h = a.rect.bottom.min(b.rect.bottom) - a.rect.top.max(b.rect.top);
    if w <= 2 || h <= 2 {
        return false;
    }
    let decorative = |c: HWND| {
        is_group_box(c)
            || GetWindow(c, GW_CHILD).is_ok_and(|x| !x.is_invalid())
            || (class_of(c).eq_ignore_ascii_case("Static") && text_of(c).is_empty())
    };
    !decorative(a.hwnd) && !decorative(b.hwnd)
}

unsafe fn is_group_box(c: HWND) -> bool {
    class_of(c).eq_ignore_ascii_case("Button") && (style_of(c) & 0xF) == 7
}

/// What a control's own text needs beyond its rect, or `None` when it fits (or when the
/// control does not lay out text the standard way: edits, lists, owner-drawn, ellipsized).
unsafe fn text_overflow(c: HWND, r: RECT) -> Option<String> {
    let text = text_of(c);
    if text.is_empty() {
        return None;
    }
    let style = style_of(c);
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let class = class_of(c);
    let (flags, pad, wraps) = if class.eq_ignore_ascii_case("Button") {
        button_measure(c, style)?
    } else if class.eq_ignore_ascii_case("Static") {
        static_measure(style)?
    } else {
        return None;
    };
    let avail = w - pad;
    let (need_w, need_h) = measure(c, &text, flags, if wraps { avail } else { 0 });
    if need_w > avail + SLACK {
        return Some(format!("text needs {need_w} px wide, has {avail}"));
    }
    if need_h > h + SLACK {
        return Some(format!("text needs {need_h} px tall, has {h}"));
    }
    None
}

/// DrawText flags, horizontal padding and whether the text wraps, for a BUTTON; `None` for a
/// button that does not draw text itself (owner-drawn, icon, bitmap).
unsafe fn button_measure(c: HWND, style: u32) -> Option<(DRAW_TEXT_FORMAT, i32, bool)> {
    const BS_ICON: u32 = 0x40;
    const BS_BITMAP: u32 = 0x80;
    const BS_MULTILINE: u32 = 0x2000;
    if style & (BS_ICON | BS_BITMAP) != 0 {
        return None;
    }
    let pad = match style & 0xF {
        0 | 1 => dpi_scale(c, BUTTON_PAD),
        2..=6 | 9 => dpi_scale(c, CHECK_GLYPH),
        7 => dpi_scale(c, 16),
        _ => return None, // owner-drawn and the rest paint their own text
    };
    if style & BS_MULTILINE != 0 {
        Some((DT_WORDBREAK, pad, true))
    } else {
        Some((DT_SINGLELINE, pad, false))
    }
}

/// DrawText flags, padding and wrap for a STATIC; `None` for one that is not plain text
/// (icons, bitmaps, owner-drawn) or that ellipsizes on purpose.
fn static_measure(style: u32) -> Option<(DRAW_TEXT_FORMAT, i32, bool)> {
    const SS_NOPREFIX: u32 = 0x80;
    const SS_ELLIPSISMASK: u32 = 0xC000;
    if style & SS_ELLIPSISMASK != 0 {
        return None;
    }
    let prefix = if style & SS_NOPREFIX != 0 {
        DT_NOPREFIX
    } else {
        DRAW_TEXT_FORMAT(0)
    };
    match style & 0x1F {
        0..=2 => Some((DT_WORDBREAK | prefix, 0, true)),
        0xB | 0xC => Some((DT_SINGLELINE | prefix, 0, false)),
        _ => None,
    }
}

/// The (width, height) `text` takes in `c`'s own font; `wrap_w > 0` wraps at that width.
unsafe fn measure(c: HWND, text: &str, flags: DRAW_TEXT_FORMAT, wrap_w: i32) -> (i32, i32) {
    let hdc = GetDC(Some(c));
    let font = SendMessageW(c, WM_GETFONT, None, None).0;
    let old = (font != 0).then(|| SelectObject(hdc, HGDIOBJ(font as *mut c_void)));
    let mut w: Vec<u16> = text.encode_utf16().collect();
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: wrap_w.max(0),
        bottom: 0,
    };
    DrawTextW(hdc, &mut w, &mut rc, flags | DT_CALCRECT);
    if let Some(o) = old {
        SelectObject(hdc, o);
    }
    ReleaseDC(Some(c), hdc);
    (rc.right - rc.left, rc.bottom - rc.top)
}

unsafe fn finding(kind: &'static str, c: HWND, detail: String) -> LayoutFinding {
    LayoutFinding {
        kind,
        class: class_of(c),
        id: GetDlgCtrlID(c),
        text: text_of(c),
        detail,
    }
}

unsafe fn class_of(c: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(c, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe fn text_of(c: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = GetWindowTextW(c, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe fn style_of(c: HWND) -> u32 {
    GetWindowLongW(c, GWL_STYLE) as u32
}

fn fmt_rect(r: RECT) -> String {
    format!("({},{})-({},{})", r.left, r.top, r.right, r.bottom)
}

/// Append `hwnd`'s findings to the file `ST2K_LAYOUT_AUDIT` names, one JSON object a line,
/// each tagged with the window's title. A no-op when the variable is unset.
pub(super) unsafe fn audit_to_env_file(hwnd: HWND) {
    use std::io::Write;
    let Some(path) = std::env::var_os("ST2K_LAYOUT_AUDIT") else {
        return;
    };
    let window = text_of(hwnd);
    let lines: String = audit_layout(hwnd)
        .into_iter()
        .map(|f| {
            serde_json::json!({
                "window": window,
                "kind": f.kind,
                "class": f.class,
                "id": f.id,
                "text": f.text,
                "detail": f.detail,
            })
            .to_string()
                + "\n"
        })
        .collect();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(lines.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "system" fn plain_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    /// The sweep over every window and language only means something if the auditor sees what
    /// it is looking for: a control past the client edge, and a label too small for its text.
    /// Without this, an auditor that quietly stopped reporting would read as "all clean".
    #[test]
    fn it_reports_a_control_past_the_edge_and_text_that_does_not_fit() {
        unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
            let class = w!("St2kLayoutAuditTest");
            register_app_class(class, Some(plain_wndproc), hinst);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("audit"),
                WS_OVERLAPPED | WS_CAPTION,
                0,
                0,
                dpi_scale_dpi(300, 96),
                dpi_scale_dpi(200, 96),
                None,
                None,
                Some(hinst),
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let past = ctl(hwnd, BUTTON, "OK", WS_VISIBLE, 16, 400, 80, 30, 1, hinst);
            let cramped = ctl(
                hwnd,
                STATIC,
                "A label far too long for the little box it was given",
                WS_VISIBLE,
                16,
                16,
                60,
                16,
                2,
                hinst,
            );
            let found = audit_layout(hwnd);
            let _ = DestroyWindow(hwnd);
            assert!(
                found.iter().any(|f| f.kind == "clipped" && f.id == 1),
                "the button below the edge is reported: {found:?} ({past:?})"
            );
            assert!(
                found.iter().any(|f| f.kind == "text" && f.id == 2),
                "the cramped label is reported: {found:?} ({cramped:?})"
            );
        }
    }
}
