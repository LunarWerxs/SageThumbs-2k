//! Guard: the read path holds bounded memory without the caller asking
//! (READ_CACHE_BOUNDED).
//!
//! Rendering memoises what it decoded. Until 0.33 nothing ever gave that
//! memory back on its own: the eviction API needed `&mut DjVuDocument`, every
//! render entry point holds a shared `&DjVuPage`, and a program that only
//! rendered therefore grew about 5 MB per page of a colour book until it ran
//! out. `djvu_rs::render_cache` closes that — a process-wide ceiling, swept
//! when a cache fill crosses it.
//!
//! The unit the sweep drops is one *layer*, not one page (#813,
//! RENDER_CACHE_LAYER_EVICT): a background, a mask, a converted pixmap. Each
//! carries its own last-used tick, so a page's warm mask survives while its
//! stale background goes, and the resident total overshoots the ceiling by at
//! most one layer rather than one page.
//!
//! The ceiling is process-global state, and `cargo test` runs a binary's tests
//! on parallel threads, so every test here takes `BUDGET_LOCK` and restores the
//! default ceiling before it returns.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use djvu_rs::djvu_document::{DjVuDocument, DjVuPage};
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::render_cache;

/// A colour book with 62 pages: its pages carry BG44, so a full-resolution
/// render really costs megabytes and a ceiling has something to bite on.
const SUBJECT: &str = "tests/fixtures/colorbook.djvu";

/// Serialises the tests in this file — they all move the process-wide ceiling.
static BUDGET_LOCK: Mutex<()> = Mutex::new(());

/// Take the lock, start from a cold cache, and put the default ceiling back
/// when the guard is dropped.
fn exclusive() -> MutexGuard<'static, ()> {
    let guard = BUDGET_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    render_cache::set_budget(render_cache::DEFAULT_BUDGET);
    render_cache::clear();
    guard
}

fn full_size(page: &DjVuPage) -> RenderOptions {
    RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        ..Default::default()
    }
}

/// Render the first `pages` pages of the subject and return the highest
/// process-wide resident total seen along the way.
fn render_pages(doc: &DjVuDocument, pages: usize) -> usize {
    let mut peak = 0;
    for i in 0..pages {
        let page = doc.page(i).expect("page in range");
        let _ = render_pixmap(page, &full_size(page)).expect("render must succeed");
        peak = peak.max(render_cache::resident_bytes());
    }
    peak
}

fn open_subject() -> DjVuDocument {
    let data = std::fs::read(SUBJECT).expect("fixture must exist");
    DjVuDocument::parse(&data).expect("fixture must parse")
}

/// The largest single layer a full-resolution render of `page` leaves in its
/// cache, measured through the public handles: the wavelet background, the
/// RGB pixmap converted from it (same width and height, four bytes a pixel),
/// the mask and the foreground. The overshoot bound below is stated in this
/// unit.
///
/// Touches every layer, so call it before the measurement it serves and
/// clear the cache afterwards.
fn largest_layer(page: &DjVuPage) -> usize {
    let mut largest = 0;
    if let Some(bg) = page.decoded_bg44() {
        largest = largest.max(bg.heap_bytes());
        largest = largest.max(bg.width as usize * bg.height as usize * 4);
    }
    if let Some(mask) = page.decoded_mask() {
        largest = largest.max(mask.data.len());
    }
    if let Some(fg) = page.decoded_fg44() {
        largest = largest.max(fg.data.len());
    }
    largest
}

#[test]
fn the_ceiling_is_on_by_default() {
    let _guard = exclusive();
    assert_eq!(
        render_cache::budget(),
        render_cache::DEFAULT_BUDGET,
        "a program that sets no ceiling must still get one; an unbounded \
         default is the defect READ_CACHE_BOUNDED fixes"
    );
    assert_eq!(
        render_cache::DEFAULT_BUDGET,
        256 * 1024 * 1024,
        "the documented default is 256 MiB; change the docs with the constant"
    );
}

#[test]
fn a_long_read_stays_under_the_ceiling() {
    let _guard = exclusive();
    let doc = open_subject();

    // One page first, to learn what a page of this book costs.
    let one = render_pages(&doc, 1);
    assert!(
        one > 1 << 20,
        "{SUBJECT} now costs only {one} B for a full-resolution page; this \
         guard needs a book whose pages cost megabytes. Replace the fixture."
    );

    let layer = largest_layer(doc.page(0).expect("page 0"));
    assert!(
        layer > 0 && layer < one,
        "one layer ({layer} B) must be a real fraction of a page ({one} B)"
    );
    render_cache::clear();

    // A ceiling of three pages, then render ten. Without the governor the
    // total would be ten pages' worth and keep climbing.
    let ceiling = one * 3;
    render_cache::set_budget(ceiling);
    let peak = render_pages(&doc, 10);

    // Only the layer being filled is protected from the sweep (#813), so the
    // peak can exceed the ceiling by up to one layer. Nothing else may. Before
    // layer-granular eviction the bound was one whole page's cache.
    assert!(
        peak <= ceiling + layer,
        "render cache peaked at {peak} B against a {ceiling} B ceiling \
         ({one} B/page, {layer} B for the largest layer). The sweep is not \
         running, or it is protecting more than the layer being filled."
    );

    render_cache::set_budget(render_cache::DEFAULT_BUDGET);
}

