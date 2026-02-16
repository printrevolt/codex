## 0) Executive Summary (3–6 paragraphs)

PrintRevolt uses Codex CLI in **agent mode** to read, edit, and run commands inside repositories. As adoption scales, “best-effort” prompt instructions (e.g., `AGENTS.md`) are insufficient: the model can ignore them, and safety/operational invariants (branching, verification, destructive command avoidance, finalize-gating) must be enforced deterministically. The fork’s core value is **guaranteed enforcement at the tool boundary**: the Codex process will not execute blocked tool calls, regardless of model output.

This Low-Level Design specifies a PrintRevolt-maintained fork of Codex CLI that isolates changes in `crates/pr_*` and touches only a small set of upstream call-sites: **session configure/init**, **prompt submit (template picker) + `UserTurn` dispatch**, **tool dispatch**, and **turn completion/abort/interrupt**. PrintRevolt adds (1) a policy engine that can allow/deny/modify tool calls and gate completion, (2) a safe-by-default hook bus with a strict script contract, (3) a template picker + draft template wizard that links metadata to policy defaults, (4) a compatibility-first config strategy with **no upstream config clobbering**, (5) structured audit/events logging, and (6) an updater plus upstream-sync automation.

Upstream Codex CLI lives in the `openai/codex` GitHub repository (https://github.com/openai/codex).

The chosen approach is “**policy clamp + hook chain**”: built-in policy performs a fast pre-check, optional script hooks run next (can deny/modify), and built-in policy performs a final clamp and re-validation before the tool executes. This ensures templates/hooks can tighten behavior but cannot weaken PrintRevolt/Org-required safety floors. Verification evidence and staleness are tracked deterministically from tool results, with a freshness window and a repo fingerprint model.

Key design decisions include: (a) a **shared domain types crate** (`pr_types`) to enable clean interfaces across policy/hooks/templates without cyclic dependencies, (b) **read-mostly** config handling and default writes to `printrevolt.toml` (Mode B) to avoid formatting churn and unknown-key loss in upstream `config.toml`, (c) a hook engine that is always active but **repo-provided hooks disabled by default** with a trust allowlist, and (d) Windows + WSL treated as tier-1, with explicit canonicalization/path containment rules and conservative shell parsing.

The primary risks are bypass attempts (alternate tools/MCP, config poisoning, hook spoofing) and cross-platform edge cases (Windows junctions/reparse points, WSL path mixing, shell quoting). Mitigations include: intercepting **all** tool calls (including plugin/MCP tool dispatch), clamping policy floors after hook modifications, ensuring default-deny on evaluation errors for mutating operations, strict repo-root path containment for filesystem mutation tools, storing and verifying update metadata, and comprehensive adversarial tests (symlink/junction traversal, destructive command patterns, and tool interception coverage).

---

## 1) Requirements

### 1.1 Functional requirements

1. Deterministic enforcement for agent-initiated operations within the Codex CLI process:
   - `before_tool` interception: allow/deny/modify tool calls.
   - `before_finalize` interception: gate completion/finalization.
2. Enforce PrintRevolt operational rules (configurable with non-bypassable policy floors):
   - branch safety (no direct writes/commits on protected branches)
   - branch naming scheme requirement
   - verification required before `git push` and/or finalize (per policy)
   - denylist for destructive commands (cross-platform)
   - filesystem mutation constrained to repo root (path containment)
3. Hook bus with lifecycle events:
   - `on_session_start` (fires after upstream session is configured; may fire again on session reconfigure)
   - `on_session_end` (best-effort; fires on clean shutdown only)
   - `before_task` (fires before a new upstream `UserTurn`/task starts; can deny the task)
   - `on_turn_start`
   - `before_tool`
   - `after_tool`
   - `before_finalize` (fires when upstream is about to complete a turn/task; can deny completion)
   - `on_turn_complete`
   - `on_turn_aborted` (reason: `interrupted` | `replaced` | `review_ended`)
   - `on_interrupt`
   - `after_task` (fires when a task completes or aborts)
   - optional high-volume: `on_item_started`, `on_item_completed`
   External script hooks must follow explicit JSON stdin/stdout contracts, deterministic ordering, and safe defaults.
4. Pipeline engine (Pipelines → Workflows → Parts):
   - define deterministic automation as typed/allowlisted “parts” (no arbitrary shell by default)
   - run preflight verification at session/task start
   - run verify/fix loops at finalize time with retry/loop guards
   - execute pipeline-initiated commands through the same tool dispatch path (policy floors + approvals + hooks)
   - support global + project pipelines with inheritance and clamping
5. Template system:
   - discover templates in repo and user locations
   - prompt-time template picker (after prompt submit)
   - draft/one-time template wizard (generate/review/save/regenerate/edit/cancel)
   - template metadata must map to a policy overlay, clamped to policy floors.
6. Configuration and compatibility:
   - preserve upstream Codex config/state with **no clobbering**
   - support Mode A (`[printrevolt.*]` in existing `config.toml`) and Mode B (`printrevolt.toml`)
   - deterministic precedence across CLI overrides, project config, user config, defaults, and template overlay
   - preserve unknown keys and formatting in upstream configs.
7. Updater:
   - non-blocking version checks with caching
   - skip-version persistence
   - safe update mechanisms (npm global update and/or binary swap), never clobbering `CODEX_HOME`
8. Upstream sync automation:
   - PR-based workflow to merge/rebase upstream updates
   - CI-based guardrails limiting diff-sprawl outside `crates/pr_*` and allowlisted touchpoints
9. Observability:
   - structured audit/events logs (JSONL)
   - redaction (strict by default)
   - troubleshooting outputs and “doctor” command.
10. User-facing documentation:
   - concise “what changed vs upstream Codex CLI” overview
   - configuration reference (Mode A/Mode B, precedence, safe defaults)
   - hooks guide (events, lifecycle mapping, JSON contract, trust model)
   - pipelines guide (parts catalog, retry/loop guards, preflight + verify/fix loop examples)
   - templates guide (contract, picker flow, draft wizard)
   - updater guide (channels, safety, rollback)
   - troubleshooting guide (doctor output, common blocks, how to debug hooks safely)

### 1.2 Non-functional requirements

**Performance budgets**
- Added agent session-start overhead (PrintRevolt features, excluding any optional model-based classifier):
  - median ≤ 75ms
  - p95 ≤ 150ms
- Added overhead per tool call (policy evaluation + dispatch interception excluding external hook execution):
  - median ≤ 2ms
  - p95 ≤ 5ms
- Hook execution overhead (per hook process):
  - p95 ≤ 50ms for local scripts
  - p99 ≤ 200ms with IO
- Memory ceilings:
  - steady-state additional memory ≤ 30MB
  - audit buffers bounded; no unbounded in-memory stdout/stderr retention.

**Safety posture**
- PrintRevolt policy is a **hard gate** before tool execution and finalize completion.
- Deny-by-default for mutating tools when policy evaluation or hook execution fails (configurable only to be *more strict*).
- Align with upstream approvals/sandbox: PrintRevolt adds additional enforcement; it must not bypass upstream safety layers.

**Cross-platform requirements**
- Tier-1: Linux, Windows-native, WSL2.
- Tier-1 shells: PowerShell on Windows-native; bash/sh on Linux/WSL.
- Explicit path containment invariants; handle Windows junctions/reparse points.
- Process execution uses structured argv when possible; conservative parsing otherwise.

**Operability requirements**
- Structured event/audit logs with stable schemas and reason codes.
- “Doctor” output for effective config resolution and environment detection.
- Safe mode: disable repo hooks, model classification, and auto-update apply while keeping policy enforcement enabled.
- CI guardrails:
  - fail builds if upstream files modified outside allowlist
  - cross-platform smoke tests.

---

## 2) System Overview

### 2.1 High-level component diagram (ASCII)

```
+-------------------------------+         +------------------------+
|        Upstream Codex CLI     |         |    External Scripts    |
|  (agent loop, TUI, tools)     |         |  (hook executables)    |
+---------------+---------------+         +-----------+------------+
                |                                         ^
     (few call-sites)                                      |
                v                                         |
+---------------+-------------------------------+          |
|         crates/pr_runtime                     |----------+
|  - composition root / wiring                  |  invokes hooks
|  - tool interception adapter                  |
|  - finalize gate adapter                      |
|  - template picker adapter                    |
+----+--------------+--------------+------------+
     |              |              |
     v              v              v
+----+----+    +----+-----+   +----+------+
| pr_config|   | pr_policy |   | pr_hooks  |
| config   |   | decisions |   | hook bus  |
+----+----+    +----+-----+   +----+------+
     |              |              |
     v              v              v
+----+----------------+     +------+-----------------+
| pr_templates         |     | pr_audit              |
| discovery + wizard   |     | JSONL events + redact |
+---------------------+     +------------------------+
              |
              v
        +-----+------+
        | pr_updater |
        | version UX |
        +------------+

Shared types/schemas: crates/pr_types
```

### 2.2 Boundaries and responsibilities

- **Core agent loop (upstream):** owns the conversation loop, model I/O, and dispatching tool calls.
- **Tool dispatch (upstream + pr_runtime):** upstream constructs tool calls and executes them; pr_runtime intercepts *before* execution and *after* execution, and may modify calls.
- **Policy layer (pr_policy):** deterministic decisions for allow/deny/modify at `before_tool` and gating at `before_finalize`, plus evidence tracking updates on `after_tool`.
- **Hooks (pr_hooks):** runs configured external hooks with strict contracts; merges decisions deterministically; never routes hook subprocess actions back through tool dispatch.
- **Pipelines (pr_pipelines):** a deterministic pipeline runner (pipelines → workflows → parts) that can run preflight + verify/fix loops using the same tool dispatch path (policy floors + approvals + hooks); includes retry/loop guards and emits pipeline events.
- **Templates (pr_templates):** template discovery, picker UX integration, draft wizard, template contract validation, and conversion of template metadata to a policy overlay.
- **Config (pr_config):** read/merge config layers; ensure no clobbering; provide effective config and policy floors; handle file locations and precedence.
- **Updater (pr_updater):** non-blocking update checks, skip-version state, optional apply mechanism; never modifies `CODEX_HOME` except PrintRevolt-owned state files.
- **Audit/events (pr_audit):** stable JSONL event schema, redaction policy, rotation/retention.

### 2.3 Source of truth for state

- **Configuration:** read from upstream Codex config + PrintRevolt config layers (Mode A and/or Mode B). PrintRevolt writes only to PrintRevolt-owned files by default.
- **Pipelines:** global pipeline bundle at `CODEX_HOME/printrevolt/pipelines.json` plus optional project bundle at `<repo>/.codex/printrevolt/pipelines.json` (trusted repos only). Pipeline run state is in-memory per session/task.
- **Audit logs:** append-only JSONL log at PrintRevolt-owned path under `CODEX_HOME/printrevolt/` (default).
- **Evidence store:** in-memory per session; optional on-disk evidence cache under `CODEX_HOME/printrevolt/evidence.json` (opt-in). Evidence is always validated against a repo fingerprint and TTL.

---

## 3) Module / Crate Architecture (Rust)

### 3.1 Crate layout

Required crates (per plan):
- `crates/pr_policy`
- `crates/pr_hooks`
- `crates/pr_templates`
- `crates/pr_pipelines`

Additional strongly-recommended crates (keeps interfaces clean and diffs isolated):
- `crates/pr_types` (shared domain model + schemas + reason codes)
- `crates/pr_config` (config resolution + compatibility + no-clobber IO)
- `crates/pr_audit` (event schema + sinks + redaction + retention)
- `crates/pr_updater` (update detection/apply + state)
- `crates/pr_runtime` (composition root / upstream adapter; the only crate that imports upstream internals)

> Diff-minimization rule: upstream modifications must be limited to a small allowlist and call into `pr_runtime` only.

---

### 3.2 `crates/pr_types`

**Responsibilities**
- Canonical domain entities used across crates (SessionContext, ToolCall, decisions, events).
- Stable enums for hook events and policy reason codes.
- JSON (serde) representations for hook stdin/stdout schemas.
- Utility types for redaction-safe logging (e.g., `RedactedString`, `ContentHash`).

**Public APIs**

```rust
// crates/pr_types/src/lib.rs
pub mod session;
pub mod tool;
pub mod policy;
pub mod hooks;
pub mod pipelines;
pub mod templates;
pub mod audit;
pub mod errors;
```

Key types (selected):

```rust
// session.rs
use serde::{Serialize, Deserialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionId(pub String); // UUID string (lowercase)

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionContext {
    pub id: SessionId,
    pub repo_root: PathBuf,        // user-provided / upstream working dir
    pub repo_root_real: PathBuf,   // canonicalized
    pub cwd: PathBuf,              // canonicalized
    pub os: OsKind,
    pub shell: ShellKind,
    pub template: Option<TemplateRef>,
    pub repo_state: RepoState,     // refreshed by runtime
    pub approvals: ApprovalsState, // mirrors upstream approvals (summary only)
    pub started_at_ms: i64,        // unix epoch millis
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoState {
    pub active_branch: Option<String>,
    pub base_branch: Option<String>,
    pub head_sha: Option<String>,
    pub is_dirty: Option<bool>,
    pub fingerprint: Option<String>, // stable string, see §5.3
    pub fingerprint_ts_ms: Option<i64>,
}
```

```rust
// tool.rs
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallId(pub String); // UUID string

	#[derive(Clone, Debug, Serialize, Deserialize)]
	#[serde(tag = "kind", rename_all = "snake_case")]
	pub enum ToolCall {
	    Shell(ShellCall),
	    FsWrite(FsWriteCall),
	    FsPatch(FsPatchCall),
	    FsDelete(FsDeleteCall),
	    Git(GitCall),          // if upstream distinguishes; otherwise derived from Shell
	    Mcp(McpCall),          // plugin/MCP tool boundary
	    Other(OtherToolCall),  // unknown tool kinds; treated conservatively
	}

	#[derive(Clone, Debug, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub enum ToolInitiator {
	    Agent,
	    Pipeline,
	    User,
	}

	#[derive(Clone, Debug, Serialize, Deserialize)]
	pub struct ShellCall {
	    pub id: ToolCallId,
	    pub initiator: ToolInitiator,
	    pub argv: Option<Vec<String>>, // preferred
	    pub raw: Option<String>,       // if only string available
	    pub cwd: std::path::PathBuf,
	    pub env_redacted: Vec<(String, String)>, // allowlisted+redacted
	}

	#[derive(Clone, Debug, Serialize, Deserialize)]
	pub struct FsWriteCall {
	    pub id: ToolCallId,
	    pub initiator: ToolInitiator,
	    pub path: std::path::PathBuf,
	    pub bytes_len: u64,
	    pub sha256: Option<String>, // of content (if available), hex
	}

	#[derive(Clone, Debug, Serialize, Deserialize)]
	pub struct FsPatchCall {
	    pub id: ToolCallId,
	    pub initiator: ToolInitiator,
	    pub path: std::path::PathBuf,
	    pub patch_sha256: Option<String>,
	    pub patch_len: u64,
	}

	#[derive(Clone, Debug, Serialize, Deserialize)]
	pub struct FsDeleteCall {
	    pub id: ToolCallId,
	    pub initiator: ToolInitiator,
	    pub path: std::path::PathBuf,
	}
```

```rust
// policy.rs
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Decision {
    Allow,
    Deny(DenyInfo),
    Modify(ModifyInfo),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DenyInfo {
    pub code: ReasonCode,
    pub reason: String,           // human readable
    pub remediation: Vec<String>, // actionable steps
    pub docs: Option<String>,     // optional doc identifier (no URL requirement)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModifyInfo {
    pub code: ReasonCode,
    pub reason: String,
    pub modified_call: ToolCall,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReasonCode {
    // Policy/gates
    PrVerifyRequired,
    PrVerifyStale,
    PrVerifyExecutionDenied,
    PrProtectedBranch,
    PrBranchSchemeMismatch,
    PrDangerousCommand,
    PrPathEscape,
    PrHookFailure,
    PrHookDenied,
    PrFinalizeDenied,
    PrPolicyError,
    PrBreakGlassRequired,
    // Config/update
    PrConfigInvalid,
    PrUpdaterError,
    // ... keep extensible; stable serialization via serde(rename)
}
```

**Dependencies**
- `serde`, `serde_json`, `uuid` (or custom), `thiserror`, `time`/`chrono`.

**Test strategy**
- Pure serde round-trip tests for all public schemas.
- Golden tests for hook schemas and event envelopes (stable field names).

---

### 3.3 `crates/pr_config`

**Responsibilities**
- Locate config files using `CODEX_HOME` semantics.
- Load and merge PrintRevolt config from:
  - Mode A: `[printrevolt]` tables in upstream `config.toml`
  - Mode B: `printrevolt.toml` adjacent to upstream config (user and project)
- Implement precedence + policy clamping (floor).
- Ensure **no clobbering**:
  - default is read-only for upstream config
  - writes go to PrintRevolt-owned files (Mode B) unless explicitly enabled.

**Public APIs**

```rust
// pr_config/lib.rs
use pr_types::errors::ConfigError;
use pr_types::policy::PolicyConfig;
use pr_types::templates::TemplateOverlay;
use std::path::PathBuf;

pub struct ConfigPaths {
    pub codex_home: PathBuf,
    pub user_codex_config: PathBuf,     // ~/.codex/config.toml
    pub project_codex_config: PathBuf,  // <repo>/.codex/config.toml
    pub user_printrevolt: PathBuf,      // ~/.codex/printrevolt.toml
    pub project_printrevolt: PathBuf,   // <repo>/.codex/printrevolt.toml
}

#[derive(Clone, Debug)]
pub struct EffectiveConfig {
    pub printrevolt: PrintRevoltConfig,
    pub source_trace: ConfigTrace, // where each field came from
}

#[derive(Clone, Debug)]
pub struct ConfigTrace {
    // For operability: field->source (cli/project/user/default/template)
    pub fields: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PrintRevoltConfig {
    pub enabled: bool,
    pub enforcement: EnforcementConfig,
    pub policy: PolicyConfig,
    pub hooks: HooksConfig,
    pub templates: TemplatesConfig,
    pub audit: AuditConfig,
    pub updater: UpdaterConfig,
}

pub struct ConfigLoader;

impl ConfigLoader {
    pub fn discover_paths(repo_root: &std::path::Path) -> Result<ConfigPaths, ConfigError>;
    pub fn load_effective(
        paths: &ConfigPaths,
        cli_overrides: &CliOverrides,
        template_overlay: Option<&TemplateOverlay>,
    ) -> Result<EffectiveConfig, ConfigError>;

    // Optional: write Mode B safely (atomic, no clobber)
    pub fn write_printrevolt_toml(
        target: &std::path::Path,
        config: &PrintRevoltConfig,
    ) -> Result<(), ConfigError>;
}
```

**Dependencies**
- `toml`, `toml_edit` (Mode A round-trip), `serde`, `dirs`/`home`, `fs2` for locking.

**Test strategy**
- Precedence tests (golden).
- Mode A round-trip tests: unknown keys + comments preserved; minimal formatting churn.
- Mode B atomic write tests with concurrent writers (lock behavior).

---

### 3.4 `crates/pr_policy`

**Responsibilities**
- Deterministic policy evaluation at:
  - `before_tool`
  - `before_finalize`
- Evidence tracking from `after_tool` results:
  - verify evidence collection
  - freshness / staleness via repo fingerprint + TTL
- Policy “floor” clamping behavior is enforced by config (inputs into policy are already clamped),
  but policy re-validates invariants after hook modifications.

**Public APIs**

```rust
// pr_policy/lib.rs
use pr_types::{session::SessionContext, tool::ToolCall, tool::ToolResult};
use pr_types::policy::{Decision, PolicyState, PolicyConfig};
use pr_types::errors::PolicyError;

pub struct PolicyEngine {
    cfg: PolicyConfig,
    state: PolicyState,
}

impl PolicyEngine {
    pub fn new(cfg: PolicyConfig) -> Self;

    pub fn state(&self) -> &PolicyState;
    pub fn state_mut(&mut self) -> &mut PolicyState;

    pub fn evaluate_before_tool(
        &self,
        ctx: &SessionContext,
        call: &ToolCall,
    ) -> Result<Decision, PolicyError>;

    pub fn observe_after_tool(
        &mut self,
        ctx: &SessionContext,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Result<(), PolicyError>;

    pub fn evaluate_before_finalize(
        &self,
        ctx: &SessionContext,
    ) -> Result<Decision, PolicyError>;

    /// Re-validate a modified tool call (from hooks) against policy floors.
    pub fn clamp_modified_call(
        &self,
        ctx: &SessionContext,
        original: &ToolCall,
        modified: &ToolCall,
    ) -> Result<Decision, PolicyError>;
}
```

**Dependencies**
- `regex` (compiled and cached), `once_cell`/`lazy_static`, `serde`.

**Test strategy**
- Unit tests for each rule:
  - branch restrictions
  - verify evidence and staleness
  - dangerous command patterns (unix + windows)
  - path containment for fs tools
  - deny-by-default on policy errors for mutating tools (as configured)
- Property tests for path containment and normalization.

---

### 3.5 `crates/pr_hooks`

**Responsibilities**
- Resolve hook specs from config and repo/user sources.
- Execute hook processes with:
  - strict stdin/stdout JSON schema
  - timeouts, IO caps, deterministic ordering
  - env sanitization and redaction
  - safe defaults on error/timeouts (deny for mutating tools)
- Merge hook decisions deterministically.

**Public APIs**

	```rust
	// pr_hooks/lib.rs
	use pr_types::hooks::{HookEvent, HookRequest, HookResponse, HookSpec, HookExecResult};
	use pr_types::tool::{ToolCall, ToolResult};
	use pr_types::session::{SessionContext, TaskContext, TurnContext, TaskOutcome, TurnAbortReason, InterruptContext};
	use pr_types::errors::HookError;

pub struct HookBus {
    specs: HookSpecsByEvent,
    runner: HookRunner,
}

pub type HookSpecsByEvent = std::collections::BTreeMap<HookEvent, Vec<HookSpec>>;

	impl HookBus {
	    pub fn new(specs: HookSpecsByEvent, runner: HookRunner) -> Self;

	    pub async fn fire_on_session_start(
	        &self,
	        ctx: &SessionContext,
	    ) -> Vec<HookExecResult>;

	    /// Fired on clean shutdown (best-effort; observe-only).
	    pub async fn fire_on_session_end(
	        &self,
	        ctx: &SessionContext,
	    ) -> Vec<HookExecResult>;

	    /// Fired before starting a new upstream task (a `UserTurn`).
	    /// If denied, the task is rejected and no model/tool execution occurs.
	    pub async fn fire_before_task(
	        &self,
	        ctx: &SessionContext,
	        task: &TaskContext,
	        policy_summary: &pr_types::policy::PolicySummary,
	    ) -> Vec<HookExecResult>;

	    /// Fired when upstream signals a new turn has started (`turn_started`).
	    pub async fn fire_on_turn_start(
	        &self,
	        ctx: &SessionContext,
	        task: &TaskContext,
	        turn: &TurnContext,
	    ) -> Vec<HookExecResult>;

	    pub async fn fire_before_tool(
	        &self,
	        ctx: &SessionContext,
	        task: &TaskContext,
	        turn: &TurnContext,
	        call: &ToolCall,
	        policy_summary: &pr_types::policy::PolicySummary,
	    ) -> Vec<HookExecResult>;

	    pub async fn fire_after_tool(
	        &self,
	        ctx: &SessionContext,
	        task: &TaskContext,
	        turn: &TurnContext,
	        call: &ToolCall,
	        result: &ToolResult,
	        policy_summary: &pr_types::policy::PolicySummary,
	    ) -> Vec<HookExecResult>;

	    pub async fn fire_before_finalize(
	        &self,
	        ctx: &SessionContext,
	        task: &TaskContext,
	        turn: &TurnContext,
	        policy_summary: &pr_types::policy::PolicySummary,
	    ) -> Vec<HookExecResult>;

	    pub async fn fire_on_turn_complete(
	        &self,
	        ctx: &SessionContext,
	        task: &TaskContext,
	        turn: &TurnContext,
	    ) -> Vec<HookExecResult>;

	    pub async fn fire_on_turn_aborted(
	        &self,
	        ctx: &SessionContext,
	        task: &TaskContext,
	        turn: &TurnContext,
	        reason: TurnAbortReason,
	    ) -> Vec<HookExecResult>;

	    pub async fn fire_on_interrupt(
	        &self,
	        ctx: &SessionContext,
	        interrupt: &InterruptContext,
	    ) -> Vec<HookExecResult>;

	    pub async fn fire_after_task(
	        &self,
	        ctx: &SessionContext,
	        task: &TaskContext,
	        outcome: &TaskOutcome,
	    ) -> Vec<HookExecResult>;

	    /// Merge hook results into a single decision (deny wins).
	    pub fn merge_before_tool_results(
	        &self,
	        original: &ToolCall,
	        results: &[HookExecResult],
	    ) -> Result<pr_types::policy::Decision, HookError>;

	    /// Merge hook results into a single gate decision (deny wins; modify is invalid).
	    pub fn merge_gate_results(
	        &self,
	        results: &[HookExecResult],
	    ) -> Result<pr_types::policy::Decision, HookError>;
	}

pub struct HookRunner {
    pub timeout_ms: u64,
    pub stdin_max_bytes: usize,
    pub stdout_max_bytes: usize,
    pub stderr_max_bytes: usize,
    pub env_allowlist: Vec<String>,
}

impl HookRunner {
    pub async fn exec(&self, spec: &HookSpec, req: &HookRequest) -> HookExecResult;
}
```

**Dependencies**
- `tokio` (process + timeout), `serde_json`, `bytes`, `tracing`.

**Test strategy**
- Contract tests: valid JSON, invalid JSON, modify responses.
- Timeout tests.
- IO cap tests.
- Trust model tests (repo hooks disabled unless allowed).

---

### 3.6 `crates/pr_pipelines`

**Responsibilities**
- Define and validate the pipeline/workflow/part schema (via `pr_types::pipelines`).
- Compile pipeline definitions into a deterministic state machine (pre-parsed regex/globs, normalized argv templates).
- Execute pipelines at lifecycle gates (session/task/finalize) with:
  - retry/loop guards (max cycles, backoff, repeated-failure detection)
  - deterministic tool execution via the same dispatch path (policy floors + approvals + hooks)
  - bounded, redacted failure summaries surfaced to the agent/UI as remediation
- Emit structured pipeline audit/events (no secrets).

**Public APIs**

```rust
// pr_pipelines/lib.rs
use pr_types::pipelines::{EffectivePipeline, PipelineEventTrigger, PipelineOutcome, PipelineRunState};
use pr_types::session::{SessionContext, TaskContext, TurnContext};
use pr_types::policy::Decision;
use pr_types::errors::PipelineError;

pub struct PipelineEngine;

impl PipelineEngine {
    pub fn new(cfg: pr_types::pipelines::PipelinesConfig) -> Result<Self, PipelineError>;

    /// Loads + clamps the effective pipeline bundle (global + project + CLI overrides).
    pub fn load_effective(&self, ctx: &SessionContext) -> Result<EffectivePipeline, PipelineError>;

    /// Evaluate/execute pipeline work for a lifecycle trigger.
    ///
    /// For gate triggers (`before_task`, `before_finalize`), a returned `Decision::Deny`
    /// MUST block progress unless enforcement is in warn mode.
    pub async fn run_for_trigger(
        &mut self,
        trigger: PipelineEventTrigger,
        ctx: &SessionContext,
        task: Option<&TaskContext>,
        turn: Option<&TurnContext>,
        state: &mut PipelineRunState,
    ) -> Result<(Decision, PipelineOutcome), PipelineError>;
}
```

**Dependencies**
- `serde`, `serde_json`, `regex`, `globset`, `tracing`.

**Test strategy**
- Schema/validation tests (invalid graphs rejected; stable error codes).
- Determinism tests (same inputs => same compiled output hash).
- Loop-guard tests (max cycles; repeated failure signature).
- Integration tests that pipeline-initiated tool calls still pass through policy + hooks and emit audit events.

---

### 3.7 `crates/pr_templates`

**Responsibilities**
- Discover templates from repo and user paths.
- Parse and validate template frontmatter and required contract sections.
- Provide picker + draft wizard UX integration adapters (TUI/CLI).
- Convert template metadata defaults into `TemplateOverlay` (policy overlay).
- Provide heuristic classifier; optional model-based classifier toggle.

**Public APIs**

```rust
// pr_templates/lib.rs
use pr_types::templates::{Template, TemplateId, TemplateSource, TemplateOverlay};
use pr_types::errors::TemplateError;

pub struct TemplateStore;

impl TemplateStore {
    pub fn discover(repo_root: &std::path::Path, codex_home: &std::path::Path)
        -> Result<Vec<Template>, TemplateError>;

    pub fn resolve_by_id(templates: &[Template], id: &TemplateId)
        -> Option<Template>;

    pub fn validate(template: &Template) -> Result<(), TemplateError>;
}

pub struct TemplatePicker;

pub struct PickerResult {
    pub selected: Template,
    pub overlay: TemplateOverlay, // derived from template defaults
}

/// UI adapter is injected to keep core logic testable.
pub trait TemplateUi {
    fn pick_template(&mut self, templates: &[Template], suggested: Option<&TemplateId>) -> Result<TemplateId, TemplateError>;
    fn run_draft_wizard(&mut self, prompt: &str, context: &DraftContext) -> Result<DraftWizardResult, TemplateError>;
}

pub struct DraftContext {
    pub repo_root: std::path::PathBuf,
    pub bounded_context: Option<String>, // collected text (heuristic mode)
}

pub enum DraftWizardResult {
    ContinueWithoutSaving { selected: Template },
    SaveAndContinue { selected: Template, save_to: TemplateSource },
    CancelReturnToPrompt,
    DiscardPromptAndExit, // requires confirmation in UI layer
}

pub struct Classifier;
impl Classifier {
    pub fn heuristic_suggest(templates: &[Template], prompt: &str) -> Option<(TemplateId, f32 /*confidence*/)>;

    // Optional: model-based classification interface (implemented in pr_runtime to call upstream model).
    pub fn model_suggest_placeholder() {}
}
```

**Dependencies**
- `serde_yaml` (frontmatter), `pulldown-cmark` (optional), `globwalk`, `ignore`, `regex`.

**Test strategy**
- Template discovery tests across repo/user paths.
- Schema validation tests (required keys/sections).
- Golden tests for generated draft templates.
- Heuristic classifier tests.

---

### 3.8 `crates/pr_audit`

**Responsibilities**
- Structured JSONL events with stable schema and redaction.
- Sinks:
  - file JSONL (default)
  - stdout (optional)
  - SQLite (optional feature)
- Rotation/retention:
  - size-based rotation and TTL pruning.

**Public APIs**

```rust
// pr_audit/lib.rs
use pr_types::audit::{AuditEvent, AuditSinkConfig};
use pr_types::errors::AuditError;

pub struct AuditLogger {
    sink: Box<dyn AuditSink + Send + Sync>,
}

impl AuditLogger {
    pub fn new(cfg: &AuditSinkConfig) -> Result<Self, AuditError>;
    pub fn emit(&self, event: &AuditEvent) -> Result<(), AuditError>;
}

pub trait AuditSink {
    fn emit(&self, event: &AuditEvent) -> Result<(), AuditError>;
}
```

---

### 3.9 `crates/pr_updater`

**Responsibilities**
- Update check (non-blocking) and prompt UX.
- Skip-version persistence.
- Optional update apply:
  - npm global update
  - binary swap with signature/checksum verification.

**Public APIs**

```rust
// pr_updater/lib.rs
use pr_types::errors::UpdaterError;
use pr_types::audit::AuditEvent;

pub struct Updater {
    cfg: pr_types::updater::UpdaterConfig,
    state: UpdaterState,
}

impl Updater {
    pub fn load(cfg: pr_types::updater::UpdaterConfig, codex_home: &std::path::Path) -> Result<Self, UpdaterError>;
    pub async fn maybe_check_for_updates(&mut self) -> Result<Option<UpdateOffer>, UpdaterError>;
    pub fn record_user_choice(&mut self, choice: UpdateChoice) -> Result<(), UpdaterError>;
    pub async fn apply_update(&mut self, offer: &UpdateOffer) -> Result<(), UpdaterError>; // optional / gated
    pub fn audit_events(&self) -> Vec<AuditEvent>;
}
```

---

### 3.10 `crates/pr_runtime`

**Responsibilities**
- The only PrintRevolt crate that imports upstream Codex internals.
- Wires together:
  - config loading
  - template picker (before task start / `UserTurn`)
  - policy + hooks chaining
  - audit logging
  - updater checks
- Exposes a minimal interface for upstream call-sites.

**Public APIs (the only interface upstream uses)**

```rust
// pr_runtime/lib.rs
use pr_types::{
    session::{SessionContext, TaskContext, TurnContext, TaskOutcome, TurnAbortReason, InterruptContext},
    tool::{ToolCall, ToolResult},
    policy::Decision,
};
use pr_types::errors::RuntimeError;

pub struct PrintRevoltRuntime { /* contains config/policy/hooks/templates/audit/updater */ }

pub struct RuntimeInit {
    pub repo_root: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
    pub codex_home: std::path::PathBuf,
    pub os: pr_types::session::OsKind,
    pub shell: pr_types::session::ShellKind,
    pub cli_overrides: pr_config::CliOverrides,
}

impl PrintRevoltRuntime {
    pub async fn init(init: RuntimeInit) -> Result<Self, RuntimeError>;

    /// Called after user submits a prompt for a new task but before the `UserTurn` is dispatched.
    pub async fn on_prompt_submitted(
        &mut self,
        prompt: &str,
        ui: &mut dyn pr_templates::TemplateUi,
    ) -> Result<String /*prompt_with_template_prefix*/, RuntimeError>;

	    /// Fired after upstream session configuration is applied (`SessionConfigured`), and again on reconfigure.
	    pub async fn on_session_start(&mut self, ctx: &SessionContext) -> Result<(), RuntimeError>;

	    /// Fired on clean shutdown (best-effort). This MUST NOT be relied upon for critical cleanup.
	    pub async fn on_session_end(&mut self, ctx: &SessionContext) -> Result<(), RuntimeError>;

    /// Fired before starting a new task (`UserTurn`). Deny blocks the task from starting.
    pub async fn before_task(
        &mut self,
        ctx: &SessionContext,
        task: &TaskContext,
        prompt_with_template_prefix: &str,
    ) -> Result<Decision, RuntimeError>;

    pub async fn on_turn_start(
        &mut self,
        ctx: &SessionContext,
        task: &TaskContext,
        turn: &TurnContext,
    ) -> Result<(), RuntimeError>;

    pub async fn before_tool(
        &mut self,
        ctx: &SessionContext,
        task: &TaskContext,
        turn: &TurnContext,
        call: &ToolCall,
    ) -> Result<Decision, RuntimeError>;

    pub async fn after_tool(
        &mut self,
        ctx: &SessionContext,
        task: &TaskContext,
        turn: &TurnContext,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Result<(), RuntimeError>;

    /// Fired when upstream is about to complete the current turn/task.
    /// Deny prevents completion and forces the agent to continue with remediation.
    pub async fn before_finalize(
        &mut self,
        ctx: &SessionContext,
        task: &TaskContext,
        turn: &TurnContext,
    ) -> Result<Decision, RuntimeError>;

    pub async fn on_turn_complete(
        &mut self,
        ctx: &SessionContext,
        task: &TaskContext,
        turn: &TurnContext,
    ) -> Result<(), RuntimeError>;

    pub async fn on_turn_aborted(
        &mut self,
        ctx: &SessionContext,
        task: &TaskContext,
        turn: &TurnContext,
        reason: TurnAbortReason,
    ) -> Result<(), RuntimeError>;

    pub async fn on_interrupt(
        &mut self,
        ctx: &SessionContext,
        interrupt: &InterruptContext,
    ) -> Result<(), RuntimeError>;

    pub async fn after_task(
        &mut self,
        ctx: &SessionContext,
        task: &TaskContext,
        outcome: &TaskOutcome,
    ) -> Result<(), RuntimeError>;
}
```

---

### 3.11 Upstream touchpoints to modify

Because upstream layout may evolve, the contract is defined by responsibility. The fork must keep modifications bounded and documented in `docs/EXTENSION_POINTS.md`.

Required upstream integration points:

1. **Session initialization**
   - Create `PrintRevoltRuntime::init(...)` when agent mode session begins.
   - Create an initial `SessionContext` with canonicalized repo root/cwd and OS/shell info.
	   - After upstream applies `ConfigureSession` (and emits/records `SessionConfigured`), call `runtime.on_session_start(ctx)`.
	     - If upstream supports reconfigure mid-session, call `runtime.on_session_start(ctx)` again after reconfigure is applied (after any abort/replacement side effects are handled).
	   - On clean shutdown, call `runtime.on_session_end(ctx)` as the last best-effort hook point (may not fire on crash/forced kill).

2. **Prompt submit path (agent mode / TUI)**
   - After user submits prompt for a new task but before dispatching `UserTurn`, call `runtime.on_prompt_submitted(prompt, ui)`.
   - Replace prompt with returned `prompt_with_template_prefix`.
   - Create a `TaskContext` (stable `task_id`, prompt preview, timestamps) and call `runtime.before_task(ctx, task, prompt_with_template_prefix)`.
     - If `Deny`: do not dispatch `UserTurn`; surface remediation; emit audit; call `runtime.after_task(..., outcome=DeniedByPolicyOrHook)`.

3. **Turn lifecycle observation**
   - When upstream emits `turn_started`, create/refresh a `TurnContext` and call `runtime.on_turn_start(ctx, task, turn)`.
   - When upstream emits `turn_aborted` (reason: `interrupted` | `replaced` | `review_ended`), call `runtime.on_turn_aborted(ctx, task, turn, reason)`.

4. **Tool dispatch interception**
   - Immediately before executing any tool call, call `runtime.before_tool(ctx, task, turn, call)`.
     - If `Deny`: do not execute tool; surface reason; record audit.
     - If `Modify`: execute modified call (and surface in approvals UI if upstream has it).
   - Immediately after tool call completes, call `runtime.after_tool(ctx, task, turn, call, result)`.

5. **Finalize gate**
   - Before upstream emits `turn_complete` (i.e., when the agent indicates it is done for this turn/task), call `runtime.before_finalize(ctx, task, turn)`.
     - If `Deny`: do not emit `turn_complete`; surface remediation and continue the agent loop.
   - When upstream emits `turn_complete`, call `runtime.on_turn_complete(ctx, task, turn)`.

6. **Task end**
   - When the task completes or aborts, call `runtime.after_task(ctx, task, outcome)`.

7. **Interrupt handling**
   - When upstream receives `Op::Interrupt`, call `runtime.on_interrupt(ctx, interrupt)`.
   - If the interrupt causes a turn/task abort, also call `runtime.on_turn_aborted(...)` and `runtime.after_task(...)`.

Additionally, CI must enforce a “diff budget” allowlist for upstream files touched.

---

## 4) Domain Model (LLD-level entities)

This section defines the entities and invariants used across the system. All structs are canonical in `pr_types` and serialized consistently for hooks and audit logs.

### 4.1 SessionContext

Fields (see `pr_types::session::SessionContext` above) plus invariants:

**Invariants**
- `repo_root_real` and `cwd` MUST be canonical absolute paths.
- `cwd` MUST be within `repo_root_real` at session start; otherwise:
  - if `cwd` cannot be canonicalized: deny session start (requires user remediation).
  - if `cwd` outside repo root: set `cwd = repo_root_real` and log `SessionCwdReset` event.
- `template` contains the selected template reference (source + id) and is immutable after session start (except when user restarts session).

### 4.1.1 Task / Turn / Interrupt context

Codex CLI’s protocol distinguishes **sessions**, **tasks** (started by a `UserTurn`), and **turns** within a task (with `turn_started` / `turn_complete` / `turn_aborted`). PrintRevolt mirrors this for correlation, hooks, and audit.

> Note: “task queue” concepts such as priority, dependencies, and scheduling live outside codex-pr (e.g., in PrintRevolt Command Center). codex-pr sees one active task at a time and treats `TaskContext` as an opaque correlation envelope for hooks/pipelines/audit.
>
> Command Center maintains the task queue table (priority/status/dependencies). codex-pr does not maintain a task DB. When supervised, Command Center MAY pass task metadata for correlation only (e.g., `cc_task_id`, `priority`) via environment/session overlay; codex-pr may include it in HookRequest/audit as optional fields but MUST NOT use it to weaken safety or skip gates.

```rust
// pr_types/session.rs
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TaskId(pub String); // stable per `UserTurn` (opaque string)

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TaskContext {
    pub id: TaskId,
    pub user_turn_id: Option<String>,   // upstream id if available
    pub prompt_preview: Option<String>, // redacted + truncated (default ≤ 256 chars)
    pub started_at_ms: i64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnId(pub String); // stable per upstream turn (opaque string)

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnContext {
    pub id: TurnId,
    pub index: u32, // 1-based within task
    pub started_at_ms: i64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAbortReason { Interrupted, Replaced, ReviewEnded }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct InterruptContext {
    pub id: String,            // upstream interrupt id if available; else generated
    pub received_at_ms: i64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskOutcome {
    Completed,
    Aborted { reason: TurnAbortReason },
    DeniedByPolicyOrHook,
    Errored { code: String },
}
```

**Invariants**
- `prompt_preview` MUST be redacted and bounded; it MUST NOT contain repo file contents.
- `task.id` and `turn.id` MUST be stable across all hook/audit emissions within their scope.
- `turn.index` increments monotonically within a task; it resets for each new task.

### 4.2 ToolCall

Tool calls are normalized into one of the `ToolCall` enum variants. Every tool call has:
- `id`: UUID string
- `kind`: variant discriminant
- `cwd`: canonical path (for shell calls)
- Tool-specific payload

**Invariants**
- All filesystem tool calls (`FsWrite`, `FsPatch`, `FsDelete`) MUST reference a path that can be canonicalized *at decision time* and must be within `repo_root_real`.
- Shell tool calls MUST provide at least one of:
  - `argv` (preferred), or
  - `raw` command string.

### 4.3 ToolResult

```rust
// pr_types/tool.rs
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub tool_call_id: ToolCallId,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub stdout_ref: OutputRef,
    pub stderr_ref: OutputRef,
    pub timed_out: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OutputRef {
    pub trunc: bool,
    pub bytes_len: u64,
    pub sha256: Option<String>,
    pub preview: Option<String>, // redacted + truncated (configurable)
}
```

**Invariants**
- `preview` MUST be redacted and bounded (default ≤ 4KB).
- Full stdout/stderr MUST NOT be persisted by default (audit logs record hashes + previews only).

### 4.4 PolicyState

```rust
// pr_types/policy.rs
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PolicyState {
    pub verify: Option<VerifyEvidence>,
    pub break_glass: BreakGlassState,
    pub allow_deny_history: Vec<PolicyDecisionRecord>, // bounded ring buffer
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct VerifyEvidence {
    pub cmd_normalized: String,
    pub exit_code: i32,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub repo_fingerprint: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BreakGlassState {
    pub one_off: Option<BreakGlassGrant>,
    pub session_override: Option<BreakGlassGrant>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BreakGlassGrant {
    pub granted_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub reason: String,
    pub scope: BreakGlassScope,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum BreakGlassScope { OneOff, Session }
```

**Invariants**
- `allow_deny_history` is bounded to `policy.history_max` (default 200) to prevent memory growth.
- `one_off` grants are consumed upon first use (whether allowed or denied by another floor).

### 4.5 HookSpec + HookResult

```rust
// pr_types/hooks.rs
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    OnSessionStart,
    OnSessionEnd,
    BeforeTask,
    OnTurnStart,
    BeforeTool,
    AfterTool,
    BeforeFinalize,
    OnTurnComplete,
    OnTurnAborted,
    OnInterrupt,
    AfterTask,
    OnItemStarted,
    OnItemCompleted,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HookSpec {
    pub id: String, // stable: e.g., "user:before_tool:~/.../hook.sh"
    pub event: HookEvent,
    pub exec_path: String, // raw config string; expanded by runtime
    pub timeout_ms: u64,
    pub trusted: bool,
    pub source: HookSource, // user or repo
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum HookSource { User, Repo }

	#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
	pub struct HookExecResult {
	    pub spec_id: String,
	    pub ok: bool,
	    pub duration_ms: u64,
	    pub exit_code: Option<i32>,
	    pub response: Option<HookResponse>,  // only when ok and parse success
	    pub error: Option<String>,           // redacted
	}
	```

### 4.5.1 Pipelines (Pipeline / Workflow / Part)

Pipelines are stored as bundles (global + project) and compiled into deterministic executable graphs. The CLI supports only **typed parts**; a “run command” part always uses canonical argv (no raw shell by default).

```rust
// pr_types/pipelines.rs
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PipelineId(pub String);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WorkflowId(pub String);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PartId(pub String);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PipelineBundle {
    pub schema_version: String, // "1"
    pub pipelines: Vec<Pipeline>,
    pub workflows: Vec<Workflow>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Pipeline {
    pub id: PipelineId,
    pub name: String,
    // Optional enable flag for “library” pipelines stored in a bundle but not active by default.
    // Disabled pipelines are ignored for lifecycle triggers and cannot be selected unless explicitly enabled.
    pub enabled: Option<bool>, // default true
    pub triggers: Vec<PipelineTrigger>,
    pub entry_workflow: WorkflowId,
    pub loop_guards: LoopGuards,
    pub capabilities: Vec<PipelineCapability>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineTrigger {
    OnSessionStart,
    BeforeTask,
    BeforeFinalize,
    OnSessionEnd,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Workflow {
    pub id: WorkflowId,
    pub name: String,
    pub parts: Vec<WorkflowPart>,
    // Optional workflow-level teardown that runs on any exit (success/failure/block/interrupt),
    // best-effort and bounded. See §6.4.6.
    pub finally_workflow: Option<WorkflowId>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WorkflowPart {
    pub id: PartId,
    pub label: Option<String>,
    pub kind: PartKind,
    pub when: Option<Predicate>,
    // Optional explicit transitions. If omitted, compilation sets:
    // - `on_success = Next::NextPart` (unless this is the last part)
    // - `on_failure = Next::Block{...}` for gate triggers (before_task/before_finalize),
    //   or `Next::Complete` for observe-only triggers.
    pub on_success: Option<Next>,
    pub on_failure: Option<Next>,
    // Optional per-part retry policy (bounded by pipeline-level loop guards).
    pub retry: Option<RetryPolicy>,
    // Optional per-part deferred cleanup actions. If the part executes (not skipped),
    // these are pushed onto a cleanup stack and run on workflow/pipeline exit (LIFO).
    // Each cleanup action is executed via the normal tool boundary (policy + hooks + approvals).
    pub defer: Option<Vec<PartKind>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FactKey(pub String);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FactValue {
    String(String),
    Int(i64),
    Bool(bool),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,          // default 1; max clamped by config
    pub backoff_ms: Option<u64>,    // default none; bounded
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Next {
    NextPart,
    GotoWorkflow { workflow_id: WorkflowId },
    GotoPart { workflow_id: WorkflowId, part_id: PartId },
    Complete,
    Block { code: String, reason: String, remediation: Vec<String> },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PartKind {
    EmitNote { message: String },
    RequireApproval { scope: String, prompt: String },
    ComputeChangedFiles { base_ref: String, include_uncommitted: bool },
    RunCommand {
        cwd: String,
        // Exactly one of `argv` or `command_id` MUST be present.
        argv: Option<Vec<String>>,
        command_id: Option<String>,
        category: String,
        timeout_ms: Option<u64>,
        // Optional child-process posture. Note: this is best-effort and platform-dependent.
        child_process_policy: Option<ChildProcessPolicy>,
    },
    AssertLastResult { exit_code: Option<i32>, stderr_regex: Option<String> },
    EnsureWorktree { worktree_root: String, naming: String, require_approval: bool },
    // Ensure work occurs on a non-protected branch, typically per-task.
    // Branch creation/switching is approval-gated and clamped by protected branch rules.
    EnsureBranch {
        base_branch: Option<String>, // default `${var.BASE_BRANCH}`
        naming: String,              // e.g. `prcc/${session_id}`
        require_approval: bool,
    },
    CheckDocker { mode: String, probe_argv: Vec<String> },
    SuggestAgentFix { template: String, require_user_confirm: bool },
    // Derive a small FactValue from bounded tool output previews.
    ExtractValue {
        source: ExtractSource,         // stdout|stderr|combined
        parser: ExtractParser,         // json_path|regex
        output_key: FactKey,
        // Optional normalization (e.g., lowercasing) before parsing.
        normalize: Option<Vec<String>>,
    },
    // Gate/branch based on derived facts without rerunning tools.
    AssertFact { key: FactKey, equals: Option<FactValue>, matches_regex: Option<String> },
    // Optional advanced part: query the agent (Codex) for a structured decision and route accordingly.
    // This is still bounded and deterministic: the agent must choose from an allowlisted set of next steps.
    RequestAgentDecision {
        prompt_template: String,
        allowed_next_workflows: Vec<WorkflowId>,
        require_user_confirm: bool,
    },
    LoopGuard { max_cycles: u32, repeat_failure_signature_limit: u32 },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildProcessPolicy {
    // Default: no per-child gating beyond upstream sandbox/OS containment.
    Inherit,
    // Record child execs and fail/branch if disallowed (platform-specific).
    AuditAllowlist,
    // Attempt to prevent non-allowlisted execs (platform-specific; may be unsupported).
    EnforceAllowlist,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractSource { Stdout, Stderr, Combined }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtractParser {
    JsonPath { path: String },
    Regex { pattern: String, group: Option<u32> },
}
```

**Invariants**
- All strings that may contain secrets (tool output, prompts) MUST be redacted/bounded before persistence or emission.
- `RunCommand.argv` MUST be canonical argv (no shell string) unless explicitly break-glass enabled.
- Pipeline capabilities are clamped by policy floors.

### 4.6 Template (saved + draft)

```rust
// pr_types/templates.rs
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TemplateId(pub String); // stable id derived from source+path

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TemplateRef {
    pub id: TemplateId,
    pub name: String,
    pub source: TemplateSource,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum TemplateSource { Repo, User, Draft }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Template {
    pub id: TemplateId,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub source: TemplateSource,
    pub path: Option<std::path::PathBuf>,
    pub defaults: TemplateOverlay,
    pub body_markdown: String,
    pub contract: TemplateContract, // parsed/validated sections
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TemplateContract {
    pub role_objective: String,
    pub procedure: String,
    pub outputs: String,
    pub policy_defaults: String,
    pub tooling_scope: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TemplateOverlay {
    pub policy: pr_types::policy::PolicyOverlay, // see §8 and §7.4
}
```

**Invariants**
- Saved templates MUST validate contract sections; invalid templates are ignored (with audit warning).
- Draft templates MUST include metadata defaults; they are treated as overlays only and never as floors.

### 4.7 Config layers and effective config

Config sources (highest precedence first):
1. CLI overrides (`--printrevolt.*` and/or upstream `-c printrevolt.*=...`)
2. Session overlay file (optional; referenced by `PRINTREVOLT_SESSION_CONFIG`)
3. Project PrintRevolt config (Mode B file or Mode A table)
4. User PrintRevolt config (Mode B file or Mode A table)
5. PrintRevolt compiled defaults
6. Template overlay (applied after prompt submit, but clamped to floors; see §7.4 and §8)

**Session overlay file (`PRINTREVOLT_SESSION_CONFIG`)**
- Purpose: allow a supervisor UI (e.g., PrintRevolt Command Center) to pass per-session/per-task selections without rewriting persistent user/project config.
- Format: Mode B TOML with the same schema as `printrevolt.toml` (subset allowed).
- Loading:
  - if the env var is set, codex-pr loads the file (best-effort) and applies it at layer #2.
  - parse/validation failures are audited; for safety-critical fields, failures default to safe behavior (deny for mutating tools).
- Floors:
  - the overlay cannot weaken floor-monotonic fields; it is clamped like any other layer.

> Note: Some fields are “floor-monotonic”: repo-level config and templates cannot weaken them.

---

## 5) Policy Engine (“Guaranteed enforcement”)

### 5.1 Threat model + bypass analysis

**Guarantee boundary**
PrintRevolt can guarantee enforcement only for operations that transit the Codex CLI tool boundary within this process:
- agent-triggered shell execution
- agent-triggered filesystem mutation tools
- agent-triggered plugin/MCP tool dispatch
- agent turn/task completion (`turn_complete`) in agent mode

It cannot enforce:
- user manual commands outside the CLI
- server-side branch protection rules (recommended, not guaranteed)
- other tooling not invoked via Codex tool dispatch.

**Bypass vectors and mitigations**

1. **Malicious prompt injection**
   - Vector: prompt tries to coerce model into running destructive commands or exfiltrating secrets.
   - Mitigation: deterministic denylist patterns; path containment; finalize gate requires verify; audit logs with reason codes.

2. **Destructive commands**
   - Vector: `rm -rf /`, `mkfs`, `dd`, Windows `Format-Volume`, PowerShell `Remove-Item -Recurse -Force C:\`.
   - Mitigation: cross-platform destructive denylist; deny wins even if upstream approvals might allow.

3. **Bypass via alternate tools / MCP tools**
   - Vector: model uses plugin tool to run commands or write files outside normal shell/fs tools.
   - Mitigation: treat all plugin/MCP dispatch as `ToolCall::Mcp` and apply policy/hook interception uniformly; unknown tools are conservatively classified as mutating unless proven read-only.

4. **Config poisoning**
   - Vector: repo commits `.codex/config.toml` or `.codex/printrevolt.toml` that disables enforcement or weakens floors.
  - Mitigation: “floor-monotonic” merge rules: user-level floors cannot be weakened by repo/project layers or templates. Repo-provided hooks are disabled by default and require explicit trust allowlist. Provide `codex-pr doctor` output and audit warnings if project config attempts to weaken locked fields.

5. **Update spoofing / supply chain**
   - Vector: attacker provides malicious update binary or npm package.
   - Mitigation: pin update sources; verify signatures/checksums (binary mode) and registry metadata (npm mode); store update state separately; never execute repo-provided postinstall hooks.

6. **Hook spoofing**
   - Vector: repo supplies a hook script that exfiltrates secrets or weakens enforcement.
  - Mitigation: repo-provided hooks disabled by default; trust allowlist by repo root + optional git remote check; env sanitization; hooks cannot weaken policy floors because policy clamps after modify.

### 5.2 Enforcement points

#### 5.2.1 `before_tool` decision point

**Signature**
```rust
pub fn evaluate_before_tool(&self, ctx: &SessionContext, call: &ToolCall)
    -> Result<Decision, PolicyError>;
```

**Semantics**
- Returns:
  - `Allow`: tool may execute (still subject to upstream approvals/sandbox).
  - `Deny`: tool MUST NOT execute; UI must show `reason`, `code`, `remediation`.
  - `Modify`: tool MAY execute only the returned `modified_call` after policy clamp re-validation.

**Rule evaluation order (deterministic)**
1. Break-glass evaluation:
   - If break-glass grant is active and scope matches, proceed to rule evaluation but mark decision as “override used” for audit. Break-glass does **not** bypass “hard floors” such as path containment or destructive command denylist unless explicitly configured (default: does not bypass destructive denylist).
2. Tool classification:
   - Determine whether tool is `read_only`, `mutating`, or `high_risk`.
3. Path containment checks (fs tools):
   - Fail-closed (deny) if canonicalization fails or escapes repo root.
4. Dangerous command checks (shell tools):
   - Check both raw string (if present) and argv (if present) against denylist matchers.
5. Branch rules:
   - If tool is mutating and current branch is protected or missing, deny.
   - If branch scheme required and branch does not match, deny.
6. Verification gating rules:
   - If tool is `git push` (detected via argv/raw normalization), require fresh verify evidence.
   - Optional “verification execution posture”:
     - If `policy.verify_execution="pipeline_only"`, deny agent-initiated verification commands (e.g., `npm test`) unless the command is being invoked by a pipeline run at an allowed lifecycle gate (e.g., `before_finalize`).
     - If `policy.verify_execution="require_approval"`, require explicit user approval for verification commands when invoked by the agent outside pipeline automation.
7. If any evaluation step errors:
   - Mutating tool: deny by default (`PrPolicyError`) unless configured to allow in warn mode.
   - Read-only tool: allow with warning event (configurable).

#### 5.2.2 `before_finalize` decision point

**Signature**
```rust
pub fn evaluate_before_finalize(&self, ctx: &SessionContext) -> Result<Decision, PolicyError>;
```

**Semantics**
- If `policy.require_verify_on_finalize=true` then finalize is denied unless:
  - verify evidence exists AND is fresh (TTL + repo fingerprint unchanged), OR
  - break-glass approval is granted (as configured).

Finalize denials must include:
- `code = PrFinalizeDenied` (or `PrVerifyRequired` / `PrVerifyStale`)
- actionable remediation steps (run verify cmd, create branch, etc.)
- if `policy.verify_execution="pipeline_only"` and no fresh evidence exists, remediation SHOULD explicitly mention:
  - “Verification is configured as pipeline-only; add/enable verify pipeline coverage or change `verify_execution`.”

#### 5.2.3 “Break-glass” override behavior

Break-glass is not implicit. It is only granted via explicit user action:

- One-off approval:
  - UI prompts: “Type `BREAK_GLASS` to allow this action once.”
  - Stored as `break_glass.one_off` with optional short expiry (default 5 minutes).
  - Consumed on next tool call decision.
- Session override:
  - Requires CLI flag `--break-glass` (or config) and typed confirmation at session start.
  - Time-boxed (default 15 minutes) and audited.

**Logging**
Every break-glass action emits:
- `BreakGlassGranted`
- `BreakGlassUsed` (tool/finalize) with scope and reason
- `BreakGlassExpired`

### 5.3 Evidence tracking

#### 5.3.1 How “verify passed” is determined

Verify evidence is recorded only when:
- A tool call matches the configured `verify_cmd` pattern (normalized), AND
- The tool result `exit_code == 0`.

**Avoiding duplicate work**
- If the agent runs the configured verify command early (before the pipeline would have run it), this still records verify evidence.
- A `before_finalize` verification pipeline SHOULD:
  - check for fresh verify evidence first (or rely on finalize gating) and
  - skip rerunning verification when evidence is already fresh.

**Normalization**
- For `argv`:
  - Join argv with `\u{0}` separator to avoid quoting ambiguity for matching.
- For `raw`:
  - Use a conservative normalization:
    - trim whitespace
    - collapse internal whitespace to single spaces
    - lowercase on Windows (PowerShell/cmd) for comparison
- Matching strategy:
  - `verify_cmd` config compiles into one of:
    - exact argv match (preferred)
    - regex match (explicitly configured)
    - prefix match (explicitly configured, e.g., `./devctl verify` + args)
  - Default is **exact match** on argv if available; else exact match on normalized raw.

#### 5.3.2 Repo fingerprint (staleness model)

At the time verify completes successfully, runtime must update `ctx.repo_state.fingerprint`.

Fingerprint format (string):
```
"git:head=<sha>;dirty=<0|1>"
```

- `sha`: from `git rev-parse HEAD`
- `dirty`: derived from `git status --porcelain` (non-empty -> 1)

**Freshness window**
- Evidence is fresh when:
  - `now_ms - verify.finished_at_ms <= verify_ttl_minutes * 60_000`
  - AND current `ctx.repo_state.fingerprint == evidence.repo_fingerprint`

If fingerprint cannot be computed:
- Treat verify evidence as not fresh (deny finalize and push), unless config allows break-glass.

#### 5.3.3 Persistence strategy

Default:
- Evidence is in-memory per session only.

Optional (`printrevolt.policy.persist_evidence=true`):
- Persist last successful verify evidence to:
  - `CODEX_HOME/printrevolt/evidence.json` (0600)
- Only persisted if repo root matches and fingerprint matches.
- Used only to improve UX across session restarts, never as a bypass:
  - evidence is still validated for TTL and fingerprint at decision time.

### 5.4 Performance budget

- Policy evaluation must not run external processes.
  - RepoState refresh (git calls) occurs in runtime on a throttled cadence (default: at most once per 2 seconds and only when needed).
- Dangerous command matchers:
  - regex compiled once and cached (static `OnceLock<Vec<Regex>>`)
  - prefer exact token matching for common cases (`rm`, `del`, `format`, `mkfs`) before regex fallback.
- Max added latency per tool call from policy:
  - median ≤ 2ms, p95 ≤ 5ms.

---

## 6) Pipelines (Automation) + Hook Bus + Script Hooks

### 6.1 Lifecycle and ordering

Hook events:
- `on_session_start`
- `on_session_end`
- `before_task`
- `on_turn_start`
- `before_tool`
- `after_tool`
- `before_finalize`
- `on_turn_complete`
- `on_turn_aborted`
- `on_interrupt`
- `after_task`
- optional high-volume: `on_item_started`, `on_item_completed`

**Upstream lifecycle mapping (Codex CLI protocol v1)**
- Session:
  - After `ConfigureSession` is applied (and `SessionConfigured` is emitted), fire `on_session_start`.
  - On session reconfigure, upstream aborts any running execution; then fire `on_session_start` again with the new effective context.
  - On clean shutdown, fire `on_session_end` (best-effort; may not fire on crashes/kill -9).
- Task:
  - A task starts on `UserTurn`. Fire `before_task` after prompt/template preparation but before dispatching `UserTurn`.
  - A task ends when upstream reports completion or an abort. Fire `after_task` with the final outcome.
- Turn:
  - Fire `on_turn_start` on `turn_started`.
  - Fire `before_finalize` immediately before upstream would emit `turn_complete`.
  - Fire `on_turn_complete` after `turn_complete` is emitted.
  - Fire `on_turn_aborted` on `turn_aborted` (reason: `interrupted` | `replaced` | `review_ended`).
- Interrupt:
  - Fire `on_interrupt` when `Op::Interrupt` is received (before any resulting `turn_aborted` / task abort propagation).
- Item (optional; high volume):
  - Fire `on_item_started` / `on_item_completed` on upstream `item_started` / `item_completed`.

**Ordering guarantees**
- Hooks for a given event run sequentially in deterministic order:
  1. user hooks (in configured order)
  2. repo hooks (in configured order) — only if trusted
- For **gate/decision** events (`on_session_start`, `before_task`, `before_tool`, `before_finalize`):
  - `deny` wins globally (across all hooks and built-in policy).
  - For `before_tool`, the overall ordering is:
  1. built-in policy pre-check (fast deny for obvious violations)
  2. hook chain (may deny or modify)
  3. built-in policy clamp re-validation (deny or accept modified call)
- For **observe-only** events (`after_tool`, `on_turn_start`, `on_turn_complete`, `on_turn_aborted`, `after_task`, `on_interrupt`, `on_item_*`):
  - hook responses do not affect control flow (decisions are ignored)
  - failures are audited; they do not block progress by default.

**Reentrancy**
- Hook execution is non-reentrant per tool call.
- Hooks cannot mutate `SessionContext`/`TaskContext`/`TurnContext`. The only supported mutation is `modify` on `before_tool`, applied only to the tool call (not to config/policy floors).
- Hooks do not run in parallel unless explicitly enabled in config (default: serial for determinism).

### 6.2 Hook execution engine

**Timeouts**
- Default `timeout_ms = 2000` per hook.
- A hook exceeding timeout is killed:
  - Unix: SIGKILL after grace (100ms)
  - Windows: `TerminateProcess`.

**Retries**
- Default: no retries (determinism).
- Optional retry policy:
  - only for `on_session_start`
  - max 1 retry
  - must be explicitly configured.

**Cancellation**
- If user interrupts the session or tool call is canceled, hook process is terminated.

**IO caps**
- stdin max bytes: 256KB (hard)
- stdout max bytes: 256KB (hard)
- stderr max bytes: 256KB (hard)
If exceeded, treat as hook failure.

**Env var sanitization**
- Default: allowlist environment keys and redact values:
  - `PATH`, `HOME`, `USERPROFILE`, `TEMP`, `TMP`, `SystemRoot`, `COMSPEC`
  - `CODEX_HOME`
  - `PRINTREVOLT_*`
- Explicitly strip common secret env vars by default:
  - `OPENAI_API_KEY`, `GITHUB_TOKEN`, `AWS_*`, `AZURE_*`, `GOOGLE_*`, etc.
- Hooks receive `PRINTREVOLT_HOOK_EVENT`, `PRINTREVOLT_SESSION_ID`, and `PRINTREVOLT_REPO_ROOT`.
- When available, runtime also sets: `PRINTREVOLT_TASK_ID`, `PRINTREVOLT_TURN_ID`, `PRINTREVOLT_TURN_INDEX`, `PRINTREVOLT_USER_TURN_ID`, `PRINTREVOLT_INTERRUPT_ID`.

**Network restrictions**
- No portable cross-platform “no-network” sandbox is claimed.
- Best-effort hygiene:
  - clear proxy env vars (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`)
  - optionally set `GIT_ASKPASS` to disable credential prompts.
