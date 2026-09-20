// Regenerate the data-driven parts of the site's index.html from SOURCE OF TRUTH, so the site
// can never drift from the shipping software again. It rewrites:
//   * the format wall  (bar + per-category chip lists, plus a keyboard/touch-reachable
//     description panel per category) from `st2k formats --json`
//   * the version pills (softwareVersion, download meta, footer) from Cargo.toml
//   * the "hundreds of formats, N categories" eyebrow, derived from the same data
// The prose deliberately avoids hard-coded format counts and installer byte claims.
// Only the auto-generated wall below carries exact, self-updating category counts;
// release notes carry each published installer's exact bytes and digest.
//
// Run before deploying the site:
//   node scripts/gen-site.mjs [path\to\st2k.exe] [--site <path\to\index.html>] [--check]
//   node scripts/gen-site.mjs --self-test
//
// st2k.exe resolution order: arg -> $ST2K -> resolved cargo target dir -> installed -> PATH.
// The resolved target dir follows scripts/_targetdir.ps1's own order (CARGO_TARGET_DIR env,
// then a `target-dir` redirect in .cargo/config.toml, then the default ./target next to the
// workspace), never a hardcoded dev-machine path.
//
// AUDIT F24 (P3): the target file used to be hardcoded to the app repo's own gitignored
// `site/index.html` staging copy - fine for the documented local workflow (pull the deploy
// repo, copy ITS index.html over this staging copy, run this script on it, mirror back
// excluding CNAME - see CLAUDE.md section 7), but nothing stopped this script from also
// pairing that staging copy with a STALE st2k.exe: resolution used to fall through to
// whatever built binary happened to exist, while the version stamped into the page came
// from Cargo.toml (the source, not the binary) - so a binary older than the source could
// silently ship a format table that does not match the version the page claims. `--site`
// arkitect-allow: no-bandaids - build-release.ps1's [site] step runs `node gen-site.mjs <st2k.exe>` with no --site, so the no-flag write target must be site\index.html.
// makes the write target explicit (defaults to the old staging copy for back-compat) and
// the executable's OWN reported version is now asserted equal to Cargo.toml's before
// anything is written. `--check` runs the full pipeline (including that assertion) without
// writing, so it can gate a deploy without mutating the target file. `--self-test` proves
// the failure modes without needing a real st2k.exe at all - the assertions live in the
// sibling scripts/gen-site.test.mjs, which --self-test runs under `node --test`.
//
// Idempotent: running it twice (for real, against the same st2k.exe + index.html) is a no-op.
// CRLF-preserving.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync, spawnSync } from 'node:child_process';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const THIS_FILE = fileURLToPath(import.meta.url);
const TEST_FILE = path.join(path.dirname(THIS_FILE), 'gen-site.test.mjs');

// ---- CLI args ---------------------------------------------------------------
/** Split argv into the flags and the one positional (the st2k executable). Pure, so the
 *  self-test can pin it: without `--site`, `siteFlagAt` is -1 and a bare `i !== siteFlagAt + 1`
 *  used to drop argv[0], which is the executable itself, so `node gen-site.mjs path\to\st2k.exe`
 *  silently ignored the path and generated from whatever stale release binary was lying around
 *  (found 2026-09-07 when a page came out with no capability sentences at all). */
export function parseArgs(argv) {
  const siteFlagAt = argv.indexOf('--site');
  const SITE_ARG = siteFlagAt >= 0 ? argv[siteFlagAt + 1] : undefined;
  const positional = argv.filter((a, i) =>
    a !== '--check' && a !== '--self-test' && a !== '--site' && (siteFlagAt < 0 || i !== siteFlagAt + 1));
  return { SITE_ARG, ST2K_ARG: positional[0] };
}

// ---- resolve the cargo target dir, mirroring scripts/_targetdir.ps1 --------
function resolveTargetDir() {
  if (process.env.CARGO_TARGET_DIR) return process.env.CARGO_TARGET_DIR;
  const cfg = path.join(ROOT, '.cargo', 'config.toml');
  if (fs.existsSync(cfg)) {
    const m = fs.readFileSync(cfg, 'utf8').match(/target-dir\s*=\s*"([^"]+)"/);
    if (m) return m[1];
  }
  return path.join(ROOT, 'target');
}

