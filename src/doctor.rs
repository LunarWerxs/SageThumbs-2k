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
    CLASSES_ROOT
        .open(path)
        .ok()
        .and_then(|k| k.get_string("").ok())
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
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
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
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|d| d.join("sagethumbs2k.dll"))
}

/// Windows-side switches that disable thumbnails for EVERY program, not just us. When
/// one of these is set the extension is registered perfectly and still shows nothing,
/// which is the most misleading failure mode there is.
fn check_windows_switches(r: &mut Report) {
    r.head("Windows thumbnail settings");

    let advanced = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
    let icons_only = CURRENT_USER
        .open(advanced)
        .ok()
        .and_then(|k| k.get_u32("IconsOnly").ok());
    match icons_only {
        Some(1) => r.fail_with_fix(
            "IconsOnly",
            "1 — Windows is set to 'Always show icons, never thumbnails'",
            "UNCHECK it in the real dialog: File Explorer -> ... -> Options -> View tab -> \
             'Always show icons, never thumbnails'. Use the CHECKBOX, not the registry — \
             Explorer keeps its own copy of this value and writes it back over yours when it \
             exits, so a registry edit + restart silently reverts. Afterwards clear the \
             thumbnail cache (Disk Cleanup, or delete \
             %LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\thumbcache_*.db with Explorer \
             closed): while the switch was on, Explorer recorded 'no thumbnail' for every \
             file it saw and keeps serving those stale answers.",
        ),
        Some(v) => r.line(S::Ok, "IconsOnly", &format!("{v} — thumbnails allowed")),
        None => r.line(S::Ok, "IconsOnly", "unset — thumbnails allowed"),
    }

    // Performance Options -> "Adjust for best performance" switches OFF the "Show thumbnails
    // instead of icons" visual effect, which IS IconsOnly. Worth its own check because of how
    // it presents: the profile re-applies its own value, so IconsOnly can READ 0 while Explorer
    // keeps behaving as though it were 1, and every registry-level check says everything is
    // fine. That contradiction cost hours on a real machine (2026-08-05).
    //
    // Keyed ONLY on VisualFXSetting == 2. The per-effect `ThumbnailsOrIcon\DefaultApplied`
    // reads 1 on perfectly healthy machines (it means "this effect is at its profile default",
    // not "off"), so reporting on it would fail every install that works — the same false-alarm
    // mistake the DisableThumbnailCache note above records from issue #11.
    let visual_fx = CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Explorer\VisualEffects")
        .ok()
        .and_then(|k| k.get_u32("VisualFXSetting").ok());
    if let Some(detail) = performance_profile_detail(visual_fx, icons_only) {
        r.fail_with_fix(
            "Performance profile",
            detail,
            "System Properties -> Advanced -> Performance -> Settings -> pick 'Custom' (or \
             'Adjust for best appearance') and TICK 'Show thumbnails instead of icons', then \
             Apply. Fixing IconsOnly alone will not hold while this profile is set.",
        );
    }

    check_thumbnail_policies(r);
}

/// The verdict on the performance profile, split out so it can be tested against every
/// combination without a live registry (and without setting "best performance" on a real
/// machine to see what happens, which would switch that machine's own thumbnails off).
///
/// `None` means say nothing. Only `VisualFXSetting == 2` is worth reporting; see the call
/// site for why the per-effect `DefaultApplied` value must NOT be used for this.
fn performance_profile_detail(
    visual_fx: Option<u32>,
    icons_only: Option<u32>,
) -> Option<&'static str> {
    if visual_fx != Some(2) {
        return None;
    }
    Some(match icons_only {
        // The nasty shape, and the reason this check exists: the switch reads as allowed, so
        // every registry-level check passes, while the profile keeps turning it back off.
        Some(0) | None => {
            "2 — 'Adjust for best performance' is on. It owns the thumbnail switch and will \
             keep turning it back off, even though IconsOnly currently reads as allowed"
        }
        _ => "2 — 'Adjust for best performance' is on, which is what turned thumbnails off",
    })
}