#[test]
fn the_ceiling_can_be_lifted() {
    let _guard = exclusive();
    let doc = open_subject();

    let one = render_pages(&doc, 1);
    render_cache::clear();

    // `usize::MAX` is the documented opt-out: the 0.32 behaviour, for a caller
    // who manages memory itself.
    render_cache::set_budget(usize::MAX);
    let peak = render_pages(&doc, 6);
    assert!(
        peak > one * 4,
        "with the ceiling lifted the cache must grow freely: {peak} B after \
         six pages of {one} B each"
    );

    render_cache::set_budget(render_cache::DEFAULT_BUDGET);
}

#[test]
fn eviction_never_pulls_a_layer_out_from_under_a_reader() {
    let _guard = exclusive();
    let doc = open_subject();

    let page = doc.page(0).expect("page 0");
    let _ = render_pixmap(page, &full_size(page)).expect("render must succeed");
    let mask = page
        .decoded_mask()
        .expect("colorbook page 0 has a JB2 mask");
    let before = mask.data.len();
    assert!(before > 0, "the mask must hold real bytes");

    // The whole point of handing out shared handles: the cache can drop its
    // own copy at any moment and a reader that already has one is unaffected.
    render_cache::clear();
    assert_eq!(
        mask.data.len(),
        before,
        "a held layer must survive the eviction of the cache it came from"
    );
    assert_eq!(
        Arc::strong_count(&mask),
        1,
        "after the sweep the reader must hold the only handle; a leftover \
         reference means the cache did not really let go"
    );
    assert_eq!(
        doc.render_cache_bytes(),
        0,
        "clear() must leave the document's caches empty"
    );

    // And the layer is still reachable — it decodes again, identically.
    let again = page
        .decoded_mask()
        .expect("mask decodes again after eviction");
    assert_eq!(again.data, mask.data, "a rebuilt layer must be identical");

    render_cache::set_budget(render_cache::DEFAULT_BUDGET);
}

#[test]
fn a_page_can_drop_its_own_cache_through_a_shared_borrow() {
    let _guard = exclusive();
    let doc = open_subject();

    let page = doc.page(0).expect("page 0");
    let _ = render_pixmap(page, &full_size(page)).expect("render must succeed");
    assert!(
        page.render_cache_bytes() > 0,
        "the render must leave a cache"
    );

    // `&self`, not `&mut self`: this is what lets a render bound itself.
    page.evict_render_cache();
    assert_eq!(
        page.render_cache_bytes(),
        0,
        "evict_render_cache() through a shared borrow must free the page"
    );
    assert_eq!(
        render_cache::resident_bytes(),
        0,
        "the process-wide total must follow a per-page eviction"
    );
}

/// The acceptance figure for RENDER_CACHE_LAYER_EVICT: under a 16 MiB ceiling
/// the first eight pages of the colour book hold the resident total within one
/// layer of the ceiling on both sides, once the ceiling has been reached.
///
/// A page-granular sweep could only land somewhere in a band one *page* wide
/// below the ceiling (it drops the stalest page whole). Dropping the stalest
/// layer stops as soon as the total dips under the ceiling, so it undershoots
/// by less than one layer; the fill that triggered it overshot by at most one
/// layer. Every page render after the first crossing therefore leaves the
/// total in `(ceiling - layer, ceiling]` and peaks at most `ceiling + layer`.
#[test]
fn a_tight_ceiling_holds_the_total_within_one_layer() {
    let _guard = exclusive();
    let doc = open_subject();

    let one = render_pages(&doc, 1);
    let layer = largest_layer(doc.page(0).expect("page 0"));
    render_cache::clear();

    let ceiling = 16 * 1024 * 1024;
    assert!(
        one * 2 < ceiling && ceiling < one * 8,
        "the subject must cross a {ceiling} B ceiling within eight pages and \
         not within two: it costs {one} B a page"
    );

    render_cache::set_budget(ceiling);
    let mut crossed = false;
    let mut peak = 0;
    let mut low_after_crossing = usize::MAX;
    for i in 0..8 {
        let page = doc.page(i).expect("page in range");
        let _ = render_pixmap(page, &full_size(page)).expect("render must succeed");
        let resident = render_cache::resident_bytes();
        peak = peak.max(resident);
        // The first render that lands under the ceiling by less than a layer,
        // or over it, marks the crossing; from there on the band must hold.
        if resident + layer > ceiling {
            crossed = true;
        }
        if crossed {
            low_after_crossing = low_after_crossing.min(resident);
        }
    }
    assert!(
        crossed,
        "eight pages of {one} B never reached a {ceiling} B ceiling"
    );
    // For the reader who runs this with `--nocapture`: the band the entry in
    // PERF_EXPERIMENTS.md quotes.
    println!(
        "16 MiB ceiling over 8 pages of {one} B: resident peaked at {peak} B, \
         held at least {low_after_crossing} B after crossing; one layer is \
         {layer} B"
    );
    assert!(
        peak <= ceiling + layer,
        "resident total peaked at {peak} B against a {ceiling} B ceiling; \
         the overshoot must be at most one layer ({layer} B), not one page \
         ({one} B)"
    );
    assert!(
        low_after_crossing + layer > ceiling,
        "after crossing the ceiling the total fell to {low_after_crossing} B, \
         more than one layer ({layer} B) under {ceiling} B: the sweep is \
         dropping whole pages, not the stalest layers"
    );

    render_cache::set_budget(render_cache::DEFAULT_BUDGET);
}

