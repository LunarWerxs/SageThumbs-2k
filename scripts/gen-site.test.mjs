// Co-located tests for scripts/gen-site.mjs, run by `node --test` (which is also what the
// script's own `--self-test` flag now invokes). These drive the pure helpers directly and
// never touch the network, a real st2k.exe, or any file outside os.tmpdir() (in fact they
// write no files at all).
import test from 'node:test';
import assert from 'node:assert/strict';

import {
  parseArgs,
  esc,
  sourceSentence,
  codecSentences,
  codecSentence,
  capabilityParts,
  capabilitySentence,
  capabilityParagraphs,
  assertCapabilityFields,
  assertVersionMatch,
  buildFormatWall,
  applyAll,
  fixtureHtml,
} from './gen-site.mjs';

const PNG = { ext: 'png', category: 'Image', description: 'PNG', source: 'full_decode', convertible: true, preview_listing: false, os_codec: null };

test('parseArgs honours the executable argument, --site, and paths with spaces', () => {
  assert.equal(parseArgs(['x.exe']).ST2K_ARG, 'x.exe');
  assert.equal(parseArgs(['x.exe', '--check']).ST2K_ARG, 'x.exe');
  const both = parseArgs(['--site', 'p.html', 'x.exe']);
  assert.equal(both.ST2K_ARG, 'x.exe');
  assert.equal(both.SITE_ARG, 'p.html');
  const flagLast = parseArgs(['x.exe', '--site', 'p.html']);
  assert.equal(flagLast.ST2K_ARG, 'x.exe');
  assert.equal(flagLast.SITE_ARG, 'p.html');
  assert.equal(parseArgs(['--check']).ST2K_ARG, undefined);
  // A path with a space (e.g. C:\Program Files\...) must survive both orderings verbatim.
  const spaced = parseArgs(['C:\\Program Files\\SageThumbs2K\\st2k.exe', '--site', 'C:\\my site\\index.html']);
  assert.equal(spaced.ST2K_ARG, 'C:\\Program Files\\SageThumbs2K\\st2k.exe');
  assert.equal(spaced.SITE_ARG, 'C:\\my site\\index.html');
  const spacedLast = parseArgs(['--site', 'C:\\my site\\index.html', 'C:\\Program Files\\SageThumbs2K\\st2k.exe']);
  assert.equal(spacedLast.ST2K_ARG, 'C:\\Program Files\\SageThumbs2K\\st2k.exe');
  assert.equal(spacedLast.SITE_ARG, 'C:\\my site\\index.html');
});

test('a binary without the capability fields is refused, never rendered blank', () => {
  const stale = [{ ext: 'png', category: 'Image', description: 'PNG' }];
  assert.throws(() => assertCapabilityFields(stale, 'old.exe'), /lack the capability fields.*old\.exe predates them/s);
  const fresh = [{ ...PNG }];
  assert.doesNotThrow(() => assertCapabilityFields(fresh, 'new.exe'));
  const halfway = [{ ...fresh[0], os_codec: undefined }];
  assert.throws(() => assertCapabilityFields(halfway, 'x.exe'), /lack the capability fields/);
});

test('applyAll throws when a required page marker is missing', () => {
  const { block, presentCategories } = buildFormatWall([{ ...PNG }], '\r\n');
  const ctx = { block, VERSION: '1.0.0', presentCategories };
  assert.throws(() => applyAll(fixtureHtml({ pills: 0 }), ctx), /js-app-version pills/);
  assert.throws(() => applyAll(fixtureHtml({ schema: 0 }), ctx), /softwareVersion/);
  assert.throws(() => applyAll(fixtureHtml({ withBarRegion: false }), ctx), /could not locate the format-wall region/);
});

test('assertVersionMatch rejects a mismatched build and accepts a match', () => {
  assert.throws(() => assertVersionMatch('2.4.0', '2.5.0', 'fake.exe'), /reports version 2\.4\.0.*says 2\.5\.0/s);
  assertVersionMatch('2.5.0', '2.5.0', 'fake.exe'); // must not throw
});

