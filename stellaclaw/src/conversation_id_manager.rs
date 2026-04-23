use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConversationIdStore {
    #[serde(default)]
    next_index_by_channel: BTreeMap<String, u64>,
    #[serde(default)]
    mappings: BTreeMap<String, String>,
}

pub struct ConversationIdManager {
    path: PathBuf,
    store: ConversationIdStore,
}

impl ConversationIdManager {
    pub fn load_under(workdir: &Path) -> Result<Self, String> {
        let dir = workdir.join(".log").join("stellaclaw");
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
        let path = dir.join("conversation_ids.json");
        let store = if path.exists() {
            let raw = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            serde_json::from_str(&raw)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?
        } else {
            ConversationIdStore::default()
        };
        Ok(Self { path, store })
    }

    pub fn get_or_create(
        &mut self,
        channel_id: &str,
        platform_chat_id: &str,
    ) -> Result<String, String> {
        let key = format!("{channel_id}::{platform_chat_id}");
        if let Some(existing) = self.store.mappings.get(&key) {
            return Ok(existing.clone());
        }
        let next = self
            .store
            .next_index_by_channel
            .entry(channel_id.to_string())
            .or_insert(1);
        let conversation_id = format!("{channel_id}-{:06}", *next);
        *next = next.saturating_add(1);
        self.store.mappings.insert(key, conversation_id.clone());
        self.save()?;
        Ok(conversation_id)
    }

    fn save(&self) -> Result<(), String> {
        let raw = serde_json::to_string_pretty(&self.store)
            .map_err(|error| format!("failed to serialize conversation id store: {error}"))?;
        fs::write(&self.path, raw)
            .map_err(|error| format!("failed to write {}: {error}", self.path.display()))
    }
}
