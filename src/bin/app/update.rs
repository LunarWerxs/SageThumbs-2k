//! "Check for updates" — ask the GitHub releases API for the latest tag and compare it
//! to the running build. Reuses the sponsor fetch (WinINet HTTPS, bounded timeout, and
//! the `SageThumbs2K` User-Agent the GitHub API requires). Best-effort: any failure
//! (offline, repo renamed/moved, no releases yet, rate-limited) becomes `Failed`, so the
//! UI can fall back to "couldn't reach the update server — check GitHub manually."

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sponsors::http_fetch;

/// The GitHub "latest release" endpoint for this repo.
const RELEASES_API: &str = "https://api.github.com/repos/LunarWerxs/SageThumbs-2k/releases/latest";

/// Where the user is pointed to check / download by hand (also the README badge target).
pub(crate) const RELEASES_URL: &str = "https://github.com/LunarWerxs/SageThumbs-2k/releases";

/// Settings-panel custom message (`WM_APP + 8`; `WM_APP_SPONSORS` is `+7`): the lazy
/// background check found a newer release. Posted from the worker; the dialog turns the
/// "Check for updates" button into a quiet nudge. Carries a `Box<String>` (the tag) in
/// `LPARAM` — the handler reclaims it.
pub(crate) const WM_APP_UPDATE: u32 = 0x8000 + 8;

/// Don't hit the network more than once per this interval — a previous result (cached on
/// disk) answers in between, so opening Settings repeatedly never hammers GitHub.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) enum UpdateCheck {
    /// Running the newest published release (or newer, e.g. a dev build).
    UpToDate,
    /// A newer release exists; carries its display tag (e.g. "0.4.6").
    Available(String),
    /// Couldn't reach / parse the update server — tell the user to check manually.
    Failed,
}

/// Parse a version string ("v0.4.6", "0.4.6", "0.4.6-rc1") into `(major, minor, patch)`.
/// Tolerant: a missing minor/patch is 0; a pre-release/build suffix is dropped.
fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next().unwrap_or(core); // strip -rc1 / +build
    let mut it = core.split('.');
    let maj = it.next()?.parse::<u32>().ok()?;
    let min = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let pat = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((maj, min, pat))
}

/// Synchronously query GitHub for the latest release and compare to this build. Bounded
/// by the fetch's own per-phase timeout, so a dead network returns `Failed` quickly.
pub(crate) fn check() -> UpdateCheck {
    let Some(bytes) = http_fetch(RELEASES_API, true) else {
        return UpdateCheck::Failed;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return UpdateCheck::Failed;
    };
    // No "tag_name" → an error body (404 when there are no releases, a rate-limit notice,
    // etc.) → treat as unreachable so the UI offers the manual fallback.
    let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) else {
        return UpdateCheck::Failed;
    };
    match (parse_ver(tag), parse_ver(env!("CARGO_PKG_VERSION"))) {
        (Some(latest), Some(current)) if latest > current => {
            UpdateCheck::Available(tag.trim_start_matches(['v', 'V']).to_string())
        }
        (Some(_), Some(_)) => UpdateCheck::UpToDate,
        _ => UpdateCheck::Failed, // unparseable tag — don't guess
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The tiny throttle/cache file ("`<unix_secs>\n<latest_tag>\n`"), next to the diagnostics
/// log in `%LOCALAPPDATA%`.
fn cache_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("SageThumbs2K-update.txt"))
}

fn read_cache() -> Option<(u64, String)> {
    let text = std::fs::read_to_string(cache_path()?).ok()?;
    let mut lines = text.lines();
    let secs = lines.next()?.trim().parse::<u64>().ok()?;
    let tag = lines.next()?.trim().to_string();
    (!tag.is_empty()).then_some((secs, tag))
}

fn write_cache(secs: u64, tag: &str) {
    if let Some(p) = cache_path() {
        let _ = std::fs::write(p, format!("{secs}\n{tag}\n"));
    }
}

/// Is `tag` strictly newer than the running build?
fn is_newer(tag: &str) -> bool {
    matches!(
        (parse_ver(tag), parse_ver(env!("CARGO_PKG_VERSION"))),
        (Some(latest), Some(current)) if latest > current
    )
}

/// Kick off a LAZY, THROTTLED, background update check. Runs entirely on a worker thread
/// (never blocks the Settings window opening), hits the network at most once per
/// [`CHECK_INTERVAL`] — answering from the on-disk cache in between — and is SILENT unless
/// a newer version is known, in which case it calls `on_newer(tag)` from the worker thread
/// (the caller marshals to the UI, e.g. via `PostMessage`). Up-to-date / offline never nag.
pub(crate) fn lazy_check<F: FnOnce(String) + Send + 'static>(on_newer: F) {
    std::thread::spawn(move || {
        let now = now_secs();
        // Within the interval: answer from the cache (no network), but still nudge about a
        // previously-found update so the user isn't left unaware between checks.
        if let Some((last, tag)) = read_cache() {
            if now.saturating_sub(last) < CHECK_INTERVAL.as_secs() {
                if is_newer(&tag) {
                    on_newer(tag);
                }
                return;
            }
        }
        // Stale or first run: one real check. Cache a definitive result (up-to-date or a
        // newer tag) so we don't re-hit for a day; on a transient failure leave the cache
        // untouched so the NEXT Settings open retries instead of waiting out the interval.
        match check() {
            UpdateCheck::Available(tag) => {
                write_cache(now, &tag);
                on_newer(tag);
            }
            UpdateCheck::UpToDate => write_cache(now, env!("CARGO_PKG_VERSION")),
            UpdateCheck::Failed => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::parse_ver;

    #[test]
    fn parses_and_orders_versions() {
        assert_eq!(parse_ver("v0.4.6"), Some((0, 4, 6)));
        assert_eq!(parse_ver("0.4.5"), Some((0, 4, 5)));
        assert_eq!(parse_ver("V1.0"), Some((1, 0, 0)));
        assert_eq!(parse_ver("2"), Some((2, 0, 0)));
        assert_eq!(parse_ver("0.4.6-rc1"), Some((0, 4, 6)));
        assert_eq!(parse_ver("0.5.0+build7"), Some((0, 5, 0)));
        assert_eq!(parse_ver("not-a-version"), None);

        // The ordering the check relies on (tuple compare = correct semver ordering here).
        assert!(parse_ver("0.4.6") > parse_ver("0.4.5"));
        assert!(parse_ver("0.5.0") > parse_ver("0.4.9"));
        assert!(parse_ver("1.0.0") > parse_ver("0.9.9"));
        assert!(parse_ver("0.4.5") <= parse_ver("0.4.5")); // equal = up to date
    }
}
