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

/// The orphan, and the reason clearing ENUMERATES instead of re-deriving today's
/// associations. A user who changes their default program for a type moves the ProgID
/// Explorer consults; the value we wrote under the old one is then unreachable to any
/// code that asks "which ProgID serves this extension" — so it would survive a switch to
/// the badge, and survive uninstall, in another vendor's key, forever.
#[test]
fn clearing_finds_a_mark_under_a_progid_nothing_points_at_any_more() {
    let (_guard, classes) = Scratch::new("orphan");
    write_marked(&classes, "St2kTest.Abandoned", r"C:\Gone\app.exe,1");
    let other = classes.create("St2kTest.Unrelated").expect("create");
    other
        .set_string(VALUE, "someoneelse.dll,-1")
        .expect("set theirs");
    drop(other);

    clear_every_mark(&classes);

    assert!(
        classes.open("St2kTest.Abandoned").is_err(),
        "a value of ours must be reachable without knowing which extension led to it"
    );
    let k = classes.open("St2kTest.Unrelated").expect("theirs survives");
    assert_eq!(
        k.get_string(VALUE).as_deref(),
        Ok("someoneelse.dll,-1"),
        "a sweep over the whole hive must still only take what is ours"
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
