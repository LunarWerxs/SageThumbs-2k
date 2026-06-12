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
Name: "full"; Description: "Full - all 179 formats (recommended)"
Name: "compact"; Description: "Compact - common formats only (no ImageMagick)"
Name: "custom"; Description: "Custom"; Flags: iscustom

[Components]
Name: "core"; Description: "SageThumbs 2K shell extension + Options"; Types: full compact custom; Flags: fixed
Name: "magick"; Description: "ImageMagick engine - 100+ extra formats (RAW, DICOM, PSD, PCX, TGA, JPEG-2000, ...)"; Types: full custom

[Files]
Source: "stage\{#AppDll}"; DestDir: "{app}"; Flags: ignoreversion; Components: core
Source: "stage\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion; Components: core
Source: "stage\st2k.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
; Branding assets: icon (shortcut/uninstall) + swappable logo/banner overrides.
Source: "stage\app.ico"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\logo.png"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\banner.png"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\README.md"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
Source: "stage\LICENSE*"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist; Components: core
; Bundled ImageMagick (magick.exe + DLLs + modules\ + hardened policy.xml).
Source: "stage\magick\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs; Components: magick

[Icons]
Name: "{group}\SageThumbs 2K Options"; Filename: "{app}\{#AppExe}"; IconFilename: "{app}\app.ico"
Name: "{group}\Uninstall SageThumbs 2K"; Filename: "{uninstallexe}"

[Run]
; Register the thumbnail provider + classic context-menu handler (HKLM).
Filename: "{sys}\regsvr32.exe"; Parameters: "/s ""{app}\{#AppDll}"""; \
  StatusMsg: "Registering the shell extension..."; Flags: runhidden waituntilterminated
Filename: "{app}\{#AppExe}"; Description: "Open SageThumbs 2K Options"; \
  Flags: postinstall nowait skipifsilent unchecked

[UninstallRun]
; Unregister before files are removed (our DllUnregisterServer also unhooks the
; 179 formats and fires SHChangeNotify).
Filename: "{sys}\regsvr32.exe"; Parameters: "/u /s ""{app}\{#AppDll}"""; \
  Flags: runhidden waituntilterminated; RunOnceId: "UnregSt2k"
