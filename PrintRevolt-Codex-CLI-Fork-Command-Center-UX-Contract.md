# PrintRevolt × Codex CLI Fork: Command Center UX + Integration Contract

This document consolidates the **core fork design (LLD)** and the **agent-session slash command extension** into a single, UX-ready contract for building PrintRevolt **Command Center** mocks and product flows.

It answers:
- What runs **in agent (TUI) vs outside agent** (headless).
- What each feature does (**templates, policy, pipelines, sessions**).
- What the **supervisor** (Command Center) can control/disable.
- The **review/approval** and **backup/restore** guarantees.
- Concrete **UX scenarios** and user expectations.

If there is any conflict, **tool-boundary safety (policy + approvals) wins**.

---

## 1) Scope and Goals

### 1.1 What the fork guarantees

PrintRevolt’s fork of Codex CLI enforces safety **inside the Codex process**:
- Every tool call is intercepted and evaluated **before execution**.
- Hooks may deny/modify, but policy re-validates after modifications.
- Finalization can be gated (e.g., verification evidence required).

This is the “non-bypassable boundary”: even if the model outputs something unsafe, the process will not execute disallowed actions.

### 1.2 What Command Center builds on top

Command Center provides “supervisor UX” and can:
- Disable certain TUI slash commands so users are funneled into Command Center flows.
- Run **headless** PrintRevolt operations (templates/policy/pipelines/backups) outside agent sessions.
- Present review loops and patch previews before applying any persistent change.

Command Center **does not weaken** enforcement. It only orchestrates it.

---

## 2) Terminology

- **Agent session (TUI):** The interactive terminal UI where a user chats with Codex and can use slash commands.
- **Headless CLI (`codex-pr`):** PrintRevolt’s compatibility surface for data-plane operations (list/validate/preview/apply/restore).
- **Supervisor / Command Center:** A parent product that launches or embeds Codex and may provide its own UX for templates/policy/pipelines.
- **Policy floor:** Non-bypassable minimum safety rules. Suggestions/templates/pipelines may tighten behavior but cannot loosen floors.
- **Trusted repo:** A repo explicitly allowlisted (config) so repo-provided hooks/templates/pipelines can be used.
- **Mode A vs Mode B config:** PrintRevolt reads both but writes by default to **Mode B** (`CODEX_HOME/printrevolt.toml`) to avoid clobbering upstream config.

---

## 3) Where Features Run (Agent vs Outside Agent)

### 3.1 In agent session (TUI)

The TUI supports user-facing slash commands (when enabled):
- `/templates ...` prompt template selection, drafting, preview, mode/default management
- `/policy ...` policy visibility and recommendation preview (apply is a reviewed write)
- `/pipelines ...` pipeline list/show/create/restore (manual run/status may be gated or advanced)

Template selection may also occur implicitly at **prompt send time** based on configuration (off/once/every_time).

### 3.2 Outside agent session (Command Center or scripts)

Use `codex-pr` for:
- Config resolution (`doctor`)
- Template discovery/validation/compose-prompt preview
- Policy status/explainability
- Pipelines list/show/draft/restore
- Backups list/restore

Command Center typically uses `codex-pr` to implement:
- “Preview before apply” and “review loop” UIs
- Project-aware suggestions (bounded scan) without binding to the TUI

### 3.3 Supervisor control when running the TUI

When Codex is launched under Command Center, Command Center can disable specific slash commands so the user uses Command Center’s UX instead of TUI flows (details in §5).

---

## 4) Enforcement and Review Invariants

### 4.1 Tool boundary sequence (non-bypassable)

For any tool call (user-driven or pipeline-driven):
1) Policy pre-check (fast allow/deny)
2) Optional hook chain (may deny or modify tool call)
3) Policy re-validation / clamp after modification
4) Tool execution
5) After-tool accounting (audit + verify evidence tracking)

### 4.2 Review-first, apply-only-on-confirm

