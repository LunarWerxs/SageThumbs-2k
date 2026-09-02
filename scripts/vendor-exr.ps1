<#
  vendor-exr.ps1 - regenerate crates/vendor/exr from the pristine crates.io source plus
  crates/vendor/exr-patches/exr.patch. Mirrors vendor-jxl.ps1's design (see its own header for
  the full rationale: the SOURCE OF TRUTH is the patch file, not the vendored copy, so a
  version bump or a lost hand-edit is always visible as "the patch no longer applies" rather
  than silently rotting).

      pwsh scripts\vendor-exr.ps1                 # regenerate at the pinned version
      pwsh scripts\vendor-exr.ps1 -Check          # verify the tree matches; changes nothing
      pwsh scripts\vendor-exr.ps1 -Version 1.74.3 # try a new upstream release

  ONE DIFFERENCE FROM vendor-jxl.ps1: exr is excluded from [workspace] and consumed only
  through a `[patch.crates-io]` PATH override (Cargo.toml), so cargo never resolves or
  downloads the original crates.io tarball into `~/.cargo/registry/src` - there is nothing
  there to copy from, unlike jxl-render/jxl-oxide. This script downloads the pristine tarball
  directly from static.crates.io instead, and verifies it against the SHA-256 documented in
  crates/vendor/exr/SAGETHUMBS-PATCH.md before trusting it.

  A SECOND DIFFERENCE, and the reason this file does not just add exr to vendor-jxl.ps1's own
  loop: unlike the jxl crates' pristine sources (LF, per vendor-jxl.ps1's own .gitattributes
  comment), this particular exr 1.74.2 crates.io package ships the 4 files the patch touches
  (Cargo.toml, Cargo.toml.orig, src/lib.rs, src/compression/dwa/lossy_dct/transfer_curve.rs)
  as CRLF, while the committed vendored copy has always stored them as LF (SAGETHUMBS-PATCH.md
  predates this script and was written by hand against an LF pristine copy). Verified
  empirically while building this script: `git apply` under this tree's own
  `/crates/vendor/exr/** -text` (no EOL translation - deliberate, see .gitattributes) FAILS
  to match context on a CRLF pristine copy against an LF-authored patch. So: normalize exactly
  those 4 files to LF right after extracting the pristine tarball, before diffing OR applying -
  every other file in the tree is left exactly as extracted, matching how the rest of the
  committed copy has always been whatever the tarball happened to contain.

  DELETE ALL OF IT (this file, crates/vendor/exr-patches, crates/vendor/exr, the
  `[patch.crates-io]` line and its `[workspace] exclude` entry) once an upstream `exr` release
  ships the same leak-free, loader-zeroed DWA transfer-table storage fix - see
  crates/vendor/exr/SAGETHUMBS-PATCH.md for exactly what the patch does and why.
#>
[CmdletBinding()]
param(
    [string]$Version = '1.74.2',
    # Verify only: regenerate into a temp directory and diff against the committed tree.
    [switch]$Check
)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
$patchPath = Join-Path $root 'crates\vendor\exr-patches\exr.patch'
$docPath = Join-Path $root 'crates\vendor\exr\SAGETHUMBS-PATCH.md'
# Files the patch touches, which this particular pristine release ships as CRLF (see the
# header above) and which the committed tree has always stored as LF. Normalized right after
# extraction, before this script diffs OR applies anything against them.
$crlfTouchedFiles = @(
    'Cargo.toml',
    'Cargo.toml.orig',
    'src\lib.rs',
    'src\compression\dwa\lossy_dct\transfer_curve.rs'
)

if (-not (Get-Command git -ErrorAction SilentlyContinue)) { throw 'git is required (for git apply)' }
if (-not (Test-Path $patchPath)) { throw "patch not found: $patchPath" }

