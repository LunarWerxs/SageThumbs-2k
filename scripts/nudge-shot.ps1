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
#   * Every capture CONSUMES an ask, and the daily gap would gate the next one, so the seed is
#     rewritten before each capture; without that the second run would silently show nothing.
#   * It writes `SignInNudge` under HKCU\Software\SageThumbs2K and restores whatever was there
#     (usually: removes it) afterwards, so a capture leaves the machine as it found it.
#
# -Lang captures the banner in another language, and it is the reason this script matters more
# than it used to. The card's copy is translated into all 36 locales and its heights and button
# widths are MEASURED from that copy, so "does it still fit" has 36 answers, not one. German and
# Russian are the long ones; ja/zh are the short ones; ar/he/fa are right-to-left. A layout bug
# here is invisible in the code and invisible to every test -- only a capture shows it.
#
# Usage: pwsh -File scripts\nudge-shot.ps1 [-Out out.png] [-Tab 9] [-Theme dark|light] [-Lang de]
[CmdletBinding()]
param(
    [string] $Out = '',
    # Which Settings page to capture behind the banner. It is page-independent chrome, so any
    # page shows it; 9 (Data & Backup) is the one that also shows what it is offering.
    [int]    $Tab = 9,
    [ValidateSet('dark', 'light')]
    [string] $Theme = 'dark',
    # A locale code from assets/locales (e.g. de, ru, ja, ar). Empty = whatever this machine
    # is set to. Restored afterwards like SignInNudge is.
    [string] $Lang = '',
    # How many asks this user has ALREADY seen. The month-long dismissal only exists from the
    # fourth ask on, so -AskCount 3 is how you capture the three-button banner and the default 0
    # captures the two-button one. Anything else is a layout nobody ships.
    [int]    $AskCount = 0
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
# Resolved through _targetdir.ps1, never a hardcoded dev-machine path.
$exe = Join-Path (& (Join-Path $PSScriptRoot '_targetdir.ps1')) 'release\SageThumbs2K.exe'
if (-not (Test-Path $exe)) { throw "exe not found: $exe -- build it first" }
if ([string]::IsNullOrWhiteSpace($Out)) {
    $Out = Join-Path $projectRoot "nudge-shot-$Theme.png"
}
Remove-Item $Out -Force -ErrorAction SilentlyContinue

$key = 'HKCU:\Software\SageThumbs2K'
$name = 'SignInNudge'
New-Item -Path $key -Force | Out-Null
$prior = (Get-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue).$name

# The language override lives beside it, and is captured BEFORE anything is written so the
# finally block can put back "no override" as distinctly as it puts back a real value.
$priorLang = (Get-ItemProperty -Path $key -Name 'Lang' -ErrorAction SilentlyContinue).'Lang'
if ($Lang) {
    $localeFile = Join-Path $projectRoot "assets/locales/$Lang.toml"
    if (-not (Test-Path $localeFile)) { throw "no such locale: $Lang (looked for $localeFile)" }
    Set-ItemProperty -Path $key -Name 'Lang' -Value $Lang
}

# A long-time user's history: installed a month ago, several sessions, and $AskCount asks already
# behind them. `last_ask_at` stays null so the next one is never gated on the daily gap; the count
# is what decides whether the month-long dismissal is on the card.
[int64]$now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
[int64]$installed = $now - (30 * 86400000)
$seed = '{"v":1,"installed_at":' + $installed + ',"session_count":6,"last_ask_at":null,' +
        '"ask_count":' + $AskCount + ',"consecutive_declines":0,"cadence":"default","stopped":null,' +
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
    if ($state.ask_count -le $AskCount) {
        Write-Warning "[nudge-shot] the engine did not ask -- this PNG shows NO banner. Signed in, or the gate is still shut."
        exit 1
    }
    $lang = if ($Lang) { $Lang } else { 'system' }
    $buttons = if ($state.ask_count -gt 3) { '3 buttons' } else { '2 buttons' }
    Write-Host "[nudge-shot] $Theme, $lang, page $Tab, ask #$($state.ask_count) ($buttons) -> $Out ($((Get-Item $Out).Length) bytes)"
}
finally {
    $env:ST2K_THEME = $null
    if ($null -ne $prior) {
        Set-ItemProperty -Path $key -Name $name -Value $prior
    } else {
        Remove-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue
    }
    if ($Lang) {
        if ($null -ne $priorLang) {
            Set-ItemProperty -Path $key -Name 'Lang' -Value $priorLang
        } else {
            Remove-ItemProperty -Path $key -Name 'Lang' -ErrorAction SilentlyContinue
        }
    }
}
