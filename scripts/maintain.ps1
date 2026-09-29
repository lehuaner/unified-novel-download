<#
.SYNOPSIS
Runs the local maintenance checks for Unified Novel Downloader.

.DESCRIPTION
The project has a single build recipe (third-party parsing, all novel sources enabled).
`cargo --all-features` is still invalid: `tts` and `tts-native` pick mutually exclusive
TTS backends, and `docker` switches off self-update. So this script checks the default
set plus the lightweight cross-target set used by musl/android releases.

Run from the repository root:
  pwsh ./scripts/maintain.ps1
  powershell -ExecutionPolicy Bypass -File ./scripts/maintain.ps1
#>

param(
    [switch]$SkipFmt,
    [switch]$SkipCross,
    [switch]$SkipTree
)

$ErrorActionPreference = "Stop"

function Invoke-Step {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [Parameter(Mandatory = $true)]
        [scriptblock]$Script
    )

    Write-Host "`n==> $Name" -ForegroundColor Cyan
    & $Script
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

Invoke-Step "Rust toolchain" {
    rustc --version
    cargo --version
}

if (-not $SkipFmt) {
    Invoke-Step "Format check" {
        cargo fmt --all -- --check
    }
}

Invoke-Step "Default feature tests" {
    cargo test
}

Invoke-Step "Default feature clippy" {
    cargo clippy --all-targets -- -D warnings
}

if (-not $SkipCross) {
    Invoke-Step "Cross-target feature tests (musl/android set)" {
        cargo test --no-default-features --features shuqi,qimao,tts-native,clipboard
    }

    Invoke-Step "Cross-target clippy (musl/android set)" {
        cargo clippy --no-default-features --features shuqi,qimao,tts-native,clipboard --all-targets -- -D warnings
    }
}

if (-not $SkipTree) {
    Invoke-Step "Duplicate dependency overview" {
        cargo tree -d
    }
}

Write-Host "`nAll requested maintenance checks completed." -ForegroundColor Green
