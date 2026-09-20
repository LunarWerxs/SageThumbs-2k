#![cfg(test)]

use super::*;

/// Round-trip a representative tree (DWORDs + a string at the root, plus a per-format
/// `<ext>\Enabled` subkey) through export → wipe → import, against a throwaway HKCU
/// key, and assert every value survives with the right type.
#[test]
fn round_trips_values_and_subkeys() {
    const KEY: &str = r"Software\SageThumbs2K_iotest";
    let _ = CURRENT_USER.remove_tree(KEY); // clean slate
    let root = CURRENT_USER.create(KEY).unwrap();
    root.set_u32("Width", 333).unwrap();
    root.set_u32("EnableThumbs", 0).unwrap();
    root.set_string("Lang", "fr").unwrap();
    root.create("jpg").unwrap().set_u32("Enabled", 0).unwrap();

    let json = export_tree(Some(&root));
    assert!(json.contains("\"Width\": 333"), "{json}");
    assert!(json.contains("\"Lang\": \"fr\""), "{json}");
    assert!(json.contains("\"jpg\""), "{json}");

    // Wipe, then import the JSON back into a fresh key.
    CURRENT_USER.remove_tree(KEY).unwrap();
    let root = CURRENT_USER.create(KEY).unwrap();
    let n = import_tree(&root, &json).unwrap();
    assert!(n >= 4, "wrote {n}");
    assert_eq!(root.get_u32("Width").unwrap(), 333);
    assert_eq!(root.get_u32("EnableThumbs").unwrap(), 0);
    assert_eq!(root.get_string("Lang").unwrap(), "fr");
    assert_eq!(root.open("jpg").unwrap().get_u32("Enabled").unwrap(), 0);

    let _ = CURRENT_USER.remove_tree(KEY); // cleanup
}

/// `export_tree` must never surface the OAuth subkey, and `import_tree` must never let a
/// crafted/foreign export doc write one into the registry (which would clobber the local
/// refresh token / sign the user out).
#[test]
fn export_and_import_skip_oauth_subkey() {
    const KEY: &str = r"Software\SageThumbs2K_iotest_oauth";
    let _ = CURRENT_USER.remove_tree(KEY);
    let root = CURRENT_USER.create(KEY).unwrap();
    root.set_u32("Width", 42).unwrap();
    root.create("OAuth")
        .unwrap()
        .set_string("RefreshToken", "super-secret-blob")
        .unwrap();

    let json = export_tree(Some(&root));
    assert!(
        !json.contains("OAuth"),
        "export leaked the OAuth subkey: {json}"
    );
    assert!(
        !json.contains("super-secret-blob"),
        "export leaked the refresh token: {json}"
    );

    // A doc that carries an OAuth subkey (e.g. a hand-edited or foreign export) must not
    // be able to write/overwrite it via import.
    CURRENT_USER.remove_tree(KEY).unwrap();
    let root = CURRENT_USER.create(KEY).unwrap();
    let malicious = r#"{"values":{},"subkeys":{"OAuth":{"RefreshToken":"attacker-value"}}}"#;
    assert!(
        import_tree(&root, malicious).is_err(),
        "no other settings, should be a no-op"
    );
    assert!(
        root.open("OAuth").is_err(),
        "import must not create the OAuth subkey"
    );

    let _ = CURRENT_USER.remove_tree(KEY); // cleanup
}

/// An out-of-range u32 in the `values` table (e.g. `4294967296`, one past u32::MAX) must
/// be skipped rather than silently truncated by a bare `as u32` (which would wrap it to 0
/// and still count the write as successful).
#[test]
fn write_values_rejects_out_of_range_u32() {
    const KEY: &str = r"Software\SageThumbs2K_iotest_overflow";
    let _ = CURRENT_USER.remove_tree(KEY);
    let root = CURRENT_USER.create(KEY).unwrap();

    let doc = r#"{"values":{"Good":10,"TooBig":4294967296},"subkeys":{}}"#;
    let n = import_tree(&root, doc).unwrap();
    assert_eq!(n, 1, "only the in-range value should be written");
    assert_eq!(root.get_u32("Good").unwrap(), 10);
    assert!(
        root.get_u32("TooBig").is_err(),
        "the truncated wraparound value must not land"
    );

    let _ = CURRENT_USER.remove_tree(KEY); // cleanup
}

