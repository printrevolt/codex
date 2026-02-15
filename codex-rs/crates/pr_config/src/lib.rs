use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use fs2::FileExt;
use serde::Serialize;
use thiserror::Error;
use toml::Value as TomlValue;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    Defaults,
    CliOverrides,
    SessionOverlay { file: PathBuf },
    ProjectModeA { file: PathBuf },
    ProjectModeB { file: PathBuf },
    UserModeA { file: PathBuf },
    UserModeB { file: PathBuf },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadStatus {
    Missing,
    ReadOk,
    ReadError,
    ParseError,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceReport {
    pub source: ConfigSource,
    pub status: ReadStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedPrintRevoltConfig {
    pub printrevolt: TomlValue,
    pub trace: BTreeMap<String, ConfigSource>,
    pub sources: Vec<SourceReport>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Error)]
pub enum PrConfigError {
    #[error("failed to read {path}: {message}")]
    ReadFailed { path: PathBuf, message: String },

    #[error("failed to parse toml {path}: {message}")]
    ParseFailed { path: PathBuf, message: String },

    #[error("failed to write {path}: {message}")]
    WriteFailed { path: PathBuf, message: String },
}

pub struct PrintRevoltPaths {
    pub user_config_toml: PathBuf,
    pub user_printrevolt_toml: PathBuf,
    pub project_config_toml: Option<PathBuf>,
    pub project_printrevolt_toml: Option<PathBuf>,
}

pub fn compute_paths(codex_home: &Path, project_root: Option<&Path>) -> PrintRevoltPaths {
    let user_config_toml = codex_home.join("config.toml");
    let user_printrevolt_toml = codex_home.join("printrevolt.toml");

    let (project_config_toml, project_printrevolt_toml) = match project_root {
        Some(root) => {
            let dot_codex = root.join(".codex");
            (
                Some(dot_codex.join("config.toml")),
                Some(dot_codex.join("printrevolt.toml")),
            )
        }
        None => (None, None),
    };

    PrintRevoltPaths {
        user_config_toml,
        user_printrevolt_toml,
        project_config_toml,
        project_printrevolt_toml,
    }
}

pub fn resolve_printrevolt_config(
    codex_home: &Path,
    project_root: Option<&Path>,
    cli_overrides: Option<TomlValue>,
) -> ResolvedPrintRevoltConfig {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();

    let paths = compute_paths(codex_home, project_root);

    let defaults = default_printrevolt_table();
    let mut merged = defaults.clone();
    let mut trace = init_trace_from_defaults(&defaults, ConfigSource::Defaults);

    // user Mode A then user Mode B (Mode B wins per-field).
    merged = merge_layer(
        merged,
        read_mode_a_printrevolt_table(
            &paths.user_config_toml,
            |file| ConfigSource::UserModeA { file },
            &mut sources,
            &mut warnings,
        ),
        &mut trace,
    );
    merged = merge_layer(
        merged,
        read_mode_b_printrevolt_value(
            &paths.user_printrevolt_toml,
            |file| ConfigSource::UserModeB { file },
            &mut sources,
            &mut warnings,
        ),
        &mut trace,
    );

    // project Mode A then project Mode B (Mode B wins per-field).
    if let Some(project_mode_a) = paths.project_config_toml.as_deref() {
        merged = merge_layer(
            merged,
            read_mode_a_printrevolt_table(
                project_mode_a,
                |file| ConfigSource::ProjectModeA { file },
                &mut sources,
                &mut warnings,
            ),
            &mut trace,
        );
    }
    if let Some(project_mode_b) = paths.project_printrevolt_toml.as_deref() {
        merged = merge_layer(
            merged,
            read_mode_b_printrevolt_value(
                project_mode_b,
                |file| ConfigSource::ProjectModeB { file },
                &mut sources,
                &mut warnings,
            ),
            &mut trace,
        );
    }

    // Session overlay (Mode B); higher precedence than project/user, lower than CLI overrides.
    if let Ok(path) = std::env::var("PRINTREVOLT_SESSION_CONFIG") {
        let path = PathBuf::from(path);
        merged = merge_layer(
            merged,
            read_mode_b_printrevolt_value(
                &path,
                |file| ConfigSource::SessionOverlay { file },
                &mut sources,
                &mut warnings,
            ),
            &mut trace,
        );
    }

    if let Some(cli_overrides) = cli_overrides {
        sources.push(SourceReport {
            source: ConfigSource::CliOverrides,
            status: ReadStatus::ReadOk,
            message: None,
        });
        merged = merge_layer(
            merged,
            Some((cli_overrides, ConfigSource::CliOverrides)),
            &mut trace,
        );
    }

    ResolvedPrintRevoltConfig {
        printrevolt: merged,
        trace,
        sources,
        warnings,
    }
}

pub fn flatten_printrevolt(value: &TomlValue) -> BTreeMap<String, TomlValue> {
    let mut out = BTreeMap::new();
    flatten_toml_value("", value, &mut out);
    out
}

pub fn write_mode_b_printrevolt_toml_atomic(
    target: &Path,
    value: &TomlValue,
) -> Result<(), PrConfigError> {
    let parent = target.parent().ok_or_else(|| PrConfigError::WriteFailed {
        path: target.to_path_buf(),
        message: "missing parent directory".to_string(),
    })?;
    std::fs::create_dir_all(parent).map_err(|err| PrConfigError::WriteFailed {
        path: parent.to_path_buf(),
        message: err.to_string(),
    })?;

    let lock_path = lock_path_for(target);
    let lock_file = File::options()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|err| PrConfigError::WriteFailed {
            path: lock_path.clone(),
            message: err.to_string(),
        })?;
    lock_file
        .try_lock_exclusive()
        .map_err(|err| PrConfigError::WriteFailed {
            path: lock_path.clone(),
            message: format!("failed to acquire lock: {err}"),
        })?;

    let mut tmp_path = parent.join(
        target
            .file_name()
            .unwrap_or_else(|| OsStr::new("printrevolt.toml")),
    );
    tmp_path.set_extension("toml.tmp");

    let encoded = render_stable_toml_document(value).map_err(|err| PrConfigError::WriteFailed {
        path: target.to_path_buf(),
        message: err,
    })?;

    {
        let mut tmp = File::create(&tmp_path).map_err(|err| PrConfigError::WriteFailed {
            path: tmp_path.clone(),
            message: err.to_string(),
        })?;
        tmp.write_all(encoded.as_bytes())
            .map_err(|err| PrConfigError::WriteFailed {
                path: tmp_path.clone(),
                message: err.to_string(),
            })?;
        tmp.sync_all().map_err(|err| PrConfigError::WriteFailed {
            path: tmp_path.clone(),
            message: err.to_string(),
        })?;
    }

    std::fs::rename(&tmp_path, target).map_err(|err| PrConfigError::WriteFailed {
        path: target.to_path_buf(),
        message: err.to_string(),
    })?;

    let dir_file = File::open(parent).map_err(|err| PrConfigError::WriteFailed {
        path: parent.to_path_buf(),
        message: err.to_string(),
    })?;
    let _ = dir_file.sync_all();

    Ok(())
}

