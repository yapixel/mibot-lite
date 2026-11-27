use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::MediaOptions;
use mibot_command::{CmdError, CmdResult, Invocation, code, escape, group};
use mibot_httpx::{Method, Request, Response, do_request};
use mibot_media::{Tags, tagged_mp3, voice_ogg};
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

const ENDPOINT: &str = "https://api.fish.audio/v1/tts";
const MAX_TEXT: usize = 1000;
const DEFAULT_ROLE: &str = "雷军";

const BUILTIN_ROLES: &[(&str, &str)] = &[
    ("薯薯", "cc1c9874effe4526883662166456513c"),
    ("麦当劳", "4066d617322e41abb30ed70eaeaf273f"),
    ("影视飓风", "91648d8a8d9841c5a1c54fb18e54ab04"),
    ("丁真", "54a5170264694bfc8e9ad98df7bd89c3"),
    ("雷军", "aebaa2305aa2452fbdc8f41eec852a79"),
    ("蔡徐坤", "e4642e5edccd4d9ab61a69e82d4f8a14"),
    ("邓紫棋", "3b55b3d84d2f453a98d8ca9bb24182d6"),
    ("周杰伦", "1512d05841734931bf905d0520c272b1"),
    ("周星驰", "faa3273e5013411199abc13d8f3d6445"),
    ("孙笑川", "e80ea225770f42f79d50aa98be3cedfc"),
    ("央视配音", "59cb5986671546eaa6ca8ae6f29f6d22"),
    ("阿诺", "daeda14f742f47b8ac243ccf21c62df8"),
    ("卢本伟", "24d524b57c5948f598e9b74c4dacc7ab"),
    ("电棍", "25d496c425d14109ba4958b6e47ea037"),
    ("炫狗", "b48533d37bed4ef4b9ad5b11d8b0b694"),
    ("阿梓", "c2a6125240f343498e26a9cf38db87b7"),
    ("七海", "a7725771e0974eb5a9b044ba357f6e13"),
    ("嘉然", "1d11381f42b54487b895486f69fb14fb"),
    ("东雪莲", "7af4d620be1c4c6686132f21940d51c5"),
    ("永雏塔菲", "e1cfccf59a1c4492b5f51c7c62a8abd2"),
    ("可莉", "626bb6d3f3364c9cbc3aa6a67300a664"),
    ("刻晴", "5611bf78886a4a9998f56538c4ec7d8c"),
    ("烧姐姐", "60d377ebaae44829ad4425033b94fdea"),
    ("AD学姐", "7f92f8afb8ec43bf81429cc1c9199cb1"),
    ("御姐", "f44181a3d6d444beae284ad585a1af37"),
    ("台湾女", "e855dc04a51f48549b484e41c4d4d4cc"),
    ("御女茉莉", "6ce7ea8ada884bf3889fa7c7fb206691"),
    ("真实女声", "c189c7cff21c400ba67592406202a3a0"),
    ("女大学生", "5c353fdb312f4888836a9a5680099ef0"),
    ("温情女学生", "a1417155aa234890aab4a18686d12849"),
    ("蒋介石", "918a8277663d476b95e2c4867da0f6a6"),
    ("李云龙", "2e576989a8f94e888bf218de90f8c19a"),
    ("姜文", "ee58439a2e354525bd8fa79380418f4d"),
    ("黑手", "f7561ff309bd4040a59f1e600f4f4338"),
    ("马保国", "794ed17659b243f69cfe6838b03fd31a"),
    ("罗永浩", "9cc8e9b9d9ed471a82144300b608bf7f"),
    ("祁同伟", "4729cb883a58431996b998f2fca7f38b"),
    ("郭继承", "ecf03a0cf954498ca0005c472ce7b141"),
    ("麦克阿瑟", "405736979e244634914add64e37290b0"),
    ("营销号", "9d2a825024ce4156a16ba3ff799c4554"),
    ("蜡笔小新", "60b9a847ba6e485fa8abbde1b9470bc4"),
    ("奶龙", "3d1cb00d75184099992ddbaf0fdd7387"),
    ("懒羊羊", "131c6b3a889543139680d8b3aa26b98d"),
    ("剑魔", "ffb55be33cbb4af19b07e9a0ef64dab1"),
    ("小明剑魔", "a9372068ed0740b48326cf9a74d7496a"),
    ("唐僧", "0fb04af381e845e49450762bc941508c"),
    ("孙悟空", "8d96d5525334476aa67677fb43059dc5"),
    ("王琨", "4f201abba2574feeae11e5ebf737859e"),
    ("麦辣鸡腿堡", "c293697468924f3089cd9b90520dbc16"),
    ("猪八戒", "4313e3ec56f14eb3946630dbdad01059"),
    ("夏(中配) 蔚蓝档案", "c5fca4f670214e3cb7fbb9d595552e6e"),
    ("蔚蓝档案阿洛娜", "6ec8168d8392467c82358a780b35c5ca"),
    ("蔚蓝档案星野", "057265ac020c41a9a91d57c747d3b4c0"),
];

