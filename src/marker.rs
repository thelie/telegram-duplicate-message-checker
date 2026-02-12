use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Result};
use grammers_client::Client;
use grammers_session::types::{PeerKind, PeerRef};
use grammers_tl_types as tl;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::tracker::ForwardLocation;

/// Delay between consecutive mark-as-read API calls to avoid flood limits.
const MARK_READ_DELAY: Duration = Duration::from_millis(500);

/// Caches peer references and names so we can make API calls for any known chat.
pub struct Marker {
    client: Client,
    /// chat_id (bot_api_dialog_id) -> (PeerRef, display name)
    peer_cache: HashMap<i64, (PeerRef, String)>,
}

impl Marker {
    pub fn new(client: Client) -> Self {
        Marker {
            client,
            peer_cache: HashMap::new(),
        }
    }

    /// Populate the peer cache by iterating all dialogs.
    pub async fn build_peer_cache(&mut self) -> Result<()> {
        let mut dialogs = self.client.iter_dialogs();
        let total = dialogs.total().await?;
        info!("Building peer cache from {} dialogs", total);

        while let Some(dialog) = dialogs.next().await? {
            let peer = dialog.peer();
            let chat_id = peer.id().bot_api_dialog_id();
            if let Some(peer_ref) = peer.to_ref().await {
                let name = peer.name().unwrap_or("unnamed").to_owned();
                self.peer_cache.insert(chat_id, (peer_ref, name));
            }
        }

        info!("Peer cache built with {} entries", self.peer_cache.len());
        Ok(())
    }

    /// Cache a peer reference we learn about from an incoming update.
    pub fn cache_peer(&mut self, chat_id: i64, peer_ref: PeerRef, name: String) {
        self.peer_cache.entry(chat_id).or_insert((peer_ref, name));
    }

    /// Look up the display name for a chat, falling back to its numeric ID.
    pub fn get_chat_name(&self, chat_id: i64) -> &str {
        self.peer_cache
            .get(&chat_id)
            .map(|(_, name)| name.as_str())
            .unwrap_or("unknown")
    }

    /// Mark messages up to `max_id` as read in a given chat.
    pub async fn mark_read(&self, chat_id: i64, max_id: i32) -> Result<()> {
        let peer_ref = match self.peer_cache.get(&chat_id) {
            Some((p, _)) => *p,
            None => {
                bail!("No cached peer for chat_id={}, cannot mark as read", chat_id);
            }
        };

        debug!("Marking as read: chat_id={}, max_id={}", chat_id, max_id);

        if peer_ref.id.kind() == PeerKind::Channel {
            self.client
                .invoke(&tl::functions::channels::ReadHistory {
                    channel: peer_ref.into(),
                    max_id,
                })
                .await
                .map(drop)?;
        } else {
            self.client
                .invoke(&tl::functions::messages::ReadHistory {
                    peer: peer_ref.into(),
                    max_id,
                })
                .await
                .map(drop)?;
        }

        Ok(())
    }

    /// Mark a list of forward locations as read, with delays between calls
    /// to avoid Telegram flood limits.
    pub async fn mark_forwards_read(&self, forwards: &[ForwardLocation]) -> Result<()> {
        for (i, fwd) in forwards.iter().enumerate() {
            if i > 0 {
                sleep(MARK_READ_DELAY).await;
            }
            if let Err(e) = self.mark_read(fwd.chat_id, fwd.message_id).await {
                warn!(
                    "Failed to mark forward as read (chat={}, msg={}): {}",
                    fwd.chat_id, fwd.message_id, e
                );
            }
        }
        Ok(())
    }
}
