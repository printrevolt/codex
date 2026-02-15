# Local install, test plan, and rollback (PrintRevolt fork)

This doc covers:
- where to find **Codex CLI usage** documentation
- how to **build/install** this PrintRevolt fork locally
- what to **test** (covers the PrintRevolt features wired into upstream)
- how to **rollback** and use upstream Codex CLI via `npx` if anything goes wrong

## Where is the documentation on using Codex CLI?

Start with upstream docs in this repo:
- `docs/getting-started.md` (basic usage)
- `docs/slash_commands.md` (interactive commands)
- `docs/config.md` + `docs/example-config.md` (configuration)
- `docs/exec.md` (non-interactive / exec mode)
- `docs/sandbox.md` + `docs/execpolicy.md` (approvals/sandboxing)
- `docs/skills.md` (skills system)

PrintRevolt-specific docs:
- `codex-rs/docs/PRINTREVOLT.md` (what’s added in this fork)
- `codex-rs/docs/EXTENSION_POINTS.md` (where it hooks into upstream)

## Build / install options

### Recommended: build with the PrintRevolt dev bootstrap toolchain

From the repo root:

```bash
bash scripts/dev/bootstrap.sh
```

Bootstrap provisions a user-scoped Rust toolchain under `.printrevolt-dev/`.

If you don’t already have Rust on PATH, enable the bootstrap toolchain in your current shell:

```bash
export RUSTUP_HOME="$PWD/.printrevolt-dev/rustup"
export CARGO_HOME="$PWD/.printrevolt-dev/cargo"
export PATH="$CARGO_HOME/bin:$PATH"
```

Then build:

```bash
cd codex-rs
just fmt
cargo build -p codex-cli -p codex-pr-cli
```

### Install binaries into your PATH (optional)

If you want `codex`/`codex-pr` on PATH via Cargo:

```bash
cd codex-rs
cargo install --path cli --bin codex --locked
cargo install --path crates/pr_cli --bin codex-pr --locked
```

To confirm what you’re running:

```bash
which codex
which codex-pr
```

## Configure PrintRevolt

PrintRevolt reads config in this precedence order (highest last):
1. user Mode A: `CODEX_HOME/config.toml` `[printrevolt.*]`
2. user Mode B: `CODEX_HOME/printrevolt.toml`
3. project Mode A: `<repo>/.codex/config.toml` `[printrevolt.*]`
4. project Mode B: `<repo>/.codex/printrevolt.toml`
5. session overlay Mode B: `PRINTREVOLT_SESSION_CONFIG=/path/to/file.toml`

To avoid touching your real config while testing, use a temporary `CODEX_HOME`:

```bash
export CODEX_HOME="$(mktemp -d)"
```

Minimal Mode B config to enable the key behaviors:

```toml
# $CODEX_HOME/printrevolt.toml
[printrevolt]
enabled = true

[printrevolt.policy]
deny_dangerous_always = true

[printrevolt.policy.verify]
required = true
max_age_ms = 600000

# Evidence is recorded when the executed argv starts with one of these prefixes.
# Prefer the `shell` tool form (argv array):
command_prefixes = [["npm","test"], ["cargo","test"]]
#
# If your environment/tooling emits verify via `shell_command` (single string),
# use single-element prefixes like:
# command_prefixes = [["npm test"], ["cargo test"]]

[printrevolt.audit]
enabled = true

[printrevolt.hooks]
enabled = false
trusted_repo_roots = []
```

## What to test (covers PrintRevolt features wired into upstream)

### 1) `codex-pr` CLI surfaces

Tip: use `--help` on any subcommand to see required flags and whether it has a plan/apply mode.

```bash
cd codex-rs
cargo run -p codex-pr-cli --bin codex-pr -- doctor
cargo run -p codex-pr-cli --bin codex-pr -- doctor --json
cargo run -p codex-pr-cli --bin codex-pr -- doctor --recommend --json
```

Config layering + session overlay:

```bash
export CODEX_HOME="$(mktemp -d)"
cat > /tmp/pr_overlay.toml <<'TOML'
[printrevolt.policy.verify]
required = true
TOML
export PRINTREVOLT_SESSION_CONFIG=/tmp/pr_overlay.toml

cd codex-rs
cargo run -p codex-pr-cli --bin codex-pr -- doctor --json
```

Expected: the JSON report shows the session overlay file as a source and the final merged config reflects it.

Templates:

```bash
cd codex-rs
cargo run -p codex-pr-cli --bin codex-pr -- templates list --json
cargo run -p codex-pr-cli --bin codex-pr -- templates validate
```

Expected:
- repo templates are only listed when the repo root is trusted/allowlisted (see “Trust checks” below)
- `draft` writes only under `CODEX_HOME/printrevolt/drafts/`

