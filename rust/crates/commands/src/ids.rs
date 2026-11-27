use crate::kit;
use chrono::{DateTime, Local, TimeZone};
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BotError, Client, Message};
use mibot_command::{CmdError, Invocation, code, escape, group};
use std::sync::Arc;

const ID_TIMELINE: &[(i64, i64)] = &[
    (0, 1_376_438_400),
    (50_000_000, 1_400_000_000),
    (150_000_000, 1_451_606_400),
    (350_000_000, 1_483_228_800),
    (500_000_000, 1_514_764_800),
    (900_000_000, 1_559_347_200),
    (1_100_000_000, 1_585_699_200),
    (1_450_000_000, 1_609_459_200),
    (2_150_000_000, 1_640_995_200),
    (5_100_000_000, 1_654_041_600),
    (5_600_000_000, 1_672_531_200),
    (6_800_000_000, 1_704_067_200),
    (7_800_000_000, 1_735_689_600),
    (8_500_000_000, 1_767_225_600),
];

fn estimate_creation(id: i64, now: DateTime<Local>) -> DateTime<Local> {
    let mut lower = ID_TIMELINE[ID_TIMELINE.len() - 2];
    let mut upper = ID_TIMELINE[ID_TIMELINE.len() - 1];
    for pair in ID_TIMELINE.windows(2) {
        if id >= pair[0].0 && id <= pair[1].0 {
            lower = pair[0];
            upper = pair[1];
            break;
        }
    }
    let stamp = lower.1
        + ((id - lower.0) as i128 * (upper.1 - lower.1) as i128 / (upper.0 - lower.0) as i128)
            as i64;
    let est = Local
        .timestamp_opt(stamp, 0)
        .single()
        .unwrap_or(now);
    if est > now { now } else { est }
}

fn dc_place(dc: i32) -> &'static str {
    match dc {
        1 | 3 => kit::tr("Miami", "迈阿密"),
        2 | 4 => kit::tr("Amsterdam", "阿姆斯特丹"),
        5 => kit::tr("Singapore", "新加坡"),
        _ => "",
    }
}

fn dc_label(dc: i32) -> String {
    if dc <= 0 {
        return kit::tr("Unknown (no public photo)", "看不出（没有公开头像）").into();
    }
    let place = dc_place(dc);
    if place.is_empty() {
        format!("DC{dc}")
    } else if kit::is_zh() {
        format!("DC{dc}（{place}）")
    } else {
        format!("DC{dc} ({place})")
    }
}

fn unknown_peer_msg() -> &'static str {
    kit::tr(
        "Unknown ID; the account must encounter this peer in a chat first, or use @username / reply instead",
        "没见过这个 ID，账号需要先在某个对话里遇到过对方；用 @用户名 或回复对方的消息更可靠",
    )
}

struct Target {
    peer: Option<tl::enums::InputPeer>,
    user_id: i64,
    reply: Option<Message>,
}

async fn resolve_entity(
    inv: &Invocation,
    fallback: tl::enums::InputPeer,
) -> Result<Target, CmdError> {
    let argument = inv.arg(0).trim();
    if !argument.is_empty() {
        return resolve_argument(inv, argument).await;
    }
    if inv.message.reply_to_id != 0
        && let Some(reply) = kit::reply_msg(inv).await?
    {
        return addressed(&inv.client, reply.sender.clone(), Some(reply));
    }
    Ok(Target {
        peer: Some(fallback),
        user_id: 0,
        reply: None,
    })
}

async fn resolve_argument(inv: &Invocation, argument: &str) -> Result<Target, CmdError> {
    if let Some(id) = mentioned_user(&inv.message) {
        return addressed(
            &inv.client,
            tl::types::PeerUser { user_id: id }.into(),
            None,
        );
    }
    match inv.client.resolve_target(argument).await {
        Ok(peer) => Ok(Target {
            peer: Some(peer),
            user_id: 0,
            reply: None,
        }),
        Err(BotError::UnaddressablePeer(_)) => {
            if let Ok(id) = argument.parse::<i64>()
                && id > 0
            {
                return Ok(Target {
                    peer: None,
                    user_id: id,
                    reply: None,
                });
            }
            Err(kit::fail(unknown_peer_msg()))
        }
        Err(_) => Err(kit::fail(if kit::is_zh() {
            format!("找不到 {argument}")
        } else {
            format!("Cannot find {argument}")
        })),
    }
}

