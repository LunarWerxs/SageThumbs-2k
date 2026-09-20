//! The inline fonts: the family, the cache keyed by style and the lookup.

use super::*;

/// The five font variants a block draws with, created once and freed together.
pub(in super::super) struct Fonts {
    pub(in super::super) reg: HFONT,
    pub(in super::super) bold: HFONT,
    pub(in super::super) ital: HFONT,
    pub(in super::super) bi: HFONT,
    pub(in super::super) mono: HFONT,
    pub(in super::super) px: i32,
    pub(in super::super) base_bold: bool,
    pub(in super::super) base_italic: bool,
}

impl Fonts {
    pub(in super::super) unsafe fn new(
        hwnd: HWND,
        px: i32,
        base_bold: bool,
        base_italic: bool,
    ) -> Fonts {
        Fonts {
            reg: font(hwnd, px, base_bold, base_italic, false),
            bold: font(hwnd, px, true, base_italic, false),
            ital: font(hwnd, px, base_bold, true, false),
            bi: font(hwnd, px, true, true, false),
            mono: font(hwnd, px - 1, false, false, true),
            px,
            base_bold,
            base_italic,
        }
    }
    pub(in super::super) fn pick(&self, r: &Run) -> HFONT {
        if r.code {
            return self.mono;
        }
        let b = self.base_bold || r.bold;
        let i = self.base_italic || r.italic;
        match (b, i) {
            (true, true) => self.bi,
            (true, false) => self.bold,
            (false, true) => self.ital,
            (false, false) => self.reg,
        }
    }
    /// The spec of the font [`Fonts::pick`] would return — recorded per drawn token so
    /// hit-testing can re-create it after these handles are freed. MUST mirror `pick`/`new`.
    pub(in super::super) fn spec(&self, r: &Run) -> FontSpec {
        if r.code {
            return FontSpec {
                px: self.px - 1,
                bold: false,
                italic: false,
                mono: true,
            };
        }
        FontSpec {
            px: self.px,
            bold: self.base_bold || r.bold,
            italic: self.base_italic || r.italic,
            mono: false,
        }
    }
    pub(in super::super) unsafe fn free(self) {
        for f in [self.reg, self.bold, self.ital, self.bi, self.mono] {
            let _ = DeleteObject(f.into());
        }
    }
}

/// One [`Fonts`] set's cache key: the same `(px, base_bold, base_italic)` triple
/// [`Fonts::new`] takes. `mono` isn't part of it — a `Fonts` always carries a fixed
/// Consolas variant at `px - 1` regardless of the base style, so it never varies per key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct FontKey {
    pub(super) px: i32,
    pub(super) bold: bool,
    pub(super) italic: bool,
}

/// Cache of [`Fonts`] sets keyed by the style that built them, so one `render` pass over a
/// document with N headings/paragraphs/list-items/quotes builds each distinct
/// `(px, bold, italic)` combination once instead of once per block. Before this, every block
/// paint called `Fonts::new` + `free` on its own, so scrolling a long document created and
/// freed hundreds of `HFONT`s per repaint even though a typical document only ever mixes a
/// handful of distinct styles (the heading sizes + body + quote). A `FontCache` is meant to be
/// created once per paint pass (or held longer, by whoever owns it) and dropped when done —
/// `Drop` frees every cached handle, so there's no separate teardown call to remember.
#[derive(Default)]
pub(in super::super) struct FontCache {
    /// The DPI the cached entries were built at. 0 = empty/never built (real DPI is always
    /// >= 96), matching the "0 means unset" convention `win::scaling`'s DPI helpers use.
    pub(super) dpi: i32,
    pub(super) entries: Vec<(FontKey, Fonts)>,
}

impl FontCache {
    /// Look up (or build) the `(px, bold, italic)` entry and return its index. Shared by
    /// [`FontCache::get`]/[`FontCache::get2`] so the DPI-invalidation and build-or-reuse logic
    /// lives in exactly one place.
    pub(super) unsafe fn ensure(&mut self, hwnd: HWND, px: i32, bold: bool, italic: bool) -> usize {
        // `dpi_scale` is the only DPI accessor this module has; 9600 (96 * 100) round-trips
        // through its MulDiv with no rounding, so dividing back by 100 recovers the window's
        // real DPI without a second public accessor in `win::scaling`.
        let dpi = crate::win::dpi_scale(hwnd, 9600) / 100;
        if dpi != self.dpi {
            self.clear();
            self.dpi = dpi;
        }
        let key = FontKey { px, bold, italic };
        match self.entries.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                self.entries.push((key, Fonts::new(hwnd, px, bold, italic)));
                self.entries.len() - 1
            }
        }
    }

    /// Borrowed handles for `(px, base_bold, base_italic)` at `hwnd`'s current DPI —
    /// building and caching a new [`Fonts`] set the first time this exact combination is
    /// asked for. If `hwnd`'s DPI has changed since the last call (a monitor move), every
    /// cached entry is freed and rebuilt first: a `Fonts` bakes in the DPI-scaled pixel size
    /// at creation, so a stale-DPI entry would be wrong, not just wasted.
    ///
    /// Safety: same requirement as [`Fonts::new`] — `hwnd` must be a live window.
    pub(in super::super) unsafe fn get(
        &mut self,
        hwnd: HWND,
        px: i32,
        bold: bool,
        italic: bool,
    ) -> &Fonts {
        let idx = self.ensure(hwnd, px, bold, italic);
        &self.entries[idx].1
    }

    /// Two entries at once, e.g. a table's body + header style, which several helpers need
    /// live together. Both are resolved (built if missing) before either reference is taken, so
    /// the second lookup's possible `Vec` growth can never invalidate the first.
    ///
    /// Safety: same requirement as [`FontCache::get`].
    pub(in super::super) unsafe fn get2(
        &mut self,
        hwnd: HWND,
        a: (i32, bool, bool),
        b: (i32, bool, bool),
    ) -> (&Fonts, &Fonts) {
        let ia = self.ensure(hwnd, a.0, a.1, a.2);
        let ib = self.ensure(hwnd, b.0, b.1, b.2);
        (&self.entries[ia].1, &self.entries[ib].1)
    }

    /// Free every cached handle now, rather than waiting for `Drop`. [`FontCache::ensure`]
    /// calls this itself on a DPI change; nothing else needs to.
    pub(super) fn clear(&mut self) {
        for (_, fonts) in self.entries.drain(..) {
            unsafe { fonts.free() };
        }
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Re-create the font a drawn token was measured with (hit-testing; caller frees it).
pub(crate) unsafe fn font_for(hwnd: HWND, s: FontSpec) -> HFONT {
    font(hwnd, s.px, s.bold, s.italic, s.mono)
}

/// Create a font: `px` @96dpi (DPI-scaled), Segoe UI (or Consolas if `mono`), bold/italic.
pub(in super::super) unsafe fn font(
    hwnd: HWND,
    px: i32,
    bold: bool,
    italic: bool,
    mono: bool,
) -> HFONT {
    let h = crate::win::dpi_scale(hwnd, px);
    let face = crate::win::wide(if mono { "Consolas" } else { "Segoe UI" });
    CreateFontW(
        -h,
        0,
        0,
        0,
        if bold { 700 } else { 400 },
        u32::from(italic),
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        Default::default(),
        PCWSTR(face.as_ptr()),
    )
}