fn builtin_role_id(name: &str) -> Option<&'static str> {
    BUILTIN_ROLES
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| *v)
}

fn builtin_cover(role: &str) -> &'static str {
    if role == "薯薯" {
        "https://raw.githubusercontent.com/Yu9191/-/main/image.png"
    } else {
        ""
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct UserConfig {
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(rename = "defaultRole")]
    default_role: String,
    #[serde(rename = "defaultRoleId")]
    default_role_id: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Document {
    users: HashMap<String, UserConfig>,
    roles: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    covers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    model: String,
}

impl Document {
    fn merged_roles(&self) -> HashMap<String, String> {
        let mut merged: HashMap<String, String> = BUILTIN_ROLES
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        for (k, v) in &self.roles {
            merged.insert(k.clone(), v.clone());
        }
        merged
    }

    fn role_names(&self) -> Vec<String> {
        let all = self.merged_roles();
        let mut builtin = Vec::new();
        let mut own = Vec::new();
        for name in all.keys() {
            if builtin_role_id(name).is_some() {
                builtin.push(name.clone());
            } else {
                own.push(name.clone());
            }
        }
        builtin.sort();
        own.sort();
        builtin.extend(own);
        builtin
    }

    fn cover(&self, role: &str) -> &str {
        self.covers
            .get(role)
            .map(String::as_str)
            .unwrap_or_else(|| builtin_cover(role))
    }

    fn voice(&self, self_id: &str) -> (String, String) {
        if let Some(u) = self.users.get(self_id)
            && !u.default_role.is_empty()
            && !u.default_role_id.is_empty()
        {
            return (u.default_role.clone(), u.default_role_id.clone());
        }
        let id = self
            .merged_roles()
            .get(DEFAULT_ROLE)
            .cloned()
            .unwrap_or_default();
        (DEFAULT_ROLE.to_owned(), id)
    }
}

fn emoji_ranges_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[\x{1F600}-\x{1F64F}\x{1F300}-\x{1F5FF}\x{1F680}-\x{1F6FF}\x{2600}-\x{26FF}\x{2700}-\x{27BF}\x{FE0F}\x{200D}]")
            .expect("valid emoji regex")
    })
}

fn not_spoken_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[^\x{4e00}-\x{9fa5}a-zA-Z0-9\s，。？！、,?!.]")
            .expect("valid not_spoken regex")
    })
}

fn voice_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9a-f]{32}$").expect("valid voice id regex"))
}

fn model_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]{1,40}$").expect("valid model regex"))
}

fn clean_text(text: &str) -> String {
    let no_emoji = emoji_ranges_re().replace_all(text, "");
    let filtered = not_spoken_re().replace_all(&no_emoji, "");
    let mut out = String::with_capacity(filtered.len());
    let mut previous = '\0';
    for r in filtered.chars() {
        if r == previous && "，。？！、,?!.".contains(r) {
            continue;
        }
        out.push(r);
        previous = r;
    }
    out.trim().to_owned()
}

