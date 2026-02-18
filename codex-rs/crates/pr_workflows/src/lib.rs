use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use codex_pr_types::WorkflowComponentV1;
use codex_pr_types::WorkflowEntryV1;
use codex_pr_types::WorkflowGraphV1;
use codex_pr_types::WorkflowStepV1;
use codex_pr_types::WorkflowsFileV1;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use thiserror::Error;

const WORKFLOWS_SCHEMA_VERSION: &str = "1";

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("invalid run state: {0}")]
    InvalidRunState(String),
    #[error("missing step: {0}")]
    MissingStep(String),
    #[error("missing feedback key: {0}")]
    MissingFeedbackKey(String),
    #[error("artifact too large: {actual_bytes} > {max_bytes}")]
    ArtifactTooLarge { actual_bytes: u64, max_bytes: u64 },
    #[error("feedback too large: {actual_bytes} > {max_bytes}")]
    FeedbackTooLarge { actual_bytes: u64, max_bytes: u64 },
}

pub fn read_workflows_file(path: &Path) -> Result<WorkflowsFileV1, WorkflowError> {
    if !path.exists() {
        return Ok(WorkflowsFileV1::default());
    }
    let raw = std::fs::read_to_string(path)?;
    let file = serde_json::from_str::<WorkflowsFileV1>(&raw)?;
    file.validate().map_err(WorkflowError::Validation)?;
    Ok(file)
}

pub fn write_workflows_file(path: &Path, file: &WorkflowsFileV1) -> Result<(), WorkflowError> {
    file.validate().map_err(WorkflowError::Validation)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_string_pretty(file)?;
    std::fs::write(path, encoded)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ArtifactStoreConfig {
    pub root: PathBuf,
    pub max_artifact_bytes: u64,
    pub max_feedback_bytes: u64,
}

impl ArtifactStoreConfig {
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            max_artifact_bytes: 262_144,
            max_feedback_bytes: 8_192,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    cfg: ArtifactStoreConfig,
}

#[derive(Debug, Clone)]
pub struct PersistArtifactRequest<'a> {
    pub run_id: &'a str,
    pub artifact_ref: &'a str,
    pub artifact_kind: &'a str,
    pub content: &'a str,
}

#[derive(Debug, Clone)]
pub struct PersistFeedbackRequest<'a> {
    pub run_id: &'a str,
    pub step_id: &'a str,
    pub feedback_key: &'a str,
    pub feedback: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub run_id: String,
    pub artifact_ref: String,
    pub artifact_kind: String,
    pub sha256: String,
    pub bytes: u64,
    pub created_at: DateTime<Utc>,
    pub object_rel_path: String,
    pub metadata_rel_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackRecord {
    pub run_id: String,
    pub step_id: String,
    pub feedback_key: String,
    pub sha256: String,
    pub bytes: u64,
    pub created_at: DateTime<Utc>,
    pub content_rel_path: String,
    pub metadata_rel_path: String,
}

