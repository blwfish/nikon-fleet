# One-time setup: stage the Nikon Remote SDK runtime files into sdk-runtime\
# so the fleet binary can load them with the default --sdk-bundle path.
#
# Usage (from the repo root, in PowerShell):
#   scripts\setup-sdk-runtime.ps1 C:\path\to\S-SDKZ-200BF-ALLIN
#
# Idempotent: re-running just overwrites the files in place.

param(
    [Parameter(Mandatory=$true)]
    [string]$SdkRoot
)

$ErrorActionPreference = "Stop"

$ProjectDir = Split-Path -Parent $PSScriptRoot
$Runtime    = Join-Path $ProjectDir "sdk-runtime"
$WinBin     = Join-Path $SdkRoot "Module\Win\BinaryFile"

if (-not (Test-Path $WinBin)) {
    Write-Error "Expected Windows SDK binaries at: $WinBin"
    exit 1
}

Write-Host "Staging SDK runtime into $Runtime"
New-Item -ItemType Directory -Force -Path $Runtime | Out-Null

# DLLs: ControlServiceLayer is the entry point; the others are its dependencies.
$dlls = @("ControlServiceLayer.dll", "NkdPTP.dll", "NkRoyalmile.dll", "dnssd.dll")
foreach ($dll in $dlls) {
    $src = Join-Path $WinBin $dll
    if (-not (Test-Path $src)) {
        Write-Error "Missing: $src"
        exit 1
    }
    Copy-Item $src $Runtime -Force
    Write-Host "  copied $dll"
}

# Config files.
$configs = @("MaidLayer.config", "RangeValue.config", "DC_PTP_Config.config")
foreach ($cfg in $configs) {
    $src = Join-Path $WinBin $cfg
    if (Test-Path $src) {
        Copy-Item $src $Runtime -Force
        Write-Host "  copied $cfg"
    }
}

# Names for what a snapshot holds: the base capability names (Maid3.h) and the value names the
# SDK's PDF documents. Derived from the licensed SDK documents, so it goes into the gitignored
# sdk-runtime\ and never into the repository. Optional: without it `nikon-fleet show` prints raw
# numbers and says so. Needs python3 and poppler's pdftotext.
Write-Host ""
Write-Host "Extracting capability and value names from the SDK documents"
$python = Get-Command python3 -ErrorAction SilentlyContinue
if (-not $python) { $python = Get-Command python -ErrorAction SilentlyContinue }
if ($python) {
    & $python.Source (Join-Path $PSScriptRoot "extract-maid-labels.py") $SdkRoot `
        --out (Join-Path $Runtime "maid-labels.json") `
        --maid-layer-config (Join-Path $Runtime "MaidLayer.config")
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "names were not extracted (see above); the runtime works without them."
        Write-Warning "After installing what is missing: python3 scripts\extract-maid-labels.py $SdkRoot"
    }
} else {
    Write-Warning "python not found; skipping scripts\extract-maid-labels.py (names stay raw numbers)."
}

Write-Host ""
Write-Host "Done. Build and run:"
Write-Host "  cargo build"
Write-Host "  cargo run -- discover"
