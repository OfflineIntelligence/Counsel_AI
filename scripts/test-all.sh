#!/usr/bin/env bash
set -euo pipefail

# Simple unified test runner for Offline_Intelligence
# Runs backend (Rust) and frontend (Vitest) suites

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")"/.. && pwd)"

echo "==> Backend: cargo test --lib"
(
  cd "$ROOT_DIR/crates/offline-intelligence"
  cargo test --lib
)

echo "==> Frontend: npm run test -- --run"
(
  cd "$ROOT_DIR/apps/desktop"
  npm install --legacy-peer-deps --silent
  npm run test -- --run
)

echo "==> All tests passed"
