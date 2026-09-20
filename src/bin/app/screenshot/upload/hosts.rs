//! The upload hosts: the built-in list and the user's config file.

use super::*;

pub(super) const HTTPS_PORT: u16 = 443;

/// A resolved upload endpoint (owned, because it can come from the registry / config).
pub(super) struct UploadHost {
    pub(super) host: String,
    pub(super) path: String,
    /// The multipart field the file goes in.
    pub(super) field: String,
    /// Any extra form fields the host wants (e.g. catbox's `reqtype=fileupload`).
    pub(super) extra: Vec<(String, String)>,
    /// How the host returns the link: `false` → the reply IS the bare URL (x0.at,
    /// catbox); `true` → the URL is embedded in a JSON reply (uguu.se). See [`extract_url`].
    pub(super) json: bool,
}

/// The built-in keyless hosts, tried in order until one returns a URL. All are
/// no-account / no-API-key and rate-limit per end-user IP; ordered
/// **permanent-first, temporary-last**, so a normal upload gets a permanent link and
/// only falls back to an expiring one when every permanent host is down.
///
/// Built from [`sagethumbs2k_core::upload_config::BUILTIN_HOSTS`] rather than its
/// own hardcoded list, so this chain and the config template's "current built-in
/// defaults" comment can never drift apart — x0.at (currently the only *up* permanent
/// keyless host), catbox.moe (kept in the chain so uploads return to it automatically
/// once its storage issue clears; its "paused" reply just isn't a URL while it's down),
/// litterbox.catbox.moe (catbox's separate-storage 72h TEMPORARY host, the last-resort
/// permanent-operator fallback), and uguu.se (a THIRD, independent operator, ~3h temp,
/// JSON reply — `{"files":[{"url":"…"}]}` with `\/`-escaped slashes).
pub(super) fn builtin_hosts() -> Vec<UploadHost> {
    sagethumbs2k_core::upload_config::BUILTIN_HOSTS
        .iter()
        .map(|&(host, path, field, extra, json)| UploadHost {
            host: host.into(),
            path: path.into(),
            field: field.into(),
            extra: extra.iter().map(|&(k, v)| (k.into(), v.into())).collect(),
            json,
        })
        .collect()
}

/// Resolve the upload endpoint(s), in precedence order:
///
/// 1. **The config FILE** (`%APPDATA%\SageThumbs2K\upload-hosts.conf`) — when it
///    defines ≥1 host, it fully controls the chain. This is the user-facing knob.
/// 2. **The legacy HKCU single-host override** (`ScreenshotUploadUrl` /
///    `…Field` / `…Extra`) — kept for back-compat.
/// 3. **The [`builtin_hosts`] fallback chain** — the shipped default.
///
/// A user-configured host (file or registry) is **authoritative**: we use ONLY what
/// they chose and do NOT fall through to the built-ins, so a file is never sent to a
/// host they didn't pick (privacy).
///
/// Returns `Err(message)` for a misconfigured registry URL: the POST always runs over
/// TLS (port 443 + `INTERNET_FLAG_SECURE`), so an `http://` or scheme-less override
/// can't be honored as written — we reject it with a clear message instead of silently
/// treating it as HTTPS or uploading to a different host than configured. (Bad *file*
/// lines are just skipped — a file can list many hosts, so one typo shouldn't abort.)
pub(super) fn upload_hosts() -> Result<Vec<UploadHost>, String> {
    // Always make sure the self-documenting config file exists (all-commented =
    // "use the built-in defaults"), so it's there to find and edit. Path + template
    // live in the shared core module so the `st2k` CLI resolves the SAME file.
    let cfg = sagethumbs2k_core::upload_config::ensure_config();

    // 1) The config file wins when it defines any host. A file whose ACTIVE lines are all
    //    unusable is a misconfiguration, not "no configuration": the user chose a destination
    //    and it cannot be honoured, so nothing may be sent anywhere else (2026-09-19 audit
    //    F22: an `http://` typo in the only active line silently selected the public
    //    defaults). The all-commented template still means the built-ins, as documented.
    if let Some(path) = cfg {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (hosts, rejected) = parse_hosts_config(&text);
                if !hosts.is_empty() {
                    return Ok(hosts);
                }
                if !rejected.is_empty() {
                    return Err(format!(
                        "The upload-hosts file names a destination that cannot be used, and no \
                         other:\n\n{}\n\nEvery host must be an https:// URL on port 443 with no \
                         user info. Fix the line or comment it out; nothing was uploaded.\n\n{}",
                        rejected.join("\n"),
                        path.display()
                    ));
                }
            }
            Err(e) => {
                return Err(format!(
                    "The upload-hosts file exists but could not be read ({e}); nothing was \
                     uploaded.\n\n{}",
                    path.display()
                ));
            }
        }
    }

    // 2) Legacy single-host override. Routed through settings::get_string_opt (not a
    // direct CURRENT_USER open) so a portable install (marker-INI backend) reads the
    // same value it can actually set — opening the registry here would silently miss
    // a portable override and could pick up stale machine-registry state instead.
    if let Some(raw) = sagethumbs2k_core::settings::get_string_opt("ScreenshotUploadUrl") {
        let url = raw.trim().to_string();
        if !url.is_empty() {
            let Some((host, path)) = crate::http::split_https(&url) else {
                return Err(format!(
                    "Custom screenshot upload host must be a valid https:// URL on port 443 \
                     (uploads always use TLS).\n\n\
                     Got: {url}\n\nFix it in HKCU\\Software\\SageThumbs2K\\ScreenshotUploadUrl \
                     (or use the upload-hosts config file)."
                ));
            };
            let field = sagethumbs2k_core::settings::get_string_opt("ScreenshotUploadField")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "file".into());
            let extra = sagethumbs2k_core::settings::get_string_opt("ScreenshotUploadExtra")
                .filter(|s| !s.is_empty())
                .and_then(|kv| {
                    kv.split_once('=')
                        .map(|(k, v)| vec![(k.to_string(), v.to_string())])
                })
                .unwrap_or_default();
            return Ok(vec![UploadHost {
                host,
                path,
                field,
                extra,
                json: false,
            }]);
        }
    }

    // 3) Built-in fallback chain.
    Ok(builtin_hosts())
}

