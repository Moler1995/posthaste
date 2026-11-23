#Requires -Version 5.1
<#
.SYNOPSIS
    Measure the startup time of a Posthaste build.

.DESCRIPTION
    Launches the executable once to warm the file cache, then runs it a number
    of times and reports the time from process creation to the main window
    being created, with the minimum, median, and maximum.

    The launch uses a temporary workspace and redirects the preference, cookie,
    and recents paths under target/bench, so it does not read or write the
    user's own data. Each instance is stopped without a graceful close, so no
    session document is written.

.PARAMETER Exe
    The executable to measure. Defaults to the dist build for the GNU target,
    which scripts/build-dist.ps1 produces.

.PARAMETER Runs
    Number of measured launches after the warm-up. Defaults to 15.

.EXAMPLE
    pwsh scripts/bench-startup.ps1

.EXAMPLE
    pwsh scripts/bench-startup.ps1 -Runs 30 -Exe target/x86_64-pc-windows-gnu/debug/posthaste.exe
#>
[CmdletBinding()]
param(
    [int]$Runs = 15,
    [string]$Exe
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = Split-Path -Parent $PSScriptRoot
if (-not $Exe) {
    $Exe = Join-Path $Root 'target/x86_64-pc-windows-gnu/dist/posthaste.exe'
} elseif (-not [System.IO.Path]::IsPathRooted($Exe)) {
    $Exe = Join-Path $Root $Exe
}
if (-not (Test-Path -LiteralPath $Exe)) {
    throw "$Exe does not exist. Build it with 'pwsh scripts/build-dist.ps1' first."
}
$Exe = (Resolve-Path -LiteralPath $Exe).Path
if ($Runs -lt 1) {
    throw '-Runs must be at least 1.'
}

$Bench = Join-Path $Root 'target/bench'
$Workspace = Join-Path $Bench 'workspace'
New-Item -ItemType Directory -Force -Path $Workspace | Out-Null
$workspaceDocument = Join-Path $Workspace 'workspace.json'
if (-not (Test-Path -LiteralPath $workspaceDocument)) {
    $json = '{"schema_version":1,"name":"Startup benchmark","vars":{}}'
    [System.IO.File]::WriteAllText(
        $workspaceDocument,
        $json,
        (New-Object System.Text.UTF8Encoding($false))
    )
}

$env:POSTHASTE_WORKSPACE = $Workspace
$env:POSTHASTE_PREFERENCES = Join-Path $Bench 'preferences.json'
$env:POSTHASTE_COOKIE_DIR = Join-Path $Bench 'cookies'
# A queued draft would change what the measured launch opens.
Remove-Item Env:POSTHASTE_DRAFT_URL -ErrorAction SilentlyContinue

function Measure-Startup {
    $process = Start-Process -FilePath $Exe -PassThru
    $started = $process.StartTime
    try {
        # Poll without sleeping: Start-Sleep rounds up to the system timer tick,
        # which would add up to ~15 ms of error to every measurement.
        while (-not $process.HasExited) {
            $process.Refresh()
            if ($process.MainWindowHandle -ne 0) {
                return ([DateTime]::Now - $started).TotalMilliseconds
            }
        }
        throw "Posthaste exited with code $($process.ExitCode) before its window appeared."
    } finally {
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
            $process.WaitForExit(5000) | Out-Null
        }
    }
}

Write-Host "Measuring $Exe"
Write-Host 'Warming the file cache...'
Measure-Startup | Out-Null

$times = @()
for ($i = 0; $i -lt $Runs; $i++) {
    $times += Measure-Startup
    Write-Progress -Activity 'Measuring startup' -Status "$($i + 1)/$Runs" -PercentComplete ((($i + 1) / $Runs) * 100)
}
Write-Progress -Activity 'Measuring startup' -Completed

$sorted = @($times | Sort-Object)
$middle = [int][math]::Floor($Runs / 2)
$median = if ($Runs % 2 -eq 1) {
    $sorted[$middle]
} else {
    ($sorted[$middle - 1] + $sorted[$middle]) / 2
}

Write-Host ''
Write-Host ("launches:  {0} measured after one warm-up" -f $Runs)
Write-Host ("min:       {0:N0} ms" -f $sorted[0])
Write-Host ("median:    {0:N0} ms" -f $median)
Write-Host ("max:       {0:N0} ms" -f $sorted[-1])
Write-Host ("all:       {0}" -f (($times | ForEach-Object { '{0:N0}' -f $_ }) -join ', '))