Limitations must be documented.

**Deterministic behavior in degraded conditions**
- If hook config is invalid or hook fails:
  - for gate events (`on_session_start`, `before_task`, `before_finalize`): deny with `PrHookFailure` (unless in warn mode; see §8 defaults)
  - for `before_tool`:
    - mutating tools: deny with `PrHookFailure` (unless in warn mode; see §8 defaults)
    - read-only tools: allow but emit `HookFailed` audit event (configurable)
  - for observe-only events: allow progress but emit `HookFailed` audit event

### 6.3 Script hook contract (MUST be explicit)

Hooks are executed as OS processes.

**Invocation**
- Executable path: `HookSpec.exec_path` after expansion:
  - `~` expansion (HOME/USERPROFILE)
  - environment variable expansion (`%USERPROFILE%` on Windows, `${HOME}` on Unix)
- Working directory: `ctx.repo_root_real`
- stdin: UTF-8 JSON (HookRequest)
- stdout: UTF-8 JSON (HookResponse)
- stderr: diagnostics only (truncated/redacted)
- exit codes:
  - `0`: stdout must contain a valid HookResponse JSON
  - non-zero: treated as hook failure (no response applied)

#### 6.3.1 stdin JSON schema (HookRequest v2)

**Schema version:** `"2"`

