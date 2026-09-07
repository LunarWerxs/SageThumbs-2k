//! The doctor's shareable bundle must never carry the sign-in state (2026-09-05 audit, E01),
//! proven end to end on a PORTABLE copy: a scratch ini seeded with a synthetic refresh token,
//! licence certificate and identity, `doctor::bundle` run in-process over it, and every entry
//! of the zip searched byte for byte for each of them. The preferences beside them have to
//! come through, or "nothing leaked" would be equally true of an empty file.
//!
//! One process per storage backend: `settings::portable()` resolves once from
//! `ST2K_PORTABLE_INI`, so the installed-copy half is `doctor_bundle_scrub_registry.rs`.
//! `LOCALAPPDATA` is pointed at the scratch folder too, so the bundle's log tail is the
//! deterministic "no log yet" line rather than whatever this machine's real log holds.
//!
//! Has teeth: with `doctor::without_credentials` reduced to the identity this fails on the
//! token, the certificate and the name, because the snapshot reads the ini's whole root.

mod common;

use sagethumbs2k_core::{doctor, settings};

const TOKEN: &str = "synthetic-refresh-token-7f3a9c";
const CERT: &str = "synthetic-licence-cert-2b8e41";
const NAME: &str = "Synthetic Person";
const EMAIL: &str = "synthetic-relay@example.invalid";
const SUB: &str = "synthetic-sub-0f1e2d";

#[test]
fn portable_bundle_carries_preferences_but_never_the_sign_in_state() {
    let dir =
        std::env::temp_dir().join(format!("st2k_doctor_scrub_portable_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let ini = dir.join("SageThumbs2K.ini");
    std::fs::write(
        &ini,
        format!(
            "[Settings]\nTheme=1\nMaxSize=200\n\
             OAuth_RefreshToken={TOKEN}\nOAuth_LicenceCert={CERT}\n\
             OAuth_Sub={SUB}\nOAuth_Email={EMAIL}\nOAuth_Name={NAME}\n\
             [jpg]\nEnabled=0\n"
        ),
    )
    .expect("seed ini");

    // Before ANY settings call: the storage mode resolves once per process.
    unsafe {
        common::remove_test_env("ST2K_SETTINGS_ROOT");
        common::set_test_env("ST2K_PORTABLE_INI", &ini);
        common::set_test_env("LOCALAPPDATA", &dir);
    }
    assert!(
        settings::portable(),
        "ST2K_PORTABLE_INI must put this process in portable mode"
    );
    assert_eq!(
        settings::get_string_opt("OAuth_Name").as_deref(),
        Some(NAME),
        "the seeded sign-in state must be readable, or the scrub has nothing to prove"
    );

    let out = dir.join("bundle.zip");
    doctor::bundle(&out, None).expect("bundle");
    let entries = common::zip_entries(&out);
    common::assert_bundle_is_scrubbed(&entries, &[TOKEN, CERT, NAME, EMAIL, SUB]);

    let _ = std::fs::remove_dir_all(&dir);
}
