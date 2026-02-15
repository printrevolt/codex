use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use codex_pr_audit::AuditAfterTool;
use codex_pr_audit::AuditBeforeTool;
use codex_pr_audit::AuditDecision;
use codex_pr_audit::AuditEvent;
use codex_pr_audit::AuditEventKind;
use codex_pr_audit::AuditSink;
use codex_pr_audit::JsonlAuditSink;
use codex_pr_config::resolve_printrevolt_config;
use codex_pr_hooks::HookRunner;
use codex_pr_hooks::HookSpec;
use codex_pr_policy::PolicyEngine;
use codex_pr_templates::TemplateRef;
use codex_pr_types::DecisionKind;
use codex_pr_types::HookPayloadV2;
use codex_pr_types::LifecycleEventKind;
use codex_pr_types::PrintRevoltConfig;
use codex_pr_types::SessionContext;
use codex_pr_types::ToolCall;
use codex_pr_types::ToolCallDecision;
use codex_pr_types::ToolOutcome;
use codex_pr_types::VerifyEvidence;
use codex_protocol::ThreadId;
use tokio::sync::Mutex;

#[cfg(feature = "probe")]
pub mod probe {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use codex_protocol::ThreadId;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum LifecycleEvent {
        SessionStart {
            session_id: ThreadId,
        },
        BeforeTask {
            session_id: ThreadId,
            turn_id: String,
        },
        BeforeFinalize {
            session_id: ThreadId,
            turn_id: String,
        },
        SessionEnd {
            session_id: ThreadId,
        },
    }

    static EVENTS: OnceLock<Mutex<Vec<LifecycleEvent>>> = OnceLock::new();

    fn events() -> &'static Mutex<Vec<LifecycleEvent>> {
        EVENTS.get_or_init(|| Mutex::new(Vec::new()))
    }

    pub fn reset() {
        let mut guard = events().lock().expect("lock probe events");
        guard.clear();
    }

    pub fn take() -> Vec<LifecycleEvent> {
        let mut guard = events().lock().expect("lock probe events");
        std::mem::take(&mut *guard)
    }

    pub(crate) fn record(event: LifecycleEvent) {
        let mut guard = events().lock().expect("lock probe events");
        guard.push(event);
    }
}

#[derive(Clone)]
pub struct PrRuntime {
    state: Arc<Mutex<PrRuntimeState>>,
}

impl Default for PrRuntime {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(PrRuntimeState::default())),
        }
    }
}

impl PrRuntime {
    pub async fn on_session_start(&self, session_id: ThreadId, codex_home: &Path, cwd: &Path) {
        tracing::debug!(%session_id, cwd = %cwd.display(), "pr_runtime.on_session_start");
        #[cfg(feature = "probe")]
        probe::record(probe::LifecycleEvent::SessionStart { session_id });

        let mut guard = self.state.lock().await;
        if guard.init.is_none() {
            if let Err(err) = guard.initialize(codex_home.to_path_buf(), cwd).await {
                tracing::warn!(error = %err, "failed to initialize printrevolt runtime");
            }
        }
        if let Some(init) = guard.init.as_ref()
            && let Some(audit) = init.audit.as_ref()
        {
            let _ = audit.write_json(&AuditEvent {
                kind: AuditEventKind::SessionStart,
                payload: serde_json::json!({ "session_id": session_id.to_string(), "cwd": cwd.display().to_string() }),
            });
        }
        if let Some(init) = guard.init.as_ref() {
            tracing::debug!(
                session_id = %session_id,
                commands = init.commands.len(),
                templates = init.templates.len(),
                "printrevolt initialized"
            );
        }
    }