// ---- locate st2k.exe -------------------------------------------------------
function findSt2k(argExe) {
  const cands = [
    argExe,
    process.env.ST2K,
    path.join(resolveTargetDir(), 'release', 'st2k.exe'),
    'C:/Program Files/SageThumbs2K/st2k.exe',
    'st2k',
  ].filter(Boolean);
  for (const c of cands) {
    // A candidate that fails to run is simply not the st2k we are looking for: the error
    // carries no information beyond "try the next one", and the throw below names them all.
    try { execFileSync(c, ['formats', '--json'], { stdio: 'ignore' }); return c; } catch { /* next candidate */ }
  }
  throw new Error('st2k.exe not found. Build it (cargo build --release) or pass its path as arg 1.');
}

/** Parses "st2k 2.5.0" (or similar) into "2.5.0". Throws on anything unparsable, because
 *  silently treating "unknown version" as "matches" is exactly the bug this exists to catch. */
function getExeVersion(exe) {
  const out = execFileSync(exe, ['--version'], { encoding: 'utf8' });
  const m = out.match(/(\d+\.\d+\.\d+)/);
  if (!m) throw new Error(`gen-site: could not parse a version out of "${exe} --version" (got: ${JSON.stringify(out.trim())})`);
  return m[1];
}

function readCargoVersion(root) {
  const cargo = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
  const v = (cargo.match(/^version\s*=\s*"([^"]+)"/m) || [])[1];
  if (!v) throw new Error('could not read version from Cargo.toml');
  return v;
}

/** The whole point of this file: an executable's format table and a version string are two
 *  independent pieces of state (one lives in a built binary, one in source), and nothing
 *  before this assertion ever checked they described the same build. A stale exe paired
 *  with a bumped Cargo.toml would ship yesterday's format table under today's version number
 *  and nothing would notice. */
export function assertVersionMatch(exeVersion, sourceVersion, exePath) {
  if (exeVersion !== sourceVersion) {
    throw new Error(
      `gen-site: ${exePath} reports version ${exeVersion}, but the source (Cargo.toml) says ` +
      `${sourceVersion}. Its format table would ship under a version it wasn't built at. ` +
      `Rebuild st2k (cargo build --release) or pass the correct executable path as arg 1.`);
  }
}

export const esc = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

// EVERY category `st2k formats --json` can emit must appear here. A category missing
// from this list is silently dropped from the wall AND from the bar, so the bar stops
// summing to 100% and the site under-reports what ships (that is exactly what happened
// to `Archive` between its release and 2026-08-01). buildFormatWall()'s assertion enforces it.
const ORDER = [
  ['img', 'Image', 'Image', '#4d9cff'], ['doc', 'Document', 'Document', '#ef8b5a'],
  ['raw', 'Camera RAW', 'Camera RAW', '#b48bff'], ['vid', 'Video', 'Video', '#f06ab0'],
  ['aud', 'Audio', 'Audio', '#38d39f'], ['ebk', 'Ebook', 'Ebook &amp; comics', '#f2c14e'],
  ['arc', 'Archive', 'Archive', '#7d8ca3'],
];
const ARIA = { img: 'image', doc: 'document', raw: 'camera raw', vid: 'video', aud: 'audio', ebk: 'ebook and comics', arc: 'archive' };

