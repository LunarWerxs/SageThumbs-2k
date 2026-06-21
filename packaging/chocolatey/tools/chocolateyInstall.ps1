$ErrorActionPreference = 'Stop'

# Downloads the official GitHub-release installer and runs it silently. The checksum is
# filled from the actual built installer at release time (packaging\make-choco.ps1).
$version  = '0.4.9'
$url      = "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v$version/SageThumbs2K-Setup-$version.exe"

$packageArgs = @{
  packageName   = 'sagethumbs2k'
  fileType      = 'exe'
  url           = $url
  checksum      = 'E7104789901C68922F6FFF9BFB6986892B4A46F3D30F6D85444983DD8D6B058E'
  checksumType  = 'sha256'
  # Inno Setup silent flags. FORCECLOSEAPPLICATIONS lets an UPGRADE replace the DLL while
  # Explorer holds it (the installer restarts Explorer itself); SUPPRESSMSGBOXES alone
  # would otherwise abort on the "can't close app" prompt.
  silentArgs    = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /FORCECLOSEAPPLICATIONS'
  validExitCodes = @(0)
}

Install-ChocolateyPackage @packageArgs
