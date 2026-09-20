#![cfg(test)]

use super::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;

/// A minimal, never-shown `Shot` — enough state for the keyboard paths these tests
/// exercise. `shot`/`dimmed` are throwaway 1x1 compatible bitmaps, not the real
/// full-screen snapshot (nothing here paints).
unsafe fn test_shot() -> Box<Shot> {
    let screen = GetDC(None);
    let shot = CreateCompatibleDC(Some(screen));
    let shot_bmp = CreateCompatibleBitmap(screen, 1, 1);
    let dimmed = CreateCompatibleDC(Some(screen));
    let dimmed_bmp = CreateCompatibleBitmap(screen, 1, 1);
    let _ = ReleaseDC(None, screen);
    Box::new(Shot {
        shot,
        shot_bmp,
        dimmed,
        dimmed_bmp,
        vx: 0,
        vy: 0,
        vw: 100,
        vh: 100,
        // None, matching the real overlay's initial state (before any region drag) —
        // these tests only exercise the keyboard/typing paths, none of which read
        // `sel`, and leaving it `None` means `handle_key`'s VK_RETURN path takes the
        // "nothing to copy" branch instead of a real `finish_copy` clipboard write,
        // which would otherwise clobber whatever is on the machine's clipboard.
        sel: None,
        sel_dragging: false,
        sel_anchor: POINT::default(),
        tool: Tool::Text,
        cur_color: {
            let (r, g, b) = PALETTE[0];
            rgb(r, g, b)
        },
        thickness: 3,
        shapes: Vec::new(),
        redo: Vec::new(),
        draw_from: None,
        pen_pts: Vec::new(),
        cur: POINT::default(),
        typing: None,
        typing_drag: false,
        eye_copied: false,
        pending_hi: None,
        number_next: 1,
        selected: None,
        move_from: None,
        text_font: tools::default_text_font(18),
        color_flyout: false,
        customs: Vec::new(),
        cust_colors: [COLORREF(0); 16],
        text_flyout: false,
        font_dropdown: false,
        hover_btn: None,
        tip_show: false,
        focus: None, // matching the real overlay: nothing is focused until Tab asks
        born: 0,     // 0, not "now": tests must not trip the just-opened SETTLE_CLOSE_MS guard
        automation: None,
        ocr_mode: false,
        win_hint: None,
        win_hint_scan_ms: 0,
        tb_cache_key: None,
        tb_cache: Vec::new(),
    })
}

/// A never-shown window carrying a live `test_shot()` in `GWLP_USERDATA`, so a test
/// can drive `handle_key`/`shot_wndproc` directly instead of reimplementing their
/// logic. `DestroyWindow` (called by every test before it returns) delivers a real
/// synchronous `WM_DESTROY` to `shot_wndproc`, which frees the box and the GDI
/// objects exactly the way the production teardown does — no separate cleanup path
/// for tests to drift from.
unsafe fn test_window() -> HWND {
    let class = w!("st2k_test_shot_input");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(shot_wndproc),
        hInstance: HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0),
        lpszClassName: class,
        ..Default::default()
    };
    let _ = RegisterClassW(&wc); // a 2nd test's registration of the same class is a no-op
    #[allow(clippy::unwrap_used)]
    // test-only: an unusable HWND means abort the run, not skip it
    let hwnd = CreateWindowExW(
        Default::default(),
        class,
        w!(""),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let state = test_shot();
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    hwnd
}

/// A `test_window()` whose annotation buffer is already open with `text` — the setup
/// every `WM_CHAR` test needs before it can drive `shot_wndproc`.
unsafe fn typing_window(text: &str) -> HWND {
    let hwnd = test_window();
    let s = &mut *shot_ptr(hwnd);
    s.typing = Some((POINT::default(), text.to_string()));
    hwnd
}

