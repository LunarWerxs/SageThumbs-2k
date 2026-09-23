# Where this machine keeps the VM test scratch (the Windows ISO under isos\, the VHDX disks under
# Hyper-V\, the results under isos\win10-*-results\): $env:ST2K_VM_SCRATCH when set, else the
# scratch root that already holds the cargo target dir (.cargo/config.toml names it; on the
# reference machine that is the folder beside build-cache). No machine path is written here, so
# the one place a machine path lives stays .cargo/config.toml.
if ($env:ST2K_VM_SCRATCH) {
    $env:ST2K_VM_SCRATCH
} else {
    Split-Path (Split-Path (& (Join-Path $PSScriptRoot '..\_targetdir.ps1')) -Parent) -Parent
}
