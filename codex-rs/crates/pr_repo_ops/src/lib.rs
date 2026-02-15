use std::fs::File;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use fs2::FileExt;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RepoOpsError {
    #[error("not a git repository: {0}")]
    NotAGitRepo(String),
    #[error("invalid naming path (must be relative, no ..): {0}")]
    InvalidNaming(String),
    #[error("target path escapes worktree root")]
    WorktreeEscapesRoot,
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitCommand {
    pub cwd: PathBuf,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoOpPlan {
    pub action: String,
    pub action_hash: String,
    pub commands: Vec<GitCommand>,
    pub notes: Vec<String>,
}

pub struct WorktreeEnsureArgs<'a> {
    pub repo_root: &'a Path,
    pub worktree_root: &'a Path,
    pub naming: &'a str,
    pub branch_name: &'a str,
    pub base_branch: &'a str,
}

pub fn ensure_worktree_plan(
    args: WorktreeEnsureArgs<'_>,
) -> Result<(PathBuf, RepoOpPlan), RepoOpsError> {
    ensure_git_repo(args.repo_root)?;
    let target_rel = normalize_relative_path(Path::new(args.naming))?;
    let root_canon = canonicalize_existing(args.worktree_root)?;
    let target = root_canon.join(&target_rel);
    if !target.starts_with(&root_canon) {
        return Err(RepoOpsError::WorktreeEscapesRoot);
    }

    let mut notes = Vec::new();
    notes.push(format!(
        "Target worktree path: {}",
        target.display().to_string()
    ));
    notes.push("Note: creating a worktree does not automatically move the current Codex session. Restart the session in the new worktree if needed.".to_string());

    let commands = vec![GitCommand {
        cwd: args.repo_root.to_path_buf(),
        argv: vec![
            "git".to_string(),
            "worktree".to_string(),
            "add".to_string(),
            target.display().to_string(),
            "-b".to_string(),
            args.branch_name.to_string(),
            args.base_branch.to_string(),
        ],
    }];

    let plan = RepoOpPlan {
        action: "ensure_worktree".to_string(),
        action_hash: hash_plan("ensure_worktree", &commands),
        commands,
        notes,
    };
    Ok((target, plan))
}

pub struct BranchEnsureArgs<'a> {
    pub repo_root: &'a Path,
    pub base_branch: &'a str,
    pub branch_name: &'a str,
    pub protected_branches: &'a [&'a str],
}

pub fn ensure_branch_plan(args: BranchEnsureArgs<'_>) -> Result<RepoOpPlan, RepoOpsError> {
    ensure_git_repo(args.repo_root)?;
    if args
        .protected_branches
        .iter()
        .any(|b| *b == args.branch_name)
    {
        return Err(RepoOpsError::GitFailed(format!(
            "refusing to create/switch to protected branch: {}",
            args.branch_name
        )));
    }

    let commands = vec![
        GitCommand {
            cwd: args.repo_root.to_path_buf(),
            argv: vec![
                "git".to_string(),
                "fetch".to_string(),
                "--all".to_string(),
                "--prune".to_string(),
            ],
        },
        GitCommand {
            cwd: args.repo_root.to_path_buf(),
            argv: vec![
                "git".to_string(),
                "checkout".to_string(),
                "-B".to_string(),
                args.branch_name.to_string(),
                args.base_branch.to_string(),
            ],
        },
    ];

    Ok(RepoOpPlan {
        action: "ensure_branch".to_string(),
        action_hash: hash_plan("ensure_branch", &commands),
        commands,
        notes: vec![],
    })
}

pub fn remove_worktree_plan(
    repo_root: &Path,
    worktree_root: &Path,
    path: &Path,
    force: bool,
) -> Result<RepoOpPlan, RepoOpsError> {
    ensure_git_repo(repo_root)?;
    let root_canon = canonicalize_existing(worktree_root)?;
    let target = path_absolutize_under_root(&root_canon, path)?;
    if !target.starts_with(&root_canon) {
        return Err(RepoOpsError::WorktreeEscapesRoot);
    }

    let mut argv = vec![
        "git".to_string(),
        "worktree".to_string(),
        "remove".to_string(),
        target.display().to_string(),
    ];
    if force {
        argv.push("--force".to_string());
    }
    let commands = vec![GitCommand {
        cwd: repo_root.to_path_buf(),
        argv,
    }];

    Ok(RepoOpPlan {
        action: "remove_worktree".to_string(),
        action_hash: hash_plan("remove_worktree", &commands),
        commands,
        notes: vec![format!("Target worktree path: {}", target.display())],
    })
}

