//! Content pipeline for the viewer: classify a path, decode images on a budgeted worker
//! thread (never on the UI thread — hard constraint §4/#3), build a DIB, and aspect-fit
//! paint it. Ported from `previewhandler.rs` (`make_dib` / `draw` / the budgeted-decode
//! worker), which does exactly this for the Explorer preview pane.

use core::ffi::c_void;
use core::time::Duration;
use std::cell::RefCell;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject,
    FillRect, SelectObject, SetStretchBltMode, StretchBlt, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::window::{ContentKind, WM_APP_ANIM, WM_APP_PDFINFO, WM_APP_RENDER};

/// Wall-clock budget for a single decode (plan §7 uses 12 s, matching the preview pane).
const DECODE_BUDGET: Duration = Duration::from_secs(12);

/// A decoded image ready to become a DIB. `Send`, so it crosses the worker→UI post.
pub(super) struct DecodedRgba {
    pub w: i32,
    pub h: i32,
    pub rgba: Vec<u8>,
    /// The NATIVE size of the source image, which is not always `(w, h)`.
    ///
    /// A codec-scaled decode ([`display_scaled_first_paint`]) holds only as many pixels as the
    /// screen can show, and everything user-facing — the size reported in the caption, the
    /// window the viewer opens at, what "100%" means to the zoom — has to keep answering about
    /// the real image. Carrying the native size here is what lets the pixels be small without
    /// any of that changing.
    pub nat: (i32, i32),
}

impl DecodedRgba {
    /// A full-resolution decode: the pixels ARE the image.
    pub(super) fn full(w: i32, h: i32, rgba: Vec<u8>) -> Self {
        Self {
            w,
            h,
            rgba,
            nat: (w, h),
        }
    }

    /// A codec-scaled decode of a `nat`-sized image.
    fn scaled(w: i32, h: i32, rgba: Vec<u8>, nat: (i32, i32)) -> Self {
        Self { w, h, rgba, nat }
    }

    /// True when these pixels are the whole image, so a zoom has nothing sharper to fetch.
    pub(super) fn is_full(&self) -> bool {
        self.nat == (self.w, self.h)
    }
}

/// What a finished decode is posted to the UI thread as. `Arc`, not a bare `DecodedRgba`,
/// so a cache hit is a refcount bump instead of a copy: MEASURED, the copy cost 7-8 ms on a
/// 12 MP photo (48 MB of RGBA) and 18 ms on a 24 MP one, which is most of what a "instant"
/// revisit was still paying. The UI only ever reads the pixels (`make_render`/`make_dib`
/// take a slice), so sharing them is free.
pub(super) type SharedRgba = std::sync::Arc<DecodedRgba>;

// ── decoded-image cache + prefetch (issue #20: stepping ←/→ felt slow) ────────────────
//
// Every ←/→ step used to pay a full read + decode, even for a file shown two seconds ago,
// because nothing remembered a decode once it had been painted. The fix is a small MRU of
// finished decodes plus a one-file read-ahead in the direction of travel.

/// How much decoded RGBA to keep.
///
/// MEASURED, not guessed. Decoded RGBA is ~4 bytes per pixel, so a 12 MP camera photo is
/// ~48 MB and a 24 MP one ~96 MB — the BYTE budget is the real bound here, never the count.
/// This started at 192 MB, which sounded generous and held only FOUR photos: `--bench-nav`
/// walking a 9-file folder showed every wrap-around revisit still paying a full ~250 ms
/// decode, because the earlier entries had already been evicted. 384 MB covers a run of
/// eight, which is the "flick back a few frames" the cache exists for. It is a ceiling, not
/// a reservation: it only fills if the user actually visits that many large images, and the
/// viewer is a throwaway per-preview process that exits with the window.
const CACHE_MAX_BYTES: usize = 384 << 20;
/// Belt-and-braces bound for the opposite case: many small images.
const CACHE_MAX_ENTRIES: usize = 16;
/// Cap on read-ahead workers, so holding down → cannot fan out a thread per keypress.
const MAX_PREFETCH_IN_FLIGHT: usize = 2;

/// Identity of a cached decode. Carries size + mtime, not just the path: a file edited or
/// replaced under the same name MUST miss, or the viewer would confidently show stale pixels.
type CacheKey = (String, u64, i64);

static CACHE: std::sync::Mutex<Vec<(CacheKey, std::sync::Arc<DecodedRgba>)>> =
    std::sync::Mutex::new(Vec::new());

// ── abandoning work the user has already navigated past ───────────────────────────────────
//
// The UI thread has always FENCED stale results (`on_render` drops a payload whose generation
// no longer matches), but the worker that produced it still ran to completion. Hold ← or → down
// and every file passed over spawns a decode that keeps burning CPU against the one the user is
// actually waiting for — the read-ahead's own workers included. Fencing the result is not the
// same as not doing the work.
//
// So publish the generation somewhere a worker can see, and have each one check it BEFORE it
// starts.
//
// **Only where the work cannot be reused, and that restriction is measured, not cautious.** The
// main decode populates the shared cache, so a worker that gives up mid-flight throws away a
// read the user is quite likely to want again the moment they arrow back - the read-ahead exists
// precisely to bank that. A first attempt abandoned it after the read too, and the held-key
// bench came out consistently WORSE for it (111 ms catch-up against 101 ms), because files that
// would have been cached had to be decoded a second time. Cancelling is only free for work whose
// result nothing else can use: the PSD composite (seconds of ImageMagick, posted to one
// generation and never cached) and a PDF page render. Those keep their checks; the cache-filling
// decode only checks on ENTRY, where nothing has been spent yet.

/// The generation the viewer currently cares about. Written by the UI thread when it starts a
/// load, read by decode workers.
static LIVE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Publish the generation a load is being started for. Called by the UI thread, next to the
/// `decode_gen` bump it mirrors.
pub(super) fn begin_generation(gen: u64) {
    LIVE_GEN.store(gen, std::sync::atomic::Ordering::SeqCst);
}

/// Has the user moved on since `gen` was started? Workers use this to abandon.
///
/// Deliberately `>` rather than `!=`: a worker must only ever give up for a NEWER generation.
/// Equality is the live case, and a generation older than the worker's cannot happen from a
/// monotonic counter — but treating "different" as "stale" would make a wrapped or reset
/// counter silently cancel live work instead of merely wasting some.
fn abandoned(gen: u64) -> bool {
    if cancellation_disabled() {
        return false;
    }
    let stale = LIVE_GEN.load(std::sync::atomic::Ordering::SeqCst) > gen;
    if stale {
        ABANDONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    stale
}

/// How many workers have given up so far. Reported by `--bench-mash`.
///
/// Latency cannot see this. The PSD composite runs asynchronously and posts a SECOND result, so
/// abandoning one never changes when anything paints - it changes how much ImageMagick the
/// machine runs for documents nobody is looking at any more. A count is the honest measurement
/// of that; a stopwatch is not.
static ABANDONED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `--bench-mash` hook: how many workers abandoned superseded work.
pub(super) fn bench_abandoned_count() -> usize {
    ABANDONED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Dev switch: `ST2K_NO_CANCEL=1` makes every worker run to completion as it did before.
///
/// Exists so the two behaviours can be measured on ONE binary. Comparing two separate builds
/// across a machine whose background load moves is how several confident wrong readings got
/// made in this file's history; an A/B on the same executable, minutes apart, has none of that.
/// Read once, so the hot path is an atomic load and not a `getenv` per check.
fn cancellation_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("ST2K_NO_CANCEL").is_some())
}
static PREFETCH_IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn cache_key(path: &str) -> Option<CacheKey> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Windows paths are case-insensitive, so the same file reached two ways is one entry.
    Some((path.to_ascii_lowercase(), md.len(), mtime))
}

/// A cached decode for `path`, moved to the front of the MRU. `None` on a miss. Hands back a
/// SHARE of the pixels, never a copy — see [`SharedRgba`].
fn cache_get(path: &str) -> Option<SharedRgba> {
    let key = cache_key(path)?;
    let mut c = CACHE.lock().ok()?;
    let found = c.iter().position(|(k, _)| *k == key);
    sagethumbs2k_core::safety::log_debug(&format!(
        "preview cache: {} for {path} ({} entries held)",
        if found.is_some() { "HIT" } else { "miss" },
        c.len()
    ));
    let pos = found?;
    let hit = c.remove(pos);
    let out = std::sync::Arc::clone(&hit.1);
    c.insert(0, hit);
    Some(out)
}

/// True when `path` is already cached — or unreadable, in which case there is nothing worth
/// prefetching either, so "true" (don't bother) is the right answer for both callers.
fn cache_has(path: &str) -> bool {
    let Some(key) = cache_key(path) else {
        return true;
    };
    CACHE
        .lock()
        .map(|c| c.iter().any(|(k, _)| *k == key))
        .unwrap_or(true)
}

fn cache_put(path: &str, img: std::sync::Arc<DecodedRgba>) {
    let Some(key) = cache_key(path) else {
        return;
    };
    let Ok(mut c) = CACHE.lock() else {
        return;
    };
    // Never DOWNGRADE an entry. Two workers can be in flight for one file - a zoom's
    // full-resolution fetch and a revisit's codec-scaled decode - and they finish in whatever
    // order the scheduler picks. Without this, the scaled one landing second would evict the
    // full-resolution pixels a zoom had already paid for, and the next zoom would have to
    // fetch them all over again.
    if !img.is_full() {
        if let Some((_, held)) = c.iter().find(|(k, _)| *k == key) {
            if held.is_full() {
                return;
            }
        }
    }
    c.retain(|(k, _)| *k != key);
    c.insert(0, (key, img));
    // Trim from the back. `Vec::retain` visits front-to-back in order, so a running total
    // evicts exactly the least-recently-used tail. An image that alone busts the budget is
    // dropped immediately, which is intended: caching it would blow the bound on its own.
    let (mut total, mut kept) = (0usize, 0usize);
    c.retain(|(_, v)| {
        total += v.rgba.len();
        kept += 1;
        kept <= CACHE_MAX_ENTRIES && total <= CACHE_MAX_BYTES
    });
}

/// Decode `path` in the background purely to warm the cache — nothing is posted and nothing
/// is shown. Called for the file the user is about to arrow onto.
pub(super) fn spawn_prefetch(path: String) {
    use std::sync::atomic::Ordering;
    if cache_has(&path) {
        return;
    }
    // Still images only. Video, text and archives have their own load paths, and PDF goes
    // through `spawn_decode_pdf` (page-aware), so a plain entry for one would never be read.
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "pdf" || classify(&path) != ContentKind::Image {
        return;
    }
    if PREFETCH_IN_FLIGHT
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < MAX_PREFETCH_IN_FLIGHT).then_some(n + 1)
        })
        .is_err()
    {
        return; // already at the cap — the user is arrowing faster than we can read ahead
    }
    std::thread::spawn(move || {
        // Warm the cache with the SAME thing a real load would install — the codec-scaled
        // decode where that is available, the full one otherwise. Reading ahead at full
        // resolution would mean the read-ahead costing four times what the load it is racing
        // does, which is exactly backwards for the case it exists to serve: a held-down arrow
        // key, where the prefetch has to finish before the user arrives.
        //
        // The animated extensions are excluded for the same reason `spawn_decode` excludes
        // them: a scaled decode of one yields a single still, and a still sitting in the cache
        // is what a later load would find and post. (It also initialises its own COM apartment,
        // so this bare worker thread needs nothing — the neighbouring path learned that one the
        // hard way.)
        let d = (!matches!(ext.as_str(), "gif" | "png" | "apng" | "webp"))
            .then(|| display_scaled_first_paint(&path))
            .flatten()
            .or_else(|| read_and_decode(&path));
        if let Some(d) = d {
            cache_put(&path, std::sync::Arc::new(d));
        }
        PREFETCH_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    });
}

