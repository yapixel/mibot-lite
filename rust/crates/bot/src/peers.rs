use grammers_tl_types as tl;
use mibot_tgstate::TgState;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub const MAX_PERSIST_BATCH: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    pub id: i64,
    pub hash: i64,
    pub has_hash: bool,
    pub first_name: String,
    pub last_name: String,
    pub username: String,
    pub bot: bool,
    pub photo_id: i64,
    pub photo_dc: i32,
    pub emoji_status: String,
}

impl UserInfo {
    pub fn display_name(&self) -> String {
        let mut name = format!("{} {}", self.first_name.trim(), self.last_name.trim())
            .trim()
            .to_owned();
        if name.is_empty() {
            name = self.id.to_string();
        }
        if !self.username.is_empty() {
            name.push_str(" (@");
            name.push_str(&self.username);
            name.push(')');
        }
        name
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChannelInfo {
    pub id: i64,
    pub hash: i64,
    pub has_hash: bool,
    pub title: String,
    pub username: String,
    pub broadcast: bool,
    pub megagroup: bool,
    pub noforwards: bool,
    pub creator: bool,
    pub left: bool,
    pub admin_rights: Option<tl::types::ChatAdminRights>,
    pub photo_id: i64,
    pub photo_dc: i32,
}

#[derive(Debug, Clone, Default)]
pub struct ChatInfo {
    pub id: i64,
    pub title: String,
    pub noforwards: bool,
    pub creator: bool,
    pub left: bool,
    pub admin_rights: Option<tl::types::ChatAdminRights>,
    pub photo_id: i64,
    pub photo_dc: i32,
}

pub trait HashLookup: Send + Sync {
    fn get_channel_access_hash(&self, channel_id: i64) -> Option<i64>;
    fn get_user_access_hash(&self, target_user_id: i64) -> Option<i64>;
}

pub trait HashStore: HashLookup {
    fn set_user_access_hash(&self, target_user_id: i64, access_hash: i64);
    fn set_channel_access_hash(&self, channel_id: i64, access_hash: i64);
}

impl HashLookup for TgState {
    fn get_channel_access_hash(&self, channel_id: i64) -> Option<i64> {
        TgState::get_channel_access_hash(self, channel_id)
    }

    fn get_user_access_hash(&self, target_user_id: i64) -> Option<i64> {
        TgState::get_user_access_hash(self, target_user_id)
    }
}

impl HashStore for TgState {
    fn set_user_access_hash(&self, target_user_id: i64, access_hash: i64) {
        let _ = TgState::set_user_access_hash(self, target_user_id, access_hash);
    }

    fn set_channel_access_hash(&self, channel_id: i64, access_hash: i64) {
        let _ = TgState::set_channel_access_hash(self, channel_id, access_hash);
    }
}

#[derive(Default)]
struct CacheInner {
    users: HashMap<i64, UserInfo>,
    channels: HashMap<i64, ChannelInfo>,
    chats: HashMap<i64, ChatInfo>,
    self_id: i64,
    durable: Option<Arc<dyn HashLookup>>,
    writable: Option<Arc<dyn HashStore>>,
}

struct PendingHash {
    id: i64,
    hash: i64,
    channel: bool,
}

/// In-memory + durable write-through cache for Telegram users, chats, and channels.
/// Never guesses `access_hash` or trusts `min` entities' hashes.
#[derive(Default)]
pub struct PeerCache {
    inner: RwLock<CacheInner>,
}

impl PeerCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_self(&self, id: i64) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .self_id = id;
    }

    pub fn self_id(&self) -> i64 {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .self_id
    }