    pub async fn before_task(&self, session_id: ThreadId, turn_id: &str, cwd: &Path) {
        tracing::debug!(
            %session_id,
            turn_id,
            cwd = %cwd.display(),
            "pr_runtime.before_task"
        );
        #[cfg(feature = "probe")]
        probe::record(probe::LifecycleEvent::BeforeTask {
            session_id,
            turn_id: turn_id.to_string(),
        });

        let guard = self.state.lock().await;
        if let Some(init) = guard.init.as_ref()
            && let Some(audit) = init.audit.as_ref()
        {
            let _ = audit.write_json(&AuditEvent {
                kind: AuditEventKind::BeforeTask,
                payload: serde_json::json!({ "session_id": session_id.to_string(), "turn_id": turn_id, "cwd": cwd.display().to_string() }),
            });
        }
    }

    pub async fn before_tool_call(
        &self,
        session_id: ThreadId,
        turn_id: &str,
        cwd: &Path,
        call: ToolCall,
    ) -> ToolCallDecision {
        let mut guard = self.state.lock().await;
        let Some(init) = guard.init.as_mut() else {
            return ToolCallDecision::block(
                "PrRuntimeInitFailed",
                "PrintRevolt runtime was not initialized (on_session_start did not run).",
            );
        };

        let ctx = init.ctx(session_id, Some(turn_id.to_string()), cwd);

        if let Some(audit) = init.audit.as_ref() {
            let _ = audit.write_json(&AuditEvent {
                kind: AuditEventKind::BeforeTool,
                payload: AuditBeforeTool {
                    tool_call: call.clone(),
                },
            });
        }

        // Hook decision first (may modify).
        let mut candidate = call.clone();
        if init.cfg.hooks.enabled {
            if let Some(def) = init.cfg.hooks.before_tool.clone() {
                if def.is_repo_provided && !init.is_trusted_repo() {
                    // Repo hook exists but repo untrusted: allow, but do not run headlessly.
                } else {
                    let hook = HookSpec {
                        argv: def.argv,
                        timeout_ms: def.timeout_ms,
                        headless_only: def.headless_only,
                        is_repo_provided: def.is_repo_provided,
                    };
                    let payload = HookPayloadV2 {
                        ctx: ctx.clone(),
                        event_kind: LifecycleEventKind::BeforeTool,
                        tool_call: Some(candidate.clone()),
                        tool_outcome: None,
                        vars: init.vars.clone(),
                    };
                    match init.hook_runner.run_hook(&hook, &payload).await {
                        Ok(resp) => {
                            if let Some(audit) = init.audit.as_ref() {
                                let _ = audit.write_json(&AuditEvent {
                                    kind: AuditEventKind::HookDecision,
                                    payload: AuditDecision {
                                        tool_call: candidate.clone(),
                                        decision: resp.decision.clone(),
                                    },
                                });
                            }
                            match resp.decision.kind {
                                DecisionKind::Allow => {}
                                DecisionKind::Block => return resp.decision,
                                DecisionKind::Modify => {
                                    if let Some(modified) = resp.decision.modified_call.clone() {
                                        candidate = modified;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            return ToolCallDecision::block(
                                "PrHookFailed",
                                format!("Hook failed: {err}"),
                            );
                        }
                    }
                }
            }
        }

        // Policy revalidate after hook modifications.
        let decision = init.policy.evaluate_tool_call(&candidate);
        if let Some(audit) = init.audit.as_ref() {
            let _ = audit.write_json(&AuditEvent {
                kind: AuditEventKind::PolicyDecision,
                payload: AuditDecision {
                    tool_call: candidate.clone(),
                    decision: decision.clone(),
                },
            });
        }

        if decision.kind == DecisionKind::Allow && candidate != call {
            return ToolCallDecision::modify(candidate);
        }
        decision
    }

    pub async fn after_tool_call(
        &self,
        session_id: ThreadId,
        turn_id: &str,
        cwd: &Path,
        call: ToolCall,
        outcome: ToolOutcome,
    ) {
        let mut guard = self.state.lock().await;
        let Some(init) = guard.init.as_mut() else {
            return;
        };
        let ctx = init.ctx(session_id, Some(turn_id.to_string()), cwd);

        if init.policy.should_record_verify(&call) {
            init.last_verify = Some(VerifyEvidence {
                recorded_at: ctx.triggered_at,
                command: match &call.input {
                    codex_pr_types::ToolInput::LocalShell { command, .. } => command.clone(),
                    codex_pr_types::ToolInput::Function { arguments }
                        if call.tool_name == "shell_command" =>
                    {
                        serde_json::from_str::<serde_json::Value>(arguments)
                            .ok()
                            .and_then(|v| {
                                v.get("command")
                                    .and_then(|c| c.as_str())
                                    .map(|s| vec![s.to_string()])
                            })
                            .unwrap_or_default()
                    }
                    _ => Vec::new(),
                },
                success: outcome.success,
            });
        }

        if let Some(audit) = init.audit.as_ref() {
            let _ = audit.write_json(&AuditEvent {
                kind: AuditEventKind::AfterTool,
                payload: AuditAfterTool {
                    tool_call: call,
                    outcome,
                },
            });
        }
    }

    pub async fn before_finalize(&self, session_id: ThreadId, turn_id: &str, cwd: &Path) {
        tracing::debug!(
            %session_id,
            turn_id,
            cwd = %cwd.display(),
            "pr_runtime.before_finalize"
        );
        #[cfg(feature = "probe")]
        probe::record(probe::LifecycleEvent::BeforeFinalize {
            session_id,
            turn_id: turn_id.to_string(),
        });

        let guard = self.state.lock().await;
        if let Some(init) = guard.init.as_ref()
            && let Some(audit) = init.audit.as_ref()
        {
            let _ = audit.write_json(&AuditEvent {
                kind: AuditEventKind::BeforeFinalize,
                payload: serde_json::json!({ "session_id": session_id.to_string(), "turn_id": turn_id, "cwd": cwd.display().to_string() }),
            });
        }
    }

    pub async fn finalize_gate(
        &self,
        session_id: ThreadId,
        turn_id: &str,
        cwd: &Path,
    ) -> Option<String> {
        let mut guard = self.state.lock().await;
        let Some(init) = guard.init.as_mut() else {
            return None;
        };
        let ctx = init.ctx(session_id, Some(turn_id.to_string()), cwd);
        let decision = init
            .policy
            .evaluate_finalize(ctx.triggered_at, init.last_verify.as_ref());
        let Some(decision) = decision else {
            return None;
        };
        Some(format!(
            "[{}] {}",
            decision
                .reason_code
                .unwrap_or_else(|| "PrFinalizeDenied".to_string()),
            decision
                .message
                .unwrap_or_else(|| "Finalize denied by policy.".to_string())
        ))
    }

    pub async fn on_session_end(&self, session_id: ThreadId, cwd: &Path) {
        tracing::debug!(%session_id, cwd = %cwd.display(), "pr_runtime.on_session_end");
        #[cfg(feature = "probe")]
        probe::record(probe::LifecycleEvent::SessionEnd { session_id });

        let guard = self.state.lock().await;
        if let Some(init) = guard.init.as_ref()
            && let Some(audit) = init.audit.as_ref()
        {
            let _ = audit.write_json(&AuditEvent {
                kind: AuditEventKind::SessionEnd,
                payload: serde_json::json!({ "session_id": session_id.to_string(), "cwd": cwd.display().to_string() }),
            });
        }
    }
}

#[derive(Default)]
struct PrRuntimeState {
    init: Option<PrRuntimeInit>,
}

impl PrRuntimeState {
    async fn initialize(&mut self, codex_home: PathBuf, cwd: &Path) -> Result<()> {
        let project_root = discover_repo_root(cwd);
        let resolved =
            resolve_printrevolt_config(codex_home.as_path(), project_root.as_deref(), None);
        let cfg_toml = toml::to_string(&resolved.printrevolt).unwrap_or_default();
        let cfg: PrintRevoltConfig = toml::from_str(&cfg_toml).unwrap_or_default();

        let global_commands_path = codex_home.join("printrevolt").join("commands.json");
        let global_commands = load_commands_file(&global_commands_path);
        let repo_commands_path = project_root
            .as_deref()
            .map(|r| r.join(".codex").join("printrevolt").join("commands.json"));
        let repo_commands = repo_commands_path
            .as_deref()
            .filter(|_| {
                let Some(root) = project_root.as_ref() else {
                    return false;
                };
                let root = root.to_string_lossy().to_string();
                cfg.hooks
                    .trusted_repo_roots
                    .iter()
                    .any(|trusted| trusted == &root)
            })
            .and_then(load_commands_file);
        let effective_commands =
            codex_pr_types::CommandsFileV1::merge_effective(global_commands, repo_commands)
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "failed to merge commands catalogs");
                    codex_pr_types::CommandsFileV1 {
                        schema_version: "1".to_string(),
                        commands: Vec::new(),
                    }
                });
        let commands = effective_commands
            .commands
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();

        let audit_stdout = std::env::var("PRINTREVOLT_AUDIT_STDOUT")
            .ok()
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        let audit_path = codex_home
            .join("printrevolt")
            .join("audit")
            .join("events.jsonl");

        let audit = if cfg.audit.enabled {
            Some(JsonlAuditSink::new(
                audit_path,
                true,
                audit_stdout,
                cfg.audit.max_file_bytes,
                cfg.audit.max_files,
                cfg.audit.redact_patterns.clone(),
            ))
        } else {
            None
        };

        let policy = PolicyEngine::new(cfg.policy.clone())
            .map_err(|err| anyhow::anyhow!("failed to initialize policy engine: {err}"))?;

        let vars = cfg.vars.clone();

        let repo_trusted = project_root.as_ref().is_some_and(|root| {
            let root = root.to_string_lossy().to_string();
            cfg.hooks.trusted_repo_roots.iter().any(|t| t == &root)
        });
        let templates = codex_pr_templates::discover_templates(
            codex_home.as_path(),
            project_root.as_deref(),
            repo_trusted,
            codex_pr_templates::TemplateDiscoveryConfig::default(),
        );
        let template_refs = templates
            .templates
            .iter()
            .map(codex_pr_templates::template_ref)
            .collect::<Vec<_>>();
        for w in templates.warnings {
            tracing::warn!(warning = %w, "template discovery warning");
        }

        self.init = Some(PrRuntimeInit {
            cfg,
            repo_root: project_root,
            vars,
            last_verify: None,
            commands,
            templates: template_refs,
            policy,
            hook_runner: HookRunner,
            audit,
        });
        Ok(())
    }
}

