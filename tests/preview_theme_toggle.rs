//! The Quick preview's light/dark caption button (`Btn::Theme`, headless `--toggle-theme`).
//!
//! The button exists because the app-wide Theme setting cannot serve one case: a dark photograph
//! or a bright scan that reads better against the opposite background from the one you normally
//! want. So it flips THIS window and nothing else, for as long as it is open.
//!
//! What makes it worth a test rather than a glance is that flipping the palette is only a third
//! of the job. The theme is also baked into things that were composited BEFORE the click: the
//! letterbox background inside the decoded bitmap, the Markdown inline-image cache, and the DWM
//! frame attribute that was applied once at window creation. `toggle_theme` handles that by
//! reloading, and a still of a window that merely OPENED in the other skin would prove none of
//! it. These captures drive the real click path (`--toggle-theme` calls `do_action(Btn::Theme)`,
//! exactly as `--toggle-source` does for the source toggle) through the documented headless
//! `--shot --window preview` harness (CLAUDE.md §6).
//!
//! Assertions are on SAMPLED PIXELS, not on whole-image bytes: the point here is "did the surface
//! actually change colour", and a byte comparison would pass just as happily on a window that
//! changed one glyph. The caption and the content area are sampled separately because they are
//! painted by different code — an early version of this change repainted the client and left the
//! frame dark around a light page.
//!
//! Scratch dirs are removed only when a test PASSES, so a failure leaves its PNGs on disk
//! (%TEMP%\st2k_theme_shot_<pid>_<case>) as the evidence.
//!
//! Needs a window station (real GDI + `PrintWindow`), like the other headless shot tooling.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Write `body` as `name` in a per-case scratch dir; returns the dir and the file path.
fn sample(case: &str, name: &str, body: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("st2k_theme_shot_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let doc = dir.join(name);
    std::fs::write(&doc, body).expect("write sample");
    (dir, doc)
}

/// Drop `case`'s scratch dir. Call AFTER the asserts.
fn cleanup(case: &str) {
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("st2k_theme_shot_{}_{case}", std::process::id())),
    );
}

