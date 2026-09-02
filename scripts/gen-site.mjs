// Regenerate the data-driven parts of site/index.html from SOURCE OF TRUTH, so the site can
// never drift from the shipping software again. It rewrites:
//   * the format wall  (bar + per-category chip lists) from `st2k formats --json`
//   * the version pills (softwareVersion, download meta, footer) from Cargo.toml
// The prose deliberately avoids hard-coded format counts and installer byte claims.
// Only the auto-generated wall below carries exact, self-updating category counts;
// release notes carry each published installer's exact bytes and digest.
//
// Run before deploying the site (the site lives at sagethumbs2k.github.io):
//   node scripts/gen-site.mjs [path\to\st2k.exe]
// st2k.exe resolution order: arg -> $ST2K -> resolved cargo target dir -> installed -> PATH.
// The resolved target dir follows scripts/_targetdir.ps1's own order (CARGO_TARGET_DIR env,
// then a `target-dir` redirect in .cargo/config.toml, then the default ./target next to the
// workspace), never a hardcoded dev-machine path.
// Idempotent: running it twice is a no-op. CRLF-preserving.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SITE = path.join(ROOT, 'site', 'index.html');

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
function findSt2k() {
  const cands = [
    process.argv[2],
    process.env.ST2K,
    path.join(resolveTargetDir(), 'release', 'st2k.exe'),
    'C:/Program Files/SageThumbs2K/st2k.exe',
    'st2k',
  ].filter(Boolean);
  for (const c of cands) {
    try { execFileSync(c, ['formats', '--json'], { stdio: 'ignore' }); return c; } catch {}
  }
  throw new Error('st2k.exe not found. Build it (cargo build --release) or pass its path as arg 1.');
}
const ST2K = findSt2k();
const formats = JSON.parse(execFileSync(ST2K, ['formats', '--json'], { encoding: 'utf8' }));
const TOTAL = formats.length;

// ---- version (Cargo.toml) --------------------------------------------------
const cargo = fs.readFileSync(path.join(ROOT, 'Cargo.toml'), 'utf8');
const VERSION = (cargo.match(/^version\s*=\s*"([^"]+)"/m) || [])[1];
if (!VERSION) throw new Error('could not read version from Cargo.toml');

// ---- build the format-wall block (bar + fmtwall) ---------------------------
const esc = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
// EVERY category `st2k formats --json` can emit must appear here. A category missing
// from this list is silently dropped from the wall AND from the bar, so the bar stops
// summing to 100% and the site under-reports what ships (that is exactly what happened
// to `Archive` between its release and 2026-08-01). The assertion below enforces it.
const ORDER = [
  ['img', 'Image', 'Image', '#4d9cff'], ['doc', 'Document', 'Document', '#ef8b5a'],
  ['raw', 'Camera RAW', 'Camera RAW', '#b48bff'], ['vid', 'Video', 'Video', '#f06ab0'],
  ['aud', 'Audio', 'Audio', '#38d39f'], ['ebk', 'Ebook', 'Ebook &amp; comics', '#f2c14e'],
  ['arc', 'Archive', 'Archive', '#7d8ca3'],
];
const ARIA = { img: 'image', doc: 'document', raw: 'camera raw', vid: 'video', aud: 'audio', ebk: 'ebook and comics', arc: 'archive' };
// Line endings are DETECTED from the page, not assumed. This used to be a hard-coded
// '\r\n', and both the generated block and the end-of-region search below depended on it -
// so once the deployed index.html came back as LF-only (which is what a `git pull` of the
// deploy repo hands you), the region search found nothing and the whole script died with
// "could not locate the format-wall region". The format wall then silently stopped tracking
// `st2k formats`, which is the one thing this file exists to prevent.
const EXISTING = fs.readFileSync(SITE, 'utf8');
const CR = EXISTING.includes('\r\n') ? '\r\n' : '\n';
const by = {};
for (const x of formats) (by[x.category] = by[x.category] || []).push(x);

