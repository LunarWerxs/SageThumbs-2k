//! Word wrapping: runs into tokens, tokens into lines, overwide words split at cluster boundaries.

use super::*;

/// A measured, placeable token from the flattened run stream. `doc` is the token's slice of the
/// selection document (`None` on dry/unselectable passes).
pub(in super::super) enum Tok {
    Word {
        s: Vec<u16>,
        w: i32,
        pad: i32,
        font: HFONT,
        color: u32,
        code: bool,
        strike: bool,
        link: Option<String>,
        doc: Option<(usize, usize)>,
        spec: FontSpec,
    },
    Space(i32),
    Break,
}

/// Selection wiring for one [`run_block`] call: the active range, the document (to measure a
/// partially-selected word), this block's per-run document offsets, and the hit collector.
pub(in super::super) struct RunSel<'a> {
    pub(in super::super) range: Option<(usize, usize)>,
    pub(in super::super) doc: &'a str,
    pub(in super::super) bases: &'a [usize],
    pub(in super::super) hits: &'a mut Vec<SelHit>,
    pub(in super::super) bg: u32,
}

/// How many UTF-16 units from the front of `w16` fit within `max_w` px — the char-level split
/// point for a token too wide to wrap any other way.
///
/// `at[i]` is the source byte offset of the character unit `i` belongs to, so two units sharing
/// an offset are the halves of one surrogate pair and the boundary walks back off them. Always
/// returns at least one whole character: a single glyph wider than the column still has to be
/// placed somewhere (the cell clip contains it), and returning 0 would spin the caller's loop.
pub(super) unsafe fn units_fitting(hdc: HDC, w16: &[u16], max_w: i32, at: &[usize]) -> usize {
    // Measure at most this many units per probe. `GetTextExtentExPointW` costs time proportional
    // to the string HANDED to it, not to the answer, and the caller re-probes the whole shrinking
    // remainder each round — so passing a multi-megabyte token straight through makes splitting it
    // quadratic and the window appears to hang. No real column fits anywhere near this many
    // characters, so the cap only ever costs an extra chunk boundary, never correctness.
    const PROBE_CAP: usize = 2048;
    let probe = &w16[..w16.len().min(PROBE_CAP)];
    let mut fit = 0i32;
    let mut sz = SIZE::default();
    let ok = GetTextExtentExPointW(
        hdc,
        PCWSTR(probe.as_ptr()),
        probe.len() as i32,
        max_w,
        Some(&mut fit as *mut i32),
        None,
        &mut sz,
    )
    .as_bool();
    if !ok {
        return probe.len();
    }
    let mut n = (fit as usize).min(probe.len());
    // Back off any boundary that would cut a character apart. Two units sharing a source byte
    // offset are the halves of one surrogate pair; a combining mark or a ZWJ joiner belongs to
    // the character before it, so splitting there detaches an accent or shears an emoji cluster.
    while n > 0 && n < probe.len() && splits_a_cluster(probe, at, n) {
        n -= 1;
    }
    if n == 0 {
        // A single character wider than the whole column still has to go somewhere; the cell
        // clip contains it, and returning 0 would spin the caller's loop forever.
        n = 1;
        while n < probe.len() && at[n] == at[n - 1] {
            n += 1;
        }
    }
    n
}

/// Would breaking `w16` before unit `n` split one user-perceived character?
///
/// `at[i]` is the source byte offset of the character unit `i` belongs to, so equal offsets mean
/// one codepoint's surrogate halves. Combining marks and ZWJ sequences are SEPARATE codepoints
/// with their own offsets, so they need the explicit checks.
pub(super) fn splits_a_cluster(w16: &[u16], at: &[usize], n: usize) -> bool {
    const ZWJ: u16 = 0x200D;
    if at[n] == at[n - 1] {
        return true; // mid surrogate pair
    }
    if w16[n - 1] == ZWJ {
        return true; // joiner may not end a cluster
    }
    // A lone BMP unit is its own char; a leading surrogate is never a mark, so this is enough.
    char::from_u32(w16[n] as u32).is_some_and(|c| c == '\u{200D}' || is_combining(c))
}

/// Per-run token metadata that stays fixed across one [`Run`]'s whole word-walk in
/// [`tokenize_runs`]: bundled so [`flush_word`]/[`split_overwide_word`] take one struct instead
/// of eight positional parameters that never change within the run.
pub(super) struct RunTokCtx<'a> {
    pub(super) font: HFONT,
    pub(super) color: u32,
    pub(super) pad: i32,
    pub(super) code: bool,
    pub(super) strike: bool,
    pub(super) link: &'a Option<String>,
    pub(super) spec: FontSpec,
    pub(super) base: Option<usize>,
}