impl ArtifactStore {
    pub fn new(cfg: ArtifactStoreConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &ArtifactStoreConfig {
        &self.cfg
    }

    pub fn persist_artifact(
        &self,
        req: PersistArtifactRequest<'_>,
    ) -> Result<ArtifactRecord, WorkflowError> {
        let bytes = req.content.as_bytes();
        if (bytes.len() as u64) > self.cfg.max_artifact_bytes {
            return Err(WorkflowError::ArtifactTooLarge {
                actual_bytes: bytes.len() as u64,
                max_bytes: self.cfg.max_artifact_bytes,
            });
        }

        let sha256 = sha256_hex(bytes);
        let created_at = Utc::now();

        let objects_dir = self.cfg.root.join("objects");
        std::fs::create_dir_all(&objects_dir)?;

        let object_filename = format!("{sha256}.txt");
        let object_path = objects_dir.join(&object_filename);
        if !object_path.exists() {
            std::fs::write(&object_path, bytes)?;
        }

        let artifact_ref_segment = sanitize_path_segment(req.artifact_ref);
        let run_artifacts_dir = self
            .cfg
            .root
            .join("runs")
            .join(req.run_id)
            .join("artifacts");
        std::fs::create_dir_all(&run_artifacts_dir)?;

        let metadata_filename = format!("{artifact_ref_segment}.meta.json");
        let metadata_path = run_artifacts_dir.join(&metadata_filename);

        let record = ArtifactRecord {
            run_id: req.run_id.to_string(),
            artifact_ref: req.artifact_ref.to_string(),
            artifact_kind: req.artifact_kind.to_string(),
            sha256,
            bytes: bytes.len() as u64,
            created_at,
            object_rel_path: format!("objects/{object_filename}"),
            metadata_rel_path: format!("runs/{}/artifacts/{metadata_filename}", req.run_id),
        };

        std::fs::write(&metadata_path, serde_json::to_string_pretty(&record)?)?;
        Ok(record)
    }

    pub fn persist_feedback(
        &self,
        req: PersistFeedbackRequest<'_>,
    ) -> Result<FeedbackRecord, WorkflowError> {
        let bytes = req.feedback.as_bytes();
        if (bytes.len() as u64) > self.cfg.max_feedback_bytes {
            return Err(WorkflowError::FeedbackTooLarge {
                actual_bytes: bytes.len() as u64,
                max_bytes: self.cfg.max_feedback_bytes,
            });
        }

        let sha256 = sha256_hex(bytes);
        let created_at = Utc::now();
        let step_segment = sanitize_path_segment(req.step_id);
        let feedback_name = format!("{step_segment}-{}.txt", short_hash(&sha256));
        let run_feedback_dir = self.cfg.root.join("runs").join(req.run_id).join("feedback");
        std::fs::create_dir_all(&run_feedback_dir)?;

        let feedback_path = run_feedback_dir.join(&feedback_name);
        std::fs::write(&feedback_path, bytes)?;

        let meta_name = format!("{step_segment}-{}.meta.json", short_hash(&sha256));
        let metadata_path = run_feedback_dir.join(&meta_name);

        let record = FeedbackRecord {
            run_id: req.run_id.to_string(),
            step_id: req.step_id.to_string(),
            feedback_key: req.feedback_key.to_string(),
            sha256,
            bytes: bytes.len() as u64,
            created_at,
            content_rel_path: format!("runs/{}/feedback/{feedback_name}", req.run_id),
            metadata_rel_path: format!("runs/{}/feedback/{meta_name}", req.run_id),
        };

        std::fs::write(&metadata_path, serde_json::to_string_pretty(&record)?)?;
        Ok(record)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowEngineLimits {
    pub default_max_revisions: u32,
}

impl Default for WorkflowEngineLimits {
    fn default() -> Self {
        Self {
            default_max_revisions: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkflowEngine {
    graph: WorkflowGraphV1,
    components: BTreeMap<String, WorkflowComponentV1>,
    limits: WorkflowEngineLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    Waiting,
    Completed,
    NeedsHuman,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "pending_kind", rename_all = "snake_case")]
pub enum PendingAction {
    InvokeAgent {
        step_id: String,
        artifact_ref: String,
        artifact_kind: String,
        next_step: Option<String>,
    },
    ReviewArtifact {
        step_id: String,
    },
    RunPipeline {
        step_id: String,
        next_step: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunState {
    pub run_id: String,
    #[serde(default)]
    pub workflow_id: Option<String>,
    pub current_step_id: String,
    pub status: WorkflowRunStatus,
    #[serde(default)]
    pub revision_counters: BTreeMap<String, u32>,
    #[serde(default)]
    pub feedback: BTreeMap<String, String>,
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactRecord>,
    pub pending: Option<PendingAction>,
    pub last_error: Option<String>,
}

impl WorkflowRunState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            WorkflowRunStatus::Completed
                | WorkflowRunStatus::NeedsHuman
                | WorkflowRunStatus::Failed
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action_kind", rename_all = "snake_case")]
pub enum WorkflowAction {
    InvokeAgent {
        run_id: String,
        step_id: String,
        template_id: String,
        artifact_ref: String,
        artifact_kind: String,
        inputs: BTreeMap<String, String>,
        feedback: Option<String>,
    },
    RequestReview {
        run_id: String,
        step_id: String,
        artifact_ref: String,
        prompt: String,
        max_revisions: u32,
        current_revisions: u32,
    },
    RunPipeline {
        run_id: String,
        step_id: String,
        pipeline_id: String,
        pipeline_scope: Option<String>,
    },
    Complete {
        run_id: String,
        step_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result_kind", rename_all = "snake_case")]
pub enum WorkflowActionResult {
    AgentOutput {
        content: String,
    },
    ReviewDecision {
        approved: bool,
        feedback: Option<String>,
    },
    PipelineOutput {
        success: bool,
        summary: Option<String>,
    },
}

impl WorkflowEngine {
    pub fn new(
        graph: WorkflowGraphV1,
        limits: WorkflowEngineLimits,
    ) -> Result<Self, WorkflowError> {
        Self::new_with_components(graph, BTreeMap::new(), limits)
    }

    pub fn new_with_components(
        graph: WorkflowGraphV1,
        components: BTreeMap<String, WorkflowComponentV1>,
        limits: WorkflowEngineLimits,
    ) -> Result<Self, WorkflowError> {
        validate_graph(&graph, &components)?;
        Ok(Self {
            graph,
            components,
            limits,
        })
    }

    pub fn action_for_pending(
        &self,
        state: &WorkflowRunState,
    ) -> Result<Option<WorkflowAction>, WorkflowError> {
        let Some(pending) = state.pending.as_ref() else {
            return Ok(None);
        };
        let step = self.materialize_step(&state.current_step_id)?;
        let action = match (pending, step) {
            (
                PendingAction::InvokeAgent {
                    step_id,
                    artifact_ref,
                    artifact_kind,
                    ..
                },
                WorkflowStepV1::GenerateArtifact {
                    template_id,
                    inputs,
                    ..
                },
            ) => WorkflowAction::InvokeAgent {
                run_id: state.run_id.clone(),
                step_id: step_id.clone(),
                template_id,
                artifact_ref: artifact_ref.clone(),
                artifact_kind: artifact_kind.clone(),
                inputs,
                feedback: None,
            },
            (
                PendingAction::InvokeAgent {
                    step_id,
                    artifact_ref,
                    artifact_kind,
                    ..
                },
                WorkflowStepV1::ReviseArtifact {
                    template_id,
                    feedback_key,
                    ..
                },
            ) => WorkflowAction::InvokeAgent {
                run_id: state.run_id.clone(),
                step_id: step_id.clone(),
                template_id,
                artifact_ref: artifact_ref.clone(),
                artifact_kind: artifact_kind.clone(),
                inputs: BTreeMap::new(),
                feedback: state.feedback.get(&feedback_key).cloned(),
            },
            (
                PendingAction::ReviewArtifact { step_id },
                WorkflowStepV1::ReviewArtifact {
                    artifact_ref,
                    prompt,
                    max_revisions,
                    revision_counter_key,
                    ..
                },
            ) => {
                let counter_key = revision_counter_lookup_key(step_id, &revision_counter_key);
                let current_revisions = state
                    .revision_counters
                    .get(&counter_key)
                    .copied()
                    .unwrap_or(0);
                WorkflowAction::RequestReview {
                    run_id: state.run_id.clone(),
                    step_id: step_id.clone(),
                    artifact_ref,
                    prompt,
                    max_revisions,
                    current_revisions,
                }
            }
            (
                PendingAction::RunPipeline { step_id, .. },
                WorkflowStepV1::RunPipeline {
                    pipeline_id,
                    pipeline_scope,
                    ..
                },
            ) => WorkflowAction::RunPipeline {
                run_id: state.run_id.clone(),
                step_id: step_id.clone(),
                pipeline_id,
                pipeline_scope,
            },
            _ => {
                return Err(WorkflowError::InvalidRunState(
                    "pending action does not match current step".to_string(),
                ));
            }
        };
        Ok(Some(action))
    }

    pub fn start(&self, run_id: impl Into<String>) -> WorkflowRunState {
        WorkflowRunState {
            run_id: run_id.into(),
            workflow_id: None,
            current_step_id: self.graph.entry.clone(),
            status: WorkflowRunStatus::Running,
            revision_counters: BTreeMap::new(),
            feedback: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            pending: None,
            last_error: None,
        }
    }

    pub fn next_action(
        &self,
        state: &mut WorkflowRunState,
    ) -> Result<Option<WorkflowAction>, WorkflowError> {
        if state.is_terminal() {
            return Ok(None);
        }
        if state.pending.is_some() {
            return Err(WorkflowError::InvalidRunState(
                "cannot produce next action while a previous action is pending".to_string(),
            ));
        }

        let step = self.materialize_step(&state.current_step_id)?;

        match &step {
            WorkflowStepV1::GenerateArtifact {
                template_id,
                artifact_kind,
                inputs,
                next_step,
            } => {
                state.status = WorkflowRunStatus::Waiting;
                state.pending = Some(PendingAction::InvokeAgent {
                    step_id: state.current_step_id.clone(),
                    artifact_ref: artifact_kind.clone(),
                    artifact_kind: artifact_kind.clone(),
                    next_step: next_step.clone(),
                });
                Ok(Some(WorkflowAction::InvokeAgent {
                    run_id: state.run_id.clone(),
                    step_id: state.current_step_id.clone(),
                    template_id: template_id.to_string(),
                    artifact_ref: artifact_kind.to_string(),
                    artifact_kind: artifact_kind.to_string(),
                    inputs: inputs.clone(),
                    feedback: None,
                }))
            }
            WorkflowStepV1::ReviewArtifact {
                artifact_ref,
                prompt,
                max_revisions,
                revision_counter_key,
                ..
            } => {
                let counter_key =
                    revision_counter_lookup_key(&state.current_step_id, revision_counter_key);
                let current_revisions = state
                    .revision_counters
                    .get(&counter_key)
                    .copied()
                    .unwrap_or(0);
                state.status = WorkflowRunStatus::Waiting;
                state.pending = Some(PendingAction::ReviewArtifact {
                    step_id: state.current_step_id.clone(),
                });
                Ok(Some(WorkflowAction::RequestReview {
                    run_id: state.run_id.clone(),
                    step_id: state.current_step_id.clone(),
                    artifact_ref: artifact_ref.to_string(),
                    prompt: prompt.to_string(),
                    max_revisions: *max_revisions,
                    current_revisions,
                }))
            }
            WorkflowStepV1::ReviseArtifact {
                template_id,
                artifact_ref,
                feedback_key,
                next_step,
            } => {
                let feedback = state
                    .feedback
                    .get(feedback_key)
                    .cloned()
                    .ok_or_else(|| WorkflowError::MissingFeedbackKey(feedback_key.clone()))?;
                state.status = WorkflowRunStatus::Waiting;
                state.pending = Some(PendingAction::InvokeAgent {
                    step_id: state.current_step_id.clone(),
                    artifact_ref: artifact_ref.clone(),
                    artifact_kind: artifact_ref.clone(),
                    next_step: next_step.clone(),
                });
                Ok(Some(WorkflowAction::InvokeAgent {
                    run_id: state.run_id.clone(),
                    step_id: state.current_step_id.clone(),
                    template_id: template_id.to_string(),
                    artifact_ref: artifact_ref.to_string(),
                    artifact_kind: artifact_ref.to_string(),
                    inputs: BTreeMap::new(),
                    feedback: Some(feedback),
                }))
            }
            WorkflowStepV1::RunPipeline {
                pipeline_id,
                pipeline_scope,
                next_step,
            } => {
                state.status = WorkflowRunStatus::Waiting;
                state.pending = Some(PendingAction::RunPipeline {
                    step_id: state.current_step_id.clone(),
                    next_step: next_step.clone(),
                });
                Ok(Some(WorkflowAction::RunPipeline {
                    run_id: state.run_id.clone(),
                    step_id: state.current_step_id.clone(),
                    pipeline_id: pipeline_id.to_string(),
                    pipeline_scope: pipeline_scope.clone(),
                }))
            }
            WorkflowStepV1::Complete => {
                state.status = WorkflowRunStatus::Completed;
                Ok(Some(WorkflowAction::Complete {
                    run_id: state.run_id.clone(),
                    step_id: state.current_step_id.clone(),
                }))
            }
            WorkflowStepV1::UseComponent { .. } => Err(WorkflowError::InvalidRunState(
                "use_component must be materialized before execution".to_string(),
            )),
        }
    }

    pub fn apply_action_result(
        &self,
        state: &mut WorkflowRunState,
        action: &WorkflowAction,
        result: WorkflowActionResult,
        artifact_store: &ArtifactStore,
    ) -> Result<(), WorkflowError> {
        if state.pending.is_none() {
            return Err(WorkflowError::InvalidRunState(
                "no pending action to apply a result to".to_string(),
            ));
        }

        match (state.pending.clone(), action, result) {
            (
                Some(PendingAction::InvokeAgent {
                    step_id,
                    artifact_ref,
                    artifact_kind,
                    next_step,
                }),
                WorkflowAction::InvokeAgent {
                    step_id: action_step_id,
                    ..
                },
                WorkflowActionResult::AgentOutput { content },
            ) => {
                if &step_id != action_step_id {
                    return Err(WorkflowError::InvalidRunState(format!(
                        "invoke_agent result step mismatch: pending={step_id} action={action_step_id}"
                    )));
                }

                let record = artifact_store.persist_artifact(PersistArtifactRequest {
                    run_id: &state.run_id,
                    artifact_ref: &artifact_ref,
                    artifact_kind: &artifact_kind,
                    content: &content,
                })?;
                state.artifacts.insert(artifact_ref, record);
                state.pending = None;
                transition_to_next_step(state, next_step);
                Ok(())
            }
            (
                Some(PendingAction::ReviewArtifact { step_id }),
                WorkflowAction::RequestReview {
                    step_id: action_step_id,
                    ..
                },
                WorkflowActionResult::ReviewDecision { approved, feedback },
            ) => {
                if &step_id != action_step_id {
                    return Err(WorkflowError::InvalidRunState(format!(
                        "review result step mismatch: pending={step_id} action={action_step_id}"
                    )));
                }
                let step = self.materialize_step(&step_id)?;
                let WorkflowStepV1::ReviewArtifact {
                    on_approved,
                    on_feedback,
                    max_revisions,
                    revision_counter_key,
                    ..
                } = step
                else {
                    return Err(WorkflowError::InvalidRunState(format!(
                        "step {step_id} is not review_artifact"
                    )));
                };

                state.pending = None;
                if approved {
                    state.status = WorkflowRunStatus::Running;
                    state.current_step_id = on_approved;
                    return Ok(());
                }

                let feedback_text = feedback.unwrap_or_default();
                artifact_store.persist_feedback(PersistFeedbackRequest {
                    run_id: &state.run_id,
                    step_id: &step_id,
                    feedback_key: &format!(
                        "{}.feedback",
                        revision_counter_lookup_key(&step_id, &revision_counter_key)
                    ),
                    feedback: &feedback_text,
                })?;

                let counter_key = revision_counter_lookup_key(&step_id, &revision_counter_key);
                let counter = state
                    .revision_counters
                    .entry(counter_key.clone())
                    .or_insert(0);
                *counter += 1;

                let key_for_feedback = format!("{counter_key}.feedback");
                state.feedback.insert(key_for_feedback, feedback_text);

                let effective_max = if max_revisions == 0 {
                    self.limits.default_max_revisions
                } else {
                    max_revisions
                };

                if *counter > effective_max {
                    state.status = WorkflowRunStatus::NeedsHuman;
                    state.last_error = Some(format!(
                        "revision bound exceeded at {}: {} > {}",
                        step_id, *counter, effective_max
                    ));
                    return Ok(());
                }

                state.status = WorkflowRunStatus::Running;
                state.current_step_id = on_feedback;
                Ok(())
            }
            (
                Some(PendingAction::RunPipeline { step_id, next_step }),
                WorkflowAction::RunPipeline {
                    step_id: action_step_id,
                    ..
                },
                WorkflowActionResult::PipelineOutput { success, summary },
            ) => {
                if &step_id != action_step_id {
                    return Err(WorkflowError::InvalidRunState(format!(
                        "run_pipeline result step mismatch: pending={step_id} action={action_step_id}"
                    )));
                }
                state.pending = None;
                if success {
                    transition_to_next_step(state, next_step);
                } else {
                    state.status = WorkflowRunStatus::Failed;
                    state.last_error = summary;
                }
                Ok(())
            }
            _ => Err(WorkflowError::InvalidRunState(
                "action/result pair does not match pending step".to_string(),
            )),
        }
    }

    fn materialize_step(&self, step_id: &str) -> Result<WorkflowStepV1, WorkflowError> {
        let step = self
            .graph
            .steps
            .get(step_id)
            .ok_or_else(|| WorkflowError::MissingStep(step_id.to_string()))?
            .clone();
        let WorkflowStepV1::UseComponent {
            component_id,
            next_step,
            ..
        } = step
        else {
            return Ok(step);
        };
        let component = self
            .components
            .get(&component_id)
            .ok_or_else(|| WorkflowError::MissingStep(component_id.clone()))?;
        let mut component_step = component.step.clone();
        match &mut component_step {
            WorkflowStepV1::GenerateArtifact {
                next_step: component_next,
                ..
            }
            | WorkflowStepV1::ReviseArtifact {
                next_step: component_next,
                ..
            }
            | WorkflowStepV1::RunPipeline {
                next_step: component_next,
                ..
            } => {
                if next_step.is_some() {
                    *component_next = next_step;
                }
            }
            WorkflowStepV1::ReviewArtifact { .. }
            | WorkflowStepV1::UseComponent { .. }
            | WorkflowStepV1::Complete => {
                return Err(WorkflowError::InvalidRunState(format!(
                    "component {component_id} has unsupported step for use_component"
                )));
            }
        }
        Ok(component_step)
    }
}

pub fn write_run_state(path: &Path, state: &WorkflowRunState) -> Result<(), WorkflowError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_string_pretty(state)?;
    std::fs::write(path, encoded)?;
    Ok(())
}

pub fn read_run_state(path: &Path) -> Result<WorkflowRunState, WorkflowError> {
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str::<WorkflowRunState>(&raw)?)
}

pub fn action_to_json(action: &WorkflowAction) -> Result<String, WorkflowError> {
    Ok(serde_json::to_string_pretty(action)?)
}

pub fn action_result_from_json(json: &str) -> Result<WorkflowActionResult, WorkflowError> {
    Ok(serde_json::from_str::<WorkflowActionResult>(json)?)
}

fn validate_graph(
    graph: &WorkflowGraphV1,
    components: &BTreeMap<String, WorkflowComponentV1>,
) -> Result<(), WorkflowError> {
    let mut workflows = BTreeMap::<String, WorkflowEntryV1>::new();
    workflows.insert(
        "_validation".to_string(),
        WorkflowEntryV1 {
            id: "_validation".to_string(),
            name: "Validation".to_string(),
            enabled: true,
            workflow: graph.clone(),
        },
    );
    let file = WorkflowsFileV1 {
        schema_version: WORKFLOWS_SCHEMA_VERSION.to_string(),
        components: components.clone(),
        workflows,
    };
    file.validate().map_err(WorkflowError::Validation)
}

fn transition_to_next_step(state: &mut WorkflowRunState, next_step: Option<String>) {
    match next_step {
        Some(next_step) => {
            state.current_step_id = next_step;
            state.status = WorkflowRunStatus::Running;
        }
        None => {
            state.status = WorkflowRunStatus::Completed;
        }
    }
}

fn revision_counter_lookup_key(step_id: &str, raw: &str) -> String {
    if raw.trim().is_empty() {
        step_id.to_string()
    } else {
        raw.to_string()
    }
}

fn sanitize_path_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        return "item".to_string();
    }
    out
}

fn short_hash(hex: &str) -> &str {
    let end = hex.len().min(12);
    &hex[..end]
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn simple_graph(max_revisions: u32) -> WorkflowGraphV1 {
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
                prompt: "review".to_string(),
                on_approved: "complete".to_string(),
                on_feedback: "revise_prd".to_string(),
                max_revisions,
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
        steps.insert("complete".to_string(), WorkflowStepV1::Complete);

        WorkflowGraphV1 {
            entry: "generate_prd".to_string(),
            steps,
        }
    }

    #[test]
    fn schema_io_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workflows.json");

        let mut file = WorkflowsFileV1::default();
        file.workflows.insert(
            "flow".to_string(),
            WorkflowEntryV1 {
                id: "flow".to_string(),
                name: "Flow".to_string(),
                enabled: false,
                workflow: simple_graph(3),
            },
        );

        write_workflows_file(&path, &file).expect("write");
        let loaded = read_workflows_file(&path).expect("read");
        assert_eq!(loaded, file);
    }

    #[test]
    fn artifact_store_persists_content_addressed_artifact_and_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::new(ArtifactStoreConfig::with_root(dir.path().to_path_buf()));

        let record = store
            .persist_artifact(PersistArtifactRequest {
                run_id: "r1",
                artifact_ref: "prd",
                artifact_kind: "prd",
                content: "hello artifact",
            })
            .expect("persist");

        let object_path = dir.path().join(&record.object_rel_path);
        let metadata_path = dir.path().join(&record.metadata_rel_path);
        assert_eq!(object_path.exists(), true);
        assert_eq!(metadata_path.exists(), true);
    }

    #[test]
    fn artifact_store_rejects_oversized_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = ArtifactStoreConfig::with_root(dir.path().to_path_buf());
        cfg.max_artifact_bytes = 4;
        let store = ArtifactStore::new(cfg);
        let result = store.persist_artifact(PersistArtifactRequest {
            run_id: "r1",
            artifact_ref: "prd",
            artifact_kind: "prd",
            content: "too large",
        });
        assert_eq!(
            matches!(result, Err(WorkflowError::ArtifactTooLarge { .. })),
            true
        );
    }

    #[test]
    fn engine_approve_path_reaches_complete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::new(ArtifactStoreConfig::with_root(dir.path().to_path_buf()));
        let engine =
            WorkflowEngine::new(simple_graph(3), WorkflowEngineLimits::default()).expect("engine");
        let mut state = engine.start("run-approve");

        let a1 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a1,
                WorkflowActionResult::AgentOutput {
                    content: "prd v1".to_string(),
                },
                &store,
            )
            .expect("apply");

        let a2 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a2,
                WorkflowActionResult::ReviewDecision {
                    approved: true,
                    feedback: None,
                },
                &store,
            )
            .expect("apply");