/// Long edge the deferred decode is taken at: the largest the viewer's content pane can ever be
/// on this machine, which the monitor bounds.
///
/// **This used to be a hard-coded 2048, with a comment claiming a maximised viewer on a 4K panel
/// was still under it. That was wrong**, and the way it was wrong is instructive: the viewer
/// opens at up to 80% of the work area, so on a 3840-wide desktop a 12 MP photo aspect-fits to
/// about 2200 px — already more than 2048. `wants_full_resolution` therefore fired on the plain
/// FIT view of every single navigation, so each step did the scaled decode AND the full one, and
/// the full-resolution results then evicted everything else from the cache. The arrow bench read
/// as a win on the first pass through a folder and a loss on the second, which is exactly what
/// that looks like.
///
/// **The ceiling is the load-bearing part, and it is arithmetic, not a guess.** A JPEG reduces
/// only by halving, so a 4000 px photo can be decoded at 2000 and nothing between that and full
/// size. A 4K pane wants about 2200. Ask for 2200 and the codec cannot help, so WIC decodes the
/// whole image and resamples: measured at 292 ms per step against 250 ms for simply decoding it
/// normally, i.e. the "fast path" became the slow path. Ask for 2048 and the halving applies:
/// 59 ms. Sizing this to the monitor is therefore exactly wrong above ~2K, which is why an
/// earlier attempt to "fix" the hard-coded value made the arrow bench three times slower.
///
/// So the reduction is only reachable if a modest upscale at fit is acceptable. It is, and
/// [`FIT_UPSCALE_TOLERANCE`] is where that judgement is written down.
fn display_edge() -> u32 {
    static EDGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *EDGE.get_or_init(|| {
        let (_dpi, work) = crate::win::cursor_monitor_metrics();
        let long = (work.right - work.left).max(work.bottom - work.top).max(1) as u32;
        // Floor keeps a small or remote desktop from decoding uselessly little; the ceiling is
        // what keeps the codec's halving reachable for ordinary camera photos (see above).
        long.clamp(1024, 2048)
    })
}

/// How far the display bitmap may be stretched before the real pixels are fetched.
///
/// Not a corner cut — the enabling condition. Without SOME tolerance the fit view on a 4K panel
/// (about 2200 px from a 2048 px bitmap, a 7% upscale) would demand a full decode on every
/// single navigation, which is precisely the behaviour this defers, and the measured cost of
/// that was every arrow step paying both decodes and the results then evicting the cache.
///
/// 25% is chosen so a 7-10% fit-view stretch of a photo, which no one can see, costs nothing,
/// while the very first wheel notch (1.2x, i.e. 29%) fetches the real thing. Zooming is
/// deliberate; browsing is not.
const FIT_UPSCALE_TOLERANCE: f64 = 1.25;

/// Only worth a scaled decode when the source is meaningfully bigger than the pane; below this
/// the full decode is already quick and asking the codec twice would be pure overhead.
fn scaled_first_paint_min() -> u32 {
    display_edge().saturating_mul(3) / 2
}

/// A display-sized decode for the FIRST paint, asking the OS codec for a small picture rather
/// than decoding full size and shrinking afterwards.
///
/// This is the technique fast viewers are built on: a JPEG can be reconstructed at 1/2, 1/4 or
/// 1/8 straight from the compressed data, skipping most of the work. Our pure-Rust JPEG tier
/// has no such API (`image` 0.25 dropped it, `zune-jpeg` never had it), but WIC does, now that
/// the scaler sits ahead of the format converter so the codec's own transform is reachable.
///
/// SAFE BY CONSTRUCTION: this only ever produces an EARLIER paint. `spawn_decode` still runs
/// the normal full decode straight afterwards and posts it over the top, so the image the user
/// ends up looking at is byte-for-byte what it was before, and zoom still has full resolution
/// behind it. Measured first: the two tiers differ by at most 1 level per channel on a plain
/// JPEG (`decode::tests::pure_rust_and_wic_agree_on_a_plain_jpeg`), so the swap is invisible.
///
/// `None` whenever it is not clearly worth it: small source, unknown dimensions, or a format
/// the OS codecs decline.
fn display_scaled_first_paint(path: &str) -> Option<DecodedRgba> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    let (w, h) = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if w.max(h) <= scaled_first_paint_min() {
        return None;
    }
    // WIC is COM, and this runs on a bare decode worker that has no apartment -- without this
    // every call returned `CoInitialize has not been called (0x800401F0)`, so the fast path
    // silently did nothing at all while looking like it worked. (The neighbouring
    // `decode_preview_budgeted` initialises COM on its own sub-thread for the same reason.)
    let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    // `_if_codec_scales`, NOT the plain scaled decode. This is a PRE-pass — the full decode
    // still runs after it — so it is only ever worth doing where the codec can decode small in
    // its own domain. JPEG can (68 ms against 270 ms for full, measured); PNG cannot, so WIC
    // decodes the whole thing and resamples (605 ms against 690 ms), which meant this pre-pass
    // was doing a SECOND full decode of every large PNG for a first paint barely any earlier.
    let decoded =
        sagethumbs2k_core::decode::wic_scaled_from_path_if_codec_scales(path, display_edge());
    if inited {
        unsafe { CoUninitialize() };
    }
    let img = decoded?;
    let rgba = img.to_rgba8();
    let (dw, dh) = (rgba.width() as i32, rgba.height() as i32);
    // The pixels are small; `nat` keeps the real size, which is what the caption, the window
    // sizing and the zoom's "100%" all answer against.
    Some(DecodedRgba::scaled(
        dw,
        dh,
        rgba.into_raw(),
        (w as i32, h as i32),
    ))
}

/// The REAL composite for a container format whose preview is only a small baked-in
/// thumbnail — PSD/PSB, where Photoshop's resource-1036 preview is often ~160 px wide no
/// matter how big the document is (issue #20: "PSD/PSB appear lower in resolution").
///
/// `None` when there is nothing better to show: not such a container, the document is not
/// meaningfully bigger than what is already on screen, or there is no compositor (the compact
/// install has no ImageMagick, so `decode_full` returns the same baked preview back).
fn sharper_composite(path: &str, head: &[u8], shown: (i32, i32)) -> Option<DecodedRgba> {
    let (rw, rh) = sagethumbs2k_core::real_dims(head)?;
    // Only pay for a second decode when the document is clearly bigger than what we drew.
    if rw <= (shown.0.max(1) as u32).saturating_mul(3) / 2
        && rh <= (shown.1.max(1) as u32).saturating_mul(3) / 2
    {
        return None;
    }
    // Re-read the file WHOLE. The bytes the preview stage worked from came from
    // `read_preview_capped`, which for these very formats deliberately returns only a head
    // PREFIX (that is how a 100 MB PSD thumbnails cheaply) — and a truncated PSD cannot be
    // composited, so handing those bytes to `decode_full` would silently fall straight back
    // to the baked preview we are trying to replace.
    let whole = sagethumbs2k_core::decode::read_capped(path).ok()?;
    let full = sagethumbs2k_core::decode::decode_full(&whole).ok()?;
    let rgba = full.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    // Guard the no-compositor case explicitly: on a compact install `decode_full` falls back
    // to the same baked preview, and swapping in an identical image is a repaint for nothing.
    if w <= shown.0 && h <= shown.1 {
        sagethumbs2k_core::safety::log_debug(&format!(
            "preview: no sharper composite for {path} (document {rw}x{rh}, \
             full decode {w}x{h}, already showing {}x{})",
            shown.0, shown.1
        ));
        return None;
    }
    sagethumbs2k_core::safety::log_debug(&format!(
        "preview: sharpened {path} from {}x{} to {w}x{h} (document is {rw}x{rh})",
        shown.0, shown.1
    ));
    Some(DecodedRgba::full(w, h, rgba.into_raw()))
}

/// The current image render installed in the window (the DIB + its natural dims + the bg
/// it was composited over). Sole owner of `hbmp`; freed when replaced or on window destroy.
pub(super) struct RenderData {
    pub hbmp: HBITMAP,
    /// The NATIVE dimensions of the image — what the file actually contains.
    ///
    /// Everything user-visible keys off these and always has: the aspect-fit geometry, what
    /// "100%" means to the zoom, the size the window opens at, and the dimensions in the
    /// caption. They stay native even when `hbmp` is a smaller codec-scaled decode, which is
    /// what makes holding fewer pixels invisible.
    pub iw: i32,
    pub ih: i32,
    /// The dimensions of `hbmp` ITSELF, which is a different question. Equal to `(iw, ih)` for
    /// a full-resolution decode; smaller when the codec was asked for a display-sized picture
    /// and [`paint_image`] stretches the last little bit.
    bw: i32,
    bh: i32,
    /// The bitmap holds PREMULTIPLIED alpha and must be composed with `AlphaBlend`, not blitted.
    /// Only ever true for the main image pane (see [`make_render`]); cover art and inline Markdown
    /// images stay flattened, so they keep the plain blit and want no checkerboard.
    pub alpha: bool,
    /// The premultiplied pixels of `hbmp`, which is a DIB SECTION, so this is its own backing
    /// memory rather than a second copy. Valid exactly as long as `hbmp`. Null unless `alpha`.
    src: *const u8,
    /// Last box-filtered downscale, keyed by the size it was built for. See [`scaled_for`] for
    /// why the resampling is done here rather than left to GDI.
    scaled: RefCell<Option<(i32, i32, HBITMAP)>>,
}

impl RenderData {
    /// A fully opaque render: plain `StretchBlt`, no checkerboard, no resampling cache.
    pub(super) fn opaque(hbmp: HBITMAP, iw: i32, ih: i32) -> Self {
        Self {
            hbmp,
            iw,
            ih,
            bw: iw,
            bh: ih,
            alpha: false,
            src: core::ptr::null(),
            scaled: RefCell::new(None),
        }
    }

    /// Re-label a render whose bitmap is a codec-scaled decode of a larger image, so the
    /// geometry keeps answering about the real thing. See the `iw`/`bw` split above.
    fn with_native(mut self, nat: (i32, i32)) -> Self {
        self.bw = self.iw;
        self.bh = self.ih;
        self.iw = nat.0;
        self.ih = nat.1;
        self
    }
}

