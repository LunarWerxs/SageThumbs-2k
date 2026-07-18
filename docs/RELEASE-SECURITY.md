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
  Skyhigh's `BehavesLike…` prefix is explicitly heuristic; APEX is a pure-ML engine with a
  well-known false-positive rate on VirusTotal.
- **ESET assigned two DIFFERENT cluster IDs to consecutive builds of the same program**
  (`NJDPIFC` → `MMSQLBT`). Signature detections are stable across builds; ML cluster IDs are
  not. That instability is itself the tell.
- **`ObfuscatedPoly` describes Inno Setup, not us.** An Inno installer is an LZMA-compressed
  self-extractor by construction, which is exactly what "packed/obfuscated" heuristics look for.
- The behaviour profile is inherently malware-shaped: writes to Program Files, `regsvr32`s a
  DLL, registers an Appx package, adds an autostart entry. That is what a shell extension
  installer *does*.

## Root cause

Two separate things are going on, and conflating them leads to the wrong fix.

### The baseline (APEX + Skyhigh, every release since 0.8.0)

Unsigned Inno Setup installer, low prevalence, LZMA self-extractor, bundled unsigned
third-party DLLs. Constant, harmless, and it has never moved. Code-signing would clear this.

### The ESET detection (new at 1.1.0) — an artifact of the INSTALLER, not of our code

The decisive test: scan the shipped binaries individually rather than the installer.

| Component | VirusTotal |
|---|---|
| `SageThumbs2K.exe` (options dialog + Quick preview — contains ALL the selection/clipboard code) | **0/68 clean** |
| `sagethumbs2k.dll` (the shell extension itself) | **0/69 clean** |
| `st2k.exe` (the CLI / MCP server) | **0/69 clean** |
| `SageThumbs2K-Setup-*.exe` (the Inno wrapper around them) | 2–3 flagged |

**Not one engine objects to any code this project ships.** Every detection, ESET's included,
exists only once the binaries are wrapped in the Inno Setup self-extractor.

That kills the intuitive theory, which is worth recording so nobody re-derives it: 1.1.0 was a
single commit adding text selection, which introduced `GetKeyState`, `SetCapture` and
`set_clipboard` — *poll keys → capture input → extract displayed text → write to clipboard*,
which reads like an infostealer. Plausible, and wrong: the binary containing all of that code
is clean on its own. The feature is not the trigger.

What is actually happening: ESET's `Generik.*` bucket is an ML verdict computed over the
**LZMA-compressed installer image**. Those bytes are re-derived from scratch on every build, so
their statistical fingerprint shifts unpredictably whenever the payload changes at all. That
explains every observation:

- it appeared at 1.1.0 with no corresponding change in what we bundle (size moved 9.28 → 9.29 MB);
- consecutive builds landed in **different clusters** (`NJDPIFC` then `MMSQLBT`) — signature
  matches are stable, ML clusterings are not;
- re-scanning the same 1.1.1 bytes reproduces the verdict exactly (deterministic *given* the
  bytes) while a neighbouring build is clean (unpredictable *across* builds).

So: a packing artifact on an unsigned self-extractor, not a property of the software. Nothing
in the codebase needs changing, and changing code to appease it would be chasing noise.

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

The honest summary: apart from signing, there is no lever here that is evidenced to work.
Changing our code or packaging to appease a dice roll would be chasing noise.

## Fixing it properly

**Code-sign `Setup.exe`.** The only remedy with real evidence behind it. Current (2026) options:

| Option | Cost | Catch |
|---|---|---|
| **Azure Trusted Signing** (now "Artifact Signing") | **$9.99/mo**, 5 000 signatures | Individual sign-up is **US/Canada residents only** (public preview). Organisations need **3 years** of verifiable business history, and the service covers US/CA/EU/UK only. A new org does not qualify; a US/CA individual does, with no waiting period. |
| **SignPath.io** | **Free tier for OSS** | Worth pursuing first given this project is source-available on GitHub — no hardware, no monthly cost. |
| **OV certificate** (Sectigo et al.) | ~$219/yr | Since June 2023 the private key must live on a **FIPS 140-2 L2 hardware token**, which badly complicates automated release signing. |
| **EV certificate** | Most expensive | No longer grants instant SmartScreen trust — Microsoft states "this behavior no longer exists." |

Note that signing is not an instant fix either: SmartScreen reputation is per-publisher-identity
and still accrues over time. It is, however, the only lever that changes the underlying
situation rather than re-rolling the dice.

Signing costs money and requires identity verification, so it is an owner decision and has not
been done unilaterally.

**Report the false positives.** Vendors act on these and it is free:

- ESET: <https://support.eset.com/en/kb141-submit-a-virus-website-or-potential-false-positive-sample-to-eset-lab> (or email `samples@eset.com`, subject prefixed `False positive`)
- Skyhigh/Trellix: <https://www.trellix.com/support/submit-sample/>
- Microsoft (if it ever flags us — it does not currently): <https://www.microsoft.com/en-us/wdsi/filesubmission>

Include the VirusTotal permalink, the download URL, and that the project is open-source at
<https://github.com/LunarWerxs/SageThumbs-2k>.

**Note for whoever handles SourceForge:** its listing reflects ESET's verdict. Getting the ESET
false positive retracted is what clears it; there is no separate SourceForge appeal to file.