/// Group Policy's thumbnail switches, split from the per-user ones above purely for length.
fn check_thumbnail_policies(r: &mut Report) {
    // Group Policy can kill thumbnails machine-wide or per-user. Only `DisableThumbnails`
    // actually does that; the two *Cache* values disable the on-disk thumbnail CACHE
    // (thumbcache_*.db) and nothing else — thumbnails still generate, they are just
    // recomputed every time. Reporting those as "thumbnails are disabled" sent a reporter
    // (issue #11) chasing four scary FAILs on an install whose thumbnails worked fine.
    let pol = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    let mut any_policy = false;
    let mut cache_off = false;
    for (root, root_name) in [(CURRENT_USER, "HKCU"), (LOCAL_MACHINE, "HKLM")] {
        if let Some(1) = root
            .open(pol)
            .ok()
            .and_then(|k| k.get_u32("DisableThumbnails").ok())
        {
            any_policy = true;
            r.fail_with_fix(
                &format!("{root_name}\\...\\DisableThumbnails"),
                "1 — policy disables thumbnails",
                "Set this value to 0 or delete it (Group Policy / registry).",
            );
        }
        for value in ["NoThumbnailCache", "DisableThumbnailCache"] {
            if let Some(1) = root.open(pol).ok().and_then(|k| k.get_u32(value).ok()) {
                cache_off = true;
                r.line(
                    S::Info,
                    &format!("{root_name}\\...\\{value}"),
                    "1 — thumbnail CACHE off (thumbnails still work, just slower)",
                );
            }
        }
    }
    if cache_off {
        r.line(
            S::Info,
            "Thumbnail cache",
            "disabled by policy — every thumbnail is recomputed on each visit",
        );
    }
    if !any_policy {
        r.line(S::Ok, "Thumbnail policies", "no disabling policy found");
    }
}

/// The decade-old original SageThumbs, if it is still on disk.
///
/// Inert by itself — nothing in the registry points at it once we are installed — so this is
/// NOT reported as a failure. It is reported because of a specific footgun: running that
/// install's `unins000.exe` unregisters the shell-extension entries by CLSID and file
/// association, and the overlap with ours is enough to leave a working SageThumbs 2K with no
/// thumbnails and no obvious reason why. Someone tidying up their Program Files would have no
/// way to know that, which is exactly when they would run it.
fn check_legacy_install(r: &mut Report) {
    let legacy: Vec<PathBuf> = ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|base| Path::new(&base).join("SageThumbs"))
        .filter(|p| p.is_dir())
        .collect();
    for dir in legacy {
        let uninstaller = dir.join("unins000.exe");
        if uninstaller.is_file() {
            r.line(
                S::Warn,
                "Old SageThumbs install",
                &format!(
                    "{} — harmless where it sits, but do NOT run its unins000.exe",
                    dir.display()
                ),
            );
            r.line(
                S::Info,
                "  why",
                "that uninstaller strips shell-extension registrations by CLSID and file type, \
                 and would take SageThumbs 2K's with it. Delete the FOLDER instead if you want \
                 it gone.",
            );
        } else {
            r.line(
                S::Info,
                "Old SageThumbs install",
                &format!("{} — leftover files, no uninstaller, inert", dir.display()),
            );
        }
    }
}

/// The COM half: is each coclass registered, does its DLL exist, and will it load.
fn check_registration(r: &mut Report) -> bool {
    r.head("COM registration");
    check_legacy_install(r);

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
        let listed = approved
            .as_ref()
            .and_then(|k| k.get_string(clsid).ok())
            .is_some();
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
    r.line(
        S::Info,
        "Formats enabled in settings",
        &format!("{enabled} (of {})", FORMATS.len()),
    );
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
        r.line(
            S::Ok,
            "Hooked by SageThumbs 2K",
            &format!("{ours}/{enabled}"),
        );
    } else if ours == 0 {
        r.fail_with_fix(
            "Hooked by SageThumbs 2K",
            &format!("0/{enabled} — no format is hooked"),
            "Registration never landed. Settings -> Advanced -> 'Repair file associations', \
             or reinstall.",
        );
    } else {
        r.line(
            S::Warn,
            "Hooked by SageThumbs 2K",
            &format!("{ours}/{enabled}"),
        );
    }

    if missing > 0 {
        r.line(
            S::Warn,
            "  not hooked",
            &format!("{missing}  e.g. {}", missing_examples.join(", ")),
        );
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
    r.line(
        S::Info,
        "Max file size",
        &max_file_size_detail(crate::settings::max_file_size_bytes()),
    );
    r.line(
        S::Info,
        "Max thumbnail size",
        &format!("{} px", crate::settings::max_thumb_size()),
    );
    r.line(
        S::Info,
        "Embedded previews preferred",
        if crate::settings::use_embedded() {
            "yes"
        } else {
            "no"
        },
    );
    if crate::settings::format_badge() {
        r.line(
            S::Info,
            "Format badge",
            if crate::settings::format_badge_icon() {
                "on (icon)"
            } else {
                "on (text)"
            },
        );
        // Only relevant when there IS a badge for Windows' icon to sit on top of.
        type_overlay_note(r);
    }
    if crate::settings::thumb_checker() {
        r.line(
            S::Info,
            "Transparency checkerboard",
            "burned into thumbnails (ThumbChecker) — thumbnails are opaque",
        );
    }
}

