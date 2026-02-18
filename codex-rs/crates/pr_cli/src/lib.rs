use std::collections::BTreeMap;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use codex_pr_config::ConfigResolveScope;
use codex_pr_config::flatten_printrevolt;
use codex_pr_config::resolve_printrevolt_config;
use codex_pr_config::resolve_printrevolt_config_scoped;
use codex_pr_config::write_mode_b_printrevolt_toml_atomic;
use codex_pr_pipelines::ComponentV2;
use codex_pr_pipelines::ExpandLimits;
use codex_pr_pipelines::Pipeline;
use codex_pr_pipelines::PipelineEntryV2;
use codex_pr_pipelines::expand_pipeline;
use codex_pr_profiles::LoadedProfilesRegistry;
use codex_pr_profiles::ProfileScope;
use codex_pr_profiles::ProfilesResolverCache;
use codex_pr_profiles::benchmark_resolver_warm_path;
use codex_pr_profiles::load_profiles_registry;
use codex_pr_profiles::resolve_subject_profiles;
use codex_pr_profiles::resolve_subject_profiles_cached;
use codex_pr_repo_ops::BranchEnsureArgs;
use codex_pr_repo_ops::RepoOpPlan;
use codex_pr_repo_ops::WorktreeEnsureArgs;
use codex_pr_runtime::WorkflowAgentInvokeRequest;
use codex_pr_runtime::invoke_workflow_agent;
use codex_pr_types::GuidelineProfilesFileV1;
use codex_pr_types::PolicyProfilesFileV1;
use codex_pr_types::PrintRevoltConfig;
use codex_pr_types::ProfileRefs;
use codex_pr_types::WorkflowComponentV1;
use codex_pr_types::WorkflowEntryV1;
use codex_pr_types::WorkflowGraphV1;
use codex_pr_types::WorkflowStepV1;
use codex_pr_updater::UpdateChannel;
use codex_pr_updater::UpdateCheckRequest;
use codex_pr_workflows::ArtifactStore;
use codex_pr_workflows::ArtifactStoreConfig;
use codex_pr_workflows::WorkflowAction;
use codex_pr_workflows::WorkflowActionResult;
use codex_pr_workflows::WorkflowEngine;
use codex_pr_workflows::WorkflowEngineLimits;
use codex_pr_workflows::WorkflowRunState;
use codex_pr_workflows::WorkflowRunStatus;
use codex_pr_workflows::action_result_from_json;
use codex_pr_workflows::action_to_json;
use codex_pr_workflows::read_run_state;
use codex_pr_workflows::read_workflows_file as read_workflows_file_core;
use codex_pr_workflows::write_run_state;
use codex_pr_workflows::write_workflows_file as write_workflows_file_core;
use codex_utils_home_dir::find_codex_home;
use sha2::Digest as _;
use sha2::Sha256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Scope {
    Global,
    Project,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum LayerScope {
    Global,
    Project,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Both => "both",
        }
    }
}

