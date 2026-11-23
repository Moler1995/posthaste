#Requires -Version 5.1
<#
.SYNOPSIS
    Measure the startup time of a Posthaste build.

.DESCRIPTION
    Launches the executable once to warm the file cache, then runs it a number
    of times and reports the time from process creation to the app being ready
    for input, with the minimum, median, and maximum. The window appears before
    the workspace finishes loading, so input idle is the honest end point.

    The launch uses a temporary workspace and redirects the preference, cookie,
    and recents paths under target/bench, so it does not read or write the
    user's own data. Each instance is stopped without a graceful close, so no
    session document is written.

.PARAMETER Exe
    The executable to measure. Defaults to the dist build for the GNU target,
    which scripts/build-dist.ps1 produces.

.PARAMETER Runs
    Number of measured launches after the warm-up. Defaults to 15.

.PARAMETER Workspace
    Workspace to open. Defaults to an empty workspace under target/bench.
    Pass a large workspace to measure loading it; its request index is cached
    under the platform cache directory, so the first launch of a workspace
    built without one is slower than later launches.

.PARAMETER SkipWarmup
    Skip the unmeasured warm-up launch. Use this for a first launch whose
    cost is the measurement, such as opening a workspace with no cached index.

.EXAMPLE
    pwsh scripts/bench-startup.ps1

.EXAMPLE
    pwsh scripts/bench-startup.ps1 -Runs 30 -Exe target/x86_64-pc-windows-gnu/debug/posthaste.exe

.EXAMPLE
    pwsh scripts/bench-startup.ps1 -Workspace target/bench/large-workspace-1
#>
[CmdletBinding()]
param(
    [int]$Runs = 15,
    [string]$Exe,
    [string]$Workspace,
    [switch]$SkipWarmup
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
if ($Workspace) {
    if (-not [System.IO.Path]::IsPathRooted($Workspace)) {
        $Workspace = Join-Path $Root $Workspace
    }
    if (-not (Test-Path -LiteralPath (Join-Path $Workspace 'workspace.json'))) {
        throw "$Workspace does not look like a workspace: workspace.json is missing."
    }
    $Workspace = (Resolve-Path -LiteralPath $Workspace).Path
} else {
    $Workspace = Join-Path $Bench 'workspace'
}
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
        # The window handle appears before the workspace is loaded, so the
        # measurement waits for the app to be ready for input instead. Boot
        # blocks the UI thread while the documents load, which keeps the
        # process busy until the message loop is idle.
        if (-not $process.WaitForInputIdle(120000)) {
            throw 'Posthaste did not become ready for input within 120 s.'
        }
        return ([DateTime]::Now - $started).TotalMilliseconds
    } finally {
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
            $process.WaitForExit(5000) | Out-Null
        }
    }
}

Write-Host "Measuring $Exe"
if ($SkipWarmup) {
    Write-Host "Skipping the warm-up (first launch of $Workspace is measured)."
} else {
    Write-Host 'Warming the file cache...'
    Measure-Startup | Out-Null
}

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
Write-Host ("workspace: {0}" -f $Workspace)
$launches = if ($SkipWarmup) {
    "launches:  {0} measured, no warm-up" -f $Runs
} else {
    "launches:  {0} measured after one warm-up" -f $Runs
}
Write-Host $launches
Write-Host ("min:       {0:N0} ms" -f $sorted[0])
Write-Host ("median:    {0:N0} ms" -f $median)
Write-Host ("max:       {0:N0} ms" -f $sorted[-1])
Write-Host ("all:       {0}" -f (($times | ForEach-Object { '{0:N0}' -f $_ }) -join ', '))
