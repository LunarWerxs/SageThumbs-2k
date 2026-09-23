//! `st2k doctor` — a read-only self-check that answers "why do I have no thumbnails?"
//!
//! Existing diagnostics only prove the DECODER works (`st2k thumbnail` never touches
//! COM). But every "not working at all" report so far has been about the shell never
//! *asking* us in the first place — a registration that didn't land, a DLL the loader
//! can't load, or a Windows-side switch that turns thumbnails off globally. None of
//! that was observable from outside, so triage was guesswork.
//!
//! This walks the whole chain a thumbnail actually travels:
//!
//! ```text
//!   Explorer wants a thumbnail for  foo.psd
//!     -> is thumbnailing even ON in Windows?        (IconsOnly / policy)
//!     -> HKCR\.psd\shellex\{E357FCCD…}              -> our CLSID?
//!     -> HKCR\CLSID\{7B2E6A14…}\InprocServer32      -> a path that exists?
//!     -> can the loader actually LOAD that DLL?     (missing runtime => silent nothing)
//!     -> is the CLSID in the Approved list?         (mandatory on locked-down boxes)
//!     -> is the format enabled in OUR settings?
//! ```
//!
//! Most checks are a registry/file READ or a `LoadLibrary`+`FreeLibrary`, but not all:
//! `check_engine` decodes a PNG in memory, `check_space_preview` enumerates windows,
//! `check_format_capability` probes OS codecs, and the per-file probe can fill Explorer's
//! thumbnail cache. Nothing is elevated, so it is always safe to ask a user to run it and
//! paste the output. That is the point: the report is designed to be pasted into an issue.

use crate::formats::FORMATS;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use windows_registry::{CLASSES_ROOT, CURRENT_USER, LOCAL_MACHINE};

/// One line of the report. `Fail` means "this alone explains no thumbnails".
#[derive(PartialEq, Clone, Copy)]
enum S {
    Ok,
    Warn,
    Fail,
    Info,
}

impl S {
    fn tag(self) -> &'static str {
        match self {
            S::Ok => "[ ok ]",
            S::Warn => "[warn]",
            S::Fail => "[FAIL]",
            S::Info => "[    ]",
        }
    }
}

/// Accumulates report lines and remembers the failures so we can end with a verdict
/// instead of making the reader diff a wall of text.
struct Report {
    out: String,
    problems: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Report {
            out: String::new(),
            problems: Vec::new(),
        }
    }

    fn head(&mut self, title: &str) {
        let _ = write!(self.out, "\n{title}\n{}\n", "-".repeat(title.len()));
    }

    fn line(&mut self, s: S, label: &str, detail: &str) {
        let _ = writeln!(self.out, "{} {label:<34} {detail}", s.tag());
        if s == S::Fail {
            self.problems.push(format!("{label}: {detail}"));
        }
    }

    /// An info line for a path, with the byte size of the file it points at (0 when the
    /// metadata cannot be read).
    fn line_with_size(&mut self, label: &str, p: &Path) {
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        self.line(S::Info, label, &format!("{} ({size} bytes)", p.display()));
    }

    /// A failure that also carries the fix, so the user is not left holding a symptom.
    fn fail_with_fix(&mut self, label: &str, detail: &str, fix: &str) {
        self.line(S::Fail, label, detail);
        if let Some(last) = self.problems.last_mut() {
            let _ = write!(last, "\n         FIX: {fix}");
        }
    }
}

mod filecheck;
mod registration;
mod settingscheck;
mod winchecks;

use filecheck::*;
use registration::*;
use settingscheck::*;
use winchecks::*;

pub use winchecks::served_window_kind;

