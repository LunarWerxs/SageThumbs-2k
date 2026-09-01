# AV false-positive submission - prepared package

Fill-in-the-blanks kit for reporting the installer as a false positive. The final submit
needs a signed-in Microsoft account and a file upload, so that last click is yours; every
piece of information you need is assembled here.

## 2026-08-31: the count was MEASURED end to end, and the payload is not the cause (issue #30)

A user reported v2.5.0 x64 at 9 detections and called it abnormally high. They were right that
it is high, and right that nobody on our side had noticed. Everything below is measured, not
argued.

### Every published installer, looked up by hash on the same afternoon

```text
release        arch     VT      engines
v2.5.0         x64     9/71     APEX DeepInstinct Fortinet Google Microsoft Rising Skyhigh
                                TrellixENS TrendMicro-HouseCall
v2.5.0         arm64   2/71     APEX Skyhigh
v2.4.1         x64    10/71     + Cylance alibabacloud
v2.4.1         arm64   1/71     APEX
v2.4.0         x64     5/71  ;  v2.4.0 arm64  2/71
v2.3.2         x64     6/71  ;  v2.3.2 arm64  3/71
v2.3.1         x64    13/71  ;  v2.3.1 arm64  1/71
v2.3.0         x64     4/69  ;  v2.2.0 x64 3/71 ; v2.1.2 x64 4/70 ; v2.0.0 x64 4/71
v1.12.0        x64     5/71  ;  v1.8.0 x64 5/71 ; v1.7.5 x64 4/71
```

`scripts\check-av.ps1` regenerates this table on demand; it is wired into `release.ps1` at
step [1/6] so the NEXT release always reports on the LAST one.

### The controlled experiment: removing half the installer does not help

The standing hypothesis was that the shape of the artifact, a 1.2 MB stub dragging a 12.6 MB
entropy-8.00 overlay (91.3% of the file), is what the heuristic engines score. `build-release.ps1`
already has a `-NoImageMagick` flag, so this was testable for the price of two builds. Both were
built from ONE tree and uploaded within ten minutes of each other, so the engine set, the
signature set and the file age are all constant and the payload is the only variable:

```text
variant                          size         overlay   VT      engines
WITH ImageMagick (as shipped)    13,860,373   91%       2/71    APEX, Skyhigh
WITHOUT ImageMagick               7,189,270   83%       3/70    APEX, Elastic, Skyhigh
```

**Halving the download did not reduce detections; it slightly increased them.** So the payload
is not what is being scored, and the ideas that follow from that hypothesis are dead:
splitting ImageMagick out of the installer, downloading it on demand, shipping Compact by
default. Do not spend a product decision on any of them. (Downloading it on demand would also
add a network surface for a marginal, now-disproven benefit.)

### What the same experiment DID prove, and it is the whole answer

That freshly-built Full installer is the same product as the published v2.5.0 x64, built from
the same tree minutes apart. It scored **2/71**. The published one scored **9/70** the same
afternoon.

Same program. Same code. Same day, same engines, same signatures. The only differences are that
one hash is four days old and has been downloaded ~570 times, and the other had existed for ten
minutes.

This is the cleanest evidence this project has ever had for the prevalence explanation the older
sections below argue from release-to-release history. It also sharpens it into a LIFECYCLE
rather than a straight line, which is what the older `1.3.4 clean / 1.3.8 flagged` alternation
was really showing:

1. a brand-new hash is nearly clean, because the cloud/ML systems have not processed it yet;
2. it SPIKES over the following days as it is downloaded, submitted and scored;
3. it DECAYS again as it accrues prevalence and reputation (1.2.2 is at 1/75).

The uncomfortable corollary is worth stating plainly: **the better a release does, the worse its
VirusTotal number looks for a while.** v2.5.0 x64 (572 downloads) reads 9; v2.4.0 x64 (69
downloads) reads 5. That is not a quality signal about the build.

### PROVEN: it is ONLY the installer wrapper. Our own binaries are 0/70.

The published 2.5.0 payload was pulled out and every one of our binaries scanned on its own:

