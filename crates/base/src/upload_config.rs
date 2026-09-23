//! The user-editable **upload-hosts config file** — its location and documented
//! template, shared so there's ONE source of truth for both consumers:
//!
//! - the app EXE (`bin/app/screenshot/upload.rs`) — reads it to build the upload
//!   chain, and the Settings ▸ Screenshots "Edit upload hosts…" button opens it;
//! - the `st2k` CLI (`st2k upload-hosts [--open]`) — prints / opens it.
//!
//! The file lives at `%APPDATA%\SageThumbs2K\upload-hosts.conf` normally, or beside the
//! portable ini when running portable (a portable copy must not leave anything in the host
//! profile - same split `settings.rs`/`update.rs::cache_path()` already apply). The parsing
//! itself stays with each consumer (the app turns lines into its own `UploadHost` type); this
//! module only owns the *path*, the *template*, and "create it if missing".

use std::path::{Path, PathBuf};

/// Pure path derivation, over an already-resolved portable-ini path and `%APPDATA%` value -
/// no registry or env access here, so it's unit-testable directly. Beside the portable ini's
/// directory when `ini` is `Some` (mirrors `update.rs`'s `cache_path()` - a portable copy
/// must not leave anything in the host's `%APPDATA%`); `%APPDATA%\SageThumbs2K\...` otherwise.
fn resolve_config_path(ini: Option<&Path>, appdata: Option<&str>) -> Option<PathBuf> {
    if let Some(ini) = ini {
        return ini.parent().map(|d| d.join("upload-hosts.conf"));
    }
    appdata.map(|base| {
        Path::new(base)
            .join("SageThumbs2K")
            .join("upload-hosts.conf")
    })
}

/// Path to the config: beside the portable ini when running portable; otherwise
/// `%APPDATA%\SageThumbs2K\upload-hosts.conf` (`None` if `%APPDATA%` is somehow unset).
pub fn config_path() -> Option<PathBuf> {
    resolve_config_path(
        crate::settings::ini_path().map(PathBuf::as_path),
        std::env::var("APPDATA").ok().as_deref(),
    )
}

/// The built-in keyless upload chain: the single source of truth both
/// `upload.rs`'s `builtin_hosts()` and [`template`]'s "current built-in defaults" comment
/// build from, so the two can no longer drift the way they did when the template hard-coded
/// its own copy of the four host lines as plain doc-comment text — a chain reorder or a
/// dropped host (this module's `upload.rs` sibling already narrates x0.at/catbox/uguu
/// outages) left that hard-coded comment stale, and since the file is written once at first
/// run, a user who followed "uncomment to pin them" could pin an already-outdated chain
/// that survived every later upgrade.
///
/// Each entry is `(host, path, field, extra_fields, json_reply)` — see
/// `upload.rs::UploadHost` for what each means; `json_reply` is whether the host embeds the
/// link in a JSON reply (`true`) or returns it as the bare response body (`false`).
pub type BuiltinHost = (
    &'static str,
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
    bool,
);

pub const BUILTIN_HOSTS: &[BuiltinHost] = &[
    // x0.at — 0x0-style keyless host; plain-text URL, field `file`, no extra fields.
    // Retention scales with size (small screenshots are effectively long-lived).
    ("x0.at", "/", "file", &[], false),
    // catbox.moe — keyless, no expiry date (its FAQ: an anonymous upload is removed only after
    // 2 years without a view). Kept in the chain so uploads return to it
    // automatically once its storage issue is resolved; it's simply skipped (its "paused"
    // reply isn't a URL) while it's down.
    (
        "catbox.moe",
        "/user/api.php",
        "fileToUpload",
        &[("reqtype", "fileupload")],
        false,
    ),
    // litterbox.catbox.moe — catbox's TEMPORARY host (separate storage), 72h max.
    // Last-resort permanent-operator fallback: a working 72-hour link beats a failed upload.
    (
        "litterbox.catbox.moe",
        "/resources/internals/api.php",
        "fileToUpload",
        &[("reqtype", "fileupload"), ("time", "72h")],
        false,
    ),
    // uguu.se — a THIRD, independent operator (not x0 / not catbox), so a full outage of
    // one operator can't take the whole chain down. Keyless, ~3h temp, JSON reply.
    ("uguu.se", "/upload.php", "files[]", &[], true),
];

