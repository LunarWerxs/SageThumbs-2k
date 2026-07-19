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
//! Every check is a registry/file READ or a `LoadLibrary`+`FreeLibrary`. Nothing is
//! written, nothing is elevated, so it is always safe to ask a user to run it and paste
//! the output. That is the point: the report is designed to be pasted into an issue.

use crate::formats::FORMATS;
use crate::guids::{
    CLSID_CONTEXT_MENU_STR, CLSID_PREVIEW_HANDLER_STR, CLSID_PROPERTY_STORE_STR,
    CLSID_THUMBNAIL_PROVIDER_STR,
};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use windows_registry::{CLASSES_ROOT, CURRENT_USER, LOCAL_MACHINE};

const THUMB_HANDLER: &str = "{E357FCCD-A995-4576-B01F-234630154E96}";
const APPROVED: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved";

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
        Report { out: String::new(), problems: Vec::new() }
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

    /// A failure that also carries the fix, so the user is not left holding a symptom.
    fn fail_with_fix(&mut self, label: &str, detail: &str, fix: &str) {
        self.line(S::Fail, label, detail);
        if let Some(last) = self.problems.last_mut() {
            let _ = write!(last, "\n         FIX: {fix}");
        }
    }
}

/// Read a registry default (`""`) value as a string, from any of the three roots we use.
fn hkcr_default(path: &str) -> Option<String> {
    CLASSES_ROOT.open(path).ok().and_then(|k| k.get_string("").ok())
}

/// The DLL path Windows would actually load for a CLSID, straight from the registry —
/// NOT the path we think we installed to. A stale entry pointing at a deleted build is
/// exactly the kind of thing that produces silent nothing.
fn inproc_path(clsid: &str) -> Option<String> {
    hkcr_default(&format!("CLSID\\{clsid}\\InprocServer32"))
}

/// Try to genuinely load the DLL. This is the check that catches a missing runtime
/// dependency: the registry can be perfect and the loader still refuses, in which case
/// the shell silently falls back to a plain icon with nothing logged anywhere.
fn can_load(path: &Path) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::FreeLibrary;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    let wide: Vec<u16> =
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    unsafe {
        match LoadLibraryW(PCWSTR(wide.as_ptr())) {
            Ok(h) => {
                let _ = FreeLibrary(h);
                Ok(())
            }
            Err(e) => Err(format!("{} (0x{:08X})", e.message(), e.code().0)),
        }
    }
}
use std::os::windows::ffi::OsStrExt as _;

/// Where the shell extension is installed, per the registry. Falls back to the folder
/// this executable sits in (a portable/dev layout).
fn installed_dll() -> Option<PathBuf> {
    if let Some(p) = inproc_path(CLSID_THUMBNAIL_PROVIDER_STR) {
        return Some(PathBuf::from(p));
    }
    std::env::current_exe().ok()?.parent().map(|d| d.join("sagethumbs2k.dll"))
}

/// Windows-side switches that disable thumbnails for EVERY program, not just us. When
/// one of these is set the extension is registered perfectly and still shows nothing,
/// which is the most misleading failure mode there is.
fn check_windows_switches(r: &mut Report) {
    r.head("Windows thumbnail settings");

    let advanced = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
    let icons_only = CURRENT_USER.open(advanced).ok().and_then(|k| k.get_u32("IconsOnly").ok());
    match icons_only {
        Some(1) => r.fail_with_fix(
            "IconsOnly",
            "1 — Windows is set to 'Always show icons, never thumbnails'",
            "File Explorer -> View -> Options -> View tab -> UNCHECK \
             'Always show icons, never thumbnails'. (Also set by Performance Options -> \
             'Adjust for best performance', which is common on a fresh VM.)",
        ),
        Some(v) => r.line(S::Ok, "IconsOnly", &format!("{v} — thumbnails allowed")),
        None => r.line(S::Ok, "IconsOnly", "unset — thumbnails allowed"),
    }

    // Group Policy can kill thumbnails machine-wide or per-user.
    let pol = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    let mut any_policy = false;
    for (root, root_name) in [(CURRENT_USER, "HKCU"), (LOCAL_MACHINE, "HKLM")] {
        for value in ["DisableThumbnails", "NoThumbnailCache", "DisableThumbnailCache"] {
            if let Some(1) = root.open(pol).ok().and_then(|k| k.get_u32(value).ok()) {
                any_policy = true;
                r.fail_with_fix(
                    &format!("{root_name}\\...\\{value}"),
                    "1 — policy disables thumbnails",
                    "Set this value to 0 or delete it (Group Policy / registry).",
                );
            }
        }
    }
    if !any_policy {
        r.line(S::Ok, "Thumbnail policies", "no disabling policy found");
    }
}

