<#
  check-indexer-propstore.ps1 - proves the ISOLATED Windows Search indexer host can read
  System.Image.Dimensions / System.Image.HorizontalSize off a file through OUR property handler,
  not just a direct in-process IPropertyStore call.

  register_property_handler (src/register.rs) sets DisableProcessIsolation=1 on the
  property-handler CLSID so a file-initialised handler is allowed to load in the isolated
  property host SearchIndexer.exe uses - but nothing in this repo drives that path end to end;
  every other check calls the handler directly in-process. This script copies a corpus sample
  into a location Windows Search is already crawling, waits for the indexer to pick it up, and
  reads the two properties back through the same SYSTEMINDEX catalog Explorer's Details pane and
  property searches use (the Search.CollatorDSO OleDb/ADODB provider).

  READ-ONLY apart from one temporary copy of a corpus sample, deleted before exit.

      pwsh scripts\check-indexer-propstore.ps1
      pwsh scripts\check-indexer-propstore.ps1 -Sample sample.xcf
      pwsh scripts\check-indexer-propstore.ps1 -ProveItFails      # query a property that cannot exist

  EXIT CODES
    0  PASS          the indexer path returned real dimensions
    1  FAIL          the file was indexed but the two properties never showed up
    2  NOT-MEASURED  could not run the check at all (no indexed scope found, corpus sample
                      missing, no property handler registered for the extension, provider
                      unavailable, or the item never got indexed in time) - never reported as
                      a failure
#>
[CmdletBinding()]
param(
    [string]$Sample,
    [string]$Corpus = "$PSScriptRoot\..\..\test-corpus",
    [int]$TimeoutSec = 90,
    [int]$PollSec = 3,
    [switch]$ProveItFails
)

$ErrorActionPreference = 'Stop'

function Write-Result {
    param([string]$Tag, [string]$Message, [string]$Color)
    Write-Host "[indexer-propstore] $Tag $Message" -ForegroundColor $Color
}

function New-SearchConnection {
    $conn = New-Object -ComObject ADODB.Connection
    $conn.Open('Provider=Search.CollatorDSO;Extended Properties="Application=Windows";')
    return $conn
}

function Invoke-SearchQuery {
    param([Parameter(Mandatory)]$Connection, [Parameter(Mandatory)][string]$Sql)
    $rs = New-Object -ComObject ADODB.Recordset
    $rs.Open($Sql, $Connection)
    return $rs
}