/// Prove the decoder itself works, end to end, without touching the disk or the shell.
/// Separating this from the COM checks is the whole diagnostic value: "engine fine,
/// shell never asked" and "engine broken" look identical to a user and need opposite fixes.
/// Render the MaxSize setting for the report.
///
/// `MaxSize = 0` means "no user limit", which [`crate::settings::max_file_size_bytes`]
/// represents as `u64::MAX`. Dividing that by a megabyte and printing it told the user
/// their cap was 17,592,186,044,415 MB — a fabricated number in the one tool whose whole
/// value is that its statements can be trusted. Pure so the sentinel case is testable
/// without writing to the machine's own registry.
fn max_file_size_detail(bytes: u64) -> String {
    if bytes == u64::MAX {
        format!(
            "Unlimited (the provider still caps a single read at {} MB)",
            crate::decode::limits::MAX_INPUT_BYTES / (1024 * 1024)
        )
    } else {
        format!("{} MB (larger files are skipped)", bytes / (1024 * 1024))
    }
}

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
                r.line(
                    S::Fail,
                    "Self-test image",
                    &format!("could not encode: {e}"),
                );
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
        Err(e) => r.line(
            S::Fail,
            "Decode self-test",
            &format!("FAILED on a generated PNG: {e}"),
        ),
    }
    // Video thumbnails ride the OS Media Foundation codecs; the "N"/"KN" editions ship
    // without MF entirely, and then every video keeps its default icon while everything
    // above reports healthy. One line so that shape is visible in every pasted report.
    if crate::video::media_foundation_available() {
        r.line(
            S::Ok,
            "Media Foundation",
            "present — video thumbnails available (frames decode via the OS codecs)",
        );
    } else {
        r.line(
            S::Warn,
            "Media Foundation",
            "MISSING (a Windows \"N\"/\"KN\" edition without the Media Feature Pack?) — \
             video files keep their default icon",
        );
    }
}

/// Probe ONE specific file end-to-end: is its extension one we hook, is that format
/// enabled, and — the part the global checks can't tell you — does THIS file actually
/// DECODE? The global report proves registration is healthy; it stays silent on "we're
/// registered fine but can't render the one file you care about", which is exactly the
/// shape of the modern-`.xcf` reports (GIMP 2.10+/3.0 writes an XCF version the bundled
/// ImageMagick's coder can't read). Read-only: opens + decodes the file, writes nothing.
/// OneDrive (and any Files-On-Demand provider) leaves a *placeholder* on disk: the metadata is
/// local, the bytes are not, and the first read pulls the whole file down over the network.
///
/// That matters here because of HOW MUCH we have to read. Formats with a baked-in preview are
/// cheap even on a placeholder: `stream_source` reads a bounded prefix or seeks straight to a
/// cover, so only a slice is ever recalled. The formats with NO such shortcut fall through to
/// the whole-file read, and on a cloud-only file that means downloading it in full inside
/// Explorer's thumbnail host, which is slow enough to be indistinguishable from broken and
/// leaves a cached failure behind. `.xcf` is the sharp edge (a report, 2026-08-05): GIMP writes
/// no embedded thumbnail, so there is nothing to read but the entire image.
///
/// Reported, never "fixed" silently: refusing to hydrate would take thumbnails away from people
/// whose files ARE downloaded and working today. `std::fs::metadata` reads attributes without
/// triggering recall, so this check itself never pulls anything down.
fn cloud_placeholder_note(r: &mut Report, p: &Path, ext: &str) {
    use std::os::windows::fs::MetadataExt;

    // Win32 file attributes: OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS.
    const OFFLINE: u32 = 0x0000_1000;
    const RECALL_ON_OPEN: u32 = 0x0004_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

    let Ok(meta) = std::fs::metadata(p) else {
        return;
    };
    let attrs = meta.file_attributes();
    if attrs & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) == 0 {
        return; // fully local: nothing to say
    }

    // Deliberately NOT sniffing the header to say whether this format could be served from a
    // prefix: reading even the first bytes of a placeholder is what triggers the recall this
    // check exists to warn about. The advice is the same either way.
    let len = meta.len();
    let size = if len >= 1024 * 1024 {
        format!("{} MB", len / (1024 * 1024))
    } else {
        format!("{} KB", len.div_ceil(1024))
    };
    // Says what is true in general without claiming anything about THIS file's internals,
    // which would need the header read this check exists to avoid.
    r.fail_with_fix(
        "Cloud file (OneDrive)",
        &format!(
            "the bytes are not on this PC yet ({size}). Fetching them happens inside Explorer's \
             thumbnail host, slow enough to look like nothing is happening, and a format with \
             no embedded preview (.xcf is one) needs the WHOLE file, not a slice — this one \
             is .{ext}"
        ),
        "Right-click the file or its folder -> 'Always keep on this device'. Once the bytes are \
         local the thumbnail appears normally. If it stays blank after downloading, Explorer \
         cached the earlier failure: clear thumbcache_*.db (see the IconsOnly fix above).",
    );
}

