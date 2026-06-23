//! The remote sponsor-banner system (security-hardened).
//!
//! A small JSON manifest, fetched at runtime over HTTPS (WinINet), is the SINGLE
//! source of truth: it both lists the sponsors (image / link / tip) AND gates the
//! whole feature. The banner appears **only** when the manifest is reachable, not
//! disabled, and lists at least one sponsor — otherwise nothing shows. So:
//!   - feed unreachable (offline, DNS/TLS fail, timeout) → OFF (fail-safe);
//!   - feed reachable but `"enabled": false` → OFF (remote kill switch);
//!   - feed reachable with no sponsors → OFF;
//!   - feed reachable + enabled + ≥1 sponsor → the banner shows.
//!
//! There is no local on/off file: the URL for the ads lives in the feed, so if we
//! can't read the feed there's nothing (and no reason) to show. The manifest is
//! fetched ONCE per app run — synchronously (bounded by a short timeout) for the
//! startup layout decision, then reused from cache for the async image decode, so
//! the window opens at the right size with no reflow and the UI never hangs.
//!
//! All remote input is treated as hostile — `http(s)`-only URL guards, an
//! HTTPS-only fetch path, and a hard byte cap — so a compromised feed can't open a
//! local file, launch a protocol handler, or exhaust memory.

use core::ffi::c_void;
use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::{set_static_bitmap, wide, URL_PRODUCT};

/// Remote sponsor manifest — a small JSON file, loaded at runtime so the sponsors
/// can change without a rebuild. Schema:
/// `{ "enabled": bool, "rotate_seconds": N, "random": bool,
///    "sponsors": [ { "image": url | [url, …], "text": tip, "link": url }, … ] }`.
/// `enabled` defaults to true when absent; set it `false` as a remote kill switch.
/// Each sponsor carries one click `link` + one hover `text`, and either a single
/// `image` or a list of them — when it's a list, a random one is shown each time
/// that sponsor comes up.
///
/// The startup fetch appends a small, **non-identifying** query string
/// (`?v=<app-version>&os=<win-generation-build>&new=<0|1>`) — see [`manifest_bytes`]. No
/// identifier is sent; `new=1` is a one-shot "fresh install" marker, not a per-machine id.
pub(crate) const BANNER_URL: &str = "https://st2k.lunarwerx.com/sponsor";

/// Banner default artwork, embedded so the reserved banner area shows *something*
/// while the real sponsor images download (only ever displayed once the feed has
/// already confirmed sponsors exist — see [`sponsors_enabled`]). A `banner.png`
/// dropped next to the EXE overrides at runtime (user-swappable).
pub(crate) const BANNER_PNG: &[u8] = include_bytes!("../../../assets/banner.png");

/// Max bytes we'll pull for ANY remote banner asset (manifest JSON or image). A
/// 440×56 banner and a small manifest are tiny; this is a hostile-input cap, not
/// a real limit — an over-cap response is treated as a failed fetch.
const MAX_REMOTE_BYTES: usize = 4 * 1024 * 1024;

/// How long the startup manifest check may block before we give up and treat the
/// feed as unreachable (→ sponsors off). A small text file off a CDN is well under
/// this; the bound just stops a slow/dead network from freezing the Settings open.
const MANIFEST_TIMEOUT_SECS: u64 = 5;

/// The remote manifest bytes for this app run, fetched at most once and cached
/// (including the "couldn't reach it" result as `None`). The fetch is run on a
/// helper thread and waited on with a [`MANIFEST_TIMEOUT_SECS`] bound, so a hung
/// network can't block the (synchronous) caller indefinitely. Shared by the
/// startup gate ([`sponsors_enabled`]) and the async decode ([`spawn_remote_sponsors`]).
fn manifest_bytes() -> Option<&'static [u8]> {
    static CACHE: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            // Append the (non-identifying) params: app version, OS generation+build, and
            // a one-shot `new=1` the FIRST time this install ever reports. No identifier,
            // no IP (the server sees that from the connection itself), nothing per-user.
            let is_new = !sagethumbs2k_core::settings::install_reported();
            // On a fresh report, a leftover "tombstone" version (left by a prior uninstall)
            // marks this as a reinstall rather than a first-time install — note that plus the
            // version it came from. Still no identifier; just a one-shot returning-vs-new flag.
            let prev = is_new.then(sagethumbs2k_core::settings::tombstone_version).flatten();
            let reinstall = match &prev {
                Some(v) => format!("&reinstall=1&prev={v}"),
                None => String::new(),
            };
            let url = format!(
                "{BANNER_URL}?v={}&os={}&new={}{}",
                env!("CARGO_PKG_VERSION"),
                os_tag(),
                u8::from(is_new),
                reinstall,
            );
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let res = http_fetch(&url, true);
                // Burn the one-shot "fresh install" marker — and the reinstall tombstone —
                // only once the report has actually reached the server (an offline first run
                // retries next time).
                if is_new && res.is_some() {
                    sagethumbs2k_core::settings::set_install_reported();
                    sagethumbs2k_core::settings::clear_tombstone();
                }
                let _ = tx.send(res);
            });
            // recv_timeout → Err on timeout; a still-running fetch is abandoned
            // (its eventual send hits a dropped receiver and is discarded).
            rx.recv_timeout(std::time::Duration::from_secs(MANIFEST_TIMEOUT_SECS)).ok().flatten()
        })
        .as_deref()
}

