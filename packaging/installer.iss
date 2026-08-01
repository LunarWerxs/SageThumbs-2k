; Inno Setup script for SageThumbs 2K.
; Built by scripts\build-release.ps1, which stages files into packaging\stage\
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
; it — the count is whatever formats.rs FORMATS.len() returns). Count-free fallback so a
; bare ISCC compile still produces sensible text.
#ifndef FmtCount
  #define FmtCount "300+"
#endif

#define AppName "SageThumbs 2K"
#define AppExe "SageThumbs2K.exe"
#define AppDll "sagethumbs2k.dll"
#define Publisher "lunarwerx"

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
OutputDir=..\dist
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
VersionInfoVersion={#AppVer}
VersionInfoProductVersion={#AppVer}
VersionInfoCompany={#Publisher}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} Setup
VersionInfoCopyright=SageThumbs 2K
ArchitecturesAllowed={#ArchitectureMatcher}
ArchitecturesInstallIn64BitMode={#ArchitectureMatcher}
; Shell-extension registration writes HKLM + Program Files -> needs elevation.
PrivilegesRequired=admin
MinVersion=10.0

[Types]
#if (Architecture == "x64") && (CompactOnly == "0")
Name: "full"; Description: "Full - all {#FmtCount} formats (recommended)"
#endif
Name: "compact"; Description: "Compact - common formats only (no ImageMagick)"
Name: "custom"; Description: "Custom"; Flags: iscustom

[Components]
#if (Architecture == "x64") && (CompactOnly == "0")
Name: "core"; Description: "SageThumbs 2K shell extension + Options"; Types: full compact custom; Flags: fixed
#else
Name: "core"; Description: "SageThumbs 2K shell extension + Options"; Types: compact custom; Flags: fixed
#endif
#if (Architecture == "x64") && (CompactOnly == "0")
Name: "magick"; Description: "ImageMagick engine - 100+ extra formats"; Types: full custom
#endif

[InstallDelete]
; ImageMagick is a curated, flattened payload. Inno upgrades in place and does
; not remove old files merely because a newer [Files] list omits them, so clear
; only the basenames/directories SageThumbs manages before repopulating a Full
; install. These entries intentionally have no Components condition: switching
; from Full to Compact must remove the complete prior engine as well.
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
Source: "{#StageDir}\{#AppDll}"; DestDir: "{app}"; Flags: ignoreversion; Components: core
Source: "{#StageDir}\st2k.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "{#StageDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion; Components: core
; Signed sparse package + its public cert -> the Windows 11 modern context menu.
; Built by packaging\make-msix.ps1 (self-signed; skipped with -NoModernMenu).
Source: "{#StageDir}\SageThumbs2K.msix"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "{#StageDir}\SageThumbs2K.cer"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
; Branding assets: icon (shortcut/uninstall) + swappable logo/banner overrides.
Source: "{#StageDir}\app.ico"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "{#StageDir}\logo.png"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "{#StageDir}\banner.png"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "{#StageDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "{#StageDir}\LICENSE*"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
; The hardened policy is core: Compact can deliberately fall back to a Program
; Files ImageMagick and must constrain it too. The full bundle's duplicate copy
; exists for exact staged testing but is excluded from this second installer row.
Source: "{#StageDir}\policy.xml"; DestDir: "{app}"; Flags: ignoreversion; Components: core
; Bundled ImageMagick (magick.exe + DLLs + modules\).
#if (Architecture == "x64") && (CompactOnly == "0")
Source: "{#StageDir}\magick\*"; DestDir: "{app}"; Excludes: "policy.xml"; Flags: ignoreversion recursesubdirs createallsubdirs; Components: magick
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
; Remove-first: a leftover DEV registration (unpackaged `Add-AppxPackage -Register`, Dev
; Mode) blocks the signed package with 0x80073CFB ("already installed an unpackaged
; version") — and -ForceUpdateFromAnyVersion does NOT clear that. Removing any existing
; registration first makes the step idempotent for dev boxes and stuck states alike; on a
; clean end-user upgrade the remove is a harmless no-op-or-quick-swap.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -Command ""Get-AppxPackage -Name SageThumbs2K | Remove-AppxPackage -ErrorAction SilentlyContinue; Import-Certificate -FilePath '{app}\SageThumbs2K.cer' -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null; Add-AppxPackage -Path '{app}\SageThumbs2K.msix' -ExternalLocation '{app}' -ForceUpdateFromAnyVersion"""; \
  StatusMsg: "Registering the modern context menu..."; Flags: runhidden waituntilterminated; Check: ModernMenuUsable
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

[UninstallRun]
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

[Code]
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
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
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
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  R: Integer;
begin
  Result := '';
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
      'It''s optional. Add contact details only if you''d like us to reply.';

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
    ContactLbl.Caption := 'Email or other contact (optional, if you''d like a reply)';

    Contact := TNewEdit.Create(F);
    Contact.Parent := F;
    Contact.Left := ScaleX(16);
    Contact.Top := y + ScaleY(94);
    Contact.Width := F.ClientWidth - ScaleX(32);
    Contact.MaxLength := 120;

    BtnSend := TNewButton.Create(F);
    BtnSend.Parent := F;
    BtnSend.Width := ScaleX(130);
    BtnSend.Height := ScaleY(28);
    BtnSend.Top := F.ClientHeight - ScaleY(40);
    BtnSend.Left := F.ClientWidth - ScaleX(146);
    BtnSend.Caption := 'Send feedback';
    BtnSend.ModalResult := mrOk;
    BtnSend.Default := True;

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
  end;
end;
