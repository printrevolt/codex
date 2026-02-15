#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

need() { command -v "$1" >/dev/null 2>&1; }

echo "== PrintRevolt dev bootstrap =="

DEV_ROOT="${PRINTREVOLT_DEV_ROOT:-$(pwd)/.printrevolt-dev}"
export RUSTUP_HOME="${RUSTUP_HOME:-${DEV_ROOT}/rustup}"
export CARGO_HOME="${CARGO_HOME:-${DEV_ROOT}/cargo}"
export PATH="${CARGO_HOME}/bin:${PATH}"

if ! need git; then
  echo "Missing required tool: git" >&2
  exit 2
fi

if ! need curl; then
  echo "Missing required tool: curl" >&2
  exit 2
fi

if ! need rustup; then
  echo "Installing rustup (user-scoped)..."
  mkdir -p "${RUSTUP_HOME}" "${CARGO_HOME}"
  curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
fi

echo "Rust: $(rustc --version)"
echo "Cargo: $(cargo --version)"

if ! need just; then
  echo "Installing just (cargo install, user-scoped)..."
  cargo install just
fi
echo "Just: $(just --version)"

if ! need jq; then
  echo "jq not found (recommended for upstream sync scripts)."
  echo "Install it via your package manager (e.g., apt, brew, choco) if you plan to run scripts/printrevolt_upstream_sync.sh."
fi

if ! need pkg-config; then
  echo "pkg-config not found (required on Linux/WSL to build dependencies that link OpenSSL)."
  echo "Install it via your package manager (e.g., apt install pkg-config) along with OpenSSL headers (e.g., libssl-dev)."
fi

if [[ "${PRINTREVOLT_BOOTSTRAP_INSTALL_SYSTEM_DEPS:-0}" == "1" ]]; then
  if need apt-get && need sudo; then
    echo "Installing common Linux dev deps via apt-get (requires sudo)..."
    sudo apt-get update
    sudo apt-get install -y pkg-config libssl-dev libcap-dev
  else
    echo "PRINTREVOLT_BOOTSTRAP_INSTALL_SYSTEM_DEPS=1 was set, but apt-get/sudo not available."
  fi
fi

echo
echo "Next:"
echo "  cd codex-rs"
echo "  just fmt"
echo "  cargo test -p codex-pr-integration-tests"
