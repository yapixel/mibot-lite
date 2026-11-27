use crate::{SeenMessages, is_edit_update};
use grammers_session::updates::UpdatesLike;
use grammers_tl_types as tl;
use mibot_bot::{PeerCache, input_peer_to_peer, peer_id};
use mibot_logtail::{Level, Ring};
use std::sync::Arc;
use tokio::sync::mpsc;

pub type MessageSink = Arc<dyn Fn(&tl::types::Message, bool) -> bool + Send + Sync>;

/// Bridges `SenderPool::updates` immediately to live command dispatch while non-blockingly
/// forwarding raw `UpdatesLike` items into `UpdatesReceiver` for background `MessageBoxes` sync.
pub struct LiveHandler {
    peers: Arc<PeerCache>,
    seen: Arc<SeenMessages>,
    logs: Arc<Ring>,
    on_message: MessageSink,
}

impl LiveHandler {
    pub fn new(
        peers: Arc<PeerCache>,
        seen: Arc<SeenMessages>,
        logs: Arc<Ring>,
        on_message: MessageSink,
    ) -> Self {
        Self {
            peers,
            seen,
            logs,
            on_message,
        }
    }

    /// Processes a single `UpdatesLike` packet immediately for peer caching, echo suppression,
    /// and live incoming/outgoing command handling without waiting for gap-fill `UpdatesReceiver`.
    pub fn process_live(&self, packet: &UpdatesLike) {
        match packet {
            UpdatesLike::ShortSentMessage { request, update } => {
                let self_id = self.peers.self_id();
                if let Some(target) = input_peer_to_peer(self_id, &request.peer) {
                    self.seen.mark(&peer_id(&target), update.id);
                }
            }
            UpdatesLike::Updates(u) => {
                self.handle_tl_updates(u);
            }
            UpdatesLike::AffectedMessages(_) | UpdatesLike::ConnectionClosed => {}
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }

    pub fn handle_tl_updates(&self, updates: &tl::enums::Updates) {
        match updates {
            tl::enums::Updates::Updates(u) => {
                self.peers.remember_entities(&u.users, &u.chats);
                for entry in &u.updates {
                    self.dispatch_tl_update(entry);
                }
            }
            tl::enums::Updates::Combined(u) => {
                self.peers.remember_entities(&u.users, &u.chats);
                for entry in &u.updates {
                    self.dispatch_tl_update(entry);
                }
            }
            tl::enums::Updates::UpdateShort(u) => {
                self.dispatch_tl_update(&u.update);
            }
            tl::enums::Updates::UpdateShortMessage(s) => {
                let self_id = self.peers.self_id();
                let msg = short_dm_to_message(s, self_id);
                (self.on_message)(&msg, false);
            }
            tl::enums::Updates::UpdateShortChatMessage(s) => {
                let msg = short_chat_to_message(s);
                (self.on_message)(&msg, false);
            }
            tl::enums::Updates::UpdateShortSentMessage(s) => {
                // `ShortSentMessage` carries `request.peer` above; direct bare variant has no peer.
                let _ = s;
            }
            tl::enums::Updates::TooLong => {}
        }
    }

    pub fn dispatch_tl_update(&self, update: &tl::enums::Update) {
        let edited = is_edit_update(update);
        match update {
            tl::enums::Update::NewMessage(u) => {
                if let tl::enums::Message::Message(msg) = &u.message {
                    (self.on_message)(msg, edited);
                }
            }
            tl::enums::Update::NewChannelMessage(u) => {
                if let tl::enums::Message::Message(msg) = &u.message {
                    (self.on_message)(msg, edited);
                }
            }
            tl::enums::Update::EditMessage(u) => {
                if let tl::enums::Message::Message(msg) = &u.message {
                    (self.on_message)(msg, true);
                }
            }
            tl::enums::Update::EditChannelMessage(u) => {
                if let tl::enums::Message::Message(msg) = &u.message {
                    (self.on_message)(msg, true);
                }
            }
            _ => {}
        }
    }

    /// Runs the forwarder task that immediately handles live updates and forwards all packets
    /// to `UpdatesReceiver` via `try_send` so slow catch-up never stalls `SenderPoolRunner`.
    pub async fn run_pump(
        self: Arc<Self>,
        mut pool_updates: mpsc::Receiver<UpdatesLike>,
        receiver_tx: mpsc::Sender<UpdatesLike>,
    ) {
        let mut dropped_logged = false;
        while let Some(packet) = pool_updates.recv().await {
            self.process_live(&packet);
            match receiver_tx.try_send(packet) {
                Ok(()) => dropped_logged = false,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if !dropped_logged {
                        self.logs.record(
                            Level::Warn,
                            "updates.forward_full",
                            &[("note", "dropping packet for UpdatesReceiver catch-up queue")],
                        );
                        dropped_logged = true;
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
    }
}

fn short_dm_to_message(s: &tl::types::UpdateShortMessage, self_id: i64) -> tl::types::Message {
    let peer: tl::enums::Peer = tl::types::PeerUser { user_id: s.user_id }.into();
    let from_id = if s.out && self_id != 0 {
        Some(tl::types::PeerUser { user_id: self_id }.into())
    } else {
        Some(peer.clone())
    };
    blank_msg_from(
        s.out,
        s.mentioned,
        s.media_unread,
        s.silent,
        s.id,
        from_id,
        peer,
        s.fwd_from.clone(),
        s.via_bot_id,
        s.reply_to.clone(),
        s.date,
        s.message.clone(),
        s.entities.clone(),
        s.ttl_period,
    )
}

fn short_chat_to_message(s: &tl::types::UpdateShortChatMessage) -> tl::types::Message {
    let peer: tl::enums::Peer = tl::types::PeerChat { chat_id: s.chat_id }.into();
    let from_id = Some(tl::types::PeerUser { user_id: s.from_id }.into());
    blank_msg_from(
        s.out,
        s.mentioned,
        s.media_unread,
        s.silent,
        s.id,
        from_id,
        peer,
        s.fwd_from.clone(),
        s.via_bot_id,
        s.reply_to.clone(),
        s.date,
        s.message.clone(),
        s.entities.clone(),
        s.ttl_period,
    )
}

#[allow(clippy::too_many_arguments)]
fn blank_msg_from(
    out: bool,
    mentioned: bool,
    media_unread: bool,
    silent: bool,
    id: i32,
    from_id: Option<tl::enums::Peer>,
    peer_id: tl::enums::Peer,
    fwd_from: Option<tl::enums::MessageFwdHeader>,
    via_bot_id: Option<i64>,
    reply_to: Option<tl::enums::MessageReplyHeader>,
    date: i32,
    message: String,
    entities: Option<Vec<tl::enums::MessageEntity>>,
    ttl_period: Option<i32>,
) -> tl::types::Message {
    tl::types::Message {
        out,
        mentioned,
        media_unread,
        silent,
        post: false,
        from_scheduled: false,
        legacy: false,
        edit_hide: false,
        pinned: false,
        noforwards: false,
        invert_media: false,
        offline: false,
        video_processing_pending: false,
        paid_suggested_post_stars: false,
        paid_suggested_post_ton: false,
        id,
        from_id,
        from_boosts_applied: None,
        from_rank: None,
        peer_id,
        saved_peer_id: None,
        fwd_from,
        via_bot_id,
        via_business_bot_id: None,
        guestchat_via_from: None,
        reply_to,
        date,
        message,
        media: None,
        reply_markup: None,
        entities,
        views: None,
        forwards: None,
        replies: None,
        edit_date: None,
        post_author: None,
        grouped_id: None,
        reactions: None,
        restriction_reason: None,
        ttl_period,
        quick_reply_shortcut_id: None,
        effect: None,
        factcheck: None,
        report_delivery_until_date: None,
        paid_message_stars: None,
        suggested_post: None,
        schedule_repeat_period: None,
        summary_from_language: None,
        rich_message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn live_handler_remembers_entities_suppresses_short_sent_and_dispatches_immediately() {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(10);
        let seen = Arc::new(SeenMessages::new());
        let logs = Arc::new(Ring::new());
        let seen_hits = Arc::new(AtomicUsize::new(0));
        let seen_clone = Arc::clone(&seen_hits);
        let s_ref = Arc::clone(&seen);

        let live = LiveHandler::new(
            Arc::clone(&peers),
            Arc::clone(&seen),
            logs,
            Arc::new(move |msg, edited| {
                if edited || !s_ref.first(&peer_id(&msg.peer_id), msg.id) {
                    return false;
                }
                seen_clone.fetch_add(1, Ordering::SeqCst);
                true
            }),
        );

        let short_req = tl::functions::messages::SendMessage {
            no_webpage: false,
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            allow_paid_floodskip: false,
            peer: tl::types::InputPeerUser {
                user_id: 55,
                access_hash: 99,
            }
            .into(),
            reply_to: None,
            message: "hi".into(),
            random_id: 1,
            reply_markup: None,
            entities: None,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            allow_paid_stars: None,
            suggested_post: None,
            rich_message: None,
        };
        live.process_live(&UpdatesLike::ShortSentMessage {
            request: short_req,
            update: tl::types::UpdateShortSentMessage {
                out: true,
                id: 700,
                pts: 1,
                pts_count: 1,
                date: 100,
                media: None,
                entities: None,
                ttl_period: None,
            },
        });
        assert!(!seen.first("55", 700));

        let incoming: tl::enums::Updates = tl::enums::Updates::UpdateShortMessage(
            tl::types::UpdateShortMessage {
                out: true,
                mentioned: false,
                media_unread: false,
                silent: false,
                id: 701,
                user_id: 55,
                message: ".ping".into(),
                pts: 2,
                pts_count: 1,
                date: 1_800_000_001,
                fwd_from: None,
                via_bot_id: None,
                reply_to: None,
                entities: None,
                ttl_period: None,
            },
        );
        live.process_live(&UpdatesLike::Updates(incoming));
        assert_eq!(seen_hits.load(Ordering::SeqCst), 1);
    }
}
