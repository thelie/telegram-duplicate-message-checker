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

/// Actions that the handler determines need to happen, computed while
/// holding only the tracker lock. Executed afterward with only the marker lock.
pub enum Action {
    /// No action needed.
    None,
    /// Mark these forward locations as read, and optionally cache a peer.
    MarkForwards {
        forwards: Vec<ForwardLocation>,
        cache_peer: Option<(i64, PeerRef)>,
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
        Action::MarkForwards {
            forwards,
            cache_peer,
        } => {
            if let Some((chat_id, peer_ref)) = cache_peer {
                marker.cache_peer(chat_id, peer_ref);
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

    // Grab peer ref for caching (async but no tracker involvement)
    let cache_peer = message
        .peer_ref()
        .await
        .map(|peer_ref| (chat_id, peer_ref));

    debug!(
        "Forward detected: original=({}, {}) -> forward=({}, {})",
        original.peer_id, original.message_id, forward.chat_id, forward.message_id
    );

    let already_read = tracker.register_forward(original, forward.clone());

    if already_read {
        info!(
            "Original already read, marking forward as read: chat={}, msg={}",
            forward.chat_id, forward.message_id
        );
        Action::MarkForwards {
            forwards: vec![forward],
            cache_peer,
        }
    } else {
        // Still cache the peer even if no marking needed
        if let Some((cid, pref)) = cache_peer {
            Action::MarkForwards {
                forwards: vec![],
                cache_peer: Some((cid, pref)),
            }
        } else {
            Action::None
        }
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

    info!(
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

    info!("Marking {} other forwards as read", all_forwards.len());

    Action::MarkForwards {
        forwards: all_forwards,
        cache_peer: None,
    }
}