/// Ask the SHELL for this file's thumbnail, the same way Explorer does, and report what
/// comes back.
///
/// This is the check every other check in this report is a proxy for. Everything above
/// proves *our half* works: registration is healthy, the DLL loads, the decoder renders
/// these bytes. None of it can see the one thing that actually decides what you look at —
/// whether Explorer, on this path, in this folder, calls us at all and keeps the result.
///
/// It matters because those two answers really do come apart. Issue #16 is the shape:
/// registration perfect, decode of the exact file perfect, thumbnail still missing — for a
/// file inside a OneDrive sync root, where the shell can route thumbnails through the sync
/// provider instead of the per-extension handler. Without this line the report says "no
/// blocking problem found" and the user is told, wrongly, that it must be their cache.
///
/// `SIIGBF_THUMBNAILONLY` is what makes the answer meaningful: it tells the shell to FAIL
/// rather than quietly substitute the file's icon, so "no thumbnail" is reported as no
/// thumbnail instead of arriving as a 256px picture of a document.
///
/// Read-only in the sense that matters (it writes nothing of the user's), with one honest
/// caveat: extracting a thumbnail lets Windows populate its own thumbnail cache for this
/// item — exactly what browsing to the folder would have done.
fn shell_roundtrip(r: &mut Report, path: &str) {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY,
    };

    // The shell wants an absolute path — handed a relative one (`st2k doctor file.mkv` from
    // the file's own folder), SHCreateItemFromParsingName fails with FILE_NOT_FOUND and this
    // check would report a spurious "shell returned NO thumbnail". Canonicalize first, and
    // undo the extended-length prefix canonicalize adds (the parsing name grammar rejects
    // it): `\\?\C:\…` -> `C:\…`, and the UNC form `\\?\UNC\server\share\…` -> the plain
    // `\\server\share\…` (stripping just `\\?\` there would leave `UNC\…`, which no API
    // resolves — a network-share doctor run would then fail this check falsely).
    let abs = Path::new(path)
        .canonicalize()
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{rest}")
            } else if let Some(rest) = s.strip_prefix(r"\\?\") {
                rest.to_string()
            } else {
                s
            }
        })
        .unwrap_or_else(|_| path.to_string());
    let path = abs.as_str();

    // The shell objects need an apartment. Uninitialise only if WE initialised, so this
    // never tears down an apartment a caller (the MCP server, a future GUI host) owns.
    let inited = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result: Result<(i32, i32), windows::core::Error> = (|| unsafe {
        let item: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
        let hbmp = item.GetImage(SIZE { cx: 256, cy: 256 }, SIIGBF_THUMBNAILONLY)?;
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            hbmp.into(),
            core::mem::size_of::<BITMAP>() as i32,
            Some(core::ptr::addr_of_mut!(bm).cast()),
        );
        let _ = DeleteObject(hbmp.into());
        if got == 0 {
            return Err(windows::core::Error::from_thread());
        }
        Ok((bm.bmWidth, bm.bmHeight.abs()))
    })();
    if inited {
        unsafe { CoUninitialize() };
    }

    match result {
        Ok((w, h)) => r.line(
            S::Ok,
            "Explorer's own thumbnail",
            &format!("the shell returned a {w}x{h} thumbnail for this path"),
        ),
        Err(e) => r.fail_with_fix(
            "Explorer's own thumbnail",
            &format!(
                "the shell returned NO thumbnail for this path ({:#010x}) — even though our \
                 decoder can render this file",
                e.code().0
            ),
            "our half is working, so something between us and Explorer is dropping it. In \
             order: rebuild the thumbnail cache (Settings > Advanced), then check the note \
             about this file's folder below — a cloud-synced folder can serve thumbnails \
             from the sync provider instead of from us. Copying the file to a plain local \
             folder and re-running this command tells the two apart in one step.",
        ),
    }
}

