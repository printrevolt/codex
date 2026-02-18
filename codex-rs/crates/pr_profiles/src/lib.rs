use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use codex_pr_types::GuidelineProfileV1;
use codex_pr_types::GuidelineProfilesFileV1;
use codex_pr_types::PolicyConfig;
use codex_pr_types::PolicyProfilePatchV1;
use codex_pr_types::PolicyProfileV1;
use codex_pr_types::PolicyProfilesFileV1;
use codex_pr_types::ProfileRefs;
use lru::LruCache;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;

const SCHEMA_VERSION: &str = "1";

#[derive(Debug, Error)]
pub enum ProfilesError {
    #[error("io error for {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("json parse error for {path}: {message}")]
    JsonParse { path: PathBuf, message: String },
    #[error("validation error: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileScope {
    Global,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileProvenanceEntry {
    pub profile_type: String,
    pub profile_id: String,
    pub scope: ProfileScope,
}

#[derive(Debug, Clone)]
pub struct LoadedProfilesRegistry {
    pub policy_profiles: BTreeMap<String, PolicyProfileV1>,
    pub guideline_profiles: BTreeMap<String, GuidelineProfileV1>,
    pub policy_scope: BTreeMap<String, ProfileScope>,
    pub guideline_scope: BTreeMap<String, ProfileScope>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedSubjectProfiles {
    pub effective_policy: PolicyConfig,
    pub guidelines: Vec<String>,
    pub provenance: Vec<ProfileProvenanceEntry>,
    pub warnings: Vec<String>,
    pub missing_policy_profiles: Vec<String>,
    pub missing_guideline_profiles: Vec<String>,
    pub fingerprint_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolverPerfResult {
    pub iterations: usize,
    pub p50_micros: u64,
    pub p95_micros: u64,
    pub p99_micros: u64,
    pub max_micros: u64,
}

pub struct ProfilesResolverCache {
    inner: Mutex<LruCache<String, ResolvedSubjectProfiles>>,
}

impl ProfilesResolverCache {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            inner: Mutex::new(LruCache::new(
                NonZeroUsize::new(cap).expect("nonzero cache size"),
            )),
        }
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.clear();
        }
    }
}

impl Default for ProfilesResolverCache {
    fn default() -> Self {
        Self::new(512)
    }
}

pub fn global_policy_profiles_path(codex_home: &Path) -> PathBuf {
    codex_home.join("printrevolt").join("policy_profiles.json")
}

pub fn project_policy_profiles_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".codex")
        .join("printrevolt")
        .join("policy_profiles.json")
}

pub fn global_guideline_profiles_path(codex_home: &Path) -> PathBuf {
    codex_home
        .join("printrevolt")
        .join("guideline_profiles.json")
}

pub fn project_guideline_profiles_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".codex")
        .join("printrevolt")
        .join("guideline_profiles.json")
}

pub fn load_profiles_registry(
    codex_home: &Path,
    project_root: Option<&Path>,
    project_trusted: bool,
) -> Result<LoadedProfilesRegistry, ProfilesError> {
    let global_policy =
        read_policy_profiles_file(global_policy_profiles_path(codex_home).as_path())?;
    let global_guideline =
        read_guideline_profiles_file(global_guideline_profiles_path(codex_home).as_path())?;

    let mut policy_profiles = global_policy.profiles;
    let mut guideline_profiles = global_guideline.profiles;
    let mut policy_scope = policy_profiles
        .keys()
        .cloned()
        .map(|k| (k, ProfileScope::Global))
        .collect::<BTreeMap<_, _>>();
    let mut guideline_scope = guideline_profiles
        .keys()
        .cloned()
        .map(|k| (k, ProfileScope::Global))
        .collect::<BTreeMap<_, _>>();
    let mut warnings = Vec::<String>::new();

    if let Some(project_root) = project_root {
        if !project_trusted {
            warnings.push(format!(
                "project profiles ignored (repo not trusted): {}",
                project_root.display()
            ));
        } else {
            let project_policy =
                read_policy_profiles_file(project_policy_profiles_path(project_root).as_path())?;
            let project_guideline = read_guideline_profiles_file(
                project_guideline_profiles_path(project_root).as_path(),
            )?;

            for (id, profile) in project_policy.profiles {
                policy_profiles.insert(id.clone(), profile);
                policy_scope.insert(id, ProfileScope::Project);
            }
            for (id, profile) in project_guideline.profiles {
                guideline_profiles.insert(id.clone(), profile);
                guideline_scope.insert(id, ProfileScope::Project);
            }
        }
    }

    validate_profile_graph("policy_profile", &policy_profiles, |p| {
        p.includes.as_slice()
    })?;
    validate_profile_graph("guideline_profile", &guideline_profiles, |p| {
        p.includes.as_slice()
    })?;

    Ok(LoadedProfilesRegistry {
        policy_profiles,
        guideline_profiles,
        policy_scope,
        guideline_scope,
        warnings,
    })
}