**Compatibility**
- Runtime SHOULD support HookRequest `"1"` for a transition period.
- Default emission is `"2"` (configurable per hook or globally via `printrevolt.hooks.request_schema_version`).

```json
{
  "schema_version": "2",
  "event": "before_tool",
  "session": {
    "id": "019aa3db-5685-79f1-a222-9ceb1bd22e94",
    "repo_root": "/repo",
    "cwd": "/repo",
    "os": "linux",
    "shell": "bash",
    "template": { "id": "repo:.codex/templates/security.md", "name": "Security Reviewer", "source": "repo" }
  },
  "task": {
    "id": "task_01J8QZ2Y4X2H3S8Q8C0W7E1Q9B",
    "user_turn_id": "user_turn_01J8QZ2Y4X2H3S8Q8C0W7E1Q9B",
    "prompt_preview": "Fix failing tests in ./crates/pr_hooks",
    "started_at_ms": 1771096800000,
    "supervisor": {
      "cc_task_id": "019aa3db-5685-79f1-a222-9ceb1bd22e01",
      "priority": 100,
      "status": "running",
      "depends_on_cc_task_ids": []
    }
  },
  "turn": {
    "id": "turn_01J8QZ2Y4X2H3S8Q8C0W7E1Q9C",
    "index": 1,
    "started_at_ms": 1771096800123
  },
  "tool_call": {
    "kind": "shell",
    "id": "b7b85c6a-5b67-4a54-a819-6b3cf3f0f75b",
    "cwd": "/repo",
    "argv": ["git", "push", "origin", "HEAD"],
    "raw": null
  },
  "tool_result": null,
  "policy_summary": {
    "active_branch": "pr/20260214-fix-thing",
    "verify_fresh": false,
    "protected_branches": ["main", "master"]
  }
}
```

