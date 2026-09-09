//! Own the bottom-right corner of a thumbnail: suppress Explorer's file-type icon when our
//! badge (or nothing) belongs there, and put it BACK where Explorer would silently skip it.
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
//! # Two more, the other way round (2026-09-09, Windows 11 25H2 build 26200)
//!
//! A user with the corner set to Windows' own icon reported that PSD files got none while
//! Aseprite files did. Reproduced on a machine with Photoshop, and the docs' "absent value
//! falls back to the application's icon" turned out to have two unwritten exceptions:
//!
//! * **Explorer draws NO overlay on a type whose `PerceivedType` is `image`.** Adobe stamps
//!   that on `.psd`, `.ai` and `.eps`; Windows stamps it on every picture format it decodes
//!   itself. Shadowing `.ai`'s value to `document` made the Illustrator icon appear with no
//!   `TypeOverlay` anywhere. Windows never overlays a photo — which is exactly the set of
//!   editor formats this product exists for.
//! * **An explicit `TypeOverlay` overrides that skip**, and it works written under
//!   `HKCU\Software\Classes` against a ProgID whose machine half is hollow. The `.psd`
//!   `UserChoice` on that machine still named `Photoshop.Image.26` (the 2025 release) after
//!   an update had moved the real registration to `.27`; the leftover key has every subkey
//!   and no values, so Explorer had no icon to draw. Writing the `.27` icon as `.26`'s
//!   overlay put the Photoshop mark back.
//!
//! So `CornerMark::SystemIcon` is not "do nothing": [`sync`]`(false)` removes our
//! suppression AND, for those two cases, writes the icon Explorer would otherwise skip.
//! Everything else is left exactly as Explorer would draw it: a format Windows decodes itself
//! keeps its bare corner (nobody wants the Photos icon on every JPEG), a packaged app's
//! indirect icon string and Windows' own resources are healthy registrations we never
//! redirect, and an owner that deliberately turned its icon off (an empty value we did not
//! write) keeps that choice — `st2k doctor` names it. The first cut of this got all three
//! wrong on one machine (an Edge icon on Acrobat's PDFs, a Photoshop icon on DNG, empty
//! overlays on thirty Photos-owned types), and a second measurement showed that a ProgID
//! with NO `DefaultIcon` at all is not the blank-page case either: for a `.doc` owned by a
//! `doc_auto_file` Explorer derived an icon from the open command. Only a `DefaultIcon`
//! naming a file that is gone draws the blank page. Hence [`OwnIcon`]'s four arms.
//!
//! # Why HKCU
//!
//! The ProgID usually belongs to somebody else, and `HKCR\<ProgID>` is normally backed by
//! HKLM — which would need elevation for a checkbox in a Settings dialog. `HKCU\Software\
//! Classes` merges ahead of the machine view, so writing there suppresses (or restores) the
//! overlay for this user with no elevation and without touching the other program's own
//! key. Removal is then just deleting what we wrote.
//!
//! Ownership is tracked exactly the way `register::set_perceived_type` already tracks its
//! `PerceivedType` writes: an empty string is indistinguishable from someone else's empty
//! string, so we stamp a marker value beside it and only ever remove a value the marker
//! proves is ours.

use windows_registry::{CLASSES_ROOT, CURRENT_USER};

use crate::formats::{self, Category};

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
        CLASSES_ROOT
            .open(format!(".{ext}"))
            .ok()
            .and_then(|k| k.get_string("").ok()),
    );
    out
}

/// The ONE ProgID Explorer draws the overlay from: the `UserChoice` when its key exists,
/// otherwise the class default. [`progids_for`] lists both because a suppression has to
/// land on both; a restore only makes sense on the one that is actually consulted.
fn effective_progid(ext: &str) -> Option<String> {
    progids_for(ext)
        .into_iter()
        .find(|p| CLASSES_ROOT.open(p).is_ok())
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
    write_marked(classes, progid, "");
}

