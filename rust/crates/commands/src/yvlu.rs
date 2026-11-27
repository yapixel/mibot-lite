use crate::kit;
use crate::sticker::{add_to_pack, explain, pack_name_rule, valid_pack_name};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{Client, MediaOptions, Message, PeerCache, code, envelope, escape, utf16_len};
use mibot_command::{CmdError, CmdResult, Command, Invocation, fail, group};
use mibot_httpx::{Method, Request, do_request, reason as httpx_reason};
use mibot_imaging::{decode as imaging_decode, encode_png, flatten, resize_cover};
use mibot_logtail::Level;
use mibot_media::{StickerAttribute, find_ffmpeg, resize_sticker, sticker_document, to_sticker_webm};
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

const QUOTE_ENDPOINT: &str = "https://quote-api-enhanced.zhetengsha.eu.org/generate.webp";
const QUOTE_USER_AGENT: &str = "TeleBox/0.2.1";
const AVATAR_CACHE_LIMIT: usize = 32;
const AVATAR_BUDGET: Duration = Duration::from_secs(20);
const MEDIA_BUDGET: Duration = Duration::from_secs(45);
const TAG_BUDGET: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct YvluConfig {
    #[serde(rename = "stickerSetShortName")]
    sticker_set: String,
}

#[derive(Debug, Clone, Serialize)]
struct QuotePhoto {
    url: String,
}

#[derive(Debug, Clone, Default, Serialize)]
struct QuoteFrom {
    id: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    first_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    last_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    photo: Option<QuotePhoto>,
    #[serde(skip_serializing_if = "String::is_empty")]
    emoji_status: String,
}

#[derive(Debug, Clone, Serialize)]
struct QuoteEntityUser {
    id: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
struct QuoteEntity {
    offset: i32,
    length: i32,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    language: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    custom_emoji_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<QuoteEntityUser>,
}

#[derive(Debug, Clone, Serialize)]
struct QuoteReply {
    name: String,
    text: String,
    entities: Vec<QuoteEntity>,
    #[serde(rename = "chatId", skip_serializing_if = "is_zero_i64")]
    chat_id: i64,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Serialize)]
struct QuoteAudio {
    title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    performer: String,
    #[serde(skip_serializing_if = "is_zero_i32")]
    duration: i32,
}

#[derive(Debug, Clone, Serialize)]
struct QuoteVoice {
    waveform: Vec<i32>,
    #[serde(skip_serializing_if = "is_zero_i32")]
    duration: i32,
}

#[derive(Debug, Clone, Serialize)]
struct QuoteDoc {
    file_name: String,
}

#[derive(Debug, Clone, Serialize)]
struct QuoteFwd {
    label: String,
}

#[derive(Debug, Clone, Serialize)]
struct QuoteMessage {
    from: QuoteFrom,
    text: String,
    entities: Vec<QuoteEntity>,
    avatar: bool,
    #[serde(rename = "replyMessage", skip_serializing_if = "Option::is_none")]
    reply_message: Option<QuoteReply>,
    #[serde(skip_serializing_if = "Option::is_none")]
    media: Option<QuotePhoto>,
    #[serde(rename = "mediaType", skip_serializing_if = "String::is_empty")]
    media_type: String,
    #[serde(rename = "mediaDuration", skip_serializing_if = "is_zero_i32")]
    media_duration: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<QuoteVoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    document: Option<QuoteDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    forward: Option<QuoteFwd>,
    #[serde(rename = "senderTag", skip_serializing_if = "String::is_empty")]
    sender_tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio: Option<QuoteAudio>,
}

#[derive(Debug, Clone, Serialize)]
struct QuotePayload {
    #[serde(rename = "type")]
    kind: String,
    format: String,
    #[serde(rename = "backgroundColor")]
    background_color: String,
    width: i32,
    height: i32,
    scale: i32,
    #[serde(rename = "emojiBrand")]
    emoji_brand: String,
    messages: Vec<QuoteMessage>,
}

struct YvluOptions {
    count: i32,
    include_reply: bool,
    format: String,
    fake_text: String,
    fake_ents: Vec<tl::enums::MessageEntity>,
    fake_sender: Option<tl::enums::InputPeer>,
    fake_author: Option<QuoteFrom>,
}

struct YvluService {
    a: Arc<App>,
    store: Store<YvluConfig>,
    quote_endpoint: String,
    avatars: StdMutex<AvatarCache>,
}

#[derive(Default)]
struct AvatarCache {
    map: HashMap<String, Option<QuotePhoto>>,
    order: VecDeque<String>,
}

impl AvatarCache {
    fn get(&self, key: &str) -> Option<Option<QuotePhoto>> {
        self.map.get(key).cloned()
    }

    fn insert(&mut self, key: String, photo: Option<QuotePhoto>) {
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.map.entry(key.clone()) {
            e.insert(photo);
            return;
        }
        if self.map.len() >= AVATAR_CACHE_LIMIT
            && let Some(oldest) = self.order.pop_front()
        {
            self.map.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.map.insert(key, photo);
    }
}

pub fn register(a: &Arc<App>) {
    let endpoint = a
        .env
        .get("MIBOT_QUOTE_ENDPOINT", QUOTE_ENDPOINT)
        .trim()
        .to_owned();
    let endpoint = if endpoint.is_empty() {
        QUOTE_ENDPOINT.to_owned()
    } else {
        endpoint
    };
    let svc = Arc::new(YvluService {
        a: Arc::clone(a),
        store: kit::new_store(a, "yvlu.json", YvluConfig::default),
        quote_endpoint: endpoint,
        avatars: StdMutex::new(AvatarCache::default()),
    });
    a.registry.register(
        Command {
            name: "yvlu".into(),
            group: group::MEDIA.into(),
            description: "Create quote stickers".into(),
            usage: "[count]".into(),
            help_fn: Some(Arc::new(yvlu_help)),
            timeout: Some(Duration::from_secs(300)),
            handle: Arc::new(move |inv| {
                let svc = Arc::clone(&svc);
                Box::pin(async move {
                    svc.handle(&inv)
                        .await
                        .map_err(|e| kit::wrap_fail(kit::tr("Failed to generate quote", "语录生成失败"), e))
                })
            }),
            ..Default::default()
        }
        .zh("把消息做成语录贴纸", "[消息数]"),
    );
}

fn yvlu_help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "📝 <b>生成文字语录贴纸</b>\n\n\
             • <code>{p}yvlu [消息数]</code> 回复消息生成语录，最多 5 条\n\
             • <code>{p}yvlu r [消息数]</code> 包含被引用的内容\n\
             • <code>{p}yvlu f 文本</code> 伪造文本（<code>fr</code> 同时包含回复）\n\
             • <code>{p}yvlu u 用户 ID|@用户名 [消息数]</code> 伪造发送者（<code>ur</code> 同时包含回复）\n\
             • <code>{p}yvlu webp|image|png|stories [消息数]</code> 指定输出格式\n\
             • <code>{p}yvlu s</code> 保存回复的贴纸或图片到贴纸包\n\
             • <code>{p}yvlu config</code> 查看配置\n\
             • <code>{p}yvlu config sticker 贴纸包名</code> 设置贴纸包\n\n\
             图片由远程 quote 服务渲染，需要网络可达。"
        )
    } else {
        format!(
            "📝 <b>Quote Stickers</b>\n\n\
             • <code>{p}yvlu [count]</code> reply to generate a quote sticker (max 5 messages)\n\
             • <code>{p}yvlu r [count]</code> include replied context\n\
             • <code>{p}yvlu f text</code> fake message text (<code>fr</code> also includes reply)\n\
             • <code>{p}yvlu u user_id|@username [count]</code> fake sender (<code>ur</code> also includes reply)\n\
             • <code>{p}yvlu webp|image|png|stories [count]</code> choose output format\n\
             • <code>{p}yvlu s</code> save replied sticker or image to sticker pack\n\
             • <code>{p}yvlu config</code> view settings\n\
             • <code>{p}yvlu config sticker pack_name</code> set sticker pack\n\n\
             Rendered by a remote quote service; requires network access."
        )
    }
}