// Fail loudly rather than quietly shipping a wall that omits a whole category.
const missing = Object.keys(by).filter(c => !ORDER.some(([, cat]) => cat === c));
if (missing.length) {
  throw new Error(
    `gen-site: ${missing.length} format categor${missing.length === 1 ? 'y is' : 'ies are'} ` +
    `missing from ORDER: ${missing.join(', ')}. Add each one (with a swatch colour and an ` +
    `ARIA label) plus a matching .fmtgroup[data-cat="..."] rule in site/index.html.`);
}
const aria = [], spans = [], groups = [];
for (const [dc, cat, label, color] of ORDER) {
  const items = (by[cat] || []).slice().sort((a, b) => a.ext.localeCompare(b.ext));
  const n = items.length, pct = (n / TOTAL * 100).toFixed(1);
  aria.push(n + ' ' + ARIA[dc]);
  spans.push(`      <span style="width:${pct}%;background:${color}"></span>`);
  const chips = items.map(x => `<span class="fc" title="${esc(x.description)}">.${x.ext}</span>`).join(' ');
  groups.push(`      <div class="fmtgroup reveal" data-cat="${dc}">${CR}        <h3 class="fgh"><span class="sw"></span>${label} <span class="cnt">${n}</span></h3>${CR}        <div class="fgchips">${chips}</div>${CR}      </div>`);
}
const block = `    <div class="bar reveal" role="img" aria-label="Format coverage by category: ${aria.join(', ')}">${CR}${spans.join(CR)}${CR}    </div>${CR}    <div class="fmtwall reveal">${CR}${groups.join(CR)}${CR}    </div>`;

// ---- splice + scalar syncs -------------------------------------------------
let html = EXISTING;
const before = html;
const startIdx = html.indexOf('    <div class="bar reveal"');
const endIdx = html.indexOf(CR + '  </div>' + CR + '</section>', startIdx);
if (startIdx < 0 || endIdx < 0) throw new Error('could not locate the format-wall region in site/index.html');
const region = html.slice(startIdx, endIdx);
if (!region.includes('fmtwall')) throw new Error('safety: located region does not look like the format wall');
html = html.slice(0, startIdx) + block + html.slice(endIdx);

// version pills + schema softwareVersion (the only scalar kept current; format count and
// exact installer bytes are intentionally not hard-coded so they cannot drift).
// NOTE: these are only the build-time FALLBACK. index.html also ships a small script
// (the `.js-app-version` updater) that fetches the latest GitHub release tag at load
// and overrides the pills + softwareVersion at runtime, so a new release does NOT need
// a site redeploy for the version to update. Keep both: this sets the value shown when
// the API is unreachable/rate-limited; the script sets it when it isn't.
// Scoped to the pills ON PURPOSE. This used to be a blanket /\bv\d+\.\d+\.\d+\b/g,
// which rewrote EVERY version-shaped string in the file - including the illustrative
// `// "v1.2.3" -> "1.2.3"` comment in the updater script below, whose left half kept
// getting stamped with the release of the day while the right half (no `v` prefix)
// did not, leaving a comment that contradicted itself. Any "since vX.Y.Z" note, alt
// text or versioned URL added later would have been silently rewritten the same way.
const pillRe = /(class="js-app-version">)v\d+\.\d+\.\d+(<)/g;
const pills = (html.match(pillRe) || []).length;
if (pills < 2) throw new Error(`expected at least 2 .js-app-version pills, found ${pills} - did the markup change?`);
html = html.replace(pillRe, `$1v${VERSION}$2`);
html = html.replace(/("softwareVersion":\s*")\d+\.\d+\.\d+(")/g, `$1${VERSION}$2`);

fs.writeFileSync(SITE, html);
console.log(`gen-site: st2k=${ST2K}`);
console.log(`  formats=${TOTAL}  ` + ORDER.map(o => o[1] + '=' + (by[o[1]] || []).length).join(' '));
console.log(`  version=v${VERSION}`);
console.log(html === before ? '  site/index.html already up to date (no change)' : '  site/index.html updated');