// Audit E03: one sentence per category, derived from `st2k formats --json`'s capability
// fields (`source`/`os_codec`) rather than hand-typed prose that can drift from what the
// app actually does. Every category maps to exactly one `source` by construction
// (`formats::capability` in the Rust source derives it from the same category), so the
// FIRST item's source stands for the whole group; OS-codec-dependent extensions within
// the group are named (or, when the dependency covers the whole group, said once).
const SOURCE_SENTENCE = {
  full_decode: 'Each thumbnail is a full decode of the image itself.',
  embedded_preview: "Each thumbnail rides the file's embedded preview, with a full demosaic as the backstop when there isn't one.",
  cover_art: "Each thumbnail is the file's embedded cover art (a waveform when there is none and the format is uncompressed PCM).",
  cover_or_first_page: "Each thumbnail is the format's own cover image, or its first page rendered.",
  video_frame: 'Each thumbnail is a representative frame grabbed from the video.',
  contained_images: 'Each thumbnail shows the images found inside the archive, not one photo.',
};
// Audit E03 #1: a MIXED group (some formats a full decode, others a carried/embedded
// preview - Image is exactly this since PSD/EPS/APK/Blender/etc. ride a container preview)
// gets a per-source COUNT instead of `SOURCE_SENTENCE`'s single blanket claim. Same map,
// phrased as "N format(s) <verb phrase>" rather than "Each thumbnail is <noun phrase>."
const SOURCE_COUNT_PHRASE = {
  full_decode: 'are a full decode of the image itself',
  embedded_preview: "ride the file's own embedded/carried preview instead of a full decode",
  cover_art: "show the file's embedded cover art",
  cover_or_first_page: "show the format's own cover image or first page rendered",
  video_frame: 'show a frame grabbed from the video',
  contained_images: 'show the images found inside instead of one photo',
};
const CODEC_NAMES = {
  media_foundation: 'the OS Media Foundation codecs',
  wmphoto: "the OS's WIC JPEG XR codec",
  heif: "the OS's WIC HEIF codec",
  av1: 'the AV1 Video Extension',
};
// Codecs the bundled ImageMagick can stand in for on a Full install (see capabilityParts).
const CODEC_OPTIONAL_WITH_BUNDLE = new Set(['heif', 'av1']);

/** One capability sentence for a category's items, built from the data (never hand-typed
 *  per category) - see the `SOURCE_SENTENCE`/`SOURCE_COUNT_PHRASE`/`CODEC_NAMES` maps
 *  above. Audit E03 #3: an unrecognized `source`/`os_codec` value THROWS rather than
 *  silently rendering an empty sentence or the raw wire token - a renamed vocabulary
 *  string must fail the build, not ship a blank/garbled sentence to the live site. */
export function capabilityParts(items) {
  return { source: sourceSentence(items), codecs: codecSentences(items) };
}

/** "Each thumbnail is ..." for a single-source group; "N formats ... ; M formats ..." for a
 *  mixed one (audit E03 #1: never claim one blanket behaviour for a category whose formats
 *  don't share it - say how many are which, biggest group first). */
export function sourceSentence(items) {
  const bySource = {};
  for (const x of items) (bySource[x.source] = bySource[x.source] || []).push(x);
  const sourceKeys = Object.keys(bySource);
  if (sourceKeys.length <= 1) {
    const source = items[0] && items[0].source;
    const s = SOURCE_SENTENCE[source];
    if (s === undefined) {
      throw new Error(`gen-site: unknown capability source "${source}" - add it to SOURCE_SENTENCE (and SOURCE_COUNT_PHRASE)`);
    }
    return s;
  }
  const bits = sourceKeys
    .toSorted((a, b) => bySource[b].length - bySource[a].length)
    .map(src => {
      const phrase = SOURCE_COUNT_PHRASE[src];
      if (!phrase) throw new Error(`gen-site: unknown capability source "${src}" - add it to SOURCE_COUNT_PHRASE (and SOURCE_SENTENCE)`);
      return `${bySource[src].length} ${phrase}`;
    });
  return bits.join('; ') + '.';
}

/** One sentence per Windows codec some of the group's formats use, as an array (one
 *  paragraph each on the page: the Image group's three notes ran to 60 words joined, and
 *  the site's copy budget refuses any paragraph over 40). Empty when none. */
export function codecSentences(items) {
  const byCodec = {};
  for (const x of items) {
    if (x.os_codec) (byCodec[x.os_codec] = byCodec[x.os_codec] || []).push(x.ext);
  }
  return Object.entries(byCodec).map(([codec, exts]) => codecSentence(codec, exts, items.length));
}