**Type constraints**
- `schema_version`: string `"2"`
- `event`: one of:
  - `"on_session_start"`
  - `"on_session_end"`
  - `"before_task"`
  - `"on_turn_start"`
  - `"before_tool"`
  - `"after_tool"`
  - `"before_finalize"`
  - `"on_turn_complete"`
  - `"on_turn_aborted"`
  - `"on_interrupt"`
  - `"after_task"`
  - `"on_item_started" | "on_item_completed"`
- `session.id`: string UUID-like; not required to be UUID but stable.
- `task`: present for task/turn/tool/finalize events; omitted/null for `on_session_start` if no task is active yet.
- `turn`: present for turn/tool/finalize events; omitted/null otherwise.
- `tool_call`: present for `before_tool` and `after_tool`; omitted/null otherwise.
- `tool_result`: present for `after_tool`; omitted/null otherwise.
- `policy_summary`: redacted, non-secret summary. MUST NOT include secrets or full file contents.
- `task.supervisor`: optional; present only when a supervisor (e.g., Command Center) provides metadata:
  - `cc_task_id`: UUIDv7 string (Command Center task id)
  - `priority`: int
  - `status`: string (`draft`|`waiting_deps`|`queued`|`running`|`waiting_approval`|`blocked`|`done`|`failed`|`canceled`)
  - `depends_on_cc_task_ids`: string[]
  - This metadata is for correlation/UX only; it MUST NOT affect safety decisions.

#### 6.3.2 stdout JSON schema (HookResponse v2)

Hook response is one of:

**allow**
```json
{
  "schema_version": "2",
  "decision": "allow",
  "reason": "OK"
}
```

**deny**
```json
{
  "schema_version": "2",
  "decision": "deny",
  "code": "PR_VERIFY_REQUIRED",
  "reason": "Verification required before push",
  "remediation": ["Run: ./devctl verify", "Re-run the push"]
}
```

**modify**
```json
{
  "schema_version": "2",
  "decision": "modify",
  "code": "PR_REWRITE",
  "reason": "Force using --force-with-lease instead of --force",
  "modified_tool_call": {
    "kind": "shell",
    "id": "b7b85c6a-5b67-4a54-a819-6b3cf3f0f75b",
    "cwd": "/repo",
    "argv": ["git", "push", "--force-with-lease", "origin", "HEAD"],
    "raw": null
  }
}
```

**Type constraints**
- `schema_version`: `"2"`
- `decision`: `"allow" | "deny" | "modify"`
- `reason`: required for all decisions (human-readable)
- `code`:
  - required for `deny`
  - required for `modify`
  - must be stable string suitable for audit and support triage
- `remediation`: required for `deny` (array of strings; may be empty but discouraged)
- `modified_tool_call`: required for `modify`, must be a full `ToolCall` object.
- Unknown fields are ignored (forward compatibility).

**Decision applicability**
- `modify` is valid only for `before_tool`.
- `deny` is honored only for gate/decision events (`on_session_start`, `before_task`, `before_tool`, `before_finalize`).
- For observe-only events, hooks SHOULD return `allow`. If a hook returns `deny`/`modify` for an observe-only event, runtime MUST treat it as a contract error and ignore the decision (audit as `HookContractViolation`).

#### 6.3.3 Exit code semantics

- `0`:
  - stdout must parse as HookResponse
  - if parse fails => treat as hook failure
- non-zero:
  - treat as hook failure
  - stderr may be included in audit as redacted preview.

**Failure handling (default)**
- `on_session_start` / `before_task`:
  - deny start (PrHookFailure) unless enforcement mode is warn
- `before_tool`:
  - mutating call => deny (PrHookFailure)
  - read-only call => allow with warning event
- `before_finalize`:
  - deny finalize (PrHookFailure) unless break-glass is granted
- observe-only events:
  - allow progress; audit failure

---

### 6.4 Pipelines (Pipelines → Workflows → Parts)

Pipelines are **engine-owned automation** implemented inside the codex-pr process. They are designed for:
- **Preflight** (session/task start): detect missing prerequisites and either fix safely (with approval) or block with remediation.
- **Verify/fix loops** (finalize gate): run verification (tests/lint/build), and if it fails, drive a bounded repair loop before allowing completion.

Key principle: pipeline-initiated work MUST execute via the same tool boundary as agent tools:
1) policy pre-check
2) hook chain (optional; may deny/modify)
3) policy clamp + re-validation
4) tool execution
5) `after_tool` evidence observation + audit

This preserves the “guaranteed enforcement boundary” even when automation is not model-driven.

#### 6.4.1 Lifecycle triggers

Pipelines run on a subset of the same lifecycle signals as hooks:
- `on_session_start`: run preflight that is independent of the user prompt (best-effort; should be fast).
- `before_task`: run preflight that depends on task/prompt/template and may deny start.
- `before_finalize`: run verify/fix loop gates; may deny completion.
- `on_session_end`: best-effort cleanup/reporting only (MUST NOT be required for correctness).

**Manual invocation (no bypass)**
- In this fork, pipelines are primarily lifecycle-driven (automatic at gates).
- There is no built-in “run pipeline now” slash command specified by default.
- Optional extension (recommended for Command Center + TUI parity):
  - Add a CLI/TUI command: `/pipelines run <pipeline_id> [--trigger before_finalize]`
  - Add a headless protocol method: `session.run_pipeline { pipeline_id, trigger }`
  - Semantics:
    - runs the pipeline as if the given trigger fired (“synthetic trigger”)
    - all actions still transit the normal tool boundary (policy + hooks + approvals)
    - emits `Pipeline*` events so UIs can show progress and blocks
    - does not alter policy floors or evidence directly; it only drives tool calls that may produce evidence.

Observe-only integration points (pipeline can emit notes but should not block):
- `on_turn_aborted`, `on_interrupt`, `after_task` (e.g., mark pipeline run as aborted).

#### 6.4.2 Definition, compilation, and precedence

**Bundle locations**
- Global (user) pipeline bundle:
  - `<CODEX_HOME>/printrevolt/pipelines.json`
- Project pipeline bundle:
  - `<repo_root>/.codex/printrevolt/pipelines.json`

**Bundle format (`pipelines.json`, schema `"1"`)**
- Top-level:
  - `schema_version`: `"1"`
  - `pipelines`: list of pipeline headers (selectable by id)
  - `workflows`: list of workflow definitions (reusable units)
- Pipeline headers may include:
  - `enabled` (default true): supports keeping “library” pipelines in a bundle without activating them.
- Workflows are executable graphs expressed as ordered parts plus optional explicit transitions:
  - If a part omits `on_success`, compilation treats it as `next_part` (or `complete` if last).
  - If a part omits `on_failure`, compilation uses safe defaults:
    - gate triggers (`on_session_start`, `before_task`, `before_finalize`): `block` with remediation
    - observe-only triggers: `complete` (do not block)
  - `when` predicates are evaluated with a bounded Facts store; if `when` is false, the part is skipped and treated as success.
- Workflows may optionally specify teardown:
  - `finally_workflow`: workflow id to run on any exit (success/failure/block/interrupt).
  - Parts may optionally specify `defer`: a list of cleanup actions registered when the part executes; these run LIFO on exit.

**Precedence**
Highest → lowest:
1) CLI overrides (session-only)
2) Project pipeline bundle (only if repo is trusted; see trust model)
3) Global pipeline bundle
4) Built-in defaults (minimal safe pipeline)

**Active vs inactive**
- A pipeline is considered active for lifecycle triggers only when:
  - pipelines are enabled globally (`[printrevolt.pipelines].enabled=true`)
  - the pipeline is selected (`default_pipeline_id` or manual invocation) AND `enabled!=false`
  - the current lifecycle event is present in `triggers`
- Unselected or `enabled=false` pipelines are valid “library” definitions:
  - kept for future use
  - surfaced in UIs
  - excluded from reachability calculations unless selected.

**Trust model**
- Repo pipelines are treated like repo hooks: disabled by default and enabled only when the repo is explicitly trusted/allowlisted.
- Even when trusted, pipelines are clamped by policy floors and capability allowlists (see below).

**Scoping and merge strategy**
- Bundles are scoped:
  - Global bundle: user-wide defaults
  - Project bundle: repo-specific automation (trusted/allowlisted only)
- Effective pipeline set is computed by precedence (see above) with conservative rules:
  - No deep merge of workflow graphs at runtime.
  - Project bundle may add pipelines/workflows and may override an existing pipeline/workflow by id.
  - “Pack composition” (import/includes) is a compiler concern (flatten deterministically) rather than runtime merge.

**Compilation**
- Pipeline definitions are compiled into a deterministic executable form:
  - pre-parsed regex/glob predicates
  - normalized argv templates (no raw shell by default)
  - a transition table and loop guards
- Compiled output is content-addressed (sha256) for auditability.

**Compiler invariants (end detection + safety)**
- Compilation MUST produce an explicit, total control-flow graph:
  - every part has explicit `on_success` and `on_failure` in the compiled form
  - every transition target exists
  - every workflow has at least one terminal path:
    - `complete` OR `block`
- “Falling off the end” is not allowed in the compiled form:
  - if a workflow’s last part omits `on_success`, the compiler MUST fill `complete`
  - if a part omits `on_failure`, the compiler MUST fill `block` for gate triggers and `complete` for observe-only triggers (per §6.4.2), but the compiled graph must still be explicit
- If compilation cannot satisfy these invariants, codex-pr MUST disable the pipeline and audit the reason.

**Pack composition (`includes`)**
- Runtime deep-merge of graphs is intentionally avoided.
- Pipelines should be composed at compile time:
  - a pipeline/workflow definition MAY reference `includes` (pack ids/versioned references)
  - the compiler flattens/includes into a single deterministic bundle:
    - stable ids preserved
    - overrides are explicit “replace by id” (no partial merges)
  - compiled bundle contains `compiler_version` and `source_packs` metadata for audit.

#### 6.4.2.1 Example bundle (default verify/fix loop)

