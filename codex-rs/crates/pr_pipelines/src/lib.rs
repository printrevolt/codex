use std::collections::BTreeMap;

use codex_pr_types::ChildProcessPolicy;
use codex_pr_types::CommandSpecV1;
use codex_pr_types::ToolCall;
use codex_pr_types::ToolInput;
use codex_pr_types::ToolKind;
use codex_pr_types::ToolOutcome;
use codex_pr_types::templating::TemplateContext;
use codex_pr_types::templating::render_template;
use regex_lite::Regex;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

pub type WorkflowId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelinePhase {
    Main,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    OneOff,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Gate,
    Confirm,
}

impl Default for ApprovalMode {
    fn default() -> Self {
        Self::Gate
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractSource {
    Stdout,
    Stderr,
    Combined,
}

impl Default for ExtractSource {
    fn default() -> Self {
        Self::Combined
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "parser_kind", rename_all = "snake_case")]
pub enum ExtractParser {
    JsonPath {
        json_path: String,
    },
    Regex {
        pattern: String,
        group: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizeOp {
    Trim,
    Lowercase,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "part_kind", rename_all = "snake_case")]
pub enum Part {
    RunTool {
        tool_call: ToolCall,
    },
    RunCommand {
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        argv: Option<Vec<String>>,
        #[serde(default)]
        command_id: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        child_process_policy: ChildProcessPolicy,
    },
    RequireApproval {
        scope: ApprovalScope,
        #[serde(default)]
        mode: ApprovalMode,
        prompt: String,
        #[serde(default)]
        remediation: Vec<String>,
        action_hash: String,
        #[serde(default)]
        output_key: Option<String>,
    },
    ExtractValue {
        #[serde(default)]
        source: ExtractSource,
        parser: ExtractParser,
        output_key: String,
        #[serde(default)]
        normalize: Vec<NormalizeOp>,
    },
    AssertFact {
        key: String,
        #[serde(default)]
        equals: Option<String>,
        #[serde(default)]
        matches_regex: Option<String>,
    },
    EnsureWorktree {
        worktree_root: String,
        naming: String,
        #[serde(default)]
        base_branch: Option<String>,
        branch_name: String,
        #[serde(default = "default_require_approval_true")]
        require_approval: bool,
    },
    EnsureBranch {
        #[serde(default)]
        base_branch: Option<String>,
        branch_name: String,
        #[serde(default)]
        protected_branches: Vec<String>,
        #[serde(default = "default_require_approval_true")]
        require_approval: bool,
    },
    SetVar {
        key: String,
        value: String,
    },
    Fail {
        message: String,
    },
    Defer {
        part: Box<Part>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub parts: Vec<Part>,
    #[serde(default)]
    pub finally_workflow: Option<WorkflowId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub workflows: BTreeMap<WorkflowId, Workflow>,
    pub entry: WorkflowId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineLimits {
    pub max_cycles: u32,
}

impl Default for PipelineLimits {
    fn default() -> Self {
        Self { max_cycles: 3 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineFacts {
    pub derived: BTreeMap<String, String>,
    pub last_tool_outcome: Option<ToolOutcome>,
    pub last_tool_output_stdout: String,
    pub last_tool_output_stderr: String,
}

impl Default for PipelineFacts {
    fn default() -> Self {
        Self {
            derived: BTreeMap::new(),
            last_tool_outcome: None,
            last_tool_output_stdout: String::new(),
            last_tool_output_stderr: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineRunContext {
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub vars: BTreeMap<String, String>,
    pub repo_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineRunState {
    pub phase: PipelinePhase,
    pub vars: BTreeMap<String, String>,
    pub facts: PipelineFacts,
    pub deferred: Vec<Part>,
    pub cycles: u32,
    pub failed: bool,
    pub failure_message: Option<String>,

    cursor: Cursor,
    cleanup_cursor: Option<Cursor>,
    waiting: Option<WaitingFor>,
    pending_tool: Option<ToolCall>,
    ctx: PipelineRunContext,
}

#[derive(Debug, Clone, PartialEq)]
struct Cursor {
    workflow_id: WorkflowId,
    part_index: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum WaitingFor {
    Tool,
    Approval {
        mode: ApprovalMode,
        output_key: Option<String>,
    },
}

impl PipelineRunState {
    pub fn new(pipeline: &Pipeline, limits: &PipelineLimits, ctx: PipelineRunContext) -> Self {
        let _ = limits;
        Self {
            phase: PipelinePhase::Main,
            vars: ctx.vars.clone(),
            facts: PipelineFacts::default(),
            deferred: Vec::new(),
            cycles: 0,
            failed: false,
            failure_message: None,
            cursor: Cursor {
                workflow_id: pipeline.entry.clone(),
                part_index: 0,
            },
            cleanup_cursor: None,
            waiting: None,
            pending_tool: None,
            ctx,
        }
    }

    pub fn template_ctx(&self) -> TemplateContext<'_> {
        TemplateContext {
            session_id: self.ctx.session_id.as_str(),
            task_id: self.ctx.task_id.as_deref(),
            turn_id: self.ctx.turn_id.as_deref(),
            vars: &self.vars,
            facts: &self.facts.derived,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PipelineAction {
    InvokeTool {
        tool_call: ToolCall,
        phase: PipelinePhase,
    },
    EmitNote {
        message: String,
    },
    RequestApproval {
        scope: ApprovalScope,
        mode: ApprovalMode,
        prompt: String,
        remediation: Vec<String>,
        action_hash: String,
        output_key: Option<String>,
        phase: PipelinePhase,
    },
    Done {
        failed: bool,
        message: Option<String>,
    },
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("unknown workflow: {0}")]
    UnknownWorkflow(String),
    #[error("pipeline exceeded max_cycles={0}")]
    MaxCyclesExceeded(u32),
    #[error("engine is waiting for external input")]
    WaitingForExternalInput,
    #[error("missing tool outcome for extract/assert")]
    MissingToolOutcome,
    #[error("extract failed: {0}")]
    ExtractFailed(String),
    #[error("assert failed: {0}")]
    AssertFailed(String),
    #[error("invalid part: {0}")]
    InvalidPart(String),
    #[error("template error: {0}")]
    TemplateError(String),
    #[error("unsupported child_process_policy={0:?} on this platform")]
    UnsupportedChildProcessPolicy(ChildProcessPolicy),
    #[error("unknown command_id: {0}")]
    UnknownCommandId(String),
    #[error("missing repo_root for git operation")]
    MissingRepoRoot,
}

pub trait CommandResolver {
    fn resolve(&self, command_id: &str) -> Option<CommandSpecV1>;
}

pub struct PipelineEngine {
    limits: PipelineLimits,
}

impl PipelineEngine {
    pub fn new(limits: PipelineLimits) -> Self {
        Self { limits }
    }

    pub fn step<R: CommandResolver>(
        &self,
        pipeline: &Pipeline,
        state: &mut PipelineRunState,
        resolver: &R,
    ) -> Result<PipelineAction, PipelineError> {
        if state.waiting.is_some() {
            return Err(PipelineError::WaitingForExternalInput);
        }

        if let Some(tool_call) = state.pending_tool.take() {
            state.waiting = Some(WaitingFor::Tool);
            return Ok(PipelineAction::InvokeTool {
                tool_call,
                phase: state.phase.clone(),
            });
        }

        if state.failed && state.phase == PipelinePhase::Main {
            self.begin_cleanup(pipeline, state)?;
        }

        if state.phase == PipelinePhase::Cleanup {
            if let Some(cursor) = state.cleanup_cursor.clone() {
                return self.step_cursor(pipeline, state, cursor, resolver);
            }
            if let Some(part) = state.deferred.pop() {
                return self.step_part(pipeline, state, part, resolver);
            }
            return Ok(PipelineAction::Done {
                failed: state.failed,
                message: state.failure_message.clone(),
            });
        }

        let cursor = state.cursor.clone();
        self.step_cursor(pipeline, state, cursor, resolver)
    }

    pub fn provide_tool_outcome(
        &self,
        state: &mut PipelineRunState,
        outcome: ToolOutcome,
        stdout_preview: String,
        stderr_preview: String,
    ) -> Result<(), PipelineError> {
        let Some(WaitingFor::Tool) = state.waiting.take() else {
            return Err(PipelineError::InvalidPart(
                "provide_tool_outcome called while not waiting for a tool".to_string(),
            ));
        };
        state.facts.last_tool_outcome = Some(outcome);
        state.facts.last_tool_output_stdout = stdout_preview;
        state.facts.last_tool_output_stderr = stderr_preview;
        Ok(())
    }

    pub fn provide_approval_result(
        &self,
        state: &mut PipelineRunState,
        approved: bool,
    ) -> Result<(), PipelineError> {
        let Some(WaitingFor::Approval { mode, output_key }) = state.waiting.take() else {
            return Err(PipelineError::InvalidPart(
                "provide_approval_result called while not waiting for approval".to_string(),
            ));
        };
        match mode {
            ApprovalMode::Gate => {
                if !approved {
                    state.failed = true;
                    state.failure_message = Some("approval denied".to_string());
                }
            }
            ApprovalMode::Confirm => {
                if let Some(key) = output_key {
                    state
                        .facts
                        .derived
                        .insert(key, if approved { "true" } else { "false" }.to_string());
                }
            }
        }
        Ok(())
    }

    fn begin_cleanup(
        &self,
        pipeline: &Pipeline,
        state: &mut PipelineRunState,
    ) -> Result<(), PipelineError> {
        state.phase = PipelinePhase::Cleanup;
        let main = pipeline
            .workflows
            .get(&state.cursor.workflow_id)
            .ok_or_else(|| PipelineError::UnknownWorkflow(state.cursor.workflow_id.clone()))?;
        if let Some(finally_id) = main.finally_workflow.clone() {
            state.cleanup_cursor = Some(Cursor {
                workflow_id: finally_id,
                part_index: 0,
            });
        } else {
            state.cleanup_cursor = None;
        }
        Ok(())
    }

    fn step_cursor<R: CommandResolver>(
        &self,
        pipeline: &Pipeline,
        state: &mut PipelineRunState,
        cursor: Cursor,
        resolver: &R,
    ) -> Result<PipelineAction, PipelineError> {
        state.cycles += 1;
        if state.cycles > self.limits.max_cycles {
            return Err(PipelineError::MaxCyclesExceeded(self.limits.max_cycles));
        }

        let workflow = pipeline
            .workflows
            .get(&cursor.workflow_id)
            .ok_or_else(|| PipelineError::UnknownWorkflow(cursor.workflow_id.clone()))?;

        if cursor.part_index >= workflow.parts.len() {
            if state.phase == PipelinePhase::Main {
                self.begin_cleanup(pipeline, state)?;
                return self.step(pipeline, state, resolver);
            }
            state.cleanup_cursor = None;
            return self.step(pipeline, state, resolver);
        }

        let part = workflow.parts[cursor.part_index].clone();
        if state.phase == PipelinePhase::Main {
            state.cursor.part_index += 1;
        } else if let Some(clean) = state.cleanup_cursor.as_mut() {
            clean.part_index += 1;
        }
        self.step_part(pipeline, state, part, resolver)
    }

    fn step_part<R: CommandResolver>(
        &self,
        _pipeline: &Pipeline,
        state: &mut PipelineRunState,
        part: Part,
        resolver: &R,
    ) -> Result<PipelineAction, PipelineError> {
        match part {
            Part::RunTool { tool_call } => {
                state.waiting = Some(WaitingFor::Tool);
                Ok(PipelineAction::InvokeTool {
                    tool_call,
                    phase: state.phase.clone(),
                })
            }
            Part::RunCommand {
                cwd,
                argv,
                command_id,
                timeout_ms: _,
                child_process_policy,
            } => {
                if child_process_policy != ChildProcessPolicy::Inherit
                    && !child_process_policy_supported(child_process_policy)
                {
                    return Err(PipelineError::UnsupportedChildProcessPolicy(
                        child_process_policy,
                    ));
                }

                let (resolved_cwd, resolved_argv, note) = if let Some(command_id) = command_id {
                    let spec = resolver
                        .resolve(command_id.as_str())
                        .ok_or_else(|| PipelineError::UnknownCommandId(command_id.clone()))?;
                    let note = format!(
                        "Resolved command_id {} -> cwd={} argv={:?}",
                        spec.id, spec.cwd, spec.argv
                    );
                    (spec.cwd, spec.argv, Some(note))
                } else {
                    let argv = argv.ok_or_else(|| {
                        PipelineError::InvalidPart(
                            "run_command requires either argv or command_id".to_string(),
                        )
                    })?;
                    (cwd.unwrap_or_else(|| ".".to_string()), argv, None)
                };

                let tool_call = ToolCall {
                    call_id: "pipeline-run-command".to_string(),
                    tool_name: "local_shell".to_string(),
                    tool_kind: ToolKind::LocalShell,
                    input: ToolInput::LocalShell {
                        command: resolved_argv.clone(),
                        workdir: Some(resolved_cwd),
                    },
                };
                if let Some(note) = note {
                    state.pending_tool = Some(tool_call);
                    Ok(PipelineAction::EmitNote { message: note })
                } else {
                    state.waiting = Some(WaitingFor::Tool);
                    Ok(PipelineAction::InvokeTool {
                        tool_call,
                        phase: state.phase.clone(),
                    })
                }
            }
            Part::RequireApproval {
                scope,
                mode,
                prompt,
                remediation,
                action_hash,
                output_key,
            } => {
                let prompt = render_template_or_err(&prompt, &state.template_ctx())?;
                state.waiting = Some(WaitingFor::Approval {
                    mode: mode.clone(),
                    output_key: if mode == ApprovalMode::Confirm {
                        output_key.clone()
                    } else {
                        None
                    },
                });
                Ok(PipelineAction::RequestApproval {
                    scope,
                    mode,
                    prompt,
                    remediation,
                    action_hash,
                    output_key,
                    phase: state.phase.clone(),
                })
            }
            Part::ExtractValue {
                source,
                parser,
                output_key,
                normalize,
            } => {
                let extracted = extract_value(state, source, parser)?;
                let normalized = apply_normalize(extracted, normalize);
                state.facts.derived.insert(output_key, normalized);
                Ok(PipelineAction::EmitNote {
                    message: "extract_value: ok".to_string(),
                })
            }
            Part::AssertFact {
                key,
                equals,
                matches_regex,
            } => {
                assert_fact(state, &key, equals.as_deref(), matches_regex.as_deref())?;
                Ok(PipelineAction::EmitNote {
                    message: "assert_fact: ok".to_string(),
                })
            }
            Part::EnsureWorktree {
                worktree_root,
                naming,
                base_branch,
                branch_name,
                require_approval,
            } => {
                let repo_root = state
                    .ctx
                    .repo_root
                    .as_deref()
                    .ok_or(PipelineError::MissingRepoRoot)?;
                let worktree_root = render_template_or_err(&worktree_root, &state.template_ctx())?;
                let naming = render_template_or_err(&naming, &state.template_ctx())?;
                let base_branch = base_branch.as_deref().unwrap_or("main").to_string();
                let branch_name = render_template_or_err(&branch_name, &state.template_ctx())?;

                let action_hash = sha256_json(&serde_json::json!({
                    "op": "ensure_worktree",
                    "repo_root": repo_root,
                    "worktree_root": worktree_root,
                    "naming": naming,
                    "branch_name": branch_name,
                    "base_branch": base_branch,
                }));

                if require_approval {
                    state.waiting = Some(WaitingFor::Approval {
                        mode: ApprovalMode::Gate,
                        output_key: None,
                    });
                    return Ok(PipelineAction::RequestApproval {
                        scope: ApprovalScope::OneOff,
                        mode: ApprovalMode::Gate,
                        prompt: "Create/ensure a git worktree for this task?".to_string(),
                        remediation: vec![
                            "Deny to proceed without creating a worktree.".to_string(),
                        ],
                        action_hash,
                        output_key: None,
                        phase: state.phase.clone(),
                    });
                }

                state.waiting = Some(WaitingFor::Tool);
                Ok(PipelineAction::InvokeTool {
                    tool_call: ToolCall {
                        call_id: "pipeline-ensure-worktree".to_string(),
                        tool_name: "local_shell".to_string(),
                        tool_kind: ToolKind::LocalShell,
                        input: ToolInput::LocalShell {
                            command: vec![
                                "codex-pr".to_string(),
                                "repo".to_string(),
                                "ensure-worktree".to_string(),
                                "--repo-root".to_string(),
                                repo_root.to_string(),
                                "--worktree-root".to_string(),
                                worktree_root,
                                "--naming".to_string(),
                                naming,
                                "--branch-name".to_string(),
                                branch_name,
                                "--base-branch".to_string(),
                                base_branch,
                                "--apply".to_string(),
                            ],
                            workdir: Some(repo_root.to_string()),
                        },
                    },
                    phase: state.phase.clone(),
                })
            }
            Part::EnsureBranch {
                base_branch,
                branch_name,
                protected_branches,
                require_approval,
            } => {
                let repo_root = state
                    .ctx
                    .repo_root
                    .as_deref()
                    .ok_or(PipelineError::MissingRepoRoot)?;
                let base_branch = base_branch.as_deref().unwrap_or("main").to_string();
                let branch_name = render_template_or_err(&branch_name, &state.template_ctx())?;

                let action_hash = sha256_json(&serde_json::json!({
                    "op": "ensure_branch",
                    "repo_root": repo_root,
                    "base_branch": base_branch,
                    "branch_name": branch_name,
                    "protected": protected_branches,
                }));

                if require_approval {
                    state.waiting = Some(WaitingFor::Approval {
                        mode: ApprovalMode::Gate,
                        output_key: None,
                    });
                    return Ok(PipelineAction::RequestApproval {
                        scope: ApprovalScope::OneOff,
                        mode: ApprovalMode::Gate,
                        prompt: "Create/switch to a git branch for this task?".to_string(),
                        remediation: vec!["Deny to proceed without changing branches.".to_string()],
                        action_hash,
                        output_key: None,
                        phase: state.phase.clone(),
                    });
                }

                let mut cmd = vec![
                    "codex-pr".to_string(),
                    "repo".to_string(),
                    "ensure-branch".to_string(),
                    "--repo-root".to_string(),
                    repo_root.to_string(),
                    "--base-branch".to_string(),
                    base_branch,
                    "--branch-name".to_string(),
                    branch_name,
                ];
                for p in protected_branches {
                    cmd.push("--protected".to_string());
                    cmd.push(p);
                }
                cmd.push("--apply".to_string());

                state.waiting = Some(WaitingFor::Tool);
                Ok(PipelineAction::InvokeTool {
                    tool_call: ToolCall {
                        call_id: "pipeline-ensure-branch".to_string(),
                        tool_name: "local_shell".to_string(),
                        tool_kind: ToolKind::LocalShell,
                        input: ToolInput::LocalShell {
                            command: cmd,
                            workdir: Some(repo_root.to_string()),
                        },
                    },
                    phase: state.phase.clone(),
                })
            }
            Part::SetVar { key, value } => {
                let value = render_template_or_err(&value, &state.template_ctx())?;
                state.vars.insert(key, value);
                Ok(PipelineAction::EmitNote {
                    message: "set_var: ok".to_string(),
                })
            }
            Part::Fail { message } => {
                let message = render_template_or_err(&message, &state.template_ctx())?;
                state.failed = true;
                state.failure_message = Some(message);
                Ok(PipelineAction::EmitNote {
                    message: "fail: set failed".to_string(),
                })
            }
            Part::Defer { part } => {
                state.deferred.push(*part);
                Ok(PipelineAction::EmitNote {
                    message: "defer: registered".to_string(),
                })
            }
        }
    }
}

fn child_process_policy_supported(policy: ChildProcessPolicy) -> bool {
    match policy {
        ChildProcessPolicy::Inherit => true,
        // v1: plumbing only. Enforcement is platform-specific and may be implemented later.
        ChildProcessPolicy::AuditAllowlist | ChildProcessPolicy::EnforceAllowlist => false,
    }
}

fn default_require_approval_true() -> bool {
    true
}

fn sha256_json(value: &serde_json::Value) -> String {
    use sha2::Digest;
    use sha2::Sha256;

    let bytes = serde_json::to_vec(value).unwrap_or_default();
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

fn render_template_or_err(input: &str, ctx: &TemplateContext<'_>) -> Result<String, PipelineError> {
    render_template(input, ctx).map_err(|err| PipelineError::TemplateError(err.to_string()))
}

fn extract_value(
    state: &PipelineRunState,
    source: ExtractSource,
    parser: ExtractParser,
) -> Result<String, PipelineError> {
    if state.facts.last_tool_outcome.is_none() {
        return Err(PipelineError::MissingToolOutcome);
    }
    let input = match source {
        ExtractSource::Stdout => state.facts.last_tool_output_stdout.as_str(),
        ExtractSource::Stderr => state.facts.last_tool_output_stderr.as_str(),
        ExtractSource::Combined => state
            .facts
            .last_tool_outcome
            .as_ref()
            .map(|o| o.output_preview.as_str())
            .unwrap_or_default(),
    };

    const MAX_INPUT_BYTES: usize = 32 * 1024;
    let input = if input.len() > MAX_INPUT_BYTES {
        &input[..MAX_INPUT_BYTES]
    } else {
        input
    };

    match parser {
        ExtractParser::JsonPath { json_path } => {
            if !json_path.starts_with('/') {
                return Err(PipelineError::ExtractFailed(
                    "json_path must be a JSON pointer starting with '/'".to_string(),
                ));
            }
            let v: serde_json::Value = serde_json::from_str(input)
                .map_err(|err| PipelineError::ExtractFailed(format!("json parse failed: {err}")))?;
            let Some(found) = v.pointer(json_path.as_str()) else {
                return Err(PipelineError::ExtractFailed(format!(
                    "json_path not found: {json_path}"
                )));
            };
            let s = if let Some(s) = found.as_str() {
                s.to_string()
            } else {
                found.to_string()
            };
            Ok(bound_output(s))
        }
        ExtractParser::Regex { pattern, group } => {
            if pattern.len() > 512 {
                return Err(PipelineError::ExtractFailed("regex too large".to_string()));
            }
            let re = Regex::new(&pattern)
                .map_err(|err| PipelineError::ExtractFailed(format!("invalid regex: {err}")))?;
            let caps = re
                .captures(input)
                .ok_or_else(|| PipelineError::ExtractFailed("no match".to_string()))?;
            let out = match group {
                Some(idx) => caps
                    .get(idx)
                    .ok_or_else(|| PipelineError::ExtractFailed("missing capture".to_string()))?
                    .as_str()
                    .to_string(),
                None => caps
                    .get(0)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default(),
            };
            Ok(bound_output(out))
        }
    }
}

fn assert_fact(
    state: &PipelineRunState,
    key: &str,
    equals: Option<&str>,
    matches_regex: Option<&str>,
) -> Result<(), PipelineError> {
    let Some(value) = state.facts.derived.get(key) else {
        return Err(PipelineError::AssertFailed(format!("missing fact: {key}")));
    };
    if let Some(eq) = equals {
        if value != eq {
            return Err(PipelineError::AssertFailed(format!(
                "fact {key} expected {eq}, got {value}"
            )));
        }
    }
    if let Some(pat) = matches_regex {
        let re = Regex::new(pat)
            .map_err(|err| PipelineError::AssertFailed(format!("invalid regex: {err}")))?;
        if !re.is_match(value) {
            return Err(PipelineError::AssertFailed(format!(
                "fact {key} did not match regex"
            )));
        }
    }
    Ok(())
}

fn apply_normalize(mut s: String, ops: Vec<NormalizeOp>) -> String {
    for op in ops {
        match op {
            NormalizeOp::Trim => s = s.trim().to_string(),
            NormalizeOp::Lowercase => s = s.to_lowercase(),
        }
    }
    s
}

fn bound_output(mut s: String) -> String {
    const MAX_OUT_BYTES: usize = 1024;
    if s.len() > MAX_OUT_BYTES {
        s.truncate(MAX_OUT_BYTES);
    }
    s
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[derive(Default)]
    struct TestResolver {
        cmds: BTreeMap<String, CommandSpecV1>,
    }

    impl CommandResolver for TestResolver {
        fn resolve(&self, command_id: &str) -> Option<CommandSpecV1> {
            self.cmds.get(command_id).cloned()
        }
    }

    fn ctx() -> PipelineRunContext {
        PipelineRunContext {
            session_id: "s1".to_string(),
            task_id: Some("t1".to_string()),
            turn_id: Some("turn1".to_string()),
            vars: BTreeMap::new(),
            repo_root: Some("/repo".to_string()),
        }
    }

    fn engine() -> PipelineEngine {
        PipelineEngine::new(PipelineLimits { max_cycles: 50 })
    }

    #[test]
    fn runs_finally_and_defer_on_failure() {
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "cleanup".to_string(),
            Workflow {
                parts: vec![Part::SetVar {
                    key: "cleanup".to_string(),
                    value: "1".to_string(),
                }],
                finally_workflow: None,
            },
        );
        workflows.insert(
            "main".to_string(),
            Workflow {
                parts: vec![
                    Part::Defer {
                        part: Box::new(Part::SetVar {
                            key: "deferred".to_string(),
                            value: "1".to_string(),
                        }),
                    },
                    Part::Fail {
                        message: "boom".to_string(),
                    },
                ],
                finally_workflow: Some("cleanup".to_string()),
            },
        );
        let pipeline = Pipeline {
            workflows,
            entry: "main".to_string(),
        };
        let mut state = PipelineRunState::new(&pipeline, &PipelineLimits::default(), ctx());
        let resolver = TestResolver::default();
        let eng = engine();

        // Step through until done.
        loop {
            let action = eng.step(&pipeline, &mut state, &resolver).unwrap();
            match action {
                PipelineAction::Done { .. } => break,
                PipelineAction::InvokeTool { .. } => unreachable!(),
                PipelineAction::RequestApproval { .. } => unreachable!(),
                PipelineAction::EmitNote { .. } => {}
            }
        }

        assert_eq!(state.vars.get("cleanup"), Some(&"1".to_string()));
        assert_eq!(state.vars.get("deferred"), Some(&"1".to_string()));
        assert!(state.failed);
    }

    #[test]
    fn extract_and_assert_fact_works() {
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "main".to_string(),
            Workflow {
                parts: vec![
                    Part::RunTool {
                        tool_call: ToolCall {
                            call_id: "c1".to_string(),
                            tool_name: "local_shell".to_string(),
                            tool_kind: ToolKind::LocalShell,
                            input: ToolInput::LocalShell {
                                command: vec!["echo".to_string(), "hi".to_string()],
                                workdir: None,
                            },
                        },
                    },
                    Part::ExtractValue {
                        source: ExtractSource::Combined,
                        parser: ExtractParser::Regex {
                            pattern: "(?i)service is (down|up)".to_string(),
                            group: Some(1),
                        },
                        output_key: "svc".to_string(),
                        normalize: vec![NormalizeOp::Lowercase],
                    },
                    Part::AssertFact {
                        key: "svc".to_string(),
                        equals: Some("down".to_string()),
                        matches_regex: None,
                    },
                ],
                finally_workflow: None,
            },
        );
        let pipeline = Pipeline {
            workflows,
            entry: "main".to_string(),
        };
        let mut state = PipelineRunState::new(&pipeline, &PipelineLimits::default(), ctx());
        let resolver = TestResolver::default();
        let eng = engine();

        // RunTool
        let action = eng.step(&pipeline, &mut state, &resolver).unwrap();
        let PipelineAction::InvokeTool { .. } = action else {
            panic!("expected InvokeTool");
        };
        eng.provide_tool_outcome(
            &mut state,
            ToolOutcome {
                executed: true,
                success: true,
                duration_ms: 1,
                output_preview: "Service is DOWN".to_string(),
            },
            "Service is DOWN".to_string(),
            "".to_string(),
        )
        .unwrap();

        // ExtractValue and AssertFact
        loop {
            let action = eng.step(&pipeline, &mut state, &resolver).unwrap();
            match action {
                PipelineAction::EmitNote { .. } => {}
                PipelineAction::Done { .. } => break,
                PipelineAction::InvokeTool { .. } => unreachable!(),
                PipelineAction::RequestApproval { .. } => unreachable!(),
            }
        }

        assert_eq!(state.facts.derived.get("svc"), Some(&"down".to_string()));
    }

    #[test]
    fn require_approval_confirm_writes_bool_and_continues() {
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "main".to_string(),
            Workflow {
                parts: vec![
                    Part::RequireApproval {
                        scope: ApprovalScope::OneOff,
                        mode: ApprovalMode::Confirm,
                        prompt: "cleanup now?".to_string(),
                        remediation: vec![],
                        action_hash: "h1".to_string(),
                        output_key: Some("cleanup_ok".to_string()),
                    },
                    Part::AssertFact {
                        key: "cleanup_ok".to_string(),
                        equals: Some("false".to_string()),
                        matches_regex: None,
                    },
                ],
                finally_workflow: None,
            },
        );
        let pipeline = Pipeline {
            workflows,
            entry: "main".to_string(),
        };
        let mut state = PipelineRunState::new(&pipeline, &PipelineLimits::default(), ctx());
        let resolver = TestResolver::default();
        let eng = engine();

        let action = eng.step(&pipeline, &mut state, &resolver).unwrap();
        let PipelineAction::RequestApproval { .. } = action else {
            panic!("expected RequestApproval");
        };
        eng.provide_approval_result(&mut state, false).unwrap();

        loop {
            let action = eng.step(&pipeline, &mut state, &resolver).unwrap();
            match action {
                PipelineAction::Done { .. } => break,
                PipelineAction::EmitNote { .. } => {}
                PipelineAction::InvokeTool { .. } => unreachable!(),
                PipelineAction::RequestApproval { .. } => unreachable!(),
            }
        }

        assert_eq!(
            state.facts.derived.get("cleanup_ok"),
            Some(&"false".to_string())
        );
        assert!(!state.failed);
    }

    #[test]
    fn command_id_emits_note_then_invokes_tool() {
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "main".to_string(),
            Workflow {
                parts: vec![Part::RunCommand {
                    cwd: None,
                    argv: None,
                    command_id: Some("frontend:test".to_string()),
                    timeout_ms: None,
                    child_process_policy: ChildProcessPolicy::Inherit,
                }],
                finally_workflow: None,
            },
        );
        let pipeline = Pipeline {
            workflows,
            entry: "main".to_string(),
        };

        let mut resolver = TestResolver::default();
        resolver.cmds.insert(
            "frontend:test".to_string(),
            CommandSpecV1 {
                id: "frontend:test".to_string(),
                name: "Frontend tests".to_string(),
                cwd: ".".to_string(),
                argv: vec!["npm".to_string(), "test".to_string()],
                category: None,
                timeout_ms: None,
                child_process_policy: ChildProcessPolicy::Inherit,
                script_runner: true,
                requires_sandbox: false,
            },
        );

        let mut state = PipelineRunState::new(&pipeline, &PipelineLimits::default(), ctx());
        let eng = engine();

        let first = eng.step(&pipeline, &mut state, &resolver).unwrap();
        let PipelineAction::EmitNote { message } = first else {
            panic!("expected EmitNote");
        };
        assert!(message.contains("Resolved command_id"));

        let second = eng.step(&pipeline, &mut state, &resolver).unwrap();
        let PipelineAction::InvokeTool { tool_call, .. } = second else {
            panic!("expected InvokeTool");
        };
        assert_eq!(
            tool_call.input,
            ToolInput::LocalShell {
                command: vec!["npm".to_string(), "test".to_string()],
                workdir: Some(".".to_string())
            }
        );
    }

    #[test]
    fn ensure_worktree_emits_approval_request_by_default() {
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "main".to_string(),
            Workflow {
                parts: vec![Part::EnsureWorktree {
                    worktree_root: "/tmp/worktrees".to_string(),
                    naming: "prcc/${session_id}".to_string(),
                    base_branch: Some("main".to_string()),
                    branch_name: "prcc/${session_id}".to_string(),
                    require_approval: true,
                }],
                finally_workflow: None,
            },
        );
        let pipeline = Pipeline {
            workflows,
            entry: "main".to_string(),
        };
        let mut state = PipelineRunState::new(&pipeline, &PipelineLimits::default(), ctx());
        let resolver = TestResolver::default();
        let eng = engine();

        let action = eng.step(&pipeline, &mut state, &resolver).unwrap();
        assert!(matches!(action, PipelineAction::RequestApproval { .. }));
    }
}
