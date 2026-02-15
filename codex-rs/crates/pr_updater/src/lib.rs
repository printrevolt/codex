use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    Npm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateCheckRequest {
    pub channel: UpdateChannel,
    pub package: String,
    pub current_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub update_available: bool,
    pub current_version: String,
    pub latest_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdatePlan {
    pub channel: UpdateChannel,
    pub package: String,
    pub current_version: String,
    pub target_version: String,
    pub action_hash: String,
    pub command: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("invalid version: {0}")]
    InvalidVersion(String),
}

pub fn compare_semver(a: &str, b: &str) -> Result<std::cmp::Ordering, UpdateError> {
    let pa = parse_semver(a)?;
    let pb = parse_semver(b)?;
    Ok(pa.cmp(&pb))
}

pub fn check_update(
    req: UpdateCheckRequest,
    latest_version: String,
) -> Result<UpdateCheckResult, UpdateError> {
    let ordering = compare_semver(req.current_version.as_str(), latest_version.as_str())?;
    Ok(UpdateCheckResult {
        update_available: ordering == std::cmp::Ordering::Less,
        current_version: req.current_version,
        latest_version,
    })
}

pub fn plan_update_npm(
    package: &str,
    current_version: &str,
    target_version: &str,
) -> Result<UpdatePlan, UpdateError> {
    let _ = parse_semver(current_version)?;
    let _ = parse_semver(target_version)?;
    let cmd = vec![
        "npm".to_string(),
        "install".to_string(),
        "-g".to_string(),
        format!("{package}@{target_version}"),
    ];
    let action_hash = sha256_json(&serde_json::json!({
        "channel": "npm",
        "package": package,
        "current_version": current_version,
        "target_version": target_version,
        "command": cmd,
    }));
    Ok(UpdatePlan {
        channel: UpdateChannel::Npm,
        package: package.to_string(),
        current_version: current_version.to_string(),
        target_version: target_version.to_string(),
        action_hash,
        command: vec![
            "npm".to_string(),
            "install".to_string(),
            "-g".to_string(),
            format!("{package}@{target_version}"),
        ],
        notes: vec![
            "This plan installs the target version globally via npm.".to_string(),
            "If npm is unavailable, run the equivalent command in your environment manager."
                .to_string(),
        ],
    })
}

fn parse_semver(s: &str) -> Result<(u64, u64, u64), UpdateError> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split_once('-').map(|(c, _)| c).unwrap_or(s);
    let mut it = core.split('.');
    let major = it
        .next()
        .ok_or_else(|| UpdateError::InvalidVersion(s.to_string()))?;
    let minor = it
        .next()
        .ok_or_else(|| UpdateError::InvalidVersion(s.to_string()))?;
    let patch = it
        .next()
        .ok_or_else(|| UpdateError::InvalidVersion(s.to_string()))?;
    if it.next().is_some() {
        return Err(UpdateError::InvalidVersion(s.to_string()));
    }
    Ok((
        major
            .parse()
            .map_err(|_| UpdateError::InvalidVersion(s.to_string()))?,
        minor
            .parse()
            .map_err(|_| UpdateError::InvalidVersion(s.to_string()))?,
        patch
            .parse()
            .map_err(|_| UpdateError::InvalidVersion(s.to_string()))?,
    ))
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

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn semver_compare_works() {
        assert_eq!(
            compare_semver("0.101.0", "0.101.0").unwrap(),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_semver("0.101.0", "0.102.0").unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_semver("1.0.0", "0.999.0").unwrap(),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn update_plan_hash_stable() {
        let plan1 = plan_update_npm("codex", "0.1.0", "0.2.0").unwrap();
        let plan2 = plan_update_npm("codex", "0.1.0", "0.2.0").unwrap();
        assert_eq!(plan1.action_hash, plan2.action_hash);
        assert_eq!(plan1.command, plan2.command);
    }
}
