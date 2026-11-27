use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BotError, Message, escape, utf16_len};
use mibot_command::{CmdError, Invocation};
use mibot_store::{Store, StoreError, UpdateError};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub use mibot_command::{fail, fail as fail_err, fail_with, fail_with as fail_with_err, is_user_error};

pub fn wrap_fail(msg: &str, err: CmdError) -> CmdError {
    if is_user_error(err.as_ref()) {
        return err;
    }
    let arc: Arc<dyn std::error::Error + Send + Sync> = Arc::from(err);
    let label = format!("{msg}（{}）", mibot_command::brief(arc.as_ref()));
    Box::new(mibot_command::UserError {
        text: label,
        cause: Some(arc),
    })
}

pub fn usage(prefix: &str, spec: &str) -> CmdError {
    fail(format!("用法：{prefix}{spec}"))
}

pub fn feedback(state: &str, title: &str, detail: &str) -> String {
    let icon = match state {
        "success" => "✅",
        "error" => "❌",
        _ => "⏳",
    };
    if detail.is_empty() {
        format!("{icon} {}", escape(title))
    } else {
        format!("{icon} {}\n{}", escape(title), escape(detail))
    }
}

pub fn working(text: &str) -> String {
    let clean = text.strip_suffix('…').unwrap_or(text);
    format!("⏳ {clean}…")
}

pub fn data_path(app: &App, name: &str) -> PathBuf {
    app.data_dir().join(name)
}

pub fn new_store<T, F>(app: &App, name: &str, defaults: F) -> Store<T>
where
    T: Clone + Serialize + for<'de> Deserialize<'de>,
    F: Fn() -> T + Send + Sync + 'static,
{
    Store::new(data_path(app, name), defaults)
}

pub fn warn(app: &App, event: &str, err: &dyn std::error::Error) {
    app.warn(event, &err.to_string());
}

pub async fn reply_msg(inv: &Invocation) -> Result<Option<Message>, CmdError> {
    if inv.message.reply_to_id == 0 {
        return Ok(None);
    }
    match inv.get_reply().await {
        Ok(Some(m)) => Ok(Some(m)),
        Ok(None) => Err(fail("被回复的消息已经不在了")),
        Err(e) => Err(fail_with("读不到被回复的消息", e)),
    }
}

pub fn on_off(value: &str) -> Result<bool, CmdError> {
    match value.to_ascii_lowercase().as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(fail("请输入 on 或 off")),
    }
}

pub fn on_off_text(value: bool) -> &'static str {
    if value { "开启" } else { "关闭" }
}

pub async fn sleep(cancel: &CancellationToken, d: Duration) -> bool {
    if d.is_zero() {
        return !cancel.is_cancelled();
    }
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(d) => true,
    }
}

/// Retries `op` up to `attempts` times on transient errors or arbitrarily long `FLOOD_WAIT`
/// (used by bulk-delete `.dme` where rate limits can exceed `Retrier`'s 60s cap).
pub async fn retry_flood<F, Fut, T>(
    cancel: &CancellationToken,
    attempts: usize,
    mut op: F,
) -> Result<T, BotError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, BotError>>,
{
    let mut attempt = 0usize;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if cancel.is_cancelled() || attempt >= attempts {
                    return Err(err);
                }
                let wait = if let Some(flood) = err.flood_wait() {
                    flood + Duration::from_secs(1)
                } else {
                    Duration::from_secs(2 * (attempt as u64 + 1))
                };
                attempt += 1;
                if !sleep(cancel, wait).await {
                    return Err(BotError::Cancelled);
                }
            }
        }
    }
}

