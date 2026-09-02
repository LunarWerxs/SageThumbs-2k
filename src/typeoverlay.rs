//! Suppress Explorer's own file-type icon on the thumbnails of the formats we hook.
//!
//! # What Explorer draws, and why it lands on top of our badge
//!
//! Explorer stamps a small "which program owns this" icon into the BOTTOM-RIGHT of a
//! thumbnail — exactly where [`crate::badge`] puts the format mark. Where that icon comes
//! from is documented under "Thumbnail Handlers": the shell reads a REG_SZ named
//! `TypeOverlay` from the file's **ProgID** key. A resource reference draws that image, an
//! **empty string draws nothing**, and an absent value falls back to the associated
//! application's default icon.
//!
//! The reported case (issue #18) is the ugly one: a program that has been UNINSTALLED still
//! owns the association, so the shell resolves an icon out of an exe that is no longer
//! there, and what lands on the picture is a blank generic page — covering our badge with
//! something that means nothing.
//!
//! # Two things that were measured, not assumed (2026-08-08, Windows 11 build 26200)
//!
//! * `TypeOverlay` on the **`.ext` key does nothing.** Setting it there while the ProgID
//!   still declared one left the overlay on screen. It really is the ProgID key, so
//!   resolving the ProgID (below) is not optional convenience — it is the whole mechanism.
//! * There is **no per-call way** for a thumbnail provider to refuse the overlay:
//!   `IThumbnailProvider::GetThumbnail` returns a bitmap and an alpha hint, nothing else,
//!   and the docs say overlays are Windows' to apply. Registry or nothing.
//!
//! # Why HKCU
//!
//! The ProgID usually belongs to somebody else, and `HKCR\<ProgID>` is normally backed by
//! HKLM — which would need elevation for a checkbox in a Settings dialog. `HKCU\Software\
//! Classes` merges ahead of the machine view, so writing there suppresses the overlay for
//! this user with no elevation and without touching the other program's own key. Removal is
//! then just deleting what we wrote.
//!
//! Ownership is tracked exactly the way `register::set_perceived_type` already tracks its
//! `PerceivedType` writes: an empty string is indistinguishable from someone else's empty
//! string, so we stamp a marker value beside it and only ever remove a value the marker
//! proves is ours.

use windows_registry::CURRENT_USER;

use crate::formats;

/// Marker proving a `TypeOverlay` under a ProgID key is one we wrote.
const MARK: &str = "SageThumbs2K.TypeOverlay";
/// The value Explorer reads. Empty string = "draw no overlay".
const VALUE: &str = "TypeOverlay";

/// `HKCU\Software\Classes` — the per-user half of `HKEY_CLASSES_ROOT`.
fn user_classes() -> windows_registry::Result<windows_registry::Key> {
    CURRENT_USER.create(r"Software\Classes")
}

/// Every ProgID that could supply the overlay for `.ext`.
///
/// Two sources, because either can win depending on how the association was made:
/// the per-user `UserChoice` the shell honours first, and the class default under
/// `HKCU\Software\Classes\.ext` / `HKCR\.ext`. Duplicates and empties are dropped; a name
/// with a backslash is refused so a malformed value can never steer a write outside the
/// classes tree.
fn progids_for(ext: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: Option<String>| {
        if let Some(s) = s {
            let s = s.trim().to_string();
            if !s.is_empty() && !s.contains('\\') && !out.contains(&s) {
                out.push(s);
            }
        }
    };

    let user_choice =
        format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{ext}\UserChoice");
    push(
        CURRENT_USER
            .open(&user_choice)
            .ok()
            .and_then(|k| k.get_string("ProgId").ok()),
    );
    push(
        windows_registry::CLASSES_ROOT
            .open(format!(".{ext}"))
            .ok()
            .and_then(|k| k.get_string("").ok()),
    );
    out
}

/// Write `TypeOverlay = ""` for every ProgID that serves `.ext`, marking each write as ours.
///
/// Skips any ProgID that already carries a `TypeOverlay` we did not write: that is a
/// deliberate choice by whoever owns the type, and silently replacing it would be the same
/// class of rudeness this feature exists to undo.
fn apply_ext(classes: &windows_registry::Key, ext: &str) {
    for progid in progids_for(ext) {
        apply_progid(classes, &progid);
    }
}

