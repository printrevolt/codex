use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

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
use codex_pr_repo_ops::BranchEnsureArgs;
use codex_pr_repo_ops::RepoOpPlan;
use codex_pr_repo_ops::WorktreeEnsureArgs;
use codex_pr_types::WorkflowEntryV1;
use codex_pr_types::WorkflowGraphV1;
use codex_pr_types::WorkflowStepV1;
use codex_pr_types::WorkflowsFileV1;
use codex_pr_updater::UpdateChannel;
use codex_pr_updater::UpdateCheckRequest;
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
    source: LayerScope,
    pipeline: Pipeline,
}

#[derive(Debug, Clone, serde::Serialize)]
struct WorkflowResolvedEntryV1 {
    id: String,
    name: String,
    enabled: bool,
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
                                pipeline: v.pipeline,
                            },
                        )
                    })
                    .collect(),
            },
        }
    }
}

fn read_workflows_file_from_path(path: &Path) -> Result<WorkflowsFileV1> {
    if !path.exists() {
        return Ok(WorkflowsFileV1::default());
    }
    let raw = std::fs::read_to_string(path)?;
    let file = serde_json::from_str::<WorkflowsFileV1>(&raw)?;
    file.validate()
        .map_err(|err| anyhow::anyhow!("invalid workflows.json ({}): {err}", path.display()))?;
    Ok(file)
}

fn write_workflows_file(
    codex_home: &Path,
    path: &Path,
    file: &WorkflowsFileV1,
) -> Result<Option<PathBuf>> {
    let backup = backup_file_if_present(path, codex_home, "workflows");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_string_pretty(file)?;
    std::fs::write(path, encoded)?;
    Ok(backup)
}

fn merged_workflows(
    codex_home: &Path,
    project_root: Option<&Path>,
    scope: Scope,
) -> Result<MergedWorkflows> {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::<String>::new();

    if matches!(scope, Scope::Global | Scope::Both) {
        let global_path = scoped_workflows_json_path(codex_home, project_root, LayerScope::Global)?;
        let global = read_workflows_file_from_path(global_path.as_path())?;
        for (id, entry) in global.workflows {
            out.insert(
                id,
                WorkflowResolvedEntryV1 {
                    id: entry.id,
                    name: entry.name,
                    enabled: entry.enabled,
                    source: LayerScope::Global,
                    workflow: entry.workflow,
                },
            );
        }
    }
    if matches!(scope, Scope::Project | Scope::Both) {
        if let Some(project_root) = project_root {
            let trusted = repo_trusted(Some(project_root), codex_home);
            if !trusted {
                warnings.push(format!(
                    "Repo workflows bundle ignored (repo not trusted): {}",
                    project_root.display()
                ));
            } else {
                let project_path = scoped_workflows_json_path(
                    codex_home,
                    Some(project_root),
                    LayerScope::Project,
                )?;
                let project = read_workflows_file_from_path(project_path.as_path())?;
                for (id, entry) in project.workflows {
                    out.insert(
                        id,
                        WorkflowResolvedEntryV1 {
                            id: entry.id,
                            name: entry.name,
                            enabled: entry.enabled,
                            source: LayerScope::Project,
                            workflow: entry.workflow,
                        },
                    );
                }
            }
        }
    }
    Ok(MergedWorkflows {
        entries: out,
        warnings,
    })
}