        let a3 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        assert_eq!(matches!(a3, WorkflowAction::Complete { .. }), true);
        assert_eq!(state.status, WorkflowRunStatus::Completed);
    }

    #[test]
    fn engine_feedback_path_revises_then_completes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::new(ArtifactStoreConfig::with_root(dir.path().to_path_buf()));
        let engine =
            WorkflowEngine::new(simple_graph(3), WorkflowEngineLimits::default()).expect("engine");
        let mut state = engine.start("run-feedback");

        let a1 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a1,
                WorkflowActionResult::AgentOutput {
                    content: "prd v1".to_string(),
                },
                &store,
            )
            .expect("apply");

        let a2 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a2,
                WorkflowActionResult::ReviewDecision {
                    approved: false,
                    feedback: Some("needs clearer scope".to_string()),
                },
                &store,
            )
            .expect("apply");

        let a3 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        assert_eq!(matches!(a3, WorkflowAction::InvokeAgent { .. }), true);
        engine
            .apply_action_result(
                &mut state,
                &a3,
                WorkflowActionResult::AgentOutput {
                    content: "prd v2".to_string(),
                },
                &store,
            )
            .expect("apply");

        let a4 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a4,
                WorkflowActionResult::ReviewDecision {
                    approved: true,
                    feedback: None,
                },
                &store,
            )
            .expect("apply");

        let _ = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        assert_eq!(state.status, WorkflowRunStatus::Completed);
    }

    #[test]
    fn engine_enforces_revision_bounds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ArtifactStore::new(ArtifactStoreConfig::with_root(dir.path().to_path_buf()));
        let engine =
            WorkflowEngine::new(simple_graph(1), WorkflowEngineLimits::default()).expect("engine");
        let mut state = engine.start("run-bound");

        let a1 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a1,
                WorkflowActionResult::AgentOutput {
                    content: "prd v1".to_string(),
                },
                &store,
            )
            .expect("apply");

        let a2 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a2,
                WorkflowActionResult::ReviewDecision {
                    approved: false,
                    feedback: Some("first".to_string()),
                },
                &store,
            )
            .expect("apply");

        let a3 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a3,
                WorkflowActionResult::AgentOutput {
                    content: "prd v2".to_string(),
                },
                &store,
            )
            .expect("apply");

        let a4 = engine
            .next_action(&mut state)
            .expect("next")
            .expect("action");
        engine
            .apply_action_result(
                &mut state,
                &a4,
                WorkflowActionResult::ReviewDecision {
                    approved: false,
                    feedback: Some("second".to_string()),
                },
                &store,
            )
            .expect("apply");

        assert_eq!(state.status, WorkflowRunStatus::NeedsHuman);
    }

    #[test]
    fn action_protocol_json_roundtrip() {
        let action = WorkflowAction::RequestReview {
            run_id: "r1".to_string(),
            step_id: "review_prd".to_string(),
            artifact_ref: "prd".to_string(),
            prompt: "Review".to_string(),
            max_revisions: 3,
            current_revisions: 1,
        };
        let json = action_to_json(&action).expect("json");
        let roundtrip = serde_json::from_str::<WorkflowAction>(&json).expect("decode");
        assert_eq!(roundtrip, action);

        let result_json = r#"{"result_kind":"review_decision","approved":true,"feedback":null}"#;
        let result = action_result_from_json(result_json).expect("result");
        assert_eq!(
            result,
            WorkflowActionResult::ReviewDecision {
                approved: true,
                feedback: None
            }
        );
    }
}