```text
sagethumbs2k.dll   7,126,016   0/70
st2k.exe           7,257,088   0/70
SageThumbs2K.exe  10,415,104   0/71
st2k_dlghook.dll     194,048   0/70
```

Zero. Not one engine flags any of the code we write. Every detection in this document is on the
Inno Setup stub that carries them. That settles a question worth never re-opening: there is
nothing to find in our source, and no amount of code review or hardening moves this number.

(The portable zip, which is the same binaries without the wrapper, read 1/66 the same day. It
is NOT to be recommended as a workaround, per owner directive 2026-08-31: pointing users at a
different download concedes the premise, and the installer is what we ship. It is recorded here
only as further evidence that the wrapper is the whole story.)
### Five of the nine cannot block anybody

Sorting the v2.5.0 x64 flaggers by whether a real user could be stopped by one:

- **Microsoft** - the only one documented to have actually blocked an install here (issue #12,
  a real Defender cloud-ML quarantine). Defender is on by default on essentially every Windows
  machine, so this is the detection that matters, and it is the one to submit every release.
- **TrendMicro-HouseCall, Rising** - real consumer products, plausible on a home machine.
- **DeepInstinct, Fortinet, Skyhigh, TrellixENS** - enterprise EDR and gateway products sold to
  IT departments. A machine running these is usually also blocking unsigned installers by
  policy, so clearing the verdict would frequently not even unblock the install.
- **Google** - not a shipping antivirus at all. It is Google's own scanner contributed to
  VirusTotal. Nobody's download is blocked by it; it only moves the ratio.
- **APEX** - SecureAge's AI engine, and this project's single most persistent flagger: it
  appears on nearly every build of every version including the cleanest arm64 ones.

So the headline ratio overstates user harm by roughly a factor of four. Fix the number people
actually get blocked by, and treat the rest as cosmetic.

### The x64 / arm64 gap is the engines, not the build

Every x64 build scores 3-13 while the arm64 build of the SAME release scores 1-3. The two come
from one `installer.iss`, the same ImageMagick pin and file inventory, the same cargo features
and the same stub-DLL treatment. The VirusTotal denominator is identical (71 on both), so the
engines are running on the ARM64 file; they simply flag it far less. Conclusion: engines model
x86-64 code far more deeply than ARM64. There is no x64 build defect to find, and an x64 number
is only meaningful against other x64 numbers - which is why `check-av.ps1` reports a band per
architecture rather than one global threshold.

### Vendor false-positive channels, verified 2026-08-31

Cross-checked against VirusTotal's own maintained contact list plus live fetches. Ranked by
whether the submission reduces REAL user harm rather than the VT ratio:

| engine | channel | account? | worth it |
|---|---|---|---|
| Microsoft | https://www.microsoft.com/en-us/wdsi/filesubmission | yes, MS account | **the one that matters** |
| SecureAge (APEX) | https://uav.secureage.com/falsepositive | no | high: our most persistent flagger |
| Fortinet | https://www.fortiguard.com/faq/classificationdispute | no | medium |
| TrendMicro | https://www.trendmicro.com/en_us/about/legal/detection-reevaluation.html | no | medium, real consumer AV |
| Rising | fp@rising.com.cn | no | low unless we have China traffic |
| DeepInstinct | vt-fps-requests@deepinstinct.com | no | cosmetic |
| Trellix | datasubmission@trellix.com | no | cosmetic |
| Skyhigh | GatewayAnti-Malware-SupportEscalations@SkyhighSecurity.com | no | cosmetic |
| Google | google-at-virustotal@google.com | no | cosmetic only, not a shipping AV |

**Close the loop on every submission.** Three to seven days later run
`python push_to_vt.py --hash <sha256>` on that exact hash: success is the engine's row
disappearing. If it has not cleared in a week the submission did not land, so resubmit. A
submission nobody re-checks is the same defect as a gate nobody looks at twice.

### What the count actually tracks, and what I could NOT establish (2026-08-31)

