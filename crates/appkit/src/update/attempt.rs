//! A self-update in flight, and what its setup log says once it is over.
//!
//! Issue #60: an update whose installer gave up said nothing at all. The setup window showed,
//! ran, vanished, the app never came back, and the next launch was simply the old version, so
//! the only report the owner ever got was "in-app update doesn't work", with nothing to go on.
//! Now the updater leaves a one-line record of the attempt and hands setup a log path it knows,
//! and the next Settings launch reads both: an update that landed is forgotten silently, and
//! one that did not is reported once, with the reason setup itself logged.

use super::*;

/// How long an attempt with no verdict in its log may still be running. Past this the setup
/// process is long gone (a silent update takes well under a minute), so it is reported.
const GIVE_UP_SECS: u64 = 30 * 60;

/// Inno's log of the setup this app launched, beside the update-check cache.
fn setup_log_path() -> Option<PathBuf> {
    cache_path()?
        .parent()
        .map(|d| d.join("SageThumbs2K-setup.log"))
}

/// The record of the attempt: `<unix_secs>\n<tag>\n<1 if setup was given the log, else 0>\n`.
fn attempt_path() -> Option<PathBuf> {
    cache_path()?
        .parent()
        .map(|d| d.join("SageThumbs2K-update-attempt.txt"))
}

/// An empty setup log, created by THIS user, so the path handed to setup is known writable.
/// Inno treats a log it cannot create as fatal ("Error creating log file"), and a fatal error
/// before the first file is copied is the last thing an update needs; `None` launches setup
/// without `/LOG`, and its own `SetupLogging` log in `%TEMP%` still exists.
pub(super) fn fresh_setup_log() -> Option<PathBuf> {
    let p = setup_log_path()?;
    std::fs::write(&p, b"").ok()?;
    Some(p)
}

/// The switches for the launched setup. `/SILENT` is a bare progress bar with no wizard,
/// `/NORESTART` never reboots, and `/UPDATED` is OUR marker that keys the post-update
/// "you're now on <ver>" relaunch (installer.iss `WasSelfUpdate`). Deliberately NOT
/// `/SUPPRESSMSGBOXES`: it made setup answer its own Abort/Retry/Ignore with Abort and roll the
/// whole update back without a word (issue #60). Nor `/FORCECLOSEAPPLICATIONS`: setup no longer
/// closes anything (installer.iss `CloseApplications=no`, `SwapAsideHeldFiles`).
pub(super) fn install_flags(log: Option<&Path>) -> String {
    let mut flags = String::from("/SILENT /NORESTART /UPDATED");
    if let Some(p) = log {
        flags.push_str(&format!(" /LOG=\"{}\"", p.display()));
    }
    flags
}

/// Note that setup is about to install `tag`, and whether it was handed the log: when
/// [`fresh_setup_log`] could not empty the file, whatever is in it belongs to an EARLIER setup,
/// and reading its verdict for this attempt could forget a failure or quote the wrong reason.
/// Best-effort: without the record only the report is lost.
pub(super) fn record_attempt(tag: &str, log_handed: bool) {
    if let Some(p) = attempt_path() {
        let _ = std::fs::write(
            p,
            format!("{}\n{tag}\n{}\n", now_secs(), u8::from(log_handed)),
        );
    }
}

