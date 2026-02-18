# PrintRevolt Codex CLI Fork: Slash Commands Extension

Note: `PrintRevolt-Codex-CLI-Fork-Command-Center-UX-Contract.md` is the consolidated, UX-ready contract for Command Center (agent vs headless flows, review/apply/backup expectations). This document remains the implementation roadmap for the TUI slash command surface.

This document is the implementation roadmap for adding **agent-session user-facing commands** around:

- **Policy** (visibility, recommendations, and safe apply)
- **Templates** (discover, validate, draft, preview, select/use)
- **Pipelines** (discover, preview, manual run, lifecycle integration)
- **Sessions** (clarify expected behavior when templates/policy are session-scoped)

It is intentionally higher-level than `PrintRevolt-Codex-CLI-Fork-LLD.md` and focuses on deliverables, UX, phases, and acceptance criteria for agent-session slash commands.

## Guiding Principles

1. **Policy is never bypassed**
   - All tool execution is still mediated by the same policy + approvals boundary.
   - “Convenience” commands can orchestrate behavior (e.g., run a pipeline) but cannot weaken floors.

2. **Review-first, preview-first, explicit apply**
   - Anything that changes persisted state (config, templates, pipelines bundles) must be previewable and requires explicit confirmation.
   - Any suggestion/generation flow must have a review phase where the user can approve, request changes, regenerate, or cancel.
   - Default behavior is read-only and non-destructive.

3. **Trust-aware, safe-by-default**
   - Repo-provided templates/pipelines/hooks are *discoverable* but *execution is constrained* until the repo is trusted/allowlisted.
   - Untrusted posture should be obvious in UX and logs.

4. **Session-scoped clarity**
   - Template selection is **task-scoped** (immutable once the task/prompt is dispatched).
   - Session state contains only selection UX configuration (mode/default/sticky/MRU) and is mutable via explicit user actions.
   - Policy overlays from templates are applied deterministically and clamped.

5. **Backups and rollback**
   - Any write that replaces an existing file must create an automatic backup (and keep a last-known-good fallback).
   - Restore must be a first-class operation (not manual filesystem surgery).

## Deliverables (User-Facing)

### TUI Slash Commands (agent session)

Add built-in slash commands that surface PrintRevolt functionality directly inside the agent session:

- `/templates`
  - `list` (show discovered templates; indicate source/trust)
  - `validate` (validate all discovered templates; show warnings)
  - `draft` (create a one-time draft from current prompt or provided text)
  - `preview <template_id>` (render template + overlay summary; no state changes)
  - `preview-prompt [<template_id>]` (show the exact prompt that would be sent after template application)
  - `use <template_id>` (set the session sticky template immediately)
  - `clear` (clear the session sticky template; in `mode=once` you will be prompted again on next send)
  - `mode off|once|every_time [--scope agent|global]` (configure when template selection is prompted; default scope is `agent`)
  - `default set <template_id> [--scope agent|global]` (set the “default-for-picker” template; appears near top in the picker, but is not auto-applied unless a picker is shown; default scope is `agent`)
  - `default clear [--scope agent|global]` (remove the “default-for-picker” template; default scope is `agent`)
  - `current` (show mode + sticky template + default-for-picker + overlay summary)

- `/policy`
  - `status` (effective policy summary + floors + provenance pointers)
  - `why` (explain last deny/modify decision, with reason code and remediation)
  - `recommend` (run bounded advisor scan and show recommended diffs)
  - `apply <recommendation_id>` (enter review flow, then apply with explicit confirmation)
  - `restore` (restore policy/config from a backup snapshot)

- `/pipelines`
  - `list` (show enabled pipelines and triggers; indicate source/trust)
  - `show <pipeline_id>` (render compiled/effective pipeline summary)
  - `run <pipeline_id> [--trigger before_task|before_finalize|...]` (manual invocation using normal boundaries)
  - `status` (last pipeline outcome + any suggested remediation message)
  - `create` (start a pipeline creation wizard)
  - `edit <pipeline_id>` (open a review/edit flow for an existing pipeline)
  - `restore` (restore pipelines bundle from a backup snapshot)

Notes:
- Command names/args above are the *user-visible* contract. Exact parsing and UI surfaces (popup, picker, modal) are implementation details.
- Commands should degrade gracefully: “not available in this build” should explain what is missing and how to use `codex-pr` headless CLI as a fallback.
- Slash commands must be supervisor-disableable (see “Supervisor Control” below).

