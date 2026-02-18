# Agent Workflows (PRD -> UX -> Dev): Implementation Plan

This document is a thorough, implementation-ready plan for adding **agent-orchestrated, multi-phase workflows** to this fork. The goal is to support flows like:

User prompt -> generate PRD (via prompt template) -> user review/feedback loop -> generate UI/UX plan -> review/feedback loop -> development -> verification -> complete.

## Executive Summary

Current `pipelines.json` (schema `"1"` / `"2"`) is a **typed tool/command orchestration engine**. It is not a workflow engine for agent-generated artifacts and iterative review loops.

This plan proposes introducing **Agent Workflows v1** as a new layer that:
- orchestrates agent generation steps, review/feedback loops, and phase transitions
- reuses existing PrintRevolt assets where possible (templates, policy, audit, pipelines)
- preserves the existing safety boundary: any tool execution still flows through policy + hooks + approvals
- supports global and project scope uniformly, with trust gating for project scope
- defaults safe-by-default: generated starter workflows are disabled until explicitly enabled

High-level decision:
- Create a new `workflows.json` (schema `"1"`) rather than extending `pipelines.json` to schema `"3"`.
- Rationale: avoids breaking/complicating existing pipelines, and cleanly separates "agent orchestration" from "typed tool runner".

## Current State (What We Have Today)

1. Pipelines
- Stored in `pipelines.json` (global and project scope), schema `"1"` / `"2"`.
- Schema `"2"` supports reusable pipeline components (`components` + `use_component`) with compile-time expansion.
- Pipeline runtime is linear and typed, with a limited set of parts.
- Pipelines feature gate exists: `[printrevolt.pipelines] enabled = false|true`.
- Per-entry `enabled` exists in pipeline entries; starter drafts set `enabled=false`.
- Project pipelines are trust gated (repo must be allowlisted).

2. Templates
- Markdown templates with YAML frontmatter and a prompt contract.
- `codex-pr templates compose-prompt` can generate the final prompt string and sha256 for review.
- Templates selection UI defaults to off (`selection_mode="off"`).

3. Policy, hooks, audit
- Tool boundary enforcement exists today and must remain the enforcement point.

## Goal: What "Agent Workflows" Must Support

1. Phase-based orchestration
- PRD phase, UX phase, implementation phase, verification phase, completion phase.
- Each phase can generate one or more artifacts.

2. Review and revision loops
- After artifact generation, user must be prompted to approve or provide feedback.
- If feedback is provided, workflow repeats generation for that artifact with feedback injected.
- Revision loops must be bounded.

3. Prompt templates per step
- Each generation step uses a prompt template (existing PrintRevolt templates).
- The user's initial prompt is passed through.
- Prior artifacts and feedback can be included.

4. Uniform configuration and scoping
- Global and project definitions supported uniformly.
- Project scope trust gated.
- Enable/disable at the feature level and at the individual workflow level.
- Defaults safe-by-default.

5. Headless and interactive parity
- Must be runnable headlessly (CLI) with a well-defined "action protocol".
- Must be embeddable into TUI/Command Center with the same behavior.

## Non-Goals (v1)

1. "Fully autonomous app builder"
- The system should not execute tools automatically without review gates and policy boundaries.

2. Arbitrary code execution inside the workflow engine
- Tool execution remains tool-based and policy mediated.

3. Unlimited conversational branching
- v1 should be "structured loops" and "explicit transitions", not a general programming language.

## Core Design Decisions

### Decision A: New `workflows.json` (recommended)

Add:
- Global: `<CODEX_HOME>/printrevolt/workflows.json`
- Project: `<repo_root>/.codex/printrevolt/workflows.json` (trusted only)

Why:
- `pipelines.json` remains focused on typed tool/command runner semantics.
- Agent workflows have different primitives and state (artifacts, review feedback, phase transitions).

### Decision B: Reuse PrintRevolt Templates as Prompt Templates

Generation steps reference existing `templates` by `template_id`.