impl Drop for RenderData {
    fn drop(&mut self) {
        unsafe {
            if let Some((_, _, s)) = self.scaled.borrow_mut().take() {
                let _ = DeleteObject(s.into());
            }
            let _ = DeleteObject(self.hbmp.into());
        }
    }
}

/// Decide how to present `path`: directory / unsupported → InfoCard; text/markdown (gated on
/// the settings) → Text; any of the ~315 supported formats → Image; an unknown-but-textual
/// file → Text. Phase 3's text branch shows the file as readable monospace text; rendered
/// GitHub-style Markdown + syntax highlighting (WebView2 + syntect) is a later enhancement.
pub(super) fn classify(path: &str) -> ContentKind {
    use sagethumbs2k_core::{formats, settings};
    let p = std::path::Path::new(path);
    if p.is_dir() {
        return ContentKind::InfoCard;
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // Markdown (rendered) + text/code, ahead of the image path (a `.md`/`.txt` is never an image).
    if settings::preview_markdown() && formats::is_preview_markdown(&ext) {
        return ContentKind::Markdown;
    }
    // Structured docs ride the markdown PIPELINE (converted at load — see `docconv`), but each
    // honors the toggle a user would expect to govern it: a notebook is a markdown document,
    // CSV/TSV are data/text files (review finding, 2026-07-13 — with Markdown off + Text on,
    // csv used to fall through to the raw-text sniff and lose its table view).
    if formats::is_preview_doc(&ext) {
        let on = if ext.eq_ignore_ascii_case("ipynb") {
            settings::preview_markdown()
        } else {
            settings::preview_text()
        };
        if on {
            return ContentKind::Markdown;
        }
    }
    if settings::preview_text() && formats::is_preview_text(&ext) {
        return ContentKind::Text;
    }
    if formats::is_known(&ext) {
        // Video AND audio play in-viewer via the shared Media-Foundation engine + transport strip
        // (audio is a video with no picture — same seek/volume/play controls). Everything else
        // (documents, images, incl. embedded album art) takes the decoded-image path.
        if matches!(
            formats::category(&ext),
            formats::Category::Video | formats::Category::Audio
        ) {
            return ContentKind::Video;
        }
        return ContentKind::Image;
    }
    // Unknown extension: if it sniffs as text (and text preview is on), show it as text.
    if settings::preview_text() && looks_like_text(path) {
        return ContentKind::Text;
    }
    // Still unknown, so fall back to the CONTENT. A file with no extension (or a wrong one) that
    // is really a picture used to land on the info card, which reads as "we can't open this" when
    // in fact every decoder we own would have handled it. The decoders all content-sniff anyway;
    // this only gets them the chance to run.
    if looks_like_image(path) {
        return ContentKind::Image;
    }
    ContentKind::InfoCard
}

/// Magic-number sniff for the common raster containers, used ONLY as the last resort in
/// [`classify`] when the extension told us nothing.
///
/// Deliberately a short list of unambiguous, fixed-offset signatures rather than a general
/// detector: this runs on the UI thread, and being wrong here costs a decode attempt that ends in
/// the same info card we would have shown anyway.
fn looks_like_image(path: &str) -> bool {
    let Some((head, _)) = read_capped(path, 64) else {
        return false;
    };
    magic_is_image(&head)
}

/// The signature table behind [`looks_like_image`], split out so it is testable without a file.
fn magic_is_image(h: &[u8]) -> bool {
    let at = |off: usize, sig: &[u8]| h.len() >= off + sig.len() && &h[off..off + sig.len()] == sig;
    at(0, b"\x89PNG\r\n\x1a\n")                                   // PNG / APNG
        || at(0, b"\xFF\xD8\xFF")                                 // JPEG
        || at(0, b"GIF87a")
        || at(0, b"GIF89a")
        || at(0, b"BM")                                           // BMP
        || (at(0, b"RIFF") && at(8, b"WEBP"))
        || at(0, b"qoif")                                         // QOI
        || at(4, b"ftypavif")                                     // AVIF
        || at(4, b"ftypheic")
        || at(4, b"ftypheix")
        || at(4, b"ftypmif1")                                     // HEIF
        || at(0, b"II*\0")                                        // TIFF little-endian
        || at(0, b"MM\0*") // TIFF big-endian
}

/// Read a text/code file for preview: cap at 5 MB, reject binaries, decode (BOM-aware, lossy),
/// truncate absurdly long lines, and mark a capped file. `None` if unreadable or binary.
pub(super) fn read_text(path: &str) -> Option<String> {
    const CAP: usize = 5 * 1024 * 1024;
    let (bytes, capped) = read_capped(path, CAP)?;
    if is_binary(&bytes) {
        return None;
    }
    let mut text = truncate_long_lines(&decode_text(&bytes), 10_000);
    if capped {
        text.push_str("\n\n… (file truncated at 5 MB)");
    }
    Some(text)
}

/// Like [`read_text`] but WITHOUT the long-line truncation — for structured documents
/// (CSV/TSV/`.ipynb`) that must be parsed whole. A minified single-line notebook JSON or a wide
/// CSV row would otherwise be cut at 10 000 chars, breaking the parse. Same 5 MB cap + binary
/// reject + BOM-aware decode. `None` if unreadable or binary.
pub(super) fn read_doc(path: &str) -> Option<String> {
    const CAP: usize = 5 * 1024 * 1024;
    let (bytes, _capped) = read_capped(path, CAP)?;
    if is_binary(&bytes) {
        return None;
    }
    Some(decode_text(&bytes))
}

/// Extensions shown as a file LISTING (container formats with no cover/thumbnail — deliberately
/// NOT comics/ebooks/office, which already preview their embedded image). The long tail here is
/// all just zip-in-disguise: appx/msix (Windows packages), oxt (LibreOffice extensions) —
/// `list_archive` sniffs the signature, so a mislabeled file falls through safely. Android
/// packages (apk/apks/xapk/apkm) are NOT here anymore: they have a real cover now (the
/// launcher icon, `container/apk.rs`), the same reason cbz/epub never were.
pub(super) fn is_archive_ext(ext: &str) -> bool {
    matches!(
        ext,
        "zip"
            | "7z"
            | "rar"
            | "jar"
            | "war"
            | "xpi"
            | "whl"
            | "nupkg"
            | "vsix"
            | "ipa"
            | "aar"
            | "appx"
            | "msix"
            | "appxbundle"
            | "msixbundle"
            | "oxt"
    )
}

/// Read an archive and format its entries (name + size) as a scrollable text listing, sorted with
/// directories first then case-insensitively by path. Never extracts (header/central-dir read only).
/// `None` if unreadable, not a recognized archive, or larger than the read cap (keeps the UI snappy).
pub(super) fn archive_listing(path: &str) -> Option<String> {
    // Cap the in-memory read: list_archive needs the whole byte slice, and this runs on the UI
    // thread. 64 MB covers the vast majority of previewed .zip/.jar/.apk without a visible hang.
    const CAP: u64 = 64 * 1024 * 1024;
    if std::fs::metadata(path).ok()?.len() > CAP {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let mut entries = sagethumbs2k_core::list_archive(&bytes)?;
    entries.sort_by(|a, b| {
        b.2.cmp(&a.2) // directories (is_dir=true) first
            .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()))
    });
    let files = entries.iter().filter(|e| !e.2).count();
    let total: u64 = entries.iter().map(|e| e.1).sum();
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = format!(
        "{name}\n{files} file(s) · {} uncompressed\n\n",
        human_size(total)
    );
    for (n, sz, is_dir) in &entries {
        if *is_dir {
            out.push_str(&format!("             {}/\n", n.trim_end_matches('/')));
        } else {
            out.push_str(&format!("{:>10}   {n}\n", human_size(*sz)));
        }
    }
    Some(out)
}

/// Human-readable byte size (B/KB/MB/GB/TB, one decimal above bytes).
pub(super) fn human_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b as f64, 0usize);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Quick "is this a text file" sniff for unknown extensions: read the first 16 KB and treat it
/// as text unless it has two consecutive NUL bytes (the standard binary heuristic).
fn looks_like_text(path: &str) -> bool {
    match read_capped(path, 16 * 1024) {
        Some((bytes, _)) => !bytes.is_empty() && !is_binary(&bytes),
        None => false,
    }
}

/// Read up to `cap` bytes of `path`; the bool is whether the file was longer (i.e. truncated).
fn read_capped(path: &str, cap: usize) -> Option<(Vec<u8>, bool)> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    // Read one byte past the cap so we can tell "exactly cap" from "longer than cap".
    f.take(cap as u64 + 1).read_to_end(&mut buf).ok()?;
    let capped = buf.len() > cap;
    buf.truncate(cap);
    Some((buf, capped))
}

/// Two consecutive NUL bytes in the first 16 KB = binary (matches the plan's sniff).
fn is_binary(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .take(16 * 1024)
        .zip(bytes.iter().skip(1))
        .any(|(a, b)| *a == 0 && *b == 0)
}

/// Decode bytes to a String: honor a UTF-16 LE/BE or UTF-8 BOM, sniff BOM-less UTF-16, take
/// strict UTF-8 when it validates, and otherwise fall back to the legacy codepage tiers in
/// [`decode_legacy`].
///
/// The UTF-8-lossy-everything shortcut this replaced turned every non-Unicode CJK file into a
/// solid wall of U+FFFD: GBK/GB18030 is still a Chinese national standard, and Shift-JIS,
/// Big5 and EUC-KR are all over real-world `.txt`/`.csv`/`.srt` files. Those users saw
/// nothing but replacement characters.
fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    // UTF-32 BOMs must be tested BEFORE UTF-16's, because the UTF-32LE BOM ("FF FE 00 00") STARTS
    // with the UTF-16LE one. Checked in the other order, every UTF-32LE file decoded as UTF-16LE
    // and came out as text interleaved with NULs.
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE, 0x00, 0x00]) {
        return utf32(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0x00, 0x00, 0xFE, 0xFF]) {
        return utf32(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16_le(rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16_be(rest);
    }
    // BOM-less UTF-16. Notepad writes a BOM but plenty of tools (and Windows' own older
    // exports) don't; without this such a file decodes as interleaved-NUL garbage. ASCII-range
    // UTF-16 text never trips `is_binary` (it has no two CONSECUTIVE NULs), so it reaches here.
    match sniff_utf16(bytes) {
        Some(true) => return utf16_le(bytes),
        Some(false) => return utf16_be(bytes),
        None => {}
    }
    // Strict, not lossy: valid UTF-8 is the overwhelmingly common case and must win outright,
    // but a FAILED validation is now real evidence that this is a legacy-codepage file rather
    // than something to paper over with U+FFFD.
    match core::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => decode_legacy(bytes),
    }
}