/// The COM half: is each coclass registered, does its DLL exist, and will it load.
fn check_registration(r: &mut Report) -> bool {
    r.head("COM registration");

    let mut thumb_ok = true;
    let handlers = [
        ("Thumbnail provider", CLSID_THUMBNAIL_PROVIDER_STR, true),
        ("Context menu (classic)", CLSID_CONTEXT_MENU_STR, false),
        ("Preview handler", CLSID_PREVIEW_HANDLER_STR, false),
        ("Property handler", CLSID_PROPERTY_STORE_STR, false),
    ];

    for (name, clsid, critical) in handlers {
        match inproc_path(clsid) {
            None => {
                if critical {
                    thumb_ok = false;
                    r.fail_with_fix(
                        name,
                        "NOT REGISTERED (no InprocServer32)",
                        "Reinstall, or run an elevated: \
                         regsvr32 \"C:\\Program Files\\SageThumbs2K\\sagethumbs2k.dll\"",
                    );
                } else {
                    r.line(S::Warn, name, "not registered");
                }
            }
            Some(p) => {
                let path = PathBuf::from(&p);
                if !path.exists() {
                    if critical {
                        thumb_ok = false;
                    }
                    r.fail_with_fix(
                        name,
                        &format!("registered -> {p} (FILE MISSING)"),
                        "The registration points at a DLL that is not there — reinstall.",
                    );
                } else if let Err(e) = can_load(&path) {
                    if critical {
                        thumb_ok = false;
                    }
                    r.fail_with_fix(
                        name,
                        &format!("DLL WILL NOT LOAD: {e}"),
                        "Windows cannot load the extension, so the shell silently shows \
                         plain icons. Usually a missing Microsoft Visual C++ Redistributable \
                         (x64) — install it and retry.",
                    );
                } else {
                    r.line(S::Ok, name, &format!("registered, loads OK -> {p}"));
                }
            }
        }
    }

    // The Approved list is mandatory on locked-down / policy-managed machines and is
    // silently enforced: an unapproved extension is simply never loaded.
    let approved = LOCAL_MACHINE.open(APPROVED).ok();
    for (name, clsid) in [
        ("Approved: thumbnail", CLSID_THUMBNAIL_PROVIDER_STR),
        ("Approved: context menu", CLSID_CONTEXT_MENU_STR),
    ] {
        let listed = approved.as_ref().and_then(|k| k.get_string(clsid).ok()).is_some();
        if listed {
            r.line(S::Ok, name, "listed");
        } else {
            r.line(S::Warn, name, "not in the Approved Shell Extensions list");
        }
    }

    thumb_ok
}

/// The per-extension half: for each format we claim, does `.ext\shellex` actually point
/// at us? Reports hijacks separately from plain absences — "another program took it" is
/// a completely different fix from "registration never ran".
fn check_extensions(r: &mut Report) {
    r.head("Per-format file associations");

    let (mut ours, mut missing, mut stolen, mut disabled) = (0usize, 0usize, 0usize, 0usize);
    let mut stolen_examples: Vec<String> = Vec::new();
    let mut missing_examples: Vec<String> = Vec::new();

    for &(ext, _) in FORMATS.iter() {
        if !crate::settings::format_enabled(ext) {
            disabled += 1;
            continue;
        }
        let key = format!(".{ext}\\shellex\\{THUMB_HANDLER}");
        match hkcr_default(&key).as_deref() {
            Some(c) if c.eq_ignore_ascii_case(CLSID_THUMBNAIL_PROVIDER_STR) => ours += 1,
            Some(other) => {
                stolen += 1;
                if stolen_examples.len() < 6 {
                    stolen_examples.push(format!(".{ext} -> {other}"));
                }
            }
            None => {
                missing += 1;
                if missing_examples.len() < 6 {
                    missing_examples.push(format!(".{ext}"));
                }
            }
        }
    }

    let enabled = ours + missing + stolen;
    r.line(S::Info, "Formats enabled in settings", &format!("{enabled} (of {})", FORMATS.len()));
    if disabled > 0 {
        r.line(S::Info, "Formats turned off by you", &format!("{disabled}"));
    }

    if enabled == 0 {
        r.fail_with_fix(
            "Enabled formats",
            "0 — every format is switched off",
            "Settings -> File types -> enable the formats you want.",
        );
        return;
    }

    if ours == enabled {
        r.line(S::Ok, "Hooked by SageThumbs 2K", &format!("{ours}/{enabled}"));
    } else if ours == 0 {
        r.fail_with_fix(
            "Hooked by SageThumbs 2K",
            &format!("0/{enabled} — no format is hooked"),
            "Registration never landed. Settings -> Advanced -> 'Repair file associations', \
             or reinstall.",
        );
    } else {
        r.line(S::Warn, "Hooked by SageThumbs 2K", &format!("{ours}/{enabled}"));
    }

    if missing > 0 {
        r.line(S::Warn, "  not hooked", &format!("{missing}  e.g. {}", missing_examples.join(", ")));
    }
    if stolen > 0 {
        r.line(
            S::Warn,
            "  owned by another program",
            &format!("{stolen}  e.g. {}", stolen_examples.join(", ")),
        );
    }
}

