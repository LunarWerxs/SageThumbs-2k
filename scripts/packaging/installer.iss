; Inno Setup script for SageThumbs 2K.
; Built by scripts\build-release.ps1, which stages files into scripts\packaging\stage\
; and passes the version via /DAppVer.  Do not run this by hand - run the
; pipeline so the binaries + bundled ImageMagick are freshly staged.

#ifndef AppVer
  #define AppVer "0.0.0"
#endif
#ifndef Architecture
  #define Architecture "x64"
#endif
#ifndef StageDir
  #define StageDir "stage"
#endif
#ifndef CompactOnly
  #define CompactOnly "0"
#endif
#ifndef OutputSuffix
  #define OutputSuffix ""
#endif

; Keep the bare-ISCC developer path compatible with the existing x64 release.
; Release automation supplies these explicitly for each architecture.
#if (Architecture != "x64") && (Architecture != "arm64")
  #error Unsupported Architecture: pass /DArchitecture=x64 or /DArchitecture=arm64
#endif
#if (CompactOnly != "0") && (CompactOnly != "1")
  #error CompactOnly must be 0 or 1
#endif
#if (Architecture == "x64")
  ; ARM64 has its own native installer. Do not let the x64 variant install under
  ; emulation, where Explorer could not load its in-process shell extension.
  #define ArchitectureMatcher "x64compatible and not arm64"
#else
  #define ArchitectureMatcher "arm64"
#endif
; Live format count, injected by build-release.ps1 from `st2k formats` (never hardcode
; it - the count is whatever formats.rs FORMATS.len() returns). Count-free fallback so a
; bare ISCC compile still produces sensible text.
#ifndef FmtCount
  #define FmtCount "300+"
#endif

#define AppName "SageThumbs 2K"
#define AppExe "SageThumbs2K.exe"
#define AppDll "sagethumbs2k.dll"
; Publisher string must match the VERSIONINFO the Rust binaries carry EXACTLY (build.rs /
; windres write "LunarWerx"). An installer whose CompanyName disagrees with the binaries it
; drops is a classic dropper trait and reads as one to ML classifiers; it was "lunarwerx"
; here against "LunarWerx" in every payload PE.
#define Publisher "LunarWerx"