```json
{
  "schema_version": "1",
  "pipelines": [
    {
      "id": "default",
      "name": "Default Verify Loop",
      "triggers": ["before_task","before_finalize"],
      "entry_workflow": "preflight",
      "loop_guards": { "max_cycles": 3, "max_attempts_per_part": 2, "repeat_failure_signature_limit": 2 },
      "capabilities": ["read_only","verify_exec_safe"]
    }
  ],
  "workflows": [
    {
      "id": "preflight",
      "name": "Preflight",
      "parts": [
        { "id": "note_preflight", "kind": { "kind":"emit_note", "message":"Running preflight checks." } },
        {
          "id": "ensure_worktree",
          "kind": { "kind":"ensure_worktree", "worktree_root":"~/.codex/printrevolt/worktrees", "naming":"prcc/${session_id}", "require_approval": true }
        },
        {
          "id": "docker",
          "kind": { "kind":"check_docker", "mode":"warn", "probe_argv":["docker","info"] }
        },
        {
          "id": "changed_files",
          "kind": { "kind":"compute_changed_files", "base_ref":"merge_base_main", "include_uncommitted": true },
          "on_success": { "kind":"goto_workflow", "workflow_id":"verify_frontend" }
        }
      ]
    },
    {
      "id": "verify_frontend",
      "name": "Verify Frontend",
      "parts": [
        {
          "id": "run_frontend_tests",
          "when": { "kind":"changed_files_any", "globs":["frontend/**","web/**","ui/**"] },
          "kind": { "kind":"run_command", "cwd":".", "argv":["npm","test"], "category":"verify", "timeout_ms": 900000 }
        },
        {
          "id":"assert_ok",
          "kind": { "kind":"assert_last_result", "exit_code": 0 },
          "on_success": { "kind":"complete" },
          "on_failure": { "kind":"goto_workflow", "workflow_id":"fix_and_retry" }
        }
      ]
    },
    {
      "id": "fix_and_retry",
      "name": "Suggest Fix + Retry",
      "parts": [
        {
          "id":"suggest_fix",
          "kind": { "kind":"suggest_agent_fix", "template":"Tests failed. Please fix and re-run: ${failed_command}", "require_user_confirm": true }
        },
        {
          "id":"guard",
          "kind": { "kind":"loop_guard", "max_cycles": 3, "repeat_failure_signature_limit": 2 },
          "on_success": { "kind":"goto_workflow", "workflow_id":"verify_frontend" }
        }
      ]
    }
  ]
}
```

Notes:
- The compiled form includes explicit transitions (shown here) and loop guards. When `assert_ok` fails during `before_finalize`, the pipeline blocks completion until either:
  - verification passes, or
  - loop guards trip and the pipeline blocks with “needs human review”.
- `suggest_agent_fix` produces a structured `suggested_user_message`; UIs may choose to send it to the agent (auto-send configurable).

#### 6.4.2.2 Variables, derived facts, and templating

Pipelines need a small, safe way to:
- pass non-secret parameters (e.g., base branch, worktree naming)
- derive simple “status” values from tool output (e.g., “service down”)
- reuse those values for branching and messaging

**Part-to-part dataflow (what users should expect)**
- Parts pass data to subsequent parts only via:
  1) **Config vars** (`[printrevolt.vars]`, resolved by precedence),
  2) **Cleanup defaults** (`[printrevolt.cleanup]`, resolved by precedence; primarily used by UX/builders to choose prompt vs always/manual teardown), and
  3) **Facts store** (ephemeral values produced during the current pipeline run).
- Parts MUST NOT pass unbounded logs or raw file contents through Facts.
- Facts are not a persistence mechanism:
  - they are in-memory and bounded
  - they are cleared when the pipeline run ends (and generally when the session ends)
  - durable artifacts (full logs, diffs) are stored as artifact files and referenced by hash/id only.

**Three data channels**
1) **Config variables** (“vars”): user/project/session configuration values that are not secrets.
   - Example: `BASE_BRANCH="main"`, `WORKTREE_ROOT="~/.codex/printrevolt/worktrees"`
1.1) **Cleanup defaults**: user/project/session configuration that controls recommended teardown posture (prompt/always/manual/never).
2) **Facts**: bounded, ephemeral values produced by pipeline parts during execution (changed files, last tool result, derived statuses).
3) **Secrets**: MUST NOT be stored in pipeline bundles or vars. Secrets remain in environment variables or OS keychains and are never surfaced in Facts except as “present/absent”.

**Templating**
- Pipelines may use a conservative placeholder system in a subset of fields:
  - `ensure_worktree.naming`
  - `emit_note.message`
  - `suggest_agent_fix.template`
  - `request_agent_decision.prompt_template`
  - (optional) command catalog argv elements, if enabled
- Supported placeholders:
  - `${session_id}`, `${task_id}`, `${turn_id}`
  - `${var.<NAME>}` (config vars)
  - `${fact.<KEY>}` (derived facts)
  - `${failed_command}` (from `facts.last_tool_result.argv` when present)
  - `${stderr_preview}` / `${stdout_preview}` (bounded previews only)

**Resolution order**
- On each part execution, resolve placeholders using:
  1) stable ids (`session_id`, `task_id`, `turn_id`)
  2) config vars (merged by precedence: session override → project → user → defaults)
  3) Facts (values computed earlier in the same pipeline run)
- Missing placeholder:
  - treat as part failure with remediation (“missing var/fact …”), unless the field explicitly allows empty.

**Approval hashing**
- Any approval prompt that is meant to authorize a subsequent action MUST hash the fully-resolved, canonical action JSON (post-templating), so “approve worktree X” cannot execute “worktree Y”.

#### 6.4.2.3 Command catalogs (user-defined commands as data)

To avoid embedding long argv lists in every pipeline, codex-pr supports an optional command catalog:
- Global command catalog:
  - `<CODEX_HOME>/printrevolt/commands.json`
- Project command catalog:
  - `<repo_root>/.codex/printrevolt/commands.json` (trusted/allowlisted only; mirrors repo pipeline trust)

`RunCommand` may reference either:
- `argv` (inline canonical argv), or
- `command_id` (lookup from effective command catalog; resolved to canonical argv + metadata).

**Trust and clamping**
- Repo command catalogs are disabled unless the repo is trusted/allowlisted.
- Resolved commands are still subject to:
  - pipeline capability clamping
  - PrintRevolt policy floors
  - allowlisted argv prefixes per category
  - upstream sandbox + approvals

**“No hidden commands” guarantee**
- Before executing a command_id-resolved command, codex-pr MUST emit a note/audit field with the fully-resolved argv and cwd so UIs can display it.

**Command catalog format (`commands.json`, schema `"1"`)**
```json
{
  "schema_version": "1",
  "commands": [
    {
      "id": "frontend:test",
      "name": "Frontend tests",
      "cwd": ".",
      "argv": ["npm","test"],
      "category": "verify",
      "timeout_ms": 900000,
      "child_process_policy": "inherit",
      "script_runner": false,
      "requires_sandbox": false
    }
  ]
}
```

Rules:
- `id` is a stable string key referenced by `run_command.command_id`.
- `argv` is canonical argv (no raw shell string).
- Project catalogs may override a global `id` by replacement (no deep merge).
- `script_runner` (optional):
  - set true for commands like `npm run`, `make`, `yarn`, which may spawn many child processes.
  - used for UX warnings and to apply stricter posture.
- `requires_sandbox` (optional):
  - if true, codex-pr MUST deny running the command unless upstream sandbox mode is active (unless break-glass).

#### 6.4.3 Capability model (clamped)

Each pipeline compiles to a `capabilities` mask, clamped by policy floors:
- `READ_ONLY`: observe repo state and emit notes.
- `VERIFY_EXEC_SAFE`: run allowlisted verification commands (tests/lint/build) with canonical argv.
- `REPO_MUTATE_SAFE`: apply bounded repo mutations via tool calls (still subject to policy path containment).
- `GIT_MUTATE_SAFE`: safe git operations (create branch, worktree ops) subject to branch protections and approvals.

Floor rule:
`capabilities_effective = capabilities_compiled ∩ capabilities_allowed_by_policy_floor`

If a pipeline requires a capability not allowed by floors, it MUST be disabled and audited as `PipelineClampedDisabled`.

#### 6.4.4 Execution model

Pipelines maintain an in-memory `PipelineRunState` scoped to the current session/task. State is bounded and includes:
- pipeline id + compiled sha
- current workflow/part pointer
- attempt counters (per part and per cycle)
- last failure signature (for repeated failure detection)
- a cleanup stack (deferred teardown actions; see §6.4.6)
- cached facts (bounded; see below)

**Loop guards**
- `max_cycles` (default 3) for verify↔fix loops.
- `max_attempts_per_part` (default 2) for flaky commands.
- `repeat_failure_signature_limit` (default 2): if the same failure signature repeats, stop and require human review.

**Failure signatures**
- Computed from: `(part_id, tool argv hash, exit_code, stderr_preview_hash)` to avoid leaking sensitive output while still detecting “same failure again”.

**Facts caching / throttling**
- `compute_changed_files` is potentially expensive (git calls). The engine/runtime MUST:
  - compute it at most once per `(repo_fingerprint, base_ref, include_uncommitted)` per pipeline run, and cache the result in Facts
  - reuse cached `facts.changed_files` across workflows within the same run
  - optionally throttle recomputation across back-to-back triggers using a short TTL (default 2s) in runtime

**Trace/event volume controls**
- Pipeline engine emits `PipelinePartStarted/Completed` for observability, but MUST remain bounded:
  - cap maximum part events per run (default 500); if exceeded, stop and `block` with `PR_PIPELINE_TOO_MANY_STEPS`
  - coalesce repeated failures (“retry #n”) into a single summarized note event where possible
  - large outputs remain artifacts; events contain only hashes + bounded previews

**Interaction with the agent**
- Pipelines do not silently rewrite the agent’s final answer.
- When a pipeline blocks completion, it must provide:
  - a stable code
  - a human reason + remediation steps
  - an optional `suggested_user_message` that a UI may send to the agent to request a fix (configurable: auto-send vs require user confirmation).

**Prompting the agent from pipelines (bounded)**
- Pipelines MAY ask the agent to perform follow-up work when something happens (usually on error, sometimes on success), but MUST do so without enabling arbitrary execution.
- Supported patterns:
  - `suggest_agent_fix`: produce a suggested user message; UI may send it to the agent (default: require user confirmation).
  - `request_agent_decision`: ask the agent for a structured next-step choice from an allowlist of `allowed_next_workflows` (validated; no arbitrary commands).
- Infinite-loop defenses (must be present in any verify→fix→retry design):
  - `loop_guard` + `max_cycles`
  - `repeat_failure_signature_limit`
  - a per-run cap on agent prompts (default 1; configurable) so a misconfigured pipeline cannot spam prompts across retries.

#### 6.4.5 Pipeline parts catalog (v1)

Pipeline “parts” are typed, allowlisted primitives. A part may:
- emit notes/warnings
- compute facts (changed files, repo state)
- run a bounded tool call via the tool boundary
- request approval / user input (via upstream approvals)

**Facts store (bounded)**
- Pipelines maintain a bounded in-memory Facts store per session/task, used for predicates and for UX:
  - `facts.changed_files`: output of `compute_changed_files`
  - `facts.last_tool_result`: summary of last `ToolCall` result (hashes + bounded previews)
  - `facts.derived`: map of small derived values from `extract_value` (string/int/bool; bounded)
  - `facts.repo_fingerprint`: current repo fingerprint (for verify freshness checks)
  - `facts.session_id`, `facts.task_id`, `facts.turn_id`: identifiers for templating and audit correlation

All parts share common fields:
- `id` (stable string; used in audit + failure signatures)
- `label` (optional human-friendly label for UI)
- `when` (optional predicate; if false → skipped and treated as success)
- `on_success` / `on_failure` (optional transitions; if omitted, compilation fills safe defaults; see §6.4.2)
- `retry` (optional per-part retry policy; bounded by pipeline loop guards)
- `defer` (optional cleanup actions; if the part executes, cleanup actions run LIFO on exit; see §6.4.6)
- `cleanup` (optional metadata for teardown UX/policy):
  - `kind`: `worktree` | `branch` | `service` | `db` | `tmpdir` | `other`
  - `destructive`: bool (default false)
  - `policy`: `required` | `optional` (default `required`)

Common predicates (`when`):
- `changed_files_any`: true when computed `changed_files` contains any path matching configured glob(s).
- `os_is`: match on `ctx.os` (linux/windows/macos).
- `env_var_present`: checks for required env var presence (value not exposed).
- `fact_equals`: checks `facts.derived[<key>]` equals a literal.
- `fact_matches`: checks `facts.derived[<key>]` matches regex (bounded).

**Transition targets**
- `next_part`: proceed within the current workflow.
- `goto_workflow`: jump to another workflow by id.
- `goto_part`: jump to a specific part in a workflow.
- `complete`: stop pipeline evaluation for the current trigger.
- `block`: stop and emit a blocking failure (code/reason/remediation). For gate triggers this blocks task start/finalize.

**Part: `emit_note`**
- Purpose: add trace context (e.g., “Running frontend tests because frontend files changed”).
- Inputs: `message` (string; bounded, redacted).
- Output: none.
- Facts: does not read/write Facts.

**Part: `require_approval`**
- Purpose: request an explicit user decision using the existing approval system.
- Inputs:
  - `scope`: `one_off` | `session`
  - `mode`: `gate` | `confirm` (default `gate`)
  - `prompt`: short text shown to user
  - `remediation`: list of user actions
  - `action_hash`: sha256 of canonical JSON of the requested action(s)
- `confirm` mode only:
  - `output_key`: writes `facts.derived[output_key]=true|false`
- Output:
  - `gate`: approved/denied
  - `confirm`: writes a derived bool and continues
- Semantics:
  - `gate`:
    - denial blocks the pipeline and (if at a gate trigger) blocks task start/finalize.
  - `confirm`:
    - denial is treated as “No”: write `false` and continue (no block).
    - intended for user-controlled teardown (“Stop the db we started?” default No) and other non-fatal choices.
- Policy/approvals:
  - implemented by emitting an upstream `ApprovalRequested` and waiting for `session.resolve_approval`.
  - approvals are bound to `action_hash` to prevent “approve once, run something else”.
  - UI default MUST be safe:
    - default choice is deny/cancel
    - “approve for session” is allowed only when explicitly enabled by policy/config.

**Part: `compute_changed_files`**
- Purpose: compute a change set for routing verification.
- Inputs:
  - `base_ref`: `"merge_base_main"` | `"base_branch"` | explicit ref
  - `include_uncommitted`: bool (default true)
- Output:
  - `changed_files`: list of repo-relative paths (bounded; default max 5k).
- Implementation:
  - uses git plumbing (in runtime, throttled) and MUST NOT read file contents.
- Facts:
  - writes `facts.changed_files`.

**Part: `run_command`** (canonical argv only)
- Purpose: run verification/setup commands deterministically.
- Inputs:
  - `cwd`: repo-relative path (default `.`)
  - `argv`: string[] (inline) OR `command_id`: string (catalog reference)
  - `category`: `verify` | `lint` | `build` | `setup` | `git` (used for allowlists and UX labels)
  - `env`: optional map of additional environment variables (bounded; secrets forbidden)
  - `timeout_ms`: optional override (bounded by global max)
  - `child_process_policy`: optional:
    - `inherit` (default): no per-child gating beyond upstream sandbox/OS containment
    - `audit_allowlist`: record child execs and branch/fail if disallowed (platform-specific)
    - `enforce_allowlist`: attempt to prevent non-allowlisted child execs (platform-specific; may be unsupported)
  - `expect`: optional assertions:
    - `exit_code` (default 0)
    - `stdout_regex` / `stderr_regex` (bounded)
- Output: a `ToolResult` summary (hashes + previews).
- Semantics:
  - executed as a normal `ToolCall::Shell` and thus flows through policy + hooks + approvals.
  - `argv` MUST match an allowlisted prefix for its category (configurable), otherwise the pipeline is disabled and audited.
  - `expect` is evaluated without re-running the command; failure routes through `on_failure`.
  - Child processes:
    - Upstream approvals/prefix rules apply to the top-level command only; child execs are not individually gated.
    - If `child_process_policy` is enabled, codex-pr MAY run the command through a platform-specific wrapper to record/enforce child exec behavior; if unsupported, it MUST fail closed (deny) unless explicitly configured otherwise.
    - If the resolved command is classified as a script runner (`script_runner=true`) and/or `requires_sandbox=true`:
      - codex-pr MUST surface a warning note/event with this classification
      - codex-pr SHOULD require upstream sandbox mode for execution by default (and deny otherwise unless break-glass is active)
- Facts:
  - writes `facts.last_tool_result`.

**Part: `assert_last_result`**
- Purpose: gate based on prior tool result without rerunning.
- Inputs:
  - predicates on last result: exit_code, regex on previews, timed_out.
- Output: success/failure.
- Semantics:
  - If there is no `facts.last_tool_result`, treat as failure with remediation (pipeline misconfiguration).

**Part: `ensure_worktree`** (safe git automation)
- Purpose: ensure the session is operating in an isolated worktree when configured.
- Inputs:
  - `worktree_root`: absolute or repo-relative base directory (default: `CODEX_HOME/printrevolt/worktrees/`)
  - `naming`: template (e.g., `prcc/<session_id>`)
  - `require_approval`: bool (default true)
- Output: ensured/blocked.
- Semantics:
  - if already in a worktree/isolated path, emits a note and continues.
  - if not, may create a worktree via git commands (subject to policy floors and approval).
- Safety:
  - MUST NOT create worktrees outside `worktree_root`.
  - MUST NOT rebase/merge/reset as part of this part.

**Part: `ensure_branch`** (safe git automation)
- Purpose: ensure work occurs on a non-protected, per-task branch.
- Inputs:
  - `base_branch`: optional; default `${var.BASE_BRANCH}`
  - `naming`: template for branch name (e.g., `prcc/${session_id}` or `prcc/${task_id}`)
  - `require_approval`: bool (default true)
- Output: ensured/blocked.
- Semantics:
  - if current branch is already non-protected and not detached, emit note and continue.
  - if on a protected branch (e.g., `main`) or detached HEAD:
    - require approval (default deny) to create/switch to a new branch
    - create branch from `base_branch` (or current HEAD if `base_branch` not resolvable) using allowlisted git operations
- Safety:
  - MUST NOT switch to or create a protected branch.
  - MUST respect policy floors for branch protections and any “require verify before push” posture.

**Part: `check_docker`**
- Purpose: verify Docker is installed/running before tasks that depend on it.
- Inputs:
  - `mode`: `require` | `warn`
  - `probe_argv`: defaults to `["docker","info"]`
- Output: ok/warn/blocked.
- Semantics:
  - `require` blocks at gate triggers with remediation.
  - `warn` emits a note only.

**Part: `suggest_agent_fix`**
- Purpose: provide a structured suggested user message when verification fails.
- Inputs:
  - `template`: string with placeholders (e.g., `${failed_command}`, `${stderr_preview}`)
  - `require_user_confirm`: bool (default true)
- Output:
  - `suggested_user_message` (bounded + redacted).
- Semantics:
  - does not automatically change the conversation; UIs may choose to send it.

**Part: `extract_value`** (derive a fact from output)
- Purpose: convert an unstructured error message into a small derived value that can be used for branching (e.g., detect “service is down”).
- Inputs:
  - `source`: `stdout` | `stderr` | `combined` (reads bounded previews only)
  - `parser`:
    - `json_path`: extract from JSON output (preferred when possible)
      - v1: `json_path` is a JSON Pointer (RFC 6901) starting with `/` (e.g., `/status/healthy`)
    - `regex`: bounded regex + optional capture group
  - `output_key`: string key to store into `facts.derived`
  - `normalize`: optional list (e.g., `lowercase`, `trim`)
- Output: writes `facts.derived[output_key]`.
- Semantics:
  - MUST be bounded: max input bytes, max regex steps/time, max output bytes.
  - MUST NOT persist full output; only derived small values are stored.

**Part: `assert_fact`**
- Purpose: gate/branch using derived facts without rerunning tools.
- Inputs:
  - `key`
  - `equals` (optional literal) and/or `matches_regex` (optional)
- Output: success/failure.

**Part: `request_agent_decision`** (bounded, allowlisted branching)
- Purpose: ask Codex (the agent) for a structured next step decision when a command fails, without allowing arbitrary execution.
- Inputs:
  - `prompt_template`: string with placeholders (e.g., `${failed_command}`, `${stderr_preview}`, `${artifact_ref}`)
  - `allowed_next_workflows`: list of workflow ids the agent is allowed to choose from
  - `require_user_confirm`: bool (default true)
- Output:
  - `agent_decision`: `{ next_workflow_id, rationale }` (bounded)
- Semantics:
  - The engine MUST NOT execute arbitrary commands based on free-form model text.
  - The engine MUST enforce a per-run budget for agent prompting (see `printrevolt.pipelines.limits.max_agent_prompts_per_run`); if exceeded, `block` with “needs human review”.
  - The decision MUST be validated:
    - `next_workflow_id` must be in `allowed_next_workflows`
    - output must parse as strict JSON (schema pinned in compiler)
    - otherwise treat as failure and route through `on_failure` with remediation (“Pick one of: …”).
  - UX:
    - default behavior is to emit a `suggested_user_message` asking the agent for a decision; UI sends it only with confirmation.
    - optional future behavior: codex-pr may run an internal “agent query” and wait for the response, but only when explicitly enabled.

**Part: `loop_guard`**
- Purpose: enforce retry limits and break infinite loops.
- Inputs:
  - `max_cycles`, `repeat_failure_signature_limit`
- Output: allow/blocked with remediation.
- Semantics:
  - increments the pipeline cycle counter when entered from a failure transition.
  - if limits are exceeded, routes to `block` with a stable code (e.g., `PR_PIPELINE_NEEDS_HUMAN_REVIEW`).

This catalog is intentionally conservative. Additional parts may be added only if they remain typed, deterministic, and enforceable through policy floors.

**Typed parts: engine-defined, schema is public**
- Part kinds are implemented inside codex-pr (engine-defined).
- Pipeline bundles are “public” in the sense that:
  - the JSON schema is documented and versioned
  - Command Center and users can author bundles against that schema
- Adding a new part kind requires a codex-pr update (and must be reflected in the documented schema).
- Any notion of “user-defined executable parts” (arbitrary code) is considered break-glass and must be treated like running arbitrary shell: off by default, trusted-only, and still routed through policy/approvals.

**Expected predefined parts (roadmap)**
From a user perspective, the following additional primitives are commonly expected. These SHOULD be added only if they remain bounded/typed and enforceable through floors:
- Preflight:
  - `check_tool_installed` (git/node/docker present + version constraints)
  - `check_env_var` (present/absent; never reveal value)
  - `assert_clean_worktree` (block or warn if dirty)
  - `prompt_user_input` (bounded string/int input; safe defaults; writes to `facts.derived` — bool prompts are covered by `require_approval(mode="confirm")`)
- Verification ergonomics:
  - `select_commands_for_changes` (route changed paths → command_id list deterministically)
  - `collect_test_failures` (parse common test output into structured summary)
  - `assert_verify_fresh` (explicitly gate using verify evidence + fingerprint)
- Git/worktree:
  - (now predefined) `ensure_branch` (create/switch safe branch name; approval gated)
  - `snapshot_diff` (create diff artifact + hash for review)
- Process/service helpers (advanced; optional):
  - `start_service` / `stop_service` with mandatory `defer` registration (if true background processes are needed beyond “docker compose up -d” patterns)

#### 6.4.6 Teardown / cleanup (finally + defer + user control)

Some verification flows need “always run cleanup” semantics (e.g., bring up an e2e backend, run tests, then shut it down even on failure). Other flows create resources that users may want to keep (e.g., a database snapshot or a worktree for later inspection). PrintRevolt treats teardown as **typed steps** with **user-controlled destructive cleanup by default**.

Pipelines support two complementary teardown mechanisms:

1) **Workflow-level `finally_workflow`**
- If set, codex-pr MUST attempt to run the `finally_workflow` whenever the workflow exits for any reason:
  - success, failure, `block`, `complete`, interrupt, turn abort.