pub fn execute_plan(plan: &RepoOpPlan) -> Result<(), RepoOpsError> {
    for cmd in &plan.commands {
        run(cmd.cwd.as_path(), &cmd.argv)?;
    }
    Ok(())
}

pub fn acquire_worktree_lock(worktree_root: &Path) -> Result<File, RepoOpsError> {
    std::fs::create_dir_all(worktree_root)?;
    let root = canonicalize_existing(worktree_root)?;
    let lock_path = root.join(".codex-pr-worktree.lock");
    let file = File::options().create(true).write(true).open(lock_path)?;
    file.try_lock_exclusive()?;
    Ok(file)
}

fn ensure_git_repo(repo_root: &Path) -> Result<(), RepoOpsError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(RepoOpsError::Io)?;
    if !output.status.success() {
        return Err(RepoOpsError::NotAGitRepo(repo_root.display().to_string()));
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s != "true" {
        return Err(RepoOpsError::NotAGitRepo(repo_root.display().to_string()));
    }
    Ok(())
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf, RepoOpsError> {
    if path.exists() {
        Ok(path.canonicalize()?)
    } else {
        // For non-existent roots, resolve the nearest existing parent then re-append.
        let mut cur = path.to_path_buf();
        let mut suffix = Vec::<PathBuf>::new();
        while !cur.exists() {
            let file_name = cur
                .file_name()
                .map(|s| PathBuf::from(s))
                .unwrap_or_default();
            suffix.push(file_name);
            if !cur.pop() {
                break;
            }
        }
        let mut base = match cur.canonicalize() {
            Ok(p) => p,
            Err(_) => return Ok(path.to_path_buf()),
        };
        while let Some(seg) = suffix.pop() {
            if !seg.as_os_str().is_empty() {
                base.push(seg);
            }
        }
        Ok(base)
    }
}

pub fn normalize_relative_path(path: &Path) -> Result<PathBuf, RepoOpsError> {
    let mut result = PathBuf::new();
    let mut saw_component = false;
    for component in path.components() {
        saw_component = true;
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return Err(RepoOpsError::InvalidNaming(path.display().to_string()));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(RepoOpsError::InvalidNaming(path.display().to_string()));
            }
        }
    }
    if !saw_component || result.as_os_str().is_empty() {
        return Err(RepoOpsError::InvalidNaming(path.display().to_string()));
    }
    Ok(result)
}

fn path_absolutize_under_root(root: &Path, path: &Path) -> Result<PathBuf, RepoOpsError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let rel = normalize_relative_path(path)?;
    Ok(root.join(rel))
}

fn run(cwd: &Path, argv: &[String]) -> Result<(), RepoOpsError> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| RepoOpsError::GitFailed("empty argv".to_string()))?;
    let status = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .status()
        .map_err(RepoOpsError::Io)?;
    if !status.success() {
        return Err(RepoOpsError::GitFailed(format!(
            "{} (exit {:?})",
            argv.join(" "),
            status.code()
        )));
    }
    Ok(())
}

fn hash_plan(action: &str, commands: &[GitCommand]) -> String {
    let payload = serde_json::json!({
        "action": action,
        "commands": commands,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    to_lower_hex(&digest)
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn normalize_relative_path_rejects_absolute_and_dotdot() {
        assert!(normalize_relative_path(Path::new("/abs")).is_err());
        assert!(normalize_relative_path(Path::new("../x")).is_err());
        assert!(normalize_relative_path(Path::new("a/../../b")).is_err());
    }

    #[test]
    fn normalize_relative_path_allows_subdirs() {
        let p = normalize_relative_path(Path::new("prcc/s1")).unwrap();
        assert_eq!(p, PathBuf::from("prcc").join("s1"));
    }

    #[test]
    fn worktree_plan_hash_stable() {
        let tmp = tempdir().unwrap();
        // No git needed; we only hash commands.
        let commands = vec![GitCommand {
            cwd: tmp.path().to_path_buf(),
            argv: vec!["git".to_string(), "status".to_string()],
        }];
        let h1 = hash_plan("x", &commands);
        let h2 = hash_plan("x", &commands);
        assert_eq!(h1, h2);
    }
}