    pub fn set_durable_lookup(&self, lookup: Arc<dyn HashLookup>) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.durable = Some(lookup);
        guard.writable = None;
    }

    pub fn set_durable_store(&self, store: Arc<dyn HashStore>) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.durable = Some(store.clone());
        guard.writable = Some(store);
    }

    pub fn remember_entities(&self, users: &[tl::enums::User], chats: &[tl::enums::Chat]) {
        let (pending, writable) = {
            let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
            let mut pending = Vec::new();
            for u in users {
                if let tl::enums::User::User(user) = u
                    && let Some(p) = remember_user_locked(&mut guard.users, user)
                {
                    pending.push(p);
                }
            }
            for c in chats {
                if let Some(p) = remember_chat_enum_locked(&mut guard, c) {
                    pending.push(p);
                }
            }
            (pending, guard.writable.clone())
        };
        persist_hashes(writable.as_deref(), &pending);
    }

    pub fn remember_users(&self, users: &[tl::enums::User]) {
        self.remember_entities(users, &[]);
    }

    pub fn remember_chats(&self, chats: &[tl::enums::Chat]) {
        self.remember_entities(&[], chats);
    }

    pub fn user(&self, id: i64) -> Option<UserInfo> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .users
            .get(&id)
            .cloned()
    }

    pub fn channel(&self, id: i64) -> Option<ChannelInfo> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .channels
            .get(&id)
            .cloned()
    }

    pub fn chat(&self, id: i64) -> Option<ChatInfo> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .chats
            .get(&id)
            .cloned()
    }

    pub fn title(&self, peer: &tl::enums::Peer) -> String {
        match peer {
            tl::enums::Peer::User(u) => {
                if let Some(info) = self.user(u.user_id) {
                    let name = format!("{} {}", info.first_name.trim(), info.last_name.trim())
                        .trim()
                        .to_owned();
                    if !name.is_empty() {
                        return name;
                    }
                }
                u.user_id.to_string()
            }
            tl::enums::Peer::Chat(c) => match self.chat(c.chat_id) {
                Some(info) if !info.title.is_empty() => info.title,
                _ => format!("-{}", c.chat_id),
            },
            tl::enums::Peer::Channel(ch) => match self.channel(ch.channel_id) {
                Some(info) if !info.title.is_empty() => info.title,
                _ => format!("-100{}", ch.channel_id),
            },
        }
    }

    pub fn input_peer(&self, peer: &tl::enums::Peer) -> Option<tl::enums::InputPeer> {
        let (self_id, durable, direct) = {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            let direct = match peer {
                tl::enums::Peer::User(u) => {
                    if guard.self_id != 0 && u.user_id == guard.self_id {
                        Some(tl::enums::InputPeer::PeerSelf)
                    } else {
                        guard
                            .users
                            .get(&u.user_id)
                            .filter(|info| info.has_hash)
                            .map(|info| {
                                tl::types::InputPeerUser {
                                    user_id: u.user_id,
                                    access_hash: info.hash,
                                }
                                .into()
                            })
                    }
                }
                tl::enums::Peer::Chat(c) => Some(
                    tl::types::InputPeerChat { chat_id: c.chat_id }.into(),
                ),
                tl::enums::Peer::Channel(ch) => guard
                    .channels
                    .get(&ch.channel_id)
                    .filter(|info| info.has_hash)
                    .map(|info| {
                        tl::types::InputPeerChannel {
                            channel_id: ch.channel_id,
                            access_hash: info.hash,
                        }
                        .into()
                    }),
            };
            (guard.self_id, guard.durable.clone(), direct)
        };
        if direct.is_some() {
            return direct;
        }
        let lookup = durable?;
        if self_id == 0 {
            return None;
        }
        match peer {
            tl::enums::Peer::User(u) => lookup.get_user_access_hash(u.user_id).map(|hash| {
                tl::types::InputPeerUser {
                    user_id: u.user_id,
                    access_hash: hash,
                }
                .into()
            }),
            tl::enums::Peer::Channel(ch) => {
                lookup.get_channel_access_hash(ch.channel_id).map(|hash| {
                    tl::types::InputPeerChannel {
                        channel_id: ch.channel_id,
                        access_hash: hash,
                    }
                    .into()
                })
            }
            tl::enums::Peer::Chat(_) => None,
        }
    }
}

fn persist_hashes(writable: Option<&dyn HashStore>, pending: &[PendingHash]) {
    let Some(store) = writable else {
        return;
    };
    if pending.is_empty() || pending.len() > MAX_PERSIST_BATCH {
        return;
    }
    for item in pending {
        if item.channel {
            store.set_channel_access_hash(item.id, item.hash);
        } else {
            store.set_user_access_hash(item.id, item.hash);
        }
    }
}