/** The sentence for one codec: "Every format here needs/uses ..." when the whole group
 *  shares it, otherwise ".a, .b additionally need(s)/use(s) ...". */
export function codecSentence(codec, exts, groupSize) {
  const name = CODEC_NAMES[codec];
  if (!name) throw new Error(`gen-site: unknown os_codec "${codec}" - add it to CODEC_NAMES`);
  // The HEIF and AV1 image routes are the FAST route, not the only one: a Full install
  // decodes those through the bundled ImageMagick when Windows has no codec (2026-09-19
  // audit F23, measured on real files). Video and JPEG XR genuinely need theirs.
  const optional = CODEC_OPTIONAL_WITH_BUNDLE.has(codec);
  const tail = optional
    ? `${name} when Windows has it; a Full install decodes ${exts.length === 1 ? 'it' : 'them'} through the bundled decoder otherwise.`
    : `${name}.`;
  if (exts.length === groupSize) {
    return optional ? `Every format here uses ${tail}` : `Every format here needs ${tail}`;
  }
  const verb = optional ? 'use' : 'need';
  return `.${exts.toSorted().join(', .')} additionally ${verb}${exts.length === 1 ? 's' : ''} ${tail}`;
}

/** One sentence group, for the self-tests and anything else that wants it as text. */
export function capabilitySentence(items) {
  return capabilityParagraphs(items).join(' ');
}

/** What the page renders: how the thumbnails are made, and (separately) which Windows codecs
 *  some of the formats need. Two paragraphs, not one - the Image group's single caption came
 *  to 53 words on 2026-09-17 and the site's copy budget (every paragraph <= 40) went red. */
export function capabilityParagraphs(items) {
  const { source, codecs } = capabilityParts(items);
  return [source, ...codecs].filter(Boolean);
}

/** Builds the bar + fmtwall block from `formats` (the parsed `st2k formats --json` array).
 *  Throws if `formats` names a category ORDER does not know about - failing loudly beats
 *  quietly shipping a wall that omits a whole category. Each chip keeps a native `title`
 *  (a no-cost fallback for touch/assistive tech - see index.html's tooltip script, which
 *  used to strip it) and each group gets a single focusable toggle button that reveals a
 *  plain-text description list, so every format's description is keyboard- and
 *  touch-reachable without adding one tab stop per chip (hundreds of them). */
/** Every item `st2k formats --json` hands us must carry the capability fields (audit E03).
 *  A binary built before those fields existed still answers with ext/category/description
 *  only, and `capabilitySentence` then quietly emits NOTHING for every group - a page with no
 *  capability sentences at all, from a run that exited 0 (that is exactly what a stale
 *  release st2k.exe produced on 2026-09-07). So the absence of the fields is an error naming
 *  the binary, not a blank. */
export function assertCapabilityFields(formats, exePath) {
  const bad = formats.filter(x =>
    typeof x.source !== 'string' || typeof x.convertible !== 'boolean' ||
    typeof x.preview_listing !== 'boolean' || !('os_codec' in x) ||
    (x.os_codec !== null && typeof x.os_codec !== 'string'));
  if (bad.length) {
    throw new Error(
      `gen-site: ${bad.length} of ${formats.length} format entries lack the capability fields ` +
      `(source/convertible/preview_listing/os_codec), first: ${JSON.stringify(bad[0])}. ` +
      `${exePath} predates them - rebuild st2k and pass that executable.`);
  }
}