/// The annotation buffer currently open in `hwnd`, or `None` if nothing is being typed.
unsafe fn typed_text(hwnd: HWND) -> Option<String> {
    let s = &*shot_ptr(hwnd);
    s.typing.as_ref().map(|(_, b)| b.clone())
}

/// The keyboard focus model may only claim keys this window did nothing with, and it
/// must claim ALL of them, because a key it does not list can never move focus no matter
/// what the traversal code says. Every letter, every digit and every bracket has to stay
/// out: those are the tool shortcuts and the size/thickness keys, and swallowing one
/// would change the editor for a user who has never pressed Tab.
#[test]
fn only_the_dead_keys_belong_to_the_focus_model() {
    for k in [
        VK_TAB, VK_SPACE, VK_RETURN, VK_ESCAPE, VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN,
    ] {
        assert!(is_focus_key(k.0), "a focus key is missing from the table");
    }
    for vk in b'A'..=b'Z' {
        assert!(!is_focus_key(vk as u16), "letters are tool shortcuts");
    }
    for vk in [
        0xDBu16,
        0xDD,
        VK_DELETE.0,
        VK_SHIFT.0,
        VK_CONTROL.0,
        VK_F8.0,
    ] {
        assert!(
            !is_focus_key(vk),
            "this key already means something else here"
        );
    }
}

/// DEL (0x7F, sent by Ctrl+Backspace on some layouts) must never land in the
/// annotation buffer as a literal character — it used to render as a tofu glyph.
#[test]
fn wm_char_rejects_del_without_inserting_a_tofu_glyph() {
    unsafe {
        let hwnd = typing_window("");
        shot_wndproc(hwnd, WM_CHAR, WPARAM(0x7F), LPARAM(0));
        let buf = typed_text(hwnd);
        assert_eq!(
            buf,
            Some(String::new()),
            "DEL (0x7F) must not be inserted into the annotation text"
        );
        let _ = DestroyWindow(hwnd);
    }
}

/// A normal printable character is unaffected by the DEL exclusion — the guard must
/// be specific to 0x7F, not an accidental narrowing of the whole `u >= 0x20` range.
#[test]
fn wm_char_still_accepts_an_ordinary_character() {
    unsafe {
        let hwnd = typing_window("");
        shot_wndproc(hwnd, WM_CHAR, WPARAM(b'A' as usize), LPARAM(0));
        let buf = typed_text(hwnd);
        assert_eq!(buf.as_deref(), Some("A"));
        let _ = DestroyWindow(hwnd);
    }
}

/// Enter mid-annotation must insert a newline (via the WM_CHAR that follows a real
/// keypress), never commit + close the capture — that made multi-line annotations
/// structurally impossible.
#[test]
fn enter_while_typing_inserts_a_newline_instead_of_closing_the_capture() {
    unsafe {
        let hwnd = typing_window("line one");
        let consumed = handle_key(hwnd, VK_RETURN.0);
        assert!(
            !consumed,
            "VK_RETURN while typing must not report itself as handled by handle_key \
             (WM_CHAR does the actual insert)"
        );
        {
            let s = &*shot_ptr(hwnd);
            assert!(
                s.typing.is_some(),
                "Enter mid-annotation must not commit/close the text"
            );
        }
        // The WM_CHAR(0x0D) a real Enter keypress generates must land the newline.
        shot_wndproc(hwnd, WM_CHAR, WPARAM(0x0D), LPARAM(0));
        let buf = typed_text(hwnd);
        assert_eq!(buf.as_deref(), Some("line one\n"));
        let _ = DestroyWindow(hwnd);
    }
}

