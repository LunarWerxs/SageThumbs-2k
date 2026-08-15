# SageThumbs 2K: Packaging & Distribution

Why the project ships through **winget** instead of Scoop, how the winget listing stays
up to date, and the one gotcha that looks like a break but isn't.

---

## 1. Why winget, not Scoop

Scoop is a portable, no-admin package manager: it installs into a per-user versioned app
directory and expects to be able to swap that directory out from under the app on
`scoop update`. SageThumbs 2K doesn't fit that model:

- The installer is an **admin Inno Setup installer**, not a portable extraction.
- It runs `regsvr32` to register the shell-extension DLL with Explorer.
- It trusts a **self-signed certificate** into `LocalMachine\TrustedPeople` (needed for the
  modern Win11 context-menu's signed sparse package).
- **Explorer locks the shell-extension DLL while it's loaded.** A Scoop-style directory swap
  (`scoop update` replaces the versioned app folder) would collide with that lock and corrupt
  the update.

None of that is compatible with Scoop's portable/unprivileged install model, so Scoop was
considered and rejected. **winget** natively supports admin MSI/EXE installers with
silent-install switches, which matches how SageThumbs 2K already installs.

> The standalone **`st2k.exe`** CLI has none of these constraints (no admin, no DLL lock, no
> cert trust) and would fit a Scoop manifest fine if a portable-CLI-only distribution is ever
> requested. That's a separate package from the shell extension, not a workaround for it.

---

## 2. Current status: onboarded and live

The package is registered in the community `microsoft/winget-pkgs` repo as
**`LunarWerxs.SageThumbs2K`**. Install with:

```powershell
winget install LunarWerxs.SageThumbs2K
```

---

## 3. How releases reach winget

**`scripts/release.ps1` submits it, from this machine, at the end of a release.** It calls
`scripts/winget-submit.ps1`, which reads the release's own published sha256 digests, rewrites
the last published manifest triplet for the new version, syncs the `LunarWerxs/winget-pkgs`
fork, pushes a branch and opens the pull request against `microsoft/winget-pkgs`.

**There is no secret and no PAT.** It uses the local `gh` login, which already carries the
required scope. To submit (or re-submit) any published version by hand:

```powershell
pwsh scripts\winget-submit.ps1 -Version 1.12.0          # submit
pwsh scripts\winget-submit.ps1 -Version 1.12.0 -DryRun  # build the manifests and stop
```

It is idempotent and self-checking: it exits early if that version is already published
upstream or already has an open pull request, so re-running it is always safe.

### Why it is not a GitHub Action any more (2026-08-14)

It was, and it failed silently for a year. `winget.yml` ran Komac driven by a `WINGET_TOKEN`
secret, and:

- a **classic PAT expired** after 1.7.2, so 1.7.3 and 1.7.4 never published;
- the **fine-grained PAT** that replaced it on 2026-08-06 was answering **401 within two
  hours**;
- worst of all, the workflow's onboarding guard treated **any** non-200 answer as "package not
  onboarded yet" and skipped the whole job **with a green tick**. Nine consecutive releases
  (1.8.2 through 1.12.0) reported success while publishing nothing, and winget users sat on
  1.8.1 for a week, on a build whose own one-click updater was broken until 1.10.1.

The token was re-minted several times, and it could never have worked in the shape it was
asked to: a fine-grained PAT only carries permissions on repositories its owner **owns**, so
`POST /repos/microsoft/winget-pkgs/pulls` is 403 by construction, and a classic PAT is a
credential that expires and has to be re-issued forever. Needing a token at all was the
problem; the recurring "the token is dead again" was only the symptom.

`.github/workflows/winget.yml` is now a **manual, `workflow_dispatch`-only diagnostic** that
reports onboarding state and whether a given version has a pull request. It uses the built-in
`GITHUB_TOKEN` and needs no provisioning. It is deliberately **not** triggered by a release,
because `release.ps1` already submits and two submitters would open duplicate pull requests.
**The `WINGET_TOKEN` secret can be deleted; nothing reads it.**

The very first submission of a brand-new package ID still has to be done once by hand with
[Komac](https://github.com/russellbanks/Komac) (there is nothing yet for a version bump to be
based on):

```powershell
winget install RussellBanks.Komac
komac new LunarWerxs.SageThumbs2K --version <ver> --urls <installer-url>
```

**That step is already done** — `LunarWerxs.SageThumbs2K` was onboarded at v0.10.0. Komac is
only needed again if the package is ever re-created under a new ID.

---

## 4. Gotcha: `winget show` lags right after a release

The submitted PR merges on Microsoft's own validation pipeline, which runs on their
schedule (typically hours, not minutes) and is outside this project's control. Until that PR
merges, `winget show LunarWerxs.SageThumbs2K` (and `winget upgrade`) will report the
**previous** version.

This is normal, not a broken pipeline. Before assuming a release didn't publish correctly,
check for an open PR against `microsoft/winget-pkgs` titled
`New version: LunarWerxs.SageThumbs2K version <ver>`. If it's there and unmerged, the
workflow did its job and the rest is just waiting on Microsoft's validation.

---

## 5. SourceForge: the green Download button must be pointed at x64, every release

SourceForge picks its own "default download" per platform. Left alone it picks wrong, and the
way it is wrong is the expensive way: on 2026-08-05 the Windows default for v1.7.5 was
`SageThumbs2K-Setup-1.7.5-arm64.exe`. That is the file the big green button hands to every
visitor, and it will not run on the x64 machines that are nearly all of them. Confirmed
straight from the horse's mouth:

```
https://sourceforge.net/projects/sagethumbs-2k/best_release.json
  -> platform_releases.windows.filename
```

**Fix, and it has to be redone on every release:** SourceForge File Manager -> open the release
folder -> click the ⓘ (info) button on `SageThumbs2K-Setup-<ver>.exe` -> under **Default
Download For**, tick **Windows**. Then do the same on the `-arm64.exe` and make sure Windows is
**un**ticked there. The setting lives on the FILE, not on the project, so a new release starts
with a fresh set of files and no default carried over. That is why this recurs rather than
staying fixed.

Check it after every upload with the `best_release.json` URL above: it is one request and it
reports exactly what a visitor would be given. Do not check it by eye on the project page,
because SourceForge tailors that button to the visitor's own platform, so an x64 maintainer can
be shown the correct x64 file while everyone else is being handed ARM64.