/// A malformed file and an empty document are both rejected (no partial writes).
#[test]
fn rejects_garbage_and_empty() {
    const KEY: &str = r"Software\SageThumbs2K_iotest2";
    let root = CURRENT_USER.create(KEY).unwrap();
    assert!(import_tree(&root, "not json at all").is_err());
    assert!(import_tree(&root, "{}").is_err());
    assert!(import_tree(&root, r#"{"values":{}}"#).is_err());
    let _ = CURRENT_USER.remove_tree(KEY);
}

/// F04: a document that is refused must leave the existing configuration EXACTLY as it
/// was. The first version deleted every stale root value and subkey first and only then
/// found the document carried nothing, so `{"values":{}}` into a configured instance wiped
/// it and reported "No settings were found". Seeds real state, imports every refusable
/// shape, and asserts value-for-value preservation after each.
#[test]
fn a_refused_import_leaves_existing_settings_untouched() {
    const KEY: &str = r"Software\SageThumbs2K_iotest_refused";
    let _ = CURRENT_USER.remove_tree(KEY);
    let root = CURRENT_USER.create(KEY).unwrap();
    root.set_u32("MaxSize", 200).unwrap();
    root.set_string("Lang", "fr").unwrap();
    root.create("jpg").unwrap().set_u32("Enabled", 0).unwrap();
    root.create("MenuItems")
        .unwrap()
        .set_u32("menu_convert_into", 0)
        .unwrap();
    root.create("OAuth")
        .unwrap()
        .set_string("RefreshToken", "blob")
        .unwrap();
    let before = export_tree(Some(&root));

    for doc in [
        r#"{"values":{}}"#,
        r#"{"values":{},"subkeys":{}}"#,
        r#"{"values":{"Unsupported":[1,2,3],"Null":null,"Neg":-1,"Frac":1.5}}"#,
        r#"{"values":{},"subkeys":{"jpg":{},"MenuItems":{"x":{}}}}"#,
        r#"{"subkeys":{"OAuth":{"RefreshToken":"attacker"}}}"#,
        "{}",
        "[]",
        "not json",
    ] {
        let err = import_tree(&root, doc).expect_err(doc);
        assert!(
            err.contains("No settings") || err.contains("valid settings file"),
            "{doc}: {err}"
        );
        assert_eq!(
            export_tree(Some(&root)),
            before,
            "state changed under a refused import of {doc}"
        );
    }
    assert_eq!(
        root.open("OAuth")
            .unwrap()
            .get_string("RefreshToken")
            .unwrap(),
        "blob"
    );

    let _ = CURRENT_USER.remove_tree(KEY); // cleanup
}

/// F05: the portable-mode credential names (`OAuth_*`, kept in the ROOT beside ordinary
/// preferences) and the sync retry marker are protected state in BOTH backends: absent
/// from exports, never written by an import, never removed by its replace pass. The same
/// classification the ini path uses is exercised here against a registry root.
#[test]
fn protected_root_values_are_neither_exported_nor_imported_nor_deleted() {
    const KEY: &str = r"Software\SageThumbs2K_iotest_protected_root";
    let _ = CURRENT_USER.remove_tree(KEY);
    let root = CURRENT_USER.create(KEY).unwrap();
    root.set_u32("Width", 42).unwrap();
    root.set_string("OAuth_RefreshToken", "encrypted-blob")
        .unwrap();
    root.set_string("oauth_name", "Some One").unwrap();
    root.set_string("OAuth_LicenceCert", "cert-blob").unwrap();
    root.set_u32("ConnectionsSyncPending", 1).unwrap();

    let json = export_tree(Some(&root));
    for leaked in [
        "OAuth_",
        "oauth_",
        "encrypted-blob",
        "Some One",
        "cert-blob",
        "ConnectionsSyncPending",
    ] {
        assert!(!json.contains(leaked), "export leaked {leaked}: {json}");
    }
    assert!(json.contains("\"Width\": 42"), "{json}");

    // A Theme-only backup: the replace pass drops Width (unprotected, not in the doc) and
    // must keep every protected value exactly as it was.
    let n = import_tree(&root, r#"{"values":{"Theme":1},"subkeys":{}}"#).unwrap();
    assert_eq!(n, 1);
    assert_eq!(root.get_u32("Theme").unwrap(), 1);
    assert!(root.get_u32("Width").is_err(), "Width was replaced away");
    assert_eq!(
        root.get_string("OAuth_RefreshToken").unwrap(),
        "encrypted-blob"
    );
    assert_eq!(root.get_string("oauth_name").unwrap(), "Some One");
    assert_eq!(root.get_string("OAuth_LicenceCert").unwrap(), "cert-blob");
    assert_eq!(root.get_u32("ConnectionsSyncPending").unwrap(), 1);

    // A backup that CARRIES protected names (another machine's, or hand-edited) cannot
    // inject them: they are dropped from the plan, the rest imports normally.
    let n = import_tree(
        &root,
        r#"{"values":{"Theme":2,"OAuth_RefreshToken":"attacker","OAuth_Sub":"x","ConnectionsSyncPending":0},"subkeys":{}}"#,
    )
    .unwrap();
    assert_eq!(n, 1, "only Theme is a settable value here");
    assert_eq!(root.get_u32("Theme").unwrap(), 2);
    assert_eq!(
        root.get_string("OAuth_RefreshToken").unwrap(),
        "encrypted-blob"
    );
    assert!(
        root.get_string("OAuth_Sub").is_err(),
        "must not be injected"
    );
    assert_eq!(root.get_u32("ConnectionsSyncPending").unwrap(), 1);

    let _ = CURRENT_USER.remove_tree(KEY); // cleanup
}

/// The portable plan is built from the document alone, so its filters are checkable
/// without a portable backend: protected names out, ini-unsafe NAMES out, multi-line
/// values out, unrepresentable types out, and the empty result refused. A value that
/// merely LOOKS like ini syntax (`; nope`) stays: the store quotes it (2026-09-19, F17).
#[test]
fn the_portable_plan_drops_protected_unsafe_and_unrepresentable_entries() {
    let plan = Plan::from_document(
        r#"{"values":{"Theme":1,"OAuth_RefreshToken":"blob","ConnectionsSyncPending":1,
                      "Bad=Name":1,"Multi":"a\nb","Comment":"; nope","Lang":"fr","Flag":true,
                      "Arr":[1],"Neg":-3},
            "subkeys":{"OAuth":{"RefreshToken":"blob"},"jpg":{"Enabled":0},"[x]":{"a":1},
                       "Empty":{},"NotATable":5}}"#,
        true,
    )
    .unwrap();
    assert_eq!(
        plan.values.keys().cloned().collect::<Vec<_>>(),
        ["Comment", "Flag", "Lang", "Theme"]
    );
    assert_eq!(text_of(&plan.values["Flag"]), "1");
    assert_eq!(text_of(&plan.values["Comment"]), "; nope");
    assert_eq!(plan.subkeys.keys().cloned().collect::<Vec<_>>(), ["jpg"]);
    assert_eq!(plan.planned(), 5);

    assert!(Plan::from_document(r#"{"values":{"OAuth_Name":"x"}}"#, true).is_err());
    assert!(Plan::from_document(r#"{"values":{"Bad=Name":1}}"#, true).is_err());
    // The same unsafe name is FINE for the registry, where `=` is an ordinary character.
    assert!(Plan::from_document(r#"{"values":{"Bad=Name":1}}"#, false).is_ok());
}

/// The classification this module consults, pinned where it is consumed: every
/// portable-mode credential name, in any case, and nothing that merely resembles one.
#[test]
fn credential_and_sync_state_classification() {
    for name in [
        "OAuth_RefreshToken",
        "OAuth_LicenceCert",
        "OAuth_Sub",
        "OAuth_Email",
        "OAuth_Name",
        "OAuth_Picture",
        "oauth_refreshtoken",
        "OAUTH_X",
    ] {
        assert!(protected_root_value(name), "{name} must be protected");
    }
    assert!(protected_root_value("ConnectionsSyncPending"));
    for name in [
        "Theme",
        "OAuth",
        "OAuthy",
        "MaxSize",
        "oauth",
        "Lang",
        "OAuth-Name",
    ] {
        assert!(!protected_root_value(name), "{name} must NOT be protected");
    }
    assert!(protected_subkey("OAuth"));
    assert!(protected_subkey("oauth"));
    assert!(!protected_subkey("jpg"));
    assert!(!protected_subkey("OAuth_"));
}

/// Item 33/221: import used to MERGE - a root value or a whole subkey the target already
/// had but the imported document didn't mention survived untouched, so restoring a backup
/// left a hybrid state indistinguishable from a correct restore. This pins the fix: both
/// kinds of leftover must be gone after import, not just overwritten where the document
/// happens to agree.
#[test]
fn import_replaces_rather_than_merges() {
    const KEY: &str = r"Software\SageThumbs2K_iotest_replace";
    let _ = CURRENT_USER.remove_tree(KEY);
    let root = CURRENT_USER.create(KEY).unwrap();
    root.set_u32("Width", 333).unwrap();
    root.set_u32("StaleLeftover", 1).unwrap();
    root.create("jpg").unwrap().set_u32("Enabled", 0).unwrap();

    // Restoring a backup that never had `StaleLeftover` set and never touched the `jpg`
    // subkey - both a root value and an entire subkey are declared absent.
    let doc = r#"{"values":{"Width":333},"subkeys":{}}"#;
    let n = import_tree(&root, doc).unwrap();
    assert_eq!(n, 1, "wrote {n}");

    assert_eq!(root.get_u32("Width").unwrap(), 333);
    assert!(
        root.get_u32("StaleLeftover").is_err(),
        "a root value absent from the document must be deleted, not merged in from before"
    );
    assert!(
        root.open("jpg").is_err(),
        "a subkey absent from the document's subkeys table must be deleted entirely"
    );

    let _ = CURRENT_USER.remove_tree(KEY); // cleanup
}

/// The replace-deletion pass must never remove the OAuth subkey, regardless of the case
/// it happens to be stored in - registry subkey names are case-insensitive, so a lookup
/// that only matched the canonical `"OAuth"` casing would still delete a stray `"oauth"`
/// (item 113, applied to the new deletion pass as well as the existing write-skip).
#[test]
fn import_replace_never_deletes_the_oauth_subkey() {
    const KEY: &str = r"Software\SageThumbs2K_iotest_replace_oauth";
    let _ = CURRENT_USER.remove_tree(KEY);
    let root = CURRENT_USER.create(KEY).unwrap();
    root.create("oauth")
        .unwrap()
        .set_string("RefreshToken", "keep-me")
        .unwrap();

    let doc = r#"{"values":{"Width":100},"subkeys":{}}"#;
    import_tree(&root, doc).unwrap();

    assert_eq!(
        root.open("OAuth")
            .unwrap()
            .get_string("RefreshToken")
            .unwrap(),
        "keep-me",
        "the OAuth subkey must survive a full replace import even though it was stored \
         (and looked up here) under a non-canonical case"
    );

    let _ = CURRENT_USER.remove_tree(KEY); // cleanup
}

fn scratch_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "st2k_settings_io_export_{label}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn assert_no_leftover_temp_files(dir: &Path) {
    let leftovers: Vec<String> = std::fs::read_dir(dir)
        .expect("read scratch dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
}

/// A successful export writes something that parses as the documented shape - the
/// `_about` field plus `values`/`subkeys` objects [`export_tree`]'s own tests already
/// pin the CONTENT of - and leaves no staging file behind.
#[test]
fn export_settings_to_path_writes_parseable_json_with_no_leftover_temp_file() {
    let dir = scratch_dir("success");
    let path = dir.join("SageThumbs2K-settings.json");

    export_settings_to_path(&path).expect("export_settings_to_path");

    let text = std::fs::read_to_string(&path).expect("read exported file");
    let doc: Json = serde_json::from_str(&text).expect("exported file must be valid JSON");
    assert!(doc.get("_about").is_some(), "{text}");
    assert!(doc.get("values").is_some(), "{text}");
    assert!(doc.get("subkeys").is_some(), "{text}");
    assert_no_leftover_temp_files(&dir);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 2026-09-05 audit, F13, and its 2026-09-05 follow-up: `export_settings_to_file`
/// (Diagnostics ▸ Export) and `--export-settings` both used to `fs::write` straight to
/// the chosen path, so replacing an existing backup and then hitting a write failure
/// left the OLD backup truncated even though the app reported the export as failed.
/// **The read-only-destination version of this test had no teeth**: on Windows,
/// `fs::write` on a read-only file fails at `CreateFileW`, before a single byte is
/// written, so the OLD, unfixed `export_settings_to_file` (a bare `fs::write` straight
/// onto `path`) would ALSO have left `original` untouched in that scenario - the test
/// passed identically before and after the fix and proved nothing, despite its doc
/// comment claiming otherwise.
///
/// This drives the same fail-point `fsutil::write_atomically`'s own tests use
/// (`sagethumbs2k_core::fsutil::inject_partial_write_failure` - exposed across the
/// crate boundary rather than gated `#[cfg(test)]`, because `#[cfg(test)]` items are
/// only compiled when the LIB itself is the crate under test and are invisible to this
/// bin crate's own tests; see that function's doc comment) to fail the write after 4 of
/// the new content's bytes have already landed in the staging file - a scenario a bare
/// `fs::write` to `path` cannot survive.
#[test]
fn export_settings_to_path_never_destroys_a_prior_backup_on_failed_replace() {
    let dir = scratch_dir("partial_write_failure");
    let path = dir.join("SageThumbs2K-settings.json");
    let original: &[u8] = b"a previous export backup, not valid JSON on purpose";
    std::fs::write(&path, original).unwrap();

    sagethumbs2k_core::fsutil::inject_partial_write_failure(4);
    let result = export_settings_to_path(&path);
    sagethumbs2k_core::fsutil::clear_partial_write_failure();

    assert!(
        result.is_err(),
        "an injected mid-write failure must be reported, not silently succeed"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "the prior backup must survive byte-identical after a failed export"
    );
    assert_no_leftover_temp_files(&dir);

    let _ = std::fs::remove_dir_all(&dir);
}