fn api_error(resp: &Response) -> CmdError {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Detail {
        message: String,
        detail: Option<Value>,
    }
    let parsed: Detail = serde_json::from_slice(&resp.body).unwrap_or_default();
    let mut reason = parsed.message;
    if reason.is_empty()
        && let Some(d) = parsed.detail
    {
        reason = match d {
            Value::String(s) => s,
            other => other.to_string(),
        };
    }
    if reason.is_empty() {
        reason = String::from_utf8_lossy(&resp.body).trim().to_owned();
    }
    reason = mibot_command::truncate(&reason, 200);
    match resp.status {
        401 | 403 => {
            return kit::fail(if kit::is_zh() {
                format!("API Key 无效（{}）", resp.status)
            } else {
                format!("Invalid API Key ({})", resp.status)
            });
        }
        402 => return kit::fail(kit::tr("Insufficient fish.audio balance", "fish.audio 账户余额不足")),
        _ => {}
    }
    if reason.is_empty() {
        kit::fail(if kit::is_zh() {
            format!("fish.audio 返回 HTTP {}", resp.status)
        } else {
            format!("fish.audio returned HTTP {}", resp.status)
        })
    } else if kit::is_zh() {
        kit::fail(format!("fish.audio 返回 HTTP {}：{reason}", resp.status))
    } else {
        kit::fail(format!("fish.audio returned HTTP {}: {reason}", resp.status))
    }
}

async fn synthesize_with_endpoint(
    cancel: &CancellationToken,
    target: &str,
    config: &Document,
    key: &str,
    voice_id: &str,
    text: &str,
) -> Result<Vec<u8>, CmdError> {
    let payload = serde_json::json!({
        "text": text,
        "reference_id": voice_id,
        "format": "mp3",
    });
    let body = serde_json::to_vec(&payload)?;
    let mut headers = HashMap::new();
    headers.insert("Authorization".into(), format!("Bearer {key}"));
    headers.insert("Content-Type".into(), "application/json".into());
    if !config.model.is_empty() {
        headers.insert("model".into(), config.model.clone());
    }
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(kit::fail(kit::tr("Operation cancelled", "操作已取消"))),
        r = do_request(Request {
            method: Method::POST,
            url: target.to_owned(),
            headers,
            body: Some(body),
            timeout: Some(Duration::from_secs(120)),
            max_bytes: Some(20 << 20),
            ..Default::default()
        }) => r?,
    };
    if !resp.ok() {
        return Err(api_error(&resp));
    }
    if resp.body.is_empty() {
        return Err(kit::fail(kit::tr(
            "fish.audio returned empty audio",
            "fish.audio 返回了空的音频",
        )));
    }
    Ok(resp.body)
}

fn cover_link(link: &str) -> bool {
    Url::parse(link).is_ok_and(|u| {
        (u.scheme() == "https" || u.scheme() == "http") && u.host_str().is_some_and(|h| !h.is_empty())
    })
}

async fn download_cover(cancel: &CancellationToken, link: &str) -> Option<Vec<u8>> {
    if !cover_link(link) {
        return None;
    }
    let resp = tokio::select! {
        _ = cancel.cancelled() => return None,
        r = do_request(Request {
            url: link.to_owned(),
            timeout: Some(Duration::from_secs(30)),
            max_bytes: Some(5 << 20),
            ..Default::default()
        }) => r.ok()?,
    };
    resp.ok().then_some(resp.body)
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "🔊 <b>文字转语音</b>\n\n用 fish.audio 的角色声音念一段话，发成语音消息。\n\n\
             • <code>{p}t 文本</code> 发语音；回复一条消息只发 <code>{p}t</code> 念那条消息\n\
             • <code>{p}t song 歌名 歌手 文本</code> 做成带封面的 MP3，专辑名是当前角色\n\
             • <code>{p}t fm 封面链接</code> 给当前角色设封面\n\
             • <code>{p}ts [页码]</code> 看角色列表，<code>{p}ts 角色名</code> 切换，\
             <code>{p}ts 角色名 模型 ID</code> 新增并切换\n\
             • <code>{p}tk API密钥</code> 在收藏夹设置 API Key\n\
             • <code>{p}t model [名称]</code> 看或设 fish.audio 的模型，如 s1、s2-pro、s2.1-pro；\
             <code>{p}t model default</code> 交给接口决定\n\n\
             API Key 在 https://fish.audio/ 申请，更多角色见 https://fish.audio/zh-CN/app/discovery/ 。\
             一次最多 {MAX_TEXT} 字，表情和生僻符号会被去掉。只有你本人能用：按字数计费，不借给别人。"
        )
    } else {
        format!(
            "🔊 <b>Text to Speech</b>\n\nConvert text to a voice message using fish.audio voices.\n\n\
             • <code>{p}t text</code> send voice; reply to a message with <code>{p}t</code> to read it\n\
             • <code>{p}t song title artist text</code> create a tagged MP3 with cover art (album is current voice)\n\
             • <code>{p}t fm cover_url</code> set cover image for current voice\n\
             • <code>{p}ts [page]</code> list voices, <code>{p}ts name</code> switch, \
             <code>{p}ts name model_id</code> add and switch\n\
             • <code>{p}tk api_key</code> set API Key in Saved Messages\n\
             • <code>{p}t model [name]</code> view or set fish.audio model (e.g. s1, s2-pro, s2.1-pro); \
             <code>{p}t model default</code> let API choose\n\n\
             Get an API Key at https://fish.audio/ and browse voices at https://fish.audio/zh-CN/app/discovery/ . \
             Max {MAX_TEXT} chars per request; emojis and unsupported symbols are stripped. Owner-only (billed per character)."
        )
    }
}

