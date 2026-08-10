<#
.SYNOPSIS
  Fails unless a staged ImageMagick payload is dependency-closed and runnable.

.DESCRIPTION
  The directory passed as BundlePath is the *flattened install root*: magick.exe and
  CORE_RL_*.dll must be directly inside it, while modules retain their subdirectories.
  This matches Inno Setup's stage\magick\* -> {app} mapping.

  Every PE import is checked against either another bundled basename or a deliberately
  small Windows-inbox allowlist. In particular, MSVC redistributable DLLs are never
  accepted from System32: if ImageMagick imports one, it must ride in the installer.

  Unless SkipSmoke is set, the exact staged magick.exe is then run with bundle-local
  configure/module paths. The smoke test covers BMP decode, PNG encode, and TIFF module
  load/decode (TIFF hard-imports its WebP delegate in the pinned upstream build).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$BundlePath,

    [string]$ObjdumpPath,

    [string]$ConvertSourcePath = (Join-Path (Split-Path $PSScriptRoot -Parent) 'src\bin\app\convert.rs'),

    # The staged engine's expected identity comes from the SAME pin the source check
    # validates against, so an ImageMagick bump is one edit in one file. It used to be
    # a version literal here too, which meant a re-pin passed the source gate and then
    # failed the smoke gate on a string nobody remembered to update.
    [string]$SourcePinPath = (Join-Path (Split-Path $PSScriptRoot -Parent) 'scripts\packaging\imagemagick-source.json'),

    [switch]$SkipSmoke
)

$ErrorActionPreference = 'Stop'

function Resolve-Objdump {
    if ($ObjdumpPath) {
        return (Resolve-Path -LiteralPath $ObjdumpPath).Path
    }
    $command = Get-Command objdump, llvm-objdump -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $command) {
        throw 'objdump/llvm-objdump is required to verify the bundled ImageMagick PE dependency closure'
    }
    $command.Source
}

function Get-PeImports {
    param(
        [Parameter(Mandatory)][string]$PePath,
        [Parameter(Mandatory)][string]$Inspector
    )

    # MinGW objdump cannot read ARM64 PEs, so an ARM64 bundle is inspected with MSVC's
    # dumpbin. Both were verified to report identical dependency sets on the x64 bundle;
    # only the output format differs, which is all this branch accounts for.
    $usingDumpbin = [System.IO.Path]::GetFileNameWithoutExtension($Inspector) -ieq 'dumpbin'
    $output = if ($usingDumpbin) {
        @(& $Inspector /nologo /dependents $PePath 2>&1)
    } else {
        @(& $Inspector -p $PePath 2>&1)
    }
    if ($LASTEXITCODE -ne 0) {
        throw "PE inspection failed for '$PePath' using '$Inspector': $($output -join [Environment]::NewLine)"
    }
    if ($usingDumpbin) {
        $inBlock = $false
        return @($output | ForEach-Object {
            $line = [string]$_
            if ($line -match 'Image has the following dependencies') { $inBlock = $true; return }
            if ($line -match '^\s*Summary') { $inBlock = $false; return }
            if ($inBlock -and $line -match '^\s{2,}(\S.*\.dll)\s*$') { $Matches[1] }
        })
    }
    @($output | ForEach-Object {
        if ([string]$_ -match '^\s*DLL Name:\s*(\S.*?)\s*$') {
            $Matches[1]
        }
    })
}

