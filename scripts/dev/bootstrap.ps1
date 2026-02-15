$ErrorActionPreference = "Stop"

Write-Host "== PrintRevolt dev bootstrap =="

function Has($name) {
  return (Get-Command $name -ErrorAction SilentlyContinue) -ne $null
}

$DevRoot = $env:PRINTREVOLT_DEV_ROOT
if ([string]::IsNullOrWhiteSpace($DevRoot)) {
  $DevRoot = Join-Path (Get-Location) ".printrevolt-dev"
}

if ([string]::IsNullOrWhiteSpace($env:RUSTUP_HOME)) {
  $env:RUSTUP_HOME = Join-Path $DevRoot "rustup"
}
if ([string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
  $env:CARGO_HOME = Join-Path $DevRoot "cargo"
}
$env:Path = (Join-Path $env:CARGO_HOME "bin") + ";" + $env:Path

if (-not (Has "git")) {
  throw "Missing required tool: git"
}

if (-not (Has "rustup")) {
  Write-Host "rustup not found."
  Write-Host "Install Rust (user-scoped) from https://rustup.rs/ then re-run this script."
  exit 2
}

if (-not (Has "cargo")) {
  throw "cargo not found on PATH (rustup installed but cargo missing?)"
}

Write-Host ("Cargo: " + (& cargo --version))

if (-not (Has "just")) {
  Write-Host "Installing just (cargo install, user-scoped)..."
  & cargo install just
}
Write-Host ("Just: " + (& just --version))

if (-not (Has "jq")) {
  Write-Host "jq not found (recommended for upstream sync scripts). Install via winget/choco if needed."
}

if (-not (Has "pkg-config")) {
  Write-Host "pkg-config not found. On Linux/WSL it is commonly required to build dependencies that link OpenSSL."
}

Write-Host ""
Write-Host "Next:"
Write-Host "  cd codex-rs"
Write-Host "  just fmt"
Write-Host "  cargo test -p codex-pr-integration-tests"
