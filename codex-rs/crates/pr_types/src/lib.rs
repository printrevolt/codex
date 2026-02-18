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
                        .map(std::string::String::as_str)
                        .ok_or_else(|| TemplateError::MissingVar(rest.to_string()));
                }
                if let Some(rest) = key.strip_prefix("fact.") {
                    return ctx
                        .facts
                        .get(rest)
                        .map(std::string::String::as_str)
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
#[derive(Default)]
pub enum TemplateSelectionMode {
    #[default]
    Off,
    Once,
    EveryTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct TemplatesUiConfig {
    pub selection_mode: TemplateSelectionMode,
    /// Template id highlighted in the picker when the picker is shown.
    /// Empty string means "none".
    pub default_for_picker_template_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TemplatesUiOverrides {
    #[serde(default)]
    pub selection_mode: Option<TemplateSelectionMode>,
    #[serde(default)]
    pub default_for_picker_template_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct UiConfig {
    /// Slash command groups disabled by a supervisor UI (e.g., Command Center).
    /// Example values: "templates", "pipelines", "policy".
    pub disabled_slash_commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AgentConfig {
    #[serde(default)]
    pub templates: TemplatesUiOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProfileRefs {
    pub policy_profiles: Vec<String>,
    pub guideline_profiles: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PolicyProfilePatchV1 {
    pub enabled: Option<bool>,
    pub deny_dangerous_always: Option<bool>,
    pub verify_required: Option<bool>,
    pub verify_max_age_ms: Option<u64>,
    pub verify_command_prefixes_add: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PolicyProfileV1 {
    pub description: Option<String>,
    pub includes: Vec<String>,
    pub patch: PolicyProfilePatchV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GuidelineProfileV1 {
    pub description: Option<String>,
    pub includes: Vec<String>,
    pub instructions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyProfilesFileV1 {
    pub schema_version: String,
    pub profiles: BTreeMap<String, PolicyProfileV1>,
}

impl Default for PolicyProfilesFileV1 {
    fn default() -> Self {
        Self {
            schema_version: "1".to_string(),
            profiles: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuidelineProfilesFileV1 {
    pub schema_version: String,
    pub profiles: BTreeMap<String, GuidelineProfileV1>,
}

impl Default for GuidelineProfilesFileV1 {
    fn default() -> Self {
        Self {
            schema_version: "1".to_string(),
            profiles: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub enabled: bool,
    pub deny_dangerous_always: bool,
    pub verify: VerifyPolicy,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
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
#[derive(Default)]
pub enum ChildProcessPolicy {
    #[default]
    Inherit,
    AuditAllowlist,
    EnforceAllowlist,
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

fn default_enabled_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowGraphV1 {
    pub entry: String,
    #[serde(default)]
    pub steps: BTreeMap<String, WorkflowStepV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowParamSpecV1 {
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowComponentV1 {
    #[serde(default)]
    pub params: BTreeMap<String, WorkflowParamSpecV1>,
    #[serde(default)]
    pub profile_refs: ProfileRefs,
    pub step: WorkflowStepV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowEntryV1 {
    pub id: String,
    pub name: String,
    #[serde(default = "default_enabled_true")]
    pub enabled: bool,
    #[serde(default)]
    pub profile_refs: ProfileRefs,
    pub workflow: WorkflowGraphV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step_kind", rename_all = "snake_case")]
pub enum WorkflowStepV1 {
    GenerateArtifact {
        template_id: String,
        artifact_kind: String,
        #[serde(default)]
        inputs: BTreeMap<String, String>,
        #[serde(default)]
        next_step: Option<String>,
        #[serde(default)]
        profile_refs: ProfileRefs,
    },
    ReviewArtifact {
        artifact_ref: String,
        #[serde(default)]
        prompt: String,
        on_approved: String,
        on_feedback: String,
        #[serde(default = "default_max_revisions")]
        max_revisions: u32,
        #[serde(default)]
        revision_counter_key: String,
        #[serde(default)]
        profile_refs: ProfileRefs,
    },
    ReviseArtifact {
        template_id: String,
        artifact_ref: String,
        feedback_key: String,
        #[serde(default)]
        next_step: Option<String>,
        #[serde(default)]
        profile_refs: ProfileRefs,
    },
    RunPipeline {
        pipeline_id: String,
        #[serde(default)]
        pipeline_scope: Option<String>,
        #[serde(default)]
        next_step: Option<String>,
        #[serde(default)]
        profile_refs: ProfileRefs,
    },
    UseComponent {
        component_id: String,
        #[serde(default)]
        args: BTreeMap<String, String>,
        #[serde(default)]
        next_step: Option<String>,
        #[serde(default)]
        profile_refs: ProfileRefs,
    },
    Complete,
}

fn default_max_revisions() -> u32 {
    3
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkflowProfileAttachmentsV1 {
    /// workflow_id -> profile refs
    pub workflows: BTreeMap<String, ProfileRefs>,
    /// workflow_id.step_id -> profile refs
    pub steps: BTreeMap<String, ProfileRefs>,
    /// component_id -> profile refs
    pub components: BTreeMap<String, ProfileRefs>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowsFileV1 {
    pub schema_version: String,
    #[serde(default)]
    pub components: BTreeMap<String, WorkflowComponentV1>,
    #[serde(default)]
    pub workflows: BTreeMap<String, WorkflowEntryV1>,
    #[serde(default)]
    pub profile_attachments: WorkflowProfileAttachmentsV1,
}

impl Default for WorkflowsFileV1 {
    fn default() -> Self {
        Self {
            schema_version: "1".to_string(),
            components: BTreeMap::new(),
            workflows: BTreeMap::new(),
            profile_attachments: WorkflowProfileAttachmentsV1::default(),
        }
    }
}

impl WorkflowsFileV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != "1" {
            return Err(format!(
                "unknown workflows.json schema_version: {} (expected 1)",
                self.schema_version
            ));
        }
        for (component_id, component) in &self.components {
            if component_id.trim().is_empty() {
                return Err("component id cannot be empty".to_string());
            }
            match &component.step {
                WorkflowStepV1::GenerateArtifact { .. }
                | WorkflowStepV1::ReviseArtifact { .. }
                | WorkflowStepV1::RunPipeline { .. } => {}
                WorkflowStepV1::UseComponent { .. } => {
                    return Err(format!(
                        "component {component_id} cannot contain use_component (nesting is not supported)"
                    ));
                }
                WorkflowStepV1::ReviewArtifact { .. } | WorkflowStepV1::Complete => {
                    return Err(format!(
                        "component {component_id} step kind is not reusable (expected generate_artifact, revise_artifact, or run_pipeline)"
                    ));
                }
            }
        }
        for workflow_id in self.profile_attachments.workflows.keys() {
            if !self.workflows.contains_key(workflow_id) {
                return Err(format!(
                    "workflow profile attachment references missing workflow {}",
                    workflow_id
                ));
            }
        }
        for component_id in self.profile_attachments.components.keys() {
            if !self.components.contains_key(component_id) {
                return Err(format!(
                    "workflow profile attachment references missing component {}",
                    component_id
                ));
            }
        }
        for step_ref in self.profile_attachments.steps.keys() {
            let Some((workflow_id, step_id)) = step_ref.split_once('.') else {
                return Err(format!(
                    "workflow step profile attachment key must be workflow_id.step_id: {}",
                    step_ref
                ));
            };
            let Some(workflow) = self.workflows.get(workflow_id) else {
                return Err(format!(
                    "workflow step profile attachment references missing workflow {}",
                    workflow_id
                ));
            };
            if !workflow.workflow.steps.contains_key(step_id) {
                return Err(format!(
                    "workflow step profile attachment references missing step {} in workflow {}",
                    step_id, workflow_id
                ));
            }
        }
        for (workflow_key, workflow) in &self.workflows {
            if workflow_key.trim().is_empty() {
                return Err("workflow key cannot be empty".to_string());
            }
            if workflow.id.trim().is_empty() {
                return Err("workflow id cannot be empty".to_string());
            }
            if workflow.id != *workflow_key {
                return Err(format!(
                    "workflow key/id mismatch: key={} id={}",
                    workflow_key, workflow.id
                ));
            }
            if workflow.name.trim().is_empty() {
                return Err(format!("workflow {} name cannot be empty", workflow.id));
            }
            if workflow.workflow.entry.trim().is_empty() {
                return Err(format!("workflow {} entry cannot be empty", workflow.id));
            }
            if workflow.workflow.steps.is_empty() {
                return Err(format!("workflow {} steps cannot be empty", workflow.id));
            }
            if !workflow
                .workflow
                .steps
                .contains_key(&workflow.workflow.entry)
            {
                return Err(format!(
                    "workflow {} entry step not found: {}",
                    workflow.id, workflow.workflow.entry
                ));
            }

            let mut has_complete = false;
            for (step_key, step) in &workflow.workflow.steps {
                if step_key.trim().is_empty() {
                    return Err(format!("workflow {} has empty step id", workflow.id));
                }
                if matches!(step, WorkflowStepV1::Complete) {
                    has_complete = true;
                }
                if let WorkflowStepV1::ReviewArtifact { max_revisions, .. } = step
                    && *max_revisions == 0
                {
                    return Err(format!(
                        "workflow {} step {} max_revisions must be >= 1",
                        workflow.id, step_key
                    ));
                }
                if let WorkflowStepV1::RunPipeline {
                    pipeline_scope: Some(scope),
                    ..
                } = step
                    && scope.as_str() != "global"
                    && scope.as_str() != "project"
                    && scope.as_str() != "effective"
                {
                    return Err(format!(
                        "workflow {} step {} has invalid pipeline_scope={} (expected global|project|effective)",
                        workflow.id, step_key, scope
                    ));
                }
                if let WorkflowStepV1::UseComponent {
                    component_id, args, ..
                } = step
                {
                    let Some(component) = self.components.get(component_id) else {
                        return Err(format!(
                            "workflow {} step {} references missing component {}",
                            workflow.id, step_key, component_id
                        ));
                    };
                    for arg_key in args.keys() {
                        if !component.params.contains_key(arg_key) {
                            return Err(format!(
                                "workflow {} step {} arg {} is not defined in component {}",
                                workflow.id, step_key, arg_key, component_id
                            ));
                        }
                    }
                }

                let mut next_refs = Vec::<&str>::new();
                match step {
                    WorkflowStepV1::GenerateArtifact { next_step, .. }
                    | WorkflowStepV1::ReviseArtifact { next_step, .. }
                    | WorkflowStepV1::RunPipeline { next_step, .. }
                    | WorkflowStepV1::UseComponent { next_step, .. } => {
                        if let Some(next_step) = next_step.as_deref() {
                            next_refs.push(next_step);
                        }
                    }
                    WorkflowStepV1::ReviewArtifact {
                        on_approved,
                        on_feedback,
                        ..
                    } => {
                        next_refs.push(on_approved.as_str());
                        next_refs.push(on_feedback.as_str());
                    }
                    WorkflowStepV1::Complete => {}
                }
                for next_step in next_refs {
                    if !workflow.workflow.steps.contains_key(next_step) {
                        return Err(format!(
                            "workflow {} step {} references missing step {}",
                            workflow.id, step_key, next_step
                        ));
                    }
                }
            }
            if !has_complete {
                return Err(format!(
                    "workflow {} must contain at least one complete step",
                    workflow.id
                ));
            }
        }
        Ok(())
    }

    pub fn merge_effective(global: Option<Self>, repo: Option<Self>) -> Result<Self, String> {
        let mut components = BTreeMap::<String, WorkflowComponentV1>::new();
        let mut workflows = BTreeMap::<String, WorkflowEntryV1>::new();
        let mut attachments = WorkflowProfileAttachmentsV1::default();
        if let Some(global) = global {
            global.validate()?;
            for (id, component) in global.components {
                components.insert(id, component);
            }
            for (id, workflow) in global.workflows {
                workflows.insert(id, workflow);
            }
            attachments
                .workflows
                .extend(global.profile_attachments.workflows);
            attachments.steps.extend(global.profile_attachments.steps);
            attachments
                .components
                .extend(global.profile_attachments.components);
        }
        if let Some(repo) = repo {
            repo.validate()?;
            for (id, component) in repo.components {
                components.insert(id, component);
            }
            for (id, workflow) in repo.workflows {
                workflows.insert(id, workflow);
            }
            attachments
                .workflows
                .extend(repo.profile_attachments.workflows);
            attachments.steps.extend(repo.profile_attachments.steps);
            attachments
                .components
                .extend(repo.profile_attachments.components);
        }
        Ok(Self {
            schema_version: "1".to_string(),
            components,
            workflows,
            profile_attachments: attachments,
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
pub struct WorkflowsConfig {
    pub enabled: bool,
    pub max_revisions: u32,
    pub max_artifact_bytes: u64,
    pub max_feedback_bytes: u64,
}

impl Default for WorkflowsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_revisions: 3,
            max_artifact_bytes: 262_144,
            max_feedback_bytes: 8_192,
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
    pub workflows: WorkflowsConfig,
    #[serde(default)]
    pub templates: TemplatesUiConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub policy_profiles: Vec<String>,
    #[serde(default)]
    pub guideline_profiles: Vec<String>,
}

impl Default for PrintRevoltConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: PolicyConfig::default(),
            hooks: HookConfig::default(),
            audit: AuditConfig::default(),
            pipelines: PipelinesConfig::default(),
            workflows: WorkflowsConfig::default(),
            templates: TemplatesUiConfig::default(),
            ui: UiConfig::default(),
            agents: BTreeMap::new(),
            vars: BTreeMap::new(),
            policy_profiles: Vec::new(),
            guideline_profiles: Vec::new(),
        }
    }
}

impl PrintRevoltConfig {
    pub fn effective_templates_ui(&self, agent_id: Option<&str>) -> TemplatesUiConfig {
        let mut out = self.templates.clone();
        if let Some(agent_id) = agent_id
            && let Some(agent_cfg) = self.agents.get(agent_id)
        {
            if let Some(mode) = agent_cfg.templates.selection_mode {
                out.selection_mode = mode;
            }
            if let Some(id) = agent_cfg.templates.default_for_picker_template_id.as_ref() {
                out.default_for_picker_template_id = id.clone();
            }
        }
        out
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
