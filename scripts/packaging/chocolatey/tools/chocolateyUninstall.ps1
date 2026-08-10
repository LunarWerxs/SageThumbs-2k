$ErrorActionPreference = 'Stop'

# Run SageThumbs 2K's own Inno Setup uninstaller silently. Located via the standard
# Add/Remove-Programs registry key so we don't hardcode a path.
$key = Get-UninstallRegistryKey -SoftwareName 'SageThumbs 2K*'

if ($key.Count -eq 1) {
  $unins = $key.UninstallString -replace '"', ''
  Uninstall-ChocolateyPackage -PackageName 'sagethumbs2k' -FileType 'exe' `
    -SilentArgs '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /FORCECLOSEAPPLICATIONS' `
    -File $unins -ValidExitCodes @(0)
}
elseif ($key.Count -eq 0) {
  Write-Warning 'SageThumbs 2K is not installed (nothing to uninstall).'
}
else {
  Write-Warning "Multiple SageThumbs 2K entries found; skipping automatic uninstall."
  $key | ForEach-Object { Write-Warning "  $($_.DisplayName) -> $($_.UninstallString)" }
}
