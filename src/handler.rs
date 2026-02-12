use grammers_client::update::Update;
use grammers_session::types::{PeerId, PeerRef};
use grammers_tl_types as tl;
use tracing::{debug, info};

use crate::marker::Marker;
use crate::tracker::{DuplicateTracker, ForwardLocation, OriginalMessageId};

/// Extract an i64 chat identifier from a `tl::enums::Peer`.
fn peer_to_chat_id(peer: &tl::enums::Peer) -> i64 {
    PeerId::from(peer).bot_api_dialog_id()
}

/// Try to extract the original message identity from a forward header.
fn extract_original(fwd: &tl::enums::MessageFwdHeader) -> Option<OriginalMessageId> {
    let tl::enums::MessageFwdHeader::Header(header) = fwd;
    let from_id = header.from_id.as_ref()?;
    let channel_post = header.channel_post?;
    Some(OriginalMessageId {
        peer_id: peer_to_chat_id(from_id),
        message_id: channel_post,
    })
}

/// Truncate a string to at most `max` characters, appending "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Actions that the handler determines need to happen, computed while
/// holding only the tracker lock. Executed afterward with only the marker lock.
pub enum Action {
    /// No action needed.
    None,
    /// Cache a peer we learned about from an incoming message.
    CachePeer {
        chat_id: i64,
        peer_ref: PeerRef,
        name: String,
    },
    /// Mark these forward locations as read.
    MarkForwards {
        forwards: Vec<ForwardLocation>,
    },
}

/// Phase 1: Inspect the update and compute what actions are needed.
/// Only requires the tracker (no network I/O).
pub async fn plan_update(
    update: &Update,
    tracker: &mut DuplicateTracker,
) -> Action {
    match update {
        Update::NewMessage(message) => plan_new_message(message, tracker).await,
        // Read events come through as raw TL updates (not wrapped by grammers)
        Update::Raw(raw) => plan_raw_update(&raw.raw, tracker),
        _ => Action::None,
    }
}

/// Phase 2: Execute the planned action using the marker (network I/O).
/// Only requires the marker.
pub async fn execute_action(action: Action, marker: &mut Marker) {
    match action {
        Action::None => {}
        Action::CachePeer {
            chat_id,
            peer_ref,
            name,
        } => {
            marker.cache_peer(chat_id, peer_ref, name);
        }
        Action::MarkForwards { forwards } => {
            for fwd in &forwards {
                let name = marker.get_chat_name(fwd.chat_id);
                info!(
                    "Marking as read in {} (chat={}, msg={})",
                    name, fwd.chat_id, fwd.message_id
                );
            }
            if let Err(e) = marker.mark_forwards_read(&forwards).await {
                tracing::warn!("Error marking forwards as read: {}", e);
            }
        }
    }
}

/// Plan actions for an incoming new message — detect forwards and register them.
async fn plan_new_message(
    message: &grammers_client::update::Message,
    tracker: &mut DuplicateTracker,
) -> Action {
    let fwd_header = match message.forward_header() {
        Some(h) => h,
        None => return Action::None,
    };

    let original = match extract_original(&fwd_header) {
        Some(o) => o,
        None => return Action::None,
    };

    let chat_id = message.peer_id().bot_api_dialog_id();
    let forward = ForwardLocation {
        chat_id,
        message_id: message.id(),
    };

    let chat_name = message
        .peer()
        .and_then(|p| p.name().map(str::to_owned))
        .unwrap_or_else(|| chat_id.to_string());
    let preview = truncate(message.text(), 100);

    info!(
        "Forward detected in {} ({}): original=({}, {}) msg={} \"{}\"",
        chat_name, chat_id, original.peer_id, original.message_id,
        forward.message_id, preview
    );

    tracker.register_forward(original, forward);

    // Cache the peer so we can mark-read later
    match message.peer_ref().await {
        Some(peer_ref) => Action::CachePeer {
            chat_id,
            peer_ref,
            name: chat_name,
        },
        None => Action::None,
    }
}

/// Plan actions for raw updates — specifically read-history events.
fn plan_raw_update(
    raw: &tl::enums::Update,
    tracker: &mut DuplicateTracker,
) -> Action {
    match raw {
        tl::enums::Update::ReadHistoryInbox(u) => {
            let chat_id = peer_to_chat_id(&u.peer);
            plan_read_event(chat_id, u.max_id, tracker)
        }
        tl::enums::Update::ReadChannelInbox(u) => {
            let chat_id = PeerId::channel(u.channel_id).bot_api_dialog_id();
            plan_read_event(chat_id, u.max_id, tracker)
        }
        _ => Action::None,
    }
}

/// When the user reads messages in a chat, check if any tracked forwards
/// were among them and plan read-propagation to other copies.
fn plan_read_event(
    chat_id: i64,
    max_id: i32,
    tracker: &mut DuplicateTracker,
) -> Action {
    let originals = tracker.find_read_originals_in_chat(chat_id, max_id);
    if originals.is_empty() {
        return Action::None;
    }

    debug!(
        "Read event in chat {}: {} originals newly read",
        chat_id,
        originals.len()
    );

    let mut all_forwards = Vec::new();
    for original in originals {
        let forwards = tracker.mark_original_read(&original);
        // Collect forwards in other chats (or with msg_id > max_id in same chat)
        let other_forwards = forwards
            .into_iter()
            .filter(|f| !(f.chat_id == chat_id && f.message_id <= max_id));
        all_forwards.extend(other_forwards);
    }

    if all_forwards.is_empty() {
        return Action::None;
    }

    info!(
        "Read in chat {}, propagating to {} other forwards",
        chat_id,
        all_forwards.len()
    );

    Action::MarkForwards {
        forwards: all_forwards,
    }
}
