//! The installed-copy half of `doctor_bundle_scrub_portable.rs` (2026-09-05 audit, E01): the
//! sign-in state lives in an `OAuth` subkey under the settings root, so the seed goes into a
//! scratch HKCU root redirected through `ST2K_SETTINGS_ROOT`, the same isolation
//! `tests/settings_gate.rs` uses. No real credential, no real user setting is touched, and
//! the scratch root is removed at the end.
//!
//! Has teeth: with `doctor::without_credentials` reduced to the identity this fails on the
//! token, the certificate and the name, because the snapshot walks every subkey of the root.

mod common;

use sagethumbs2k_core::{doctor, settings};
use windows_registry::CURRENT_USER;

const TOKEN: &str = "synthetic-refresh-token-5c2d7e";
const CERT: &str = "synthetic-licence-cert-9a4b13";
const NAME: &str = "Synthetic Person";
const EMAIL: &str = "synthetic-relay@example.invalid";
const SUB: &str = "synthetic-sub-6e5d4c";

#[test]
fn installed_bundle_carries_preferences_but_never_the_sign_in_state() {
    let dir =
        std::env::temp_dir().join(format!("st2k_doctor_scrub_registry_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let root = format!(r"Software\SageThumbs2K_doctor_scrub_{}", std::process::id());
    let _ = CURRENT_USER.remove_tree(&root);

    // Before ANY settings call: the root resolves once per process.
    unsafe {
        common::remove_test_env("ST2K_PORTABLE_INI");
        common::set_test_env("ST2K_SETTINGS_ROOT", &root);
        common::set_test_env("LOCALAPPDATA", &dir);
    }
    assert!(!settings::portable(), "this half is the registry backend");
    assert_eq!(settings::hkcu_root_path(), root);

    let key = CURRENT_USER.create(&root).expect("scratch root");
    key.set_u32("Theme", 1).expect("seed");
    key.set_u32("MaxSize", 200).expect("seed");
    key.create("jpg")
        .and_then(|k| k.set_u32("Enabled", 0))
        .expect("seed");
    let oauth = key.create("OAuth").expect("seed");
    for (name, value) in [
        ("RefreshToken", TOKEN),
        ("LicenceCert", CERT),
        ("Sub", SUB),
        ("Email", EMAIL),
        ("Name", NAME),
        ("Picture", "https://example.invalid/picture.png"),
    ] {
        oauth.set_string(name, value).expect("seed");
    }

    let out = dir.join("bundle.zip");
    doctor::bundle(&out, None).expect("bundle");
    let entries = common::zip_entries(&out);
    common::assert_bundle_is_scrubbed(&entries, &[TOKEN, CERT, NAME, EMAIL, SUB]);

    let _ = CURRENT_USER.remove_tree(&root);
    let _ = std::fs::remove_dir_all(&dir);
}