fn lock_path_for(target: &Path) -> PathBuf {
    let mut lock = target.to_path_buf();
    let file_name = target
        .file_name()
        .unwrap_or_else(|| OsStr::new("printrevolt.toml"));
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    lock.set_file_name(lock_name);
    lock
}

fn default_printrevolt_table() -> TomlValue {
    let mut root = toml::map::Map::new();
    root.insert("enabled".to_string(), TomlValue::Boolean(true));
    root.insert("vars".to_string(), TomlValue::Table(toml::map::Map::new()));
    TomlValue::Table(root)
}

fn init_trace_from_defaults(
    value: &TomlValue,
    source: ConfigSource,
) -> BTreeMap<String, ConfigSource> {
    let mut out = BTreeMap::new();
    let mut flattened = BTreeMap::new();
    flatten_toml_value("", value, &mut flattened);
    for key in flattened.keys() {
        out.insert(key.clone(), source.clone());
    }
    out
}

fn read_mode_a_printrevolt_table(
    upstream_config_toml: &Path,
    source_kind: fn(PathBuf) -> ConfigSource,
    sources: &mut Vec<SourceReport>,
    warnings: &mut Vec<String>,
) -> Option<(TomlValue, ConfigSource)> {
    let file = upstream_config_toml.to_path_buf();
    let source = source_kind(file.clone());
    let Ok(raw) = std::fs::read_to_string(&file) else {
        sources.push(SourceReport {
            source,
            status: ReadStatus::Missing,
            message: None,
        });
        return None;
    };

    let parsed: TomlValue = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            sources.push(SourceReport {
                source: source.clone(),
                status: ReadStatus::ParseError,
                message: Some(err.to_string()),
            });
            warnings.push(format!(
                "failed to parse Mode A printrevolt table from {}: {err}",
                file.display()
            ));
            return None;
        }
    };

    sources.push(SourceReport {
        source: source.clone(),
        status: ReadStatus::ReadOk,
        message: None,
    });

    let Some(printrevolt) = parsed
        .get("printrevolt")
        .cloned()
        .filter(TomlValue::is_table)
    else {
        return None;
    };

    Some((printrevolt, source))
}

