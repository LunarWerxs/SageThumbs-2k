//! Per-user (HKCU) registration for the portable build: no elevation, no installer, an absolute DLL path.

use super::*;

/// `HKCU\Software\Classes` — the per-user half of `HKEY_CLASSES_ROOT`.
pub(super) fn user_classes() -> Result<Key> {
    CURRENT_USER.create(r"Software\Classes")
}

/// Register the thumbnail provider + classic context menu for THIS USER ONLY, pointing at
/// `dll_path`. No elevation, and it never touches the machine-wide hive, so it cannot
/// disturb an installed copy.
pub fn register_user(dll_path: &str) -> Result<()> {
    let classes = user_classes()?;
    for (clsid, name) in [
        (CLSID_THUMBNAIL_PROVIDER_STR, NAME),
        (CLSID_CONTEXT_MENU_STR, CM_NAME),
    ] {
        write_inproc_server(&classes, clsid, name, dll_path)?;
    }

    // Same per-extension layout as the machine-wide path, so precedence behaves identically.
    // Best-effort per extension: one locked-down key must not abort the rest, but every
    // outcome is counted and a pass that wrote nothing fails the call (see `register`).
    let fmt = settings::format_enabled_snapshot();
    let mut thumbs = Pass::default();
    for (ext, _) in FORMATS {
        if fmt.enabled(ext) {
            for path in thumb_keys(ext) {
                // Same non-destructive claim as the machine-wide path: note whoever held this
                // slot in the user's own hive so `remove_user_if_ours` can hand it straight
                // back. Portable mode is still a real install from the shell's point of view.
                remember_displaced_in(&classes, CURRENT_USER, &path);
                thumbs.note(
                    &path,
                    set_shellex_key(&classes, &path, CLSID_THUMBNAIL_PROVIDER_STR),
                );
            }
        } else {
            remove_user_if_ours(&classes, ext);
        }
    }
    let thumbs_failed = thumbs.report("per-user thumbnail shellex pass");
    // Sweep stale hooks from extensions older builds registered but we've since dropped —
    // mirrors register()/unregister()/unregister_user(), all three of which already do this.
    // Without it, a portable copy upgraded past a dropped extension keeps a stale HKCU
    // shellex entry for it forever (the FORMATS loop above never touches it again).
    for ext in REMOVED_EXTENSIONS {
        remove_user_if_ours(&classes, ext);
    }

    if let Err(e) = set_shellex_key(&classes, CONTEXT_MENU_KEY, CLSID_CONTEXT_MENU_STR) {
        log_error(&format!(
            "register_user: context menu key {CONTEXT_MENU_KEY}: hr={:#010x}",
            e.code().0
        ));
    }

    notify_shell();
    if thumbs_failed {
        return Err(Error::from(E_FAIL));
    }
    Ok(())
}

/// Undo [`register_user`]. Removes only keys whose value is OUR CLSID, so a handler another
/// product owns is never collateral damage, and leaves the machine-wide hive alone. The
/// per-user shell pieces go first, mirroring [`unregister`]: after the documented "run
/// `--off`, then delete the folder", a leftover folder verb would point at an EXE that is
/// gone and a leftover `TypeOverlay` would keep suppressing another program's icon.
pub fn unregister_user() -> Result<()> {
    remove_user_shell();
    unregister_user_classes()
}

/// The class-key half of [`unregister_user`]: the per-user CLSIDs and `shellex` hooks, plus the
/// per-user `DisplacedThumbHandlers` record tree under `SOFTWARE\SageThumbs2K`, not the user's
/// shell pieces. Also what the machine-wide [`register`] runs to clear a portable registration
/// that would shadow it, where taking the folder verb and overlay suppression away would be wrong.
pub(super) fn unregister_user_classes() -> Result<()> {
    let classes = user_classes()?;
    for (ext, _) in FORMATS {
        remove_user_if_ours(&classes, ext);
    }
    for ext in REMOVED_EXTENSIONS {
        remove_user_if_ours(&classes, ext);
    }
    if let Ok(k) = classes.open(CONTEXT_MENU_KEY) {
        if k.get_string("").ok().as_deref() == Some(CLSID_CONTEXT_MENU_STR) {
            let _ = classes.remove_tree(CONTEXT_MENU_KEY);
        }
    }
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}"));
    let _ = classes.remove_tree(format!("CLSID\\{CLSID_CONTEXT_MENU_STR}"));
    // The same final sweep the machine-wide `unregister` does, and for the same reason: a slot
    // a THIRD product has since taken over from us is no longer "ours", so `remove_user_if_ours`
    // skips it and its record is never restored or removed. Without this the portable path
    // leaves records behind that nothing would ever clean up again.
    let _ = CURRENT_USER.remove_tree(DISPLACED);
    notify_shell();
    Ok(())
}

