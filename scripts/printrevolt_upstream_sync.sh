#!/usr/bin/env bash
set -euo pipefail

baseline_file="printrevolt-upstream-baseline.json"
base_branch="${PRINTREVOLT_SYNC_BASE_BRANCH:-main}"

if [[ ! -f "${baseline_file}" ]]; then
  echo "Missing ${baseline_file} (expected at repo root)." >&2
  exit 2
fi

npm_pkg="$(jq -r '.npm_package' "${baseline_file}")"
if [[ -z "${npm_pkg}" || "${npm_pkg}" == "null" ]]; then
  echo "baseline npm_package is missing in ${baseline_file}" >&2
  exit 2
fi

export NPM_CONFIG_CACHE="${NPM_CONFIG_CACHE:-/tmp/npmcache}"
npm_version="$(npm view "${npm_pkg}" version)"
if [[ -z "${npm_version}" ]]; then
  echo "Failed to resolve npm version for ${npm_pkg}" >&2
  exit 2
fi

upstream_tag="rust-v${npm_version}"
git remote get-url upstream >/dev/null 2>&1 || git remote add upstream https://github.com/openai/codex.git
git fetch upstream --tags --prune

if ! git rev-parse --verify --quiet "refs/tags/${upstream_tag}" >/dev/null; then
  echo "Upstream tag not found: ${upstream_tag}" >&2
  exit 2
fi

upstream_commit="$(git rev-parse "${upstream_tag}^{commit}")"

branch="sync/${upstream_tag}"
git fetch origin "${base_branch}"
git checkout -B "${branch}" "origin/${base_branch}"
git merge --no-edit "${upstream_tag}"
recorded_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

jq \
  --arg npm_version "${npm_version}" \
  --arg upstream_tag "${upstream_tag}" \
  --arg upstream_commit "${upstream_commit}" \
  --arg recorded_at_utc "${recorded_at}" \
  '.npm_version=$npm_version | .upstream_tag=$upstream_tag | .upstream_commit=$upstream_commit | .recorded_at_utc=$recorded_at_utc' \
  "${baseline_file}" > "${baseline_file}.tmp"
mv "${baseline_file}.tmp" "${baseline_file}"

git add "${baseline_file}"
git commit -m "chore: sync upstream ${upstream_tag}" || true

echo "Prepared ${branch} for ${npm_pkg}@${npm_version} (${upstream_tag} @ ${upstream_commit})."
if [[ -n "${GITHUB_ENV:-}" ]]; then
  echo "SYNC_BRANCH=${branch}" >> "${GITHUB_ENV}"
  echo "UPSTREAM_TAG=${upstream_tag}" >> "${GITHUB_ENV}"
  echo "NPM_VERSION=${npm_version}" >> "${GITHUB_ENV}"
fi
