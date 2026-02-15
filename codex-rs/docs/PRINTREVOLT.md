# PrintRevolt Extensions (codex-pr)

This document describes the PrintRevolt extensions implemented in this fork of Codex CLI.

The design source of truth is maintained in PrintRevolt’s internal docs; this file is a user-facing overview with copy/paste examples.

For general Codex CLI usage (slash commands, config, exec mode), see the upstream docs in `docs/` at the repo root (for example `docs/getting-started.md`, `docs/slash_commands.md`, and `docs/config.md`).

## Install / Entry Points

- `codex` (agent CLI, upstream behavior plus PrintRevolt interception hooks)
- `codex-pr` (headless helper CLI for PrintRevolt configuration, repo ops, templates, updater, and advisor)

## Configuration

Config layers are merged deterministically:

1. CLI overrides (future)
2. Session overlay file via `PRINTREVOLT_SESSION_CONFIG` (Mode B TOML; subset allowed)
3. Project config: `<repo>/.codex/printrevolt.toml` then `<repo>/.codex/config.toml` `[printrevolt.*]`
4. User config: `<CODEX_HOME>/printrevolt.toml` then `<CODEX_HOME>/config.toml` `[printrevolt.*]`
5. Defaults

Minimal example (`<CODEX_HOME>/printrevolt.toml`):

```toml
[printrevolt]
enabled = true

[printrevolt.policy]
deny_dangerous_always = true

[printrevolt.policy.verify]
required = true
max_age_ms = 600000
command_prefixes = [["npm","test"]]

[printrevolt.hooks]
enabled = true
trusted_repo_roots = ["/abs/path/to/repo"]
```

Notes:
- `command_prefixes` matches **argv prefixes** for the `shell` tool (for example `["npm","test"]`).
- If verification runs via the `shell_command` tool (single string command), use single-element prefixes such as `["npm test"]`.

## Policy + Hooks (tool boundary)

All tool calls are intercepted:

1. Hook (optional) can `allow` / `block` / `modify`
2. Policy revalidates after hook modifications (modify -> revalidate)

Finalize gating can block the final response if verify evidence is required and missing/stale/failed.

Current hook surface:
- `before_tool` only (configured at `printrevolt.hooks.before_tool`)

## Pipelines (typed parts)

Pipelines are a typed state machine. Parts emit actions; any command execution is still a normal tool call and therefore goes through policy + hooks + upstream approvals.

Teardown:

- workflow `finally_workflow` always runs on exit
- `defer` registers LIFO cleanup actions
- destructive cleanup should be user-controlled via `require_approval(mode="confirm")` default deny

## Command Catalogs (`commands.json`)

Use `command_id` indirection instead of repeating argv in pipelines:

- Global: `<CODEX_HOME>/printrevolt/commands.json`
- Repo (trusted only): `<repo_root>/.codex/printrevolt/commands.json`

## Git Primitives (worktree + branch)

`codex-pr repo` provides plan/apply helpers:

```bash
codex-pr repo ensure-worktree --repo-root . --worktree-root ~/.codex/printrevolt/worktrees --naming prcc/s1 --branch-name prcc/s1 --base-branch main
codex-pr repo ensure-worktree --repo-root . --worktree-root ~/.codex/printrevolt/worktrees --naming prcc/s1 --branch-name prcc/s1 --base-branch main --apply
```

```bash
codex-pr repo ensure-branch --repo-root . --base-branch main --branch-name prcc/s1 --protected main --protected master
codex-pr repo ensure-branch --repo-root . --base-branch main --branch-name prcc/s1 --protected main --protected master --apply
```

## Templates

Templates are Markdown with YAML frontmatter and required contract sections.

Repo templates are loaded only when the repo is trusted/allowlisted (mirrors repo hooks/commands trust):

- Repo: `<repo_root>/.codex/templates/*.md` (trusted only)
- User: `<CODEX_HOME>/templates/*.md`

Adapter helpers:

```bash
codex-pr templates list --json
codex-pr templates validate
codex-pr templates draft --id one-off --name "My Template" --description "..." --tag security --prompt "..."
```

## Updater

Updater v1 is intentionally conservative and prints a plan plus an action hash. Apply is explicit.

```bash
codex-pr update check --latest 0.101.1
codex-pr update plan --target 0.101.1
codex-pr update plan --target 0.101.1 --apply
```

If `npm` is unavailable, the update check will fail with a clear message unless `--latest` is provided.

## Advisor / Recommendations

`codex-pr doctor --recommend --json` emits a bounded `RecommendationBundle` for:

- command catalogs
- verify pipeline coverage hints

This does not apply changes by default.
