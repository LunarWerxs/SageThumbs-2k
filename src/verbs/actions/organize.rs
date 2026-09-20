//! The folder-organising verbs: folder icon, files to folder, rename by pattern, sort by dimensions / date taken, tags to folders.

use super::*;

/// `VerbAction::SetFolderIcon` - one folder icon. Use the first *image* in the
/// selection. Routed to `st2k folder-icon` (helper-if-present), which runs the whole
/// verb in the disposable child; else falls back to in-process `set_folder_icon`.
pub(super) fn handle_set_folder_icon(paths: &[String]) -> ActionReport {
    first_image_action(
        paths,
        "Set folder icon",
        "couldn't set the folder icon",
        |p| folder_icon_one(st2k_exe().as_deref(), p),
    )
}

/// `VerbAction::FilesToFolder` - operates on ALL selected files (any type), not just
/// images. One file → a folder named after it (no prompt); many → the name-prompt
/// dialog in the companion app.
pub(super) fn handle_files_to_folder(paths: &[String]) -> ActionReport {
    match paths.len() {
        0 => ActionReport::default(),
        1 => {
            let stem = Path::new(&paths[0])
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("New Folder");
            // `files_to_folder` now reports (dir, moved, skipped) instead of a bare `Ok`/`Err`
            // (the multi-file dialog case's fix — see the companion app's `files_to_folder.rs`
            // — applies here too): a single-file move that silently skipped would otherwise
            // still read as a clean `applied(1, 1)`.
            match files_to_folder(paths, stem) {
                Ok((_, moved, skipped)) => {
                    let mut r = ActionReport::applied(moved + skipped, moved);
                    if skipped > 0 {
                        r.note = Some("couldn't move the file into the new folder".into());
                    }
                    r
                }
                Err(e) => {
                    crate::safety::log(&format!("Files to folder failed: {e:?}"));
                    ActionReport::applied(1, 0).with_note("couldn't create or fill the folder")
                }
            }
        }
        _ => match launch_files_to_folder(paths) {
            ListLaunch::Failed => {
                ActionReport::applied(1, 0).with_note("couldn't hand off the file list")
            }
            _ => ActionReport::delegated(),
        },
    }
}

/// `VerbAction::RenameWithPattern` - always opens the companion app's dialog (the
/// pattern/find/replace live in the user's head, not the menu), unlike
/// `VerbAction::FilesToFolder`'s single-file no-prompt shortcut. Any file type, like
/// `FilesToFolder` - the pattern engine only *optionally* reads image metadata.
pub(super) fn handle_rename_with_pattern(paths: &[String]) -> ActionReport {
    if paths.is_empty() {
        return ActionReport::default();
    }
    match launch_rename_with_pattern(paths) {
        ListLaunch::Failed => {
            ActionReport::applied(1, 0).with_note("couldn't hand off the file list")
        }
        _ => ActionReport::delegated(),
    }
}

/// `VerbAction::SortByDimensions`.
pub(super) fn handle_sort_by_dimensions(paths: &[String]) -> ActionReport {
    bucket_sort_report(
        "Sort by dimensions",
        sort_by_dimensions(paths),
        "couldn't read size / move",
        "couldn't be read or moved",
    )
}

/// The report both bucket sorts share: a skip is logged with `why_log` and noted in the
/// report as `{skipped} {why_note}`; a clean run carries no note.
pub(super) fn bucket_sort_report(
    what: &str,
    (moved, skipped): (usize, usize),
    why_log: &str,
    why_note: &str,
) -> ActionReport {
    if skipped > 0 {
        crate::safety::log(&format!(
            "{what}: {moved} moved, {skipped} skipped ({why_log})"
        ));
        ActionReport::applied(moved + skipped, moved).with_note(format!("{skipped} {why_note}"))
    } else {
        ActionReport::applied(moved + skipped, moved)
    }
}

/// `VerbAction::SortByDateTaken` - same shape as [`handle_sort_by_dimensions`]; a file
/// with no EXIF capture date is skipped and counted, not treated as an error.
pub(super) fn handle_sort_by_date_taken(paths: &[String]) -> ActionReport {
    bucket_sort_report(
        "Sort by date taken",
        sort_by_date_taken(paths),
        "no capture date / couldn't move",
        "had no capture date or couldn't be moved",
    )
}

/// `VerbAction::TagsToFolders` - audio-only; the dialog (destination/template/
/// copy-move) lives in the companion app. No audio in the selection → nothing to do.
pub(super) fn handle_tags_to_folders(paths: &[String]) -> ActionReport {
    let audio: Vec<String> = paths
        .iter()
        .filter(|p| is_audio(p.as_str()))
        .cloned()
        .collect();
    if audio.is_empty() {
        return ActionReport::default();
    }
    match launch_tags_to_folders(&audio) {
        ListLaunch::Failed => {
            ActionReport::applied(1, 0).with_note("couldn't hand off the file list")
        }
        _ => ActionReport::delegated(),
    }
}
