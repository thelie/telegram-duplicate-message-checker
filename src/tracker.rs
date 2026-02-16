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
    pub fn register_forward(
        &mut self,
        original: OriginalMessageId,
        forward: ForwardLocation,
    ) {
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
            .insert(forward, original);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn orig(peer: i64, msg: i32) -> OriginalMessageId {
        OriginalMessageId { peer_id: peer, message_id: msg }
    }

    fn fwd(chat: i64, msg: i32) -> ForwardLocation {
        ForwardLocation { chat_id: chat, message_id: msg }
    }

    #[test]
    fn register_and_lookup() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100);
        let f = fwd(2, 200);

        t.register_forward(o.clone(), f.clone());

        assert_eq!(t.lookup_forward(&f), Some(&o));
        assert!(!t.is_original_read(&o));
    }

    #[test]
    fn register_duplicate_forward_is_idempotent() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100);
        let f = fwd(2, 200);

        t.register_forward(o.clone(), f.clone());
        t.register_forward(o.clone(), f.clone());

        // Should only have one entry, not two
        assert_eq!(t.originals.get(&o).unwrap().len(), 1);
    }

    #[test]
    fn multiple_forwards_of_same_original() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100);
        let f1 = fwd(2, 200);
        let f2 = fwd(3, 300);

        t.register_forward(o.clone(), f1.clone());
        t.register_forward(o.clone(), f2.clone());

        assert_eq!(t.originals.get(&o).unwrap().len(), 2);
        assert_eq!(t.lookup_forward(&f1), Some(&o));
        assert_eq!(t.lookup_forward(&f2), Some(&o));
    }

    #[test]
    fn mark_original_read_returns_all_forwards() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100);
        let f1 = fwd(2, 200);
        let f2 = fwd(3, 300);

        t.register_forward(o.clone(), f1.clone());
        t.register_forward(o.clone(), f2.clone());

        let result = t.mark_original_read(&o);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&f1));
        assert!(result.contains(&f2));
        assert!(t.is_original_read(&o));
    }

    #[test]
    fn find_read_originals_filters_by_chat_and_max_id() {
        let mut t = DuplicateTracker::default();
        let o1 = orig(1, 100);
        let o2 = orig(1, 101);

        // Both forwarded to chat 10, different message IDs
        t.register_forward(o1.clone(), fwd(10, 50));
        t.register_forward(o2.clone(), fwd(10, 60));

        // max_id=55 should only find the forward with msg_id=50
        let found = t.find_read_originals_in_chat(10, 55);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], o1);

        // max_id=60 should find both
        let found = t.find_read_originals_in_chat(10, 60);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn find_read_originals_excludes_already_read() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100);
        t.register_forward(o.clone(), fwd(10, 50));

        // Mark as read first
        t.mark_original_read(&o);

        // Should not appear again
        let found = t.find_read_originals_in_chat(10, 50);
        assert!(found.is_empty());
    }

    #[test]
    fn find_read_originals_empty_for_unknown_chat() {
        let t = DuplicateTracker::default();
        assert!(t.find_read_originals_in_chat(999, 100).is_empty());
    }

    #[test]
    fn cleanup_removes_old_entries() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100);
        let f = fwd(2, 200);
        t.register_forward(o.clone(), f.clone());

        // Backdate the first_seen to make it old
        t.first_seen.insert(o.clone(), 0);

        t.cleanup(1); // max_age_secs=1, anything older than ~now is removed

        assert!(t.originals.is_empty());
        assert!(t.forward_index.is_empty());
        assert!(t.read_originals.is_empty());
        assert!(t.first_seen.is_empty());
        assert!(t.chat_index.is_empty());
    }

    #[test]
    fn cleanup_keeps_recent_entries() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100);
        let f = fwd(2, 200);
        t.register_forward(o.clone(), f.clone());

        // Very long max age — nothing should be cleaned
        t.cleanup(999_999_999);

        assert_eq!(t.originals.len(), 1);
        assert_eq!(t.forward_index.len(), 1);
    }

    /// Simulates the discussion group scenario: a channel post forwarded to
    /// two discussion groups. Reading the thread root (top_msg_id) in one
    /// group should propagate to the other.
    #[test]
    fn discussion_group_read_propagation() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100); // original channel post

        // Same post forwarded to two discussion groups as thread roots
        let f1 = fwd(10, 300); // discussion group A, thread root msg 300
        let f2 = fwd(20, 400); // discussion group B, thread root msg 400

        t.register_forward(o.clone(), f1.clone());
        t.register_forward(o.clone(), f2.clone());

        // User opens thread in group A → ReadChannelDiscussionInbox
        // with top_msg_id=300. We look up originals with msg_id <= 300
        // in chat 10, which finds f1.
        let found = t.find_read_originals_in_chat(10, 300);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], o);

        // Mark original as read, get all forwards to propagate
        let all_fwds = t.mark_original_read(&o);
        assert_eq!(all_fwds.len(), 2);

        // Filter out the one the user already read (f1 in chat 10)
        let to_mark: Vec<_> = all_fwds
            .into_iter()
            .filter(|f| !(f.chat_id == 10 && f.message_id <= 300))
            .collect();
        assert_eq!(to_mark.len(), 1);
        assert_eq!(to_mark[0], f2);
    }

    #[test]
    fn save_and_load_round_trip() {
        let mut t = DuplicateTracker::default();
        let o = orig(1, 100);
        let f1 = fwd(2, 200);
        let f2 = fwd(3, 300);
        t.register_forward(o.clone(), f1.clone());
        t.register_forward(o.clone(), f2.clone());
        t.mark_original_read(&o);

        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();
        t.save(&path).unwrap();

        let loaded = DuplicateTracker::load(&path).unwrap();

        // Verify data survived the round trip
        assert_eq!(loaded.originals.len(), 1);
        assert_eq!(loaded.originals.get(&o).unwrap().len(), 2);
        assert!(loaded.is_original_read(&o));
        assert_eq!(loaded.lookup_forward(&f1), Some(&o));
        assert_eq!(loaded.lookup_forward(&f2), Some(&o));
        // chat_index is rebuilt from forward_index on load
        assert!(!loaded.chat_index.is_empty());
    }
}
