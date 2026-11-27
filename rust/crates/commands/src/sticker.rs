use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BotError, Client};
use mibot_command::{CmdError, Invocation, code, escape, group};
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Settings {
    #[serde(default, rename = "sticker_default_pack", skip_serializing_if = "String::is_empty")]
    default_pack: String,
}

const FALLBACK_EMOJIS: &[&str] = &[
    "😀", "😁", "😂", "🤣", "😊", "😇", "🙂", "😉", "😋", "😎", "😍", "😘", "😜", "🤗", "🤔",
    "😴", "😌", "😅", "😆", "😄",
];

const AUTO_PACKS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Kind {
    suffix: &'static str,
    label: &'static str,
}

const STATIC_KIND: Kind = Kind {
    suffix: "_static",
    label: "静态",
};
const ANIMATED_KIND: Kind = Kind {
    suffix: "_animated",
    label: "动态",
};
const VIDEO_KIND: Kind = Kind {
    suffix: "_video",
    label: "视频",
};

#[derive(Debug, Clone)]
struct Found {
    document: tl::enums::InputDocument,
    emoji: String,
    kind: Kind,
}

pub const PACK_NAME_RULE: &str = "贴纸包名只能用字母、数字和下划线，字母开头，最长 64 个字符";

pub fn valid_pack_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    for (idx, b) in name.bytes().enumerate() {
        let letter = b.is_ascii_alphabetic();
        if idx == 0 && !letter {
            return false;
        }
        if !letter && !b.is_ascii_digit() && b != b'_' {
            return false;
        }
    }
    true
}

fn sticker_of(message: &tl::types::Message) -> Option<Found> {
    let tl::enums::MessageMedia::Document(media) = message.media.as_ref()? else {
        return None;
    };
    let tl::enums::Document::Document(raw) = media.document.as_ref()? else {
        return None;
    };
    let alt = raw.attributes.iter().find_map(|attr| match attr {
        tl::enums::DocumentAttribute::Sticker(s) => Some(s.alt.trim().to_owned()),
        _ => None,
    })?;
    let document: tl::enums::InputDocument = tl::types::InputDocument {
        id: raw.id,
        access_hash: raw.access_hash,
        file_reference: raw.file_reference.clone(),
    }
    .into();
    let emoji = if alt.is_empty() {
        FALLBACK_EMOJIS[(mibot_bot::random_id().unsigned_abs() as usize) % FALLBACK_EMOJIS.len()]
            .to_owned()
    } else {
        alt
    };
    let kind = match raw.mime_type.as_str() {
        "application/x-tgsticker" => ANIMATED_KIND,
        "video/webm" => VIDEO_KIND,
        _ => STATIC_KIND,
    };
    Some(Found {
        document,
        emoji,
        kind,
    })
}

fn own_username(self_user: Option<&tl::types::User>) -> String {
    let Some(u) = self_user else {
        return String::new();
    };
    if let Some(ref name) = u.username
        && !name.is_empty()
    {
        return name.clone();
    }
    if let Some(ref list) = u.usernames {
        for tl::enums::Username::Username(item) in list {
            if item.active {
                return item.username.clone();
            }
        }
    }
    String::new()
}

pub fn explain(err: BotError) -> CmdError {
    for (rpc_code, text) in [
        ("STICKERS_TOO_MUCH", "贴纸包已满"),
        ("STICKERPACK_STICKERS_TOO_MUCH", "贴纸包已满"),
        ("STICKER_VIDEO_LONG", "视频贴纸不能超过 3 秒"),
        ("STICKER_PNG_DIMENSIONS", "静态贴纸要有一边正好 512 像素"),
        (
            "STICKERSET_INVALID",
            "贴纸包名无效、已被别人占用，或者不是你建的",
        ),
        ("SHORTNAME_OCCUPY_FAILED", "贴纸包名已被占用"),
        ("STICKER_EMOJI_INVALID", "这张贴纸的 emoji 无效"),
        ("PACK_SHORT_NAME_INVALID", PACK_NAME_RULE),
        ("PACK_SHORT_NAME_OCCUPIED", "贴纸包名已被占用"),
    ] {
        if err.is_rpc(rpc_code) {
            return kit::fail(text);
        }
    }
    if let Some(wait) = err.flood_wait() {
        return kit::fail(format!("请求太频繁，{} 秒后再试", wait.as_secs()));
    }
    err.into()
}