fn remember_user_locked(
    users: &mut HashMap<i64, UserInfo>,
    user: &tl::types::User,
) -> Option<PendingHash> {
    let info = users.entry(user.id).or_insert_with(|| UserInfo {
        id: user.id,
        ..Default::default()
    });
    info.first_name = user.first_name.clone().unwrap_or_default();
    info.last_name = user.last_name.clone().unwrap_or_default();
    info.username = user.username.clone().unwrap_or_default();
    info.bot = user.bot;
    if let Some(tl::enums::UserProfilePhoto::Photo(photo)) = &user.photo {
        info.photo_id = photo.photo_id;
        info.photo_dc = photo.dc_id;
    }
    if let Some(tl::enums::EmojiStatus::Status(status)) = &user.emoji_status
        && status.document_id != 0
    {
        info.emoji_status = status.document_id.to_string();
    }
    if let Some(hash) = user.access_hash.filter(|&h| h != 0)
        && !user.min
    {
        let fresh = !info.has_hash || info.hash != hash;
        info.hash = hash;
        info.has_hash = true;
        if fresh {
            return Some(PendingHash {
                id: user.id,
                hash,
                channel: false,
            });
        }
    }
    None
}

fn remember_chat_enum_locked(
    guard: &mut CacheInner,
    chat: &tl::enums::Chat,
) -> Option<PendingHash> {
    match chat {
        tl::enums::Chat::Chat(c) => {
            remember_chat_locked(&mut guard.chats, c);
            None
        }
        tl::enums::Chat::Channel(ch) => remember_channel_locked(&mut guard.channels, ch),
        tl::enums::Chat::ChannelForbidden(ch) => {
            let info = guard.channels.entry(ch.id).or_insert_with(|| ChannelInfo {
                id: ch.id,
                ..Default::default()
            });
            info.title = ch.title.clone();
            info.broadcast = ch.broadcast;
            info.megagroup = ch.megagroup;
            if ch.access_hash != 0 {
                let fresh = !info.has_hash || info.hash != ch.access_hash;
                info.hash = ch.access_hash;
                info.has_hash = true;
                if fresh {
                    return Some(PendingHash {
                        id: ch.id,
                        hash: ch.access_hash,
                        channel: true,
                    });
                }
            }
            None
        }
        _ => None,
    }
}

fn remember_channel_locked(
    channels: &mut HashMap<i64, ChannelInfo>,
    ch: &tl::types::Channel,
) -> Option<PendingHash> {
    let info = channels.entry(ch.id).or_insert_with(|| ChannelInfo {
        id: ch.id,
        ..Default::default()
    });
    info.title = ch.title.clone();
    info.username = ch.username.clone().unwrap_or_default();
    if let tl::enums::ChatPhoto::Photo(photo) = &ch.photo {
        info.photo_id = photo.photo_id;
        info.photo_dc = photo.dc_id;
    }
    info.broadcast = ch.broadcast;
    info.megagroup = ch.megagroup;
    info.noforwards = ch.noforwards;
    info.left = ch.left;
    if !ch.min {
        info.creator = ch.creator;
        info.admin_rights = ch.admin_rights.as_ref().map(|r| match r {
            tl::enums::ChatAdminRights::Rights(rights) => rights.clone(),
        });
    }
    if let Some(hash) = ch.access_hash.filter(|&h| h != 0)
        && !ch.min
    {
        let fresh = !info.has_hash || info.hash != hash;
        info.hash = hash;
        info.has_hash = true;
        if fresh {
            return Some(PendingHash {
                id: ch.id,
                hash,
                channel: true,
            });
        }
    }
    None
}

