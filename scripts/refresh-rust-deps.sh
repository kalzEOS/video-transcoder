#!/usr/bin/env bash
# Refresh patch/minor versions permitted by Cargo.toml; no manifest major upgrades.
set -euo pipefail
cd "$(dirname "$0")/../src-tauri"
cargo update
cargo fmt --check
cargo test --features cli
if ! command -v cargo-audit >/dev/null 2>&1; then
  echo 'Install the cargo-audit tool to check remaining security advisories: cargo install cargo-audit --locked'
else
  cargo audit
fi