/// A compact, URL-safe OS tag for the beacon — the Windows generation + build number
/// (e.g. `win11-22631`), read from `HKLM\…\CurrentVersion`. Describes the OS, not the
/// user: it carries no identifier and can't single anyone out. Falls back to a `0`
/// build if the key can't be read.
pub(crate) fn os_tag() -> String {
    use windows_registry::LOCAL_MACHINE;
    let build: u32 = LOCAL_MACHINE
        .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()
        .and_then(|k| k.get_string("CurrentBuild").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let gen = if build >= 22000 { "win11" } else { "win10" };
    format!("{gen}-{build}")
}

/// The remote gate for the whole sponsor banner. **Off unless the feed is reachable
/// AND not disabled AND lists at least one sponsor.** Called synchronously at startup
/// (and again, from cache, when the dialog lays out) to decide whether to reserve the
/// banner's space — so a disabled/unreachable feed leaves no empty gap.
pub(crate) fn sponsors_enabled() -> bool {
    manifest_bytes().is_some_and(manifest_has_sponsors)
}

/// Pure decision from raw manifest bytes: reachable bytes that parse, are not
/// explicitly `"enabled": false`, and carry a non-empty sponsor list (preferring the
/// `sponsors` key, falling back to the legacy `ads` key). Everything else — parse
/// error, kill-switched, empty/missing list — is off. Pure so it can be unit-tested.
fn manifest_has_sponsors(bytes: &[u8]) -> bool {
    let Ok(m) = serde_json::from_slice::<serde_json::Value>(bytes) else { return false };
    if m.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        return false;
    }
    m.get("sponsors")
        .or_else(|| m.get("ads"))
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty())
}

/// Posted from the manifest-decode thread once every sponsor's art is decoded
/// (wParam = banner HWND, lParam = `*mut SponsorRotator`); installed on the UI thread.
pub(crate) const WM_APP_SPONSORS: u32 = 0x8000 + 7; // WM_APP + 7
pub(crate) const TIMER_BANNER: usize = 1; // animates the current image's GIF frames
pub(crate) const TIMER_ROTATE: usize = 2; // advances to the next sponsor / image

/// One decoded piece of art: a single HBITMAP for a still image, or many for an
/// animated GIF (with the inter-frame `delay_ms`).
pub(crate) struct SponsorImage {
    pub(crate) frames: Vec<isize>, // one HBITMAP handle per frame
    pub(crate) delay_ms: u32,      // GIF inter-frame delay (ignored for stills)
}

/// One sponsor in the rotation: one or more interchangeable images, a hover `tip`,
/// and a click `link` (both NUL-terminated wide, ready for the tooltip /
/// ShellExecute). With several images, a random one is shown per appearance.
pub(crate) struct Sponsor {
    pub(crate) images: Vec<SponsorImage>,
    pub(crate) tip: Vec<u16>,
    pub(crate) link: Vec<u16>,
}

/// The banner's sponsor-rotation state, owned by the banner control via GWLP_USERDATA
/// and freed on WM_DESTROY. TIMER_ROTATE advances the sponsor (random or in order)
/// and re-picks its image; TIMER_BANNER animates the current image while it's a GIF.
pub(crate) struct SponsorRotator {
    pub(crate) sponsors: Vec<Sponsor>,
    pub(crate) cur: usize,   // current sponsor
    pub(crate) img: usize,   // current image within the sponsor
    pub(crate) frame: usize, // current GIF frame within the image
    pub(crate) rotate_ms: u32,
    random: bool,
    rng: u32, // xorshift state for random sponsor + image picks
}

