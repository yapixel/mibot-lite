use crate::kit;
use chrono::{DateTime, FixedOffset, Timelike, Utc};
use croner::parser::{CronParser, Seconds};
use croner::Cron;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{Client, PeerCache, SendOptions, first_sent_message_id, peer_from_id, peer_id, without_ip_privacy};
use mibot_command::{CmdError, CmdResult, Command, DispatchOutcome, Invocation, PAGE_LIMIT, brief, code, escape, group, html_pages};
use mibot_store::Store;
use regex::{Captures, Regex};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const COMMAND_NAME: &str = "acron";
const MAX_SCAN: usize = 1000;
const DELIVERY_PENDING: &str = "pending";
const DELIVERY_PREPARED: &str = "prepared";
const DELIVERY_SENT: &str = "sent";

const TASK_TYPES: &[&str] = &["send", "copy", "forward", "del", "del_re", "pin", "unpin", "cmd"];

fn type_label(kind: &str) -> &str {
    match kind {
        "send" => kit::tr("send", "发送"),
        "cmd" => kit::tr("cmd", "命令"),
        "copy" => kit::tr("copy", "复制"),
        "forward" => kit::tr("forward", "转发"),
        "del" => kit::tr("del", "删除"),
        "del_re" => kit::tr("del_re", "正则删除"),
        "pin" => kit::tr("pin", "置顶"),
        "unpin" => kit::tr("unpin", "取消置顶"),
        other => other,
    }
}

fn is_task_type(v: &str) -> bool {
    matches!(v, "send" | "cmd" | "copy" | "forward" | "del" | "del_re" | "pin" | "unpin")
}

fn shanghai_offset() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("valid +08:00 offset")
}

fn de_str_or_num<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::Null => Ok(String::new()),
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(de::Error::custom("expected string, number, or null")),
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct Task {
    #[serde(default, deserialize_with = "de_str_or_num")]
    id: String,
    #[serde(rename = "type")]
    kind: String,
    cron: String,
    chat: String,
    #[serde(rename = "chatId", default, skip_serializing_if = "String::is_empty", deserialize_with = "de_str_or_num")]
    chat_id: String,
    #[serde(rename = "resolvedPeer", default, skip_serializing_if = "is_false")]
    resolved_peer: bool,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "lastRunAt", default, skip_serializing_if = "String::is_empty")]
    last_run_at: String,
    #[serde(rename = "lastResult", default, skip_serializing_if = "String::is_empty")]
    last_result: String,
    #[serde(rename = "lastError", default, skip_serializing_if = "String::is_empty")]
    last_error: String,
    #[serde(default, skip_serializing_if = "is_false")]
    disabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    remark: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    display: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entities: Option<serde_json::Value>,
    #[serde(rename = "replyTo", default, skip_serializing_if = "String::is_empty")]
    reply_to: String,
    #[serde(rename = "fromChatId", default, skip_serializing_if = "String::is_empty")]
    from_chat_id: String,
    #[serde(rename = "fromMsgId", default, skip_serializing_if = "String::is_empty")]
    from_msg_id: String,
    #[serde(rename = "msgId", default, skip_serializing_if = "String::is_empty")]
    msg_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    limit: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    regex: String,
    #[serde(default, skip_serializing_if = "is_false")]
    notify: bool,
    #[serde(rename = "pmOneSide", default, skip_serializing_if = "is_false")]
    pm_one_side: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    delivery: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct State {
    #[serde(rename = "schemaVersion")]
    schema_version: i32,
    #[serde(default, deserialize_with = "de_str_or_num")]
    seq: String,
    tasks: Vec<Task>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: 1,
            seq: "0".into(),
            tasks: Vec::new(),
        }
    }
}

impl State {
    fn normalize(&mut self) {
        if self.schema_version == 0 {
            self.schema_version = 1;
        }
        if self.seq.is_empty() {
            self.seq = "0".into();
        }
        for t in &mut self.tasks {
            if t.delivery.is_empty() {
                t.delivery = DELIVERY_PENDING.into();
            }
        }
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.tasks.iter().position(|t| t.id == id)
    }

