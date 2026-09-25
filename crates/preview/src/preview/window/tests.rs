#![cfg(test)]

use super::{
    nav_key_action, pdf_wheel_action, savepage_visible, scroll_from_thumb_offset,
    scroll_thumb_geometry, sort_paths_like_explorer, text_scroll_limits, wheel_notches,
    ContentKind, NavKey, WheelAction,
};
use std::path::PathBuf;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_UP,
};

/// `Btn::SavePage` gains a THIRD way to show, on top of the pre-existing PDF/animation
/// cases: a live video player. Without this arm the button stayed hidden for every video,
/// which is the whole defect this queue item exists to fix.
#[test]
fn savepage_shows_for_a_video_with_a_live_player() {
    assert!(savepage_visible(ContentKind::Video, 0, 0, true));
    // No player yet (fell back to an info card) — nothing to grab a frame from.
    assert!(!savepage_visible(ContentKind::Video, 0, 0, false));
}

/// The pre-existing PDF/animation gates must survive the video arm being added alongside
/// them — a video-only regression check would miss a change that broke these instead.
#[test]
fn savepage_keeps_its_pdf_and_animation_gates() {
    assert!(savepage_visible(ContentKind::Image, 3, 0, false)); // multi-page PDF
    assert!(savepage_visible(ContentKind::Image, 0, 4, false)); // animation, >1 frame
    assert!(!savepage_visible(ContentKind::Image, 1, 1, false)); // neither paged nor animated
    assert!(!savepage_visible(ContentKind::Text, 3, 4, false)); // wrong content kind entirely
}

#[test]
fn every_scroll_path_shares_the_same_limits() {
    assert_eq!(text_scroll_limits(600, 12, 2400), (576, 1824));
    assert_eq!(text_scroll_limits(600, 12, 500), (576, 0));
}

#[test]
fn scrollbar_geometry_tracks_the_document_range() {
    let (thumb_h, top) = scroll_thumb_geometry(600, 576, 1824, 0, 32).unwrap();
    assert_eq!((thumb_h, top), (144, 0));

    let (_, middle) = scroll_thumb_geometry(600, 576, 1824, 912, 32).unwrap();
    let (_, bottom) = scroll_thumb_geometry(600, 576, 1824, 1824, 32).unwrap();
    assert_eq!(middle, 228);
    assert_eq!(bottom, 456);
}

#[test]
fn scrollbar_drag_clamps_to_both_ends() {
    assert_eq!(scroll_from_thumb_offset(-50, 456, 1824), 0);
    assert_eq!(scroll_from_thumb_offset(228, 456, 1824), 912);
    assert_eq!(scroll_from_thumb_offset(900, 456, 1824), 1824);
}

#[test]
fn precision_wheel_deltas_accumulate_without_being_lost() {
    assert_eq!(wheel_notches(0, 30), (0, 30));
    assert_eq!(wheel_notches(30, 90), (1, 0));
    assert_eq!(wheel_notches(0, -60), (0, -60));
    assert_eq!(wheel_notches(-60, -60), (-1, 0));
    assert_eq!(wheel_notches(45, -45), (0, 0));
}

/// The wheel over a PDF. 2.3.1 shipped with this doing nothing at all: the routing sat
/// inside the wndproc, fell through to the ordinary image zoom, and that drives state the
/// tiled PDF paint never reads. Every test I had drove the keyboard or called the scroll
/// function directly, so all of them passed against a build where rolling the wheel on a
/// PDF was inert while the release notes promised it scrolled.
#[test]
fn a_bare_wheel_over_a_pdf_scrolls_the_document() {
    assert_eq!(pdf_wheel_action(false, false), WheelAction::Scroll);
}

/// The modifiers, and the precedence between them. Ctrl wins over Shift, so a hand resting
/// on both gets the magnifier rather than a sideways jolt.
#[test]
fn ctrl_magnifies_and_shift_slides_sideways() {
    assert_eq!(pdf_wheel_action(true, false), WheelAction::Zoom);
    assert_eq!(pdf_wheel_action(false, true), WheelAction::Pan);
    assert_eq!(
        pdf_wheel_action(true, true),
        WheelAction::Zoom,
        "Ctrl beats Shift; both held must not pan"
    );
}

/// The regression this whole split exists for. A multi-page PDF used to swallow ←/→ for
/// paging, and since `goto_pdf_page` clamps at both ends there was then NO key at all that
/// reached the next file: the popup was a dead end until you closed it. Asserted for BOTH
/// states of the flag, because the bug was precisely that one state behaved differently.
#[test]
fn left_right_always_move_between_files_even_on_a_multipage_pdf() {
    for multipage_pdf in [false, true] {
        assert_eq!(
            nav_key_action(multipage_pdf, VK_RIGHT.0),
            Some(NavKey::File(1)),
            "→ must reach the next FILE (multipage_pdf = {multipage_pdf})"
        );
        assert_eq!(
            nav_key_action(multipage_pdf, VK_LEFT.0),
            Some(NavKey::File(-1)),
            "← must reach the previous FILE (multipage_pdf = {multipage_pdf})"
        );
    }
}

/// Paging still has to work, or the fix above would just have deleted the feature.
#[test]
fn a_multipage_pdf_pages_on_up_down_and_pgup_pgdn() {
    assert_eq!(nav_key_action(true, VK_DOWN.0), Some(NavKey::Page(1)));
    assert_eq!(nav_key_action(true, VK_NEXT.0), Some(NavKey::Page(1)));
    assert_eq!(nav_key_action(true, VK_UP.0), Some(NavKey::Page(-1)));
    assert_eq!(nav_key_action(true, VK_PRIOR.0), Some(NavKey::Page(-1)));
}

/// Off a multi-page PDF nothing changed: PgUp/PgDn keep flipping files, and ↑/↓ stay
/// unclaimed so scrolling and the default handling still get them.
#[test]
fn ordinary_content_keeps_its_previous_key_meanings() {
    assert_eq!(nav_key_action(false, VK_NEXT.0), Some(NavKey::File(1)));
    assert_eq!(nav_key_action(false, VK_PRIOR.0), Some(NavKey::File(-1)));
    assert_eq!(nav_key_action(false, VK_UP.0), None);
    assert_eq!(nav_key_action(false, VK_DOWN.0), None);
}

/// Whatever the content, there is always a way out of the current file. A future edit that
/// claims ←/→ for anything else fails here rather than shipping another dead end.
#[test]
fn no_content_state_can_trap_the_keyboard() {
    for multipage_pdf in [false, true] {
        let escapes = [VK_RIGHT.0, VK_LEFT.0, VK_NEXT.0, VK_PRIOR.0]
            .into_iter()
            .filter(|&vk| matches!(nav_key_action(multipage_pdf, vk), Some(NavKey::File(_))))
            .count();
        assert!(
            escapes >= 2,
            "multipage_pdf = {multipage_pdf}: only {escapes} keys still reach another file"
        );
    }
}

#[test]
fn sibling_navigation_uses_explorer_logical_order() {
    let input = ["image10.png", "image2.png", "image1.png"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let names: Vec<String> = sort_paths_like_explorer(input)
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["image1.png", "image2.png", "image10.png"]);
}