/// How long a host keeps an uploaded file, according to the host's own published policy.
/// The link itself says nothing about this, so without it a user has no way to tell a
/// 3-hour link from one with no expiry date (a user asked for exactly this, 2026-09-21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retention {
    /// No expiry date. The one host with it, catbox.moe, removes an anonymous upload only after
    /// it goes 2 years without a single view (its FAQ, read 2026-09-21), and the strings say so.
    NoExpiry,
    /// The host deletes the file this many seconds after the upload.
    Secs(u64),
    /// A host whose policy we don't know - a custom line in the config file.
    Unknown,
}

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

/// x0.at's published rule (read off https://x0.at/ on 2026-09-21): a file is kept for
/// `MIN_AGE + (MAX_AGE - MIN_AGE) * (1 - FILE_SIZE / MAX_SIZE)^2`, with a 3-day minimum, a
/// 100-day maximum and a 1024 MiB size cap. A screenshot therefore lives about 100 days and a
/// file near the cap about 3.
fn x0_retention_secs(size: u64) -> u64 {
    const MIN_AGE: f64 = (3 * DAY) as f64;
    const MAX_AGE: f64 = (100 * DAY) as f64;
    const MAX_SIZE: f64 = 1024.0 * 1024.0 * 1024.0;
    let frac = (size as f64 / MAX_SIZE).min(1.0);
    (MIN_AGE + (MAX_AGE - MIN_AGE) * (1.0 - frac).powi(2)) as u64
}

/// litterbox's `time` form field: its upload page offers `1h`, `12h`, `24h` and `72h`. Read as
/// `<n>h` / `<n>d` / `<n>w` rather than matched against those four, so a user's config line
/// with a value the host adds later still reports the right time. Anything else is `None`.
fn parse_time_field(value: &str) -> Option<u64> {
    let v = value.trim().to_ascii_lowercase();
    let (num, unit) = [("h", HOUR), ("d", DAY), ("w", 7 * DAY)]
        .iter()
        .find_map(|&(suffix, unit)| v.strip_suffix(suffix).map(|n| (n.trim(), unit)))?;
    num.parse::<u64>()
        .ok()
        .filter(|&n| n > 0)?
        .checked_mul(unit)
}

/// What `host` does with a `size`-byte upload sent with the form fields `extra`. Keyed on the
/// host NAME rather than on [`BUILTIN_HOSTS`], so a line the user copied into the config file
/// (say litterbox with `time=24h`) reports its real expiry too. Each policy is the host's own
/// published one, checked 2026-09-21: litterbox deletes at the `time` it was asked for, uguu.se
/// after 3 hours ("files expire after 3 hours"), x0.at by size ([`x0_retention_secs`]), and
/// catbox.moe has no expiry date (it removes an anonymous upload after 2 years without a view).
pub fn retention_for<K: AsRef<str>, V: AsRef<str>>(
    host: &str,
    extra: &[(K, V)],
    size: u64,
) -> Retention {
    match host.to_ascii_lowercase().as_str() {
        "catbox.moe" => Retention::NoExpiry,
        "litterbox.catbox.moe" => extra
            .iter()
            .find(|(k, _)| k.as_ref().eq_ignore_ascii_case("time"))
            .and_then(|(_, v)| parse_time_field(v.as_ref()))
            .map_or(Retention::Unknown, Retention::Secs),
        "uguu.se" => Retention::Secs(3 * HOUR),
        "x0.at" => Retention::Secs(x0_retention_secs(size)),
        _ => Retention::Unknown,
    }
}