struct Service {
    store: Store<Document>,
}

impl Service {
    fn self_id(inv: &Invocation) -> String {
        inv.client.self_id().to_string()
    }

    async fn speak(&self, inv: &Invocation) -> CmdResult {
        match inv.arg(0).to_ascii_lowercase().as_str() {
            "help" | "h" if inv.args.len() == 1 => {
                return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
            }
            "fm" => return self.set_cover(inv, inv.arg(1)).await,
            "model" => return self.model(inv, inv.arg(1)).await,
            _ => {}
        }
        let config = self.store.read()?;
        let self_id = Self::self_id(inv);
        let api_key = config
            .users
            .get(&self_id)
            .map(|u| u.api_key.as_str())
            .unwrap_or("");
        if api_key.is_empty() {
            let prompt = if kit::is_zh() {
                format!(
                    "❌ 还没设 API Key，先发 {}。\n\n{}",
                    code(&format!("{}tk 你的密钥", inv.prefix())),
                    help(inv.prefix()),
                )
            } else {
                format!(
                    "❌ API Key is not set; send {} first.\n\n{}",
                    code(&format!("{}tk your_key", inv.prefix())),
                    help(inv.prefix()),
                )
            };
            return inv.edit(&prompt).await.map_err(Into::into);
        }
        let song = inv.arg(0).eq_ignore_ascii_case("song");
        let (title, artist, mut text) = if song {
            let t = inv.arg(1).to_owned();
            let a = inv.arg(2).to_owned();
            let rest = inv.raw_after(3).trim().to_owned();
            if t.is_empty() || a.is_empty() {
                return Err(kit::usage(
                    inv.prefix(),
                    kit::tr("t song title artist text", "t song 歌名 歌手 文本"),
                ));
            }
            (t, a, rest)
        } else {
            (String::new(), String::new(), inv.raw_after(0).trim().to_owned())
        };
        if text.is_empty()
            && inv.message.reply_to_id != 0
            && let Some(reply) = kit::reply_msg(inv).await?
        {
            text = reply.content();
        }
        if text.is_empty() {
            return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
        }
        text = clean_text(&text);
        if text.is_empty() {
            return Err(kit::fail(kit::tr(
                "No speakable text left after removing emojis and symbols",
                "去掉表情和符号之后没有能念的字了",
            )));
        }
        let count = text.chars().count();
        if count > MAX_TEXT {
            return inv
                .edit_text(&if kit::is_zh() {
                    format!("❌ 太长了：{count} 字，一次最多 {MAX_TEXT} 字")
                } else {
                    format!("❌ Too long: {count} characters (max {MAX_TEXT})")
                })
                .await
                .map_err(Into::into);
        }
        let (role, voice_id) = config.voice(&self_id);
        inv.edit_text(&if kit::is_zh() {
            format!("🔊 {role} 正在念…")
        } else {
            format!("🔊 {role} is speaking…")
        })
        .await?;
        let target = std::env::var("MIBOT_FISH_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ENDPOINT.to_owned());
        let audio = synthesize_with_endpoint(&inv.cancel, &target, &config, api_key, &voice_id, &text)
            .await
            .map_err(|e| {
                kit::fail_with(
                    kit::tr("Synthesis failed", "合成失败"),
                    mibot_command::UserError::new(e.to_string()),
                )
            })?;
        let peer = inv.client.input_peer(&inv.message.peer)?;
        let (data, options) = if song {
            let cover = download_cover(&inv.cancel, config.cover(&role)).await;
            let encoded = tagged_mp3(
                &audio,
                &Tags {
                    title: title.clone(),
                    artist: artist.clone(),
                    album: role.clone(),
                    cover,
                },
            )
            .await
            .map_err(|e| kit::fail_with(kit::tr("Audio processing failed", "音频处理失败"), e))?;
            (
                encoded.data,
                MediaOptions {
                    name: format!("{title}.mp3"),
                    mime_type: "audio/mpeg".into(),
                    caption: escape(&format!("{title} - {artist}")),
                    reply_to: Some(inv.message.reply_to_id).filter(|&id| id > 0),
                    attributes: vec![tl::enums::DocumentAttribute::Audio(
                        tl::types::DocumentAttributeAudio {
                            voice: false,
                            duration: encoded.seconds as i32,
                            title: Some(title),
                            performer: Some(artist),
                            waveform: None,
                        },
                    )],
                    ..Default::default()
                },
            )
        } else {
            let encoded = voice_ogg(&audio)
                .await
                .map_err(|e| kit::fail_with(kit::tr("Audio processing failed", "音频处理失败"), e))?;
            (
                encoded.data,
                MediaOptions {
                    name: "voice.ogg".into(),
                    mime_type: "audio/ogg".into(),
                    reply_to: Some(inv.message.reply_to_id).filter(|&id| id > 0),
                    attributes: vec![tl::enums::DocumentAttribute::Audio(
                        tl::types::DocumentAttributeAudio {
                            voice: true,
                            duration: encoded.seconds as i32,
                            title: None,
                            performer: None,
                            waveform: None,
                        },
                    )],
                    ..Default::default()
                },
            )
        };
        inv.client.send_document(&peer, &data, options).await?;
        inv.client.delete_message(&inv.message).await.map_err(Into::into)
    }

    async fn roles(&self, inv: &Invocation) -> CmdResult {
        let config = self.store.read()?;
        let self_id = Self::self_id(inv);
        let (current, _) = config.voice(&self_id);
        let page_parsed = inv.arg(0).parse::<usize>();
        if inv.arg(0).is_empty() || (page_parsed.is_ok() && inv.arg(1).is_empty()) {
            const SIZE: usize = 20;
            let names = config.role_names();
            let pages = names.len().div_ceil(SIZE).max(1);
            let page = page_parsed.unwrap_or(1).clamp(1, pages);
            let mut lines = if kit::is_zh() {
                vec![
                    format!("🎭 <b>角色</b>（{} 个）· 第 {page}/{pages} 页", names.len()),
                    format!("当前：{}", escape(&current)),
                    String::new(),
                ]
            } else {
                vec![
                    format!("🎭 <b>Voices</b> ({}) · Page {page}/{pages}", names.len()),
                    format!("Current: {}", escape(&current)),
                    String::new(),
                ]
            };
            let start = (page - 1) * SIZE;
            let end = (page * SIZE).min(names.len());
            for (idx, n) in names[start..end].iter().enumerate() {
                lines.push(format!("{}. {}", start + idx + 1, escape(n)));
            }
            lines.push(String::new());
            lines.push(if kit::is_zh() {
                format!(
                    "{} 切换 · {} 新增 · {} 下一页",
                    code(&format!("{}ts 角色名", inv.prefix())),
                    code(&format!("{}ts 角色名 模型 ID", inv.prefix())),
                    code(&format!("{}ts 2", inv.prefix())),
                )
            } else {
                format!(
                    "{} switch · {} add · {} next page",
                    code(&format!("{}ts name", inv.prefix())),
                    code(&format!("{}ts name model_id", inv.prefix())),
                    code(&format!("{}ts 2", inv.prefix())),
                )
            });
            return inv.edit(&lines.join("\n")).await.map_err(Into::into);
        }
        let name = inv.arg(0).to_owned();
        let mut id = inv.arg(1).to_owned();
        let added = !id.is_empty();
        if id.is_empty() {
            let Some(known) = config.merged_roles().get(&name).cloned() else {
                return Err(kit::fail(if kit::is_zh() {
                    format!("没有这个角色：{name}。新增用 {}ts 角色名 模型 ID", inv.prefix())
                } else {
                    format!("Voice not found: {name}. Add with {}ts name model_id", inv.prefix())
                }));
            };
            id = known;
        } else if !voice_id_re().is_match(&id) {
            return Err(kit::fail(kit::tr(
                "Model ID must be a 32-character hex string from fish.audio",
                "模型 ID 是 fish.audio 上 32 位的十六进制串",
            )));
        }
        self.store.update(|val| {
            if added {
                val.roles.insert(name.clone(), id.clone());
            }
            let u = val.users.entry(self_id.clone()).or_default();
            u.default_role = name.clone();
            u.default_role_id = id.clone();
            Ok::<(), mibot_store::StoreError>(())
        })?;
        if added {
            inv.edit(&if kit::is_zh() {
                format!("✅ 已新增角色 {} 并切换过去（{}）", escape(&name), code(&id))
            } else {
                format!("✅ Added and switched to voice {} ({})", escape(&name), code(&id))
            })
            .await
            .map_err(Into::into)
        } else {
            inv.edit(&if kit::is_zh() {
                format!("✅ 已切换到角色 {}", escape(&name))
            } else {
                format!("✅ Switched to voice {}", escape(&name))
            })
            .await
            .map_err(Into::into)
        }
    }

    async fn key(&self, inv: &Invocation) -> CmdResult {
        let value = inv.arg(0).trim().to_owned();
        if value.is_empty() {
            return Err(kit::usage(
                inv.prefix(),
                kit::tr(
                    "tk API_KEY (get one at https://fish.audio/)",
                    "tk API Key（在 https://fish.audio/ 申请）",
                ),
            ));
        }
        if !inv.message.saved {
            return Err(kit::fail(kit::tr(
                "API Key can only be set in Saved Messages",
                "API Key 只能在收藏夹中配置",
            )));
        }
        inv.edit_text(&kit::working(kit::tr("Saving", "正在保存"))).await?;
        let self_id = Self::self_id(inv);
        self.store.update(|cfg| {
            let u = cfg.users.entry(self_id.clone()).or_default();
            u.api_key = value.clone();
            if u.default_role.is_empty() {
                u.default_role = DEFAULT_ROLE.into();
                u.default_role_id = builtin_role_id(DEFAULT_ROLE).unwrap_or("").into();
            }
            Ok::<(), mibot_store::StoreError>(())
        })?;
        inv.edit_text(kit::tr("✅ API Key saved", "✅ 已保存 API Key"))
            .await
            .map_err(Into::into)
    }

    async fn set_cover(&self, inv: &Invocation, link: &str) -> CmdResult {
        if !cover_link(link) {
            return Err(kit::usage(
                inv.prefix(),
                kit::tr("t fm https://image_url", "t fm https://图片链接"),
            ));
        }
        let config = self.store.read()?;
        let (role, _) = config.voice(&Self::self_id(inv));
        self.store.update(|val| {
            val.covers.insert(role.clone(), link.to_owned());
            Ok::<(), mibot_store::StoreError>(())
        })?;
        inv.edit_text(&if kit::is_zh() {
            format!("✅ 已给角色 {role} 设置封面")
        } else {
            format!("✅ Cover image set for voice {role}")
        })
        .await
        .map_err(Into::into)
    }

    async fn model(&self, inv: &Invocation, name: &str) -> CmdResult {
        if name.is_empty() {
            let config = self.store.read()?;
            let current = if config.model.is_empty() {
                kit::tr("unset (API default)", "未指定（接口默认）")
            } else {
                &config.model
            };
            return inv
                .edit(&if kit::is_zh() {
                    format!(
                        "当前模型：{}\n可选如 s1、s2-pro、s2.1-pro、s2.1-pro-free，以 fish.audio 文档为准。",
                        code(current)
                    )
                } else {
                    format!(
                        "Current model: {}\nOptions include s1, s2-pro, s2.1-pro, s2.1-pro-free (see fish.audio docs).",
                        code(current)
                    )
                })
                .await
                .map_err(Into::into);
        }
        let next = if name.eq_ignore_ascii_case("default") {
            String::new()
        } else if !model_re().is_match(name) {
            return Err(kit::fail(kit::tr(
                "Model name may only contain letters, digits, ., _, and -",
                "模型名只能有字母、数字和 . _ -",
            )));
        } else {
            name.to_owned()
        };
        self.store.update(|val| {
            val.model = next.clone();
            Ok::<(), mibot_store::StoreError>(())
        })?;
        if next.is_empty() {
            inv.edit_text(kit::tr(
                "✅ Model reset to API default",
                "✅ 模型已改为由接口决定",
            ))
            .await
            .map_err(Into::into)
        } else {
            inv.edit(&if kit::is_zh() {
                format!("✅ 模型已设为 {}", code(&next))
            } else {
                format!("✅ Model set to {}", code(&next))
            })
            .await
            .map_err(Into::into)
        }
    }
}

pub fn register(app: &Arc<App>) {
    let svc = Arc::new(Service {
        store: kit::new_store(app, "t.json", Document::default),
    });
    let s1 = Arc::clone(&svc);
    let s2 = Arc::clone(&svc);
    let s3 = Arc::clone(&svc);
    let mut cmd_t = kit::make_cmd(
        "t",
        &[],
        group::MEDIA,
        "Text to speech",
        "text",
        help,
        move |inv| {
            let s = Arc::clone(&s1);
            Box::pin(async move { s.speak(&inv).await })
        },
    )
    .zh("把文字转成语音", "文本");
    cmd_t.timeout = Some(Duration::from_secs(180));
    cmd_t.free_text = true;
    app.registry.register(cmd_t);
    app.registry.register(
        kit::make_cmd(
            "ts",
            &[],
            group::MEDIA,
            "Manage TTS voices",
            "[page|name [model_id]]",
            help,
            move |inv| {
                let s = Arc::clone(&s2);
                Box::pin(async move { s.roles(&inv).await })
            },
        )
        .zh("管理语音合成的角色", "[页码|角色名 [模型 ID]]"),
    );
    app.registry.register(
        kit::make_cmd(
            "tk",
            &[],
            group::MEDIA,
            "Set TTS API key",
            "api_key",
            help,
            move |inv| {
                let s = Arc::clone(&s3);
                Box::pin(async move { s.key(&inv).await })
            },
        )
        .zh("设置语音合成的 API Key", "API Key"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mibot_bot::{Client, PeerCache, RpcTransport, SentRecorder};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "mibot-tts-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_clean_roles_cover_and_mibox_compat() {
        for (inp, want) in [
            ("你好😀世界", "你好世界"),
            ("Hello, world!!!", "Hello, world!"),
            ("好好好。。。真的？？", "好好好。真的？"),
            ("价格 $100 #标签 @某人", "价格 100 标签 某人"),
            ("  ☀️晴天✨  ", "晴天"),
            ("🎉🎉", ""),
        ] {
            assert_eq!(clean_text(inp), want);
        }

        let mut roles = HashMap::new();
        roles.insert("自己的".into(), "a".repeat(32));
        roles.insert("雷军".into(), "b".repeat(32));
        let cfg = Document { roles, ..Default::default() };
        assert_eq!(cfg.merged_roles()["雷军"], "b".repeat(32));
        assert_eq!(cfg.merged_roles().len(), BUILTIN_ROLES.len() + 1);
        assert_eq!(cfg.role_names().last().unwrap(), "自己的");
        assert_eq!(Document::default().voice("1").0, DEFAULT_ROLE);

        assert!(cover_link("https://example.com/a.png"));
        assert!(cover_link("http://example.com/a.png"));
        assert!(!cover_link("file:///etc/passwd"));
        assert!(!cover_link("https://"));

        let raw = r#"{"users":{"42":{"apiKey":"k","defaultRole":"丁真","defaultRoleId":"54a5170264694bfc8e9ad98df7bd89c3"}},"roles":{"丁真":"54a5170264694bfc8e9ad98df7bd89c3"},"covers":{"薯薯":"https://raw.githubusercontent.com/Yu9191/-/main/image.png"}}"#;
        let doc: Document = serde_json::from_str(raw).unwrap();
        assert_eq!(doc.voice("42").0, "丁真");
        assert_eq!(doc.users["42"].api_key, "k");

        let help_txt = help(".");
        for part in [".t text", ".t song", ".t fm", ".ts", ".tk", ".t model"] {
            assert!(help_txt.contains(part));
        }
        mibot_i18n::with_lang(mibot_i18n::Lang::Zh, || {
            let help_zh = help(".");
            assert!(help_zh.contains(".t 文本"));
        });
    }

    #[tokio::test]
    async fn test_synthesize_and_saved_guard() {
        let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
        let reply_body = Arc::new(std::sync::Mutex::new("ID3fake-mp3".to_owned()));
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let st_bg = Arc::clone(&status);
        let rb_bg = Arc::clone(&reply_body);
        let seen_bg = Arc::clone(&seen);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        tokio::spawn(async move {
            while !stop_bg.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok(Ok((mut sock, _))) =
                    tokio::time::timeout(Duration::from_millis(50), listener.accept()).await
                else {
                    continue;
                };
                let st = st_bg.load(std::sync::atomic::Ordering::Relaxed);
                let body = rb_bg.lock().unwrap().clone();
                let seen_bg = Arc::clone(&seen_bg);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    seen_bg
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                    let resp = format!(
                        "HTTP/1.1 {st} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let cancel = CancellationToken::new();
        let url = format!("http://{addr}/v1/tts");
        let audio = synthesize_with_endpoint(
            &cancel,
            &url,
            &Document { model: "s1".into(), ..Default::default() },
            "secret",
            "voice-id",
            "你好",
        )
        .await
        .unwrap();
        assert_eq!(audio, b"ID3fake-mp3");
        let req0 = seen.lock().unwrap()[0].clone();
        assert!(req0.to_ascii_lowercase().contains("authorization: bearer secret"));
        assert!(req0.to_ascii_lowercase().contains("model: s1"));

        for (code_n, body, want) in [
            (401, r#"{"message":"Unauthorized"}"#, "Invalid API Key"),
            (402, r#"{"message":"Insufficient balance"}"#, "Insufficient fish.audio balance"),
            (400, r#"{"message":"reference not found"}"#, "reference not found"),
            (500, "", "HTTP 500"),
        ] {
            status.store(code_n, std::sync::atomic::Ordering::Relaxed);
            *reply_body.lock().unwrap() = body.into();
            let err = synthesize_with_endpoint(&cancel, &url, &Document::default(), "k", "v", "x")
                .await
                .unwrap_err();
            assert!(err.to_string().contains(want), "got {err}");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);

        struct Noop;
        impl RpcTransport for Noop {
            fn invoke_raw<'a>(
                &'a self,
                _: Option<i32>,
                _: u32,
                _: Vec<u8>,
            ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, grammers_mtsender::InvocationError>> {
                Box::pin(async { Err(grammers_mtsender::InvocationError::Dropped("noop")) })
            }
        }
        let td = TempDir::new();
        let svc = Service { store: Store::new(td.path().join("t.json"), Document::default) };
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            Arc::new(Noop),
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let mut msg = kit::blank_bot_message(1, tl::enums::Peer::User(tl::types::PeerUser { user_id: 2 }));
        msg.saved = false;
        let inv = Invocation {
            client,
            registry: Arc::new(mibot_command::Registry::new()),
            message: msg,
            prefix: ".".into(),
            command: "tk".into(),
            args: vec!["test-key".into()],
            raw: "test-key".into(),
            cancel: CancellationToken::new(),
            foreign: false,
        };
        let err = svc.key(&inv).await.unwrap_err();
        assert!(err.to_string().contains("Saved Messages"));
    }
}
