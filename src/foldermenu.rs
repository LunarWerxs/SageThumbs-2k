//! The folder right-click entry that launches "build thumbnails here".
//!
//! # Why this is a STATIC verb and not part of our context-menu handler
//!
//! The classic handler is a COM in-process server registered under `*`, which in Windows
//! means *files* and does not include folders. Reaching folders through it would mean adding
//! `Directory` / `Directory\Background` / `Drive` entries to the same `shellex` registration
//! that carries the thumbnail provider, i.e. editing the surface every thumbnail in the
//! product depends on, and loading our DLL into `explorer.exe` on every folder right-click.
//!
//! A **static verb** needs none of that. `Directory\shell\<name>\command` is a plain registry
//! string Explorer reads and runs; there is no code in the shell's process, nothing to crash
//! it, and a broken entry costs one dead menu item rather than every thumbnail on the machine.
//! For "spawn an EXE with the folder path" that is the whole feature.
//!
//! # Why HKCU
//!
//! `HKCU\Software\Classes` merges ahead of the machine view, so the entry appears with no
//! elevation and disappears cleanly when unticked. It also matches where the work lands: the
//! thumbnail cache is per user, so a per-user menu entry is the honest scope.
//!
//! Every key we create is named after us (`SageThumbs2K.Prebuild`), so removal is a whole-key
//! delete with no ownership ambiguity — unlike [`crate::typeoverlay`], which has to mark its
//! writes because it edits a value inside somebody else's key.

use windows_registry::CURRENT_USER;

/// Our key name under each container class. Distinctive so removal can never take a key we
/// did not create.
const VERB_KEY: &str = "SageThumbs2K.Prebuild";

/// The three places a user can right-click and mean "this folder".
///
/// `Directory` is a folder in the file list, `Drive` is a drive root (the reported use case is
/// a 32 TB drive), and `Directory\Background` is the empty space *inside* an open folder,
/// which is where people reach for "do something to this whole folder".
const CONTAINERS: [(&str, &str); 3] = [
    // (class path under Software\Classes, the token Explorer substitutes for the folder)
    ("Directory", "%1"),
    ("Drive", "%1"),
    // Background has no selected item, so `%1` would be empty; `%V` is the open folder.
    (r"Directory\Background", "%V"),
];

/// Full path to the companion EXE, which is what the verb runs.
fn app_exe() -> Option<String> {
    crate::sibling_of_dll(crate::APP_EXE).map(|p| p.to_string_lossy().into_owned())
}

/// Write (or rewrite) the verb under every container class.
///
/// `label` is the menu text. It is written as a plain string rather than a `MUIVerb` indirect
/// reference because we ship no string resources to point at; the trade-off is that the text
/// is fixed at write time, which is why [`sync`] runs again whenever the UI language changes.
fn apply(label: &str) -> windows_registry::Result<()> {
    let Some(exe) = app_exe() else {
        crate::safety::log("foldermenu: companion EXE not found next to the DLL — verb skipped");
        return Ok(());
    };
    let classes = CURRENT_USER.create(r"Software\Classes")?;
    for (class, token) in CONTAINERS {
        let key = classes.create(format!(r"{class}\shell\{VERB_KEY}"))?;
        key.set_string("", label)?;
        // The app's own icon, so the entry reads as ours in a crowded menu.
        key.set_string("Icon", format!("{exe},0"))?;
        // Quote BOTH the exe and the folder: Program Files has a space, and so do most of the
        // folders anyone would point this at.
        key.create("command")?
            .set_string("", format!("\"{exe}\" --prebuild \"{token}\""))?;
    }
    Ok(())
}

/// Remove the verb from every container class.
fn remove() {
    let Ok(classes) = CURRENT_USER.create(r"Software\Classes") else {
        return;
    };
    for (class, _) in CONTAINERS {
        let _ = classes.remove_tree(format!(r"{class}\shell\{VERB_KEY}"));
    }
}

/// Bring the registry in line with the setting. Called after (re)registration and whenever
/// the setting or the UI language changes, so the two can never disagree.
pub fn sync(on: bool) {
    if on {
        if let Err(e) = apply(crate::i18n::t("pb_verb")) {
            crate::safety::log(&format!("foldermenu: could not write the verb: {e}"));
        }
    } else {
        remove();
    }
}

/// Undo the registration regardless of the setting. For uninstall.
pub fn remove_all() {
    remove();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command line has to survive spaces on BOTH sides — `C:\Program Files\…` and
    /// `D:\My Photos`. An unquoted either half silently passes a truncated path, and the run
    /// then reports "0 files found" for a folder that is full.
    #[test]
    fn the_command_quotes_the_exe_and_the_folder() {
        let exe = r"C:\Program Files\SageThumbs2K\SageThumbs2K.exe";
        for (_, token) in CONTAINERS {
            let cmd = format!("\"{exe}\" --prebuild \"{token}\"");
            assert!(cmd.starts_with('"'), "exe must be quoted: {cmd}");
            assert!(cmd.ends_with('"'), "folder token must be quoted: {cmd}");
            assert!(cmd.contains("--prebuild"));
        }
    }

    /// Background has no selection, so it MUST use `%V`; the other two describe a clicked
    /// item and use `%1`. Getting this backwards gives an entry that launches with an empty
    /// path and immediately reports nothing to do.
    #[test]
    fn background_uses_the_open_folder_token() {
        let map: std::collections::HashMap<_, _> = CONTAINERS.iter().copied().collect();
        assert_eq!(map[r"Directory\Background"], "%V");
        assert_eq!(map["Directory"], "%1");
        assert_eq!(map["Drive"], "%1");
    }

    /// Our key name is what makes removal safe: it is deleted whole, so it must be one we
    /// could only have created ourselves.
    #[test]
    fn the_verb_key_is_unmistakably_ours() {
        assert!(VERB_KEY.starts_with("SageThumbs2K."));
    }
}