/// One `<https-url> | <field> | <response> | <extra=value> ...` config-file line for a
/// [`BUILTIN_HOSTS`] entry, in the exact syntax [`template`]'s own FORMAT section documents.
fn builtin_host_line(
    host: &str,
    path: &str,
    field: &str,
    extra: &[(&str, &str)],
    json: bool,
) -> String {
    let mut line = format!(
        "https://{host}{path} | {field} | {}",
        if json { "json" } else { "text" }
    );
    for (k, v) in extra {
        line.push_str(&format!(" | {k}={v}"));
    }
    line
}

/// The documented, ALL-COMMENTED default template. Because every host line is
/// commented out, a freshly-created file parses to zero hosts and the app keeps using
/// its built-in fallback chain (kept current each release) until the user edits a line.
///
/// The "current built-in defaults" block is generated from [`BUILTIN_HOSTS`]
/// rather than hand-copied, so it can never show a chain the app doesn't actually use.
pub fn template() -> String {
    let mut defaults = String::new();
    for &(host, path, field, extra, json) in BUILTIN_HOSTS {
        defaults.push_str("# ");
        defaults.push_str(&builtin_host_line(host, path, field, extra, json));
        defaults.push('\n');
    }
    format!(
        "\
# SageThumbs 2K \u{2014} upload hosts
#
# The right-click \"Upload\" verb and the screenshot \"Upload\" button POST your file to
# a keyless (no-account, no-API-key) host and copy the returned link to your clipboard.
# Edit this file to choose / reorder / add hosts. Hosts are tried TOP-TO-BOTTOM until
# one returns a link.
#
# FORMAT \u{2014} one host per line:
#   <https-url> | <field> | <response> | <extra=value> | <extra=value> ...
#     https-url : the POST endpoint. MUST start with https:// (uploads always use TLS).
#     field     : the multipart form-field the file goes in.
#     response  : \"text\" = the reply IS the bare link (default) | \"json\" = the link is
#                 embedded in a JSON reply (the first https link in the body is used).
#     extra=val : optional extra form-fields the host requires (repeat as needed).
#   Lines starting with # and blank lines are ignored.
#
# While EVERY line here is commented out, SageThumbs 2K uses its BUILT-IN defaults
# (kept current with each release). Uncomment / edit lines below to take over.
#
# The current built-in defaults (uncomment to pin them, or use as a template):
#
{defaults}#
# Example \u{2014} your own server (the only truly long-term-stable option):
# https://your.host/upload | file | text
"
    )
}

/// Write the [`template`] at `path` if it doesn't exist yet (best-effort - a failure just
/// means no file to edit; uploads still run off the built-ins). Returns `path` unchanged, so
/// callers can print / open it. Split out from [`ensure_config`] so the round trip is
/// testable against a scratch path without going through the real portable-ini resolution.
fn ensure_config_at(path: PathBuf) -> PathBuf {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, template());
    }
    path
}