The owner's question was sharp and fair: we sat around 3 for dozens of releases, so how is it
suddenly 9? Ruled out, each with evidence rather than argument:

- **Our code.** Every shipped binary is 0/70 (above).
- **The payload.** Halving the installer by dropping ImageMagick moved it 2 -> 3, the wrong way.
- **Build nondeterminism.** The same source built TWICE on the same afternoon scored 2/71 and
  2/70. A fresh build is reproducibly ~2; there is no build-time lottery.
- **Installer construction.** v2.4.0 and v2.4.1 differ by one digit in AppxManifest and nothing
  else in packaging, and scored 5 and 10.
- **Installer size.** 13.75 MB at 2.0.0 to 13.86 MB at 2.5.0, under 1% growth across the jump.

What is LEFT, and it is the best-supported explanation rather than a proven one: **attention**.
The number of times a hash has been submitted to VirusTotal tracks the detection count better
than anything else measured:

```text
version   submissions   detections
1.7.5           1            4
1.8.0           1            5
2.0.0           2            4
2.2.0           3            3
2.3.2           4            6
2.4.1           8           10
2.5.0          16            9
```

Each submission pulls a fresh set of cloud/reputation engines onto the file. The 1.x era sat at
3-5 partly because almost nobody was putting those installers through VirusTotal. **The product
got popular; the scrutiny followed.** A freshly built installer nobody has submitted is 2.

⛔ **Two honest gaps, stated so nobody treats this section as finished.**

1. **v2.3.1 breaks the pattern** at 13 detections with only 2 submissions and 94 downloads. No
   explanation offered; it is the single wildest outlier in the whole dataset.
2. **The engines-got-more-aggressive theory is UNTESTED, not disproven.** The obvious test is to
   re-scan an old installer with today's engines. `POST /files/{id}/analyse` was called on five
   of them and VirusTotal accepted every request, but `last_analysis_date` never moved, so the
   identical before/after numbers proved NOTHING. That result was discarded rather than
   reported. If anyone retries this, verify the analysis date actually advanced before believing
   the verdicts.

### The signing options, priced and eliminated (researched 2026-08-31)

**Azure Artifact Signing** (renamed from Trusted Signing in 2026) is the pick. $9.99/month
Basic, 5,000 signatures a month, $0.005 per signature over. No USB token, no HSM, works from
CI through the official GitHub Action, and one account signs every Windows thing we ship.

Two things decide whether it is even available to us, and they are worth settling BEFORE
starting the identity validation, because a failed attempt has to be redone from scratch:

* **Organisation identity** (certificate says LunarWerx) requires a verifiable tax history of
  **3 or more years**, plus a DUNS that matches D&B exactly.
