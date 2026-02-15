# Development (PrintRevolt fork)

This repository is a PrintRevolt-maintained fork/mirror of `openai/codex` used to build and ship PrintRevolt extensions while keeping upstream changes small and easy to sync.

## Repo layout (what you touch)

- Rust workspace: `codex-rs/`
  - PrintRevolt-owned crates: `codex-rs/crates/pr_*`
  - Upstream extension points doc: `codex-rs/docs/EXTENSION_POINTS.md`
- Node wrapper: `codex-cli/` (upstream)
- PrintRevolt repo automation / runbooks:
  - `PRINTREVOLT_UPSTREAM_MIRROR.md`
  - `printrevolt-upstream-baseline.json`
  - `.github/workflows/printrevolt-*.yml`

## Prerequisites

Required:
- Rust toolchain (via `rustup`) with `cargo`
- `just` (task runner used by upstream)
- `git`

Linux/WSL packages (common build deps):
- `pkg-config`
- OpenSSL headers/libs (`libssl-dev` on Ubuntu/Debian)
- Linux sandbox deps (`libcap-dev` on Ubuntu/Debian)

Recommended:
- `jq` (for upstream sync scripts)
- Node + npm (only needed to resolve the `@openai/codex@latest` → `rust-vX.Y.Z` mapping automatically)

## One-command bootstrap (recommended)

Linux/macOS/WSL:

```bash
bash scripts/dev/bootstrap.sh
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/dev/bootstrap.ps1
```

Bootstrap is non-destructive: it installs missing dev tools where possible and prints instructions when it cannot.

If you used bootstrap and don’t already have Rust on PATH, enable the toolchain in your current shell:

```bash
export RUSTUP_HOME="$PWD/.printrevolt-dev/rustup"
export CARGO_HOME="$PWD/.printrevolt-dev/cargo"
export PATH="$CARGO_HOME/bin:$PATH"
```

## Local install + smoke test

See `PRINTREVOLT_LOCAL_MACHINE.md` for:
- build/install options (`cargo build` vs `cargo install`)
- a manual test checklist for PrintRevolt’s tool interception + finalize gating + audit
- rollback steps to upstream Codex CLI via `npx`

## Build / test (local)

From `codex-rs/`:

```bash
just fmt
cargo test -p codex-pr-integration-tests
```

If you see an OpenSSL/pkg-config build error, install the missing OS packages (example on Ubuntu/Debian):

```bash
sudo apt-get update
sudo apt-get install -y pkg-config libssl-dev libcap-dev
```

On Linux, some tests require the `codex-linux-sandbox` helper binary. Build it once:

```bash
cargo build -p codex-linux-sandbox
```

## PrintRevolt CLI helpers

Run config tracing:

```bash
cargo run -p codex-pr-cli -- doctor
cargo run -p codex-pr-cli -- doctor --json
```

## Upstream sync workflow (developer)

Read: `PRINTREVOLT_UPSTREAM_MIRROR.md`

Manual sync (prepares `sync/rust-vX.Y.Z` and updates `printrevolt-upstream-baseline.json`):

```bash
bash scripts/printrevolt_upstream_sync.sh
```

CI guards:
- Diff budget: `.github/workflows/printrevolt-diff-budget.yml` (limits churn outside `codex-rs/crates/pr_*`)
- Scheduled sync PR: `.github/workflows/printrevolt-upstream-sync.yml`
