//! The list of links this machine has uploaded, and when each one stops working.
//!
//! Upload hosts delete files on their own schedule (litterbox after the 72 hours we ask for,
//! uguu.se after 3, x0.at after 3 to 100 days depending on size) and the link carries no hint
//! of it, so once the result window closed there was no way to tell whether a link shared last
//! week still worked. A user asked for exactly that on 2026-09-21. Every successful upload now
//! appends one line here, its expiry worked out from the host's published policy
//! ([`crate::upload_config::retention_for`]) at the moment of upload, and the app's "Recent
//! uploads" window and `st2k upload-history` read it back.
//!
//! One tab-separated line per upload, oldest first, in `upload-history.tsv` beside
//! `upload-hosts.conf` (so a portable copy keeps it in its own folder):
//! `<uploaded>\t<expires | nodate | unknown>\t<host>\t<url>\t<file name>`, times in Unix
//! seconds. Appended in normal use; once the file passes twice [`KEEP`] lines it is rewritten
//! atomically with only the newest [`KEEP`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::upload_config::Retention;

/// How many uploads the list remembers.
pub const KEEP: usize = 100;

const FILE_NAME: &str = "upload-history.tsv";
const HEADER: &str = "# SageThumbs 2K upload history, oldest first. Columns: uploaded, expires \
(nodate / unknown / a time), host, link, file. Times are Unix seconds.\n";

/// When an uploaded file stops being available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expiry {
    /// No expiry date (catbox.moe: removed only after 2 years without a view).
    NoDate,
    /// The host deletes it at this Unix time.
    At(u64),
    /// The host's policy is not known (a custom upload host).
    Unknown,
}

impl Expiry {
    /// The expiry of a file uploaded at `uploaded` to a host with retention `r`.
    pub fn from_retention(r: Retention, uploaded: u64) -> Self {
        match r {
            Retention::NoExpiry => Expiry::NoDate,
            Retention::Secs(s) => Expiry::At(uploaded.saturating_add(s)),
            Retention::Unknown => Expiry::Unknown,
        }
    }
}

/// One remembered upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Unix time of the upload.
    pub uploaded: u64,
    pub expires: Expiry,
    /// The host it went to (`litterbox.catbox.moe`), which is not always the link's own host.
    pub host: String,
    pub url: String,
    /// The uploaded file's name (`screenshot.png` for a capture).
    pub name: String,
}

/// Where an entry stands at a given moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Still available, for this many more seconds.
    Left(u64),
    /// Deleted by the host at this Unix time.
    Expired(u64),
    /// No expiry date.
    NoExpiry,
    /// Policy unknown.
    Unknown,
}

impl Entry {
    pub fn status(&self, now: u64) -> Status {
        match self.expires {
            Expiry::NoDate => Status::NoExpiry,
            Expiry::Unknown => Status::Unknown,
            Expiry::At(t) if t > now => Status::Left(t - now),
            Expiry::At(t) => Status::Expired(t),
        }
    }

    /// True while the link should still open: not past a known expiry.
    pub fn is_live(&self, now: u64) -> bool {
        !matches!(self.status(now), Status::Expired(_))
    }
}

/// The current Unix time (0 if the clock is before 1970).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A field with every control character (tab and line breaks included) turned into a space,
/// so no value can split its line or shift its columns. A file name reaches here unchanged
/// from `Path::file_name`, and NTFS will hold a tab in one if something wrote it through the
/// native API.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// One history line for `e`, without its line break.
pub fn format_line(e: &Entry) -> String {
    let expires = match e.expires {
        Expiry::NoDate => "nodate".to_string(),
        Expiry::Unknown => "unknown".to_string(),
        Expiry::At(t) => t.to_string(),
    };
    format!(
        "{}\t{expires}\t{}\t{}\t{}",
        e.uploaded,
        clean(&e.host),
        clean(&e.url),
        clean(&e.name)
    )
}

/// Parse one history line; `None` for a comment, a blank line, or anything malformed (a line
/// that is not ours is skipped, never a reason to lose the rest of the list).
pub fn parse_line(line: &str) -> Option<Entry> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.splitn(5, '\t');
    let uploaded = fields.next()?.parse().ok()?;
    let expires = match fields.next()? {
        "nodate" => Expiry::NoDate,
        "unknown" => Expiry::Unknown,
        t => Expiry::At(t.parse().ok()?),
    };
    let host = fields.next()?.to_string();
    let url = fields.next()?.to_string();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    let name = fields.next().unwrap_or("").to_string();
    Some(Entry {
        uploaded,
        expires,
        host,
        url,
        name,
    })
}

/// Every entry in a history file's text, NEWEST FIRST, at most [`KEEP`].
pub fn parse(text: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = text.lines().filter_map(parse_line).collect();
    entries.reverse();
    entries.truncate(KEEP);
    entries
}