/// Is this file inside a cloud sync root (OneDrive and friends), and does that provider
/// register its own thumbnail source?
///
/// A sync engine built on the Cloud Files API may declare a `ThumbnailProvider` under its
/// `SyncRootManager` entry, which applies to EVERYTHING under that root rather than to one
/// file type — so it can pre-empt a per-extension handler like ours for every file in the
/// folder. That is the leading explanation for "works in a normal folder, generic icon in
/// OneDrive", and it is invisible from the file itself, so name it here rather than leaving
/// the user to guess. Purely a registry read; nothing is hydrated and nothing is written.
fn cloud_sync_root_note(r: &mut Report, p: &Path) {
    const SYNC_ROOTS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
    let Ok(file) = p.canonicalize() else {
        return;
    };
    let file = file
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_lowercase();

    let Ok(roots) = LOCAL_MACHINE.open(SYNC_ROOTS) else {
        return;
    };
    let Ok(names) = roots.keys() else {
        return;
    };
    for name in names {
        let Ok(root) = roots.open(&name) else {
            continue;
        };
        // Each provider lists its on-disk roots under UserSyncRoots\<user SID>.
        let Ok(user_roots) = root.open("UserSyncRoots") else {
            continue;
        };
        let Ok(values) = user_roots.values() else {
            continue;
        };
        let hit = values.into_iter().any(|(_, v)| {
            let path = String::try_from(v).unwrap_or_default().to_lowercase();
            !path.is_empty() && file.starts_with(path.trim_end_matches('\\'))
        });
        if !hit {
            continue;
        }
        let has_provider =
            root.open("ThumbnailProvider").is_ok() || root.get_string("ThumbnailProvider").is_ok();
        // The provider id is `<Provider>!<SID>!<account>`; the first field is the readable bit.
        let provider = name.split('!').next().unwrap_or(&name).to_string();
        if has_provider {
            r.fail_with_fix(
                "Cloud-synced folder",
                &format!(
                    "this file is inside a {provider} sync root, and {provider} registers its \
                     OWN thumbnail source for everything under it — which can take precedence \
                     over ours for every file in the folder"
                ),
                "not something we can override from here. To confirm it is the cause, copy \
                 the file to a folder outside the sync root and look again: if the thumbnail \
                 appears there, this is why it does not appear here.",
            );
        } else {
            r.line(
                S::Warn,
                "Cloud-synced folder",
                &format!(
                    "this file is inside a {provider} sync root. {provider} does not register \
                     its own thumbnail source, so ours should be used — but a sync root is \
                     still the first thing to rule out by copying the file elsewhere"
                ),
            );
        }
        return;
    }
}

/// Hooked formats whose ProgID declares a `TypeOverlay` icon — the thing Explorer stamps
/// over the bottom-right of a thumbnail, on top of our format badge (issue #18).
///
/// Worth naming because the usual culprit is a program that was UNINSTALLED: the
/// association survives, the icon it points at does not, and what lands on the picture is a
/// blank generic page.
fn type_overlay_note(r: &mut Report) {
    let foreign = crate::typeoverlay::foreign_overlays();
    if foreign.is_empty() {
        return;
    }
    let sample: Vec<String> = foreign
        .iter()
        .take(3)
        .map(|(progid, ext)| format!("{ext} -> {progid}"))
        .collect();
    r.line(
        S::Warn,
        "Windows draws its own icon",
        &format!(
            "{} of your file types stamp a program icon over the thumbnail corner  e.g. {}",
            foreign.len(),
            sample.join(", ")
        ),
    );
    r.line(
        S::Info,
        "  to hide it",
        "Settings > File types > 'Hide Windows' file-type icon on thumbnails' (this is also \
         what covers the format badge, and it often points at a program you uninstalled)",
    );
}