test('capability sentence names a PARTIAL os_codec dependency by extension', () => {
  const items = [
    { ext: 'png', source: 'full_decode', os_codec: null },
    { ext: 'jxr', source: 'full_decode', os_codec: 'wmphoto' },
    { ext: 'heic', source: 'full_decode', os_codec: 'heif' },
  ];
  const sentence = capabilitySentence(items);
  assert.match(sentence, /^Each thumbnail is a full decode of the image itself\./);
  assert.match(sentence, /\.jxr additionally needs the OS's WIC JPEG XR codec\./);
  // Audit F23: HEIF is the fast route, not a hard dependency - a Full install decodes it
  // through the bundled ImageMagick when Windows has no codec.
  assert.match(sentence, /\.heic additionally uses the OS's WIC HEIF codec when Windows has it; a Full install decodes it through the bundled decoder otherwise\./);
});

test('caption paragraphs each fit the site copy budget (40 words), Image-shaped group', () => {
  // The live Image group on 2026-09-18: 155 full decodes, 58 carried previews, and three
  // PARTIAL codec notes. As one paragraph that is 53 words (73 since the F23 wording that
  // names the bundled fallback); the site's copy-budget check refuses anything over 40,
  // so the generator emits the tier sentence and EACH codec note as its own paragraph.
  const items = [];
  for (let i = 0; i < 155; i++) items.push({ ext: `f${i}`, source: 'full_decode', os_codec: null });
  for (let i = 0; i < 58; i++) items.push({ ext: `p${i}`, source: 'embedded_preview', os_codec: null });
  for (const ext of ['avci', 'heic', 'heics', 'heif', 'heifs', 'hif']) items.push({ ext, source: 'full_decode', os_codec: 'heif' });
  items.push({ ext: 'avif', source: 'full_decode', os_codec: 'av1' });
  for (const ext of ['hdp', 'jxr', 'wdp', 'wmp']) items.push({ ext, source: 'full_decode', os_codec: 'wmphoto' });
  const paras = capabilityParagraphs(items);
  const words = (t) => t.trim().split(/\s+/).length;
  assert.strictEqual(paras.length, 4, `one tier sentence plus three codec notes, got ${paras.length}`);
  for (const p of paras) assert.ok(words(p) <= 40, `a caption paragraph is ${words(p)} words: ${p}`);
  assert.ok(words(paras.join(' ')) > 40, 'the split is load-bearing: as one paragraph this exceeds the budget');
});

test('mixed-source group states counts, not a blanket claim', () => {
  // Audit E03 #1: Image is exactly this shape now - most formats a full decode, a fixed
  // subset (PSD/EPS/APK/Blender/...) riding a carried preview instead.
  const items = [
    { ext: 'png', source: 'full_decode', os_codec: null },
    { ext: 'jpg', source: 'full_decode', os_codec: null },
    { ext: 'psd', source: 'embedded_preview', os_codec: null },
  ];
  const sentence = capabilitySentence(items);
  assert.match(sentence, /2 are a full decode of the image itself/);
  assert.match(sentence, /1 rides? the file's own embedded\/carried preview/);
  assert.doesNotMatch(sentence, /^Each thumbnail/);
});

test('an unknown capability source throws rather than rendering blank', () => {
  assert.throws(
    () => capabilitySentence([{ ext: 'zzz', source: 'made_up_source', os_codec: null }]),
    /unknown capability source "made_up_source"/);
  // ...including when it is only ONE source inside an otherwise-known mixed group.
  assert.throws(
    () => capabilitySentence([
      { ext: 'png', source: 'full_decode', os_codec: null },
      { ext: 'zzz', source: 'made_up_source', os_codec: null },
    ]),
    /unknown capability source "made_up_source"/);
});

test('os_codec handling: an unknown token throws, av1 renders its own sentence', () => {
  assert.throws(
    () => capabilitySentence([{ ext: 'zzz', source: 'full_decode', os_codec: 'made_up_codec' }]),
    /unknown os_codec "made_up_codec"/);
  const sentence = capabilitySentence([{ ext: 'avif', source: 'full_decode', os_codec: 'av1' }]);
  assert.match(sentence, /Every format here uses the AV1 Video Extension when Windows has it; a Full install decodes it through the bundled decoder otherwise\./);
});

test('an unknown category fails the wall instead of being silently dropped', () => {
  assert.throws(
    () => buildFormatWall([{ category: 'Nonsense', ext: 'zzz', description: 'made up' }], '\r\n'),
    /missing from ORDER: Nonsense/);
});

test('a hand-authored fmtgroup the generator does not own survives regeneration and is counted', () => {
  const one = [{ ...PNG }];
  const { block, presentCategories } = buildFormatWall(one, '\r\n');
  const preview = '      <div class="fmtgroup reveal" data-cat="preview">\r\n        <h3 class="fgh">Preview only</h3>\r\n        <div class="fgdesc" hidden><ul><li>.eml</li></ul></div>\r\n      </div>';
  const html = fixtureHtml().replace('      OLD\r\n', preview + '\r\n');
  const { html: out } = applyAll(html, { block, VERSION: '1.0.0', presentCategories });
  assert.ok(out.includes('data-cat="preview"'), 'the foreign group was dropped');
  assert.ok(out.includes('<li>.eml</li>'), 'the foreign group lost its nested content');
  assert.ok(out.indexOf('data-cat="img"') < out.indexOf('data-cat="preview"'), 'foreign groups follow the generated ones');
  assert.match(out, /hundreds of formats, 2 categories/, 'the eyebrow counts the preserved group');
  const { html: again } = applyAll(out, { block, VERSION: '1.0.0', presentCategories });
  assert.equal((again.match(/data-cat="preview"/g) || []).length, 1, 'a second run must not duplicate it');
});

test('a healthy run passes and is idempotent', () => {
  const formats = [
    { category: 'Image', ext: 'png', description: 'Portable Network Graphics', source: 'full_decode', convertible: true, preview_listing: false, os_codec: null },
    { category: 'Image', ext: 'jpg', description: 'JPEG', source: 'full_decode', convertible: true, preview_listing: false, os_codec: null },
    { category: 'Document', ext: 'pdf', description: 'Portable Document Format', source: 'cover_or_first_page', convertible: true, preview_listing: false, os_codec: null },
    { category: 'Camera RAW', ext: 'cr2', description: 'Canon RAW', source: 'embedded_preview', convertible: true, preview_listing: false, os_codec: null },
    { category: 'Video', ext: 'mp4', description: 'MPEG-4 Video', source: 'video_frame', convertible: true, preview_listing: false, os_codec: 'media_foundation' },
    { category: 'Audio', ext: 'mp3', description: 'MPEG Audio', source: 'cover_art', convertible: true, preview_listing: false, os_codec: null },
    { category: 'Ebook', ext: 'epub', description: 'EPUB', source: 'cover_or_first_page', convertible: true, preview_listing: false, os_codec: null },
    { category: 'Archive', ext: 'zip', description: 'Zip archive', source: 'contained_images', convertible: false, preview_listing: true, os_codec: null },
  ];
  const html0 = fixtureHtml({ eyebrow: '6' });
  const wall1 = buildFormatWall(formats, '\r\n');
  assert.equal(wall1.presentCategories, 7);
  const run1 = applyAll(html0, { block: wall1.block, VERSION: '9.9.9', presentCategories: wall1.presentCategories });
  assert.ok(run1.changed, 'first run over stale fixture should change something');
  assert.match(run1.html, /hundreds of formats, 7 categories/);
  assert.match(run1.html, /v9\.9\.9/);
  // Audit E03: the per-category capability sentence is DERIVED from the data, not
  // hand-typed - the video group must name its Media Foundation dependency. The codec
  // requirement is its own caption paragraph, so no caption outgrows the copy budget's 40.
  assert.match(run1.html, /Each thumbnail is a representative frame grabbed from the video\.<\/p>\s*<p class="fgcap">Every format here needs the OS Media Foundation codecs\.<\/p>/);
  assert.match(run1.html, /Each thumbnail shows the images found inside the archive, not one photo\./);

  const wall2 = buildFormatWall(formats, '\r\n');
  const run2 = applyAll(run1.html, { block: wall2.block, VERSION: '9.9.9', presentCategories: wall2.presentCategories });
  assert.equal(run2.html, run1.html, 'a second run over the already-updated file must be a no-op');
});

test('pure capability helpers handle an empty list and a single-item boundary', () => {
  // buildFormatWall guards empty groups (it never builds a caption for one), so an empty
  // list reaching the sentence builders throws rather than emitting a blank caption.
  assert.throws(() => sourceSentence([]), /unknown capability source "undefined"/);
  assert.throws(() => capabilitySentence([]), /unknown capability source "undefined"/);
  assert.deepEqual(codecSentences([]), []);
  // Boundary: a one-item group is "Every format here ..." and exactly one paragraph.
  const one = [{ ext: 'jxr', source: 'full_decode', os_codec: 'wmphoto' }];
  assert.deepEqual(capabilityParagraphs(one), ['Each thumbnail is a full decode of the image itself.', "Every format here needs the OS's WIC JPEG XR codec."]);
  assert.deepEqual(capabilityParts(one), { source: 'Each thumbnail is a full decode of the image itself.', codecs: ["Every format here needs the OS's WIC JPEG XR codec."] });
  assert.equal(codecSentence('wmphoto', ['jxr'], 1), "Every format here needs the OS's WIC JPEG XR codec.");
});

test('descriptions are HTML-escaped before they reach the wall', () => {
  const formats = [{ ...PNG, description: '<b>"x" & <script>' }];
  const { block } = buildFormatWall(formats, '\n');
  assert.match(block, /title="&lt;b&gt;&quot;x&quot; &amp; &lt;script&gt;"/);
  assert.ok(!block.includes('<script>'), 'raw markup leaked into the wall');
  assert.equal(esc('<a href="x">&'), '&lt;a href=&quot;x&quot;&gt;&amp;');
});
