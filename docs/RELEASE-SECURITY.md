# Release security: antivirus false positives

## Short version

SageThumbs 2K installers get flagged by 2–3 of VirusTotal's ~70 engines. Every one of those
detections is **heuristic or machine-learning**, not a signature match, and the cause is that
`SageThumbs2K-Setup-*.exe` is **not code-signed**. Roughly 67 engines — including Microsoft,
Kaspersky, Bitdefender and Sophos — return clean.

Every release is now scanned **before** it is published (see *The gate* below). Releases up to
and including v1.2.0 were not, which is why ESET's verdict on 1.1.0/1.1.1 first surfaced on
SourceForge's listing instead of in our own pipeline.

## What has actually been detected

| Build | VT | Engines |
|---|---|---|
| 1.1.0 | 3/69 | APEX `Malicious`, **ESET-NOD32 `Generik.NJDPIFC`**, Skyhigh `BehavesLike.Win32.ObfuscatedPoly.tc` |
| 1.1.1 | 3/69 | APEX `Malicious`, **ESET-NOD32 `Generik.MMSQLBT`**, Skyhigh `BehavesLike.Win32.ObfuscatedPoly.tc` |
| 1.2.0 | 2/70 | APEX `Malicious`, Skyhigh `BehavesLike.Win32.ObfuscatedPoly.tc` |

SourceForge's scanner is ESET, so its warnings are the ESET column above, not an independent
opinion.

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

## Root cause, ranked

1. **The installer is unsigned.** The self-signed certificate covers only the MSIX sparse
   package (needed for the Windows 11 context menu); `Setup.exe` itself carries no signature.
   This is the single largest contributor.
2. **Low prevalence.** Every release is a brand-new binary no engine has seen. Reputation
   systems weight this heavily, which is why detections tend to fade weeks after a release.
3. Compressed self-extracting installer (see above).
4. Bundled third-party binaries (the trimmed ImageMagick DLLs), themselves unsigned.

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