impl SponsorRotator {
    /// Build from decoded sponsors. When `random`, start on a random sponsor so a
    /// fresh open doesn't always show sponsor #0 (the bug where the banner looked
    /// "stuck").
    fn new(sponsors: Vec<Sponsor>, rotate_ms: u32, random: bool, mut rng: u32) -> Self {
        let cur = if random && sponsors.len() > 1 { (xorshift(&mut rng) as usize) % sponsors.len() } else { 0 };
        let mut r = Self { sponsors, cur, img: 0, frame: 0, rotate_ms, random, rng };
        r.pick_image();
        r
    }

    /// Pick a (random, if several) image within the current sponsor; reset to its
    /// first frame.
    fn pick_image(&mut self) {
        self.frame = 0;
        let m = self.sponsors.get(self.cur).map_or(0, |a| a.images.len());
        self.img = if m > 1 { (xorshift(&mut self.rng) as usize) % m } else { 0 };
    }

    /// Advance to the next sponsor (random avoids an immediate repeat; otherwise in
    /// order) and re-pick its image. A lone sponsor just re-rolls its own images.
    pub(crate) fn advance(&mut self) {
        let n = self.sponsors.len();
        if n > 1 {
            self.cur = if self.random {
                let mut k = (xorshift(&mut self.rng) as usize) % n;
                if k == self.cur {
                    k = (k + 1) % n;
                }
                k
            } else {
                (self.cur + 1) % n
            };
        }
        self.pick_image();
    }

    /// The image currently on display (its frames + delay).
    fn current(&self) -> Option<&SponsorImage> {
        self.sponsors.get(self.cur).and_then(|a| a.images.get(self.img))
    }

    /// Whether anything actually rotates: more than one sponsor, or any sponsor
    /// with more than one image.
    pub(crate) fn rotates(&self) -> bool {
        self.sponsors.len() > 1 || self.sponsors.iter().any(|a| a.images.len() > 1)
    }
}

/// xorshift32 — enough randomness to shuffle sponsor order without an RNG crate.
fn xorshift(s: &mut u32) -> u32 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *s = x;
    x
}

/// `http(s)://` only — the sole schemes we'll hand to `ShellExecuteW`. Rejects
/// `file:`, UNC (`\\host\…`), `javascript:` / custom protocol handlers, and any
/// control char, so a compromised sponsor manifest can't make us open a local file
/// or launch a registered protocol.
pub(crate) fn is_web_url(url: &str) -> bool {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !u.bytes().any(|b| b < 0x20)
}

/// HTTPS-only — required for assets we *fetch* (manifest + images): no plaintext
/// downloads. A stricter form of [`is_web_url`].
fn is_https_url(url: &str) -> bool {
    let u = url.trim();
    u.to_ascii_lowercase().starts_with("https://") && !u.bytes().any(|b| b < 0x20)
}

/// Fetch an HTTPS URL into memory over WinINet, capped at [`MAX_REMOTE_BYTES`] and
/// bounded by per-phase timeouts (so a dead host can't hang us — the manifest fetch
/// is on the startup path). `reload` bypasses the cache — needed for the sponsor
/// manifest: the endpoint is a Cloudflare Worker that serves the live manifest AND
/// records the anonymous check-in, so every run must actually reach origin (a cached
/// response would serve a stale manifest *and* silently skip the check-in).
/// `INTERNET_FLAG_RELOAD` forces a fresh origin fetch across the whole chain.
/// Immutable, versioned
/// per-sponsor image URLs pass `reload = false` and may be cached. Returns None on a
/// non-HTTPS URL, any WinINet failure, an empty body, or an over-cap response.
pub(crate) fn http_fetch(url: &str, reload: bool) -> Option<Vec<u8>> {
    if !is_https_url(url) {
        return None;
    }
    use windows::Win32::Networking::WinInet::{
        InternetCloseHandle, InternetOpenUrlW, InternetOpenW, InternetSetOptionW,
        INTERNET_FLAG_NO_CACHE_WRITE, INTERNET_FLAG_PRAGMA_NOCACHE, INTERNET_FLAG_RELOAD,
        INTERNET_FLAG_SECURE, INTERNET_OPTION_CONNECT_TIMEOUT, INTERNET_OPTION_RECEIVE_TIMEOUT,
        INTERNET_OPTION_SEND_TIMEOUT,
    };
    unsafe {
        let agent = wide("SageThumbs2K");
        let session = InternetOpenW(PCWSTR(agent.as_ptr()), 0, PCWSTR::null(), PCWSTR::null(), 0);
        if session.is_null() {
            return None;
        }
        // Bound each network phase so a slow/dead host can't stall the synchronous
        // startup manifest check (which would freeze the Settings window on open).
        let timeout_ms: u32 = (MANIFEST_TIMEOUT_SECS as u32) * 1000;
        for opt in [
            INTERNET_OPTION_CONNECT_TIMEOUT,
            INTERNET_OPTION_RECEIVE_TIMEOUT,
            INTERNET_OPTION_SEND_TIMEOUT,
        ] {
            let _ = InternetSetOptionW(
                Some(session),
                opt,
                Some(&timeout_ms as *const u32 as *const c_void),
                std::mem::size_of::<u32>() as u32,
            );
        }
        let url_w = wide(url);
        let mut flags = INTERNET_FLAG_SECURE; // require TLS (URL is already https)
        if reload {
            flags |=
                INTERNET_FLAG_RELOAD | INTERNET_FLAG_NO_CACHE_WRITE | INTERNET_FLAG_PRAGMA_NOCACHE;
        }
        let req = InternetOpenUrlW(session, PCWSTR(url_w.as_ptr()), None, flags, None);
        if req.is_null() {
            let _ = InternetCloseHandle(session);
            return None;
        }
        let data = crate::win::wininet_drain(req, MAX_REMOTE_BYTES);
        let _ = InternetCloseHandle(req);
        let _ = InternetCloseHandle(session);
        data.filter(|d| !d.is_empty())
    }
}

