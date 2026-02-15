use std::time::Duration;

use codex_pr_types::HookPayloadV2;
use codex_pr_types::HookResponseV2;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct HookSpec {
    pub argv: Vec<String>,
    pub timeout_ms: u64,
    pub headless_only: bool,
    pub is_repo_provided: bool,
}

#[derive(Debug, Error)]
pub enum HookRunError {
    #[error("hook argv is empty")]
    EmptyArgv,
    #[error("hook process spawn failed: {0}")]
    SpawnFailed(String),
    #[error("hook timed out after {0}ms")]
    TimedOut(u64),
    #[error("hook exited non-zero: {0}")]
    NonZeroExit(String),
    #[error("hook invalid json response: {0}")]
    InvalidJson(String),
}

#[derive(Clone)]
pub struct HookRunner;

impl HookRunner {
    pub async fn run_hook(
        &self,
        hook: &HookSpec,
        payload: &HookPayloadV2,
    ) -> Result<HookResponseV2, HookRunError> {
        let (program, args) = hook.argv.split_first().ok_or(HookRunError::EmptyArgv)?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| HookRunError::SpawnFailed(err.to_string()))?;

        let input = serde_json::to_vec(payload)
            .map_err(|err| HookRunError::InvalidJson(err.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&input)
                .await
                .map_err(|err| HookRunError::SpawnFailed(err.to_string()))?;
        }

        let timeout = Duration::from_millis(hook.timeout_ms);
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(res) => res.map_err(|err| HookRunError::SpawnFailed(err.to_string()))?,
            Err(_) => return Err(HookRunError::TimedOut(hook.timeout_ms)),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(HookRunError::NonZeroExit(stderr.to_string()));
        }

        let mut stdout = output.stdout;
        let mut buf = Vec::new();
        buf.append(&mut stdout);
        let response: HookResponseV2 = serde_json::from_slice(&buf)
            .map_err(|err| HookRunError::InvalidJson(err.to_string()))?;
        Ok(response)
    }
}
