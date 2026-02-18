use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_pr_types::PolicyConfig;
use codex_pr_types::ToolCall;
use codex_pr_types::ToolCallDecision;
use codex_pr_types::ToolInput;
use codex_pr_types::VerifyEvidence;
use regex_lite::Regex;
use serde_json::Value as JsonValue;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("invalid regex: {0}")]
    InvalidRegex(String),
}

#[derive(Clone)]
pub struct PolicyEngine {
    cfg: PolicyConfig,
    dangerous_res: Vec<Regex>,
}

impl PolicyEngine {
    pub fn new(cfg: PolicyConfig) -> Result<Self, PolicyError> {
        let patterns = vec![
            r"(?i)\brm\b.*\s-(?:[^\n]*r[^\n]*f|[^\n]*f[^\n]*r)\b".to_string(),
            r"(?i)\bdel\b\s+/s\s+/q\b".to_string(),
            r"(?i)\bformat\b\s+[a-z]:".to_string(),
        ];
        let mut dangerous_res = Vec::new();
        for p in patterns {
            dangerous_res.push(Regex::new(&p).map_err(|_| PolicyError::InvalidRegex(p.clone()))?);
        }
        Ok(Self { cfg, dangerous_res })
    }

    pub fn evaluate_tool_call(&self, call: &ToolCall) -> ToolCallDecision {
        if !self.cfg.enabled {
            return ToolCallDecision::allow();
        }
        if self.cfg.deny_dangerous_always && self.is_dangerous(call) {
            return ToolCallDecision::block(
                "PrDangerousCommandDenied",
                "Blocked dangerous command by policy floor (deny_dangerous_always=true).",
            );
        }
        ToolCallDecision::allow()
    }

    pub fn should_record_verify(&self, call: &ToolCall) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        let Some(argv) = extract_argv_like(call) else {
            return false;
        };
        self.cfg
            .verify
            .command_prefixes
            .iter()
            .any(|prefix| argv.starts_with(prefix))
    }

    pub fn evaluate_finalize(
        &self,
        now: DateTime<Utc>,
        last_verify: Option<&VerifyEvidence>,
    ) -> Option<ToolCallDecision> {
        if !self.cfg.enabled {
            return None;
        }
        if !self.cfg.verify.required {
            return None;
        }
        let Some(evidence) = last_verify else {
            return Some(ToolCallDecision::block(
                "PrVerifyExecutionDenied",
                "Verification is required, but no verify evidence was recorded for this session. Run the configured verify command(s) and retry.",
            ));
        };
        if !evidence.success {
            return Some(ToolCallDecision::block(
                "PrVerifyExecutionDenied",
                "Verification is required, but the most recent verify attempt failed. Fix the issue, re-run verify, then retry.",
            ));
        }
        let age = now - evidence.recorded_at;
        let max_age =
            Duration::milliseconds(i64::try_from(self.cfg.verify.max_age_ms).unwrap_or(0));
        if age > max_age {
            return Some(ToolCallDecision::block(
                "PrVerifyExecutionDenied",
                "Verification is required, but existing verify evidence is stale. Re-run verify and retry.",
            ));
        }
        None
    }

    fn is_dangerous(&self, call: &ToolCall) -> bool {
        let Some(text) = extract_command_text(call) else {
            return false;
        };
        self.dangerous_res.iter().any(|re| re.is_match(&text))
    }
}

fn extract_argv_like(call: &ToolCall) -> Option<Vec<String>> {
    match &call.input {
        ToolInput::LocalShell { command, .. } => Some(command.clone()),
        ToolInput::Function { arguments } if call.tool_name == "shell_command" => {
            let Ok(json) = serde_json::from_str::<JsonValue>(arguments) else {
                return None;
            };
            json.get("command")
                .and_then(|v| v.as_str())
                .map(|s| vec![s.to_string()])
        }
        _ => None,
    }
}

fn extract_command_text(call: &ToolCall) -> Option<String> {
    match &call.input {
        ToolInput::LocalShell { command, .. } => Some(command.join(" ")),
        ToolInput::Function { arguments } if call.tool_name == "shell_command" => {
            let Ok(json) = serde_json::from_str::<JsonValue>(arguments) else {
                return None;
            };
            json.get("command")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use codex_pr_types::DecisionKind;
    use codex_pr_types::ToolKind;

    #[test]
    fn blocks_rm_rf() {
        let engine = PolicyEngine::new(PolicyConfig::default()).unwrap();
        let call = ToolCall {
            call_id: "c1".to_string(),
            tool_name: "local_shell".to_string(),
            tool_kind: ToolKind::LocalShell,
            input: ToolInput::LocalShell {
                command: vec!["rm".to_string(), "-rf".to_string(), "/tmp/x".to_string()],
                workdir: None,
            },
        };
        let decision = engine.evaluate_tool_call(&call);
        assert_eq!(decision.kind, DecisionKind::Block);
        assert_eq!(
            decision.reason_code.as_deref(),
            Some("PrDangerousCommandDenied")
        );
    }
}
