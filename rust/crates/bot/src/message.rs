use crate::peers::{PeerCache, peer_id};
use crate::rich::rich_message_text;
use grammers_tl_types as tl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatType {
    Private,
    Group,
    Supergroup,
    Broadcast,
}

impl ChatType {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatType::Private => "private",
            ChatType::Group => "group",
            ChatType::Supergroup => "supergroup",
            ChatType::Broadcast => "broadcast",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: i32,
    pub peer: tl::enums::Peer,
    pub chat_id: String,
    pub chat_type: ChatType,
    pub text: String,
    pub out: bool,
    pub edited: bool,
    pub forward: bool,
    pub post: bool,
    pub saved: bool,
    pub sender: tl::enums::Peer,
    pub reply_to_id: i32,
    pub reply_to_peer: Option<tl::enums::Peer>,
    pub topic_id: i32,
    pub quote_text: String,
    pub quote_entities: Vec<tl::enums::MessageEntity>,
    pub raw: tl::types::Message,
}

impl Message {
    /// Returns visible text: plain `text` if non-empty, otherwise flattened rich message text.
    /// Command parsing reads `text` directly so rich blocks never trigger commands.
    pub fn content(&self) -> String {
        if !self.text.is_empty() {
            return self.text.clone();
        }
        rich_message_text(&self.raw)
    }

    pub fn sender_id(&self) -> i64 {
        match &self.sender {
            tl::enums::Peer::User(u) => u.user_id,
            tl::enums::Peer::Channel(ch) => ch.channel_id,
            tl::enums::Peer::Chat(c) => c.chat_id,
        }
    }

    pub fn sender_is_user(&self, id: i64) -> bool {
        matches!(&self.sender, tl::enums::Peer::User(u) if u.user_id == id)
    }

    pub fn is_group(&self) -> bool {
        matches!(self.chat_type, ChatType::Group | ChatType::Supergroup)
    }

    pub fn channel_id(&self) -> Option<i64> {
        match &self.peer {
            tl::enums::Peer::Channel(ch) => Some(ch.channel_id),
            _ => None,
        }
    }
}

pub fn envelope(
    message: &tl::types::Message,
    self_id: i64,
    edited: bool,
    peers: &PeerCache,
) -> Option<Message> {
    let chat_id = peer_id(&message.peer_id);
    if chat_id.is_empty() {
        return None;
    }

    let mut saved = message.saved_peer_id.is_some();
    let chat_type = match &message.peer_id {
        tl::enums::Peer::User(u) => {
            if u.user_id == self_id {
                saved = true;
            }
            ChatType::Private
        }
        tl::enums::Peer::Chat(_) => ChatType::Group,
        tl::enums::Peer::Channel(ch) => {
            if let Some(info) = peers.channel(ch.channel_id) {
                if info.broadcast {
                    ChatType::Broadcast
                } else {
                    ChatType::Supergroup
                }
            } else if message.post {
                ChatType::Broadcast
            } else {
                ChatType::Supergroup
            }
        }
    };

    let sender = if let Some(from) = &message.from_id {
        from.clone()
    } else if message.out {
        tl::types::PeerUser { user_id: self_id }.into()
    } else {
        message.peer_id.clone()
    };

    let mut out = message.out;
    if saved && matches!(&sender, tl::enums::Peer::User(u) if u.user_id == self_id) {
        out = true;
    }

    let is_edited = edited || message.edit_date.is_some_and(|d| d != 0);
    let is_forward = message.fwd_from.is_some();

    let mut reply_to_id = 0;
    let mut reply_to_peer = None;
    let mut topic_id = 0;
    let mut quote_text = String::new();
    let mut quote_entities = Vec::new();

    if let Some(tl::enums::MessageReplyHeader::Header(reply)) = &message.reply_to {
        reply_to_id = reply.reply_to_msg_id.unwrap_or(0);
        reply_to_peer = reply.reply_to_peer_id.clone();
        if let Some(top) = reply.reply_to_top_id {
            topic_id = top;
        } else if reply.forum_topic {
            topic_id = reply_to_id;
        }
        if reply.quote {
            quote_text = reply.quote_text.clone().unwrap_or_default();
            quote_entities = reply.quote_entities.clone().unwrap_or_default();
        }
    }

    Some(Message {
        id: message.id,
        peer: message.peer_id.clone(),
        chat_id,
        chat_type,
        text: message.message.clone(),
        out,
        edited: is_edited,
        forward: is_forward,
        post: message.post,
        saved,
        sender,
        reply_to_id,
        reply_to_peer,
        topic_id,
        quote_text,
        quote_entities,
        raw: message.clone(),
    })
}

