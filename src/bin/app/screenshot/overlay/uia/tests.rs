use super::*;

/// A toolbar laid out the way a real capture would lay it out, so the index-to-element
/// mapping is tested against the same vector the mouse hit-tests.
fn laid_out() -> Vec<(Button, RECT)> {
    toolbar::layout(
        RECT {
            left: 200,
            top: 200,
            right: 900,
            bottom: 700,
        },
        1920,
        1080,
        96,
    )
}

#[test]
fn tooltips_reduce_to_a_spoken_name_without_the_key_hint() {
    assert_eq!(
        spoken_name("Rectangle (R) \u{2014} drag to draw"),
        "Rectangle"
    );
    assert_eq!(
        spoken_name("Colour (K) \u{2014} cycle the palette"),
        "Colour"
    );
    assert_eq!(spoken_name("Undo (Ctrl+Z)"), "Undo");
    assert_eq!(
        spoken_name("Copy to the clipboard (Ctrl+C / Enter)"),
        "Copy to the clipboard"
    );
    assert_eq!(spoken_name("Close (Esc)"), "Close");
    assert_eq!(
        spoken_name("Pick colour (E) \u{2014} click a pixel to copy its hex"),
        "Pick colour"
    );
}

/// The one tooltip with TWO parentheticals: only the trailing key hint may go, because
/// "(OCR)" is part of what the button is called.
#[test]
fn a_parenthetical_that_is_not_a_key_hint_survives() {
    assert_eq!(
        spoken_name("Copy text (OCR) (Ctrl+T) \u{2014} read the words in the region"),
        "Copy text (OCR)"
    );
    assert!(!looks_like_key_hint("OCR"));
    assert!(looks_like_key_hint("R"));
    assert!(looks_like_key_hint("Ctrl+Shift+Z"));
}

/// Every button the mouse can click has to end up with something to say. An empty name is
/// how an element becomes "button" and nothing else in a screen reader's list.
#[test]
fn every_focusable_toolbar_item_has_a_name_and_a_role() {
    for (btn, _) in laid_out() {
        if matches!(btn, Button::Sep) {
            continue;
        }
        assert!(
            !button_name(btn).is_empty(),
            "a focusable toolbar item must have a spoken name"
        );
        let is_tool = matches!(btn, Button::Tool(_));
        assert_eq!(
            button_control_type(btn) == UIA_RadioButtonControlTypeId,
            is_tool,
            "only the mutually exclusive tools may report as radio buttons"
        );
    }
}

/// The element list is layer 1's index space, separators excluded, and it must agree with
/// `toolbar::hit` about which indices are real.
#[test]
fn element_indices_skip_separators_and_match_the_mouse_hit_test() {
    let buttons = laid_out();
    let ids = toolbar_elements(&buttons);
    assert_eq!(
        ids.len(),
        buttons
            .iter()
            .filter(|(b, _)| !matches!(b, Button::Sep))
            .count()
    );
    for id in &ids {
        let FocusTarget::Toolbar(i) = *id else {
            panic!("toolbar_elements must only produce toolbar targets");
        };
        let (btn, r) = buttons[i];
        assert!(!matches!(btn, Button::Sep));
        // The centre of the element's own rect must hit the element's own button.
        let cx = (r.left + r.right) / 2;
        let cy = (r.top + r.bottom) / 2;
        // `Button` carries no Debug impl (it is an icon id, not a value anyone
        // prints), so this is an assert rather than an assert_eq.
        assert!(
            toolbar::hit(&buttons, cx, cy) == Some(btn),
            "the centre of an element's rect must hit that element's own button"
        );
    }
}

/// A 150%-scaled display sitting left of and above the primary: the same case the layer-1
/// DPI regression covers, but for the rect a screen reader draws its highlight with. Client
/// space starts at (0, 0) whatever the desktop origin is, so an untranslated rect would put
/// the highlight on the wrong monitor entirely.
#[test]
fn bounding_rectangles_are_reported_in_screen_coordinates() {
    let client = RECT {
        left: 320,
        top: 240,
        right: 420,
        bottom: 268,
    };
    let r = to_uia_rect(client, -2560, -120);
    assert_eq!(r.left, -2240.0);
    assert_eq!(r.top, 120.0);
    assert_eq!(r.width, 100.0);
    assert_eq!(r.height, 28.0);
    // An identity origin must leave client geometry untouched.
    let same = to_uia_rect(client, 0, 0);
    assert_eq!(same.left, 320.0);
    assert_eq!(same.top, 240.0);
}