impl LayerScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "codex-pr")]
#[command(about = "PrintRevolt extensions for Codex CLI")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print PrintRevolt config resolution and provenance.
    Doctor {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,

        /// Emit a RecommendationBundle based on a bounded project scan.
        #[arg(long)]
        recommend: bool,
    },

    /// Headless repo operations that reuse codex-pr safety constraints.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },

    /// Template discovery, validation, and draft generation helpers.
    Templates {
        #[command(subcommand)]
        command: TemplatesCommand,
    },

    /// Policy introspection and explainability helpers.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },

    /// Profile registry, resolution, and attachment helpers.
    Profiles {
        #[command(subcommand)]
        command: ProfilesCommand,
    },

    /// Pipeline discovery and generation helpers.
    Pipelines {
        #[command(subcommand)]
        command: PipelinesCommand,
    },

    /// Workflow discovery and generation helpers.
    Workflows {
        #[command(subcommand)]
        command: WorkflowsCommand,
    },

    /// Backup inventory and restore helpers.
    Backups {
        #[command(subcommand)]
        command: BackupsCommand,
    },

    /// Update helper (plan/check/apply). Defaults to npm distribution.
    Update {
        #[command(subcommand)]
        command: UpdateCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Plan or apply creation of an isolated git worktree under a root.
    EnsureWorktree {
        /// Override repo root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        repo_root: Option<PathBuf>,

        /// Root directory under which worktrees may be created.
        #[arg(long)]
        worktree_root: PathBuf,

        /// Relative naming path under worktree_root (no absolute paths or "..").
        #[arg(long)]
        naming: String,

        /// Branch name to create for the worktree.
        #[arg(long)]
        branch_name: String,

        /// Base branch/ref to base the worktree branch from (default: main).
        #[arg(long, default_value = "main")]
        base_branch: String,

        /// Apply the plan (otherwise prints JSON plan and exits 0).
        #[arg(long)]
        apply: bool,
    },

    /// Plan or apply creation/switch to a non-protected branch.
    EnsureBranch {
        /// Override repo root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        repo_root: Option<PathBuf>,

        /// Base branch/ref to branch from (default: main).
        #[arg(long, default_value = "main")]
        base_branch: String,

        /// Branch name to create/switch to.
        #[arg(long)]
        branch_name: String,

        /// Protected branches (repeatable).
        #[arg(long)]
        protected: Vec<String>,

        /// Apply the plan (otherwise prints JSON plan and exits 0).
        #[arg(long)]
        apply: bool,
    },

    /// Plan or apply removal of a worktree under the worktree root.
    RemoveWorktree {
        /// Override repo root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        repo_root: Option<PathBuf>,

        /// Root directory under which worktrees may be removed.
        #[arg(long)]
        worktree_root: PathBuf,

        /// Path to remove (absolute or relative to worktree_root).
        #[arg(long)]
        path: PathBuf,

        /// Force removal (passes --force to git worktree remove).
        #[arg(long)]
        force: bool,

        /// Apply the plan (otherwise prints JSON plan and exits 0).
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TemplatesCommand {
    /// List discovered templates (repo templates require trust allowlist).
    List {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config/template scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Validate all discovered templates; prints warnings and exits 0.
    Validate {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config/template scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,
    },

    /// Enable template picker behavior (selection_mode=once).
    Enable {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config write scope.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,
    },

    /// Disable template picker behavior (selection_mode=off).
    Disable {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config write scope.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,
    },

    /// Create a template draft from a prompt.
    Draft {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Write scope for the draft template.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Draft file name (without extension).
        #[arg(long)]
        id: String,

        /// Template name.
        #[arg(long)]
        name: String,

        /// Template description.
        #[arg(long)]
        description: String,

        /// Tags (repeatable).
        #[arg(long)]
        tag: Vec<String>,

        /// Prompt text to embed (bounded).
        #[arg(long)]
        prompt: String,
    },

    /// Compose the generated prompt for a template + raw prompt (no side effects).
    ComposePrompt {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config/template scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Template id (from `codex-pr templates list --json`).
        #[arg(long)]
        template_id: String,

        /// Raw prompt text to wrap with the template contract.
        #[arg(long)]
        prompt: String,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PolicyCommand {
    /// Print effective policy configuration (derived from resolved PrintRevolt config).
    Status {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Policy config scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Explain the most recent non-allow policy decision from the audit log (best-effort).
    Explain {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Enable policy enforcement.
    Enable {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config write scope.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,
    },

    /// Disable policy enforcement.
    Disable {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config write scope.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProfileKind {
    Policy,
    Guideline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProfileMode {
    Merge,
    Replace,
}

#[derive(Debug, Subcommand)]
enum ProfilesCommand {
    /// List profile ids in the effective global/project registry view.
    List {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Registry scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show one profile definition by id.
    Show {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Registry scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Profile kind.
        #[arg(long, value_enum)]
        kind: ProfileKind,

        /// Profile id.
        #[arg(long)]
        id: String,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Validate registry files and include graphs.
    Validate {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Registry scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Draft a starter profile into policy_profiles.json or guideline_profiles.json.
    Draft {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Write scope for registry updates.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Profile kind.
        #[arg(long, value_enum)]
        kind: ProfileKind,

        /// Profile id to create.
        #[arg(long)]
        id: String,

        /// Optional description.
        #[arg(long)]
        description: Option<String>,

        /// Apply by writing to the registry file (otherwise preview JSON).
        #[arg(long)]
        apply: bool,
    },

    /// Resolve effective policy/guidelines from refs + registry + floor.
    Resolve {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Registry/config scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Policy profile ids (repeatable).
        #[arg(long = "policy-profile")]
        policy_profiles: Vec<String>,

        /// Guideline profile ids (repeatable).
        #[arg(long = "guideline-profile")]
        guideline_profiles: Vec<String>,

        /// Use cache + print warm-path perf stats.
        #[arg(long)]
        perf: bool,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Explain resolved profile provenance and warnings in operator-friendly text.
    Explain {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Registry/config scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Policy profile ids (repeatable).
        #[arg(long = "policy-profile")]
        policy_profiles: Vec<String>,

        /// Guideline profile ids (repeatable).
        #[arg(long = "guideline-profile")]
        guideline_profiles: Vec<String>,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Attach a profile to a template/pipeline/workflow subject.
    Attach {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Write scope for attachment mutation.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Subject key.
        /// template:<template_id> |
        /// pipeline:<pipeline_id> |
        /// pipeline_workflow:<pipeline_id>.<workflow_id> |
        /// pipeline_part:<pipeline_id>.<workflow_id>.<part_index> |
        /// pipeline_component:<component_id> |
        /// workflow:<workflow_id> |
        /// workflow_step:<workflow_id>.<step_id> |
        /// workflow_component:<component_id>
        #[arg(long)]
        subject: String,

        /// Profile kind.
        #[arg(long, value_enum)]
        kind: ProfileKind,

        /// Profile id to attach.
        #[arg(long)]
        profile_id: String,

        /// Merge appends if missing; replace overwrites the subject list for this kind.
        #[arg(long, value_enum, default_value = "merge")]
        mode: ProfileMode,
    },

    /// Detach a profile from a template/pipeline/workflow subject.
    Detach {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Write scope for attachment mutation.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Subject key (same format as `attach --subject`).
        #[arg(long)]
        subject: String,

        /// Profile kind.
        #[arg(long, value_enum)]
        kind: ProfileKind,

        /// Profile id to detach.
        #[arg(long)]
        profile_id: String,
    },

    /// Interactive helper for command-center/TUI style profile flows.
    Interactive {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Registry/config scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,
    },
}

#[derive(Debug, Subcommand)]
enum PipelinesCommand {
    /// List pipelines from global/project pipeline bundles.
    List {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Pipeline bundle scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show a pipeline entry by id from global/project pipeline bundles.
    Show {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Pipeline bundle scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Pipeline id.
        #[arg(long)]
        id: String,

        /// Expand components into concrete parts (no execution; validation only).
        #[arg(long)]
        expanded: bool,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Enable pipeline functionality.
    Enable {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config write scope.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,
    },

    /// Disable pipeline functionality.
    Disable {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config write scope.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,
    },

    /// Generate a starter verify pipeline (preview-only unless --apply is set).
    Draft {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Write scope for pipelines bundle updates.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Pipeline id to create (default: verify).
        #[arg(long, default_value = "verify")]
        id: String,

        /// Pipeline name (default: Verify).
        #[arg(long, default_value = "Verify")]
        name: String,

        /// Apply by writing to the selected pipelines.json (with backup).
        #[arg(long)]
        apply: bool,
    },

    /// Restore pipelines.json from a backup snapshot.
    Restore {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Restore scope target.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Backup file path (from `codex-pr backups list --kind pipelines`).
        #[arg(long)]
        backup_path: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum WorkflowsCommand {
    /// List workflows from global/project workflow bundles.
    List {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Workflow bundle scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show a workflow entry by id from global/project workflow bundles.
    Show {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Workflow bundle scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Workflow id.
        #[arg(long)]
        id: String,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Enable workflow functionality.
    Enable {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config write scope.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,
    },

    /// Disable workflow functionality.
    Disable {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Config write scope.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,
    },

    /// Generate a starter product workflow (preview-only unless --apply is set).
    Draft {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Write scope for workflows bundle updates.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Workflow id to create (default: product_flow).
        #[arg(long, default_value = "product_flow")]
        id: String,

        /// Workflow name (default: Product Workflow).
        #[arg(long, default_value = "Product Workflow")]
        name: String,

        /// Apply by writing to the selected workflows.json (with backup).
        #[arg(long)]
        apply: bool,
    },

    /// Run or resume a workflow and drive review gates interactively.
    Run {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Workflow bundle scope.
        #[arg(long, value_enum, default_value = "both")]
        scope: Scope,

        /// Workflow id.
        #[arg(long)]
        id: String,

        /// Run id to resume (if omitted, a new run id is generated unless --resume is set).
        #[arg(long)]
        run_id: Option<String>,

        /// Resume the latest run state for this workflow id.
        #[arg(long)]
        resume: bool,

        /// Initial user prompt used to compose generation prompts.
        #[arg(long)]
        prompt: Option<String>,

        /// Optional model override for `codex exec` invocations.
        #[arg(long)]
        model: Option<String>,

        /// Emit one pending action as JSON and exit (supervisor mode).
        #[arg(long)]
        emit_actions_json: bool,

        /// Apply an action result JSON blob to the pending action before continuing.
        #[arg(long)]
        action_result_json: Option<String>,

        /// Loop guard for command-side execution.
        #[arg(long, default_value_t = 256)]
        max_steps: u32,
    },

    /// Show the state of the latest (or selected) workflow run.
    Status {
        /// Run id to inspect (defaults to latest run state file).
        #[arg(long)]
        run_id: Option<String>,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Cancel a workflow run by marking it failed and clearing pending action.
    Cancel {
        /// Run id to cancel (defaults to latest run state file).
        #[arg(long)]
        run_id: Option<String>,
    },

    /// Restore workflows.json from a backup snapshot.
    Restore {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Restore scope target.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Backup file path (from `codex-pr backups list --kind workflows`).
        #[arg(long)]
        backup_path: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum BackupsCommand {
    /// List backups under CODEX_HOME/printrevolt/backups.
    List {
        /// Filter to a backup kind directory (e.g. config, restore, pipelines, template).
        #[arg(long)]
        kind: Option<String>,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Restore a PrintRevolt-owned file from a backup snapshot (best-effort).
    Restore {
        /// Restore target (printrevolt-toml or pipelines-json or workflows-json).
        #[arg(long)]
        target: String,

        /// Restore scope target.
        #[arg(long, value_enum, default_value = "global")]
        scope: Scope,

        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,

        /// Backup file path.
        #[arg(long)]
        backup_path: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum UpdateCommand {
    /// Check if an update is available (networked unless --latest is provided).
    Check {
        /// Package name to check (default: codex).
        #[arg(long, default_value = "codex")]
        package: String,

        /// Current version (default: workspace version).
        #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
        current: String,

        /// Latest version override (no network).
        #[arg(long)]
        latest: Option<String>,

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Print an update plan and optionally apply it.
    Plan {
        /// Package name (default: codex).
        #[arg(long, default_value = "codex")]
        package: String,

        /// Current version (default: workspace version).
        #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
        current: String,

        /// Target version (required).
        #[arg(long)]
        target: String,

        /// Apply the plan.
        #[arg(long)]
        apply: bool,
    },
}

fn discover_project_root(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path);
    }
    let mut cur = std::env::current_dir().ok()?;
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn require_project_root(project_root: Option<PathBuf>, scope: Scope) -> Result<Option<PathBuf>> {
    if scope == Scope::Project && project_root.is_none() {
        anyhow::bail!("project scope selected but no project root detected; pass --project-root");
    }
    Ok(project_root)
}

fn ensure_writable_scope(scope: Scope, flag_name: &str) -> Result<LayerScope> {
    match scope {
        Scope::Global => Ok(LayerScope::Global),
        Scope::Project => Ok(LayerScope::Project),
        Scope::Both => {
            anyhow::bail!("--scope both is not valid for write operations ({flag_name})")
        }
    }
}

fn to_config_scope(scope: Scope) -> ConfigResolveScope {
    match scope {
        Scope::Global => ConfigResolveScope::Global,
        Scope::Project => ConfigResolveScope::Project,
        Scope::Both => ConfigResolveScope::Both,
    }
}

fn repo_trusted(project_root: Option<&Path>, codex_home: &Path) -> bool {
    let Some(project_root) = project_root else {
        return false;
    };
    let resolved = resolve_printrevolt_config(codex_home, Some(project_root), None);
    let cfg_toml = toml::to_string(&resolved.printrevolt).unwrap_or_default();
    let cfg: codex_pr_types::PrintRevoltConfig = toml::from_str(&cfg_toml).unwrap_or_default();
    let root = project_root.to_string_lossy().to_string();
    cfg.hooks.trusted_repo_roots.iter().any(|t| t == &root)
}

fn to_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct PipelinesFileV1 {
    schema_version: String,
    #[serde(default)]
    pipelines: BTreeMap<String, PipelineEntryV1>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PipelineEntryV1 {
    id: String,
    name: String,
    #[serde(default = "default_true")]
    enabled: bool,
    pipeline: Pipeline,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct PipelinesFileV2 {
    schema_version: String,
    #[serde(default)]
    components: BTreeMap<String, ComponentV2>,
    #[serde(default)]
    pipelines: BTreeMap<String, PipelineEntryV2>,
    #[serde(default)]
    profile_attachments: codex_pr_pipelines::PipelineProfileAttachmentsV1,
}

#[derive(Debug, Clone)]
enum PipelinesFile {
    V1(PipelinesFileV1),
    V2(PipelinesFileV2),
}

#[derive(Debug, Clone, serde::Serialize)]
struct PipelineResolvedEntryV1 {
    id: String,
    name: String,
    enabled: bool,
    profile_refs: ProfileRefs,
    source: LayerScope,
    pipeline: Pipeline,
}

#[derive(Debug, Clone, serde::Serialize)]
struct WorkflowResolvedEntryV1 {
    id: String,
    name: String,
    enabled: bool,
    profile_refs: ProfileRefs,
    source: LayerScope,
    workflow: WorkflowGraphV1,
}

fn default_true() -> bool {
    true
}

fn global_pipelines_json_path(codex_home: &Path) -> PathBuf {
    codex_home.join("printrevolt").join("pipelines.json")
}

fn project_pipelines_json_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".codex")
        .join("printrevolt")
        .join("pipelines.json")
}

fn global_workflows_json_path(codex_home: &Path) -> PathBuf {
    codex_home.join("printrevolt").join("workflows.json")
}

fn project_workflows_json_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".codex")
        .join("printrevolt")
        .join("workflows.json")
}

fn scoped_pipelines_json_path(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: LayerScope,
) -> Result<PathBuf> {
    match scope {
        LayerScope::Global => Ok(global_pipelines_json_path(codex_home)),
        LayerScope::Project => {
            let Some(project_root) = project_root else {
                anyhow::bail!(
                    "project scope selected but no project root detected; pass --project-root"
                );
            };
            Ok(project_pipelines_json_path(project_root))
        }
    }
}

fn scoped_workflows_json_path(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: LayerScope,
) -> Result<PathBuf> {
    match scope {
        LayerScope::Global => Ok(global_workflows_json_path(codex_home)),
        LayerScope::Project => {
            let Some(project_root) = project_root else {
                anyhow::bail!(
                    "project scope selected but no project root detected; pass --project-root"
                );
            };
            Ok(project_workflows_json_path(project_root))
        }
    }
}

fn scoped_printrevolt_toml_path(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: LayerScope,
) -> Result<PathBuf> {
    match scope {
        LayerScope::Global => Ok(codex_home.join("printrevolt.toml")),
        LayerScope::Project => {
            let Some(project_root) = project_root else {
                anyhow::bail!(
                    "project scope selected but no project root detected; pass --project-root"
                );
            };
            Ok(project_root.join(".codex").join("printrevolt.toml"))
        }
    }
}

fn set_toml_bool(root: &mut toml::Value, path: &[&str], value: bool) -> Result<()> {
    let mut cur = root
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("printrevolt config root is not a table"))?;
    for key in &path[..path.len().saturating_sub(1)] {
        let entry = cur
            .entry((*key).to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        cur = entry
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("path segment is not a table: {key}"))?;
    }
    let leaf = path
        .last()
        .ok_or_else(|| anyhow::anyhow!("empty config key path"))?;
    cur.insert((*leaf).to_string(), toml::Value::Boolean(value));
    Ok(())
}

fn set_toml_string(root: &mut toml::Value, path: &[&str], value: &str) -> Result<()> {
    let mut cur = root
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("printrevolt config root is not a table"))?;
    for key in &path[..path.len().saturating_sub(1)] {
        let entry = cur
            .entry((*key).to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        cur = entry
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("path segment is not a table: {key}"))?;
    }
    let leaf = path
        .last()
        .ok_or_else(|| anyhow::anyhow!("empty config key path"))?;
    cur.insert((*leaf).to_string(), toml::Value::String(value.to_string()));
    Ok(())
}

fn update_mode_b_config(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: LayerScope,
    mut apply: impl FnMut(&mut toml::Value) -> Result<()>,
) -> Result<PathBuf> {
    let target = scoped_printrevolt_toml_path(codex_home, project_root, scope)?;
    let mut root = if target.exists() {
        let raw = std::fs::read_to_string(&target)?;
        toml::from_str::<toml::Value>(&raw)
            .map_err(|err| anyhow::anyhow!("failed to parse {}: {err}", target.display()))?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    apply(&mut root)?;
    write_mode_b_printrevolt_toml_atomic(&target, &root).map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(target)
}

fn read_pipelines_file_any_from_path(path: &Path) -> Result<PipelinesFile> {
    if !path.exists() {
        return Ok(PipelinesFile::V2(PipelinesFileV2 {
            schema_version: "2".to_string(),
            components: BTreeMap::new(),
            pipelines: BTreeMap::new(),
            profile_attachments: codex_pr_pipelines::PipelineProfileAttachmentsV1::default(),
        }));
    }
    let raw = std::fs::read_to_string(path)?;
    let value = serde_json::from_str::<serde_json::Value>(&raw)?;
    let schema_version = value
        .get("schema_version")
        .and_then(|v| v.as_str())
        .unwrap_or("1");
    match schema_version {
        "1" => Ok(PipelinesFile::V1(
            serde_json::from_value::<PipelinesFileV1>(value)?,
        )),
        "2" => Ok(PipelinesFile::V2(
            serde_json::from_value::<PipelinesFileV2>(value)?,
        )),
        other => Err(anyhow::anyhow!(
            "unknown pipelines.json schema_version: {other} (expected 1 or 2)"
        )),
    }
}

impl PipelinesFile {
    fn into_v2(self) -> PipelinesFileV2 {
        match self {
            PipelinesFile::V2(v2) => v2,
            PipelinesFile::V1(v1) => PipelinesFileV2 {
                schema_version: "2".to_string(),
                components: BTreeMap::new(),
                pipelines: v1
                    .pipelines
                    .into_iter()
                    .map(|(k, v)| {
                        (
                            k,
                            PipelineEntryV2 {
                                id: v.id,
                                name: v.name,
                                enabled: v.enabled,
                                profile_refs: codex_pr_types::ProfileRefs::default(),
                                pipeline: v.pipeline,
                            },
                        )
                    })
                    .collect(),
                profile_attachments: codex_pr_pipelines::PipelineProfileAttachmentsV1::default(),
            },
        }
    }
}

fn read_workflows_file_from_path(path: &Path) -> Result<codex_pr_types::WorkflowsFileV1> {
    read_workflows_file_core(path)
        .map_err(|err| anyhow::anyhow!("invalid workflows.json ({}): {err}", path.display()))
}

fn write_workflows_file_with_backup(
    codex_home: &Path,
    path: &Path,
    file: &codex_pr_types::WorkflowsFileV1,
) -> Result<Option<PathBuf>> {
    let backup = backup_file_if_present(path, codex_home, "workflows");
    write_workflows_file_core(path, file).map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(backup)
}

fn merged_workflows(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: Scope,
) -> Result<MergedWorkflows> {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::<String>::new();
    let mut global_file_for_merge = None;
    let mut repo_file_for_merge = None;

    if matches!(scope, Scope::Global | Scope::Both) {
        let global_path = scoped_workflows_json_path(codex_home, project_root, LayerScope::Global)?;
        let global = read_workflows_file_from_path(global_path.as_path())?;
        global_file_for_merge = Some(global.clone());
        for (id, entry) in global.workflows {
            out.insert(
                id,
                WorkflowResolvedEntryV1 {
                    id: entry.id,
                    name: entry.name,
                    enabled: entry.enabled,
                    profile_refs: entry.profile_refs,
                    source: LayerScope::Global,
                    workflow: entry.workflow,
                },
            );
        }
    }
    if matches!(scope, Scope::Project | Scope::Both)
        && let Some(project_root) = project_root
    {
        let trusted = repo_trusted(Some(project_root), codex_home);
        if !trusted {
            warnings.push(format!(
                "Repo workflows bundle ignored (repo not trusted): {}",
                project_root.display()
            ));
        } else {
            let project_path =
                scoped_workflows_json_path(codex_home, Some(project_root), LayerScope::Project)?;
            let project = read_workflows_file_from_path(project_path.as_path())?;
            repo_file_for_merge = Some(project.clone());
            for (id, entry) in project.workflows {
                out.insert(
                    id,
                    WorkflowResolvedEntryV1 {
                        id: entry.id,
                        name: entry.name,
                        enabled: entry.enabled,
                        profile_refs: entry.profile_refs,
                        source: LayerScope::Project,
                        workflow: entry.workflow,
                    },
                );
            }
        }
    }
    let merged_file = codex_pr_types::WorkflowsFileV1::merge_effective(
        global_file_for_merge,
        repo_file_for_merge,
    )
    .map_err(|err| anyhow::anyhow!("invalid effective workflows file: {err}"))?;
    Ok(MergedWorkflows {
        entries: out,
        components: merged_file.components,
        warnings,
    })
}

#[derive(Debug, Clone)]
struct MergedWorkflows {
    entries: BTreeMap<String, WorkflowResolvedEntryV1>,
    components: BTreeMap<String, WorkflowComponentV1>,
    warnings: Vec<String>,
}

fn merged_pipelines(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: Scope,
) -> Result<MergedPipelines> {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::<String>::new();
    if matches!(scope, Scope::Global | Scope::Both) {
        let global_path = scoped_pipelines_json_path(codex_home, project_root, LayerScope::Global)?;
        let global = read_pipelines_file_any_from_path(global_path.as_path())?.into_v2();
        for (id, entry) in global.pipelines {
            out.insert(
                id,
                PipelineResolvedEntryV1 {
                    id: entry.id,
                    name: entry.name,
                    enabled: entry.enabled,
                    profile_refs: entry.profile_refs,
                    source: LayerScope::Global,
                    pipeline: entry.pipeline,
                },
            );
        }
    }
    if matches!(scope, Scope::Project | Scope::Both)
        && let Some(project_root) = project_root
    {
        let trusted = repo_trusted(Some(project_root), codex_home);
        if !trusted {
            warnings.push(format!(
                "Repo pipelines bundle ignored (repo not trusted): {}",
                project_root.display()
            ));
        } else {
            let project_path =
                scoped_pipelines_json_path(codex_home, Some(project_root), LayerScope::Project)?;
            let project = read_pipelines_file_any_from_path(project_path.as_path())?.into_v2();
            for (id, entry) in project.pipelines {
                // Project entries override global entries with the same id.
                out.insert(
                    id,
                    PipelineResolvedEntryV1 {
                        id: entry.id,
                        name: entry.name,
                        enabled: entry.enabled,
                        profile_refs: entry.profile_refs,
                        source: LayerScope::Project,
                        pipeline: entry.pipeline,
                    },
                );
            }
        }
    }
    Ok(MergedPipelines {
        entries: out,
        warnings,
    })
}

#[derive(Debug, Clone)]
struct MergedPipelines {
    entries: BTreeMap<String, PipelineResolvedEntryV1>,
    warnings: Vec<String>,
}

fn scoped_policy_profiles_json_path(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: LayerScope,
) -> Result<PathBuf> {
    match scope {
        LayerScope::Global => Ok(codex_pr_profiles::global_policy_profiles_path(codex_home)),
        LayerScope::Project => {
            let Some(project_root) = project_root else {
                anyhow::bail!(
                    "project scope selected but no project root detected; pass --project-root"
                );
            };
            Ok(codex_pr_profiles::project_policy_profiles_path(
                project_root,
            ))
        }
    }
}

fn scoped_guideline_profiles_json_path(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: LayerScope,
) -> Result<PathBuf> {
    match scope {
        LayerScope::Global => Ok(codex_pr_profiles::global_guideline_profiles_path(
            codex_home,
        )),
        LayerScope::Project => {
            let Some(project_root) = project_root else {
                anyhow::bail!(
                    "project scope selected but no project root detected; pass --project-root"
                );
            };
            Ok(codex_pr_profiles::project_guideline_profiles_path(
                project_root,
            ))
        }
    }
}

fn read_policy_profiles_file_from_path(path: &Path) -> Result<PolicyProfilesFileV1> {
    if !path.exists() {
        return Ok(PolicyProfilesFileV1::default());
    }
    let raw = std::fs::read_to_string(path)?;
    let parsed = serde_json::from_str::<PolicyProfilesFileV1>(&raw).map_err(|err| {
        anyhow::anyhow!("invalid policy_profiles.json ({}): {err}", path.display())
    })?;
    if parsed.schema_version != "1" {
        anyhow::bail!(
            "unknown policy_profiles.json schema_version: {} (expected 1)",
            parsed.schema_version
        );
    }
    Ok(parsed)
}

fn read_guideline_profiles_file_from_path(path: &Path) -> Result<GuidelineProfilesFileV1> {
    if !path.exists() {
        return Ok(GuidelineProfilesFileV1::default());
    }
    let raw = std::fs::read_to_string(path)?;
    let parsed = serde_json::from_str::<GuidelineProfilesFileV1>(&raw).map_err(|err| {
        anyhow::anyhow!(
            "invalid guideline_profiles.json ({}): {err}",
            path.display()
        )
    })?;
    if parsed.schema_version != "1" {
        anyhow::bail!(
            "unknown guideline_profiles.json schema_version: {} (expected 1)",
            parsed.schema_version
        );
    }
    Ok(parsed)
}

fn write_policy_profiles_file_with_backup(
    codex_home: &Path,
    path: &Path,
    file: &PolicyProfilesFileV1,
) -> Result<Option<PathBuf>> {
    let backup = backup_file_if_present(path, codex_home, "policy_profiles");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_string_pretty(file)?;
    std::fs::write(path, encoded)?;
    Ok(backup)
}

fn write_guideline_profiles_file_with_backup(
    codex_home: &Path,
    path: &Path,
    file: &GuidelineProfilesFileV1,
) -> Result<Option<PathBuf>> {
    let backup = backup_file_if_present(path, codex_home, "guideline_profiles");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_string_pretty(file)?;
    std::fs::write(path, encoded)?;
    Ok(backup)
}

fn filter_registry_for_scope(registry: &mut LoadedProfilesRegistry, scope: Scope) {
    match scope {
        Scope::Both => {}
        Scope::Global => {
            registry
                .policy_profiles
                .retain(|id, _| registry.policy_scope.get(id) == Some(&ProfileScope::Global));
            registry
                .guideline_profiles
                .retain(|id, _| registry.guideline_scope.get(id) == Some(&ProfileScope::Global));
        }
        Scope::Project => {
            registry
                .policy_profiles
                .retain(|id, _| registry.policy_scope.get(id) == Some(&ProfileScope::Project));
            registry
                .guideline_profiles
                .retain(|id, _| registry.guideline_scope.get(id) == Some(&ProfileScope::Project));
        }
    }
}

fn load_registry_for_scope(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: Scope,
) -> Result<LoadedProfilesRegistry> {
    let project_root = match scope {
        Scope::Global => None,
        Scope::Project | Scope::Both => project_root,
    };
    let trusted = repo_trusted(project_root, codex_home);
    let mut registry = load_profiles_registry(codex_home, project_root, trusted)
        .map_err(|err| anyhow::anyhow!("failed to load profile registries: {err}"))?;
    filter_registry_for_scope(&mut registry, scope);
    Ok(registry)
}

fn global_printrevolt_config(codex_home: &Path, project_root: Option<&Path>) -> PrintRevoltConfig {
    let resolved = resolve_printrevolt_config_scoped(
        codex_home,
        project_root,
        None,
        ConfigResolveScope::Global,
    );
    let cfg_toml = toml::to_string(&resolved.printrevolt).unwrap_or_default();
    toml::from_str::<PrintRevoltConfig>(&cfg_toml).unwrap_or_default()
}

fn resolved_profiles_for_scope(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: Scope,
    policy_profiles: Vec<String>,
    guideline_profiles: Vec<String>,
    use_cache: bool,
) -> Result<serde_json::Value> {
    let project_root = require_project_root(project_root.map(Path::to_path_buf), scope)?;
    let project_root_ref = project_root.as_deref();
    let cfg = match scope {
        Scope::Global => global_printrevolt_config(codex_home, project_root_ref),
        Scope::Project | Scope::Both => effective_printrevolt_config(codex_home, project_root_ref),
    };
    let floor = global_printrevolt_config(codex_home, project_root_ref).policy;
    let refs = ProfileRefs {
        policy_profiles: if policy_profiles.is_empty() {
            cfg.policy_profiles.clone()
        } else {
            policy_profiles
        },
        guideline_profiles: if guideline_profiles.is_empty() {
            cfg.guideline_profiles.clone()
        } else {
            guideline_profiles
        },
    };
    let registry = load_registry_for_scope(codex_home, project_root_ref, scope)?;
    let resolved = if use_cache {
        let cache = ProfilesResolverCache::new(512);
        resolve_subject_profiles_cached(&cache, &registry, &refs, &cfg.policy, &floor)
    } else {
        resolve_subject_profiles(&registry, &refs, &cfg.policy, &floor)
    };
    let perf = if use_cache {
        let cache = ProfilesResolverCache::new(512);
        Some(benchmark_resolver_warm_path(
            &cache,
            &registry,
            &refs,
            &cfg.policy,
            &floor,
            200,
        ))
    } else {
        None
    };

    Ok(serde_json::json!({
        "scope": scope.as_str(),
        "requested_refs": refs,
        "resolved": resolved,
        "perf": perf,
    }))
}

fn apply_profile_ref_attach(
    refs: &mut ProfileRefs,
    kind: ProfileKind,
    profile_id: &str,
    mode: ProfileMode,
) {
    let target = match kind {
        ProfileKind::Policy => &mut refs.policy_profiles,
        ProfileKind::Guideline => &mut refs.guideline_profiles,
    };
    match mode {
        ProfileMode::Merge => {
            if !target.iter().any(|v| v == profile_id) {
                target.push(profile_id.to_string());
            }
        }
        ProfileMode::Replace => {
            target.clear();
            target.push(profile_id.to_string());
        }
    }
}

fn apply_profile_ref_detach(refs: &mut ProfileRefs, kind: ProfileKind, profile_id: &str) {
    let target = match kind {
        ProfileKind::Policy => &mut refs.policy_profiles,
        ProfileKind::Guideline => &mut refs.guideline_profiles,
    };
    target.retain(|v| v != profile_id);
}

enum ProfileSubject {
    Template {
        template_id: String,
    },
    Pipeline {
        pipeline_id: String,
    },
    PipelineWorkflow {
        pipeline_id: String,
        workflow_id: String,
    },
    PipelinePart {
        pipeline_id: String,
        workflow_id: String,
        part_index: usize,
    },
    PipelineComponent {
        component_id: String,
    },
    Workflow {
        workflow_id: String,
    },
    WorkflowStep {
        workflow_id: String,
        step_id: String,
    },
    WorkflowComponent {
        component_id: String,
    },
}

fn parse_profile_subject(raw: &str) -> Result<ProfileSubject> {
    if let Some(id) = raw.strip_prefix("template:") {
        if id.trim().is_empty() {
            anyhow::bail!("template subject requires id: template:<template_id>");
        }
        return Ok(ProfileSubject::Template {
            template_id: id.to_string(),
        });
    }
    if let Some(id) = raw.strip_prefix("pipeline:") {
        return Ok(ProfileSubject::Pipeline {
            pipeline_id: id.to_string(),
        });
    }
    if let Some(rest) = raw.strip_prefix("pipeline_workflow:") {
        let Some((pipeline_id, workflow_id)) = rest.split_once('.') else {
            anyhow::bail!(
                "pipeline_workflow subject must be pipeline_workflow:<pipeline_id>.<workflow_id>"
            );
        };
        return Ok(ProfileSubject::PipelineWorkflow {
            pipeline_id: pipeline_id.to_string(),
            workflow_id: workflow_id.to_string(),
        });
    }
    if let Some(rest) = raw.strip_prefix("pipeline_part:") {
        let mut it = rest.split('.');
        let pipeline_id = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing pipeline_id"))?;
        let workflow_id = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing workflow_id"))?;
        let part_index = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing part_index"))?
            .parse::<usize>()
            .map_err(|err| anyhow::anyhow!("invalid part_index: {err}"))?;
        return Ok(ProfileSubject::PipelinePart {
            pipeline_id: pipeline_id.to_string(),
            workflow_id: workflow_id.to_string(),
            part_index,
        });
    }
    if let Some(id) = raw.strip_prefix("pipeline_component:") {
        return Ok(ProfileSubject::PipelineComponent {
            component_id: id.to_string(),
        });
    }
    if let Some(id) = raw.strip_prefix("workflow:") {
        return Ok(ProfileSubject::Workflow {
            workflow_id: id.to_string(),
        });
    }
    if let Some(rest) = raw.strip_prefix("workflow_step:") {
        let Some((workflow_id, step_id)) = rest.split_once('.') else {
            anyhow::bail!("workflow_step subject must be workflow_step:<workflow_id>.<step_id>");
        };
        return Ok(ProfileSubject::WorkflowStep {
            workflow_id: workflow_id.to_string(),
            step_id: step_id.to_string(),
        });
    }
    if let Some(id) = raw.strip_prefix("workflow_component:") {
        return Ok(ProfileSubject::WorkflowComponent {
            component_id: id.to_string(),
        });
    }
    anyhow::bail!("invalid subject format: {raw}");
}

fn mutate_template_frontmatter_profile_refs(
    path: &Path,
    kind: ProfileKind,
    profile_id: &str,
    mode: ProfileMode,
    detach: bool,
) -> Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut lines = raw.lines();
    let Some(first) = lines.next() else {
        anyhow::bail!("template is empty: {}", path.display());
    };
    if first.trim() != "---" {
        anyhow::bail!(
            "template missing YAML frontmatter start marker (---): {}",
            path.display()
        );
    }

    let mut frontmatter_len = first.len() + 1;
    let mut frontmatter_end = None;
    for line in raw[first.len() + 1..].lines() {
        if line.trim() == "---" {
            frontmatter_end = Some(frontmatter_len - 1);
            frontmatter_len += line.len() + 1;
            break;
        }
        frontmatter_len += line.len() + 1;
    }
    let Some(end_idx) = frontmatter_end else {
        anyhow::bail!(
            "template missing YAML frontmatter end marker: {}",
            path.display()
        );
    };
    let frontmatter_raw = &raw[first.len() + 1..end_idx];
    let body_start = frontmatter_len;
    let body = if body_start <= raw.len() {
        &raw[body_start..]
    } else {
        ""
    };

    let mut frontmatter: serde_yaml::Value = serde_yaml::from_str(frontmatter_raw)
        .map_err(|err| anyhow::anyhow!("invalid template frontmatter {}: {err}", path.display()))?;
    let root = frontmatter
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("template frontmatter root is not a map"))?;
    let defaults_key = serde_yaml::Value::String("defaults".to_string());
    if !root.contains_key(&defaults_key) {
        root.insert(
            defaults_key.clone(),
            serde_yaml::Value::Mapping(Default::default()),
        );
    }
    let defaults = root
        .get_mut(&defaults_key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .ok_or_else(|| anyhow::anyhow!("template defaults is not a map"))?;

    let refs_key = serde_yaml::Value::String("profile_refs".to_string());
    if !defaults.contains_key(&refs_key) {
        defaults.insert(
            refs_key.clone(),
            serde_yaml::Value::Mapping(Default::default()),
        );
    }
    let refs_map = defaults
        .get_mut(&refs_key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .ok_or_else(|| anyhow::anyhow!("template defaults.profile_refs is not a map"))?;

    let field_name = match kind {
        ProfileKind::Policy => "policy_profiles",
        ProfileKind::Guideline => "guideline_profiles",
    };
    let field_key = serde_yaml::Value::String(field_name.to_string());
    if !refs_map.contains_key(&field_key) {
        refs_map.insert(field_key.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    let values = refs_map
        .get_mut(&field_key)
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| {
            anyhow::anyhow!("template defaults.profile_refs.{field_name} is not a list")
        })?;

    if detach {
        values.retain(|v| v.as_str() != Some(profile_id));
    } else {
        match mode {
            ProfileMode::Merge => {
                if !values.iter().any(|v| v.as_str() == Some(profile_id)) {
                    values.push(serde_yaml::Value::String(profile_id.to_string()));
                }
            }
            ProfileMode::Replace => {
                values.clear();
                values.push(serde_yaml::Value::String(profile_id.to_string()));
            }
        }
    }

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&serde_yaml::to_string(&frontmatter)?);
    out.push_str("---\n");
    out.push_str(body);
    std::fs::write(path, out)?;
    Ok(())
}

fn resolve_template_attachment_target(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: LayerScope,
    template_id: &str,
) -> Result<PathBuf> {
    let discovery_scope = match scope {
        LayerScope::Global => Scope::Global,
        LayerScope::Project => Scope::Project,
    };
    let discovered = discover_templates_for_scope(codex_home, project_root, discovery_scope);
    let Some(template) = find_template_by_id(&discovered.templates, template_id) else {
        anyhow::bail!("template not found for subject: {}", template_id);
    };
    let Some(path) = template.path.as_ref() else {
        anyhow::bail!("template does not have a writable path: {}", template_id);
    };
    Ok(path.clone())
}

fn mutate_pipelines_attachment(
    file: &mut PipelinesFileV2,
    subject: &ProfileSubject,
    kind: ProfileKind,
    profile_id: &str,
    mode: ProfileMode,
    detach: bool,
) -> Result<()> {
    match subject {
        ProfileSubject::Pipeline { pipeline_id } => {
            let entry = file
                .pipelines
                .get_mut(pipeline_id)
                .ok_or_else(|| anyhow::anyhow!("pipeline not found: {pipeline_id}"))?;
            if detach {
                apply_profile_ref_detach(&mut entry.profile_refs, kind, profile_id);
            } else {
                apply_profile_ref_attach(&mut entry.profile_refs, kind, profile_id, mode);
            }
        }
        ProfileSubject::PipelineWorkflow {
            pipeline_id,
            workflow_id,
        } => {
            let key = format!("{pipeline_id}.{workflow_id}");
            let refs = file.profile_attachments.workflows.entry(key).or_default();
            if detach {
                apply_profile_ref_detach(refs, kind, profile_id);
            } else {
                apply_profile_ref_attach(refs, kind, profile_id, mode);
            }
        }
        ProfileSubject::PipelinePart {
            pipeline_id,
            workflow_id,
            part_index,
        } => {
            let key = format!("{pipeline_id}.{workflow_id}.{part_index}");
            let refs = file.profile_attachments.parts.entry(key).or_default();
            if detach {
                apply_profile_ref_detach(refs, kind, profile_id);
            } else {
                apply_profile_ref_attach(refs, kind, profile_id, mode);
            }
        }
        ProfileSubject::PipelineComponent { component_id } => {
            if let Some(component) = file.components.get_mut(component_id) {
                if detach {
                    apply_profile_ref_detach(&mut component.profile_refs, kind, profile_id);
                } else {
                    apply_profile_ref_attach(&mut component.profile_refs, kind, profile_id, mode);
                }
            } else {
                let refs = file
                    .profile_attachments
                    .components
                    .entry(component_id.clone())
                    .or_default();
                if detach {
                    apply_profile_ref_detach(refs, kind, profile_id);
                } else {
                    apply_profile_ref_attach(refs, kind, profile_id, mode);
                }
            }
        }
        _ => anyhow::bail!("subject is not a pipeline subject"),
    }
    Ok(())
}

fn mutate_workflows_attachment(
    file: &mut codex_pr_types::WorkflowsFileV1,
    subject: &ProfileSubject,
    kind: ProfileKind,
    profile_id: &str,
    mode: ProfileMode,
    detach: bool,
) -> Result<()> {
    match subject {
        ProfileSubject::Workflow { workflow_id } => {
            let entry = file
                .workflows
                .get_mut(workflow_id)
                .ok_or_else(|| anyhow::anyhow!("workflow not found: {workflow_id}"))?;
            if detach {
                apply_profile_ref_detach(&mut entry.profile_refs, kind, profile_id);
            } else {
                apply_profile_ref_attach(&mut entry.profile_refs, kind, profile_id, mode);
            }
        }
        ProfileSubject::WorkflowStep {
            workflow_id,
            step_id,
        } => {
            let key = format!("{workflow_id}.{step_id}");
            let refs = file.profile_attachments.steps.entry(key).or_default();
            if detach {
                apply_profile_ref_detach(refs, kind, profile_id);
            } else {
                apply_profile_ref_attach(refs, kind, profile_id, mode);
            }
        }
        ProfileSubject::WorkflowComponent { component_id } => {
            if let Some(component) = file.components.get_mut(component_id) {
                if detach {
                    apply_profile_ref_detach(&mut component.profile_refs, kind, profile_id);
                } else {
                    apply_profile_ref_attach(&mut component.profile_refs, kind, profile_id, mode);
                }
            } else {
                let refs = file
                    .profile_attachments
                    .components
                    .entry(component_id.clone())
                    .or_default();
                if detach {
                    apply_profile_ref_detach(refs, kind, profile_id);
                } else {
                    apply_profile_ref_attach(refs, kind, profile_id, mode);
                }
            }
        }
        _ => anyhow::bail!("subject is not a workflow subject"),
    }
    Ok(())
}

fn discover_templates_for_scope(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: Scope,
) -> codex_pr_templates::TemplateDiscoveryResult {
    match scope {
        Scope::Global => codex_pr_templates::discover_templates(
            codex_home,
            None,
            false,
            codex_pr_templates::TemplateDiscoveryConfig::default(),
        ),
        Scope::Project => {
            let Some(root) = project_root else {
                return codex_pr_templates::TemplateDiscoveryResult {
                    templates: Vec::new(),
                    warnings: vec![
                        "project scope selected but no project root detected; pass --project-root"
                            .to_string(),
                    ],
                };
            };
            let trusted = repo_trusted(Some(root), codex_home);
            let mut res = codex_pr_templates::discover_templates(
                codex_home,
                Some(root),
                trusted,
                codex_pr_templates::TemplateDiscoveryConfig::default(),
            );
            res.templates
                .retain(|t| t.source == codex_pr_templates::TemplateSource::Repo);
            res
        }
        Scope::Both => {
            let trusted = repo_trusted(project_root, codex_home);
            codex_pr_templates::discover_templates(
                codex_home,
                project_root,
                trusted,
                codex_pr_templates::TemplateDiscoveryConfig::default(),
            )
        }
    }
}

fn effective_printrevolt_config(
    codex_home: &Path,
    project_root: Option<&Path>,
) -> PrintRevoltConfig {
    let resolved = resolve_printrevolt_config(codex_home, project_root, None);
    let cfg_toml = toml::to_string(&resolved.printrevolt).unwrap_or_default();
    toml::from_str::<PrintRevoltConfig>(&cfg_toml).unwrap_or_default()
}

fn workflows_state_root(codex_home: &Path) -> PathBuf {
    codex_home
        .join("printrevolt")
        .join("state")
        .join("workflows")
}

fn workflow_run_state_path(codex_home: &Path, run_id: &str) -> PathBuf {
    workflows_state_root(codex_home).join(format!("{run_id}.json"))
}

fn workflow_artifacts_root(codex_home: &Path) -> PathBuf {
    codex_home
        .join("printrevolt")
        .join("artifacts")
        .join("workflows")
}

fn sanitize_run_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        return "workflow-run".to_string();
    }
    out
}

fn generate_run_id(workflow_id: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}-{now}", sanitize_run_id(workflow_id))
}

fn find_latest_workflow_run_id(codex_home: &Path, workflow_id: Option<&str>) -> Option<String> {
    let root = workflows_state_root(codex_home);
    let entries = std::fs::read_dir(root).ok()?;
    let mut latest: Option<(SystemTime, String)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(workflow_id) = workflow_id {
            let Ok(state) = read_run_state(&path) else {
                continue;
            };
            if state.workflow_id.as_deref() != Some(workflow_id) {
                continue;
            }
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match &latest {
            Some((ts, _)) if &modified <= ts => {}
            _ => latest = Some((modified, stem.to_string())),
        }
    }
    latest.map(|(_, id)| id)
}

fn find_template_by_id<'a>(
    templates: &'a [codex_pr_templates::Template],
    template_id: &str,
) -> Option<&'a codex_pr_templates::Template> {
    if let Some(t) = templates.iter().find(|t| t.id == template_id) {
        return Some(t);
    }
    templates.iter().find(|t| {
        let id_tail =
            t.id.split(':')
                .next_back()
                .unwrap_or(t.id.as_str())
                .trim_end_matches(".md");
        id_tail == template_id || t.name.eq_ignore_ascii_case(template_id)
    })
}

fn prompt_yes_no(question: &str, default_yes: bool) -> Result<bool> {
    let default_hint = if default_yes { "Y/n" } else { "y/N" };
    print!("{question} [{default_hint}]: ");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    let answer = buf.trim().to_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn prompt_feedback(prompt: &str) -> Result<String> {
    print!("{prompt}: ");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn pager_print(text: &str, lines_per_page: usize) -> Result<()> {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        println!("<empty>");
        return Ok(());
    }
    let mut idx = 0usize;
    while idx < lines.len() {
        let end = (idx + lines_per_page).min(lines.len());
        for line in &lines[idx..end] {
            println!("{line}");
        }
        idx = end;
        if idx < lines.len() && !prompt_yes_no("Continue artifact preview?", true)? {
            break;
        }
    }
    Ok(())
}

fn workflow_artifact_text(
    codex_home: &Path,
    state: &WorkflowRunState,
    artifact_ref: &str,
) -> Option<String> {
    let record = state.artifacts.get(artifact_ref)?;
    let path = workflow_artifacts_root(codex_home).join(&record.object_rel_path);
    std::fs::read_to_string(path).ok()
}

fn effective_pipeline_components(
    codex_home: &Path,
    project_root: Option<&Path>,
) -> Result<BTreeMap<String, ComponentV2>> {
    let global_path = scoped_pipelines_json_path(codex_home, project_root, LayerScope::Global)?;
    let global = read_pipelines_file_any_from_path(global_path.as_path())?.into_v2();
    let mut components = global.components;
    if let Some(root) = project_root
        && repo_trusted(Some(root), codex_home)
    {
        let project_path = scoped_pipelines_json_path(codex_home, Some(root), LayerScope::Project)?;
        let project = read_pipelines_file_any_from_path(project_path.as_path())?.into_v2();
        for (id, component) in project.components {
            components.insert(id, component);
        }
    }
    Ok(components)
}

#[derive(Debug)]
enum WorkflowActionControl {
    Result(WorkflowActionResult),
    Cancelled,
}

fn pipeline_scope_to_scope(raw: Option<&str>) -> Scope {
    match raw {
        Some("global") => Scope::Global,
        Some("project") => Scope::Project,
        Some("effective") | None => Scope::Both,
        Some(_) => Scope::Both,
    }
}

fn execute_workflow_action_interactive(
    codex_home: &Path,
    project_root: Option<&Path>,
    action: &WorkflowAction,
    state: &WorkflowRunState,
    templates: &[codex_pr_templates::Template],
    guideline_instructions: &[String],
    initial_prompt: Option<&str>,
    model: Option<&str>,
) -> Result<WorkflowActionControl> {
    match action {
        WorkflowAction::InvokeAgent {
            step_id,
            template_id,
            artifact_ref,
            inputs,
            feedback,
            ..
        } => {
            let mut raw_prompt = String::new();
            if let Some(initial_prompt) = initial_prompt {
                raw_prompt.push_str(initial_prompt.trim());
                raw_prompt.push_str("\n\n");
            }
            if !inputs.is_empty() {
                raw_prompt.push_str("Inputs:\n");
                for (k, v) in inputs {
                    raw_prompt.push_str("- ");
                    raw_prompt.push_str(k);
                    raw_prompt.push_str(": ");
                    raw_prompt.push_str(v);
                    raw_prompt.push('\n');
                }
                raw_prompt.push('\n');
            }
            if let Some(previous) = workflow_artifact_text(codex_home, state, artifact_ref) {
                raw_prompt.push_str("Current artifact content:\n");
                raw_prompt.push_str(previous.trim());
                raw_prompt.push_str("\n\n");
            }
            if let Some(feedback) = feedback.as_deref() {
                raw_prompt.push_str("Revision feedback:\n");
                raw_prompt.push_str(feedback.trim());
                raw_prompt.push('\n');
            }
            if raw_prompt.trim().is_empty() {
                raw_prompt = format!("Generate artifact for step {step_id}.");
            }

            let template = find_template_by_id(templates, template_id);
            let composed_prompt = if let Some(template) = template {
                codex_pr_templates::compose_prompt_with_guidelines(
                    &raw_prompt,
                    template,
                    guideline_instructions,
                )
            } else {
                raw_prompt
            };

            let mut prompt_hash = Sha256::new();
            prompt_hash.update(composed_prompt.as_bytes());
            let prompt_sha256 = to_lower_hex(prompt_hash.finalize().as_ref());

            println!();
            println!("Step: {step_id}");
            println!("Action: invoke_agent");
            println!("Template: {template_id}");
            println!("Prompt sha256: {prompt_sha256}");
            if template.is_none() {
                println!(
                    "Warning: template {template_id} not found; using raw composed prompt only."
                );
            }
            println!("Prompt preview:");
            pager_print(composed_prompt.as_str(), 30)?;
            if !prompt_yes_no("Invoke agent now?", true)? {
                return Ok(WorkflowActionControl::Cancelled);
            }

            let request = WorkflowAgentInvokeRequest {
                prompt: composed_prompt,
                cwd: project_root
                    .map(Path::to_path_buf)
                    .or_else(|| std::env::current_dir().ok()),
                model: model.map(ToString::to_string),
            };
            let content = invoke_workflow_agent(&request)?;
            Ok(WorkflowActionControl::Result(
                WorkflowActionResult::AgentOutput { content },
            ))
        }
        WorkflowAction::RequestReview {
            step_id,
            artifact_ref,
            prompt,
            current_revisions,
            max_revisions,
            ..
        } => {
            println!();
            println!("Step: {step_id}");
            println!("Action: request_review");
            println!("Artifact: {artifact_ref}");
            println!("Revision count: {current_revisions}/{max_revisions}");
            if guideline_instructions.is_empty() {
                println!("Review prompt: {prompt}");
            } else {
                println!("Review prompt: {prompt}");
                println!("Guidelines:");
                for g in guideline_instructions {
                    println!("- {}", g.trim());
                }
            }
            if let Some(content) = workflow_artifact_text(codex_home, state, artifact_ref) {
                println!();
                println!("Artifact preview:");
                pager_print(content.as_str(), 30)?;
            } else {
                println!("Artifact content not found in local store for ref: {artifact_ref}");
            }
            if prompt_yes_no("Approve this artifact?", false)? {
                return Ok(WorkflowActionControl::Result(
                    WorkflowActionResult::ReviewDecision {
                        approved: true,
                        feedback: None,
                    },
                ));
            }
            if prompt_yes_no("Cancel this workflow run?", false)? {
                return Ok(WorkflowActionControl::Cancelled);
            }
            let feedback = prompt_feedback("Enter review feedback")?;
            Ok(WorkflowActionControl::Result(
                WorkflowActionResult::ReviewDecision {
                    approved: false,
                    feedback: Some(feedback),
                },
            ))
        }
        WorkflowAction::RunPipeline {
            step_id,
            pipeline_id,
            pipeline_scope,
            ..
        } => {
            println!();
            println!("Step: {step_id}");
            println!("Action: run_pipeline");
            println!(
                "Pipeline id: {} (scope={})",
                pipeline_id,
                pipeline_scope.as_deref().unwrap_or("effective")
            );

            let scope = pipeline_scope_to_scope(pipeline_scope.as_deref());
            let merged = merged_pipelines(codex_home, project_root, scope)?;
            for warning in &merged.warnings {
                println!("Warning: {warning}");
            }
            let Some(entry) = merged.entries.get(pipeline_id) else {
                return Ok(WorkflowActionControl::Result(
                    WorkflowActionResult::PipelineOutput {
                        success: false,
                        summary: Some(format!("pipeline not found: {pipeline_id}")),
                    },
                ));
            };
            let components = effective_pipeline_components(codex_home, project_root)?;
            let expanded = expand_pipeline(&entry.pipeline, &components, ExpandLimits::default())?;
            println!("Expanded pipeline preview:");
            println!("{}", serde_json::to_string_pretty(&expanded)?);
            if prompt_yes_no("Mark pipeline step as successful and continue?", true)? {
                return Ok(WorkflowActionControl::Result(
                    WorkflowActionResult::PipelineOutput {
                        success: true,
                        summary: Some("approved by interactive workflow runner".to_string()),
                    },
                ));
            }
            if prompt_yes_no("Cancel this workflow run?", false)? {
                return Ok(WorkflowActionControl::Cancelled);
            }
            let summary = prompt_feedback("Failure summary (optional)")?;
            Ok(WorkflowActionControl::Result(
                WorkflowActionResult::PipelineOutput {
                    success: false,
                    summary: if summary.trim().is_empty() {
                        None
                    } else {
                        Some(summary)
                    },
                },
            ))
        }
        WorkflowAction::Complete { .. } => Ok(WorkflowActionControl::Result(
            WorkflowActionResult::PipelineOutput {
                success: true,
                summary: Some("workflow complete".to_string()),
            },
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_workflow_command(
    codex_home: &Path,
    project_root: Option<PathBuf>,
    scope: Scope,
    id: String,
    run_id: Option<String>,
    resume: bool,
    prompt: Option<String>,
    model: Option<String>,
    emit_actions_json: bool,
    action_result_json: Option<String>,
    max_steps: u32,
) -> Result<()> {
    let project_root = require_project_root(project_root, scope)?;
    let merged = merged_workflows(codex_home, project_root.as_deref(), scope)?;
    let entry = merged
        .entries
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("workflow not found: {id}"))?
        .clone();
    for warning in &merged.warnings {
        println!("Warning: {warning}");
    }

    let cfg = effective_printrevolt_config(codex_home, project_root.as_deref());
    if !cfg.workflows.enabled {
        anyhow::bail!(
            "workflows are disabled in effective config ([printrevolt.workflows].enabled=false)"
        );
    }
    let floor_cfg = global_printrevolt_config(codex_home, project_root.as_deref());
    let registry = load_registry_for_scope(codex_home, project_root.as_deref(), scope)?;
    let workflow_refs = ProfileRefs {
        policy_profiles: cfg.policy_profiles.clone(),
        guideline_profiles: cfg.guideline_profiles.clone(),
    };
    let resolved_profile_ctx =
        resolve_subject_profiles(&registry, &workflow_refs, &cfg.policy, &floor_cfg.policy);
    let workflow_guidelines = resolved_profile_ctx.guidelines;

    let engine = WorkflowEngine::new_with_components(
        entry.workflow,
        merged.components,
        WorkflowEngineLimits {
            default_max_revisions: cfg.workflows.max_revisions,
        },
    )?;
    let artifact_store = ArtifactStore::new(ArtifactStoreConfig {
        root: workflow_artifacts_root(codex_home),
        max_artifact_bytes: cfg.workflows.max_artifact_bytes,
        max_feedback_bytes: cfg.workflows.max_feedback_bytes,
    });
    let templates = discover_templates_for_scope(codex_home, project_root.as_deref(), scope);
    for warning in &templates.warnings {
        println!("Warning: {warning}");
    }

    let effective_run_id = match (run_id, resume) {
        (Some(run_id), _) => sanitize_run_id(&run_id),
        (None, true) => find_latest_workflow_run_id(codex_home, Some(&id))
            .ok_or_else(|| anyhow::anyhow!("no previous run found for workflow id: {id}"))?,
        (None, false) => generate_run_id(&id),
    };
    let state_path = workflow_run_state_path(codex_home, &effective_run_id);

    let mut state = if state_path.exists() {
        read_run_state(state_path.as_path())?
    } else if resume {
        anyhow::bail!(
            "resume requested but run state file does not exist: {}",
            state_path.display()
        );
    } else {
        let mut state = engine.start(effective_run_id);
        state.workflow_id = Some(id.clone());
        state
    };
    if state.workflow_id.is_none() {
        state.workflow_id = Some(id.clone());
    }
    if state.workflow_id.as_deref() != Some(id.as_str()) {
        anyhow::bail!(
            "run {} belongs to workflow {:?}, not {}",
            state.run_id,
            state.workflow_id,
            id
        );
    }

    if let Some(result_json) = action_result_json.as_deref() {
        let pending_action = engine.action_for_pending(&state)?.ok_or_else(|| {
            anyhow::anyhow!("--action-result-json provided but no pending action")
        })?;
        let result = action_result_from_json(result_json)?;
        engine.apply_action_result(&mut state, &pending_action, result, &artifact_store)?;
        write_run_state(state_path.as_path(), &state)?;
    }

    let mut steps = 0u32;
    loop {
        if steps >= max_steps {
            anyhow::bail!(
                "workflow runner exceeded max_steps={} (run_id={})",
                max_steps,
                state.run_id
            );
        }

        if state.is_terminal() {
            write_run_state(state_path.as_path(), &state)?;
            println!(
                "Workflow run {} ended with status={:?}",
                state.run_id, state.status
            );
            return Ok(());
        }

        let action = if state.pending.is_some() {
            engine.action_for_pending(&state)?.ok_or_else(|| {
                anyhow::anyhow!("run state indicates pending action, but action cannot be rebuilt")
            })?
        } else {
            let Some(next) = engine.next_action(&mut state)? else {
                write_run_state(state_path.as_path(), &state)?;
                return Ok(());
            };
            next
        };

        if emit_actions_json {
            write_run_state(state_path.as_path(), &state)?;
            println!("{}", action_to_json(&action)?);
            return Ok(());
        }

        if matches!(action, WorkflowAction::Complete { .. }) {
            write_run_state(state_path.as_path(), &state)?;
            println!("Workflow run {} completed.", state.run_id);
            return Ok(());
        }

        let control = execute_workflow_action_interactive(
            codex_home,
            project_root.as_deref(),
            &action,
            &state,
            &templates.templates,
            workflow_guidelines.as_slice(),
            prompt.as_deref(),
            model.as_deref(),
        )?;
        match control {
            WorkflowActionControl::Cancelled => {
                state.pending = None;
                state.status = WorkflowRunStatus::Failed;
                state.last_error = Some("cancelled by user".to_string());
                write_run_state(state_path.as_path(), &state)?;
                println!("Workflow run {} cancelled.", state.run_id);
                return Ok(());
            }
            WorkflowActionControl::Result(result) => {
                engine.apply_action_result(&mut state, &action, result, &artifact_store)?;
                write_run_state(state_path.as_path(), &state)?;
                steps += 1;
            }
        }
    }
}

fn backup_file_if_present(target: &Path, codex_home: &Path, kind: &str) -> Option<PathBuf> {
    if !target.exists() {
        return None;
    }
    let file_name = target.file_name()?.to_string_lossy().to_string();
    let dir = codex_home.join("printrevolt").join("backups").join(kind);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let path = dir.join(format!(
        "{}-{file_name}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    if std::fs::copy(target, &path).is_err() {
        return None;
    }
    Some(path)
}

fn write_pipelines_file(
    codex_home: &Path,
    path: &Path,
    file: &impl serde::Serialize,
) -> Result<Option<PathBuf>> {
    let backup = backup_file_if_present(path, codex_home, "pipelines");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_string_pretty(file)?;
    std::fs::write(path, encoded)?;
    Ok(backup)
}

fn find_latest_policy_decision_in_audit(
    codex_home: &Path,
    prefer_non_allow: bool,
) -> Option<serde_json::Value> {
    use std::io::Read as _;
    use std::io::Seek as _;
    use std::io::SeekFrom;

    let audit_root = codex_home.join("printrevolt").join("audit");
    let mut audit_files = vec![audit_root.join("events.jsonl")];
    for idx in 1..=3usize {
        audit_files.push(audit_root.join(format!("events.jsonl.{idx}")));
    }

    let mut best_allow: Option<serde_json::Value> = None;
    for path in audit_files {
        let Ok(mut file) = std::fs::File::open(&path) else {
            continue;
        };
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        const MAX_BYTES: u64 = 256 * 1024;
        if size > MAX_BYTES {
            let _ = file.seek(SeekFrom::End(-(MAX_BYTES as i64)));
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        for line in text.lines().rev().take(500) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("kind").and_then(|k| k.as_str()) != Some("policy_decision") {
                continue;
            }
            if !prefer_non_allow {
                return Some(v);
            }
            let kind = v
                .get("payload")
                .and_then(|p| p.get("decision"))
                .and_then(|d| d.get("kind"))
                .and_then(|k| k.as_str())
                .unwrap_or_default();
            if kind != "allow" {
                return Some(v);
            }
            if best_allow.is_none() {
                best_allow = Some(v);
            }
        }
    }
    best_allow
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor {
            project_root,
            json,
            recommend,
        } => {
            let codex_home = find_codex_home()?;
            let project_root = discover_project_root(project_root);
            let resolved =
                resolve_printrevolt_config(codex_home.as_path(), project_root.as_deref(), None);
            if recommend {
                let bundle = doctor_recommend(project_root.as_deref())?;
                println!("{}", serde_json::to_string_pretty(&bundle)?);
                return Ok(());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&resolved)?);
                return Ok(());
            }

            println!("CODEX_HOME: {}", codex_home.display());
            match &project_root {
                Some(root) => println!("Project root: {}", root.display()),
                None => println!("Project root: (not detected)"),
            }
            println!();

            println!("Sources:");
            for source in &resolved.sources {
                println!(
                    "- {:?}: {:?}{}",
                    source.source,
                    source.status,
                    source
                        .message
                        .as_deref()
                        .map(|m| format!(" ({m})"))
                        .unwrap_or_default()
                );
            }
            if !resolved.warnings.is_empty() {
                println!();
                println!("Warnings:");
                for w in &resolved.warnings {
                    println!("- {w}");
                }
            }

            println!();
            println!("Effective [printrevolt] keys:");
            let flattened = flatten_printrevolt(&resolved.printrevolt);
            for (key, value) in flattened {
                let source = resolved
                    .trace
                    .get(&key)
                    .cloned()
                    .unwrap_or(codex_pr_config::ConfigSource::Defaults);
                println!("- {key} = {value}  (from {source:?})");
            }
        }

        Command::Policy { command } => {
            let codex_home = find_codex_home()?;
            match command {
                PolicyCommand::Status {
                    project_root,
                    scope,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let resolved = resolve_printrevolt_config_scoped(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        None,
                        to_config_scope(scope),
                    );
                    let cfg_toml = toml::to_string(&resolved.printrevolt).unwrap_or_default();
                    let cfg: codex_pr_types::PrintRevoltConfig =
                        toml::from_str(&cfg_toml).unwrap_or_default();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "scope": scope,
                                "policy": cfg.policy,
                            }))?
                        );
                    } else {
                        println!("scope={}", scope.as_str());
                        println!("enabled={}", cfg.policy.enabled);
                        println!("deny_dangerous_always={}", cfg.policy.deny_dangerous_always);
                        println!("verify.required={}", cfg.policy.verify.required);
                        println!("verify.max_age_ms={}", cfg.policy.verify.max_age_ms);
                    }
                }
                PolicyCommand::Explain { json } => {
                    let v = find_latest_policy_decision_in_audit(codex_home.as_path(), true);
                    if json {
                        println!("{}", serde_json::to_string_pretty(&v)?);
                        return Ok(());
                    }
                    let Some(v) = v else {
                        println!("No policy decisions recorded yet.");
                        return Ok(());
                    };
                    let decision = v.get("payload").and_then(|p| p.get("decision"));
                    let kind = decision
                        .and_then(|d| d.get("kind"))
                        .and_then(|k| k.as_str())
                        .unwrap_or("unknown");
                    let reason = decision
                        .and_then(|d| d.get("reason_code"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("<none>");
                    let message = decision
                        .and_then(|d| d.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    println!("decision={kind}");
                    println!("reason_code={reason}");
                    if !message.is_empty() {
                        println!("message={message}");
                    }
                }
                PolicyCommand::Enable {
                    project_root,
                    scope,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = update_mode_b_config(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                        |root| set_toml_bool(root, &["policy", "enabled"], true),
                    )?;
                    println!("Enabled policy in {}", path.display());
                }
                PolicyCommand::Disable {
                    project_root,
                    scope,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = update_mode_b_config(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                        |root| set_toml_bool(root, &["policy", "enabled"], false),
                    )?;
                    println!("Disabled policy in {}", path.display());
                }
            }
        }

        Command::Profiles { command } => {
            let codex_home = find_codex_home()?;
            match command {
                ProfilesCommand::List {
                    project_root,
                    scope,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let registry = load_registry_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        scope,
                    )?;
                    let mut policy = registry.policy_profiles.keys().cloned().collect::<Vec<_>>();
                    policy.sort();
                    let mut guideline = registry
                        .guideline_profiles
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>();
                    guideline.sort();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "scope": scope,
                                "policy_profiles": policy,
                                "guideline_profiles": guideline,
                                "warnings": registry.warnings,
                            }))?
                        );
                    } else {
                        println!("scope={}", scope.as_str());
                        println!("policy_profiles={}", policy.len());
                        for id in policy {
                            println!("- {id}");
                        }
                        println!("guideline_profiles={}", guideline.len());
                        for id in guideline {
                            println!("- {id}");
                        }
                        if !registry.warnings.is_empty() {
                            println!();
                            println!("Warnings:");
                            for w in registry.warnings {
                                println!("- {w}");
                            }
                        }
                    }
                }
                ProfilesCommand::Show {
                    project_root,
                    scope,
                    kind,
                    id,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let registry = load_registry_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        scope,
                    )?;
                    let payload = match kind {
                        ProfileKind::Policy => {
                            let profile = registry
                                .policy_profiles
                                .get(&id)
                                .ok_or_else(|| anyhow::anyhow!("policy profile not found: {id}"))?;
                            serde_json::json!({
                                "kind": "policy",
                                "id": id,
                                "scope": registry.policy_scope.get(&id),
                                "profile": profile,
                            })
                        }
                        ProfileKind::Guideline => {
                            let profile =
                                registry.guideline_profiles.get(&id).ok_or_else(|| {
                                    anyhow::anyhow!("guideline profile not found: {id}")
                                })?;
                            serde_json::json!({
                                "kind": "guideline",
                                "id": id,
                                "scope": registry.guideline_scope.get(&id),
                                "profile": profile,
                            })
                        }
                    };
                    if json {
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                        for w in registry.warnings {
                            println!("Warning: {w}");
                        }
                    }
                }
                ProfilesCommand::Validate {
                    project_root,
                    scope,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let registry = load_registry_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        scope,
                    )?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "ok": true,
                                "scope": scope,
                                "policy_profiles": registry.policy_profiles.len(),
                                "guideline_profiles": registry.guideline_profiles.len(),
                                "warnings": registry.warnings,
                            }))?
                        );
                    } else {
                        println!(
                            "Validated registries (scope={}): policy_profiles={} guideline_profiles={}",
                            scope.as_str(),
                            registry.policy_profiles.len(),
                            registry.guideline_profiles.len()
                        );
                        for w in registry.warnings {
                            println!("Warning: {w}");
                        }
                    }
                }
                ProfilesCommand::Draft {
                    project_root,
                    scope,
                    kind,
                    id,
                    description,
                    apply,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    match kind {
                        ProfileKind::Policy => {
                            let path = scoped_policy_profiles_json_path(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                            )?;
                            let mut file = read_policy_profiles_file_from_path(path.as_path())?;
                            let draft = codex_pr_types::PolicyProfileV1 {
                                description,
                                includes: Vec::new(),
                                patch: codex_pr_types::PolicyProfilePatchV1::default(),
                            };
                            if !apply {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&serde_json::json!({
                                        "path": path,
                                        "id": id,
                                        "profile": draft,
                                    }))?
                                );
                                return Ok(());
                            }
                            file.profiles.insert(id, draft);
                            let backup = write_policy_profiles_file_with_backup(
                                codex_home.as_path(),
                                path.as_path(),
                                &file,
                            )?;
                            if let Some(backup) = backup {
                                println!(
                                    "Updated policy_profiles.json (backup: {})",
                                    backup.display()
                                );
                            } else {
                                println!("Updated policy_profiles.json");
                            }
                        }
                        ProfileKind::Guideline => {
                            let path = scoped_guideline_profiles_json_path(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                            )?;
                            let mut file = read_guideline_profiles_file_from_path(path.as_path())?;
                            let draft = codex_pr_types::GuidelineProfileV1 {
                                description,
                                includes: Vec::new(),
                                instructions: vec![
                                    "Be explicit about assumptions and tradeoffs.".to_string(),
                                ],
                            };
                            if !apply {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&serde_json::json!({
                                        "path": path,
                                        "id": id,
                                        "profile": draft,
                                    }))?
                                );
                                return Ok(());
                            }
                            file.profiles.insert(id, draft);
                            let backup = write_guideline_profiles_file_with_backup(
                                codex_home.as_path(),
                                path.as_path(),
                                &file,
                            )?;
                            if let Some(backup) = backup {
                                println!(
                                    "Updated guideline_profiles.json (backup: {})",
                                    backup.display()
                                );
                            } else {
                                println!("Updated guideline_profiles.json");
                            }
                        }
                    }
                }
                ProfilesCommand::Resolve {
                    project_root,
                    scope,
                    policy_profiles,
                    guideline_profiles,
                    perf,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let payload = resolved_profiles_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        scope,
                        policy_profiles,
                        guideline_profiles,
                        perf,
                    )?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                    }
                }
                ProfilesCommand::Explain {
                    project_root,
                    scope,
                    policy_profiles,
                    guideline_profiles,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let payload = resolved_profiles_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        scope,
                        policy_profiles,
                        guideline_profiles,
                        false,
                    )?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&payload)?);
                    } else {
                        let resolved = payload
                            .get("resolved")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({}));
                        let fingerprint = resolved
                            .get("fingerprint_sha256")
                            .and_then(|v| v.as_str())
                            .unwrap_or("<none>");
                        println!("fingerprint={fingerprint}");
                        println!(
                            "provenance={}",
                            resolved
                                .get("provenance")
                                .and_then(|v| v.as_array())
                                .map(|v| v.len())
                                .unwrap_or(0)
                        );
                        if let Some(provenance) =
                            resolved.get("provenance").and_then(|v| v.as_array())
                        {
                            for entry in provenance {
                                let profile_type = entry
                                    .get("profile_type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let profile_id = entry
                                    .get("profile_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                let scope = entry
                                    .get("scope")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                println!("- {profile_type}:{profile_id} ({scope})");
                            }
                        }
                        if let Some(warnings) = resolved.get("warnings").and_then(|v| v.as_array())
                            && !warnings.is_empty()
                        {
                            println!("Warnings:");
                            for warning in warnings {
                                println!("- {}", warning.as_str().unwrap_or_default());
                            }
                        }
                    }
                }
                ProfilesCommand::Attach {
                    project_root,
                    scope,
                    subject,
                    kind,
                    profile_id,
                    mode,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let parsed_subject = parse_profile_subject(subject.as_str())?;
                    let registry = load_registry_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        Scope::Both,
                    )?;
                    let exists = match kind {
                        ProfileKind::Policy => registry.policy_profiles.contains_key(&profile_id),
                        ProfileKind::Guideline => {
                            registry.guideline_profiles.contains_key(&profile_id)
                        }
                    };
                    if !exists {
                        anyhow::bail!("profile id not found in effective registry: {profile_id}");
                    }

                    match parsed_subject {
                        ProfileSubject::Template { template_id } => {
                            let template_path = resolve_template_attachment_target(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                                template_id.as_str(),
                            )?;
                            let backup = backup_file_if_present(
                                template_path.as_path(),
                                codex_home.as_path(),
                                "templates",
                            );
                            mutate_template_frontmatter_profile_refs(
                                template_path.as_path(),
                                kind,
                                profile_id.as_str(),
                                mode,
                                false,
                            )?;
                            if let Some(backup) = backup {
                                println!(
                                    "Updated template attachment (backup: {})",
                                    backup.display()
                                );
                            } else {
                                println!("Updated template attachment");
                            }
                        }
                        ProfileSubject::Pipeline { .. }
                        | ProfileSubject::PipelineWorkflow { .. }
                        | ProfileSubject::PipelinePart { .. }
                        | ProfileSubject::PipelineComponent { .. } => {
                            let path = scoped_pipelines_json_path(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                            )?;
                            let mut file =
                                read_pipelines_file_any_from_path(path.as_path())?.into_v2();
                            mutate_pipelines_attachment(
                                &mut file,
                                &parsed_subject,
                                kind,
                                profile_id.as_str(),
                                mode,
                                false,
                            )?;
                            let backup =
                                write_pipelines_file(codex_home.as_path(), path.as_path(), &file)?;
                            if let Some(backup) = backup {
                                println!(
                                    "Updated pipelines.json attachments (backup: {})",
                                    backup.display()
                                );
                            } else {
                                println!("Updated pipelines.json attachments");
                            }
                        }
                        ProfileSubject::Workflow { .. }
                        | ProfileSubject::WorkflowStep { .. }
                        | ProfileSubject::WorkflowComponent { .. } => {
                            let path = scoped_workflows_json_path(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                            )?;
                            let mut file = read_workflows_file_from_path(path.as_path())?;
                            mutate_workflows_attachment(
                                &mut file,
                                &parsed_subject,
                                kind,
                                profile_id.as_str(),
                                mode,
                                false,
                            )?;
                            file.validate().map_err(|err| {
                                anyhow::anyhow!("invalid workflows file after attachment: {err}")
                            })?;
                            let backup = write_workflows_file_with_backup(
                                codex_home.as_path(),
                                path.as_path(),
                                &file,
                            )?;
                            if let Some(backup) = backup {
                                println!(
                                    "Updated workflows.json attachments (backup: {})",
                                    backup.display()
                                );
                            } else {
                                println!("Updated workflows.json attachments");
                            }
                        }
                    }
                }
                ProfilesCommand::Detach {
                    project_root,
                    scope,
                    subject,
                    kind,
                    profile_id,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let parsed_subject = parse_profile_subject(subject.as_str())?;
                    match parsed_subject {
                        ProfileSubject::Template { template_id } => {
                            let template_path = resolve_template_attachment_target(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                                template_id.as_str(),
                            )?;
                            let backup = backup_file_if_present(
                                template_path.as_path(),
                                codex_home.as_path(),
                                "templates",
                            );
                            mutate_template_frontmatter_profile_refs(
                                template_path.as_path(),
                                kind,
                                profile_id.as_str(),
                                ProfileMode::Merge,
                                true,
                            )?;
                            if let Some(backup) = backup {
                                println!(
                                    "Updated template attachment (backup: {})",
                                    backup.display()
                                );
                            } else {
                                println!("Updated template attachment");
                            }
                        }
                        ProfileSubject::Pipeline { .. }
                        | ProfileSubject::PipelineWorkflow { .. }
                        | ProfileSubject::PipelinePart { .. }
                        | ProfileSubject::PipelineComponent { .. } => {
                            let path = scoped_pipelines_json_path(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                            )?;
                            let mut file =
                                read_pipelines_file_any_from_path(path.as_path())?.into_v2();
                            mutate_pipelines_attachment(
                                &mut file,
                                &parsed_subject,
                                kind,
                                profile_id.as_str(),
                                ProfileMode::Merge,
                                true,
                            )?;
                            let backup =
                                write_pipelines_file(codex_home.as_path(), path.as_path(), &file)?;
                            if let Some(backup) = backup {
                                println!(
                                    "Updated pipelines.json attachments (backup: {})",
                                    backup.display()
                                );
                            } else {
                                println!("Updated pipelines.json attachments");
                            }
                        }
                        ProfileSubject::Workflow { .. }
                        | ProfileSubject::WorkflowStep { .. }
                        | ProfileSubject::WorkflowComponent { .. } => {
                            let path = scoped_workflows_json_path(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                            )?;
                            let mut file = read_workflows_file_from_path(path.as_path())?;
                            mutate_workflows_attachment(
                                &mut file,
                                &parsed_subject,
                                kind,
                                profile_id.as_str(),
                                ProfileMode::Merge,
                                true,
                            )?;
                            file.validate().map_err(|err| {
                                anyhow::anyhow!("invalid workflows file after detach: {err}")
                            })?;
                            let backup = write_workflows_file_with_backup(
                                codex_home.as_path(),
                                path.as_path(),
                                &file,
                            )?;
                            if let Some(backup) = backup {
                                println!(
                                    "Updated workflows.json attachments (backup: {})",
                                    backup.display()
                                );
                            } else {
                                println!("Updated workflows.json attachments");
                            }
                        }
                    }
                }
                ProfilesCommand::Interactive {
                    project_root,
                    scope,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    println!("Profiles Interactive");
                    println!("1) List");
                    println!("2) Resolve");
                    println!("3) Explain");
                    let choice = prompt_feedback("Select action number")?;
                    match choice.trim() {
                        "1" => {
                            let registry = load_registry_for_scope(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                scope,
                            )?;
                            println!(
                                "policy_profiles={} guideline_profiles={}",
                                registry.policy_profiles.len(),
                                registry.guideline_profiles.len()
                            );
                        }
                        "2" => {
                            let payload = resolved_profiles_for_scope(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                scope,
                                Vec::new(),
                                Vec::new(),
                                true,
                            )?;
                            println!("{}", serde_json::to_string_pretty(&payload)?);
                        }
                        "3" => {
                            let payload = resolved_profiles_for_scope(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                scope,
                                Vec::new(),
                                Vec::new(),
                                false,
                            )?;
                            println!("{}", serde_json::to_string_pretty(&payload)?);
                        }
                        _ => println!("No-op: unknown option"),
                    }
                }
            }
        }

        Command::Pipelines { command } => {
            let codex_home = find_codex_home()?;
            match command {
                PipelinesCommand::List {
                    project_root,
                    scope,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let merged =
                        merged_pipelines(codex_home.as_path(), project_root.as_deref(), scope)?;
                    let effective = merged.entries;
                    if json {
                        let global_path = global_pipelines_json_path(codex_home.as_path());
                        let project_path = project_root.as_deref().map(project_pipelines_json_path);
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "scope": scope,
                                "global_path": global_path,
                                "project_path": project_path,
                                "warnings": merged.warnings,
                                "effective": effective.values().collect::<Vec<_>>(),
                            }))?
                        );
                    } else if effective.is_empty() {
                        println!("No pipelines configured.");
                    } else {
                        for entry in effective.values() {
                            println!(
                                "{} ({}) enabled={} source={}",
                                entry.name,
                                entry.id,
                                entry.enabled,
                                entry.source.as_str()
                            );
                        }
                        if !merged.warnings.is_empty() {
                            println!();
                            println!("Warnings:");
                            for w in merged.warnings {
                                println!("- {w}");
                            }
                        }
                    }
                }
                PipelinesCommand::Show {
                    project_root,
                    scope,
                    id,
                    expanded,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let merged =
                        merged_pipelines(codex_home.as_path(), project_root.as_deref(), scope)?;
                    let effective = merged.entries;
                    let entry = effective
                        .get(&id)
                        .ok_or_else(|| anyhow::anyhow!("pipeline not found: {id}"))?;
                    let expanded_pipeline = if expanded {
                        let global_path = scoped_pipelines_json_path(
                            codex_home.as_path(),
                            project_root.as_deref(),
                            LayerScope::Global,
                        )?;
                        let global =
                            read_pipelines_file_any_from_path(global_path.as_path())?.into_v2();
                        let mut components = global.components;
                        if entry.source == LayerScope::Project {
                            let root = project_root.as_deref().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "project pipeline selected but no project root detected; pass --project-root"
                                )
                            })?;
                            if repo_trusted(Some(root), codex_home.as_path()) {
                                let project_path = scoped_pipelines_json_path(
                                    codex_home.as_path(),
                                    Some(root),
                                    LayerScope::Project,
                                )?;
                                let project =
                                    read_pipelines_file_any_from_path(project_path.as_path())?
                                        .into_v2();
                                for (k, v) in project.components {
                                    components.insert(k, v);
                                }
                            }
                        }
                        Some(expand_pipeline(
                            &entry.pipeline,
                            &components,
                            ExpandLimits::default(),
                        )?)
                    } else {
                        None
                    };
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "entry": entry,
                                "expanded": expanded_pipeline,
                                "warnings": merged.warnings,
                            }))?
                        );
                    } else {
                        println!("{}", serde_json::to_string_pretty(entry)?);
                        if !merged.warnings.is_empty() {
                            println!();
                            println!("Warnings:");
                            for w in merged.warnings {
                                println!("- {w}");
                            }
                        }
                    }
                }
                PipelinesCommand::Enable {
                    project_root,
                    scope,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = update_mode_b_config(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                        |root| set_toml_bool(root, &["pipelines", "enabled"], true),
                    )?;
                    println!("Enabled pipelines in {}", path.display());
                }
                PipelinesCommand::Disable {
                    project_root,
                    scope,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = update_mode_b_config(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                        |root| set_toml_bool(root, &["pipelines", "enabled"], false),
                    )?;
                    println!("Disabled pipelines in {}", path.display());
                }
                PipelinesCommand::Draft {
                    project_root,
                    scope,
                    id,
                    name,
                    apply,
                } => {
                    let repo_root = discover_project_root(project_root);
                    let repo_root = repo_root.as_deref();

                    let mut commands: Vec<Vec<String>> = Vec::new();
                    if repo_root.is_some_and(|root| root.join("Cargo.toml").exists()) {
                        commands.push(vec!["cargo".to_string(), "test".to_string()]);
                    }
                    if let Some(root) = repo_root
                        && root.join("package.json").exists()
                    {
                        let lockfiles = [
                            ("pnpm-lock.yaml", "pnpm"),
                            ("yarn.lock", "yarn"),
                            ("package-lock.json", "npm"),
                        ];
                        let pm = lockfiles
                            .iter()
                            .find(|(f, _)| root.join(f).exists())
                            .map(|(_, pm)| pm.to_string())
                            .unwrap_or_else(|| "npm".to_string());
                        commands.push(vec![pm, "test".to_string()]);
                    }
                    if commands.is_empty() {
                        commands.push(vec![
                            "echo".to_string(),
                            "TODO: add verify command".to_string(),
                        ]);
                    }

                    let parts = commands
                        .into_iter()
                        .map(|argv| codex_pr_pipelines::Part::RunCommand {
                            cwd: Some(".".to_string()),
                            argv: Some(argv),
                            command_id: None,
                            timeout_ms: None,
                            child_process_policy: codex_pr_types::ChildProcessPolicy::Inherit,
                        })
                        .collect::<Vec<_>>();
                    let mut workflows = BTreeMap::new();
                    workflows.insert(
                        "main".to_string(),
                        codex_pr_pipelines::Workflow {
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                            parts,
                            finally_workflow: None,
                        },
                    );
                    let pipeline = Pipeline {
                        workflows,
                        entry: "main".to_string(),
                    };

                    let entry = PipelineEntryV1 {
                        id: id.clone(),
                        name,
                        enabled: false,
                        pipeline,
                    };

                    if !apply {
                        println!("{}", serde_json::to_string_pretty(&entry)?);
                        return Ok(());
                    }

                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = scoped_pipelines_json_path(
                        codex_home.as_path(),
                        repo_root,
                        writable_scope,
                    )?;
                    let mut file = read_pipelines_file_any_from_path(path.as_path())?.into_v2();
                    file.schema_version = "2".to_string();
                    file.pipelines.insert(
                        id,
                        PipelineEntryV2 {
                            id: entry.id,
                            name: entry.name,
                            enabled: entry.enabled,
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                            pipeline: entry.pipeline,
                        },
                    );
                    let backup = write_pipelines_file(codex_home.as_path(), path.as_path(), &file)?;
                    if let Some(path) = backup {
                        println!("Updated pipelines.json (backup: {})", path.display());
                    } else {
                        println!("Updated pipelines.json");
                    }
                }
                PipelinesCommand::Restore {
                    project_root,
                    scope,
                    backup_path,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let target = scoped_pipelines_json_path(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                    )?;
                    let _ =
                        backup_file_if_present(&target, codex_home.as_path(), "pipelines_restore");
                    let bytes = std::fs::read(&backup_path)?;
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&target, bytes)?;
                    println!(
                        "Restored {} from {}",
                        target.display(),
                        backup_path.display()
                    );
                }
            }
        }

        Command::Workflows { command } => {
            let codex_home = find_codex_home()?;
            match command {
                WorkflowsCommand::List {
                    project_root,
                    scope,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let merged =
                        merged_workflows(codex_home.as_path(), project_root.as_deref(), scope)?;
                    let effective = merged.entries;
                    if json {
                        let global_path = global_workflows_json_path(codex_home.as_path());
                        let project_path = project_root.as_deref().map(project_workflows_json_path);
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "scope": scope,
                                "global_path": global_path,
                                "project_path": project_path,
                                "warnings": merged.warnings,
                                "effective": effective.values().collect::<Vec<_>>(),
                            }))?
                        );
                    } else if effective.is_empty() {
                        println!("No workflows configured.");
                    } else {
                        for entry in effective.values() {
                            println!(
                                "{} ({}) enabled={} source={}",
                                entry.name,
                                entry.id,
                                entry.enabled,
                                entry.source.as_str()
                            );
                        }
                        if !merged.warnings.is_empty() {
                            println!();
                            println!("Warnings:");
                            for w in merged.warnings {
                                println!("- {w}");
                            }
                        }
                    }
                }
                WorkflowsCommand::Show {
                    project_root,
                    scope,
                    id,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let merged =
                        merged_workflows(codex_home.as_path(), project_root.as_deref(), scope)?;
                    let effective = merged.entries;
                    let entry = effective
                        .get(&id)
                        .ok_or_else(|| anyhow::anyhow!("workflow not found: {id}"))?;
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "entry": entry,
                                "components": merged.components,
                                "warnings": merged.warnings,
                            }))?
                        );
                    } else {
                        println!("{}", serde_json::to_string_pretty(entry)?);
                        println!("components={}", merged.components.len());
                        if !merged.warnings.is_empty() {
                            println!();
                            println!("Warnings:");
                            for w in merged.warnings {
                                println!("- {w}");
                            }
                        }
                    }
                }
                WorkflowsCommand::Enable {
                    project_root,
                    scope,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = update_mode_b_config(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                        |root| set_toml_bool(root, &["workflows", "enabled"], true),
                    )?;
                    println!("Enabled workflows in {}", path.display());
                }
                WorkflowsCommand::Disable {
                    project_root,
                    scope,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = update_mode_b_config(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                        |root| set_toml_bool(root, &["workflows", "enabled"], false),
                    )?;
                    println!("Disabled workflows in {}", path.display());
                }
                WorkflowsCommand::Draft {
                    project_root,
                    scope,
                    id,
                    name,
                    apply,
                } => {
                    let project_root = discover_project_root(project_root);
                    let mut components = BTreeMap::new();
                    components.insert(
                        "generate_prd_component".to_string(),
                        WorkflowComponentV1 {
                            params: BTreeMap::new(),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                            step: WorkflowStepV1::GenerateArtifact {
                                template_id: "prd".to_string(),
                                artifact_kind: "prd".to_string(),
                                inputs: BTreeMap::new(),
                                next_step: None,
                                profile_refs: codex_pr_types::ProfileRefs::default(),
                            },
                        },
                    );
                    components.insert(
                        "revise_prd_component".to_string(),
                        WorkflowComponentV1 {
                            params: BTreeMap::new(),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                            step: WorkflowStepV1::ReviseArtifact {
                                template_id: "prd_revise".to_string(),
                                artifact_ref: "prd".to_string(),
                                feedback_key: "review_prd.feedback".to_string(),
                                next_step: None,
                                profile_refs: codex_pr_types::ProfileRefs::default(),
                            },
                        },
                    );
                    components.insert(
                        "generate_ux_component".to_string(),
                        WorkflowComponentV1 {
                            params: BTreeMap::new(),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                            step: WorkflowStepV1::GenerateArtifact {
                                template_id: "ux_plan".to_string(),
                                artifact_kind: "ux_plan".to_string(),
                                inputs: BTreeMap::new(),
                                next_step: None,
                                profile_refs: codex_pr_types::ProfileRefs::default(),
                            },
                        },
                    );
                    components.insert(
                        "revise_ux_component".to_string(),
                        WorkflowComponentV1 {
                            params: BTreeMap::new(),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                            step: WorkflowStepV1::ReviseArtifact {
                                template_id: "ux_plan_revise".to_string(),
                                artifact_ref: "ux_plan".to_string(),
                                feedback_key: "review_ux.feedback".to_string(),
                                next_step: None,
                                profile_refs: codex_pr_types::ProfileRefs::default(),
                            },
                        },
                    );
                    let mut steps = BTreeMap::new();
                    steps.insert(
                        "generate_prd".to_string(),
                        WorkflowStepV1::UseComponent {
                            component_id: "generate_prd_component".to_string(),
                            args: BTreeMap::new(),
                            next_step: Some("review_prd".to_string()),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                        },
                    );
                    steps.insert(
                        "review_prd".to_string(),
                        WorkflowStepV1::ReviewArtifact {
                            artifact_ref: "prd".to_string(),
                            prompt: "Review PRD and approve or provide feedback.".to_string(),
                            on_approved: "generate_ux".to_string(),
                            on_feedback: "revise_prd".to_string(),
                            max_revisions: 3,
                            revision_counter_key: "review_prd".to_string(),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                        },
                    );
                    steps.insert(
                        "revise_prd".to_string(),
                        WorkflowStepV1::UseComponent {
                            component_id: "revise_prd_component".to_string(),
                            args: BTreeMap::new(),
                            next_step: Some("review_prd".to_string()),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                        },
                    );
                    steps.insert(
                        "generate_ux".to_string(),
                        WorkflowStepV1::UseComponent {
                            component_id: "generate_ux_component".to_string(),
                            args: BTreeMap::new(),
                            next_step: Some("review_ux".to_string()),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                        },
                    );
                    steps.insert(
                        "review_ux".to_string(),
                        WorkflowStepV1::ReviewArtifact {
                            artifact_ref: "ux_plan".to_string(),
                            prompt: "Review UX plan and approve or provide feedback.".to_string(),
                            on_approved: "complete".to_string(),
                            on_feedback: "revise_ux".to_string(),
                            max_revisions: 3,
                            revision_counter_key: "review_ux".to_string(),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                        },
                    );
                    steps.insert(
                        "revise_ux".to_string(),
                        WorkflowStepV1::UseComponent {
                            component_id: "revise_ux_component".to_string(),
                            args: BTreeMap::new(),
                            next_step: Some("review_ux".to_string()),
                            profile_refs: codex_pr_types::ProfileRefs::default(),
                        },
                    );
                    steps.insert("complete".to_string(), WorkflowStepV1::Complete);

                    let entry = WorkflowEntryV1 {
                        id: id.clone(),
                        name,
                        enabled: false,
                        profile_refs: codex_pr_types::ProfileRefs::default(),
                        workflow: WorkflowGraphV1 {
                            entry: "generate_prd".to_string(),
                            steps,
                        },
                    };

                    if !apply {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "entry": entry,
                                "components": components,
                            }))?
                        );
                        return Ok(());
                    }

                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = scoped_workflows_json_path(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                    )?;
                    let mut file = read_workflows_file_from_path(path.as_path())?;
                    file.schema_version = "1".to_string();
                    for (component_id, component) in components {
                        file.components.insert(component_id, component);
                    }
                    file.workflows.insert(id, entry);
                    file.validate()
                        .map_err(|err| anyhow::anyhow!("invalid workflows draft: {err}"))?;
                    let backup = write_workflows_file_with_backup(
                        codex_home.as_path(),
                        path.as_path(),
                        &file,
                    )?;
                    if let Some(path) = backup {
                        println!("Updated workflows.json (backup: {})", path.display());
                    } else {
                        println!("Updated workflows.json");
                    }
                }
                WorkflowsCommand::Run {
                    project_root,
                    scope,
                    id,
                    run_id,
                    resume,
                    prompt,
                    model,
                    emit_actions_json,
                    action_result_json,
                    max_steps,
                } => {
                    let project_root = discover_project_root(project_root);
                    run_workflow_command(
                        codex_home.as_path(),
                        project_root,
                        scope,
                        id,
                        run_id,
                        resume,
                        prompt,
                        model,
                        emit_actions_json,
                        action_result_json,
                        max_steps,
                    )?;
                }
                WorkflowsCommand::Status { run_id, json } => {
                    let run_id = run_id
                        .map(|v| sanitize_run_id(&v))
                        .or_else(|| find_latest_workflow_run_id(codex_home.as_path(), None))
                        .ok_or_else(|| anyhow::anyhow!("no workflow runs found"))?;
                    let path = workflow_run_state_path(codex_home.as_path(), run_id.as_str());
                    if !path.exists() {
                        anyhow::bail!("run state file not found: {}", path.display());
                    }
                    let state = read_run_state(path.as_path())?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&state)?);
                    } else {
                        println!("run_id={}", state.run_id);
                        if let Some(workflow_id) = state.workflow_id.as_deref() {
                            println!("workflow_id={workflow_id}");
                        }
                        println!("status={:?}", state.status);
                        println!("current_step={}", state.current_step_id);
                        if let Some(pending) = state.pending.as_ref() {
                            println!("pending={}", serde_json::to_string(pending)?);
                        }
                        if let Some(last_error) = state.last_error.as_deref() {
                            println!("last_error={last_error}");
                        }
                        println!("artifacts={}", state.artifacts.len());
                    }
                }
                WorkflowsCommand::Cancel { run_id } => {
                    let run_id = run_id
                        .map(|v| sanitize_run_id(&v))
                        .or_else(|| find_latest_workflow_run_id(codex_home.as_path(), None))
                        .ok_or_else(|| anyhow::anyhow!("no workflow runs found"))?;
                    let path = workflow_run_state_path(codex_home.as_path(), run_id.as_str());
                    if !path.exists() {
                        anyhow::bail!("run state file not found: {}", path.display());
                    }
                    let mut state = read_run_state(path.as_path())?;
                    state.pending = None;
                    state.status = WorkflowRunStatus::Failed;
                    state.last_error = Some("cancelled by user".to_string());
                    write_run_state(path.as_path(), &state)?;
                    println!("Cancelled workflow run {}", state.run_id);
                }
                WorkflowsCommand::Restore {
                    project_root,
                    scope,
                    backup_path,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let target = scoped_workflows_json_path(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                    )?;
                    let _ =
                        backup_file_if_present(&target, codex_home.as_path(), "workflows_restore");
                    let bytes = std::fs::read(&backup_path)?;
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&target, bytes)?;
                    println!(
                        "Restored {} from {}",
                        target.display(),
                        backup_path.display()
                    );
                }
            }
        }

        Command::Backups { command } => {
            let codex_home = find_codex_home()?;
            match command {
                BackupsCommand::List { kind, json } => {
                    let root = codex_home.join("printrevolt").join("backups");
                    let mut out = Vec::<String>::new();
                    if let Some(kind) = kind.as_deref() {
                        let dir = root.join(kind);
                        if let Ok(entries) = std::fs::read_dir(&dir) {
                            for entry in entries.flatten() {
                                out.push(entry.path().display().to_string());
                            }
                        }
                    } else if let Ok(entries) = std::fs::read_dir(&root) {
                        for entry in entries.flatten() {
                            if entry.path().is_dir()
                                && let Ok(files) = std::fs::read_dir(entry.path())
                            {
                                for f in files.flatten() {
                                    out.push(f.path().display().to_string());
                                }
                            }
                        }
                    }
                    out.sort();
                    if json {
                        println!("{}", serde_json::to_string_pretty(&out)?);
                    } else {
                        for p in out {
                            println!("{p}");
                        }
                    }
                }
                BackupsCommand::Restore {
                    target,
                    scope,
                    project_root,
                    backup_path,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let (target_path, kind) = match target.as_str() {
                        "printrevolt-toml" => match writable_scope {
                            LayerScope::Global => (codex_home.join("printrevolt.toml"), "restore"),
                            LayerScope::Project => {
                                let root = project_root.as_deref().ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "project scope selected but no project root detected; pass --project-root"
                                    )
                                })?;
                                (root.join(".codex").join("printrevolt.toml"), "restore")
                            }
                        },
                        "pipelines-json" => (
                            scoped_pipelines_json_path(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                            )?,
                            "pipelines_restore",
                        ),
                        "workflows-json" => (
                            scoped_workflows_json_path(
                                codex_home.as_path(),
                                project_root.as_deref(),
                                writable_scope,
                            )?,
                            "workflows_restore",
                        ),
                        other => {
                            return Err(anyhow::anyhow!(
                                "unknown --target: {other} (expected printrevolt-toml or pipelines-json or workflows-json)"
                            ));
                        }
                    };
                    let _ = backup_file_if_present(&target_path, codex_home.as_path(), kind);
                    let bytes = std::fs::read(&backup_path)?;
                    if let Some(parent) = target_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&target_path, bytes)?;
                    println!(
                        "Restored {} from {}",
                        target_path.display(),
                        backup_path.display()
                    );
                }
            }
        }

        Command::Repo { command } => {
            let codex_home = find_codex_home()?;
            match command {
                RepoCommand::EnsureWorktree {
                    repo_root,
                    worktree_root,
                    naming,
                    branch_name,
                    base_branch,
                    apply,
                } => {
                    let repo_root = discover_project_root(repo_root).ok_or_else(|| {
                        anyhow::anyhow!("failed to detect repo_root; pass --repo-root")
                    })?;
                    let _lock = codex_pr_repo_ops::acquire_worktree_lock(&worktree_root)?;
                    let (target, plan) =
                        codex_pr_repo_ops::ensure_worktree_plan(WorktreeEnsureArgs {
                            repo_root: repo_root.as_path(),
                            worktree_root: worktree_root.as_path(),
                            naming: naming.as_str(),
                            branch_name: branch_name.as_str(),
                            base_branch: base_branch.as_str(),
                        })?;
                    print_plan_or_apply(&plan, apply)?;
                    if apply {
                        println!("worktree_path={}", target.display());
                    }
                }
                RepoCommand::EnsureBranch {
                    repo_root,
                    base_branch,
                    branch_name,
                    protected,
                    apply,
                } => {
                    let repo_root = discover_project_root(repo_root).ok_or_else(|| {
                        anyhow::anyhow!("failed to detect repo_root; pass --repo-root")
                    })?;
                    let protected_refs = protected
                        .iter()
                        .map(std::string::String::as_str)
                        .collect::<Vec<_>>();
                    let plan = codex_pr_repo_ops::ensure_branch_plan(BranchEnsureArgs {
                        repo_root: repo_root.as_path(),
                        base_branch: base_branch.as_str(),
                        branch_name: branch_name.as_str(),
                        protected_branches: protected_refs.as_slice(),
                    })?;
                    print_plan_or_apply(&plan, apply)?;
                }
                RepoCommand::RemoveWorktree {
                    repo_root,
                    worktree_root,
                    path,
                    force,
                    apply,
                } => {
                    let repo_root = discover_project_root(repo_root).ok_or_else(|| {
                        anyhow::anyhow!("failed to detect repo_root; pass --repo-root")
                    })?;
                    let _lock = codex_pr_repo_ops::acquire_worktree_lock(&worktree_root)?;
                    let plan = codex_pr_repo_ops::remove_worktree_plan(
                        repo_root.as_path(),
                        worktree_root.as_path(),
                        path.as_path(),
                        force,
                    )?;
                    print_plan_or_apply(&plan, apply)?;
                }
            }
            let _ = codex_home;
        }

        Command::Templates { command } => {
            let codex_home = find_codex_home()?;
            match command {
                TemplatesCommand::List {
                    project_root,
                    scope,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let res = discover_templates_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        scope,
                    );
                    let refs = res
                        .templates
                        .iter()
                        .map(codex_pr_templates::template_ref)
                        .collect::<Vec<_>>();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "scope": scope,
                                "templates": refs,
                                "warnings": res.warnings,
                            }))?
                        );
                    } else {
                        for t in refs {
                            println!("- {} ({:?})", t.name, t.source);
                        }
                        if !res.warnings.is_empty() {
                            println!();
                            println!("Warnings:");
                            for w in res.warnings {
                                println!("- {w}");
                            }
                        }
                    }
                }
                TemplatesCommand::Validate {
                    project_root,
                    scope,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let res = discover_templates_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        scope,
                    );
                    if res.warnings.is_empty() {
                        println!("ok");
                    } else {
                        println!("Warnings:");
                        for w in res.warnings {
                            println!("- {w}");
                        }
                    }
                }
                TemplatesCommand::Enable {
                    project_root,
                    scope,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = update_mode_b_config(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                        |root| set_toml_string(root, &["templates", "selection_mode"], "once"),
                    )?;
                    println!("Enabled templates in {}", path.display());
                }
                TemplatesCommand::Disable {
                    project_root,
                    scope,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let path = update_mode_b_config(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        writable_scope,
                        |root| set_toml_string(root, &["templates", "selection_mode"], "off"),
                    )?;
                    println!("Disabled templates in {}", path.display());
                }
                TemplatesCommand::Draft {
                    project_root,
                    scope,
                    id,
                    name,
                    description,
                    tag,
                    prompt,
                } => {
                    let project_root = discover_project_root(project_root);
                    let writable_scope = ensure_writable_scope(scope, "--scope")?;
                    let dir = match writable_scope {
                        LayerScope::Global => {
                            codex_home.as_path().join("printrevolt").join("drafts")
                        }
                        LayerScope::Project => {
                            let root = project_root.as_deref().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "project scope selected but no project root detected; pass --project-root"
                                )
                            })?;
                            root.join(".codex").join("templates")
                        }
                    };
                    std::fs::create_dir_all(&dir)?;
                    let path = dir.join(format!("{id}.md"));
                    let prompt = prompt.chars().take(20_000).collect::<String>();
                    let tags_yaml = if tag.is_empty() {
                        "[]".to_string()
                    } else {
                        format!(
                            "[{}]",
                            tag.iter()
                                .map(|t| format!("{t:?}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let doc = format!(
                        r#"---
name: {name}
description: {description}
tags: {tags_yaml}
defaults:
  policy:
    deny_dangerous_always: true
    verify_required: true
---

## Role + Objective
{name}

## Procedure
- Follow the user prompt.
- Prefer deterministic steps and verification.

## Outputs
- A concise summary of changes.
- Verification status.

## Policy Defaults
- deny_dangerous_always: true
- verify_required: true

## Tooling Scope
- Stay within the repo unless explicitly approved.

## Prompt
{prompt}
"#
                    );
                    std::fs::write(&path, doc)?;
                    println!("{}", path.display());
                }
                TemplatesCommand::ComposePrompt {
                    project_root,
                    scope,
                    template_id,
                    prompt,
                    json,
                } => {
                    let project_root =
                        require_project_root(discover_project_root(project_root), scope)?;
                    let res = discover_templates_for_scope(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        scope,
                    );
                    let Some(t) = res.templates.iter().find(|t| t.id == template_id) else {
                        anyhow::bail!("template not found: {template_id}");
                    };
                    let generated = codex_pr_templates::compose_prompt(&prompt, t);
                    let digest = Sha256::digest(generated.as_bytes());
                    let hash = to_lower_hex(&digest);
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "scope": scope,
                                "template_id": template_id,
                                "generated_prompt": generated,
                                "generated_prompt_sha256": hash,
                                "warnings": res.warnings,
                            }))?
                        );
                    } else {
                        println!("{generated}");
                        println!();
                        println!("sha256: {hash}");
                    }
                }
            }
        }

        Command::Update { command } => match command {
            UpdateCommand::Check {
                package,
                current,
                latest,
                json,
            } => {
                let latest = match latest {
                    Some(v) => v,
                    None => fetch_latest_npm_version(package.as_str())?,
                };
                let res = codex_pr_updater::check_update(
                    UpdateCheckRequest {
                        channel: UpdateChannel::Npm,
                        package,
                        current_version: current,
                    },
                    latest,
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&res)?);
                } else if res.update_available {
                    println!(
                        "Update available: {} -> {}",
                        res.current_version, res.latest_version
                    );
                } else {
                    println!("Up to date: {}", res.current_version);
                }
            }
            UpdateCommand::Plan {
                package,
                current,
                target,
                apply,
            } => {
                let plan = codex_pr_updater::plan_update_npm(
                    package.as_str(),
                    current.as_str(),
                    target.as_str(),
                )?;
                println!("{}", serde_json::to_string_pretty(&plan)?);
                if apply {
                    run_argv(&plan.command)?;
                }
            }
        },
    }
    Ok(())
}