### Headless CLI (`codex-pr`)

`codex-pr` already covers the data-plane operations needed by the TUI:

- `codex-pr doctor [--json] [--recommend]`
- `codex-pr templates list|validate|draft|compose-prompt|enable|disable`
  - scope-aware via `--scope global|project|both` (write actions use `global|project`)
- `codex-pr policy status|explain|enable|disable`
  - `status` is scope-aware via `--scope global|project|both`
- `codex-pr pipelines list|show|draft|restore|enable|disable`
  - scope-aware via `--scope global|project|both` (write actions use `global|project`)
  - `show --expanded` returns a fully expanded pipeline with components inlined (schema `"2"`)
- `codex-pr backups list|restore`

The TUI should prefer **calling Rust APIs directly** when available, but the headless CLI is the compatibility surface for supervisors and integration testing.

## Expected UX (What Users Should Experience)

### Supervisor Control (Command Center Embedding)

When Codex is launched/embedded under a supervisor (e.g., PrintRevolt Command Center), the supervisor must be able to disable specific slash commands so the user uses the supervisor’s own UI flows.

Best-effort mechanism (simple and robust):
- Support an environment variable such as `PRINTREVOLT_UI_DISABLE_SLASH_COMMANDS=templates,pipelines,policy`.
- Support an equivalent config key such as `[printrevolt.ui] disabled_slash_commands = ["templates","pipelines"]`.

Behavior:
- Disabled commands are hidden from the slash popup and rejected if typed manually (with a short explanation).
- Disabling UI commands must not disable the underlying safety enforcement (policy/hook/tool boundaries remain active).
- Precedence: environment variable overrides config.

### Implementation Contracts (Handoff-Ready)

This section defines the concrete, implementable contracts for configuration, state, and UX behavior so the work can be delivered without ambiguity.

**Agent identity**
- Per-agent overrides are keyed by a stable `agent_id` string.
- `agent_id` should match whatever the Codex TUI uses to switch agents (e.g., the value shown/selected via `/agent`).
- If no agent concept is available in a given entrypoint, treat `agent_id = "default"`.

**Config keys and precedence**
- Global defaults live under `[printrevolt.templates]`.
- Per-agent overrides live under `[printrevolt.agents."<agent_id>".templates]`.
- Precedence (highest to lowest):
  1. CLI overrides (if present)
  2. Per-agent overrides for the current `agent_id`
  3. Global defaults
  4. Built-in defaults

Recommended keys:
- `printrevolt.templates.selection_mode = "off" | "once" | "every_time"`
- `printrevolt.templates.default_for_picker_template_id = "<template_id>" | ""`
- `printrevolt.agents."<agent_id>".templates.selection_mode = ...` (optional override)
- `printrevolt.agents."<agent_id>".templates.default_for_picker_template_id = ...` (optional override)

**State vs config**
- Config is durable and user-controlled (global/per-agent) and should not change implicitly.
- Session state is ephemeral and changes frequently via explicit user actions:
  - `sticky_template_id` (only used for `mode=once`)
  - MRU timestamps for ordering and “used X ago”

Slash commands fall into two categories:
- Session-only state changes (no file writes): `use`, `clear`.
- Config changes (file writes with review/confirm + backup): `mode`, `default set`, `default clear`.

For config changes initiated from the TUI:
- Show a small review/confirm step (what setting will change, old value, new value, scope).
- On confirm: write to PrintRevolt-owned config (Mode B) by default and create an automatic backup snapshot.
- On cancel: no changes.

**State persistence**
- Persist MRU and last-used timestamps in a PrintRevolt-owned JSON file, not in upstream Codex config.
- Recommended path: `CODEX_HOME/printrevolt/state/templates.json`.
- Schema (versioned):
  - `schema_version: "1"`
  - `agents: { "<agent_id>": { "mru": [{ "template_id": "...", "last_used_at_ms": 0 }] } }`
- Retention:
  - Keep at most 50 MRU entries per agent.
  - Drop entries older than 90 days.

**Picker ordering contract**
When the picker is shown for a given `agent_id`:
1. `No template` (always)
2. `[default] ...` (only if `default_for_picker_template_id` resolves to a discovered template)
3. `New Prompt Template...`
4. MRU templates (sorted by `last_used_at_ms` desc, excluding default-for-picker if already listed)
5. Remaining templates (stable sort by `name` then `id`)