/// A clipboard failure via Enter or Ctrl+C must not still destroy the overlay —
/// the old bug discarded the capture with nothing copied and no way to retry it. A
/// degenerate zero-size selection makes `compose`/`finish_copy` fail without ever
/// touching the real clipboard, which is how this is exercised safely from a test.
#[test]
fn enter_and_ctrl_c_keep_the_overlay_open_when_the_copy_fails() {
    unsafe {
        let hwnd = test_window();
        {
            let s = &mut *shot_ptr(hwnd);
            s.sel = Some(RECT {
                left: 5,
                top: 5,
                right: 5,
                bottom: 5,
            }); // zero-size -> compose()/finish_copy fail with no side effects
        }
        let consumed = handle_key(hwnd, VK_RETURN.0);
        assert!(!consumed);
        assert!(
            IsWindow(Some(hwnd)).as_bool(),
            "Enter must not close the overlay when the copy failed"
        );

        let handled = {
            let s = &mut *shot_ptr(hwnd);
            on_key_clipboard_action(hwnd, s, true, b'C' as u16)
        };
        assert_eq!(handled, Some(false));
        assert!(
            IsWindow(Some(hwnd)).as_bool(),
            "Ctrl+C must not close the overlay when the copy failed"
        );
        let _ = DestroyWindow(hwnd);
    }
}

/// Enter when NOT typing keeps its normal accept-and-close behaviour (the fix must
/// only change the mid-typing case, not disable Enter generally).
#[test]
fn enter_while_not_typing_still_closes_the_capture() {
    unsafe {
        let hwnd = test_window();
        {
            let s = &mut *shot_ptr(hwnd);
            s.typing = None;
        }
        let _ = handle_key(hwnd, VK_RETURN.0);
        assert!(
            !IsWindow(Some(hwnd)).as_bool(),
            "Enter with no active typing must still accept + close the capture"
        );
    }
}

/// Deleting a shape must leave it restorable via Ctrl+Y, not discard it outright —
/// the bug this replaces cleared `redo` instead of pushing the removed shape onto it.
#[test]
fn deleting_a_shape_pushes_it_onto_redo_instead_of_discarding_it() {
    unsafe {
        let hwnd = test_window();
        {
            let s = &mut *shot_ptr(hwnd);
            s.shapes.push(Shape::Number {
                at: POINT { x: 5, y: 5 },
                n: 7,
                color: s.cur_color,
            });
            s.selected = Some(0);
        }
        let consumed = handle_key(hwnd, VK_DELETE.0);
        assert!(consumed);
        {
            let s = &*shot_ptr(hwnd);
            assert!(
                s.shapes.is_empty(),
                "the shape must be removed from the live list"
            );
            assert_eq!(
                s.redo.len(),
                1,
                "the deleted shape must be preserved for Ctrl+Y, not discarded"
            );
            match &s.redo[0] {
                Shape::Number { n, .. } => assert_eq!(*n, 7, "wrong shape landed in redo"),
                _ => panic!("expected the deleted Number shape in redo, got a different kind"),
            }
        }
        let _ = DestroyWindow(hwnd);
    }
}

/// The flyout's +/- and the `[`/`]` keyboard shortcut must not disagree on the
/// text-size ceiling — a size the flyout allowed used to silently shrink the moment
/// `]` was pressed next, because this path clamped to a smaller, different max.
#[test]
fn text_size_keyboard_shortcut_shares_the_flyout_ceiling() {
    unsafe {
        let hwnd = test_window();
        {
            let s = &mut *shot_ptr(hwnd);
            s.tool = Tool::Text;
            // As if the flyout's + button had just set it to ITS max.
            s.text_font.lfHeight = -tools::TEXT_SIZE_MAX;
        }
        let consumed = handle_key(hwnd, 0xDD); // ']'
        assert!(consumed);
        {
            let s = &*shot_ptr(hwnd);
            assert_eq!(
                -s.text_font.lfHeight,
                tools::TEXT_SIZE_MAX,
                "the keyboard shortcut must not clamp below what the flyout just allowed"
            );
        }
        let _ = DestroyWindow(hwnd);
    }
}