/// Build the [`Tok::Word`] both [`flush_word`] and [`split_overwide_word`] push: the run's fixed
/// style comes from `rc`, `cx` is the measured text width (the padding is added here) and `doc`
/// the selection-document span this token maps back to.
pub(super) fn word_tok(s: Vec<u16>, cx: i32, doc: Option<(usize, usize)>, rc: &RunTokCtx) -> Tok {
    Tok::Word {
        s,
        w: cx + 2 * rc.pad,
        pad: rc.pad,
        font: rc.font,
        color: rc.color,
        code: rc.code,
        strike: rc.strike,
        link: rc.link.clone(),
        doc,
        spec: rc.spec,
    }
}

/// Measure the pending `word` and push it onto `toks` as one [`Tok::Word`] (or several, via
/// [`split_overwide_word`], when it is wider than `width`). `wend` is the source byte offset the
/// word ends at, `wstart` where it began (both only matter for the selection-document span this
/// token maps back to. No-op (matching the macro this replaced) when `word` is empty. Clears
/// `word`/`unit_at` before returning.
#[allow(clippy::too_many_arguments)] // one call site, all genuinely distinct inputs
pub(super) unsafe fn flush_word(
    hdc: HDC,
    word: &mut Vec<u16>,
    unit_at: &mut Vec<usize>,
    wstart: usize,
    wend: usize,
    width: i32,
    toks: &mut Vec<Tok>,
    rc: &RunTokCtx,
) {
    if word.is_empty() {
        return;
    }
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, word.as_slice(), &mut sz);
    if width > 0 && sz.cx + 2 * rc.pad > width && word.len() > 1 {
        // A token wider than the ENTIRE line offers the greedy breaker below no break
        // opportunity, so it used to be placed anyway - running over the next table column and
        // off the pane edge, which is exactly what a CSV full of 90-character API keys looked
        // like. Split it between characters instead, the way CSS `overflow-wrap: anywhere` does.
        split_overwide_word(hdc, word, unit_at.as_slice(), wend, width, sz, toks, rc);
    } else {
        toks.push(word_tok(
            core::mem::take(word),
            sz.cx,
            rc.base.map(|b| (b + wstart, b + wend)),
            rc,
        ));
    }
    unit_at.clear();
}

/// [`flush_word`]'s over-wide branch: split `word` into character chunks no wider than `width`,
/// each becoming its own [`Tok::Word`] mapped back to its own slice of the selection document.
/// `sz` is the whole word's already-measured width, used to pro-rate a chunk whose own measure
/// fails. Clears `word` before returning.
#[allow(clippy::too_many_arguments)] // one call site, all genuinely distinct inputs
pub(super) unsafe fn split_overwide_word(
    hdc: HDC,
    word: &mut Vec<u16>,
    unit_at: &[usize],
    wend: usize,
    width: i32,
    sz: SIZE,
    toks: &mut Vec<Tok>,
    rc: &RunTokCtx,
) {
    let mut a = 0usize;
    while a < word.len() {
        let n = units_fitting(hdc, &word[a..], (width - 2 * rc.pad).max(1), &unit_at[a..]);
        let b = (a + n).min(word.len());
        let mut csz = SIZE::default();
        if !GetTextExtentPoint32W(hdc, &word[a..b], &mut csz).as_bool() {
            // A failed measure would leave the chunk 0 px wide, which both stacks the chunks
            // on top of each other and gives selection a zero-width hit rect. The whole-token
            // width is already known, so pro-rate it rather than trusting the zero.
            csz.cx = sz.cx * (b - a) as i32 / word.len().max(1) as i32;
        }
        let to = if b < word.len() { unit_at[b] } else { wend };
        let from = unit_at[a];
        toks.push(word_tok(
            word[a..b].to_vec(),
            csz.cx,
            rc.base.map(|bb| (bb + from, bb + to)),
            rc,
        ));
        a = b;
    }
    word.clear();
}