#[derive(Debug, Clone)]
struct MergedWorkflows {
    entries: BTreeMap<String, WorkflowResolvedEntryV1>,
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
                    source: LayerScope::Global,
                    pipeline: entry.pipeline,
                },
            );
        }
    }
    if matches!(scope, Scope::Project | Scope::Both) {
        if let Some(project_root) = project_root {
            let trusted = repo_trusted(Some(project_root), codex_home);
            if !trusted {
                warnings.push(format!(
                    "Repo pipelines bundle ignored (repo not trusted): {}",
                    project_root.display()
                ));
            } else {
                let project_path = scoped_pipelines_json_path(
                    codex_home,
                    Some(project_root),
                    LayerScope::Project,
                )?;
                let project = read_pipelines_file_any_from_path(project_path.as_path())?.into_v2();
                for (id, entry) in project.pipelines {
                    // Project entries override global entries with the same id.
                    out.insert(
                        id,
                        PipelineResolvedEntryV1 {
                            id: entry.id,
                            name: entry.name,
                            enabled: entry.enabled,
                            source: LayerScope::Project,
                            pipeline: entry.pipeline,
                        },
                    );
                }
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

fn backup_file_if_present(target: &Path, codex_home: &Path, kind: &str) -> Option<PathBuf> {
    if !target.exists() {
        return None;
    }
    let file_name = target.file_name()?.to_string_lossy().to_string();
    let dir = codex_home
        .join("printrevolt")
        .join("backups")
        .join(kind.to_string());
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
    let backup = backup_file_if_present(&path, codex_home, "pipelines");
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
                if json {
                    println!("{}", serde_json::to_string_pretty(&bundle)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&bundle)?);
                }
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
                        name: name.clone(),
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
                        id.clone(),
                        PipelineEntryV2 {
                            id: entry.id,
                            name: entry.name,
                            enabled: entry.enabled,
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
                    let mut steps = BTreeMap::new();
                    steps.insert(
                        "generate_prd".to_string(),
                        WorkflowStepV1::GenerateArtifact {
                            template_id: "prd".to_string(),
                            artifact_kind: "prd".to_string(),
                            inputs: BTreeMap::new(),
                            next_step: Some("review_prd".to_string()),
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
                        },
                    );
                    steps.insert(
                        "revise_prd".to_string(),
                        WorkflowStepV1::ReviseArtifact {
                            template_id: "prd_revise".to_string(),
                            artifact_ref: "prd".to_string(),
                            feedback_key: "review_prd.feedback".to_string(),
                            next_step: Some("review_prd".to_string()),
                        },
                    );
                    steps.insert(
                        "generate_ux".to_string(),
                        WorkflowStepV1::GenerateArtifact {
                            template_id: "ux_plan".to_string(),
                            artifact_kind: "ux_plan".to_string(),
                            inputs: BTreeMap::new(),
                            next_step: Some("review_ux".to_string()),
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
                        },
                    );
                    steps.insert(
                        "revise_ux".to_string(),
                        WorkflowStepV1::ReviseArtifact {
                            template_id: "ux_plan_revise".to_string(),
                            artifact_ref: "ux_plan".to_string(),
                            feedback_key: "review_ux.feedback".to_string(),
                            next_step: Some("review_ux".to_string()),
                        },
                    );
                    steps.insert("complete".to_string(), WorkflowStepV1::Complete);

                    let entry = WorkflowEntryV1 {
                        id: id.clone(),
                        name: name.clone(),
                        enabled: false,
                        workflow: WorkflowGraphV1 {
                            entry: "generate_prd".to_string(),
                            steps,
                        },
                    };

                    if !apply {
                        println!("{}", serde_json::to_string_pretty(&entry)?);
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
                    file.workflows.insert(id.clone(), entry);
                    file.validate()
                        .map_err(|err| anyhow::anyhow!("invalid workflows draft: {err}"))?;
                    let backup = write_workflows_file(codex_home.as_path(), path.as_path(), &file)?;
                    if let Some(path) = backup {
                        println!("Updated workflows.json (backup: {})", path.display());
                    } else {
                        println!("Updated workflows.json");
                    }
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
                            if entry.path().is_dir() {
                                if let Ok(files) = std::fs::read_dir(entry.path()) {
                                    for f in files.flatten() {
                                        out.push(f.path().display().to_string());
                                    }
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
                    let protected_refs = protected.iter().map(|s| s.as_str()).collect::<Vec<_>>();
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
                                .map(|t| format!("{:?}", t))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let doc = format!(
                        r#"---
name: {name}
description: {description}
tags: {tags}
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
"#,
                        name = name,
                        description = description,
                        tags = tags_yaml,
                        prompt = prompt
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