[Setup]
; One stable identity and directory cover both release architectures. They share
; COM CLSIDs and the sparse-package identity, so they cannot safely coexist; on an
; ARM64 machine the native installer upgrades/replaces any older emulated x64 copy.
; Keep these values shared so Inno finds and updates the same uninstall record.
AppId={{B0A1C2D3-E4F5-4607-8899-AABBCCDDEEFF}
AppName={#AppName}
AppVersion={#AppVer}
AppPublisher={#Publisher}
AppPublisherURL=https://github.com/LunarWerxs/SageThumbs-2k
DefaultDirName={autopf}\SageThumbs2K
UsePreviousAppDir=yes
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UninstallDisplayIcon={app}\app.ico
SetupIconFile={#StageDir}\app.ico
OutputDir=..\..\dist
OutputBaseFilename=SageThumbs2K-Setup-{#AppVer}{#OutputSuffix}
; The full payload contains three closely related Rust binaries plus a curated
; native codec tree. A 64 MiB dictionary keeps those repeated code regions in one
; solid window; /max's 8 MiB window evicts them and adds several megabytes without
; changing the installed files. 64 MiB is modest on supported Windows 10/11.
Compression=lzma2/ultra64
; Spend compile time, not runtime compatibility, on a denser match search.
LZMANumFastBytes=273
SolidCompression=yes
WizardStyle=modern
; Rich VERSIONINFO on Setup.exe - a metadata-less installer is heuristic-AV
; false-positive bait (same reason the binaries + magick stubs carry it).
; COMPLETE, truthful VERSIONINFO. This is not cosmetic: a PE with blank OriginalFilename /
; InternalName and a LegalCopyright that is not a copyright statement is exactly the sparse-
; metadata shape heuristic engines score toward malware. Measured precedent in this repo:
; giving a payload stub DLL a real VERSIONINFO moved it from 6/64 to 1/69 on VirusTotal
; (see build-release.ps1). Setup.exe had none of these filled in.
VersionInfoVersion={#AppVer}
VersionInfoProductVersion={#AppVer}
VersionInfoCompany={#Publisher}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} Setup
VersionInfoCopyright=(C) 2026 {#Publisher}
VersionInfoOriginalFileName=SageThumbs2K-Setup-{#AppVer}{#OutputSuffix}.exe
VersionInfoTextVersion={#AppVer}
AppCopyright=(C) 2026 {#Publisher}
ArchitecturesAllowed={#ArchitectureMatcher}
ArchitecturesInstallIn64BitMode={#ArchitectureMatcher}
; Shell-extension registration writes HKLM + Program Files -> needs elevation.
PrivilegesRequired=admin
MinVersion=10.0

; NO [Types] / [Components] SECTIONS, DELIBERATELY (2026-08-12).
;
; There used to be a Full / Compact / Custom choice, where Compact dropped ImageMagick and
; with it the tier-3 long tail of formats. It was removed: it made the product's headline
; claim ("all {#FmtCount} formats") conditional on a checkbox most people never understood,
; and a Compact user hitting an undecodable file had no way to know why. One payload now,
; always complete. Removing both sections is what suppresses Inno's Select-Components page -
; a single `fixed` component would still render the page with one greyed-out checkbox.
;
; `CompactOnly` survives ONLY as an internal BUILD switch (`build-release.ps1
; -NoImageMagick`) so CI jobs that do not care about the engine can skip staging the payload.
; It is not a shipped product tier and must never regain a user-facing selection.

[InstallDelete]
; ImageMagick is a curated, flattened payload. Inno upgrades in place and does
; not remove old files merely because a newer [Files] list omits them, so clear
; only the basenames/directories SageThumbs manages before repopulating. This
; also carries an existing Compact install forward: its half-populated engine
; directory is cleared here before the complete one is written.
Type: filesandordirs; Name: "{app}\modules"
Type: files; Name: "{app}\magick.exe"
Type: files; Name: "{app}\CORE_RL_*.dll"
Type: files; Name: "{app}\mfc140u.dll"
Type: files; Name: "{app}\msvcp140*.dll"
Type: files; Name: "{app}\vcomp140.dll"
Type: files; Name: "{app}\vcruntime140*.dll"
Type: files; Name: "{app}\colors.xml"
Type: files; Name: "{app}\configure.xml"
Type: files; Name: "{app}\delegates.xml"
Type: files; Name: "{app}\english.xml"
Type: files; Name: "{app}\locale.xml"
Type: files; Name: "{app}\log.xml"
Type: files; Name: "{app}\mime.xml"
Type: files; Name: "{app}\policy.xml"
Type: files; Name: "{app}\thresholds.xml"
Type: files; Name: "{app}\type-ghostscript.xml"
Type: files; Name: "{app}\type.xml"
Type: files; Name: "{app}\License.txt"
Type: files; Name: "{app}\NOTICE.txt"

[Files]
; Keep the DLL and CLI adjacent in the solid stream. Both are mostly the shared
; core, so adjacency also helps lower-memory tooling and keeps their repeated
; regions close in the solid stream.
; `restartreplace uninsrestartdelete` is the BACKSTOP behind `SwapAsideInUseDll` (see [Code]).
; The swap normally frees this name before [Files] runs, so neither flag fires; if a rename
; ever fails, these turn "setup cannot continue" into "reboot to finish" instead. The uninstall
; half matters for the same reason install does: a file manager holding the DLL would otherwise
; leave it behind. See issue #15.
Source: "{#StageDir}\{#AppDll}"; DestDir: "{app}"; Flags: ignoreversion restartreplace uninsrestartdelete
Source: "{#StageDir}\st2k.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
; The Open/Save-dialog selection reader. NOT a shell extension and never registered: the app
; loads it on demand, into the dialog's own process, for one question. Absent = the app simply
; has no dialog support, which is why this row is skipifsourcedoesntexist.
Source: "{#StageDir}\st2k_dlghook.dll"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; Signed sparse package + its public cert -> the Windows 11 modern context menu.
; Built by scripts\packaging\make-msix.ps1 (self-signed; skipped with -NoModernMenu).
Source: "{#StageDir}\SageThumbs2K.msix"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\SageThumbs2K.cer"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; Branding assets: icon (shortcut/uninstall) + swappable logo/banner overrides.
Source: "{#StageDir}\app.ico"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\logo.png"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\banner.png"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\LICENSE*"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
; The hardened policy ships unconditionally, ahead of the engine it constrains. The decoder
; can fall back to a separately-installed Program Files ImageMagick if our bundled one is
; missing or unusable, and that copy must be constrained too - so this row must NOT become
; conditional on the engine payload. The bundle's duplicate copy exists for exact staged
; testing and is excluded from the engine row below.
Source: "{#StageDir}\policy.xml"; DestDir: "{app}"; Flags: ignoreversion
; Bundled ImageMagick (magick.exe + DLLs + modules\).
#if CompactOnly == "0"
Source: "{#StageDir}\magick\*"; DestDir: "{app}"; Excludes: "policy.xml"; Flags: ignoreversion recursesubdirs createallsubdirs
#endif

[Icons]
Name: "{group}\SageThumbs 2K"; Filename: "{app}\{#AppExe}"; IconFilename: "{app}\app.ico"
Name: "{group}\Uninstall SageThumbs 2K"; Filename: "{uninstallexe}"

[Registry]
; Clean up the now-inert ModernMenuActive marker (written by <= 1.3.0). It used to gate
; whether the classic IContextMenu handler emitted ITS OWN quick verbs, on the false belief
; that Windows bridges the packaged (modern-compact-menu) verbs into "Show more options". It
; doesn't - packaged verbs live ONLY in the compact flyout - so that suppression just hid the
; quick verbs on every classic-menu-default machine. The classic handler now always shows its
; quick verbs (nothing reads this key anymore). Deleted on install so an upgrade from <= 1.3.0
; doesn't leave a dead value behind; the empty parent key is dropped on uninstall.
Root: HKLM; Subkey: "Software\SageThumbs2K"; ValueType: none; ValueName: "ModernMenuActive"; \
  Flags: deletevalue uninsdeletekeyifempty

[Run]
; Register the thumbnail provider + classic context-menu handler (HKLM).
Filename: "{sys}\regsvr32.exe"; Parameters: "/s ""{app}\{#AppDll}"""; \
  StatusMsg: "Registering the shell extension..."; Flags: runhidden waituntilterminated
; Modern Win11 context menu (signed sparse package): trust our self-signed cert
; (machine TrustedPeople - app packages only, not a root CA), then sideload the
; package bound to the install dir. ONE -NoProfile powershell call using native
; cmdlets (Import-Certificate + Add-AppxPackage) - deliberately NO -ExecutionPolicy
; Bypass (it only gates script *files*, never the inline cmdlets we pass via -Command)
; and NO certutil, so the installer doesn't resemble a script-dropper to AV heuristics.
; Runs only when the package was bundled.
; ADD FIRST, and remove only if that fails. A leftover DEV registration (unpackaged
; `Add-AppxPackage -Register`, Dev Mode) blocks the signed package with 0x80073CFB
; ("already installed an unpackaged version") and -ForceUpdateFromAnyVersion does NOT clear
; it, so the remove has to stay reachable - but it does not have to run EVERY time. Doing it
; unconditionally made every upgrade pay for two deployments, and this is the step users see
; as a hang ("Registering modern context menu - installation freezes", reported on 2.3.1).
; Measured on a warm box, min of 3 runs: 1,162 ms before, 552 ms after.
;
; It is also SAFER in this order. The old one deregistered the working package before doing
; anything else, so ANY later failure left the user with no modern menu at all. Now the
; existing registration is only torn down after an Add has already failed.
;
; The certificate import is skipped when that exact thumbprint is already trusted, which is
; the case on every upgrade. Compared by THUMBPRINT, never by subject: a rotated signing cert
; keeps the same subject, so a subject match would skip importing the new cert and break
; registration. X509Certificate2 is plain .NET and Cert:\ is a built-in provider, so the test
; is nearly free, while Import-Certificate drags in the whole PKI module.
;
; Single quotes only - no double quote appears anywhere inside the command - so the Inno
; string below needs no nested "" escaping that could go wrong.
;
; EVERY PowerShell BRACE IS DOUBLED, and it has to be. Inno reads `{` as the start of one of
; its own constants, so a PowerShell block like `catch{...}` is read as a constant named
; "catch" and the compile ABORTS with `Unknown constant`. `{{` is how Inno spells a literal
; brace; a closing `}` needs no escape. `{app}` stays single because it really is a constant.
; This is not theoretical: the first version of this line shipped with bare braces and broke
; the release build outright, which nothing but a full installer compile catches - hence
; `installer_iss::powershell_braces_are_escaped_for_inno`, which now catches it in a second.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -Command ""$t=(New-Object Security.Cryptography.X509Certificates.X509Certificate2 '{app}\SageThumbs2K.cer').Thumbprint; if(-not(Test-Path ('Cert:\LocalMachine\TrustedPeople\'+$t))){{Import-Certificate -FilePath '{app}\SageThumbs2K.cer' -CertStoreLocation Cert:\LocalMachine\TrustedPeople|Out-Null}; try{{Add-AppxPackage -Path '{app}\SageThumbs2K.msix' -ExternalLocation '{app}' -ForceUpdateFromAnyVersion -ErrorAction Stop}catch{{Get-AppxPackage -Name SageThumbs2K|Remove-AppxPackage -ErrorAction SilentlyContinue; Add-AppxPackage -Path '{app}\SageThumbs2K.msix' -ExternalLocation '{app}' -ForceUpdateFromAnyVersion}"""; \
  StatusMsg: "Registering the modern context menu (this can take a moment)..."; Flags: runhidden waituntilterminated; Check: ModernMenuUsable
; UPGRADE ONLY: suppress the first-run welcome window. Someone who already had SageThumbs
; installed has long since decided about Quick preview and the capture hotkey, and greeting
; them as a new user would silently re-offer (and, if they clicked through, re-enable)
; things they may have deliberately turned off. waituntilterminated, NOT nowait: the
; postinstall Settings launch below is what would show the window, so the flag has to be
; written before it starts. runasoriginaluser because the flag lives in the USER's HKCU,
; and setup itself is elevated.
Filename: "{app}\{#AppExe}"; Parameters: "--first-run-seen"; \
  Flags: runhidden waituntilterminated runasoriginaluser; Check: IsUpgrade
; Restart Explorer and drop thumbcache_*.db, so thumbnails appear for files the user has
; ALREADY browsed. Registering the provider does not invalidate anything Explorer cached,
; and for every one of our formats it has cached the generic icon it drew before we existed
; - so without this, a fresh install can look like it did nothing, which is exactly how it
; looked on a real machine (2026-08-05). Listed BEFORE the Settings launch so the shell is
; back up by the time the window appears.
;
; A checkbox, ticked by default, never silent: it closes the user's open Explorer windows,
; which is not something setup should do behind their back. `runasoriginaluser` because the
; thumbnail cache lives in the USER's LOCALAPPDATA, not the elevated installer's.
Filename: "{app}\{#AppExe}"; Parameters: "--rebuild-thumbnail-cache"; \
  Description: "Restart File Explorer to show thumbnails now (closes open Explorer windows)"; \
  Flags: postinstall nowait skipifsilent runasoriginaluser
; Launch Settings right after install (checked by default) so the user sees the app.
; `skipifsilent` keeps unattended installs quiet.
Filename: "{app}\{#AppExe}"; Description: "Open SageThumbs 2K Settings"; \
  Flags: postinstall nowait skipifsilent
; After a SILENT self-update (the running app launched setup with /UPDATED), relaunch the
; freshly installed app with --updated <ver> so it shows a "you're now on <ver>" note (and
; heals the hotkey daemon the setup had to kill). Gated on /UPDATED via WasSelfUpdate, so a
; normal interactive install never triggers it (and it runs even though that install was
; silent - no skipifsilent here, deliberately). runasoriginaluser is LOAD-BEARING: without
; it this runs in the ELEVATED setup context, and the daemon it heals would inherit that
; elevation - a non-elevated Settings window is then UIPI-blocked from ever posting
; WM_RELOAD to it, so later hotkey changes would silently stop applying.
Filename: "{app}\{#AppExe}"; Parameters: "--updated {#AppVer}"; \
  Flags: nowait runasoriginaluser; Check: WasSelfUpdate
; Restart the resident hotkey daemon after EVERY install, silent or not: the setup killed
; it (PrepareToInstall / Restart Manager) to replace the EXE, and nothing else brings it
; back until the next logon - a user whose hotkeys are on would otherwise find them dead
; after any reinstall/upgrade. --heal-hotkeys is a silent, instant no-op when the feature
; is off or the daemon is already back. Same runasoriginaluser rationale as above.
Filename: "{app}\{#AppExe}"; Parameters: "--heal-hotkeys"; \
  Flags: nowait runasoriginaluser
; Register the per-user update-check Scheduled Task ("SageThumbs2K.exe --update-check",
; daily with a 6h repetition, /rl LIMITED). Before this, the ONLY periodic update check
; lived inside the OPT-IN resident screenshot helper - so every install where the user
; never enabled screenshots got zero update checks, forever, while the default-on
; "Automatically check for updates" setting made it look wired up. The task is not a
; resident process: it starts, checks (throttled to once a day in code), toasts if there
; is something newer, and exits. runasoriginaluser is required - a task registered from
; the ELEVATED setup context would be owned by the wrong principal and could not toast
; into the user's session.
Filename: "{app}\{#AppExe}"; Parameters: "--update-task"; \
  Flags: nowait runasoriginaluser

[UninstallRun]
; Drop the update-check Scheduled Task first, while our EXE is still on disk. Harmless if
; it was never created (schtasks /delete /f on a missing task just fails quietly). NOTE:
; [UninstallRun] does NOT accept runasoriginaluser (ISCC rejects it outright) - but it does
; not need it either: the task lives in the machine-wide Task Scheduler library, so the
; elevated uninstaller can delete it regardless of which principal it runs as.
Filename: "{app}\{#AppExe}"; Parameters: "--update-task remove"; \
  Flags: runhidden waituntilterminated; RunOnceId: "DelUpdateTask"
; Remove the modern-menu package + its trusted cert (best-effort; harmless if the
; package was never installed). ONE -NoProfile powershell call with native cmdlets,
; no -ExecutionPolicy Bypass / certutil (see the [Run] note) so the uninstaller stays
; off AV heuristics too. Done before the DLL unregister/file removal.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -Command ""Get-AppxPackage -Name SageThumbs2K | Remove-AppxPackage; Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object Subject -like '*SageThumbs2K*' | Remove-Item -Force"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "UnregAppx"
; Unregister before files are removed (our DllUnregisterServer also unhooks every
; registered format and fires SHChangeNotify).
Filename: "{sys}\regsvr32.exe"; Parameters: "/u /s ""{app}\{#AppDll}"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "UnregSt2k"

[UninstallDelete]
; Tidy the per-user runtime files Windows would otherwise leave behind (diagnostics log +
; update-check cache in %LOCALAPPDATA%), so an uninstall leaves nothing stray on disk.
Type: files; Name: "{localappdata}\SageThumbs2K.log"
Type: files; Name: "{localappdata}\SageThumbs2K-update.txt"
; Any DLL copies parked aside by `SwapAsideInUseDll` on a machine that has not rebooted since.
; They are already scheduled for deletion on restart, so this is only a tidier immediate sweep;
; whichever is still mapped simply refuses and goes on the reboot list as before.
Type: files; Name: "{app}\{#AppDll}.old*"

[Code]
// Set by PrepareToInstall (the last moment it is knowable) and read by IsUpgrade.
var
  WasUpgrade: Boolean;

// The signed sparse package is bundled only when build-release.ps1 ran with the
// Windows SDK present (i.e. not -NoModernMenu). Gate the cert-trust + Appx
// registration on the .msix actually being there, so a classic-only build's
// installer doesn't try (and fail) to register a package it never shipped.
function ModernMenuBundled: Boolean;
begin
  Result := FileExists(ExpandConstant('{app}\SageThumbs2K.msix'));
end;

// The modern flyout is a Windows 11 feature: only Win11's Explorer surfaces a package's
// IExplorerCommand verbs. Build 22000 is the floor.
function IsWindows11: Boolean;
var
  V: TWindowsVersion;
begin
  GetWindowsVersionEx(V);
  Result := (V.Major > 10) or ((V.Major = 10) and (V.Build >= 22000));
end;

// Register the sparse package only where Explorer can surface its modern compact-menu
// commands. The legacy "Show more options" menu is independent: its classic handler
// always emits its own enabled quick verbs and does not read a package-active marker.
function ModernMenuUsable: Boolean;
begin
  Result := ModernMenuBundled and IsWindows11;
end;

// Post-install sanity check: did regsvr32 actually register us?
//
// The regsvr32 [Run] entry is `/s` (silent) and its exit code is not inspected, so before
// this check the installer would happily report "Setup completed successfully" while the
// shell extension was not registered at all -- the user then sees nothing working and has
// no idea why (issue #5). Reading back the thumbnail-provider CLSID is the cheapest true
// test that DllRegisterServer ran: our own DllRegisterServer writes it, nothing else does.
//
// This only REPORTS. It never fails the install -- a false alarm must not block a user
// whose machine is otherwise fine.
// Does the file on disk carry the version this setup ships? False = it was NOT replaced.
// Compares the numeric VS_FIXEDFILEINFO major.minor.rev, NOT a formatted string: Windows
// stores four components and Inno renders them as "1.7.1.0", so a string compare against
// AppVer ("1.7.1") would report a mismatch on every single install.
function FileIsCurrent(const FileName: String): Boolean;
var
  Major, Minor, Rev, Build: Word;
begin
  Result := True; // unreadable version info -> say nothing rather than cry wolf
  if GetVersionComponents(ExpandConstant('{app}\' + FileName), Major, Minor, Rev, Build) then
    Result := Format('%d.%d.%d', [Major, Minor, Rev]) = '{#AppVer}';
end;

// Did the files we just installed actually land? Returns the offending file name, or ''.
//
// An UPGRADE can "succeed" while replacing nothing: if a file is locked and Restart Manager
// cannot close the holder, Inno queues the replace for the next reboot, and nobody ever
// reboots. The user then runs the new setup, watches it finish, and is still on the old
// version - "version upgrades not working", reported 2026-08-02 by a user still on 1.4.1,
// with no clue why. Reading the version back off disk is the only way to know.
//
// This is the INTERACTIVE channel (a user who downloaded the setup and ran it). The SILENT
// self-update passes /SUPPRESSMSGBOXES, so its box never shows - that path is covered
// instead by the --updated relaunch, whose toast already compares the installer's version
// to the running image and says "restart Windows to finish" on a mismatch.
function StaleAfterInstall: String;
begin
  Result := '';
  if not FileIsCurrent('{#AppDll}') then
    Result := '{#AppDll}'
  else if not FileIsCurrent('{#AppExe}') then
    Result := '{#AppExe}';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Stale: String;
begin
  if CurStep = ssPostInstall then
  begin
    Stale := StaleAfterInstall;
    if Stale <> '' then
      MsgBox('SageThumbs 2K could not replace ' + Stale + ', so this PC is STILL RUNNING THE'
        + ' OLD VERSION.'
        + #13#10#13#10
        + 'Windows keeps that file open while File Explorer or another app is using it, and'
        + ' it can only be swapped on the next restart.'
        + #13#10#13#10
        + 'To finish the update: restart Windows. If the version still does not change after'
        + ' a restart, security software is most likely blocking the file - allow the install'
        + ' folder in your antivirus and run this installer again.',
        mbError, MB_OK);
    if not RegKeyExists(HKEY_CLASSES_ROOT,
         'CLSID\{7B2E6A14-9C3D-4F8A-B1E7-2A5D9F0C6E31}\InprocServer32') then
      MsgBox('SageThumbs 2K installed its files, but registering the shell extension with'
        + ' Windows did not succeed, so thumbnails and the right-click menu will not appear.'
        + #13#10#13#10
        + 'This is almost always security software blocking or quarantining'
        + ' sagethumbs2k.dll during setup.'
        + #13#10#13#10
        + 'To fix it: allow the install folder in your antivirus, then open SageThumbs 2K'
        + ' Settings and use Advanced > Repair file associations.',
        mbError, MB_OK);
  end;
end;

// Stop the resident hotkey/Quick Preview helper BEFORE any file is copied so its
// old EXE image cannot hold the destination open. The second sweep mops up a
// concurrent Settings/logon launch that slipped through the first pass's window.
// The [Run] --heal-hotkeys step brings the helper back from the NEW exe once the
// files are in place.
// Move an IN-USE shell-extension DLL out of the way so setup never has to ask anyone to close
// anything. Returns True if a swap was needed and done.
//
// ISSUE #15: "the installer has trouble automatically closing several processes/services, for
// me, Everything and Directory Opus. The Sage DLL also cannot be terminated automatically."
// That is not a permissions problem (setup is already elevated) and it is not really fixable by
// closing harder. ANY process that has opened a file dialog has loaded our shell extension and
// keeps it mapped until it exits; Directory Opus is an Explorer REPLACEMENT that loads shell
// extensions in-process exactly as Explorer does, and Everything runs as a service. Restart
// Manager can only close what cooperates, and quitting the user's file manager to install a
// thumbnail handler is a bad trade even when it works.
//
// The way around it is a Windows detail: a MAPPED dll cannot be deleted or overwritten, but it
// CAN be renamed. So move the old one aside, let [Files] write the new one into the now-free
// name, and schedule the renamed copy for deletion on the next reboot. Existing holders keep
// running against their old mapping until they exit, every process that loads it afterwards
// gets the new one, nothing has to be closed, and no reboot is needed to FINISH the install.
// This is the same technique the dev install script has used for a while for the same reason.
function SwapAsideInUseDll: Boolean;
var
  Dll, Aside: String;
  i: Integer;
begin
  Result := False;
  Dll := ExpandConstant('{app}\{#AppDll}');
  if not FileExists(Dll) then
    Exit;  // fresh install: nothing is holding anything
  // RENAME, never delete-then-recreate. An earlier version of this called DeleteFile first
  // "because the ordinary copy would have worked anyway", which quietly replaced Inno's
  // atomic in-place overwrite with a window where the DLL does not exist at all - on EVERY
  // upgrade, not just the in-use case this exists for. Setup failing anywhere between here
  // and the file copy would then leave registered COM CLSIDs pointing at a missing file.
  // A rename has no such window: the old file survives under a new name until it is replaced.
  for i := 0 to 20 do
  begin
    Aside := Dll + '.old' + IntToStr(i);
    // Sweep a leftover from an earlier swap on a machine that has not rebooted since. If it
    // deletes, the slot is reusable; if not, it is still mapped and the next index is tried.
    if FileExists(Aside) then
      DeleteFile(Aside);
    if not FileExists(Aside) then
      if RenameFile(Dll, Aside) then
      begin
        // Nothing was holding it after all: drop the copy now rather than leave a file behind
        // and a reboot-time deletion queued for an install that never needed either.
        if DeleteFile(Aside) then
          Result := False
        else
        begin
          // Still mapped. Empty destination = delete on restart. Best-effort: if that fails
          // the sweep above tidies it on the next install, which is why nothing gates on it.
          RestartReplace(Aside, '');
          Result := True;
        end;
        Exit;
      end;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  R: Integer;
begin
  Result := '';
  // Before anything else, and deliberately BEFORE the early-exit below: an in-use DLL is the
  // one thing that can fail this install outright, and it is just as likely on a repair as on
  // an upgrade. No-ops on a fresh install (the file does not exist yet).
  SwapAsideInUseDll;
  // Remember whether SageThumbs was ALREADY here, before any file is copied (afterwards the
  // exe always exists, so this is the only moment the answer is knowable). Drives IsUpgrade,
  // which suppresses the first-run welcome for someone who has used the app for months.
  WasUpgrade := FileExists(ExpandConstant('{app}\{#AppExe}'));
  // Only a PRIOR install can have a resident daemon locking our files, and only then is
  // the kill needed. Gating on the installed EXE existing means a FRESH install never
  // spawns taskkill at all. That also fixes unattended installs run from a headless
  // session (e.g. Windows Sandbox's LogonCommand), where spawning a console app like
  // taskkill from a windowless parent can deadlock and hang setup before any file copy.
  if not FileExists(ExpandConstant('{app}\{#AppExe}')) then
    Exit;
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, R);
  Sleep(400);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, R);
end;

// Was SageThumbs already installed when this setup started? Set by PrepareToInstall, which
// is the last moment the answer exists. Gates the --first-run-seen [Run] entry.
function IsUpgrade: Boolean;
begin
  Result := WasUpgrade;
end;

// True when the running app launched this setup as a SILENT self-update - it passes the
// custom /UPDATED switch. Gates the post-update "you're now on <ver>" relaunch so a normal
// interactive install never shows it.
function WasSelfUpdate: Boolean;
var
  i: Integer;
begin
  Result := False;
  for i := 1 to ParamCount do
    if CompareText(ParamStr(i), '/UPDATED') = 0 then
    begin
      Result := True;
      Exit;
    end;
end;

// The "why are you leaving?" answer collected by the uninstall survey (AskUninstallReason),
// read by NotifyUninstall. Reason is a short bucket key; Note and Contact are optional text.
var
  UninstallReason: String;
  UninstallNote: String;
  UninstallContact: String;

// Percent-encode a string for safe use as a URL query value. ASCII only - any non-ASCII
// char is dropped rather than mis-encoded (the survey note is best-effort, not exact text).
function UrlEncode(const S: String): String;
var
  i, Code: Integer;
  C: Char;
begin
  Result := '';
  for i := 1 to Length(S) do begin
    C := S[i];
    if ((C >= 'A') and (C <= 'Z')) or ((C >= 'a') and (C <= 'z')) or
       ((C >= '0') and (C <= '9')) or (C = '-') or (C = '_') or (C = '.') or (C = '~') then
      Result := Result + C
    else begin
      Code := Ord(C);
      if Code <= 127 then
        Result := Result + '%' + Format('%.2x', [Code]);
      // non-ASCII: dropped on purpose (avoids corrupting multi-byte chars w/o a UTF-8 encoder)
    end;
  end;
end;

// Is this a plausible single email address? The reply field used to accept "email or other
// contact", and in practice people typed their COMPLAINT into it instead of an address, so
// the field produced unreachable rows and lost the text (it is not shown as the message).
// It is email-only now, and this is the gate. Deliberately structural rather than RFC-exact:
// it rejects the junk that actually shows up (prose, "n/a", bare handles) without bouncing
// an address a real person owns. Empty is still fine - the whole field stays optional.
function LooksLikeEmail(const S: String): Boolean;
var
  i, At, Dot, L: Integer;
  C: Char;
begin
  Result := False;
  L := Length(S);
  // a@b.co is the shortest thing worth accepting; 120 matches the field's MaxLength.
  if (L < 6) or (L > 120) then Exit;
  At := 0;
  for i := 1 to L do begin
    C := S[i];
    if C = '@' then begin
      if At <> 0 then Exit;                     // a second '@' - not one address
      At := i;
    end
    // Anything outside printable ASCII (spaces and control chars included) means this is
    // prose or a handle, not an address. Listed out rather than using Pos with a Char so
    // no implicit Char->String conversion is relied on.
    else if (C <= ' ') or (C > #126) then Exit
    else if (C = '(') or (C = ')') or (C = '[') or (C = ']') or (C = '{') or (C = '}') or
            (C = ',') or (C = ';') or (C = ':') or (C = '"') or (C = '<') or (C = '>') or
            (C = '\') or (C = '/') then Exit;
  end;
  if (At < 2) or (At = L) then Exit;            // needs a local part AND a domain
  if At - 1 > 64 then Exit;                     // local part cap (RFC 5321)
  if S[At + 1] = '-' then Exit;                 // domain can't start with '-'
  if S[L] = '-' then Exit;                      // ...or end with one
  // The domain needs a dot that is neither first nor last, no empty labels, and a TLD of
  // two or more letters - which is what rules out "me@localhost" and "me@1".
  Dot := 0;
  for i := At + 1 to L do
    if S[i] = '.' then begin
      if i = At + 1 then Exit;                  // domain starts with '.'
      if (Dot > 0) and (i = Dot + 1) then Exit; // '..'
      Dot := i;
    end;
  if (Dot = 0) or (Dot = L) then Exit;
  if L - Dot < 2 then Exit;
  for i := Dot + 1 to L do begin
    C := S[i];
    if not (((C >= 'A') and (C <= 'Z')) or ((C >= 'a') and (C <= 'z'))) then Exit;
  end;
  Result := True;
end;

// The survey's reply field, its inline warning, and the Send button, at script scope ONLY so
// the OnChange handler below can reach them (Pascal Script has no closures).
var
  SurveyContact: TNewEdit;
  SurveyWarn: TNewStaticText;
  SurveySend: TNewButton;

// Live validation, re-run on every keystroke in the email box: a malformed address disables
// Send and shows an inline reason, and clearing the box re-enables it. This shape was chosen
// over vetoing the close (TSetupForm.ModalResult and TWinControl.SetFocus are BOTH absent from
// Pascal Script - ISCC rejects them, verified) and over re-showing the modal, because it needs
// no assumption about VCL's button/close ordering, which an uninstaller-only dialog gives no
// way to test. SKIP is deliberately never disabled, so this can never wedge an uninstall.
procedure SurveyContactChange(Sender: TObject);
var
  Bad: Boolean;
  C: String;
begin
  C := Trim(SurveyContact.Text);
  Bad := (C <> '') and not LooksLikeEmail(C);
  SurveyWarn.Visible := Bad;
  SurveySend.Enabled := not Bad;
end;

// A small, skippable "why are you uninstalling?" survey shown right before removal.
// Pure-Win32 modal (no browser); fills UninstallReason/UninstallNote/UninstallContact.
// Either button lets the uninstall proceed - Skip just leaves them empty. Never shown on a
// silent uninstall.
procedure AskUninstallReason;
var
  F: TSetupForm;
  Lbl, NoteLbl, ReadLbl, ContactLbl: TNewStaticText;
  Radios: array[0..6] of TNewRadioButton;
  Note, Contact: TNewEdit;
  BtnSend, BtnSkip: TNewButton;
  Keys, Texts: array[0..6] of String;
  i, y: Integer;
begin
  UninstallReason := '';
  UninstallNote := '';
  UninstallContact := '';

  Keys[0] := 'buggy';       Texts[0] := 'It did not work - no thumbnails, errors, or crashes';
  Keys[1] := 'slow';        Texts[1] := 'Too slow or used too much memory / CPU';
  Keys[2] := 'missing';     Texts[2] := 'Missing a file format or feature I needed';
  Keys[3] := 'alternative'; Texts[3] := 'Found a better alternative';
  Keys[4] := 'temporary';   Texts[4] := 'Just trying it out / only needed it temporarily';
  Keys[5] := 'confusing';   Texts[5] := 'Too confusing or hard to use';
  Keys[6] := 'other';       Texts[6] := 'Other (please tell us below)';

  // MUST be CreateCustomForm, NOT TSetupForm.Create(nil): the uninstaller binary carries no
  // TSetupForm DFM resource, so TSetupForm.Create there dies with a fatal "Resource TSetupForm
  // not found" runtime error and aborts the whole uninstall (issue #3, Win11). CreateCustomForm
  // builds the form via CreateNew (no resource lookup) and works in Setup AND the uninstaller.
  // Client size is a construction arg (read-only afterward since Inno 6.6.0); the two True flags
  // keep both dimensions fixed (no autosize) for this fixed-layout dialog.
  F := CreateCustomForm(ScaleX(470), ScaleY(420), True, True);
  try
    F.Caption := 'SageThumbs 2K';
    // Native look: the modern UI font. CreateCustomForm already inits Setup's dialog font;
    // pin Segoe UI explicitly. Set BEFORE creating children so labels/radios/buttons inherit it.
    F.Font.Name := 'Segoe UI';
    F.Font.Size := 9;
    F.Position := poScreenCenter;
    F.BorderStyle := bsDialog;

    Lbl := TNewStaticText.Create(F);
    Lbl.Parent := F;
    Lbl.Left := ScaleX(16);
    Lbl.Top := ScaleY(14);
    Lbl.Width := F.ClientWidth - ScaleX(32);
    Lbl.AutoSize := False;
    Lbl.WordWrap := True;
    Lbl.Height := ScaleY(38);
    Lbl.Caption := 'Sorry to see you go! Mind telling us why you''re uninstalling?' + #13#10 +
      'It''s optional. Add your email only if you''d like us to reply.';

    y := ScaleY(58);
    for i := 0 to 6 do begin
      Radios[i] := TNewRadioButton.Create(F);
      Radios[i].Parent := F;
      Radios[i].Left := ScaleX(18);
      Radios[i].Top := y;
      Radios[i].Width := F.ClientWidth - ScaleX(36);
      Radios[i].Caption := Texts[i];
      y := y + ScaleY(23);
    end;

    NoteLbl := TNewStaticText.Create(F);
    NoteLbl.Parent := F;
    NoteLbl.Left := ScaleX(16);
    NoteLbl.Top := y + ScaleY(6);
    NoteLbl.Caption := 'Anything else? (optional)';

    Note := TNewEdit.Create(F);
    Note.Parent := F;
    Note.Left := ScaleX(16);
    Note.Top := y + ScaleY(24);
    Note.Width := F.ClientWidth - ScaleX(32);
    Note.MaxLength := 200;

    ReadLbl := TNewStaticText.Create(F);
    ReadLbl.Parent := F;
    ReadLbl.Left := ScaleX(16);
    ReadLbl.Top := y + ScaleY(52);
    ReadLbl.Caption := 'We read every one of these messages.';
    ReadLbl.Font.Style := [fsBold];

    ContactLbl := TNewStaticText.Create(F);
    ContactLbl.Parent := F;
    ContactLbl.Left := ScaleX(16);
    ContactLbl.Top := y + ScaleY(76);
    ContactLbl.Caption := 'Your email (optional - only if you''d like a reply)';

    Contact := TNewEdit.Create(F);
    Contact.Parent := F;
    Contact.Left := ScaleX(16);
    Contact.Top := y + ScaleY(94);
    Contact.Width := F.ClientWidth - ScaleX(32);
    Contact.MaxLength := 120;

    // Inline reason for a disabled Send button. Hidden until the address is actually bad, so
    // the dialog looks unchanged for the (large) majority who leave the field empty. It sits
    // in the gap between the email box and the button row - no layout reflow needed.
    SurveyWarn := TNewStaticText.Create(F);
    SurveyWarn.Parent := F;
    SurveyWarn.Left := ScaleX(16);
    SurveyWarn.Top := y + ScaleY(118);
    SurveyWarn.Width := F.ClientWidth - ScaleX(32);
    SurveyWarn.AutoSize := False;
    SurveyWarn.WordWrap := True;
    SurveyWarn.Height := ScaleY(30);
    SurveyWarn.Caption := 'That is not a valid email address. Fix it, or clear the box to send' + #13#10 +
      'without one - put your message in the box above instead.';
    SurveyWarn.Font.Color := clRed;
    SurveyWarn.Visible := False;

    BtnSend := TNewButton.Create(F);
    BtnSend.Parent := F;
    BtnSend.Width := ScaleX(130);
    BtnSend.Height := ScaleY(28);
    BtnSend.Top := F.ClientHeight - ScaleY(40);
    BtnSend.Left := F.ClientWidth - ScaleX(146);
    BtnSend.Caption := 'Send feedback';
    BtnSend.ModalResult := mrOk;
    BtnSend.Default := True;

    // Wire live email validation only once BOTH the warning label and Send exist - OnChange
    // touches both, and TNewEdit fires it on the first keystroke, not before.
    SurveyContact := Contact;
    SurveySend := BtnSend;
    Contact.OnChange := @SurveyContactChange;

    BtnSkip := TNewButton.Create(F);
    BtnSkip.Parent := F;
    BtnSkip.Width := ScaleX(100);
    BtnSkip.Height := ScaleY(28);
    BtnSkip.Top := BtnSend.Top;
    BtnSkip.Left := BtnSend.Left - ScaleX(108);
    BtnSkip.Caption := 'Skip';
    BtnSkip.ModalResult := mrCancel;
    BtnSkip.Cancel := True;

    if F.ShowModal = mrOk then begin
      for i := 0 to 6 do
        if Radios[i].Checked then
          UninstallReason := Keys[i];
      UninstallNote := Trim(Note.Text);
      UninstallContact := Trim(Contact.Text);
      // Belt and braces. Send is already disabled while this is invalid, so reaching here with
      // junk would take a VCL quirk - but the guarantee that ships is "the contact field holds
      // an email or nothing", and that must not depend on a button's Enabled state.
      if (UninstallContact <> '') and not LooksLikeEmail(UninstallContact) then
        UninstallContact := '';
    end;
  finally
    F.Free;
  end;
end;

// Best-effort one-shot HTTPS GET on uninstall, over WinHttp with short timeouts and all
// errors swallowed so it never blocks or slows the uninstall. Only a real uninstall
// reaches it - an in-place upgrade does not run the uninstaller. Carries the optional
// survey answer (reason bucket + note + optional reply contact) from the uninstall prompt.
procedure NotifyUninstall;
var
  Http: Variant;
  Url: String;
  DevFlag: Cardinal;
begin
  try
    Url := 'https://st2k.lunarwerx.com/sponsor?uninstall=1&v={#AppVer}';
    // The developer's own test box (HKCU DevMachine=1) tags the request with &dev=1. The
    // subtree is still present here (it's deleted AFTER this), so read it before the delete.
    if RegQueryDWordValue(HKEY_CURRENT_USER, 'Software\SageThumbs2K', 'DevMachine', DevFlag) and (DevFlag = 1) then
      Url := Url + '&dev=1';
    if UninstallReason <> '' then
      Url := Url + '&reason=' + UninstallReason;
    if UninstallNote <> '' then
      Url := Url + '&note=' + UrlEncode(UninstallNote);
    if UninstallContact <> '' then
      Url := Url + '&contact=' + UrlEncode(UninstallContact);
    Http := CreateOleObject('WinHttp.WinHttpRequest.5.1');
    // resolve, connect, send, receive (ms) - capped so a dead network fails fast.
    Http.SetTimeouts(1500, 1500, 1500, 2000);
    Http.Open('GET', Url, False);
    Http.SetRequestHeader('User-Agent', 'SageThumbs2K-Uninstaller');
    Http.Send('');
  except
    // best-effort only - never surface or block on failure.
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then begin
    // Ask why first (interactive uninstalls only), then send the optional survey answer.
    if not UninstallSilent then
      AskUninstallReason;
    NotifyUninstall;
    // Tidy the per-user leftovers Windows keeps on uninstall: drop our whole HKCU settings
    // subtree, then leave only a tiny marker noting the version last installed.
    RegDeleteKeyIncludingSubkeys(HKEY_CURRENT_USER, 'Software\SageThumbs2K');
    RegWriteStringValue(HKEY_CURRENT_USER, 'Software\SageThumbs2K', 'Tombstone', '{#AppVer}');
    // The screenshot daemon's autostart entry lives in a SEPARATE registry location
    // (…\CurrentVersion\Run, not our own Software\SageThumbs2K subtree — see
    // screenshot/enable.rs's RUN_KEY/RUN_NAME), so the delete above never touches it. Left
    // behind, it points at the now-deleted exe and fails silently at every logon. Mirrors
    // install.ps1's dev-uninstaller, which already removes this same value.
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'SageThumbs2KScreenshot');
  end;
end;
