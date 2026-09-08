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

  READ-ONLY apart from two temporary copies (the sample and a control PNG that Windows'
  own property handler serves, so an idle or backed-off indexer reads as NOT-MEASURED rather
  than as our failure), both deleted before exit.

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
function Test-PropertyValue {
    param($Value)
    return ($null -ne $Value) -and ($Value -isnot [System.DBNull]) -and ("$Value" -ne '')
}
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
# CONTROL: a PNG served by Windows' OWN property handler, dropped beside the sample. If the
# indexer does not fill Dimensions for Microsoft's handler either, it is not extracting
# image properties at all right now (backoff under load, a paused catalog, a scope set to
# file properties only) and an empty result says nothing about OUR handler. On 2026-09-08
# this check reported FAIL three times on a desk where the control was empty too.
$controlSrc = Join-Path $Corpus 'sample.png'
$controlName = $null
$controlFile = $null
if ((Test-Path -LiteralPath $controlSrc -PathType Leaf) -and ($ext -ne '.png')) {
    $controlName = "st2k-indexer-control-$([guid]::NewGuid().ToString('N')).png"
    $controlFile = Join-Path $dir $controlName
    Copy-Item -LiteralPath $controlSrc -Destination $controlFile -Force
}
function Read-IndexedDims {
    param($Connection, [string]$Dir, [string]$Name)
    # Returns @{ Found; Dims; HSize } for one file, reading through the catalog.
    $r = @{ Found = $false; Dims = $null; HSize = $null }
    try {
        $rs = Invoke-SearchQuery -Connection $Connection -Sql (
            "SELECT System.ItemPathDisplay, System.Image.Dimensions, System.Image.HorizontalSize " +
            "FROM SYSTEMINDEX WHERE SCOPE='file:$Dir' AND System.FileName='$Name'"
        )
        if (-not $rs.EOF) {
            $r.Found = $true
            try { $r.Dims = $rs.Fields.Item('System.Image.Dimensions').Value } catch { $r.Dims = $null }
            try { $r.HSize = $rs.Fields.Item('System.Image.HorizontalSize').Value } catch { $r.HSize = $null }
        }
        $rs.Close()
    } catch {
    }
    return $r
}
try {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $main = @{ Found = $false; Dims = $null; HSize = $null }
    $ctl = @{ Found = $false; Dims = $null; HSize = $null }
    while ((Get-Date) -lt $deadline) {
        $main = Read-IndexedDims -Connection $conn -Dir $dir -Name $unique
        # The item shows up in the catalog from the gatherer's FIRST pass (file-system
        # properties only); the property-handler pass that fills Dimensions lands LATER,
        # often seconds later. Breaking on the first row read the gap between the two
        # passes as "the handler returned nothing" (a false FAIL on 2026-09-08), so keep
        # polling until a value lands or the deadline passes.
        if ((Test-PropertyValue $main.Dims) -or (Test-PropertyValue $main.HSize)) { break }
        if ($controlName) {
            $ctl = Read-IndexedDims -Connection $conn -Dir $dir -Name $controlName
        }
        Start-Sleep -Seconds $PollSec
    }
    if (-not $main.Found) {
        Write-Result 'NOT-MEASURED' "$unique never appeared in the index within ${TimeoutSec}s (indexer may be busy or paused) - not a proof of failure" Yellow
        exit 2
    }
    $dims = $main.Dims
    $hsize = $main.HSize
    if ((Test-PropertyValue $dims) -or (Test-PropertyValue $hsize)) {
        Write-Result 'PASS' "indexer served System.Image.Dimensions='$dims' System.Image.HorizontalSize='$hsize' for $unique via the $ext property handler" Green
        exit 0
    }
    if ($controlName) {
        $ctlHas = (Test-PropertyValue $ctl.Dims) -or (Test-PropertyValue $ctl.HSize)
        if (-not $ctlHas) {
            Write-Result 'NOT-MEASURED' "$unique stayed without dimensions for ${TimeoutSec}s, but so did the control PNG served by Windows' own handler ($controlName): the indexer is not extracting image properties right now (backoff under load, paused, or a properties-only scope) - not a proof of failure" Yellow
            exit 2
        }
        Write-Result 'FAIL' "$unique stayed without System.Image.Dimensions/HorizontalSize for the whole ${TimeoutSec}s window while the control PNG got '$($ctl.Dims)' from Windows' own handler - the isolated indexer host is not getting properties from our handler" Red
        exit 1
    }
    Write-Result 'FAIL' "$unique was indexed but System.Image.Dimensions/HorizontalSize stayed empty for the whole ${TimeoutSec}s window (no control PNG available to rule the indexer out) - the isolated indexer host is not getting properties from our handler" Red
    exit 1
} finally {
    Remove-Item -LiteralPath $dstFile -Force -ErrorAction SilentlyContinue
    if ($controlFile) { Remove-Item -LiteralPath $controlFile -Force -ErrorAction SilentlyContinue }
    $conn.Close()
}