/// Decode `bytes` as UTF-32 (`le` picks the byte order); a trailing partial unit and any invalid
/// scalar (a surrogate, or above U+10FFFF) become U+FFFD rather than failing the whole file.
/// BOM-only, never sniffed: a BOM-less UTF-32 file is vanishingly rare and guessing one would
/// misread ordinary ASCII that happens to contain NULs.
fn utf32(bytes: &[u8], le: bool) -> String {
    bytes
        .chunks_exact(4)
        .map(|c| {
            let v = if le {
                u32::from_le_bytes([c[0], c[1], c[2], c[3]])
            } else {
                u32::from_be_bytes([c[0], c[1], c[2], c[3]])
            };
            char::from_u32(v).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect()
}

/// Decode `bytes` as UTF-16LE (odd trailing byte dropped).
fn utf16_le(bytes: &[u8]) -> String {
    let u: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u)
}

/// Decode `bytes` as UTF-16BE (odd trailing byte dropped).
fn utf16_be(bytes: &[u8]) -> String {
    let u: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u)
}

/// BOM-less UTF-16 sniff over the first 4 KB: `Some(true)` = LE, `Some(false)` = BE, `None` =
/// not UTF-16. Looks for the giveaway pattern of ASCII-range text — a NUL in a consistent
/// parity slot for most 2-byte units — and demands a strong majority so a legacy-codepage file
/// (which has essentially no NULs at all) can never be mistaken for UTF-16.
fn sniff_utf16(bytes: &[u8]) -> Option<bool> {
    let head = &bytes[..bytes.len().min(4096)];
    if head.len() < 16 {
        return None;
    }
    let pairs = head.len() / 2;
    let (mut hi_nul, mut lo_nul) = (0usize, 0usize);
    for c in head.chunks_exact(2) {
        // c[1] is the high byte in LE: NUL there means an ASCII-range char stored little-endian.
        if c[1] == 0 && c[0] != 0 {
            hi_nul += 1;
        }
        if c[0] == 0 && c[1] != 0 {
            lo_nul += 1;
        }
    }
    // 60% of units carrying the same-parity NUL is far above anything 8-bit text produces.
    let thresh = pairs * 3 / 5;
    match (hi_nul > thresh, lo_nul > thresh) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// Decode non-UTF-8 bytes via Windows' in-box codepage tables — zero bundled bytes, since every
/// one of these ships with the OS.
///
/// The double-byte codepages are tried FIRST, and only they compete on score. That ordering is
/// the whole trick: a single-byte codepage like Windows-1252 maps almost every possible byte, so
/// "1252 validated" is no evidence at all, and letting it into the contest means Latin-1
/// mojibake (`ÖÐÄÄ`) beats the correct `中文` on any per-character scoring you care to invent. A
/// DBCS codepage validating the WHOLE buffer under `MB_ERR_INVALID_CHARS` is real evidence,
/// because it requires every high byte to form a well-formed lead/trail pair — an ordinary
/// Latin-1 file with isolated accented characters fails that immediately.
///
/// Ties fall to the system ANSI codepage when it is itself DBCS, which is the case that matters
/// most: a Chinese/Japanese/Korean user opening a local file on their own localized Windows,
/// where the ACP already IS 936/932/949.
///
/// Known limit: pure-ideograph text with no kana or hangul is genuinely ambiguous between GBK,
/// Shift-JIS and EUC-KR — the same bytes are valid in all three. Real sentences carry kana or
/// hangul and [`cjk_score`] keys off those, but a short hanzi-only string on a non-CJK machine
/// can still land on the wrong one. Dedicated detectors have the same problem without
/// frequency tables, which are more weight than this is worth.
fn decode_legacy(bytes: &[u8]) -> String {
    use windows::Win32::Globalization::GetACP;

    let acp = unsafe { GetACP() };
    // ACP first when it's double-byte, so it wins ties; then the rest, minus any duplicate.
    let mut candidates: Vec<u32> = Vec::with_capacity(DBCS_CODEPAGES.len() + 1);
    if is_dbcs(acp) {
        candidates.push(acp);
    }
    candidates.extend(DBCS_CODEPAGES.iter().copied().filter(|cp| *cp != acp));

    let mut best: Option<(i64, String)> = None;
    for cp in candidates {
        let Some(s) = decode_codepage(bytes, cp, true) else {
            continue;
        };
        let score = cjk_score(&s);
        if score <= 0 {
            continue; // validated, but the result doesn't look like CJK text
        }
        // Strictly-greater, so a tie goes to whichever was scored FIRST.
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, s));
        }
    }
    if let Some((_, s)) = best {
        return s;
    }

    // No double-byte reading held up. Fall back to the system codepage — correct for the
    // single-byte locales (Cyrillic, Greek, Turkish, Vietnamese, Arabic, Thai) where a user's
    // own files match their own ACP — and finally to a lossy 1252, which maps every byte and so
    // always yields something readable instead of U+FFFD soup.
    decode_codepage(bytes, acp, true)
        .or_else(|| decode_codepage(bytes, acp, false))
        .or_else(|| decode_codepage(bytes, 1252, false))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// The double-byte codepages worth testing, in preference order. These are the encodings
/// real-world text files in the affected markets are actually saved in.
const DBCS_CODEPAGES: &[u32] = &[
    936, // GBK / GB18030 — Simplified Chinese
    932, // Shift-JIS — Japanese
    949, // EUC-KR / Unified Hangul — Korean
    950, // Big5 — Traditional Chinese
];

/// Is `cp` one of the double-byte codepages we test?
fn is_dbcs(cp: u32) -> bool {
    DBCS_CODEPAGES.contains(&cp)
}

/// Decode `bytes` with Windows codepage `cp`. With `strict`, an invalid byte sequence for that
/// codepage makes this return `None` (that's `MB_ERR_INVALID_CHARS`); without it, undecodable
/// bytes become the codepage's default char.
fn decode_codepage(bytes: &[u8], cp: u32, strict: bool) -> Option<String> {
    use windows::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};

    if bytes.is_empty() {
        return Some(String::new());
    }
    let flags = if strict {
        MB_ERR_INVALID_CHARS
    } else {
        Default::default()
    };
    let n = unsafe { MultiByteToWideChar(cp, flags, bytes, None) };
    if n <= 0 {
        return None;
    }
    let mut buf = vec![0u16; n as usize];
    let written = unsafe { MultiByteToWideChar(cp, flags, bytes, Some(&mut buf)) };
    if written <= 0 {
        return None;
    }
    buf.truncate(written as usize);
    Some(String::from_utf16_lossy(&buf))
}

/// How much does this decode look like genuine CJK text? `0` means "reject this codepage".
///
/// Only non-ASCII characters are judged — the ASCII in a source file or a CSV decodes
/// identically under every candidate, so counting it would just dilute the signal.
///
/// The discrimination comes from SCRIPT DOMINANCE, not from per-character weights. A per-char
/// bonus for kana/hangul reads well and is wrong: decoding Chinese GBK bytes through EUC-KR
/// yields a roughly 50/50 hangul-and-hanja mixture, and enough of those small bonuses beat the
/// correct all-ideograph reading outright (it did, until this was rewritten). What actually
/// separates the languages is the SHAPE of the mixture — real Korean is overwhelmingly hangul,
/// real Japanese always carries a solid fraction of kana, and real Chinese has neither — so the
/// bonus is awarded once, on the whole string, only when a script genuinely dominates.
fn cjk_score(s: &str) -> i64 {
    // Letters are the script evidence; CJK punctuation is shared by all of them and so is
    // counted as plausible but kept out of the fractions.
    let (mut ideo, mut kana, mut hangul, mut punct, mut bad, mut non_ascii) = (0i64, 0, 0, 0, 0, 0);
    for ch in s.chars().take(20_000) {
        let c = ch as u32;
        if c < 0x80 {
            continue;
        }
        non_ascii += 1;
        match c {
            0x3040..=0x30FF => kana += 1,
            0xAC00..=0xD7A3 => hangul += 1,
            0x4E00..=0x9FFF => ideo += 1,
            0x3000..=0x303F | 0xFF01..=0xFF60 | 0xFFE0..=0xFFE6 => punct += 1,
            // Halfwidth katakana is DELIBERATELY not kana: bytes 0xA1–0xDF decode to it under
            // Shift-JIS unconditionally, so any high-byte run validates as a katakana string and
            // would otherwise hijack every Chinese and Korean file on the machine.
            0xFF61..=0xFF9F => bad += 1,
            // Private use, specials, rare extension and compatibility blocks: what a WRONG
            // table produces.
            0xE000..=0xF8FF | 0xFFF0..=0xFFFF => bad += 3,
            0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x2_0000..=0x3_FFFF => bad += 2,
            _ => bad += 1,
        }
    }
    if non_ascii == 0 {
        return 0; // pure ASCII — a double-byte reading adds nothing over plain UTF-8
    }
    let good = ideo + kana + hangul + punct;
    // Demand a strong majority of the non-ASCII content be plausible CJK, so an ordinary
    // Latin-1 file that happens to validate can't be dragged into a CJK reading.
    if good * 4 < non_ascii * 3 {
        return 0;
    }
    let letters = ideo + kana + hangul;
    // Dominance thresholds: Korean prose is almost entirely hangul (a wrong reading of Chinese
    // lands near half), and any real Japanese sentence carries particles and okurigana in kana.
    // Chinese matches neither and wins on the base score alone, plus the ACP/list ordering that
    // breaks its tie with Big5.
    let dominant = letters > 0 && ((hangul * 20 >= letters * 13) || (kana * 20 >= letters * 3));
    let bonus = if dominant { 1_000 } else { 0 };
    (good - bad + bonus).max(1)
}

