param(
  [switch]$Release
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$profileDir = if ($Release) { 'release' } else { 'debug' }
$exe = Join-Path $root "target\$profileDir\wx_station.exe"

if (-not (Test-Path $exe)) {
  $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
  if (-not (Test-Path $cargo)) {
    throw "cargo.exe not found. Install Rust with rustup first."
  }

  Push-Location $root
  try {
    if ($Release) {
      & $cargo build --release
    } else {
      & $cargo build
    }
    if ($LASTEXITCODE -ne 0) {
      throw "Cargo build failed with exit code $LASTEXITCODE"
    }
  } finally {
    Pop-Location
  }
}

& $exe
exit $LASTEXITCODE