/// For a video file: name the codec inside it and say whether THIS Windows can decode it.
///
/// Frames come from the OS Media Foundation codecs, and Windows does not ship them all —
/// HEVC and AV1 are Microsoft Store add-ons, not inbox — so "registration healthy, file
/// healthy, still no thumbnail" is routinely a codec gap rather than a bug in anything.
/// Without this line that failure is invisible: the decode check below just says FAILED,
/// and the old hint blamed ImageMagick, which never touches video. (Born of an uninstall
/// feedback that said, in full, "mkv thumbnail not showing" — this is the report that
/// would have answered it.)
fn video_codec_note(r: &mut Report, path: &str) {
    // Without Media Foundation there are no video thumbnails at all, whatever the codec.
    // check_engine already prints the global warning; this is the per-file FAIL with a fix.
    if !crate::video::media_foundation_available() {
        r.fail_with_fix(
            "Media Foundation",
            "NOT present on this Windows (\"N\"/\"KN\" editions omit it) — video thumbnails \
             decode through its codecs",
            "install the 'Media Feature Pack' (Settings > Apps > Optional features), then \
             sign out and back in",
        );
        return;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return; // the Read-file check below reports this with its own message
    };
    let mut file = std::io::BufReader::new(file);
    let Some(info) = crate::vcodec::identify(&mut file) else {
        r.line(
            S::Info,
            "Video codec",
            "not identifiable from the container header (only Matroska/WebM and MP4/MOV \
             carry one we parse) — the decode check below is the real test",
        );
        return;
    };
    let label = format!("{} ({})", info.name, info.raw);
    match info.subtype.map(crate::vcodec::decoder_installed) {
        Some(Some(true)) => r.line(
            S::Ok,
            "Video codec",
            &format!("{label} — a Windows decoder is installed"),
        ),
        Some(Some(false)) => r.fail_with_fix(
            "Video codec",
            &format!(
                "{label} — NO Windows decoder for this codec is installed, so no frame \
                 can be decoded (this is the usual cause of a missing video thumbnail)"
            ),
            // Careful wording: this branch fires with MF PRESENT but an inbox decoder
            // missing — a Server / stripped-down edition, where "install the Media
            // Feature Pack" is a setting that does not exist. Name both possibilities
            // instead of sending the user hunting for a control their edition lacks.
            info.install_hint.unwrap_or(
                "this decoder normally ships with consumer Windows; a Server or \
                 stripped-down edition may simply not include it (on an \"N\"/\"KN\" \
                 edition, the Media Feature Pack under Settings > Apps > Optional \
                 features restores it)",
            ),
        ),
        // MF vanished between the gate above and the probe — report it, don't guess.
        Some(None) => r.line(
            S::Warn,
            "Video codec",
            &format!("{label} — could not query Media Foundation for a decoder"),
        ),
        None if info.known => r.fail_with_fix(
            "Video codec",
            &format!("{label} — Windows has no decoder for this codec"),
            "none exists to install; re-encode the file (H.264 plays everywhere), or rely \
             on attached cover art, which we show when no frame can be decoded",
        ),
        None => r.line(
            S::Warn,
            "Video codec",
            &format!("{label} — an id we don't recognize, so we can't check for a decoder"),
        ),
    }
    // An embedded poster (a Matroska attachment or an MP4 `covr` item) means a thumbnail
    // exists even with no codec at all, which is the whole answer for an HEVC library on a
    // machine without the Store extension. Say so, and say which rule is currently in force.
    if crate::vcodec::cover_art(&mut file).is_some() {
        let detail = if crate::settings::prefer_cover_art() {
            "present, and Settings prefers it, so this is the thumbnail you get"
        } else {
            "present - used when no frame can be decoded. Settings > General > 'Use a \
             video's cover art instead of a frame' makes it the first choice"
        };
        r.line(S::Ok, "Embedded cover art", detail);
    }
}