/// Write the [`template`] if the file doesn't exist yet. Returns the resolved path (whether
/// or not the write happened), so callers can print / open it.
pub fn ensure_config() -> Option<PathBuf> {
    Some(ensure_config_at(config_path()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_config_path_prefers_the_portable_ini_dir() {
        let ini = Path::new(r"C:\Portable\SageThumbs2K.ini");
        assert_eq!(
            resolve_config_path(Some(ini), Some(r"C:\Users\someone\AppData\Roaming")),
            Some(PathBuf::from(r"C:\Portable\upload-hosts.conf")),
        );
    }

    #[test]
    fn resolve_config_path_falls_back_to_appdata_when_not_portable() {
        assert_eq!(
            resolve_config_path(None, Some(r"C:\Users\someone\AppData\Roaming")),
            Some(PathBuf::from(
                r"C:\Users\someone\AppData\Roaming\SageThumbs2K\upload-hosts.conf"
            )),
        );
    }

    #[test]
    fn resolve_config_path_is_none_when_neither_is_available() {
        assert_eq!(resolve_config_path(None, None), None);
    }

    #[test]
    fn every_builtin_host_has_a_known_retention() {
        // A host added to the chain without a policy here would upload fine and then show no
        // expiry at all, which is the exact gap the retention table exists to close.
        for &(host, _, _, extra, _) in BUILTIN_HOSTS {
            assert_ne!(
                retention_for(host, extra, 1_000_000),
                Retention::Unknown,
                "{host} has no retention policy"
            );
        }
    }

    #[test]
    fn litterbox_expires_at_the_time_it_was_asked_for() {
        let lb = |t: &str| retention_for("litterbox.catbox.moe", &[("time", t)], 10);
        assert_eq!(lb("72h"), Retention::Secs(72 * HOUR));
        assert_eq!(lb("1h"), Retention::Secs(HOUR));
        assert_eq!(lb(" 24H "), Retention::Secs(24 * HOUR));
        assert_eq!(lb("2d"), Retention::Secs(2 * DAY));
        assert_eq!(lb("forever"), Retention::Unknown);
        assert_eq!(lb("0h"), Retention::Unknown);
        assert_eq!(lb("h"), Retention::Unknown);
        assert_eq!(lb("1é"), Retention::Unknown);
        let no_time: &[(&str, &str)] = &[("reqtype", "fileupload")];
        assert_eq!(
            retention_for("litterbox.catbox.moe", no_time, 10),
            Retention::Unknown
        );
        // The shipped chain asks for 72 hours.
        let shipped = BUILTIN_HOSTS
            .iter()
            .find(|h| h.0 == "litterbox.catbox.moe")
            .expect("litterbox is in the chain");
        assert_eq!(
            retention_for(shipped.0, shipped.3, 10),
            Retention::Secs(72 * HOUR)
        );
    }

    #[test]
    fn x0_follows_its_published_size_formula() {
        let x0 = |size: u64| match retention_for::<&str, &str>("x0.at", &[], size) {
            Retention::Secs(s) => s,
            other => panic!("x0.at must be timed, got {other:?}"),
        };
        assert_eq!(x0(0), 100 * DAY);
        assert_eq!(x0(1024 * 1024 * 1024), 3 * DAY);
        assert_eq!(x0(u64::MAX), 3 * DAY, "past the cap clamps to the minimum");
        // Half the cap: 3 + 97 * 0.25 = 27.25 days.
        assert_eq!(x0(512 * 1024 * 1024), 27 * DAY + 6 * HOUR);
        // A 2 MB screenshot keeps nearly the full 100 days.
        assert!(x0(2_000_000) > 99 * DAY);
    }

    #[test]
    fn the_other_hosts_have_no_expiry_three_hours_or_unknown() {
        let none: &[(&str, &str)] = &[];
        assert_eq!(retention_for("catbox.moe", none, 5), Retention::NoExpiry);
        assert_eq!(retention_for("CATBOX.MOE", none, 5), Retention::NoExpiry);
        assert_eq!(retention_for("uguu.se", none, 5), Retention::Secs(3 * HOUR));
        assert_eq!(retention_for("your.host", none, 5), Retention::Unknown);
    }

    #[test]
    fn ensure_config_at_writes_and_round_trips_through_the_portable_branch() {
        let dir =
            std::env::temp_dir().join(format!("st2k-upload-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ini = dir.join("SageThumbs2K.ini");
        let path = resolve_config_path(Some(&ini), None).expect("portable branch always resolves");

        let written = ensure_config_at(path.clone());
        assert_eq!(written, path);
        assert_eq!(path, dir.join("upload-hosts.conf"));

        let contents =
            std::fs::read_to_string(&path).expect("ensure_config_at should have written the file");
        assert_eq!(contents, template());

        // Second call must not clobber a user's edits.
        std::fs::write(&path, "# user-edited\n").expect("overwrite for round-trip check");
        ensure_config_at(path.clone());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# user-edited\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