/// Parse the sponsor manifest JSON and decode each sponsor's art (sized to `w`×`h`).
/// Returns the usable sponsors plus the rotation cadence (ms) and order flag, or
/// None if the JSON is unparseable, kill-switched (`"enabled": false`), carries no
/// `sponsors`/`ads` array, or yields no decodable sponsor. Sponsors whose image
/// fails to download/decode are dropped. Creates GDI bitmaps (via CreateDIBSection —
/// no window needed), so it runs headless too.
fn build_sponsors_from_manifest(bytes: &[u8], w: u32, h: u32) -> Option<(Vec<Sponsor>, u32, bool)> {
    let manifest = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    // Honor the remote kill switch here too (defense in depth — the gate already
    // checks it, but the decode path must never resurrect a disabled feed).
    if manifest.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    // Clamp the seconds in u64 (≥1, ≤ 1 day) BEFORE the lossy u32 cast. Casting
    // first lets a huge manifest value wrap (e.g. 0x1_0000_0001 → 1) and yield a
    // 1-second rotation; the saturating_mul only guards overflow AFTER the cast, too
    // late to rescue an already-wrapped value. 86_400 * 1000 fits in u32.
    let rotate_ms =
        (manifest.get("rotate_seconds").and_then(|v| v.as_u64()).unwrap_or(10).clamp(1, 86_400) as u32) * 1000;
    let random = manifest.get("random").and_then(|v| v.as_bool()).unwrap_or(false);
    // Prefer the `sponsors` array; fall back to the legacy `ads` key so an existing
    // remote feed keeps working unchanged after the rename.
    let items = manifest
        .get("sponsors")
        .or_else(|| manifest.get("ads"))
        .and_then(|v| v.as_array())?;

    let mut sponsors: Vec<Sponsor> = Vec::new();
    for item in items {
        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
        // Only accept an http(s) click target from the (remote, untrusted)
        // manifest; anything else falls back to the product page. `open_url`
        // re-checks at the ShellExecute boundary (defense in depth).
        let link = item
            .get("link")
            .and_then(|v| v.as_str())
            .filter(|s| is_web_url(s))
            .unwrap_or(URL_PRODUCT);
        // `image` is a single URL or a list of interchangeable URLs.
        let urls: Vec<&str> = if let Some(s) = item.get("image").and_then(|v| v.as_str()) {
            vec![s]
        } else if let Some(arr) = item.get("image").and_then(|v| v.as_array()) {
            arr.iter().filter_map(|v| v.as_str()).collect()
        } else {
            continue;
        };
        let mut images: Vec<SponsorImage> = Vec::new();
        for url in urls {
            let Some(img_bytes) = http_fetch(url, false) else { continue };
            // Animated GIF → many frames; anything else → one still frame.
            let (frames, delay_ms) =
                if let Some((fr, d)) = sagethumbs2k_core::app_image::decode_gif_frames_sized(&img_bytes, w, h) {
                    (fr, d)
                } else if let Some(handle) = sagethumbs2k_core::app_image::image_to_hbitmap_sized(&img_bytes, w, h) {
                    (vec![handle], 0)
                } else {
                    continue;
                };
            if frames.is_empty() {
                continue;
            }
            images.push(SponsorImage { frames, delay_ms });
        }
        if images.is_empty() {
            continue;
        }
        sponsors.push(Sponsor { images, tip: wide(text), link: wide(link) });
    }
    if sponsors.is_empty() {
        return None;
    }
    Some((sponsors, rotate_ms, random))
}