fn mentioned_user(message: &Message) -> Option<i64> {
    let entities = message.raw.entities.as_deref()?;
    for entity in entities {
        if let tl::enums::MessageEntity::MentionName(m) = entity {
            return Some(m.user_id);
        }
    }
    None
}

fn addressed(
    client: &Client,
    peer: tl::enums::Peer,
    reply: Option<Message>,
) -> Result<Target, CmdError> {
    if let Ok(input) = client.input_peer(&peer) {
        return Ok(Target {
            peer: Some(input),
            user_id: 0,
            reply,
        });
    }
    if let tl::enums::Peer::User(u) = peer {
        return Ok(Target {
            peer: None,
            user_id: u.user_id,
            reply,
        });
    }
    Err(kit::fail(unknown_peer_msg()))
}

#[derive(Debug, Default)]
struct EntityInfo {
    kind: &'static str,
    id: i64,
    name: String,
    username: String,
    dc: i32,
    about: String,
    common: i32,
    members: i32,
    flags: Vec<&'static str>,
    unknown: bool,
}

async fn lookup_dc(
    inv: &Invocation,
    fallback: tl::enums::InputPeer,
) -> Result<EntityInfo, CmdError> {
    let found = resolve_entity(inv, fallback).await?;
    let mut primary_err = match found.peer {
        None => Err(kit::fail(unknown_peer_msg())),
        Some(ref peer) => fetch_entity(&inv.client, peer)
            .await
            .map_err(|e| kit::wrap_fail(kit::tr("Lookup failed", "查询失败"), e)),
    };
    if primary_err.is_err()
        && let Some(ref reply) = found.reply
        && let Ok(chat) = inv.client.input_peer(&reply.peer)
        && let Ok(fallback_info) = fetch_entity(&inv.client, &chat).await
    {
        primary_err = Ok(fallback_info);
    }
    primary_err
}

async fn fetch_entity(client: &Client, peer: &tl::enums::InputPeer) -> Result<EntityInfo, CmdError> {
    if let Some(user) = kit::input_user(peer) {
        let tl::enums::users::UserFull::Full(full) = client
            .invoke(&tl::functions::users::GetFullUser { id: user })
            .await?;
        client.peers().remember_entities(&full.users, &full.chats);
        let tl::enums::UserFull::Full(detail) = full.full_user;
        let mut info = EntityInfo {
            kind: "user",
            about: detail.about.unwrap_or_default(),
            common: detail.common_chats_count,
            ..Default::default()
        };
        for candidate in full.users {
            let tl::enums::User::User(u) = candidate else {
                continue;
            };
            if u.id != detail.id {
                continue;
            }
            info.id = u.id;
            info.name = format!(
                "{} {}",
                u.first_name.as_deref().unwrap_or(""),
                u.last_name.as_deref().unwrap_or("")
            )
            .trim()
            .to_owned();
            info.username = primary_username(u.username.as_deref(), u.usernames.as_deref());
            if let Some(tl::enums::UserProfilePhoto::Photo(p)) = u.photo {
                info.dc = p.dc_id;
            }
            for (set, label) in [
                (u.bot, kit::tr("🤖 Bot", "🤖 机器人")),
                (u.verified, kit::tr("✅ Verified", "✅ 已验证")),
                (u.premium, "⭐ Premium"),
                (u.scam, kit::tr("⚠️ Scam", "⚠️ 诈骗")),
                (u.fake, kit::tr("❌ Fake", "❌ 虚假")),
                (u.deleted, kit::tr("🗑 Deleted", "🗑 已注销")),
            ] {
                if set {
                    info.flags.push(label);
                }
            }
        }
        if info.name.is_empty() {
            info.name = kit::tr("Deleted Account", "已注销的账号").into();
        }
        return Ok(info);
    }
    if let Some(channel) = kit::input_channel(peer) {
        let tl::enums::messages::ChatFull::Full(full) = client
            .invoke(&tl::functions::channels::GetFullChannel {
                channel: channel.clone().into(),
            })
            .await?;
        client.peers().remember_entities(&full.users, &full.chats);
        let mut info = EntityInfo {
            kind: "channel",
            ..Default::default()
        };
        if let tl::enums::ChatFull::ChannelFull(detail) = full.full_chat {
            info.about = detail.about;
            info.members = detail.participants_count.unwrap_or(0);
        }
        for candidate in full.chats {
            let tl::enums::Chat::Channel(ch) = candidate else {
                continue;
            };
            if ch.id != channel.channel_id {
                continue;
            }
            info.id = ch.id;
            info.name = ch.title;
            info.username = primary_username(ch.username.as_deref(), ch.usernames.as_deref());
            if ch.megagroup {
                info.kind = "supergroup";
            }
            if let tl::enums::ChatPhoto::Photo(p) = ch.photo {
                info.dc = p.dc_id;
            }
            if ch.verified {
                info.flags.push(kit::tr("✅ Verified", "✅ 已验证"));
            }
            if ch.scam {
                info.flags.push(kit::tr("⚠️ Scam", "⚠️ 诈骗"));
            }
        }
        return Ok(info);
    }
    if let tl::enums::InputPeer::Chat(chat) = peer {
        let tl::enums::messages::ChatFull::Full(full) = client
            .invoke(&tl::functions::messages::GetFullChat {
                chat_id: chat.chat_id,
            })
            .await?;
        client.peers().remember_entities(&full.users, &full.chats);
        let mut info = EntityInfo {
            kind: "group",
            id: chat.chat_id,
            ..Default::default()
        };
        if let tl::enums::ChatFull::Full(detail) = full.full_chat {
            info.about = detail.about;
        }
        for candidate in full.chats {
            if let tl::enums::Chat::Chat(c) = candidate
                && c.id == chat.chat_id
            {
                info.name = c.title;
                info.members = c.participants_count;
                if let tl::enums::ChatPhoto::Photo(p) = c.photo {
                    info.dc = p.dc_id;
                }
            }
        }
        return Ok(info);
    }
    Err(kit::fail(kit::tr("Unsupported peer type", "这种对象查不了")))
}

