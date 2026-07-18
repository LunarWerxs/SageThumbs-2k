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

**Read this table carefully, because it refutes the obvious explanation.** APEX and Skyhigh
are the CONSTANT baseline — present on every build since 0.8.0. That pair is the
unsigned/Inno-Setup noise floor, and it has never moved.

ESET is the anomaly: it appeared for the first time at **1.1.0**. The project has *always*
been unsigned, so "unsigned" cannot explain a change that starts at one specific version. It
is the standing background risk, not the trigger.

SourceForge's scanner is ESET, so its warnings are the ESET column, not an independent opinion.

### It is not a transient ESET model glitch

Re-analysed 1.1.1's identical bytes on 2026-07-18 with current engine versions: ESET returned
**the same `Generik.MMSQLBT` verdict**, and Rising had additionally joined (4 detections). So
the verdict is stable and reproducible against those bytes, not a bad afternoon for ESET's
model that has since been corrected.

### Do not read 1.2.0's clean result as "fixed"

1.2.0 was scanned hours after being built. ESET's verdict on a brand-new file leans on cloud
reputation that has not matured yet, and 1.2.0 *contains* the 1.1.0 code that correlates with
the detection. Treat a clean first-day scan as **not yet scored**, not as exonerated, and
re-check a few days after each release.

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

## Fixing it properly

**Code-sign `Setup.exe`.** This is the real remedy and would likely take detections to zero.
Options, cheapest first:

- **Azure Trusted Signing** — roughly $10/month, Microsoft-operated, no hardware token. Requires
  a verifiable organisation identity (typically 3+ years of history) or an individual identity.
- **OV certificate** (Sectigo/DigiCert) — roughly $200–400/year, now requires the key on
  hardware, which complicates automated signing.
- **EV certificate** — most expensive, but grants immediate SmartScreen reputation.

Signing is an owner decision (it costs money and requires identity verification), so it has not
been done unilaterally.

**Report the false positives.** Vendors act on these and it is free:

- ESET: <https://support.eset.com/en/kb141-submit-a-virus-website-or-potential-false-positive-sample-to-eset-lab> (or email `samples@eset.com`, subject prefixed `False positive`)
- Skyhigh/Trellix: <https://www.trellix.com/support/submit-sample/>
- Microsoft (if it ever flags us — it does not currently): <https://www.microsoft.com/en-us/wdsi/filesubmission>

Include the VirusTotal permalink, the download URL, and that the project is open-source at
<https://github.com/LunarWerxs/SageThumbs-2k>.

**Note for whoever handles SourceForge:** its listing reflects ESET's verdict. Getting the ESET
false positive retracted is what clears it; there is no separate SourceForge appeal to file.