function Invoke-MagickProbe {
    param(
        [Parameter(Mandatory)][string]$Exe,
        [Parameter(Mandatory)][string[]]$Arguments,
        [Parameter(Mandatory)][string]$WorkingDirectory,
        [Parameter(Mandatory)][string]$BundleRoot,
        [switch]$DebugAll,
        [int]$TimeoutSeconds = 25
    )

    $stdout = Join-Path $WorkingDirectory ("stdout-{0}.txt" -f [Guid]::NewGuid().ToString('N'))
    $stderr = Join-Path $WorkingDirectory ("stderr-{0}.txt" -f [Guid]::NewGuid().ToString('N'))
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $Exe
    $start.WorkingDirectory = $WorkingDirectory
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach ($argument in $Arguments) {
        [void]$start.ArgumentList.Add($argument)
    }

    # Do not let an installed ImageMagick tree on PATH supply modules/configuration. The
    # Windows loader still searches System32, which is why the explicit import allowlist
    # above is the authoritative clean-machine dependency check.
    $system32 = Join-Path $env:SystemRoot 'System32'
    $start.Environment['PATH'] = "$BundleRoot;$system32;$env:SystemRoot"
    $start.Environment['MAGICK_HOME'] = $BundleRoot
    $start.Environment['MAGICK_CONFIGURE_PATH'] = $BundleRoot
    $start.Environment['MAGICK_CODER_MODULE_PATH'] = Join-Path $BundleRoot 'modules\coders'
    $start.Environment['MAGICK_FILTER_MODULE_PATH'] = Join-Path $BundleRoot 'modules\filters'
    $start.Environment['MAGICK_TEMPORARY_PATH'] = $WorkingDirectory
    if ($DebugAll) {
        $start.Environment['MAGICK_DEBUG'] = 'All'
    } else {
        [void]$start.Environment.Remove('MAGICK_DEBUG')
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
        if (-not $process.Start()) {
            throw "Could not start staged ImageMagick: $Exe"
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill($true)
            throw "Staged ImageMagick timed out after $TimeoutSeconds seconds: $($Arguments -join ' ')"
        }
        $stdoutText = $stdoutTask.GetAwaiter().GetResult()
        $stderrText = $stderrTask.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            throw "Staged ImageMagick failed (exit $($process.ExitCode)): $($Arguments -join ' ')`n$stdoutText`n$stderrText"
        }
        [pscustomobject]@{ Stdout = $stdoutText; Stderr = $stderrText }
    } finally {
        $process.Dispose()
        Remove-Item -LiteralPath $stdout, $stderr -Force -ErrorAction SilentlyContinue
    }
}

function Test-StartsWithBytes {
    param(
        [Parameter(Mandatory)][byte[]]$Bytes,
        [Parameter(Mandatory)][byte[]]$Prefix,
        [int]$Offset = 0
    )
    if ($Offset -lt 0 -or $Bytes.Length -lt $Offset + $Prefix.Length) { return $false }
    for ($index = 0; $index -lt $Prefix.Length; $index++) {
        if ($Bytes[$Offset + $index] -ne $Prefix[$index]) { return $false }
    }
    $true
}

