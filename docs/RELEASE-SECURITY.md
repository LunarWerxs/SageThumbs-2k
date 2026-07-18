# Release security: antivirus false positives

## Short version

**Every binary SageThumbs 2K ships is clean on VirusTotal — 0 detections out of ~69, all three
of them.** Only the Inno Setup installer that wraps them is flagged, by 2–3 of ~70 engines, and
every one of those is a heuristic/ML verdict rather than a signature match.

The detections are an artifact of wrapping unsigned binaries in a compressed self-extractor.
They are not a property of the software, and no code change is warranted.

Every release is now scanned **before** it is published (see *The gate* below). Releases up to
and including v1.2.0 were not, which is why ESET's verdict on 1.1.0/1.1.1 first surfaced on
SourceForge's listing instead of in our own pipeline.

## What has actually been detected

Full history, every installer still in `dist/`, looked up on VirusTotal by hash:

| Build | Built | VT | ESET | Others |
|---|---|---|---|---|
| 0.8.0 | 2026-07-07 | 2/68 | clean | APEX, Skyhigh |
| 0.10.0 | 2026-07-13 | 2/69 | clean | APEX, Skyhigh |
| 1.0.0 | 2026-07-14 | 2/70 | clean | APEX, Skyhigh |
| 1.0.1 | 2026-07-14 | 2/69 | clean | APEX, Skyhigh |
| **1.1.0** | 2026-07-17 | 3/69 | **`Generik.NJDPIFC`** | APEX, Skyhigh |
| **1.1.1** | 2026-07-17 | 3/69 | **`Generik.MMSQLBT`** | APEX, Skyhigh |
| 1.2.0 | 2026-07-18 | 2/70 | clean *(so far — see below)* | APEX, Skyhigh |

**That table is misleading, and the trap is worth naming.** Those are each build's *stored*
verdict from whenever VirusTotal last analysed it — mostly the day it was released. Comparing
them looks like a timeline of our software. It is not; it is a timeline of *when each file
happened to be scanned*.

### The decisive test: re-scan OLD builds with TODAY's engines

| Build | Stored verdict | Re-analysed 2026-07-18 |
|---|---|---|
| 1.0.0 | clean (scanned Jul 17) | **`Generik.CBCUAMQ`** — flipped to flagged |
| 1.0.1 | clean (scanned Jul 14) | clean |
| 1.1.1 | `Generik.MMSQLBT` | `Generik.MMSQLBT` (unchanged) |

**1.0.0 predates the 1.1.0 selection feature entirely, was clean yesterday, and flags today on
identical bytes.** Nothing about the file changed. ESET's model did.

That single result disposes of every "what did we change at 1.1.0" theory, including two this
document previously advanced. Across builds of one product ESET has now issued **four
different cluster IDs** — `CBCUAMQ`, `NJDPIFC`, `MMSQLBT`, and clean — with no correspondence
to anything in the source.

### What is actually happening

ESET's `Generik.*` is a generic ML/heuristic bucket, not a signature. Malware authors also
package payloads with Inno Setup, so vendors periodically ship heuristics matching the Inno
**stub** itself — which is why this catches legitimate vendors and why it fires on some builds
and not others with no meaningful change. It is a lottery over the compressed installer image,
re-rolled whenever the vendor updates its model.

Corroborating evidence that this is an industry-wide Inno problem, not ours:

- Inno Setup's own community group carries recurring threads
  ([1](https://groups.google.com/g/innosetup/c/w2weZ4afFqs),
  [2](https://groups.google.com/g/innosetup/c/58LUdjrJUUI),
  [3](https://groups.google.com/g/innosetup/c/lvsb2vWhklk)) — enough that a moderator has a
  standing "contact your AV vendor, not us" reply.
- Microsoft's own Q&A: [False Positives using Inno Setup](https://learn.microsoft.com/en-us/answers/questions/2736482/false-positives-using-inno-setup) — Defender flags Inno output too.
- [node-innosetup-compiler#10](https://github.com/felicienfrancois/node-innosetup-compiler/issues/10):
  Defender's detection depended on the output **filename** and vanished when renamed, with no
  code change. Arbitrary to the point of absurdity.

### Do not read a clean scan as "fixed"

A clean result means "not flagged in this roll," not "exonerated." 1.2.0 may well be flagged
next week, and 1.1.x may go clean. Re-check a few days after each release.

## Why these are false positives (the specific evidence, not a shrug)

- **No engine returns a named malware family.** ESET's `Generik.*` is its generic ML bucket;
  Skyhigh's `BehavesLike…` prefix is explicitly heuristic; APEX and Bkav are ML engines with
  well-known false-positive rates on VirusTotal.
- **A build flipped clean → flagged with unchanged bytes** (1.0.0, above). Nothing about the
  file changed, so nothing about the file is what is being detected.
- **Four different cluster IDs across builds of one product.** Signature detections are stable;
  ML clusterings are not.
- **Scan the components and everything is clean:**

| Component | VirusTotal |
|---|---|
| `SageThumbs2K.exe` (options dialog + Quick preview) | **0/68 clean** |
| `sagethumbs2k.dll` (the shell extension itself) | **0/69 clean** |
| `st2k.exe` (the CLI / MCP server) | **0/69 clean** |
| `SageThumbs2K-Setup-*.exe` (the Inno wrapper) | 2–3 flagged |
| the same binaries in a plain zip, no wrapper (tested, NOT shipped) | 1 flagged (Bkav) |

**Not one engine objects to any code this project ships.** Detections appear only once the
binaries are wrapped, and they follow the wrapper, not the contents.

## Two dead-end theories, recorded so they are not re-derived

Both of these were advanced in earlier drafts of this document and both are wrong. They are
kept because each is the obvious first guess.

**"It is because we are unsigned."** Unsigned is the standing background condition, but it
cannot explain a detection that appears on some builds and not others — the project has been
unsigned since day one.

**"1.1.0 added text selection, which looks like an infostealer."** 1.1.0 was a single commit
introducing `GetKeyState`, `SetCapture` and `set_clipboard` — *poll keys → capture input →
extract displayed text → write to clipboard* genuinely does read like a credential stealer.
Refuted twice over: the binary containing every line of that code scans **0/68**, and 1.0.0,
which predates the feature entirely, now flags anyway.

## The gate

`scripts/release.ps1` step **4b** runs `push_to_vt.py --gate` on the exact artifact about to be
published, after the installer is built and before `gh release create`.

It fails the release when:

- any **tier-1** engine flags the build (`Microsoft`, `Kaspersky`, `BitDefender`, `Symantec`,
  `Sophos`, `TrendMicro`, `McAfee`, `Avast`, `AVG`, `DrWeb`, `F-Secure`, `GData`,
  `Malwarebytes`), **or**
- total detections exceed **6**.

It deliberately does **not** fail on the routine 2–3 heuristic hits. A gate that fails on every
release is a gate everyone learns to click past, which is worse than no gate. The threshold
exists to catch a *change* — a real compromise, or a build change that makes us look far worse
than baseline.

It is skipped with a warning (not an error) when `.env` or Python is unavailable: tooling
absence must not block a release, only a real verdict should.

Run it by hand any time:

```
python push_to_vt.py dist/SageThumbs2K-Setup-<ver>.exe --gate
```

## What does NOT work (so nobody burns a day on it)

Researched against the Inno Setup community group, Microsoft docs, and real test repos. Most
of the popular advice is cargo-cult:

| Suggested "fix" | Verdict |
|---|---|
| Change `Compression=` (lzma2 → zip → none) | **No evidence.** Nothing links Inno's compression choice to heuristic detection. |
| `SolidCompression=no` | **No evidence.** The only real test data ([teeks99/inno-test](https://github.com/teeks99/inno-test)) measures build time and size, not detections. |
| Rich `VersionInfo` metadata | **Unverified.** Already set here regardless — it is cheap and sensible. |
| Avoid the name `Setup.exe` | **No general evidence**, though one documented Defender case turned on filename alone. Ours is already versioned. |
| Upgrade Inno Setup | **Weakly evidenced.** A specific version's stub can get "poisoned" when malware campaigns use it; moving off it plausibly helps, but it is not immunity. |
| Wait for it to fade | **Wrong direction for this.** Microsoft documents SmartScreen warnings fading with prevalence, but that is not the same mechanism, and 1.0.0 got *worse* with age. |

The honest summary: no lever here is evidenced to work. Changing our code or packaging to
appease a dice roll would be chasing noise.

## Fixing it properly

### Code signing is OFF THE TABLE — do not propose it

Settled owner decision (2026-07-18), not an open trade-off. Do not raise it, do not list it
under "next steps," and do not resurrect it in a later session because the context looks new.
It is recorded here only because it is the first thing anyone researching AV false positives
will reach for, and re-litigating it wastes everyone's time.

(This does not touch the **self-signed** cert for the MSIX sparse package — Windows will not
load an unsigned sparse package at all, so that one is a technical requirement and stays.)

### What is left

1. **File vendor false-positive reports** (below). Free, and the case here is unusually strong.
2. **Accept and document.** All shipped binaries are 0/69; only the wrapper is flagged, and
   flagging is a per-build dice roll re-rolled on the vendor's schedule. Pointing users at this
   document is a legitimate answer, and is the current position.

### Changing installer format does NOT fix this

Assessed properly before anyone spends days on it:

| Format | Avoids the packed-stub class? | Verdict |
|---|---|---|
| NSIS | No | [NSIS's own docs](https://nsis.sourceforge.io/NSIS_False_Positives) say vendors signature the stub itself |
| 7-Zip SFX | No | It *is* a decompressing PE stub |
| Squirrel / Velopack | No | Long history of `HEUR:Trojan.Win32.Generic` flags |
| MSIX only | Yes | **Dead end** — requires a trusted signature to install at all |
| MSI / WiX | Structurally, probably | **Unevidenced.** No before/after case study exists; [Tauri #4749](https://github.com/tauri-apps/tauri/issues/4749) had an unsigned MSI flagged *more* than its EXEs, and [Defender flags MSIs too](https://learn.microsoft.com/en-us/answers/questions/746120/msi-is-detected-as-a-virus-by-windows-defender). SmartScreen documents **no** MSI-vs-EXE distinction. Costs 2–4 days of WiX work, and the MSIX sparse package registers per-user, which fights a per-machine MSI. |
| Portable zip (no installer) | **Yes** | The only format with no stub at all — and tested: it drops from 2 detections to 1, trading the packed-stub hits for Bkav instead. Not zero, so **not shipped**; the installer remains the only distribution. |

The conclusion to hold onto: AV false positives are a tax on unsigned distribution, not a
property of Inno Setup. No format choice removes them.

### Report the false positives

Vendors act on these and it is free:

- ESET: <https://support.eset.com/en/kb141-submit-a-virus-website-or-potential-false-positive-sample-to-eset-lab> (or email `samples@eset.com`, subject prefixed `False positive`)
- Skyhigh/Trellix: <https://www.trellix.com/support/submit-sample/>
- Microsoft (if it ever flags us — it does not currently): <https://www.microsoft.com/en-us/wdsi/filesubmission>

Include the VirusTotal permalink, the download URL, and that the project is open-source at
<https://github.com/LunarWerxs/SageThumbs-2k>.

**Note for whoever handles SourceForge:** its listing reflects ESET's verdict. Getting the ESET
false positive retracted is what clears it; there is no separate SourceForge appeal to file.
