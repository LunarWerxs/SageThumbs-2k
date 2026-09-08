//! DIFF_FUZZ — corpus-mutation differential fuzzer against DjVuLibre.
//!
//! The existing `fuzz/` targets (libFuzzer, coverage-guided) catch panics,
//! timeouts and OOM but cannot catch *semantic* divergence: a mutant that both
//! we and DjVuLibre accept, but render to different pixels — a silent wrong
//! answer rather than a crash. `interop_pixdiff` (round-5 #14) and
//! `interop_encode` (#507) compare against DjVuLibre, but only on the fixed,
//! well-formed corpus. This harness runs the same comparison over *mutated*
//! inputs: take a corpus file, apply structured, chunk-aware mutations
//! (truncate at a chunk boundary, bit-flip within a chunk's payload, resize a
//! chunk's declared length), and for each mutant classify:
//!
//!   - both us and `djvudump` reject             → uninteresting, tally only
//!   - we accept, `djvudump` rejects              → divergence: our-laxer
//!   - we reject, `djvudump` accepts               → divergence: our-stricter
//!   - our parse/render panics                     → crash finding (always saved)
//!   - both accept → render ours vs `ddjvu`, diff pixels (reusing
//!     `examples/support` — the same helper `interop_pixdiff` uses) → a large
//!     diff is a pixel-mismatch finding
//!
//! Findings (crashes, pixel mismatches, parse divergences) are saved under
//! `--out-dir` (default `fuzz/corpus-regressions/diff_fuzz`) as the mutant
//! bytes plus a `.txt` sidecar describing the mutation and the divergence.
//!
//! The run is bounded and seed-deterministic (`--seed`), a native-only example
//! (no wasm/Date constraints apply), so a given seed always replays the same
//! mutant sequence. Per-mutant work is bounded by the existing decode caps
//! (`MAX_PAGE_SYMBOL_PIXELS` etc., src/jb2.rs) plus a wall-clock timeout on
//! both the our-side attempt and the reference-tool subprocess, so a mutant
//! that would otherwise spin cannot stall the whole run.
//!
//! ## Reference tool
//!
//! Requires `ddjvu`/`djvudump` (DjVuLibre: `brew install djvulibre`) on PATH,
//! or pass `--ddjvu`/--djvudump` (or set `DIFF_FUZZ_DDJVU`/`DIFF_FUZZ_DJVUDUMP`)
//! to point at them explicitly. If neither is found, the harness still runs
//! the mutation + our-side parse/render pipeline standalone ("solo mode"):
//! useful on its own (structured-mutation parse-robustness fuzzing), and it
//! prints the exact command to re-run in full differential mode once the
//! tools are installed.
//!
//! Usage:
//!   cargo run --release --example diff_fuzz -- [options] [file.djvu ...]
//!
//! Options:
//!   --seed <u64>            deterministic seed (default 0xD1FF_F022_1234_5678)
//!   --mutants <n>            mutants per input file (default 500)
//!   --timeout-ms <n>         our-side per-mutant timeout, ms (default 2000)
//!   --ref-timeout-ms <n>     djvudump/ddjvu per-mutant timeout, ms (default 5000)
//!   --max-seconds <n>        overall wall-clock budget (default 1200 = 20 min)
//!   --out-dir <path>         where findings are saved
//!   --ddjvu <path>           path to ddjvu (else $DIFF_FUZZ_DDJVU or PATH)
//!   --djvudump <path>        path to djvudump (else $DIFF_FUZZ_DJVUDUMP or PATH)
//!   --djvused <path>         path to djvused (else $DIFF_FUZZ_DJVUSED or PATH);
//!                            enables the #597 metadata-plane differential
//!                            (txt/ant/outline extraction vs ours)
//!
//! With no positional files, mutates three representative corpus files
//! spanning a bundled multi-page bilevel doc, a small bundled doc, and a
//! single-page doc (see `default_files`).

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[path = "support/mod.rs"]
mod support;

use djvu_rs::djvu_document::{DjVuBookmark, DjVuDocument};
use djvu_rs::djvu_render::render_pixmap;
use support::{DiffStats, diff_stats, native_opts, parse_ppm};

