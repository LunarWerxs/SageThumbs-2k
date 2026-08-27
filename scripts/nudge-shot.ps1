# Screenshot the sign-in banner, honestly.
#
# The banner is deliberately hard to see: it needs a week of ownership, several openings of the
# Settings window, AND a signed-out app. Nothing a headless `--shot` does on a fresh profile
# produces it, which is why it needs a harness -- and exactly why that harness must not fake the
# banner into existence. This fakes only the CALENDAR. Everything else is the shipping build: the
# real engine decides, the real owner-draw code paints it, and if the gate is shut the capture
# honestly shows the window without a banner (which this script then reports as a failure rather
# than handing you a PNG of nothing).
#
# Two things worth knowing:
#
#   * Every capture CONSUMES an ask. The engine allows three in a lifetime, so the seed is
#     rewritten before each one; without that the second run would silently show nothing.
#   * It writes `SignInNudge` under HKCU\Software\SageThumbs2K and restores whatever was there
#     (usually: removes it) afterwards, so a capture leaves the machine as it found it.
#
# Usage: pwsh -File scripts\nudge-shot.ps1 [-Out out.png] [-Tab 9] [-Theme dark|light]
[CmdletBinding()]
param(
    [string] $Out = '',
    # Which Settings page to capture behind the banner. It is page-independent chrome, so any
    # page shows it; 9 (Data & Backup) is the one that also shows what it is offering.
    [int]    $Tab = 9,
    [ValidateSet('dark', 'light')]
    [string] $Theme = 'dark'
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$exe = 'D:\.DevScratch\build-cache\st2k-target\release\SageThumbs2K.exe'
if (-not (Test-Path $exe)) { throw "exe not found: $exe -- build it first" }
if ([string]::IsNullOrWhiteSpace($Out)) {
    $Out = Join-Path $projectRoot "nudge-shot-$Theme.png"
}
Remove-Item $Out -Force -ErrorAction SilentlyContinue

$key = 'HKCU:\Software\SageThumbs2K'
$name = 'SignInNudge'
New-Item -Path $key -Force | Out-Null
$prior = (Get-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue).$name

# A long-time user's history: installed a month ago, several sessions, never asked.
[int64]$now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
[int64]$installed = $now - (30 * 86400000)
$seed = '{"v":1,"installed_at":' + $installed + ',"session_count":6,"last_ask_at":null,' +
        '"ask_count":0,"consecutive_declines":0,"cadence":"default","stopped":null,' +
        '"pending_ask":null,"converted":[]}'
Set-ItemProperty -Path $key -Name $name -Value $seed

$env:ST2K_THEME = $Theme
try {
    & $exe '--shot' $Out '--tab' "$Tab" | Out-Null
    if (-not (Test-Path $Out)) { throw "no PNG was written" }

    # Did the engine actually ask? `ask_count` is the only honest answer, and a capture with no
    # banner in it is worse than no capture: it looks like a successful check of something that
    # was never on screen.
    $after = (Get-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue).$name
    $state = $after | ConvertFrom-Json
    if ($state.ask_count -lt 1) {
        Write-Warning "[nudge-shot] the engine did not ask -- this PNG shows NO banner. Signed in, or the gate is still shut."
        exit 1
    }
    Write-Host "[nudge-shot] $Theme, page $Tab, ask #$($state.ask_count) -> $Out ($((Get-Item $Out).Length) bytes)"
}
finally {
    $env:ST2K_THEME = $null
    if ($null -ne $prior) {
        Set-ItemProperty -Path $key -Name $name -Value $prior
    } else {
        Remove-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue
    }
}
