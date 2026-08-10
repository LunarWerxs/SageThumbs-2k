$ErrorActionPreference = 'Stop'

# Downloads the official GitHub-release installer and runs it silently. The checksum is
# filled from the actual built installer at release time (packaging\make-choco.ps1).
$version  = '0.5.0'
$url      = "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v$version/SageThumbs2K-Setup-$version.exe"

$packageArgs = @{
  packageName   = 'sagethumbs2k'
  fileType      = 'exe'
  url           = $url
  checksum      = '65536D528C6461E06A361596D9FCBE9F25344B64899C31C89DC9D7228A09B28E'
  checksumType  = 'sha256'
  # Inno Setup silent flags. FORCECLOSEAPPLICATIONS lets an UPGRADE replace the DLL while
  # Explorer holds it (the installer restarts Explorer itself); SUPPRESSMSGBOXES alone
  # would otherwise abort on the "can't close app" prompt.
  silentArgs    = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /FORCECLOSEAPPLICATIONS'
  validExitCodes = @(0)
}

Install-ChocolateyPackage @packageArgs