**Used X ago formatting**
- Render a human-friendly relative time in the picker (e.g., `used 5m ago`, `used 2h ago`, `used 3d ago`).
- If the timestamp is missing, omit the suffix (do not show incorrect times).

**Review-before-send payload**
The review step should be backed by a single structured object so both TUI and supervisor UIs can render it:
- `agent_id`
- `task_intent` (optional short label)
- `raw_prompt` (in-memory only)
- `template_id` (or explicit `none`)
- `generated_prompt` (in-memory only)
- `generated_prompt_sha256` (persistable)
- `policy_overlay_summary` (bounded; persistable)
- `writes_planned` (list of file changes if the user chooses Save in the wizard; empty for “continue without saving”)

**Task immutability**
- Once the user approves and the `UserTurn` is dispatched, the chosen `template_id` and `generated_prompt_sha256` are immutable for that task.
- Switching templates affects only future prompts.

### Acceptance Criteria (Slash Commands)

Templates:
- Composer always displays `Template: <name|None>` and `Mode: off|once|every_time`.
- `mode=off`: no picker is shown; generated prompt equals raw prompt.
- `mode=once`: picker is shown only when `sticky_template_id` is unset; selecting a template sets sticky; `/templates clear` unsets sticky.
- `mode=every_time`: picker is shown for every send; the selection does not change sticky.
- Picker ordering matches the contract exactly, including MRU ordering and “used X ago” formatting.
- Review-before-send always shows the exact generated prompt string and the template id (or explicit none).

Supervisor control:
- When disabled via env/config, the slash popup does not list disabled commands and manual invocation returns a clear “disabled by supervisor” message.

Backups:
- Any config-changing command creates a backup snapshot and can be restored in one step.

### Code Touchpoints (Where To Implement)

TUI:
- Register new slash commands and descriptions in `codex/codex-rs/tui/src/slash_command.rs`.
- Implement command dispatch behavior (open picker, review modal, config edits) in `codex/codex-rs/tui/src/chatwidget.rs`.
- Implement composer indicator (template + mode) in the bottom pane composer code: `codex/codex-rs/tui/src/bottom_pane/chat_composer.rs`.

PrintRevolt crates:
- Template discovery/contract parsing and policy-default clamping live in `codex/codex-rs/crates/pr_templates/src/lib.rs`.
- Prompt composition helper (used by preview + send) is implemented in `codex/codex-rs/crates/pr_templates/src/lib.rs` and exposed via `codex-pr templates compose-prompt`.
- Headless CLI surface is `codex/codex-rs/crates/pr_cli/src/lib.rs` (templates/policy/pipelines/backups; pipelines support `show --expanded` for component expansion previews).
- Runtime wiring (persist MRU, compute prompt hash, attach task template + hash) will touch `codex/codex-rs/crates/pr_runtime/src/lib.rs` and the upstream prompt submit path described in the LLD.

### Universal Review Flow (Templates, Pipelines, Policy)

Any time Codex/PrintRevolt produces a suggestion or generates content (template draft, pipeline draft, policy changes), the UX should behave like a small “chat-style” review loop:

1. **Propose**
   - show a preview (rendered artifact + structured summary)
   - show an explicit patch/diff-like view of what would change on disk
2. **Review**
   - user chooses: Approve, Request changes (free text), Regenerate, or Cancel
3. **Revise (optional, repeatable)**
   - Codex/PrintRevolt produces a revised proposal based on feedback
4. **Apply**
   - apply only after explicit confirmation
   - create a backup snapshot automatically
5. **Audit**
   - record “proposed hash”, “applied hash”, and “backup ref”

The review loop must be bounded and safe:
- No automatic writes during proposal/review.
- Clear “what will be written where” messaging.
- Cancel always returns to the prior state without side effects.

### Suggestions and Preview

Users can ask Codex/PrintRevolt for *project-aware suggestions* based on repo root structure, but the tool must be conservative:

- What is scanned (bounded):
  - repo root metadata (paths, presence of common manifests)
  - git metadata (branch, status)
  - known script catalogs (e.g., `package.json` scripts) when present
  - existing PrintRevolt config state
- What is not scanned by default:
  - file contents beyond bounded metadata needed for safe suggestions

Preview expectations:
- Recommendations are shown as a patch/diff-like preview (what fields/files will change).
- User can deny, request revision, or apply.
- Apply is audited (before/after hashes).

### Template Lifecycle in Agent Session