- The finally workflow is executed in a dedicated `cleanup` phase:
  - `PipelinePart*` events MUST include `phase = "cleanup"` (default is `"main"`).
  - Cleanup parts run through the normal tool boundary (policy + hooks + approvals).
- Cleanup failures:
  - MUST be audited and surfaced to the user.
  - MUST NOT “hide” the primary failure reason (the failure that caused cleanup to run).
  - MUST NOT be treated as verification success; they only affect whether cleanup completed.

2) **Part-level `defer` (cleanup stack)**
- Any part may include `defer: [<cleanup parts>]`.
- When the part executes (not skipped), its defer list is pushed onto a cleanup stack (LIFO).
- On workflow exit, the engine runs the cleanup stack in LIFO order (best-effort, bounded).
- Intended use: a “start service” part registers “stop service” cleanup right where the service is started.

**User-controlled cleanup (recommended default)**
- Destructive teardown steps (e.g., deleting a worktree, dropping a DB, removing a volume) SHOULD be authored as **optional** cleanup:
  - include a `require_approval(mode="confirm", output_key="cleanup_ok", ...)` step (UI default deny)
  - gate destructive cleanup steps with `when: { fact_equals: ["cleanup_ok", true] }`
- In Command Center, this maps naturally to an explicit “Cleanup now?” prompt with default **No**, plus a persisted “remember for session” option when enabled.
- In particular: pipelines MUST NOT delete worktrees/branches automatically as part of teardown; deletion is always explicit and (by default) user-confirmed.

**Safety + determinism**
- Cleanup actions are still tool calls and thus:
  - subject to policy floors and hook chain
  - may require approval (e.g., `git` mutations, running heavy commands)
- Cleanup is best-effort and bounded:
  - max cleanup duration per trigger (default 60s; configurable)
  - per-part cleanup timeout uses normal tool call timeouts (bounded)
- `on_session_end` is not relied upon for correctness; it may be used for additional best-effort reporting/cleanup only.

#### 6.4.7 Git automation primitives (worktree + branch) — baked into codex-pr

Worktree and branch safety are core invariants for PrintRevolt and are implemented in codex-pr (not just “best-effort prompt instructions”).

codex-pr provides **typed Git primitives** used by pipelines and (optionally) by supervisors:
- `ensure_worktree`:
  - creates/ensures the session is running in an isolated worktree under `WORKTREE_ROOT`
  - uses allowlisted git operations + policy checks (no rebase/merge/reset)
  - SHOULD use a local lock file under `WORKTREE_ROOT` to avoid accidental collisions in standalone use
- `ensure_branch`:
  - creates/switches to a non-protected per-task branch using a naming template
  - clamped by protected branch rules + branch scheme rules + approvals

**Supervisor extension (Command Center)**
- A supervisor MAY still own durable orchestration state (task→worktree mapping, locks, retention), but it SHOULD treat codex-pr as the source of truth for enforcement.
- If the supervisor performs Git operations directly, it MUST apply equivalent floors (protected branches, scheme, verify freshness) and maintain exclusive worktree locks.
- Preferred integration path: supervisor calls codex-pr Git primitives in a **headless repo-ops mode** (CLI subcommands or CEP methods) so approvals/policy/hook semantics are identical.

**Headless repo-ops mode (sketch)**
codex-pr MAY expose a non-agent surface that runs the same git primitive planning/execution with explicit apply gating (suitable for supervisor orchestration; approvals are still explicit and auditable via action hashing):
- `codex-pr repo ensure-worktree --repo-root ... --worktree-root ... --naming ... --branch-name ... --base-branch ... [--apply]`
- `codex-pr repo ensure-branch --repo-root ... --base-branch ... --branch-name ... [--protected ...] [--apply]`
- `codex-pr repo remove-worktree --repo-root ... --worktree-root ... --path ... [--force] [--apply]` (destructive; SHOULD be optional cleanup with default No)

Non-goals:
- codex-pr is not a full Git client (no interactive rebase UI, no arbitrary shell scripting, no push/merge automation without explicit verify freshness + approval gates).

#### 6.4.8 Unused detection (reachability / dead code)

It is valid for bundles to contain:
- pipelines that are defined but not selected (`default_pipeline_id` points elsewhere)
- workflows that are not currently reachable from a pipeline entry
- command catalog entries not referenced by any pipeline

To keep bundles maintainable at scale:
- The pipeline compiler SHOULD compute a reachability report per pipeline id:
  - reachable workflows/parts starting from `entry_workflow` following all `on_success`/`on_failure` edges
  - unreachable workflows/parts (“dead code”)
  - referenced `command_id`s and unused command ids (if catalogs are enabled)
- codex-pr MAY emit a warning audit event when unreachable nodes exist, but MUST NOT fail the bundle solely due to unused nodes.
- UIs (Command Center) SHOULD surface “Unused” items and provide safe cleanup actions (delete/disable/export) without affecting execution.

---

## 7) Templates System (Saved + Draft)

### 7.1 Discovery and precedence

Template search paths:

- Repo templates:
  - `<repo_root>/.codex/templates/*.md`
- User templates:
  - `<CODEX_HOME>/templates/*.md` (default CODEX_HOME is `~/.codex`)

Trust posture:
- Repo templates are loaded only when the repo is trusted/allowlisted (mirrors repo hooks/commands trust).
- Untrusted repos still allow user templates, and UIs may display a note: “Repo templates disabled (untrusted repo)”.

**Precedence and conflict resolution**
- Each template has a stable `TemplateId = "<source>:<relative_path>"`.
- Display name conflicts are resolved in UI by showing suffix:
  - `"Security Reviewer (repo)"`
  - `"Security Reviewer (user)"`
- No implicit overriding by name.
- Suggested template selection prefers repo templates when confidence ties, because repo templates are typically aligned to repo workflows; this is configurable.

### 7.2 Draft template wizard (CLI/TUI behavior)

Implementation note (v1):
- codex-pr provides headless helpers for UIs to implement the picker/wizard: `codex-pr templates list --json`, `codex-pr templates validate`, `codex-pr templates draft ...` (writes draft `.md` under `CODEX_HOME/printrevolt/drafts/`).
- The full interactive picker/wizard UX is owned by the Command Center (or another supervisor UI) using these adapters; codex-pr remains the source of truth for discovery/validation and safe persistence locations.

**Menu option naming**
In the template picker list, include:
- `Default (no template)`
- `<template.name> — <template.description>` for each discovered template
- `✨ Create one-time template from prompt…` (draft wizard entry)

**State machine**

1. **Prompt Editor (upstream)**
   - user writes prompt
   - user submits prompt

2. **Template Picker**
   - displays discovered templates + Default + Draft option
   - selection transitions:
     - select existing template -> Apply and start session
     - select Draft option -> Draft Wizard
     - cancel -> return to Prompt Editor (prompt preserved)

3. **Draft Wizard**
   - Steps:
     1) Generate draft (heuristic by default; optional model mode)
     2) Review screen (name/description editable; preview visible)
     3) Actions:
        - **Continue (don’t save)** (default)
        - Save & continue
        - Regenerate
        - Edit prompt
        - Cancel

**Default action**
- On review screen, Enter selects **Continue (don’t save)**.

**Cancel behavior**
- Cancel always returns to Prompt Editor with original prompt preserved.
- “Discard prompt and exit” is a separate explicit action requiring confirmation:
  - user must type `DISCARD` to proceed.

**Save scope**
- `Save & continue` prompts for scope:
  - `User template` (default)
  - `Repo template` (only if repo is writable and user confirms)

**Persistence**
- Prompt text is stored in-memory.
- Optional crash-safety draft:
  - write to `CODEX_HOME/printrevolt/drafts/<session_id>.json` with mode 0600
  - delete on successful session start
  - TTL cleanup (default 7 days).

### 7.3 Template Contract (base spec)

Templates are Markdown with YAML frontmatter.

**Frontmatter required keys**
- `name`: string
- `description`: string (one line)
- `tags`: optional list of strings
- `defaults`: optional table mapping to policy overlay (see §7.4)

**Required Markdown sections**
Templates must include headings (case-insensitive; `##` required):

1. `## Role + Objective`
2. `## Procedure`
3. `## Outputs`
4. `## Policy Defaults`
5. `## Tooling Scope`

**Validation rules**
- Missing required frontmatter keys => template ignored.
- Missing required sections => template ignored.
- Templates larger than `templates.max_bytes` (default 256KB) => ignored (performance + safety).
- For draft templates, missing `defaults` is allowed but discouraged; policy overlay becomes empty.

### 7.4 Metadata -> Policy linkage (non-bypassable)

Templates map metadata defaults into a `TemplateOverlay` applied after the user picks a template.

**Key principle**
- Template overlay can only tighten behavior; it cannot weaken enforced floors.

**Overlay application**
1. Load `EffectiveConfig` (user/project/cli/default layers).
2. Compute `PolicyFloor` (subset of policy fields with monotonic clamping rules).
3. Apply template overlay to produce `ProposedPolicyConfig`.
4. Clamp against floor:
   - for boolean “required” fields: `effective = floor || proposed`
   - for denylist patterns: `effective = floor ∪ proposed`
   - for TTL minutes (where stricter is smaller): `effective = min(proposed, floor_max_ttl)`
   - for protected branches list: `effective = floor ∪ proposed`
   - for verify_cmd:
     - if `floor.verify_cmd_locked=true`: ignore template verify_cmd
     - else: accept template verify_cmd only if it matches allowed patterns (deny otherwise and audit)

**Non-bypassable guarantee**
- Template overlays are applied only inside `pr_runtime` and never exposed as mutable text to hooks.
- Hook modifications cannot alter config; only tool calls.
- Any policy decision is re-validated after hook modifications.

### 7.5 Template inference

**Heuristic classifier (default)**
- Scans the user prompt for keywords/tags:
  - e.g., `security`, `threat model`, `audit`, `incident`, `migration`, `performance`, `refactor`, `tests`
- Computes a score per template:
  - keyword hits + tag matches + name match
- Suggest a template when confidence ≥ `templates.suggest_threshold` (default 0.7).

**Optional model-based classification**
- Controlled by `templates.draft_generation_mode="model"` or `templates.model_classify=true`.
- Privacy constraints:
  - send only user prompt + template names/descriptions/tags
  - never send repo file contents
- Latency constraints:
  - hard timeout 300ms; on timeout fall back to heuristic suggestion.

---

## 8) Configuration & Compatibility (Don’t Clobber)

### 8.1 Locations and precedence

**CODEX_HOME**
- Use upstream `CODEX_HOME` if set; otherwise default to:
  - Unix: `~/.codex`
  - Windows: `%USERPROFILE%\.codex`

**Files**
- Upstream Codex config:
  - user: `<CODEX_HOME>/config.toml`
  - project: `<repo>/.codex/config.toml`
- PrintRevolt Mode B:
  - user: `<CODEX_HOME>/printrevolt.toml`
  - project: `<repo>/.codex/printrevolt.toml`

**Precedence**
Highest → lowest:
1. CLI overrides
2. project PrintRevolt (Mode B then Mode A)
3. user PrintRevolt (Mode B then Mode A)
4. defaults

Template overlay is applied after prompt submit and is clamped.

### 8.2 PrintRevolt config strategy

**Mode A:** `[printrevolt.*]` namespace in existing `config.toml`  
**Mode B:** separate `printrevolt.toml`

**Default and rationale**
- Default writes: **Mode B**
  - avoids formatting churn and unknown key/comment loss in upstream config
  - avoids merge conflicts with upstream Codex config schema changes
- Default reads: Mode A + Mode B (both supported)

**Coexistence / merge rules**
- Both Mode A and Mode B may exist; values are merged with deterministic precedence:
  - Mode B overrides Mode A at the same layer (user or project), because Mode B is explicitly PrintRevolt-owned.
  - Example: user layer:
    - read user Mode A table from `<CODEX_HOME>/config.toml` (if present)
    - read user Mode B file `<CODEX_HOME>/printrevolt.toml` (if present)
    - merge: Mode B wins per-field.
- CLI overrides always win.

### 8.3 Preserve unknown keys strategy

**Hard requirements**
- Never drop unknown keys or comments in upstream `config.toml`.
- Never rewrite upstream `config.toml` unless explicitly requested (Mode A write enabled).

**Mode A write behavior**
- Disabled by default.
- If enabled, use `toml_edit` to:
  - modify only `[printrevolt]` table keys
  - preserve ordering and comments elsewhere
  - avoid reformatting unrelated parts

**Mode B write behavior**
- Safe to rewrite because it is PrintRevolt-owned.
- Still avoid churn:
  - stable key ordering
  - minimal whitespace changes
- Atomic writes:
  - write to temp file in same directory
  - `fsync` file + dir
  - rename over target
- Locking:
  - `fs2::FileExt::try_lock_exclusive()` on a `.lock` file.

### 8.4 Migration & rollback

**Migration**
- No automatic config writes on first run.
- `codex-pr doctor` prints:
  - which files were read
  - precedence outcome per key (`ConfigTrace`)
  - warnings for invalid files (with line/column if available)
- If config is missing:
  - use safe defaults (policy enabled, enforcement warn mode for alpha, repo hooks off)

**Rollback**
- Session-only:
  - `--printrevolt=off` disables PrintRevolt features for that invocation (except audit minimal).
- Persistent:
  - set `printrevolt.enabled=false` in Mode B user config
- If config invalid:
  - fall back to defaults
  - emit `ConfigInvalid` audit event.

### 8.5 Advisor / recommendations (project-aware config suggestions)

PrintRevolt can optionally generate **recommendations** for policy, pipelines, command catalogs, and templates based on the current project, then present them to the user for explicit approval.

**Goals**
- Help users avoid common misconfigurations (missing verify coverage, unsafe allowlists, missing worktree/branch posture).
- Make changes explicit and auditable (no silent auto-mutation).
- Keep safety monotonic by default (recommend tightening first; relaxations are opt-in).

**CLI entrypoint**
- `codex-pr doctor --recommend`:
  - performs a bounded local scan (no file contents by default; uses metadata such as repo root, git status, `package.json` scripts when present, and current PrintRevolt config)
  - emits a structured `RecommendationBundle` (for UIs) and prints a human summary
  - does **not** apply changes by default
- Optional apply:
  - `codex-pr doctor --recommend --apply` applies selected recommendations only with explicit approvals and action hashing (same approval system as other tool calls).

**Recommendation types (examples)**
- Policy:
  - suggest `verify_execution="pipeline_only"` only if verify pipelines exist; otherwise suggest adding pipeline coverage first
  - suggest tightening dangerous denylist patterns or protected branch list
- Pipelines:
  - suggest adding `compute_changed_files` routing when verify commands are broad
  - suggest adding `loop_guard` / retry caps when missing
- Commands:
  - suggest creating `commands.json` ids for discovered scripts (e.g., `frontend:test`, `backend:test`) so pipelines remain canonical and reviewable
- Templates:
  - suggest adding/normalizing `## Policy Defaults` and tags so template selection and overlay behavior are predictable

**User interaction model**
- Recommendations are shown as diffs/patches (what would change).
- User can: approve / deny / request revision (iterative; e.g., “use pnpm instead of npm”) / cancel.
- Any applied recommendation is audited with before/after hashes; denied recommendations are also optionally recorded as a note for traceability.

**Coverage warning (critical)**
- If `policy.verify_execution="pipeline_only"` is enabled but there is no enabled verify pipeline coverage for the project, the advisor MUST emit a high-severity warning recommendation:
  - add/enable a verify pipeline, OR
  - loosen the posture (e.g., `verify_execution="allow"`), with explicit acknowledgement.

### 8.6 Concrete config schema with defaults

Default `printrevolt.toml` (Mode B):