/// Drop the attempt record: the launch failed (the caller already said why), or the
/// installer's `--updated` relaunch arrived, which is the update reporting on itself.
pub fn forget_update_attempt() {
    if let Some(p) = attempt_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// `--remove-user-state`: this user's update records, beside [`cache_path`]'s own removal.
pub fn remove_update_records() {
    forget_update_attempt();
    if let Some(p) = setup_log_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// What a setup log says about how the install ended.
#[derive(Debug, PartialEq)]
pub(super) enum SetupOutcome {
    Succeeded,
    /// It stopped without installing; carries setup's own words for why, when it logged any.
    Failed(Option<String>),
    /// No verdict yet: still running, or killed before it could write one.
    Unfinished,
}

/// Inno prefixes each entry with `yyyy-mm-dd hh:mm:ss.mmm`; a multi-line entry continues on
/// lines without one. Returns the entries, prefix stripped, continuation lines kept.
fn log_entries(log: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in log.trim_start_matches('\u{feff}').lines() {
        match strip_timestamp(line) {
            Some(text) => entries.push(text.trim().to_string()),
            None => {
                if let Some(last) = entries.last_mut() {
                    last.push('\n');
                    last.push_str(line.trim());
                }
            }
        }
    }
    entries
}

fn strip_timestamp(line: &str) -> Option<&str> {
    let b = line.as_bytes();
    if b.len() < 24 {
        return None;
    }
    let shape = b"dddd-dd-dd dd:dd:dd.ddd";
    let ok = shape.iter().zip(b).all(|(&s, &c)| {
        if s == b'd' {
            c.is_ascii_digit()
        } else {
            c == s
        }
    });
    (ok && b[23] == b' ').then(|| &line[23..])
}

/// The text of an entry after its first line, flattened to one line ("" lines dropped).
fn entry_body(entry: &str) -> Option<String> {
    let body: Vec<&str> = entry
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut text = body.join(" ");
    if text.is_empty() {
        return None;
    }
    if text.chars().count() > 400 {
        text = text.chars().take(400).collect::<String>() + "\u{2026}";
    }
    Some(text)
}

pub(super) fn setup_log_outcome(log: &str) -> SetupOutcome {
    let entries = log_entries(log);
    if entries
        .iter()
        .any(|e| e.starts_with("Installation process succeeded"))
    {
        return SetupOutcome::Succeeded;
    }
    let ended = entries.iter().any(|e| {
        e.starts_with("Rolling back changes")
            || e.starts_with("User canceled the installation process")
            || e.starts_with("Fatal exception during installation process")
            || e.starts_with("Deinitializing Setup")
    });
    if !ended {
        return SetupOutcome::Unfinished;
    }
    // A fatal exception names its own cause. Otherwise the cause is the last message box setup
    // raised, answered by the user or (under /SUPPRESSMSGBOXES) by setup itself: for a held
    // file, the Abort/Retry/Ignore whose Abort is what rolled the update back.
    let fatal = entries
        .iter()
        .rev()
        .filter(|e| e.starts_with("Fatal exception during installation process"))
        .find_map(|e| entry_body(e));
    let boxed = || {
        entries
            .iter()
            .rev()
            .filter(|e| {
                e.starts_with("Message box (")
                    || (e.starts_with("Defaulting to ") && e.contains("suppressed message box"))
            })
            .find_map(|e| entry_body(e))
    };
    SetupOutcome::Failed(fatal.or_else(boxed))
}

/// What to do with a recorded attempt.
#[derive(Debug, PartialEq)]
pub(super) enum Verdict {
    /// Not over yet: ask again next launch.
    Keep,
    /// It landed, or setup finished and the `--updated` toast has spoken: drop the record.
    Forget,
    /// It did not install: report it (setup's reason when it gave one), then drop the record.
    Report(Option<String>),
}

pub(super) fn attempt_verdict(
    started: u64,
    now: u64,
    tag: &str,
    running: &str,
    log: Option<&str>,
) -> Verdict {
    let pending = matches!(
        (parse_ver(tag), parse_ver(running)),
        (Some(t), Some(r)) if t > r
    );
    if !pending {
        return Verdict::Forget;
    }
    match log.map(setup_log_outcome) {
        Some(SetupOutcome::Succeeded) => Verdict::Forget,
        Some(SetupOutcome::Failed(reason)) => Verdict::Report(reason),
        _ if now.saturating_sub(started) >= GIVE_UP_SECS => Verdict::Report(None),
        _ => Verdict::Keep,
    }
}

pub(super) fn report_text(
    tag: &str,
    running: &str,
    reason: Option<&str>,
    log: Option<&Path>,
) -> String {
    let mut text = format!(
        "The update to SageThumbs 2K {tag} didn't install, so this PC is still on {running}."
    );
    if let Some(r) = reason {
        text.push_str(&format!("\n\nSetup reported: {r}"));
    }
    if let Some(p) = log {
        text.push_str(&format!("\n\nSetup's full log: {}", p.display()));
    }
    text.push_str(&format!(
        "\n\nOpen the releases page to install {tag} by hand?"
    ));
    text
}

/// The most of setup's log [`failed_update_report`] reads: its verdict is at the end, a whole
/// install logs a few hundred KB, and the file sits in a user-writable folder.
const LOG_TAIL: u64 = 4 * 1024 * 1024;

fn read_log_tail(p: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL))).ok()?;
    let mut buf = Vec::new();
    f.take(LOG_TAIL).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The Settings launch check: the message to show if the last self-update did not install,
