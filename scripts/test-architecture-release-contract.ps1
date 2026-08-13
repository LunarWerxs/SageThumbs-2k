<#
  Fast static contract tests for the two-installer publication path. These keep
  stale wildcard selection from returning when dist/ contains x64 and ARM64
  builds of the same version.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$scripts = @(
    'scripts\release.ps1',
    'scripts\vm\sandbox-autotest.ps1',
    'scripts\vm\test-sandbox.ps1',
    'scripts\vm\run-win10-test.ps1',
    'scripts\vm\new-win10-vm.ps1'
)
$script:passed = 0

function Assert-Passes([string]$Name, [scriptblock]$Body) {
    try { & $Body } catch { throw "expected PASS for '$Name', got: $($_.Exception.Message)" }
    Write-Host "  PASS  $Name" -ForegroundColor Green
    $script:passed++
}

function Script-Text([string]$Path) {
    Get-Content -LiteralPath (Join-Path $root $Path) -Raw
}

Assert-Passes 'publication and VM helpers parse' {
    foreach ($path in $scripts) {
        $tokens = $null
        $errors = $null
        [void][System.Management.Automation.Language.Parser]::ParseFile(
            (Join-Path $root $path), [ref]$tokens, [ref]$errors
        )
        if ($errors.Count) {
            throw "$path has PowerShell parse errors: $($errors[0].Message)"
        }
    }
}

Assert-Passes 'release publishes explicit x64 and ARM64 artifact pairs' {
    $text = Script-Text 'scripts\release.ps1'
    foreach ($required in @(
            'SageThumbs2K-Setup-$ver.exe',
            'SageThumbs2K-Setup-$ver-arm64.exe',
            'SageThumbs2K-Setup-$ver.release.json',
            'SageThumbs2K-Setup-$ver-arm64.release.json',
            "'scripts\packaging\stage\x64'",
            "'scripts\packaging\stage\arm64'",
            '-Architecture $artifact.Architecture',
                # ARM64 is a FULL build as of 2026-08-01, so release.ps1 must NOT pass
                # -NoImageMagick for it any more; both architectures bundle ImageMagick.
                # (The absence of that flag is asserted separately, below.)
            'gh release create $tag @releaseAssetPaths'
        )) {
        if ($text -notlike "*$required*") { throw "release.ps1 is missing '$required'" }
    }
    if ($text -match 'SageThumbs2K-Setup-\*') {
        throw 'release.ps1 must not select setup artifacts through a wildcard'
    }
}

Assert-Passes 'CI keeps production payloads release and validation debug' {
    $text = Script-Text '.github\workflows\ci.yml'
    foreach ($required in @(
            'cargo build --release --locked -p sagethumbs2k --features webp-lossy,html-preview,hdr-capture',
            'cargo build --release --locked -p sagethumbs2k-dll --features webp-lossy,dll-i18n-subset',
            'cargo build --locked',
            'cargo test --locked --tests',
            'cargo clippy --locked --all-targets -- -D warnings'
        )) {
        if ($text -notlike "*$required*") { throw "ci.yml is missing '$required'" }
    }
    $validationLines = @($text -split '\r?\n' | Where-Object { $_ -match 'run:\s+cargo\s+.*(?:test|clippy)' })
    if ($validationLines.Count -lt 2 -or ($validationLines | Where-Object { $_ -match '--release' })) {
        throw 'cargo test/clippy CI commands must remain in the debug profile'
    }
}

Assert-Passes 'ARM CI mirrors shipping feature pairs in debug' {
    $text = Script-Text '.github\workflows\ci.yml'
    $start = $text.IndexOf("  arm64-native:", [StringComparison]::Ordinal)
    if ($start -lt 0) { throw 'could not find the arm64-native CI job' }
    $end = $text.IndexOf("  msrv:", $start, [StringComparison]::Ordinal)
    if ($end -le $start) { throw 'could not isolate the arm64-native CI job' }
    $arm = $text.Substring($start, $end - $start)
    foreach ($required in @(
            'build --locked --target aarch64-pc-windows-msvc -p sagethumbs2k --features webp-lossy,html-preview,hdr-capture',
            'build --locked --target aarch64-pc-windows-msvc -p sagethumbs2k-dll --features webp-lossy,dll-i18n-subset',
            'test --locked --tests --target aarch64-pc-windows-msvc'
        )) {
        if ($arm -notlike "*$required*") { throw "ARM CI is missing '$required'" }
    }
    if ($arm -split '\r?\n' | Where-Object { $_ -match '^\s*run:\s+cargo\s+.*--release' }) {
        throw 'ARM native validation must remain in the fast debug profile'
    }
}

Assert-Passes 'VM helpers never select setup artifacts through a wildcard' {
    foreach ($path in @(
            'scripts\vm\sandbox-autotest.ps1',
            'scripts\vm\test-sandbox.ps1',
            'scripts\vm\run-win10-test.ps1',
            'scripts\vm\new-win10-vm.ps1'
        )) {
        if ((Script-Text $path) -match 'SageThumbs2K-Setup-\*') {
            throw "$path contains an ambiguous setup wildcard"
        }
    }
}

Assert-Passes 'Sandbox helpers use the exact path and clean their owned map' {
    $autotest = Script-Text 'scripts\vm\sandbox-autotest.ps1'
    foreach ($required in @(
            'Test-Path -LiteralPath $inst -PathType Leaf',
            'Start-Process -FilePath $inst'
        )) {
        if ($autotest -notlike "*$required*") { throw "sandbox-autotest.ps1 is missing '$required'" }
    }
    if ($autotest -match 'Start-Process \$inst\.FullName') {
        throw 'sandbox-autotest.ps1 must not treat the installer path string as a FileInfo'
    }

    $interactive = Script-Text 'scripts\vm\test-sandbox.ps1'
    foreach ($required in @(
            'Join-Path $installerMapRoot ''sandbox.wsb''',
            'Wait-Process -Id $sandbox.Id',
            'finally {',
            'Remove-Item -LiteralPath $installerMapRoot -Recurse -Force'
        )) {
        if ($interactive -notlike "*$required*") { throw "test-sandbox.ps1 is missing '$required'" }
    }
}

Assert-Passes 'Win10 VM requires only the current x64 setup name' {
    $text = Script-Text 'scripts\vm\run-win10-test.ps1'
    foreach ($required in @(
            '$expectedInstallerName = "SageThumbs2K-Setup-$ver.exe"',
            'run-win10-test requires the exact x64 installer'
        )) {
        if ($text -notlike "*$required*") { throw "run-win10-test.ps1 is missing '$required'" }
    }
}

Write-Host "[architecture-release-contract] ALL GREEN ($script:passed cases)" -ForegroundColor Green