fn is_full(err: &BotError) -> bool {
    err.is_rpc("STICKERS_TOO_MUCH") || err.is_rpc("STICKERPACK_STICKERS_TOO_MUCH")
}

async fn exists(client: &Client, name: &str) -> Result<bool, BotError> {
    let req = tl::functions::messages::GetStickerSet {
        stickerset: tl::types::InputStickerSetShortName {
            short_name: name.to_owned(),
        }
        .into(),
        hash: 0,
    };
    match client.invoke(&req).await {
        Ok(_) => Ok(true),
        Err(e) if e.is_rpc("STICKERSET_INVALID") => Ok(false),
        Err(e) => Err(e),
    }
}

pub async fn add_to_pack(
    client: &Client,
    name: &str,
    title: &str,
    item: tl::types::InputStickerSetItem,
) -> Result<bool, BotError> {
    if exists(client, name).await? {
        client
            .invoke(&tl::functions::stickers::AddStickerToSet {
                stickerset: tl::types::InputStickerSetShortName {
                    short_name: name.to_owned(),
                }
                .into(),
                sticker: item.into(),
            })
            .await?;
        Ok(false)
    } else {
        client
            .invoke(&tl::functions::stickers::CreateStickerSet {
                masks: false,
                emojis: false,
                text_color: false,
                user_id: tl::enums::InputUser::UserSelf,
                title: title.to_owned(),
                short_name: name.to_owned(),
                thumb: None,
                stickers: vec![item.into()],
                software: None,
            })
            .await?;
        Ok(true)
    }
}

async fn save(
    client: &Client,
    prefix: &str,
    target: &str,
    sticker: &Found,
) -> Result<(String, bool), CmdError> {
    let item = tl::types::InputStickerSetItem {
        document: sticker.document.clone(),
        emoji: sticker.emoji.clone(),
        mask_coords: None,
        keywords: None,
    };
    let self_user = client.self_user();
    let username = own_username(self_user.as_ref());
    let mut title = format!("@{username} 的收藏（{}）", sticker.kind.label);
    if !target.is_empty() {
        if username.is_empty() {
            title = target.to_owned();
        }
        let created = add_to_pack(client, target, &title, item)
            .await
            .map_err(explain)?;
        return Ok((target.to_owned(), created));
    }
    if username.is_empty() {
        return Err(kit::fail(format!(
            "账号没有用户名，没法自动给贴纸包起名。先用 {prefix}sticker 包名 设一个默认包"
        )));
    }
    for idx in 1..=AUTO_PACKS {
        let name = format!("{username}{}_{idx}", sticker.kind.suffix);
        match add_to_pack(client, &name, &title, item.clone()).await {
            Ok(created) => return Ok((name, created)),
            Err(e) if is_full(&e) => continue,
            Err(e) => return Err(explain(e)),
        }
    }
    Err(kit::fail(format!(
        "自动命名的 {AUTO_PACKS} 个贴纸包都满了，用 {prefix}sticker to 包名 存到别的包"
    )))
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "⭐ <b>收藏贴纸</b>\n\n\
        把别人的贴纸存进你自己的贴纸包，静态、动态、视频贴纸都行。\n\n\
        • 回复一个贴纸发 <code>{p}sticker</code> 存到默认包；没设默认包就存到自动命名的 \
        <code>用户名_static_1</code> 这类包里，满了自动换下一个\n\
        • 回复贴纸发 <code>{p}sticker to 包名</code> 这一次存到指定的包\n\
        • <code>{p}sticker 包名</code> 设默认包，包不存在的话第一次存时新建\n\
        • <code>{p}sticker cancel</code> 取消默认包\n\
        • 不回复发 <code>{p}sticker</code> 看当前设置\n\n\
        包名只能用字母、数字和下划线，字母开头，最长 64 个字符。贴纸没带 emoji 时随机配一个。"
    )
}

