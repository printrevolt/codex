use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationKind {
    AddCommandsCatalog,
    AddVerifyPipeline,
    TightenPolicy,
    CoverageGap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Recommendation {
    pub id: String,
    pub kind: RecommendationKind,
    pub severity: RecommendationSeverity,
    pub title: String,
    pub details: String,
    pub suggested_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecommendationBundle {
    pub repo_root: Option<String>,
    pub detected: DetectedProjectSignals,
    pub recommendations: Vec<Recommendation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DetectedProjectSignals {
    pub has_package_json: bool,
    pub package_manager: Option<String>,
    pub has_frontend_dir: bool,
    pub has_backend_dir: bool,
}

#[derive(Debug, Error)]
pub enum AdvisorError {
    #[error("io error: {0}")]
    Io(String),
    #[error("invalid json: {0}")]
    InvalidJson(String),
}

pub struct AdvisorInput<'a> {
    pub repo_root: Option<&'a std::path::Path>,
    pub package_json: Option<&'a str>,
    pub lockfiles: &'a [&'a str],
    pub existing_commands_json: Option<&'a str>,
    pub existing_pipelines: Option<&'a str>,
}

pub fn recommend(input: AdvisorInput<'_>) -> Result<RecommendationBundle, AdvisorError> {
    let mut detected = DetectedProjectSignals::default();
    let mut recs = Vec::new();

    if let Some(pj) = input.package_json {
        detected.has_package_json = true;
        let scripts = parse_package_json_scripts(pj)?;
        if !scripts.is_empty() && input.existing_commands_json.is_none() {
            recs.push(Recommendation {
                id: "commands-json".to_string(),
                kind: RecommendationKind::AddCommandsCatalog,
                severity: RecommendationSeverity::Warning,
                title: "Add commands catalog (commands.json)".to_string(),
                details: format!(
                    "Detected package.json scripts: {}. Consider creating CODEX_HOME/printrevolt/commands.json (or trusted repo catalog) so pipelines use canonical command_ids.",
                    scripts.join(", ")
                ),
                suggested_files: vec![
                    "<CODEX_HOME>/printrevolt/commands.json".to_string(),
                    "<repo_root>/.codex/printrevolt/commands.json".to_string(),
                ],
            });
        }
    }

    detected.package_manager = detect_package_manager(input.lockfiles);
    if detected.package_manager.is_some() && input.existing_pipelines.is_none() {
        recs.push(Recommendation {
            id: "verify-pipeline".to_string(),
            kind: RecommendationKind::AddVerifyPipeline,
            severity: RecommendationSeverity::Info,
            title: "Add a verify pipeline".to_string(),
            details: "Add a pipeline that runs verification (tests/lint/build) after agent work, so verify evidence can be recorded deterministically.".to_string(),
            suggested_files: vec!["printrevolt.toml".to_string()],
        });
    }

    Ok(RecommendationBundle {
        repo_root: input.repo_root.map(|p| p.display().to_string()),
        detected,
        recommendations: recs,
    })
}

fn detect_package_manager(lockfiles: &[&str]) -> Option<String> {
    for f in lockfiles {
        match *f {
            "pnpm-lock.yaml" => return Some("pnpm".to_string()),
            "yarn.lock" => return Some("yarn".to_string()),
            "package-lock.json" => return Some("npm".to_string()),
            _ => {}
        }
    }
    None
}

fn parse_package_json_scripts(raw: &str) -> Result<Vec<String>, AdvisorError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| AdvisorError::InvalidJson(e.to_string()))?;
    let Some(obj) = v.get("scripts").and_then(|s| s.as_object()) else {
        return Ok(Vec::new());
    };
    let mut keys = obj.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn recommends_commands_json_when_scripts_present() {
        let pj = r#"{"name":"x","scripts":{"test":"vitest","lint":"eslint ."}} "#;
        let bundle = recommend(AdvisorInput {
            repo_root: None,
            package_json: Some(pj),
            lockfiles: &["pnpm-lock.yaml"],
            existing_commands_json: None,
            existing_pipelines: None,
        })
        .unwrap();
        assert_eq!(bundle.detected.package_manager, Some("pnpm".to_string()));
        assert!(
            bundle
                .recommendations
                .iter()
                .any(|r| r.kind == RecommendationKind::AddCommandsCatalog)
        );
    }
}