Why:
- One template system for interactive user prompt composition and workflow step prompts.
- Consistent hashing, preview, and provenance.

### Decision C: Introduce an Artifact Store with Content Addressing

Workflows produce artifacts with:
- `kind`: `prd`, `ux_plan`, `implementation_plan`, `diff_plan`, `release_notes`, etc.
- `content_sha256`
- `template_id` and `generated_prompt_sha256`
- `created_at_ms`, `workflow_run_id`, `step_id`

Artifacts should be stored as files and referenced by stable ids.

## Proposed Architecture

### New Crate: `codex_pr_workflows`

Add a new Rust crate:
- Schema types and validation (`WorkflowsFileV1`, `WorkflowEntryV1`, `WorkflowGraphV1`)
- Execution engine for workflow control flow (`WorkflowEngine`)
- Artifact store interface (`ArtifactStore`)
- Action protocol (`WorkflowAction`) used by runtime/CLI/TUI

### Relationship to Existing Pipeline Engine

Workflows can reuse pipelines in two ways:
1. A workflow step can invoke an existing pipeline by id (`run_pipeline` step).
2. A workflow can inline tool/command orchestration by delegating to `codex_pr_pipelines` engine internally.

Recommendation for v1:
- Implement a `run_pipeline` step that invokes an existing pipeline entry (expanded first for validation).
- Keep pipeline definitions and pipeline component reuse separate and intact.

### Workflow Engine Model (State Machine)

Represent workflows as an explicit state machine of steps with transitions:
- Each step yields a `WorkflowAction` until the step completes.
- The runtime executes the action (invoke agent, request approval, write artifact, run pipeline).
- The step consumes the result and transitions.

This preserves deterministic behavior and makes headless execution possible.

## Schema Proposal: `workflows.json` (Schema "1")

Top-level:
- `schema_version`: `"1"`
- `components`: optional for reusable step blocks (future; not required for v1 MVP)
- `workflows`: map of `workflow_id -> WorkflowEntryV1`

`WorkflowEntryV1`:
- `id`: string
- `name`: string
- `enabled`: bool (starter drafts default `false`)
- `entry`: step id
- `steps`: map of `step_id -> StepV1`

`StepV1` kinds (minimum set for the described flow):

1. `generate_artifact`
- Inputs: `template_id`, `artifact_kind`, `inputs` (list of named bindings, sourced from workflow context)
- Outputs: `artifact_ref` (stored output), `generated_prompt_sha256`

2. `review_artifact`
- Inputs: `artifact_ref`, `prompt` (templatable display string)
- Outputs: `approved` boolean, `feedback` string (optional, bounded)
- Transitions: `on_approved -> next_step_id`, `on_feedback -> next_step_id`

3. `revise_artifact`
- Inputs: `template_id` (revision prompt template), `artifact_ref`, `feedback`
- Outputs: `artifact_ref` (new revision)

4. `run_pipeline`
- Inputs: `pipeline_id`, `scope` (`global|project|effective`) or resolved at runtime
- Outputs: pipeline run status, notes, evidence references

5. `complete`
- Terminal.

Bounded loops:
- Each `review_artifact` step should have `max_revisions` (default 3) and `revision_counter_key` (stored in workflow facts).

## Workflow Context and Dataflow

Workflows need a safe and explicit data model. Recommended channels:

1. Inputs (immutable for a run)
- `user_prompt` (the original user prompt)
- `repo_root` (optional)
- `selected_template_id` (optional)
- workflow parameters (non-secret)

2. Facts (mutable within a run)
- `artifact_refs` by kind and step id
- `feedback` strings (bounded)
- counters (`revision_count.*`)
- pipeline outcomes (bounded summary, not raw logs)

3. Artifacts (durable)
- PRD doc, UX plan, implementation plan, etc.
- stored as files, referenced by `artifact_ref`

Rule:
- Do not store secrets in facts or artifacts by default.
- Enforce size limits for feedback strings and artifact content.