fn probe_file(r: &mut Report, path: &str) {
    r.head("This file");
    let p = Path::new(path);
    r.line(S::Info, "Path", path);
    if !p.is_file() {
        r.fail_with_fix(
            "File",
            "does not exist / not a file",
            "check the path (quote it if it has spaces)",
        );
        return;
    }

    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext.is_empty() {
        r.line(
            S::Warn,
            "Extension",
            "none — Explorer keys thumbnails off the extension",
        );
        return;
    }
    // Is this extension one SageThumbs hooks at all? If not, THAT is the whole answer —
    // Explorer never asks us, no matter how healthy registration is.
    if !crate::formats::is_known(&ext) {
        r.fail_with_fix(
            &format!(".{ext}"),
            "NOT a format SageThumbs handles — Explorer will never ask us for it",
            "this file type isn't supported; open an issue to request it",
        );
        return;
    }
    r.line(S::Ok, &format!(".{ext}"), "a supported format");
    cloud_placeholder_note(r, p, &ext);
    cloud_sync_root_note(r, p);
    if !crate::settings::format_enabled(&ext) {
        r.fail_with_fix(
            "Enabled in settings",
            "this format is unchecked in Settings > File types",
            "tick it in Settings > File types (or 'Select all')",
        );
    }
    let is_video = matches!(
        crate::formats::category(&ext),
        crate::formats::Category::Video
    );
    if is_video {
        video_codec_note(r, path);
    }

    // The decisive step: actually run the thumbnail decoder on THIS file's bytes, the
    // same preview-fidelity path Explorer's provider uses.
    match crate::decode::read_preview_capped(path) {
        Err(e) => r.fail_with_fix(
            "Read file",
            &format!("could not read the bytes: {e}"),
            "check the file isn't locked, truncated, or over the size limit",
        ),
        Ok(bytes) => match crate::decode::decode_preview(&bytes) {
            Ok(img) => {
                r.line(
                    S::Ok,
                    "Decode this file",
                    &format!(
                        "OK ({}x{}) — a thumbnail CAN be produced",
                        img.width(),
                        img.height()
                    ),
                );
                // Our half is proven good, so now ask the shell the same question and see
                // whether the two answers agree. When they don't, that disagreement IS the
                // diagnosis, and it is the only line in this report that can produce it.
                shell_roundtrip(r, path);
                // Reaching here means the decoder is fine and the file is fine, yet the user
                // is running `doctor` on it — so what is left is almost always the shell, and
                // this is the one the report cannot see. Explorer remembers a view PER FOLDER,
                // and Details / List / Small icons never draw thumbnails at all, by design; a
                // folder Windows auto-classified as "Documents" opens in Details. Only said on
                // success, where it is the likely remaining answer rather than noise.
                r.line(
                    S::Info,
                    "  if it still looks wrong",
                    "check this file's FOLDER view: Details, List and Small icons never show \
                     thumbnails. Set Medium icons or larger (View menu, or Ctrl+Shift+2..4).",
                );
            }
            Err(_) if is_video => {
                // Video never touches ImageMagick — the frame comes from the OS Media
                // Foundation codecs, so point at the codec finding instead of the
                // (irrelevant, and previously misleading) ImageMagick hint.
                r.fail_with_fix(
                    "Decode this file",
                    "FAILED — no frame could be decoded from this video",
                    "see the 'Video codec' line above: a missing OS decoder is the usual \
                     cause. If a decoder IS installed, an unusual profile (10-bit, Dolby \
                     Vision) or a truncated file are the next suspects",
                );
            }
            Err(_) => {
                // Registered + enabled, but the pixels won't come out. Point at the
                // likely reason: the long-tail formats decode only through the bundled
                // ImageMagick, whose coders lag newer file-format versions.
                let magick = crate::decode::magick_available();
                let hint = if magick {
                    "ImageMagick is present but its coder could not decode this file \
                     (often a newer version of the format than the coder supports)"
                } else {
                    "this format decodes only via ImageMagick, which is NOT installed here \
                     (use the full installer, or install ImageMagick)"
                };
                r.fail_with_fix(
                    "Decode this file",
                    "FAILED — no thumbnail possible for this file",
                    hint,
                );
            }
        },
    }
}