    fn next_id(&self) -> String {
        let mut next = self.seq.parse::<u64>().unwrap_or_else(|_| {
            self.tasks
                .iter()
                .filter_map(|t| t.id.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
        });
        loop {
            next += 1;
            let cand = next.to_string();
            if self.find(&cand).is_none() {
                return cand;
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoredEntity {
    offset: i32,
    length: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    language: String,
    #[serde(rename = "userId", skip_serializing_if = "String::is_empty")]
    user_id: String,
    #[serde(rename = "documentId", skip_serializing_if = "String::is_empty")]
    document_id: String,
    #[serde(skip_serializing_if = "is_false")]
    collapsed: bool,
    #[serde(rename = "className")]
    class_name: String,
}

fn encode_entities(entities: &[tl::enums::MessageEntity]) -> Option<serde_json::Value> {
    let mut list = Vec::new();
    for entity in entities {
        let mut s = StoredEntity::default();
        match entity {
            tl::enums::MessageEntity::Unknown(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityUnknown".into(); }
            tl::enums::MessageEntity::Mention(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityMention".into(); }
            tl::enums::MessageEntity::Hashtag(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityHashtag".into(); }
            tl::enums::MessageEntity::BotCommand(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityBotCommand".into(); }
            tl::enums::MessageEntity::Url(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityUrl".into(); }
            tl::enums::MessageEntity::Email(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityEmail".into(); }
            tl::enums::MessageEntity::Bold(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityBold".into(); }
            tl::enums::MessageEntity::Italic(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityItalic".into(); }
            tl::enums::MessageEntity::Code(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityCode".into(); }
            tl::enums::MessageEntity::Pre(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityPre".into(); s.language = x.language.clone(); }
            tl::enums::MessageEntity::TextUrl(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityTextUrl".into(); s.url = x.url.clone(); }
            tl::enums::MessageEntity::MentionName(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityMentionName".into(); s.user_id = x.user_id.to_string(); }
            tl::enums::MessageEntity::Phone(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityPhone".into(); }
            tl::enums::MessageEntity::Cashtag(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityCashtag".into(); }
            tl::enums::MessageEntity::Underline(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityUnderline".into(); }
            tl::enums::MessageEntity::Strike(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityStrike".into(); }
            tl::enums::MessageEntity::BankCard(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityBankCard".into(); }
            tl::enums::MessageEntity::Spoiler(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntitySpoiler".into(); }
            tl::enums::MessageEntity::CustomEmoji(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityCustomEmoji".into(); s.document_id = x.document_id.to_string(); }
            tl::enums::MessageEntity::Blockquote(x) => { s.offset = x.offset; s.length = x.length; s.class_name = "MessageEntityBlockquote".into(); s.collapsed = x.collapsed; }
            _ => continue,
        }
        list.push(s);
    }
    if list.is_empty() {
        return None;
    }
    serde_json::to_value(list).ok()
}

fn json_i64(val: Option<&serde_json::Value>) -> Option<i64> {
    match val? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

fn decode_entities(raw: Option<&serde_json::Value>) -> Vec<tl::enums::MessageEntity> {
    let Some(serde_json::Value::Array(items)) = raw else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else { continue };
        let Some(o) = json_i64(obj.get("offset")).and_then(|v| i32::try_from(v).ok()) else { continue };
        let Some(l) = json_i64(obj.get("length")).and_then(|v| i32::try_from(v).ok()) else { continue };
        if o < 0 || l <= 0 {
            continue;
        }
        let Some(name) = obj.get("className").and_then(|v| v.as_str()) else { continue };
        let e: tl::enums::MessageEntity = match name {
            "MessageEntityUnknown" => tl::types::MessageEntityUnknown { offset: o, length: l }.into(),
            "MessageEntityMention" => tl::types::MessageEntityMention { offset: o, length: l }.into(),
            "MessageEntityHashtag" => tl::types::MessageEntityHashtag { offset: o, length: l }.into(),
            "MessageEntityBotCommand" => tl::types::MessageEntityBotCommand { offset: o, length: l }.into(),
            "MessageEntityUrl" => tl::types::MessageEntityUrl { offset: o, length: l }.into(),
            "MessageEntityEmail" => tl::types::MessageEntityEmail { offset: o, length: l }.into(),
            "MessageEntityBold" => tl::types::MessageEntityBold { offset: o, length: l }.into(),
            "MessageEntityItalic" => tl::types::MessageEntityItalic { offset: o, length: l }.into(),
            "MessageEntityCode" => tl::types::MessageEntityCode { offset: o, length: l }.into(),
            "MessageEntityPre" => tl::types::MessageEntityPre {
                offset: o,
                length: l,
                language: obj.get("language").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
            }
            .into(),
            "MessageEntityTextUrl" => {
                let Some(url) = obj.get("url").and_then(|v| v.as_str()).filter(|u| !u.is_empty()) else { continue };
                tl::types::MessageEntityTextUrl { offset: o, length: l, url: url.to_owned() }.into()
            }
            "MessageEntityMentionName" => {
                let Some(uid) = json_i64(obj.get("userId")) else { continue };
                tl::types::MessageEntityMentionName { offset: o, length: l, user_id: uid }.into()
            }
            "MessageEntityPhone" => tl::types::MessageEntityPhone { offset: o, length: l }.into(),
            "MessageEntityCashtag" => tl::types::MessageEntityCashtag { offset: o, length: l }.into(),
            "MessageEntityUnderline" => tl::types::MessageEntityUnderline { offset: o, length: l }.into(),
            "MessageEntityStrike" => tl::types::MessageEntityStrike { offset: o, length: l }.into(),
            "MessageEntityBankCard" => tl::types::MessageEntityBankCard { offset: o, length: l }.into(),
            "MessageEntitySpoiler" => tl::types::MessageEntitySpoiler { offset: o, length: l }.into(),
            "MessageEntityCustomEmoji" => {
                let Some(did) = json_i64(obj.get("documentId")) else { continue };
                tl::types::MessageEntityCustomEmoji { offset: o, length: l, document_id: did }.into()
            }
            "MessageEntityBlockquote" => tl::types::MessageEntityBlockquote {
                offset: o,
                length: l,
                collapsed: obj.get("collapsed").and_then(|v| v.as_bool()).unwrap_or(false),
            }
            .into(),
            _ => continue,
        };
        out.push(e);
    }
    out
}

fn input_entities(peers: &PeerCache, entities: &[tl::enums::MessageEntity]) -> Vec<tl::enums::MessageEntity> {
    let mut out = Vec::with_capacity(entities.len());
    for e in entities {
        if let tl::enums::MessageEntity::MentionName(m) = e {
            let Some(ip) = peers.input_peer(&tl::types::PeerUser { user_id: m.user_id }.into()) else {
                continue;
            };
            let Some(user_id) = kit::input_user(&ip) else {
                continue;
            };
            out.push(
                tl::types::InputMessageEntityMentionName {
                    offset: m.offset,
                    length: m.length,
                    user_id,
                }
                .into(),
            );
        } else {
            out.push(e.clone());
        }
    }
    out
}

fn sunday_seven(field: &str) -> String {
    field
        .split(',')
        .map(|part| {
            if part == "7" {
                "0".to_owned()
            } else if let Some(prefix) = part.strip_suffix("-7").filter(|p| !p.contains('/')) {
                format!("{prefix}-6,0")
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_cron(expression: &str) -> Result<Cron, CmdError> {
    let mut fields: Vec<String> = expression.split_whitespace().map(str::to_owned).collect();
    if fields.len() != 6 {
        return Err(kit::fail(kit::tr(
            "Cron expression must have 6 fields: sec min hour day month weekday",
            "Cron 表达式要写 6 段：秒 分 时 日 月 星期",
        )));
    }
    fields[5] = sunday_seven(&fields[5]);
    let joined = fields.join(" ");
    let parser = CronParser::builder().seconds(Seconds::Required).build();
    parser.parse(&joined).map_err(|_| {
        kit::fail(kit::tr(
            "Invalid Cron expression; expected 6 fields: sec min hour day month weekday",
            "Cron 表达式无效，6 段依次是 秒 分 时 日 月 星期",
        ))
    })
}

fn next_run(expression: &str, now: SystemTime) -> Option<SystemTime> {
    let cron = parse_cron(expression).ok()?;
    let dt = DateTime::<Utc>::from(now).with_timezone(&shanghai_offset());
    let next = cron.find_next_occurrence(&dt, false).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(next.timestamp().max(0) as u64))
}

fn js_unicode_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\u\{([0-9A-Fa-f]{1,6})\}|\\u([0-9A-Fa-f]{4})").expect("valid unicode re"))
}

fn compile_regex(raw: &str) -> Result<Regex, CmdError> {
    let trimmed = raw.trim();
    let (pattern, flags) = if trimmed.starts_with('/')
        && let Some(last) = trimmed.rfind('/')
        && last > 0
    {
        (&trimmed[1..last], &trimmed[last + 1..])
    } else {
        (trimmed, "")
    };
    let pattern_owned = js_unicode_re().replace_all(pattern, |c: &Captures| {
        let hex = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()).unwrap_or("");
        format!("\\x{{{hex}}}")
    }).into_owned();
    let pattern = pattern_owned.as_str();
    if pattern.is_empty() {
        return Err(kit::fail(kit::tr(
            "Regular expression is empty",
            "正则表达式是空的",
        )));
    }
    let mut modifiers = String::new();
    for flag in flags.chars() {
        match flag {
            'i' | 'm' | 's' => {
                if !modifiers.contains(flag) {
                    modifiers.push(flag);
                }
            }
            'g' | 'u' => {}
            _ => {
                return Err(kit::fail(if kit::is_zh() {
                    format!("不支持正则标志 {flag}，能用的是 i、m、s")
                } else {
                    format!("Unsupported regex flag {flag}; supported flags are i, m, s")
                }));
            }
        }
    }
    let final_pat = if modifiers.is_empty() {
        pattern.to_owned()
    } else {
        format!("(?{modifiers}){pattern}")
    };
    Regex::new(&final_pat).map_err(|_| {
        kit::fail(kit::tr(
            "Invalid regular expression (lookaround and backreferences are unsupported)",
            "正则表达式无效（不支持环视和反向引用）",
        ))
    })
}

fn message_id_i32(value: &str) -> i32 {
    value.trim().parse::<i32>().ok().filter(|&id| id > 0).unwrap_or(0)
}

fn split_target(value: &str) -> (String, String) {
    let parts: Vec<&str> = value
        .split(['|', '｜'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let chat = parts.first().copied().unwrap_or("").to_owned();
    let reply_to = parts.get(1).copied().unwrap_or("").to_owned();
    (chat, reply_to)
}

fn remark_after(line: &str, n: usize) -> String {
    let mut rest = line;
    for _ in 0..n {
        rest = rest.trim_start();
        let Some(end) = rest.find(char::is_whitespace) else {
            return String::new();
        };
        rest = &rest[end..];
    }
    rest.trim().to_owned()
}

fn parse_flag(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "y")
}

fn flag_text(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

fn copy_command(t: &Task, prefix: &str) -> String {
    let remark = if t.remark.is_empty() {
        String::new()
    } else {
        format!(" {}", t.remark)
    };
    let reply = if t.reply_to.is_empty() {
        String::new()
    } else {
        format!("|{}", t.reply_to)
    };
    let head = format!("{prefix}acron {} {} {}", t.kind, t.cron, t.chat);
    match t.kind.as_str() {
        "send" | "copy" | "forward" => format!("{head}{reply}{remark}"),
        "cmd" => format!("{head}{reply}{remark}\n{}", t.message),
        "del" | "unpin" => format!("{head} {}{remark}", t.msg_id),
        "del_re" => format!("{head} {} {}{remark}", t.limit, t.regex),
        "pin" => format!(
            "{head} {} {} {}{remark}",
            t.msg_id,
            flag_text(t.notify),
            flag_text(t.pm_one_side)
        ),
        _ => format!("{prefix}acron"),
    }
}

fn copy_html(t: &Task, prefix: &str) -> String {
    let text = copy_command(t, prefix);
    if text.contains('\n') {
        format!("<pre>{}</pre>", escape(&text))
    } else {
        code(&text)
    }
}

fn format_time(at: SystemTime) -> String {
    let dt = DateTime::<Utc>::from(at).with_timezone(&shanghai_offset());
    if dt.second() != 0 {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M").to_string()
    }
}

fn millis(at: SystemTime) -> String {
    at.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0).to_string()
}

fn parse_millis(value: &str) -> Option<SystemTime> {
    if let Ok(ms) = value.parse::<i64>()
        && ms > 0
    {
        return Some(UNIX_EPOCH + Duration::from_millis(ms as u64));
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(UNIX_EPOCH + Duration::from_secs(dt.timestamp().max(0) as u64));
    }
    None
}

fn peer_of(input: &tl::enums::InputPeer, self_id: i64) -> Option<tl::enums::Peer> {
    match input {
        tl::enums::InputPeer::User(u) => Some(tl::types::PeerUser { user_id: u.user_id }.into()),
        tl::enums::InputPeer::Chat(c) => Some(tl::types::PeerChat { chat_id: c.chat_id }.into()),
        tl::enums::InputPeer::Channel(c) => Some(tl::types::PeerChannel { channel_id: c.channel_id }.into()),
        tl::enums::InputPeer::PeerSelf => Some(tl::types::PeerUser { user_id: self_id }.into()),
        _ => None,
    }
}

fn chat_label(client: Option<&Client>, t: &Task) -> String {
    let fallback = code(&t.chat);
    let Some(c) = client else { return fallback };
    if t.chat_id.is_empty() {
        return fallback;
    }
    let Some(peer) = peer_from_id(&t.chat_id) else {
        return fallback;
    };
    if let tl::enums::Peer::User(u) = &peer
        && u.user_id == c.self_id()
    {
        return kit::tr("Saved Messages", "收藏夹").into();
    }
    let title = c.peers().title(&peer);
    if title.is_empty() || title == t.chat_id {
        fallback
    } else if kit::is_zh() {
        format!("{}（{}）", escape(&title), code(&t.chat))
    } else {
        format!("{} ({})", escape(&title), code(&t.chat))
    }
}

fn message_link(chat_id: &str, id: &str) -> String {
    if let Some(ch) = chat_id.strip_prefix("-100")
        && !ch.is_empty()
        && message_id_i32(id) > 0
    {
        return format!("<a href=\"https://t.me/c/{}/{}\">{}</a>", escape(ch), escape(id), escape(id));
    }
    code(id)
}

type DispatchFn = Arc<dyn Fn(CancellationToken, tl::types::Message) -> DispatchOutcome + Send + Sync>;

struct Service {
    app: Option<Arc<App>>,
    store: Store<State>,
    dispatch: Option<DispatchFn>,
    mu: Mutex<ServiceState>,
    serial: Mutex<()>,
}

#[derive(Default)]
struct ServiceState {
    handles: std::collections::HashMap<String, CancellationToken>,
    running: std::collections::HashSet<String>,
    client: Option<Arc<Client>>,
    lifecycle: Option<CancellationToken>,
}

impl Service {
    fn invokes_self(&self, text: &str) -> bool {
        let Some(app) = &self.app else {
            return false;
        };
        app.registry.parse(text).is_some_and(|r| r.command == COMMAND_NAME)
    }

    fn read(&self) -> Result<State, CmdError> {
        let mut st = self.store.read()?;
        st.normalize();
        Ok(st)
    }

    fn update_task<F: FnOnce(&mut Task)>(&self, id: &str, mutate: F) -> Result<(), CmdError> {
        kit::update_ok(&self.store, |st| {
            if let Some(idx) = st.find(id) {
                mutate(&mut st.tasks[idx]);
            }
        })
        .map_err(Into::into)
    }

    async fn schedule(self: &Arc<Self>, id: &str, cron_expr: &str) {
        let Ok(parsed) = parse_cron(cron_expr) else {
            return;
        };
        let mut g = self.mu.lock().await;
        if g.handles.contains_key(id) {
            return;
        }
        let Some(lifecycle) = g.lifecycle.clone() else {
            return;
        };
        let tok = lifecycle.child_token();
        g.handles.insert(id.to_owned(), tok.clone());
        let svc = Arc::clone(self);
        let tid = id.to_owned();
        tokio::spawn(async move {
            loop {
                let now = Utc::now().with_timezone(&shanghai_offset());
                let Ok(next) = parsed.find_next_occurrence(&now, false) else {
                    break;
                };
                let wait_ms = (next.timestamp_millis() - now.timestamp_millis()).max(50) as u64;
                if !kit::sleep(&tok, Duration::from_millis(wait_ms)).await {
                    break;
                }
                let s = Arc::clone(&svc);
                let id_clone = tid.clone();
                tokio::spawn(async move {
                    s.fire(&id_clone).await;
                });
            }
        });
    }

    async fn unschedule(&self, id: &str) {
        let mut g = self.mu.lock().await;
        if let Some(tok) = g.handles.remove(id) {
            tok.cancel();
        }
    }

    async fn start(self: &Arc<Self>, cancel: CancellationToken, client: Arc<Client>) {
        {
            let mut g = self.mu.lock().await;
            g.client = Some(client);
            g.lifecycle = Some(cancel.clone());
        }
        if let Ok(st) = self.read() {
            for t in st.tasks {
                if !t.disabled && parse_cron(&t.cron).is_ok() {
                    self.schedule(&t.id, &t.cron).await;
                }
            }
        }
        cancel.cancelled().await;
        let mut g = self.mu.lock().await;
        for (_, tok) in g.handles.drain() {
            tok.cancel();
        }
    }

    async fn fire(&self, id: &str) {
        let (client, lifecycle) = {
            let mut g = self.mu.lock().await;
            let (Some(c), Some(l)) = (g.client.clone(), g.lifecycle.clone()) else {
                return;
            };
            if !g.running.insert(id.to_owned()) {
                return;
            }
            (c, l)
        };
        let _serial = self.serial.lock().await;
        if !lifecycle.is_cancelled() {
            self.execute(&lifecycle, &client, id).await;
        }
        self.mu.lock().await.running.remove(id);
    }

    async fn execute(&self, cancel: &CancellationToken, client: &Client, id: &str) {
        let reg = self.app.as_ref().map(|a| Arc::clone(&a.registry));
        let track_tok = cancel.child_token();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let mut tracked = false;
        if let Some(r) = &reg {
            let mut tx = Some(done_tx);
            if !r.long_running(track_tok.clone(), move |t| {
                Box::pin(async move {
                    t.cancelled().await;
                    if let Some(sender) = tx.take() {
                        let _ = sender.send(());
                    }
                })
            }) {
                kit::opt_log(
                    self.app.as_ref().map(|a| &a.logs),
                    mibot_logtail::Level::Info,
                    "acron.skipped",
                    &[("task", id)],
                );
                return;
            }
            tracked = true;
        }
        struct TrackDrop(CancellationToken);
        impl Drop for TrackDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let guard = TrackDrop(track_tok);
        self.execute_tracked(cancel, client, id).await;
        drop(guard);
        if tracked {
            let _ = done_rx.await;
            tokio::task::yield_now().await;
        }
    }

    async fn execute_tracked(&self, cancel: &CancellationToken, client: &Client, id: &str) {

        let Ok(st) = self.read() else {
            return;
        };
        let Some(idx) = st.find(id) else {
            return;
        };
        let t = st.tasks[idx].clone();
        if t.disabled {
            return;
        }
        if t.delivery == DELIVERY_PREPARED {
            kit::opt_log(
                self.app.as_ref().map(|a| &a.logs),
                mibot_logtail::Level::Warn,
                "acron.skipped_unconfirmed",
                &[("task", id)],
            );
            let _ = self.update_task(id, |task| {
                task.delivery = DELIVERY_PENDING.into();
                task.last_error = kit::tr(
                    "Previous run did not finish before shutdown; skipped to avoid duplicates",
                    "上次执行没等到结果进程就停了，这次跳过，免得重复",
                )
                .into();
            });
            return;
        }
        if let Err(err) = self.update_task(id, |task| task.delivery = DELIVERY_PREPARED.into()) {
            kit::opt_log(
                self.app.as_ref().map(|a| &a.logs),
                mibot_logtail::Level::Error,
                "acron.save_failed",
                &[("task", id), ("error", &err.to_string())],
            );
            return;
        }
        let run_cancel = cancel.child_token();
        let res = tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(600)) => {
                run_cancel.cancel();
                Err(kit::fail(kit::tr("Execution timed out", "执行超时")))
            }
            r = self.run(&run_cancel, client, &t) => r,
        };
        let now_str = millis(SystemTime::now());
        let (res_chat, ok_msg, err_msg) = match &res {
            Ok((msg, cid)) => (cid.clone(), msg.clone(), String::new()),
            Err(e) => (
                String::new(),
                String::new(),
                if cancel.is_cancelled() {
                    kit::tr("Service stopped before completion", "服务停了，没执行完").into()
                } else {
                    brief(e.as_ref())
                },
            ),
        };
        let _ = self.update_task(id, |task| {
            if !res_chat.is_empty() && !task.resolved_peer {
                task.chat_id = res_chat;
                task.resolved_peer = true;
            }
            task.last_run_at = now_str;
            if res.is_ok() {
                task.last_result = ok_msg;
                task.last_error.clear();
                task.delivery = DELIVERY_SENT.into();
            } else {
                task.last_result.clear();
                task.last_error = err_msg;
                task.delivery = DELIVERY_PENDING.into();
            }
        });
    }

    async fn run(&self, _cancel: &CancellationToken, client: &Client, t: &Task) -> Result<(String, String), CmdError> {
        let (peer, chat_id) = target(client, t).await?;
        let reply_to = message_id_i32(&t.reply_to);
        match t.kind.as_str() {
            "send" => {
                let entities = input_entities(client.peers(), &decode_entities(t.entities.as_ref()));
                let opts = SendOptions {
                    reply_to: (reply_to > 0).then_some(reply_to),
                    no_webpage: false,
                    ..Default::default()
                };
                without_ip_privacy(|| client.send_plain(&peer, &t.message, (!entities.is_empty()).then_some(entities), opts))
                    .await?;
                Ok((kit::tr("Sent 1 message", "已发送 1 条消息").into(), chat_id))
            }
            "cmd" => {
                let r = self.run_command(client, &peer, &t.message, reply_to).await?;
                Ok((r, chat_id))
            }
            "copy" => {
                copy_message(client, t, &peer, reply_to).await?;
                Ok((
                    kit::tr("Copied and sent 1 message", "已复制发送 1 条消息").into(),
                    chat_id,
                ))
            }
            "forward" => {
                let (from, id) = source(client, t)?;
                client
                    .forward_messages(&from, &peer, &[id], (reply_to > 0).then_some(reply_to))
                    .await?;
                Ok((
                    kit::tr("Forwarded 1 message", "已转发 1 条消息").into(),
                    chat_id,
                ))
            }
            "del" => {
                let id = message_id_i32(&t.msg_id);
                if id == 0 {
                    return Err(kit::fail(kit::tr("Invalid message ID", "消息 ID 无效")));
                }
                client.delete_messages(&peer, &[id]).await?;
                let msg = if kit::is_zh() {
                    format!("已尝试删除消息 {}", t.msg_id)
                } else {
                    format!("Attempted to delete message {}", t.msg_id)
                };
                Ok((msg, chat_id))
            }
            "del_re" => {
                let deleted = delete_matching(client, &peer, t).await?;
                let msg = if kit::is_zh() {
                    format!("匹配并删除 {deleted} 条")
                } else {
                    format!("Matched and deleted {deleted} message(s)")
                };
                Ok((msg, chat_id))
            }
            "pin" | "unpin" => {
                let id = message_id_i32(&t.msg_id);
                if id == 0 {
                    return Err(kit::fail(kit::tr("Invalid message ID", "消息 ID 无效")));
                }
                let unpin = t.kind == "unpin";
                client
                    .invoke(&tl::functions::messages::UpdatePinnedMessage {
                        silent: !unpin && !t.notify,
                        unpin,
                        pm_oneside: !unpin && t.pm_one_side,
                        peer,
                        id,
                    })
                    .await?;
                let msg = if unpin {
                    if kit::is_zh() {
                        format!("已取消置顶消息 {}", t.msg_id)
                    } else {
                        format!("Unpinned message {}", t.msg_id)
                    }
                } else if kit::is_zh() {
                    format!("已置顶消息 {}", t.msg_id)
                } else {
                    format!("Pinned message {}", t.msg_id)
                };
                Ok((msg, chat_id))
            }
            _ => Err(kit::fail(if kit::is_zh() {
                format!("不认识的任务类型 {}", t.kind)
            } else {
                format!("Unknown task type {}", t.kind)
            })),
        }
    }

    async fn run_command(
        &self,
        client: &Client,
        peer: &tl::enums::InputPeer,
        text: &str,
        reply_to: i32,
    ) -> Result<String, CmdError> {
        if text.trim().is_empty() {
            return Err(kit::fail(kit::tr("No command in task", "任务里没有命令")));
        }
        if self.invokes_self(text) {
            return Err(kit::fail(kit::tr(
                "Command invokes .acron itself, which would recursively add tasks; skipped — please delete this task",
                "命令是 .acron 本身，执行一次就会再加一个任务，已跳过；请删掉这个任务",
            )));
        }
        let updates = without_ip_privacy(|| {
            client.send_plain(
                peer,
                text,
                None,
                SendOptions {
                    reply_to: (reply_to > 0).then_some(reply_to),
                    ..Default::default()
                },
            )
        })
        .await?;
        let id = first_sent_message_id(&updates).unwrap_or(0);
        let p = peer_of(peer, client.self_id()).unwrap_or_else(|| tl::types::PeerUser { user_id: client.self_id() }.into());
        let mut raw = kit::blank_tl_message(id, p);
        raw.out = true;
        raw.message = text.to_owned();
        if reply_to > 0 {
            raw.reply_to = Some(
                tl::types::MessageReplyHeader {
                    reply_to_scheduled: false,
                    forum_topic: false,
                    quote: false,
                    reply_to_ephemeral: false,
                    reply_to_msg_id: Some(reply_to),
                    reply_to_peer_id: None,
                    reply_from: None,
                    reply_media: None,
                    reply_to_top_id: None,
                    quote_text: None,
                    quote_entities: None,
                    quote_offset: None,
                    todo_item_id: None,
                    poll_option: None,
                    quote_rich: None,
                }
                .into(),
            );
        }
        let outcome = if let Some(disp) = &self.dispatch {
            disp(CancellationToken::new(), raw)
        } else {
            DispatchOutcome::NotCommand
        };
        match outcome {
            DispatchOutcome::Queued => Ok(kit::tr("Command queued for execution", "已提交执行命令").into()),
            DispatchOutcome::Rejected(_) => Err(kit::fail(kit::tr(
                "Command sent, but not executed (too many concurrent commands or service is updating/stopping)",
                "命令已发出，但当时同时执行的命令太多（或服务正在更新、退出），没有执行",
            ))),
            DispatchOutcome::NotCommand => Ok(kit::tr(
                "Command sent (not executed: not a valid command; check prefix and command name)",
                "已发送命令（未执行：不是可用的命令，检查前缀和命令名）",
            )
            .into()),
        }
    }

    async fn handle(self: &Arc<Self>, inv: &Invocation) -> CmdResult {
        let sub = inv.arg(0).to_ascii_lowercase();
        match sub.as_str() {
            "" => inv.respond_pages(&html_pages(&help(inv.prefix()), PAGE_LIMIT)).await.map_err(Into::into),
            "list" | "ls" | "la" => self.list(inv, sub == "la").await,
            "rm" | "remove" | "del_task" => self.remove(inv, &sub).await,
            "disable" | "off" => self.disable(inv, &sub).await,
            "enable" | "on" => self.enable(inv, &sub).await,
            _ if is_task_type(&sub) => self.add(inv, &sub).await,
            _ => Err(kit::fail(if kit::is_zh() {
                format!(
                    "没有这个子命令：{}，{}help acron 看用法",
                    inv.arg(0),
                    inv.prefix()
                )
            } else {
                format!(
                    "Unknown subcommand: {}; use {}help acron for usage",
                    inv.arg(0),
                    inv.prefix()
                )
            })),
        }
    }

    async fn add(self: &Arc<Self>, inv: &Invocation, kind: &str) -> CmdResult {
        let body_text = inv.message.text.strip_prefix(inv.prefix()).unwrap_or(&inv.message.text);
        let (head, body) = body_text.split_once('\n').unwrap_or((body_text, ""));
        let fields: Vec<&str> = head.split_whitespace().collect();
        if fields.len() < 8 {
            return Err(kit::fail(kit::tr(
                "Cron expression must have 6 fields: sec min hour day month weekday",
                "Cron 表达式要写 6 段：秒 分 时 日 月 星期",
            )));
        }
        let expression = fields[2..8].join(" ");
        let _ = parse_cron(&expression)?;
        let (chat, reply_to) = if fields.len() > 8 {
            split_target(fields[8])
        } else {
            (String::new(), String::new())
        };
        if chat.is_empty() {
            return Err(kit::fail(kit::tr(
                "Specify target chat: numeric ID, @username, or me",
                "请写上对话：数字 ID、@用户名或 me",
            )));
        }
        if !reply_to.is_empty() && message_id_i32(&reply_to) == 0 {
            return Err(kit::fail(kit::tr(
                "After '|' in chat target, specify a message ID or topic ID",
                "对话后面「|」之后要写消息 ID 或话题 ID",
            )));
        }
        let rest = &fields[9..];
        let arg = |i: usize| rest.get(i).copied().unwrap_or("");
        let now = SystemTime::now();
        let mut t = Task {
            kind: kind.to_owned(),
            cron: expression.clone(),
            chat: chat.clone(),
            reply_to,
            created_at: millis(now),
            delivery: DELIVERY_PENDING.into(),
            ..Default::default()
        };
        let mut used = 0usize;
        match kind {
            "send" => fill_send(inv, &mut t).await?,
            "cmd" => {
                t.message = body.trim().to_owned();
                if t.message.is_empty() {
                    return Err(kit::fail(if kit::is_zh() {
                        format!("要执行的命令写在第二行，{}help acron 有例子", inv.prefix())
                    } else {
                        format!(
                            "Put the command on the second line; see {}help acron for examples",
                            inv.prefix()
                        )
                    }));
                }
                if self.invokes_self(&t.message) {
                    return Err(kit::fail(if kit::is_zh() {
                        format!(
                            "定时执行的命令不能是 {}acron：每执行一次就会再添加一个任务",
                            inv.prefix()
                        )
                    } else {
                        format!(
                            "Scheduled command cannot be {}acron: each run would add another task",
                            inv.prefix()
                        )
                    }));
                }
            }
            "copy" | "forward" => {
                let Some(reply) = kit::reply_msg(inv).await? else {
                    return Err(kit::fail(kit::tr(
                        "Reply to a message to copy or forward",
                        "请回复一条要复制或转发的消息",
                    )));
                };
                t.from_chat_id = reply.chat_id;
                t.from_msg_id = reply.id.to_string();
            }
            "del" | "unpin" | "pin" => {
                if message_id_i32(arg(0)) == 0 {
                    return Err(kit::fail(kit::tr(
                        "Specify a valid message ID (positive integer)",
                        "请写上消息 ID（正整数）",
                    )));
                }
                t.msg_id = arg(0).to_owned();
                used = 1;
                if kind == "pin" {
                    if arg(1).is_empty() || arg(2).is_empty() {
                        return Err(kit::fail(kit::tr(
                            "Specify notify and pm-only flags, e.g. 1 0 or true false",
                            "请写上发不发通知、是不是只对自己置顶，如 1 0 或 true false",
                        )));
                    }
                    t.notify = parse_flag(arg(1));
                    t.pm_one_side = parse_flag(arg(2));
                    used = 3;
                }
            }
            "del_re" => {
                let limit = arg(0).parse::<usize>().ok().filter(|&v| (1..=MAX_SCAN).contains(&v));
                let Some(limit) = limit else {
                    return Err(kit::fail(if kit::is_zh() {
                        format!("请写上 1–{MAX_SCAN} 之间的整数条数")
                    } else {
                        format!("Specify an integer count between 1 and {MAX_SCAN}")
                    }));
                };
                if arg(1).is_empty() {
                    return Err(kit::fail(kit::tr(
                        "Specify a regular expression for matching messages",
                        "请写上消息的正则表达式",
                    )));
                }
                let _ = compile_regex(arg(1))?;
                t.limit = limit.to_string();
                t.regex = arg(1).to_owned();
                used = 2;
            }
            _ => {}
        }
        t.remark = remark_after(head, 9 + used);
        let mut resolved = false;
        if let Ok(peer) = inv.client.resolve_target(&chat).await {
            if let Some(p) = peer_of(&peer, inv.client.self_id()) {
                t.chat_id = peer_id(&p);
            }
            t.resolved_peer = true;
            resolved = true;
        }
        t.display = chat_label(Some(&inv.client), &t);
        let mut saved_id = String::new();
        kit::update_ok(&self.store, |st| {
            st.normalize();
            t.id = st.next_id();
            saved_id = t.id.clone();
            st.seq = t.id.clone();
            st.schema_version = 1;
            st.tasks.push(t.clone());
        })?;
        t.id = saved_id.clone();
        self.schedule(&t.id, &t.cron).await;

        let mut display = t.display.clone();
        if !resolved {
            display.push_str(kit::tr(
                " (chat not resolved yet; will resolve at run time)",
                "（现在找不到这个对话，到时间再找）",
            ));
        }
        let title = match kind {
            "send" => kit::tr("Scheduled send task added", "已添加定时发送任务"),
            "cmd" => kit::tr("Scheduled command task added", "已添加定时命令任务"),
            "copy" => kit::tr("Scheduled copy task added", "已添加定时复制任务"),
            "forward" => kit::tr("Scheduled forward task added", "已添加定时转发任务"),
            "del" => kit::tr("Scheduled delete task added", "已添加删除消息的定时任务"),
            "del_re" => kit::tr("Scheduled regex delete task added", "已添加正则删除的定时任务"),
            "pin" => kit::tr("Scheduled pin task added", "已添加置顶消息的定时任务"),
            "unpin" => kit::tr("Scheduled unpin task added", "已添加取消置顶的定时任务"),
            _ => kit::tr("Scheduled task added", "已添加定时任务"),
        };
        let mut lines = vec![
            format!("✅ {title}"),
            format!("{}{}", kit::tr("ID: ", "ID："), code(&t.id)),
            format!("{}{display}", kit::tr("Chat: ", "对话：")),
        ];
        if !t.reply_to.is_empty() {
            let lbl = if kind == "forward" {
                kit::tr("Topic: ", "话题：")
            } else {
                kit::tr("Reply: ", "回复：")
            };
            lines.push(format!("{lbl}{}", code(&t.reply_to)));
        }
        if !t.msg_id.is_empty() {
            lines.push(format!("{}{}", kit::tr("Message ID: ", "消息 ID："), code(&t.msg_id)));
        }
        if kind == "del_re" {
            lines.push(format!("{}{}", kit::tr("Recent limit: ", "最近条数："), code(&t.limit)));
            lines.push(format!("{}{}", kit::tr("Pattern: ", "匹配："), code(&t.regex)));
        }
        if kind == "pin" {
            lines.push(format!("{}{}", kit::tr("Notify: ", "通知："), kit::on_off_text(t.notify)));
            lines.push(format!(
                "{}{}",
                kit::tr("PM one-side: ", "只对自己置顶："),
                kit::on_off_text(t.pm_one_side)
            ));
        }
        if !t.remark.is_empty() {
            lines.push(format!("{}{}", kit::tr("Remark: ", "备注："), escape(&t.remark)));
        }
        if let Some(next) = next_run(&t.cron, now) {
            lines.push(format!("{}{}", kit::tr("Next run: ", "下次执行："), format_time(next)));
        }
        lines.push(format!("{}{}", kit::tr("Copy: ", "复制："), copy_html(&t, inv.prefix())));
        inv.edit(&lines.join("\n")).await?;
        Ok(())
    }

    async fn list(&self, inv: &Invocation, mut all: bool) -> CmdResult {
        let mut kind = String::new();
        for val in inv.args.iter().skip(1) {
            let lower = val.to_ascii_lowercase();
            if lower == "all" {
                all = true;
            } else if is_task_type(&lower) && kind.is_empty() {
                kind = lower;
            } else {
                return Err(kit::fail(if kit::is_zh() {
                    format!(
                        "列表只能按 all 和任务类型筛选，{}help acron 看用法",
                        inv.prefix()
                    )
                } else {
                    format!(
                        "List filter only accepts 'all' and task types; use {}help acron for usage",
                        inv.prefix()
                    )
                }));
            }
        }
        let st = self.read()?;
        let tasks: Vec<Task> = st
            .tasks
            .into_iter()
            .filter(|t| (all || t.chat_id == inv.message.chat_id) && (kind.is_empty() || t.kind == kind))
            .collect();
        let html = render_list(
            &tasks,
            all,
            &kind,
            inv.prefix(),
            |t| chat_label(Some(&inv.client), t),
            SystemTime::now(),
        );
        inv.respond_pages(&html_pages(&html, PAGE_LIMIT)).await?;
        Ok(())
    }

    async fn remove(&self, inv: &Invocation, sub: &str) -> CmdResult {
        let id = inv.arg(1);
        if id.is_empty() {
            return Err(kit::usage(inv.prefix(), &format!("acron {sub} ID")));
        }
        let mut found = false;
        kit::update_ok(&self.store, |st| {
            if let Some(idx) = st.find(id) {
                st.tasks.remove(idx);
                found = true;
            }
        })?;
        if !found {
            return Err(not_found(inv, id));
        }
        self.unschedule(id).await;
        let msg = if kit::is_zh() {
            format!("✅ 已删除定时任务 {}", code(id))
        } else {
            format!("✅ Removed scheduled task {}", code(id))
        };
        inv.edit(&msg).await?;
        Ok(())
    }

    async fn disable(&self, inv: &Invocation, sub: &str) -> CmdResult {
        let id = inv.arg(1);
        if id.is_empty() {
            return Err(kit::usage(inv.prefix(), &format!("acron {sub} ID")));
        }
        let mut found = false;
        kit::update_ok(&self.store, |st| {
            if let Some(idx) = st.find(id) {
                st.tasks[idx].disabled = true;
                found = true;
            }
        })?;
        if !found {
            return Err(not_found(inv, id));
        }
        self.unschedule(id).await;
        let msg = if kit::is_zh() {
            format!("✅ 已关闭定时任务 {}", code(id))
        } else {
            format!("✅ Disabled scheduled task {}", code(id))
        };
        inv.edit(&msg).await?;
        Ok(())
    }

    async fn enable(self: &Arc<Self>, inv: &Invocation, sub: &str) -> CmdResult {
        let id = inv.arg(1);
        if id.is_empty() {
            return Err(kit::usage(inv.prefix(), &format!("acron {sub} ID")));
        }
        let st = self.read()?;
        let Some(idx) = st.find(id) else {
            return Err(not_found(inv, id));
        };
        let mut t = st.tasks[idx].clone();
        if parse_cron(&t.cron).is_err() {
            return Err(kit::fail(if kit::is_zh() {
                format!("定时任务 {id} 的 Cron 表达式无效，没法开启")
            } else {
                format!("Scheduled task {id} has an invalid Cron expression and cannot be enabled")
            }));
        }
        self.schedule(id, &t.cron).await;
        if let Err(err) = self.update_task(id, |task| task.disabled = false) {
            self.unschedule(id).await;
            return Err(err);
        }
        t.disabled = false;
        let head = if kit::is_zh() {
            format!("✅ 已开启定时任务 {}", code(id))
        } else {
            format!("✅ Enabled scheduled task {}", code(id))
        };
        let mut lines = vec![head];
        if let Some(next) = next_run(&t.cron, SystemTime::now()) {
            lines.push(format!("{}{}", kit::tr("Next run: ", "下次执行："), format_time(next)));
        }
        lines.push(format!("{}{}", kit::tr("Copy: ", "复制："), copy_html(&t, inv.prefix())));
        inv.edit(&lines.join("\n")).await?;
        Ok(())
    }
}

async fn fill_send(inv: &Invocation, t: &mut Task) -> CmdResult {
    let Some(reply) = kit::reply_msg(inv).await? else {
        return Err(kit::fail(kit::tr(
            "Reply to a text message to schedule sending its text",
            "请回复一条文字消息，到时间发的就是它的文字",
        )));
    };
    let raw = reply.raw;
    if let Some(media) = &raw.media {
        match media {
            tl::enums::MessageMedia::WebPage(_) | tl::enums::MessageMedia::Empty => {}
            _ => {
                return Err(kit::fail(kit::tr(
                    "Scheduled send only supports plain text; use copy or forward for media",
                    "定时发送只存文字，带媒体的消息请用 copy 或 forward",
                )));
            }
        }
    }
    if raw.reply_markup.is_some() {
        return Err(kit::fail(kit::tr(
            "Scheduled send only supports plain text; use copy or forward for messages with buttons",
            "定时发送只存文字，带按钮的消息请用 copy 或 forward",
        )));
    }
    if raw.message.trim().is_empty() {
        return Err(kit::fail(kit::tr(
            "Replied message has no text",
            "被回复的消息里没有文字",
        )));
    }
    t.message = raw.message;
    t.entities = encode_entities(raw.entities.as_deref().unwrap_or(&[]));
    Ok(())
}

async fn target(client: &Client, t: &Task) -> Result<(tl::enums::InputPeer, String), CmdError> {
    if !t.chat_id.is_empty()
        && let Ok(peer) = client.input_peer_from_chat_id(&t.chat_id)
    {
        return Ok((peer, t.chat_id.clone()));
    }
    if t.chat.trim().is_empty() {
        return Err(kit::fail(kit::tr("Task has no target chat", "任务里没有对话")));
    }
    let peer = client.resolve_target(&t.chat).await.map_err(|e| {
        let label = if kit::is_zh() {
            format!("找不到对话 {}", t.chat)
        } else {
            format!("Chat {} not found", t.chat)
        };
        kit::fail_with(label, e)
    })?;
    let chat_id = peer_of(&peer, client.self_id())
        .map(|p| peer_id(&p))
        .unwrap_or_default();
    if t.resolved_peer && !t.chat_id.is_empty() && t.chat_id != chat_id {
        return Err(kit::fail(kit::tr(
            "Target now points to a different chat; please recreate this task",
            "目标已指向另一个对话，请重新添加任务",
        )));
    }
    Ok((peer, chat_id))
}

fn source(client: &Client, t: &Task) -> Result<(tl::enums::InputPeer, i32), CmdError> {
    let id = message_id_i32(&t.from_msg_id);
    if id == 0 {
        return Err(kit::fail(kit::tr("Invalid source message ID", "源消息 ID 无效")));
    }
    let from = client
        .input_peer_from_chat_id(&t.from_chat_id)
        .map_err(|e| kit::fail_with(kit::tr("Source chat not found", "找不到源消息所在的对话"), e))?;
    Ok((from, id))
}

async fn copy_message(client: &Client, t: &Task, to: &tl::enums::InputPeer, reply_to: i32) -> CmdResult {
    let (from, id) = source(client, t)?;
    let found = client.get_messages(&from, &[id]).await?;
    let Some(original) = found.into_iter().find(|m| m.id == id) else {
        return Err(kit::fail(kit::tr("Source message no longer exists", "源消息已经不在了")));
    };
    let media = copy_media(&original)?;
    let raw_ents = original.entities.as_deref().unwrap_or(&[]);
    let entities = input_entities(client.peers(), raw_ents);
    if let Some(input_media) = media {
        client
            .send_media(
                to,
                input_media,
                mibot_bot::MediaOptions {
                    caption: original.message,
                    entities: (!entities.is_empty()).then_some(entities),
                    reply_to: (reply_to > 0).then_some(reply_to),
                    html: false,
                    ..Default::default()
                },
            )
            .await?;
        return Ok(());
    }
    if original.rich_message.is_some() {
        let full = match client.full_rich(&from, original.clone()).await {
            Ok(f) => f,
            Err(_) => original,
        };
        client
            .copy_rich(
                to,
                &full,
                SendOptions {
                    reply_to: (reply_to > 0).then_some(reply_to),
                    ..Default::default()
                },
            )
            .await?;
        return Ok(());
    }
    if original.message.trim().is_empty() {
        return Err(kit::fail(kit::tr(
            "Source message has no copyable content",
            "源消息里没有能复制的内容",
        )));
    }
    let has_preview = matches!(original.media, Some(tl::enums::MessageMedia::WebPage(_)));
    client
        .send_plain(
            to,
            &original.message,
            (!entities.is_empty()).then_some(entities),
            SendOptions {
                reply_to: (reply_to > 0).then_some(reply_to),
                no_webpage: !has_preview,
                ..Default::default()
            },
        )
        .await?;
    Ok(())
}

fn copy_media(message: &tl::types::Message) -> Result<Option<tl::enums::InputMedia>, CmdError> {
    let Some(media) = &message.media else {
        return Ok(None);
    };
    match media {
        tl::enums::MessageMedia::Empty | tl::enums::MessageMedia::WebPage(_) => Ok(None),
        tl::enums::MessageMedia::Photo(p) => {
            if let Some(tl::enums::Photo::Photo(photo)) = &p.photo {
                return Ok(Some(
                    tl::types::InputMediaPhoto {
                        spoiler: p.spoiler,
                        live_photo: false,
                        id: tl::types::InputPhoto {
                            id: photo.id,
                            access_hash: photo.access_hash,
                            file_reference: photo.file_reference.clone(),
                        }
                        .into(),
                        ttl_seconds: None,
                        video: None,
                    }
                    .into(),
                ));
            }
            Err(kit::fail(kit::tr(
                "This message type cannot be copied; use forward instead",
                "这种消息没法复制，可以改用 forward 定时转发",
            )))
        }
        tl::enums::MessageMedia::Document(d) => {
            if let Some(tl::enums::Document::Document(doc)) = &d.document {
                return Ok(Some(
                    tl::types::InputMediaDocument {
                        spoiler: d.spoiler,
                        id: tl::types::InputDocument {
                            id: doc.id,
                            access_hash: doc.access_hash,
                            file_reference: doc.file_reference.clone(),
                        }
                        .into(),
                        video_cover: None,
                        video_timestamp: None,
                        ttl_seconds: None,
                        query: None,
                    }
                    .into(),
                ));
            }
            Err(kit::fail(kit::tr(
                "This message type cannot be copied; use forward instead",
                "这种消息没法复制，可以改用 forward 定时转发",
            )))
        }
        tl::enums::MessageMedia::Geo(g) => {
            if let tl::enums::GeoPoint::Point(pt) = &g.geo {
                return Ok(Some(
                    tl::types::InputMediaGeoPoint {
                        geo_point: tl::types::InputGeoPoint {
                            lat: pt.lat,
                            long: pt.long,
                            accuracy_radius: pt.accuracy_radius,
                        }
                        .into(),
                    }
                    .into(),
                ));
            }
            Err(kit::fail(kit::tr(
                "This message type cannot be copied; use forward instead",
                "这种消息没法复制，可以改用 forward 定时转发",
            )))
        }
        tl::enums::MessageMedia::Venue(v) => {
            if let tl::enums::GeoPoint::Point(pt) = &v.geo {
                return Ok(Some(
                    tl::types::InputMediaVenue {
                        geo_point: tl::types::InputGeoPoint {
                            lat: pt.lat,
                            long: pt.long,
                            accuracy_radius: pt.accuracy_radius,
                        }
                        .into(),
                        title: v.title.clone(),
                        address: v.address.clone(),
                        provider: v.provider.clone(),
                        venue_id: v.venue_id.clone(),
                        venue_type: v.venue_type.clone(),
                    }
                    .into(),
                ));
            }
            Err(kit::fail(kit::tr(
                "This message type cannot be copied; use forward instead",
                "这种消息没法复制，可以改用 forward 定时转发",
            )))
        }
        tl::enums::MessageMedia::Contact(c) => Ok(Some(
            tl::types::InputMediaContact {
                phone_number: c.phone_number.clone(),
                first_name: c.first_name.clone(),
                last_name: c.last_name.clone(),
                vcard: c.vcard.clone(),
            }
            .into(),
        )),
        tl::enums::MessageMedia::Dice(d) => Ok(Some(
            tl::types::InputMediaDice {
                emoticon: d.emoticon.clone(),
            }
            .into(),
        )),
        tl::enums::MessageMedia::Poll(p) => {
            let tl::enums::Poll::Poll(poll) = &p.poll;
            Ok(Some(
                tl::types::InputMediaPoll {
                    poll: tl::types::Poll {
                        id: mibot_bot::random_id(),
                        closed: false,
                        public_voters: false,
                        multiple_choice: poll.multiple_choice,
                        quiz: false,
                        open_answers: false,
                        revoting_disabled: false,
                        shuffle_answers: false,
                        hide_results_until_close: false,
                        creator: false,
                        subscribers_only: false,
                        question: poll.question.clone(),
                        answers: poll.answers.clone(),
                        close_period: None,
                        close_date: None,
                        countries_iso2: None,
                        hash: 0,
                    }
                    .into(),
                    correct_answers: None,
                    attached_media: None,
                    solution: None,
                    solution_entities: None,
                    solution_media: None,
                }
                .into(),
            ))
        }
        _ => Err(kit::fail(kit::tr(
            "This message type cannot be copied; use forward instead",
            "这种消息没法复制，可以改用 forward 定时转发",
        ))),
    }
}

async fn delete_matching(client: &Client, peer: &tl::enums::InputPeer, t: &Task) -> Result<usize, CmdError> {
    let Some(limit) = t.limit.trim().parse::<usize>().ok().filter(|&v| (1..=MAX_SCAN).contains(&v)) else {
        return Err(kit::fail(if kit::is_zh() {
            format!("条数要在 1–{MAX_SCAN} 之间")
        } else {
            format!("Count must be between 1 and {MAX_SCAN}")
        }));
    };
    if t.regex.trim().is_empty() {
        return Err(kit::fail(kit::tr(
            "Task has no regular expression",
            "任务里没有正则表达式",
        )));
    }
    let pattern = compile_regex(&t.regex)?;
    let messages = recent_messages(client, peer, limit).await?;
    let ids: Vec<i32> = messages
        .into_iter()
        .filter(|m| pattern.is_match(&m.message))
        .map(|m| m.id)
        .collect();
    for chunk in ids.chunks(100) {
        client.delete_messages(peer, chunk).await?;
    }
    Ok(ids.len())
}

async fn recent_messages(
    client: &Client,
    peer: &tl::enums::InputPeer,
    limit: usize,
) -> Result<Vec<tl::types::Message>, mibot_bot::BotError> {
    let mut found = Vec::new();
    let mut offset = 0i32;
    let mut seen = 0usize;
    while seen < limit {
        let batch = 100.min(limit - seen) as i32;
        let res = client
            .invoke(&tl::functions::messages::GetHistory {
                peer: peer.clone(),
                offset_id: offset,
                offset_date: 0,
                add_offset: 0,
                limit: batch,
                max_id: 0,
                min_id: 0,
                hash: 0,
            })
            .await?;
        let page = kit::unpack_messages(res);
        let page_len = page.len();
        for item in page {
            if seen >= limit {
                break;
            }
            seen += 1;
            let id = kit::message_id(&item);
            if offset == 0 || id < offset {
                offset = id;
            }
            if let tl::enums::Message::Message(m) = item {
                found.push(m);
            }
        }
        if (page_len as i32) < batch {
            break;
        }
    }
    Ok(found)
}

fn render_list<F: Fn(&Task) -> String>(
    tasks: &[Task],
    all: bool,
    kind: &str,
    prefix: &str,
    chat: F,
    now: SystemTime,
) -> String {
    let mut title = if all {
        kit::tr("All Scheduled Tasks", "全部定时任务").to_owned()
    } else {
        kit::tr("Scheduled Tasks Here", "当前对话的定时任务").to_owned()
    };
    if !kind.is_empty() {
        title.push_str(" · ");
        title.push_str(type_label(kind));
    }
    if tasks.is_empty() {
        let hint = if all {
            if kit::is_zh() {
                format!("还没有，{} 看怎么添加", code(&format!("{prefix}help acron")))
            } else {
                format!(
                    "None yet; see {} for how to add one",
                    code(&format!("{prefix}help acron"))
                )
            }
        } else if kit::is_zh() {
            format!("还没有，{} 看全部", code(&format!("{prefix}acron la")))
        } else {
            format!("None yet; use {} to view all", code(&format!("{prefix}acron la")))
        };
        return format!("⏰ <b>{title}</b>\n\n{hint}");
    }
    let header = if kit::is_zh() {
        format!("⏰ <b>{title}</b>（{} 个）", tasks.len())
    } else {
        format!("⏰ <b>{title}</b> ({})", tasks.len())
    };
    let mut lines = vec![header];
    for disabled in [false, true] {
        let group_tasks: Vec<&Task> = tasks.iter().filter(|t| t.disabled == disabled).collect();
        if group_tasks.is_empty() {
            continue;
        }
        lines.push(String::new());
        lines.push(format!("<b>{}</b>", kit::on_off_text(!disabled)));
        for t in group_tasks {
            lines.push(String::new());
            lines.extend(task_lines(t, prefix, &chat(t), now));
        }
    }
    lines.join("\n")
}

fn task_lines(t: &Task, prefix: &str, chat: &str, now: SystemTime) -> Vec<String> {
    let mut head = format!("{} · {}", code(&t.id), escape(type_label(&t.kind)));
    if !t.remark.is_empty() {
        head.push_str(&format!(" · {}", escape(&t.remark)));
    }
    let mut lines = vec![head, format!("{}{chat}", kit::tr("Chat: ", "对话："))];
    if !t.disabled {
        let cid = kit::or_default(&t.chat_id, &t.chat);
        if !t.msg_id.is_empty() {
            lines.push(format!(
                "{}{}",
                kit::tr("Message: ", "消息："),
                message_link(cid, &t.msg_id)
            ));
        }
        if !t.from_chat_id.is_empty() && !t.from_msg_id.is_empty() {
            lines.push(format!(
                "{}{}",
                kit::tr("Source: ", "源消息："),
                message_link(&t.from_chat_id, &t.from_msg_id)
            ));
        }
        if !t.reply_to.is_empty() && t.kind == "forward" {
            lines.push(format!(
                "{}{}",
                kit::tr("Topic: ", "话题："),
                message_link(cid, &t.reply_to)
            ));
        } else if !t.reply_to.is_empty() && matches!(t.kind.as_str(), "send" | "cmd" | "copy") {
            lines.push(format!(
                "{}{}",
                kit::tr("Reply: ", "回复："),
                message_link(cid, &t.reply_to)
            ));
        }
        if let Some(next) = next_run(&t.cron, now) {
            lines.push(format!(
                "{}{}",
                kit::tr("Next run: ", "下次执行："),
                format_time(next)
            ));
        }
    }
    if let Some(at) = parse_millis(&t.last_run_at) {
        lines.push(format!(
            "{}{}",
            kit::tr("Last run: ", "上次执行："),
            format_time(at)
        ));
    }
    if !t.last_result.is_empty() {
        lines.push(format!(
            "{}{}",
            kit::tr("Result: ", "结果："),
            escape(&t.last_result)
        ));
    }
    if !t.last_error.is_empty() {
        lines.push(format!(
            "{}{}",
            kit::tr("Error: ", "错误："),
            escape(&t.last_error)
        ));
    }
    lines.push(format!(
        "{}{}",
        kit::tr("Copy: ", "复制："),
        copy_html(t, prefix)
    ));
    lines
}

fn not_found(inv: &Invocation, id: &str) -> CmdError {
    kit::fail(if kit::is_zh() {
        format!(
            "没有 ID 是 {id} 的定时任务，{}acron la 可以看全部",
            inv.prefix()
        )
    } else {
        format!(
            "No scheduled task with ID {id}; use {}acron la to list all",
            inv.prefix()
        )
    })
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let c = |text: &str| format!("<code>{p}acron {}</code>", escape(text));
    if kit::is_zh() {
        format!(
            "⏰ <b>定时任务</b>\n\n\
             按六段 Cron 定时发送、复制、转发、删除、置顶消息，或者执行命令。Cron 依次是 秒 分 时 日 月 星期，按北京时间；下面的 {} 是每天 2 点。\n\n\
             对话写数字 ID、@用户名或 me（收藏夹）。send、copy、cmd 可以在对话后面加「|消息 ID」回复那条消息，论坛里写话题 ID 就发进那个话题；forward 后面只能写话题 ID。末尾都可以加备注。\n\n\
             <b>发送</b>\n\
             • {} 回复一条文字消息，到时间把这段文字连同格式发到对话\n\
             • {} 回复一条消息，到时间复制过去，不带转发来源，图片和文件也行\n\
             • {} 回复一条消息，到时间转发过去\n\n\
             <b>执行命令</b>\n\
             命令写在第二行，到时间发到对话，再按当前的前缀和别名执行。比如每天 2 点备份：\n\
             <pre>{p}acron cmd 0 0 2 * * * me 定时备份\n{p}bf</pre>\n\
             列表里的结果写明是「已提交执行命令」还是「已发送命令（未执行：原因）」。\n\n\
             <b>删除与置顶</b>\n\
             • {} 删除一条消息\n\
             • {} 在最近 100 条消息里删除文字匹配正则的；条数 1–1000，正则写成 /模式/标志 或只写模式，标志可用 i、m、s，不支持环视和反向引用\n\
             • {} 置顶；两个数字依次是发不发通知、是不是只对自己置顶（私聊里），写 1|0，也收 true|false、yes|no\n\
             • {} 取消置顶\n\n\
             <b>管理</b>\n\
             • {} 当前对话的任务；all 看全部，写类型（如 del）只看这一种；{} 同 ls all\n\
             • {} 删除任务\n\
             • {}、{} 关闭、开启任务（也收 disable、enable）",
            code("0 0 2 * * *"),
            c("send 0 0 2 * * * 对话 [备注]"),
            c("copy 0 0 2 * * * 对话 [备注]"),
            c("forward 0 0 2 * * * 对话 [备注]"),
            c("del 0 0 2 * * * 对话 消息 ID [备注]"),
            c("del_re 0 0 2 * * * 对话 100 /^test/i [备注]"),
            c("pin 0 0 2 * * * 对话 消息 ID 1 0 [备注]"),
            c("unpin 0 0 2 * * * 对话 消息 ID [备注]"),
            c("ls [all] [类型]"),
            c("la"),
            c("rm ID"),
            c("off ID"),
            c("on ID"),
        )
    } else {
        format!(
            "⏰ <b>Scheduled Tasks</b>\n\n\
             Schedule sending, copying, forwarding, deleting, pinning messages, or running commands with a 6-field Cron expression (sec min hour day month weekday, UTC+8); {} below means 02:00 daily.\n\n\
             Chat can be a numeric ID, @username, or me (Saved Messages). For send/copy/cmd, append \"|msg_id\" to reply (or topic ID in forums); forward only accepts a topic ID. Optional remarks go at the end.\n\n\
             <b>Messages</b>\n\
             • {} Reply to a text message to send its text and formatting on schedule\n\
             • {} Reply to any message to copy it without forward header (media supported)\n\
             • {} Reply to a message to forward it on schedule\n\n\
             <b>Commands</b>\n\
             Put the command on the second line; it is sent to the chat and dispatched using current prefixes/aliases (e.g. daily backup at 02:00):\n\
             <pre>{p}acron cmd 0 0 2 * * * me backup\n{p}bf</pre>\n\
             Task list shows whether it was queued or sent without execution.\n\n\
             <b>Delete &amp; Pin</b>\n\
             • {} Delete a message\n\
             • {} Delete messages matching regex within last 1–1000 messages (/pattern/flags or pattern; flags: i, m, s; no lookaround/backreferences)\n\
             • {} Pin a message; flags are notify and pm-only (1|0, true|false, yes|no)\n\
             • {} Unpin a message\n\n\
             <b>Management</b>\n\
             • {} List tasks in current chat (add all or task type to filter); {} equals ls all\n\
             • {} Remove a task\n\
             • {}, {} Disable or enable a task (also accepts disable/enable)",
            code("0 0 2 * * *"),
            c("send 0 0 2 * * * chat [remark]"),
            c("copy 0 0 2 * * * chat [remark]"),
            c("forward 0 0 2 * * * chat [remark]"),
            c("del 0 0 2 * * * chat msg_id [remark]"),
            c("del_re 0 0 2 * * * chat 100 /^test/i [remark]"),
            c("pin 0 0 2 * * * chat msg_id 1 0 [remark]"),
            c("unpin 0 0 2 * * * chat msg_id [remark]"),
            c("ls [all] [type]"),
            c("la"),
            c("rm ID"),
            c("off ID"),
            c("on ID"),
        )
    }
}

pub fn register(app: &Arc<App>) {
    let app_weak = Arc::clone(app);
    let dispatch: DispatchFn = Arc::new(move |cancel, msg| {
        let Some(env) = mibot_bot::envelope(&msg, app_weak.client.self_id(), false, &app_weak.peers) else {
            return DispatchOutcome::NotCommand;
        };
        app_weak.registry.dispatch(env, cancel)
    });
    let svc = Arc::new(Service {
        app: Some(Arc::clone(app)),
        store: kit::new_store(app, "acron.json", State::default),
        dispatch: Some(dispatch),
        mu: Mutex::new(ServiceState::default()),
        serial: Mutex::new(()),
    });
    let s_cmd = Arc::clone(&svc);
    app.registry.register(
        Command {
            name: COMMAND_NAME.into(),
            group: group::MEDIA.into(),
            description: "Schedule messages and commands".into(),
            usage: "type cron chat [args]".into(),
            help_fn: Some(Arc::new(help)),
            handle: Arc::new(move |inv| {
                let s = Arc::clone(&s_cmd);
                Box::pin(async move { s.handle(&inv).await })
            }),
            ..Default::default()
        }
        .zh("定时发送、删除消息或执行命令", "类型 Cron 对话 [参数]"),
    );

    let s_job = Arc::clone(&svc);
    app.on_ready(Arc::new(move |app, cancel| {
        let s = Arc::clone(&s_job);
        let client = Arc::clone(&app.client);
        Box::pin(async move {
            s.start(cancel, client).await;
        })
    }));
    let _ = TASK_TYPES;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, PeerCache, RpcTransport, SentRecorder, parse_html};
    use mibot_logtail::Ring;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-acron-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_parse_cron_regex_helpers_and_entities_roundtrip() {
        for good in ["0 0 2 * * *", "*/30 * * * * *", "0 30 9 * * 1-5", "0 0 9 * * 7", "0 0 9 * * 1-7", "0 0 0 1 JAN SUN"] {
            assert!(parse_cron(good).is_ok(), "good={good}");
        }
        for bad in ["0 2 * * *", "@daily", "0 0 2 * * * *", "0 0 25 * * *", "a b c d e f", ""] {
            assert!(parse_cron(bad).is_err(), "bad={bad}");
        }
        assert_eq!(sunday_seven("7"), "0");
        assert_eq!(sunday_seven("1-7"), "1-6,0");
        assert_eq!(sunday_seven("1,7"), "1,0");
        assert_eq!(sunday_seven("5-7/2"), "5-7/2");

        let dt = shanghai_offset().with_ymd_and_hms(2026, 9, 30, 10, 0, 0).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(dt.timestamp() as u64);
        assert_eq!(format_time(next_run("0 0 2 * * *", now).unwrap()), "2026-10-01 02:00");
        assert_eq!(format_time(next_run("0 0 9 * * 7", now).unwrap()), "2026-10-04 09:00");
        assert_eq!(format_time(next_run("30 0 10 * * *", now).unwrap()), "2026-09-30 10:00:30");

        let re = compile_regex(r"/^test/i").unwrap();
        assert!(re.is_match("TEST 1") && !re.is_match("a test"));
        let re_u = compile_regex(r"/\u4f60/g").unwrap();
        assert!(re_u.is_match("你好") && !re_u.is_match("好"));
        for bad in ["/a/x", "(?=a)", r"(a)\1", "", "//i"] {
            assert!(compile_regex(bad).is_err(), "bad_re={bad}");
        }

        assert_eq!(message_id_i32("4294967301"), 0);
        assert_eq!(split_target("@a|12"), ("@a".into(), "12".into()));
        assert_eq!(split_target("-1001｜5"), ("-1001".into(), "5".into()));
        assert_eq!(remark_after("acron del 0 0 2 * * * me 12   早上  清理 ", 10), "早上  清理");

        let v2_entities = r#"[{"offset":0,"length":2,"className":"MessageEntityBold"},{"offset":2,"length":3,"url":"https://x.y","className":"MessageEntityTextUrl"},{"offset":5,"length":2,"documentId":"5368324170671202286","className":"MessageEntityCustomEmoji"},{"offset":7,"length":1,"userId":"12345","className":"MessageEntityMentionName"},{"offset":8,"length":1,"language":"go","className":"MessageEntityPre"},{"offset":9,"length":1,"collapsed":true,"className":"MessageEntityBlockquote"}]"#;
        let val: serde_json::Value = serde_json::from_str(v2_entities).unwrap();
        let decoded = decode_entities(Some(&val));
        assert_eq!(decoded.len(), 6);
        let re_encoded = encode_entities(&decoded).unwrap();
        assert_eq!(re_encoded, val);

        let v2_data = format!(
            r#"{{"schemaVersion":1,"seq":"4","tasks":[
                {{"id":"1","type":"send","cron":"0 0 8 * * *","chat":"me","chatId":"1","resolvedPeer":true,"createdAt":"1759000000000","delivery":"sent","display":"<code>me</code>","message":"早上好","entities":{v2_entities},"remark":"早安","lastRunAt":"1759100000000","lastResult":"已发送 1 条消息"}},
                {{"id":2,"type":"pin","cron":"0 0 9 * * 1-5","chat":"@friend","chatId":42,"resolvedPeer":true,"createdAt":"1759000000001","delivery":"pending","msgId":"77","notify":true,"pmOneSide":false,"disabled":true}},
                {{"id":"3","type":"del_re","cron":"0 */5 * * * *","chat":"-100100","createdAt":"1759000000002","limit":"50","regex":"/广告/i","lastError":"DEL_RE_TIMEOUT"}},
                {{"id":"4","type":"forward","cron":"0 0 12 * * *","chat":"-100100","chatId":"-100100","resolvedPeer":true,"replyTo":"3","fromChatId":"42","fromMsgId":"5","createdAt":"1759000000003","delivery":"prepared"}}]}}"#
        );
        let mut st: State = serde_json::from_str(&v2_data).unwrap();
        st.normalize();
        assert_eq!(st.tasks.len(), 4);
        assert_eq!(st.next_id(), "5");
        assert_eq!(st.tasks[1].id, "2");
        assert_eq!(st.tasks[1].chat_id, "42");
        let html = render_list(&st.tasks, true, "", ".", |t| code(&t.chat), UNIX_EPOCH + Duration::from_millis(1759200000000));
        parse_html(&html, None).unwrap();
        assert!(html.contains(" (4)"));
        assert!(html.contains("Copy: <code>.acron send 0 0 8 * * * me 早安</code>"));
    }

    struct FakeTg {
        next_id: std::sync::atomic::AtomicI32,
        stored: StdMutex<HashMap<i32, tl::types::Message>>,
        history: StdMutex<Vec<tl::types::Message>>,
        edits: StdMutex<Vec<String>>,
        sent_msgs: StdMutex<Vec<tl::functions::messages::SendMessage>>,
        deletes: StdMutex<Vec<tl::functions::messages::DeleteMessages>>,
        pins: StdMutex<Vec<tl::functions::messages::UpdatePinnedMessage>>,
        forwards: StdMutex<Vec<tl::functions::messages::ForwardMessages>>,
        medias: StdMutex<Vec<tl::functions::messages::SendMedia>>,
    }

    impl FakeTg {
        fn new() -> Self {
            Self {
                next_id: std::sync::atomic::AtomicI32::new(10),
                stored: StdMutex::new(HashMap::new()),
                history: StdMutex::new(Vec::new()),
                edits: StdMutex::new(Vec::new()),
                sent_msgs: StdMutex::new(Vec::new()),
                deletes: StdMutex::new(Vec::new()),
                pins: StdMutex::new(Vec::new()),
                forwards: StdMutex::new(Vec::new()),
                medias: StdMutex::new(Vec::new()),
            }
        }
    }

    use std::sync::Mutex as StdMutex;

    fn friend_user() -> tl::enums::User {
        tl::types::User {
            access_hash: Some(7),
            first_name: Some("Friend".into()),
            username: Some("friend".into()),
            ..kit::blank_tl_user(42)
        }
        .into()
    }

    impl RpcTransport for FakeTg {
        fn invoke_raw<'a>(&'a self, _: Option<i32>, cid: u32, body: Vec<u8>) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                if cid == tl::functions::contacts::ResolveUsername::CONSTRUCTOR_ID {
                    let req = tl::functions::contacts::ResolveUsername::from_bytes(&body[4..]).unwrap();
                    if req.username != "friend" {
                        return Err(InvocationError::Rpc(RpcError {
                            code: 400, name: "USERNAME_NOT_OCCUPIED".into(), value: None, caused_by: None,
                        }));
                    }
                    let resp: tl::enums::contacts::ResolvedPeer = tl::types::contacts::ResolvedPeer {
                        peer: tl::types::PeerUser { user_id: 42 }.into(),
                        chats: vec![],
                        users: vec![friend_user()],
                    }
                    .into();
                    return Ok(resp.to_bytes());
                }
                if cid == tl::functions::messages::SendMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::SendMessage::from_bytes(&body[4..]).unwrap();
                    self.sent_msgs.lock().unwrap().push(req);
                    let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    let resp = tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                        out: true, id, pts: 1, pts_count: 1, date: 0, media: None, entities: None, ttl_period: None,
                    });
                    return Ok(resp.to_bytes());
                }
                if cid == tl::functions::messages::GetMessages::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::GetMessages::from_bytes(&body[4..]).unwrap();
                    let g = self.stored.lock().unwrap();
                    let mut msgs = Vec::new();
                    for item in req.id {
                        if let tl::enums::InputMessage::Id(i) = item
                            && let Some(m) = g.get(&i.id)
                        {
                            msgs.push(m.clone().into());
                        }
                    }
                    let resp = kit::tl_messages(msgs, vec![], vec![friend_user()]);
                    return Ok(resp.to_bytes());
                }
                if cid == tl::functions::messages::GetHistory::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::GetHistory::from_bytes(&body[4..]).unwrap();
                    let h = self.history.lock().unwrap();
                    let mut page = Vec::new();
                    for m in h.iter().rev() {
                        if page.len() >= req.limit as usize {
                            break;
                        }
                        if req.offset_id == 0 || m.id < req.offset_id {
                            page.push(m.clone().into());
                        }
                    }
                    let resp = kit::tl_messages(page, vec![], vec![]);
                    return Ok(resp.to_bytes());
                }
                if cid == tl::functions::messages::DeleteMessages::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::DeleteMessages::from_bytes(&body[4..]).unwrap();
                    self.deletes.lock().unwrap().push(req);
                    let resp: tl::enums::messages::AffectedMessages =
                        tl::types::messages::AffectedMessages { pts: 1, pts_count: 1 }.into();
                    return Ok(resp.to_bytes());
                }
                if cid == tl::functions::messages::EditMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::EditMessage::from_bytes(&body[4..]).unwrap();
                    self.edits.lock().unwrap().push(req.message.unwrap_or_default());
                } else if cid == tl::functions::messages::UpdatePinnedMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::UpdatePinnedMessage::from_bytes(&body[4..]).unwrap();
                    self.pins.lock().unwrap().push(req);
                } else if cid == tl::functions::messages::ForwardMessages::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::ForwardMessages::from_bytes(&body[4..]).unwrap();
                    self.forwards.lock().unwrap().push(req);
                } else if cid == tl::functions::messages::SendMedia::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::SendMedia::from_bytes(&body[4..]).unwrap();
                    self.medias.lock().unwrap().push(req);
                } else {
                    return Err(InvocationError::Dropped("unexpected rpc"));
                }
                let resp = tl::enums::Updates::Updates(tl::types::Updates {
                    updates: vec![], users: vec![], chats: vec![], date: 0, seq: 0,
                });
                Ok(resp.to_bytes())
            })
        }
    }

    fn make_chan_100() -> tl::enums::Chat {
        tl::types::Channel {
            access_hash: Some(9),
            ..kit::blank_tl_channel(100, "频道")
        }
        .into()
    }

    #[tokio::test]
    async fn test_execute_tasks_and_management() {
        let dir = temp_dir("exec");
        let fake = Arc::new(FakeTg::new());
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        peers.remember_chats(&[make_chan_100()]);
        let client = Arc::new(Client::from_transport(
            fake.clone(),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let app = App::for_testing(
            dir.clone(),
            "test",
            "boot",
            Arc::clone(&client),
            Arc::clone(&peers),
            Arc::new(Ring::new()),
        );
        app.registry.set_prefixes(vec![".".into(), ",".into()]);
        app.registry.set_aliases(&HashMap::from([("ac".into(), "acron".into()), ("backup".into(), "bf".into())]));
        register(&app);

        let dispatched = Arc::new(StdMutex::new(Vec::<tl::types::Message>::new()));
        let disp_ref = Arc::clone(&dispatched);
        let svc = Arc::new(Service {
            app: Some(Arc::clone(&app)),
            store: kit::new_store(&app, "acron.json", State::default),
            dispatch: Some(Arc::new(move |_, m| {
                let is_cmd = m.message.starts_with('.');
                disp_ref.lock().unwrap().push(m);
                if is_cmd { DispatchOutcome::Queued } else { DispatchOutcome::NotCommand }
            })),
            mu: Mutex::new(ServiceState::default()),
            serial: Mutex::new(()),
        });

        let tok = CancellationToken::new();
        // 1. Reassigned target rejected
        kit::update_ok(&svc.store, |s| {
            s.tasks = vec![Task {
                id: "1".into(), kind: "del".into(), chat: "@friend".into(),
                chat_id: "43".into(), resolved_peer: true, msg_id: "7".into(), ..Default::default()
            }];
        }).unwrap();
        svc.execute(&tok, &client, "1").await;
        assert!(fake.deletes.lock().unwrap().is_empty());
        assert!(svc.read().unwrap().tasks[0].last_error.contains("different chat"));

        // 2. Send preserves IP and saves chatId
        kit::update_ok(&svc.store, |s| {
            s.tasks = vec![Task {
                id: "1".into(), kind: "send".into(), cron: "0 0 8 * * *".into(),
                chat: "@friend".into(), reply_to: "5".into(), delivery: DELIVERY_PENDING.into(),
                message: "早上好 1.2.3.4".into(),
                entities: serde_json::from_str(r#"[{"offset":0,"length":3,"className":"MessageEntityBold"}]"#).ok(),
                ..Default::default()
            }];
        }).unwrap();
        svc.execute(&tok, &client, "1").await;
        {
            let sent = fake.sent_msgs.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].message, "早上好 1.2.3.4");
            let t = &svc.read().unwrap().tasks[0];
            assert_eq!(t.delivery, DELIVERY_SENT);
            assert_eq!(t.chat_id, "42");
            assert!(t.resolved_peer);
        }

        // 3. Command dispatch + blocks self-invocation
        kit::update_ok(&svc.store, |s| {
            s.tasks = vec![
                Task { id: "1".into(), kind: "cmd".into(), cron: "0 0 2 * * *".into(), chat: "me".into(), chat_id: "1".into(), resolved_peer: true, message: ".bf".into(), reply_to: "7".into(), ..Default::default() },
                Task { id: "2".into(), kind: "cmd".into(), cron: "0 0 2 * * *".into(), chat: "me".into(), chat_id: "1".into(), resolved_peer: true, message: "hello".into(), ..Default::default() },
                Task { id: "3".into(), kind: "cmd".into(), cron: "0 * * * * *".into(), chat: "me".into(), chat_id: "1".into(), resolved_peer: true, message: ".ac ls".into(), ..Default::default() },
            ];
        }).unwrap();
        svc.execute(&tok, &client, "1").await;
        svc.execute(&tok, &client, "2").await;
        svc.execute(&tok, &client, "3").await;
        assert_eq!(dispatched.lock().unwrap().len(), 2);
        let st = svc.read().unwrap();
        assert_eq!(st.tasks[0].last_result, "Command queued for execution");
        assert!(st.tasks[1].last_result.starts_with("Command sent (not executed:"));
        assert!(st.tasks[2].last_error.contains("invokes .acron itself"));

        // 4. Add rejects self invocation (alias/prefix)
        let mk_inv = |text: &str, reply_to: i32| {
            let trimmed = text.strip_prefix('.').unwrap_or(text);
            let fields: Vec<String> = trimmed.split_whitespace().map(str::to_owned).collect();
            let mut message = kit::blank_bot_message(500, tl::types::PeerUser { user_id: 1 }.into());
            message.text = text.into();
            message.out = true;
            message.reply_to_id = reply_to;
            Invocation {
                client: Arc::clone(&client),
                registry: Arc::clone(&app.registry),
                message,
                prefix: ".".into(),
                command: fields[0].clone(),
                args: fields[1..].to_vec(),
                raw: trimmed.split_once(char::is_whitespace).map(|x| x.1.to_owned()).unwrap_or_default(),
                cancel: CancellationToken::new(),
                foreign: false,
            }
        };
        let err = svc.handle(&mk_inv(".acron cmd 0 0 2 * * * me\n.ac ls", 0)).await.unwrap_err();
        assert!(err.to_string().contains("cannot be .acron"));

        // 5. del_re matching
        {
            let mut h = fake.history.lock().unwrap();
            for (id, txt) in [(1, "广告 旧的"), (2, "广告"), (3, "正常"), (4, "广告 AD"), (5, "ad 广告")] {
                let mut m = kit::blank_tl_message(id, tl::types::PeerUser { user_id: 42 }.into());
                m.message = txt.into();
                h.push(m);
            }
        }
        kit::update_ok(&svc.store, |s| {
            s.tasks = vec![
                Task { id: "1".into(), kind: "del_re".into(), cron: "0 * * * * *".into(), chat: "@friend".into(), limit: "3".into(), regex: "/^广告/i".into(), ..Default::default() },
                Task { id: "2".into(), kind: "del_re".into(), cron: "0 * * * * *".into(), chat: "@friend".into(), limit: "5000".into(), regex: "x".into(), ..Default::default() },
            ];
        }).unwrap();
        svc.execute(&tok, &client, "1").await;
        svc.execute(&tok, &client, "2").await;
        assert_eq!(fake.deletes.lock().unwrap()[0].id, vec![4]);
        let st2 = svc.read().unwrap();
        assert_eq!(st2.tasks[0].last_result, "Matched and deleted 1 message(s)");
        assert_eq!(st2.tasks[1].last_error, "Count must be between 1 and 1000");

        let _ = std::fs::remove_dir_all(dir);
    }
}
