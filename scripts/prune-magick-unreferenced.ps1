<#
.SYNOPSIS
  Removes only explicitly nominated, mechanically unreferenced DLLs from a Magick bundle.

.DESCRIPTION
  This is intentionally not a generic "delete every zero-indegree DLL" optimizer: Magick
  loads coder modules dynamically. A caller must nominate reviewed root-level candidates.
  Each candidate is removed only if no other staged PE imports its basename and no other
  staged file contains that basename as either ASCII or UTF-16LE (covering configuration
  and ordinary LoadLibrary calls). Any reference fails closed.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$BundlePath,

    [Parameter(Mandatory)]
    [string]$ObjdumpPath,

    [Parameter(Mandatory)]
    [string[]]$Candidate
)

$ErrorActionPreference = 'Stop'

$root = (Resolve-Path -LiteralPath $BundlePath).Path.TrimEnd('\')
$inspector = (Resolve-Path -LiteralPath $ObjdumpPath).Path
$peFiles = @(Get-ChildItem -LiteralPath $root -Recurse -File |
    Where-Object { $_.Extension -in '.exe', '.dll' })
$allFiles = @(Get-ChildItem -LiteralPath $root -Recurse -File)
$candidateSet = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
foreach ($name in $Candidate) {
    if ([System.IO.Path]::GetFileName($name) -cne $name -or -not $name.EndsWith('.dll', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Unreferenced Magick prune candidate must be a DLL basename, got '$name'"
    }
    if (-not $candidateSet.Add($name)) { throw "Duplicate Magick prune candidate: $name" }
}

$imports = [System.Collections.Generic.Dictionary[string, System.Collections.Generic.List[string]]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
# MinGW objdump cannot read an ARM64 PE at all ("file format not recognized"), so an
# ARM64 bundle is inspected with MSVC's dumpbin, which handles every machine type and
# ships with the same VS BuildTools the ARM64 toolchain already requires. The two were
# verified to report IDENTICAL dependency sets across the x64 bundle before this branch
# was trusted; the parse differs only because the output formats do.
$usingDumpbin = [System.IO.Path]::GetFileNameWithoutExtension($inspector) -ieq 'dumpbin'

foreach ($pe in $peFiles) {
    $output = if ($usingDumpbin) {
        @(& $inspector /nologo /dependents $pe.FullName 2>&1)
    } else {
        @(& $inspector -p $pe.FullName 2>&1)
    }
    if ($LASTEXITCODE -ne 0) { throw "PE inspection failed for '$($pe.FullName)'" }
    $inDependencyBlock = $false
    foreach ($line in $output) {
        if ($usingDumpbin) {
            # dumpbin lists dependencies as 4-space-indented names under a header, and the
            # block ends at the Summary section. Anything outside that block is not an import.
            if ([string]$line -match 'Image has the following dependencies') { $inDependencyBlock = $true; continue }
            if ([string]$line -match '^\s*Summary') { $inDependencyBlock = $false; continue }
            if (-not $inDependencyBlock) { continue }
            if ([string]$line -notmatch '^\s{2,}(\S.*\.dll)\s*$') { continue }
            $dependency = $Matches[1]
            if (-not $imports.ContainsKey($dependency)) {
                $imports[$dependency] = [System.Collections.Generic.List[string]]::new()
            }
            $imports[$dependency].Add($pe.FullName)
            continue
        }
        if ([string]$line -match '^\s*DLL Name:\s*(\S.*?)\s*$') {
            $dependency = $Matches[1]
            if (-not $imports.ContainsKey($dependency)) {
                $imports[$dependency] = [System.Collections.Generic.List[string]]::new()
            }
            $imports[$dependency].Add($pe.FullName)
        }
    }
}

$candidatePaths = [System.Collections.Generic.Dictionary[string, string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
foreach ($name in $candidateSet) {
    $path = Join-Path $root $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Reviewed Magick prune candidate is absent from the bundle: $name"
    }
    $candidatePaths[$name] = $path
    if ($imports.ContainsKey($name)) {
        $importers = $imports[$name] | ForEach-Object { [System.IO.Path]::GetRelativePath($root, $_) }
        throw "Refusing to prune referenced Magick DLL '$name'; imported by: $($importers -join ', ')"
    }
}

# Scan each file once, not once per candidate. Latin-1 preserves every byte one-to-one,
# so IndexOf finds ASCII DLL names; decode a second view for UTF-16LE LoadLibrary literals.
foreach ($file in $allFiles) {
    $bytes = [System.IO.File]::ReadAllBytes($file.FullName)
    $asciiView = [System.Text.Encoding]::Latin1.GetString($bytes)
    $utf16View = [System.Text.Encoding]::Unicode.GetString($bytes)
    foreach ($name in $candidateSet) {
        if ($file.FullName -ieq $candidatePaths[$name]) { continue }
        if ($asciiView.IndexOf($name, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 -or
            $utf16View.IndexOf($name, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
            $relative = [System.IO.Path]::GetRelativePath($root, $file.FullName)
            throw "Refusing to prune dynamically/configuration-referenced Magick DLL '$name'; literal found in '$relative'"
        }
    }
}

$removedBytes = [int64]0
foreach ($name in $candidateSet) {
    $path = $candidatePaths[$name]
    $length = (Get-Item -LiteralPath $path).Length
    [System.IO.File]::Delete($path)
    $removedBytes += $length
    Write-Host "      pruned mechanically unreferenced $name ($length bytes)" -ForegroundColor DarkGray
}

Write-Host "[magick-prune] PASS removed $($candidateSet.Count) reviewed DLLs ($removedBytes raw bytes)" -ForegroundColor Green