/// Build the whole report. Read-only; safe to run unelevated, and safe to paste. When
/// `file` is given, a per-file probe section is appended (`st2k doctor <path>`).
pub fn report(file: Option<&str>) -> String {
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
            r.line(
                S::Info,
                "Shell extension DLL",
                &format!("{} ({size} bytes)", p.display()),
            );
        }
        None => r.line(S::Warn, "Shell extension DLL", "could not determine a path"),
    }
    match crate::safety::log_file() {
        Some(p) if p.exists() => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            r.line(
                S::Info,
                "Diagnostics log",
                &format!("{} ({size} bytes)", p.display()),
            );
        }
        Some(p) => r.line(
            S::Info,
            "Diagnostics log",
            &format!("{} (not created yet)", p.display()),
        ),
        None => r.line(S::Warn, "Diagnostics log", "LOCALAPPDATA is unset"),
    }

    check_windows_switches(&mut r);
    check_registration(&mut r);
    check_extensions(&mut r);
    check_settings(&mut r);
    check_engine(&mut r);
    if let Some(f) = file {
        probe_file(&mut r, f);
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

    /// The performance-profile verdict, over every combination that matters.
    ///
    /// Testing this through the real registry would mean setting "Adjust for best performance"
    /// on the machine running the tests, which switches that machine's own thumbnails off —
    /// hence the pure helper.
    #[test]
    fn performance_profile_only_fires_on_best_performance() {
        // Not the performance profile: silent, whatever IconsOnly says. `None` covers the
        // common case of the value never having been written.
        for fx in [None, Some(0), Some(1), Some(3)] {
            for icons in [None, Some(0), Some(1)] {
                assert_eq!(
                    performance_profile_detail(fx, icons),
                    None,
                    "VisualFXSetting={fx:?} IconsOnly={icons:?} should not be reported"
                );
            }
        }
        // "Best performance" always reports, INCLUDING when IconsOnly reads fine — that
        // combination is the whole reason the check exists, so pin its wording.
        let looks_fine = performance_profile_detail(Some(2), Some(0))
            .expect("best-performance + IconsOnly=0 must be reported");
        assert!(
            looks_fine.contains("keep turning it back off"),
            "the IconsOnly-looks-fine case must explain the contradiction: {looks_fine}"
        );
        assert_eq!(
            performance_profile_detail(Some(2), None),
            performance_profile_detail(Some(2), Some(0)),
            "an unset IconsOnly is the same 'looks allowed' case as an explicit 0"
        );
        let already_off = performance_profile_detail(Some(2), Some(1))
            .expect("best-performance + IconsOnly=1 must be reported");
        assert!(
            already_off.contains("turned thumbnails off"),
            "{already_off}"
        );
    }

    /// A file whose bytes are not local (OneDrive placeholder) must be called out, and a
    /// normal local file must NOT be — a false "your file is in the cloud" on every ordinary
    /// probe would be worse than saying nothing.
    #[test]
    fn cloud_placeholder_is_reported_only_when_offline() {
        let dir = std::env::temp_dir().join(format!("st2k-doctor-cloud-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("probe.xcf");
        std::fs::write(&file, b"gimp xcf file\0").unwrap();

        let mut local = Report::new();
        cloud_placeholder_note(&mut local, &file, "xcf");
        assert!(
            local.out.is_empty(),
            "a fully local file must produce no cloud note, got: {}",
            local.out
        );

        // FILE_ATTRIBUTE_OFFLINE is exactly what a Files-On-Demand placeholder carries, and
        // it is settable here, so this exercises the real attribute check rather than a mock.
        set_offline(&file);
        let mut cloud = Report::new();
        cloud_placeholder_note(&mut cloud, &file, "xcf");
        assert!(
            cloud.out.contains("not on this PC yet"),
            "offline file should be reported: {}",
            cloud.out
        );
        // `fail_with_fix` prints the symptom inline and files the fix under Verdict, so the
        // fix lives in `problems` rather than in `out`.
        assert!(
            cloud
                .problems
                .iter()
                .any(|p| p.contains("Always keep on this device")),
            "the report must carry the actual fix: {:?}",
            cloud.problems
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Set FILE_ATTRIBUTE_OFFLINE, preserving whatever else is set.
    fn set_offline(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let attrs = std::fs::metadata(path).unwrap().file_attributes() | 0x0000_1000;
        let ok = unsafe {
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(attrs),
            )
        };
        ok.expect("SetFileAttributesW(OFFLINE) should succeed on a temp file");
    }

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

    /// A user is told to paste this. It must not leak their username via the paths we
    /// print, beyond the log path they already know about... which does contain it —
    /// so this test just pins that we print no OTHER profile-derived path.
    #[test]
    fn report_is_plain_text() {
        let out = report(None);
        assert!(!out.contains('\u{0}'), "report contains NUL");
        assert!(out.is_ascii() || out.chars().all(|c| !c.is_control() || c == '\n'));
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

    /// `MaxSize = 0` ("no limit") reaches here as `u64::MAX`, and dividing that by a
    /// megabyte printed a 17-terabyte cap that does not exist. Driven through the pure
    /// helper on purpose: the value comes from HKCU, so a report-level assertion would
    /// silently pass on any machine whose MaxSize happens not to be 0.
    #[test]
    fn max_file_size_reports_the_unlimited_sentinel_as_unlimited() {
        let unlimited = max_file_size_detail(u64::MAX);
        assert!(
            unlimited.contains("Unlimited"),
            "u64::MAX must read as Unlimited, got: {unlimited}"
        );
        assert!(
            !unlimited.contains("17592186044415"),
            "the sentinel leaked as a number: {unlimited}"
        );
        // An ordinary cap still renders as plain megabytes.
        assert_eq!(
            max_file_size_detail(500 * 1024 * 1024),
            "500 MB (larger files are skipped)"
        );
    }
}