fn remember_chat_locked(chats: &mut HashMap<i64, ChatInfo>, c: &tl::types::Chat) {
    let info = chats.entry(c.id).or_insert_with(|| ChatInfo {
        id: c.id,
        ..Default::default()
    });
    info.title = c.title.clone();
    info.noforwards = c.noforwards;
    info.creator = c.creator;
    info.left = c.left;
    info.admin_rights = c.admin_rights.as_ref().map(|r| match r {
        tl::enums::ChatAdminRights::Rights(rights) => rights.clone(),
    });
    if let tl::enums::ChatPhoto::Photo(photo) = &c.photo {
        info.photo_id = photo.photo_id;
        info.photo_dc = photo.dc_id;
    }
}

/// Formats a peer using Teleproto's signed-prefix decimal format (`user`, `-chat`, `-100channel`).
pub fn peer_id(peer: &tl::enums::Peer) -> String {
    match peer {
        tl::enums::Peer::User(u) => u.user_id.to_string(),
        tl::enums::Peer::Chat(c) => format!("-{}", c.chat_id),
        tl::enums::Peer::Channel(ch) => format!("-100{}", ch.channel_id),
    }
}

/// Parses a signed-prefix decimal peer ID back into a `tl::enums::Peer`.
pub fn peer_from_id(id: &str) -> Option<tl::enums::Peer> {
    if id.is_empty() {
        return None;
    }
    if let Some(rest) = id.strip_prefix("-100") {
        let value: i64 = rest.parse().ok()?;
        if value <= 0 {
            return None;
        }
        return Some(tl::types::PeerChannel { channel_id: value }.into());
    }
    let value: i64 = id.parse().ok()?;
    if value == 0 {
        return None;
    }
    if value < 0 {
        Some(tl::types::PeerChat { chat_id: -value }.into())
    } else {
        Some(tl::types::PeerUser { user_id: value }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        mu: Mutex<FakeStoreData>,
    }

    #[derive(Default)]
    struct FakeStoreData {
        users: HashMap<i64, i64>,
        channels: HashMap<i64, i64>,
        user_writes: usize,
        channel_writes: usize,
    }

    impl HashLookup for FakeStore {
        fn get_channel_access_hash(&self, channel_id: i64) -> Option<i64> {
            self.mu.lock().unwrap().channels.get(&channel_id).copied()
        }

        fn get_user_access_hash(&self, target_user_id: i64) -> Option<i64> {
            self.mu.lock().unwrap().users.get(&target_user_id).copied()
        }
    }

    impl HashStore for FakeStore {
        fn set_user_access_hash(&self, target_user_id: i64, access_hash: i64) {
            let mut g = self.mu.lock().unwrap();
            g.users.insert(target_user_id, access_hash);
            g.user_writes += 1;
        }

        fn set_channel_access_hash(&self, channel_id: i64, access_hash: i64) {
            let mut g = self.mu.lock().unwrap();
            g.channels.insert(channel_id, access_hash);
            g.channel_writes += 1;
        }
    }

    fn mk_user(id: i64, hash: Option<i64>, min: bool) -> tl::enums::User {
        tl::types::User {
            is_self: false,
            contact: false,
            mutual_contact: false,
            deleted: false,
            bot: false,
            bot_chat_history: false,
            bot_nochats: false,
            verified: false,
            restricted: false,
            min,
            bot_inline_geo: false,
            support: false,
            scam: false,
            apply_min_photo: false,
            fake: false,
            bot_attach_menu: false,
            premium: false,
            attach_menu_enabled: false,
            bot_can_edit: false,
            close_friend: false,
            stories_hidden: false,
            stories_unavailable: false,
            contact_require_premium: false,
            bot_business: false,
            bot_has_main_app: false,
            bot_forum_view: false,
            bot_forum_can_manage_topics: false,
            bot_can_manage_bots: false,
            bot_guestchat: false,
            bot_guard: false,
            bot_forum_sidebar: false,
            id,
            access_hash: hash,
            first_name: None,
            last_name: None,
            username: None,
            phone: None,
            photo: None,
            status: None,
            bot_info_version: None,
            restriction_reason: None,
            bot_inline_placeholder: None,
            lang_code: None,
            emoji_status: None,
            usernames: None,
            stories_max_id: None,
            color: None,
            profile_color: None,
            bot_active_users: None,
            bot_verification_icon: None,
            send_paid_messages_stars: None,
            linked_community_id: None,
        }
        .into()
    }

    fn mk_channel(id: i64, hash: Option<i64>, min: bool) -> tl::enums::Chat {
        tl::types::Channel {
            creator: false,
            left: false,
            broadcast: false,
            verified: false,
            megagroup: false,
            restricted: false,
            signatures: false,
            min,
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
            id,
            access_hash: hash,
            title: String::new(),
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
        .into()
    }

    #[test]
    fn write_through_and_dedup_and_bulk_limit() {
        let store = Arc::new(FakeStore::default());
        let peers = PeerCache::new();
        peers.set_self(1);
        peers.set_durable_store(store.clone());

        peers.remember_users(&[
            mk_user(42, Some(7), false),
            mk_user(43, None, false),
            mk_user(44, Some(9), true),
        ]);
        assert_eq!(store.get_user_access_hash(42), Some(7));
        assert_eq!(store.get_user_access_hash(43), None);
        assert_eq!(store.get_user_access_hash(44), None);
        assert_eq!(store.mu.lock().unwrap().user_writes, 1);

        // Same hash again does not re-write.
        peers.remember_users(&[mk_user(42, Some(7), false)]);
        assert_eq!(store.mu.lock().unwrap().user_writes, 1);

        // Changed hash writes again.
        peers.remember_users(&[mk_user(42, Some(8), false)]);
        assert_eq!(store.get_user_access_hash(42), Some(8));
        assert_eq!(store.mu.lock().unwrap().user_writes, 2);

        // Bulk > 32 stays in memory only.
        let bulk: Vec<_> = (100..100 + MAX_PERSIST_BATCH as i64 + 1)
            .map(|id| mk_user(id, Some(id * 3), false))
            .collect();
        peers.remember_users(&bulk);
        assert_eq!(store.mu.lock().unwrap().user_writes, 2);
        assert!(peers.input_peer(&tl::types::PeerUser { user_id: 100 }.into()).is_some());

        // Channel + ChannelForbidden.
        peers.remember_chats(&[
            mk_channel(55, Some(11), false),
            mk_channel(56, Some(12), true),
            tl::types::ChannelForbidden {
                broadcast: false,
                megagroup: false,
                monoforum: false,
                id: 57,
                access_hash: 13,
                title: String::new(),
                until_date: None,
            }
            .into(),
        ]);
        assert_eq!(store.get_channel_access_hash(55), Some(11));
        assert_eq!(store.get_channel_access_hash(56), None);
        assert_eq!(store.get_channel_access_hash(57), Some(13));
    }

    #[test]
    fn peer_id_round_trip_and_display_name() {
        for raw in ["100100100", "-5115533475", "-1001234567890"] {
            let parsed = peer_from_id(raw).unwrap();
            assert_eq!(peer_id(&parsed), raw);
        }
        for bad in ["", "abc", "0", "-100"] {
            assert!(peer_from_id(bad).is_none());
        }
        let u = UserInfo {
            id: 5,
            first_name: "Ada".into(),
            last_name: "L".into(),
            username: "ada".into(),
            ..Default::default()
        };
        assert_eq!(u.display_name(), "Ada L (@ada)");
        assert_eq!(
            UserInfo {
                id: 5,
                ..Default::default()
            }
            .display_name(),
            "5"
        );
    }

    #[test]
    fn lookup_only_and_unknown_hash_refusal() {
        struct ReadOnly;
        impl HashLookup for ReadOnly {
            fn get_channel_access_hash(&self, _: i64) -> Option<i64> {
                None
            }
            fn get_user_access_hash(&self, _: i64) -> Option<i64> {
                None
            }
        }
        let peers = PeerCache::new();
        peers.set_self(7);
        peers.set_durable_lookup(Arc::new(ReadOnly));
        assert!(peers.input_peer(&tl::types::PeerChannel { channel_id: 123 }.into()).is_none());
        assert!(peers.input_peer(&tl::types::PeerChat { chat_id: 55 }.into()).is_some());
    }
}