function Assert-MagickOutputSignature {
    param(
        [Parameter(Mandatory)][string]$Extension,
        [Parameter(Mandatory)][byte[]]$Bytes
    )
    $ascii = [System.Text.Encoding]::ASCII
    $valid = switch ($Extension) {
        'avif' {
            (Test-StartsWithBytes $Bytes ($ascii.GetBytes('ftyp')) 4) -and
                $Bytes.Length -ge 12 -and
                ([string[]]@('avif', 'avis', 'mif1') -contains $ascii.GetString($Bytes, 8, 4))
        }
        'jxl' {
            (Test-StartsWithBytes $Bytes ([byte[]](0xFF, 0x0A))) -or
                (Test-StartsWithBytes $Bytes ($ascii.GetBytes('JXL ')) 4)
        }
        'psd'  { Test-StartsWithBytes $Bytes ($ascii.GetBytes('8BPS')) }
        'dds'  { Test-StartsWithBytes $Bytes ($ascii.GetBytes('DDS ')) }
        'jp2'  { Test-StartsWithBytes $Bytes ([byte[]](0, 0, 0, 12, 0x6A, 0x50, 0x20, 0x20, 13, 10, 0x87, 10)) }
        'pcx'  { $Bytes.Length -ge 1 -and $Bytes[0] -eq 0x0A }
        'sgi'  { Test-StartsWithBytes $Bytes ([byte[]](0x01, 0xDA)) }
        'exr'  { Test-StartsWithBytes $Bytes ([byte[]](0x76, 0x2F, 0x31, 0x01)) }
        'hdr'  {
            (Test-StartsWithBytes $Bytes ($ascii.GetBytes('#?RADIANCE'))) -or
                (Test-StartsWithBytes $Bytes ($ascii.GetBytes('#?RGBE')))
        }
        'ff'   { Test-StartsWithBytes $Bytes ($ascii.GetBytes('farbfeld')) }
        'pam'  { Test-StartsWithBytes $Bytes ($ascii.GetBytes("P7`n")) }
        'pfm'  {
            (Test-StartsWithBytes $Bytes ($ascii.GetBytes("PF`n"))) -or
                (Test-StartsWithBytes $Bytes ($ascii.GetBytes("Pf`n")))
        }
        'dpx'  {
            (Test-StartsWithBytes $Bytes ($ascii.GetBytes('SDPX'))) -or
                (Test-StartsWithBytes $Bytes ($ascii.GetBytes('XPDS')))
        }
        'fits' { Test-StartsWithBytes $Bytes ($ascii.GetBytes('SIMPLE  =')) }
        'xpm'  { Test-StartsWithBytes $Bytes ($ascii.GetBytes('/* XPM */')) }
        'pict' {
            # PICT v2: conventional 512-byte zero header, 2-byte size + 8-byte frame,
            # then the 0x0011 0x02ff version opcode.
            $Bytes.Length -gt 526 -and
                -not ($Bytes[0..511] | Where-Object { $_ -ne 0 } | Select-Object -First 1) -and
                (Test-StartsWithBytes $Bytes ([byte[]](0x00, 0x11, 0x02, 0xFF)) 522)
        }
        'ras'  { Test-StartsWithBytes $Bytes ([byte[]](0x59, 0xA6, 0x6A, 0x95)) }
        'palm' {
            # Palm bitmap header: big-endian width/height/rowBytes, then flags,
            # pixel size and version. The probe is fixed at 64x64.
            $Bytes.Length -ge 16 -and
                $Bytes[0] -eq 0 -and $Bytes[1] -eq 64 -and
                $Bytes[2] -eq 0 -and $Bytes[3] -eq 64 -and
                (($Bytes[4] -shl 8) -bor $Bytes[5]) -gt 0 -and
                $Bytes[8] -in 1, 2, 4, 8, 16 -and $Bytes[9] -le 3
        }
        default { throw "No signature validator is defined for advertised Magick output '.$Extension'" }
    }
    if (-not $valid) {
        $prefixLength = [Math]::Min(16, $Bytes.Length)
        $prefix = if ($prefixLength) {
            [BitConverter]::ToString($Bytes, 0, $prefixLength)
        } else {
            '<empty>'
        }
        throw "Advertised Magick output '.$Extension' has the wrong signature (prefix $prefix)"
    }
}

