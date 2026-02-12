use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;
use tracing::info;

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct OriginalMessageId {
    pub peer_id: i64,
    pub message_id: i32,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForwardLocation {
    pub chat_id: i64,
    pub message_id: i32,
}

/// serde_json can't use structs as map keys (JSON keys must be strings).
/// These helpers serialize HashMap<K,V> as Vec<(K,V)> instead.
mod map_as_vec {
    use super::*;

    pub fn serialize<S, K, V>(map: &HashMap<K, V>, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
        K: Serialize + Eq + Hash,
        V: Serialize,
    {
        let vec: Vec<(&K, &V)> = map.iter().collect();
        vec.serialize(serializer)
    }

    pub fn deserialize<'de, D, K, V>(deserializer: D) -> std::result::Result<HashMap<K, V>, D::Error>
    where
        D: Deserializer<'de>,
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
    {
        let vec: Vec<(K, V)> = Vec::deserialize(deserializer)?;
        Ok(vec.into_iter().collect())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DuplicateTracker {
    /// original -> all known forwards
    #[serde(with = "map_as_vec")]
    originals: HashMap<OriginalMessageId, Vec<ForwardLocation>>,
    /// forward location -> its original
    #[serde(with = "map_as_vec")]
    forward_index: HashMap<ForwardLocation, OriginalMessageId>,
    /// originals the user has read
    read_originals: HashSet<OriginalMessageId>,
    /// timestamp (seconds since epoch) when each original was first seen
    #[serde(default, with = "map_as_vec")]
    first_seen: HashMap<OriginalMessageId, u64>,
    /// chat_id -> set of (message_id, original) for O(1) read-event lookups.
    /// Rebuilt from forward_index on load, so not critical to persist.
    #[serde(skip)]
    chat_index: HashMap<i64, Vec<(i32, OriginalMessageId)>>,
}

impl DuplicateTracker {
    /// Register a forwarded message as a copy of an original.
    /// Returns true if the original was already marked as read (meaning
    /// the caller should mark this forward as read too).
    pub fn register_forward(
        &mut self,
        original: OriginalMessageId,
        forward: ForwardLocation,
    ) -> bool {
        let now = epoch_secs();
        self.first_seen.entry(original.clone()).or_insert(now);

        let forwards = self.originals.entry(original.clone()).or_default();
        if !forwards.contains(&forward) {
            forwards.push(forward.clone());
        }

        // Update chat_index for fast read-event lookups
        let chat_entries = self.chat_index.entry(forward.chat_id).or_default();
        if !chat_entries.iter().any(|(mid, _)| *mid == forward.message_id) {
            chat_entries.push((forward.message_id, original.clone()));
        }

        self.forward_index
            .insert(forward, original.clone());

        self.read_originals.contains(&original)
    }

    /// Mark an original as read. Returns all forward locations
    /// that should also be marked as read.
    pub fn mark_original_read(&mut self, original: &OriginalMessageId) -> Vec<ForwardLocation> {
        self.read_originals.insert(original.clone());
        self.originals
            .get(original)
            .cloned()
            .unwrap_or_default()
    }

    /// Look up which original a forward belongs to.
    #[allow(dead_code)]
    pub fn lookup_forward(&self, forward: &ForwardLocation) -> Option<&OriginalMessageId> {
        self.forward_index.get(forward)
    }

    /// Check if an original has been read.
    #[allow(dead_code)]
    pub fn is_original_read(&self, original: &OriginalMessageId) -> bool {
        self.read_originals.contains(original)
    }

    /// Find originals for forwards in a given chat with message_id <= max_id
    /// that haven't been marked as read yet. Uses the chat_index for O(1)
    /// lookup by chat_id instead of scanning the entire forward_index.
    pub fn find_read_originals_in_chat(
        &self,
        chat_id: i64,
        max_id: i32,
    ) -> Vec<OriginalMessageId> {
        let entries = match self.chat_index.get(&chat_id) {
            Some(e) => e,
            None => return Vec::new(),
        };

        let mut originals = Vec::new();
        for (msg_id, orig) in entries {
            if *msg_id <= max_id && !self.read_originals.contains(orig) {
                originals.push(orig.clone());
            }
        }
        originals
    }

    /// Remove entries older than `max_age_secs`.
    pub fn cleanup(&mut self, max_age_secs: u64) {
        let cutoff = epoch_secs().saturating_sub(max_age_secs);
        let old_originals: Vec<OriginalMessageId> = self
            .first_seen
            .iter()
            .filter(|(_, &ts)| ts < cutoff)
            .map(|(k, _)| k.clone())
            .collect();

        let count = old_originals.len();
        for orig in &old_originals {
            if let Some(forwards) = self.originals.remove(orig) {
                for fwd in &forwards {
                    self.forward_index.remove(fwd);
                    if let Some(chat_entries) = self.chat_index.get_mut(&fwd.chat_id) {
                        chat_entries.retain(|(mid, _)| *mid != fwd.message_id);
                        if chat_entries.is_empty() {
                            self.chat_index.remove(&fwd.chat_id);
                        }
                    }
                }
            }
            self.read_originals.remove(orig);
            self.first_seen.remove(orig);
        }
        if count > 0 {
            info!("Cleaned up {} old entries", count);
        }
    }

    /// Rebuild the chat_index from forward_index.
    fn rebuild_chat_index(&mut self) {
        self.chat_index.clear();
        for (fwd, orig) in &self.forward_index {
            self.chat_index
                .entry(fwd.chat_id)
                .or_default()
                .push((fwd.message_id, orig.clone()));
        }
    }

    /// Load state from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .context("Failed to read state file")?;
        let mut tracker: Self =
            serde_json::from_str(&data).context("Failed to parse state file")?;
        // chat_index is skipped during serde, always rebuild it
        tracker.rebuild_chat_index();
        Ok(tracker)
    }

    /// Save state to a JSON file atomically (write .tmp then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp_path = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self)
            .context("Failed to serialize state")?;
        std::fs::write(&tmp_path, data)
            .context("Failed to write temp state file")?;
        std::fs::rename(&tmp_path, path)
            .context("Failed to rename temp state file")?;
        Ok(())
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