The customer-expected UX is a hybrid of (a) an always-visible indicator in the composer and (b) a picker + review phase that only appears when required.

**Composer indicator**
- The prompt input area shows the current template state:
  - `Template: None` or `Template: <name>`
  - `Mode: off|once|every_time`
- This avoids “surprise modal” behavior and makes it obvious what will happen when the user sends a prompt.

**Selection modes**
- `mode=off`: no picker is shown; no template is applied.
- `mode=once`: if no sticky template is set for this session, sending a prompt opens the picker; once selected, it becomes sticky until cleared or switched.
- `mode=every_time`: every send opens the picker (preselect last-used to be fast).

**Picker ordering**
When the picker is shown, it must be stable and predictable:
1. `No template`
2. `[default] <template-name>` (only if default-for-picker is configured; this is not auto-applied unless the picker is shown)
3. `New Prompt Template...` (draft wizard)
4. Most recently used templates (with “used X ago”)
5. Remaining templates (alphabetical)

**Review-before-send**
After the user selects a template (or confirms `No template`), show a review step:
- Generated prompt preview (exact string sent to the agent)
- Policy overlay summary (if any), and a clear note that floors clamp it
- Actions: Approve and send, Change template, Edit prompt, Cancel

**Template application**
If a template is selected:
- the prompt is transformed by inserting a stable prefix/content contract derived from the template (the “generated prompt”)
- template `defaults` become a **policy overlay**
- the overlay is clamped against floors and stored in session state for enforcement

During tool execution and completion:
- `before_tool` still allow/deny/modify
- hooks may propose modifications, but policy re-validates after modification
- `before_finalize` may deny completion (e.g., missing verify evidence), with remediation guidance

User expectations:
- Templates make behavior more consistent and structured.
- Templates can make the system stricter (more blocks) but never silently less safe.
- Users can see the selected template and the derived policy overlay summary.
- Users can preview the generated prompt before starting a task (`/templates preview-prompt`).

### Template Config Model (Global vs Per-Agent)

Configuration must support:
- **Global defaults** (applies to all agent sessions unless overridden)
- **Per-agent overrides** (applies to a specific agent identity/profile)

Minimum configuration surface:
- `templates.selection_mode = off|once|every_time`
- `templates.default_for_picker_template_id = <template_id>|null`

Session state (not persisted as config by default):
- `templates.sticky_template_id = <template_id>|null` (cleared by `/templates clear` or on session restart)

State needed for UX quality:
- MRU list with last-used timestamps (for ordering and “used X ago” display)
- Store MRU in PrintRevolt-owned state (under `CODEX_HOME/printrevolt/`), bounded retention

### Pipelines and Policy

- Pipelines are orchestration, not an escape hatch.
- Pipeline-initiated tool calls are still subject to approvals/policy/hook constraints.
- “Manual run” exists for debugging and parity with supervisors, but it must not bypass gates.

### Pipeline Creation and Generation (Customer-Expected UX)

Pipelines must be creatable without users hand-authoring JSON. The “best” customer experience is:

1. User intent: “set up verification for this repo” or “run frontend tests on changes”.
2. PrintRevolt produces a **draft pipeline** derived from bounded repo scan + known scripts/config.
3. User reviews the draft in a dedicated review step:
   - pipeline name/id, triggers, and which commands run when
   - safety posture (capabilities, trust requirements)
   - what evidence it produces and how it gates finalize (if configured)
4. User requests changes (optional) and iterates until satisfied.
5. User explicitly chooses where to save:
   - user scope (safe default)
   - repo scope (requires explicit confirmation and trust posture check)

Backstops:
- If generation fails or the repo is unfamiliar, offer a minimal “verify-only” skeleton pipeline plus guidance.
- If a generated pipeline would create a posture mismatch (e.g., `verify_execution="pipeline_only"` but no verify coverage), the review step must flag it as high-severity and block apply until resolved.

## Backups, Snapshots, and Rollback

Any apply/save that changes persisted state must create an automatic backup snapshot:

- Backup triggers:
  - applying a policy recommendation
  - saving a template over an existing template file
  - saving or updating a pipelines bundle
- Backup properties:
  - stored under PrintRevolt-owned paths (e.g., `CODEX_HOME/printrevolt/backups/...`)
  - bounded retention with clear cleanup policy
  - restore is a one-step action from TUI (`/policy restore`, `/pipelines restore`) and headless CLI (`codex-pr backups restore`)