Any flow that:
- generates a template/pipeline/policy suggestion, OR
- writes persistent state

must have an explicit UX phase where the user can:
- **Preview** the artifact / diff / final payload
- **Approve**, **request changes**, **regenerate**, or **cancel**

Cancel must be side-effect free.

### 4.3 Backups and restore are first-class

Any write that replaces an existing file must create an automatic backup, and restore must be available from UX and headless CLI.

---

## 5) Supervisor (Command Center) Controls

### 5.1 Disable slash commands

Command Center can disable slash command groups:
- `templates`
- `policy`
- `pipelines`

Mechanisms:
- Env: `PRINTREVOLT_UI_DISABLE_SLASH_COMMANDS=templates,pipelines,policy`
- Config: `[printrevolt.ui] disabled_slash_commands = ["templates","pipelines"]`

Precedence:
- Environment variable overrides config.

Expected behavior:
- Disabled commands are **hidden** from the slash popup.
- If typed manually, they are **rejected** with a clear “disabled by supervisor” message.
- Underlying safety enforcement remains active.

### 5.2 Trust gating for repo-provided assets

Repo-provided hooks/templates/pipelines are:
- discoverable, but
- disabled unless the repo root is allowlisted (`trusted_repo_roots`).

UX must clearly communicate “repo not trusted” when a repo-provided item is unavailable.

---

## 6) Configuration Model and Precedence (for UX + product logic)

### 6.1 Config layers (high level)

Effective PrintRevolt config is resolved from layered sources:
1) Defaults
2) User Mode A (`CODEX_HOME/config.toml`) then User Mode B (`CODEX_HOME/printrevolt.toml`)
3) Project Mode A/B (`<repo>/.codex/...`) when present
4) Optional Session overlay (env `PRINTREVOLT_SESSION_CONFIG`)
5) Optional CLI overrides

PrintRevolt writes by default to **Mode B** (`CODEX_HOME/printrevolt.toml`).

### 6.2 Template UI config keys (global + per-agent)

Global:
- `printrevolt.templates.selection_mode = "off" | "once" | "every_time"`
- `printrevolt.templates.default_for_picker_template_id = "<template_id>" | ""`

Per agent override:
- `printrevolt.agents."<agent_id>".templates.selection_mode = ...`
- `printrevolt.agents."<agent_id>".templates.default_for_picker_template_id = ...`

Precedence:
1) Per-agent override (if present)
2) Global default
3) Built-in default

### 6.3 Session state vs durable config

Durable (file-backed):
- template selection mode / default-for-picker
- supervisor disable list
- (future) policy recommendations applied to `printrevolt.toml`
- (future) pipeline bundle selections

Session-only:
- `sticky_template_id` (for `mode=once`)
- `last_selected_template_id` (used for preselect UX)
- pending review objects (prompt/template/pipeline proposals)

Semi-durable PrintRevolt UX state:
- templates MRU timestamps in `CODEX_HOME/printrevolt/state/templates.json` (bounded retention)

---

## 7) Templates (UX Contract)

### 7.1 Template discovery locations

- User templates: `CODEX_HOME/templates/*.md`
- Draft templates: `CODEX_HOME/printrevolt/drafts/*.md`
- Repo templates (trusted only): `<repo>/.codex/templates/*.md`

Each discovered template has:
- `template_id` (stable: `user:...`, `draft:...`, `repo:...`)
- `name`, `description`, `tags`
- a parsed “contract” section set used for prompt composition

### 7.2 Selection modes (customer expectation)

- `off`: never prompt, never apply; generated prompt == raw prompt
- `once`: if no sticky template is set, prompt via picker on send; approved selection becomes sticky until cleared/switched
- `every_time`: picker appears on every send; selection is not sticky (but can be preselected)

### 7.3 Picker ordering contract

When shown:
1) `No template`
2) `[default] <template>` (only if configured and resolvable)
3) `New Prompt Template...`
4) MRU templates (most recent first; show “used X ago” when available)
5) Remaining templates alphabetical