/// Decode the (already-fetched, cached) sponsor manifest on a background thread:
/// download + decode each sponsor's art (sized to the control) and hand the finished
/// `SponsorRotator` to the UI thread, which installs it and starts rotating. The
/// embedded placeholder stays until (and unless) the images arrive. No-op if the
/// manifest is missing, kill-switched, or yields no usable sponsor — but the gate
/// ([`sponsors_enabled`]) has already confirmed sponsors exist before the banner is
/// created, so the usual path does install a rotator.
pub(crate) fn spawn_remote_sponsors(banner: HWND, w: u32, h: u32) {
    let hwnd = banner.0 as usize;
    std::thread::spawn(move || {
        let Some(bytes) = manifest_bytes() else { return };
        let Some((sponsors, rotate_ms, random)) = build_sponsors_from_manifest(bytes, w, h) else { return };

        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or(0x9e37_79b9)
            | 1; // xorshift seed must be non-zero
        let rot = Box::into_raw(Box::new(SponsorRotator::new(sponsors, rotate_ms, random, seed)));

        let banner = HWND(hwnd as *mut c_void);
        unsafe {
            let parent = IsWindow(Some(banner)).as_bool().then(|| GetParent(banner).ok()).flatten();
            match parent {
                // Ownership of `rot` transfers to the UI thread only if the post
                // succeeds. If PostMessageW fails (queue full / window torn down
                // between the IsWindow check and here), the message — and the
                // rotator + its GDI bitmaps — would leak; free them instead.
                Some(p) => {
                    if PostMessageW(Some(p), WM_APP_SPONSORS, WPARAM(hwnd), LPARAM(rot as isize)).is_err() {
                        drop_sponsor_rotator(rot);
                    }
                }
                None => drop_sponsor_rotator(rot), // window gone; free everything
            }
        }
    });
}

/// Show the rotator's current image on the banner and (re)arm the GIF frame timer
/// for it. `free_prev` uses set_static_bitmap to delete the bitmap currently held
/// (only true for the very first swap, which frees the embedded placeholder); every
/// later swap reuses bitmaps that the rotator still owns, so it must NOT delete them.
pub(crate) unsafe fn show_current_image(hwnd: HWND, banner: HWND, r: &SponsorRotator, free_prev: bool) {
    let Some(img) = r.current() else { return };
    if let Some(&first) = img.frames.first() {
        if free_prev {
            set_static_bitmap(banner, HBITMAP(first as *mut c_void));
        } else {
            SendMessageW(banner, STM_SETIMAGE, Some(WPARAM(IMAGE_BITMAP.0 as usize)), Some(LPARAM(first)));
        }
    }
    let _ = KillTimer(Some(hwnd), TIMER_BANNER);
    if img.frames.len() > 1 {
        let _ = SetTimer(Some(hwnd), TIMER_BANNER, img.delay_ms.max(20), None);
    }
}

/// Free a sponsor rotator: every frame of every image of every sponsor, then the box.
pub(crate) unsafe fn drop_sponsor_rotator(ptr: *mut SponsorRotator) {
    if ptr.is_null() {
        return;
    }
    let rot = Box::from_raw(ptr);
    for sponsor in &rot.sponsors {
        for img in &sponsor.images {
            for &f in &img.frames {
                let _ = DeleteObject(HGDIOBJ(f as *mut c_void));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The gate is OFF unless the (reachable) manifest parses, isn't kill-switched,
    // and lists at least one sponsor. An unreachable feed yields no bytes at all and
    // never reaches here — so every "off" case below is the fail-safe.
    #[test]
    fn gate_requires_enabled_populated_manifest() {
        // Reachable + has sponsors (enabled absent = on, or explicitly true).
        assert!(manifest_has_sponsors(br#"{ "sponsors": [ { "image": "x", "link": "y" } ] }"#));
        assert!(manifest_has_sponsors(br#"{ "enabled": true, "sponsors": [ {} ] }"#));
        // Legacy "ads" key still recognised.
        assert!(manifest_has_sponsors(br#"{ "ads": [ {} ] }"#));

        // Remote kill switch wins even with sponsors present.
        assert!(!manifest_has_sponsors(br#"{ "enabled": false, "sponsors": [ {} ] }"#));
        // Empty / missing sponsor list.
        assert!(!manifest_has_sponsors(br#"{ "sponsors": [] }"#));
        assert!(!manifest_has_sponsors(br#"{}"#));
        // Parse failure / empty body (what an unreachable-but-somehow-empty feed looks like).
        assert!(!manifest_has_sponsors(b"not json"));
        assert!(!manifest_has_sponsors(b""));
    }
}