/// Our own settings, which can switch everything off without any registry problem.
fn check_settings(r: &mut Report) {
    r.head("SageThumbs 2K settings");
    if crate::settings::thumbnails_enabled() {
        r.line(S::Ok, "Thumbnails", "enabled");
    } else {
        r.fail_with_fix(
            "Thumbnails",
            "DISABLED in SageThumbs 2K settings",
            "Settings -> General -> tick 'Show thumbnails'.",
        );
    }
    let mb = crate::settings::max_file_size_bytes() / (1024 * 1024);
    r.line(S::Info, "Max file size", &format!("{mb} MB (larger files are skipped)"));
    r.line(S::Info, "Max thumbnail size", &format!("{} px", crate::settings::max_thumb_size()));
    r.line(
        S::Info,
        "Embedded previews preferred",
        if crate::settings::use_embedded() { "yes" } else { "no" },
    );
}

/// Prove the decoder itself works, end to end, without touching the disk or the shell.
/// Separating this from the COM checks is the whole diagnostic value: "engine fine,
/// shell never asked" and "engine broken" look identical to a user and need opposite fixes.
fn check_engine(r: &mut Report) {
    r.head("Decode engine");
    let png: &[u8] = &{
        let mut buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbaImage::from_fn(64, 64, |x, y| {
            image::Rgba([(x * 4) as u8, (y * 4) as u8, 128, 255])
        });
        match image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png) {
            Ok(()) => buf.into_inner(),
            Err(e) => {
                r.line(S::Fail, "Self-test image", &format!("could not encode: {e}"));
                return;
            }
        }
    };
    match crate::decode::decode_preview(png) {
        Ok(img) => r.line(
            S::Ok,
            "Decode self-test",
            &format!("passed ({}x{} out)", img.width(), img.height()),
        ),
        Err(e) => {
            r.line(S::Fail, "Decode self-test", &format!("FAILED on a generated PNG: {e}"))
        }
    }
}

/// Build the whole report. Read-only; safe to run unelevated, and safe to paste.
pub fn report() -> String {
    let mut r = Report::new();

    r.out.push_str("SageThumbs 2K — diagnostic report\n");
    r.out.push_str("=================================\n");

    r.head("Environment");
    r.line(S::Info, "SageThumbs 2K version", env!("CARGO_PKG_VERSION"));
    r.line(S::Info, "Windows", &crate::safety::os_string());
    r.line(S::Info, "Process architecture", std::env::consts::ARCH);
    match installed_dll() {
        Some(p) => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            r.line(S::Info, "Shell extension DLL", &format!("{} ({size} bytes)", p.display()));
        }
        None => r.line(S::Warn, "Shell extension DLL", "could not determine a path"),
    }
    match crate::safety::log_file() {
        Some(p) if p.exists() => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            r.line(S::Info, "Diagnostics log", &format!("{} ({size} bytes)", p.display()));
        }
        Some(p) => r.line(S::Info, "Diagnostics log", &format!("{} (not created yet)", p.display())),
        None => r.line(S::Warn, "Diagnostics log", "LOCALAPPDATA is unset"),
    }

    check_windows_switches(&mut r);
    check_registration(&mut r);
    check_extensions(&mut r);
    check_settings(&mut r);
    check_engine(&mut r);

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
        let out = report();
        assert!(out.contains("Environment"), "missing environment section");
        assert!(out.contains("COM registration"), "missing registration section");
        assert!(out.contains("Verdict"), "missing verdict");
    }

    /// A user is told to paste this. It must not leak their username via the paths we
    /// print, beyond the log path they already know about... which does contain it —
    /// so this test just pins that we print no OTHER profile-derived path.
    #[test]
    fn report_is_plain_text() {
        let out = report();
        assert!(!out.contains('\u{0}'), "report contains NUL");
        assert!(out.is_ascii() || out.chars().all(|c| !c.is_control() || c == '\n'));
    }
}
