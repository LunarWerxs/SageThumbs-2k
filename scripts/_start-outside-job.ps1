<#
  Dot-source, then `Start-OutsideJob '<command line>' [-Hidden]`: start a process that belongs
  to NO job object, as this user, in this session, and return its pid (0 when WMI refused).

  Why: `~/.claude/tools/fairjob.cmd` (the throttle every heavy agent command runs under) puts its
  command in a job with KILL_ON_JOB_CLOSE and no breakaway, and a CI step's job does the same.
  A child that Start-Process starts inherits that job and dies the moment the job closes. So the
  Explorer that `verify.ps1 -Install` restarted, and the tray helper its `--heal-hotkeys` brought
  back (the screenshot hotkey AND the Quick preview Space hook), were both killed as soon as an
  agent's install finished: Space and the hotkey stayed dead until the owner's next sign-in,
  after every agent install, while every stored setting was untouched (2026-10-05).

  A process Win32_Process.Create starts is in no job, runs as the calling user in the calling
  session, and gets WinSta0\Default (all three measured that day), so it is the same launch,
  minus the job. -Hidden starts it with SW_HIDE, for a console program; GUI programs need none.
#>
function Start-OutsideJob {
    param(
        [Parameter(Mandatory)][string]$CommandLine,
        [switch]$Hidden
    )
    $create = @{ CommandLine = $CommandLine }
    if ($Hidden) {
        $create.ProcessStartupInformation = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ ShowWindow = [uint16]0 }
    }
    $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments $create
    if ($r.ReturnValue -ne 0) {
        Write-Warning "Start-OutsideJob: Win32_Process.Create returned $($r.ReturnValue) for: $CommandLine"
        return 0
    }
    return [int]$r.ProcessId
}