fn primary_username(username: Option<&str>, usernames: Option<&[tl::enums::Username]>) -> String {
    if let Some(u) = username
        && !u.is_empty()
    {
        return u.to_owned();
    }
    if let Some(list) = usernames {
        for item in list {
            let tl::enums::Username::Username(u) = item;
            if u.active {
                return u.username.clone();
            }
        }
    }
    String::new()
}

async fn joined_at(inv: &Invocation, user: &tl::enums::InputPeer) -> Option<DateTime<Local>> {
    if !inv.message.is_group() {
        return None;
    }
    let chat = inv.client.input_peer(&inv.message.peer).ok()?;
    let channel = kit::input_channel(&chat)?;
    let tl::enums::channels::ChannelParticipant::Participant(res) = inv
        .client
        .invoke(&tl::functions::channels::GetParticipant {
            channel: channel.into(),
            participant: user.clone(),
        })
        .await
        .ok()?;
    let ts = match res.participant {
        tl::enums::ChannelParticipant::Participant(p) => p.date,
        tl::enums::ChannelParticipant::Admin(p) => p.date,
        tl::enums::ChannelParticipant::ParticipantSelf(p) => p.date,
        tl::enums::ChannelParticipant::Banned(p) => p.date,
        _ => return None,
    };
    Local.timestamp_opt(i64::from(ts), 0).single()
}