#[test]
fn palette_colours_get_a_spoken_name_rather_than_six_hex_digits() {
    let names: Vec<&str> = PALETTE
        .iter()
        .map(|&(r, g, b)| color_name(rgb(r, g, b)))
        .collect();
    assert_eq!(
        names,
        vec!["Red", "Green", "Blue", "Yellow", "Black", "White"]
    );
    assert_eq!(
        swatch_name(Swatch::Custom(None)),
        "Empty custom colour slot"
    );
    assert_eq!(swatch_name(Swatch::Picker), "More colours");
    assert_eq!(
        swatch_name(Swatch::Custom(Some(rgb(250, 250, 250)))),
        "Custom colour, White"
    );
}

/// The font field announces what the font IS, not just that it is the font field, and every
/// preset row names its own face.
#[test]
fn text_flyout_rows_name_themselves() {
    assert_eq!(
        text_item_name(TextItem::FontField, "Segoe UI"),
        "Font, Segoe UI"
    );
    for (i, face) in toolbar::PRESET_FONTS.iter().enumerate() {
        assert_eq!(text_item_name(TextItem::FontOption(i), "Segoe UI"), *face);
    }
    // An index past the end must NOT panic: these come off a layout that lengthens and
    // shortens with the dropdown, and this binary aborts on panic.
    assert_eq!(
        text_item_name(TextItem::FontOption(usize::MAX), "Segoe UI"),
        "Font"
    );
    assert_eq!(text_item_name(TextItem::Bold, "Segoe UI"), "Bold");
}

/// Runtime ids and automation ids must separate the three groups, or a swatch and a toolbar
/// button with the same index would look like the same element to a client.
#[test]
fn element_ids_are_unique_across_the_three_groups() {
    let ids = [
        FocusTarget::Toolbar(3),
        FocusTarget::ColorFlyout(3),
        FocusTarget::TextFlyout(3),
    ];
    let runtime: Vec<[i32; 3]> = ids.iter().map(|id| runtime_id_parts(*id)).collect();
    assert_ne!(runtime[0], runtime[1]);
    assert_ne!(runtime[1], runtime[2]);
    assert_ne!(runtime[0], runtime[2]);
    for parts in &runtime {
        assert_eq!(parts[0], UiaAppendRuntimeId as i32);
        assert_eq!(parts[2], 3);
    }
    let keys: Vec<u64> = ids.iter().map(|id| key_of(*id)).collect();
    assert_ne!(keys[0], keys[1]);
    assert_ne!(keys[1], keys[2]);
    assert_ne!(keys[0], keys[2]);
    assert!(!keys.contains(&ROOT_KEY));
    assert_eq!(automation_id(FocusTarget::Toolbar(3)), "toolbar.3");
    assert_eq!(automation_id(FocusTarget::ColorFlyout(3)), "colour.3");
    assert_eq!(automation_id(FocusTarget::TextFlyout(3)), "text.3");
}

/// The message payload is the only place an element id is flattened into two integers, so
/// it has to round-trip for every group, and a payload that was never encoded (a zeroed
/// message, or a stray WM_APP from another window) must decode to nothing at all.
#[test]
fn element_ids_round_trip_through_the_message_payload() {
    for id in [
        FocusTarget::Toolbar(0),
        FocusTarget::Toolbar(23),
        FocusTarget::ColorFlyout(10),
        FocusTarget::TextFlyout(7),
    ] {
        let (w, l) = encode_target(id);
        assert_eq!(decode_target(w, l), Some(id));
    }
    assert_eq!(decode_target(WPARAM(0), LPARAM(0)), None);
    assert_eq!(decode_target(WPARAM(9), LPARAM(1)), None);
    assert_eq!(decode_target(WPARAM(1), LPARAM(-1)), None);
}