fn read_mode_b_printrevolt_value(
    printrevolt_toml: &Path,
    source_kind: fn(PathBuf) -> ConfigSource,
    sources: &mut Vec<SourceReport>,
    warnings: &mut Vec<String>,
) -> Option<(TomlValue, ConfigSource)> {
    let file = printrevolt_toml.to_path_buf();
    let source = source_kind(file.clone());
    let Ok(raw) = std::fs::read_to_string(&file) else {
        sources.push(SourceReport {
            source,
            status: ReadStatus::Missing,
            message: None,
        });
        return None;
    };

    let parsed: TomlValue = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            sources.push(SourceReport {
                source: source.clone(),
                status: ReadStatus::ParseError,
                message: Some(err.to_string()),
            });
            warnings.push(format!(
                "failed to parse Mode B printrevolt.toml {}: {err}",
                file.display()
            ));
            return None;
        }
    };

    sources.push(SourceReport {
        source: source.clone(),
        status: ReadStatus::ReadOk,
        message: None,
    });

    let Some(printrevolt) = parsed
        .get("printrevolt")
        .cloned()
        .filter(TomlValue::is_table)
    else {
        return None;
    };

    Some((printrevolt, source))
}

fn merge_layer(
    base: TomlValue,
    layer: Option<(TomlValue, ConfigSource)>,
    trace: &mut BTreeMap<String, ConfigSource>,
) -> TomlValue {
    let Some((layer_value, source)) = layer else {
        return base;
    };
    deep_merge_with_trace(base, layer_value, source, trace)
}

fn deep_merge_with_trace(
    base: TomlValue,
    overlay: TomlValue,
    source: ConfigSource,
    trace: &mut BTreeMap<String, ConfigSource>,
) -> TomlValue {
    let mut overlay_flattened = BTreeMap::new();
    flatten_toml_value("", &overlay, &mut overlay_flattened);
    for (path, _) in overlay_flattened {
        trace.insert(path, source.clone());
    }

    match (base, overlay) {
        (TomlValue::Table(mut base_table), TomlValue::Table(overlay_table)) => {
            for (key, overlay_value) in overlay_table {
                match base_table.remove(&key) {
                    Some(existing) => {
                        let merged =
                            deep_merge_with_trace(existing, overlay_value, source.clone(), trace);
                        base_table.insert(key, merged);
                    }
                    None => {
                        base_table.insert(key, overlay_value);
                    }
                }
            }
            TomlValue::Table(base_table)
        }
        (_, overlay_leaf) => overlay_leaf,
    }
}

fn flatten_toml_value(prefix: &str, value: &TomlValue, out: &mut BTreeMap<String, TomlValue>) {
    match value {
        TomlValue::Table(table) => {
            for (k, v) in table {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_toml_value(&next, v, out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

fn render_stable_toml_document(value: &TomlValue) -> Result<String, String> {
    let mut doc = toml_edit::DocumentMut::new();
    let mut root = toml_edit::Table::new();
    root.set_implicit(true);

    let printrevolt = value
        .as_table()
        .ok_or_else(|| "printrevolt value must be a table".to_string())?;
    let mut printrevolt_table = toml_edit::Table::new();
    for key in sorted_keys(printrevolt) {
        let v = printrevolt
            .get(key.as_str())
            .ok_or_else(|| format!("missing key {key}"))?;
        insert_toml_value(&mut printrevolt_table, key.as_str(), v)?;
    }
    root.insert("printrevolt", toml_edit::Item::Table(printrevolt_table));
    doc.as_table_mut().extend(root);
    Ok(doc.to_string())
}

fn sorted_keys(table: &toml::map::Map<String, TomlValue>) -> Vec<String> {
    let mut keys = table.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys
}

fn insert_toml_value(
    table: &mut toml_edit::Table,
    key: &str,
    value: &TomlValue,
) -> Result<(), String> {
    match value {
        TomlValue::String(s) => {
            table.insert(key, toml_edit::value(s.clone()));
            Ok(())
        }
        TomlValue::Integer(i) => {
            table.insert(key, toml_edit::value(*i));
            Ok(())
        }
        TomlValue::Float(f) => {
            table.insert(key, toml_edit::value(*f));
            Ok(())
        }
        TomlValue::Boolean(b) => {
            table.insert(key, toml_edit::value(*b));
            Ok(())
        }
        TomlValue::Datetime(dt) => {
            table.insert(key, toml_edit::value(dt.to_string()));
            Ok(())
        }
        TomlValue::Array(arr) => {
            let mut out = toml_edit::Array::new();
            for item in arr {
                match item {
                    TomlValue::String(s) => out.push(s.as_str()),
                    TomlValue::Integer(i) => out.push(*i),
                    TomlValue::Float(f) => out.push(*f),
                    TomlValue::Boolean(b) => out.push(*b),
                    _ => return Err(format!("unsupported array item for key {key}")),
                }
            }
            table.insert(key, toml_edit::Item::Value(toml_edit::Value::Array(out)));
            Ok(())
        }
        TomlValue::Table(nested) => {
            let mut nested_table = toml_edit::Table::new();
            for nested_key in sorted_keys(nested) {
                let nested_value = nested
                    .get(nested_key.as_str())
                    .ok_or_else(|| format!("missing key {nested_key}"))?;
                insert_toml_value(&mut nested_table, nested_key.as_str(), nested_value)?;
            }
            table.insert(key, toml_edit::Item::Table(nested_table));
            Ok(())
        }
    }
}