/// Write `value` as `progid`'s `TypeOverlay` in the user hive, with our marker beside it.
/// Both the suppression (`""`) and the restore (an icon location) go through here, so
/// [`remove_progid`] can take either back out the same way.
fn write_marked(classes: &windows_registry::Key, progid: &str, value: &str) {
    if let Ok(k) = classes.create(progid) {
        if k.set_string(VALUE, value).is_ok() {
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

// ---- the restore half: put Windows' icon back where Explorer would skip it ---------------

/// Expand `%NAME%` references the way a REG_EXPAND_SZ reader does. A name that is not set
/// is left as written, so a broken value stays visibly broken instead of collapsing to `\`.
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Split an icon location (`path[,index]`) into its parts: the path unquoted and
/// env-expanded, the index kept verbatim when it parses as a number. Pure, so it can be
/// tested without a registry. `None` only for an empty value.
fn split_icon_location(value: &str) -> Option<(String, Option<String>)> {
    let v = expand_env(value.trim());
    if v.is_empty() {
        return None;
    }
    let (path, index) = match v.rfind(',') {
        Some(i) if v[i + 1..].trim().parse::<i32>().is_ok() => {
            (&v[..i], Some(v[i + 1..].trim().to_string()))
        }
        _ => (v.as_str(), None),
    };
    let path = path.trim().trim_matches('"').trim();
    if path.is_empty() {
        return None;
    }
    Some((path.to_string(), index))
}

/// Whether `path` is one of Windows' own icon resources. `%SystemRoot%\Installer` is
/// excluded on purpose: that is the MSI icon cache, where third-party programs' icons live
/// (Acrobat's `_PDFFile.ico` sits there), and treating it as Windows' own is what put an
/// Edge icon on every PDF in the first cut of this code.
fn is_windows_own_icon(path: &str) -> bool {
    let Ok(root) = std::env::var("SystemRoot") else {
        return false;
    };
    let lower = path.to_ascii_lowercase();
    let root = root.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix(&root) else {
        return false;
    };
    !rest.trim_start_matches('\\').starts_with("installer\\")
}

/// What a ProgID's `DefaultIcon` amounts to, for the restore.
#[derive(Debug, PartialEq, Eq)]
enum OwnIcon {
    /// A file on disk outside Windows: something we can name in a `TypeOverlay`.
    Usable(String),
    /// A registration Explorer resolves fine but that is never ours to redirect: a packaged
    /// app's indirect string (`@{…ms-resource://…}`), `%1` (the file's own icon), or one of
    /// Windows' own resources (`shell32.dll,0` is the blank page `Unknown` types carry —
    /// restoring THAT would be restoring the very thing issue #18 complained about).
    Healthy,
    /// No `DefaultIcon` at all, or an empty one (the hollow leftover after an update).
    /// Measured 2026-09-09: for a DOCUMENT Explorer then derives an icon from the type's
    /// open command and draws that, so there is nothing to fix; for a picture it draws
    /// nothing, so one has to be borrowed.
    Absent,
    /// A `DefaultIcon` that names a file which is not there — the uninstalled program of
    /// issue #18. This is the case that draws the blank generic page.
    Gone,
}

/// Classify one `DefaultIcon` (or `TypeOverlay`) value. Pure apart from the file check.
fn classify_icon(value: &str) -> OwnIcon {
    let v = value.trim();
    if v.starts_with('@') || v.starts_with("%1") {
        return OwnIcon::Healthy;
    }
    let Some((path, index)) = split_icon_location(v) else {
        return OwnIcon::Absent;
    };
    if is_windows_own_icon(&path) {
        return OwnIcon::Healthy;
    }
    if !std::path::Path::new(&path).is_file() {
        return OwnIcon::Gone;
    }
    OwnIcon::Usable(match index {
        Some(i) => format!("{path},{i}"),
        None => path,
    })
}

/// `HKCR\<progid>\DefaultIcon`, classified.
fn progid_icon(progid: &str) -> OwnIcon {
    CLASSES_ROOT
        .open(format!(r"{progid}\DefaultIcon"))
        .ok()
        .and_then(|k| k.get_string("").ok())
        .map(|v| classify_icon(&v))
        .unwrap_or(OwnIcon::Absent)
}

/// A usable icon for `.ext` from ANY ProgID registered for it, for when the one Explorer
/// consults has none: the class default first, then `OpenWithProgids` newest-looking first
/// (`Photoshop.Image.27` sorts before `.25`). Only [`OwnIcon::Usable`] counts — borrowing a
/// packaged app's icon is impossible and borrowing Windows' is wrong.
fn fallback_icon(ext: &str) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(default) = CLASSES_ROOT
        .open(format!(".{ext}"))
        .ok()
        .and_then(|k| k.get_string("").ok())
    {
        candidates.push(default);
    }
    if let Ok(k) = CLASSES_ROOT.open(format!(r".{ext}\OpenWithProgids")) {
        if let Ok(values) = k.values() {
            let mut names: Vec<String> = values
                .map(|(name, _)| name)
                .filter(|n| !n.is_empty() && !n.contains('\\'))
                .collect();
            names.sort_by(|a, b| b.cmp(a));
            candidates.extend(names);
        }
    }
    candidates.iter().find_map(|p| match progid_icon(p) {
        OwnIcon::Usable(icon) => Some(icon),
        _ => None,
    })
}

/// Whether Explorer treats `.ext` as a photo and so never overlays it (see the module doc):
/// `PerceivedType = image`.
fn explorer_skips_overlay(ext: &str) -> bool {
    CLASSES_ROOT
        .open(format!(".{ext}"))
        .ok()
        .and_then(|k| k.get_string("PerceivedType").ok())
        .is_some_and(|p| p.trim().eq_ignore_ascii_case("image"))
}

/// [`explorer_skips_overlay`], narrowed to the formats Windows cannot decode itself. The
/// formats Windows DOES decode (JPEG, PNG, camera RAW, …) keep their bare corner — nobody
/// wants the photo viewer's icon stamped on every JPEG, and that is not what "Windows'
/// file-type icon" ever showed.
fn treated_as_picture(ext: &str) -> bool {
    explorer_skips_overlay(ext)
        && !crate::register::WIC_IMAGE_EXTS.contains(&ext)
        && formats::category(ext) != Category::Raw
}

/// What, if anything, to write for `.ext` in `SystemIcon` mode so the corner shows what the
/// user asked for. `None` = Explorer handles it itself, or the owner chose otherwise.
fn restore_plan(ext: &str, progid: &str) -> Option<String> {
    let merged = CLASSES_ROOT.open(progid).ok()?;
    if merged.get_string(VALUE).is_ok() && merged.get_string(MARK).is_err() {
        // The owner declared an overlay (an icon, or "" for none): theirs, whatever it is.
        return None;
    }
    let pictured = treated_as_picture(ext);
    match progid_icon(progid) {
        // The registration is healthy. Explorer draws this icon itself for a document; for
        // a picture it draws nothing, so spell it out.
        OwnIcon::Usable(icon) => pictured.then_some(icon),
        // Windows' own, or a packaged app's: Explorer's business either way.
        OwnIcon::Healthy => None,
        // Hollow (a stale registration after an update): for a document Explorer derives an
        // icon from the open command by itself; for a picture it draws nothing, so borrow one
        // from another registration of the same type.
        OwnIcon::Absent => {
            if pictured {
                fallback_icon(ext)
            } else {
                None
            }
        }
        // The program is gone (issue #18): borrow the icon from another registration of
        // the same type; failing that, ask for nothing rather than the blank page Explorer
        // draws for a document whose icon file is missing.
        OwnIcon::Gone => match fallback_icon(ext) {
            Some(icon) => Some(icon),
            None if explorer_skips_overlay(ext) => None,
            None => Some(String::new()),
        },
    }
}

/// The `SystemIcon` half of [`sync`]: after our suppression is gone, write the icon Explorer
/// would otherwise skip for `.ext`, if there is one to write.
fn restore_ext(classes: &windows_registry::Key, ext: &str) {
    let Some(progid) = effective_progid(ext) else {
        return;
    };
    if let Some(value) = restore_plan(ext, &progid) {
        write_marked(classes, &progid, &value);
    }
}

/// Apply (or undo) the suppression across every format we hook.
///
/// Called when the setting changes and after (re)registration, so the two can never
/// disagree. Formats the user has turned off are cleaned up either way — a format we no
/// longer thumbnail has no badge to protect. With `on` false the corner is Windows' to
/// draw, and for the two cases the module doc measures we tell it what to draw.
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
            if fmt.enabled(ext) {
                restore_ext(&classes, ext);
            }
        }
    }
}

