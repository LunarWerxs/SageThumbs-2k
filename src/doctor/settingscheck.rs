//! SageThumbs 2K's own settings, licence, decode self-test and format-capability
//! sections of `st2k doctor`, plus the settings snapshot and diagnostics-log tail that
//! back `bundle`'s support attachment.

use super::*;

/// Our own settings, which can switch everything off without any registry problem.
pub(super) fn check_settings(r: &mut Report) {
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
    } else if crate::settings::corner_mark() == crate::settings::CornerMark::SystemIcon {
        system_icon_note(r);
    }
    if crate::settings::thumb_checker() {
        r.line(
            S::Info,
            "Transparency checkerboard",
            "burned into thumbnails (ThumbChecker) — thumbnails are opaque",
        );
    }
}

/// The business-licence lock, in the doctor's own words. A Business copy past its 7-day
/// evaluation and 3-day notice with no key refuses every thumbnail, preview and menu
/// (`licence_state::shell_locked`), which to the person looking at Explorer is
/// indistinguishable from any other "no thumbnails" - so this is a `Fail` with the fix,
/// the same shape as the master switch above it. The evaluation and the notice are said
/// too, as information and as a warning, so a support thread can see the clock. A
/// Personal copy prints one line and nothing else: free is free.
pub(super) fn check_licence(r: &mut Report) {
    use crate::licence_state::{current_phase, days_until, now_unix, read_mode, Mode, Phase};
    r.head("Licence");
    if read_mode() == Mode::Personal {
        r.line(
            S::Info,
            "Licence",
            "personal use (free); nothing here can block thumbnails",
        );
        return;
    }
    let now = now_unix();
    match current_phase() {
        Phase::Clear => r.line(
            S::Info,
            "Licence",
            "business use; not blocking thumbnails (licensed, or reminders only)",
        ),
        Phase::Trial { ends_unix } => r.line(
            S::Info,
            "Business evaluation",
            &format!(
                "running, {} day(s) left; without a licence key, thumbnails stop {} day(s) after that",
                days_until(now, ends_unix),
                crate::licence_state::LOCK_GRACE_SECS / (24 * 60 * 60)
            ),
        ),
        Phase::Expiring { locks_unix } => r.line(
            S::Warn,
            "Business evaluation",
            &format!(
                "ENDED; thumbnails, previews and the menu stop in {} day(s) unless a licence key is entered under Settings -> Licence",
                days_until(now, locks_unix)
            ),
        ),
        Phase::Locked => r.fail_with_fix(
            "Licence",
            "STOPPED: this copy is installed for business use, its evaluation has ended (or its licence was revoked), and no licence key is entered - every thumbnail and preview is refused",
            "Settings -> Licence -> Redeem key. No key yet? Buy one at st2k.lunarwerx.com/buy, or reinstall and choose Personal if this is not a work computer.",
        ),
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

pub(super) fn check_engine(r: &mut Report) {
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

/// Audit E03: the format table hooks 300+ extensions, but "hooked" doesn't mean "full
/// decode" - a RAW file's thumbnail is its embedded JPEG, an archive shows contents, and a
/// handful of formats depend on an OS codec that may or may not be installed. This block
/// makes that visible: a per-source-kind count over the whole table, then each OS-codec
/// dependency named with whether the codec is actually present HERE. A missing codec is a
/// WARNING, never an error - the plain-English consequence is always "these keep their
/// default icon", same severity `check_engine`'s Media Foundation line already uses.
pub(super) fn check_format_capability(r: &mut Report) {
    r.head("Format capability");

    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for &(ext, _) in FORMATS {
        *counts
            .entry(crate::formats::capability(ext).source.as_str())
            .or_insert(0) += 1;
    }
    let by_source: Vec<String> = counts.iter().map(|(k, v)| format!("{k}={v}")).collect();
    r.line(S::Info, "By source", &by_source.join(", "));

    use crate::formats::OsCodec;
    // `Av1` is deliberately NOT in this array - it has no `os_codec_available` component
    // lookup to run at all (no WIC container GUID exists for it), so it gets its own honest
    // block below instead of a present/MISSING verdict this loop can't actually back up.
    for codec in [OsCodec::MediaFoundation, OsCodec::WmPhoto, OsCodec::Heif] {
        let exts: Vec<&str> = FORMATS
            .iter()
            .filter(|&&(ext, _)| crate::formats::capability(ext).os_codec == Some(codec))
            .map(|&(ext, _)| ext)
            .collect();
        if exts.is_empty() {
            continue;
        }
        let label = match codec {
            OsCodec::MediaFoundation => "OS codec: Media Foundation (video)",
            OsCodec::WmPhoto => "OS codec: WIC JPEG XR / HD Photo",
            OsCodec::Heif => "OS codec: WIC HEIC/HEIF",
            OsCodec::Av1 => unreachable!("Av1 excluded from this loop above"),
        };
        if crate::decode::os_codec_available(codec) {
            if codec == OsCodec::Heif {
                // The WIC container-decoder lookup above proves HEIC/HEIF CONTAINERS parse,
                // not that the HEVC pixels inside decode - that needs the separate "HEVC
                // Video Extension" Store package. Audit E03 #4: printing bare "present" here
                // let doctor claim success on a machine where `.heic` still fails. Probe the
                // same way `video_codec_note` does for a video HEVC stream - a real Media
                // Foundation decoder-presence query (`vcodec::decoder_installed`), not a guess.
                use windows::Win32::Media::MediaFoundation::MFVideoFormat_HEVC;
                match crate::vcodec::decoder_installed(MFVideoFormat_HEVC) {
                    Some(true) => r.line(
                        S::Ok,
                        label,
                        &format!(
                            "container decoder present; the HEVC Video Extension it needs is \
                             ALSO installed - {} format(s) decode here ({})",
                            exts.len(),
                            exts.join(", ")
                        ),
                    ),
                    // The OS route is the fast, hardware-assisted one, not the ONLY one: a
                    // Full install decodes HEIC/HEIF through the bundled ImageMagick when
                    // Windows cannot (2026-09-19 audit F23 measured real corpus files
                    // rendering that way), so a missing Store extension is a slower route on
                    // such a copy, and a genuine gap only on a Compact one.
                    Some(false) if crate::decode::magick_available() => r.line(
                        S::Info,
                        label,
                        &format!(
                            "container decoder present, but the HEVC Video Extension is NOT \
                             installed - {} format(s) decode through the bundled ImageMagick \
                             instead, slower and without the OS's hardware route ({}); the \
                             \"HEVC Video Extensions\" from the Microsoft Store would speed them up",
                            exts.len(),
                            exts.join(", ")
                        ),
                    ),
                    Some(false) => r.fail_with_fix(
                        label,
                        &format!(
                            "container decoder present, but the HEVC Video Extension it needs \
                             is NOT installed, and this Compact install has no bundled decoder \
                             to fall back on - {} format(s) keep their default icon ({})",
                            exts.len(),
                            exts.join(", ")
                        ),
                        "install the \"HEVC Video Extensions\" (or \"HEIF Image Extensions\", \
                         which bundles it) from the Microsoft Store, or reinstall the Full edition",
                    ),
                    None => r.line(
                        S::Info,
                        label,
                        &format!(
                            "container decoder present; the HEVC Video Extension it needs was \
                             not verified (Media Foundation unavailable) - {} format(s) may or \
                             may not decode here ({})",
                            exts.len(),
                            exts.join(", ")
                        ),
                    ),
                }
            } else {
                r.line(
                    S::Ok,
                    label,
                    &format!(
                        "present - {} format(s) decode here ({})",
                        exts.len(),
                        exts.join(", ")
                    ),
                );
            }
        } else {
            r.line(
                S::Warn,
                label,
                &format!(
                    "MISSING - {} format(s) keep their default icon ({})",
                    exts.len(),
                    exts.join(", ")
                ),
            );
        }
    }

    // AV1 (AVIF): no WIC container GUID exists to probe (see `OsCodec::Av1`'s doc), so - unlike
    // the loop above - this is reported honestly as unverified rather than guessed at (audit
    // E03 #2). Consistent with `video_codec_note`'s AV1 handling: both name the dependency
    // without claiming a verdict the code can't actually back up.
    let av1_exts: Vec<&str> = FORMATS
        .iter()
        .filter(|&&(ext, _)| crate::formats::capability(ext).os_codec == Some(OsCodec::Av1))
        .map(|&(ext, _)| ext)
        .collect();
    if !av1_exts.is_empty() {
        r.line(
            S::Info,
            "OS codec: AV1 (AVIF)",
            &format!(
                "needs the AV1 Video Extension; not probed (no WIC container GUID for it) - \
                 {} format(s) affected ({})",
                av1_exts.len(),
                av1_exts.join(", ")
            ),
        );
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
        "Settings > Appearance > 'In the corner of a thumbnail' (the badge suppresses \
         Windows' icon only where its owner has not declared one; this one often points at a \
         program you uninstalled)",
    );
}

/// The corner is Windows' to draw (`CornerMark::SystemIcon`), so say where we had to help it
/// and where its owner told it not to. Explorer never overlays a type it treats as a
/// picture, and draws nothing for a registration left hollow by an update, so "I chose
/// Windows' icon and PSD has none" is a real report (2026-09-09) with two different answers;
/// `typeoverlay.rs` writes the icon for those two and leaves an owner's own "" alone.
fn system_icon_note(r: &mut Report) {
    let restored = crate::typeoverlay::restored_overlays();
    if !restored.is_empty() {
        let sample: Vec<String> = restored
            .iter()
            .take(3)
            .map(|x| {
                if x.value.is_empty() {
                    format!("{} -> {} (nothing: its icon is gone)", x.ext, x.progid)
                } else {
                    format!("{} -> {}", x.ext, x.progid)
                }
            })
            .collect();
        r.line(
            S::Ok,
            "Corner icon",
            &format!(
                "Windows' file-type icon; SageThumbs tells Explorer which icon to draw on {} of \
                 your file types that it would otherwise leave bare  e.g. {}",
                restored.len(),
                sample.join(", ")
            ),
        );
        // Never report the whole set as healthy without looking. The value is an absolute
        // path, and the program that owns it can be upgraded into a new versioned directory
        // or removed at any time without telling us — at which point the corner silently goes
        // bare again and an unconditional "ok" line above would be the one thing standing
        // between the user and an explanation.
        let stale: Vec<&crate::typeoverlay::Restored> =
            restored.iter().filter(|x| x.stale).collect();
        if !stale.is_empty() {
            r.fail_with_fix(
                "  one of those icons has moved",
                &format!(
                    "{} of them name a file that is no longer there, so those corners are bare \
                     again  e.g. {}",
                    stale.len(),
                    stale
                        .iter()
                        .take(3)
                        .map(|x| format!("{} -> {}", x.ext, x.value))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "the owning program was updated or uninstalled since SageThumbs last looked; \
                 open Settings and press Apply, or reinstall, and the corner is re-derived",
            );
        }
    }
    let suppressed = crate::typeoverlay::owner_suppressed();
    if !suppressed.is_empty() {
        let sample: Vec<String> = suppressed
            .iter()
            .take(3)
            .map(|(progid, ext)| format!("{ext} -> {progid}"))
            .collect();
        r.line(
            S::Warn,
            "No corner icon by its owner's choice",
            &format!(
                "{} of your file types belong to a program that tells Windows to draw no icon \
                 on its thumbnails  e.g. {}",
                suppressed.len(),
                sample.join(", ")
            ),
        );
        r.line(
            S::Info,
            "  to mark them anyway",
            "Settings > Appearance > 'In the corner of a thumbnail' > 'A SageThumbs format mark'",
        );
    }
}

/// Bounded read: the last `max_bytes` of `path`, lossy-decoded (a seek can land mid a
/// multi-byte UTF-8 character at the boundary; `String::from_utf8_lossy` degrades that one
/// character instead of losing the whole tail). Read the log's ~1 MiB rotation cap ([`crate::
/// safety`]'s `maybe_rotate`) makes this cheap either way, but a bounded read stays cheap
/// even if that cap is ever raised.
fn tail_bytes(path: &Path, max_bytes: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(max_bytes).read_to_end(&mut buf);
    buf
}

pub(super) fn read_log_tail(path: &Path, max_bytes: u64) -> String {
    String::from_utf8_lossy(&tail_bytes(path, max_bytes)).into_owned()
}

/// Bounded read + the last `limit` lines containing any of `needles` — used for the ERROR/
/// PANIC scan below, and shared with [`bundle`]'s log-tail file.
pub(super) const LOG_TAIL_SCAN_BYTES: u64 = 256 * 1024;
const LOG_TAIL_LINES: usize = 20;

pub(super) fn tail_matching_lines(
    path: &Path,
    max_bytes: u64,
    needles: &[&str],
    limit: usize,
) -> Vec<String> {
    let text = read_log_tail(path, max_bytes);
    let mut out: Vec<String> = text
        .lines()
        .filter(|l| needles.iter().any(|n| l.contains(n)))
        .map(str::to_string)
        .collect();
    if out.len() > limit {
        let drop = out.len() - limit;
        out.drain(0..drop);
    }
    out
}

/// `log_error` (`ERROR ...`) and the panic hook (`PANIC [...] at ...: ...`, which
/// itself goes through `log_error` and so also matches) had no reader anywhere but the
/// panic hook itself — the doctor report printed only the log's path and size, so the most
/// common issue report ("X shows the generic icon") produced an empty-looking paste with
/// nothing in it. Appends the last [`LOG_TAIL_LINES`] matching lines from `path` and its
/// `.old` rotation sibling, oldest first.
pub(super) fn append_log_tail(r: &mut Report, path: &Path) {
    const NEEDLES: [&str; 2] = [" ERROR ", "PANIC"];
    let mut tail = tail_matching_lines(path, LOG_TAIL_SCAN_BYTES, &NEEDLES, LOG_TAIL_LINES);
    if tail.len() < LOG_TAIL_LINES {
        let old = path.with_file_name("SageThumbs2K.log.old");
        if old.exists() {
            let need = LOG_TAIL_LINES - tail.len();
            let mut older = tail_matching_lines(&old, LOG_TAIL_SCAN_BYTES, &NEEDLES, need);
            older.extend(tail);
            tail = older;
        }
    }
    if tail.is_empty() {
        r.line(S::Info, "  recent ERROR/PANIC lines", "none found");
        return;
    }
    r.head("Recent ERROR/PANIC lines from the diagnostics log");
    for line in &tail {
        let _ = writeln!(r.out, "{line}");
    }
}

/// One stored settings section for the bundle: `None` is the root (the registry root key,
/// or the ini's `[Settings]`), `Some(name)` a subkey or section. Values are `(name, text)`.
type SettingsSection = (Option<String>, Vec<(String, String)>);

/// A registry key's values as text, in name order so two snapshots diff cleanly. DWORDs and
/// strings are the only two types the app stores; anything else is named rather than
/// skipped, because a report that silently omits a value is the one report that cannot be
/// trusted to say what is there.
fn registry_values(key: &windows_registry::Key) -> Vec<(String, String)> {
    let mut values: Vec<(String, String)> = key
        .values()
        .map(|vs| {
            vs.map(|(name, value)| {
                let text = u32::try_from(value.clone())
                    .map(|n| n.to_string())
                    .or_else(|_| String::try_from(value))
                    .unwrap_or_else(|_| "(a value type this snapshot does not render)".into());
                (name, text)
            })
            .collect()
        })
        .unwrap_or_default();
    values.sort();
    values
}

/// The settings tree of an installed copy: the root's values plus one level of subkeys,
/// which is the whole depth the app ever writes. Absent root means nothing configured.
fn registry_settings() -> Vec<SettingsSection> {
    let Ok(root) = CURRENT_USER.open(crate::settings::hkcu_root_path()) else {
        return Vec::new();
    };
    let mut out = vec![(None, registry_values(&root))];
    let mut names: Vec<String> = root.keys().map(Iterator::collect).unwrap_or_default();
    names.sort();
    for name in names {
        if let Ok(sub) = root.open(&name) {
            let values = registry_values(&sub);
            out.push((Some(name), values));
        }
    }
    out
}

/// The settings tree of a portable copy, read through the same accessors the app uses
/// rather than by re-parsing the ini here, so a comment or a quirk the store tolerates is
/// rendered the way the app understands it.
fn portable_settings() -> Vec<SettingsSection> {
    let sorted = |mut v: Vec<(String, String)>| {
        v.sort();
        v
    };
    let mut out = vec![(None, sorted(crate::settings::portable_values(None)))];
    let mut sections = crate::settings::portable_subkeys();
    sections.sort();
    for name in sections {
        let values = sorted(crate::settings::portable_values(Some(&name)));
        out.push((Some(name), values));
    }
    out
}

/// Drop the sign-in state from a settings tree: the credential section on an installed copy
/// and the prefixed root values on a portable one (2026-09-05 audit, E01). The rule is
/// [`crate::settings::is_credential_subkey`] / [`crate::settings::is_credential_root_value`],
/// the same one the settings export applies, and deliberately NOT a list of value names: the
/// credential store writes everything under that one container, so a credential it grows
/// later is scrubbed here without anyone remembering to add it. The root filter runs for
/// both backends because a value can only be where the rule looks for it, and checking the
/// other backend's shape as well costs nothing.
///
/// Both checks are on the SECTION and the VALUE NAME, never on the value text: a search for
/// the token's bytes would need the token, and a snapshot that has to know the secret to
/// hide it has already read it.
fn without_credentials(mut sections: Vec<SettingsSection>) -> Vec<SettingsSection> {
    sections.retain(|(name, _)| {
        !name
            .as_deref()
            .is_some_and(crate::settings::is_credential_subkey)
    });
    for (_, values) in &mut sections {
        values.retain(|(name, _)| !crate::settings::is_credential_root_value(name));
    }
    sections
}

/// The snapshot as ini text, the shape a portable user already knows from the file beside
/// the EXE, so one reader serves both backends. `storage` says where it came from.
fn render_settings(storage: &str, sections: &[SettingsSection]) -> String {
    // The header does not name the credential container: a bundle scanner that greps for
    // it would otherwise flag every clean bundle, and the reader gains nothing from the name.
    let mut s = format!(
        "; SageThumbs 2K settings as stored ({storage}).\n\
         ; The sign-in state (refresh token, licence certificate, identity) is left out on \
         purpose.\n"
    );
    if sections.is_empty() {
        s.push_str("\n; (nothing stored yet, every setting is at its default)\n");
        return s;
    }
    for (name, values) in sections {
        let heading = name
            .as_deref()
            .unwrap_or(crate::settings::PORTABLE_ROOT_SECTION);
        let _ = write!(s, "\n[{heading}]\n");
        for (k, v) in values {
            let _ = writeln!(s, "{k}={v}");
        }
        if values.is_empty() {
            s.push_str("; (empty)\n");
        }
    }
    s
}

/// Every stored preference, for the bundle's `settings.txt`, with the sign-in state
/// scrubbed. The report's own settings section names the handful of switches that can
/// blank a thumbnail; a bug about a format toggle, a menu item or a convert default needs
/// the rest, which used to be a separate export the reporter had to think to attach.
pub(super) fn settings_snapshot() -> String {
    let (storage, sections) = match crate::settings::ini_path() {
        Some(ini) => (
            format!("portable ini at {}", ini.display()),
            portable_settings(),
        ),
        None => (
            format!(r"registry, HKCU\{}", crate::settings::hkcu_root_path()),
            registry_settings(),
        ),
    };
    render_settings(&storage, &without_credentials(sections))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Only lines containing " ERROR " or "PANIC" survive the scan, in file order, and
    /// the result is capped at `limit` — the last N, not the first N (a doctor paste should
    /// show the MOST RECENT failures, not the oldest ones from a long-lived log).
    #[test]
    fn tail_matching_lines_filters_and_caps_to_the_most_recent() {
        let dir = std::env::temp_dir().join(format!("st2k_doctor_logtail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("test.log");
        let mut body = String::new();
        for i in 0..25 {
            body.push_str(&format!("[pid 1 +{i}ms] ERROR failure number {i}\n"));
            body.push_str(&format!("[pid 1 +{i}ms] just a debug line {i}\n"));
        }
        body.push_str("[pid 1 +999ms] ERROR PANIC [thumbprovider] at foo.rs:1: boom\n");
        std::fs::write(&log, &body).unwrap();

        let lines = tail_matching_lines(&log, LOG_TAIL_SCAN_BYTES, &[" ERROR ", "PANIC"], 20);
        assert_eq!(lines.len(), 20, "must cap at the requested limit");
        assert!(
            lines
                .iter()
                .all(|l| l.contains(" ERROR ") || l.contains("PANIC")),
            "every returned line must match a needle: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("just a debug line")),
            "a non-matching line must never survive the filter"
        );
        // The most recent 20 of 26 matching lines were kept, so "failure number 0..4" (the
        // oldest 5) must have been dropped and the trailing PANIC line must be present.
        assert!(!lines.iter().any(|l| l.contains("failure number 4")));
        assert!(lines.last().unwrap().contains("PANIC"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file with no matching lines returns an empty vec, not an error or a panic — the
    /// common case (a healthy install whose log has no ERROR/PANIC lines at all).
    #[test]
    fn tail_matching_lines_on_a_clean_log_is_empty() {
        let dir =
            std::env::temp_dir().join(format!("st2k_doctor_logtail_clean_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("clean.log");
        std::fs::write(&log, "[pid 1 +1ms] everything is fine\n").unwrap();

        let lines = tail_matching_lines(&log, LOG_TAIL_SCAN_BYTES, &[" ERROR ", "PANIC"], 20);
        assert!(lines.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bundle's scrub, over a synthetic tree of both backends' shapes at once: the
    /// credential section goes, the prefixed root values go, and every ordinary preference
    /// beside them stays, including one whose NAME resembles a credential's. The end-to-end
    /// proof over a real ini and a real scratch registry root is `tests/doctor_bundle_scrub_*`,
    /// one process per backend since the storage mode resolves once.
    #[test]
    fn settings_snapshot_scrubs_the_credential_container_and_keeps_the_rest() {
        let tree: Vec<SettingsSection> = vec![
            (
                None,
                vec![
                    ("MaxSize".into(), "200".into()),
                    ("OAuth_LicenceCert".into(), "cert-blob".into()),
                    ("OAuth_Name".into(), "Some One".into()),
                    ("OAuth_RefreshToken".into(), "token-blob".into()),
                    ("Sub".into(), "a preference that only looks like one".into()),
                    ("Theme".into(), "1".into()),
                ],
            ),
            (
                Some("MenuItems".into()),
                vec![("Convert".into(), "0".into())],
            ),
            (
                Some("OAuth".into()),
                vec![
                    ("Email".into(), "who@example.invalid".into()),
                    ("RefreshToken".into(), "token-blob".into()),
                ],
            ),
            (Some("jpg".into()), Vec::new()),
        ];
        let text = render_settings("test", &without_credentials(tree));
        for kept in [
            "[Settings]",
            "MaxSize=200",
            "Sub=a preference",
            "Theme=1",
            "[MenuItems]",
            "Convert=0",
            "[jpg]",
            "; (empty)",
        ] {
            assert!(text.contains(kept), "{kept} missing from:\n{text}");
        }
        for gone in [
            "token-blob",
            "cert-blob",
            "Some One",
            "who@example.invalid",
            "[OAuth]",
            "OAuth_",
        ] {
            assert!(!text.contains(gone), "{gone} leaked into:\n{text}");
        }
        assert!(
            render_settings("test", &[]).contains("nothing stored yet"),
            "an unconfigured copy must say so rather than render an empty file"
        );
    }

    /// `bundle` must produce a zip with exactly the four named entries, each
    /// non-empty, and readable back — the whole point is a single self-contained
    /// attachment a support triage can accept as-is.
    #[test]
    fn bundle_writes_a_zip_with_report_formats_log_tail_and_settings() {
        let dir = std::env::temp_dir().join(format!("st2k_doctor_bundle_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("bundle.zip");

        bundle(&out, None).unwrap();
        assert!(out.exists());

        let f = std::fs::File::open(&out).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        for expect in [
            "doctor-report.txt",
            "formats.json",
            "log-tail.txt",
            "settings.txt",
        ] {
            assert!(
                names.contains(&expect.to_string()),
                "missing {expect}: {names:?}"
            );
        }
        let mut settings_text = String::new();
        std::io::Read::read_to_string(
            &mut zip.by_name("settings.txt").unwrap(),
            &mut settings_text,
        )
        .unwrap();
        assert!(
            settings_text.starts_with("; SageThumbs 2K settings as stored"),
            "{settings_text}"
        );
        let mut report_text = String::new();
        std::io::Read::read_to_string(
            &mut zip.by_name("doctor-report.txt").unwrap(),
            &mut report_text,
        )
        .unwrap();
        assert!(report_text.contains("SageThumbs 2K"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