/// A sweep frees a page's stale background while the mask that was used more
/// recently stays cached — the whole point of ranking layers, not pages.
#[test]
fn a_sweep_drops_the_stale_background_and_keeps_the_warm_mask() {
    let _guard = exclusive();
    let doc = open_subject();

    let page = doc.page(0).expect("page 0");
    let _ = render_pixmap(page, &full_size(page)).expect("render must succeed");
    let whole = page.render_cache_bytes();
    // Touch the mask last, so it is the most recently used layer of the page.
    let mask = page
        .decoded_mask()
        .expect("colorbook page 0 has a JB2 mask");
    let mask_bytes = mask.data.len();
    assert!(
        mask_bytes > 0 && mask_bytes < whole / 2,
        "the mask ({mask_bytes} B) must be a small part of the page's cache \
         ({whole} B) for this test to tell layers from pages"
    );

    // A ceiling that fits the mask and nothing else on this page.
    let freed = render_cache::set_budget(mask_bytes + 1);
    assert!(freed > 0, "the sweep must have dropped something");
    assert_eq!(
        page.render_cache_bytes(),
        mask_bytes,
        "the page must keep exactly its mask: a page-granular sweep leaves 0, \
         a sweep that ignores ticks leaves some other layer"
    );
    assert!(
        render_cache::resident_bytes() <= mask_bytes + 1,
        "the process-wide total ({} B) must follow the sweep",
        render_cache::resident_bytes()
    );
    let again = page.decoded_mask().expect("the mask is still cached");
    assert!(
        Arc::ptr_eq(&mask, &again),
        "the mask must be the same cached handle, not a fresh decode"
    );

    render_cache::set_budget(render_cache::DEFAULT_BUDGET);
}

/// A layer a reader holds survives its own eviction: the sweep drops the
/// cache's handle, never the reader's, and the reader's handle is then the
/// only one.
#[test]
fn a_held_layer_survives_the_sweep_that_evicts_it() {
    let _guard = exclusive();
    let doc = open_subject();

    let page = doc.page(0).expect("page 0");
    let bg = page
        .decoded_bg44()
        .expect("colorbook page 0 has a BG44 background");
    let (w, h) = (bg.width, bg.height);
    let bytes = bg.heap_bytes();
    assert!(bytes > 0, "the background must hold real bytes");

    // A ceiling under the background's size: the sweep must drop it.
    let freed = render_cache::set_budget(bytes / 2);
    assert!(
        freed >= bytes,
        "the sweep freed {freed} B; it must have dropped the {bytes} B background"
    );
    assert_eq!(
        Arc::strong_count(&bg),
        1,
        "after the sweep the reader must hold the only handle"
    );
    assert_eq!(
        (bg.width, bg.height),
        (w, h),
        "the held background must be intact"
    );
    assert_eq!(bg.heap_bytes(), bytes, "the held background must be intact");

    // Lifting the ceiling, the layer decodes again — a new handle, same data.
    render_cache::set_budget(render_cache::DEFAULT_BUDGET);
    let again = page.decoded_bg44().expect("the background decodes again");
    assert!(
        !Arc::ptr_eq(&bg, &again),
        "a rebuilt layer must be a fresh handle, not the reader's"
    );
    assert_eq!(
        (again.width, again.height, again.heap_bytes()),
        (w, h, bytes),
        "a rebuilt layer must match the original"
    );
}
