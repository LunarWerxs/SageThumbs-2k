//! Settings export/import through the shipped EXE, in PORTABLE mode and back into a scratch
//! registry root, against synthetic credentials. This is the end-to-end half of the 2026-09-05
//! audit's F04/F05 fixes; the plan/filter logic has unit tests in `settings_io.rs`.
//!
//! - **F05:** on a portable copy the credential and identity values live as `OAuth_*` names in
//!   the ini's ROOT section, beside every ordinary preference, so the export used to carry the
//!   (DPAPI-encrypted) refresh token, the licence certificate and the account identity, and an
//!   import of another backup replaced or deleted the current sign-in. Now they are absent from
//!   the export, immune to the import's replace pass, and cannot be injected by a document that
//!   carries them, on the ini side AND when that export lands in an installed copy's registry.
//! - **F04:** an import that carries no usable setting is refused BEFORE anything is touched;
//!   the first version deleted the existing configuration first and then reported "No settings
//!   were found". The ini must be byte-identical after every refused import.
//!
//! Each case is its own EXE process (the portable switch resolves once per process), driven by
//! the hidden `--export-settings <path>` / `--import-settings <path>` flags with
//! `ST2K_PORTABLE_INI` pointing at a scratch ini, or `ST2K_SETTINGS_ROOT` at a scratch HKCU
//! subkey for the installed side. No real credentials, no real user settings.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch_dir() -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("st2k_settings_io_portable_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Run the app EXE in portable mode against `ini` with `args`; returns whether it exited 0.
fn run_portable(ini: &Path, args: &[&Path]) -> bool {
    Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .env("ST2K_PORTABLE_INI", ini)
        .env_remove("ST2K_SETTINGS_ROOT")
        .args(args)
        .status()
        .expect("spawn SageThumbs2K (portable)")
        .success()
}

/// Run the app EXE in INSTALLED mode with its settings root redirected to `root`.
fn run_installed(root: &str, args: &[&Path]) -> bool {
    Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .env_remove("ST2K_PORTABLE_INI")
        .env("ST2K_SETTINGS_ROOT", root)
        .args(args)
        .status()
        .expect("spawn SageThumbs2K (installed)")
        .success()
}

const SEEDED_INI: &str = "[Settings]\n\
                          Theme=1\n\
                          MaxSize=200\n\
                          OAuth_RefreshToken=synthetic-encrypted-blob\n\
                          OAuth_Name=Some One\n\
                          OAuth_LicenceCert=synthetic-cert-blob\n\
                          ConnectionsSyncPending=1\n\
                          [jpg]\n\
                          Enabled=0\n";

/// `key=value` lines of an ini's `[Settings]`-or-header-less root, plus each section's lines
/// prefixed `section/`, sorted, so two states compare with `assert_eq!`. Comment lines (the
/// store writes a two-line `;` banner) are not values and are skipped.
fn ini_lines(ini: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(ini).unwrap_or_default();
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with([';', '#']))
    {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_string();
        } else if section.is_empty() || section == "Settings" {
            out.push(line.to_string());
        } else {
            out.push(format!("{section}/{line}"));
        }
    }
    out.sort();
    out
}

#[test]
fn portable_export_omits_credentials_and_import_preserves_them() {
    let dir = scratch_dir();
    let ini = dir.join("SageThumbs2K.ini");
    let export = dir.join("export.json");
    std::fs::write(&ini, SEEDED_INI).expect("seed ini");

    // ---- export: preferences only -----------------------------------------------------
    let export_flag = Path::new("--export-settings");
    assert!(
        run_portable(&ini, &[export_flag, &export]),
        "export must succeed"
    );
    let json = std::fs::read_to_string(&export).expect("export written");
    for leaked in [
        "OAuth_",
        "synthetic-encrypted-blob",
        "Some One",
        "synthetic-cert-blob",
        "ConnectionsSyncPending",
    ] {
        assert!(
            !json.contains(leaked),
            "portable export leaked {leaked}:\n{json}"
        );
    }
    assert!(json.contains("\"Theme\": 1"), "{json}");
    assert!(json.contains("\"MaxSize\": 200"), "{json}");
    assert!(json.contains("\"jpg\""), "{json}");

    // ---- refused imports leave the ini byte-identical (F04) --------------------------
    let before = std::fs::read(&ini).expect("read ini");
    let import_flag = Path::new("--import-settings");
    for (name, doc) in [
        ("empty-values", r#"{"values":{}}"#),
        ("empty-both", r#"{"values":{},"subkeys":{}}"#),
        (
            "only-protected",
            r#"{"values":{"OAuth_RefreshToken":"attacker"},"subkeys":{"OAuth":{"RefreshToken":"attacker"}}}"#,
        ),
        ("unrepresentable", r#"{"values":{"Arr":[1],"Neg":-1}}"#),
        ("garbage", "not json"),
    ] {
        let doc_path = dir.join(format!("{name}.json"));
        std::fs::write(&doc_path, doc).expect("write doc");
        assert!(
            !run_portable(&ini, &[import_flag, &doc_path]),
            "{name}: an import with nothing usable must exit non-zero"
        );
        assert_eq!(
            std::fs::read(&ini).expect("read ini"),
            before,
            "{name}: a refused import changed the ini"
        );
    }

    // ---- a real import replaces preferences and keeps protected state (F05) ------------
    let theme_only = dir.join("theme-only.json");
    std::fs::write(
        &theme_only,
        r#"{"values":{"Theme":2,"OAuth_RefreshToken":"attacker","ConnectionsSyncPending":0},"subkeys":{"OAuth":{"RefreshToken":"attacker"}}}"#,
    )
    .expect("write doc");
    assert!(
        run_portable(&ini, &[import_flag, &theme_only]),
        "import must succeed"
    );
    let after = ini_lines(&ini);
    assert_eq!(
        after,
        [
            "ConnectionsSyncPending=1",
            "OAuth_LicenceCert=synthetic-cert-blob",
            "OAuth_Name=Some One",
            "OAuth_RefreshToken=synthetic-encrypted-blob",
            "Theme=2",
        ],
        "protected values intact, Theme replaced, MaxSize and [jpg] gone, nothing injected"
    );

    // ---- the portable export lands in an installed copy without the credentials -------
    let root = format!(r"Software\SageThumbs2K_iotest_exe_{}", std::process::id());
    let _ = windows_registry::CURRENT_USER.remove_tree(&root);
    assert!(
        run_installed(&root, &[import_flag, &export]),
        "installed import must succeed"
    );
    let key = windows_registry::CURRENT_USER
        .open(&root)
        .expect("scratch root exists");
    assert_eq!(key.get_u32("Theme").ok(), Some(1));
    assert_eq!(key.get_u32("MaxSize").ok(), Some(200));
    assert_eq!(
        key.open("jpg").and_then(|k| k.get_u32("Enabled")).ok(),
        Some(0)
    );
    let names: Vec<String> = key
        .values()
        .map(|v| v.map(|(n, _)| n).collect())
        .unwrap_or_default();
    assert!(
        !names
            .iter()
            .any(|n| n.to_ascii_lowercase().starts_with("oauth_")
                || n.eq_ignore_ascii_case("ConnectionsSyncPending")),
        "the installed copy must not receive protected values: {names:?}"
    );
    let _ = windows_registry::CURRENT_USER.remove_tree(&root);

    let _ = std::fs::remove_dir_all(&dir);
}