### 7.4 Review-before-send contract

After a user picks a template (or “No template”), show a review step:
- Exact **generated prompt** (the string that will be sent)
- `sha256` of generated prompt
- Template label (id if known; “one-time” if not persisted)
- Actions:
  - Approve and send
  - Change template
  - Edit prompt (return raw to composer)
  - Cancel (return raw to composer)

Semantics:
- The **raw prompt** is what appears in transcript/history.
- The **generated prompt** is what is sent to the model.
- Once sent, the template selection for that task is immutable.

### 7.5 “New Prompt Template” wizard contract

The wizard exists in two entrypoints:
- Picker item: `New Prompt Template...` (applies to the pending prompt)
- Slash command: `/templates draft` (draft without applying)

Wizard start step (customer expectation):
- The first screen is a choice:
  - **Generate with Codex…** (Codex fills in fields from a “template intent” prompt)
  - **Write manually…** (user edits fields directly)
- The “template intent” prompt is **separate from the task prompt**.
  - The user’s pending task prompt remains in the composer/pending submission until the user later chooses “Use once” or “Save and use”.

Wizard fields (editable, multi-line):
- Name
- Description
- Tags (comma-separated; optional)
- Role + Objective
- Procedure
- Outputs
- Policy Defaults (advisory text)
- Tooling Scope

Wizard review step must allow:
- Generate with Codex… (if not yet generated) OR Regenerate with changes… (if previously generated)
- Use once (do not save) (when invoked from picker)
- Save (Drafts/User/Repo scope) (with backup if overwriting)
- Save and use (when invoked from picker)
- Edit
- Cancel

Generation UX details:
- The “Generate with Codex” turn is treated as an internal, structured-output request.
- The generated JSON is parsed into wizard fields; it should not be shown as a normal assistant message in the transcript.

Save scopes:
- **Drafts**: `CODEX_HOME/printrevolt/drafts/`
- **User**: `CODEX_HOME/templates/`
- **Repo**: `<repo>/.codex/templates/` (only if repo trusted + repo root known)

---

## 8) Policy (UX Contract)

### 8.1 What Policy governs

Policy is the safety and operational rule set that decides:
- allow / deny tool calls (and why)
- whether finalize is allowed (e.g., verification evidence required)

Policy is enforced at the boundary regardless of UX surface.

### 8.2 What UIs must show

Minimum UX for Command Center:
- Effective policy summary (“what is active now?”)
- Explainability for the last deny/modify (“why did it block?”)
- Recommendations (bounded scan) presented as previewable patches
- Apply with explicit confirmation + backup
- Restore from backups

TUI contract:
- `/policy status` shows effective values
- `/policy why` shows last recorded non-allow decision (best-effort via audit)
- `/policy enable|disable` toggles policy enforcement in selected config scope
- `/policy recommend` shows bounded recommendations
- `/policy apply <id>` should enter review/apply flow (future: patch preview + confirm + write)
- `/policy restore` restores from backup snapshot

---

## 9) Pipelines (UX Contract)

### 9.1 What pipelines are (and are not)

Pipelines are deterministic orchestration:
- preflight checks
- verify/fix loops
- evidence production

They are not a bypass:
- pipeline-initiated actions still go through approvals/policy/hook constraints.

### 9.2 Bundle locations

User (global) bundle:
- `CODEX_HOME/printrevolt/pipelines.json`

Repo/project bundle:
- `<repo>/.codex/printrevolt/pipelines.json`

### 9.3 UX flow: create/generate pipeline (customer expectation)

Create flow:
1) Bounded scan of repo root (manifests, package scripts, lockfiles, existing config)
2) Generate a draft pipeline (often “Verify”)
3) Review step:
   - what it runs, when it would run (future: lifecycle integration), and safety posture
   - expanded/compiled view (for example `codex-pr pipelines show --expanded --json`)
   - what it writes (if any)