/// Cap any single line at `max` chars (so one minified/no-newline line can't blow up layout).
fn truncate_long_lines(text: &str, max: usize) -> String {
    if !text.lines().any(|l| l.chars().count() > max) {
        return text.to_string();
    }
    text.lines()
        .map(|l| {
            if l.chars().count() > max {
                let mut s: String = l.chars().take(max).collect();
                s.push('…');
                s
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Kick off an async decode of `path` on a detached worker thread. The result (or `None`
/// on failure/timeout) is posted back to `hwnd` as `WM_APP_RENDER` carrying a boxed
/// `(gen, Option<SharedRgba>)`; `gen` lets the UI thread drop a stale result after the
/// user has already switched files. The UI thread NEVER blocks on the decode.
pub(super) unsafe fn spawn_decode(hwnd: HWND, path: String, gen: u64) {
    // Cache hit: answer on the spot, no thread, no read, no decode. Stepping ←/→ through a
    // folder revisits the same files constantly, and this is what makes that feel instant.
    if let Some(hit) = cache_get(&path) {
        post_render(hwnd, gen, Some(hit));
        return;
    }
    begin_generation(gen);
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        // Reconstruct the HWND inside the worker (HWND isn't `Send`; the raw pointer is).
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // Held-down arrow key: by the time the scheduler gets here the user may already be two
        // files further on. Nothing has been read or decoded yet, so this costs one atomic load
        // and reclaims the entire worker.
        if abandoned(gen) {
            return;
        }
        // Formats that stream + downscale off the file handle (OpenEXR) skip the read
        // entirely — a 12K render pass is past every in-memory cap. Never animated, so
        // this can post the single-frame result straight away.
        if let Some(decoded) = streamed_decode(&path) {
            let payload: Box<(u64, Option<SharedRgba>)> =
                Box::new((gen, Some(std::sync::Arc::new(decoded))));
            let raw = Box::into_raw(payload);
            if PostMessageW(
                Some(hwnd),
                WM_APP_RENDER,
                WPARAM(gen as usize),
                LPARAM(raw as isize),
            )
            .is_err()
            {
                drop(Box::from_raw(raw));
            }
            return;
        }
        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // CODEC-SCALED DECODE, ahead of the read: when this succeeds nothing else needs the
        // file's bytes at all, so reading them first would be pure waste.
        //
        // The animated extensions are excluded rather than ordered around. They need the bytes
        // for the frame probe below regardless, and none of them is a codec that decodes small,
        // so nothing is given up. That exclusion also preserves the rule this path was built
        // under: an animated file posts `WM_APP_ANIM` and returns, and posting a still render
        // as well left the window with two render paths half-applied, which crashed outright on
        // a 24 MP PNG (access violation, found by `--bench-nav` at exactly the step that
        // reaches it). Keeping the two mutually exclusive is what makes that unrepresentable.
        if !matches!(ext.as_str(), "gif" | "png" | "apng" | "webp") {
            // …and STOPS there. The full decode is deferred until something actually needs
            // those pixels, which for a fit view is never: the pane shows about a megapixel and
            // this holds up to 2048 on the long edge. Measured, 12 MP JPEG: 59 ms here against
            // 250 ms for the full decode, so an arrow step stops paying the 250 ms at all.
            //
            // What makes deferring SAFE rather than a downgrade is `DecodedRgba::nat`: the
            // pixels are small but the render still reports the real image size, so the
            // caption, the window sizing and "100%" are unchanged.
            // `window::ensure_full_for_zoom` fetches the real thing the moment a zoom asks for
            // more detail than this holds.
            //
            // Only reached for codecs that genuinely decode small (JPEG's DCT reduction);
            // anything else returns `None` and falls through to the full decode unchanged.
            if let Some(quick) = display_scaled_first_paint(&path) {
                let quick = std::sync::Arc::new(quick);
                cache_put(&path, std::sync::Arc::clone(&quick));
                post_render(hwnd, gen, Some(quick));
                return;
            }
        }
        // One bounded/path-aware read for BOTH the animation probe and static fallback.
        // This also gives the standalone viewer the core's PSD/PSB/Blender head-preview and
        // oversized streamed-cover fast paths instead of blindly buffering the whole file.
        // Shared, never copied: the decode moves it into its worker and the sharpen pass needs
        // the same buffer afterwards. Cloning it instead cost a full copy of the file per
        // preview (measured: ~120 MB on a 24 MP PNG, for nothing).
        let bytes = sagethumbs2k_core::decode::read_preview_capped(&path)
            .ok()
            .map(std::sync::Arc::new);
        // Animated GIF/APNG/animated-WebP → post the whole frame list (WM_APP_ANIM). A static
        // file of the same extension returns None and falls through to the single-frame path.
        if matches!(ext.as_str(), "gif" | "png" | "apng" | "webp") {
            if let Some(bytes) = bytes.as_deref() {
                if let Some(frames) = super::anim::decode_animation(bytes, &ext) {
                    let payload: Box<(u64, Vec<(DecodedRgba, u32)>)> = Box::new((gen, frames));
                    let raw = Box::into_raw(payload);
                    if PostMessageW(
                        Some(hwnd),
                        WM_APP_ANIM,
                        WPARAM(gen as usize),
                        LPARAM(raw as isize),
                    )
                    .is_err()
                    {
                        drop(Box::from_raw(raw));
                    }
                    return;
                }
            }
        }
        let decoded = bytes
            .clone()
            .and_then(decode_loaded)
            .map(std::sync::Arc::new);
        // Cache and hand over the SAME allocation — one decode, no copy of the pixels.
        let shown = decoded.as_ref().map(|d| (d.w, d.h));
        if let Some(d) = &decoded {
            cache_put(&path, std::sync::Arc::clone(d));
        }
        post_render(hwnd, gen, decoded);
        // The fast preview is now on screen. For PSD/PSB that preview is Photoshop's small
        // baked-in thumbnail, so chase it with the real composite and post a SECOND result.
        // Two-stage on purpose: the composite shells out to ImageMagick and can take seconds,
        // and paying that up front would trade an instant preview for a long blank window.
        if let (Some(bytes), Some(shown)) = (bytes, shown) {
            spawn_sharpen(hwnd, path, bytes, shown, gen);
        }
    });
}

/// Decode `path` at FULL resolution and post it, skipping the codec-scaled shortcut.
///
/// The other half of the deferral in [`spawn_decode`]: the fit view is served by display-sized
/// pixels, and this is what fetches the real ones once a zoom asks for detail they do not hold.
/// Deliberately a separate entry point rather than a flag — a caller that wants full resolution
/// wants it unconditionally, and threading a "no really, all of it" boolean through the normal
/// path is how the shortcut would eventually get taken by accident.
pub(super) unsafe fn spawn_decode_full(hwnd: HWND, path: String, gen: u64) {
    if let Some(hit) = cache_get(&path).filter(|d| d.is_full()) {
        post_render(hwnd, gen, Some(hit));
        return;
    }
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        if abandoned(gen) {
            return; // zoomed, then navigated away before this got a slice of CPU
        }
        let decoded = read_and_decode(&path).map(std::sync::Arc::new);
        if let Some(d) = &decoded {
            // Replaces the scaled entry under the same key, so a later revisit gets the full
            // pixels straight away rather than re-deciding.
            cache_put(&path, std::sync::Arc::clone(d));
        }
        post_render(hwnd, gen, decoded);
    });
}

/// Post a finished decode to the UI thread, reclaiming the box if the window has already gone.
unsafe fn post_render(hwnd: HWND, gen: u64, decoded: Option<SharedRgba>) {
    let payload: Box<(u64, Option<SharedRgba>)> = Box::new((gen, decoded));
    let raw = Box::into_raw(payload);
    if PostMessageW(
        Some(hwnd),
        WM_APP_RENDER,
        WPARAM(gen as usize),
        LPARAM(raw as isize),
    )
    .is_err()
    {
        // Window died between the decode and the post — reclaim the box so it can't leak.
        drop(Box::from_raw(raw));
    }
}

/// Decode the full composite on its own worker and post it as a second `WM_APP_RENDER`.
///
/// Reuses `gen`, so if the user has already arrowed on, the upgrade is dropped by the exact
/// same staleness check that guards the first result — no new state, no new message. COM is
/// initialised here because `decode_full` can land on the WIC tier, which needs an apartment.
unsafe fn spawn_sharpen(
    hwnd: HWND,
    path: String,
    bytes: std::sync::Arc<Vec<u8>>,
    shown: (i32, i32),
    gen: u64,
) {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // Worth the most of any of these checks: the composite shells out to ImageMagick and
        // can take SECONDS. Running one to completion for a document the user has already
        // arrowed past is the single largest piece of wasted work the viewer could do.
        if abandoned(gen) {
            return;
        }
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let sharp = sharper_composite(&path, &bytes, shown);
        if inited {
            unsafe { CoUninitialize() };
        }
        if let Some(sharp) = sharp {
            let arc = std::sync::Arc::new(sharp);
            // Cache the SHARP one: arrowing back must not drop to the small preview again.
            cache_put(&path, std::sync::Arc::clone(&arc));
            unsafe { post_render(hwnd, gen, Some(arc)) };
        }
    });
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// Temp files are process-id suffixed so concurrent `cargo test` runs cannot race
    /// (the repo-wide convention — see DEVELOPMENT_GOTCHAS).
    fn temp_file(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("st2k_cachetest_{tag}_{}", std::process::id()));
        std::fs::write(&p, body).expect("write temp file");
        p
    }

    fn sample() -> std::sync::Arc<DecodedRgba> {
        std::sync::Arc::new(DecodedRgba::full(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]))
    }

    #[test]
    fn cached_decode_round_trips() {
        let p = temp_file("roundtrip", b"original");
        let path = p.to_string_lossy().into_owned();
        cache_put(&path, sample());
        let hit = cache_get(&path).expect("just-cached entry must hit");
        assert_eq!((hit.w, hit.h), (2, 1));
        assert_eq!(hit.rgba, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let _ = std::fs::remove_file(&p);
    }

    /// The one that matters: a file edited under the same name MUST miss. A cache keyed on
    /// the path alone would confidently paint the previous file's pixels — worse than slow.
    #[test]
    fn edited_file_misses() {
        let p = temp_file("edited", b"original contents");
        let path = p.to_string_lossy().into_owned();
        cache_put(&path, sample());
        assert!(cache_get(&path).is_some(), "sanity: it should be cached");

        // A different length changes the key even if the clock has not ticked over.
        std::fs::write(&p, b"different contents entirely").expect("rewrite");
        assert!(
            cache_get(&path).is_none(),
            "an edited file must not serve the old decode"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// A cached FULL-resolution decode must never be replaced by a scaled one.
    ///
    /// Both can be in flight for one file at once - a zoom's full-resolution fetch and a
    /// revisit's codec-scaled decode - and they finish in whichever order the scheduler picks.
    /// Losing the full pixels to a late-landing scaled result would silently undo the work a
    /// zoom had already paid for, and the only symptom would be the next zoom being slow again.
    #[test]
    fn a_scaled_decode_never_evicts_a_full_resolution_one() {
        let p = temp_file("nodowngrade", b"original");
        let path = p.to_string_lossy().into_owned();

        let full = std::sync::Arc::new(DecodedRgba::full(4, 4, vec![9u8; 4 * 4 * 4]));
        cache_put(&path, full);
        assert!(cache_get(&path).expect("cached").is_full());

        // A scaled decode of the SAME file lands afterwards: it must be ignored.
        let scaled = std::sync::Arc::new(DecodedRgba::scaled(2, 2, vec![1u8; 2 * 2 * 4], (4, 4)));
        cache_put(&path, scaled);
        assert!(
            cache_get(&path).expect("still cached").is_full(),
            "the full-resolution entry must survive a later scaled one"
        );

        // The reverse order is fine: an upgrade always wins.
        let p2 = temp_file("upgrade", b"original");
        let path2 = p2.to_string_lossy().into_owned();
        cache_put(
            &path2,
            std::sync::Arc::new(DecodedRgba::scaled(2, 2, vec![1u8; 2 * 2 * 4], (4, 4))),
        );
        cache_put(
            &path2,
            std::sync::Arc::new(DecodedRgba::full(4, 4, vec![9u8; 4 * 4 * 4])),
        );
        assert!(
            cache_get(&path2).expect("cached").is_full(),
            "a full-resolution decode must replace a scaled one"
        );

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn missing_file_never_hits_and_is_not_worth_prefetching() {
        let missing = std::env::temp_dir()
            .join(format!("st2k_cachetest_absent_{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        assert!(cache_get(&missing).is_none());
        // `cache_has` answers "true" for unreadable paths so `spawn_prefetch` skips them
        // rather than spawning a worker that can only fail.
        assert!(cache_has(&missing));
    }
}

#[cfg(test)]
mod render_size_tests {
    use super::*;

    /// The opaque fast path in [`make_dib_hinted`] claims to be the compositing loop with the
    /// multiply and divide removed, not an approximation of it. That is only true if the
    /// arithmetic genuinely reduces to the identity at full alpha — so check every combination
    /// rather than trusting the algebra in the comment.
    #[test]
    fn composite_at_full_alpha_is_exactly_the_source() {
        for s in 0..=255u32 {
            for d in 0..=255u32 {
                assert_eq!(
                    composite_channel(s, d, 255),
                    s as u8,
                    "a fully opaque source must ignore the background (s={s}, d={d})"
                );
            }
        }
    }

    /// Abandonment must fire for a NEWER generation and never for an equal or older one.
    ///
    /// The `>` rather than `!=` is the whole test. Treating "different" as "stale" reads fine
    /// and is wrong in one direction that matters: a worker whose generation is somehow AHEAD
    /// of the published one would cancel itself, i.e. live work would be silently dropped and
    /// the viewer would sit on "Loading…" forever. Wasting some work is recoverable; cancelling
    /// the work someone is waiting for is not.
    #[test]
    fn only_a_newer_generation_abandons_a_worker() {
        begin_generation(100);
        assert!(!abandoned(100), "the live generation must never abandon");
        assert!(abandoned(99), "an older worker has been superseded");
        assert!(
            !abandoned(101),
            "a worker AHEAD of the published generation must keep going, not cancel itself"
        );
        begin_generation(0); // leave the global as other tests expect to find it
    }

    /// And the other end: at zero alpha the background must survive untouched, which is what
    /// makes the loop a real source-over rather than a lerp with rounding bias.
    #[test]
    fn composite_at_zero_alpha_is_exactly_the_background() {
        for d in 0..=255u32 {
            assert_eq!(composite_channel(200, d, 0), d as u8);
        }
    }

    /// The invariant the whole deferred-decode scheme rests on: a scaled decode holds SMALL
    /// pixels while the render still reports the REAL image size.
    ///
    /// If `iw`/`ih` ever came back as the bitmap's size instead, the failure would be quiet and
    /// everywhere — the caption would report the wrong dimensions, the window would open at the
    /// wrong size, "100%" would mean 100% of a downscale, and `wants_full_resolution` could
    /// never fire because the bitmap would always look big enough.
    #[test]
    fn a_scaled_render_reports_the_real_image_size_not_the_bitmaps() {
        let nat = (4000, 3000);
        let d = DecodedRgba::scaled(400, 300, vec![255u8; 400 * 300 * 4], nat);
        assert!(!d.is_full(), "a scaled decode is not the whole image");
        let rd = unsafe { make_render_for(&d, 0x0020_2020) }.expect("build");
        assert_eq!((rd.iw, rd.ih), nat, "reports the real image size");
        assert_eq!((rd.bw, rd.bh), (400, 300), "holds only the small pixels");

        // A full decode must be indistinguishable from before any of this existed.
        let f = DecodedRgba::full(400, 300, vec![255u8; 400 * 300 * 4]);
        assert!(f.is_full());
        let rd = unsafe { make_render_for(&f, 0x0020_2020) }.expect("build");
        assert_eq!((rd.iw, rd.ih), (400, 300));
        assert_eq!((rd.bw, rd.bh), (400, 300));
    }

    /// Zoom escalation has to fire exactly when the bitmap runs out of detail, and never for a
    /// full-resolution render (there is nothing sharper to fetch, and asking forever would
    /// spawn a decode per repaint).
    #[test]
    fn full_resolution_is_requested_only_once_the_zoom_outgrows_the_bitmap() {
        let pane = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let scaled = DecodedRgba::scaled(400, 300, vec![255u8; 400 * 300 * 4], (4000, 3000));
        let rd = unsafe { make_render_for(&scaled, 0) }.expect("build");
        // Aspect-fit puts 4000x3000 into 800x600, i.e. 800 px on screen against 400 held.
        assert!(
            wants_full_resolution(&rd, &pane, 1.0),
            "800 px of screen from a 400 px bitmap is a 2x stretch, well past tolerance"
        );
        // Half the pane: 400 px on screen, exactly what the bitmap holds.
        let half = RECT {
            right: 400,
            bottom: 300,
            ..pane
        };
        assert!(!wants_full_resolution(&rd, &half, 1.0), "exactly covered");
        // Inside the tolerance: a 20% stretch is not worth a decode (see
        // `FIT_UPSCALE_TOLERANCE` for why this band has to exist at all).
        assert!(
            !wants_full_resolution(&rd, &half, 1.2),
            "a stretch under tolerance must NOT trigger a fetch, or the fit view on a 4K              panel would demand a full decode on every navigation"
        );
        assert!(
            wants_full_resolution(&rd, &half, 1.35),
            "past tolerance, the real pixels are fetched"
        );
        assert!(
            wants_full_resolution(&rd, &half, 2.0),
            "zooming well past what it holds must ask"
        );

        let full = DecodedRgba::full(4000, 3000, vec![255u8; 16]);
        // `make_render_for` would need the real pixel buffer; check the predicate directly on a
        // full-resolution render built at its own size.
        let rd_full = unsafe { make_render(2, 2, &[255u8; 16], 0) }.expect("build");
        assert!(
            !wants_full_resolution(&rd_full, &pane, 8.0),
            "a full-resolution render must never ask, at any zoom"
        );
        assert!(full.is_full());
    }
}

/// `--bench-preview` hook: decode `path` the way a COLD arrow-key step does — full read plus
/// full decode, no cache — then populate the cache exactly as `spawn_decode` does. Returns the
/// decoded size so the caller can tell a real decode from a miss.
pub(super) fn bench_decode_uncached(path: &str) -> Option<(i32, i32)> {
    let d = read_and_decode(path)?;
    let dims = (d.w, d.h);
    cache_put(path, std::sync::Arc::new(d));
    Some(dims)
}

/// `--bench-preview` hook: the WARM path — what `spawn_decode` does on a revisit. Deliberately
/// goes through `cache_get`, copy included, so the number is the real cost of a cache hit and
/// not an idealised pointer lookup.
pub(super) fn bench_decode_cached(path: &str) -> Option<(i32, i32)> {
    cache_get(path).map(|d| (d.w, d.h))
}

/// `--bench-preview` hook: the DISPLAY cost — turning decoded pixels into the premultiplied DIB
/// the window blits. Measured separately because on a cache hit it is the ONLY work left, and
/// the end-to-end arrow bench says a prefetched 12 MP photo still costs ~100 ms per step. If
/// that time is here, no decoder change can help it.
///
/// `--bench-preview` hook: what the codec-scaled decode costs, against the full decode in the
/// `cold` column.
///
/// This is the number that decides whether the full decode can become LAZY (issue 4/5): if
/// asking the codec for a display-sized picture is a fraction of decoding full size, then an
/// arrow step never needs the full one until the user zooms. If it is not, there is nothing
/// to win and the idea dies here. `None` for anything the fast path declines — a small source,
/// or a format the OS codecs will not open.
pub(super) fn bench_scaled_decode(path: &str) -> Option<u128> {
    let t = std::time::Instant::now();
    let d = display_scaled_first_paint(path)?;
    let us = t.elapsed().as_micros();
    let _ = d;
    Some(us)
}

pub(super) fn bench_make_render(path: &str) -> Option<u128> {
    let d = cache_get(path)?;
    let t = std::time::Instant::now();
    let rd = unsafe { make_render(d.w, d.h, &d.rgba, 0x0020_2020) };
    let us = t.elapsed().as_micros();
    drop(rd); // frees the HBITMAP; leaking one per benched file would skew later steps
    Some(us)
}

/// Synchronous decode for the headless `--shot` path (off the UI hot path, no worker).
///
/// Resolves the sharp composite inline: a still capture gets no second paint, so the upgrade
/// the live viewer receives asynchronously has to happen here for the shot to show it.
pub(super) fn decode_sync(path: &str) -> Option<DecodedRgba> {
    let first = read_and_decode(path)?;
    if let Ok(head) = sagethumbs2k_core::decode::read_preview_capped(path) {
        if let Some(sharp) = sharper_composite(path, &head, (first.w, first.h)) {
            return Some(sharp);
        }
    }
    Some(first)
}

/// Markdown remote-image fetch cap: badges are a few KB, hotlinked art rarely tops 8 MB.
const MD_IMG_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Per-phase network timeout for one markdown image (seconds).
const MD_IMG_TIMEOUT_SECS: u64 = 8;

/// Fetch + decode one REMOTE markdown image on a worker thread (opt-in toggle path).
/// HTTPS-only + byte-capped via `http_fetch_capped`; decode is budget-bounded; the result
/// posts back as `WM_APP_MDIMG` with `Box<(gen, src, Option<DecodedRgba>)>` (a stale `gen`
/// is dropped by the handler). The UI thread never blocks.
pub(super) unsafe fn spawn_md_img(hwnd: HWND, src: String, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        let decoded =
            crate::sponsors::http_fetch_capped(&src, false, MD_IMG_MAX_BYTES, MD_IMG_TIMEOUT_SECS)
                .and_then(|b| decode_preview_budgeted(std::sync::Arc::new(b)))
                .map(|img| {
                    // Same display-cap policy as local markdown images (bounds the cached DIB).
                    let img = if img.width() > 2048 || img.height() > 4096 {
                        img.thumbnail(2048, 4096)
                    } else {
                        img
                    };
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    DecodedRgba::full(w, h, rgba.into_raw())
                });
        let payload: Box<(u64, String, Option<DecodedRgba>)> = Box::new((gen, src, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            super::window::WM_APP_MDIMG,
            WPARAM(gen as usize),
            LPARAM(raw as isize),
        )
        .is_err()
        {
            drop(Box::from_raw(raw)); // window died before the post — reclaim
        }
    });
}

/// Decode PDF `page` (0-based) via the OS renderer + fetch the page count, posting the count
/// (`WM_APP_PDFINFO`) and then the page image (`WM_APP_RENDER`, reusing the normal install path).
pub(super) unsafe fn spawn_decode_pdf(hwnd: HWND, path: String, page: u32, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // Page-turn key held down: the OS rasteriser is the expensive part, so bail before it
        // rather than render a page nobody is on any more.
        if abandoned(gen) {
            return;
        }
        let rendered = sagethumbs2k_core::decode::read_capped(&path)
            .ok()
            .and_then(|bytes| sagethumbs2k_core::pdf::render_page_counted(&bytes, page, 1600));
        let (rgba, count) = match rendered {
            Some((png, count)) => {
                let d = image::load_from_memory(&png).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    DecodedRgba::full(w, h, rgba.into_raw())
                });
                (d, Some(count))
            }
            None => (None, None),
        };
        if let Some(c) = count {
            let cb: Box<(u64, u32)> = Box::new((gen, c));
            let raw = Box::into_raw(cb);
            if PostMessageW(
                Some(hwnd),
                WM_APP_PDFINFO,
                WPARAM(gen as usize),
                LPARAM(raw as isize),
            )
            .is_err()
            {
                drop(Box::from_raw(raw));
            }
        }
        let payload: Box<(u64, Option<SharedRgba>)> =
            Box::new((gen, rgba.map(std::sync::Arc::new)));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            WM_APP_RENDER,
            WPARAM(gen as usize),
            LPARAM(raw as isize),
        )
        .is_err()
        {
            drop(Box::from_raw(raw));
        }
    });
}