pub fn resolve_subject_profiles(
    registry: &LoadedProfilesRegistry,
    refs: &ProfileRefs,
    base_policy: &PolicyConfig,
    policy_floor: &PolicyConfig,
) -> ResolvedSubjectProfiles {
    let mut warnings = registry.warnings.clone();
    let mut missing_policy_profiles = Vec::<String>::new();
    let mut missing_guideline_profiles = Vec::<String>::new();

    let resolved_policy_ids = resolve_requested_ids(
        refs.policy_profiles.as_slice(),
        &registry.policy_profiles,
        |p| p.includes.as_slice(),
        &mut missing_policy_profiles,
    );
    let resolved_guideline_ids = resolve_requested_ids(
        refs.guideline_profiles.as_slice(),
        &registry.guideline_profiles,
        |p| p.includes.as_slice(),
        &mut missing_guideline_profiles,
    );

    for id in &missing_policy_profiles {
        warnings.push(format!("missing policy profile reference: {id}"));
    }
    for id in &missing_guideline_profiles {
        warnings.push(format!("missing guideline profile reference: {id}"));
    }

    let mut effective_policy = base_policy.clone();
    let mut provenance = Vec::<ProfileProvenanceEntry>::new();

    for id in &resolved_policy_ids {
        let Some(profile) = registry.policy_profiles.get(id) else {
            continue;
        };
        apply_policy_patch(
            &mut effective_policy,
            policy_floor,
            id.as_str(),
            &profile.patch,
            &mut warnings,
        );
        provenance.push(ProfileProvenanceEntry {
            profile_type: "policy_profile".to_string(),
            profile_id: id.clone(),
            scope: *registry
                .policy_scope
                .get(id)
                .unwrap_or(&ProfileScope::Global),
        });
    }

    // Final floor clamp guarantees monotonic safety even if base policy is weaker.
    clamp_to_floor(
        &mut effective_policy,
        policy_floor,
        "<resolved>",
        &mut warnings,
    );

    let mut guidelines = Vec::<String>::new();
    for id in &resolved_guideline_ids {
        let Some(profile) = registry.guideline_profiles.get(id) else {
            continue;
        };
        for instruction in &profile.instructions {
            if !instruction.trim().is_empty() {
                guidelines.push(instruction.clone());
            }
        }
        provenance.push(ProfileProvenanceEntry {
            profile_type: "guideline_profile".to_string(),
            profile_id: id.clone(),
            scope: *registry
                .guideline_scope
                .get(id)
                .unwrap_or(&ProfileScope::Global),
        });
    }

    let fingerprint_sha256 = hash_resolution(
        &effective_policy,
        guidelines.as_slice(),
        provenance.as_slice(),
        warnings.as_slice(),
        refs,
    );

    ResolvedSubjectProfiles {
        effective_policy,
        guidelines,
        provenance,
        warnings,
        missing_policy_profiles,
        missing_guideline_profiles,
        fingerprint_sha256,
    }
}