```toml
[printrevolt]
enabled = true

[printrevolt.enforcement]
mode = "warn"                # "warn" | "deny"
deny_dangerous_always = true # beta/stable hard floor
break_glass_enabled = true
break_glass_scope = "one_off" # "one_off" | "session"
safe_mode = false

[printrevolt.policy]
# Floors and core rules
require_branch = true
protected_branches = ["main", "master"]
branch_scheme = "pr/%Y%m%d-{slug}"

require_verify_on_finalize = true
verify_cmd = "./devctl verify"
verify_cmd_match = "exact"     # "exact" | "prefix" | "regex"
verify_ttl_minutes = 10
verify_floor_max_ttl_minutes = 30
verify_cmd_locked = false
allowed_verify_cmd_regex = ["^\\./devctl verify(\\s|$)"]

# Verification execution posture (optional)
# Goal: prevent “random test runs” by the agent when you want pipelines to own verification routing.
# - "allow": agent or pipelines may run verification commands (recommended default).
# - "require_approval": agent verification requires explicit approval; pipelines still run normally.
# - "pipeline_only": only pipeline-invoked verification runs without approval (agent-initiated verify is denied).
verify_execution = "allow"      # "allow" | "require_approval" | "pipeline_only"

# Dangerous commands: always additive (floor ∪ requested)
dangerous_shell_denylist = [
  # Unix
  "(?i)\\brm\\b.*\\s-rf\\s+/",
  "(?i)\\bmkfs\\b",
  "(?i)\\bdd\\b\\s+if=",
  # Windows / PowerShell
  "(?i)\\bformat(-volume)?\\b",
  "(?i)\\bremove-item\\b.*-recurse.*-force",
  "(?i)\\bdiskpart\\b"
]

# Filesystem containment
deny_writes_outside_repo = true
deny_mutations_in_paths = [".git/", ".codex/", ".agents/"]

history_max = 200

[printrevolt.hooks]
enabled = true
trust_repo_hooks = false
allowed_repo_roots = []
allowed_repo_origins = []      # optional: match git remote
timeout_ms = 2000
stdin_max_bytes = 262144
stdout_max_bytes = 262144
stderr_max_bytes = 262144
env_allowlist = ["PATH", "HOME", "USERPROFILE", "TEMP", "TMP", "SystemRoot", "COMSPEC", "CODEX_HOME"]
request_schema_version = "2"   # "1" | "2"

# Hook lists are explicit per event
on_session_start = []
on_session_end = []
before_task = []
on_turn_start = []
before_tool = ["~/.codex/pr-hooks/before-tool.sh"]
after_tool = []
before_finalize = ["~/.codex/pr-hooks/before-finalize.sh"]
on_turn_complete = []
on_turn_aborted = []
on_interrupt = []
after_task = []
on_item_started = []
on_item_completed = []

[printrevolt.pipelines]
enabled = true
default_pipeline_id = "default"
trust_repo_pipelines = false
allowed_repo_roots = []
allowed_repo_origins = []      # optional: match git remote

# Optional non-secret vars for templating and routing
[printrevolt.vars]
BASE_BRANCH = "main"
WORKTREE_ROOT = "~/.codex/printrevolt/worktrees"
BRANCH_NAMING = "prcc/${session_id}"

# Cleanup defaults (user-controlled teardown)
[printrevolt.cleanup]
mode = "prompt"          # "prompt" | "always" | "never" | "manual"
prompt_default = "deny"  # "deny" | "approve"
# Optional per-kind overrides (destructive kinds should default prompt/manual)
worktree = "manual"
branch = "manual"
service = "prompt"
db = "prompt"

# Optional command catalog (user-defined commands as data)
[printrevolt.commands]
enabled = true
trust_repo_commands = false

# Guardrails for pipeline execution volume and expensive facts
[printrevolt.pipelines.limits]
max_part_events_per_run = 500
max_changed_files = 5000
facts_cache_ttl_ms = 2000
max_agent_prompts_per_run = 1   # cap `request_agent_decision`/agent-interaction parts per pipeline run

# Loop guards
max_cycles = 3
max_attempts_per_part = 2
repeat_failure_signature_limit = 2

# Interaction behavior
auto_send_suggested_user_message = false

# Allowlisted command families (examples; still subject to policy floors)
allowed_verify_argv_prefixes = [
  ["npm","test"],
  ["npm","run","test"],
  ["pnpm","test"],
  ["pnpm","run","test"],
  ["yarn","test"],
  ["cargo","test"]
]

# Optional additional allowlists (default: empty => `run_command` in that category blocks)
allowed_lint_argv_prefixes = [
  ["npm","run","lint"],
  ["pnpm","run","lint"],
  ["cargo","fmt"]
]
allowed_build_argv_prefixes = [
  ["npm","run","build"],
  ["pnpm","run","build"],
  ["cargo","build"]
]
allowed_setup_argv_prefixes = []
allowed_git_argv_prefixes = [
  ["git","worktree"],
  ["git","checkout","-b"]
]

[printrevolt.templates]
prompt_after_submit = true
allow_draft_templates = true
draft_generation_mode = "heuristic" # "heuristic" | "model"
model_classify = false
suggest_threshold = 0.70
max_bytes = 262144
draft_save_default_scope = "user"    # "user" | "repo"

# Bounded repo context collector for draft generation
context_max_files = 200
context_max_bytes = 524288
context_time_budget_ms = 250
context_exclude_globs = [".git/**", ".env", "**/*.pem", "**/*.key", "**/*.p12", "**/*.pfx"]

[printrevolt.audit]
enabled = true
path = "~/.codex/printrevolt/audit/events.jsonl"
redaction_mode = "strict"      # "strict" | "balanced"
max_file_mb = 50
retain_days = 30

[printrevolt.updater]
enabled = true
check_interval_hours = 24
timeout_ms = 300
channel = "stable"             # "alpha" | "beta" | "stable"
source = "github"              # "npm" | "github"
skip_versions = []
auto_apply = false             # default off

[printrevolt.advisor]
enabled = false                # default off
mode = "heuristic"             # "heuristic" | "model"
allow_relaxations = false      # default: recommend tightening only
```

---

## 9) Updater + Upstream Sync Automation

### 9.1 Update detection and UX

**Version sources**
- `source="npm"`:
  - query npm registry for `@printrevolt/codex` latest dist-tag (fork distribution)
- `source="github"`:
  - query GitHub Releases for fork release tags

**Upstream baseline (npx Codex)**
- The fork baseline is the current “npx codex” distribution: npm `@openai/codex@latest` (source repo `openai/codex`).
- Upstream changes are handled via upstream sync PR automation (§9.3). Users do not “update upstream directly”; they update the fork distribution after the sync is merged and released.

**Non-blocking behavior**
- Update check is best-effort:
  - timeout `updater.timeout_ms` (default 300ms)
  - cached with `check_interval_hours` and persisted state
- If network unavailable or request fails:
  - do not block session start
  - emit `UpdateCheckFailed` audit event (redacted error)

**UX**
- When a newer version is detected and not skipped:
  - prompt: `Update now` / `Later` / `Skip this version`
- `Skip this version` persists in updater state.

**Rollback**
- For binary swap, keep prior binary as `.bak` and allow `--rollback-update` to swap back (optional).
- For npm, rollback is user-managed (install prior version).

### 9.2 Installation/update mechanisms

**Npm global update**
- Spawn `npm` as a subprocess:
  - Windows: `npm.cmd` resolution via PATH
  - Unix: `npm`
- Use structured argv:
  - `npm i -g @printrevolt/codex@<version>`
- Never run repo-local scripts; do not execute `npm` in repo dir (set cwd to safe dir).
- Capture output with truncation and audit.

**Binary swap**
- Download release artifact appropriate for OS/arch to a temp dir under:
  - `CODEX_HOME/printrevolt/updater/tmp/`
- Verify integrity:
  - checksum file (SHA256) from release assets, or signature verification if available
- Swap:
  - rename current binary -> `.bak`
  - rename new binary into place
  - ensure executable bit on Unix
- Never touch `CODEX_HOME` beyond PrintRevolt-owned updater directories.

**Preserving `CODEX_HOME`**
- Updater MUST NOT delete, overwrite, or migrate upstream `CODEX_HOME` contents.
- PrintRevolt-owned state lives under `CODEX_HOME/printrevolt/`.

### 9.3 Upstream sync PR automation

A GitHub Action (in fork repo) performs:

1. Detect the current upstream baseline:
   - resolve npm `@openai/codex@latest` to a concrete version
   - map to an upstream GitHub tag/release in `openai/codex` (preferred: `v<version>`)
   - if no matching tag exists, pin to the release commit and record provenance in the PR body
2. If new version:
   - create branch `sync/upstream-<version>`
   - merge/rebase upstream onto fork
3. Run CI:
   - unit tests + integration tests
   - cross-platform smoke tests (Linux + Windows)
   - diff-budget guard: fail if upstream files changed outside allowlist
4. Open PR with changelog summary and required reviewers.

**Conflict minimization**
- Keep PrintRevolt changes isolated to `crates/pr_*`.
- Use stable extension points.
- Enable `git rerere` in CI to learn repeated conflict resolutions (where feasible).

**Required CI checks**
- `cargo test --workspace`
- `cargo clippy --workspace -- -D warnings`
- `cargo fmt --check`
- E2E test suite on Linux + Windows
- Diff-budget allowlist check

---

## 10) Audit/Events/Logs (Observability)

### 10.1 Event model

All events share a common envelope:

```json
{
  "schema_version": "1",
  "ts_ms": 1739491200000,
  "session_id": "019aa3db-5685-79f1-a222-9ceb1bd22e94",
  "task_id": "task_01J8QZ2Y4X2H3S8Q8C0W7E1Q9B",
  "turn_id": "turn_01J8QZ2Y4X2H3S8Q8C0W7E1Q9C",
  "event_type": "PolicyBlocked",
  "payload": { }
}
```

`task_id` and `turn_id` are optional and are populated only when an event is scoped to a specific task/turn.

**Core event types**
- `SessionStarted`
- `SessionCwdReset`
- `TaskStarted`
- `TaskCompleted`
- `TaskAborted`
- `TaskDenied`
- `TurnStarted`
- `TurnCompleted`
- `TurnAborted`
- `InterruptReceived`
- `PipelineSelected`
- `PipelineStarted`
- `PipelinePartStarted`
- `PipelinePartCompleted`
- `PipelineBlocked`
- `PipelineCompleted`
- `PipelineClampedDisabled`
- `TemplateDiscovered`
- `TemplateSelected`
- `DraftTemplateGenerated`
- `DraftTemplateSaved`
- `PolicyEvaluated`
- `PolicyBlocked`
- `PolicyModified`
- `VerifyRunObserved`
- `VerifyEvidenceRecorded`
- `HookFired`
- `HookFailed`
- `HookContractViolation`
- `FinalizeAllowed`
- `FinalizeBlocked`
- `BreakGlassGranted`
- `BreakGlassUsed`
- `UpdateCheckStarted`
- `UpdateOffered`
- `UpdateSkipped`
- `UpdateApplied`
- `UpdateCheckFailed`
- `ConfigInvalid`
- `DoctorRun`
- `AdvisorRecommendationsGenerated`
- `AdvisorRecommendationsApplied`
- `AdvisorRecommendationsRejected`

### 10.2 Sinks

Default:
- File JSONL: `CODEX_HOME/printrevolt/audit/events.jsonl`

Optional:
- stdout sink for debugging (`audit.stdout=true`)
- SQLite sink (feature-flag) for enterprise/reporting environments.

### 10.3 Redaction policy

**Strict (default)**
- Do not log:
  - full environment variables
  - file contents
  - unbounded tool stdout/stderr
- Log only:
  - hashes (`sha256`) of stdout/stderr previews
  - truncated previews (default ≤ 4KB)
  - command argv with redaction of arguments matching secret patterns:
    - `(?i)key|token|secret|password|pwd`
- Any captured errors are truncated and scrubbed.

**Balanced**
- May log slightly larger previews (≤ 16KB) and include patch metadata for fs patch tools, still without file contents unless explicitly enabled.

### 10.4 Troubleshooting playbook

- `codex-pr doctor`:
  - prints effective config and trace
  - prints environment detection (Windows vs WSL vs Linux)
  - prints template discovery results
  - prints hook trust status
- If tools are blocked:
  - read `PolicyBlocked` events for reason code
  - follow remediation steps printed in UI
- If hooks fail:
  - read `HookFailed` events
  - run hook executable manually with saved stdin payload (optional debug mode that writes payload to disk under `CODEX_HOME/printrevolt/debug/` with 0600)

---

## 11) Failure Modes & Resilience

### 11.1 Policy subsystem

- **Git missing / repo state unavailable**
  - Impact: cannot compute branch or fingerprint
  - Behavior:
    - mutating tools denied (PrPolicyError) unless warn mode
    - read-only tools allowed
    - finalize denied if verify required
- **Repo fingerprint computation fails**
  - Behavior: treat verify evidence as stale; deny finalize/push

### 11.2 Hook subsystem

- **Hook executable not found**
  - Behavior: hook failure
    - mutating tools denied by default
    - audit `HookFailed`
- **Hook times out/hangs**
  - Behavior: terminate hook; handle as failure
- **Hook returns invalid JSON**
  - Behavior: failure; deny mutating tools
- **Repo hooks untrusted**
  - Behavior:
    - hooks/pipelines/commands remain **discoverable and configurable**
    - repo-provided entries run only via the normal policy+approval boundary
    - default posture in untrusted repos:
      - require explicit user approval per invocation (default choice = deny)
      - disallow “headless” execution unless the repo is explicitly trusted/allowlisted
    - if a repo hook is configured as `headless_only=true`, treat it as skipped in untrusted repos and emit `HookSkippedUntrusted`

### 11.3 Templates subsystem

- **No templates found**
  - Behavior: picker shows Default only; continue
- **Invalid template schema**
  - Behavior: ignore invalid template; emit warning event
- **Draft wizard generation fails**
  - Behavior: fallback to “continue without saving” with Default template; prompt preserved
- **Classifier fails / times out**
  - Behavior: no suggestion; Default preselected

### 11.4 Config subsystem

- **Invalid TOML**
  - Behavior: fall back to defaults; emit `ConfigInvalid`
- **Permission errors writing Mode B**
  - Behavior: run read-only; warn; do not block session
- **Mode A write enabled but toml_edit fails**
  - Behavior: deny write attempt; do not corrupt file; emit audit

### 11.5 Updater

- **Network unavailable**
  - Behavior: skip check, emit `UpdateCheckFailed`, do not block
- **Update verification fails (checksum/signature)**
  - Behavior: do not apply; emit `UpdateOffered` with failure reason and instructions
- **Npm missing**
  - Behavior: do not auto-apply; show instructions for manual install

### 11.6 Platform edge cases

- **WSL missing**
  - Behavior: on Windows-native, do not attempt WSL path translation; run PowerShell path logic only.
- **Path canonicalization errors**
  - Behavior: fail closed for filesystem mutation tools; deny with `PrPathEscape`
- **Windows junction/symlink traversal**
  - Behavior: if canonical path is outside repo root or canonicalization uncertain -> deny.

---

## 12) Security & Privacy

### 12.1 Sensitive data inventory

Potentially sensitive:
- repository source code
- environment variables (API keys)
- command outputs (may include secrets)
- template drafts and prompts (may include internal details)
- audit logs and evidence store.

### 12.2 Storage at rest

- Audit logs stored under `CODEX_HOME/printrevolt/` with permissions:
  - Unix: 0600 files, 0700 dirs
  - Windows: best-effort restricted ACL inheritance; document limitations
- Draft prompt persistence:
  - 0600 equivalent and TTL cleanup
- Evidence store:
  - only stores command string + timestamps + repo fingerprint; no stdout.

### 12.3 Command allow/deny safety model

- Default denylist for destructive commands is always active (`deny_dangerous_always=true`).
- Path containment is always enforced for fs tools.
- Unknown tools are treated as mutating unless classified read-only in config allowlist.

### 12.4 Supply chain risks and mitigations

- Updater uses pinned sources and verifies integrity.
- Hook engine supports user hooks and repo hooks. Repo-provided hooks/pipelines/commands are always discoverable/configurable.
- Trust allowlist controls whether repo-provided hooks/pipelines/commands can execute **headlessly**; in untrusted repos they can still run, but only with explicit approvals (default deny).

### 12.5 Approval/sandbox alignment

- PrintRevolt policy runs before upstream approval prompts.
- PrintRevolt does not disable upstream sandbox/approval controls.
- If upstream runs in sandbox mode, PrintRevolt still enforces its own floors.
 - PrintRevolt is compatible with upstream command gating mechanisms (e.g., approval presets and argv-prefix based exec rules). PrintRevolt adds additional enforcement; it does not weaken upstream gating.

**Child processes**
- Upstream approval prompts and prefix rules apply to the top-level tool call (canonical argv).
- If that command spawns child processes (e.g., `npm run ...`), codex-pr cannot rely on upstream approvals to gate each child exec individually.
- Mitigations:
  - prefer running verification commands inside upstream sandbox mode (and avoid approving “run outside sandbox” for verification)
  - optionally enable pipeline `child_process_policy` auditing/enforcement where supported (platform-specific; see §6.4.5 `run_command`)
  - treat disallowed/unknown child exec activity as verification failure that requires human review.

---

## 13) Performance Plan

### 13.1 Measurable targets

- Session start:
  - ≤150ms p95 added overhead (excluding model classification)
- Policy path:
  - ≤5ms p95 per tool call
- Hook execution:
  - ≤50ms p95 per hook process on local disk

### 13.2 Profiling strategy

- Add tracing spans:
  - `pr.session_init`
  - `pr.template_picker`
  - `pr.policy.before_tool`
  - `pr.hooks.exec`
  - `pr.policy.after_tool`
  - `pr.policy.before_finalize`
- Collect:
  - histograms for durations
  - counters for denies/allows/modifies by reason code
- Use:
  - `cargo flamegraph` on Linux
  - Windows ETW profiling for hook spawning hot paths
  - `tokio-console` for async task visibility (dev builds)

### 13.3 Memory and log retention strategy

- Audit event buffering bounded (single event serialized and appended).
- Rotation: max 50MB per file; retain 30 days.
- Tool stdout/stderr previews truncated; no full capture by default.

---

## 14) Testing & Quality Gates

### 14.1 Unit tests

- Policy:
  - dangerous command patterns (Unix + Windows)
  - verify evidence freshness and fingerprint invalidation
  - branch restrictions and scheme matching
  - path containment with symlinks/junctions (property tests)
- Config:
  - precedence resolution
  - Mode A round-trip preservation (unknown keys/comments)
  - Mode B atomic write and locking
- Hooks:
  - JSON contract parsing
  - failure/timeout semantics
  - deny-wins merge rules
- Templates:
  - discovery, validation, frontmatter parsing
  - draft generation invariants

### 14.2 Integration tests

- Tool dispatch interception:
  - ensure `before_tool` called for every tool kind
  - ensure denied tools do not execute
  - ensure modified tool calls are re-validated by policy clamp
- Hook runner:
  - spawn sample hook scripts for allow/deny/modify
  - timeout and IO caps enforced

### 14.3 E2E tests

- Template picker:
  - default selection
  - heuristic suggestion behavior
  - draft wizard continue without save
  - save to user scope and repo scope
  - cancel returns to prompt editor with prompt preserved
- Finalize gating:
  - cannot finalize without fresh verify when required
- Update UX:
  - skip version persistence
  - verify `CODEX_HOME` tree unchanged except PrintRevolt-owned dirs

### 14.4 Cross-platform tests

- CI matrix:
  - Linux (bash)
  - Windows (PowerShell)
  - WSL2 (nightly or dedicated environment)
- Adversarial path fixtures:
  - WSL `/mnt/c/...`
  - Windows junctions/symlinks
  - UNC paths (explicitly supported or explicitly blocked and tested)

### 14.5 Acceptance criteria + definition of done

Acceptance criteria are tracked with stable IDs so implementation tasks can reference them.