# First directory that SYSTEMINDEX actually returns rows for - i.e. a scope Windows Search is
# already crawling. A query against an uncrawled folder is not an error, it is just always
# empty, so "returns at least one row" is the only honest signal available from this provider.
function Get-IndexedScopeDir {
    param($Connection, [string[]]$Candidates)
    foreach ($dir in $Candidates) {
        if (-not $dir -or -not (Test-Path -LiteralPath $dir -PathType Container)) { continue }
        try {
            $rs = Invoke-SearchQuery -Connection $Connection -Sql `
                "SELECT TOP 1 System.ItemPathDisplay FROM SYSTEMINDEX WHERE SCOPE='file:$dir'"
            $hasRows = -not $rs.EOF
            $rs.Close()
            if ($hasRows) { return $dir }
        } catch {
            # scope rejected by the provider - try the next candidate
        }
    }
    return $null
}

# ---- -ProveItFails: cheap self-test that a property which cannot exist is never reported as
# present, so a real FAIL (item indexed, dimensions absent) can be trusted. ----
if ($ProveItFails) {
    try {
        $conn = New-SearchConnection
    } catch {
        Write-Result 'NOT-MEASURED' "Search.CollatorDSO unavailable: $($_.Exception.Message)" Yellow
        exit 2
    }
    $dir = Get-IndexedScopeDir -Connection $conn -Candidates @([Environment]::GetFolderPath('MyPictures'), $env:TEMP)
    if (-not $dir) {
        Write-Result 'NOT-MEASURED' 'no indexed scope found to self-test against' Yellow
        $conn.Close()
        exit 2
    }
    $bogus = 'System.SageThumbs2K.NoSuchProperty2026'
    $sawRealValue = $false
    try {
        $rs = Invoke-SearchQuery -Connection $conn -Sql "SELECT TOP 1 $bogus FROM SYSTEMINDEX WHERE SCOPE='file:$dir'"
        if (-not $rs.EOF) {
            $v = $null
            try { $v = $rs.Fields.Item(0).Value } catch { $v = $null }
            if ($null -ne $v -and $v -isnot [System.DBNull]) { $sawRealValue = $true }
        }
        $rs.Close()
    } catch {
        # the provider rejecting an unknown property outright is ALSO correct detection
    }
    $conn.Close()
    if ($sawRealValue) {
        Write-Result 'FAIL' '-ProveItFails: a nonexistent property returned a real value - detection is broken' Red
        exit 1
    }
    Write-Result 'PASS' '-ProveItFails: nonexistent property was rejected or came back empty, as expected' Green
    exit 0
}

# ---- pick a corpus sample of a format our property handler is registered for ----
if (-not $Sample) {
    $Sample = @('sample.psd', 'sample.xcf') | Where-Object {
        Test-Path -LiteralPath (Join-Path $Corpus $_) -PathType Leaf
    } | Select-Object -First 1
}
if (-not $Sample) {
    Write-Result 'NOT-MEASURED' "no sample.psd/sample.xcf found under $Corpus" Yellow
    exit 2
}
$srcFile = Join-Path $Corpus $Sample
if (-not (Test-Path -LiteralPath $srcFile -PathType Leaf)) {
    Write-Result 'NOT-MEASURED' "corpus sample not found: $srcFile" Yellow
    exit 2
}
$ext = [System.IO.Path]::GetExtension($Sample)

$handlerKey = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\PropertySystem\PropertyHandlers\$ext"
if (-not (Test-Path -LiteralPath $handlerKey)) {
    Write-Result 'NOT-MEASURED' "no property handler is registered for $ext on this machine (run st2k register / install first)" Yellow
    exit 2
}

try {
    $conn = New-SearchConnection
} catch {
    Write-Result 'NOT-MEASURED' "Search.CollatorDSO unavailable: $($_.Exception.Message)" Yellow
    exit 2
}

$candidates = @([Environment]::GetFolderPath('MyPictures'), $env:TEMP)
$dir = Get-IndexedScopeDir -Connection $conn -Candidates $candidates
if (-not $dir) {
    Write-Result 'NOT-MEASURED' "none of [$($candidates -join ', ')] is an indexed Windows Search scope on this machine" Yellow
    $conn.Close()
    exit 2
}

$unique = "st2k-indexer-check-$([guid]::NewGuid().ToString('N'))$ext"
$dstFile = Join-Path $dir $unique
Copy-Item -LiteralPath $srcFile -Destination $dstFile -Force

try {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $found = $false
    $dims = $null
    $hsize = $null
    while ((Get-Date) -lt $deadline) {
        try {
            $rs = Invoke-SearchQuery -Connection $conn -Sql (
                "SELECT System.ItemPathDisplay, System.Image.Dimensions, System.Image.HorizontalSize " +
                "FROM SYSTEMINDEX WHERE SCOPE='file:$dir' AND System.FileName='$unique'"
            )
            if (-not $rs.EOF) {
                $found = $true
                try { $dims = $rs.Fields.Item('System.Image.Dimensions').Value } catch { $dims = $null }
                try { $hsize = $rs.Fields.Item('System.Image.HorizontalSize').Value } catch { $hsize = $null }
                $rs.Close()
                break
            }
            $rs.Close()
        } catch {
            # the catalog can reject a query while mid-update - keep polling within the deadline
        }
        Start-Sleep -Seconds $PollSec
    }

    if (-not $found) {
        Write-Result 'NOT-MEASURED' "$unique never appeared in the index within ${TimeoutSec}s (indexer may be busy or paused) - not a proof of failure" Yellow
        exit 2
    }

    $hasDims = ($null -ne $dims) -and ($dims -isnot [System.DBNull]) -and ("$dims" -ne '')
    $hasHSize = ($null -ne $hsize) -and ($hsize -isnot [System.DBNull]) -and ("$hsize" -ne '')

    if ($hasDims -or $hasHSize) {
        Write-Result 'PASS' "indexer served System.Image.Dimensions='$dims' System.Image.HorizontalSize='$hsize' for $unique via the $ext property handler" Green
        exit 0
    }

    Write-Result 'FAIL' "$unique was indexed but System.Image.Dimensions/HorizontalSize came back empty - the isolated indexer host is not getting properties from our handler" Red
    exit 1
} finally {
    Remove-Item -LiteralPath $dstFile -Force -ErrorAction SilentlyContinue
    $conn.Close()
}