/// Read the file and run the budgeted decoder, converting the result to tight RGBA8.
fn read_and_decode(path: &str) -> Option<DecodedRgba> {
    // Formats that stream + downscale off the file handle (OpenEXR) never go
    // through the bounded whole-file read — a 12K render pass is past every cap.
    if let Some(img) = streamed_decode(path) {
        return Some(img);
    }
    let bytes = sagethumbs2k_core::decode::read_preview_capped(path).ok()?;
    decode_loaded(std::sync::Arc::new(bytes))
}

/// The by-path streaming decode (see `decode::decode_preview_streamed`), converted
/// to tight RGBA8. `None` when the path isn't one of those formats.
fn streamed_decode(path: &str) -> Option<DecodedRgba> {
    let img = sagethumbs2k_core::decode::decode_preview_streamed(
        path,
        sagethumbs2k_core::decode::EXR_PATH_EDGE,
    )?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    Some(DecodedRgba::full(w, h, rgba.into_raw()))
}

/// Decode bytes already acquired by the path-aware reader. Keeping this separate lets the
/// animation probe fall through without issuing a second file read for ordinary PNG/WebP/GIF.
///
/// Takes the buffer as an `Arc` because the caller ALSO hands it to the sharpen pass. It used
/// to be a `Vec` and the caller cloned it, which is a full copy of the file on every preview:
/// invisible for a 2 MB JPEG, 120 MB for a big PNG.
fn decode_loaded(bytes: std::sync::Arc<Vec<u8>>) -> Option<DecodedRgba> {
    let img = decode_preview_budgeted(bytes)?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    Some(DecodedRgba::full(w, h, rgba.into_raw()))
}

