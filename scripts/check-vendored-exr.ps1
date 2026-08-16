<#
  Offline provenance/drift gate for the patched exr 1.74.2 source.

  The unmodified-tree digest covers every crates.io file except the five
  intentional SageThumbs patch files listed below. Those five carry individual
  reviewed hashes. Any edit therefore requires an explicit checksum update.
  cargo metadata additionally proves the application resolves exr from this
  local directory rather than silently falling back to the registry.

  Scope note: this is a DRIFT check only - it proves the vendored source still
  matches what was reviewed. It does NOT check whether upstream exr has since
  shipped the fix this vendor/patch exists for (that would need a network call
  to crates.io, which a pre-push gate that must stay offline and non-flaky
  can't take on). Retiring the vendor override once upstream ships is still a
  manual, periodic call - see docs/TODO.md.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$vendor = Join-Path $root 'crates\vendor\exr'
$expectedVersion = '1.74.2'
$expectedRegistryChecksum = '711fe42c9964295e01ee3fba3f9fe0e1d24b98886950d68efe81b1c76e21adf3'
$expectedVcsCommit = '719d14e57bc6be8db98d8ebdc0b0872bec3cfea7'
$expectedUnmodifiedFileCount = 95
$expectedUnmodifiedTreeDigest = '3c4d3f513b2ba3b5d6485fd56108a5929481768a6ade0aad85ea14189981e726'

$intentionalHashes = [ordered]@{
    'Cargo.toml' = '3f070680b70737c6a076c23bf220d58e8b3404347df76bb909c23738a8581392'
    'Cargo.toml.orig' = '3ef9217ce0329041d02021b4127fecd3cc4271d26588f50687bbc4b9706cf23f'
    'SAGETHUMBS-PATCH.md' = '7f23c96bb3ef0dbc2a2e9e3ba7bc6c736f077af04bdb59dea5d5c3ac74efe30f'
    'src/lib.rs' = 'c2c66a0b527205057bfa49bb8b4c5cfbfe293a0969c6afc0e3b8af555a75d232'
    'src/compression/dwa/lossy_dct/transfer_curve.rs' = '94e1f1efa15e523e6cc328f777adf26716f2496a9c7dd9342caaa1412a96c358'
}
$excluded = @($intentionalHashes.Keys) + @('Cargo.lock')

if (-not (Test-Path -LiteralPath $vendor -PathType Container)) {
    throw "vendored exr source not found: $vendor"
}

$patchDoc = Get-Content -LiteralPath (Join-Path $vendor 'SAGETHUMBS-PATCH.md') -Raw
if ($patchDoc -notmatch [regex]::Escape($expectedRegistryChecksum)) {
    throw 'vendored exr patch documentation lost the crates.io package checksum'
}

$vcs = Get-Content -LiteralPath (Join-Path $vendor '.cargo_vcs_info.json') -Raw | ConvertFrom-Json
if ([string]$vcs.git.sha1 -cne $expectedVcsCommit) {
    throw "vendored exr VCS provenance changed: $($vcs.git.sha1)"
}

foreach ($entry in $intentionalHashes.GetEnumerator()) {
    $path = Join-Path $vendor $entry.Key
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "intentional exr patch file is missing: $($entry.Key)"
    }
    $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -cne $entry.Value) {
        throw "intentional exr patch file changed without provenance review: $($entry.Key)`nexpected $($entry.Value)`nactual   $actual"
    }
}

$lines = [System.Collections.Generic.List[string]]::new()
Get-ChildItem -LiteralPath $vendor -Recurse -File | ForEach-Object {
    $relative = [IO.Path]::GetRelativePath($vendor, $_.FullName).Replace('\', '/')
    if ($excluded -notcontains $relative) {
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $lines.Add("$relative`t$hash")
    }
}
$canonical = $lines.ToArray()
[Array]::Sort($canonical, [StringComparer]::Ordinal)
if ($canonical.Count -ne $expectedUnmodifiedFileCount) {
    throw "vendored exr unmodified file count changed: expected $expectedUnmodifiedFileCount, got $($canonical.Count)"
}
$canonicalText = ($canonical -join "`n") + "`n"
$canonicalBytes = [Text.UTF8Encoding]::new($false).GetBytes($canonicalText)
$treeDigest = [Convert]::ToHexString(
    [Security.Cryptography.SHA256]::HashData($canonicalBytes)
).ToLowerInvariant()
if ($treeDigest -cne $expectedUnmodifiedTreeDigest) {
    throw "vendored exr unmodified source drifted:`nexpected $expectedUnmodifiedTreeDigest`nactual   $treeDigest"
}

$metadataText = & cargo metadata --locked --format-version 1 --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) {
    throw 'cargo metadata --locked failed while checking vendored exr resolution'
}
$metadata = $metadataText | ConvertFrom-Json
$exrPackages = @($metadata.packages | Where-Object { $_.name -eq 'exr' })
if ($exrPackages.Count -ne 1) {
    throw "expected exactly one resolved exr package, got $($exrPackages.Count)"
}
$exr = $exrPackages[0]
if ([string]$exr.version -cne $expectedVersion) {
    throw "resolved exr version is $($exr.version), expected $expectedVersion"
}
if ($null -ne $exr.source) {
    throw "exr resolved from a registry/git source instead of crates/vendor/exr: $($exr.source)"
}
$expectedManifest = [IO.Path]::GetFullPath((Join-Path $vendor 'Cargo.toml'))
$actualManifest = [IO.Path]::GetFullPath([string]$exr.manifest_path)
if ($actualManifest -cne $expectedManifest) {
    throw "exr resolved from '$actualManifest', expected '$expectedManifest'"
}

Write-Host "[vendor] exr $expectedVersion resolves locally; crates.io provenance + intentional patch hashes passed." -ForegroundColor Green