## Prompt Template Strategy

### Template Reference

Use template ids from the existing templates system.

Generation prompt building:
- Use `codex-pr templates compose-prompt` logic internally (same library path) so the workflow engine can produce the exact generated prompt string and `generated_prompt_sha256` for audit and review.

### Step Prompt Inputs

Standardize a minimal input object the workflow engine passes to prompt composition:
- `user_prompt`
- `phase` (for example `prd`, `ux`, `dev`)
- `artifact_history` (refs + bounded previews)
- `feedback` (bounded)
- `repo_context` (optional; bounded metadata only)

Implementation note:
- Templates today do not have a native structured variable system; they are "prompt wrappers".
- v1 can pass "inputs" by pre-rendering a structured header section (JSON or YAML) into the raw prompt and letting the template wrap it.

## Runtime Integration Plan (Codex Session)

Workflows require the runtime to execute new kinds of actions.

Add a new action protocol:
- `WorkflowAction::InvokeAgent { prompt, prompt_sha256, output_schema_hint, bounds }`
- `WorkflowAction::RequestUserReview { artifact_ref, summary, choices }`
- `WorkflowAction::RunPipeline { pipeline_id, expanded_preview_sha, bounds }`
- `WorkflowAction::EmitNote { message }`
- `WorkflowAction::Done { status }`

Runtime responsibilities:
- Execute `InvokeAgent` by calling the underlying model, producing an artifact payload.
- Execute `RequestUserReview` through UI: show artifact, capture approve/feedback.
- Execute `RunPipeline` by delegating to existing pipeline runtime in-process.

## CLI Plan (`codex-pr workflows ...`)

Add a headless CLI surface similar to pipelines:

1. `codex-pr workflows list --scope global|project|both [--json]`
2. `codex-pr workflows show --scope ... --id <id> [--json]`
3. `codex-pr workflows draft --scope global|project [--apply]`
- Generates a starter "PRD -> Review -> UX -> Review -> Complete" workflow. Default: `enabled=false`.
4. `codex-pr workflows enable|disable --scope global|project`
- Toggles feature gate key `[printrevolt.workflows] enabled=...`.
5. `codex-pr workflows run --id <id> ...`
- Headless action protocol mode: either executes fully if in interactive terminal mode, or emits JSON actions to be consumed by a supervisor (Command Center).

Backups:
- Any write to `workflows.json` should create backups under `CODEX_HOME/printrevolt/backups/workflows`.

## CLI Contract (Detailed)

This section defines the concrete CLI UX so it is easy to integrate in terminal, supervisors, and TUI implementations.

### Common flags

- `--project-root <path>`: override project root detection (required for `--scope project` when repo root cannot be detected).
- `--scope global|project|both`: read scope. Writes accept only `global|project`.
- `--json`: emit machine-readable JSON (no extra text).

### `codex-pr workflows list`

Intent: show discovered workflows and provenance.

JSON shape (proposed):
- `scope`
- `global_path`
- `project_path`
- `warnings` (for example "project workflows ignored (repo not trusted)")
- `effective` list of entries: `{ id, name, enabled, source }`

### `codex-pr workflows show`

Intent: show a single workflow entry by id.

JSON shape (proposed):
- `entry`
- `warnings`

### `codex-pr workflows draft`

Intent: generate a starter workflow definition (PRD -> review -> UX plan -> review -> complete).

Rules:
- Preview by default (prints JSON).
- `--apply` writes to the selected `workflows.json` with backup.
- Starter entry defaults `enabled=false`.

### `codex-pr workflows enable|disable`

Intent: toggle the feature gate in config:
- `enable` writes `[printrevolt.workflows].enabled=true` in the selected scope.
- `disable` writes `[printrevolt.workflows].enabled=false` in the selected scope.

### `codex-pr workflows run`

Intent: execute a workflow run in one of two modes.

