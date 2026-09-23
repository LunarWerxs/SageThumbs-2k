#![cfg(test)]

use super::{
    classify_selection, is_everything_class, is_everything_exe_name, is_rooted_path, join_under,
    resolve_result_row, SelectionOutcome,
};

/// Build the cell vector for a row, as `lv_cell` would return it.
fn cells(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// Everything 1.4's default layout: Name / Path / Size / Date Modified.
#[test]
fn default_columns_resolve_to_the_focused_file() {
    let row = cells(&["sample.png", "D:\\corpus", "6 KB", "01/02/2026 03:04"]);
    let got = resolve_result_row(&row, |p| p == "D:\\corpus\\sample.png");
    assert_eq!(got.as_deref(), Some("D:\\corpus\\sample.png"));
}

/// THE regression this rule exists for. A Path cell is itself an existing directory, so a
/// resolver that tests bare cells before joining hands back the PARENT folder — the file the
/// user actually focused never gets previewed.
#[test]
fn a_path_cell_that_exists_does_not_win_over_the_join() {
    let row = cells(&["Temp", "C:\\Users\\me\\AppData\\Local"]);
    // BOTH the bare Path cell and the joined path exist, exactly as on a real machine.
    let got = resolve_result_row(&row, |p| {
        p == "C:\\Users\\me\\AppData\\Local" || p == "C:\\Users\\me\\AppData\\Local\\Temp"
    });
    assert_eq!(got.as_deref(), Some("C:\\Users\\me\\AppData\\Local\\Temp"));
}

/// A user who shows "Full Path & Name" has no separate directory cell to join with.
#[test]
fn a_single_full_path_column_resolves_on_its_own() {
    let row = cells(&["D:\\corpus\\sample.png", "6 KB"]);
    let got = resolve_result_row(&row, |p| p == "D:\\corpus\\sample.png");
    assert_eq!(got.as_deref(), Some("D:\\corpus\\sample.png"));
}

/// Columns are reorderable, and the rule must not assume the path sits at index 1.
#[test]
fn the_directory_cell_can_be_any_column() {
    let row = cells(&["sample.png", "6 KB", "01/02/2026", "\\\\nas\\share\\pics"]);
    let got = resolve_result_row(&row, |p| p == "\\\\nas\\share\\pics\\sample.png");
    assert_eq!(got.as_deref(), Some("\\\\nas\\share\\pics\\sample.png"));
}

/// Nothing on disk matches (a deleted result, or an ETP/FTP row) → no path, not a guess.
#[test]
fn a_row_that_matches_nothing_on_disk_yields_nothing() {
    let row = cells(&["gone.png", "D:\\corpus", "6 KB"]);
    assert!(resolve_result_row(&row, |_| false).is_none());
    // An empty/odd row must not panic or invent an answer either.
    assert!(resolve_result_row(&cells(&[]), |_| true).is_none());
    assert!(resolve_result_row(&cells(&["", ""]), |_| true).is_none());
}

/// A drive root's Path cell is empty, so the row has nothing to join with. Previewing a
/// whole volume is meaningless anyway — the point is that it declines instead of panicking.
#[test]
fn a_drive_root_row_declines() {
    assert!(resolve_result_row(&cells(&["C:", ""]), |_| true).is_none());
}

/// `C:` + `x` must be `C:\x`. Plain concatenation gives `C:x`, which means "x relative to
/// C:'s current directory" — a different file, and almost never the one on screen.
#[test]
fn joining_handles_bare_drives_and_trailing_separators() {
    assert_eq!(join_under("C:", "x.png"), "C:\\x.png");
    assert_eq!(join_under("D:\\corpus", "x.png"), "D:\\corpus\\x.png");
    assert_eq!(join_under("D:\\corpus\\", "x.png"), "D:\\corpus\\x.png");
    // Only the TRAILING separator is trimmed; interior ones are left alone, because the
    // file APIs take mixed separators and rewriting a path is a good way to break a
    // legitimately odd one. Everything itself only ever emits backslashes.
    assert_eq!(join_under("D:/corpus/", "x.png"), "D:/corpus\\x.png");
    assert_eq!(
        join_under("\\\\nas\\share", "x.png"),
        "\\\\nas\\share\\x.png"
    );
}

#[test]
fn rooted_paths_are_told_apart_from_names_and_servers() {
    assert!(is_rooted_path("C:\\x"));
    assert!(is_rooted_path("d:/x"));
    assert!(is_rooted_path("\\\\nas\\share"));
    assert!(!is_rooted_path("C:")); // a bare drive, not a rooted path
    assert!(!is_rooted_path("sample.png"));
    assert!(!is_rooted_path("6 KB"));
    assert!(!is_rooted_path(""));
}

/// Everything's class name carries the INSTANCE name, so the stem is all we can match on.
/// These are the four real shapes it takes in the wild.
#[test]
fn everything_class_matches_every_instance_name() {
    assert!(is_everything_class("EVERYTHING")); // 1.4, and 1.5 from beta on
    assert!(is_everything_class("EVERYTHING_(1.5a)")); // 1.5 alpha, alpha_instance on
    assert!(is_everything_class("EVERYTHING_(portable)")); // -instance portable
    assert!(is_everything_class("EVERYTHING_TASKBAR_NOTIFICATION")); // never foreground
}

#[test]
fn everything_class_does_not_match_the_shell_or_a_lookalike() {
    assert!(!is_everything_class("CabinetWClass"));
    assert!(!is_everything_class("Progman"));
    assert!(!is_everything_class("SageThumbs2KViewer"));
    assert!(!is_everything_class("Everything")); // window classes are case-sensitive
    assert!(!is_everything_class(""));
}

/// Issue #209/P15: the class-name check alone trusts any local process that
/// registers a window under a lookalike class — the owning-process EXE name
/// is the actual gate. Every real voidtools shape must pass.
#[test]
fn everything_exe_name_matches_every_real_build() {
    assert!(is_everything_exe_name("Everything.exe"));
    assert!(is_everything_exe_name("Everything64.exe"));
    assert!(is_everything_exe_name("Everything-1.5a.exe"));
    assert!(is_everything_exe_name("EVERYTHING.EXE")); // exe names are case-insensitive
    assert!(is_everything_exe_name("EverythingPortable.exe"));
}

/// A spoofing process is free to register a window class that starts with
/// "EVERYTHING", but it cannot rename its own EXE to pass this check too.
#[test]
fn everything_exe_name_rejects_a_lookalike_process() {
    assert!(!is_everything_exe_name("evil.exe"));
    assert!(!is_everything_exe_name("notepad.exe"));
    assert!(!is_everything_exe_name("Everything.exe.bat")); // not a .exe
    assert!(!is_everything_exe_name(""));
}

/// Zero selected items (an empty `raw`) must read as `Empty`, never `VirtualOnly` — this is
/// the case a `SelectedItems().Count()` of 0 produces, and it must stay indistinguishable
/// from "the automation call itself failed", not get folded into the virtual-selection path.
#[test]
fn no_candidates_is_empty_not_virtual() {
    assert!(matches!(
        classify_selection(&[], |_| true),
        SelectionOutcome::Empty
    ));
}

/// THE regression this rule exists for: selecting the Recycle Bin (or This PC, or any other
/// virtual-namespace item) must be told apart from selecting nothing at all. Every candidate
/// present but none of them a real path -> `VirtualOnly`, not `Empty`.
#[test]
fn every_candidate_virtual_is_virtual_only() {
    let raw = vec![
        String::new(),                                          // Path() returned nothing
        "Recycle Bin".to_string(),                              // a bare display name
        "::{645FF040-5081-101B-9F08-00AA002F954E}".to_string(), // a shell namespace path
    ];
    assert!(matches!(
        classify_selection(&raw, |_| true),
        SelectionOutcome::VirtualOnly
    ));
}

/// A rooted-LOOKING path that isn't actually on disk (stale, moved, or a virtual item that
/// happens to hand back something path-shaped) must still count as virtual — syntax alone is
/// not enough, `exists` is the real gate.
#[test]
fn a_rooted_path_that_does_not_exist_is_virtual_only() {
    let raw = vec!["C:\\gone\\file.txt".to_string()];
    assert!(matches!(
        classify_selection(&raw, |_| false),
        SelectionOutcome::VirtualOnly
    ));
}

/// A mix of virtual and real items keeps only the real ones — one file selected alongside
/// the Recycle Bin still previews/acts on that file.
#[test]
fn a_mix_of_virtual_and_real_keeps_only_the_real_paths() {
    let raw = vec![
        "Recycle Bin".to_string(),
        "C:\\corpus\\sample.png".to_string(),
    ];
    let got = classify_selection(&raw, |p| p == "C:\\corpus\\sample.png");
    match got {
        SelectionOutcome::Paths(paths) => assert_eq!(paths, vec!["C:\\corpus\\sample.png"]),
        _ => panic!("expected Paths"),
    }
}
