//! The user-editable **upload-hosts config file** — its location and documented
//! template, shared so there's ONE source of truth for both consumers:
//!
//! - the app EXE (`bin/app/screenshot/upload.rs`) — reads it to build the upload
//!   chain, and the Settings ▸ Screenshots "Edit upload hosts…" button opens it;
//! - the `st2k` CLI (`st2k upload-hosts [--open]`) — prints / opens it.
//!
//! The file lives at `%APPDATA%\SageThumbs2K\upload-hosts.conf`. The parsing itself
//! stays with each consumer (the app turns lines into its own `UploadHost` type); this
//! module only owns the *path*, the *template*, and "create it if missing".

use std::path::{Path, PathBuf};

/// Path to the config: `%APPDATA%\SageThumbs2K\upload-hosts.conf` (None if `%APPDATA%`
/// is somehow unset).
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var("APPDATA").ok()?;
    Some(
        Path::new(&base)
            .join("SageThumbs2K")
            .join("upload-hosts.conf"),
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
    // catbox.moe — keyless & PERMANENT. Kept in the chain so uploads return to it
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

/// Write the [`template`] if the file doesn't exist yet (best-effort — a failure just
/// means no file to edit; uploads still run off the built-ins). Returns the resolved
/// path (whether or not the write happened), so callers can print / open it.
pub fn ensure_config() -> Option<PathBuf> {
    let path = config_path()?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, template());
    }
    Some(path)
}
