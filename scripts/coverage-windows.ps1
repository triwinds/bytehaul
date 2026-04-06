[CmdletBinding()]
param(
    [ValidateSet("all-targets", "tests", "lib")]
    [string]$Scope = "all-targets",

    [ValidateSet("html", "json")]
    [string]$Format = "html",

    [string]$OutputPath,

    [switch]$NoClean
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$cargoLlvmCov = Get-Command cargo-llvm-cov -ErrorAction SilentlyContinue
if (-not $cargoLlvmCov) {
    $fallback = Join-Path $HOME ".cargo\bin\cargo-llvm-cov.exe"
    if (Test-Path $fallback) {
        $cargoLlvmCovPath = $fallback
    } else {
        throw "cargo-llvm-cov not found. Install it with: rustup component add llvm-tools-preview; cargo install cargo-llvm-cov"
    }
} else {
    $cargoLlvmCovPath = $cargoLlvmCov.Source
}

$activeBuilds = Get-Process cargo, rustc -ErrorAction SilentlyContinue
if ($activeBuilds) {
    $processList = ($activeBuilds | Sort-Object ProcessName, Id | ForEach-Object { "$($_.ProcessName)#$($_.Id)" }) -join ", "
    throw "Found active cargo/rustc processes: $processList. Stop them before generating a Windows coverage report to avoid locked files and incomplete output."
}

$scopeArgs = switch ($Scope) {
    "all-targets" { @("-p", "bytehaul", "--all-targets") }
    "tests" { @("-p", "bytehaul", "--tests") }
    "lib" { @("-p", "bytehaul", "--lib") }
}

$reportRoot = Join-Path $repoRoot "target\windows-coverage"
$runId = [guid]::NewGuid().ToString("N")
$targetDir = Join-Path $repoRoot "target\llvm-cov-windows-$Scope-$runId"

if (-not $OutputPath) {
    $OutputPath = switch ($Format) {
        "html" { Join-Path $reportRoot "$Scope-html" }
        "json" { Join-Path $reportRoot "$Scope-summary.json" }
    }
}

$outputParent = Split-Path -Parent $OutputPath
if ($outputParent -and -not (Test-Path $outputParent)) {
    New-Item -ItemType Directory -Path $outputParent -Force | Out-Null
}

if ($Format -eq "html" -and -not (Test-Path $OutputPath)) {
    New-Item -ItemType Directory -Path $OutputPath -Force | Out-Null
}

$env:CARGO_BUILD_JOBS = "1"
$env:CARGO_TARGET_DIR = $targetDir

if (-not $NoClean) {
    & $cargoLlvmCovPath llvm-cov clean --workspace
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}

$reportArgs = switch ($Format) {
    "html" { @("--html", "--output-dir", $OutputPath) }
    "json" { @("--json", "--summary-only", "--output-path", $OutputPath) }
}

$collectArgs = @("llvm-cov", "--no-report") + $scopeArgs
$exportArgs = @("llvm-cov", "report", "-p", "bytehaul") + $reportArgs

Write-Host "Collecting Windows coverage data with single-job build and isolated target dir..."
Write-Host "$cargoLlvmCovPath $($collectArgs -join ' ')"

& $cargoLlvmCovPath @collectArgs
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Host "Exporting coverage report..."
Write-Host "$cargoLlvmCovPath $($exportArgs -join ' ')"

& $cargoLlvmCovPath @exportArgs
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

if ($Format -eq "html") {
    Write-Host "HTML report written to $OutputPath"
} else {
    Write-Host "JSON summary written to $OutputPath"
}