Flags (proposed):
- `--id <workflow_id>` (required)
- `--mode interactive|emit-actions` (default: `interactive` when stdout is a tty; otherwise `emit-actions`)
- `--run-id <id>` (optional resume; otherwise a new run id is created)
- `--input-prompt <string>` (optional; defaults to reading from stdin in headless mode, and from interactive UI in TUI mode)

Mode: `interactive`
- Prompts user for approvals and feedback in-terminal.
- Writes artifacts to the artifact store as steps complete.

Mode: `emit-actions`
- Emits a stream of JSON objects (one per line, JSONL) describing the next action to perform.
- Consumes results on stdin (JSONL) to advance the run (supervisor-driven).

Action protocol (proposed, JSONL):
- `{"kind":"note","message":"..."}`
- `{"kind":"invoke_agent","run_id":"...","step_id":"...","prompt":"...","prompt_sha256":"...","bounds":{...}}`
- `{"kind":"request_review","run_id":"...","step_id":"...","artifact_ref":"...","summary":"...","max_feedback_bytes":8192}`
- `{"kind":"done","run_id":"...","status":"approved|needs_human|cancelled|error","message":"..."}`

Result protocol (proposed, JSONL to stdin):
- `{"kind":"agent_result","run_id":"...","step_id":"...","content":"...","content_type":"text/markdown"}`
- `{"kind":"review_result","run_id":"...","step_id":"...","approved":true,"feedback":""}`
- `{"kind":"cancel","run_id":"..."}`

Exit codes (proposed):
- `0` completed successfully (status approved/complete)
- `10` needs human (revision bound exceeded or explicit block)
- `20` cancelled
- `30` invalid config / disabled / not trusted / missing workflow id
- `40` internal error

## Configuration Keys

Add a new feature gate:

```toml
[printrevolt.workflows]
enabled = false
max_revisions = 3
max_artifact_bytes = 262144
max_feedback_bytes = 8192
```

Rules:
- Defaults safe-by-default (`enabled=false`).
- Starter workflow drafts default to `workflow.enabled=false`.

## Scoping, Precedence, and Trust

Mirror pipelines/templates:

1. Scope
- Global and project bundles exist.
- Read operations can merge both with a precedence order.

2. Precedence
- Project workflows override global workflows by `workflow_id`.

3. Trust gating
- Project workflows are ignored unless repo root is allowlisted in `printrevolt.hooks.trusted_repo_roots`.
- CLI should return warnings when project workflows are ignored for lack of trust.

## Safety, Policy, and Audit

Safety invariants:
- Only tool/command execution goes through policy/hook/approval boundary.
- Agent generation steps never execute tools directly.

Audit requirements:
- Record all workflow runs and step transitions.
- Record: workflow id and scope provenance.
- Record: step id and kind.
- Record: generated prompt sha256.
- Record: artifact sha256 and storage location.
- Record: user review decisions and feedback hashes.
- Record: pipeline invocations (pipeline id, expanded sha).

Data minimization:
- Feedback stored bounded.
- Artifact content may be persisted, but should be size-limited and redactable.

## Artifact Store

Recommendation:
- Default artifact root: `CODEX_HOME/printrevolt/artifacts/workflows/<workflow_run_id>/...`
- Optional repo writeback step can be a separate explicit step type later.

Artifacts should include metadata files:
- `<artifact_id>.md` or `.json` content file
- `<artifact_id>.meta.json` metadata

Backups:
- If a workflow step writes into the repo, it must create a backup snapshot similar to pipeline and config backups.

## Testing Strategy

1. Unit tests
- Schema parsing, validation, trust gating, precedence merging.
- Deterministic step transitions and loop bounds.
- Artifact hashing and size limits.

2. Integration tests (no network)
- Provide a fake `AgentExecutor` that returns deterministic strings.
- Execute a workflow run end-to-end (approve path): generate artifact -> approve -> next phase.
- Execute a workflow run end-to-end (feedback path): generate artifact -> feedback -> revise -> approve.
- Execute a workflow run end-to-end (guard path): loop guard triggers after max revisions.