$root = (Resolve-Path -LiteralPath $BundlePath).Path.TrimEnd('\')
$magickExe = Join-Path $root 'magick.exe'
if (-not (Test-Path -LiteralPath $magickExe -PathType Leaf)) {
    throw "Flattened ImageMagick bundle must contain magick.exe directly at '$magickExe'"
}
if (Test-Path -LiteralPath (Join-Path $root 'magick\magick.exe')) {
    throw "ImageMagick payload is nested twice; BundlePath must represent the final flattened install root: $root"
}
foreach ($required in 'policy.xml', 'License.txt', 'NOTICE.txt') {
    if (-not (Test-Path -LiteralPath (Join-Path $root $required) -PathType Leaf)) {
        throw "ImageMagick bundle is missing required runtime/legal file: $required"
    }
}
foreach ($directory in 'modules\coders', 'modules\filters') {
    if (-not (Test-Path -LiteralPath (Join-Path $root $directory) -PathType Container)) {
        throw "ImageMagick bundle is missing required module directory: $directory"
    }
}

$inspector = Resolve-Objdump
$peFiles = @(Get-ChildItem -LiteralPath $root -Recurse -File |
    Where-Object { $_.Extension -in '.exe', '.dll' })
if ($peFiles.Count -eq 0) {
    throw "ImageMagick bundle contains no PE files: $root"
}

$bundledByName = [System.Collections.Generic.Dictionary[string, string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
foreach ($file in $peFiles) {
    if ($bundledByName.ContainsKey($file.Name)) {
        throw "ImageMagick bundle contains duplicate PE basename '$($file.Name)': '$($bundledByName[$file.Name])' and '$($file.FullName)'"
    }
    $bundledByName[$file.Name] = $file.FullName
}

# Deliberately explicit. api-ms-win-* and ext-ms-win-* are Windows API-set contracts.
# MSVCP/VCRUNTIME/VCOMP are intentionally absent: clean Windows does not guarantee them.
$windowsInbox = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
foreach ($name in @(
    'advapi32.dll',
    'bcrypt.dll',
    'combase.dll',
    'comdlg32.dll',
    'crypt32.dll',
    # Both are inbox on every supported Windows (verified present in System32). They only
    # appear now because ARM64 ships the GENUINE glib: on x64 glib is stubbed away, so its
    # networking imports never reached this check. Inbox means no redistributable is
    # required, which is the only property this allowlist is asserting.
    'dnsapi.dll',
    'iphlpapi.dll',
    'gdi32.dll',
    'gdiplus.dll',
    'kernel32.dll',
    'ntdll.dll',
    'ole32.dll',
    'oleaut32.dll',
    'shell32.dll',
    'shlwapi.dll',
    'ucrtbase.dll',
    'user32.dll',
    'version.dll',
    'wininet.dll',
    'winmm.dll',
    'ws2_32.dll'
)) {
    [void]$windowsInbox.Add($name)
}

$missing = [System.Collections.Generic.List[object]]::new()
$edgeCount = 0
foreach ($file in $peFiles) {
    foreach ($dependency in (Get-PeImports -PePath $file.FullName -Inspector $inspector)) {
        $edgeCount++
        $isApiSet = $dependency.StartsWith('api-ms-win-', [System.StringComparison]::OrdinalIgnoreCase) -or
            $dependency.StartsWith('ext-ms-win-', [System.StringComparison]::OrdinalIgnoreCase)
        if (-not $bundledByName.ContainsKey($dependency) -and
            -not $windowsInbox.Contains($dependency) -and
            -not $isApiSet) {
            $missing.Add([pscustomobject]@{
                Importer = [System.IO.Path]::GetRelativePath($root, $file.FullName)
                Dependency = $dependency
            })
        }
    }
}

if ($missing.Count -gt 0) {
    $details = $missing |
        Sort-Object Dependency, Importer -Unique |
        ForEach-Object { "  $($_.Importer) -> $($_.Dependency)" }
    throw "ImageMagick bundle is not dependency-closed for clean Windows:`n$($details -join "`n")"
}
Write-Host "[magick-bundle] dependency closure PASS ($($peFiles.Count) PE files, $edgeCount import edges)" -ForegroundColor Green

if (-not $SkipSmoke) {
    # Keep the packaging contract locked to the actual Convert dialog. Any newly
    # advertised exotic output must add an explicit coder + identify/signature probe
    # here before a release can be built.
    $outputPairs = @(
        [pscustomobject]@{ Extension = 'avif'; Coder = 'AVIF'; Identify = 'AVIF' },
        [pscustomobject]@{ Extension = 'jxl';  Coder = 'JXL'; Identify = 'JXL' },
        [pscustomobject]@{ Extension = 'psd';  Coder = 'PSD'; Identify = 'PSD' },
        [pscustomobject]@{ Extension = 'dds';  Coder = 'DDS'; Identify = 'DDS' },
        [pscustomobject]@{ Extension = 'jp2';  Coder = 'JP2'; Identify = 'JP2' },
        [pscustomobject]@{ Extension = 'pcx';  Coder = 'PCX'; Identify = 'PCX' },
        [pscustomobject]@{ Extension = 'sgi';  Coder = 'SGI'; Identify = 'SGI' },
        [pscustomobject]@{ Extension = 'pfm';  Coder = 'PFM'; Identify = 'PFM' },
        [pscustomobject]@{ Extension = 'dpx';  Coder = 'DPX'; Identify = 'DPX' },
        [pscustomobject]@{ Extension = 'fits'; Coder = 'FITS'; Identify = 'FITS' },
        [pscustomobject]@{ Extension = 'xpm';  Coder = 'XPM'; Identify = 'XPM' },
        [pscustomobject]@{ Extension = 'pict'; Coder = 'PICT'; Identify = 'PICT' },
        [pscustomobject]@{ Extension = 'ras';  Coder = 'RAS'; Identify = 'RAS' },
        [pscustomobject]@{ Extension = 'palm'; Coder = 'PALM'; Identify = 'PALM' }
    )
    $convertSource = Get-Content -LiteralPath (Resolve-Path -LiteralPath $ConvertSourcePath) -Raw
    $formatBlock = [regex]::Match(
        $convertSource,
        '(?s)const\s+CV_MAGICK_FORMATS\s*:\s*&\[[^\]]+\]\s*=\s*&\[(.*?)\];'
    )
    if (-not $formatBlock.Success) {
        throw "Could not parse CV_MAGICK_FORMATS from '$ConvertSourcePath'"
    }
    $advertisedExtensions = @(
        [regex]::Matches($formatBlock.Groups[1].Value, '\(\s*"[^"]*"\s*,\s*"([^"]+)"\s*\)') |
            ForEach-Object { $_.Groups[1].Value }
    )
    $probedExtensions = @($outputPairs | ForEach-Object { $_.Extension })
    if ([string]::Join("`n", $advertisedExtensions) -cne [string]::Join("`n", $probedExtensions)) {
        throw "Advertised Magick output list drifted from packaging smoke coverage.`n" +
              "  advertised: $($advertisedExtensions -join ', ')`n" +
              "  probed:     $($probedExtensions -join ', ')"
    }

    $probeRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("st2k-magick-smoke-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $probeRoot | Out-Null
    try {
        $input = Join-Path $probeRoot 'input.bmp'
        $encodeInput = Join-Path $probeRoot 'runtime-input.png'
        $png = Join-Path $probeRoot 'output.png'
        $tiff = Join-Path $probeRoot 'roundtrip.tiff'

        # Deterministic 64x64 24-bit BMP fixture, built without relying on another
        # image library/coder. Runtime feeds Magick a PNG; BMP exercises the same
        # module discovery while allowing the redundant PNM/PAM module to stay out.
        $bitmap = [byte[]]::new(54 + 64 * 64 * 3)
        $bitmap[0] = [byte][char]'B'
        $bitmap[1] = [byte][char]'M'
        [Array]::Copy([BitConverter]::GetBytes([int]$bitmap.Length), 0, $bitmap, 2, 4)
        [Array]::Copy([BitConverter]::GetBytes([int]54), 0, $bitmap, 10, 4)
        [Array]::Copy([BitConverter]::GetBytes([int]40), 0, $bitmap, 14, 4)
        [Array]::Copy([BitConverter]::GetBytes([int]64), 0, $bitmap, 18, 4)
        [Array]::Copy([BitConverter]::GetBytes([int]64), 0, $bitmap, 22, 4)
        [Array]::Copy([BitConverter]::GetBytes([int16]1), 0, $bitmap, 26, 2)
        [Array]::Copy([BitConverter]::GetBytes([int16]24), 0, $bitmap, 28, 2)
        [Array]::Copy([BitConverter]::GetBytes([int](64 * 64 * 3)), 0, $bitmap, 34, 4)
        for ($y = 0; $y -lt 64; $y++) {
            for ($x = 0; $x -lt 64; $x++) {
                $pixel = 54 + ($y * 64 + $x) * 3
                $bitmap[$pixel] = 128
                $bitmap[$pixel + 1] = [byte]($y * 4)
                $bitmap[$pixel + 2] = [byte]($x * 4)
            }
        }
        [System.IO.File]::WriteAllBytes($input, $bitmap)

        $version = Invoke-MagickProbe -Exe $magickExe -Arguments @('-version') -WorkingDirectory $probeRoot -BundleRoot $root
        $expectedVersion = (Get-Content -LiteralPath $SourcePinPath -Raw | ConvertFrom-Json).identity.versionLinePattern
        if ($version.Stdout -notmatch "(?m)$expectedVersion") {
            throw "Staged ImageMagick smoke test reported an unexpected identity: $($version.Stdout.Trim())"
        }

        $moduleTrace = Invoke-MagickProbe `
            -Exe $magickExe `
            -Arguments @($input, '-auto-orient', '-resize', '1x1!', "PNG:$png") `
            -WorkingDirectory $probeRoot `
            -BundleRoot $root `
            -DebugAll
        $openedModules = @(
            [regex]::Matches(
                ($moduleTrace.Stdout + "`n" + $moduleTrace.Stderr),
                '(?im)^\s*Opening module at path "([^"]+)"\s*$'
            ) | ForEach-Object { $_.Groups[1].Value }
        )
        if ($openedModules.Count -eq 0) {
            throw 'MAGICK_DEBUG=All produced no observable module-open paths during BMP-to-PNG smoke'
        }
        $bundlePrefix = [IO.Path]::GetFullPath($root).TrimEnd('\', '/') +
            [IO.Path]::DirectorySeparatorChar
        foreach ($openedModule in $openedModules) {
            $modulePath = [IO.Path]::GetFullPath($openedModule)
            if (-not $modulePath.StartsWith(
                    $bundlePrefix,
                    [StringComparison]::OrdinalIgnoreCase
                )) {
                throw "Staged ImageMagick opened a module outside its bundle: $modulePath"
            }
        }
        Write-Host (
            "[magick-bundle] module-origin trace PASS ({0} opens, all under bundle)" -f
            $openedModules.Count
        ) -ForegroundColor Green
        [void](Invoke-MagickProbe -Exe $magickExe -Arguments @($input, "PNG:$encodeInput") -WorkingDirectory $probeRoot -BundleRoot $root)
        [void](Invoke-MagickProbe -Exe $magickExe -Arguments @($input, "TIFF:$tiff") -WorkingDirectory $probeRoot -BundleRoot $root)
        [void](Invoke-MagickProbe -Exe $magickExe -Arguments @($tiff, '-resize', '1x1!', "PNG:$png") -WorkingDirectory $probeRoot -BundleRoot $root)

        if (-not (Test-Path -LiteralPath $png -PathType Leaf)) {
            throw 'Staged ImageMagick smoke test did not produce its PNG output'
        }
        $signature = [System.IO.File]::ReadAllBytes($png)
        $expectedPng = [byte[]](137, 80, 78, 71, 13, 10, 26, 10)
        if ($signature.Length -lt $expectedPng.Length) {
            throw "Staged ImageMagick emitted a truncated PNG ($($signature.Length) bytes)"
        }
        for ($i = 0; $i -lt $expectedPng.Length; $i++) {
            if ($signature[$i] -ne $expectedPng[$i]) {
                throw 'Staged ImageMagick smoke output does not have a valid PNG signature'
            }
        }

        foreach ($pair in $outputPairs) {
            $output = Join-Path $probeRoot ("advertised." + $pair.Extension)
            $arguments = [System.Collections.Generic.List[string]]::new()
            # Runtime hands ImageMagick a SageThumbs-generated PNG, never the
            # original untrusted input. Exercise that exact encoder boundary.
            $arguments.Add($encodeInput)
            if ($pair.Extension -in 'avif', 'jxl') {
                $arguments.Add('-quality')
                $arguments.Add('50')
            }
            $arguments.Add("$($pair.Coder):$output")
            [void](Invoke-MagickProbe -Exe $magickExe -Arguments $arguments.ToArray() -WorkingDirectory $probeRoot -BundleRoot $root)
            if (-not (Test-Path -LiteralPath $output -PathType Leaf)) {
                throw "Advertised Magick output '.$($pair.Extension)' produced no file"
            }

            $identified = Invoke-MagickProbe -Exe $magickExe -Arguments @('identify', '-quiet', '-format', '%m', $output) -WorkingDirectory $probeRoot -BundleRoot $root
            $detected = $identified.Stdout.Trim()
            if ($detected -notmatch "^(?:$([regex]::Escape($pair.Identify)))+$") {
                throw "Advertised Magick output '.$($pair.Extension)' detected as '$detected', expected '$($pair.Identify)'"
            }
            Assert-MagickOutputSignature -Extension $pair.Extension -Bytes ([System.IO.File]::ReadAllBytes($output))
        }
    } finally {
        if (Test-Path -LiteralPath $probeRoot) {
            [System.IO.Directory]::Delete($probeRoot, $true)
        }
    }
    Write-Host '[magick-bundle] flattened runtime smoke PASS (decode path + 14/14 advertised Magick outputs)' -ForegroundColor Green
}
