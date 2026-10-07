#![cfg(test)]

use super::{
    cleanup_installer_payload, is_security_body, offer_for, parse_cache, parse_ver, update_offer,
    LatestRelease, Offer,
};
use ed25519_dalek::{Signer, SigningKey};
use std::os::windows::process::CommandExt;
use std::path::Path;

/// The downloaded installer must be swept regardless of outcome — before this fix, only
/// the failure arm of `download_and_install`'s match on `launch_installer_silent` ever
/// deleted it, leaving a real ~10-15 MB setup .exe behind in %TEMP% on every ordinary,
/// successful update.
#[test]
fn cleanup_installer_payload_removes_the_file() {
    let path = std::env::temp_dir().join(format!(
        "st2k_update_cleanup_{}_{}.exe",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, b"fake installer bytes").unwrap();
    assert!(path.exists());

    cleanup_installer_payload(&path);

    assert!(
        !path.exists(),
        "the installer payload must be swept on every outcome, not only on failure"
    );
}

#[test]
fn parses_and_orders_versions() {
    assert_eq!(parse_ver("v0.4.6"), Some((0, 4, 6)));
    assert_eq!(parse_ver("0.4.5"), Some((0, 4, 5)));
    assert_eq!(parse_ver("V1.0"), Some((1, 0, 0)));
    assert_eq!(parse_ver("2"), Some((2, 0, 0)));
    assert_eq!(parse_ver("0.4.6-rc1"), Some((0, 4, 6)));
    assert_eq!(parse_ver("0.5.0+build7"), Some((0, 5, 0)));
    assert_eq!(parse_ver("not-a-version"), None);

    // The ordering the check relies on (tuple compare = correct semver ordering here).
    assert!(parse_ver("0.4.6") > parse_ver("0.4.5"));
    assert!(parse_ver("0.5.0") > parse_ver("0.4.9"));
    assert!(parse_ver("1.0.0") > parse_ver("0.9.9"));
    assert!(parse_ver("0.4.5") <= parse_ver("0.4.5")); // equal = up to date
}

#[test]
fn updated_toast_never_claims_a_version_we_arent_running() {
    use super::{updated_notice, UpdatedNotice as N};
    // Normal silent update: installer version == this image's version.
    assert_eq!(updated_notice("1.3.8", "1.3.8", false), N::Done);

    // Deferred-to-reboot replace: the installer was 1.3.8 but we're still the old EXE.
    // The toast must NOT say "you're now on 1.3.8" — that's the mystery-update report.
    assert_eq!(updated_notice("1.3.8", "1.3.7", false), N::StillOld);
    assert_eq!(updated_notice("1.3.8", "1.3.7", true), N::StillOld);

    // The EXE landed but setup queued another file for the restart (an ImageMagick DLL held
    // by a scanner): the versions match, and only setup's --restart-pending knows the update
    // is not finished. "You're now on 1.3.8" with nothing more was the wrong report there.
    assert_eq!(updated_notice("1.3.8", "1.3.8", true), N::RestartToFinish);

    // A "v"-prefixed tag is the same version, not a mismatch.
    assert_eq!(updated_notice("v1.3.8", "1.3.8", false), N::Done);

    // Unparseable installer version → don't cry "restart" at the user.
    assert_eq!(updated_notice("", "1.3.8", false), N::Done);
}

/// A throttle stamp from the future (written while the clock was wrong) must not read as
/// "checked a moment ago": that silenced every update check until the clock caught up.
#[test]
fn a_throttle_stamp_from_the_future_is_due() {
    let now = 1_800_000_000;
    let day = super::CHECK_INTERVAL.as_secs();
    assert!(
        !super::throttle_expired(now - 60, now),
        "checked a minute ago"
    );
    assert!(super::throttle_expired(now - day, now), "a day ago");
    assert!(
        super::throttle_expired(now + 365 * day, now),
        "stamped a year ahead by a wrong clock"
    );
}

/// The installer download must be able to finish on a slow link. The budget used to be a flat
/// eight minutes, so a 15 MB setup over a 16 KiB/s connection (~16 minutes) failed every time.
#[test]
fn a_slow_link_can_finish_the_installer_download() {
    let size: u64 = 15 * 1024 * 1024;
    let at_16_kib = size / (16 * 1024);
    assert!(
        super::installer_download_deadline_secs(size) > at_16_kib,
        "{} s budget for a download taking {at_16_kib} s",
        super::installer_download_deadline_secs(size)
    );
    // Never below the old floor, whatever the advertised size.
    assert_eq!(
        super::installer_download_deadline_secs(0),
        super::DOWNLOAD_TIMEOUT_SECS * 4
    );
}

/// GitHub's rate limit (403/429 on the unauthenticated API) is "try again in a while", and a
/// release that really has no installer for this PC is its own case: both used to read as
/// "couldn't find the installer for this PC".
#[test]
fn a_rate_limited_lookup_is_not_reported_as_a_missing_installer() {
    use super::fetch_failure_key as key;
    assert_eq!(key(Some(403), "upd_err_unreachable"), "upd_err_busy");
    assert_eq!(key(Some(429), "upd_err_download"), "upd_err_busy");
    assert_eq!(key(Some(404), "upd_err_unreachable"), "upd_err_http");
    assert_eq!(key(None, "upd_err_unreachable"), "upd_err_unreachable");
    assert_eq!(key(None, "upd_err_download"), "upd_err_download");
}

/// A portable copy never registers (or removes) the per-user Scheduled Task: Apply in its
/// Settings used to leave a task on the host pointing at the USB stick.
#[test]
fn a_portable_copy_leaves_the_scheduled_task_alone() {
    use super::task::{task_action, TaskAction as A};
    assert_eq!(task_action(true, true), A::Leave);
    assert_eq!(task_action(true, false), A::Leave);
    assert_eq!(task_action(false, true), A::Install);
    assert_eq!(task_action(false, false), A::Remove);
}

/// Every "update available" balloon leads somewhere on a click, and never promises a portable
/// copy an install it cannot do. The balloon used to do nothing when clicked.
#[test]
fn the_update_balloon_leads_to_what_this_copy_can_do() {
    use super::task::{toast_choice, ToastClick as C};
    let outside = Offer::OutsideWindow { ends_unix: 1 };
    assert_eq!(
        toast_choice(&Offer::Install, false),
        ("upd_toast_body", C::Settings("nav_advanced"))
    );
    assert_eq!(
        toast_choice(&Offer::Install, true),
        ("upd_toast_body_portable", C::Releases)
    );
    assert_eq!(
        toast_choice(&outside, false),
        ("upd_outside_toast", C::Settings("nav_licence"))
    );
    assert_eq!(
        toast_choice(&outside, true),
        ("upd_outside_toast", C::Settings("nav_licence"))
    );
}

#[test]
fn sha256_matches_nist_vectors() {
    assert_eq!(
        super::sha256_hex(b"abc").as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    assert_eq!(
        super::sha256_hex(b"").as_deref(),
        Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
}

/// A fixed-seed key so the test is deterministic - not the real signing key, and never
/// will be; see `the_compiled_in_key_is_not_the_placeholder` below for that one.
fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// 2026-09-19 audit concern 3: the signature says "ours", the stamp says "which". A
/// genuine older installer served under a newer tag, or the right tag with the wrong
/// bytes, is refused; only the advertised version, newer than the running one, passes.
#[test]
fn version_binding_refuses_a_mismatch_and_a_downgrade() {
    use super::VersionRefusal as R;
    assert_eq!(
        super::version_binding(Some((3, 1, 2)), "v3.1.2", "3.1.1"),
        Ok(())
    );
    assert_eq!(
        super::version_binding(Some((3, 1, 2)), "3.1.2", "3.1.1"),
        Ok(())
    );
    // A genuine, signed 3.0.5 offered as 3.1.2: the stamp gives it away.
    assert_eq!(
        super::version_binding(Some((3, 0, 5)), "v3.1.2", "3.1.1"),
        Err(R::NotTheOffered {
            stamped: (3, 0, 5),
            offered: (3, 1, 2)
        })
    );
    // The advertised tag matches the stamp but is not newer than what runs here.
    assert_eq!(
        super::version_binding(Some((3, 1, 1)), "v3.1.1", "3.1.1"),
        Err(R::NotNewer {
            stamped: (3, 1, 1),
            running: (3, 1, 1)
        })
    );
    assert_eq!(
        super::version_binding(Some((3, 0, 9)), "v3.0.9", "3.1.1"),
        Err(R::NotNewer {
            stamped: (3, 0, 9),
            running: (3, 1, 1)
        })
    );
    // No stamp at all is a refusal, never a pass.
    assert_eq!(
        super::version_binding(None, "v3.1.2", "3.1.1"),
        Err(R::NoStamp)
    );
    // An unparseable tag is a refusal too.
    assert_eq!(
        super::version_binding(Some((3, 1, 2)), "latest", "3.1.1"),
        Err(R::BadTag)
    );
}

/// The stamp reader on real PEs: a Windows system file carries the OS version, and a missing
/// file is `None`, not a panic. (That the app's own EXE carries the crate version is proven by
/// the binary's tests, the only test build the resource is linked into.)
#[test]
fn pe_stamped_version_reads_a_real_resource_and_tolerates_none() {
    let sys = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap())
        .join("System32")
        .join("kernel32.dll");
    let v = super::pe_stamped_version(&sys).expect("kernel32 carries a version resource");
    assert!(v.0 >= 6, "an NT 6+ kernel32: {v:?}");
    assert_eq!(
        super::pe_stamped_version(std::path::Path::new("C:\\does\\not\\exist.exe")),
        None
    );
}

fn hex_sig(sig: &ed25519_dalek::Signature) -> String {
    sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn sign_then_verify_round_trips() {
    let key = test_signing_key();
    let bytes = b"a release artifact's exact bytes";
    let sig_hex = hex_sig(&key.sign(bytes));
    assert!(super::verify_signature(
        &key.verifying_key().to_bytes(),
        bytes,
        &sig_hex
    ));
}

#[test]
fn a_flipped_byte_fails_verification() {
    let key = test_signing_key();
    let bytes = b"a release artifact's exact bytes";
    let sig_hex = hex_sig(&key.sign(bytes));
    let tampered = b"A release artifact's exact bytes"; // first byte flipped
    assert!(!super::verify_signature(
        &key.verifying_key().to_bytes(),
        tampered,
        &sig_hex
    ));
}

#[test]
fn a_wrong_key_fails_verification() {
    let key = test_signing_key();
    let other_key = SigningKey::from_bytes(&[9u8; 32]);
    let bytes = b"a release artifact's exact bytes";
    let sig_hex = hex_sig(&key.sign(bytes));
    assert!(!super::verify_signature(
        &other_key.verifying_key().to_bytes(),
        bytes,
        &sig_hex
    ));
}

#[test]
fn malformed_hex_is_refused_without_panicking() {
    let key = test_signing_key().verifying_key().to_bytes();
    for junk in [
        "",
        "not-hex-at-all-but-128-chars-long-so-length-alone-cannot-be-what-refuses-itxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        "deadbeef", // far too short
        "gg", // not hex, and too short
    ] {
        assert!(
            !super::verify_signature(&key, b"anything", junk),
            "{junk:?} should not verify"
        );
    }
    // Exactly 128 chars but with one non-hex character - the length check alone must not
    // be enough to pass this.
    let mut almost_valid = "a".repeat(128);
    almost_valid.replace_range(0..1, "z");
    assert!(!super::verify_signature(&key, b"anything", &almost_valid));
    // A non-ASCII (multi-byte) 128-char string must not panic on the byte-index slicing.
    let non_ascii: String = "é".repeat(64); // 128 bytes, but not ASCII hex
    assert!(!super::verify_signature(&key, b"anything", &non_ascii));
}

#[test]
fn the_compiled_in_key_is_not_the_placeholder() {
    // Fails until the integrator runs `examples/update-keygen.rs` and pastes its printed
    // public-key array literal over `UPDATE_PUBLIC_KEY` in this file. That is intentional:
    // an all-zero key makes every signature check refuse (see the constant's doc comment),
    // so a build that still carries the placeholder is safe, just permanently un-updatable
    // - this test is what turns "permanently un-updatable" into a build-time signal instead
    // of a silent trap discovered only when a real self-update is attempted.
    assert_ne!(
        super::UPDATE_PUBLIC_KEY,
        [0u8; 32],
        "UPDATE_PUBLIC_KEY is still the placeholder - paste in the real key from \
         examples/update-keygen.rs before shipping a build that must self-update"
    );
}

#[test]
fn finds_the_sibling_sig_asset_beside_the_installer() {
    let json = serde_json::json!({
        "tag_name": "v0.7.0",
        "assets": [
            { "name": "SageThumbs2K-Setup-0.7.0.exe",
              "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe",
              "size": 100u64,
              "digest": "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" },
            { "name": "SageThumbs2K-Setup-0.7.0.exe.sig",
              "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe.sig" }
        ]
    });
    let (_, asset) = super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
    assert_eq!(
        asset.sig_url.as_deref(),
        Some("https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe.sig")
    );
}

#[test]
fn an_absent_or_off_site_sig_asset_gives_no_sig_url() {
    let mut json = serde_json::json!({
        "tag_name": "v0.7.0",
        "assets": [
            { "name": "SageThumbs2K-Setup-0.7.0.exe",
              "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe",
              "size": 100u64,
              "digest": "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }
        ]
    });
    let (_, asset) = super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
    assert_eq!(asset.sig_url, None);

    // A signature served from anywhere but this project's releases is never fetched.
    json["assets"]
        .as_array_mut()
        .expect("assets")
        .push(serde_json::Value::Null);
    for url in [
        "https://evil.example/SageThumbs2K-Setup-0.7.0.exe.sig",
        "https://github.com/someone/else/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe.sig",
    ] {
        json["assets"][1] = serde_json::json!({ "name": "SageThumbs2K-Setup-0.7.0.exe.sig", "browser_download_url": url });
        let (_, asset) =
            super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
        assert_eq!(asset.sig_url, None, "{url}");
    }
}

#[test]
fn picks_x64_setup_exe_and_normalizes_digest() {
    let json = serde_json::json!({
        "tag_name": "v0.6.3",
        "assets": [
            { "name": "notes.txt", "browser_download_url": "https://x/notes.txt", "size": 1 },
            { "name": "SageThumbs2K-Setup-debug.exe",
              "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.6.3/SageThumbs2K-Setup-debug.exe",
              "size": 42u64,
              "digest": "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" },
            { "name": "SageThumbs2K-Setup-0.6.3.exe",
              "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.6.3/SageThumbs2K-Setup-0.6.3.exe",
              "size": 9_223_820u64,
              "digest": "sha256:09D79A0C6589D7DC5AF5472CB8B1B56AAC0DFF51A47003B1146A9409F65C9835" }
        ]
    });
    let (tag, asset) =
        super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
    assert_eq!(tag, "0.6.3");
    assert!(asset.url.ends_with("SageThumbs2K-Setup-0.6.3.exe"));
    assert_eq!(asset.size, 9_223_820);
    assert_eq!(
        asset.sha256,
        "09d79a0c6589d7dc5af5472cb8b1b56aac0dff51a47003b1146a9409f65c9835"
    );
}

#[test]
fn picks_only_the_matching_arm64_setup_exe() {
    let json = serde_json::json!({
        "tag_name": "v0.6.3",
        "assets": [
            { "name": "SageThumbs2K-Setup-0.6.3.exe",
              "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.6.3/SageThumbs2K-Setup-0.6.3.exe",
              "size": 100u64,
              "digest": "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" },
            { "name": "SageThumbs2K-Setup-0.6.3-arm64.exe",
              "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.6.3/SageThumbs2K-Setup-0.6.3-arm64.exe",
              "size": 200u64,
              "digest": "sha256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB" }
        ]
    });

    let (_, x64) = super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
    assert!(x64.url.ends_with("SageThumbs2K-Setup-0.6.3.exe"));
    assert_eq!(x64.size, 100);

    let (_, arm64) =
        super::installer_asset_from_json_for_arch(&json, "aarch64").expect("ARM64 asset");
    assert!(arm64.url.ends_with("SageThumbs2K-Setup-0.6.3-arm64.exe"));
    assert_eq!(arm64.size, 200);

    let x64_only = serde_json::json!({
        "tag_name": "v0.6.3",
        "assets": [json["assets"][0].clone()]
    });
    assert!(
        super::installer_asset_from_json_for_arch(&x64_only, "aarch64").is_none(),
        "ARM64 must not accept the x64 installer"
    );
    assert!(super::installer_asset_from_json_for_arch(&json, "x86").is_none());
}

#[test]
fn native_windows_architecture_controls_cross_arch_update() {
    use windows::Win32::System::SystemInformation::{
        PROCESSOR_ARCHITECTURE, PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM64,
    };

    assert_eq!(
        super::installer_arch_for_native(PROCESSOR_ARCHITECTURE_ARM64, "x86_64"),
        "aarch64",
        "an emulated x64 build on ARM64 must migrate to the native installer"
    );
    assert_eq!(
        super::installer_arch_for_native(PROCESSOR_ARCHITECTURE_AMD64, "x86_64"),
        "x86_64"
    );
    assert_eq!(
        super::installer_arch_for_native(PROCESSOR_ARCHITECTURE(u16::MAX), "aarch64"),
        "aarch64",
        "an unknown Windows architecture must fall back to the process target"
    );
}

#[test]
fn installer_asset_requires_digest_and_canonical_repo_url() {
    let base = serde_json::json!({
        "tag_name": "v1.2.3",
        "assets": [{
            "name": "SageThumbs2K-Setup-1.2.3.exe",
            "browser_download_url":
                "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v1.2.3/setup.exe",
            "size": 123
        }]
    });
    assert!(super::installer_asset_from_json(&base).is_none());

    let mut wrong_host = base;
    wrong_host["assets"][0]["digest"] = serde_json::json!(
        "sha256:09d79a0c6589d7dc5af5472cb8b1b56aac0dff51a47003b1146a9409f65c9835"
    );
    wrong_host["assets"][0]["browser_download_url"] =
        serde_json::json!("https://downloads.example.test/setup.exe");
    assert!(super::installer_asset_from_json(&wrong_host).is_none());
}

#[test]
fn launch_failures_stay_distinguishable() {
    use super::{
        classify_launch_failure as classify, launch_error, LaunchFailure as F, UpdateError as E,
    };

    // A declined UAC prompt: access-denied with ERROR_CANCELLED behind it. The ONLY
    // case the UI is allowed to swallow.
    assert_eq!(classify(5, 1223, false, false), F::Cancelled);
    assert!(matches!(launch_error(F::Cancelled, true), E::Cancelled));

    // The same prompt closed by a STANDARD account, which had no administrator to type in:
    // silence there left them clicking Update into the same dead end every day.
    assert_eq!(classify(5, 1223, false, true), F::NeedsAdmin);
    assert!(matches!(launch_error(F::NeedsAdmin, false), E::Blocked(_)));

    // Same access-denied return, but nothing cancelled — a policy or scanner refusal.
    // This used to be reported as "cancelled at the Windows permission prompt" and then
    // silently discarded, which is the bug: the user saw nothing at all.
    assert_eq!(classify(5, 0, false, false), F::Refused);
    assert!(matches!(launch_error(F::Refused, false), E::Blocked(_)));

    // The verified installer vanishing from %TEMP% between write and launch is a
    // quarantine, whatever ShellExecute claims (and whoever the user is).
    assert_eq!(classify(5, 1223, true, true), F::AvRemoved);
    assert_eq!(classify(2, 0, false, false), F::AvRemoved); // SE_ERR_FNF
    assert_eq!(classify(226, 226, false, false), F::AvRemoved); // ERROR_VIRUS_DELETED
    assert!(matches!(launch_error(F::AvRemoved, false), E::Blocked(_)));

    // A sharing violation is its own diagnosis now. It used to fall through to the
    // generic branch, which told the user to go find an administrator - for a file our
    // own write handle was holding shut.
    assert_eq!(classify(26, 0, false, false), F::Share);
    assert!(matches!(launch_error(F::Share, false), E::Blocked(_)));

    // Anything else is a plain failure, by its code - without guessing at permissions,
    // which is a cause the earlier branches already cover.
    assert_eq!(classify(31, 0, false, true), F::Other(31));
    assert!(matches!(launch_error(F::Other(31), false), E::Failed(_)));
}

#[test]
fn installer_file_stays_write_locked_until_launch() {
    let bytes = b"MZlocked-installer-test";
    let asset = super::InstallerAsset {
        url: String::new(),
        size: bytes.len() as u64,
        sha256: super::sha256_hex(bytes).expect("SHA-256"),
        sig_url: None,
    };
    let (path, lock) =
        super::write_locked_installer("test", bytes, &asset).expect("create locked installer");
    assert!(
        std::fs::OpenOptions::new().write(true).open(&path).is_err(),
        "a second writer must not be able to replace the verified installer"
    );
    assert!(
        std::fs::remove_file(&path).is_err(),
        "a deleter must not be able to remove the verified installer either"
    );
    drop(lock);
    std::fs::remove_file(path).expect("remove test installer");
}

/// The regression that shipped in 1.3.3 and broke one-click self-update for twenty
/// releases: the lock held across the launch carried WRITE access, and Windows will not
/// map an executable image whose file somebody else has open for writing. Every update
/// attempt died with `ERROR_SHARING_VIOLATION` -> `SE_ERR_SHARE` (26), reported to the
/// user as "installing an update needs an administrator".
///
/// Assert it against a REAL `CreateProcess` on a REAL image, because that is the only
/// thing that would have caught it - the unit test above passed happily throughout, since
/// denying writers was never the part that was broken.
#[test]
fn locked_installer_can_still_be_launched() {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let system_exe = Path::new(&root).join("System32").join("whoami.exe");
    let Ok(bytes) = std::fs::read(&system_exe) else {
        eprintln!("skipping: {} is not readable", system_exe.display());
        return;
    };
    let asset = super::InstallerAsset {
        url: String::new(),
        size: bytes.len() as u64,
        sha256: super::sha256_hex(&bytes).expect("SHA-256"),
        sig_url: None,
    };
    let (path, lock) =
        super::write_locked_installer("launch", &bytes, &asset).expect("create locked installer");

    // ShellExecuteW("runas") ultimately maps the image exactly as this does.
    let spawned = std::process::Command::new(&path)
        .creation_flags(st2k_base::host::CREATE_NO_WINDOW)
        .output();
    // ...and the lock is still doing its job while that happens.
    let writer_refused = std::fs::OpenOptions::new().write(true).open(&path).is_err();

    drop(lock);
    let _ = std::fs::remove_file(&path);

    assert!(
        spawned.is_ok(),
        "the lock held across the launch blocked the launch itself: {:?}",
        spawned.err()
    );
    assert!(writer_refused, "the lock stopped protecting the installer");
}

#[test]
fn selftest_refuses_a_non_executable_before_any_launch() {
    let path = std::env::temp_dir().join(format!("st2k-selftest-notpe-{}.bin", std::process::id()));
    std::fs::write(&path, b"definitely not a PE image").unwrap();
    // Fails at verification (no MZ), so nothing is ever handed to ShellExecuteW —
    // which is also what makes this safe to run un-elevated in any environment.
    assert_eq!(super::run_selftest(&path), super::SELFTEST_FAILED);
    std::fs::remove_file(path).unwrap();
}

/// Setup refuses to install over a file of ours that Windows' restart-time list still names
/// (Inno's "previous installation was not completed"), and under the updater's /SILENT that
/// refusal was a box nobody answered: CI's second held update waited on it for an hour. The
/// updater refuses first on exactly those names, and never on the parked-image deletions
/// nearly every update leaves, which would hold back every second update until a restart.
#[test]
fn the_restart_list_holds_back_an_update_only_where_setup_would_refuse() {
    let dir = r"C:\Program Files\SageThumbs2K\";
    let cases: &[(&str, bool)] = &[
        // A held file setup queued for the restart: its waiting copy, then its target.
        (
            r"*1\??\C:\Program Files\SageThumbs2K\is-NDHP5HTGSW.tmp",
            true,
        ),
        (r"*1!\??\C:\Program Files\SageThumbs2K\st2k.exe", true),
        (r"!\??\c:\program files\sagethumbs2k\SageThumbs2K.exe", true),
        // Anything under modules\, a parked one included: setup clears that whole folder.
        (
            r"*1\??\C:\Program Files\SageThumbs2K\modules\coders\IM_MOD_RL_png_.dll.old0",
            true,
        ),
        // Parked images at the top level, waiting for deletion: setup never touches them.
        (
            r"*1\??\C:\Program Files\SageThumbs2K\sagethumbs2k.dll.old0",
            false,
        ),
        (
            r"\??\C:\Program Files\SageThumbs2K\CORE_RL_MagickCore_.dll.old12",
            false,
        ),
        (
            r"\??\C:\Program Files\SageThumbs2K\SageThumbs2K.exe.old3",
            false,
        ),
        // Not a parked image: no number, or not an image.
        (
            r"\??\C:\Program Files\SageThumbs2K\sagethumbs2k.dll.old",
            true,
        ),
        (r"\??\C:\Program Files\SageThumbs2K\policy.xml.old0", true),
        // A delete's empty destination, and other programs' files.
        ("", false),
        (r"\??\C:\Program Files\SageThumbs2K Extras\x.dll", false),
        (r"*1!\??\C:\Windows\System32\drivers\x.sys", false),
    ];
    for &(entry, blocks) in cases {
        let list = [entry.to_string()];
        assert_eq!(
            !super::pending::blocking_names(&list, dir).is_empty(),
            blocks,
            "{entry:?}"
        );
    }
}

/// The app finds the installed copy through the uninstall key Inno names after the installer's
/// AppId. A new AppId with the old key here would switch the check above off without a sound.
#[test]
fn the_uninstall_key_is_the_installers_app_id() {
    let iss = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/packaging/installer.iss"),
    )
    .unwrap();
    let id = iss
        .lines()
        .find_map(|l| l.strip_prefix("AppId={"))
        .expect("installer.iss sets AppId");
    assert!(
        super::pending::UNINSTALL_KEY.ends_with(&format!(r"\{id}_is1")),
        "{} does not name AppId {id}",
        super::pending::UNINSTALL_KEY
    );
}
// ---- The updates-window offer decision -------------------------------------------

/// A `LicenceSnapshot` with nothing in it but the two fields `update_offer` reads. Built
/// by hand rather than through `license::at`, which would go to the registry, the
/// breadcrumb file and the credential store for facts this decision does not use.
fn snap_with_window(maint_unix: Option<u64>) -> crate::license::LicenceSnapshot {
    crate::license::LicenceSnapshot {
        mode: crate::license::Mode::Business,
        posture: crate::license::Posture::Silent,
        key_prefix: "esk_A1B2".into(),
        last_positive_unix: WINDOW_END - 1000,
        last_status: "active".into(),
        last_reason: String::new(),
        cert_expires_unix: None,
        maint_unix,
        now_unix: WINDOW_END + 1000,
        entitled: true,
    }
}

const WINDOW_END: u64 = 1_800_000_000;
const DAY: u64 = 24 * 60 * 60;

/// A personal-use / never-redeemed copy has no window at all, and must be offered every
/// build exactly as it always has been. This is also the state of every licensed machine
/// that has not yet heard a window from the relay, which is why "unknown" can never be
/// allowed to read as "closed".
#[test]
fn no_window_on_record_always_installs() {
    let snap = snap_with_window(None);
    assert_eq!(
        update_offer(&snap, Some(WINDOW_END + 10 * DAY), false),
        Offer::Install
    );
    assert_eq!(update_offer(&snap, None, false), Offer::Install);
    assert_eq!(
        update_offer(&snap, Some(WINDOW_END + 10 * DAY), true),
        Offer::Install
    );
}

/// Inside the window: an ordinary install, and the boundary is INCLUSIVE - a build
/// published at the very instant the window ends is one the customer paid for. Same
/// `<=` rule `licence_cert::verify` applies to `build_date <= maint`.
#[test]
fn a_build_inside_the_window_installs_and_the_boundary_is_inclusive() {
    let snap = snap_with_window(Some(WINDOW_END));
    assert_eq!(
        update_offer(&snap, Some(WINDOW_END - DAY), false),
        Offer::Install
    );
    assert_eq!(update_offer(&snap, Some(WINDOW_END), false), Offer::Install);
}

/// One second past the end is outside, and the decision names the date so the message
/// can say WHEN rather than just "no".
#[test]
fn a_build_published_after_the_window_offers_the_renewal() {
    let snap = snap_with_window(Some(WINDOW_END));
    assert_eq!(
        update_offer(&snap, Some(WINDOW_END + 1), false),
        Offer::OutsideWindow {
            ends_unix: WINDOW_END
        }
    );
}

/// ⛔ THE RULE THAT MUST SURVIVE EVERY FUTURE EDIT HERE: a security release is offered to
/// a licensed installation whatever its window says. A customer who has stopped paying
/// for new features has not stopped being someone we shipped software to.
#[test]
fn a_security_release_overrides_a_closed_window() {
    let snap = snap_with_window(Some(WINDOW_END));
    assert_eq!(
        update_offer(&snap, Some(WINDOW_END + 365 * DAY), true),
        Offer::Install
    );
}

/// No publication date (an un-redeployed Worker, a GitHub hiccup, a cache file written by
/// an older build): we cannot place the build against the window, so we do not pretend to.
#[test]
fn an_unknown_publication_date_installs() {
    let snap = snap_with_window(Some(WINDOW_END));
    assert_eq!(update_offer(&snap, None, false), Offer::Install);
}

/// `offer_for` is the only thing that can answer `Offer::None`, and it does so for exactly
/// one input: no release known.
#[test]
fn offer_for_answers_none_only_when_there_is_no_release() {
    let snap = snap_with_window(Some(WINDOW_END));
    assert_eq!(offer_for(&snap, None), Offer::None);
    let late = LatestRelease {
        tag: "9.9.9".into(),
        published_unix: Some(WINDOW_END + DAY),
        security: false,
    };
    assert_eq!(
        offer_for(&snap, Some(&late)),
        Offer::OutsideWindow {
            ends_unix: WINDOW_END
        }
    );
}

/// The security marker is matched as plain text, case-insensitively, anywhere in the
/// notes - so release notes may format it however they like - and nothing else trips it.
#[test]
fn the_security_marker_is_recognised_only_as_itself() {
    assert!(is_security_body(
        "## Fixes\n\n[security-release] CVE-2026-1 in the SVG path."
    ));
    assert!(is_security_body("**[SECURITY-RELEASE]**"));
    assert!(!is_security_body("A security fix, but nobody marked it."));
    assert!(
        !is_security_body("security-release"),
        "the brackets are the marker"
    );
    assert!(!is_security_body(""));
}

/// The cache file gained two lines on 2026-09-10. A two-line file written by an older
/// build must still parse, into the "offer it to everyone" shape - upgrading must never
/// briefly refuse a build over a date the new code simply has not fetched yet.
#[test]
fn an_old_two_line_cache_still_parses_as_a_dateless_release() {
    let (secs, latest) = parse_cache("1700000000\n3.0.1\n").expect("two-line cache");
    assert_eq!(secs, 1_700_000_000);
    assert_eq!(latest, LatestRelease::bare("3.0.1".into()));

    let (_, latest) = parse_cache("1700000000\n3.0.2\n1800000000\n1\n").expect("four-line");
    assert_eq!(
        latest,
        LatestRelease {
            tag: "3.0.2".into(),
            published_unix: Some(1_800_000_000),
            security: true,
        }
    );

    // `0` on line 3 is the on-disk spelling of "not known", never 1970.
    let (_, latest) = parse_cache("1700000000\n3.0.3\n0\n0\n").expect("zeroed");
    assert_eq!(latest.published_unix, None);
    assert!(!latest.security);

    assert!(parse_cache("").is_none());
    assert!(
        parse_cache("1700000000\n\n").is_none(),
        "an empty tag is no answer"
    );
    assert!(parse_cache("not-a-number\n3.0.1\n").is_none());
}