// ── Deterministic PRNG (splitmix64) — no external `rand` dependency needed,
//    keeps the harness self-contained like `interop_pixdiff`/`interop_encode`. ──

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the degenerate all-zero state.
        Rng(seed ^ 0x9E3779B97F4A7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform in `0..n`. `n == 0` returns 0 (caller must not call with an
    /// empty range).
    fn gen_range(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }
}

// ── Chunk walker — enough IFF structure to find mutation targets ───────────
//
// Deliberately independent of `djvu-iff`'s parsers: this only needs *byte
// offsets* of each chunk's length field and payload (for well-formed input —
// the corpus files are valid), not decoding. `FORM` chunks recurse (DJVM →
// nested FORM:DJVU) so mutations can land inside inner chunks (Sjbz, BG44,
// ...), not just the outer container.

#[derive(Clone, Copy)]
struct ChunkSpan {
    id: [u8; 4],
    len_field_offset: usize,
    data_offset: usize,
    data_len: usize,
    /// Offset just past this chunk's (padded) data — where the next sibling
    /// chunk would start.
    end_offset: usize,
    /// 0-based page index this chunk lives under (the nearest enclosing
    /// `FORM:DJVU`), or `None` for document-global chunks (e.g. a `DJVM`
    /// root's own span, or its direct `DIRM`) that aren't inside any single
    /// page. Mutating a page-scoped chunk only shows up when *that* page is
    /// rendered — for a bundled multi-page corpus file, rendering page 0
    /// unconditionally would silently miss most mutations.
    page_index: Option<usize>,
}

fn collect_chunks(data: &[u8]) -> Vec<ChunkSpan> {
    let mut out = Vec::new();
    if data.len() < 12 || &data[0..4] != b"AT&T" {
        return out;
    }
    let mut page_counter = 0usize;
    let top_end = data.len();
    walk_chunks(data, 4, top_end, 0, None, &mut page_counter, &mut out);
    out
}

/// `page_index` is the page scope inherited from the enclosing container.
/// Encountering a `FORM:DJVU` chunk (including possibly the root itself, for
/// a non-bundled single-page file) opens a new page scope and claims the
/// next number from `page_counter`.
///
/// `end_bound` is the end offset (exclusive) of the *enclosing* container
/// chunk currently being walked — `data.len()` for the initial top-level
/// call, or the specific `FORM`'s own padded data-end when recursing into
/// its children. Without this bound the loop below would keep interpreting
/// bytes past a nested FORM's declared length as further sibling chunk
/// headers, walking straight into compressed payload data (Sjbz/BG44/TXTz)
/// of subsequent sibling pages and occasionally hitting byte patterns that
/// coincidentally look like valid `FORM`/`DJVU` framing — this was observed
/// inflating `page_counter` into the thousands on watchmaker.djvu (a 12-page
/// file) and reporting ~40k bogus chunk spans instead of ~100-120 real ones.
fn walk_chunks(
    data: &[u8],
    mut offset: usize,
    end_bound: usize,
    depth: u32,
    page_index: Option<usize>,
    page_counter: &mut usize,
    out: &mut Vec<ChunkSpan>,
) {
    const MAX_DEPTH: u32 = 16;
    if depth > MAX_DEPTH {
        return;
    }
    let end_bound = end_bound.min(data.len());
    while offset + 8 <= end_bound {
        let id = [
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ];
        let len_field_offset = offset + 4;
        let length = u32::from_be_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;
        let data_offset = offset + 8;

        if data_offset
            .checked_add(length)
            .is_none_or(|end| end > end_bound)
        {
            // Declared length overruns this container's own bound (or the
            // buffer) — record a clipped span as a mutation target and stop
            // walking this level (nothing valid follows within it).
            let clipped_len = end_bound.saturating_sub(data_offset);
            out.push(ChunkSpan {
                id,
                len_field_offset,
                data_offset,
                data_len: clipped_len,
                end_offset: end_bound,
                page_index,
            });
            return;
        }

        let data_end = data_offset + length;
        let padded_end = (data_end + (length % 2)).min(end_bound);
        let is_form = &id == b"FORM";
        let this_scope =
            if is_form && length >= 4 && data.get(data_offset..data_offset + 4) == Some(b"DJVU") {
                let p = *page_counter;
                *page_counter += 1;
                Some(p)
            } else {
                page_index
            };
        out.push(ChunkSpan {
            id,
            len_field_offset,
            data_offset,
            data_len: length,
            end_offset: padded_end,
            page_index: this_scope,
        });

        if is_form && length >= 4 {
            // Nested chunk list starts after the 4-byte secondary form type,
            // and must not be walked past this FORM's own (unpadded) data
            // end — that boundary, not the whole buffer, is what stops the
            // recursive scan from running into a sibling chunk's payload.
            walk_chunks(
                data,
                data_offset + 4,
                data_end,
                depth + 1,
                this_scope,
                page_counter,
                out,
            );
        }

        offset = padded_end;
    }
}

// ── Mutation operators ──────────────────────────────────────────────────────

struct Mutation {
    bytes: Vec<u8>,
    desc: String,
    chunk: String,
    page_index: Option<usize>,
}

/// Returns the mutated bytes plus enough structured metadata to bucket later
/// render divergences by the chunk kind that was touched.
fn apply_mutation(base: &[u8], chunks: &[ChunkSpan], rng: &mut Rng) -> Mutation {
    if chunks.is_empty() {
        // No parseable IFF structure (shouldn't happen for real corpus files)
        // — fall back to a generic whole-file bit-flip so the harness still
        // does *something* useful.
        let mut v = base.to_vec();
        if !v.is_empty() {
            let i = rng.gen_range(v.len());
            v[i] ^= 1 << rng.gen_range(8);
        }
        return Mutation {
            bytes: v,
            desc: "generic-bitflip".to_string(),
            chunk: "<whole-file>".to_string(),
            page_index: None,
        };
    }

    let c = chunks[rng.gen_range(chunks.len())];
    let id_str = String::from_utf8_lossy(&c.id).to_string();

    let (mutant, desc) = match rng.gen_range(3) {
        0 => {
            // Truncate at a chunk boundary: either drop this chunk's payload
            // (and everything after it) or keep this chunk intact and drop
            // everything after it.
            let at_start = rng.gen_range(2) == 0;
            let cut = if at_start {
                c.data_offset
            } else {
                c.end_offset
            }
            .min(base.len());
            (
                base[..cut].to_vec(),
                format!(
                    "truncate-{}-of-{id_str}@{cut}",
                    if at_start { "start" } else { "end" }
                ),
            )
        }
        1 => {
            let mut v = base.to_vec();
            let span_len = c.data_len.min(v.len().saturating_sub(c.data_offset));
            let mut n = 0;
            if span_len > 0 {
                let n_flips = 1 + rng.gen_range(3);
                for _ in 0..n_flips {
                    let idx = c.data_offset + rng.gen_range(span_len);
                    let bit = rng.gen_range(8);
                    v[idx] ^= 1 << bit;
                    n += 1;
                }
            }
            (v, format!("bitflip-{n}bits-in-{id_str}@{}", c.data_offset))
        }
        _ => {
            let mut v = base.to_vec();
            let mut new_len = 0u32;
            if c.len_field_offset + 4 <= v.len() {
                let cur = u32::from_be_bytes([
                    v[c.len_field_offset],
                    v[c.len_field_offset + 1],
                    v[c.len_field_offset + 2],
                    v[c.len_field_offset + 3],
                ]);
                new_len = match rng.gen_range(3) {
                    0 => cur.wrapping_add(1 + rng.gen_range(64) as u32),
                    1 => cur.wrapping_sub(1 + rng.gen_range(64) as u32),
                    _ => rng.next_u32(),
                };
                v[c.len_field_offset..c.len_field_offset + 4]
                    .copy_from_slice(&new_len.to_be_bytes());
            }
            (v, format!("resize-length-of-{id_str}->{new_len}"))
        }
    };
    Mutation {
        bytes: mutant,
        desc,
        chunk: id_str,
        page_index: c.page_index,
    }
}

// ── Our-side attempt (thread + channel timeout; panics caught) ─────────────

enum OurOutcome {
    /// `DjVuDocument::parse` or `doc.page(0)` failed — structural reject,
    /// analogous to `djvudump` rejecting the file.
    StructReject(String),
    /// Parse/page succeeded but render failed — a narrower, separately
    /// tallied case (not a structural reject, not a render).
    RenderFailed(String),
    /// Full success: page parsed and rendered.
    Rendered {
        w: u32,
        h: u32,
        rgba: Vec<u8>,
    },
    Panicked(String),
    Timeout,
}

fn panic_payload_to_string(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

fn our_attempt(mutant: Vec<u8>, page_idx: usize, timeout: Duration) -> OurOutcome {
    let (tx, rx) = mpsc::channel();
    let builder = std::thread::Builder::new()
        .name("diff-fuzz-mutant".into())
        .stack_size(8 * 1024 * 1024);
    let spawned = builder.spawn(move || {
        let result = panic::catch_unwind(AssertUnwindSafe(
            || -> Result<(u32, u32, Vec<u8>), String> {
                let doc = DjVuDocument::parse(&mutant).map_err(|e| format!("parse: {e}"))?;
                let page = doc.page(page_idx).map_err(|e| format!("page: {e}"))?;
                let (pw, ph) = (page.width() as u32, page.height() as u32);
                if pw == 0 || ph == 0 {
                    return Err("render: zero dimensions".to_string());
                }
                let opts = native_opts(pw, ph);
                let pm = render_pixmap(page, &opts).map_err(|e| format!("render: {e}"))?;
                // Report the actual rendered pixmap's dimensions, not the
                // pre-rotation INFO dims from page.width()/height(): for
                // 90deg/270deg-rotated pages the two differ (dimensions swap
                // on render), and comparing the pre-rotation dims against
                // ddjvu's (correctly rotated) reported dims produces a false
                // "dim-mismatch" classification (see PERF_EXPERIMENTS.md
                // round 46).
                Ok((pm.width, pm.height, pm.data))
            },
        ));
        let _ = tx.send(result);
    });
    if spawned.is_err() {
        return OurOutcome::StructReject("failed to spawn worker thread".into());
    }

    match rx.recv_timeout(timeout) {
        Ok(Ok(Ok((w, h, rgba)))) => OurOutcome::Rendered { w, h, rgba },
        Ok(Ok(Err(e))) => {
            if e.starts_with("render:") {
                OurOutcome::RenderFailed(e)
            } else {
                OurOutcome::StructReject(e)
            }
        }
        Ok(Err(payload)) => OurOutcome::Panicked(panic_payload_to_string(payload)),
        Err(_) => OurOutcome::Timeout,
        // Note: the worker thread is intentionally left detached on timeout —
        // this is a bounded example run, not long-lived server code, and the
        // existing decode caps (MAX_PAGE_SYMBOL_PIXELS etc.) make a genuine
        // hang very unlikely; the timeout is a safety net, not the primary
        // defense.
    }
}

// ── Reference-tool subprocess helpers (djvudump / ddjvu) ───────────────────

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// `Some(true/false)` = tool ran and accepted/rejected; `None` = tool missing
/// or failed to spawn (caller falls back to solo mode).
fn djvudump_accepts(djvudump_bin: &str, path: &Path, timeout: Duration) -> Option<bool> {
    let child = Command::new(djvudump_bin)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    wait_with_timeout(child, timeout).map(|s| s.success())
}

/// `page_no` is 1-based (ddjvu's `-page=` convention), matching `page_idx + 1`
/// from the 0-based index used everywhere else in this harness.
fn ddjvu_render(
    ddjvu_bin: &str,
    path: &Path,
    page_no: usize,
    out_ppm: &Path,
    timeout: Duration,
) -> Option<bool> {
    let child = Command::new(ddjvu_bin)
        .args([
            "-format=ppm",
            &format!("-page={page_no}"),
            &path.to_string_lossy(),
            &out_ppm.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let ok = wait_with_timeout(child, timeout).map(|s| s.success());
    Some(ok.unwrap_or(false) && out_ppm.exists())
}

/// Re-run a reference-tool command capturing stderr, only for the handful of
/// mutants that actually get saved as findings (keeps the hot per-mutant path
/// clean — every other call redirects stderr to `/dev/null`).
fn command_stderr(bin: &str, args: &[&str], timeout: Duration) -> String {
    let Ok(mut child) = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return String::new();
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    let Ok(out) = child.wait_with_output() else {
        return String::new();
    };
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn djvudump_stderr(djvudump_bin: &str, path: &Path, timeout: Duration) -> String {
    command_stderr(djvudump_bin, &[&path.to_string_lossy()], timeout)
}

fn ddjvu_stderr(
    ddjvu_bin: &str,
    path: &Path,
    page_no: usize,
    out_ppm: &Path,
    timeout: Duration,
) -> String {
    command_stderr(
        ddjvu_bin,
        &[
            "-format=ppm",
            &format!("-page={page_no}"),
            &path.to_string_lossy(),
            &out_ppm.to_string_lossy(),
        ],
        timeout,
    )
}

// ── Metadata planes (#597): djvused differential ────────────────────────────
//
// The render ladder above is blind to the metadata planes: a mutant whose
// TXTz/ANTz/NAVM we read differently from DjVuLibre still renders identical
// pixels. For mutants where at least one side structurally accepts, both
// sides additionally run text / annotation / outline extraction (our
// `page.text()` / `page.annotations()` / `doc.bookmarks()` vs `djvused
// print-pure-txt / print-ant / print-outline`) and each plane is classified
// accept / reject / content-divergence. Comparison signatures are
// deliberately coarse (whitespace-insensitive text, maparea count, bookmark
// title+url multiset with djvused's octal escapes decoded) — enough to flag a
// divergence for adjudication without re-implementing djvused's printer.
// A per-(page, plane) baseline on the *unmutated* file gates findings, so a
// pre-existing normalization gap is reported once, not per mutant.

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaneSide {
    /// Extraction errored (parse failure, subprocess failure, or timeout).
    Reject(String),
    /// Extraction succeeded and the plane is absent/empty.
    Empty,
    /// Extraction succeeded; normalized comparison signature.
    Content(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum MetaClass {
    BothReject,
    BothEmpty,
    Match,
    /// Both extract content, signatures differ — the headline divergence.
    Diverge,
    /// We extract content where djvused rejects or sees nothing.
    OursOnly,
    /// djvused extracts content where we reject or see nothing.
    TheirsOnly,
    /// One side rejects while the other sees an empty plane — a lenient/strict
    /// acceptance gap with no content at stake (tallied, not saved).
    AcceptMismatch,
}

impl MetaClass {
    fn label(self) -> &'static str {
        match self {
            MetaClass::BothReject => "both-reject",
            MetaClass::BothEmpty => "both-empty",
            MetaClass::Match => "match",
            MetaClass::Diverge => "content-diverge",
            MetaClass::OursOnly => "ours-only",
            MetaClass::TheirsOnly => "theirs-only",
            MetaClass::AcceptMismatch => "accept-mismatch",
        }
    }

    fn is_finding(self) -> bool {
        matches!(
            self,
            MetaClass::Diverge | MetaClass::OursOnly | MetaClass::TheirsOnly
        )
    }
}

const META_PLANES: [&str; 3] = ["txt", "ant", "outline"];

fn meta_class(ours: &PlaneSide, theirs: &PlaneSide) -> MetaClass {
    use PlaneSide::*;
    match (ours, theirs) {
        (Reject(_), Reject(_)) => MetaClass::BothReject,
        (Empty, Empty) => MetaClass::BothEmpty,
        (Content(a), Content(b)) => {
            if a == b {
                MetaClass::Match
            } else {
                MetaClass::Diverge
            }
        }
        (Content(_), _) => MetaClass::OursOnly,
        (_, Content(_)) => MetaClass::TheirsOnly,
        (Reject(_), Empty) | (Empty, Reject(_)) => MetaClass::AcceptMismatch,
    }
}

/// Whitespace- and control-character-insensitive text signature: our text
/// layer keeps DjVu's zone-separator control bytes and djvused's pure-txt
/// prints its own line-break convention, so only the character content is
/// comparable.
fn text_signature(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect()
}

fn flatten_bookmarks(marks: &[DjVuBookmark], out: &mut Vec<String>) {
    for m in marks {
        out.push(text_signature(&m.title));
        out.push(text_signature(&m.url));
        flatten_bookmarks(&m.children, out);
    }
}

fn bookmarks_signature(marks: &[DjVuBookmark]) -> Option<String> {
    if marks.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    flatten_bookmarks(marks, &mut parts);
    parts.sort();
    Some(parts.join("|"))
}

/// Extract the quoted strings from djvused s-expression output, decoding its
/// escapes (`\"`, `\\`, and octal `\NNN` byte escapes for non-ASCII).
fn sexpr_quoted_strings(raw: &str) -> Vec<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        i += 1;
        let mut cur: Vec<u8> = Vec::new();
        while i < bytes.len() && bytes[i] != b'"' {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                let c = bytes[i + 1];
                if c.is_ascii_digit() {
                    let mut val = 0u32;
                    let mut n = 0;
                    while n < 3 && i + 1 + n < bytes.len() && bytes[i + 1 + n].is_ascii_digit() {
                        val = val * 8 + u32::from(bytes[i + 1 + n] - b'0');
                        n += 1;
                    }
                    cur.push((val & 0xFF) as u8);
                    i += 1 + n;
                } else {
                    let decoded = match c {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        other => other,
                    };
                    cur.push(decoded);
                    i += 2;
                }
            } else {
                cur.push(bytes[i]);
                i += 1;
            }
        }
        i += 1; // closing quote
        out.push(String::from_utf8_lossy(&cur).into_owned());
    }
    out
}

/// Our-side metadata extraction, panic-caught and time-bounded like
/// `our_attempt`. A document parse failure rejects all three planes — that
/// asymmetry vs djvused (which may still print planes of a file whose other
/// chunks are damaged) is exactly a divergence worth surfacing.
fn our_meta(mutant: Vec<u8>, page_idx: usize, timeout: Duration) -> [PlaneSide; 3] {
    let (tx, rx) = mpsc::channel();
    let builder = std::thread::Builder::new()
        .name("diff-fuzz-meta".into())
        .stack_size(8 * 1024 * 1024);
    let spawned = builder.spawn(move || {
        let result = panic::catch_unwind(AssertUnwindSafe(|| -> [PlaneSide; 3] {
            let doc = match DjVuDocument::parse(&mutant) {
                Ok(d) => d,
                Err(e) => {
                    let r = PlaneSide::Reject(format!("parse: {e}"));
                    return [r.clone(), r.clone(), r];
                }
            };
            let outline = match bookmarks_signature(doc.bookmarks()) {
                Some(sig) => PlaneSide::Content(sig),
                None => PlaneSide::Empty,
            };
            let (txt, ant) = match doc.page(page_idx) {
                Ok(page) => {
                    let txt = match page.text() {
                        Ok(Some(t)) if !text_signature(&t).is_empty() => {
                            PlaneSide::Content(text_signature(&t))
                        }
                        Ok(_) => PlaneSide::Empty,
                        Err(e) => PlaneSide::Reject(format!("text: {e}")),
                    };
                    let ant = match page.annotations() {
                        Ok(Some((_ann, areas))) => {
                            PlaneSide::Content(format!("maparea:{}", areas.len()))
                        }
                        Ok(None) => PlaneSide::Empty,
                        Err(e) => PlaneSide::Reject(format!("ant: {e}")),
                    };
                    (txt, ant)
                }
                Err(e) => {
                    let r = PlaneSide::Reject(format!("page: {e}"));
                    (r.clone(), r)
                }
            };
            [txt, ant, outline]
        }));
        let _ = tx.send(result);
    });
    if spawned.is_err() {
        let r = PlaneSide::Reject("failed to spawn worker thread".into());
        return [r.clone(), r.clone(), r];
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(planes)) => planes,
        Ok(Err(payload)) => {
            let r = PlaneSide::Reject(format!("panic: {}", panic_payload_to_string(payload)));
            [r.clone(), r.clone(), r]
        }
        Err(_) => {
            let r = PlaneSide::Reject("timeout".into());
            [r.clone(), r.clone(), r]
        }
    }
}

/// Run one `djvused -e '<script>'` and capture stdout with a kill-on-timeout
/// guard. `None` = spawn failure or timeout (caller records a Reject).
fn command_stdout(bin: &str, args: &[&str], timeout: Duration) -> Option<(bool, String)> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break None,
        }
    };
    let out = child.wait_with_output().ok()?;
    let status = status?;
    Some((
        status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

fn their_meta(
    djvused_bin: &str,
    path: &Path,
    page_idx: usize,
    timeout: Duration,
) -> [PlaneSide; 3] {
    let page_no = page_idx + 1; // djvused select is 1-based
    let path_str = path.to_string_lossy();

    let run = |script: String| -> PlaneSide {
        match command_stdout(djvused_bin, &[path_str.as_ref(), "-e", &script], timeout) {
            None => PlaneSide::Reject("djvused spawn/timeout".into()),
            Some((false, _)) => PlaneSide::Reject("djvused nonzero exit".into()),
            Some((true, out)) => {
                if out.trim().is_empty() {
                    PlaneSide::Empty
                } else {
                    PlaneSide::Content(out)
                }
            }
        }
    };

    let txt = match run(format!("select {page_no}; print-pure-txt")) {
        PlaneSide::Content(out) => {
            let sig = text_signature(&out);
            if sig.is_empty() {
                PlaneSide::Empty
            } else {
                PlaneSide::Content(sig)
            }
        }
        other => other,
    };

    let ant = match run(format!("select {page_no}; print-ant")) {
        PlaneSide::Content(out) => {
            PlaneSide::Content(format!("maparea:{}", out.matches("(maparea").count()))
        }
        other => other,
    };

    let outline = match run("print-outline".to_string()) {
        PlaneSide::Content(out) => {
            let mut parts: Vec<String> = sexpr_quoted_strings(&out)
                .iter()
                .map(|s| text_signature(s))
                .collect();
            if parts.is_empty() {
                PlaneSide::Empty
            } else {
                parts.sort();
                PlaneSide::Content(parts.join("|"))
            }
        }
        other => other,
    };

    [txt, ant, outline]
}

fn plane_side_brief(p: &PlaneSide) -> String {
    match p {
        PlaneSide::Reject(e) => format!("reject({e})"),
        PlaneSide::Empty => "empty".to_string(),
        PlaneSide::Content(sig) => {
            let mut s = sig.clone();
            if s.chars().count() > 160 {
                s = s.chars().take(160).collect::<String>() + "…";
            }
            format!("content[{s}]")
        }
    }
}

// ── Classification & findings ───────────────────────────────────────────────

// Both sides are modeled with the same three-level acceptance ladder:
//   0 = structurally rejected (parse/page, or djvudump, fails)
//   1 = structurally accepted but the actual pixel decode/render fails
//       (djvudump only walks the IFF chunk tree; it does not decode JB2/IW44,
//       so it can accept a file whose data ddjvu's decoder later refuses —
//       confirmed on this corpus: an INFO-height bit-flip that djvudump
//       reports as a well-formed `DjVu 192x384` FORM, but `ddjvu` rejects with
//       "Corrupted data (Incorrect size in BG44 chunk)")
//   2 = fully decoded and rendered
// Comparing the two ladders (not just a binary accept/reject) is what
// surfaces the interesting case: our_level==2 while their_level==1 means we
// *rendered pixels* for input DjVuLibre's own decoder calls corrupt — a
// silent-wrong-output risk, not merely a stricter/laxer parse difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Class {
    BothReject,               // both sides reject at the structural level
    BothRenderFail,           // both sides structurally accept but neither renders
    OurLaxer,                 // we accept (any level), djvudump structurally rejects
    OurStricter,              // we structurally reject, djvudump structurally accepts
    OurCrash,                 // our parse/render panicked
    OurTimeout,               // our attempt exceeded the per-mutant timeout
    OurRenderFail,            // structurally ok both sides, but only ddjvu renders
    OurRendersWhatTheyReject, // we render pixels for data ddjvu's own decoder refuses
    DimMismatch,              // both rendered, dimensions disagree
    PixelMismatch,            // both rendered, dims agree, distribution is far off floor
    BothAcceptMatch,          // both rendered, pixel diff within the established floor
    SoloAccept,               // solo mode (no reference tool): we accepted
    SoloReject,               // solo mode: we rejected (uninteresting)
}

impl Class {
    fn label(self) -> &'static str {
        match self {
            Class::BothReject => "both-reject",
            Class::BothRenderFail => "both-render-fail",
            Class::OurLaxer => "our-laxer",
            Class::OurStricter => "our-stricter",
            Class::OurCrash => "our-crash",
            Class::OurTimeout => "our-timeout",
            Class::OurRenderFail => "our-render-fail",
            Class::OurRendersWhatTheyReject => "our-renders-what-they-reject",
            Class::DimMismatch => "dim-mismatch",
            Class::PixelMismatch => "pixel-mismatch",
            Class::BothAcceptMatch => "both-accept-match",
            Class::SoloAccept => "solo-accept",
            Class::SoloReject => "solo-reject",
        }
    }

    /// Findings worth persisting to disk (crashes and anything that isn't
    /// plain double-rejection / expected-match noise).
    fn is_finding(self) -> bool {
        matches!(
            self,
            Class::OurLaxer
                | Class::OurStricter
                | Class::OurCrash
                | Class::OurTimeout
                | Class::OurRenderFail
                | Class::OurRendersWhatTheyReject
                | Class::DimMismatch
                | Class::PixelMismatch
        )
    }
}