pub fn registry_fingerprint_sha256(registry: &LoadedProfilesRegistry) -> String {
    let payload = serde_json::json!({
        "policy_profiles": registry.policy_profiles,
        "guideline_profiles": registry.guideline_profiles,
        "policy_scope": registry.policy_scope,
        "guideline_scope": registry.guideline_scope,
        "warnings": registry.warnings,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_lower_hex(hasher.finalize().as_ref())
}

pub fn resolve_subject_profiles_cached(
    cache: &ProfilesResolverCache,
    registry: &LoadedProfilesRegistry,
    refs: &ProfileRefs,
    base_policy: &PolicyConfig,
    policy_floor: &PolicyConfig,
) -> ResolvedSubjectProfiles {
    let key_payload = serde_json::json!({
        "registry_fingerprint": registry_fingerprint_sha256(registry),
        "refs": refs,
        "base_policy": base_policy,
        "policy_floor": policy_floor,
    });
    let key_bytes = serde_json::to_vec(&key_payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(key_bytes);
    let key = to_lower_hex(hasher.finalize().as_ref());

    if let Ok(mut guard) = cache.inner.lock()
        && let Some(hit) = guard.get(&key)
    {
        return hit.clone();
    }

    let resolved = resolve_subject_profiles(registry, refs, base_policy, policy_floor);
    if let Ok(mut guard) = cache.inner.lock() {
        guard.put(key, resolved.clone());
    }
    resolved
}

pub fn benchmark_resolver_warm_path(
    cache: &ProfilesResolverCache,
    registry: &LoadedProfilesRegistry,
    refs: &ProfileRefs,
    base_policy: &PolicyConfig,
    policy_floor: &PolicyConfig,
    iterations: usize,
) -> ResolverPerfResult {
    let mut samples = Vec::<Duration>::with_capacity(iterations.max(1));
    let run_count = iterations.max(1);

    // Prime cache once so measured loop is warm-path.
    let _ = resolve_subject_profiles_cached(cache, registry, refs, base_policy, policy_floor);
    for _ in 0..run_count {
        let start = Instant::now();
        let _ = resolve_subject_profiles_cached(cache, registry, refs, base_policy, policy_floor);
        samples.push(start.elapsed());
    }
    samples.sort();

    let to_micros = |idx: usize| -> u64 {
        samples
            .get(idx.min(samples.len().saturating_sub(1)))
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    };
    let p50_idx = ((run_count - 1) * 50) / 100;
    let p95_idx = ((run_count - 1) * 95) / 100;
    let p99_idx = ((run_count - 1) * 99) / 100;

    ResolverPerfResult {
        iterations: run_count,
        p50_micros: to_micros(p50_idx),
        p95_micros: to_micros(p95_idx),
        p99_micros: to_micros(p99_idx),
        max_micros: to_micros(run_count - 1),
    }
}

fn read_policy_profiles_file(path: &Path) -> Result<PolicyProfilesFileV1, ProfilesError> {
    if !path.exists() {
        return Ok(PolicyProfilesFileV1::default());
    }
    let raw = std::fs::read_to_string(path).map_err(|err| ProfilesError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    let file = serde_json::from_str::<PolicyProfilesFileV1>(&raw).map_err(|err| {
        ProfilesError::JsonParse {
            path: path.to_path_buf(),
            message: err.to_string(),
        }
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(ProfilesError::Validation(format!(
            "unsupported policy_profiles schema_version={} (expected {})",
            file.schema_version, SCHEMA_VERSION
        )));
    }
    Ok(file)
}

fn read_guideline_profiles_file(path: &Path) -> Result<GuidelineProfilesFileV1, ProfilesError> {
    if !path.exists() {
        return Ok(GuidelineProfilesFileV1::default());
    }
    let raw = std::fs::read_to_string(path).map_err(|err| ProfilesError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    let file = serde_json::from_str::<GuidelineProfilesFileV1>(&raw).map_err(|err| {
        ProfilesError::JsonParse {
            path: path.to_path_buf(),
            message: err.to_string(),
        }
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(ProfilesError::Validation(format!(
            "unsupported guideline_profiles schema_version={} (expected {})",
            file.schema_version, SCHEMA_VERSION
        )));
    }
    Ok(file)
}

fn validate_profile_graph<T>(
    kind: &str,
    profiles: &BTreeMap<String, T>,
    includes: fn(&T) -> &[String],
) -> Result<(), ProfilesError> {
    for id in profiles.keys() {
        if id.trim().is_empty() {
            return Err(ProfilesError::Validation(format!(
                "{} id cannot be empty",
                kind
            )));
        }
    }

    let mut state = BTreeMap::<String, VisitState>::new();
    for id in profiles.keys() {
        if !matches!(state.get(id), Some(VisitState::Done)) {
            let mut stack = Vec::<String>::new();
            validate_profile_graph_dfs(
                kind,
                id.as_str(),
                profiles,
                includes,
                &mut state,
                &mut stack,
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Done,
}

fn validate_profile_graph_dfs<T>(
    kind: &str,
    id: &str,
    profiles: &BTreeMap<String, T>,
    includes: fn(&T) -> &[String],
    state: &mut BTreeMap<String, VisitState>,
    stack: &mut Vec<String>,
) -> Result<(), ProfilesError> {
    match state.get(id) {
        Some(VisitState::Done) => return Ok(()),
        Some(VisitState::Visiting) => {
            stack.push(id.to_string());
            return Err(ProfilesError::Validation(format!(
                "{} include cycle detected: {}",
                kind,
                stack.join(" -> ")
            )));
        }
        None => {}
    }

    let Some(node) = profiles.get(id) else {
        return Err(ProfilesError::Validation(format!(
            "{} include references missing id: {}",
            kind, id
        )));
    };

    state.insert(id.to_string(), VisitState::Visiting);
    stack.push(id.to_string());

    for child in includes(node) {
        if !profiles.contains_key(child) {
            return Err(ProfilesError::Validation(format!(
                "{} {} includes missing {}",
                kind, id, child
            )));
        }
        validate_profile_graph_dfs(kind, child.as_str(), profiles, includes, state, stack)?;
    }

    stack.pop();
    state.insert(id.to_string(), VisitState::Done);
    Ok(())
}

fn resolve_requested_ids<T>(
    requested: &[String],
    profiles: &BTreeMap<String, T>,
    includes: fn(&T) -> &[String],
    missing_out: &mut Vec<String>,
) -> Vec<String> {
    let mut ordered = Vec::<String>::new();
    let mut seen = BTreeSet::<String>::new();
    for id in requested {
        resolve_requested_ids_dfs(
            id.as_str(),
            profiles,
            includes,
            &mut seen,
            &mut ordered,
            missing_out,
        );
    }
    ordered
}

fn resolve_requested_ids_dfs<T>(
    id: &str,
    profiles: &BTreeMap<String, T>,
    includes: fn(&T) -> &[String],
    seen: &mut BTreeSet<String>,
    ordered: &mut Vec<String>,
    missing_out: &mut Vec<String>,
) {
    let Some(profile) = profiles.get(id) else {
        missing_out.push(id.to_string());
        return;
    };
    if seen.contains(id) {
        return;
    }
    for child in includes(profile) {
        resolve_requested_ids_dfs(
            child.as_str(),
            profiles,
            includes,
            seen,
            ordered,
            missing_out,
        );
    }
    if seen.insert(id.to_string()) {
        ordered.push(id.to_string());
    }
}

fn apply_policy_patch(
    cfg: &mut PolicyConfig,
    floor: &PolicyConfig,
    profile_id: &str,
    patch: &PolicyProfilePatchV1,
    warnings: &mut Vec<String>,
) {
    if let Some(enabled) = patch.enabled {
        if enabled {
            cfg.enabled = true;
        } else if cfg.enabled || floor.enabled {
            warnings.push(format!(
                "policy_profile {} attempted to relax policy.enabled=false; ignored",
                profile_id
            ));
        }
    }

    if let Some(deny) = patch.deny_dangerous_always {
        if deny {
            cfg.deny_dangerous_always = true;
        } else if cfg.deny_dangerous_always || floor.deny_dangerous_always {
            warnings.push(format!(
                "policy_profile {} attempted to relax policy.deny_dangerous_always=false; ignored",
                profile_id
            ));
        }
    }

    if let Some(required) = patch.verify_required {
        if required {
            cfg.verify.required = true;
        } else if cfg.verify.required || floor.verify.required {
            warnings.push(format!(
                "policy_profile {} attempted to relax policy.verify.required=false; ignored",
                profile_id
            ));
        }
    }

    if let Some(max_age_ms) = patch.verify_max_age_ms {
        if max_age_ms < cfg.verify.max_age_ms {
            cfg.verify.max_age_ms = max_age_ms;
        } else if max_age_ms > cfg.verify.max_age_ms {
            warnings.push(format!(
                "policy_profile {} attempted to relax policy.verify.max_age_ms to {}; ignored",
                profile_id, max_age_ms
            ));
        }
    }

    for prefix in &patch.verify_command_prefixes_add {
        if !cfg.verify.command_prefixes.contains(prefix) {
            cfg.verify.command_prefixes.push(prefix.clone());
        }
    }

    clamp_to_floor(cfg, floor, profile_id, warnings);
}

fn clamp_to_floor(
    cfg: &mut PolicyConfig,
    floor: &PolicyConfig,
    profile_id: &str,
    warnings: &mut Vec<String>,
) {
    if floor.enabled && !cfg.enabled {
        cfg.enabled = true;
        warnings.push(format!(
            "policy floor clamp applied after {}: policy.enabled=true",
            profile_id
        ));
    }
    if floor.deny_dangerous_always && !cfg.deny_dangerous_always {
        cfg.deny_dangerous_always = true;
        warnings.push(format!(
            "policy floor clamp applied after {}: policy.deny_dangerous_always=true",
            profile_id
        ));
    }
    if floor.verify.required && !cfg.verify.required {
        cfg.verify.required = true;
        warnings.push(format!(
            "policy floor clamp applied after {}: policy.verify.required=true",
            profile_id
        ));
    }
    if cfg.verify.max_age_ms > floor.verify.max_age_ms {
        cfg.verify.max_age_ms = floor.verify.max_age_ms;
        warnings.push(format!(
            "policy floor clamp applied after {}: policy.verify.max_age_ms={}",
            profile_id, cfg.verify.max_age_ms
        ));
    }

    for floor_prefix in &floor.verify.command_prefixes {
        if !cfg.verify.command_prefixes.contains(floor_prefix) {
            cfg.verify.command_prefixes.push(floor_prefix.clone());
        }
    }
}

fn hash_resolution(
    effective_policy: &PolicyConfig,
    guidelines: &[String],
    provenance: &[ProfileProvenanceEntry],
    warnings: &[String],
    refs: &ProfileRefs,
) -> String {
    let payload = serde_json::json!({
        "effective_policy": effective_policy,
        "guidelines": guidelines,
        "provenance": provenance,
        "warnings": warnings,
        "requested_refs": refs,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_lower_hex(hasher.finalize().as_ref())
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
    fn detects_include_cycle() {
        let mut map = BTreeMap::<String, PolicyProfileV1>::new();
        map.insert(
            "a".to_string(),
            PolicyProfileV1 {
                includes: vec!["b".to_string()],
                ..Default::default()
            },
        );
        map.insert(
            "b".to_string(),
            PolicyProfileV1 {
                includes: vec!["a".to_string()],
                ..Default::default()
            },
        );
        let err = validate_profile_graph("policy_profile", &map, |p| p.includes.as_slice())
            .expect_err("must fail");
        assert!(format!("{err}").contains("cycle"));
    }

    #[test]
    fn monotonic_merge_rejects_relax_attempts() {
        let floor = PolicyConfig {
            enabled: true,
            deny_dangerous_always: true,
            verify: codex_pr_types::VerifyPolicy {
                required: true,
                max_age_ms: 60_000,
                command_prefixes: vec![vec!["npm".to_string(), "test".to_string()]],
            },
        };
        let base = floor.clone();
        let mut registry = LoadedProfilesRegistry {
            policy_profiles: BTreeMap::new(),
            guideline_profiles: BTreeMap::new(),
            policy_scope: BTreeMap::new(),
            guideline_scope: BTreeMap::new(),
            warnings: Vec::new(),
        };
        registry.policy_profiles.insert(
            "unsafe_relax".to_string(),
            PolicyProfileV1 {
                patch: PolicyProfilePatchV1 {
                    enabled: Some(false),
                    deny_dangerous_always: Some(false),
                    verify_required: Some(false),
                    verify_max_age_ms: Some(999_999),
                    verify_command_prefixes_add: Vec::new(),
                },
                ..Default::default()
            },
        );
        registry
            .policy_scope
            .insert("unsafe_relax".to_string(), ProfileScope::Global);

        let result = resolve_subject_profiles(
            &registry,
            &ProfileRefs {
                policy_profiles: vec!["unsafe_relax".to_string()],
                guideline_profiles: Vec::new(),
            },
            &base,
            &floor,
        );

        assert_eq!(result.effective_policy, floor);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("attempted to relax"))
        );
    }

    #[test]
    fn deterministic_resolution_order_and_fingerprint() {
        let mut registry = LoadedProfilesRegistry {
            policy_profiles: BTreeMap::new(),
            guideline_profiles: BTreeMap::new(),
            policy_scope: BTreeMap::new(),
            guideline_scope: BTreeMap::new(),
            warnings: Vec::new(),
        };
        registry.policy_profiles.insert(
            "base".to_string(),
            PolicyProfileV1 {
                patch: PolicyProfilePatchV1 {
                    verify_max_age_ms: Some(10_000),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        registry.policy_profiles.insert(
            "child".to_string(),
            PolicyProfileV1 {
                includes: vec!["base".to_string()],
                patch: PolicyProfilePatchV1 {
                    verify_required: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        registry
            .policy_scope
            .insert("base".to_string(), ProfileScope::Global);
        registry
            .policy_scope
            .insert("child".to_string(), ProfileScope::Global);

        let base = PolicyConfig::default();
        let floor = PolicyConfig::default();
        let refs = ProfileRefs {
            policy_profiles: vec!["child".to_string()],
            guideline_profiles: Vec::new(),
        };

        let r1 = resolve_subject_profiles(&registry, &refs, &base, &floor);
        let r2 = resolve_subject_profiles(&registry, &refs, &base, &floor);

        assert_eq!(r1.effective_policy.verify.required, true);
        assert_eq!(r1.effective_policy.verify.max_age_ms, 10_000);
        assert_eq!(r1.fingerprint_sha256, r2.fingerprint_sha256);
        assert_eq!(r1.provenance.len(), 2);
        assert_eq!(r1.provenance[0].profile_id, "base");
        assert_eq!(r1.provenance[1].profile_id, "child");
    }

    #[test]
    fn cache_and_perf_harness_work() {
        let mut registry = LoadedProfilesRegistry {
            policy_profiles: BTreeMap::new(),
            guideline_profiles: BTreeMap::new(),
            policy_scope: BTreeMap::new(),
            guideline_scope: BTreeMap::new(),
            warnings: Vec::new(),
        };
        registry.policy_profiles.insert(
            "strict".to_string(),
            PolicyProfileV1 {
                patch: PolicyProfilePatchV1 {
                    verify_required: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        registry
            .policy_scope
            .insert("strict".to_string(), ProfileScope::Global);

        let cache = ProfilesResolverCache::new(32);
        let refs = ProfileRefs {
            policy_profiles: vec!["strict".to_string()],
            guideline_profiles: Vec::new(),
        };
        let base = PolicyConfig::default();
        let floor = PolicyConfig::default();
        let r1 = resolve_subject_profiles_cached(&cache, &registry, &refs, &base, &floor);
        let r2 = resolve_subject_profiles_cached(&cache, &registry, &refs, &base, &floor);
        assert_eq!(r1.fingerprint_sha256, r2.fingerprint_sha256);

        let perf = benchmark_resolver_warm_path(&cache, &registry, &refs, &base, &floor, 64);
        assert_eq!(perf.iterations, 64);
        assert!(perf.p95_micros <= perf.max_micros);
    }
}