4) Revise/regenerate loop (optional)
5) Save/apply with explicit confirmation and backup

TUI baseline contract:
- `/pipelines create` generates a starter pipeline and shows a review-before-save UI
- `/pipelines enable|disable` toggles pipeline functionality in selected config scope
- `/pipelines list|show|restore` provide visibility and rollback

Manual run/status:
- Should be supported as an “advanced” action in Command Center (future: synthetic triggers), but must not bypass gates.

---

## 10) Backups, Restore, and Audit (UX Requirements)

### 10.1 Backups

Backups are stored under:
- `CODEX_HOME/printrevolt/backups/<kind>/...`

Kinds used by current implementation:
- `config`, `restore` (for `printrevolt.toml`)
- `template` (for template overwrites)
- `pipelines` / `pipelines_restore` (for `pipelines.json`)

UX expectations:
- “Apply” or “Save” must display the backup created (or state “no prior file to backup”).
- Restore must require confirmation and create a backup of the current file first.

### 10.2 Audit log (for explainability + debugging)

Audit events are written to:
- `CODEX_HOME/printrevolt/audit/events.jsonl` (rotated)

Command Center can use this to:
- power “Why was this blocked?” views
- show last decisions and remediation

---

## 11) UX Scenarios (ready-to-mock)

### Scenario A: Templates `mode=once`, first prompt
1) User types prompt and presses Enter.
2) Picker opens (No template / default / New Prompt Template / MRU / alphabetical).
3) User selects a template.
4) Review opens showing generated prompt + sha256.
5) User approves.
6) Task starts; composer indicator shows selected template; future prompts use sticky template.

### Scenario B: Templates `mode=once`, user clears sticky
1) User runs `/templates clear` (or Command Center clears session sticky).
2) Next prompt send re-opens picker.

### Scenario C: Templates `mode=every_time`
1) Every send triggers picker + review.
2) Selection affects only that prompt; preselect last-used for speed.

### Scenario D: Supervisor disables templates in TUI
1) Command Center launches Codex with `PRINTREVOLT_UI_DISABLE_SLASH_COMMANDS=templates`.
2) Slash popup hides `/templates`.
3) If user types `/templates`, TUI shows “disabled by supervisor”.
4) Command Center provides its own Templates UI, using headless operations and the same review/apply rules.

### Scenario E: Repo is untrusted
1) Repo templates/pipelines are not loaded.
2) UI shows warning “repo templates disabled (repo not trusted)”.
3) Any attempt to “save to repo templates” is blocked with a clear error and remediation (trust/allowlist).

### Scenario F: Policy deny explainability
1) Tool call is blocked by policy.
2) Audit records a policy decision event.
3) `/policy why` (or Command Center “Why”) shows decision kind, reason code, and message.

### Scenario G: Pipelines create + restore
1) User chooses “Create pipeline”.
2) Review shows proposed pipeline.
3) Save writes `pipelines.json` in the selected scope (`global` or `project`) and creates a backup if replacing.
4) Restore lists backups and restores after confirmation.

---

## 12) What Command Center Needs to Mock (Checklist)

Templates:
- Template picker (ordering rules, MRU “used X ago”, default-for-picker placement)
- Generated prompt review (approve/change/edit/cancel)
- Template wizard (field entry → review → save/use-once)
- Config controls (selection mode + default-for-picker; global vs per-agent scope)
- Session controls (sticky clear/switch)

Policy:
- Status dashboard (effective values + repo trust)
- “Why” screen (reason codes, last deny/modify)
- Recommendations list with preview and apply (review loop + backup)
- Restore selector + confirm

Pipelines:
- List/show
- Create (generate → review → save)
- Restore selector + confirm
- (Advanced) manual run/status UI that mirrors trigger semantics without bypassing gates

Supervisor:
- Toggle/declare which command groups are disabled in TUI
- Surfaces for “repo trusted” posture and allowlist management