# The provenance hash lives in SAGETHUMBS-PATCH.md's own text (one line, backtick-quoted) so
# there is exactly one place to update it when -Version changes - not a second copy here that
# can drift from the doc a human actually reads.
$docText = Get-Content -LiteralPath $docPath -Raw
$hashMatch = [regex]::Match($docText, '\(`([0-9a-f]{64})`\)')
if (-not $hashMatch.Success) {
    throw "could not find the pinned SHA-256 in $docPath - expected a line like ``(`` + 64 hex chars + ``)``"
}
$pinnedHash = $hashMatch.Groups[1].Value

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("st2k-vendor-exr-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $scratch | Out-Null
try {
    $tarball = Join-Path $scratch "exr-$Version.crate"
    # A real User-Agent is required - crates.io's CDN returns a bare 403 with no body to the
    # default curl/PowerShell UA, which reads exactly like a network failure with no clue why.
    Invoke-WebRequest -Uri "https://static.crates.io/crates/exr/exr-$Version.crate" `
        -OutFile $tarball -UserAgent 'sagethumbs2k-vendor-exr (st2k.lunarwerx.com)'
    $actualHash = (Get-FileHash -LiteralPath $tarball -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $pinnedHash) {
        throw "exr $Version hash mismatch: expected $pinnedHash (from SAGETHUMBS-PATCH.md), got $actualHash." +
              " Either the pin is stale for this -Version, or the download is not what it claims to be - do not proceed either way."
    }

    $pristine = Join-Path $scratch 'pristine'
    New-Item -ItemType Directory -Force $pristine | Out-Null
    # .crate files are gzipped tarballs with one top-level "exr-<version>/" directory; strip it
    # so $pristine holds the crate root directly, matching crates/vendor/exr's own layout.
    tar -xzf $tarball -C $pristine --strip-components=1
    if ($LASTEXITCODE -ne 0) { throw 'tar extraction failed' }

    foreach ($rel in $crlfTouchedFiles) {
        $f = Join-Path $pristine $rel
        if (-not (Test-Path $f)) { throw "expected file missing from pristine tarball: $rel" }
        $text = [IO.File]::ReadAllText($f) -replace "`r`n", "`n"
        [IO.File]::WriteAllText($f, $text)
    }

    $dest = if ($Check) { Join-Path $scratch 'result' } else { Join-Path $root 'crates\vendor\exr' }
    if ($Check) {
        New-Item -ItemType Directory -Force $dest | Out-Null
        Copy-Item "$pristine\*" $dest -Recurse -Force
    }
    # else: regenerate IN PLACE, same as vendor-jxl.ps1 does for its two crates - the committed
    # tree already carries whatever cargo-added metadata (.cargo-ok) and our own
    # SAGETHUMBS-PATCH.md doc, neither of which the patch touches, so overwriting only what
    # the pristine copy + patch actually produce leaves them alone. A version bump instead
    # replaces the whole tree in place, same as vendor-jxl.ps1.
    if (-not $Check) {
        Get-ChildItem $dest -Force | Where-Object { $_.Name -notin '.cargo-ok', 'SAGETHUMBS-PATCH.md' } |
            Remove-Item -Recurse -Force
        Copy-Item "$pristine\*" $dest -Recurse -Force
    }

    Push-Location $dest
    try {
        # CAPTURE THE EXIT CODE BEFORE ANYTHING ELSE RUNS - see vendor-jxl.ps1's identical
        # comment for why (a pipelined git call loses $LASTEXITCODE to the pipeline instead of
        # git, and this script would then report "patch applied" for one that had not).
        $applyOut = & git apply --verbose -p2 $patchPath 2>&1
        $applyRc = $LASTEXITCODE
        $applyOut | ForEach-Object { Write-Verbose $_ }
        if ($applyRc -ne 0) {
            Write-Host "[vendor-exr] exr $Version - PATCH DID NOT APPLY" -ForegroundColor Red
            Write-Host "             The upstream source moved under the patch, or -Version is not 1.74.2." -ForegroundColor Yellow
            Write-Host "             Re-check the hunks: git apply -p2 --reject $patchPath" -ForegroundColor Yellow
            Write-Host "             ...then regenerate the patch from the fixed tree. Do NOT hand-edit the vendored copy." -ForegroundColor Yellow
            exit 1
        }
    } finally { Pop-Location }
    Write-Host ("[vendor-exr] exr        {0}  patch applied" -f $Version) -ForegroundColor Green

    if ($Check) {
        $committed = Join-Path $root 'crates\vendor\exr'
        # .cargo-ok and SAGETHUMBS-PATCH.md are not produced by pristine + patch (see the
        # header above) - exclude them from the comparison the same way the patch generation
        # excluded them, rather than reporting permanent, meaningless drift on every run.
        $diff = & git diff --no-index --stat -- $committed $dest `
            ':(exclude)*/.cargo-ok' ':(exclude)*/SAGETHUMBS-PATCH.md' 2>&1
        if ($LASTEXITCODE -ne 0 -and $diff) {
            Write-Host "[vendor-exr] the committed vendor tree does NOT match pristine + patch:" -ForegroundColor Red
            Write-Host $diff
            Write-Host "             Someone hand-edited the vendored copy, or the patch changed." -ForegroundColor Yellow
            Write-Host "             Re-run without -Check to regenerate, and fold real changes into the patch." -ForegroundColor Yellow
            exit 1
        }
        Write-Host "[vendor-exr] OK - the committed vendor tree is exactly pristine + patch." -ForegroundColor Green
    }
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