/// at most once per attempt. `None` when there was no attempt, it landed, or it may still be
/// running.
pub fn failed_update_report() -> Option<String> {
    let marker = attempt_path()?;
    let record = std::fs::read_to_string(&marker).ok()?;
    let mut lines = record.lines();
    let started = lines.next().and_then(|l| l.trim().parse::<u64>().ok());
    // Re-printed from its numbers: the record sits in a user-writable folder, and its text
    // must never reach the dialog as written.
    let tag = lines
        .next()
        .and_then(|l| parse_ver(l.trim()))
        .map(|(a, b, c)| format!("{a}.{b}.{c}"));
    let (Some(started), Some(tag)) = (started, tag) else {
        let _ = std::fs::remove_file(&marker);
        return None;
    };
    // Only the log this attempt handed to setup speaks for it (see `record_attempt`).
    let log_handed = lines.next().is_some_and(|l| l.trim() == "1");
    let log_path = setup_log_path().filter(|p| log_handed && p.exists());
    let log = log_path.as_deref().and_then(read_log_tail);
    let running = env!("CARGO_PKG_VERSION");
    let tag = tag.as_str();
    match attempt_verdict(started, now_secs(), tag, running, log.as_deref()) {
        Verdict::Keep => None,
        Verdict::Forget => {
            let _ = std::fs::remove_file(&marker);
            None
        }
        Verdict::Report(reason) => {
            let _ = std::fs::remove_file(&marker);
            st2k_base::safety::log(&format!(
                "update: the update to {tag} did not install: {}",
                reason.as_deref().unwrap_or("setup logged no reason")
            ));
            Some(report_text(
                tag,
                running,
                reason.as_deref(),
                log_path.as_deref(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{attempt_verdict, setup_log_outcome, SetupOutcome, Verdict, GIVE_UP_SECS};

    /// A silent update that hit a held file, as Inno logged it: the suppressed
    /// Abort/Retry/Ignore was answered Abort and the install rolled back. The report must carry
    /// the file and the error, which is the whole of what #60 never had.
    const ROLLED_BACK: &str = "\
2026-10-06 14:02:10.101   Setup version: Inno Setup version 7.1.0
2026-10-06 14:02:11.123   Dest filename: C:\\Program Files\\SageThumbs2K\\CORE_RL_MagickCore_.dll
2026-10-06 14:02:11.130   Defaulting to Abort for suppressed message box (Abort/Retry/Ignore):
                          C:\\Program Files\\SageThumbs2K\\CORE_RL_MagickCore_.dll

                          An error occurred while trying to replace the existing file:
                          DeleteFile failed; code 5.
                          Access is denied.
2026-10-06 14:02:11.131   User canceled the installation process.
2026-10-06 14:02:11.132   Rolling back changes.
2026-10-06 14:02:12.000   Deinitializing Setup.
";

    #[test]
    fn a_rolled_back_update_reports_setups_own_reason() {
        let SetupOutcome::Failed(Some(reason)) = setup_log_outcome(ROLLED_BACK) else {
            panic!("a rollback must read as a failure with a reason");
        };
        assert!(reason.contains("CORE_RL_MagickCore_.dll"), "{reason}");
        assert!(reason.contains("Access is denied."), "{reason}");
    }

    #[test]
    fn a_finished_install_is_success_and_a_log_without_a_verdict_is_unfinished() {
        let ok = "2026-10-06 14:02:10.101   Setup version: Inno Setup version 7.1.0\n\
                  2026-10-06 14:02:20.000   Installation process succeeded.\n\
                  2026-10-06 14:02:21.000   Deinitializing Setup.\n";
        assert_eq!(setup_log_outcome(ok), SetupOutcome::Succeeded);
        let running = "2026-10-06 14:02:10.101   Setup version: Inno Setup version 7.1.0\n";
        assert_eq!(setup_log_outcome(running), SetupOutcome::Unfinished);
    }

    /// The decision the Settings launch acts on: an update that landed (or finished) is never
    /// reported, a failed one is reported with its reason, and one with no verdict waits until
    /// setup cannot possibly still be running.
    #[test]
    fn attempt_verdicts() {
        let t0 = 1_000_000;
        // Landed: the running build is the tag (or newer), whatever the log says.
        assert_eq!(
            attempt_verdict(t0, t0 + 5, "3.7.0", "3.7.0", Some(ROLLED_BACK)),
            Verdict::Forget
        );
        // Failed: reported at once, with setup's reason.
        assert!(matches!(
            attempt_verdict(t0, t0 + 5, "3.7.0", "3.6.0", Some(ROLLED_BACK)),
            Verdict::Report(Some(r)) if r.contains("Access is denied.")
        ));
        // Setup finished but a file waits for a restart: the --updated toast already said so.
        let ok = "2026-10-06 14:02:20.000   Installation process succeeded.\n";
        assert_eq!(
            attempt_verdict(t0, t0 + 5, "3.7.0", "3.6.0", Some(ok)),
            Verdict::Forget
        );
        // No verdict, no log: wait, then report once setup cannot still be running.
        assert_eq!(
            attempt_verdict(t0, t0 + 60, "3.7.0", "3.6.0", None),
            Verdict::Keep
        );
        assert_eq!(
            attempt_verdict(t0, t0 + GIVE_UP_SECS, "3.7.0", "3.6.0", None),
            Verdict::Report(None)
        );
    }
}
