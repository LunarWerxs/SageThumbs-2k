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

**`.github/workflows/winget.yml`** auto-publishes every GitHub release to
`microsoft/winget-pkgs`. It requires the `WINGET_TOKEN` repository secret and can only
**update** a package manifest that already exists in the winget-pkgs index.

The very first submission of a brand-new package ID can't go through that workflow (there's
nothing yet for it to update) and has to be done by hand once, with
[Komac](https://github.com/russellbanks/Komac):

```powershell
winget install RussellBanks.Komac
komac new LunarWerxs.SageThumbs2K --version <ver> --urls <installer-url>
```

**That first-submission step is already done** (`LunarWerxs.SageThumbs2K` was onboarded by
v0.10.0). Every release since then has gone through `winget.yml` automatically: cutting a
GitHub release fires the workflow, which opens a PR against `microsoft/winget-pkgs` titled
`New version: LunarWerxs.SageThumbs2K version <ver>`. There is no manual Komac step for
routine releases; Komac is only needed again if the package is ever re-created under a new ID.

---

## 4. Gotcha: `winget show` lags right after a release

The `winget.yml`-opened PR merges on Microsoft's own validation pipeline, which runs on their
schedule (typically hours, not minutes) and is outside this project's control. Until that PR
merges, `winget show LunarWerxs.SageThumbs2K` (and `winget upgrade`) will report the
**previous** version.

This is normal, not a broken pipeline. Before assuming a release didn't publish correctly,
check for an open PR against `microsoft/winget-pkgs` titled
`New version: LunarWerxs.SageThumbs2K version <ver>`. If it's there and unmerged, the
workflow did its job and the rest is just waiting on Microsoft's validation.