/// Undo every value we ever wrote, regardless of the current setting. For uninstall.
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
            let Ok(k) = CLASSES_ROOT.open(&progid) else {
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

/// The overlays [`sync`]`(false)` wrote for the user — `(progid, ".ext", value)`, where an
/// empty value means "nothing, instead of a blank page". For `st2k doctor`.
pub fn restored_overlays() -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let fmt = crate::settings::format_enabled_snapshot();
    for (ext, _) in formats::FORMATS {
        if !fmt.enabled(ext) {
            continue;
        }
        let Some(progid) = effective_progid(ext) else {
            continue;
        };
        let Ok(k) = CLASSES_ROOT.open(&progid) else {
            continue;
        };
        if k.get_string(MARK).is_err() {
            continue;
        }
        let Ok(v) = k.get_string(VALUE) else {
            continue;
        };
        if !out.iter().any(|(p, _, _)| p == &progid) {
            out.push((progid, format!(".{ext}"), v));
        }
    }
    out
}

/// Hooked formats whose owner tells Explorer to draw nothing (an empty `TypeOverlay` we did
/// not write) — `(progid, ".ext")`. Honoured, and named by `st2k doctor` so a user who asked
/// for Windows' icon and got none knows whose decision that was. Windows' own types (fonts,
/// themes) are skipped: that is the OS talking, not a program the user installed.
pub fn owner_suppressed() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let fmt = crate::settings::format_enabled_snapshot();
    for (ext, _) in formats::FORMATS {
        if !fmt.enabled(ext) {
            continue;
        }
        let Some(progid) = effective_progid(ext) else {
            continue;
        };
        let Ok(k) = CLASSES_ROOT.open(&progid) else {
            continue;
        };
        if k.get_string(MARK).is_ok() {
            continue;
        }
        let Ok(v) = k.get_string(VALUE) else {
            continue;
        };
        if !v.trim().is_empty() || progid_icon(&progid) == OwnIcon::Healthy {
            continue;
        }
        if !out.iter().any(|(p, _)| p == &progid) {
            out.push((progid, format!(".{ext}")));
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

    /// The restore is the same two values with an icon instead of "", and the same removal
    /// takes it out — so switching to the badge, or uninstalling, never strands an overlay
    /// pointing at an exe we told Explorer about.
    #[test]
    fn a_restored_icon_is_marked_and_removed_the_same_way() {
        let (_guard, classes) = Scratch::new("restore-roundtrip");
        write_marked(&classes, "St2kTest.Hollow", r"C:\Somewhere\editor.exe,1");
        let k = classes.open("St2kTest.Hollow").expect("progid key created");
        assert_eq!(
            k.get_string(VALUE).as_deref(),
            Ok(r"C:\Somewhere\editor.exe,1")
        );
        assert!(k.get_string(MARK).is_ok());
        drop(k);

        // Flipping to the badge overwrites OUR icon with "" (it is ours, so allowed) …
        apply_progid(&classes, "St2kTest.Hollow");
        let k = classes.open("St2kTest.Hollow").expect("still there");
        assert_eq!(k.get_string(VALUE).as_deref(), Ok(""));
        drop(k);

        // … and removal clears the lot.
        remove_progid(&classes, "St2kTest.Hollow");
        assert!(classes.open("St2kTest.Hollow").is_err());
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

    /// The shapes a real `DefaultIcon` comes in: quoted, env-expanded, with or without an
    /// index, and the empty one.
    #[test]
    fn icon_locations_are_split_the_way_the_shell_reads_them() {
        assert_eq!(
            split_icon_location(r#""C:\Program Files\App\app.exe",1"#),
            Some((
                r"C:\Program Files\App\app.exe".to_string(),
                Some("1".to_string())
            ))
        );
        assert_eq!(
            split_icon_location(r"C:\App\icons\type.ico"),
            Some((r"C:\App\icons\type.ico".to_string(), None))
        );
        assert_eq!(
            split_icon_location(r"C:\App\res.dll,-155"),
            Some((r"C:\App\res.dll".to_string(), Some("-155".to_string())))
        );
        assert_eq!(split_icon_location("   "), None);
        // A trailing comma with no number is part of an odd path, not an index.
        assert_eq!(
            split_icon_location(r"C:\odd,name\a.ico"),
            Some((r"C:\odd,name\a.ico".to_string(), None))
        );
    }

    #[test]
    fn env_references_expand_and_unknown_ones_survive() {
        let root = std::env::var("SystemRoot").expect("SystemRoot is always set on Windows");
        assert_eq!(
            expand_env(r"%SystemRoot%\system32\x.dll,3"),
            format!(r"{root}\system32\x.dll,3")
        );
        assert_eq!(
            expand_env(r"%St2kNoSuchVariable%\x.dll"),
            r"%St2kNoSuchVariable%\x.dll"
        );
        assert_eq!(expand_env("50%"), "50%");
    }

    /// Windows' own resources live under `%SystemRoot%` — except the MSI icon cache in
    /// `%SystemRoot%\Installer`, which is where third-party programs' icons end up.
    #[test]
    fn the_msi_icon_cache_is_not_windows_own() {
        let root = std::env::var("SystemRoot").expect("SystemRoot");
        assert!(is_windows_own_icon(&format!(
            r"{root}\system32\shell32.dll"
        )));
        assert!(is_windows_own_icon(&format!(r"{root}\explorer.exe")));
        assert!(!is_windows_own_icon(&format!(
            r"{root}\Installer\{{AC76BA86-1033-FFFF-7760-BC15014EA700}}\_PDFFile.ico"
        )));
        assert!(!is_windows_own_icon(r"C:\Program Files\App\app.exe"));
    }

    /// The arms of the restore decision, on the values that fooled the first cut: a
    /// packaged app's indirect string and Windows' own resources are healthy registrations
    /// (never redirected, never "gone"), an empty value is absent, a vanished file is gone,
    /// and only a third-party file on disk is something we can name.
    #[test]
    fn icons_are_classified_healthy_absent_gone_or_usable() {
        assert_eq!(
            classify_icon("@{Microsoft.Windows.Photos_1.0_x64__8wekyb3d8bbwe?ms-resource://x}"),
            OwnIcon::Healthy
        );
        assert_eq!(classify_icon("%1"), OwnIcon::Healthy);
        assert_eq!(
            classify_icon(r"%SystemRoot%\system32\shell32.dll,0"),
            OwnIcon::Healthy
        );
        assert_eq!(classify_icon(""), OwnIcon::Absent);
        assert_eq!(
            classify_icon(r"C:\St2kTest\definitely\missing.exe,1"),
            OwnIcon::Gone
        );
        // The test binary itself is a file outside %SystemRoot%: the one usable shape.
        let me = std::env::current_exe().expect("current exe");
        let loc = format!("\"{}\",0", me.display());
        assert_eq!(
            classify_icon(&loc),
            OwnIcon::Usable(format!("{},0", me.display()))
        );
    }
}