Repo ops (plan vs apply):

```bash
cd codex-rs
cargo run -p codex-pr-cli --bin codex-pr -- repo ensure-branch --help
cargo run -p codex-pr-cli --bin codex-pr -- repo ensure-worktree --help
```

Updater:

```bash
cd codex-rs
cargo run -p codex-pr-cli --bin codex-pr -- update check --help
cargo run -p codex-pr-cli --bin codex-pr -- update plan --help
```

Expected: help/JSON output works; no writes occur unless an `--apply` flag is used (where applicable).

### 2) Tool interception: allow / block / modify

Run Codex (from source):

```bash
cd codex-rs
cargo run -p codex-cli --bin codex
```

In a session, ask the agent to run a clearly dangerous command (example: `rm -rf /tmp/printrevolt_safety_test`).
Expected:
- the tool call is blocked with a reason code like `PrDangerousCommandDenied`
- no side effects occur

### 3) Finalize gate (verify required)

With `printrevolt.policy.verify.required=true`:
- start a session
- ask the agent to do work *without* running a configured verify command
Expected:
- the final response is blocked with `PrVerifyExecutionDenied`

Then run a configured verify command (one of your `command_prefixes`), and retry:
Expected:
- final response succeeds (evidence recorded + fresh)

### 4) Audit output (file + optional stdout mirror)

Audit events are written under:
- `CODEX_HOME/printrevolt/audit/events.jsonl`

Optional: mirror audit to stdout:

```bash
export PRINTREVOLT_AUDIT_STDOUT=1
```

Expected:
- JSONL contains session start/end, tool decisions, and tool outcomes
- sensitive values matching redaction patterns are replaced with `[REDACTED]`

### 5) Trust checks: repo-provided templates + command catalogs

Repo-provided assets are only loaded when the repo root is listed in `printrevolt.hooks.trusted_repo_roots`.

To test repo template trust:
1. Add a template under `<repo>/.codex/templates/example.md`.
2. Run `codex-pr templates list --json`.
3. Add the repo root to `printrevolt.hooks.trusted_repo_roots`, rerun `templates list`, and confirm the repo template is now listed.

To test repo commands catalog trust:
1. Create `<repo>/.codex/printrevolt/commands.json`.
2. Start `codex` with audit enabled.
3. Add/remove the repo root from `printrevolt.hooks.trusted_repo_roots` and confirm repo commands only load when trusted.

### 5) Optional: `before_tool` hook (deny / modify) + clamp

This fork supports a single hook point today: `printrevolt.hooks.before_tool`.

Create a hook script that returns a decision JSON payload. Example `hook_allow.py`:

```python
import json, sys
_ = json.load(sys.stdin)
print(json.dumps({"decision": {"kind": "allow", "reason_code": None, "message": None, "modified_call": None}}))
```

Then configure (Mode B):

```toml
[printrevolt.hooks]
enabled = true
trusted_repo_roots = []

[printrevolt.hooks.before_tool]
argv = ["python3", "/abs/path/to/hook_allow.py"]
timeout_ms = 2000
headless_only = true
is_repo_provided = false
```

Expected:
- hook runs before each tool call
- if the hook returns `block`, the tool does not execute
- if the hook returns `modify`, the policy revalidates and can still block (modify → revalidate)

### 6) Optional: MCP tool interception

If you use MCP servers/tools, verify PrintRevolt still sees those tool calls:
- run a turn that triggers an MCP tool call
- confirm the tool was logged in audit (event kind `before_tool` / `after_tool`)

### 7) WSL-specific smoke: PowerShell parsing for safe-command heuristics

If you’re on WSL and your environment uses Windows PowerShell, ensure safe commands don’t regress:
- commands like `powershell.exe -NoProfile -Command "ls -Name"` should be parsed successfully by the safe-command detection logic

## Rollback plan: fall back to upstream Codex CLI (npx)

### Plan A (preferred): keep fork installed and also have a known-good upstream command

Use upstream Codex CLI without installing anything globally:

```bash
npx -y @openai/codex@latest --help
```

If `codex` from this fork is installed and you need to bypass it temporarily:
- invoke upstream via `npx ...` explicitly, or
- adjust PATH so the upstream `codex` is first

### Plan B: remove forked binaries and use upstream only

If you installed via `cargo install`:

```bash
cargo uninstall codex-cli || true
cargo uninstall codex-pr-cli || true
hash -r  # refresh shell command cache
```

If you added any custom symlinks/aliases, remove those too.

Re-test with upstream:

```bash
command -v codex || true
npx -y @openai/codex@latest --help
```

Optional cleanup (PrintRevolt dev toolchain + test config):

```bash
rm -rf .printrevolt-dev
rm -rf "${CODEX_HOME:-$HOME/.codex}/printrevolt"
```