3. Golden tests
- Starter draft JSON output stable.
- Prompt sha256 stable for fixed inputs.

## UX / Command Center Integration

Minimum UX requirements:
- Show current phase and step.
- Render artifact for review.
- Allow approve or enter feedback text.
- Show the prompt template used and prompt sha256.
- Provide cancel and resume behavior.

Key UX contract:
- No silent writes.
- Explicit review and confirmation gates between phases.
- Clear provenance: global vs project definition, trust status, and enablement.

## TUI Slash Commands Contract (Interactive)

This section is the UX contract for implementing easy-to-use interactive flows in the TUI (and for supervisors to mirror).

Slash group: `/workflows`

Commands (proposed):
- `/workflows list` shows effective workflows, with source and enabled state.
- `/workflows show <id>` shows the workflow graph (steps, templates, bounds).
- `/workflows create` runs a wizard that:
 - asks for: workflow name, phase templates (PRD template id, UX template id, etc.), bounds
 - previews the generated `workflows.json` entry and where it would be saved (global vs project)
 - writes only after explicit confirmation and creates a backup
- `/workflows run <id>` starts or resumes a run:
 - displays current phase and last artifact
 - runs generation steps by invoking the agent with a previewable prompt (show prompt sha256)
 - enters a review screen for each artifact with actions: Approve, Provide feedback, Cancel
- `/workflows status [<run_id>]` shows current step, pending action, last artifact, and last decision.
- `/workflows cancel [<run_id>]` cancels a run.

Interaction requirements:
- Every generation step must show a preview of what will be asked (prompt template + generated prompt) before invoking the agent.
- Every artifact must be reviewable before proceeding; feedback is captured as text (bounded) and fed into revise steps.
- The UI must surface trust status and scope provenance (global vs project; project ignored when untrusted).

State and resume:
- Runs should persist resumable state to `CODEX_HOME/printrevolt/state/workflows/` (exact schema TBD in WF-07/WF-08).
- The TUI should offer "resume last run" when a run is incomplete.

## Task Tracker (Maintained)

This table is the single source of truth for prioritization and sequencing. Update it as work lands.

Status values:
- Planned
- In progress
- Blocked
- Done

Priority values:
- P0: required for MVP workflow run
- P1: required for CLI completeness and supervisor integration
- P2: required for TUI/Command Center parity
- P3: nice-to-have / future

| ID | Task | Priority | Status | Depends on | Notes |
| --- | --- | --- | --- | --- | --- |
| WF-00 | Lock schema approach: new `workflows.json` vs extend `pipelines.json` | P0 | Planned | - | This plan assumes new `workflows.json` schema "1". |
| WF-01 | Define `workflows.json` schema "1" (types, validation rules) | P0 | Planned | WF-00 | Include `enabled=false` default for starter drafts. |
| WF-02 | Add config gate `[printrevolt.workflows]` (enabled + bounds) | P0 | Planned | WF-01 | Must default `enabled=false`. |
| WF-03 | Add trust gating + scope merge for workflows (global/project/both) | P0 | Planned | WF-01 | Mirror pipelines/templates trust behavior. |
| WF-04 | Add backups for `workflows.json` writes | P1 | Planned | WF-03 | Store under `CODEX_HOME/printrevolt/backups/workflows`. |
| WF-05 | Create crate `codex_pr_workflows` (engine skeleton + schema IO) | P0 | Planned | WF-01 | Keep the engine pure and testable. |
| WF-06 | Artifact store (content-addressed files + metadata + size limits) | P0 | Planned | WF-05, WF-02 | Default root under `CODEX_HOME/printrevolt/artifacts/workflows/`. |
| WF-07 | Workflow engine control flow (steps + transitions + bounded revision loop) | P0 | Planned | WF-06 | Implement `generate_artifact`, `review_artifact`, `revise_artifact`, `complete`. |
| WF-08 | Action protocol (`WorkflowAction` + results) for headless runner | P0 | Planned | WF-07 | Must support "emit JSON actions" mode for supervisors. |
| WF-09 | CLI: `codex-pr workflows list/show/draft/restore` | P1 | Planned | WF-03, WF-04, WF-05 | Ensure output includes warnings when project scope ignored for trust. |
| WF-10 | CLI: `codex-pr workflows run` (interactive terminal runner) | P1 | Planned | WF-08, WF-09 | Captures approve/feedback and persists artifacts. |
| WF-11 | Wire `InvokeAgent` to real model call path in runtime | P2 | Planned | WF-08 | Needs careful boundary: generation must not execute tools. |
| WF-12 | Add `run_pipeline` step kind (invoke existing pipelines with expanded preview) | P2 | Planned | WF-07 | Optional for v1, required for v1.1 per acceptance criteria. |
| WF-13 | TUI/Command Center UX: review artifact pager + feedback input | P2 | Planned | WF-10, WF-11 | Must implement cancel/resume semantics. |
| WF-14 | Docs: update LLD + PRINTREVOLT docs for workflows | P2 | Planned | WF-01, WF-09 | Keep docs aligned with shipping behavior. |
| WF-15 | Workflow "components" for step reuse (like pipeline components) | P3 | Planned | WF-07 | Defer until v1 is stable. |

