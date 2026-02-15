use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateSource {
    Repo,
    User,
    Draft,
}

pub type TemplateId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateRef {
    pub id: TemplateId,
    pub name: String,
    pub source: TemplateSource,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TemplateDefaults {
    #[serde(default)]
    pub policy: PolicyDefaults,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PolicyDefaults {
    #[serde(default)]
    pub deny_dangerous_always: Option<bool>,
    #[serde(default)]
    pub verify_required: Option<bool>,
    #[serde(default)]
    pub verify_max_age_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateContract {
    pub role_objective: String,
    pub procedure: String,
    pub outputs: String,
    pub policy_defaults: String,
    pub tooling_scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub id: TemplateId,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub source: TemplateSource,
    pub path: Option<PathBuf>,
    pub defaults: TemplateDefaults,
    pub body_markdown: String,
    pub contract: TemplateContract,
}

#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("missing or invalid YAML frontmatter")]
    MissingFrontmatter,
    #[error("invalid YAML frontmatter: {0}")]
    InvalidFrontmatter(String),
    #[error("missing required frontmatter key: {0}")]
    MissingFrontmatterKey(&'static str),
    #[error("template missing required section heading: {0}")]
    MissingSection(&'static str),
    #[error("template too large: {0} bytes")]
    TooLarge(usize),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct TemplateDiscoveryConfig {
    pub max_bytes: usize,
}

impl Default for TemplateDiscoveryConfig {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TemplateDiscoveryResult {
    pub templates: Vec<Template>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    defaults: Option<TemplateDefaults>,
}

pub fn discover_templates(
    codex_home: &Path,
    repo_root: Option<&Path>,
    repo_trusted: bool,
    cfg: TemplateDiscoveryConfig,
) -> TemplateDiscoveryResult {
    let mut warnings = Vec::new();
    let mut templates = Vec::new();

    let user_dir = codex_home.join("templates");
    load_from_dir(
        &user_dir,
        TemplateSource::User,
        "user",
        cfg.max_bytes,
        &mut templates,
        &mut warnings,
    );

    if repo_trusted {
        if let Some(repo_root) = repo_root {
            let repo_dir = repo_root.join(".codex").join("templates");
            load_from_dir(
                &repo_dir,
                TemplateSource::Repo,
                "repo",
                cfg.max_bytes,
                &mut templates,
                &mut warnings,
            );
        }
    } else if repo_root.is_some() {
        warnings.push("repo templates are disabled (repo not trusted)".to_string());
    }

    TemplateDiscoveryResult {
        templates,
        warnings,
    }
}

pub fn template_ref(t: &Template) -> TemplateRef {
    TemplateRef {
        id: t.id.clone(),
        name: t.name.clone(),
        source: t.source.clone(),
        description: t.description.clone(),
        tags: t.tags.clone(),
    }
}

pub fn clamp_policy_defaults(
    base: &codex_pr_types::PolicyConfig,
    defaults: &PolicyDefaults,
) -> codex_pr_types::PolicyConfig {
    let mut out = base.clone();
    if let Some(true) = defaults.deny_dangerous_always {
        out.deny_dangerous_always = true;
    }
    if let Some(req) = defaults.verify_required {
        out.verify.required = out.verify.required || req;
    }
    if let Some(max_age_ms) = defaults.verify_max_age_ms {
        if max_age_ms < out.verify.max_age_ms {
            out.verify.max_age_ms = max_age_ms;
        }
    }
    out
}

fn load_from_dir(
    dir: &Path,
    source: TemplateSource,
    source_prefix: &str,
    max_bytes: usize,
    out: &mut Vec<Template>,
    warnings: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        match load_template(&path, dir, source.clone(), source_prefix, max_bytes) {
            Ok(t) => out.push(t),
            Err(err) => warnings.push(format!("ignored template {}: {err}", path.display())),
        }
    }
}

fn load_template(
    path: &Path,
    root_dir: &Path,
    source: TemplateSource,
    source_prefix: &str,
    max_bytes: usize,
) -> Result<Template, TemplateError> {
    let raw = std::fs::read_to_string(path)?;
    if raw.len() > max_bytes {
        return Err(TemplateError::TooLarge(raw.len()));
    }

    let (frontmatter, body) = split_frontmatter(&raw)?;
    let fm: Frontmatter = serde_yaml::from_str(frontmatter)
        .map_err(|e| TemplateError::InvalidFrontmatter(e.to_string()))?;

    let name = fm
        .name
        .ok_or(TemplateError::MissingFrontmatterKey("name"))?;
    let description = fm
        .description
        .ok_or(TemplateError::MissingFrontmatterKey("description"))?;
    let tags = fm.tags;
    let defaults = fm.defaults.unwrap_or_default();

    let contract = extract_contract_sections(&body)?;

    let rel = path.strip_prefix(root_dir).unwrap_or(path);
    let id = format!(
        "{source_prefix}:{}",
        rel.display().to_string().replace('\\', "/")
    );

    Ok(Template {
        id,
        name,
        description,
        tags,
        source,
        path: Some(path.to_path_buf()),
        defaults,
        body_markdown: body.to_string(),
        contract,
    })
}

fn split_frontmatter(raw: &str) -> Result<(&str, &str), TemplateError> {
    let mut lines = raw.lines();
    let first = lines.next().ok_or(TemplateError::MissingFrontmatter)?;
    if first.trim() != "---" {
        return Err(TemplateError::MissingFrontmatter);
    }

    let mut offset = first.len() + 1;
    for line in lines {
        if line.trim() == "---" {
            let front = &raw[first.len() + 1..offset - 1];
            let body = &raw[offset + line.len() + 1..];
            return Ok((front, body));
        }
        offset += line.len() + 1;
    }
    Err(TemplateError::MissingFrontmatter)
}

fn extract_contract_sections(body: &str) -> Result<TemplateContract, TemplateError> {
    let mut sections = BTreeMap::<&'static str, String>::new();
    let mut current: Option<&'static str> = None;
    let mut buf = String::new();

    for line in body.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            if let Some(key) = current.take() {
                sections.insert(key, buf.trim().to_string());
            }
            buf.clear();
            current = match normalize_heading(h) {
                Some(k) => Some(k),
                None => None,
            };
            continue;
        }
        if current.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(key) = current.take() {
        sections.insert(key, buf.trim().to_string());
    }

    Ok(TemplateContract {
        role_objective: must_section(&sections, "role_objective")?,
        procedure: must_section(&sections, "procedure")?,
        outputs: must_section(&sections, "outputs")?,
        policy_defaults: must_section(&sections, "policy_defaults")?,
        tooling_scope: must_section(&sections, "tooling_scope")?,
    })
}

fn must_section(
    map: &BTreeMap<&'static str, String>,
    key: &'static str,
) -> Result<String, TemplateError> {
    map.get(key)
        .cloned()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            TemplateError::MissingSection(match key {
                "role_objective" => "## Role + Objective",
                "procedure" => "## Procedure",
                "outputs" => "## Outputs",
                "policy_defaults" => "## Policy Defaults",
                "tooling_scope" => "## Tooling Scope",
                _ => "## <unknown>",
            })
        })
}

fn normalize_heading(h: &str) -> Option<&'static str> {
    let n = h.trim().to_lowercase();
    match n.as_str() {
        "role + objective" | "role and objective" => Some("role_objective"),
        "procedure" => Some("procedure"),
        "outputs" => Some("outputs"),
        "policy defaults" => Some("policy_defaults"),
        "tooling scope" => Some("tooling_scope"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_and_validates_template() {
        let tmp = tempdir().unwrap();
        let codex_home = tmp.path().join("home");
        let dir = codex_home.join("templates");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("security.md");
        std::fs::write(
            &p,
            r#"---
name: Security Reviewer
description: Reviews for security issues
tags: [security]
defaults:
  policy:
    deny_dangerous_always: true
    verify_required: true
---

## Role + Objective
Be strict.

## Procedure
Do steps.

## Outputs
List issues.

## Policy Defaults
Tighten policy.

## Tooling Scope
No network.
"#,
        )
        .unwrap();

        let res = discover_templates(&codex_home, None, false, TemplateDiscoveryConfig::default());
        assert_eq!(res.templates.len(), 1);
        assert_eq!(res.templates[0].name, "Security Reviewer");
    }
}