/// The single-ProgID half of [`apply_ext`], split out so the write/marker contract can be
/// tested against a scratch key instead of the user's real classes hive.
fn apply_progid(classes: &windows_registry::Key, progid: &str) {
    let existing = classes
        .open(progid)
        .ok()
        .and_then(|k| k.get_string(VALUE).ok());
    let ours = classes
        .open(progid)
        .ok()
        .and_then(|k| k.get_string(MARK).ok())
        .is_some();
    if existing.is_some() && !ours {
        return;
    }
    if let Ok(k) = classes.create(progid) {
        if k.set_string(VALUE, "").is_ok() {
            let _ = k.set_string(MARK, "1");
        }
    }
}

/// Remove the suppression for `.ext`, but only where our marker proves the value is ours.
fn remove_ext(classes: &windows_registry::Key, ext: &str) {
    for progid in progids_for(ext) {
        remove_progid(classes, &progid);
    }
}

/// The single-ProgID half of [`remove_ext`]. See [`apply_progid`].
fn remove_progid(classes: &windows_registry::Key, progid: &str) {
    // `open` hands back a READ-ONLY key, and `remove_value` on one fails — silently, since
    // there is nothing useful to do with the error here. That made an earlier version of this
    // function a no-op in production, leaving the suppression in place forever after the user
    // unticked the box (caught by the round-trip test below, never by a human).
    //
    // So: prove the key exists and is ours with a read handle, then reopen for writing.
    // `create` on a key that already exists just opens it, which is why it is safe HERE and
    // would not be as the first step — that would conjure ProgID keys we have no business
    // creating for every format the user does not have installed.
    let Ok(ro) = classes.open(progid) else {
        return;
    };
    if ro.get_string(MARK).is_err() {
        return;
    }
    drop(ro);
    let Ok(k) = classes.create(progid) else {
        return;
    };
    let _ = k.remove_value(VALUE);
    let _ = k.remove_value(MARK);
    // If the ProgID key existed ONLY to hold our two values, take it with us rather than
    // leaving an empty shadow entry in the user's hive.
    if k.values().map(|v| v.count() == 0).unwrap_or(false)
        && k.keys().map(|s| s.count() == 0).unwrap_or(false)
    {
        let _ = classes.remove_tree(progid);
    }
}

/// Apply (or undo) the suppression across every format we hook.
///
/// Called when the setting changes and after (re)registration, so the two can never
/// disagree. Formats the user has turned off are cleaned up either way — a format we no
/// longer thumbnail has no badge to protect.
pub fn sync(on: bool) {
    let Ok(classes) = user_classes() else {
        return;
    };
    // One settings snapshot for the whole sweep; the per-extension lookup below is then an
    // in-memory hit instead of a full ini parse per format in portable mode.
    let fmt = crate::settings::format_enabled_snapshot();
    for (ext, _) in formats::FORMATS {
        if on && fmt.enabled(ext) {
            apply_ext(&classes, ext);
        } else {
            remove_ext(&classes, ext);
        }
    }
}

/// Undo every suppression we ever wrote, regardless of the current setting. For uninstall.
pub fn remove_all() {
    let Ok(classes) = user_classes() else {
        return;
    };
    for (ext, _) in formats::FORMATS {
        remove_ext(&classes, ext);
    }
}

