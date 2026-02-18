use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

const SCHEMA_VERSION_V1: &str = "1";
const MAX_MRU_ENTRIES_PER_AGENT: usize = 50;
const MAX_MRU_AGE_MS: u64 = 90 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct TemplatesStateFileV1 {
    pub(crate) schema_version: String,
    #[serde(default)]
    pub(crate) agents: HashMap<String, AgentTemplatesStateV1>,
}

impl Default for TemplatesStateFileV1 {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION_V1.to_string(),
            agents: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) struct AgentTemplatesStateV1 {
    #[serde(default)]
    pub(crate) mru: Vec<MruEntryV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct MruEntryV1 {
    pub(crate) template_id: String,
    pub(crate) last_used_at_ms: u64,
}

#[derive(Debug)]
pub(crate) struct TemplatesMruStore {
    path: PathBuf,
    data: TemplatesStateFileV1,
    dirty: bool,
}

impl TemplatesMruStore {
    pub(crate) fn load(codex_home: &Path) -> Self {
        let path = codex_home
            .join("printrevolt")
            .join("state")
            .join("templates.json");
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<TemplatesStateFileV1>(&raw).ok())
            .filter(|d| d.schema_version == SCHEMA_VERSION_V1)
            .unwrap_or_default();
        let mut out = Self {
            path,
            data,
            dirty: false,
        };
        out.prune_old_entries(now_ms());
        out
    }

    pub(crate) fn last_used_at_ms(&self, agent_id: &str, template_id: &str) -> Option<u64> {
        self.data
            .agents
            .get(agent_id)
            .and_then(|a| a.mru.iter().find(|e| e.template_id == template_id))
            .map(|e| e.last_used_at_ms)
    }

    pub(crate) fn mru_template_ids(&self, agent_id: &str) -> Vec<String> {
        self.data
            .agents
            .get(agent_id)
            .map(|a| a.mru.iter().map(|e| e.template_id.clone()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn note_used(&mut self, agent_id: &str, template_id: &str) {
        let now = now_ms();
        let agent = self.data.agents.entry(agent_id.to_string()).or_default();
        agent.mru.retain(|e| e.template_id != template_id);
        agent.mru.insert(
            0,
            MruEntryV1 {
                template_id: template_id.to_string(),
                last_used_at_ms: now,
            },
        );
        agent.mru.truncate(MAX_MRU_ENTRIES_PER_AGENT);
        self.prune_old_entries(now);
        self.dirty = true;
    }

    pub(crate) fn persist_if_dirty(&mut self) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        let parent = self.path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "missing parent directory")
        })?;
        std::fs::create_dir_all(parent)?;

        let tmp = parent.join("templates.json.tmp");
        let raw = serde_json::to_string_pretty(&self.data)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))?;
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &self.path)?;

        self.dirty = false;
        Ok(())
    }

    fn prune_old_entries(&mut self, now_ms: u64) {
        let mut mutated = false;
        for agent in self.data.agents.values_mut() {
            let before = agent.mru.len();
            agent
                .mru
                .retain(|e| now_ms.saturating_sub(e.last_used_at_ms) <= MAX_MRU_AGE_MS);
            agent.mru.truncate(MAX_MRU_ENTRIES_PER_AGENT);
            mutated |= before != agent.mru.len();
        }
        if mutated {
            self.dirty = true;
        }
    }
}

pub(crate) fn format_used_ago(now_ms: u64, last_used_at_ms: u64) -> Option<String> {
    let delta_ms = now_ms.checked_sub(last_used_at_ms)?;
    let secs = delta_ms / 1000;
    if secs < 60 {
        return Some(format!("used {secs}s ago"));
    }
    let mins = secs / 60;
    if mins < 60 {
        return Some(format!("used {mins}m ago"));
    }
    let hours = mins / 60;
    if hours < 24 {
        return Some(format!("used {hours}h ago"));
    }
    let days = hours / 24;
    Some(format!("used {days}d ago"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
