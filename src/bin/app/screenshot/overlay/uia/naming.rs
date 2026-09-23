//! What each element of the screenshot editor is called and reported as: automation ids, runtime ids, control types and the names a screen reader speaks.

use super::*;

/// A stable, non-localised handle for an element, for test harnesses and for a client that
/// wants to remember "the Undo button" across a re-read of the tree.
pub(super) fn automation_id(id: FocusTarget) -> String {
    match id {
        FocusTarget::Toolbar(i) => format!("toolbar.{i}"),
        FocusTarget::ColorFlyout(i) => format!("colour.{i}"),
        FocusTarget::TextFlyout(i) => format!("text.{i}"),
    }
}

/// `[UiaAppendRuntimeId, group tag, index]`, the shape UIA expects from a fragment whose root
/// is a window: the first element tells UIA to prefix the host window's own id, so the rest
/// only has to be unique WITHIN this overlay.
pub(super) fn runtime_id_parts(id: FocusTarget) -> [i32; 3] {
    let (tag, index) = match id {
        FocusTarget::Toolbar(i) => (1i32, i),
        FocusTarget::ColorFlyout(i) => (2i32, i),
        FocusTarget::TextFlyout(i) => (3i32, i),
    };
    [
        UiaAppendRuntimeId as i32,
        tag,
        i32::try_from(index).unwrap_or(i32::MAX),
    ]
}

/// The tools are one mutually exclusive set, so they read as radio buttons; everything else on
/// the bar performs an action and reads as a button.
pub(super) fn button_control_type(btn: Button) -> UIA_CONTROLTYPE_ID {
    match btn {
        Button::Tool(_) => UIA_RadioButtonControlTypeId,
        _ => UIA_ButtonControlTypeId,
    }
}

/// Undo and Redo genuinely do nothing with an empty stack (see `actions::handle_button`), and
/// saying so is the point of the property. A deleted shape waiting to be restored counts as work
/// for Undo even when no shapes are left. The bar does not grey them out, so this is the only
/// place that difference is visible, which is a small honesty gain for a screen reader user
/// rather than a change to what the bar does.
pub(super) fn button_enabled(s: &Shot, btn: Button) -> bool {
    match btn {
        Button::Undo => !s.shapes.is_empty() || super::super::input::has_pending_delete(),
        Button::Redo => !s.redo.is_empty(),
        _ => true,
    }
}

/// The name a screen reader should speak for a toolbar item.
pub(super) fn button_name(btn: Button) -> String {
    spoken_name(&toolbar::button_tip(btn)).to_string()
}

/// Reduce a toolbar tooltip to a spoken name.
///
/// The tips are written for a mouse user hovering a bare icon, so each one is three things
/// glued together: a name, a keyboard hint, and a sentence about how to use the tool. Read
/// aloud on every arrow press that is unbearable, so keep the head and drop the rest. The
/// shortcut is not being hidden from anyone, it is still on the tooltip and still in the
/// Settings help; it simply does not belong in the element's NAME, which is what a reader
/// repeats every single time focus lands.
pub(super) fn spoken_name(tip: &str) -> &str {
    // U+2014 EM DASH, written as an escape rather than the character itself.
    let head = tip.split('\u{2014}').next().unwrap_or(tip).trim();
    strip_key_hint(head)
}

/// Drop a trailing parenthetical, but only when it is a keyboard hint.
///
/// "Copy text (OCR) (Ctrl+T)" has two parentheticals and only the last one is a hint, so this
/// tests the content rather than assuming the last group is always droppable.
pub(super) fn strip_key_hint(s: &str) -> &str {
    let t = s.trim_end();
    let Some(rest) = t.strip_suffix(')') else {
        return t;
    };
    let Some(open) = rest.rfind('(') else {
        return t;
    };
    let inner = &rest[open + 1..];
    if looks_like_key_hint(inner) {
        rest[..open].trim_end()
    } else {
        t
    }
}

pub(super) fn looks_like_key_hint(inner: &str) -> bool {
    if inner.chars().count() == 1 {
        return true; // the single-letter tool shortcuts: (R), (O), (A), ...
    }
    [
        "Ctrl", "Alt", "Shift", "Esc", "Enter", "Del", "Tab", "Space",
    ]
    .iter()
    .any(|w| inner.contains(w))
}

/// The nearest basic colour name for `c`.
///
/// A swatch named "#E62828" is technically complete and useless out loud: six digits spelled
/// one at a time, with nothing to tell the red one from the green one. The palette carries no
/// names of its own (it is six RGB triples), so the name is DERIVED, which also means a custom
/// colour the user picked gets a sensible name for free.
pub(super) fn color_name(c: COLORREF) -> &'static str {
    const NAMED: [(&str, i32, i32, i32); 13] = [
        ("Black", 0, 0, 0),
        ("White", 255, 255, 255),
        ("Grey", 128, 128, 128),
        ("Red", 220, 30, 30),
        ("Orange", 245, 130, 30),
        ("Yellow", 240, 220, 40),
        ("Green", 40, 170, 60),
        ("Cyan", 40, 200, 220),
        ("Blue", 40, 100, 220),
        ("Purple", 130, 60, 200),
        ("Magenta", 220, 60, 180),
        ("Brown", 130, 80, 40),
        ("Pink", 245, 150, 180),
    ];
    let r = (c.0 & 0xff) as i32;
    let g = ((c.0 >> 8) & 0xff) as i32;
    let b = ((c.0 >> 16) & 0xff) as i32;
    let mut best = NAMED[0].0;
    let mut best_d = i32::MAX;
    for (name, nr, ng, nb) in NAMED {
        let d = (r - nr) * (r - nr) + (g - ng) * (g - ng) + (b - nb) * (b - nb);
        if d < best_d {
            best_d = d;
            best = name;
        }
    }
    best
}

pub(super) fn swatch_name(swatch: Swatch) -> String {
    match swatch {
        Swatch::Color(c) => color_name(c).to_string(),
        Swatch::Custom(Some(c)) => format!("Custom colour, {}", color_name(c)),
        Swatch::Custom(None) => "Empty custom colour slot".to_string(),
        Swatch::Picker => "More colours".to_string(),
    }
}

/// `face` is the font currently in force, so the field announces what it is set TO rather than
/// just what it is.
///
/// Localized (audit F29, 2026-09-06): pre-fix these were a SECOND, independent set of hardcoded
/// English strings that happened to describe the same controls `textflyout`'s own paint code
/// already localizes - a screen reader user got English regardless of the active language even
/// after the visible captions were fixed. `Bold`/`Underline` now go through the exact same
/// locale keys `checkbox_label` paints with, so the two can never drift apart again; the
/// remaining three have no on-screen caption of their own ("−"/"+" are language-neutral, and the
/// font dropdown toggle has no separate label), so they get their own keys.
pub(super) fn text_item_name(item: TextItem, face: &str) -> String {
    match item {
        TextItem::FontField => crate::win::t("shot_text_font_field").replace("{face}", face),
        TextItem::FontOption(i) => toolbar::PRESET_FONTS
            .get(i)
            .map_or_else(|| "Font".to_string(), |n| (*n).to_string()),
        TextItem::SizeDown => crate::win::t("shot_text_size_down").to_string(),
        TextItem::SizeUp => crate::win::t("shot_text_size_up").to_string(),
        TextItem::Bold => crate::win::t("shot_text_bold").to_string(),
        TextItem::Underline => crate::win::t("shot_text_underline").to_string(),
        TextItem::More => crate::win::t("shot_text_more_options").to_string(),
    }
}