/// Run `decode::decode_preview` on a detached sub-thread, returning its result only if it
/// finishes within [`DECODE_BUDGET`]. On timeout returns `None` and abandons the
/// sub-thread (it sends into a dropped channel and exits on its own). The sub-thread holds
/// a COM MTA apartment because the WIC decode tier (HEIC/RAW/JPEG-XR) needs it — the
/// detach/timeout shape is verbatim from `previewhandler::decode_preview_budgeted`, minus the
/// DLL `ModuleRef` pin (this is an EXE, not the shell-loaded DLL).
///
/// **Deliberately NOT `decode_preview_capped`, unlike the preview-pane version.** This is the
/// one decode this whole viewer uses for zoom-to-detail (`spawn_decode_full`), the headless
/// `--shot` capture (`decode_sync`), and the `--bench-preview` "cold, full decode" measurement
/// — all three need the real resolution, and `display_scaled_first_paint`'s doc comment
/// depends on this call always being a full decode ("zoom still has full resolution behind
/// it... byte-for-byte what it was before"). Capping it to the pane's small target edge would
/// fix the same 12s-budget risk previewhandler's issue #11 fix addressed, but it would also
/// silently cap every zoom and screenshot in the viewer — that needs a separate, smaller-edge
/// decode path for the background-prefetch case specifically, not a blanket cap here.
fn decode_preview_budgeted(bytes: std::sync::Arc<Vec<u8>>) -> Option<image::DynamicImage> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = sagethumbs2k_core::decode::decode_preview(&bytes).ok();
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(out);
    });
    rx.recv_timeout(DECODE_BUDGET).ok().flatten()
}

/// Build a top-down 32bpp DIB of `rgba` composited over the opaque `bg` (`COLORREF`
/// 0x00BBGGRR), so painting is a plain `StretchBlt`. `None` on a malformed size /
/// allocation failure (never panics on attacker-controlled dims). Verbatim port of
/// `previewhandler::make_dib`.
pub(super) unsafe fn make_dib(iw: i32, ih: i32, rgba: &[u8], bg: u32) -> Option<HBITMAP> {
    make_dib_hinted(iw, ih, rgba, bg, None)
}

/// True when every pixel is fully opaque — the common case for a photo, and the one that lets
/// [`make_dib_hinted`] skip the compositing arithmetic entirely.
fn all_opaque(rgba: &[u8], px: usize) -> bool {
    (0..px).all(|i| rgba[i * 4 + 3] == 255)
}

/// Source-over composite of one channel: `s` at coverage `a` laid onto `d`. Mirrors the private
/// `comp` closure inside `sagethumbs2k_core::safety::composite_rgba_over_bg` (the now-shared
/// implementation `make_dib_hinted` below delegates to) — kept here, `#[cfg(test)]`-only, so the
/// `a == 255` / `a == 0` reductions that fast path relies on stay a property a test can assert
/// against, rather than just a claim in a comment. Not production code any more (nothing here
/// calls it outside `render_size_tests`), hence the test-only gate.
#[cfg(test)]
fn composite_channel(s: u32, d: u32, a: u32) -> u8 {
    (((s * a) + (d * (255 - a)) + 127) / 255) as u8
}

/// [`make_dib`] with a caller-supplied opacity answer. `None` means "work it out", which costs a
/// full pass over the alpha bytes; callers that already know (because they had to ask the same
/// question to choose a DIB builder at all) pass `Some` and save it.
///
/// A thin wrapper over [`sagethumbs2k_core::safety::composite_rgba_over_bg`] — the actual
/// compositing loop is shared with `previewhandler::make_dib` (the Explorer preview-pane host),
/// which used to carry its own hand-copied duplicate that never got this opacity-hint fast
/// path. `all_opaque` above stays local: `make_render`'s premultiplied path below still needs it
/// directly, and that path is NOT part of this shared loop (different output — premultiplied
/// BGRA for `AlphaBlend`, not composited-over-bg).
unsafe fn make_dib_hinted(
    iw: i32,
    ih: i32,
    rgba: &[u8],
    bg: u32,
    opaque: Option<bool>,
) -> Option<HBITMAP> {
    sagethumbs2k_core::safety::composite_rgba_over_bg(iw, ih, rgba, bg, opaque)
}

/// The same DIB, but for the MAIN image pane, where transparency has to survive to paint time so a
/// checkerboard can show through it. Returns `(bitmap, has_alpha)`.
///
/// When the image is fully opaque this produces byte-identical output to [`make_dib`] and the
/// caller keeps using the plain `StretchBlt` path, so the overwhelmingly common case (a photo) is
/// completely unchanged, HALFTONE downscaling included. Only when some pixel is actually
/// translucent does it emit PREMULTIPLIED BGRA for `AlphaBlend`; premultiplied data is also what
/// makes the intermediate `StretchBlt` in [`paint_image`] filter correctly, since premultiplied
/// channels are linearly interpolatable and straight ones are not.
pub(super) unsafe fn make_render(iw: i32, ih: i32, rgba: &[u8], bg: u32) -> Option<RenderData> {
    if iw <= 0 || ih <= 0 {
        return None;
    }
    let px = (iw as usize).checked_mul(ih as usize)?;
    if rgba.len() < px.checked_mul(4)? {
        return None;
    }
    let has_alpha = !all_opaque(rgba, px);
    if !has_alpha {
        // The opacity question is already answered — hand it down rather than let `make_dib`
        // walk all 12 million alpha bytes a second time to reach the same conclusion.
        return make_dib_hinted(iw, ih, rgba, bg, Some(true))
            .map(|h| RenderData::opaque(h, iw, ih));
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = iw;
    bmi.bmiHeader.biHeight = -ih; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        return None;
    }
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    for i in 0..px {
        let a = rgba[i * 4 + 3] as u32;
        let pm = |s: u8| (((s as u32 * a) + 127) / 255) as u8;
        dst[i * 4] = pm(rgba[i * 4 + 2]); // B
        dst[i * 4 + 1] = pm(rgba[i * 4 + 1]); // G
        dst[i * 4 + 2] = pm(rgba[i * 4]); // R
        dst[i * 4 + 3] = a as u8;
    }
    Some(RenderData {
        hbmp,
        iw,
        ih,
        bw: iw,
        bh: ih,
        alpha: true,
        src: bits as *const u8,
        scaled: RefCell::new(None),
    })
}

/// Build the window's image render from a finished decode, whether that decode is the whole
/// image or a codec-scaled stand-in for it.
///
/// The one place that knows how to keep `DecodedRgba::nat` and `RenderData::iw` in step, so no
/// caller has to remember that the pixels and the image can be different sizes.
pub(super) unsafe fn make_render_for(d: &DecodedRgba, bg: u32) -> Option<RenderData> {
    let rd = make_render(d.w, d.h, &d.rgba, bg)?;
    Some(if d.is_full() {
        rd
    } else {
        rd.with_native(d.nat)
    })
}

/// A `dw`x`dh` copy of `rd`, box-filtered (a true area average), cached until the size changes.
/// `None` when it is not worth doing or could not be built, and the caller then lets GDI scale.
///
/// **Why this exists.** A translucent image cannot go through `StretchBlt`'s good `HALFTONE`
/// filter, because that mode treats the surface as plain RGB and destroys the alpha byte. The only
/// GDI call that respects alpha is `AlphaBlend`, and it ignores the stretch mode entirely, so it
/// point-samples when shrinking. Measured on a concentric-ring test pattern at a 3x downscale:
/// `AlphaBlend` produced 58% more spurious high-frequency energy than the true area average, versus
/// `HALFTONE`'s 21%, i.e. visible aliasing on fine detail. Averaging the pixels here fixes that and
/// is actually CLOSER to ground truth than `HALFTONE` is.
///
/// Only used when SHRINKING. Enlarging point-samples, which is what you want for inspecting pixels.
unsafe fn scaled_for(rd: &RenderData, dw: i32, dh: i32) -> Option<HBITMAP> {
    // Against the BITMAP's dims, not the image's: `hbmp` may already be a scaled decode, and
    // shrinking is only worth doing relative to what is actually there to shrink.
    if rd.src.is_null() || dw <= 0 || dh <= 0 || dw >= rd.bw || dh >= rd.bh {
        return None;
    }
    if let Some((cw, ch, h)) = *rd.scaled.borrow() {
        if cw == dw && ch == dh {
            return Some(h);
        }
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = dw;
    bmi.bmiHeader.biHeight = -dh;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0;
    let mut bits: *mut c_void = core::ptr::null_mut();
    let out = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(out.into());
        return None;
    }
    let (sw, sh) = (rd.bw as usize, rd.bh as usize);
    let src = core::slice::from_raw_parts(rd.src, sw * sh * 4);
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, (dw * dh) as usize * 4);
    // Source span of each destination row/column, precomputed so the inner loop stays tight.
    let xs: Vec<(usize, usize)> = (0..dw as usize)
        .map(|x| {
            let a = x * sw / dw as usize;
            let b = ((x + 1) * sw).div_ceil(dw as usize).min(sw);
            (a, b.max(a + 1))
        })
        .collect();
    for y in 0..dh as usize {
        let y0 = y * sh / dh as usize;
        let y1 = (((y + 1) * sh).div_ceil(dh as usize)).min(sh).max(y0 + 1);
        for (x, &(x0, x1)) in xs.iter().enumerate() {
            let (mut b, mut g, mut r, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                let row = sy * sw * 4;
                for sx in x0..x1 {
                    let i = row + sx * 4;
                    b += src[i] as u32;
                    g += src[i + 1] as u32;
                    r += src[i + 2] as u32;
                    a += src[i + 3] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * dw as usize + x) * 4;
            dst[o] = (b / n) as u8;
            dst[o + 1] = (g / n) as u8;
            dst[o + 2] = (r / n) as u8;
            dst[o + 3] = (a / n) as u8;
        }
    }
    if let Some((_, _, old)) = rd.scaled.borrow_mut().replace((dw, dh, out)) {
        let _ = DeleteObject(old.into());
    }
    Some(out)
}

