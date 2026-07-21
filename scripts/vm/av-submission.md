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
| SHA-256 (1.2.2) | `11D60A2FB9674897CF5340B2EE6FB3B855644624B06944A8E206F72F955151F7` (clean on Defender 2026-07-20) |
| SHA-256 (1.3.0) | `4DDC405E2D0BF4F58EE22AF68FE8CEAAB58C67260A46B00829FEDDB414F25384` (clean on Defender 2026-07-21) |
| Publisher | Lunarwerx (unsigned build) |
| Note | The 1.3.0 hash CHANGED on 2026-07-21 (installer rebuilt to include the Markdown-copy fix). `37CF9D5C…F035` was never published, so nothing was lost — but whatever ships must be the hash above, and the VirusTotal link in the release notes has to be for THAT file. |
| Category | Installer (Inno Setup) that registers a COM shell extension + an optional updater |

## Microsoft Defender false-positive portal (only if Defender flags it)

1. Go to **https://www.microsoft.com/en-us/wdsi/filesubmission** and sign in.
2. Submission type: **Software developer**.
3. Upload the exact `SageThumbs2K-Setup-<ver>.exe` (the hash above).
4. Detection name: **<the threat name the user reported>** (required; leave the submission
   until you have it, since Defender is clean here).
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
