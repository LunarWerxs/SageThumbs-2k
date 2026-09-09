# Release security: antivirus false positives

## Short version

**Every binary SageThumbs 2K ships is clean on VirusTotal - 0 detections out of ~69, all three
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
| 1.2.0 | 2026-07-18 | 2/70 | clean *(so far - see below)* | APEX, Skyhigh |

**That table is misleading, and the trap is worth naming.** Those are each build's *stored*
verdict from whenever VirusTotal last analysed it - mostly the day it was released. Comparing
them looks like a timeline of our software. It is not; it is a timeline of *when each file
happened to be scanned*.

### The decisive test: re-scan OLD builds with TODAY's engines

| Build | Stored verdict | Re-analysed 2026-07-18 |
|---|---|---|
| 1.0.0 | clean (scanned Jul 17) | **`Generik.CBCUAMQ`** - flipped to flagged |
| 1.0.1 | clean (scanned Jul 14) | clean |
| 1.1.1 | `Generik.MMSQLBT` | `Generik.MMSQLBT` (unchanged) |

**1.0.0 predates the 1.1.0 selection feature entirely, was clean yesterday, and flags today on
identical bytes.** Nothing about the file changed. ESET's model did.

That single result disposes of every "what did we change at 1.1.0" theory, including two this
document previously advanced. Across builds of one product ESET has now issued **four
different cluster IDs** - `CBCUAMQ`, `NJDPIFC`, `MMSQLBT`, and clean - with no correspondence
to anything in the source.

### What is actually happening

ESET's `Generik.*` is a generic ML/heuristic bucket, not a signature. Malware authors also
package payloads with Inno Setup, so vendors periodically ship heuristics matching the Inno
**stub** itself - which is why this catches legitimate vendors and why it fires on some builds
and not others with no meaningful change. It is a lottery over the compressed installer image,
re-rolled whenever the vendor updates its model.

Corroborating evidence that this is an industry-wide Inno problem, not ours:

- Inno Setup's own community group carries recurring threads
  ([1](https://groups.google.com/g/innosetup/c/w2weZ4afFqs),
  [2](https://groups.google.com/g/innosetup/c/58LUdjrJUUI),
  [3](https://groups.google.com/g/innosetup/c/lvsb2vWhklk)) - enough that a moderator has a
  standing "contact your AV vendor, not us" reply.
- Microsoft's own Q&A: [False Positives using Inno Setup](https://learn.microsoft.com/en-us/answers/questions/2736482/false-positives-using-inno-setup) - Defender flags Inno output too.
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
cannot explain a detection that appears on some builds and not others - the project has been
unsigned since day one.

**"1.1.0 added text selection, which looks like an infostealer."** 1.1.0 was a single commit
introducing `GetKeyState`, `SetCapture` and `set_clipboard` - *poll keys → capture input →
extract displayed text → write to clipboard* genuinely does read like a credential stealer.
Refuted twice over: the binary containing every line of that code scans **0/68**, and 1.0.0,
which predates the feature entirely, now flags anyway.

## The gate

`scripts/release.ps1` step **4b** runs `push_to_vt.py --gate` on each exact artifact about to
be published - the x64 Full installer and ARM64 Compact installer - after they are built and
before `gh release create`.

It fails the release when:

- any **tier-1** engine flags the build (`Microsoft`, `Kaspersky`, `BitDefender`, `Symantec`,
  `Sophos`, `TrendMicro`, `McAfee`, `Avast`, `AVG`, `DrWeb`, `F-Secure`, `GData`,
  `Malwarebytes`), **or**
- total detections exceed **6**.

It deliberately does **not** fail on the routine 2–3 heuristic hits. A gate that fails on every
release is a gate everyone learns to click past, which is worse than no gate. The threshold
exists to catch a *change* - a real compromise, or a build change that makes us look far worse
than baseline.

It is skipped with a warning (not an error) when `.env` or Python is unavailable: tooling
absence must not block a release, only a real verdict should.

### Exit codes: 1 means a VERDICT and nothing else

`release.ps1` reads a non-zero exit from the gate as "this file is malware", so the script's
exit codes are a contract, not an implementation detail:

| Code | Meaning | `release.ps1` behaviour |
|---|---|---|
| `0` | Scanned, under threshold | Publish |
| `1` | A real verdict: tier-1 engine flagged it, or total detections exceeded the cap | **Abort the release** |
| `75` (`EX_TEMPFAIL`) | The scanner could not be reached or never finished. Says nothing about the file | Warn and continue |

Two releases have now been aborted by a *tooling* failure wearing a verdict's clothing: 1.6.0
(analysis never completed, which produced the `75` path) and 1.7.2 (a `ConnectionResetError`
mid-TLS-handshake escaped the polling `api()` call as an unhandled traceback, so Python exited
1). The poll loop now catches network exceptions, retries on the next 15 s tick, and exits `75`
after 8 consecutive failures. **Any new failure mode added to this script must be classified
into that table before it ships.**

When a release does abort at step 4b, re-run the gate by hand on the identical bytes before
believing it. If it passes, resume with `release.ps1 -SkipBuild`: main is already pushed and
CI-green by that point, the installers are built, and the provenance and digest gates all re-run.

Note this script is **gitignored** (`.gitignore:60`) because it carries the VirusTotal API key
path, so its hardening lives only on the release box and is not recoverable from the repo.

Run it by hand any time:

```
python push_to_vt.py dist/SageThumbs2K-Setup-<ver>.exe --gate
python push_to_vt.py dist/SageThumbs2K-Setup-<ver>-arm64.exe --gate
```

## What does NOT work (so nobody burns a day on it)

Researched against the Inno Setup community group, Microsoft docs, and real test repos. Most
of the popular advice is cargo-cult:

| Suggested "fix" | Verdict |
|---|---|
| Change `Compression=` (lzma2 → zip → none) | **No evidence.** Nothing links Inno's compression choice to heuristic detection. |
| `SolidCompression=no` | **No evidence.** The only real test data ([teeks99/inno-test](https://github.com/teeks99/inno-test)) measures build time and size, not detections. |
| Rich `VersionInfo` metadata | **Unverified.** Already set here regardless - it is cheap and sensible. |
| Avoid the name `Setup.exe` | **No general evidence**, though one documented Defender case turned on filename alone. Ours is already versioned. |
| Upgrade Inno Setup | **Weakly evidenced.** A specific version's stub can get "poisoned" when malware campaigns use it; moving off it plausibly helps, but it is not immunity. |
| Wait for it to fade | **Wrong direction for this.** Microsoft documents SmartScreen warnings fading with prevalence, but that is not the same mechanism, and 1.0.0 got *worse* with age. |

The honest summary: no lever here is evidenced to work. Changing our code or packaging to
appease a dice roll would be chasing noise.

## Fixing it properly

### Code signing: reopened by the owner on 2026-09-01

The 2026-07-18 decision below stood for six weeks. On 2026-09-01 the owner reopened it: an
Azure Trusted Signing account is being set up, and once it is live the release pipeline signs
the installer, its embedded uninstaller, the four binaries inside it, the portable zip's
binaries, and (since 2026-09-09) the sparse MSIX for the modern menu as well, so the installer
no longer trusts anything into a machine certificate store; an upgrade from a self-signed
install removes the certificate that install added. Until it is live, everything below
this heading still describes the shipping position, and nothing in the release flow assumes a
certificate exists. What changed the calculus was the 2.5.0 x64 installer reaching 9/70 on
VirusTotal, including Microsoft's own ML engine (`Trojan:Win32/Wacatac.B!ml`, issue #30), which
is the one verdict users see without going looking.

#### The release gate, decided 2026-09-06

**No release ships until the installer is signed, and the next one is numbered 3.0** (owner
decision, Michael, 2026-09-06). Do not cut a release, bump `Cargo.toml`, or tag anything until
`sign-release.ps1 -Status` reports READY. The version bump is a release-time step for exactly
that reason, so a development build never claims to be 3.0.

#### The signing account is live and the pipeline is proven (2026-09-09)

**A real signature was produced from this machine on 2026-09-09**: a scratch copy of
`st2k.exe`, signed through `sign-release.ps1` with the credential leased from the Connections
vault, reads back through Windows as `CN=LUNARWERX LLC, O=LUNARWERX LLC, L=Harrisonville,
S=Missouri, C=US`, issuer `Microsoft ID Verified CS AOC CA 04`, status **Valid**, timestamped
by the Microsoft Public RSA Time Stamping Authority. The round trip to Azure took two seconds.
Nothing in the pipeline changed to get there; the three `ST2K_SIGN_*` names and the leased
`AZURE_*` triple were all it ever needed.

**The account exists and the certificate profile is Active**, verified in the Azure portal on
2026-09-05 by a session signed in as the owner, reading the resources rather than an email.
Identity validation for the company passed on its first documentation attempt, the Public
Trust certificate profile is Active, and the signer role is held both by the owner's user and
by the `lunawerx-artifact-signing` service principal. The internal identifiers (subscription,
resource group, validation and principal object ids) are deliberately NOT repeated in this
public file; they live in the Connections memory
`azure-artifact-signing-is-live-lunawerx-public-trust`.

**The client secret is in the vault.** It was minted and pasted by a person on 2026-09-09 (Azure
reveals a secret's Value exactly once, inside a cross-origin portal iframe that an agent's
browser tools cannot read) and lives in the Connections Studio Microsoft connection named
**`artifact-signing`** (account `lunawerx-artifact-signing`). The value never enters a
conversation: the `shell` tool leases it into the build process and nothing else can read it.

⛔ **Correct a stale claim before repeating it.** An earlier entry here, and the previous
paragraph of this document, said the tenant had zero Azure subscriptions and therefore no
signing account. **That was a measurement artifact, not a fact.** The Connections vault's
Microsoft credential is a Graph-scoped app registration with no Azure RBAC anywhere, so
`GET /subscriptions` returns an empty list for it whether or not subscriptions exist. An
empty list from that credential is evidence about the credential, never about the tenant. The
`lunawerx-artifact-signing` registration also carries no client secret by design, which is
exactly what "the only missing piece is the paste" looks like from the outside, and it was
misread as "nothing has been set up".

- **This machine signs.** `Microsoft.ArtifactSigning.Client` 1.0.128 is dropped under
  `tools/artifact-signing/` (gitignored, so it can never ship in the public repo); `-Status`
  finds the dlib and the signtool, `-SelfTest` passes, and with the three `ST2K_SIGN_*` names
  plus the leased `AZURE_*` triple the verdict is READY and a signature verifies (above). The
  `AZURE_CLIENT_SECRET` must never be written into a file in this repo or pasted into an
  agent's context: it stays in the Connections vault and is leased into the build shell.
- **The whole installer pipeline has now run signed, end to end (2026-09-09).** A full
  `build-release.ps1` with the lease produced `dist/SageThumbs2K-Setup-2.5.0.exe` in which the
  four staged binaries, the sparse MSIX, the embedded uninstaller and `Setup.exe` itself all
  read back through Windows as `CN=LUNARWERX LLC`, status Valid, timestamped. **The first
  attempt failed at the installer step**, and that is exactly why the dry run was worth its
  twenty minutes: Inno Setup substitutes `$f` already quoted, the Sign Tool definition wrapped
  it in `$q` again, and the uninstaller's path was handed to the signer cut off at its first
  space. Fixed in `build-release.ps1` and pinned both ways in `test-release-pipeline.ps1`,
  because nothing else in the pipeline can see that shape until release day.
- **The modern-menu package is chain-signed too, and the installer has stopped touching the
  certificate store (2026-09-09).** `make-msix.ps1 -AzureSign` patches the package's Publisher
  to the certificate subject (an MSIX is only valid when the two are equal, and signtool
  refuses it otherwise), signs through `sign-release.ps1`, and emits no `.cer`. The publisher
  change changes the package family, so `installer.iss` removes a registration from any other
  publisher before registering (two families would register the COM classes twice), deletes
  last release's `.cer` from `{app}` before `[Files]`, and takes the self-signed certificate out
  of `LocalMachine\TrustedPeople` on upgrade: by recorded thumbprint where one exists, by the
  exact subject `CN=SageThumbs2K` on an upgrade from a pre-marker install, since no shipped
  package uses that subject any more. `check-release-manifest.ps1` and
  `write-release-manifest.ps1` read the mode off the package: self-signed must ship its `.cer`,
  chain-signed must not. **Proven on this machine with the published 2.5.0 installed first
  and the new build installed over it**: one package family under LUNARWERX LLC, zero
  `CN=SageThumbs2K` certificates left in the store, the marker cleared, the `.cer` gone, and
  the packaged COM class activating. The self-signed path stays for development machines and
  for CI's `test-msix-integrity.ps1`, whose new fail-closed case proves the chain-signed
  contract refuses a self-signed package.

#### Release day (written 2026-09-06 so it is not improvised; instance filled in 2026-09-09)

The whole release is one `shell` call through the Connections MCP, because that is the only
door that can lease the client secret into a process without the value ever entering a
conversation. The three `ST2K_SIGN_*` values are names, not secrets, and are safe to write
here; the `AZURE_*` triple is leased from the Microsoft connection that holds the pasted
secret. A Microsoft connection stores exactly the fields `tenantId`, `clientId` and
`clientSecret`, and the lease maps each to the env var the Azure dlib reads. This exact call,
with `-File scripts\packaging\sign-release.ps1 -Path <scratch copy>` in place of the release
script, is what produced the 2026-09-09 proof:

```
connections_execute { local: true, tool_name: "shell", params: {
  cwd: "<this repo>",
  shell: "powershell",
  command: "pwsh -NoProfile -File scripts\\release.ps1",
  env: {
    ST2K_SIGN_ENDPOINT: "https://eus.codesigning.azure.net",
    ST2K_SIGN_ACCOUNT:  "lunawerxsigning",
    ST2K_SIGN_PROFILE:  "lunawerx-public-trust"
  },
  secrets: [{ service: "microsoft", instance: "artifact-signing",
              as: { tenantId: "AZURE_TENANT_ID", clientId: "AZURE_CLIENT_ID",
                    clientSecret: "AZURE_CLIENT_SECRET" } }]
} }
```

`connections_accounts { service: "microsoft" }` lists the instance names; `artifact-signing`
is the one created for signing, never `default` (which fronts `vsce` publishing). The lease
is loud on failure: a missing field returns an error naming the fields it found, rather than
running unsigned. **Expect SmartScreen to keep warning for a while after 3.0 ships**: since
March 2026 Microsoft issues Artifact Signing certificates from intermediates with no
accumulated reputation (`Microsoft ID Verified CS AOC CA 04` is the one on our proof), and
reputation is earned per publisher over downloads. The signature is what stops the
machine-learning "unknown binary" verdicts and lets that counter go up at all.

Before that call, in order: rename `## Unreleased` in `docs/CHANGELOG.md` to `## 3.0.0`
(the exporter takes exactly that heading); bump `version` in `Cargo.toml` and the
`Version="…"` attribute in `scripts/packaging/AppxManifest.xml` to `3.0.0` / `3.0.0.0` (the
consistency check refuses a mismatch); and rewrite the README FAQ answer "Why did Windows or
my antivirus flag the installer?", which today correctly says the installer is unsigned and
signing is planned. On release day that becomes: 3.0 and later are signed by LUNARWERX LLC
through Azure Artifact Signing; the machine-learning "unknown binary" verdicts are what the
signature removes; SmartScreen's reputation prompt can still appear for a while because
reputation is earned per publisher over downloads, and the More info / Run anyway steps stay.
Leave the two dated 2026-08-31 measurements in place as history. Do NOT make that README
change before the signed installer exists: the README is read by people downloading the
current release, and until 3.0 is out that release is unsigned. Commit those four files on
`main`.

**Verify the notes before you publish them.** A release note is a public promise, and these
were largely written by agents. The Script Vault instrument `changelog_claim_verifier` pulls
every mechanically checkable token out of the release section (keyboard shortcuts, CLI flags,
verb names, file extensions) and asserts each one exists in the source, exiting non-zero with
the offending list. Run it after renaming the heading and before `release.ps1`. It last ran
clean on 2026-09-06 over 36 tokens. It cannot check prose, so it lowers the risk rather than
removing it.

**The website is a separate repo and does not update itself.** After the release publishes,
in the site checkout: run the app repo's `gen-site.mjs` against the signed 3.0 `st2k.exe`
with `--site` pointed at the site's `index.html`, which regenerates the version pill and the
structured data; then by hand bump the `current_version` and "Last updated" lines in
`pricing.md` and `llms-full.txt`, which that generator does not touch. Remove the
"installer is not code-signed, click More info then Run anyway" answer from the site FAQ and
the three matching unsigned/SmartScreen claims in `llms-full.txt`, the same edit as the
README's antivirus answer and subject to the same rule: only once the signed installer is
actually published. Finally, drop the "arrives in version 3.0, released shortly" clauses added
to the licensing copy on 2026-09-06: they exist because the site sells a licence whose
redemption screen ships in 3.0, so the moment 3.0 is out they become wrong in the other
direction. `release.ps1` reads
the version from `Cargo.toml`, refuses an existing tag, and runs the gate. After it, the
`winget-submit.ps1` step is part of `release.ps1` and is idempotent. The proof that the
pipeline only wants the names was run without a build or a secret on 2026-09-06: with the
three `ST2K_SIGN_*` values set and nothing else, `-Status` reports READY.
- **Timestamping is mandatory, not optional.** Artifact Signing certificates are short-lived
  by design, so an untimestamped signature stops validating within days.
  `sign-release.ps1` already timestamps through `timestamp.acs.microsoft.com`; do not remove
  it as an optimisation.
- **Signing does not silence SmartScreen on day one.** Reputation still accrues with download
  history, and Artifact Signing does not issue EV certificates, so there is no instant-trust
  option to buy. It is what stops the machine-learning "unknown binary" verdicts and gives the
  false-positive submissions a publisher to attach to.

#### Where it stands on 2026-09-04

- **The pipeline half is done and idle.** `scripts/packaging/sign-release.ps1` signs through
  Azure Artifact Signing (`signtool /dlib Azure.CodeSigning.Dlib.dll /dmdf metadata.json`, the
  dlib from the `Microsoft.ArtifactSigning.Client` NuGet package, timestamped by
  `timestamp.acs.microsoft.com`) and reads every signature back through Windows before it
  reports success. `build-release.ps1` runs it on the four staged PE files before the installer,
  the portable zip and the MSIX are assembled, and hands it to Inno Setup as the `SignTool`
  command so `Setup.exe` and the embedded uninstaller are signed too. It is switched on by three
  environment variables that are names, not secrets (`ST2K_SIGN_ENDPOINT`, `ST2K_SIGN_ACCOUNT`,
  `ST2K_SIGN_PROFILE`); the service principal's credentials are read by the Azure dlib alone,
  through the standard `AZURE_TENANT_ID` / `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` triple,
  never by our scripts. Unset, every release builds exactly as before and prints one yellow
  "unsigned" line. `-Status` says what a machine has, `-SelfTest` proves the signtool and
  verify steps with the local self-signed certificate, `-WhatIf` prints the exact command.
- **The account half does not exist yet.** The Connections Studio vault holds the Lunarwerx
  Entra tenant (it can mint Graph and ARM tokens), but that tenant has **zero Azure
  subscriptions**, and an Artifact Signing account is a billable Azure resource inside one.
  So there is nothing to sign with today. What has to happen, in order: an Azure subscription
  on a billing account whose type is **Organization** (the type is decided at subscription
  creation and it decides whether the certificate can carry the company name instead of a
  person's); an Artifact Signing account in a supported region; organisation identity
  validation (country-gated only, no company-age rule at GA, one named human still completes
  the phone ID scan; three document attempts, then that onboarding is dead); a Public Trust
  certificate profile; and a service principal granted *Artifact Signing Certificate Profile
  Signer* on it, which is the identity the three `AZURE_*` variables name. Billing starts the
  month the account is created whether or not validation completes.
- **Then the next release closes #30.** SmartScreen reputation is still earned per-publisher
  over time even with a signature; the signature is what stops the ML "unknown binary"
  verdicts and gives the false-positive submissions a name to attach to.

#### The 2026-07-18 position, kept for the record

Settled owner decision (2026-07-18), not an open trade-off at the time. It is recorded here
because it is the first thing anyone researching AV false positives will reach for, and the
reasoning still holds for everything except a tier-1 ML flag on the installer itself.

(This does not touch the **self-signed** cert for the MSIX sparse package - Windows will not
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
| MSIX only | Yes | **Dead end** - requires a trusted signature to install at all |
| MSI / WiX | Structurally, probably | **Unevidenced.** No before/after case study exists; [Tauri #4749](https://github.com/tauri-apps/tauri/issues/4749) had an unsigned MSI flagged *more* than its EXEs, and [Defender flags MSIs too](https://learn.microsoft.com/en-us/answers/questions/746120/msi-is-detected-as-a-virus-by-windows-defender). SmartScreen documents **no** MSI-vs-EXE distinction. Costs 2–4 days of WiX work, and the MSIX sparse package registers per-user, which fights a per-machine MSI. |
| Portable zip (no installer) | **Yes** | The only format with no stub at all - and tested: it drops from 2 detections to 1, trading the packed-stub hits for Bkav instead. Not zero, so **not shipped**; the installer remains the only distribution. |

The conclusion to hold onto: AV false positives are a tax on unsigned distribution, not a
property of Inno Setup. No format choice removes them.

### Report the false positives

Vendors act on these and it is free:

- ESET: <https://support.eset.com/en/kb141-submit-a-virus-website-or-potential-false-positive-sample-to-eset-lab> (or email `samples@eset.com`, subject prefixed `False positive`)
- Skyhigh/Trellix: <https://www.trellix.com/support/submit-sample/>
- Microsoft (if it ever flags us - it does not currently): <https://www.microsoft.com/en-us/wdsi/filesubmission>

Include the VirusTotal permalink, the download URL, and that the project is open-source at
<https://github.com/LunarWerxs/SageThumbs-2k>.

**Note for whoever handles SourceForge:** its listing reflects ESET's verdict. Getting the ESET
false positive retracted is what clears it; there is no separate SourceForge appeal to file.

## The update signature

A separate mechanism from everything above, and not about antivirus at all: it's what stops
the in-app updater from trusting a release it shouldn't.

**The problem.** Before this existed, the self-updater's only proof that a downloaded
installer was genuine was a sha256 digest carried in the same GitHub API response that named
the download URL. Anyone who could influence that one response - a compromised release, a
misdirected DNS/proxy, a malicious mirror - could hand the app a bad file with a digest that
matches itself. The digest checked "did the bytes arrive intact," never "did they come from
us."

**What is signed.** Every installer and portable zip gets a detached ed25519 signature at
release time (`examples/update-sign.rs`, run from `scripts/release.ps1`), uploaded as a
sibling release asset named `<file>.sig` - 128 lowercase hex characters, no framing. The
signature covers the exact bytes of the file it sits beside; nothing else (not the JSON, not
the filename) is signed or checked.

**Where the public key lives.** Baked into the app at compile time: `UPDATE_PUBLIC_KEY` in
`src/bin/app/update.rs`. The matching private key never ships - it lives only in whoever's
`.env` holds `ST2K_UPDATE_SIGNING_KEY`, generated once by `examples/update-keygen.rs` and
never printed or logged by anything in this repo.

**What the app refuses.** Before it ever launches a downloaded installer, the updater looks
for a `<installer-name>.sig` asset beside it in the same release. If that asset is missing, or
its content isn't 128 valid hex characters, or the signature doesn't verify against
`UPDATE_PUBLIC_KEY`, the update stops there and nothing runs - regardless of whether the size
and sha256 checks passed. There is no bypass for a release published without a signature; an
unsigned release simply cannot self-install.

**Key rotation.** Ship the new public key in a release that is itself signed with the OLD
key. A machine on an earlier build still trusts the old key at the moment it fetches that
release, verifies it, and installs the new binary - which is the one that now carries the new
`UPDATE_PUBLIC_KEY` and starts trusting the new key from then on. Skipping a signed handoff
release (jumping straight to signing with a brand-new key nothing yet trusts) strands every
installed copy: their compiled-in key will never verify anything again, and the update path
stops working silently rather than loudly. Note the private half separately somewhere durable
before rotating - there is no recovery for a lost signing key beyond that handoff step, run
once, by hand.