struct PrRuntimeInit {
    cfg: PrintRevoltConfig,
    repo_root: Option<PathBuf>,
    vars: BTreeMap<String, String>,
    last_verify: Option<VerifyEvidence>,
    commands: BTreeMap<String, codex_pr_types::CommandSpecV1>,
    templates: Vec<TemplateRef>,
    policy: PolicyEngine,
    hook_runner: HookRunner,
    audit: Option<JsonlAuditSink>,
}

impl PrRuntimeInit {
    fn is_trusted_repo(&self) -> bool {
        let Some(repo_root) = &self.repo_root else {
            return false;
        };
        let repo_root = repo_root.to_string_lossy().to_string();
        self.cfg
            .hooks
            .trusted_repo_roots
            .iter()
            .any(|trusted| trusted == &repo_root)
    }

    fn ctx(&self, session_id: ThreadId, turn_id: Option<String>, cwd: &Path) -> SessionContext {
        SessionContext {
            session_id,
            turn_id,
            triggered_at: chrono::Utc::now(),
            cwd: cwd.display().to_string(),
            repo_root: self.repo_root.as_ref().map(|p| p.display().to_string()),
        }
    }
}

fn discover_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

// Note: runtime state is session-scoped via `SessionServices`.

fn load_commands_file(path: &Path) -> Option<codex_pr_types::CommandsFileV1> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return None;
    };
    let parsed: codex_pr_types::CommandsFileV1 = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "failed to parse commands.json");
            return None;
        }
    };
    if parsed.schema_version != "1" {
        tracing::warn!(
            path = %path.display(),
            schema_version = %parsed.schema_version,
            "unsupported commands.json schema_version"
        );
        return None;
    }
    Some(parsed)
}