export function buildFormatWall(formats, CR) {
  const TOTAL = formats.length;
  const by = {};
  for (const x of formats) (by[x.category] = by[x.category] || []).push(x);

  const missing = Object.keys(by).filter(c => !ORDER.some(([, cat]) => cat === c));
  if (missing.length) {
    throw new Error(
      `gen-site: ${missing.length} format categor${missing.length === 1 ? 'y is' : 'ies are'} ` +
      `missing from ORDER: ${missing.join(', ')}. Add each one (with a swatch colour and an ` +
      `ARIA label) plus a matching .fmtgroup[data-cat="..."] rule in index.html.`);
  }

  const aria = [], spans = [], groups = [];
  for (const [dc, cat, label, color] of ORDER) {
    const items = (by[cat] || []).toSorted((a, b) => a.ext.localeCompare(b.ext));
    const n = items.length, pct = TOTAL ? (n / TOTAL * 100).toFixed(1) : '0.0';
    aria.push(n + ' ' + ARIA[dc]);
    spans.push(`      <span style="width:${pct}%;background:${color}"></span>`);
    const chips = items.map(x => `<span class="fc" title="${esc(x.description)}">.${x.ext}</span>`).join(' ');
    const panelId = `fgdesc-${dc}`;
    const descList = items.map(x => `<li><code>.${x.ext}</code> ${esc(x.description)}</li>`).join(CR + '            ');
    const capParagraphs = n ? capabilityParagraphs(items) : [];
    groups.push(
      `      <div class="fmtgroup reveal" data-cat="${dc}">${CR}` +
      `        <h3 class="fgh"><span class="sw"></span>${label} <span class="cnt">${n}</span></h3>${CR}` +
      capParagraphs.map(p => `        <p class="fgcap">${esc(p)}</p>${CR}`).join('') +
      `        <div class="fgchips">${chips}</div>${CR}` +
      `        <button type="button" class="fgtoggle" aria-expanded="false" aria-controls="${panelId}">Show ${label} format descriptions</button>${CR}` +
      `        <div class="fgdesc" id="${panelId}" hidden>${CR}` +
      `          <ul>${CR}            ${descList}${CR}          </ul>${CR}` +
      `        </div>${CR}` +
      `      </div>`);
  }
  const block = `    <div class="bar reveal" role="img" aria-label="Format coverage by category: ${aria.join(', ')}">${CR}${spans.join(CR)}${CR}    </div>${CR}    <div class="fmtwall reveal">${CR}${groups.join(CR)}${CR}    </div>`;
  const presentCategories = ORDER.filter(([, cat]) => (by[cat] || []).length > 0).length;
  return { block, by, TOTAL, presentCategories };
}

/** Splices the format-wall block into `html` and syncs the version-derived scalars
 *  (js-app-version pills, JSON-LD softwareVersion, the "N categories" eyebrow). Every
 *  marker is asserted present (with an expected minimum count) before being rewritten,
 *  and an absent marker is a thrown error, never a silent no-op - a template edit that
 *  moves or renames a marker must fail the run, not ship stale content next to it. */
/** The `fmtgroup` blocks in a format-wall region whose `data-cat` this generator does not
 *  own (not in ORDER), each as its complete `<div class="fmtgroup ...">...</div>` element,
 *  in page order. Balanced-tag scan, not a regex: a group nests `<div class="fgdesc">`. */
export function foreignFormatGroups(region) {
  const owned = new Set(ORDER.map(([dc]) => dc));
  const open = /<div class="fmtgroup[^"]*"[^>]*data-cat="([a-z0-9-]+)"[^>]*>/g;
  const out = [];
  let m;
  while ((m = open.exec(region))) {
    if (owned.has(m[1])) continue;
    let depth = 0, i = m.index;
    const tag = /<\/?div\b[^>]*>/g;
    tag.lastIndex = m.index;
    let end = -1, t;
    while ((t = tag.exec(region))) {
      depth += t[0].startsWith('</') ? -1 : 1;
      if (depth === 0) { end = t.index + t[0].length; break; }
    }
    if (end < 0) throw new Error(`gen-site: unbalanced fmtgroup "${m[1]}" in the existing wall`);
    out.push(region.slice(m.index, end).replace(/^\s+/, '      '));
    open.lastIndex = end;
    i = end;
  }
  return out;
}