pub fn utf16_slice(text: &str, offset: i32, length: i32) -> String {
    if offset < 0 || length <= 0 {
        return String::new();
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let start = offset as usize;
    if start >= units.len() {
        return String::new();
    }
    let end = start.saturating_add(length as usize).min(units.len());
    String::from_utf16_lossy(&units[start..end])
}

pub fn utf16_len_usize(text: &str) -> usize {
    utf16_len(text).max(0) as usize
}

pub fn peer_key(peer: &tl::enums::Peer) -> String {
    match peer {
        tl::enums::Peer::User(u) => format!("userId:{}", u.user_id),
        tl::enums::Peer::Channel(c) => format!("channelId:{}", c.channel_id),
        tl::enums::Peer::Chat(c) => format!("chatId:{}", c.chat_id),
    }
}

pub fn truncate_runes(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    match text.char_indices().nth(n) {
        Some((idx, _)) => text[..idx].to_owned(),
        None => text.to_owned(),
    }
}

pub fn message_id(item: &tl::enums::Message) -> i32 {
    match item {
        tl::enums::Message::Message(m) => m.id,
        tl::enums::Message::Service(m) => m.id,
        tl::enums::Message::Empty(m) => m.id,
    }
}

pub fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

pub fn is_numeric_id(value: &str) -> bool {
    is_digits(value.strip_prefix('-').unwrap_or(value))
}

pub fn clamp(value: i64, low: i64, high: i64) -> i64 {
    value.clamp(low, high)
}

pub fn or_default<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

pub fn or_dash(value: &str) -> &str {
    if value.trim().is_empty() {
        "—"
    } else {
        value
    }
}

pub fn format_bytes(size: i64) -> String {
    mibot_sysinfo::format_bytes(size.max(0) as u64)
}

pub fn version(app: &App) -> &str {
    if app.version.is_empty() {
        "未知"
    } else {
        &app.version
    }
}

pub fn input_channel(peer: &tl::enums::InputPeer) -> Option<tl::types::InputChannel> {
    match peer {
        tl::enums::InputPeer::Channel(ch) => Some(tl::types::InputChannel {
            channel_id: ch.channel_id,
            access_hash: ch.access_hash,
        }),
        _ => None,
    }
}

pub fn input_user(peer: &tl::enums::InputPeer) -> Option<tl::enums::InputUser> {
    match peer {
        tl::enums::InputPeer::PeerSelf => Some(tl::enums::InputUser::UserSelf),
        tl::enums::InputPeer::User(u) => Some(
            tl::types::InputUser {
                user_id: u.user_id,
                access_hash: u.access_hash,
            }
            .into(),
        ),
        _ => None,
    }
}

pub fn unpack_messages(res: tl::enums::messages::Messages) -> Vec<tl::enums::Message> {
    match res {
        tl::enums::messages::Messages::Messages(m) => m.messages,
        tl::enums::messages::Messages::Slice(m) => m.messages,
        tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
        tl::enums::messages::Messages::NotModified(_) => Vec::new(),
    }
}

pub async fn send_self(client: &mibot_bot::Client, html: &str) -> Result<i32, BotError> {
    let updates = client
        .send_html(
            &tl::enums::InputPeer::PeerSelf,
            html,
            mibot_bot::SendOptions {
                no_webpage: true,
                ..Default::default()
            },
        )
        .await?;
    Ok(mibot_bot::first_sent_message_id(&updates).unwrap_or(0))
}

pub fn default_banned_rights() -> tl::types::ChatBannedRights {
    tl::types::ChatBannedRights {
        view_messages: false,
        send_messages: false,
        send_media: false,
        send_stickers: false,
        send_gifs: false,
        send_games: false,
        send_inline: false,
        embed_links: false,
        send_polls: false,
        change_info: false,
        invite_users: false,
        pin_messages: false,
        manage_topics: false,
        send_photos: false,
        send_videos: false,
        send_roundvideos: false,
        send_audios: false,
        send_voices: false,
        send_docs: false,
        send_plain: false,
        edit_rank: false,
        send_reactions: false,
        manage_linked_peers: false,
        until_date: 0,
    }
}

pub fn ban_rights(until_date: i32) -> tl::types::ChatBannedRights {
    tl::types::ChatBannedRights {
        view_messages: true,
        send_messages: true,
        send_media: true,
        send_stickers: true,
        send_gifs: true,
        send_games: true,
        send_inline: true,
        embed_links: true,
        until_date,
        ..default_banned_rights()
    }
}

pub fn mute_rights(until_date: i32) -> tl::types::ChatBannedRights {
    tl::types::ChatBannedRights {
        send_messages: true,
        send_media: true,
        send_stickers: true,
        send_gifs: true,
        send_games: true,
        send_inline: true,
        embed_links: true,
        send_polls: true,
        send_photos: true,
        send_videos: true,
        send_roundvideos: true,
        send_audios: true,
        send_voices: true,
        send_docs: true,
        send_plain: true,
        until_date,
        ..default_banned_rights()
    }
}

pub fn update_ok<T, F>(store: &Store<T>, mutate: F) -> Result<(), StoreError>
where
    T: Clone + Serialize + for<'de> Deserialize<'de>,
    F: FnOnce(&mut T),
{
    match store.update(|doc| {
        mutate(doc);
        Ok::<(), std::convert::Infallible>(())
    }) {
        Ok(()) => Ok(()),
        Err(UpdateError::Store(e)) => Err(e),
        Err(UpdateError::Mutate(inf)) => match inf {},
    }
}

pub fn update_read<T, F>(store: &Store<T>, mutate: F) -> Result<T, StoreError>
where
    T: Clone + Serialize + for<'de> Deserialize<'de>,
    F: FnOnce(&mut T) -> Result<(), StoreError>,
{
    match store.update(mutate) {
        Ok(()) => store.read(),
        Err(UpdateError::Store(e)) | Err(UpdateError::Mutate(e)) => Err(e),
    }
}

#[cfg(test)]
pub fn default_admin_rights() -> tl::types::ChatAdminRights {
    tl::types::ChatAdminRights {
        change_info: false,
        post_messages: false,
        edit_messages: false,
        delete_messages: false,
        ban_users: false,
        invite_users: false,
        pin_messages: false,
        add_admins: false,
        anonymous: false,
        manage_call: false,
        other: false,
        manage_topics: false,
        post_stories: false,
        edit_stories: false,
        delete_stories: false,
        manage_direct_messages: false,
        manage_ranks: false,
        manage_linked_peers: false,
        manage_welcome_messages: false,
    }
}

pub fn opt_log(
    logs: Option<&Arc<mibot_logtail::Ring>>,
    level: mibot_logtail::Level,
    event: &str,
    fields: &[(&str, &str)],
) {
    if let Some(ring) = logs {
        ring.record(level, event, fields);
    }
}

pub fn blank_tl_message(id: i32, peer: tl::enums::Peer) -> tl::types::Message {
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

#[cfg(test)]
pub fn blank_tl_user(id: i64) -> tl::types::User {
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
        min: false,
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
        access_hash: None,
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
}

#[cfg(test)]
pub fn blank_tl_channel(id: i64, title: &str) -> tl::types::Channel {
    tl::types::Channel {
        creator: false,
        left: false,
        broadcast: false,
        verified: false,
        megagroup: true,
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
        id,
        access_hash: None,
        title: title.to_owned(),
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
}

#[cfg(test)]
pub fn blank_bot_message(id: i32, peer: tl::enums::Peer) -> Message {
    Message {
        id,
        chat_id: mibot_bot::peer_id(&peer),
        chat_type: mibot_bot::ChatType::Private,
        text: String::new(),
        out: false,
        edited: false,
        forward: false,
        post: false,
        saved: false,
        sender: tl::types::PeerUser { user_id: 1 }.into(),
        reply_to_id: 0,
        reply_to_peer: None,
        topic_id: 0,
        quote_text: String::new(),
        quote_entities: Vec::new(),
        raw: blank_tl_message(id, peer.clone()),
        peer,
    }
}

#[cfg(test)]
pub fn tl_messages(
    messages: Vec<tl::enums::Message>,
    chats: Vec<tl::enums::Chat>,
    users: Vec<tl::enums::User>,
) -> tl::enums::messages::Messages {
    tl::types::messages::Messages {
        messages,
        topics: vec![],
        chats,
        users,
    }
    .into()
}

/// Ensures `cmd` has consistent defaults wrapped in `Arc` when registering.
pub fn make_cmd(
    name: &str,
    aliases: &[&str],
    group: &str,
    description: &str,
    usage: &str,
    help_fn: impl Fn(&str) -> String + Send + Sync + 'static,
    handle: impl Fn(Invocation) -> mibot_bot::BoxFuture<'static, mibot_command::CmdResult>
        + Send
        + Sync
        + 'static,
) -> mibot_command::Command {
    mibot_command::Command {
        name: name.to_owned(),
        aliases: aliases.iter().map(|s| (*s).to_owned()).collect(),
        group: group.to_owned(),
        description: description.to_owned(),
        usage: usage.to_owned(),
        help: String::new(),
        help_fn: Some(Arc::new(help_fn)),
        handle: Arc::new(handle),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_helpers_handle_surrogate_pairs() {
        assert_eq!(utf16_len_usize("a中𝚊"), 4);
        assert_eq!(utf16_slice("hello 世界", 6, 2), "世界");
        assert_eq!(utf16_slice("a😀b", 1, 2), "😀");
    }

    #[test]
    fn digits_and_numeric_id_match_go_tests() {
        for (input, want) in [
            ("42", true),
            ("007", true),
            ("", false),
            ("-1", false),
            ("+1", false),
            ("4x2", false),
            ("１２", false),
        ] {
            assert_eq!(is_digits(input), want, "is_digits({input:?})");
        }
        for (input, want) in [
            ("42", true),
            ("-1001234", true),
            ("-", false),
            ("", false),
            ("@42", false),
            ("4x2", false),
            ("+42", false),
        ] {
            assert_eq!(is_numeric_id(input), want, "is_numeric_id({input:?})");
        }
    }
}
