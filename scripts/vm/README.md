# VM / clean-room testing

Two ways to test SageThumbs 2K on a machine that has never seen it. Use these to
reproduce "not working on a fresh install" reports and to sanity-check installer
behavior (SmartScreen, Defender) without touching your dev box.

## 1. Windows Sandbox — instant, throwaway (`test-sandbox.ps1`)

```powershell
pwsh scripts\vm\test-sandbox.ps1
```

Boots a disposable Windows instance in seconds, maps the newest `dist\` installer and
the `test-corpus` in read-only, opens Explorer so you can install and immediately
right-click samples. Close it and everything is gone.

**Good for:** first-run install flow, SmartScreen/Defender reaction on a pristine box,
"does a clean machine get thumbnails at all."

**Cannot test:** Windows 10-specific behavior. Sandbox is built from the HOST OS, so on
this Win11 machine it is a Win11 guest. The modern-menu-on-Win10 and N/KN-edition bugs
would NOT show up here.

## 2. Hyper-V Windows 10 VM — the real Win10 target

The Sandbox is Win11 (it mirrors the host), so it CANNOT reproduce Windows 10. issue #5's
reporter is on Win10 Home 22H2, so this builds exactly that and runs the same clean-room
test on it.

**Get the ISO once** (put it at `D:\isos\Win10_22H2_x64.iso`):
```powershell
# official MS consumer ISO URL via Fido, then download:
Invoke-WebRequest 'https://github.com/pbatard/Fido/raw/master/Fido.ps1' -OutFile D:\isos\Fido.ps1
$u = powershell -ExecutionPolicy Bypass -File D:\isos\Fido.ps1 -Win 10 -Rel 22H2 -Ed 'Home/Pro' -Lang English -Arch x64 -GetUrl
Start-BitsTransfer -Source $u -Destination D:\isos\Win10_22H2_x64.iso
```

**`run-win10-test.ps1`** — fully automated, elevated. Builds + tests in one shot:
```powershell
# elevated PowerShell:
.\scripts\vm\run-win10-test.ps1 -Iso D:\isos\Win10_22H2_x64.iso
.\scripts\vm\run-win10-test.ps1 -Resume     # reuse an already-applied VHDX (skips the ~12 min DISM apply)
```
It partitions a VHDX, DISM-applies "Windows 10 Home" (index 1), writes UEFI boot files, and
drops `autounattend-win10.xml` at `Windows\Panther\unattend.xml` so first boot installs
unattended (local admin `vmadmin`, auto-logon). No DVD boot, no "press any key", no WinPE
Setup UI. Then it drives the test over **PowerShell Direct** (no guest network needed):
silent-install the built installer, `st2k doctor`, thumbnail a modern `.xcf` + a `.zip`, and
copy the results back to `D:\isos\win10-test-results\` (a PASS/FAIL + the produced PNGs).
Pass `-Keep` to leave the VM up (`vmconnect localhost st2k-win10`); default tears it down.
A FAILED run keeps the applied VHDX so the next attempt can `-Resume`; only a PASS reclaims it.

Whole run from `-Resume`: ~80 seconds (bcdboot 3 s, guest boots to PowerShell Direct in ~60 s,
test ~10 s). A cold run adds the DISM apply, ~12 min.

**Result 2026-07-20 (v1.3.0): PASS on Windows 10 Home 22H2 build 19045.** Silent install exit 0;
all four coclasses registered *and* `LoadLibrary`-probed OK; 326/326 formats hooked; decode
self-test passed; a modern GIMP `.xcf` decoded (750x1624) and an image `.zip` produced its
collage. So issue #5's "no thumbnails on Win10" is not a Win10 registration problem.

### This is a RELEASE-CHECKLIST step, and the repo says so publicly

Run it before shipping a release. issue #5 is closed with a public commitment that a fresh
Win10 VM install + decode is checked before every release, so keep that true.

Note the twist worth remembering: issue #5's actual bug (modern GIMP `.xcf`) was **not**
OS-specific at all. It failed identically on Windows 11 — the bundled ImageMagick cannot read
XCF written by GIMP 2.10/3, and `magick.exe` returns
`not enough pixel data @ error/xcf.c/ReadXCFImage/1495` on either OS. The reporter's VM was
Win10 by coincidence, and the genuinely Win10-specific bugs earlier in the same thread (the
Repair "network path" error, the missing modern-menu entries) made it look like one story.
The clean-room run stays in the checklist anyway, because what actually went wrong was
discounting a clean-VM reproduction against "hundreds of installs, no complaints." A 90-second
automated run removes the temptation to make that argument.

(`new-win10-vm.ps1` is the older interactive variant — creates the VM + boots the ISO for a
hands-on install. Prefer `run-win10-test.ps1` for the automated end-to-end test.)

### Three traps that cost hours here — all fixed in the script, don't re-discover them

1. **Never use the HOST's `bcdboot` for a Windows 10 image.** On a Win11 24H2+ host with Secure
   Boot on and the 2023 PCA in the Secure Boot DB, `bfsvc` decides it must service the "Ex"
   (2023-signed) boot binaries and looks for `<win>\Boot\EFI_EX\bootmgfw_EX.efi` — a file
   Windows 10 never shipped. It fails with `Failed to validate boot manager checksum … 0xc1`
   and **exit 193**, leaving a VM that boots to *"The boot loader did not load an operating
   system."* `BFSVC_USE_EX_BINS` is **not** an environment variable (it still logs `:y` when you
   set it to `0`), so there is no switch. Fix: copy `bcdboot.exe` + `bfsvc.dll` out of the
   applied image and run **the image's own** bcdboot. Works first try.
2. **`diskpart assign letter=` cannot letter the ESP.** It is a *hidden* system partition, and
   diskpart refuses every letter with the very misleading `The specified drive letter is not
   free to be assigned` — which reads like a host-wide drive-letter outage and sends you off
   restarting VDS and enabling automount for nothing. `Add-PartitionAccessPath -AssignDriveLetter`
   assigns it without complaint.
3. **Verify a drive letter via the partition's `AccessPaths`, never `Test-Path`.** A letter that
   already points somewhere *else* makes `Test-Path X:\` return true, and you then bcdboot into
   the wrong volume. (Also: a re-mounted VHDX silently gets its previous letters back from
   `MountedDevices`, so check for an existing letter before assigning one.)

Corollary for anything copying out of an applied image: `Copy-Item` fails **silently** on
TrustedInstaller-owned paths like `<win>\Windows\Boot\`. Use `robocopy` and check its exit code,
or you will blame the wrong component.

---

## The "flagged as a virus" question (installer false positives)

This is a **reputation / heuristic** flag, not actual malware, and it is expected for a
brand-new unsigned installer. Why the *original* SageThumbs installs clean and ours gets
flagged:

* **Age and prevalence, not signing.** The original `sagethumbs_2.0.0.23_setup.exe` is a
  ~15-year-old NSIS installer that has been downloaded by millions and is **itself
  unsigned** (verified: `Get-AuthenticodeSignature` = NotSigned). SmartScreen and most AV
  engines carry reputation/prevalence data: a binary seen safely for over a decade is
  effectively whitelisted. A newly built binary with zero history is "unknown", and
  unknown + installer-behavior scores as suspicious by default. So signing is not what
  separates them; **reputation is.**
* **What raises OUR heuristic score** (beyond just being new): an installer that writes to
  `Program Files` + registers shell extensions + (in our case) an updater that can download
  and launch a newer installer. That download-and-execute shape is exactly what generic
  "dropper/downloader" heuristics look for. It is legitimate here, but engines can't tell.

**Non-signing mitigations** (in rough order of payoff):
1. **Submit the false positive.** Microsoft: https://www.microsoft.com/wdsi/filesubmission
   (Defender is the one most users hit). Repeat for any other engine a user names. Turnaround
   is usually a few days and it clears the specific hash.
2. **Publish a VirusTotal link** on the release so a user can see it's e.g. 1/72 and which
   engine, instead of trusting a single scary popup. Transparency defuses most reports.
3. **Offer a portable ZIP** alongside the installer (just the DLL + EXEs + a register .bat).
   No installer wrapper means far fewer heuristics fire; power users prefer it anyway.
4. **Let reputation accrue.** Each clean download on the same stable hash lowers the score
   over time; re-releasing a new hash every few days resets that clock, so avoid churny
   re-uploads of the installer.

(Deliberately not listed: code signing. It's a standing project decision not to go that
route; the items above are the non-signing path.)
