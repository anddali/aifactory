# The whole suite with one command (Windows PowerShell). See test.sh.
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
docker compose up -d --wait postgres; if (-not $?) { exit 1 }
if ($env:FACTORY_DOCKER_IMAGES -eq "1") { docker compose --profile images build runner-base runner-rust; if (-not $?) { exit 1 } }
cargo build --workspace; if (-not $?) { exit 1 }
cargo test --workspace --no-fail-fast; if (-not $?) { exit 1 }
Push-Location portal; npm ci --no-audit --no-fund; npm test; if (-not $?) { exit 1 }; npm run build; if (-not $?) { exit 1 }; Pop-Location
Write-Host "all tests passed"