#[cfg(test)]
pub(crate) use tests::blank_tl_message;

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn blank_tl_message(id: i32, peer: tl::enums::Peer) -> tl::types::Message {
        tl::types::Message {
            out: false,
            mentioned: false,
            media_unread: false,
            silent: false,
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
            from_id: None,
            from_boosts_applied: None,
            from_rank: None,
            peer_id: peer,
            saved_peer_id: None,
            fwd_from: None,
            via_bot_id: None,
            via_business_bot_id: None,
            guestchat_via_from: None,
            reply_to: None,
            date: 0,
            message: String::new(),
            media: None,
            reply_markup: None,
            entities: None,
            views: None,
            forwards: None,
            replies: None,
            edit_date: None,
            post_author: None,
            grouped_id: None,
            reactions: None,
            restriction_reason: None,
            ttl_period: None,
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

    #[test]
    fn envelope_classifies_chats_and_saved_and_quotes() {
        let peers = PeerCache::new();
        peers.set_self(7);
        peers.remember_chats(&[tl::types::Channel {
            creator: false,
            left: false,
            broadcast: true,
            verified: false,
            megagroup: false,
            restricted: false,
            signatures: false,
            min: false,
            scam: false,
            has_link: false,
            has_geo: false,
            slowmode_enabled: false,
            call_active: false,
            call_not_empty: false,
            fake: false,
            gigagroup: false,
            noforwards: false,
            join_to_send: false,
            join_request: false,
            forum: false,
            stories_hidden: false,
            stories_hidden_min: false,
            stories_unavailable: false,
            signature_profiles: false,
            autotranslation: false,
            broadcast_messages_allowed: false,
            monoforum: false,
            forum_tabs: false,
            in_news: false,
            forum_sidebar: false,
            id: 100,
            access_hash: Some(5),
            title: "news".into(),
            username: None,
            photo: tl::enums::ChatPhoto::Empty,
            date: 0,
            restriction_reason: None,
            admin_rights: None,
            banned_rights: None,
            default_banned_rights: None,
            participants_count: None,
            usernames: None,
            stories_max_id: None,
            color: None,
            profile_color: None,
            emoji_status: None,
            level: None,
            subscription_until_date: None,
            bot_verification_icon: None,
            send_paid_messages_stars: None,
            linked_monoforum_id: None,
            linked_community_id: None,
            pinned_msg_id: None,
        }
        .into()]);

        // Saved Messages without Out flag set by Telegram still marks Saved & Out.
        let mut saved_msg = blank_tl_message(1, tl::types::PeerUser { user_id: 7 }.into());
        saved_msg.message = ".ping".into();
        let env = envelope(&saved_msg, 7, false, &peers).unwrap();
        assert_eq!(env.chat_type, ChatType::Private);
        assert!(env.saved);
        assert!(env.out);
        assert_eq!(env.sender_id(), 7);

        // Other user's DM stays incoming.
        let mut incoming = blank_tl_message(4, tl::types::PeerUser { user_id: 99 }.into());
        incoming.from_id = Some(tl::types::PeerUser { user_id: 99 }.into());
        let env_in = envelope(&incoming, 7, false, &peers).unwrap();
        assert!(!env_in.out);
        assert!(!env_in.saved);
        assert_eq!(env_in.sender_id(), 99);

        // Broadcast channel
        let mut post = blank_tl_message(2, tl::types::PeerChannel { channel_id: 100 }.into());
        post.post = true;
        assert_eq!(
            envelope(&post, 7, false, &peers).unwrap().chat_type,
            ChatType::Broadcast
        );

        // Reply quote + forum topic + edit date
        let mut msg = blank_tl_message(9, tl::types::PeerChannel { channel_id: 100 }.into());
        msg.reply_to = Some(
            tl::types::MessageReplyHeader {
                reply_to_scheduled: false,
                forum_topic: true,
                quote: true,
                reply_to_ephemeral: false,
                reply_to_msg_id: Some(4),
                reply_to_peer_id: None,
                reply_from: None,
                reply_media: None,
                reply_to_top_id: None,
                quote_text: Some("只要这一段".into()),
                quote_entities: Some(vec![
                    tl::types::MessageEntityBold {
                        offset: 0,
                        length: 2,
                    }
                    .into(),
                ]),
                quote_offset: None,
                todo_item_id: None,
                poll_option: None,
                quote_rich: None,
            }
            .into(),
        );
        msg.edit_date = Some(1_700_000_000);
        let env_q = envelope(&msg, 7, false, &peers).unwrap();
        assert_eq!(env_q.reply_to_id, 4);
        assert_eq!(env_q.topic_id, 4);
        assert!(env_q.edited);
        assert_eq!(env_q.quote_text, "只要这一段");
        assert_eq!(env_q.quote_entities.len(), 1);
    }
}
