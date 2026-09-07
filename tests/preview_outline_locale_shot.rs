//! Audit F29 (2026-09-06): the Quick preview's Markdown outline header must follow the active
//! locale instead of being hardcoded to the literal "CONTENTS".
//!
//! Drives the real headless `--shot --window preview` harness (the same one
//! `tests/preview_markdown_shot.rs` uses) twice on the SAME Markdown file: once under the
//! default (English) locale, once with the language forced to French via a scratch HKCU key
//! redirected through `ST2K_SETTINGS_ROOT` (see `settings::hkcu_root`), so the developer's real
//! `HKCU\Software\SageThumbs2K\Lang` is never touched. The two renders must differ — proving the
//! header text (and everything else this finding covers) actually changes with the locale
//! rather than being baked in.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use windows_registry::CURRENT_USER;

/// Render `md` through the headless preview harness, optionally forcing the UI language via a
/// throwaway HKCU subkey. Returns the child's exit status and the PNG path.
fn shot_markdown_with_lang(
    case: &str,
    md: &str,
    lang: Option<&str>,
) -> (std::process::ExitStatus, PathBuf) {
    let dir = std::env::temp_dir().join(format!("st2k_f29_outline_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let doc = dir.join("doc.md");
    let out = dir.join("out.png");
    std::fs::write(&doc, md).expect("write markdown");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"));
    cmd.arg("--shot")
        .arg(&out)
        .args(["--window", "preview", "--file"])
        .arg(&doc)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Some(code) = lang {
        // A scratch HKCU subkey rather than the developer's real `Software\SageThumbs2K\Lang` —
        // see settings::hkcu_root's ST2K_SETTINGS_ROOT redirect. Unique per case + pid so
        // parallel test runs (and repeated local runs) never collide.
        let root = format!(
            r"Software\SageThumbs2K\__test_f29_lang_{}_{case}",
            std::process::id()
        );
        CURRENT_USER
            .create(&root)
            .expect("create scratch HKCU key")
            .set_string("Lang", code)
            .expect("set Lang");
        cmd.env("ST2K_SETTINGS_ROOT", &root);
    }

    let status = cmd.status().expect("spawn SageThumbs2K --shot");
    (status, out)
}

fn cleanup(out: &Path, lang_case: Option<&str>) {
    if let Some(case) = lang_case {
        let root = format!(
            r"Software\SageThumbs2K\__test_f29_lang_{}_{case}",
            std::process::id()
        );
        let _ = CURRENT_USER.remove_tree(&root);
    }
    if let Some(dir) = out.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn outline_header_follows_the_active_locale_not_a_hardcoded_english_literal() {
    let md = "# Heading one\n\nbody text here\n\n## Heading two\n\nmore body text\n";

    let (status_en, out_en) = shot_markdown_with_lang("en", md, None);
    assert!(
        status_en.success(),
        "English shot failed: exit {:?}",
        status_en.code()
    );
    assert!(out_en.is_file(), "English shot produced no PNG");

    let (status_fr, out_fr) = shot_markdown_with_lang("fr", md, Some("fr"));
    assert!(
        status_fr.success(),
        "French shot failed: exit {:?}",
        status_fr.code()
    );
    assert!(out_fr.is_file(), "French shot produced no PNG");

    let en_bytes = std::fs::read(&out_en).expect("read English PNG");
    let fr_bytes = std::fs::read(&out_fr).expect("read French PNG");

    assert_ne!(
        en_bytes, fr_bytes,
        "the preview render is byte-identical between English and French — the outline header \
         (\"CONTENTS\" vs its French translation) is still hardcoded and ignores the active \
         locale. Manually confirmed 2026-09-06: the English render shows \"CONTENTS\", the \
         French render shows \"SOMMAIRE\"."
    );

    cleanup(&out_en, None);
    cleanup(&out_fr, Some("fr"));
}