fn render_ids(
    info: &EntityInfo,
    joined: Option<DateTime<Local>>,
    now: DateTime<Local>,
) -> String {
    let zh = kit::is_zh();
    let id_text = info.id.to_string();
    let username = if info.username.is_empty() {
        kit::tr("None", "无").to_owned()
    } else {
        format!("@{}", info.username)
    };
    let mut lines = Vec::new();
    if info.kind == "user" {
        lines.push(format!("👤 <b>{}</b>", escape(&info.name)));
        lines.push(String::new());
        if zh {
            lines.push(format!("• 用户名：{}", code(&username)));
            lines.push(format!("• ID：{}", code(&id_text)));
            lines.push(format!(
                "• 注册时间：{} <i>按 ID 估算，误差约两个月</i>",
                code(&estimate_creation(info.id, now).format("约 %Y 年 %-m 月").to_string())
            ));
            if let Some(j) = joined {
                lines.push(format!("• 入群时间：{}", code(&j.format("%Y-%m-%d %H:%M").to_string())));
            }
            if info.unknown {
                lines.push("• DC：未知".into());
                lines.push(String::new());
                lines.push("<i>账号没见过这个用户，只能给出按 ID 推算的信息</i>".into());
            } else {
                lines.push(format!("• DC：{}", escape(&dc_label(info.dc))));
                lines.push(format!("• 共同群组：{} 个", code(&info.common.to_string())));
            }
        } else {
            lines.push(format!("• Username: {}", code(&username)));
            lines.push(format!("• ID: {}", code(&id_text)));
            lines.push(format!(
                "• Registered: {} <i>estimated from ID, ~2 months margin</i>",
                code(&estimate_creation(info.id, now).format("~%Y-%m").to_string())
            ));
            if let Some(j) = joined {
                lines.push(format!("• Joined: {}", code(&j.format("%Y-%m-%d %H:%M").to_string())));
            }
            if info.unknown {
                lines.push("• DC: Unknown".into());
                lines.push(String::new());
                lines.push("<i>Unseen user; showing ID-based estimate only</i>".into());
            } else {
                lines.push(format!("• DC: {}", escape(&dc_label(info.dc))));
                lines.push(format!("• Common chats: {}", code(&info.common.to_string())));
            }
        }
    } else {
        let kind = match info.kind {
            "channel" => kit::tr("Channel", "频道"),
            "supergroup" => kit::tr("Supergroup", "超级群"),
            _ => kit::tr("Group", "普通群"),
        };
        let full_id = if info.kind == "group" {
            id_text.clone()
        } else {
            format!("-100{id_text}")
        };
        lines.push(format!("📢 <b>{}</b>", escape(&info.name)));
        lines.push(String::new());
        if zh {
            lines.push(format!("• 类型：{kind}"));
            lines.push(format!("• 用户名：{}", code(&username)));
            lines.push(format!("• ID：{}", code(&full_id)));
            if info.members > 0 {
                lines.push(format!("• 成员：{}", code(&info.members.to_string())));
            }
            lines.push(format!("• DC：{}", escape(&dc_label(info.dc))));
        } else {
            lines.push(format!("• Type: {kind}"));
            lines.push(format!("• Username: {}", code(&username)));
            lines.push(format!("• ID: {}", code(&full_id)));
            if info.members > 0 {
                lines.push(format!("• Members: {}", code(&info.members.to_string())));
            }
            lines.push(format!("• DC: {}", escape(&dc_label(info.dc))));
        }
    }
    if !info.flags.is_empty() {
        lines.push(format!(
            "{}{}",
            kit::tr("• Status: ", "• 状态："),
            info.flags.join(" ")
        ));
    }
    let about = info.about.trim();
    if !about.is_empty() {
        let truncated = if about.chars().count() > 300 {
            format!("{}…", kit::truncate_runes(about, 300))
        } else {
            about.to_owned()
        };
        lines.push(String::new());
        lines.push(kit::tr("<b>Bio</b>", "<b>简介</b>").into());
        lines.push(escape(&truncated));
    }
    let mut links = Vec::new();
    if info.kind == "user" {
        links.push(format!(
            r#"<a href="tg://user?id={id_text}">{}</a>"#,
            kit::tr("Profile", "用户资料")
        ));
        links.push(format!(
            r#"<a href="tg://openmessage?user_id={id_text}">{}</a>"#,
            kit::tr("Open Chat", "打开对话")
        ));
    }
    if !info.username.is_empty() {
        let esc = escape(&info.username);
        links.push(format!(r#"<a href="https://t.me/{esc}">t.me/{esc}</a>"#));
    }
    if !links.is_empty() {
        lines.push(String::new());
        lines.push(kit::tr("<b>Links</b>", "<b>链接</b>").into());
        lines.push(links.join(" · "));
        if info.kind == "user" {
            lines.push(code(&format!("tg://user?id={id_text}")));
        }
    }
    lines.join("\n")
}

fn ids_help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "🆔 <b>用户信息</b>\n\n\
            • <code>{p}ids</code> 自己\n\
            • <code>{p}ids @用户名</code> 或 <code>{p}ids 用户 ID</code>，也可以在命令后提及对方\n\
            • 回复某人的消息发 <code>{p}ids</code>\n\n\
            显示 ID、用户名、估算的注册时间、所在 DC、共同群组数、简介和跳转链接；在超级群里还会显示入群时间。频道和群也能查。\n\n\
            注册时间是按 ID 估算的，Telegram 不公开真实时间。"
        )
    } else {
        format!(
            "🆔 <b>User Info</b>\n\n\
            • <code>{p}ids</code> Yourself\n\
            • <code>{p}ids @username</code> or <code>{p}ids user_id</code>, or mention a user\n\
            • Reply to a message with <code>{p}ids</code>\n\n\
            Shows ID, username, estimated registration date, DC, common chats, bio, and profile links (plus join date in supergroups). Also works for channels and groups.\n\n\
            Registration date is estimated from ID; Telegram does not expose exact dates."
        )
    }
}