/// Walk one run's characters, flushing words at newlines/whitespace/script boundaries via
/// [`flush_word`]. Mutates `word`/`unit_at`/`wstart` (the pending-word scratch state) and
/// pushes finished tokens onto `toks`. Split out of [`tokenize_runs`] so the per-run setup
/// loop there doesn't also carry this walk's own branching.
#[allow(clippy::too_many_arguments)] // one call site, all genuinely distinct inputs
pub(super) unsafe fn walk_run_chars(
    hdc: HDC,
    text: &str,
    width: i32,
    word: &mut Vec<u16>,
    unit_at: &mut Vec<usize>,
    wstart: &mut usize,
    toks: &mut Vec<Tok>,
    rc: &RunTokCtx,
) {
    let mut chars = text.char_indices().peekable();
    while let Some((ci, ch)) = chars.next() {
        match ch {
            '\n' => {
                flush_word(hdc, word, unit_at, *wstart, ci, width, toks, rc);
                toks.push(Tok::Break);
            }
            ' ' | '\t' => {
                flush_word(hdc, word, unit_at, *wstart, ci, width, toks, rc);
                let mut sz = SIZE::default();
                let sp = [b' ' as u16];
                let _ = GetTextExtentPoint32W(hdc, &sp, &mut sz);
                toks.push(Tok::Space(sz.cx));
            }
            _ => {
                if word.is_empty() {
                    *wstart = ci;
                }
                let mut b = [0u16; 2];
                for u in ch.encode_utf16(&mut b) {
                    word.push(*u);
                    unit_at.push(ci);
                }
                // Scripts that don't put spaces between words get their break
                // opportunities here instead. Without this a Chinese/Japanese paragraph is
                // ONE token, and the greedy line-breaker below places an over-wide token
                // anyway, so the whole paragraph ran off the pane edge and was clipped.
                if let Some(&(ni, next)) = chars.peek() {
                    if can_break_between(ch, next) {
                        flush_word(hdc, word, unit_at, *wstart, ni, width, toks, rc);
                    }
                }
            }
        }
    }
    flush_word(hdc, word, unit_at, *wstart, text.len(), width, toks, rc);
}

/// Flattens `runs` into measured tokens (words / spaces / hard breaks), each remembering the
/// run bytes it came from so it maps back to the selection document. Splits any word wider
/// than `width` into character chunks (the way CSS `overflow-wrap: anywhere` does), so a
/// single token can never run off the pane edge unbroken. `sel` only needs read access here
/// (per-run document-offset bases); the draw pass reborrows it mutably for hit-testing.
pub(super) unsafe fn tokenize_runs(
    hdc: HDC,
    runs: &[Run],
    fonts: &Fonts,
    width: i32,
    ctx: &RunCtx,
    sel: Option<&RunSel>,
) -> Vec<Tok> {
    let mut toks: Vec<Tok> = Vec::new();
    for (ri, r) in runs.iter().enumerate() {
        let f = fonts.pick(r);
        let spec = fonts.spec(r);
        let color = if r.link.is_some() {
            ctx.accent
        } else {
            ctx.base_color
        };
        let pad = if r.code { ctx.code_pad } else { 0 };
        SelectObject(hdc, f.into());
        let base = sel.and_then(|s| s.bases.get(ri).copied());
        let rc = RunTokCtx {
            font: f,
            color,
            pad,
            code: r.code,
            strike: r.strike,
            link: &r.link,
            spec,
            base,
        };
        let mut word: Vec<u16> = Vec::new();
        // Per UTF-16 unit of `word`, the byte offset of the CHARACTER that unit belongs to. Only
        // read when an over-wide token has to be split below, where it both maps each chunk back
        // to its slice of the selection document and keeps a surrogate pair from being cut in half.
        let mut unit_at: Vec<usize> = Vec::new();
        let mut wstart = 0usize; // byte offset in `r.text` where the pending word began
        walk_run_chars(
            hdc,
            &r.text,
            width,
            &mut word,
            &mut unit_at,
            &mut wstart,
            &mut toks,
            &rc,
        );
    }
    toks
}

/// Greedy line-break of `toks` into `width`-wide lines, remembering each placed word's
/// line-relative x. Returns `(placements, line width)` per line.
pub(super) fn break_into_lines(toks: &[Tok], width: i32) -> Vec<(Vec<(i32, usize)>, i32)> {
    let mut lines: Vec<(Vec<(i32, usize)>, i32)> = Vec::new(); // (placements, line width)
    let mut cur: Vec<(i32, usize)> = Vec::new();
    let mut cx = 0;
    let mut pending_space = 0;
    let mut line_start = true;
    for (idx, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Break => {
                lines.push((core::mem::take(&mut cur), cx));
                cx = 0;
                pending_space = 0;
                line_start = true;
            }
            Tok::Space(sw) => {
                if !line_start {
                    pending_space += *sw;
                }
            }
            Tok::Word { w, .. } => {
                if !line_start && cx + pending_space + *w > width {
                    lines.push((core::mem::take(&mut cur), cx));
                    cx = 0;
                    pending_space = 0;
                    line_start = true;
                }
                if !line_start {
                    cx += pending_space;
                }
                pending_space = 0;
                cur.push((cx, idx));
                cx += *w;
                line_start = false;
            }
        }
    }
    if !cur.is_empty() || !line_start {
        lines.push((cur, cx));
    }
    lines
}
