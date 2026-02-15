#!/usr/bin/env bash
set -euo pipefail

base_ref="${1:-}"
head_ref="${2:-HEAD}"

baseline_file="printrevolt-upstream-baseline.json"
if [[ -z "${base_ref}" ]]; then
  if [[ ! -f "${baseline_file}" ]]; then
    echo "Missing ${baseline_file} (expected at repo root)." >&2
    exit 2
  fi
  if command -v jq >/dev/null 2>&1; then
    base_ref="$(jq -r '.upstream_commit' "${baseline_file}")"
  elif command -v python3 >/dev/null 2>&1; then
    base_ref="$(python3 - <<PY
import json
with open("${baseline_file}", "r", encoding="utf-8") as f:
    data = json.load(f)
print(data.get("upstream_commit", ""))
PY
)"
  else
    echo "Missing required tool: jq (or python3 fallback) to parse ${baseline_file}." >&2
    exit 2
  fi
  if [[ -z "${base_ref}" || "${base_ref}" == "null" ]]; then
    echo "baseline upstream_commit is missing in ${baseline_file}" >&2
    exit 2
  fi
fi

max_upstream_files="${PRINTREVOLT_DIFF_BUDGET_MAX_UPSTREAM_FILES:-10}"

mapfile -t changed < <(git diff --name-only "${base_ref}...${head_ref}" -- "codex-rs")

non_pr=()
for path in "${changed[@]}"; do
  if [[ "${path}" == "codex-rs/Cargo.lock" ]]; then
    continue
  fi
  if [[ "${path}" == codex-rs/crates/pr_* ]]; then
    continue
  fi
  non_pr+=("${path}")
done

count="${#non_pr[@]}"
if (( count > max_upstream_files )); then
  echo "Diff budget exceeded: ${count} upstream files changed outside codex-rs/crates/pr_* (max ${max_upstream_files})." >&2
  printf '%s\n' "${non_pr[@]}" | sed 's/^/ - /' >&2
  exit 1
fi

echo "Diff budget OK: ${count}/${max_upstream_files} upstream files changed outside codex-rs/crates/pr_*."