/// `st2k doctor --bundle <out.zip>`: the report, the log's tail, `formats --json` and the
/// stored settings in one file. `docs/FAQ.md` tells a confused user to run `st2k doctor`
/// and paste its output, but the crash/panic log lives at a separate path found via Settings
/// -> Advanced -> "Open diagnostics log", and nothing bundled the two (plus the format list,
/// for "which formats does this build even enable") into one attachment a support triage or
/// a Send-feedback box could accept as-is.
///
/// The settings entry carries preferences only, never the sign-in state (2026-09-05 audit,
/// E01): see [`without_credentials`]. A bundle is made to be handed to a stranger, and the
/// refresh token in it would be a stranger's way into the account.
pub fn bundle(out: &Path, file: Option<&str>) -> Result<(), String> {
    use std::io::Write;

    let report_text = report(file);
    let formats_json = crate::cli::list_formats(true);
    let settings_text = settings_snapshot();
    let log_tail = match crate::safety::log_file() {
        Some(p) if p.exists() => read_log_tail(&p, LOG_TAIL_SCAN_BYTES),
        Some(_) => "(no diagnostics log yet)".to_string(),
        None => "(LOCALAPPDATA is unset — no diagnostics log path)".to_string(),
    };

    let f =
        std::fs::File::create(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let write_entry =
        |zw: &mut zip::ZipWriter<std::fs::File>, name: &str, bytes: &[u8]| -> Result<(), String> {
            zw.start_file(name, opts)
                .map_err(|e| format!("zip: {name}: {e}"))?;
            zw.write_all(bytes).map_err(|e| format!("zip: {name}: {e}"))
        };
    write_entry(&mut zw, "doctor-report.txt", report_text.as_bytes())?;
    write_entry(&mut zw, "formats.json", formats_json.as_bytes())?;
    write_entry(&mut zw, "log-tail.txt", log_tail.as_bytes())?;
    write_entry(&mut zw, "settings.txt", settings_text.as_bytes())?;
    zw.finish().map_err(|e| format!("zip: {e}"))?;
    Ok(())
}

pub fn report(file: Option<&str>) -> String {
    let mut r = Report::new();

    r.out.push_str("SageThumbs 2K — diagnostic report\n");
    r.out.push_str("=================================\n");

    r.head("Environment");
    r.line(S::Info, "SageThumbs 2K version", env!("CARGO_PKG_VERSION"));
    r.line(S::Info, "Windows", &crate::safety::os_string());
    r.line(S::Info, "Process architecture", std::env::consts::ARCH);
    if crate::prebuild::is_elevated() {
        // Every HKCU check below reads THIS process's hive. Elevated, that is the
        // administrator's hive, not the interactive user's — a clean HKCU check here can
        // still misreport the user's own session as unregistered.
        r.line(
            S::Warn,
            "Elevated",
            "yes — HKCU checks below inspect the administrator's hive, which may not \
             match the interactive user's session. Re-run un-elevated for a per-user check.",
        );
    }
    match installed_dll() {
        Some(p) => r.line_with_size("Shell extension DLL", &p),
        None => r.line(S::Warn, "Shell extension DLL", "could not determine a path"),
    }
    match crate::safety::log_file() {
        Some(p) if p.exists() => {
            r.line_with_size("Diagnostics log", &p);
            append_log_tail(&mut r, &p);
        }
        Some(p) => r.line(
            S::Info,
            "Diagnostics log",
            &format!("{} (not created yet)", p.display()),
        ),
        None => r.line(S::Warn, "Diagnostics log", "LOCALAPPDATA is unset"),
    }

    // One snapshot for the whole report, instead of `check_extensions`'s ~330-format
    // sweep (and, now, `check_progid_handlers`'s matching sweep) each re-reading and
    // re-parsing the whole portable ini once per format.
    let snap = crate::settings::format_enabled_snapshot();

    check_windows_switches(&mut r);
    check_registration(&mut r);
    check_extensions(&mut r, &snap);
    check_progid_handlers(&mut r, &snap);
    check_displaced(&mut r);
    check_settings(&mut r);
    check_licence(&mut r);
    check_space_preview(&mut r);
    check_engine(&mut r);
    check_format_capability(&mut r);
    if let Some(f) = file {
        probe_file(&mut r, f, &snap);
    }

    r.head("Verdict");
    if r.problems.is_empty() {
        r.out.push_str(
            "No blocking problem found.\n\n\
             If thumbnails are still missing, Explorer is probably serving a cached icon:\n\
             Settings -> Advanced -> 'Rebuild thumbnail cache', then look again.\n",
        );
    } else {
        let n = r.problems.len();
        let _ = writeln!(r.out, "{n} problem(s) found:\n");
        // `problems` was built during the checks above, so this is just a replay.
        let listed = r.problems.clone();
        for (i, p) in listed.iter().enumerate() {
            let _ = writeln!(r.out, "  {}. {p}\n", i + 1);
        }
    }
    r.out.push_str(
        "\nPaste this whole report into a GitHub issue:\n\
         https://github.com/LunarWerxs/SageThumbs-2k/issues\n",
    );
    r.out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report must never panic and must always reach the verdict, whatever state
    /// the machine is in — it is the thing we ask users to run when everything is broken.
    #[test]
    fn report_runs_and_reaches_a_verdict() {
        let out = report(None);
        assert!(out.contains("Environment"), "missing environment section");
        assert!(
            out.contains("COM registration"),
            "missing registration section"
        );
        assert!(out.contains("Verdict"), "missing verdict");
    }

    /// A user is told to paste this. It must never carry NUL or other control characters,
    /// which would corrupt the pasted report, so this test just pins that the text is
    /// print-safe.
    #[test]
    fn report_is_plain_text() {
        let out = report(None);
        assert!(!out.contains('\u{0}'), "report contains NUL");
        assert!(
            out.chars().all(|c| !c.is_control() || c == '\n'),
            "report contains a control character"
        );
    }

    /// The per-file probe must run and reach a verdict for any path, including a
    /// nonexistent one and an unsupported extension — it's a diagnostic, never a crash.
    #[test]
    fn report_with_file_probes_and_never_panics() {
        let missing = report(Some("Z:\\does\\not\\exist.xcf"));
        assert!(missing.contains("This file"), "missing per-file section");
        assert!(missing.contains("Verdict"), "missing verdict");
        // An unsupported extension is reported as the whole answer, not a decode attempt.
        let unsupported = report(Some("C:\\nope.zzzznotaformat"));
        assert!(unsupported.contains("This file"));
    }
}