fn print_plan_or_apply(plan: &RepoOpPlan, apply: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(plan)?);
    if apply {
        codex_pr_repo_ops::execute_plan(plan)?;
    }
    Ok(())
}

fn doctor_recommend(project_root: Option<&Path>) -> Result<codex_pr_advisor::RecommendationBundle> {
    let package_json = project_root
        .map(|r| r.join("package.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.chars().take(128 * 1024).collect::<String>());

    let lockfiles = ["pnpm-lock.yaml", "yarn.lock", "package-lock.json"];
    let present = project_root
        .map(|r| {
            lockfiles
                .iter()
                .copied()
                .filter(|f| r.join(f).exists())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let existing_commands_json = None;
    let existing_pipelines = None;

    codex_pr_advisor::recommend(codex_pr_advisor::AdvisorInput {
        repo_root: project_root,
        package_json: package_json.as_deref(),
        lockfiles: present.as_slice(),
        existing_commands_json,
        existing_pipelines,
    })
    .map_err(|e| anyhow::anyhow!("{e}"))
}

fn fetch_latest_npm_version(package: &str) -> Result<String> {
    let output = std::process::Command::new("npm")
        .args(["view", package, "version"])
        .output();
    let output = match output {
        Ok(o) => o,
        Err(err) => {
            return Err(anyhow::anyhow!(
                "npm not available for update checks: {err}"
            ));
        }
    };
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "npm view failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_argv(argv: &[String]) -> Result<()> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty argv"))?;
    let status = std::process::Command::new(program).args(args).status()?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "command failed (exit {:?}): {}",
            status.code(),
            argv.join(" ")
        ));
    }
    Ok(())
}