/// One headless capture of `doc` with `extra` flags appended, decoded to (width, height, RGB rows).
fn shot(dir: &Path, doc: &Path, tag: &str, extra: &[&str]) -> Png {
    let out = dir.join(format!("{tag}.png"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"));
    // The baseline theme is pinned via the diagnostic env override, NOT read from the
    // machine. The first pushed version assumed the runner's Windows was in dark mode —
    // true on the dev box, false on every GitHub runner, so CI went red on both jobs while
    // the suite was green locally (there is no `--shot ... --dark` FLAG; an unrecognized
    // arg is silently ignored and the shot follows `dark::is_dark()`).
    cmd.env("ST2K_THEME", "dark");
    cmd.arg("--shot")
        .arg(&out)
        .args(["--window", "preview", "--file"])
        .arg(doc)
        .args(extra);
    let status = cmd.status().expect("spawn SageThumbs2K --shot");
    assert!(
        status.success(),
        "{tag} shot of {doc:?} failed: exit {:?} (0xC000041D = abort(), e.g. a RefCell \
         BorrowMutError under panic=abort)",
        status.code(),
    );
    let bytes = std::fs::read(&out).unwrap_or_else(|e| panic!("{tag} shot wrote no PNG: {e}"));
    assert!(!bytes.is_empty(), "{tag} shot wrote an empty PNG");
    Png::decode(&bytes, tag)
}

/// The decoded capture, just enough of one to sample a pixel.
struct Png {
    w: u32,
    h: u32,
    rgb: Vec<u8>,
}

impl Png {
    fn decode(bytes: &[u8], tag: &str) -> Self {
        let img = image::load_from_memory(bytes)
            .unwrap_or_else(|e| panic!("{tag} shot is not a readable PNG: {e}"))
            .to_rgb8();
        let (w, h) = (img.width(), img.height());
        Self {
            w,
            h,
            rgb: img.into_raw(),
        }
    }

    fn at(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let i = ((y.min(self.h - 1) * self.w + x.min(self.w - 1)) * 3) as usize;
        (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2])
    }

    /// Roughly how bright a pixel is. The two skins are 32/32/32 against 243/243/243, so a
    /// coarse threshold is all this needs and it will not go brittle if a shade is retuned.
    fn luma(&self, x: u32, y: u32) -> u32 {
        let (r, g, b) = self.at(x, y);
        (r as u32 + g as u32 + b as u32) / 3
    }
}

/// The caption strip is 36 design px tall; sample well inside it, left of the title text.
const CAPTION_Y: u32 = 20;
const CAPTION_X: u32 = 6;

/// Pressing the button must actually repaint the window in the other skin — caption AND content.
///
/// Both are checked because they are painted by different code and have come apart before: the
/// caption is drawn by `paint_into`'s own fill, while the content surface comes from the theme
/// colour that was composited in at load time. A pass that only moved one of them is a
/// half-themed window, which is worse than not offering the button.
#[test]
fn pressing_the_theme_button_repaints_the_window_in_the_other_skin() {
    let case = "flip";
    let (dir, doc) = sample(
        case,
        "note.md",
        "# Theme toggle\n\nBody text, a `code span`, and a list:\n\n- one\n- two\n",
    );
    let before = shot(&dir, &doc, "dark", &[]);
    let after = shot(&dir, &doc, "light", &["--toggle-theme"]);

    assert_eq!(
        (before.w, before.h),
        (after.w, after.h),
        "the toggle must not resize the window"
    );

    let (cap_before, cap_after) = (
        before.luma(CAPTION_X, CAPTION_Y),
        after.luma(CAPTION_X, CAPTION_Y),
    );
    let (mid_x, mid_y) = (before.w / 2, before.h / 2);
    let (body_before, body_after) = (before.luma(mid_x, mid_y), after.luma(mid_x, mid_y));

    assert!(
        cap_before < 96 && body_before < 96,
        "the baseline capture should be the DARK skin, got caption {cap_before} body \
         {body_before} — if this fails the shot harness stopped honouring --dark, and the \
         rest of this test proves nothing"
    );
    assert!(
        cap_after > 160,
        "the caption stayed dark after pressing the theme button (luma {cap_before} -> \
         {cap_after}): the palette override did not reach the caption paint"
    );
    assert!(
        body_after > 160,
        "the content area stayed dark after pressing the theme button (luma {body_before} -> \
         {body_after}): the palette flipped but whatever the old theme was baked into was \
         never rebuilt — see `toggle_theme`'s reload"
    );
    cleanup(case);
}

/// The flip has to survive the reload it triggers, on a file that goes through the DECODE path
/// rather than the text path.
///
/// This is the case the override exists for and the one most likely to regress: an image's
/// letterbox background is composited INTO the bitmap by the decode worker, so the reload has to
/// come back with the new colour already in it. If the override were process-global-but-read-once
/// (as `is_dark` used to be), or if the reload raced the override, this is where it would show.
#[test]
fn the_flip_survives_the_reload_it_triggers_on_a_decoded_image() {
    let case = "image";
    let dir = std::env::temp_dir().join(format!("st2k_theme_shot_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    // A tall, narrow picture so the fit view is letterboxed left and right — those bars ARE the
    // theme colour, baked in at decode time, which is exactly what has to be rebuilt.
    let doc = dir.join("tall.png");
    let img = image::RgbImage::from_fn(40, 400, |_, y| image::Rgb([(y % 256) as u8, 90, 200]));
    img.save(&doc).expect("write sample png");

    let before = shot(&dir, &doc, "dark", &[]);
    let after = shot(&dir, &doc, "light", &["--toggle-theme"]);

    // Sample the letterbox: near the left edge, well below the caption.
    let (x, y) = (4, before.h / 2);
    let (bg_before, bg_after) = (before.luma(x, y), after.luma(x, y));
    assert!(
        bg_before < 96,
        "baseline letterbox should be dark, got {bg_before}"
    );
    assert!(
        bg_after > 160,
        "the letterbox around the picture stayed dark after the theme flip (luma {bg_before} \
         -> {bg_after}): the decoded bitmap still carries the previous theme's background"
    );
    cleanup(case);
}