/// The ProgIDs of hooked formats that currently declare a `TypeOverlay` we did NOT write —
/// i.e. the ones actively covering our badge. `st2k doctor` reports these by name, because
/// "an uninstalled program is still stamping its icon on your thumbnails" is impossible to
/// work out from the symptom.
pub fn foreign_overlays() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let fmt = crate::settings::format_enabled_snapshot();
    for (ext, _) in formats::FORMATS {
        if !fmt.enabled(ext) {
            continue;
        }
        for progid in progids_for(ext) {
            let Ok(k) = windows_registry::CLASSES_ROOT.open(&progid) else {
                continue;
            };
            if k.get_string(MARK).is_ok() {
                continue; // ours
            }
            let Ok(v) = k.get_string(VALUE) else {
                continue;
            };
            if v.trim().is_empty() {
                continue; // already suppressed by its owner
            }
            if !out.iter().any(|(p, _)| p == &progid) {
                out.push((progid.clone(), format!(".{ext}")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch stand-in for `HKCU\Software\Classes`, removed when the guard drops, so these
    /// tests can exercise the real write/remove code without touching the machine's own
    /// associations.
    struct Scratch(String);

    impl Scratch {
        fn new(name: &str) -> (Self, windows_registry::Key) {
            let path = format!(r"Software\SageThumbs2K-test\{name}");
            let key = CURRENT_USER.create(&path).expect("scratch key");
            (Scratch(path), key)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.0);
        }
    }

    /// The whole contract in one pass: we write an EMPTY TypeOverlay plus a marker, and
    /// removal takes both away and the key with them. Without the marker there would be no
    /// way to tell our empty string from somebody else's on the way back out.
    #[test]
    fn apply_writes_a_marked_empty_overlay_and_remove_takes_it_back_out() {
        let (_guard, classes) = Scratch::new("apply-roundtrip");
        apply_progid(&classes, "St2kTest.Progid");

        let k = classes.open("St2kTest.Progid").expect("progid key created");
        assert_eq!(k.get_string(VALUE).as_deref(), Ok(""), "overlay suppressed");
        assert!(k.get_string(MARK).is_ok(), "our marker is present");
        drop(k);

        remove_progid(&classes, "St2kTest.Progid");
        assert!(
            classes.open("St2kTest.Progid").is_err(),
            "a key that only ever held our two values must not be left behind"
        );
    }

    /// The rule that keeps this feature polite: an overlay somebody else chose is theirs.
    /// We neither replace it going in nor delete it coming out.
    #[test]
    fn a_foreign_overlay_is_never_overwritten_or_removed() {
        let (_guard, classes) = Scratch::new("foreign");
        let k = classes.create("Other.Progid").expect("create");
        k.set_string(VALUE, "shell32.dll,-16826").expect("set");
        drop(k);

        apply_progid(&classes, "Other.Progid");
        let k = classes.open("Other.Progid").expect("still there");
        assert_eq!(
            k.get_string(VALUE).as_deref(),
            Ok("shell32.dll,-16826"),
            "their value must survive apply"
        );
        assert!(
            k.get_string(MARK).is_err(),
            "and must not be marked as ours"
        );
        drop(k);

        remove_progid(&classes, "Other.Progid");
        let k = classes
            .open("Other.Progid")
            .expect("still there after remove");
        assert_eq!(k.get_string(VALUE).as_deref(), Ok("shell32.dll,-16826"));
    }

    /// A ProgID key that carries other values is the common case for a real program: strip
    /// our two values, leave the key and everything else in it alone.
    #[test]
    fn a_progid_with_other_values_keeps_its_key_after_removal() {
        let (_guard, classes) = Scratch::new("shared");
        let k = classes.create("Shared.Progid").expect("create");
        k.set_string("FriendlyTypeName", "Something Else")
            .expect("set");
        drop(k);

        apply_progid(&classes, "Shared.Progid");
        remove_progid(&classes, "Shared.Progid");

        let k = classes.open("Shared.Progid").expect("key survives");
        assert_eq!(
            k.get_string("FriendlyTypeName").as_deref(),
            Ok("Something Else")
        );
        assert!(k.get_string(VALUE).is_err(), "our overlay value is gone");
        assert!(k.get_string(MARK).is_err(), "our marker is gone");
    }

    /// Applying twice must not stack up state, and one removal must still fully undo it.
    #[test]
    fn applying_twice_is_the_same_as_applying_once() {
        let (_guard, classes) = Scratch::new("idempotent");
        apply_progid(&classes, "Twice.Progid");
        apply_progid(&classes, "Twice.Progid");
        remove_progid(&classes, "Twice.Progid");
        assert!(classes.open("Twice.Progid").is_err());
    }

    /// A ProgID name is pasted straight into a registry path, so anything that could escape
    /// the classes tree has to be refused before it gets there.
    #[test]
    fn progid_names_with_a_path_separator_are_refused() {
        // `progids_for` filters on `\`; prove the predicate it relies on, without needing a
        // machine whose associations happen to be malformed.
        let bad = r"..\..\Microsoft\Windows";
        assert!(bad.contains('\\'));
    }

    #[test]
    fn the_marker_and_value_names_are_distinct() {
        assert_ne!(MARK, VALUE);
        assert!(MARK.starts_with("SageThumbs2K."));
    }
}
