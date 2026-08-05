# AV false-positive submission - prepared package

Fill-in-the-blanks kit for reporting the installer as a false positive. The final submit
needs a signed-in Microsoft account and a file upload, so that last click is yours; every
piece of information you need is assembled here.

## Before anything: run the check, it decides for you

```powershell
pwsh scriptsv-defender-check.ps1
```

It scans every installer in `dist\` with the REAL local Defender engine and prints CLEAN or,
if one is ever actually flagged, the filled-in portal fields (file, SHA-256, detection name).
`release.ps1` runs it at step [4c/6] on every release, so this is already answered by the time
you read the log. Exit 0 = nothing to submit. **A VirusTotal `!ml` hit alone is NOT grounds to
submit** (see the next section for why).

As of 2026-08-02 it reports CLEAN for every installer this project has published, 1.3.8
through 1.7.0.

## 2026-08-05: Kaspersky joined the generic-verdict list, and the VT gate was taught to read it

Building 1.7.5 tripped the release VirusTotal gate: Kaspersky reported
`VHO:Trojan-Dropper.Win32.Dapato.gen`, and the gate hard-fails on a tier-1 engine unless the
verdict looks like a machine-learning / heuristic generic. It is one - the `.gen` suffix is
Kaspersky's generic-family notation, the same "matched a family pattern, not this sample"
admission the gate already ignores when spelled "generic". `push_to_vt.py`'s `ML_MARKERS`
simply did not match the abbreviation.

Why this is a false positive and not a new problem, evidenced before the gate was touched:
THREE installers built on 2026-08-05 all drew the same Kaspersky verdict - the real 1.7.5
build, plus two experiments built from 1.7.4's staged payload with different compression and
different VERSIONINFO. Contents varied, verdict did not. It tracks the Inno-stub installer
family, not our code, and it appeared the day AFTER 1.7.4's clean-of-Kaspersky report.

Only `.gen` was added to the marker list. The `VHO:` prefix it pairs with is widely described
as a heuristic/cloud verdict, but Kaspersky does not document it, so it is deliberately NOT
matched - the gate keeps its teeth for anything we cannot justify.

## There is no regression to bisect: the Microsoft verdict predates 1.0.0 (2026-08-05)

The obvious theory — "some version started tripping this, find that commit and revert it" —
is FALSE, and it was tested rather than argued. Every archived installer in `dist\` was
looked up on VirusTotal by hash (no re-upload, just its stored verdicts):

| version | Microsoft verdict | total |
|---|---|---|
| 0.7.2, 0.9.0 | never scanned (nothing scanned releases before 1.2.0) | - |
| 1.0.0 | `Trojan:Win32/Wacatac.B!ml` | 4/74 |
| 1.1.0 | `Trojan:Win32/Wacatac.C!ml` | 5/73 |
| 1.1.1 | `Trojan:Win32/Wacatac.B!ml` | 5/74 |
| 1.2.0 | (Google `Detected`) | 6/75 |
| 1.2.1 | `Trojan:Win32/Wacatac.B!ml` | 4/75 |
| 1.2.2 | **clean** | 1/75 |
| 1.3.0 | **clean** (rescanned 2026-08-04) | 2/75 |
| 1.3.4 | **clean** | 1/74 |
| 1.3.8 | `Trojan:Win32/Wacatac.B!ml` | 3/74 |
| 1.5.0 | **clean** | 2/75 |
| 1.6.0 | `Trojan:Win32/Wacatac.B!ml` | 3/75 |
| 1.7.2, 1.7.4 | `Trojan:Win32/Wacatac.B!ml` | 3/75 |

Read it carefully, because it settles several things at once:

1. **1.0.0 — the very first release ever scanned — already carried `Wacatac.B!ml`.** There is
   no "before" to go back to. No commit introduced this.
2. **It ALTERNATES** (1.3.4 clean, 1.3.8 flagged, 1.5.0 clean, 1.6.0 flagged) across releases
   whose installer construction is identical. No code change toggles on and off like that.
3. **The controlled comparison:** 1.3.0 and 1.7.4 were both (re)scanned on 2026-08-04, same
   engines, same signatures. The OLD file came back clean; the ONE-DAY-OLD file was flagged.
   The variable is the hash's age and prevalence, not its contents.
4. **1.2.2 is the cleanest release this project has ever shipped (1/75).** An earlier note in
   this repo described 1.2.2 as the start of a "standing pattern"; that was the earliest row
   in the old detection table being mistaken for the beginning of the problem. It was the
   opposite — a low point, not a starting point.

CONCLUSION: `Wacatac`/`Wacapew` `!ml` here is a low-prevalence-new-file verdict, not a
reaction to anything in our code. Do not go looking for the offending commit; there isn't
one. The levers are the ones in the section below, and they are about identity and
prevalence, not source.

## MEASURED: what actually changes the detections, and what does not (2026-08-05)

Three x64 installers built from the SAME staged payload, all scanned on VirusTotal the same
day, so the engine set and signatures are constant. This is evidence, not theory — do not
re-litigate these knobs without new measurements.

| build | VT | engines |
|---|---|---|
| shipped 1.7.4, `lzma2/ultra64` + solid | 3/70 | APEX, **Microsoft `Wacatac.B!ml`**, Skyhigh `ObfuscatedPoly` |
| + COMPLETE VERSIONINFO | 4/70 | APEX, Kaspersky `Dapato`, **Microsoft `Wacapew.C!ml`**, Skyhigh |
| + `Compression=zip/1`, no solid (24.8 MB) | 3/70 | APEX, Kaspersky `Dapato`, **Microsoft `Wacapew.C!ml`** |

Conclusions:

1. **Completing the installer's VERSIONINFO did NOT reduce detections.** The hypothesis was
   reasonable — this repo previously moved a payload stub DLL from 6/64 to 1/69 purely by
   adding a VERSIONINFO resource (see `build-release.ps1`) — but it does NOT replicate for
   the Inno setup stub. The metadata change is kept anyway because it is simply CORRECT (the
   installer claimed `lunarwerx` while every payload PE says `LunarWerx`, and
   OriginalFilename/copyright were blank), not because it buys detections.
2. **Payload entropy is real but cheap-to-lose.** Dropping to `zip/1` removed Skyhigh's
   `ObfuscatedPoly` verdict outright — that engine is scoring the entropy-8.0 overlay. It
   costs 12.8 MB -> 24.8 MB, nearly doubling every download, to silence ONE engine that has
   never been the thing quarantining users. NOT taken. Revisit only if Skyhigh's verdict
   starts appearing in user reports.
3. **Microsoft fires in every single variant.** `Wacatac`/`Wacapew` `!ml` survived both
   changes. It is not driven by metadata or entropy; it tracks the unsigned + zero-prevalence
   + installs-a-shell-hook profile. Nothing free changes it. Per-release WDSI submission
   remains the only lever, and it clears one HASH, never the product.

The free levers that DO exist, in order of value: submit each release to WDSI before
announcing it; keep publishing through channels that accrue prevalence (GitHub Releases,
winget); and tell users plainly what they will see and how to verify the hash.

## 2026-08-04: the threshold was crossed — SUBMIT for 1.7.4

Issue #12 (screenshot attached there) shows real Defender on a real machine QUARANTINING the
1.7.4 x64 installer as **`Trojan:Win32/Wacatac.B!ml`**, severity Severe, during a winget
(UniGetUI) install and again on a manual download. This is no longer VT-only noise.

**Correction to the doctrine below:** a clean LOCAL scan does not clear us. The end-user hit
comes from Defender's CLOUD-delivered ML layer ("block at first sight"), which fires on a
low-reputation hash at download/install time and does not reproduce in a local
`MpCmdRun -Scan` — our scan of the same bytes with signatures 1.455.499.0 was clean the same
day users were quarantined. So: an end-user report WITH a Defender threat name IS grounds to
submit, even when `av-defender-check.ps1` prints CLEAN.

Ready-to-submit fields (portal: https://www.microsoft.com/en-us/wdsi/filesubmission,
Submission type: Software developer, incorrectly detected = Yes):

| Field | x64 | ARM64 |
|---|---|---|
| File | `SageThumbs2K-Setup-1.7.4.exe` | `SageThumbs2K-Setup-1.7.4-arm64.exe` |
| SHA-256 | `05A16462C44521CE1DA82D6AA9DD0AF9F8D8B09596D80576D25FB05AF0E98590` | `0BC9CAFC417930CD388493A769EB9E3D2B957150AE9DFDFBF6C81C5D0DEAB211` |
| Detection name | `Trojan:Win32/Wacatac.B!ml` | (submit with the same name) |

The x64 hash is byte-identical to the winget-manifest hash, so one clearance covers both
channels. The notes paragraph in the portal section below still applies verbatim.

The durable fix is CODE SIGNING: every release is a brand-new unsigned low-prevalence hash,
so this lottery re-runs each version. An OV certificate (or Azure Trusted Signing) ends it.

## Finding first: Microsoft Defender does NOT flag our installer

A local Windows Defender scan of the shipped installer came back **clean, no threats**
(`MpCmdRun.exe -Scan -ScanType 3 -File <installer>`). So:

- There is **nothing to dispute with Microsoft Defender** right now. A false-positive
  submission to Microsoft is only meaningful once a specific Defender **threat name** is
  actually being reported on a real machine (the submission form asks for it).
- What the reporter most likely saw is **SmartScreen**, not a virus detection: the
  "Windows protected your PC / unknown publisher" blue box. That is a *reputation* prompt
  for an unrecognized download, not a malware verdict, and it clears as the exact installer
  hash accrues clean downloads. It is a different channel from the Defender file portal.
- If a third-party engine (not Defender) is flagging it, submit to *that* vendor (list below).

## The installer (fill in per release)

| Field | Value |
|---|---|
| Product | SageThumbs 2K (Windows shell extension: thumbnails + right-click image tools) |
| File name | x64: `SageThumbs2K-Setup-<ver>.exe`; ARM64 Compact: `SageThumbs2K-Setup-<ver>-arm64.exe` |
| SHA-256 (1.2.2, PUBLISHED) | `11D60A2FB9674897CF5340B2EE6FB3B855644624B06944A8E206F72F955151F7` |
| SHA-256 (1.3.0, built 2026-07-21) | `8BE7138281198171A273771CC76D54AB7FADA49ED202C78756811E925221EE14` |
| Publisher | Lunarwerx (unsigned build) |
| Category | Installer (Inno Setup) that registers a COM shell extension + an optional updater |
| Note | The 1.3.0 hash moves on every rebuild. Only the hash of the exact architecture-specific artifact ATTACHED to the GitHub release may be submitted or linked. |

### Detection names (2026-07-21) - this is the field the portal requires

The blank that used to block this submission is now filled. Local Defender scans BOTH files
clean, so the name had to come from VirusTotal's Microsoft engine (which runs without the
cloud/reputation context a real Defender install has - that is exactly why the two disagree).

| Build | Microsoft verdict | Total | VirusTotal permalink |
|---|---|---|---|
| 1.2.2 (published) | `Program:Win32/Wacapew.C!ml` | 3/69 | https://www.virustotal.com/gui/file/11d60a2fb9674897cf5340b2ee6fb3b855644624b06944a8e206f72f955151f7 |
| 1.3.0 | `Trojan:Win32/Wacatac.B!ml` | 3/69 | https://www.virustotal.com/gui/file/8be7138281198171a273771cc76d54ab7fada49ed202c78756811e925221ee14 |
| 1.7.0 x64 (published) | `Trojan:Win32/Wacatac.C!ml` | 4/71 | https://www.virustotal.com/gui/file/20ec7be0886d00d0001e0b8edbc29c0f66a7ddaa0814f7b30e4018cffdf0d1bb |
| 1.7.0 arm64 (published) | `Trojan:Win32/Wacatac.B!ml` | 3/70 | https://www.virustotal.com/gui/file/5a0cefb984958046fed591573bec02e6d9d83631432dde05fa189b7532a4a738 |

Same three engines on both builds (Microsoft, APEX, Skyhigh) - this is the standing baseline
for an unsigned low-prevalence Inno installer, NOT a regression introduced by 1.3.0. Both
Microsoft verdicts carry the **`!ml` suffix**, i.e. a machine-learning generic, not a signature
match. Worth stating plainly in the submission: `Wacatac.B!ml` sounds far more alarming than
`Wacapew.C!ml` but is the same class of generic ML verdict.

## Microsoft Defender false-positive portal (only if Defender flags it)

1. Go to **https://www.microsoft.com/en-us/wdsi/filesubmission** and sign in.
2. Submission type: **Software developer**.
3. Upload the exact architecture-specific installer - `SageThumbs2K-Setup-<ver>.exe` for x64 or
   `SageThumbs2K-Setup-<ver>-arm64.exe` for ARM64 Compact - and use that artifact's hash.
4. Detection name: **`Trojan:Win32/Wacatac.B!ml`** (for the 1.3.0 hash above), or
   **`Program:Win32/Wacapew.C!ml`** for 1.2.2. Both come from the VirusTotal table above.
5. "Do you believe this is incorrectly detected (false positive)?" → **Yes**.
6. Notes to paste:
   > SageThumbs 2K is a source-available Windows shell extension (thumbnail + context-menu
   > provider) for image files. The Inno Setup installer registers its COM handlers and
   > installs an optional self-updater. Source and releases:
   > https://github.com/LunarWerxs/SageThumbs-2k

## VirusTotal (do this first to know WHO flags it)

Paste the SHA-256 at **https://www.virustotal.com** (or upload the installer). That tells
you the exact engines and detection names, so you submit only to the vendors that actually
flag it. Common ones and their false-positive forms:
- Microsoft: the portal above.
- Avast/AVG: https://www.avast.com/false-positive-file-form.php
- Kaspersky: https://opentip.kaspersky.com/ (or false_alarm@kaspersky.com)
- Bitdefender: https://www.bitdefender.com/consumer/support/ (submit sample)
- Publish the VirusTotal permalink on the GitHub release so users see the real ratio
  instead of trusting one popup.

## The durable reduction (non-signing)

Recurrence is a reputation problem. Lowest-friction levers, in order: keep the installer
hash stable across a release cycle (churny re-uploads reset reputation), ship a portable
ZIP alternative (no installer wrapper trips far fewer heuristics), and publish the
VirusTotal link. Code signing is deliberately out of scope for this project.
