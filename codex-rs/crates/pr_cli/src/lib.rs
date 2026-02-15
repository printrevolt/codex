use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use codex_pr_config::flatten_printrevolt;
use codex_pr_config::resolve_printrevolt_config;
use codex_pr_repo_ops::BranchEnsureArgs;
use codex_pr_repo_ops::RepoOpPlan;
use codex_pr_repo_ops::WorktreeEnsureArgs;
use codex_pr_updater::UpdateChannel;
use codex_pr_updater::UpdateCheckRequest;
use codex_utils_home_dir::find_codex_home;

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

        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Validate all discovered templates; prints warnings and exits 0.
    Validate {
        /// Override project root (defaults to searching upward from cwd for .git).
        #[arg(long)]
        project_root: Option<PathBuf>,
    },

    /// Create a draft template from a prompt (writes to CODEX_HOME/printrevolt/drafts/).
    Draft {
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
                TemplatesCommand::List { project_root, json } => {
                    let project_root = discover_project_root(project_root);
                    let trusted = repo_trusted(project_root.as_deref(), codex_home.as_path());
                    let res = codex_pr_templates::discover_templates(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        trusted,
                        codex_pr_templates::TemplateDiscoveryConfig::default(),
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
                TemplatesCommand::Validate { project_root } => {
                    let project_root = discover_project_root(project_root);
                    let trusted = repo_trusted(project_root.as_deref(), codex_home.as_path());
                    let res = codex_pr_templates::discover_templates(
                        codex_home.as_path(),
                        project_root.as_deref(),
                        trusted,
                        codex_pr_templates::TemplateDiscoveryConfig::default(),
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
                TemplatesCommand::Draft {
                    id,
                    name,
                    description,
                    tag,
                    prompt,
                } => {
                    let drafts_dir = codex_home.as_path().join("printrevolt").join("drafts");
                    std::fs::create_dir_all(&drafts_dir)?;
                    let path = drafts_dir.join(format!("{id}.md"));
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