- Last-known-good:
  - keep a pointer to the most recent applied-good snapshot for quick fallback
  - if parsing/validation of current config/bundles fails, fail closed and offer restore

## Implementation Phases

### Phase 1: Policy Introspection and Advisor Preview

Goals:
- Make policy visible and explainable.
- Provide “recommendations” preview as a first-class flow.

Work:
- Implement `codex-pr policy status|explain` (or equivalent Rust API) returning:
  - effective policy summary
  - floors summary
  - last decision + reason codes
- Extend `codex-pr doctor --recommend` output to include stable ids and patch previews suitable for a TUI picker.
- Add `/policy status`, `/policy why`, `/policy recommend` in the TUI.

Acceptance criteria:
- Users can see effective policy and understand why an action was denied.
- Recommendations can be previewed without writing files.
- Any apply action creates a backup snapshot and can be restored.

### Phase 2: Templates Slash UX + Preview

Goals:
- Make template discovery/validation usable inside the agent session.
- Provide a predictable draft creation flow with preview.

Work:
- Wire `/templates list|validate|current`.
- Add `/templates preview` and `/templates preview-prompt`.
- Implement selection-mode UX (`off|once|every_time`) and composer indicator state.
- Implement picker ordering and MRU timestamps (“used X ago”).
- Implement “default-for-picker” semantics (visible in picker; not auto-applied unless prompted).
- Add a draft flow:
  - generate draft template from prompt text (heuristic-first; model-based optional)
  - show preview
  - allow Continue (don’t save) or Save (user/repo scope with explicit confirmation)
- Ensure trust gating for repo templates is clear.

Acceptance criteria:
- `/templates list` matches `codex-pr templates list` behavior.
- Draft creation always provides a preview.
- Selecting a template shows the resulting overlay summary in-session.
- Users can see the exact generated prompt before task start.
- Global vs per-agent template selection mode behaves as configured.

### Phase 3: Pipelines Data Plane + Create/Generate + Manual Run

Goals:
- Make pipelines discoverable and runnable (safely).
- Provide create/generate flows so users do not hand-author JSON.
- Provide a manual run command for debugging and parity.

Work:
- Add `codex-pr pipelines list|show|run` (JSON-capable).
- Add `codex-pr pipelines draft|validate|apply|restore` (JSON-capable; preview by default).
- Add `/pipelines list|show|run|status|create|edit|restore`.
- Implement “synthetic trigger” semantics (manual run is equivalent to a trigger firing).
- Implement pipeline generation:
  - bounded scan -> propose draft pipeline(s)
  - run through the universal review flow (revise/regenerate/apply)
  - validate before apply; on validation failure, block apply with actionable errors
- Ensure pipeline run state and remediation outputs are surfaced in a bounded way.

Acceptance criteria:
- Manual run does not bypass policy/approvals.
- Pipeline failures can surface a bounded “suggested user message” that can be sent to the agent only with user confirmation.
- Generated pipelines are always reviewable before saving, and can be restored from backups.

### Phase 4: “Apply” and Persistence Workflows

Goals:
- Allow applying selected recommendations/templates/pipeline bundles safely.

Work:
- Add explicit apply commands (TUI + headless) that:
  - show preview
  - require confirmation
  - write only PrintRevolt-owned files by default
  - record audit events

Acceptance criteria:
- No silent writes.
- Applied changes are reproducible and auditable.
- Restores are fast and predictable (no manual recovery steps).

## Testing Strategy

- Unit tests:
  - policy explainability (stable reason codes)
  - advisor recommendation determinism (same inputs -> same bundle)
  - template contract validation and overlay mapping
  - pipeline compilation determinism and guardrails

- Integration tests:
  - TUI slash commands invoke the correct underlying APIs and render expected summaries
  - pipeline-initiated tool calls still pass through policy + approvals boundary
  - trust gating behavior matches documented posture

## Open Questions (To Finalize Before Phase 3/4)

1. Should `/pipelines run` exist in the TUI by default, or only when an “advanced” toggle is enabled?
2. What is the minimum “patch preview” format we want to standardize on for the TUI (unified diff vs structured JSON patch)?
3. Do we want a single unified `/pr` namespace (e.g., `/pr policy ...`) or keep top-level commands (`/policy`, `/templates`, `/pipelines`)?
4. What is the default backup retention policy (count-based vs age-based), and should it be configurable?