/// The history file: beside `upload-hosts.conf`, so it follows the same portable/installed
/// split ([`crate::upload_config::config_path`]).
pub fn history_path() -> Option<PathBuf> {
    crate::upload_config::config_path().map(|p| p.with_file_name(FILE_NAME))
}

/// Remember an upload. Best-effort: a history that cannot be written must never turn a
/// successful upload into a failure, so the result is only a hint.
pub fn record(e: &Entry) -> bool {
    history_path().is_some_and(|p| record_at(&p, e))
}

/// [`record`] against an explicit path (the testable half).
pub fn record_at(path: &Path, e: &Entry) -> bool {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut text = String::new();
    if !path.exists() {
        text.push_str(HEADER);
    }
    text.push_str(&format_line(e));
    text.push('\n');
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .is_ok();
    if written {
        trim_at(path);
    }
    written
}

/// Rewrite the file with only its newest [`KEEP`] entries once it holds more than twice that
/// many lines. Atomic, so a crash mid-rewrite leaves the old list, never half of one.
fn trim_at(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    if text.lines().count() <= 2 * KEEP {
        return;
    }
    let mut out = String::from(HEADER);
    for e in parse(&text).iter().rev() {
        out.push_str(&format_line(e));
        out.push('\n');
    }
    let _ = crate::fsutil::write_atomically(path, out.as_bytes());
}

/// The remembered uploads, newest first (empty when there is no history yet).
pub fn load() -> Vec<Entry> {
    history_path().map_or_else(Vec::new, |p| load_at(&p))
}

/// [`load`] from an explicit path.
pub fn load_at(path: &Path) -> Vec<Entry> {
    std::fs::read_to_string(path)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// The newest remembered upload of `url`, if any.
pub fn find(url: &str) -> Option<Entry> {
    load().into_iter().find(|e| e.url == url)
}

/// The words a remaining time is written with, one template per shape. `{d}`, `{h}` and `{m}`
/// are days, hours and minutes. Separate templates rather than a unit word each, so a language
/// can order and space them its own way ("3日4時間").
pub struct DurationWords<'a> {
    pub days: &'a str,
    pub days_hours: &'a str,
    pub hours: &'a str,
    pub hours_minutes: &'a str,
    pub minutes: &'a str,
}

/// The CLI's words (the app passes its translated ones).
pub const ENGLISH: DurationWords<'static> = DurationWords {
    days: "{d} d",
    days_hours: "{d} d {h} h",
    hours: "{h} h",
    hours_minutes: "{h} h {m} min",
    minutes: "{m} min",
};

/// `secs` as its two largest units: "3 d", "2 d 23 h", "5 h 10 min", "4 min". Minutes round
/// UP, so a link with 20 seconds left reads "1 min" rather than "0 min", and a fresh 72-hour
/// upload reads "3 d" rather than "2 d 23 h".
pub fn duration_text(secs: u64, w: &DurationWords) -> String {
    let total_min = secs.div_ceil(60).max(1);
    let (d, h, m) = (total_min / 1440, total_min % 1440 / 60, total_min % 60);
    let template = match (d, h, m) {
        (0, 0, _) => w.minutes,
        (0, _, 0) => w.hours,
        (0, _, _) => w.hours_minutes,
        (_, 0, _) => w.days,
        _ => w.days_hours,
    };
    template
        .replace("{d}", &d.to_string())
        .replace("{h}", &h.to_string())
        .replace("{m}", &m.to_string())
}

/// `unix_secs` as local "YYYY-MM-DD HH:MM", the same shape the Quick preview's info card
/// uses for a file's modified time (empty if the conversion fails).
pub fn local_datetime(unix_secs: u64) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // FILETIME ticks are 100 ns since 1601-01-01; the Unix epoch is 11_644_473_600 s later.
    let ticks = unix_secs
        .saturating_add(11_644_473_600)
        .saturating_mul(10_000_000);
    let ft = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    // SAFETY: both calls only read and write the stack structs passed to them.
    if unsafe { FileTimeToSystemTime(&ft, &mut utc) }.is_err() {
        return String::new();
    }
    let mut local = utc;
    unsafe {
        let _ = SystemTimeToTzSpecificLocalTime(None, &utc, &mut local);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}

/// One English line for `st2k`: "expires 2026-09-24 23:50 (in 3 d)", "expired ...",
/// "no expiry date (...)" or "expiry unknown".
pub fn status_text_en(e: &Entry, now: u64) -> String {
    match e.status(now) {
        Status::Left(left) => format!(
            "expires {} (in {})",
            local_datetime(now.saturating_add(left)),
            duration_text(left, &ENGLISH)
        ),
        Status::Expired(at) => format!("expired {}", local_datetime(at)),
        Status::NoExpiry => "no expiry date (removed after 2 years unopened)".to_string(),
        Status::Unknown => "expiry unknown (custom host)".to_string(),
    }
}

#[cfg(test)]
mod tests;