* **Individual identity** (certificate says a person's name plus city/state/country) is **US or
  Canada only**, and the details are pulled READ-ONLY from the Azure billing account, so that
  account's legal name and address must already match the government ID being presented.
* Either way it needs a **paid** Azure subscription. Free, trial and sponsored are refused.

⛔ **Certum's Open Source Code Signing certificate is a TRAP for this project specifically**, and
it is the cheapest thing on the market at about EUR 25, so somebody will suggest it. It puts
**"Open Source Developer"** in the publisher field rather than our name, it is issued to
individuals only, and Certum **revokes it if the signed software is distributed commercially**.
This project is PolyForm Noncommercial with a live commercial-licence prospect, so that
certificate would be revoked out from under us the moment the licensing deal lands.

Traditional OV certificates from Certum or SSL.com run roughly $116-226/year with cloud signing
and put the real company name on the certificate. They are the fallback if the Azure identity
validation fails.

One timing note that removes the usual argument for a traditional CA: since **27 February 2026**
a code signing certificate may be valid for at most ~459 days, so multi-year prepay discounts no
longer exist anywhere. Monthly and annual now cost about the same per year.

### Still the durable fix, and still a spending decision

Code signing. Every release is a fresh unsigned hash, so the lifecycle above re-runs from
scratch each version, forever. Nothing free changes that. Everything in this document is
managing a symptom.

## Before anything: run the check, it decides for you

```powershell
pwsh scripts\av-defender-check.ps1
```

It scans every installer in `dist\` with the REAL local Defender engine and prints CLEAN or,
if one is ever actually flagged, the filled-in portal fields (file, SHA-256, detection name).
`release.ps1` runs it at step [4c/6] on every release, so this is already answered by the time
you read the log. Exit 0 = nothing to submit. **A VirusTotal `!ml` hit alone is NOT grounds to
submit** (see the next section for why).

As of 2026-08-02 it reports CLEAN for every installer this project has published, 1.3.8
through 1.7.0.

## 2026-08-06: the SECOND detection surface is the app EXE + our Run key (issue #14)

Everything above is about the INSTALLER. Issue #14 is not: Kaspersky Free flagged
`SageThumbs2K.exe` itself as `Trojan-Dropper.Win32.Dapato.skmi`, on a real user's machine, and
the log names the trigger outright. The deleted object was the value **`SageThumbs2KScreenshot`**
under `HKCU\...\CurrentVersion\Run` - ours, written by `screenshot/enable.rs` when the
screenshot hotkey is enabled, because a global hotkey needs a resident daemon.

Read that the way a heuristic does: an unsigned executable with near-zero prevalence writing
itself into an autostart key. That is also the textbook description of a dropper installing
persistence, which is precisely what the `Dapato` family pattern encodes. Nothing about our code
is wrong; the SHAPE is what matches.

- The reported MD5 `3C0D2839B13E6872E1FB001E49557E41` **has never been submitted to VirusTotal**
  (API returns 404). Zero prevalence is an input to these verdicts, and the one we can only fix
  with downloads and time.
- Kaspersky **deleted** the Run value rather than merely flagging it, so the user's hotkey stops
  working at next logon and does not come back by itself. Anyone reporting "the screenshot hotkey
  died" after an AV alert needs Settings -> Screenshots -> Restart.
- Turning **"Enable screenshot hotkey"** off removes the Run entry entirely, which removes this
  detection surface. Thumbnails and the context menu never touch autostart, so that workaround
  costs the user only the feature they were not using.

### MEASURED 2026-08-06, and it reframes the whole thing: we are essentially clean

Scanned the same day the report came in:

| file | VirusTotal |
|---|---|
| `SageThumbs2K.exe` (the app, the file he reported) | **0/75.** Nobody. Kaspersky included |
| `SageThumbs2K-Setup-1.7.5.exe` (the installer) | **1/75**, APEX alone. Kaspersky and Microsoft now clean |

So there is **no static signature to work around**. Kaspersky's own engine, running the full
signature set on VirusTotal, passes our binary. Two things follow:

1. **His detection is Kaspersky's LOCAL behavioural engine, not its signature database.** File
   Anti-Virus watching an autostart key being written is a different mechanism from the scanner
   VirusTotal runs, and only the latter is what "0/75" measures.
2. **His MD5 does not match any binary here**, so he may simply be on an older build. Asked.

**This is why the Run-key change below cannot be evidence-driven the way the entropy and
VERSIONINFO knobs were.** VirusTotal is a STATIC scan. It cannot see a behavioural rule firing,
so building a Scheduled Task variant and scanning it would produce 0/75 either way and prove
nothing. Measuring it honestly needs a real machine with Kaspersky installed, doing a real
install, twice. Do not mistake a clean VT result on a rebuilt variant for a fix.

**Candidate fix, NOT implemented and NOT measured: move the daemon's autostart from the Run key
to a logon Scheduled Task.** We already create a per-user scheduled task for update checks
(`--update-task`), so the machinery exists and the pattern is proven in this codebase. A logon
task is markedly less heuristically loaded than an `HKCU\...\Run` self-reference. Before doing
it, MEASURE, the way the entropy and VERSIONINFO knobs above were measured, rather than assuming:
build one variant that registers a task instead of a Run value and scan both. The table above is
a standing reminder that a reasonable-sounding AV hypothesis can simply fail to replicate.

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