export function applyAll(html, { block, VERSION, presentCategories }) {
  const before = html;
  const CR = html.includes('\r\n') ? '\r\n' : '\n';

  const startIdx = html.indexOf('    <div class="bar reveal"');
  // The deployed page may carry either line ending; the CRLF form is tried first because it
  // is the one the checked-out template has. A single search each, kept as positions.
  const crlfEnd = html.indexOf('\r\n  </div>\r\n</section>', startIdx);
  const endIdx = crlfEnd >= 0 ? crlfEnd : html.indexOf('\n  </div>\n</section>', startIdx);
  if (startIdx < 0 || endIdx < 0) throw new Error('gen-site: could not locate the format-wall region in index.html');
  const region = html.slice(startIdx, endIdx);
  if (!region.includes('fmtwall')) throw new Error('gen-site: safety - located region does not look like the format wall');
  // Hand-authored groups survive a regeneration. The wall can carry groups this generator
  // does not own - the "Preview only" group (`data-cat="preview"`, Space-bar-only kinds) was
  // added by hand on 2026-09-10 and the 3.0.1 version bump silently deleted it, because the
  // whole region was replaced from `st2k formats --json`. Every `fmtgroup` whose `data-cat`
  // is not one of ORDER's is carried over verbatim, after the generated ones, and counted in
  // the eyebrow, which is what `sync-version.mjs` in the site repo checks the wall against.
  const foreign = foreignFormatGroups(region);
  const wallEnd = block.lastIndexOf(`${CR}    </div>`);
  if (wallEnd < 0) throw new Error('gen-site: the generated wall has no closing tag to append to');
  const merged = foreign.length
    ? block.slice(0, wallEnd) + CR + foreign.join(CR) + block.slice(wallEnd)
    : block;
  html = html.slice(0, startIdx) + merged + html.slice(endIdx);
  presentCategories += foreign.length;

  // version pills + schema softwareVersion (the only scalars kept current; format count and
  // exact installer bytes are intentionally not hard-coded so they cannot drift).
  // NOTE: these are only the build-time FALLBACK. index.html also ships a small script
  // (the `.js-app-version` updater) that fetches the latest GitHub release tag at load
  // and overrides the pills + softwareVersion at runtime, so a new release does NOT need
  // a site redeploy for the version to update. Keep both: this sets the value shown when
  // the API is unreachable/rate-limited; the script sets it when it isn't.
  // Scoped to the pills ON PURPOSE (see the git history for why a blanket \bv\d+\.\d+\.\d+\b
  // regex is wrong - it also rewrites illustrative version-shaped strings in comments).
  const pillRe = /(class="js-app-version">)v\d+\.\d+\.\d+(<)/g;
  const pills = (html.match(pillRe) || []).length;
  if (pills < 2) throw new Error(`gen-site: expected at least 2 .js-app-version pills, found ${pills} - did the markup change?`);
  html = html.replace(pillRe, `$1v${VERSION}$2`);

  const schemaRe = /("softwareVersion":\s*")\d+\.\d+\.\d+(")/g;
  const schemaHits = (html.match(schemaRe) || []).length;
  if (schemaHits < 1) throw new Error('gen-site: expected at least 1 "softwareVersion" JSON-LD marker, found 0 - did the schema change?');
  html = html.replace(schemaRe, `$1${VERSION}$2`);

  // "hundreds of formats, N categories" - N used to be hand-typed and drifted to 6 while the
  // wall itself already carried 7 (audit F24). Derive it from the same data as the wall.
  const eyebrowRe = /(<span class="eyebrow">hundreds of formats, )\d+( categories<\/span>)/g;
  const eyebrowHits = (html.match(eyebrowRe) || []).length;
  if (eyebrowHits !== 1) throw new Error(`gen-site: expected exactly 1 "N categories" eyebrow marker, found ${eyebrowHits} - did the markup change?`);
  html = html.replace(eyebrowRe, `$1${presentCategories}$2`);

  return { html, changed: html !== before };
}

