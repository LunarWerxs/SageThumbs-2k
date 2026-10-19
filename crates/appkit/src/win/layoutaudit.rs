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
use windows::core::PWSTR;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    DrawTextW, GetDC, ReleaseDC, ScreenToClient, SelectObject, DRAW_TEXT_FORMAT, DT_CALCRECT,
    DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, HGDIOBJ,
};
use windows::Win32::UI::Controls::{
    HDM_GETBITMAPMARGIN, HDM_GETITEMCOUNT, HDM_GETITEMRECT, LVCF_TEXT, LVCOLUMNW, LVIR_LABEL,
    LVITEMW, LVM_GETCOLUMNW, LVM_GETEXTENDEDLISTVIEWSTYLE, LVM_GETHEADER, LVM_GETITEMCOUNT,
    LVM_GETITEMRECT, LVM_GETITEMTEXTW, LVM_GETSTRINGWIDTHW,
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
        if let Some(detail) = list_overflow(k.hwnd) {
            out.push(finding("column", k.hwnd, detail));
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

/// What a report-view checkbox list cuts off: a visible column header too narrow for its
/// title, or a row whose first-column label does not fit. In a checkbox list that label is the
/// thing being ticked (a file extension, a menu entry), so a row cut to `.ap…` cannot be told
/// from its neighbours; a user at 150% scaling reported exactly that (2026-10-19). Later
/// columns may ellipsize on purpose (a long description), and so may lists without checkboxes.
unsafe fn list_overflow(list: HWND) -> Option<String> {
    const LVS_TYPEMASK: u32 = 0x3;
    const LVS_REPORT: u32 = 0x1;
    const LVS_EX_CHECKBOXES: usize = 0x4;
    if !class_of(list).eq_ignore_ascii_case("SysListView32")
        || style_of(list) & LVS_TYPEMASK != LVS_REPORT
        || SendMessageW(list, LVM_GETEXTENDEDLISTVIEWSTYLE, None, None).0 as usize
            & LVS_EX_CHECKBOXES
            == 0
    {
        return None;
    }
    header_overflow(list).or_else(|| first_column_overflow(list))
}

/// The first column header of `list` whose title needs more room than its column has.
unsafe fn header_overflow(list: HWND) -> Option<String> {
    let header = HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut c_void);
    if header.is_invalid() || !IsWindowVisible(header).as_bool() {
        return None;
    }
    // The header insets its text by this margin on each side.
    let margin = SendMessageW(header, HDM_GETBITMAPMARGIN, None, None).0 as i32;
    let cols = SendMessageW(header, HDM_GETITEMCOUNT, None, None).0 as i32;
    (0..cols).find_map(|col| {
        let mut rc = RECT::default();
        SendMessageW(
            header,
            HDM_GETITEMRECT,
            Some(WPARAM(col as usize)),
            Some(LPARAM(&mut rc as *mut _ as isize)),
        );
        let title = column_title(list, col);
        let (text_w, _) = measure(header, &title, DT_SINGLELINE | DT_NOPREFIX, 0);
        let has = rc.right - rc.left;
        (text_w + 2 * margin > has + SLACK).then(|| {
            format!(
                "header `{title}` needs {} px, has {has}",
                text_w + 2 * margin
            )
        })
    })
}

/// The widest row label in `list`'s first column that does not fit it, with how many do not.
/// A row of nothing but box-drawing characters is a divider drawn as a rule, meant to run off
/// the edge, and is skipped.
unsafe fn first_column_overflow(list: HWND) -> Option<String> {
    // The room the control keeps beside a label: LVSCW_AUTOSIZE leaves exactly this (6 px,
    // 3 x SM_CXEDGE at system 96 dpi) next to the widest label, at every `--dpi`.
    let pad = 3 * GetSystemMetrics(SM_CXEDGE);
    let rows = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
    let cut: Vec<(String, i32, i32)> = (0..rows)
        .filter_map(|row| {
            let label = item_label(list, row);
            if label
                .chars()
                .all(|c| ('\u{2500}'..='\u{257F}').contains(&c))
            {
                return None;
            }
            let wide: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            let need = SendMessageW(
                list,
                LVM_GETSTRINGWIDTHW,
                None,
                Some(LPARAM(wide.as_ptr() as isize)),
            )
            .0 as i32
                + pad;
            let mut rc = RECT {
                left: LVIR_LABEL as i32,
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_GETITEMRECT,
                Some(WPARAM(row as usize)),
                Some(LPARAM(&mut rc as *mut _ as isize)),
            );
            let has = rc.right - rc.left;
            (need > has + SLACK).then_some((label, need, has))
        })
        .collect();
    let (label, need, has) = cut.iter().max_by_key(|(_, need, has)| need - has)?;
    Some(format!(
        "{} row label(s) cut off in the first column, worst `{label}` needs {need} px, has {has}",
        cut.len()
    ))
}

/// `list`'s column `col` title.
unsafe fn column_title(list: HWND, col: i32) -> String {
    let mut buf = [0u16; 256];
    let mut c = LVCOLUMNW {
        mask: LVCF_TEXT,
        pszText: PWSTR(buf.as_mut_ptr()),
        cchTextMax: buf.len() as i32,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_GETCOLUMNW,
        Some(WPARAM(col as usize)),
        Some(LPARAM(&mut c as *mut _ as isize)),
    );
    wide_until_nul(&buf)
}

/// `list`'s row `row` first-column text.
unsafe fn item_label(list: HWND, row: i32) -> String {
    let mut buf = [0u16; 512];
    let mut it = LVITEMW {
        iSubItem: 0,
        pszText: PWSTR(buf.as_mut_ptr()),
        cchTextMax: buf.len() as i32,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_GETITEMTEXTW,
        Some(WPARAM(row as usize)),
        Some(LPARAM(&mut it as *mut _ as isize)),
    );
    wide_until_nul(&buf)
}

fn wide_until_nul(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
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

/// Where a process that audits many windows in a row (`--audit-layout`) sends them, the shot
/// the next audit is filed under, and how many controls the last one looked at.
struct Sink {
    path: std::path::PathBuf,
    shot: String,
    audited: Option<usize>,
}

static SINK: std::sync::Mutex<Option<Sink>> = std::sync::Mutex::new(None);

fn with_sink<R>(f: impl FnOnce(&mut Option<Sink>) -> R) -> R {
    f(&mut SINK.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Send every audit in this process to `path`, and have captures skip their PNG: the audit
/// reads where the controls are, not the pixels, and the encode is most of a capture's cost.
pub fn set_audit_sink(path: &std::path::Path) {
    with_sink(|s| {
        *s = Some(Sink {
            path: path.to_path_buf(),
            shot: String::new(),
            audited: None,
        })
    });
}

/// File the audits that follow under `shot` (`rename`, `settings:3`, ...).
pub fn set_audit_shot(shot: &str) {
    with_sink(|s| {
        if let Some(s) = s {
            s.shot = shot.to_string();
            s.audited = None;
        }
    });
}

/// How many visible controls the audit since the last [`set_audit_shot`] looked at; `None`
/// when no audit ran, which is how a run notices a window that never got built.
pub fn take_audited() -> Option<usize> {
    with_sink(|s| s.as_mut().and_then(|s| s.audited.take()))
}

/// Whether captures should skip their PNG (a sink is set).
pub(super) fn audit_only() -> bool {
    with_sink(|s| s.is_some())
}

/// Record that `shot` could not be audited, as `{"shot", "error"}`.
pub fn record_audit_error(shot: &str, why: &str) {
    if let Some(path) = with_sink(|s| s.as_ref().map(|s| s.path.clone())) {
        let line = serde_json::json!({ "shot": shot, "error": why }).to_string() + "\n";
        append(&path, &line);
    }
}

fn append(path: &std::path::Path, text: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(text.as_bytes());
    }
}

unsafe fn count_visible(parent: HWND) -> usize {
    visible_children(parent)
        .iter()
        .map(|k| 1 + count_visible(k.hwnd))
        .sum()
}

/// Audit `hwnd` and append what was found to the audit sink, or else to the file
/// `ST2K_LAYOUT_AUDIT` names (a no-op when neither is set): one JSON object a line, a finding
/// per problem (it has a `kind`), then one `{"shot", "window", "audited": N}` line saying how
/// many visible controls were looked at, so a window that built empty cannot pass for clean.
pub unsafe fn audit_window(hwnd: HWND) {
    let dest = with_sink(|s| s.as_ref().map(|s| (s.path.clone(), s.shot.clone())));
    let Some((path, shot)) =
        dest.or_else(|| std::env::var_os("ST2K_LAYOUT_AUDIT").map(|p| (p.into(), String::new())))
    else {
        return;
    };
    let window = text_of(hwnd);
    let audited = count_visible(hwnd);
    let mut lines: String = audit_layout(hwnd)
        .into_iter()
        .map(|f| {
            serde_json::json!({
                "shot": shot,
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
    lines += &(serde_json::json!({ "shot": shot, "window": window, "audited": audited })
        .to_string()
        + "\n");
    append(&path, &lines);
    with_sink(|s| {
        if let Some(s) = s {
            s.audited = Some(audited);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "system" fn plain_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    /// A report-view checkbox list whose one 40 px column holds a long row and a divider row.
    unsafe fn narrow_checkbox_list(parent: HWND, hinst: HINSTANCE, id: i32) -> HWND {
        use windows::Win32::UI::Controls::{
            InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LVIF_TEXT,
            LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVS_EX_CHECKBOXES,
            LVS_REPORT, WC_LISTVIEWW,
        };
        let icc = INITCOMMONCONTROLSEX {
            dwSize: core::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let style = WS_VISIBLE | WINDOW_STYLE(LVS_REPORT);
        let list = ctl(parent, WC_LISTVIEWW, "", style, 16, 40, 200, 100, id, hinst);
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            Some(WPARAM(0)),
            Some(LPARAM(LVS_EX_CHECKBOXES as isize)),
        );
        let mut title: Vec<u16> = "Ext".encode_utf16().chain(Some(0)).collect();
        let mut col = LVCOLUMNW {
            mask: windows::Win32::UI::Controls::LVCF_WIDTH | LVCF_TEXT,
            cx: dpi_scale(list, 40),
            pszText: PWSTR(title.as_mut_ptr()),
            ..Default::default()
        };
        SendMessageW(
            list,
            LVM_INSERTCOLUMNW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut col as *mut _ as isize)),
        );
        for (row, label) in ["a row label far wider than its column", "──────────"]
            .into_iter()
            .enumerate()
        {
            let mut text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            let mut it = LVITEMW {
                mask: LVIF_TEXT,
                iItem: row as i32,
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_INSERTITEMW,
                Some(WPARAM(0)),
                Some(LPARAM(&mut it as *mut _ as isize)),
            );
        }
        list
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
            let list = narrow_checkbox_list(hwnd, hinst, 3);
            let found = audit_layout(hwnd);
            let _ = DestroyWindow(hwnd);
            let column = found.iter().find(|f| f.kind == "column" && f.id == 3);
            assert!(
                column.is_some_and(|f| f.detail.starts_with("1 row label(s)")),
                "the checkbox row cut off by its column is reported, and the divider row drawn \
                 as a rule is not: {found:?} ({list:?})"
            );
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
