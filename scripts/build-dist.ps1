#Requires -Version 5.1
<#
.SYNOPSIS
    Build the portable Windows x86_64 Posthaste distribution.

.DESCRIPTION
    Validates the GNU toolchain, selects a shader compiler, runs the `dist`
    profile build, stages the executable and README, writes SHA256SUMS, and
    creates dist/posthaste-<version>-windows-x86_64.zip.

    The staging directory is kept after a successful run, so the release
    workflow can upload target/dist-staging/SHA256SUMS as a release asset. A
    failed run removes it.

    The shader compiler is resolved in this order: GPUI_FXC_PATH, fxc.exe on
    PATH, then tools/fxc-shim built from source. The chosen compiler is exported
    as GPUI_FXC_PATH for gpui's build script.

.EXAMPLE
    pwsh scripts/build-dist.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = Split-Path -Parent $PSScriptRoot
$Staging = Join-Path $Root 'target/dist-staging'
$Dist = Join-Path $Root 'dist'
$Target = 'x86_64-pc-windows-gnu'

function Assert-Command {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$Hint
    )
    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if (-not $command) {
        throw "Required command '$Name' was not found on PATH. $Hint"
    }
    return $command.Source
}

function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$Description,
        [Parameter(Mandatory)][scriptblock]$Command
    )
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE."
    }
}

Push-Location $Root
try {
    $mingwHint = 'Add the MinGW bin directory (for example D:\tools\mingw64\bin) to PATH.'
    Assert-Command -Name 'cargo' -Hint 'Install Rust and add cargo to PATH.' | Out-Null
    Assert-Command -Name 'windres' -Hint $mingwHint | Out-Null
    Assert-Command -Name 'x86_64-w64-mingw32-gcc' -Hint $mingwHint | Out-Null
    Assert-Command -Name 'ld' -Hint $mingwHint | Out-Null

    if (-not $env:GPUI_FXC_PATH) {
        $fxcOnPath = Get-Command 'fxc.exe' -ErrorAction SilentlyContinue
        if ($fxcOnPath) {
            $env:GPUI_FXC_PATH = $fxcOnPath.Source
        } else {
            Write-Host 'No fxc.exe found on PATH; building tools/fxc-shim...'
            Invoke-Native -Description 'Building the fxc shim' -Command {
                cargo build --release --manifest-path tools/fxc-shim/Cargo.toml
            }
            $shim = Join-Path $Root 'tools/fxc-shim/target/release/fxc-shim.exe'
            if (-not (Test-Path -LiteralPath $shim)) {
                throw "The fxc shim was built but $shim does not exist."
            }
            $env:GPUI_FXC_PATH = $shim
        }
    } elseif (-not (Test-Path -LiteralPath $env:GPUI_FXC_PATH)) {
        throw "GPUI_FXC_PATH is set to '$env:GPUI_FXC_PATH', but that file does not exist."
    }

    $env:GPUI_FXC_PATH = (Resolve-Path -LiteralPath $env:GPUI_FXC_PATH).Path
    Write-Host "Shader compiler: $env:GPUI_FXC_PATH"

    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    $package = $metadata.packages | Where-Object { $_.name -eq 'posthaste-app' } | Select-Object -First 1
    if (-not $package) {
        throw 'cargo metadata did not report a posthaste-app package.'
    }
    $version = $package.version

    Write-Host "Building Posthaste $version for $Target..."
    Invoke-Native -Description "cargo build --profile dist" -Command {
        cargo build --profile dist -p posthaste-app --target $Target
    }

    $exe = Join-Path $Root "target/$Target/dist/posthaste.exe"
    if (-not (Test-Path -LiteralPath $exe)) {
        throw "The build finished but $exe does not exist."
    }

    if (Test-Path -LiteralPath $Staging) {
        Remove-Item -LiteralPath $Staging -Recurse -Force
    }
    New-Item -ItemType Directory -Path $Staging | Out-Null

    $keepStaging = $false
    try {
        Copy-Item -LiteralPath $exe -Destination (Join-Path $Staging 'posthaste.exe')
        Copy-Item -LiteralPath (Join-Path $Root 'README.md') -Destination (Join-Path $Staging 'README.md')

        $hash = (Get-FileHash -LiteralPath (Join-Path $Staging 'posthaste.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText(
            (Join-Path $Staging 'SHA256SUMS'),
            "$hash  posthaste.exe`n",
            $utf8NoBom
        )

        New-Item -ItemType Directory -Force -Path $Dist | Out-Null
        $archive = Join-Path $Dist "posthaste-$version-windows-x86_64.zip"
        if (Test-Path -LiteralPath $archive) {
            Remove-Item -LiteralPath $archive -Force
        }

        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [System.IO.Compression.ZipFile]::CreateFromDirectory(
            $Staging,
            $archive,
            [System.IO.Compression.CompressionLevel]::Optimal,
            $false
        )

        $keepStaging = $true
        Write-Host "Created $archive"
        Write-Host "  posthaste.exe  $hash"
        Write-Host "  staging kept at $Staging"
    } finally {
        if (-not $keepStaging -and (Test-Path -LiteralPath $Staging)) {
            Remove-Item -LiteralPath $Staging -Recurse -Force
        }
    }
} finally {
    Pop-Location
}