fn pack_link(name: &str) -> String {
    let esc = escape(name);
    format!(r#"<a href="https://t.me/addstickers/{esc}">{esc}</a>"#)
}

pub fn register(a: &Arc<App>) {
    let saved = Arc::new(kit::new_store(a, "sticker.json", Settings::default));
    let mut cmd = kit::make_cmd(
        "sticker",
        &[],
        group::MEDIA,
        "把贴纸存进自己的贴纸包",
        "[to 包名|包名|cancel]",
        help,
        move |inv| {
            let saved = Arc::clone(&saved);
            Box::pin(async move {
                let sub = inv.arg(0).to_ascii_lowercase();
                let reply = kit::reply_msg(&inv).await?;
                let sticker = reply.as_ref().and_then(|r| sticker_of(&r.raw));
                let Some(sticker) = sticker else {
                    if reply.is_some() && sub.is_empty() {
                        return Err(kit::fail("回复的不是贴纸"));
                    }
                    return configure(&inv, &saved).await;
                };
                let current = saved.read()?;
                let mut target = current.default_pack;
                if sub == "to" {
                    target = inv.arg(1).to_owned();
                    if !valid_pack_name(&target) {
                        return Err(kit::fail(PACK_NAME_RULE));
                    }
                }
                inv.edit_text(&kit::working("正在收藏")).await?;
                let (name, created) =
                    save(&inv.client, inv.prefix(), &target, &sticker).await?;
                let verb = if created {
                    "新建了贴纸包并存进"
                } else {
                    "已存进"
                };
                inv.edit(&format!("✅ {verb} {}", pack_link(&name)))
                    .await?;
                if kit::sleep(&inv.cancel, Duration::from_secs(5)).await {
                    let _ = inv.delete().await;
                }
                Ok(())
            })
        },
    );
    cmd.timeout = Some(Duration::from_secs(120));
    a.registry.register(cmd);
}

async fn configure(inv: &Invocation, saved: &Store<Settings>) -> Result<(), CmdError> {
    let argument = inv.arg(0);
    if argument.is_empty() {
        let current = saved.read()?;
        let self_user = inv.client.self_user();
        let username = own_username(self_user.as_ref());
        let mut text = String::from("⭐ <b>收藏贴纸</b>\n\n");
        if !current.default_pack.is_empty() {
            text.push_str("默认贴纸包：");
            text.push_str(&pack_link(&current.default_pack));
        } else if !username.is_empty() {
            text.push_str(&format!(
                "没设默认包，会存进 {} 这类自动命名的包",
                code(&format!("{username}_static_1"))
            ));
        } else {
            text.push_str(&format!(
                "没设默认包，账号也没有用户名，收藏前要先用 {} 设一个",
                code(&format!("{}sticker 包名", inv.prefix()))
            ));
        }
        text.push_str(&format!(
            "\n\n回复一个贴纸发 {} 收藏。",
            code(&format!("{}sticker", inv.prefix()))
        ));
        return inv.edit(&text).await.map_err(Into::into);
    }
    if argument.eq_ignore_ascii_case("cancel") {
        kit::update_ok(saved, |v| v.default_pack.clear())?;
        return inv.edit_text("✅ 已取消默认贴纸包").await.map_err(Into::into);
    }
    if argument.eq_ignore_ascii_case("to") {
        return Err(kit::fail("要回复一个贴纸再用 to"));
    }
    if !valid_pack_name(argument) {
        return Err(kit::fail(PACK_NAME_RULE));
    }
    let present = exists(&inv.client, argument)
        .await
        .map_err(|e| kit::fail_with("查不了这个贴纸包", e))?;
    let pack = argument.to_owned();
    kit::update_ok(saved, move |v| v.default_pack = pack)?;
    let note = if present {
        "，要是这个包不是你建的，收藏时会失败"
    } else {
        "，第一次收藏时新建"
    };
    inv.edit(&format!("✅ 默认贴纸包已设为 {}{note}", code(argument)))
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{PeerCache, RpcTransport, SentRecorder};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Packs {
        full_code: &'static str,
        count: Mutex<HashMap<String, i32>>,
        capacity: i32,
        not_mine: HashSet<String>,
        calls: Mutex<Vec<String>>,
    }

    impl Packs {
        fn reply_set(&self, name: &str) -> Vec<u8> {
            let cnt = self.count.lock().unwrap().get(name).copied().unwrap_or(0);
            tl::enums::messages::StickerSet::Set(tl::types::messages::StickerSet {
                set: tl::types::StickerSet {
                    archived: false,
                    official: false,
                    masks: false,
                    emojis: false,
                    text_color: false,
                    channel_emoji_status: false,
                    creator: true,
                    installed_date: None,
                    id: 1,
                    access_hash: 1,
                    title: name.to_owned(),
                    short_name: name.to_owned(),
                    thumbs: None,
                    thumb_dc_id: None,
                    thumb_version: None,
                    thumb_document_id: None,
                    count: cnt,
                    hash: 0,
                }
                .into(),
                packs: vec![],
                keywords: vec![],
                documents: vec![],
            })
            .to_bytes()
        }
    }

    fn rpc_err(name: &str) -> InvocationError {
        InvocationError::Rpc(RpcError {
            code: 400,
            name: name.into(),
            value: None,
            caused_by: None,
        })
    }

    impl RpcTransport for Packs {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            cid: u32,
            body: Vec<u8>,
        ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                let payload = &body[4..];
                match cid {
                    tl::functions::messages::GetStickerSet::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::GetStickerSet::from_bytes(payload)
                            .unwrap();
                        let tl::enums::InputStickerSet::ShortName(sn) = req.stickerset else {
                            return Err(rpc_err("STICKERSET_INVALID"));
                        };
                        self.calls.lock().unwrap().push(format!("get {}", sn.short_name));
                        if !self.count.lock().unwrap().contains_key(&sn.short_name) {
                            return Err(rpc_err("STICKERSET_INVALID"));
                        }
                        Ok(self.reply_set(&sn.short_name))
                    }
                    tl::functions::stickers::AddStickerToSet::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::stickers::AddStickerToSet::from_bytes(payload).unwrap();
                        let tl::enums::InputStickerSet::ShortName(sn) = req.stickerset else {
                            return Err(rpc_err("STICKERSET_INVALID"));
                        };
                        let tl::enums::InputStickerSetItem::Item(item) = req.sticker;
                        self.calls
                            .lock()
                            .unwrap()
                            .push(format!("add {} {}", sn.short_name, item.emoji));
                        if self.not_mine.contains(&sn.short_name) {
                            return Err(rpc_err("STICKERSET_INVALID"));
                        }
                        {
                            let mut map = self.count.lock().unwrap();
                            let c = map.get(&sn.short_name).copied().unwrap_or(0);
                            if c >= self.capacity {
                                let code = if self.full_code.is_empty() {
                                    "STICKERS_TOO_MUCH"
                                } else {
                                    self.full_code
                                };
                                return Err(rpc_err(code));
                            }
                            map.insert(sn.short_name.clone(), c + 1);
                        }
                        Ok(self.reply_set(&sn.short_name))
                    }
                    tl::functions::stickers::CreateStickerSet::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::stickers::CreateStickerSet::from_bytes(payload).unwrap();
                        self.calls.lock().unwrap().push(format!(
                            "create {} {:?}",
                            req.short_name, req.title
                        ));
                        self.count.lock().unwrap().insert(req.short_name.clone(), 1);
                        Ok(self.reply_set(&req.short_name))
                    }
                    _ => Err(InvocationError::Dropped("UNEXPECTED_REQUEST")),
                }
            })
        }
    }

    fn make_client(packs: Arc<Packs>, username: &str) -> Arc<Client> {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Client::from_transport(
            packs,
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        );
        let mut self_user = kit::blank_tl_user(1);
        self_user.is_self = true;
        if !username.is_empty() {
            self_user.username = Some(username.to_owned());
        }
        client.set_self_user(self_user);
        Arc::new(client)
    }

    fn static_sticker() -> Found {
        Found {
            document: tl::types::InputDocument {
                id: 9,
                access_hash: 0,
                file_reference: vec![],
            }
            .into(),
            emoji: "😺".into(),
            kind: STATIC_KIND,
        }
    }

    #[tokio::test]
    async fn test_save_finds_or_creates_a_pack() {
        #[allow(clippy::type_complexity)]
        let cases: Vec<(
            &str,
            Vec<(&str, i32)>,
            Vec<&str>,
            &str,
            &str,
            Found,
            &str,
            bool,
            &str,
            Vec<&str>,
        )> = vec![
            (
                "没有包就新建第一个",
                vec![],
                vec![],
                "cat",
                "",
                static_sticker(),
                "cat_static_1",
                true,
                "",
                vec![
                    "get cat_static_1",
                    r#"create cat_static_1 "@cat 的收藏（静态）""#,
                ],
            ),
            (
                "满了换下一个",
                vec![("cat_static_1", 3), ("cat_static_2", 1)],
                vec![],
                "cat",
                "",
                static_sticker(),
                "cat_static_2",
                false,
                "",
                vec![
                    "get cat_static_1",
                    "add cat_static_1 😺",
                    "get cat_static_2",
                    "add cat_static_2 😺",
                ],
            ),
            (
                "视频贴纸进视频包",
                vec![],
                vec![],
                "cat",
                "",
                Found {
                    document: tl::types::InputDocument {
                        id: 9,
                        access_hash: 0,
                        file_reference: vec![],
                    }
                    .into(),
                    emoji: "🎬".into(),
                    kind: VIDEO_KIND,
                },
                "cat_video_1",
                true,
                "",
                vec![
                    "get cat_video_1",
                    r#"create cat_video_1 "@cat 的收藏（视频）""#,
                ],
            ),
            (
                "指定的包存在就加进去",
                vec![("Mine", 1)],
                vec![],
                "cat",
                "Mine",
                static_sticker(),
                "Mine",
                false,
                "",
                vec!["get Mine", "add Mine 😺"],
            ),
            (
                "指定的包不存在就新建",
                vec![],
                vec![],
                "",
                "Mine",
                static_sticker(),
                "Mine",
                true,
                "",
                vec!["get Mine", r#"create Mine "Mine""#],
            ),
            (
                "指定的包满了直接报满",
                vec![("Mine", 3)],
                vec![],
                "cat",
                "Mine",
                static_sticker(),
                "",
                false,
                "贴纸包已满",
                vec!["get Mine", "add Mine 😺"],
            ),
            (
                "别人的包",
                vec![("Theirs", 1)],
                vec!["Theirs"],
                "cat",
                "Theirs",
                static_sticker(),
                "",
                false,
                "不是你建的",
                vec!["get Theirs", "add Theirs 😺"],
            ),
            (
                "没用户名又没指定包",
                vec![],
                vec![],
                "",
                "",
                static_sticker(),
                "",
                false,
                "没有用户名",
                vec![],
            ),
        ];

        for (
            label,
            existing,
            not_mine,
            username,
            target,
            sticker,
            want_pack,
            want_created,
            failure,
            want_calls,
        ) in cases
        {
            let packs = Arc::new(Packs {
                full_code: "",
                count: Mutex::new(
                    existing
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v))
                        .collect(),
                ),
                capacity: 3,
                not_mine: not_mine.into_iter().map(str::to_owned).collect(),
                calls: Mutex::new(Vec::new()),
            });
            let c = make_client(Arc::clone(&packs), username);
            let res = save(&c, ".", target, &sticker).await;
            if !failure.is_empty() {
                let err = res.unwrap_err();
                assert!(kit::is_user_error(err.as_ref()), "{label}: {err}");
                assert!(err.to_string().contains(failure), "{label}: {err}");
            } else {
                let (pack, created) = res.unwrap();
                assert_eq!(pack, want_pack, "{label}");
                assert_eq!(created, want_created, "{label}");
            }
            let got_calls = packs.calls.lock().unwrap().clone();
            let want_owned: Vec<String> = want_calls.into_iter().map(str::to_owned).collect();
            assert_eq!(got_calls, want_owned, "{label}");
        }
    }

    #[tokio::test]
    async fn test_save_skips_full_pack_with_either_code() {
        let packs = Arc::new(Packs {
            full_code: "STICKERPACK_STICKERS_TOO_MUCH",
            count: Mutex::new(HashMap::from([("cat_static_1".into(), 3)])),
            capacity: 3,
            not_mine: HashSet::new(),
            calls: Mutex::new(Vec::new()),
        });
        let c = make_client(packs, "cat");
        let (pack, created) = save(&c, ".", "", &static_sticker()).await.unwrap();
        assert_eq!(pack, "cat_static_2");
        assert!(created);
    }

    #[test]
    fn test_sticker_of() {
        let make_msg = |mime: &str, alt: &str, sticker: bool| {
            let mut attrs: Vec<tl::enums::DocumentAttribute> = vec![
                tl::types::DocumentAttributeFilename {
                    file_name: "x".into(),
                }
                .into(),
            ];
            if sticker {
                attrs.push(
                    tl::types::DocumentAttributeSticker {
                        mask: false,
                        alt: alt.into(),
                        stickerset: tl::enums::InputStickerSet::Empty,
                        mask_coords: None,
                    }
                    .into(),
                );
            }
            let mut msg = kit::blank_tl_message(1, tl::types::PeerUser { user_id: 1 }.into());
            msg.media = Some(
                tl::types::MessageMediaDocument {
                    nopremium: false,
                    spoiler: false,
                    video: false,
                    round: false,
                    voice: false,
                    document: Some(
                        tl::types::Document {
                            id: 5,
                            access_hash: 6,
                            file_reference: vec![],
                            date: 0,
                            mime_type: mime.into(),
                            size: 0,
                            thumbs: None,
                            video_thumbs: None,
                            dc_id: 2,
                            attributes: attrs,
                        }
                        .into(),
                    ),
                    alt_documents: None,
                    video_cover: None,
                    video_timestamp: None,
                    ttl_seconds: None,
                }
                .into(),
            );
            msg
        };
        for (mime, want_kind) in [
            ("image/webp", STATIC_KIND),
            ("application/x-tgsticker", ANIMATED_KIND),
            ("video/webm", VIDEO_KIND),
        ] {
            let got = sticker_of(&make_msg(mime, "🐱", true)).unwrap();
            assert_eq!(got.kind, want_kind);
            assert_eq!(got.emoji, "🐱");
            assert!(matches!(got.document, tl::enums::InputDocument::Document(d) if d.id == 5));
        }
        let rand_emoji = sticker_of(&make_msg("image/webp", "", true)).unwrap();
        assert!(FALLBACK_EMOJIS.contains(&rand_emoji.emoji.as_str()));
        assert!(sticker_of(&make_msg("image/png", "", false)).is_none());
        assert!(
            sticker_of(&kit::blank_tl_message(
                1,
                tl::types::PeerUser { user_id: 1 }.into()
            ))
            .is_none()
        );
    }

    #[test]
    fn test_valid_pack_name() {
        for good in ["a", "Cat_static_1", "x9"] {
            assert!(valid_pack_name(good), "{good}");
        }
        for bad in ["", "1abc", "_a", "a-b", "a b", "猫", &"a".repeat(65)] {
            assert!(!valid_pack_name(bad), "{bad}");
        }
    }
}
