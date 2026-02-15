# PrintRevolt upstream mirror + sync workflow

Upstream Codex is public, but GitHub forks of public repos are also public. For PrintRevolt development, use a **private mirror** and add the public repo as an `upstream` remote.

This fork must track the current **npx codex** release: npm package `@openai/codex@latest` in repo `openai/codex` (subdir `codex-cli`).

## 1) Create a private mirror repository (one-time)

1. Create a new **private** GitHub repo (example): `PrintRevolt/codex-private`.
2. Mirror-push upstream into it:

```bash
git clone --mirror https://github.com/openai/codex.git codex.git
cd codex.git
git push --mirror git@github.com:PrintRevolt/codex-private.git
```

## 2) Clone for development + configure remotes

```bash
git clone git@github.com:PrintRevolt/codex-private.git codex-private
cd codex-private
git remote add upstream https://github.com/openai/codex.git
git fetch upstream --tags
```

## 3) Pin baseline to current `@openai/codex@latest`

Determine the npm version and map it to an upstream tag:

```bash
export NPM_CONFIG_CACHE=/tmp/npmcache
npm view @openai/codex version repository --json
```

Upstream tags are named like `rust-vX.Y.Z`. Check:

```bash
git ls-remote --tags https://github.com/openai/codex.git | rg "rust-v"
```

Record the chosen baseline in `printrevolt-upstream-baseline.json`.

## 4) Optional: sparse-checkout (CLI-only working tree)

You cannot fork only a subdirectory, but you *can* keep your working tree small:

```bash
git sparse-checkout init --cone
git sparse-checkout set codex-cli codex-rs
```

## 5) Upstream sync PR automation

This repo includes a GitHub Actions workflow that:

- fetches upstream tags
- merges the target upstream tag into a branch
- updates `printrevolt-upstream-baseline.json`
- opens (or updates) a PR

See `.github/workflows/printrevolt-upstream-sync.yml`.

## 6) Branch protection (manual)

Configure branch protections on `main` in the private mirror:

- require PRs for `main`
- require CI status checks (including diff-budget)
- restrict force pushes

