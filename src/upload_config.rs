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
    Some(Path::new(&base).join("SageThumbs2K").join("upload-hosts.conf"))
}

/// The documented, ALL-COMMENTED default template. Because every host line is
/// commented out, a freshly-created file parses to zero hosts and the app keeps using
/// its built-in fallback chain (kept current each release) until the user edits a line.
pub fn template() -> &'static str {
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
# https://x0.at/ | file | text
# https://catbox.moe/user/api.php | fileToUpload | text | reqtype=fileupload
# https://litterbox.catbox.moe/resources/internals/api.php | fileToUpload | text | reqtype=fileupload | time=72h
# https://uguu.se/upload.php | files[] | json
#
# Example \u{2014} your own server (the only truly long-term-stable option):
# https://your.host/upload | file | text
"
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