fn dc_help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "📍 <b>所在数据中心</b>\n\n\
            • <code>{p}dc</code> 当前对话（私聊里是对方）\n\
            • 回复某人的消息发 <code>{p}dc</code>\n\
            • <code>{p}dc @用户名</code>，也可以在命令后提及对方\n\n\
            DC 读自头像存放的位置，没有公开头像的账号看不出来。被回复的人查不到时，改看那条消息所在的对话。"
        )
    } else {
        format!(
            "📍 <b>Datacenter</b>\n\n\
            • <code>{p}dc</code> Current chat (or peer in PM)\n\
            • Reply to a message with <code>{p}dc</code>\n\
            • <code>{p}dc @username</code>, or mention a user\n\n\
            DC is inferred from the profile photo location; accounts without a public photo show as unknown. If the replied user cannot be resolved, falls back to the chat."
        )
    }
}

pub fn register(a: &Arc<App>) {
    a.registry.register(
        kit::make_cmd(
            "ids",
            &[],
            group::TOOLS,
            "Look up user or chat info",
            "[@username|ID]",
            ids_help,
            |inv| {
                Box::pin(async move {
                    let found = resolve_entity(&inv, tl::enums::InputPeer::PeerSelf).await?;
                    let Some(peer) = found.peer else {
                        let unknown = EntityInfo {
                            kind: "user",
                            id: found.user_id,
                            name: if kit::is_zh() {
                                format!("用户 {}", found.user_id)
                            } else {
                                format!("User {}", found.user_id)
                            },
                            unknown: true,
                            ..Default::default()
                        };
                        return inv
                            .edit(&render_ids(&unknown, None, Local::now()))
                            .await
                            .map_err(Into::into);
                    };
                    let info = fetch_entity(&inv.client, &peer)
                        .await
                        .map_err(|e| kit::wrap_fail(kit::tr("Lookup failed", "查询失败"), e))?;
                    let joined = if info.kind == "user" {
                        joined_at(&inv, &peer).await
                    } else {
                        None
                    };
                    inv.edit(&render_ids(&info, joined, Local::now()))
                        .await
                        .map_err(Into::into)
                })
            },
        )
        .zh("查用户或对话的资料", "[@用户名|ID]"),
    );
    a.registry.register(
        kit::make_cmd(
            "dc",
            &[],
            group::TOOLS,
            "Look up datacenter",
            "[@username|ID]",
            dc_help,
            |inv| {
                Box::pin(async move {
                    let here = inv
                        .client
                        .input_peer(&inv.message.peer)
                        .unwrap_or(tl::enums::InputPeer::PeerSelf);
                    let info = lookup_dc(&inv, here).await?;
                    inv.edit(&format!(
                        "📍 <b>{}</b>\n{}{}",
                        escape(&info.name),
                        kit::tr("Datacenter: ", "所在数据中心："),
                        escape(&dc_label(info.dc))
                    ))
                    .await
                    .map_err(Into::into)
                })
            },
        )
        .zh("查所在的数据中心", "[@用户名|ID]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use mibot_bot::{PeerCache, RpcTransport, SentRecorder};
    use tokio_util::sync::CancellationToken;

    struct DummyTransport;
    impl RpcTransport for DummyTransport {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            _cid: u32,
            _body: Vec<u8>,
        ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, grammers_mtsender::InvocationError>> {
            Box::pin(async { Err(grammers_mtsender::InvocationError::Dropped("offline")) })
        }
    }

    fn offline_client() -> Arc<Client> {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let mut user = kit::blank_tl_user(42);
        user.access_hash = Some(7);
        user.first_name = Some("Known".into());
        peers.remember_users(&[user.into()]);
        Arc::new(Client::from_transport(
            Arc::new(DummyTransport),
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ))
    }

    #[test]
    fn test_estimate_creation() {
        let now = Local.with_ymd_and_hms(2026, 9, 23, 0, 0, 0).unwrap();
        assert_eq!(estimate_creation(215_959_394, now).year(), 2016);
        let mut previous = Local.timestamp_opt(0, 0).unwrap();
        let mut id = 0i64;
        while id <= 8_000_000_000 {
            let got = estimate_creation(id, now);
            assert!(got >= previous, "id {id} went back in time");
            previous = got;
            id += 250_000_000;
        }
        let beyond = estimate_creation(8_600_000_000, now);
        assert!(beyond > Local.timestamp_opt(1_767_225_600, 0).unwrap() || beyond == now);
        assert_eq!(estimate_creation(99_000_000_000, now), now);
    }

    #[test]
    fn test_render_ids() {
        let now = Local.with_ymd_and_hms(2026, 9, 23, 0, 0, 0).unwrap();
        let joined = Local.with_ymd_and_hms(2024, 1, 2, 3, 4, 0).unwrap();
        let user = render_ids(
            &EntityInfo {
                kind: "user",
                id: 215_959_394,
                name: "Cat".into(),
                username: "cat".into(),
                dc: 5,
                common: 3,
                flags: vec!["⭐ Premium"],
                ..Default::default()
            },
            Some(joined),
            now,
        );
        for wanted in [
            "215959394",
            "@cat",
            "DC5 (Singapore)",
            "2024-01-02 03:04",
            "tg://user?id=215959394",
            "t.me/cat",
            "⭐ Premium",
        ] {
            assert!(user.contains(wanted), "lost {wanted}:\n{user}");
        }
        let channel = render_ids(
            &EntityInfo {
                kind: "supergroup",
                id: 1_771_725_356,
                name: "Group".into(),
                members: 42,
                ..Default::default()
            },
            None,
            now,
        );
        for wanted in ["Supergroup", "-1001771725356", "42", "Unknown"] {
            assert!(channel.contains(wanted), "lost {wanted}:\n{channel}");
        }
        assert!(!channel.contains("Registered") && !channel.contains("tg://user"));
    }

    #[test]
    fn test_render_ids_unknown_user() {
        let now = Local.with_ymd_and_hms(2026, 9, 23, 0, 0, 0).unwrap();
        let card = render_ids(
            &EntityInfo {
                kind: "user",
                id: 6_000_000_000,
                name: "User 6000000000".into(),
                unknown: true,
                ..Default::default()
            },
            None,
            now,
        );
        for wanted in [
            "6000000000",
            "Registered",
            "DC: Unknown",
            "tg://user?id=6000000000",
            "tg://openmessage?user_id=6000000000",
        ] {
            assert!(card.contains(wanted), "lost {wanted}:\n{card}");
        }
        assert!(!card.contains("Common chats") && !card.contains("no public photo"));
    }

    #[tokio::test]
    async fn test_resolve_argument() {
        let make_inv = |raw: tl::types::Message| {
            let mut msg =
                kit::blank_bot_message(1, tl::types::PeerUser { user_id: 1 }.into());
            msg.raw = raw;
            Invocation {
                client: offline_client(),
                registry: Arc::new(mibot_command::Registry::new()),
                message: msg,
                prefix: ".".into(),
                command: "ids".into(),
                args: vec![],
                raw: String::new(),
                cancel: CancellationToken::new(),
                foreign: false,
            }
        };

        let mut mention = kit::blank_tl_message(1, tl::types::PeerUser { user_id: 1 }.into());
        mention.message = ".ids Known".into();
        mention.entities = Some(vec![
            tl::types::MessageEntityBold {
                offset: 0,
                length: 4,
            }
            .into(),
            tl::types::MessageEntityMentionName {
                offset: 5,
                length: 5,
                user_id: 42,
            }
            .into(),
        ]);
        let found = resolve_argument(&make_inv(mention), "Known").await.unwrap();
        assert!(matches!(
            found.peer,
            Some(tl::enums::InputPeer::User(u)) if u.user_id == 42 && u.access_hash == 7
        ));

        let mut unseen = kit::blank_tl_message(1, tl::types::PeerUser { user_id: 1 }.into());
        unseen.message = ".ids Stranger".into();
        unseen.entities = Some(vec![
            tl::types::MessageEntityMentionName {
                offset: 5,
                length: 8,
                user_id: 99,
            }
            .into(),
        ]);
        let found_unseen = resolve_argument(&make_inv(unseen), "Stranger").await.unwrap();
        assert!(found_unseen.peer.is_none() && found_unseen.user_id == 99);

        let num = resolve_argument(
            &make_inv(kit::blank_tl_message(
                1,
                tl::types::PeerUser { user_id: 1 }.into(),
            )),
            "123456",
        )
        .await
        .unwrap();
        assert!(num.peer.is_none() && num.user_id == 123456);

        assert!(
            resolve_argument(
                &make_inv(kit::blank_tl_message(
                    1,
                    tl::types::PeerUser { user_id: 1 }.into()
                )),
                "-1001234"
            )
            .await
            .is_err()
        );
    }
}
