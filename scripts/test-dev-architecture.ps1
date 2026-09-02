<#
  Focused, dependency-free tests for the architecture selection and PE guards in
  developer install/loose-registration helpers. Does not build, register, or
  modify a package.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("st2k-dev-architecture-" + [guid]::NewGuid().ToString('N'))
$script:passed = 0
. (Join-Path $PSScriptRoot 'test-assert-lib.ps1')

function Assert-Parseable([string]$Path) {
    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw "PowerShell parse failure in ${Path}: $($errors[0].Message)" }
}

function New-PeFixture([string]$Path, [uint16]$Machine) {
    $bytes = [byte[]]::new(0x80)
    $bytes[0] = 0x4d; $bytes[1] = 0x5a
    [BitConverter]::GetBytes([int]0x40).CopyTo($bytes, 0x3c)
    $bytes[0x40] = 0x50; $bytes[0x41] = 0x45
    [BitConverter]::GetBytes($Machine).CopyTo($bytes, 0x44)
    [IO.File]::WriteAllBytes($Path, $bytes)
}

New-Item -ItemType Directory -Path $scratch -Force | Out-Null
try {
    foreach ($relative in @('scripts\install.ps1', 'scripts\packaging\register-dev.ps1')) {
        $path = Join-Path $root $relative
        Assert-Passes "$relative parses" { Assert-Parseable $path }

        $x64Dir = Join-Path $scratch ((Split-Path $relative -Leaf) + '.x64')
        $armDir = Join-Path $scratch ((Split-Path $relative -Leaf) + '.arm64')
        New-Item -ItemType Directory -Path $x64Dir, $armDir -Force | Out-Null
        foreach ($artifact in @('sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe')) {
            New-PeFixture (Join-Path $x64Dir $artifact) 0x8664
            New-PeFixture (Join-Path $armDir $artifact) 0xaa64
        }

        Assert-Passes "$relative keeps x64 legacy target/output selection" {
            $result = if ($relative -eq 'scripts\install.ps1') {
                & $path -Architecture x64 -BuildDir $x64Dir -ValidateOnly
            } else {
                & $path -Architecture x64 -ExternalLocation $x64Dir -ValidateOnly
            }
            if ($result.Architecture -ne 'x64' -or $result.RustTarget -ne 'x86_64-pc-windows-msvc' -or
                -not $result.PSObject.Properties['BuildDir'] -and -not $result.PSObject.Properties['ExternalLocation']) {
                throw 'x64 selection drifted'
            }
        }
        Assert-Passes "$relative selects isolated ARM64 target/output" {
            $result = if ($relative -eq 'scripts\install.ps1') {
                & $path -Architecture arm64 -BuildDir $armDir -ValidateOnly
            } else {
                & $path -Architecture arm64 -ExternalLocation $armDir -ValidateOnly
            }
            if ($result.Architecture -ne 'arm64' -or $result.RustTarget -ne 'aarch64-pc-windows-msvc') {
                throw 'ARM64 selection drifted'
            }
        }

        Assert-FailsLike "$relative rejects a mismatched PE architecture" '*Architecture mismatch*' {
            if ($relative -eq 'scripts\install.ps1') {
                & $path -Architecture arm64 -BuildDir $x64Dir -ValidateOnly
            } else {
                & $path -Architecture arm64 -ExternalLocation $x64Dir -ValidateOnly
            }
        }
        if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne
            [System.Runtime.InteropServices.Architecture]::Arm64) {
            Assert-FailsLike "$relative rejects ARM64 mutation on this non-ARM64 host" '*native ARM64 Windows host*' {
                if ($relative -eq 'scripts\install.ps1') {
                    & $path -Architecture arm64 -BuildDir $armDir
                } else {
                    & $path -Architecture arm64 -ExternalLocation $armDir
                }
            }
        }
    }

    $installText = Get-Content -LiteralPath (Join-Path $root 'scripts\install.ps1') -Raw
    Assert-Passes 'install defaults to legacy x64 Program Files location and isolates ARM64' {
        if ($installText -notmatch "Architecture = 'x64'" -or
            $installText -notmatch "InstallDirectoryName = 'SageThumbs2K'" -or
            $installText -notmatch "InstallDirectoryName = 'SageThumbs2K-arm64'") {
            throw 'install architecture path contract missing'
        }
    }
    Assert-Passes 'install guard runs after ValidateOnly and checks both regsvr32 operations' {
        if ($installText -notmatch 'Assert-NativeArm64Host' -or
            $installText -notmatch 'Start-Process -FilePath \$registrar' -or
            $installText -notmatch '-WindowStyle Hidden -Wait -PassThru' -or
            $installText -notmatch 'return \$process\.ExitCode' -or
            $installText -notmatch 'regsvr32 registration failed' -or
            $installText -notmatch 'regsvr32 unregister failed') {
            throw 'install host/registrar fail-closed contract missing'
        }
    }
    $registerText = Get-Content -LiteralPath (Join-Path $root 'scripts\packaging\register-dev.ps1') -Raw
    Assert-Passes 'register helper targets ARM64 explicitly and stages its manifest separately' {
        if ($registerText -notmatch '\$cargoArgs \+= @\(''--target''' -or
            $registerText -notmatch 'dev-registration-' -or
            $registerText -notmatch 'ProcessorArchitecture') {
            throw 'register architecture contract missing'
        }
    }
    Assert-Passes 'register guard keeps cross-host validation early and blocks ARM64 mutation' {
        if ($registerText -notmatch 'Assert-NativeArm64Host \$spec' -or
            $registerText.IndexOf('if ($ValidateOnly)') -gt $registerText.IndexOf('Assert-NativeArm64Host $spec')) {
            throw 'register host guard ordering contract missing'
        }
    }
    Assert-Passes 'register helper stages an ARM64-only loose manifest without touching its template' {
        $register = Join-Path $root 'scripts\packaging\register-dev.ps1'
        $stage = & {
            param($ScriptPath, $BuildDir)
            . $ScriptPath -Architecture arm64 -ExternalLocation $BuildDir -ValidateOnly | Out-Null
            New-RegistrationManifest (Get-ArchitectureSpec 'arm64') $BuildDir
        } $register (Join-Path $scratch 'register-dev.ps1.arm64')
        $text = Get-Content -LiteralPath $stage -Raw
        if ($text -notmatch 'ProcessorArchitecture="arm64"' -or
            -not (Test-Path -LiteralPath (Join-Path (Split-Path $stage) 'Assets\StoreLogo.png'))) {
            throw 'ARM64 loose manifest staging is incomplete'
        }
        $template = Get-Content -LiteralPath (Join-Path $root 'scripts\packaging\AppxManifest.xml') -Raw
        if ($template -notmatch 'ProcessorArchitecture="neutral"') {
            throw 'tracked manifest was changed by loose staging'
        }
    }
} finally {
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force }
}

Write-Host "Dev architecture helper tests passed: $script:passed" -ForegroundColor Green
