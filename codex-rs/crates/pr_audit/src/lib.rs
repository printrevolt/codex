use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use codex_pr_types::ToolCall;
use codex_pr_types::ToolCallDecision;
use codex_pr_types::ToolOutcome;
use regex_lite::Regex;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    SessionStart,
    SessionEnd,
    BeforeTask,
    BeforeTool,
    AfterTool,
    BeforeFinalize,
    PolicyDecision,
    HookDecision,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent<T: Serialize> {
    pub kind: AuditEventKind,
    pub payload: T,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditBeforeTool {
    pub tool_call: ToolCall,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditAfterTool {
    pub tool_call: ToolCall,
    pub outcome: ToolOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditDecision {
    pub tool_call: ToolCall,
    pub decision: ToolCallDecision,
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("audit disabled")]
    Disabled,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub trait AuditSink: Send + Sync {
    fn write_json<T: Serialize>(&self, event: &AuditEvent<T>) -> Result<(), AuditError>;
}

#[derive(Clone)]
pub struct JsonlAuditSink {
    inner: Arc<Mutex<JsonlAuditSinkInner>>,
}

struct JsonlAuditSinkInner {
    enabled: bool,
    stdout: bool,
    redact: Redactor,
    target_path: PathBuf,
    max_file_bytes: u64,
    max_files: usize,
}

impl JsonlAuditSink {
    pub fn new(
        target_path: PathBuf,
        enabled: bool,
        stdout: bool,
        max_file_bytes: u64,
        max_files: usize,
        redact_patterns: Vec<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(JsonlAuditSinkInner {
                enabled,
                stdout,
                redact: Redactor::new(redact_patterns),
                target_path,
                max_file_bytes,
                max_files,
            })),
        }
    }

    fn rotate_if_needed(inner: &JsonlAuditSinkInner) -> Result<(), std::io::Error> {
        if inner.max_files == 0 {
            return Ok(());
        }
        let Some(parent) = inner.target_path.parent() else {
            return Ok(());
        };
        std::fs::create_dir_all(parent)?;
        let size = std::fs::metadata(&inner.target_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if size < inner.max_file_bytes {
            return Ok(());
        }

        // Rotate: events.jsonl -> events.jsonl.1 -> ... -> events.jsonl.(max_files-1)
        for idx in (1..inner.max_files).rev() {
            let older = rotated_path(&inner.target_path, idx - 1);
            let newer = rotated_path(&inner.target_path, idx);
            if older.exists() {
                let _ = std::fs::rename(&older, &newer);
            }
        }
        if inner.target_path.exists() {
            let _ = std::fs::rename(&inner.target_path, rotated_path(&inner.target_path, 1));
        }
        Ok(())
    }
}

fn rotated_path(base: &Path, idx: usize) -> PathBuf {
    if idx == 0 {
        return base.to_path_buf();
    }
    let mut p = base.to_path_buf();
    let file_name = base
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("events.jsonl");
    p.set_file_name(format!("{file_name}.{idx}"));
    p
}

impl AuditSink for JsonlAuditSink {
    fn write_json<T: Serialize>(&self, event: &AuditEvent<T>) -> Result<(), AuditError> {
        let guard = self.inner.lock().expect("lock audit sink");
        if !guard.enabled {
            return Err(AuditError::Disabled);
        }

        Self::rotate_if_needed(&guard)?;
        let Some(parent) = guard.target_path.parent() else {
            return Ok(());
        };
        std::fs::create_dir_all(parent)?;

        let json = serde_json::to_string(event)?;
        let redacted = guard.redact.redact_string(&json);

        {
            let file = File::options()
                .create(true)
                .append(true)
                .open(&guard.target_path)?;
            let mut w = BufWriter::new(file);
            w.write_all(redacted.as_bytes())?;
            w.write_all(b"\n")?;
            w.flush()?;
        }

        if guard.stdout {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(redacted.as_bytes());
            let _ = out.write_all(b"\n");
        }

        Ok(())
    }
}

#[derive(Clone)]
struct Redactor {
    patterns: Vec<Regex>,
}

impl Redactor {
    fn new(patterns: Vec<String>) -> Self {
        let mut compiled = Vec::new();
        for p in patterns {
            if let Ok(re) = Regex::new(&p) {
                compiled.push(re);
            }
        }
        Self { patterns: compiled }
    }

    fn redact_string(&self, input: &str) -> String {
        let mut out = input.to_string();
        for re in &self.patterns {
            out = re.replace_all(&out, "[REDACTED]").to_string();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn redacts_secrets_in_json() {
        let dir = tempdir().unwrap();
        let sink = JsonlAuditSink::new(
            dir.path().join("events.jsonl"),
            true,
            false,
            1024 * 1024,
            3,
            vec![r"(?i)token\s*=\s*\S+".to_string()],
        );
        let ev = AuditEvent {
            kind: AuditEventKind::SessionStart,
            payload: serde_json::json!({ "token": "token = abc123" }),
        };
        sink.write_json(&ev).unwrap();
        let content = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(content.contains("[REDACTED]"));
    }

    #[test]
    fn rotates_when_max_bytes_exceeded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let sink = JsonlAuditSink::new(path.clone(), true, false, 120, 3, vec![]);

        for _ in 0..10 {
            let ev = AuditEvent {
                kind: AuditEventKind::SessionStart,
                payload: serde_json::json!({ "msg": "x".repeat(64) }),
            };
            let _ = sink.write_json(&ev);
        }

        assert!(path.exists());
        assert!(rotated_path(&path, 1).exists());
        assert!(rotated_path(&path, 2).exists() || rotated_path(&path, 1).exists());

        let current = std::fs::read_to_string(&path).unwrap();
        assert!(!current.is_empty());
        let rotated = std::fs::read_to_string(rotated_path(&path, 1)).unwrap();
        assert!(!rotated.is_empty());
    }

    #[test]
    fn rotated_path_stable() {
        let base = PathBuf::from("events.jsonl");
        assert_eq!(rotated_path(&base, 0), base);
        assert_eq!(rotated_path(&base, 1), PathBuf::from("events.jsonl.1"));
    }
}