struct Finding {
    /// `Class::label()` for render-ladder findings, or `meta-<plane>-<class>`
    /// for metadata-plane findings.
    class_label: String,
    file_stem: String,
    mutant_idx: usize,
    mutation_desc: String,
    mutation_chunk: String,
    page_idx: usize,
    detail: String,
}

fn save_finding(out_dir: &Path, mutant: &[u8], f: &Finding) {
    let _ = std::fs::create_dir_all(out_dir);
    let base = format!("{}_{:05}_{}", f.file_stem, f.mutant_idx, f.class_label);
    let djvu_path = out_dir.join(format!("{base}.djvu"));
    let txt_path = out_dir.join(format!("{base}.txt"));
    if std::fs::write(&djvu_path, mutant).is_ok() {
        let note = format!(
            "class: {}\nsource file: {}\nmutant index: {}\nmutation: {}\nmutated chunk: {}\ntarget page (0-based): {}\ndetail: {}\n",
            f.class_label,
            f.file_stem,
            f.mutant_idx,
            f.mutation_desc,
            f.mutation_chunk,
            f.page_idx,
            f.detail,
        );
        let _ = std::fs::write(&txt_path, note);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RenderRejectBucket {
    chunk: String,
    their_error: String,
}

#[derive(Default)]
struct RenderRejectBucketStats {
    count: usize,
    examples: Vec<String>,
    pixel_samples: usize,
    max_mean_abs: f64,
    max_abs: u8,
    max_pct_gt8: f64,
    max_pct_gt32: f64,
}

fn our_decode_path(our: &OurOutcome) -> String {
    match our {
        OurOutcome::StructReject(e) => format!("struct-reject({e})"),
        OurOutcome::RenderFailed(e) => format!("render-failed({e})"),
        OurOutcome::Rendered { w, h, .. } => format!("rendered {w}x{h}"),
        OurOutcome::Panicked(e) => format!("panic({e})"),
        OurOutcome::Timeout => "timeout".to_string(),
    }
}

fn ddjvu_error_class(stderr: &str) -> String {
    let mut fallback = None;
    for line in stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.strip_prefix("ddjvu: ").unwrap_or(line))
    {
        if line.starts_with("Cannot decode page ") {
            fallback = Some("Cannot decode page");
        } else {
            return line.trim_end_matches('.').to_string();
        }
    }
    fallback.unwrap_or("<no stderr>").to_string()
}

fn diff_stats_rgba(w: usize, h: usize, lhs_rgba: &[u8], rhs_rgba: &[u8]) -> DiffStats {
    let mut rhs_rgb = Vec::with_capacity(w * h * 3);
    for pixel in rhs_rgba.chunks_exact(4) {
        rhs_rgb.extend_from_slice(&pixel[..3]);
    }
    diff_stats(w, h, lhs_rgba, &rhs_rgb)
}

// ── CLI / config ─────────────────────────────────────────────────────────────

struct Config {
    files: Vec<PathBuf>,
    seed: u64,
    mutants_per_file: usize,
    our_timeout: Duration,
    ref_timeout: Duration,
    max_seconds: u64,
    out_dir: PathBuf,
    ddjvu_bin: String,
    djvudump_bin: String,
    djvused_bin: String,
    /// Cap on saved findings per (file, class) — avoids flooding the
    /// regressions directory with near-duplicate repros of one root cause.
    findings_cap: usize,
}

fn default_files(manifest: &Path) -> Vec<PathBuf> {
    // A representative, cheap-to-mutate spread: a bundled multi-page bilevel
    // scan (watchmaker), a small bundled two-page doc (cable), and a single-
    // page non-bundled doc (boy) — see the djvudump dumps consulted while
    // designing this harness.
    [
        "tests/corpus/watchmaker.djvu",
        "tests/corpus/cable_1973_100133.djvu",
        "tests/fixtures/boy.djvu",
        // NAVM + ANTz coverage for the metadata planes (#597) — the corpus
        // scans above only carry TXTz.
        "tests/fixtures/navm_fgbz.djvu",
    ]
    .iter()
    .map(|p| manifest.join(p))
    .collect()
}

fn parse_args() -> Config {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = Config {
        files: Vec::new(),
        seed: 0xD1FF_F022_1234_5678,
        mutants_per_file: 500,
        our_timeout: Duration::from_millis(2000),
        ref_timeout: Duration::from_millis(5000),
        max_seconds: 1200,
        out_dir: manifest.join("fuzz/corpus-regressions/diff_fuzz"),
        ddjvu_bin: std::env::var("DIFF_FUZZ_DDJVU").unwrap_or_else(|_| "ddjvu".to_string()),
        djvudump_bin: std::env::var("DIFF_FUZZ_DJVUDUMP")
            .unwrap_or_else(|_| "djvudump".to_string()),
        djvused_bin: std::env::var("DIFF_FUZZ_DJVUSED").unwrap_or_else(|_| "djvused".to_string()),
        findings_cap: 5,
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                i += 1;
                cfg.seed = args[i].parse().unwrap_or(cfg.seed);
            }
            "--mutants" => {
                i += 1;
                cfg.mutants_per_file = args[i].parse().unwrap_or(cfg.mutants_per_file);
            }
            "--timeout-ms" => {
                i += 1;
                cfg.our_timeout = Duration::from_millis(args[i].parse().unwrap_or(2000));
            }
            "--ref-timeout-ms" => {
                i += 1;
                cfg.ref_timeout = Duration::from_millis(args[i].parse().unwrap_or(5000));
            }
            "--max-seconds" => {
                i += 1;
                cfg.max_seconds = args[i].parse().unwrap_or(cfg.max_seconds);
            }
            "--out-dir" => {
                i += 1;
                cfg.out_dir = PathBuf::from(&args[i]);
            }
            "--ddjvu" => {
                i += 1;
                cfg.ddjvu_bin = args[i].clone();
            }
            "--djvudump" => {
                i += 1;
                cfg.djvudump_bin = args[i].clone();
            }
            "--djvused" => {
                i += 1;
                cfg.djvused_bin = args[i].clone();
            }
            other => cfg.files.push(PathBuf::from(other)),
        }
        i += 1;
    }

    if cfg.files.is_empty() {
        cfg.files = default_files(&manifest);
    }
    cfg
}