/// Drop one extension's per-user thumbnail hooks, ours only, and prune the containers we
/// created on the way in. Without the prune, turning the feature off leaves an empty
/// `.<ext>\shellex` behind for every one of the 300+ formats — litter in the user's own hive
/// that looks like a half-removed handler to anyone who goes looking.
pub(super) fn remove_user_if_ours(classes: &Key, ext: &str) {
    for path in thumb_keys(ext) {
        let ours = classes
            .open(&path)
            .ok()
            .and_then(|k| k.get_string("").ok())
            .as_deref()
            == Some(CLSID_THUMBNAIL_PROVIDER_STR);
        if !ours {
            continue;
        }
        let _ = classes.remove_tree(&path);
        // Hand the slot back to whoever we took it from. This also leaves the key non-empty,
        // which is what stops the prune below from deleting the chain out from under it.
        restore_displaced_in(classes, CURRENT_USER, &path);
        // Walk back up: `<assoc>\shellex`, then `<assoc>`. Stop at the first parent that
        // still holds something, so a foreign handler or a populated key is never collateral.
        let Some(shellex) = path.rsplit_once('\\').map(|(parent, _)| parent) else {
            continue;
        };
        if !is_empty_key(classes, shellex) {
            continue;
        }
        let _ = classes.remove_tree(shellex);
        if let Some(assoc) = shellex.rsplit_once('\\').map(|(parent, _)| parent) {
            if is_empty_key(classes, assoc) {
                let _ = classes.remove_tree(assoc);
            }
        }
    }
}

/// The DLL path currently registered for THIS USER, if any.
///
/// Returns the path rather than a bool because the portable build has to answer a question a
/// bool cannot: whether the registration points at *this* copy. A user who unzips a second
/// copy, or moves the folder, leaves keys aimed at a DLL that is no longer there, and the
/// symptom is thumbnails silently not appearing.
pub fn user_registration_path() -> Option<String> {
    user_classes()
        .ok()?
        .open(format!(
            "CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}\\InprocServer32"
        ))
        .ok()?
        .get_string("")
        .ok()
        .filter(|p| !p.is_empty())
}

/// The shell-extension DLL a portable copy registers: the one sitting beside the running exe.
///
/// The caller still has to check it EXISTS. A partially-unpacked (or pruned) zip is exactly the
/// case worth naming in an error message rather than reporting as a generic failure.
pub fn dll_beside_exe() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("sagethumbs2k.dll"))
}

/// Is the per-user registration pointing at THIS copy's DLL?
///
/// Compares the PATH, not mere presence: a registration left behind by a copy that has since
/// been moved or deleted reads as "on" while drawing no thumbnails at all, which is the single
/// most confusing state a portable user can land in.
pub fn user_registration_is_here() -> bool {
    let (Some(registered), Some(here)) = (user_registration_path(), dll_beside_exe()) else {
        return false;
    };
    // The keys can outlive the file they name — antivirus quarantine, a half-deleted unzip, a
    // manual cleanup that left the exes. The path still matches in that case, so a pure string
    // compare would report "on" for a handler Windows cannot load and no thumbnail will ever
    // come from. Requiring the DLL to actually BE there makes the answer mean what it says.
    if !here.is_file() {
        return false;
    }
    // Case-insensitive: the registry keeps whatever case was written and Windows paths are not
    // case-sensitive, so a pure case difference is the same file.
    registered.eq_ignore_ascii_case(&here.to_string_lossy())
}
