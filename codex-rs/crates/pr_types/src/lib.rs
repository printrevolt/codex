use std::collections::BTreeMap;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

pub mod templating {
    use std::collections::BTreeMap;

    use thiserror::Error;

    const MAX_TEMPLATE_BYTES: usize = 32 * 1024;
    const MAX_RENDERED_BYTES: usize = 64 * 1024;

    #[derive(Debug, Error)]
    pub enum TemplateError {
        #[error("template too large")]
        TemplateTooLarge,
        #[error("rendered output too large")]
        RenderedTooLarge,
        #[error("unclosed placeholder")]
        UnclosedPlaceholder,
        #[error("unknown placeholder: {0}")]
        UnknownPlaceholder(String),
        #[error("missing var: {0}")]
        MissingVar(String),
        #[error("missing fact: {0}")]
        MissingFact(String),
    }

    #[derive(Debug, Clone, Copy)]
    pub struct TemplateContext<'a> {
        pub session_id: &'a str,
        pub task_id: Option<&'a str>,
        pub turn_id: Option<&'a str>,
        pub vars: &'a BTreeMap<String, String>,
        pub facts: &'a BTreeMap<String, String>,
    }

    pub fn render_template(
        input: &str,
        ctx: &TemplateContext<'_>,
    ) -> Result<String, TemplateError> {
        if input.len() > MAX_TEMPLATE_BYTES {
            return Err(TemplateError::TemplateTooLarge);
        }

        let mut out = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                let start = i + 2;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'}' {
                    end += 1;
                }
                if end >= bytes.len() {
                    return Err(TemplateError::UnclosedPlaceholder);
                }
                let key = &input[start..end];
                let value = resolve_placeholder(key, ctx)?;
                out.push_str(value);
                i = end + 1;
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }

            if out.len() > MAX_RENDERED_BYTES {
                return Err(TemplateError::RenderedTooLarge);
            }
        }

        Ok(out)
    }

    fn resolve_placeholder<'a>(
        key: &str,
        ctx: &TemplateContext<'a>,
    ) -> Result<&'a str, TemplateError> {
        match key {
            "session_id" => Ok(ctx.session_id),
            "task_id" => ctx
                .task_id
                .ok_or_else(|| TemplateError::UnknownPlaceholder(key.to_string())),
            "turn_id" => ctx
                .turn_id
                .ok_or_else(|| TemplateError::UnknownPlaceholder(key.to_string())),
            _ => {
                if let Some(rest) = key.strip_prefix("var.") {
                    return ctx
                        .vars
                        .get(rest)
                        .map(|s| s.as_str())
                        .ok_or_else(|| TemplateError::MissingVar(rest.to_string()));
                }
                if let Some(rest) = key.strip_prefix("fact.") {
                    return ctx
                        .facts
                        .get(rest)
                        .map(|s| s.as_str())
                        .ok_or_else(|| TemplateError::MissingFact(rest.to_string()));
                }
                Err(TemplateError::UnknownPlaceholder(key.to_string()))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn substitutes_placeholders() {
            let vars = BTreeMap::from([("BASE_BRANCH".to_string(), "main".to_string())]);
            let facts = BTreeMap::from([("status".to_string(), "down".to_string())]);
            let ctx = TemplateContext {
                session_id: "s1",
                task_id: Some("t1"),
                turn_id: Some("turn1"),
                vars: &vars,
                facts: &facts,
            };
            let rendered = render_template(
                "sid=${session_id} base=${var.BASE_BRANCH} status=${fact.status}",
                &ctx,
            )
            .unwrap();
            assert_eq!(rendered, "sid=s1 base=main status=down");
        }

        #[test]
        fn missing_var_is_error() {
            let vars = BTreeMap::new();
            let facts = BTreeMap::new();
            let ctx = TemplateContext {
                session_id: "s1",
                task_id: None,
                turn_id: None,
                vars: &vars,
                facts: &facts,
            };
            let err = render_template("x=${var.MISSING}", &ctx).unwrap_err();
            assert!(matches!(err, TemplateError::MissingVar(_)));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEventKind {
    SessionStart,
    BeforeTask,
    BeforeTool,
    AfterTool,
    BeforeFinalize,
    SessionEnd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Function,
    Custom,
    LocalShell,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input_type", rename_all = "snake_case")]
pub enum ToolInput {
    Function {
        arguments: String,
    },
    Custom {
        input: String,
    },
    LocalShell {
        command: Vec<String>,
        workdir: Option<String>,
    },
    Mcp {
        server: String,
        tool: String,
        arguments: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub tool_kind: ToolKind,
    pub input: ToolInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Allow,
    Block,
    Modify,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDecision {
    pub kind: DecisionKind,
    pub reason_code: Option<String>,
    pub message: Option<String>,
    pub modified_call: Option<ToolCall>,
}

impl ToolCallDecision {
    pub fn allow() -> Self {
        Self {
            kind: DecisionKind::Allow,
            reason_code: None,
            message: None,
            modified_call: None,
        }
    }

    pub fn block(reason_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: DecisionKind::Block,
            reason_code: Some(reason_code.into()),
            message: Some(message.into()),
            modified_call: None,
        }
    }

    pub fn modify(modified_call: ToolCall) -> Self {
        Self {
            kind: DecisionKind::Modify,
            reason_code: None,
            message: None,
            modified_call: Some(modified_call),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionContext {
    pub session_id: ThreadId,
    pub turn_id: Option<String>,
    pub triggered_at: DateTime<Utc>,
    pub cwd: String,
    pub repo_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyEvidence {
    pub recorded_at: DateTime<Utc>,
    pub command: Vec<String>,
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyPolicy {
    pub required: bool,
    pub max_age_ms: u64,
    pub command_prefixes: Vec<Vec<String>>,
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        Self {
            required: false,
            max_age_ms: 30 * 60 * 1000,
            command_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub deny_dangerous_always: bool,
    pub verify: VerifyPolicy,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            deny_dangerous_always: true,
            verify: VerifyPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HookConfig {
    pub enabled: bool,
    pub trusted_repo_roots: Vec<String>,
    #[serde(default)]
    pub before_tool: Option<HookDefinition>,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trusted_repo_roots: Vec::new(),
            before_tool: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDefinition {
    pub argv: Vec<String>,
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub headless_only: bool,
    #[serde(default)]
    pub is_repo_provided: bool,
}

fn default_hook_timeout_ms() -> u64 {
    2_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditConfig {
    pub enabled: bool,
    pub max_file_bytes: u64,
    pub max_files: usize,
    pub redact_patterns: Vec<String>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_bytes: 5 * 1024 * 1024,
            max_files: 5,
            redact_patterns: vec![
                r"(?i)(api[_-]?key|token|secret|password|pwd)\s*=\s*\S+".to_string(),
                r"(?i)bearer\s+[a-z0-9._\\-]+".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildProcessPolicy {
    Inherit,
    AuditAllowlist,
    EnforceAllowlist,
}

impl Default for ChildProcessPolicy {
    fn default() -> Self {
        Self::Inherit
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCategory {
    Verify,
    Lint,
    Build,
    Setup,
    Git,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandSpecV1 {
    pub id: String,
    pub name: String,
    #[serde(default = "default_command_cwd")]
    pub cwd: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub category: Option<CommandCategory>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub child_process_policy: ChildProcessPolicy,
    #[serde(default)]
    pub script_runner: bool,
    #[serde(default)]
    pub requires_sandbox: bool,
}

fn default_command_cwd() -> String {
    ".".to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandsFileV1 {
    pub schema_version: String,
    pub commands: Vec<CommandSpecV1>,
}

impl CommandsFileV1 {
    pub fn merge_effective(global: Option<Self>, repo: Option<Self>) -> Result<Self, String> {
        let mut commands = BTreeMap::<String, CommandSpecV1>::new();
        if let Some(g) = global {
            for cmd in g.commands {
                if cmd.id.trim().is_empty() {
                    return Err("command id cannot be empty".to_string());
                }
                if cmd.argv.is_empty() {
                    return Err(format!("command {} argv cannot be empty", cmd.id));
                }
                commands.insert(cmd.id.clone(), cmd);
            }
        }
        if let Some(r) = repo {
            for cmd in r.commands {
                if cmd.id.trim().is_empty() {
                    return Err("command id cannot be empty".to_string());
                }
                if cmd.argv.is_empty() {
                    return Err(format!("command {} argv cannot be empty", cmd.id));
                }
                commands.insert(cmd.id.clone(), cmd);
            }
        }
        Ok(Self {
            schema_version: "1".to_string(),
            commands: commands.into_values().collect(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelinesConfig {
    pub enabled: bool,
    pub max_cycles: u32,
}

impl Default for PipelinesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_cycles: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrintRevoltConfig {
    pub enabled: bool,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub hooks: HookConfig,
    #[serde(default)]
    pub audit: AuditConfig,
    #[serde(default)]
    pub pipelines: PipelinesConfig,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

impl Default for PrintRevoltConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: PolicyConfig::default(),
            hooks: HookConfig::default(),
            audit: AuditConfig::default(),
            pipelines: PipelinesConfig::default(),
            vars: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookPayloadV2 {
    pub ctx: SessionContext,
    pub event_kind: LifecycleEventKind,
    pub tool_call: Option<ToolCall>,
    pub tool_outcome: Option<ToolOutcome>,
    pub vars: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookResponseV2 {
    pub decision: ToolCallDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub executed: bool,
    pub success: bool,
    pub duration_ms: u64,
    pub output_preview: String,
}
