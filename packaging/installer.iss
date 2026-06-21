; Inno Setup script for SageThumbs 2K.
; Built by scripts\build-release.ps1, which stages files into packaging\stage\
; and passes the version via /DAppVer.  Do not run this by hand — run the
; pipeline so the binaries + bundled ImageMagick are freshly staged.

#ifndef AppVer
  #define AppVer "0.0.0"
#endif

#define AppName "SageThumbs 2K"
#define AppExe "sagethumbs2k-app.exe"
#define AppDll "sagethumbs2k.dll"
#define Publisher "lunarwerx"

[Setup]
; Stable upgrade GUID — keep constant across releases so updates replace cleanly.
AppId={{B0A1C2D3-E4F5-4607-8899-AABBCCDDEEFF}
AppName={#AppName}
AppVersion={#AppVer}
AppPublisher={#Publisher}
AppPublisherURL=https://github.com/LunarWerxs/SageThumbs-2k
DefaultDirName={autopf}\SageThumbs2K
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UninstallDisplayIcon={app}\app.ico
SetupIconFile=stage\app.ico
OutputDir=..\dist
OutputBaseFilename=SageThumbs2K-Setup-{#AppVer}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Shell-extension registration writes HKLM + Program Files → needs elevation.
PrivilegesRequired=admin
MinVersion=10.0

[Types]
Name: "full"; Description: "Full - all 313 formats (recommended)"
Name: "compact"; Description: "Compact - common formats only (no ImageMagick)"
Name: "custom"; Description: "Custom"; Flags: iscustom

[Components]
Name: "core"; Description: "SageThumbs 2K shell extension + Options"; Types: full compact custom; Flags: fixed
Name: "magick"; Description: "ImageMagick engine - 100+ extra formats (RAW, DICOM, PSD, PCX, TGA, JPEG-2000, ...)"; Types: full custom

[Files]
Source: "stage\{#AppDll}"; DestDir: "{app}"; Flags: ignoreversion; Components: core
Source: "stage\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion; Components: core
Source: "stage\st2k.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
; Signed sparse package + its public cert → the Windows 11 modern context menu.
; Built by packaging\make-msix.ps1 (self-signed; skipped with -NoModernMenu).
Source: "stage\SageThumbs2K.msix"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\SageThumbs2K.cer"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
; Branding assets: icon (shortcut/uninstall) + swappable logo/banner overrides.
Source: "stage\app.ico"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\logo.png"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\banner.png"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\README.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\LICENSE*"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
; Bundled ImageMagick (magick.exe + DLLs + modules\ + hardened policy.xml).
Source: "stage\magick\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs; Components: magick

[Icons]
Name: "{group}\SageThumbs 2K"; Filename: "{app}\{#AppExe}"; IconFilename: "{app}\app.ico"
Name: "{group}\Uninstall SageThumbs 2K"; Filename: "{uninstallexe}"

[Run]
; Register the thumbnail provider + classic context-menu handler (HKLM).
Filename: "{sys}\regsvr32.exe"; Parameters: "/s ""{app}\{#AppDll}"""; \
  StatusMsg: "Registering the shell extension..."; Flags: runhidden waituntilterminated
; Modern Win11 context menu (signed sparse package). Trust our self-signed cert
; (machine TrustedPeople — app packages only, not a root CA), then sideload the
; package bound to the install dir. Both run only when the package was bundled.
Filename: "{sys}\certutil.exe"; Parameters: "-addstore -f TrustedPeople ""{app}\SageThumbs2K.cer"""; \
  StatusMsg: "Trusting the package certificate..."; Flags: runhidden waituntilterminated; Check: ModernMenuBundled
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Add-AppxPackage -Path '{app}\SageThumbs2K.msix' -ExternalLocation '{app}' -ForceUpdateFromAnyVersion"""; \
  StatusMsg: "Registering the modern context menu..."; Flags: runhidden waituntilterminated; Check: ModernMenuBundled
; Launch Settings right after install (checked by default) so the user sees the app.
; `skipifsilent` keeps unattended installs quiet.
Filename: "{app}\{#AppExe}"; Description: "Open SageThumbs 2K Settings"; \
  Flags: postinstall nowait skipifsilent

[UninstallRun]
; Remove the modern-menu package + its trusted cert (best-effort; harmless if the
; package was never installed). Done before the DLL unregister/file removal.
Filename: "powershell.exe"; \
  Parameters: "-NoProfile -ExecutionPolicy Bypass -Command ""Get-AppxPackage -Name SageThumbs2K | Remove-AppxPackage"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "UnregAppx"
Filename: "{sys}\certutil.exe"; Parameters: "-delstore TrustedPeople SageThumbs2K"; \
  Flags: runhidden waituntilterminated; RunOnceId: "DelCert"
; Unregister before files are removed (our DllUnregisterServer also unhooks the
; 313 formats and fires SHChangeNotify).
Filename: "{sys}\regsvr32.exe"; Parameters: "/u /s ""{app}\{#AppDll}"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "UnregSt2k"

[Code]
// The signed sparse package is bundled only when build-release.ps1 ran with the
// Windows SDK present (i.e. not -NoModernMenu). Gate the cert-trust + Appx
// registration on the .msix actually being there, so a classic-only build's
// installer doesn't try (and fail) to register a package it never shipped.
function ModernMenuBundled: Boolean;
begin
  Result := FileExists(ExpandConstant('{app}\SageThumbs2K.msix'));
end;

// Best-effort one-shot HTTPS GET on uninstall, over WinHttp with short timeouts and all
// errors swallowed so it never blocks or slows the uninstall. Only a real uninstall
// reaches it — an in-place upgrade does not run the uninstaller.
procedure NotifyUninstall;
var
  Http: Variant;
begin
  try
    Http := CreateOleObject('WinHttp.WinHttpRequest.5.1');
    // resolve, connect, send, receive (ms) — capped so a dead network fails fast.
    Http.SetTimeouts(1500, 1500, 1500, 2000);
    Http.Open('GET', 'https://st2k.lunarwerx.com/sponsor?uninstall=1&v={#AppVer}', False);
    Http.SetRequestHeader('User-Agent', 'SageThumbs2K-Uninstaller');
    Http.Send('');
  except
    // best-effort only — never surface or block on failure.
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    NotifyUninstall;
end;