/// Ensure the config exists, then open it in the user's default text editor. Wired to
/// the Settings ▸ Screenshots "Edit upload hosts…" button. (Path + template come from
/// the shared `sagethumbs2k_core::upload_config` module — the `st2k` CLI opens the
/// same file.)
pub(crate) unsafe fn open_hosts_config() {
    let Some(path) = sagethumbs2k_core::upload_config::ensure_config() else {
        return;
    };
    // If we couldn't create the file for some reason, open its folder instead.
    let target = if path.exists() {
        path.display().to_string()
    } else {
        path.parent()
            .map(|d| d.display().to_string())
            .unwrap_or_default()
    };
    if target.is_empty() {
        return;
    }
    let file = wide(&target);
    let verb = wide("open");
    ShellExecuteW(
        None,
        PCWSTR(verb.as_ptr()),
        PCWSTR(file.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Parse the config file into hosts. One host per non-blank, non-`#` line:
/// `https-url | field | response | extra=val | extra2=val …`
/// where `response` is `text` (the reply IS the URL; the default) or `json` (the URL
/// is embedded in a JSON reply). Malformed lines / non-`https://` URLs are skipped for the
/// host list and returned as the second element, so the caller can tell "nothing
/// configured" from "configured, but nothing usable".
pub(super) fn parse_hosts_config(text: &str) -> (Vec<UploadHost>, Vec<String>) {
    let mut hosts = Vec::new();
    let mut rejected = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('|').map(str::trim);
        let Some(url) = parts.next() else { continue };
        let Some((host, path)) = crate::http::split_https(url) else {
            rejected.push(line.to_string());
            continue;
        };
        let field = parts
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("file")
            .to_string();
        let json = parts
            .next()
            .map(|s| s.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        let extra = parts
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            })
            .collect();
        hosts.push(UploadHost {
            host,
            path,
            field,
            extra,
            json,
        });
    }
    (hosts, rejected)
}

/// Load the configured upload hosts. On failure the list file is removed (nothing was
/// uploaded) and the outcome is reported the same way [`run_upload_keep`]'s own failures
/// are: `Err` to stderr + exit(1) for the CLI (`url_to.is_some()`), a dialog otherwise.
/// Returns `None` when the caller should stop.
pub(super) unsafe fn resolve_hosts(
    list_path: &str,
    url_to: Option<&str>,
) -> Option<Vec<UploadHost>> {
    match upload_hosts() {
        Ok(h) => Some(h),
        Err(msg) => {
            let _ = std::fs::remove_file(list_path);
            if url_to.is_some() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            notify(&msg, file_caption(), true);
            None
        }
    }
}