/// Ctrl+U is the same "accept and close" shape as Ctrl+C/Ctrl+T/Ctrl+S (G199b). With
/// no selection yet (this harness's default `Shot`), it must be a safe, recognized
/// no-op rather than falling through to a tool shortcut — and must NOT actually run
/// `compose_and_spawn` (which would launch a real `--upload` child process).
#[test]
fn ctrl_u_with_no_selection_is_a_safe_recognized_no_op() {
    unsafe {
        let hwnd = test_window();
        let handled = {
            let s = &mut *shot_ptr(hwnd);
            on_key_clipboard_action(hwnd, s, true, b'U' as u16)
        };
        assert_eq!(
            handled,
            Some(false),
            "Ctrl+U must be recognized as handled, not fall through to a tool shortcut"
        );
        assert!(
            IsWindow(Some(hwnd)).as_bool(),
            "no selection means nothing to upload — the overlay must stay open"
        );
        let _ = DestroyWindow(hwnd);
    }
}

/// `accumulate_move_undo` sums consecutive ticks for the SAME shape, so Ctrl+Z can
/// invert the whole drag (not just its last tick).
#[test]
fn accumulate_move_undo_sums_deltas_for_the_same_shape() {
    let first = accumulate_move_undo(None, 3, 5, -2);
    assert_eq!(first, (3, 5, -2));
    let second = accumulate_move_undo(Some(first), 3, 1, 4);
    assert_eq!(
        second,
        (3, 6, 2),
        "the second tick must add onto the first, not replace it"
    );
}

/// A grab that changes WHICH shape is selected must restart the total at zero rather
/// than inheriting a previous drag's accumulated delta — the defensive fallback in
/// `accumulate_move_undo` in case some caller forgets to reset `MOVE_UNDO` on grab.
#[test]
fn accumulate_move_undo_restarts_when_the_grabbed_shape_changes() {
    let stale = Some((3, 5, -2));
    let fresh = accumulate_move_undo(stale, 7, 1, 1);
    assert_eq!(
        fresh,
        (7, 1, 1),
        "a different shape index must not inherit the old total"
    );
}

/// A shot holding a single Number shape at `(x, y)`, ready for an undo test.
unsafe fn shot_with_number_at(x: i32, y: i32) -> Box<Shot> {
    let mut s = test_shot();
    s.shapes.push(Shape::Number {
        at: POINT { x, y },
        n: 1,
        color: s.cur_color,
    });
    s
}

/// The bug this replaces: Move-dragging mutated a shape's position with no undo
/// entry recorded at all, so Ctrl+Z after a move either did nothing useful or
/// deleted an unrelated shape. `undo_step` must invert the recorded drag in place
/// instead of falling back to popping the shape off entirely.
#[test]
fn undo_step_reverts_a_pending_move_instead_of_deleting_the_shape() {
    unsafe {
        let mut s = shot_with_number_at(50, 50);
        undo_step(&mut s, Some((0, 10, -4)));
        assert_eq!(s.shapes.len(), 1, "a move-undo must not remove the shape");
        match &s.shapes[0] {
            Shape::Number { at, .. } => assert_eq!(
                (at.x, at.y),
                (40, 54),
                "the total drag delta must be inverted exactly"
            ),
            _ => panic!("expected the Number shape to remain, got a different kind"),
        }
        assert!(s.redo.is_empty(), "a move-undo is not a deletion");
    }
}

/// A grab that never actually dragged (zero accumulated delta — the user merely
/// clicked a shape with the Move tool) must NOT swallow the next Ctrl+Z as a no-op:
/// it has to fall through to the normal "undo the last created shape" behaviour.
#[test]
fn undo_step_falls_back_to_popping_when_no_real_move_happened() {
    unsafe {
        let mut s = shot_with_number_at(1, 1);
        undo_step(&mut s, Some((0, 0, 0)));
        assert!(
            s.shapes.is_empty(),
            "a zero-delta grab must fall through to popping the shape"
        );
        assert_eq!(s.redo.len(), 1);
    }
}