## Milestones (Phased Delivery)

Phase 0: Design lock-in
- Finalize `workflows.json` schema v1.
- Finalize action protocol shapes for headless supervisor mode.
- Finalize config keys and defaults.

Phase 1: Data plane + CLI (no runtime integration)
- New crate `codex_pr_workflows` with schema + merge + trust gating.
- `codex-pr workflows list/show/draft/restore` with backups.
- Starter draft generation default disabled.

Phase 2: Artifact store + deterministic execution (mock agent)
- Add artifact store implementation with hashing and bounds.
- Add workflow engine with `InvokeAgent` action, test with fake executor.

Phase 3: Runtime integration (headless)
- Wire `WorkflowAction::InvokeAgent` to actual model invocation in a controlled path.
- Implement `codex-pr workflows run` in interactive mode: prints prompts, captures approval/feedback, writes artifacts.

Phase 4: TUI/Command Center integration
- Add slash commands or supervisor UI integration: `/workflows create|run|status|stop` (naming TBD).
- Implement review UI for artifacts (pager view + feedback input).

Phase 5: Advanced features
- Step components (reuse like pipeline components).
- Repo writeback steps with backups.
- Pipeline invocation as a first-class step with expanded preview and capability clamps.
- Artifact diffing and patch application.

## Open Questions (Need Answers Before Phase 3/4)

1. Artifact write targets: should PRD/UX plan default to repo files or CODEX_HOME artifacts?
2. Template variable model: do we want structured variables inside templates (beyond "wrap the raw prompt")?
3. Supervisor protocol: should headless mode be "emit actions JSON" only, or also support an in-terminal interactive runner?
4. Security posture: do we forbid project workflows entirely by default even in trusted repos unless a separate flag is enabled?

## Acceptance Criteria (v1)

1. A workflow (global scope) can be defined with PRD and UX phases using template ids.
2. Running the workflow produces durable artifacts (content-addressed + metadata) and forces a review decision between phases.
3. Feedback triggers bounded revision loops (default max 3) and continues when approved; exceeding the bound yields a terminal "needs human" outcome.
4. Project workflows are ignored unless repo is trusted, with explicit warnings surfaced in CLI JSON.
5. Feature gates are safe-by-default: `[printrevolt.workflows].enabled=false` by default, and starter workflow entries default `enabled=false`.
6. Headless execution is supported: `codex-pr workflows run` can either run interactively in-terminal or emit JSON actions for a supervisor to drive.

## Acceptance Criteria (v1.1)

1. A workflow can invoke an existing pipeline as a step (`run_pipeline`) and can preview the expanded pipeline before executing it.
2. Pipeline invocations from workflows still transit the normal policy + hooks + approvals boundary with clear audit provenance.
