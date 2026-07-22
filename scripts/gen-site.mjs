// Regenerate the data-driven parts of site/index.html from SOURCE OF TRUTH, so the site can
// never drift from the shipping software again. It rewrites:
//   * the format wall  (bar + per-category chip lists) from `st2k formats --json`
//   * the version pills (softwareVersion, download meta, footer) from Cargo.toml
// The format COUNT and install SIZE are deliberately vague in the copy ("hundreds of
// formats", "tiny install") so they never go stale as the app grows; only the
// auto-generated wall below carries exact, self-updating per-category counts.
//
// Run before deploying the site (the site lives at sagethumbs2k.github.io):
//   node scripts/gen-site.mjs [path\to\st2k.exe]
// st2k.exe resolution order: arg -> $ST2K -> D:\st2k-target\release -> installed -> PATH.
// Idempotent: running it twice is a no-op. CRLF-preserving.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SITE = path.join(ROOT, 'site', 'index.html');

// ---- locate st2k.exe -------------------------------------------------------
function findSt2k() {
  const cands = [
    process.argv[2],
    process.env.ST2K,
    'D:/st2k-target/release/st2k.exe',
    path.join(ROOT, 'target', 'release', 'st2k.exe'),
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

// ---- version (Cargo.toml) + installer size (dist) --------------------------
const cargo = fs.readFileSync(path.join(ROOT, 'Cargo.toml'), 'utf8');
const VERSION = (cargo.match(/^version\s*=\s*"([^"]+)"/m) || [])[1];
if (!VERSION) throw new Error('could not read version from Cargo.toml');

// ---- build the format-wall block (bar + fmtwall) ---------------------------
const esc = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
const ORDER = [
  ['img', 'Image', 'Image', '#4d9cff'], ['doc', 'Document', 'Document', '#ef8b5a'],
  ['raw', 'Camera RAW', 'Camera RAW', '#b48bff'], ['vid', 'Video', 'Video', '#f06ab0'],
  ['aud', 'Audio', 'Audio', '#38d39f'], ['ebk', 'Ebook', 'Ebook &amp; comics', '#f2c14e'],
];
const ARIA = { img: 'image', doc: 'document', raw: 'camera raw', vid: 'video', aud: 'audio', ebk: 'ebook and comics' };
const CR = '\r\n';
const by = {};
for (const x of formats) (by[x.category] = by[x.category] || []).push(x);
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
let html = fs.readFileSync(SITE, 'utf8');
const before = html;
const startIdx = html.indexOf('    <div class="bar reveal"');
const endIdx = html.indexOf('\r\n  </div>\r\n</section>', startIdx);
if (startIdx < 0 || endIdx < 0) throw new Error('could not locate the format-wall region in site/index.html');
const region = html.slice(startIdx, endIdx);
if (!region.includes('fmtwall')) throw new Error('safety: located region does not look like the format wall');
html = html.slice(0, startIdx) + block + html.slice(endIdx);

// version pills + schema softwareVersion (the only scalar kept current; count + size
// are intentionally vague in the copy so they never drift as the app grows).
// NOTE: these are only the build-time FALLBACK. index.html also ships a small script
// (the `.js-app-version` updater) that fetches the latest GitHub release tag at load
// and overrides the pills + softwareVersion at runtime, so a new release does NOT need
// a site redeploy for the version to update. Keep both: this sets the value shown when
// the API is unreachable/rate-limited; the script sets it when it isn't.
html = html.replace(/\bv\d+\.\d+\.\d+\b/g, 'v' + VERSION);
html = html.replace(/("softwareVersion":\s*")\d+\.\d+\.\d+(")/g, `$1${VERSION}$2`);

fs.writeFileSync(SITE, html);
console.log(`gen-site: st2k=${ST2K}`);
console.log(`  formats=${TOTAL}  ` + ORDER.map(o => o[1] + '=' + (by[o[1]] || []).length).join(' '));
console.log(`  version=v${VERSION}`);
console.log(html === before ? '  site/index.html already up to date (no change)' : '  site/index.html updated');
