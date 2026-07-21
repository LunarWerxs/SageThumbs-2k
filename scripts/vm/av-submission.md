# AV false-positive submission — prepared package

Fill-in-the-blanks kit for reporting the installer as a false positive. The final submit
needs a signed-in Microsoft account and a file upload, so that last click is yours; every
piece of information you need is assembled here.

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
| File name | `SageThumbs2K-Setup-<ver>.exe` |
| SHA-256 (1.2.2, PUBLISHED) | `11D60A2FB9674897CF5340B2EE6FB3B855644624B06944A8E206F72F955151F7` |
| SHA-256 (1.3.0, built 2026-07-21) | `8BE7138281198171A273771CC76D54AB7FADA49ED202C78756811E925221EE14` |
| Publisher | Lunarwerx (unsigned build) |
| Category | Installer (Inno Setup) that registers a COM shell extension + an optional updater |
| Note | The 1.3.0 hash moves on every rebuild. Only the hash that is actually ATTACHED to the GitHub release may be submitted or linked. |

### Detection names (2026-07-21) — this is the field the portal requires

The blank that used to block this submission is now filled. Local Defender scans BOTH files
clean, so the name had to come from VirusTotal's Microsoft engine (which runs without the
cloud/reputation context a real Defender install has — that is exactly why the two disagree).

| Build | Microsoft verdict | Total | VirusTotal permalink |
|---|---|---|---|
| 1.2.2 (published) | `Program:Win32/Wacapew.C!ml` | 3/69 | https://www.virustotal.com/gui/file/11d60a2fb9674897cf5340b2ee6fb3b855644624b06944a8e206f72f955151f7 |
| 1.3.0 | `Trojan:Win32/Wacatac.B!ml` | 3/69 | https://www.virustotal.com/gui/file/8be7138281198171a273771cc76d54ab7fada49ed202c78756811e925221ee14 |

Same three engines on both builds (Microsoft, APEX, Skyhigh) — this is the standing baseline
for an unsigned low-prevalence Inno installer, NOT a regression introduced by 1.3.0. Both
Microsoft verdicts carry the **`!ml` suffix**, i.e. a machine-learning generic, not a signature
match. Worth stating plainly in the submission: `Wacatac.B!ml` sounds far more alarming than
`Wacapew.C!ml` but is the same class of generic ML verdict.

## Microsoft Defender false-positive portal (only if Defender flags it)

1. Go to **https://www.microsoft.com/en-us/wdsi/filesubmission** and sign in.
2. Submission type: **Software developer**.
3. Upload the exact `SageThumbs2K-Setup-<ver>.exe` (the hash above).
4. Detection name: **`Trojan:Win32/Wacatac.B!ml`** (for the 1.3.0 hash above), or
   **`Program:Win32/Wacapew.C!ml`** for 1.2.2. Both come from the VirusTotal table above.
5. "Do you believe this is incorrectly detected (false positive)?" → **Yes**.
6. Notes to paste:
   > SageThumbs 2K is an open-source Windows shell extension (thumbnail + context-menu
   > provider) for image files. The installer is Inno Setup; it registers COM handlers and
   > installs an optional self-updater. No bundled third-party software, no data collection
   > pitch, no PUA behavior. Source and releases: https://github.com/LunarWerxs/SageThumbs-2k

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