/// The mean-abs threshold above which a same-bytes-decoded pixel diff is
/// treated as a real mismatch rather than the expected resampling/AA noise.
/// `interop_pixdiff`'s corpus sweep establishes a mean well under 0.25 on
/// well-formed input; mutants that still decode on both sides but land here
/// are a different phenomenon (a real interop divergence), so the bar is set
/// generously above the established floor rather than at it.
const PIXEL_MISMATCH_MEAN_THRESHOLD: f64 = 4.0;
const PIXEL_MISMATCH_GT32_THRESHOLD: f64 = 2.0;

fn main() {
    // Silence the default panic hook's stderr spam — a bounded fuzz run is
    // expected to induce some panics deliberately (caught via catch_unwind);
    // printing each one would drown the summary. Real panic findings still
    // get their message via the caught payload.
    panic::set_hook(Box::new(|_info| {}));

    let cfg = parse_args();

    let have_djvudump = Command::new(&cfg.djvudump_bin)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    let have_ddjvu = Command::new(&cfg.ddjvu_bin)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    let have_djvused = Command::new(&cfg.djvused_bin)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    let solo_mode = !have_djvudump;

    println!("DIFF_FUZZ — corpus-mutation differential fuzzer");
    println!(
        "  seed={:#x}  mutants/file={}  our-timeout={:?}  ref-timeout={:?}  budget={}s",
        cfg.seed, cfg.mutants_per_file, cfg.our_timeout, cfg.ref_timeout, cfg.max_seconds
    );
    if solo_mode {
        println!(
            "  djvudump not found on PATH (or at --djvudump) — running SOLO MODE:\n\
             mutation + our-side parse/render only, no djvudump/ddjvu comparison.\n\
             Install DjVuLibre (`brew install djvulibre`) and re-run for full\n\
             differential mode, or pass --djvudump/--ddjvu to point at them."
        );
    } else if !have_ddjvu {
        println!(
            "  djvudump found but ddjvu not found — parse-divergence checks run,\n\
             pixel comparison is skipped for mutants both sides accept."
        );
    }
    if !have_djvused {
        println!(
            "  djvused not found — metadata planes (#597: txt/ant/outline) are\n\
             not compared. Install DjVuLibre or pass --djvused."
        );
    }

    let tmp_dir = std::env::temp_dir().join(format!("diff_fuzz_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);

    let overall_start = Instant::now();
    let budget = Duration::from_secs(cfg.max_seconds);

    let mut totals: std::collections::BTreeMap<Class, usize> = std::collections::BTreeMap::new();
    let mut findings_saved: std::collections::BTreeMap<Class, usize> =
        std::collections::BTreeMap::new();
    let mut meta_totals: std::collections::BTreeMap<(&'static str, MetaClass), usize> =
        std::collections::BTreeMap::new();
    let mut meta_findings_saved: std::collections::BTreeMap<(&'static str, MetaClass), usize> =
        std::collections::BTreeMap::new();
    let mut render_reject_buckets: std::collections::BTreeMap<
        RenderRejectBucket,
        RenderRejectBucketStats,
    > = std::collections::BTreeMap::new();
    let mut saved_paths: Vec<PathBuf> = Vec::new();
    let mut ran_out_of_time = false;

    for file in &cfg.files {
        let Ok(base) = std::fs::read(file) else {
            println!("SKIP (unreadable): {}", file.display());
            continue;
        };
        let file_stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("input")
            .to_string();
        let chunks = collect_chunks(&base);
        let n_pages = chunks
            .iter()
            .filter_map(|c| c.page_index)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        println!(
            "--- {} ({} bytes, {} chunk spans, {} page scopes) ---",
            file.display(),
            base.len(),
            chunks.len(),
            n_pages,
        );

        // Per-(page) metadata baseline on the unmutated file: findings are
        // gated on differing from it, so a pre-existing normalization gap
        // between our extraction and djvused surfaces once per file, not per
        // mutant. Computed lazily for the pages mutations actually target.
        let mut meta_baseline: std::collections::HashMap<usize, [MetaClass; 3]> =
            std::collections::HashMap::new();
        let mut clean_render_baseline: std::collections::HashMap<
            usize,
            Option<(u32, u32, Vec<u8>)>,
        > = std::collections::HashMap::new();

        for m in 0..cfg.mutants_per_file {
            if overall_start.elapsed() > budget {
                ran_out_of_time = true;
                break;
            }
            // Deterministic per-mutant seed: stable across reruns of the same
            // --seed regardless of file iteration order details.
            let mut hasher_seed = cfg.seed
                ^ (m as u64).wrapping_mul(0x9E3779B97F4A7C15)
                ^ (file_stem
                    .bytes()
                    .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64)));
            if hasher_seed == 0 {
                hasher_seed = 1;
            }
            let mut rng = Rng::new(hasher_seed);
            let mutation = apply_mutation(&base, &chunks, &mut rng);
            let page_idx = mutation.page_index.unwrap_or(0);

            let mutant_path = tmp_dir.join(format!("{file_stem}_{m:05}.djvu"));
            if std::fs::write(&mutant_path, &mutation.bytes).is_err() {
                continue;
            }

            let our = our_attempt(mutation.bytes.clone(), page_idx, cfg.our_timeout);

            // our_level: 0 = structural reject, 1 = struct-ok but render failed,
            // 2 = fully rendered. Computed unconditionally except for the
            // panic/timeout escapes, which short-circuit to their own class.
            let class = if let OurOutcome::Panicked(_) = &our {
                Class::OurCrash
            } else if let OurOutcome::Timeout = &our {
                Class::OurTimeout
            } else if solo_mode {
                match &our {
                    OurOutcome::Rendered { .. } | OurOutcome::RenderFailed(_) => Class::SoloAccept,
                    OurOutcome::StructReject(_) => Class::SoloReject,
                    _ => unreachable!(),
                }
            } else {
                let our_level = match &our {
                    OurOutcome::StructReject(_) => 0u8,
                    OurOutcome::RenderFailed(_) => 1,
                    OurOutcome::Rendered { .. } => 2,
                    _ => unreachable!(),
                };
                let their_struct_accept =
                    djvudump_accepts(&cfg.djvudump_bin, &mutant_path, cfg.ref_timeout);
                match their_struct_accept {
                    None => {
                        // djvudump vanished mid-run (shouldn't happen) — treat
                        // as solo-mode for this mutant only.
                        if our_level == 0 {
                            Class::SoloReject
                        } else {
                            Class::SoloAccept
                        }
                    }
                    Some(false) => {
                        // djvudump structurally rejects.
                        if our_level == 0 {
                            Class::BothReject
                        } else {
                            Class::OurLaxer
                        }
                    }
                    Some(true) if our_level == 0 => Class::OurStricter,
                    Some(true) => {
                        // Both structurally accept — determine their_level by
                        // actually trying to render (djvudump alone can't
                        // tell us this; see the module doc for the confirmed
                        // INFO/BG44-size-mismatch case that motivates it).
                        let their_level: u8;
                        let mut ppm_rgb: Option<(usize, usize, Vec<u8>)> = None;
                        if have_ddjvu {
                            let out_ppm = tmp_dir.join(format!("{file_stem}_{m:05}.ppm"));
                            let rendered = ddjvu_render(
                                &cfg.ddjvu_bin,
                                &mutant_path,
                                page_idx + 1,
                                &out_ppm,
                                cfg.ref_timeout,
                            )
                            .unwrap_or(false);
                            if rendered {
                                let ppm_bytes = std::fs::read(&out_ppm).unwrap_or_default();
                                let _ = std::fs::remove_file(&out_ppm);
                                ppm_rgb = parse_ppm(&ppm_bytes);
                                their_level = if ppm_rgb.is_some() { 2 } else { 1 };
                            } else {
                                their_level = 1;
                            }
                        } else {
                            // No ddjvu available: can't distinguish level 1 vs
                            // 2 on their side, so don't over-claim — treat any
                            // struct-ok our_level as an uneventful match.
                            their_level = our_level;
                        }

                        match our_level.cmp(&their_level) {
                            std::cmp::Ordering::Less => Class::OurRenderFail,
                            std::cmp::Ordering::Greater if our_level == 2 && their_level == 1 => {
                                Class::OurRendersWhatTheyReject
                            }
                            std::cmp::Ordering::Greater => Class::OurRenderFail, // shouldn't hit (our_level<=2)
                            std::cmp::Ordering::Equal if our_level < 2 => Class::BothRenderFail,
                            std::cmp::Ordering::Equal => {
                                // Both fully rendered — pixel comparison.
                                let OurOutcome::Rendered { w, h, rgba } = &our else {
                                    unreachable!()
                                };
                                match ppm_rgb {
                                    Some((rw, rh, ref_rgb)) => {
                                        if rw != *w as usize || rh != *h as usize {
                                            Class::DimMismatch
                                        } else {
                                            let d = diff_stats(rw, rh, rgba, &ref_rgb);
                                            if d.mean_abs > PIXEL_MISMATCH_MEAN_THRESHOLD
                                                || d.pct_gt32 > PIXEL_MISMATCH_GT32_THRESHOLD
                                            {
                                                Class::PixelMismatch
                                            } else {
                                                Class::BothAcceptMatch
                                            }
                                        }
                                    }
                                    None => Class::BothAcceptMatch, // no ddjvu; can't compare pixels
                                }
                            }
                        }
                    }
                }
            };

            *totals.entry(class).or_insert(0) += 1;

            let render_reject_stderr =
                if class == Class::OurRendersWhatTheyReject && !solo_mode && have_ddjvu {
                    let scratch_ppm = tmp_dir.join(format!("{file_stem}_{m:05}_bucket.ppm"));
                    let stderr = ddjvu_stderr(
                        &cfg.ddjvu_bin,
                        &mutant_path,
                        page_idx + 1,
                        &scratch_ppm,
                        cfg.ref_timeout,
                    );
                    let _ = std::fs::remove_file(&scratch_ppm);
                    let bucket_stats = render_reject_buckets
                        .entry(RenderRejectBucket {
                            chunk: mutation.chunk.clone(),
                            their_error: ddjvu_error_class(&stderr),
                        })
                        .or_default();
                    bucket_stats.count += 1;
                    if bucket_stats.examples.len() < 3 {
                        bucket_stats.examples.push(format!(
                            "{}_{m:05}: {} | ours {}",
                            file_stem,
                            mutation.desc,
                            our_decode_path(&our)
                        ));
                    }
                    if let OurOutcome::Rendered { w, h, rgba } = &our {
                        let baseline = clean_render_baseline.entry(page_idx).or_insert_with(|| {
                            match our_attempt(base.clone(), page_idx, cfg.our_timeout) {
                                OurOutcome::Rendered { w, h, rgba } => Some((w, h, rgba)),
                                _ => None,
                            }
                        });
                        match baseline {
                            Some((baseline_w, baseline_h, baseline_rgba))
                                if baseline_w == w && baseline_h == h =>
                            {
                                let diff =
                                    diff_stats_rgba(*w as usize, *h as usize, rgba, baseline_rgba);
                                bucket_stats.pixel_samples += 1;
                                bucket_stats.max_mean_abs =
                                    bucket_stats.max_mean_abs.max(diff.mean_abs);
                                bucket_stats.max_abs = bucket_stats.max_abs.max(diff.max_abs);
                                bucket_stats.max_pct_gt8 =
                                    bucket_stats.max_pct_gt8.max(diff.pct_gt8);
                                bucket_stats.max_pct_gt32 =
                                    bucket_stats.max_pct_gt32.max(diff.pct_gt32);
                            }
                            _ => {}
                        }
                    }
                    Some(stderr)
                } else {
                    None
                };

            if class.is_finding() {
                let saved_count = findings_saved.entry(class).or_insert(0);
                if *saved_count < cfg.findings_cap || class == Class::OurCrash {
                    let detail = our_decode_path(&our);
                    let detail = if !solo_mode
                        && matches!(
                            class,
                            Class::OurStricter | Class::OurLaxer | Class::OurCrash
                        ) {
                        let their_stderr =
                            djvudump_stderr(&cfg.djvudump_bin, &mutant_path, cfg.ref_timeout);
                        format!("{detail}\ndjvudump stderr: {their_stderr}")
                    } else if !solo_mode
                        && have_ddjvu
                        && matches!(
                            class,
                            Class::OurRendersWhatTheyReject
                                | Class::OurRenderFail
                                | Class::DimMismatch
                                | Class::PixelMismatch
                        )
                    {
                        let their_stderr = match &render_reject_stderr {
                            Some(stderr) => stderr.clone(),
                            None => {
                                let scratch_ppm =
                                    tmp_dir.join(format!("{file_stem}_{m:05}_finding.ppm"));
                                let stderr = ddjvu_stderr(
                                    &cfg.ddjvu_bin,
                                    &mutant_path,
                                    page_idx + 1,
                                    &scratch_ppm,
                                    cfg.ref_timeout,
                                );
                                let _ = std::fs::remove_file(&scratch_ppm);
                                stderr
                            }
                        };
                        let their_error = ddjvu_error_class(&their_stderr);
                        format!(
                            "{detail}\nour decode path: {}\nddjvu error class: {their_error}\nddjvu stderr: {their_stderr}",
                            our_decode_path(&our)
                        )
                    } else {
                        detail
                    };
                    let finding = Finding {
                        class_label: class.label().to_string(),
                        file_stem: file_stem.clone(),
                        mutant_idx: m,
                        mutation_desc: mutation.desc.clone(),
                        mutation_chunk: mutation.chunk.clone(),
                        page_idx,
                        detail,
                    };
                    save_finding(&cfg.out_dir, &mutation.bytes, &finding);
                    saved_paths.push(cfg.out_dir.join(format!(
                        "{}_{:05}_{}.djvu",
                        file_stem,
                        m,
                        class.label()
                    )));
                    *saved_count += 1;
                }
            }

            // ── Metadata planes (#597): txt / ant / outline vs djvused ──
            // Skipped when our side is unstable on this mutant (crash /
            // timeout) or when both sides already rejected structurally —
            // djvused has nothing meaningful to print for those.
            if have_djvused
                && !matches!(
                    class,
                    Class::BothReject | Class::OurCrash | Class::OurTimeout | Class::SoloReject
                )
            {
                let baseline = *meta_baseline.entry(page_idx).or_insert_with(|| {
                    let bo = our_meta(base.clone(), page_idx, cfg.our_timeout);
                    let bt = their_meta(&cfg.djvused_bin, file, page_idx, cfg.ref_timeout);
                    let cls = [
                        meta_class(&bo[0], &bt[0]),
                        meta_class(&bo[1], &bt[1]),
                        meta_class(&bo[2], &bt[2]),
                    ];
                    println!(
                        "  metadata baseline (page {page_idx}): txt={} ant={} outline={}",
                        cls[0].label(),
                        cls[1].label(),
                        cls[2].label()
                    );
                    cls
                });

                let ours = our_meta(mutation.bytes.clone(), page_idx, cfg.our_timeout);
                let theirs = their_meta(&cfg.djvused_bin, &mutant_path, page_idx, cfg.ref_timeout);
                for (i, plane) in META_PLANES.iter().enumerate() {
                    let mcls = meta_class(&ours[i], &theirs[i]);
                    *meta_totals.entry((plane, mcls)).or_insert(0) += 1;
                    if mcls.is_finding() && mcls != baseline[i] {
                        let saved = meta_findings_saved.entry((plane, mcls)).or_insert(0);
                        if *saved < cfg.findings_cap {
                            let label = format!("meta-{}-{}", plane, mcls.label());
                            let finding = Finding {
                                class_label: label.clone(),
                                file_stem: file_stem.clone(),
                                mutant_idx: m,
                                mutation_desc: mutation.desc.clone(),
                                mutation_chunk: mutation.chunk.clone(),
                                page_idx,
                                detail: format!(
                                    "baseline: {}\nours:   {}\ntheirs: {}",
                                    baseline[i].label(),
                                    plane_side_brief(&ours[i]),
                                    plane_side_brief(&theirs[i])
                                ),
                            };
                            save_finding(&cfg.out_dir, &mutation.bytes, &finding);
                            saved_paths
                                .push(cfg.out_dir.join(format!("{file_stem}_{m:05}_{label}.djvu")));
                            *saved += 1;
                        }
                    }
                }
            }

            let _ = std::fs::remove_file(&mutant_path);

            if (m + 1) % 200 == 0 {
                println!("  ... {} / {} mutants", m + 1, cfg.mutants_per_file);
            }
        }
        if ran_out_of_time {
            println!("  (time budget exhausted, stopping early)");
            break;
        }
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);

    let elapsed = overall_start.elapsed();
    println!(
        "\n=== summary ({:.1}s elapsed{}) ===",
        elapsed.as_secs_f64(),
        if ran_out_of_time {
            ", TIME BUDGET HIT"
        } else {
            ""
        }
    );
    let total: usize = totals.values().sum();
    println!("total mutants classified: {total}");
    for (class, count) in &totals {
        println!("  {:<18} {:>6}", class.label(), count);
    }
    if !render_reject_buckets.is_empty() {
        let mut ranked: Vec<_> = render_reject_buckets.iter().collect();
        ranked.sort_by(|(left_key, left_stats), (right_key, right_stats)| {
            right_stats
                .count
                .cmp(&left_stats.count)
                .then_with(|| left_key.chunk.cmp(&right_key.chunk))
                .then_with(|| left_key.their_error.cmp(&right_key.their_error))
        });
        println!("our-renders-what-they-reject buckets (chunk × ddjvu error):");
        for (rank, (bucket, stats)) in ranked.iter().take(10).enumerate() {
            println!(
                "  #{:<2} {:<8} {:>5}  {}  pixel-samples={} max_mean={:.3} max_abs={} max_gt8={:.3}% max_gt32={:.3}%",
                rank + 1,
                bucket.chunk,
                stats.count,
                bucket.their_error,
                stats.pixel_samples,
                stats.max_mean_abs,
                stats.max_abs,
                stats.max_pct_gt8,
                stats.max_pct_gt32
            );
            for example in &stats.examples {
                println!("       example: {example}");
            }
        }
    }
    if !meta_totals.is_empty() {
        println!("metadata planes (#597, per plane × class):");
        for ((plane, mcls), count) in &meta_totals {
            println!("  {:<8} {:<16} {:>6}", plane, mcls.label(), count);
        }
    }
    if saved_paths.is_empty() {
        println!("no findings saved (out-dir: {})", cfg.out_dir.display());
    } else {
        println!("findings saved under {}:", cfg.out_dir.display());
        for p in &saved_paths {
            println!("  {}", p.display());
        }
    }
}
