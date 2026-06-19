//! "Check for updates" — ask the GitHub releases API for the latest tag and compare it
//! to the running build. Reuses the sponsor fetch (WinINet HTTPS, bounded timeout, and
//! the `SageThumbs2K` User-Agent the GitHub API requires). Best-effort: any failure
//! (offline, repo renamed/moved, no releases yet, rate-limited) becomes `Failed`, so the
//! UI can fall back to "couldn't reach the update server — check GitHub manually."

use crate::sponsors::http_fetch;

/// The GitHub "latest release" endpoint for this repo.
const RELEASES_API: &str = "https://api.github.com/repos/LunarWerxs/SageThumbs-2k/releases/latest";

/// Where the user is pointed to check / download by hand (also the README badge target).
pub(crate) const RELEASES_URL: &str = "https://github.com/LunarWerxs/SageThumbs-2k/releases";

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
