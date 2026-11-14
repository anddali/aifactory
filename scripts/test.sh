#!/usr/bin/env bash
# The whole suite with one command. Needs Docker (for Postgres) and the Rust and
# Node toolchains; no external credentials.
#
#   scripts/test.sh            unit + end-to-end + portal
#   FACTORY_DOCKER_IMAGES=1 scripts/test.sh   also build runner images first so the
#                              Docker-launcher test runs instead of skipping
set -euo pipefail
cd "$(dirname "$0")/.."
docker compose up -d --wait postgres
if [ "${FACTORY_DOCKER_IMAGES:-0}" = "1" ]; then
  docker compose --profile images build runner-base runner-rust
fi
cargo build --workspace
cargo test --workspace --no-fail-fast
(cd portal && npm ci --no-audit --no-fund && npm test && npm run build)
echo "all tests passed"