/// Would drawing `rd` into `rc` at `zoom` magnify its bitmap, i.e. is the render a codec-scaled
/// stand-in that has run out of detail?
///
/// `false` for a full-resolution render (nothing sharper exists) and for any zoom the scaled
/// pixels still cover, which is the whole of ordinary fit-view browsing.
pub(super) fn wants_full_resolution(rd: &RenderData, rc: &RECT, zoom: f64) -> bool {
    if rd.bw >= rd.iw && rd.bh >= rd.ih {
        return false; // already the whole image
    }
    let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
    let scale = fit_scale(rd.iw, rd.ih, cw, ch) * zoom;
    let need_w = rd.iw as f64 * scale;
    let need_h = rd.ih as f64 * scale;
    need_w > rd.bw as f64 * FIT_UPSCALE_TOLERANCE || need_h > rd.bh as f64 * FIT_UPSCALE_TOLERANCE
}

/// Aspect-fit scale (image px -> screen px) of `rd` inside `(cw, ch)`. Shared by the paint
/// and the zoom-at-cursor math so they never disagree.
pub(super) fn fit_scale(iw: i32, ih: i32, cw: i32, ch: i32) -> f64 {
    if iw <= 0 || ih <= 0 || cw <= 0 || ch <= 0 {
        return 1.0;
    }
    f64::min(cw as f64 / iw as f64, ch as f64 / ih as f64)
}

/// Paint the image `rd` into `rc`, letterboxed with `bg`, at `zoom`x the aspect-fit scale and
/// offset by `pan` (device px). `zoom == 1.0`, `pan == (0,0)` is the plain aspect-fit centered
/// draw. Ported from `previewhandler::draw` (fill = letterbox, then `HALFTONE` `StretchBlt`).
pub(super) unsafe fn paint_image(
    hdc: HDC,
    rc: &RECT,
    rd: &RenderData,
    bg: u32,
    zoom: f64,
    pan: (i32, i32),
    checker: Option<i32>,
) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());

    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    if cw <= 0 || ch <= 0 || rd.iw <= 0 || rd.ih <= 0 {
        return;
    }
    let scale = fit_scale(rd.iw, rd.ih, cw, ch) * zoom;
    let dw = ((rd.iw as f64 * scale).round() as i32).max(1);
    let dh = ((rd.ih as f64 * scale).round() as i32).max(1);
    let dx = rc.left + (cw - dw) / 2 + pan.0;
    let dy = rc.top + (ch - dh) / 2 + pan.1;

    let memdc = CreateCompatibleDC(Some(hdc));
    let old = SelectObject(memdc, rd.hbmp.into());
    SetStretchBltMode(hdc, HALFTONE);
    // The branch MUST key on `rd.alpha`, never on the checkerboard setting. A translucent bitmap
    // holds PREMULTIPLIED, un-composited pixels (see `make_render`), and `StretchBlt` ignores the
    // alpha byte entirely, so blitting one paints the premultiplied values as if they were opaque:
    // a 50%-alpha white pixel lands as mid-grey. Turning the checkerboard OFF must therefore still
    // take the blend path, just without the pattern under it (the flat `bg` fill above is what it
    // composites onto instead).
    match rd.alpha {
        // Translucent: optional checkerboard, then compose the premultiplied bitmap over it.
        //
        // `AlphaBlend` does its own scaling and ignores the stretch mode, and there is no way to
        // pre-scale through HALFTONE first: `StretchBlt` in HALFTONE mode treats the surface as
        // plain RGB and DESTROYS the alpha byte, so an intermediate scratch surface comes out
        // fully transparent and nothing draws at all (measured, not assumed). So the blend reads
        // straight from the source bitmap. Only translucent images take this path.
        true => {
            if let Some(cell) = checker {
                let (c0, c1) = sagethumbs2k_core::checker::checker_shades(bg);
                let cr = RECT {
                    left: dx,
                    top: dy,
                    right: dx + dw,
                    bottom: dy + dh,
                };
                sagethumbs2k_core::checker::fill_checker(hdc, &cr, c0, c1, cell);
            }
            let bf = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            // Shrinking: area-average it ourselves first and blend 1:1, because AlphaBlend's own
            // scaler aliases badly (see `scaled_for`). Enlarging, or if the scratch could not be
            // built, blend straight from the source.
            match scaled_for(rd, dw, dh) {
                Some(s) => {
                    let sdc = CreateCompatibleDC(Some(hdc));
                    let olds = SelectObject(sdc, s.into());
                    let _ = AlphaBlend(hdc, dx, dy, dw, dh, sdc, 0, 0, dw, dh, bf);
                    SelectObject(sdc, olds);
                    let _ = DeleteDC(sdc);
                }
                None => {
                    let _ = AlphaBlend(hdc, dx, dy, dw, dh, memdc, 0, 0, rd.bw, rd.bh, bf);
                }
            }
        }
        // Opaque: unchanged from before any of this existed. `make_render` produced byte-identical
        // output to the old `make_dib` for these, so photos take exactly the old HALFTONE path.
        false => {
            let _ = StretchBlt(
                hdc,
                dx,
                dy,
                dw,
                dh,
                Some(memdc),
                0,
                0,
                rd.bw,
                rd.bh,
                SRCCOPY,
            );
        }
    }
    SelectObject(memdc, old);
    let _ = DeleteDC(memdc);
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    /// True if `s` contains a common CJK ideograph — the marker of a decode gone wrong when the
    /// input was Latin text.
    fn has_cjk(s: &str) -> bool {
        s.chars().any(|c| matches!(c as u32, 0x4E00..=0x9FFF))
    }

    #[test]
    fn utf8_wins_outright() {
        assert_eq!(decode_text("hello — 世界 🌏".as_bytes()), "hello — 世界 🌏");
        assert_eq!(decode_text(b"plain ascii\n"), "plain ascii\n");
        assert_eq!(decode_text(b""), "");
    }

    #[test]
    fn strips_boms() {
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice("héllo".as_bytes());
        assert_eq!(decode_text(&b), "héllo");

        let mut le = vec![0xFF, 0xFE];
        le.extend("hi".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_text(&le), "hi");

        let mut be = vec![0xFE, 0xFF];
        be.extend("hi".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(decode_text(&be), "hi");
    }

    #[test]
    fn bomless_utf16_is_sniffed() {
        let text = "the quick brown fox jumps over the lazy dog";
        let le: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_text(&le), text);
        let be: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(decode_text(&be), text);
    }

    /// GBK is still a Chinese national standard; before this these bytes were a wall of U+FFFD.
    #[test]
    fn gbk_chinese_decodes() {
        const GBK: &[u8] = &[
            0xC4, 0xE3, 0xBA, 0xC3, 0xA3, 0xAC, 0xCA, 0xC0, 0xBD, 0xE7, 0xA3, 0xA1, 0xD5, 0xE2,
            0xCA, 0xC7, 0xD2, 0xBB, 0xB8, 0xF6, 0xB2, 0xE2, 0xCA, 0xD4, 0xCE, 0xC4, 0xBC, 0xFE,
            0xA1, 0xA3,
        ];
        assert_eq!(decode_text(GBK), "你好，世界！这是一个测试文件。");
    }

    /// Kana is the signal that keeps Shift-JIS from being read as GBK.
    #[test]
    fn shift_jis_japanese_decodes() {
        const SJIS: &[u8] = &[
            0x82, 0xB1, 0x82, 0xEA, 0x82, 0xCD, 0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, 0x82, 0xCC,
            0x83, 0x65, 0x83, 0x4C, 0x83, 0x58, 0x83, 0x67, 0x82, 0xC5, 0x82, 0xB7, 0x81, 0x42,
        ];
        assert_eq!(decode_text(SJIS), "これは日本語のテキストです。");
    }

    /// Hangul is the equivalent signal for EUC-KR.
    #[test]
    fn euc_kr_korean_decodes() {
        const EUCKR: &[u8] = &[
            0xBE, 0xC8, 0xB3, 0xE7, 0xC7, 0xCF, 0xBC, 0xBC, 0xBF, 0xE4, 0x20, 0xC7, 0xD1, 0xB1,
            0xB9, 0xBE, 0xEE, 0x20, 0xC5, 0xD8, 0xBD, 0xBA, 0xC6, 0xAE, 0xC0, 0xD4, 0xB4, 0xCF,
            0xB4, 0xD9, 0x2E,
        ];
        assert_eq!(decode_text(EUCKR), "안녕하세요 한국어 텍스트입니다.");
    }

    /// The regression that matters in the other direction: ordinary accented Latin text must
    /// never be dragged into a CJK reading just because some table accepted the bytes.
    #[test]
    fn latin1_is_not_mangled_into_cjk() {
        const L1: &[u8] = &[
            0x43, 0x61, 0x66, 0xE9, 0x20, 0x72, 0xE9, 0x73, 0x75, 0x6D, 0xE9, 0x20, 0x6E, 0x61,
            0xEF, 0x76, 0x65, 0x20, 0x73, 0x65, 0xF1, 0x6F, 0x72,
        ];
        let out = decode_text(L1);
        assert!(!has_cjk(&out), "Latin-1 text decoded as CJK: {out:?}");
        assert!(out.starts_with("Caf"), "unexpected decode: {out:?}");
    }

    /// A pure-ASCII byte stream must come back byte-identical whatever the machine's ACP is.
    #[test]
    fn ascii_is_never_reinterpreted() {
        let src = "fn main() { println!(\"hi\"); }\n";
        assert_eq!(decode_text(src.as_bytes()), src);
    }

    #[test]
    fn cjk_score_rejects_halfwidth_katakana_soup() {
        // What Shift-JIS makes of arbitrary high bytes — must not qualify as CJK text.
        assert_eq!(cjk_score("ﾖﾐﾄﾄﾊﾟ"), 0);
        // Real Japanese does.
        assert!(cjk_score("これは日本語です") > 0);
    }
}