| ID | Area | Criteria | Verification |
|---|---|---|---|
| PR-AC-01 | Policy gate | A denied tool call never executes. | E2E: attempt blocked command; assert no side effects. |
| PR-AC-02 | Finalize gate | Finalize denials include stable code + remediation. | E2E: fail verify; assert denial includes code+remediation. |
| PR-AC-03 | Config | PrintRevolt writes only PrintRevolt-owned files and does not clobber unknown upstream config keys. | Integration test + golden fixture configs. |
| PR-AC-04 | Trust | The hook engine is always active, but repo hooks/pipelines/commands are disabled by default and require explicit trust/allowlist. | Unit tests for trust resolution; E2E in untrusted repo. |
| PR-AC-05 | Hooks | Hook runner enforces timeouts, IO caps, and deny-on-failure posture for gate events (per spec). | Unit tests with fixture hooks (timeout/invalid JSON). |
| PR-AC-06 | Pipelines | Pipeline actions always transit the same tool boundary (policy + hooks + approvals). | Integration: pipeline `run_command` emits ToolCall events and is clamped. |
| PR-AC-07 | Teardown | `finally_workflow` / `defer` cleanup runs on failure and is visible in audit (phase=cleanup); destructive cleanup is user-controlled by default (prompt default No) unless explicitly required by policy. | E2E: e2e start→fail→cleanup; assert cleanup phase events; deny optional teardown and confirm it is skipped, not reclassified as success. |
| PR-AC-08 | Vars/Facts | Vars are non-secret and merged by precedence; derived facts are bounded and ephemeral. | Unit tests for var precedence + extract/assert behavior. |
| PR-AC-09 | Command catalogs | `command_id` resolves to canonical argv and is displayed (“no hidden commands”). | Unit tests + trace snapshot in example pipeline. |
| PR-AC-10 | Child processes | Documented limitation: approvals gate top-level only; optional `child_process_policy` fails closed when unsupported. | Unit tests for config/compile; platform smoke where supported. |
| PR-AC-11 | Upstream diffs | Diff-budget CI guard enforces a small, configurable bound on upstream files touched outside `crates/pr_*` (default 10) and ignores `codex-rs/Cargo.lock` churn. | CI guard job. |
| PR-AC-12 | Git ops | `ensure_worktree` / `ensure_branch` are implemented in codex-pr, approval-gated, and clamped by protected branches + scheme; supervisors can extend by invoking the same primitives headlessly. | Integration test: start outside worktree on protected branch; deny → no changes; approve → worktree+branch created within configured root. |
| PR-AC-13 | Repo bootstrap | Upstream Codex repo (`openai/codex`, npm `@openai/codex`) is mirrored into a private repo; `upstream` remote is configured; the mirror is pinned to the current `@openai/codex@latest` release/tag; local checkout instructions (including sparse-checkout) are documented; upstream sync PR automation works. | Runbook test: fresh machine clone; verify pinned upstream ref matches npm `@openai/codex@latest`; run build; verify sync workflow opens a PR and preserves diff-budget constraints. |
| PR-AC-14 | Docs | User-facing documentation is complete and matches implemented behavior for policy/hooks/pipelines/templates/config/updater; includes copy/paste examples and remediation playbooks. | Docs review checklist + smoke run in a fresh repo; ensure every PR-AC-* has a corresponding “How to verify” section. |
| PR-AC-15 | Advisor | Recommendations are project-aware, bounded, and safe-by-default: they do not auto-apply; they cannot weaken floors unless explicitly enabled; every apply/deny is auditable. | E2E: generate recs; approve one; deny one; confirm applied config/pipeline diffs match; confirm no relaxations without allow_relaxations. |

---

## 15) Delivery Plan (Milestones & Tasks)

Milestones are aligned to a multi-engineer implementation approach with snapping interfaces.

### Source repo bootstrap (private mirror + local checkout)

This project begins by creating a **private** fork/mirror of the upstream Codex repository, then cloning it locally for development. Notes:
- GitHub “fork” is whole-repo (you cannot fork *only* the CLI subdirectory).
- For public upstream repos, GitHub forks are public; for a private development copy, use a **private mirror** repository (git push --mirror) and add the upstream as a remote.
- The fork baseline MUST be the current “npx codex” release: npm `@openai/codex@latest` (source repo `openai/codex`). Record the exact version and upstream ref in the mirror (tag/commit).
- If you only want the CLI code locally, you MAY use `git sparse-checkout` (or partial clone) to reduce working tree size while still tracking the full repo history for upstream merges.
- Avoid “extract only the CLI into a new repo” (subtree/filter-repo) unless you accept significantly harder upstream sync/merge workflows.

**Repo artifacts (kept in the fork)**
- Baseline pin: `printrevolt-upstream-baseline.json` (npm version, upstream tag, commit).
- Runbook: `PRINTREVOLT_UPSTREAM_MIRROR.md` (mirror + local checkout + sync).
- CI: `.github/workflows/printrevolt-upstream-sync.yml` and `.github/workflows/printrevolt-diff-budget.yml`.
- Extension points: `codex-rs/docs/EXTENSION_POINTS.md`.
- PrintRevolt crates: `codex-rs/crates/pr_*` (including `pr_runtime`, `pr_config`, and `pr_cli`).

### Milestones table

| Milestone | Owners | Scope (high-level) | Acceptance | Depends on |
|---|---|---|---|---|
| A | Platform/Runtime + CI | Repo bootstrap; upstream touchpoints; test harness; diff-budget guard. | Hook points exercised in integration tests. | — |
| B | Security/Platform | Policy floors + verify evidence + finalize gating + break-glass. | E2E finalize gate + blocked tool never executes. | A |
| C | Security/Platform | Hook runner v2 + trust allowlist + modify→revalidate. | Hook timeout/failure/modify covered by tests. | B |
| D | DevEx | Templates discovery/validation + picker + overlay clamp. | E2E: selecting template changes prompt prefix and overlay. | A |
| E | DevEx | Draft template wizard + bounded context collector. | Cancel preserves prompt; perf budgets met. | D |
| F | Release Eng / Platform | Updater + release + upstream sync automation. | Update is safe; sync PRs open automatically. | A |
| G | DevEx / Security / Platform | User docs + onboarding completeness. | PR-AC-14. | H |
| H | Platform/DevEx/Safety | Pipelines v1 + vars/facts + commands catalogs + teardown + git primitives. | PR-AC-06,07,08,09,10,12. | A,B,C |

### Milestone A — Extension Points + Test Harness
**Owners:** Platform/Runtime + CI
- Implement `pr_runtime` and upstream touchpoints (session init, tool dispatch, finalize, prompt submit).
- Add diff-budget guard + `docs/EXTENSION_POINTS.md`.
- Acceptance:
  - interception invoked in integration tests on Linux + Windows

### Milestone B — Policy Engine v1
**Owners:** Security/Platform
- Implement `pr_policy` rules:
  - dangerous commands
  - branch + scheme
  - verify evidence + staleness
  - path containment for fs tools
- Implement break-glass one-off
- Acceptance:
  - E2E finalize gate + blocked tool never executes

### Milestone C — Hook Bus + Contract + Trust Model
**Owners:** Security/Platform
- Implement `pr_hooks` runner and HookRequest/HookResponse JSON contract v2 (retain v1 compatibility during transition as needed).
- Repo-provided hooks disabled by default; trust allowlist.
- Acceptance:
  - tests for timeout/failure/modify+revalidate

### Milestone D — Templates v1 + Picker
**Owners:** DevEx
- Implement `pr_templates` discovery + validation + picker UI adapter.
- Metadata->policy overlay and clamp.
- Acceptance:
  - E2E: selecting template changes prompt prefix and policy overlay

### Milestone E — Draft Template Wizard
**Owners:** DevEx
- Implement wizard flow with prompt persistence and bounded repo context collector.
- Acceptance:
  - cancel preserves prompt; discard requires confirmation; perf budgets met

### Milestone F — Updater + Release + Upstream Sync Automation
**Owners:** Release Eng / Platform
- Implement updater state + check prompt.
- Add GitHub Action for upstream sync PRs.
- Acceptance:
  - update never clobbers CODEX_HOME; sync PRs open automatically.

### Milestone G — User Docs + Onboarding
**Owners:** DevEx / Security / Platform
- Author user-facing docs for all PrintRevolt features (hooks/policy/templates/updater/audit/doctor).
- Add “Getting Started” and “Common workflows” with copy/paste examples.
- Add a “What’s new vs upstream Codex CLI” page (keep it non-technical and expectation-setting).
- Acceptance:
  - docs are sufficient for a new user to install, configure, and understand why actions are blocked (with remediation)
  - docs include at least one complete example for each hook lifecycle group: task/turn/tool/finalize

### Milestone H — Pipelines v1 + Vars/Facts + Command Catalogs
**Owners:** Platform/DevEx/Safety
- Implement `crates/pr_pipelines` execution engine and integration at lifecycle gates.
- Implement vars (`[printrevolt.vars]`) + session overlay ingestion (`PRINTREVOLT_SESSION_CONFIG`) and safe templating.
- Implement `commands.json` catalogs + `run_command.command_id` resolution with trust/clamping.
- Implement derived facts (`extract_value`, `assert_fact`) and ensure boundedness + redaction.
- Implement teardown (`finally_workflow` + `defer`) and audit `phase="cleanup"`.
- Implement `require_approval(mode="confirm")` for non-fatal prompts (optional teardown, setup confirmations).
- Implement Git primitives (`ensure_worktree`, `ensure_branch`) as typed engine features (not ad-hoc shell).
- Acceptance:
  - PR-AC-06, PR-AC-07, PR-AC-08, PR-AC-09, PR-AC-10, PR-AC-12

### Dependency graph

- A is prerequisite for B/C/D/F (integration points).
- D is prerequisite for E (draft wizard).
- B is prerequisite for C (policy clamp after modify).
- A/B/C are prerequisites for H (pipeline actions must transit tool boundary + hooks + policy).
- H is prerequisite for G docs completeness (pipelines/vars/commands must be documented).

### Owner breakdown

- Policy: Security/Platform
- Hooks: Security/Platform
- Templates: DevEx
- Config: Platform
- Updater + Sync: Release/Platform
- Pipelines/Vars/Commands: Platform/DevEx/Safety

### Implementation task table (priority + dependencies)

Status values: `not_started` \| `in_progress` \| `blocked` \| `done`.

| Task ID | Milestone | Description | Priority | Depends on | Status | Acceptance |
|---|---|---|---:|---|---|---|
| PR-TASK-000 | A | Create private upstream mirror (public forks are public on GitHub) + configure `upstream` remote + branch protections; document local clone (sparse-checkout optional); add upstream sync PR automation. | 0 | — | done | PR-AC-13 |
| PR-TASK-001 | A | Wire upstream lifecycle touchpoints (`on_session_start`/`before_task`/`before_finalize`/`on_session_end`). | 0 | PR-TASK-000 | done | PR-AC-01,02 |
| PR-TASK-002 | A | Add diff-budget guard + CI job + `docs/EXTENSION_POINTS.md` (enforce “touch upstream minimally” contract). | 1 | PR-TASK-001 | done | PR-AC-11 |
| PR-TASK-003 | A | Add integration/E2E harness for interception across Linux/Windows/WSL smoke (tool dispatch, finalize gate, hook bus). | 1 | PR-TASK-001 | done | PR-AC-01,02,05 |
| PR-TASK-004 | A | Implement config system (Mode A/B precedence, no-clobber writes/locking) + `codex-pr doctor` config trace output. | 1 | PR-TASK-001 | done | PR-AC-03 |
| PR-TASK-005 | A | Implement audit/events plumbing: JSONL writer, rotation/retention, redaction guarantees, and `PRINTREVOLT_AUDIT_STDOUT` streaming. | 1 | PR-TASK-001 | done | PR-AC-01,03 |
| PR-TASK-010 | B | Implement policy floors + verify evidence model + finalize gating. | 0 | PR-TASK-001 | done | PR-AC-01,02 |
| PR-TASK-020 | C | Implement hook runner v2 + trust allowlist + modify→revalidate. | 1 | PR-TASK-010 | done | PR-AC-04,05 |
| PR-TASK-030 | H | Implement pipeline compiler/executor skeleton (`PipelineRunState`, transitions, loop guards). | 1 | PR-TASK-020 | done | PR-AC-06 |
| PR-TASK-031 | H | Implement teardown (`finally_workflow` + `defer`) and `phase="cleanup"` events. | 1 | PR-TASK-030 | done | PR-AC-07 |
| PR-TASK-032 | H | Implement vars (`[printrevolt.vars]`) + session overlay (`PRINTREVOLT_SESSION_CONFIG`) + templating. | 1 | PR-TASK-030,004 | done | PR-AC-03,08 |
| PR-TASK-033 | H | Implement `commands.json` catalogs + `command_id` resolution with trust/clamping. | 2 | PR-TASK-032 | done | PR-AC-04,09 |
| PR-TASK-034 | H | Implement `extract_value` + `assert_fact` bounded parsing and predicates. | 2 | PR-TASK-030 | done | PR-AC-08 |
| PR-TASK-035 | H | Implement `child_process_policy` plumbing (default inherit; fail-closed on unsupported). | 3 | PR-TASK-030 | done | PR-AC-10 |
| PR-TASK-036 | H | Implement `require_approval(mode=\"confirm\")` (writes derived bool; denial continues) + cleanup UX semantics. | 2 | PR-TASK-030 | done | PR-AC-07 |
| PR-TASK-037 | H | Implement `ensure_worktree` typed Git primitive (root containment + lock + approval gating). | 2 | PR-TASK-032 | done | PR-AC-12 |
| PR-TASK-038 | H | Implement `ensure_branch` typed Git primitive (protected branches + scheme + approval gating). | 2 | PR-TASK-010 | done | PR-AC-12 |
| PR-TASK-039 | H | Add headless repo-ops surface (CLI subcommands and/or CEP methods) for `ensure_worktree`/`ensure_branch`/optional remove-worktree so supervisors can reuse identical policy/approvals. | 3 | PR-TASK-037,038 | done | PR-AC-12 |
| PR-TASK-040 | D | Templates v1: discovery/validation + picker UI adapter + metadata→policy overlay clamp. | 3 | PR-TASK-001 | done | PR-AC-03 |
| PR-TASK-041 | E | Draft template wizard: prompt persistence + bounded context collector + save/cancel semantics. | 3 | PR-TASK-040 | done | PR-AC-03 |
| PR-TASK-042 | F | Updater v1: version check UI + safe apply/rollback posture + “no clobber” guarantees. | 3 | PR-TASK-002 | done | PR-AC-03 |
| PR-TASK-043 | F | Upstream sync automation: scheduled workflow opens sync PRs; ensures diff-budget + runs test suite. | 3 | PR-TASK-000,002,003 | done | PR-AC-11,13 |
| PR-TASK-044 | H | Implement advisor/recommendations: bounded project scan + `RecommendationBundle` + `doctor --recommend` output; optional apply behind approvals. | 3 | PR-TASK-004,030,033 | done | PR-AC-15 |
| PR-TASK-050 | G | Write/refresh user docs for policy/hooks/pipelines/templates/config/updater/audit/doctor + examples. | 4 | PR-TASK-010,020,031,033,041,042 | done | PR-AC-14 |
| PR-TASK-999 | ALL | Final verification: run full E2E matrix and confirm all PR-AC-* satisfied; produce sign-off checklist. | 5 | PR-TASK-010,020,031,033,037,038,041,042,043,044,050 | done | PR-AC-01..15 |
| PR-TASK-1000 | G | Local machine install + smoke test runbook: build/install the fork, validate tool interception + finalize gating + audit, and record results. (See `codex/PRINTREVOLT_LOCAL_MACHINE.md`.) | 5 | PR-TASK-999 | todo | PR-AC-14 |
| PR-TASK-1001 | ALL | Conditional rollback: if the fork is not working properly on the target machine, remove forked binaries/config and fall back to upstream Codex CLI via `npx`. If the fork works, mark this task as **done (N/A)**. (See `codex/PRINTREVOLT_LOCAL_MACHINE.md`.) | 5 | PR-TASK-1000 | conditional | PR-AC-14 |

### Open questions to resolve before implementation

- Primary Windows distribution: npm vs signed binaries (affects updater apply logic).
- Break-glass: allow session override by default, or only one-off?
- Should `cmd.exe` be tier-2 supported (parsing complexity)?
- Whether to support sending any data to model classifier in enterprise mode (default off).

---

## 16) Plan Verification & Deltas

### 16.1 Alignment verification

**Plan sources**
- This repository/workspace does not currently include a separate plan document (e.g., `*Plan*.md`) for the CLI fork.
- As a result, the LLD itself is the canonical reference for milestones/tasks/acceptance criteria in this workspace.

**What this section guarantees**
- Internal traceability exists: major feature areas in the LLD map to:
  - acceptance criteria (`PR-AC-*`), and
  - implementation tasks (`PR-TASK-*`) with explicit dependencies and a final verification task (`PR-TASK-999`).

If a plan document is added later, reconcile by:
- ensuring each plan requirement maps to at least one acceptance criterion ID, and
- ensuring each acceptance criterion is satisfied by one or more tasks and a verification procedure.

### 16.1.1 Sign-off checklist (PR-TASK-999)

This checklist is the concrete verification artifact. Each item should be checked on the target platform(s) (Linux, macOS, Windows/WSL as applicable).

Local install / rollback:
1. Follow the local install + smoke test checklist in `codex/PRINTREVOLT_LOCAL_MACHINE.md` and record results.
2. Rollback (conditional): if the fork is not working properly, execute the rollback steps in `codex/PRINTREVOLT_LOCAL_MACHINE.md`. If the fork works, mark rollback as N/A.

Policy / hooks / gating:
1. Dangerous command floors block `rm -rf` patterns even if approvals would allow.
2. Hook `modify` is revalidated by policy (modify -> revalidate).
3. Finalize gate blocks `TurnComplete` when `policy.verify.required=true` and evidence missing/stale/failed.

Audit:
1. JSONL events written under `CODEX_HOME/printrevolt/audit/` with rotation.
2. Redaction patterns replace matched values with `[REDACTED]`.
3. `PRINTREVOLT_AUDIT_STDOUT=1` mirrors events to stdout for supervisors.

Config:
1. Mode A + Mode B merge order is deterministic.
2. `PRINTREVOLT_SESSION_CONFIG` overlay merges with correct precedence.

Pipelines:
1. `finally_workflow` executes on exit.
2. `defer` cleanup stack runs LIFO.
3. `require_approval(mode="confirm")` writes derived bool and continues on deny (default deny).
4. `extract_value` and `assert_fact` are bounded and typed.
5. `child_process_policy` fails closed when unsupported.
6. `ensure_worktree` / `ensure_branch` parts default to approval-gated git ops.

Templates:
1. Frontmatter and required headings are validated; invalid templates are ignored with warnings.
2. Repo templates load only when repo is trusted/allowlisted.
3. Adapter commands work: `codex-pr templates list/validate/draft`.

Repo ops:
1. `codex-pr repo ensure-worktree/ensure-branch/remove-worktree` print a plan with action hash; execution only with `--apply`.
2. Worktree operations are contained under worktree_root and serialized via lock file.

Updater:
1. `codex-pr update check` works with `--latest` without network.
2. `codex-pr update plan` prints a deterministic plan with action hash; execution only with `--apply`.

CI:
1. `printrevolt-upstream-sync` workflow runs scheduled, enforces diff budget, runs targeted tests, and opens a PR.

### 16.2 Added

- `crates/pr_types` and explicit shared schemas for hooks/events/tool calls (not explicitly listed in the plan, but required to keep crate boundaries clean and independently implementable).
- Explicit monotonic clamping rules for template overlay → policy config linkage.
- Explicit IO caps and env allowlist defaults for hook runner.
- Explicit event envelope schema and default retention/rotation behavior.

### 16.3 Changed

- Policy evaluation is specified as **pure** (no git/process execution). Repo state is refreshed in `pr_runtime` and passed into policy via `SessionContext.repo_state` to meet performance budgets.

### 16.4 Removed

- None relative to the available plan.

### 16.5 Conflicts resolved (with rationale)

- None detected within the single available plan document. If the missing UPDATED plan introduces conflicting requirements, reconcile by:
  - preserving the “guaranteed enforcement boundary” definition
  - keeping upstream diffs bounded (diff-budget CI)
  - preserving no-clobber config guarantees as a P0 constraint.