// ---- self-test: proves the contract without needing a real st2k.exe --------
export function fixtureHtml({ pills = 2, schema = 1, eyebrow = '6', withBarRegion = true } = {}) {
  const pillTags = Array.from({ length: pills }, () => '<span class="js-app-version">v1.0.0</span>').join('\n');
  const schemaTags = Array.from({ length: schema }, () => '"softwareVersion": "1.0.0",').join('\n');
  const barRegion = withBarRegion
    ? '    <div class="bar reveal" role="img" aria-label="x">\r\n      <span></span>\r\n    </div>\r\n    <div class="fmtwall reveal">\r\n      OLD\r\n    </div>\r\n  </div>\r\n</section>'
    : '  </div>\r\n</section>';
  return (
    `<html><body>\n` +
    `<span class="eyebrow">hundreds of formats, ${eyebrow} categories</span>\n` +
    `${pillTags}\n${schemaTags}\n` +
    `<section class="sec coverage" id="formats">\r\n  <div class="wrap">\r\n${barRegion}\n` +
    `</body></html>`
  );
}

// ---- entry point -----------------------------------------------------------
function main(argv) {
  const CHECK = argv.includes('--check');
  const SELF_TEST = argv.includes('--self-test');
  const { SITE_ARG, ST2K_ARG } = parseArgs(argv);

  if (SELF_TEST) {
    // The assertions themselves live in the sibling *.test.mjs (so this churn-hot file has a
    // co-located test a checker can see). --self-test keeps its old contract: run them under
    // Node's built-in test runner and exit with its status.
    const r = spawnSync(process.execPath, ['--test', TEST_FILE], { stdio: 'inherit' });
    process.exit(r.status === null ? 1 : r.status);
  }

  // ---- real run: resolve inputs, validate, write (or --check) --------------
  const SITE = SITE_ARG ? path.resolve(SITE_ARG) : path.join(ROOT, 'site', 'index.html');
  const ST2K = findSt2k(ST2K_ARG);
  const formats = JSON.parse(execFileSync(ST2K, ['formats', '--json'], { encoding: 'utf8' }));
  assertCapabilityFields(formats, ST2K);

  const VERSION = readCargoVersion(ROOT);
  const exeVersion = getExeVersion(ST2K);
  assertVersionMatch(exeVersion, VERSION, ST2K);

  // Line endings are DETECTED from the page, not assumed. This used to be a hard-coded
  // '\r\n', and both the generated block and the end-of-region search below depended on it -
  // so once the deployed index.html came back as LF-only (which is what a `git pull` of the
  // deploy repo hands you), the region search found nothing and the whole script died with
  // "could not locate the format-wall region". The format wall then silently stopped tracking
  // `st2k formats`, which is the one thing this file exists to prevent.
  const EXISTING = fs.readFileSync(SITE, 'utf8');
  const CR = EXISTING.includes('\r\n') ? '\r\n' : '\n';

  const { block, by, TOTAL, presentCategories } = buildFormatWall(formats, CR);
  const { html, changed } = applyAll(EXISTING, { block, VERSION, presentCategories });

  if (CHECK) {
    console.log(`gen-site --check: st2k=${ST2K} (v${exeVersion} matches Cargo.toml v${VERSION})`);
    console.log(`  site=${SITE}`);
    console.log(`  formats=${TOTAL} categories_present=${presentCategories}  ` + ORDER.map(o => o[1] + '=' + (by[o[1]] || []).length).join(' '));
    console.log(changed ? '  WOULD be updated (run without --check to write)' : '  already up to date (no change would be made)');
    process.exit(0);
  }

  fs.writeFileSync(SITE, html);
  console.log(`gen-site: st2k=${ST2K} (v${exeVersion} matches Cargo.toml v${VERSION})`);
  console.log(`  site=${SITE}`);
  console.log(`  formats=${TOTAL} categories_present=${presentCategories}  ` + ORDER.map(o => o[1] + '=' + (by[o[1]] || []).length).join(' '));
  console.log(`  version=v${VERSION}`);
  console.log(changed ? '  index.html updated' : '  index.html already up to date (no change)');
}

if (process.argv[1] && path.resolve(process.argv[1]) === THIS_FILE) main(process.argv.slice(2));