fn is_format_arg(v: &str) -> bool {
    matches!(v, "webp" | "image" | "png" | "stories")
}

fn norm_format(v: &str) -> &'static str {
    match v {
        "png" | "image" => "image",
        "stories" => "stories",
        _ => "quote",
    }
}

fn parse_count(v: &str) -> i32 {
    v.parse::<i32>().ok().filter(|&n| n >= 1).unwrap_or(1)
}

fn entity_range(entity: &tl::enums::MessageEntity) -> (i32, i32) {
    match entity {
        tl::enums::MessageEntity::Bold(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Italic(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Code(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Pre(e) => (e.offset, e.length),
        tl::enums::MessageEntity::TextUrl(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Url(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Underline(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Strike(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Spoiler(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Blockquote(e) => (e.offset, e.length),
        tl::enums::MessageEntity::CustomEmoji(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Mention(e) => (e.offset, e.length),
        tl::enums::MessageEntity::MentionName(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Hashtag(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Cashtag(e) => (e.offset, e.length),
        tl::enums::MessageEntity::BotCommand(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Email(e) => (e.offset, e.length),
        tl::enums::MessageEntity::Phone(e) => (e.offset, e.length),
        _ => (0, 0),
    }
}

fn shift_entities(
    entities: &[tl::enums::MessageEntity],
    offset: i32,
) -> Vec<tl::enums::MessageEntity> {
    entities
        .iter()
        .filter(|e| {
            let (start, len) = entity_range(e);
            start + len > offset
        })
        .cloned()
        .collect()
}

fn convert_entities(entities: &[tl::enums::MessageEntity], shift: i32) -> Vec<QuoteEntity> {
    let mut out = Vec::with_capacity(entities.len());
    for entity in entities {
        let (raw_off, mut len) = entity_range(entity);
        let mut off = raw_off - shift;
        if len <= 0 {
            continue;
        }
        if off < 0 {
            len += off;
            off = 0;
            if len <= 0 {
                continue;
            }
        }
        let mut item = QuoteEntity {
            offset: off,
            length: len,
            ..Default::default()
        };
        match entity {
            tl::enums::MessageEntity::Bold(_) => item.kind = "bold".into(),
            tl::enums::MessageEntity::Italic(_) => item.kind = "italic".into(),
            tl::enums::MessageEntity::Underline(_) => item.kind = "underline".into(),
            tl::enums::MessageEntity::Strike(_) => item.kind = "strikethrough".into(),
            tl::enums::MessageEntity::Code(_) => item.kind = "code".into(),
            tl::enums::MessageEntity::Pre(e) => {
                item.kind = "pre".into();
                item.language = e.language.clone();
            }
            tl::enums::MessageEntity::Spoiler(_) => item.kind = "spoiler".into(),
            tl::enums::MessageEntity::Blockquote(_) => item.kind = "blockquote".into(),
            tl::enums::MessageEntity::Url(_) => item.kind = "url".into(),
            tl::enums::MessageEntity::TextUrl(e) => {
                item.kind = "text_link".into();
                item.url = e.url.clone();
            }
            tl::enums::MessageEntity::Mention(_) => item.kind = "mention".into(),
            tl::enums::MessageEntity::MentionName(e) => {
                item.kind = "text_mention".into();
                item.user = Some(QuoteEntityUser { id: e.user_id });
            }
            tl::enums::MessageEntity::CustomEmoji(e) => {
                item.kind = "custom_emoji".into();
                item.custom_emoji_id = e.document_id.to_string();
            }
            tl::enums::MessageEntity::Hashtag(_) => item.kind = "hashtag".into(),
            tl::enums::MessageEntity::Cashtag(_) => item.kind = "cashtag".into(),
            tl::enums::MessageEntity::BotCommand(_) => item.kind = "bot_command".into(),
            tl::enums::MessageEntity::Email(_) => item.kind = "email".into(),
            tl::enums::MessageEntity::Phone(_) => item.kind = "phone_number".into(),
            _ => continue,
        }
        out.push(item);
    }
    out
}

fn parse_yvlu(inv: &Invocation) -> Option<YvluOptions> {
    let mut opts = YvluOptions {
        count: 1,
        include_reply: false,
        format: "quote".into(),
        fake_text: String::new(),
        fake_ents: Vec::new(),
        fake_sender: None,
        fake_author: None,
    };
    let sub = inv.arg(0);
    if sub.is_empty() || kit::is_digits(sub) {
        opts.count = parse_count(sub);
    } else if sub == "r" {
        opts.include_reply = true;
        if inv.args.len() > 1 && is_format_arg(&inv.args[1]) {
            opts.format = norm_format(&inv.args[1]).into();
            opts.count = parse_count(inv.arg(2));
        } else {
            opts.count = parse_count(inv.arg(1));
        }
    } else if (sub == "u" || sub == "ur") && inv.args.len() > 1 {
        opts.include_reply = sub == "ur";
        opts.count = parse_count(inv.arg(2));
    } else if (sub == "f" || sub == "fr") && inv.args.len() > 1 {
        opts.include_reply = sub == "fr";
        let fake = inv.raw_after(1).trim_start();
        if fake.is_empty() {
            return None;
        }
        opts.fake_text = fake.to_owned();
        let full = inv.message.raw.message.as_str();
        let offset = if let Some(prefix_part) = full.strip_suffix(fake) {
            utf16_len(prefix_part)
        } else {
            let full_inv = format!("{}{} {}", inv.prefix, inv.command, inv.raw);
            utf16_len(full_inv.strip_suffix(fake).unwrap_or(""))
        };
        if let Some(entities) = &inv.message.raw.entities {
            opts.fake_ents = shift_entities(entities, offset);
        }
    } else if is_format_arg(sub) {
        opts.format = norm_format(sub).into();
        opts.count = parse_count(inv.arg(1));
    } else {
        return None;
    }
    Some(opts)
}

fn photo_fits(data: &[u8]) -> bool {
    if data.len() > 10 << 20 {
        return false;
    }
    let Ok(reader) = image::ImageReader::new(Cursor::new(data)).with_guessed_format() else {
        return false;
    };
    let Ok((w, h)) = reader.into_dimensions() else {
        return false;
    };
    if w < 1 || h < 1 || w + h > 10000 {
        return false;
    }
    w.max(h) <= 20 * w.min(h)
}

fn photo_rejected(err: &mibot_bot::BotError) -> bool {
    if err.is_rpc("CHAT_SEND_PHOTOS_FORBIDDEN") {
        return true;
    }
    err.rpc_error().is_some_and(|r| r.code == 400)
}

fn decode_waveform(packed: &[u8]) -> Vec<i32> {
    let mut samples = Vec::with_capacity(packed.len() * 8 / 5);
    let mut bits: u16 = 0;
    let mut available: u32 = 0;
    for &b in packed {
        bits |= u16::from(b) << available;
        available += 8;
        while available >= 5 {
            samples.push(i32::from(bits & 31));
            bits >>= 5;
            available -= 5;
        }
    }
    samples
}

fn extension_for(mime: &str) -> &'static str {
    match mime {
        "video/mp4" => ".mp4",
        "image/gif" => ".gif",
        "video/webm" => ".webm",
        _ => ".bin",
    }
}

fn name_hash(text: &str) -> i32 {
    let mut val: i32 = 0;
    for ch in text.chars() {
        val = val.wrapping_mul(31).wrapping_add(ch as i32);
    }
    val.wrapping_abs()
}

fn is_sticker_document(msg: &tl::types::Message) -> bool {
    let Some(tl::enums::MessageMedia::Document(m)) = &msg.media else {
        return false;
    };
    let Some(tl::enums::Document::Document(doc)) = &m.document else {
        return false;
    };
    doc.attributes
        .iter()
        .any(|a| matches!(a, tl::enums::DocumentAttribute::Sticker(_)))
}

fn user_author(id: i64, first_name: &str, last_name: &str, username: &str) -> QuoteFrom {
    let mut name = format!("{} {}", first_name.trim(), last_name.trim())
        .trim()
        .to_owned();
    if name.is_empty() {
        name = username.to_owned();
    }
    QuoteFrom {
        id,
        name,
        first_name: first_name.to_owned(),
        last_name: last_name.to_owned(),
        username: username.to_owned(),
        photo: None,
        emoji_status: String::new(),
    }
}

fn peer_author(peers: &PeerCache, peer: &tl::enums::Peer) -> Option<QuoteFrom> {
    match peer {
        tl::enums::Peer::User(u) => {
            let info = peers.user(u.user_id).unwrap_or_default();
            Some(user_author(
                u.user_id,
                &info.first_name,
                &info.last_name,
                &info.username,
            ))
        }
        tl::enums::Peer::Channel(ch) => {
            let info = peers.channel(ch.channel_id).unwrap_or_default();
            Some(QuoteFrom {
                id: ch.channel_id,
                name: info.title.clone(),
                first_name: info.title,
                username: info.username,
                ..Default::default()
            })
        }
        tl::enums::Peer::Chat(c) => {
            let info = peers.chat(c.chat_id).unwrap_or_default();
            Some(QuoteFrom {
                id: c.chat_id,
                name: info.title.clone(),
                first_name: info.title,
                ..Default::default()
            })
        }
    }
}

fn forward_author(
    peers: &PeerCache,
    msg: &Message,
) -> Option<(QuoteFrom, Option<tl::enums::Peer>)> {
    let tl::enums::MessageFwdHeader::Header(hdr) = msg.raw.fwd_from.as_ref()?;
    let mut id = 0i64;
    if let Some(peer) = &hdr.from_id {
        match peer {
            tl::enums::Peer::User(u) => {
                if let Some(info) = peers.user(u.user_id) {
                    return Some((
                        user_author(
                            u.user_id,
                            &info.first_name,
                            &info.last_name,
                            &info.username,
                        ),
                        Some(peer.clone()),
                    ));
                }
                id = u.user_id;
            }
            tl::enums::Peer::Channel(ch) => {
                if peers.channel(ch.channel_id).is_some()
                    && let Some(from) = peer_author(peers, peer)
                {
                    return Some((from, Some(peer.clone())));
                }
                id = ch.channel_id;
            }
            _ => {}
        }
    }
    let name = hdr
        .from_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(hdr.saved_from_name.as_deref().filter(|s| !s.is_empty()))
        .or(hdr.post_author.as_deref().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| kit::tr("Unknown source", "未知来源"))
        .to_owned();
    if id == 0 {
        id = i64::from(name_hash(&name));
    }
    Some((
        QuoteFrom {
            id,
            first_name: name.clone(),
            name,
            ..Default::default()
        },
        None,
    ))
}

async fn fake_author(
    client: &Client,
    peer: &tl::enums::InputPeer,
) -> Result<QuoteFrom, CmdError> {
    let peers = client.peers();
    match peer {
        tl::enums::InputPeer::PeerSelf => {
            let mut self_user = client.self_user();
            if let Ok(users) = client
                .invoke(&tl::functions::users::GetUsers {
                    id: vec![tl::enums::InputUser::UserSelf],
                })
                .await
            {
                peers.remember_users(&users);
                if let Some(tl::enums::User::User(u)) = users.first() {
                    self_user = Some(u.clone());
                }
            }
            let u = self_user.ok_or_else(|| {
                fail(kit::tr("Failed to get account info", "无法获取账号信息"))
            })?;
            Ok(user_author(
                u.id,
                u.first_name.as_deref().unwrap_or(""),
                u.last_name.as_deref().unwrap_or(""),
                u.username.as_deref().unwrap_or(""),
            ))
        }
        tl::enums::InputPeer::User(u) => {
            if peers.user(u.user_id).is_none() {
                let users = client
                    .invoke(&tl::functions::users::GetUsers {
                        id: vec![
                            tl::types::InputUser {
                                user_id: u.user_id,
                                access_hash: u.access_hash,
                            }
                            .into(),
                        ],
                    })
                    .await?;
                peers.remember_users(&users);
            }
            let info = peers
                .user(u.user_id)
                .ok_or_else(|| fail(kit::tr("User not found", "查不到这个用户")))?;
            Ok(user_author(
                u.user_id,
                &info.first_name,
                &info.last_name,
                &info.username,
            ))
        }
        tl::enums::InputPeer::Channel(ch) => {
            if peers.channel(ch.channel_id).is_none() {
                let res = client
                    .invoke(&tl::functions::channels::GetChannels {
                        id: vec![
                            tl::types::InputChannel {
                                channel_id: ch.channel_id,
                                access_hash: ch.access_hash,
                            }
                            .into(),
                        ],
                    })
                    .await?;
                let chats = match res {
                    tl::enums::messages::Chats::Chats(c) => c.chats,
                    tl::enums::messages::Chats::Slice(c) => c.chats,
                };
                peers.remember_chats(&chats);
            }
            let not_found = || kit::tr("Channel not found", "查不到这个频道");
            peers
                .channel(ch.channel_id)
                .ok_or_else(|| fail(not_found()))?;
            peer_author(peers, &tl::types::PeerChannel { channel_id: ch.channel_id }.into())
                .ok_or_else(|| fail(not_found()))
        }
        tl::enums::InputPeer::Chat(c) => {
            if peers.chat(c.chat_id).is_none() {
                let res = client
                    .invoke(&tl::functions::messages::GetChats {
                        id: vec![c.chat_id],
                    })
                    .await?;
                let chats = match res {
                    tl::enums::messages::Chats::Chats(ch) => ch.chats,
                    tl::enums::messages::Chats::Slice(ch) => ch.chats,
                };
                peers.remember_chats(&chats);
            }
            let not_found = || kit::tr("Group not found", "查不到这个群");
            peers.chat(c.chat_id).ok_or_else(|| fail(not_found()))?;
            peer_author(peers, &tl::types::PeerChat { chat_id: c.chat_id }.into())
                .ok_or_else(|| fail(not_found()))
        }
        _ => Err(fail(kit::tr("Unsupported fake target", "不支持的伪造对象"))),
    }
}

fn avatar_key(client: &Client, peer: &tl::enums::InputPeer) -> String {
    match peer {
        tl::enums::InputPeer::PeerSelf => {
            if let Some(u) = client.self_user()
                && let Some(tl::enums::UserProfilePhoto::Photo(p)) = u.photo
            {
                return format!("self:{}", p.photo_id);
            }
        }
        tl::enums::InputPeer::User(u) => {
            if let Some(info) = client.peers().user(u.user_id)
                && info.photo_id != 0
            {
                return format!("user:{}:{}", u.user_id, info.photo_id);
            }
        }
        tl::enums::InputPeer::Channel(ch) => {
            if let Some(info) = client.peers().channel(ch.channel_id)
                && info.photo_id != 0
            {
                return format!("channel:{}:{}", ch.channel_id, info.photo_id);
            }
        }
        _ => {}
    }
    String::new()
}

fn web_page_photo(
    msg: &tl::types::Message,
    preview: &tl::types::MessageMediaWebPage,
) -> Option<tl::types::Message> {
    let tl::enums::WebPage::Page(page) = &preview.webpage else {
        return None;
    };
    let Some(tl::enums::Photo::Photo(photo)) = &page.photo else {
        return None;
    };
    Some(tl::types::Message {
        media: Some(
            tl::types::MessageMediaPhoto {
                spoiler: false,
                live_photo: false,
                photo: Some(photo.clone().into()),
                ttl_seconds: None,
                video: None,
            }
            .into(),
        ),
        ..kit::blank_tl_message(msg.id, msg.peer_id.clone())
    })
}

async fn send_quote_sticker(
    client: &Client,
    peer: &tl::enums::InputPeer,
    data: &[u8],
    reply_to: i32,
) -> CmdResult {
    let media = sticker_document("quote", data, "📝", reply_to);
    let attrs: Vec<tl::enums::DocumentAttribute> = media
        .attributes
        .into_iter()
        .map(|a| match a {
            StickerAttribute::Sticker { alt } => tl::types::DocumentAttributeSticker {
                mask: false,
                alt,
                stickerset: tl::enums::InputStickerSet::Empty,
                mask_coords: None,
            }
            .into(),
            StickerAttribute::ImageSize { w, h } => tl::types::DocumentAttributeImageSize {
                w: w as i32,
                h: h as i32,
            }
            .into(),
            StickerAttribute::Video { w, h, duration } => tl::types::DocumentAttributeVideo {
                round_message: false,
                supports_streaming: true,
                nosound: false,
                duration,
                w: w as i32,
                h: h as i32,
                preload_prefix_size: None,
                video_start_ts: None,
                video_codec: None,
            }
            .into(),
        })
        .collect();
    client
        .send_document(
            peer,
            data,
            MediaOptions {
                name: media.name,
                mime_type: media.mime_type.to_owned(),
                reply_to: Some(reply_to).filter(|&id| id > 0),
                attributes: attrs,
                ..Default::default()
            },
        )
        .await?;
    Ok(())
}

impl YvluService {
    async fn handle(&self, inv: &Invocation) -> CmdResult {
        match inv.arg(0).to_ascii_lowercase().as_str() {
            "config" => return self.config(inv).await,
            "s" => return self.save_sticker(inv).await,
            _ => {}
        }
        let Some(mut opts) = parse_yvlu(inv) else {
            return inv.edit(&yvlu_help(inv.prefix())).await.map_err(Into::into);
        };
        if opts.count > 5 {
            return Err(fail(kit::tr("Too many messages (max 5)", "太多了 哒咩")));
        }
        let sub = inv.arg(0).to_ascii_lowercase();
        if sub == "u" || sub == "ur" {
            let target = inv.arg(1);
            let bad_target = || {
                if kit::is_zh() {
                    format!("无法获取 {target} 的信息，请检查用户 ID 或用户名是否正确")
                } else {
                    format!("Failed to get info for {target}; check the user ID or username")
                }
            };
            let peer = inv
                .client
                .resolve_target(target)
                .await
                .map_err(|_| fail(bad_target()))?;
            let author = fake_author(&inv.client, &peer)
                .await
                .map_err(|_| fail(bad_target()))?;
            opts.fake_sender = Some(peer);
            opts.fake_author = Some(author);
        }
        let Some(reply) = kit::reply_msg(inv).await? else {
            return Err(fail(kit::tr("Reply to a message first", "请回复一条消息")));
        };
        inv.edit(&kit::feedback(
            "working",
            kit::tr("Generating quote sticker", "正在生成语录贴纸"),
            "",
        ))
        .await?;

        let started = Instant::now();
        let payload = self.build(inv, &reply, &opts).await?;
        let built = Instant::now();
        let (picture, extension) = self.render(&payload).await?;
        let rendered = Instant::now();
        let peer = inv.client.input_peer(&inv.message.peer)?;

        if extension != "png" {
            send_quote_sticker(&inv.client, &peer, &picture, reply.id).await?;
        } else if photo_fits(&picture) {
            match inv
                .client
                .send_photo(
                    &peer,
                    &picture,
                    MediaOptions {
                        name: "quote.png".into(),
                        reply_to: Some(reply.id),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => {}
                Err(e) if photo_rejected(&e) => {
                    self.a.logs.record(
                        Level::Info,
                        "yvlu.photo_rejected",
                        &[("error", &e.to_string())],
                    );
                    inv.client
                        .send_document(
                            &peer,
                            &picture,
                            MediaOptions {
                                name: "quote.png".into(),
                                mime_type: "image/png".into(),
                                reply_to: Some(reply.id),
                                ..Default::default()
                            },
                        )
                        .await?;
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            inv.client
                .send_document(
                    &peer,
                    &picture,
                    MediaOptions {
                        name: "quote.png".into(),
                        mime_type: "image/png".into(),
                        reply_to: Some(reply.id),
                        ..Default::default()
                    },
                )
                .await?;
        }

        self.a.logs.record(
            Level::Debug,
            "yvlu.timing",
            &[
                ("collect", &format!("{:?}", built - started)),
                ("render", &format!("{:?}", rendered - built)),
                ("upload", &format!("{:?}", rendered.elapsed())),
                ("bytes", &picture.len().to_string()),
            ],
        );
        inv.delete().await.map_err(Into::into)
    }

    async fn config(&self, inv: &Invocation) -> CmdResult {
        let action = inv.arg(1).to_ascii_lowercase();
        if action.is_empty() {
            let cur = self.store.read()?;
            let name = cur.sticker_set.as_str();
            let mut text = if kit::is_zh() {
                format!(
                    "📝 <b>语录配置</b>\n贴纸包名：{}",
                    code(if name.is_empty() { "未设置" } else { name })
                )
            } else {
                format!(
                    "📝 <b>Quote Settings</b>\nSticker pack: {}",
                    code(if name.is_empty() { "Not set" } else { name })
                )
            };
            if !name.is_empty() {
                text.push_str(&if kit::is_zh() {
                    format!("\n贴纸包链接：t.me/addstickers/{}", escape(name))
                } else {
                    format!("\nPack link: t.me/addstickers/{}", escape(name))
                });
            }
            text.push_str(&format!(
                "\n{}",
                code(&if kit::is_zh() {
                    format!("{}yvlu config sticker 贴纸包名", inv.prefix())
                } else {
                    format!("{}yvlu config sticker pack_name", inv.prefix())
                })
            ));
            return inv.edit(&text).await.map_err(Into::into);
        }
        if !matches!(action.as_str(), "sticker" | "stickerset" | "set") {
            return Err(fail(if kit::is_zh() {
                format!(
                    "未知的配置项：{}。可用配置命令：{}yvlu config sticker 贴纸包名",
                    inv.arg(1),
                    inv.prefix()
                )
            } else {
                format!(
                    "Unknown setting: {}. Usage: {}yvlu config sticker pack_name",
                    inv.arg(1),
                    inv.prefix()
                )
            }));
        }
        let name = if inv.args.len() > 2 {
            inv.args[2..].join("_")
        } else {
            String::new()
        };
        if name.is_empty() {
            return Err(kit::usage(
                inv.prefix(),
                kit::tr("yvlu config sticker pack_name", "yvlu config sticker 贴纸包名"),
            ));
        }
        if !valid_pack_name(&name) {
            return Err(fail(pack_name_rule()));
        }
        let name_save = name.clone();
        kit::update_ok(&self.store, |cfg| cfg.sticker_set = name_save)?;
        inv.edit(&kit::feedback(
            "success",
            kit::tr("Sticker pack updated", "贴纸包配置已更新"),
            &if kit::is_zh() {
                format!("已设置贴纸包：{name}\n贴纸包链接：t.me/addstickers/{name}")
            } else {
                format!("Sticker pack: {name}\nPack link: t.me/addstickers/{name}")
            },
        ))
        .await
        .map_err(Into::into)
    }

    async fn save_sticker(&self, inv: &Invocation) -> CmdResult {
        let cfg = self.store.read()?;
        if cfg.sticker_set.trim().is_empty() {
            return Err(fail(if kit::is_zh() {
                format!("未配置贴纸包，请使用 {}yvlu config sticker 贴纸包名", inv.prefix())
            } else {
                format!(
                    "Sticker pack not configured; run {}yvlu config sticker pack_name",
                    inv.prefix()
                )
            }));
        }
        let Some(reply) = kit::reply_msg(inv).await? else {
            return Err(fail(kit::tr(
                "Reply to a sticker or image",
                "请回复一张贴纸或图片",
            )));
        };
        let bad_media = || {
            kit::tr(
                "Unsupported media; reply to a sticker or image",
                "不支持的媒体类型，请回复贴纸或图片",
            )
        };
        let upload_err = || kit::tr("Failed to upload sticker", "上传贴纸失败");
        let doc = if let Some(existing) = mibot_bot::document_of(&reply.raw)
            && is_sticker_document(&reply.raw)
        {
            existing
        } else {
            let file = inv
                .client
                .download_media(&reply.raw, 8 << 20)
                .await
                .map_err(|_| fail(bad_media()))?;
            let decoded = imaging_decode(&file.data).map_err(|_| fail(bad_media()))?;
            let resized = resize_sticker(&decoded)?;
            let png = encode_png(&resized)?;
            let uploaded = inv.client.upload_bytes("sticker.png", &png).await?;
            let msg_media = inv
                .client
                .invoke(&tl::functions::messages::UploadMedia {
                    business_connection_id: None,
                    peer: tl::enums::InputPeer::PeerSelf,
                    media: tl::types::InputMediaUploadedDocument {
                        nosound_video: false,
                        force_file: false,
                        spoiler: false,
                        file: uploaded,
                        thumb: None,
                        mime_type: "image/png".into(),
                        attributes: Vec::new(),
                        stickers: None,
                        video_cover: None,
                        video_timestamp: None,
                        ttl_seconds: None,
                    }
                    .into(),
                })
                .await?;
            let tl::enums::MessageMedia::Document(doc_m) = msg_media else {
                return Err(fail(upload_err()));
            };
            let Some(tl::enums::Document::Document(doc)) = doc_m.document else {
                return Err(fail(upload_err()));
            };
            tl::types::InputDocument {
                id: doc.id,
                access_hash: doc.access_hash,
                file_reference: doc.file_reference,
            }
            .into()
        };
        let item = tl::types::InputStickerSetItem {
            document: doc,
            emoji: "📝".into(),
            mask_coords: None,
            keywords: None,
        };
        let created = add_to_pack(&inv.client, &cfg.sticker_set, &cfg.sticker_set, item)
            .await
            .map_err(explain)?;
        let title = if created {
            kit::tr("Sticker pack created", "贴纸包已创建")
        } else {
            kit::tr("Sticker added", "贴纸已添加")
        };
        inv.edit(&kit::feedback(
            "success",
            title,
            &if kit::is_zh() {
                format!("贴纸包：t.me/addstickers/{}", cfg.sticker_set)
            } else {
                format!("Pack: t.me/addstickers/{}", cfg.sticker_set)
            },
        ))
        .await
        .map_err(Into::into)
    }

    async fn render(&self, payload: &QuotePayload) -> Result<(Vec<u8>, &'static str), CmdError> {
        let body = serde_json::to_vec(payload)?;
        let mut req = Request {
            method: Method::POST,
            url: self.quote_endpoint.clone(),
            body: Some(body),
            timeout: Some(Duration::from_secs(60)),
            max_bytes: Some(20 << 20),
            ..Default::default()
        };
        req.headers
            .insert("Content-Type".into(), "application/json".into());
        req.headers
            .insert("User-Agent".into(), QUOTE_USER_AGENT.into());
        let resp = do_request(req).await.map_err(|e| {
            fail(if kit::is_zh() {
                format!("quote 服务不可用：{}", httpx_reason(&e))
            } else {
                format!("Quote service unavailable: {}", httpx_reason(&e))
            })
        })?;
        if resp.status == 403 {
            return Err(fail(kit::tr(
                "Quote service rejected the request (403)",
                "quote 服务拒绝了请求（403），可能是它换了准入规则",
            )));
        }
        if !resp.ok() {
            return Err(fail(if kit::is_zh() {
                format!("语录渲染服务出错了（HTTP {}），稍后再试", resp.status)
            } else {
                format!("Quote service error (HTTP {}); try again later", resp.status)
            }));
        }
        if resp.body.len() >= 12 && &resp.body[0..4] == b"RIFF" && &resp.body[8..12] == b"WEBP" {
            return Ok((resp.body, "webp"));
        }
        if resp.body.len() >= 8 && &resp.body[1..4] == b"PNG" {
            return Ok((resp.body, "png"));
        }
        if resp.body.len() >= 4 && resp.body[0] == 0x1a && resp.body[1] == 0x45 {
            return Ok((resp.body, "webm"));
        }
        Err(fail(kit::tr(
            "Quote service returned non-image data",
            "quote 服务返回了非图片数据",
        )))
    }

    async fn build(
        &self,
        inv: &Invocation,
        reply: &Message,
        opts: &YvluOptions,
    ) -> Result<QuotePayload, CmdError> {
        let mut messages = vec![reply.clone()];
        if opts.count > 1
            && let Ok(collected) = self.following(inv, reply, opts.count).await
            && !collected.is_empty()
        {
            messages = collected;
        }
        let mut payload = QuotePayload {
            kind: opts.format.clone(),
            format: if opts.format == "quote" {
                "webp".into()
            } else {
                "png".into()
            },
            background_color: "#1b1429".into(),
            width: if opts.format == "stories" { 360 } else { 512 },
            height: if opts.format == "stories" { 640 } else { 768 },
            scale: 2,
            emoji_brand: "apple".into(),
            messages: Vec::new(),
        };

        let mut avatars: HashMap<i64, Option<QuotePhoto>> = HashMap::new();
        let mut previous: i64 = -1;

        for (idx, msg) in messages.iter().enumerate() {
            if inv.cancel.is_cancelled() {
                return Err(fail(kit::tr("Task cancelled", "任务已取消")));
            }
            let (mut author, face) = self.author(inv, msg, opts)?;
            let show = author.id != previous;
            previous = author.id;
            if show {
                let photo = if let Some(cached) = avatars.get(&author.id) {
                    cached.clone()
                } else {
                    let fetched = self.avatar(inv, face.as_ref()).await;
                    avatars.insert(author.id, fetched.clone());
                    fetched
                };
                author.photo = photo;
                if let Some(info) = inv.client.peers().user(author.id)
                    && !info.emoji_status.is_empty()
                {
                    author.emoji_status = info.emoji_status;
                }
            } else {
                author.name.clear();
                author.first_name.clear();
                author.last_name.clear();
                author.username.clear();
                author.emoji_status.clear();
            }

            let fake = idx == 0 && !opts.fake_text.is_empty();
            let (text, entities) = if fake {
                let full = inv.message.raw.message.as_str();
                let shift = if let Some(p) = full.strip_suffix(opts.fake_text.as_str()) {
                    utf16_len(p)
                } else {
                    0
                };
                (
                    opts.fake_text.clone(),
                    convert_entities(&opts.fake_ents, shift),
                )
            } else if idx == 0 && !inv.message.quote_text.is_empty() {
                (
                    inv.message.quote_text.clone(),
                    convert_entities(&inv.message.quote_entities, 0),
                )
            } else {
                let ents = msg
                    .raw
                    .entities
                    .as_deref()
                    .map(|e| convert_entities(e, 0))
                    .unwrap_or_default();
                (msg.content(), ents)
            };

            let mut item = QuoteMessage {
                from: author,
                text,
                entities,
                avatar: show,
                reply_message: None,
                media: None,
                media_type: String::new(),
                media_duration: 0,
                voice: None,
                document: None,
                forward: None,
                sender_tag: String::new(),
                audio: None,
            };

            if !fake {
                self.describe_media(inv, msg, &mut item).await;
                if let Some((from, _)) = forward_author(inv.client.peers(), msg) {
                    let label = if !from.name.is_empty() {
                        from.name
                    } else {
                        from.first_name
                    };
                    if !label.is_empty() {
                        item.forward = Some(QuoteFwd { label });
                    }
                }
            }
            let tag = self.sender_tag(inv, msg, item.from.id).await;
            if !tag.is_empty() {
                item.sender_tag = tag;
            }
            if opts.include_reply {
                item.reply_message = self.reply_block(inv, msg).await;
            }
            payload.messages.push(item);
        }

        if payload.messages.is_empty() {
            return Err(fail(kit::tr("No messages to quote", "没有可生成的消息")));
        }
        Ok(payload)
    }

    async fn following(
        &self,
        inv: &Invocation,
        reply: &Message,
        count: i32,
    ) -> Result<Vec<Message>, CmdError> {
        let peer = inv.client.input_peer(&reply.peer)?;
        let res = inv
            .client
            .invoke(&tl::functions::messages::GetHistory {
                peer,
                offset_id: reply.id,
                offset_date: 0,
                add_offset: -count,
                limit: count,
                max_id: 0,
                min_id: 0,
                hash: 0,
            })
            .await?;
        let found = kit::unpack_messages(res);
        let mut ordered = Vec::new();
        for item in found {
            let tl::enums::Message::Message(msg) = item else {
                continue;
            };
            if msg.id < reply.id || inv.registry.parse(&msg.message).is_some() {
                continue;
            }
            if let Some(env) = envelope(&msg, inv.client.self_id(), false, inv.client.peers()) {
                ordered.push(env);
            }
        }
        ordered.sort_by_key(|m| m.id);
        if ordered.first().is_none_or(|m| m.id != reply.id) {
            ordered.insert(0, reply.clone());
        }
        ordered.truncate(count as usize);
        Ok(ordered)
    }

    fn author(
        &self,
        inv: &Invocation,
        msg: &Message,
        opts: &YvluOptions,
    ) -> Result<(QuoteFrom, Option<tl::enums::InputPeer>), CmdError> {
        if let Some(fake) = &opts.fake_author {
            return Ok((fake.clone(), opts.fake_sender.clone()));
        }
        if let Some((from, origin)) = forward_author(inv.client.peers(), msg) {
            let face = origin.and_then(|p| inv.client.peers().input_peer(&p));
            return Ok((from, face));
        }
        let from = peer_author(inv.client.peers(), &msg.sender)
            .ok_or_else(|| fail(kit::tr("Failed to get message sender info", "无法获取消息发送者信息")))?;
        let face = inv.client.peers().input_peer(&msg.sender);
        Ok((from, face))
    }

    async fn avatar(
        &self,
        inv: &Invocation,
        peer: Option<&tl::enums::InputPeer>,
    ) -> Option<QuotePhoto> {
        let peer = peer?;
        let key = avatar_key(&inv.client, peer);
        if !key.is_empty()
            && let Some(cached) = self.avatars.lock().unwrap_or_else(|e| e.into_inner()).get(&key)
        {
            return cached;
        }
        let res = tokio::time::timeout(AVATAR_BUDGET, async {
            let small = inv
                .client
                .download_profile_photo(peer, false, 2 << 20)
                .await
                .ok()
                .flatten()
                .filter(|d| !d.is_empty());
            if let Some(d) = small {
                return Some(d);
            }
            inv.client
                .download_profile_photo(peer, true, 2 << 20)
                .await
                .ok()
                .flatten()
                .filter(|d| !d.is_empty())
        })
        .await
        .ok()??;

        let decoded = imaging_decode(&res).ok()?;
        let resized = resize_cover(&decoded, 256, 256).ok()?;
        let flat = flatten(&resized, 0, 0, 0).ok()?;
        let png = encode_png(&flat).ok()?;
        let photo = QuotePhoto {
            url: format!("data:image/png;base64,{}", BASE64.encode(&png)),
        };
        if !key.is_empty() {
            self.avatars
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, Some(photo.clone()));
        }
        Some(photo)
    }

    async fn describe_media(&self, inv: &Invocation, msg: &Message, item: &mut QuoteMessage) {
        let Some(media) = &msg.raw.media else {
            return;
        };
        let _ = tokio::time::timeout(MEDIA_BUDGET, async {
            match media {
                tl::enums::MessageMedia::Photo(_) => {
                    if let Ok(file) = inv.client.download_media(&msg.raw, 8 << 20).await {
                        item.media = Some(QuotePhoto {
                            url: format!(
                                "data:image/jpeg;base64,{}",
                                BASE64.encode(&file.data)
                            ),
                        });
                    }
                }
                tl::enums::MessageMedia::WebPage(wp) => {
                    if let Some(photo_msg) = web_page_photo(&msg.raw, wp)
                        && let Ok(file) = inv.client.download_media(&photo_msg, 8 << 20).await
                    {
                        item.media = Some(QuotePhoto {
                            url: format!(
                                "data:image/jpeg;base64,{}",
                                BASE64.encode(&file.data)
                            ),
                        });
                    }
                }
                tl::enums::MessageMedia::Document(d) => {
                    if let Some(tl::enums::Document::Document(doc)) = &d.document {
                        self.describe_document(inv, msg, doc, item).await;
                    }
                }
                _ => {}
            }
        })
        .await;
    }

    async fn describe_document(
        &self,
        inv: &Invocation,
        msg: &Message,
        doc: &tl::types::Document,
        item: &mut QuoteMessage,
    ) {
        let mut is_sticker = false;
        let mut animated = false;
        let mut video: Option<&tl::types::DocumentAttributeVideo> = None;
        let mut audio: Option<&tl::types::DocumentAttributeAudio> = None;
        let mut file_name = "file".to_owned();
        for attr in &doc.attributes {
            match attr {
                tl::enums::DocumentAttribute::Sticker(_) => is_sticker = true,
                tl::enums::DocumentAttribute::Animated => animated = true,
                tl::enums::DocumentAttribute::Video(v) => video = Some(v),
                tl::enums::DocumentAttribute::Audio(a) => audio = Some(a),
                tl::enums::DocumentAttribute::Filename(f) => file_name = f.file_name.clone(),
                _ => {}
            }
        }
        if is_sticker {
            return;
        }
        if let Some(a) = audio
            && !a.voice
        {
            let mut title = a.title.clone().unwrap_or_default();
            if title.is_empty() {
                title = file_name;
            }
            if title.is_empty() || title == "file" {
                title = "Audio".into();
            }
            item.audio = Some(QuoteAudio {
                title,
                performer: a.performer.clone().unwrap_or_default(),
                duration: a.duration,
            });
            return;
        }
        if let Some(a) = audio
            && a.voice
        {
            let Some(waveform) = &a.waveform else {
                return;
            };
            if waveform.is_empty() {
                return;
            }
            item.voice = Some(QuoteVoice {
                waveform: decode_waveform(waveform),
                duration: a.duration,
            });
            return;
        }
        if doc.mime_type.starts_with("image/") {
            if let Ok(file) = inv.client.download_media(&msg.raw, 8 << 20).await {
                item.media = Some(QuotePhoto {
                    url: format!(
                        "data:{};base64,{}",
                        doc.mime_type,
                        BASE64.encode(&file.data)
                    ),
                });
            }
            return;
        }
        if animated || video.is_some() {
            item.media_type = if animated { "gif" } else { "video" }.into();
            if let Some(v) = video {
                item.media_duration = v.duration as i32;
            }
            self.embed_video(inv, msg, doc, item).await;
            return;
        }
        item.document = Some(QuoteDoc { file_name });
    }

    async fn embed_video(
        &self,
        inv: &Invocation,
        msg: &Message,
        doc: &tl::types::Document,
        item: &mut QuoteMessage,
    ) {
        if doc.size > (8 << 20) || find_ffmpeg().is_err() {
            return;
        }
        let Ok(file) = inv.client.download_media(&msg.raw, 8 << 20).await else {
            return;
        };
        if doc.mime_type == "video/webm" {
            item.media = Some(QuotePhoto {
                url: format!("data:video/webm;base64,{}", BASE64.encode(&file.data)),
            });
            return;
        }
        match to_sticker_webm(&file.data, extension_for(&doc.mime_type)).await {
            Ok(converted) => {
                item.media = Some(QuotePhoto {
                    url: format!("data:video/webm;base64,{}", BASE64.encode(&converted)),
                });
            }
            Err(e) => self.a.logs.record(
                Level::Info,
                "yvlu.video_skipped",
                &[("error", &e.to_string())],
            ),
        }
    }

    async fn reply_block(&self, inv: &Invocation, msg: &Message) -> Option<QuoteReply> {
        if msg.reply_to_id == 0 {
            return None;
        }
        let replied = inv.client.get_reply(msg).await.ok()??;
        let (text, entities): (String, Vec<tl::enums::MessageEntity>) =
            if !msg.quote_text.is_empty() {
                (msg.quote_text.clone(), msg.quote_entities.clone())
            } else {
                (
                    replied.content(),
                    replied.raw.entities.clone().unwrap_or_default(),
                )
            };
        if text.trim().is_empty() {
            return None;
        }
        let mut block = QuoteReply {
            name: "unknown".into(),
            text,
            entities: convert_entities(&entities, 0),
            chat_id: 0,
        };
        match &replied.sender {
            tl::enums::Peer::User(u) => {
                if let Some(info) = inv.client.peers().user(u.user_id) {
                    let full = format!("{} {}", info.first_name.trim(), info.last_name.trim())
                        .trim()
                        .to_owned();
                    if !full.is_empty() {
                        block.name = full;
                    } else if !info.username.is_empty() {
                        block.name = info.username;
                    }
                }
                block.chat_id = u.user_id;
            }
            tl::enums::Peer::Channel(ch) => {
                if let Some(info) = inv.client.peers().channel(ch.channel_id)
                    && !info.title.is_empty()
                {
                    block.name = info.title;
                }
                block.chat_id = ch.channel_id;
            }
            _ => {}
        }
        Some(block)
    }

    async fn sender_tag(&self, inv: &Invocation, msg: &Message, author_id: i64) -> String {
        let Ok(res) = tokio::time::timeout(TAG_BUDGET, async {
            let chat = inv.client.input_peer(&msg.peer).ok()?;
            let channel = kit::input_channel(&chat)?;
            let participant = inv
                .client
                .peers()
                .input_peer(&tl::types::PeerUser { user_id: author_id }.into())?;
            inv.client
                .invoke(&tl::functions::channels::GetParticipant {
                    channel: channel.into(),
                    participant,
                })
                .await
                .ok()
        })
        .await
        else {
            return String::new();
        };
        let Some(tl::enums::channels::ChannelParticipant::Participant(p)) = res else {
            return String::new();
        };
        inv.client.peers().remember_users(&p.users);
        match p.participant {
            tl::enums::ChannelParticipant::Creator(c) => {
                c.rank.unwrap_or_default().trim().to_owned()
            }
            tl::enums::ChannelParticipant::Admin(a) => {
                a.rank.unwrap_or_default().trim().to_owned()
            }
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_inv(raw_args: &str, text: &str, ents: Vec<tl::enums::MessageEntity>) -> Invocation {
        let mut raw = kit::blank_tl_message(
            1,
            tl::types::PeerChat { chat_id: 1 }.into(),
        );
        raw.message = text.to_owned();
        raw.entities = Some(ents);
        let msg = Message {
            id: 1,
            peer: tl::types::PeerChat { chat_id: 1 }.into(),
            chat_id: "-1".into(),
            chat_type: mibot_bot::ChatType::Group,
            text: text.to_owned(),
            out: true,
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
            raw,
        };
        let args: Vec<String> = raw_args.split_whitespace().map(str::to_owned).collect();
        Invocation {
            client: Client::from_transport(
                Arc::new(DummyTransport),
                Arc::new(PeerCache::new()),
                Arc::new(mibot_bot::SentRecorder::new(Arc::new(|_, _| {}))),
            )
            .into(),
            registry: Arc::new(mibot_command::Registry::new()),
            message: msg,
            prefix: ".".into(),
            command: "yvlu".into(),
            args,
            raw: raw_args.to_owned(),
            cancel: tokio_util::sync::CancellationToken::new(),
            foreign: false,
        }
    }

    struct DummyTransport;
    impl mibot_bot::RpcTransport for DummyTransport {
        fn invoke_raw<'a>(
            &'a self,
            _dc: Option<i32>,
            _id: u32,
            _body: Vec<u8>,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>, grammers_mtsender::InvocationError>>
        {
            Box::pin(async {
                Err(grammers_mtsender::InvocationError::Dropped("dummy"))
            })
        }
    }

    #[test]
    fn test_parse_yvlu_and_entities() {
        let inv = test_inv("r 3", ".yvlu r 3", Vec::new());
        let opts = parse_yvlu(&inv).expect("opts");
        assert!(opts.include_reply);
        assert_eq!(opts.count, 3);
        assert_eq!(opts.format, "quote");

        let inv_img = test_inv("png 2", ".yvlu png 2", Vec::new());
        let opts_img = parse_yvlu(&inv_img).expect("opts_img");
        assert_eq!(opts_img.format, "image");
        assert_eq!(opts_img.count, 2);

        let inv_fake = test_inv(
            "f hello world",
            ".yvlu f hello world",
            vec![
                tl::types::MessageEntityBold {
                    offset: 8,
                    length: 5,
                }
                .into(),
                tl::types::MessageEntityBold {
                    offset: 1,
                    length: 2,
                }
                .into(),
            ],
        );
        let opts_fake = parse_yvlu(&inv_fake).expect("opts_fake");
        assert_eq!(opts_fake.fake_text, "hello world");
        let conv = convert_entities(&opts_fake.fake_ents, 8);
        assert_eq!(conv.len(), 1);
        assert_eq!(conv[0].offset, 0);
        assert_eq!(conv[0].length, 5);
        assert_eq!(conv[0].kind, "bold");
    }

    #[test]
    fn test_decode_waveform_and_photo_fits() {
        assert_eq!(decode_waveform(&[0b010_11111]), vec![31]);
        let img = image::RgbaImage::new(200, 200);
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("png");
        assert!(photo_fits(&png));
    }
}
