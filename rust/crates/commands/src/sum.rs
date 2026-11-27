use crate::ai::{
    self, ChatRequest, HtmlStyle, RequestInfo, Service as AiService, assert_allowed_model,
    chat_token_limit_field, endpoint_protocol, http_failure, markdown_html, markdown_rich,
    parse_text, provider_auth, provider_endpoint, valid_reasoning, valid_tier,
};
use crate::kit;
use chrono::{DateTime, Local, TimeZone, Utc};
use croner::Cron;
use croner::parser::{CronParser, Seconds};
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{
    Client, SendOptions, code, escape, rich_concat, rich_fits, rich_plain, rich_rejected,
};
use mibot_command::{
    CmdError, CmdResult, Command, Invocation, WorkGuard, brief, fail, group, html_pages,
};
use mibot_httpx::{Method, Request, post_json};
use mibot_logtail::Level;
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

const SUM_TYPES: &[&str] = &["auto", "chat", "responses", "gemini", "anthropic"];

const SUM_MARKDOWN_PROMPT: &str = "你是 Telegram 群聊摘要助手。根据以下聊天记录，只输出 Markdown 格式的中文总结。\n\n只使用 ### 栏目标题、- 列表、**粗体**、`代码` 和 [文字](链接)；不要使用表格、代码块、HTML 标签、裸 URL、<https://...>。聊天记录中每条消息末尾都有“来源”链接。每条摘要、资源、结论、互动、零散信息或时间线条目都必须附带最对应的 Telegram 原消息链接，格式为 [来源](Telegram消息链接)；不要编造链接。\n\n只记录聊天中明确出现的事实、反馈、决定和计划。只有存在明确完成反馈、验证结果或维护者确认时，才可使用“已确认”“已解决”“已完成”等表达；个人测试、成员讨论或推测使用“有人反馈”“初步判断”“可能”“尚待复测”“未见最终确认”等表述。不要把“计划支持”“准备测试”“正在修改”写成已经实现或可用。合并重复消息，忽略纯寒暄、表情、广告、机器人状态和无结论闲聊。\n\n总长度控制在 900-1600 个中文字符；重要讨论较多时可接近上限。信息应完整、可回溯，但不要逐条复述聊天记录。\n\n固定输出：\n### 📌 本次摘要\n用 2-3 句话概括本次聊天背景、关键结果和当前状态；末尾附 1-2 个 [来源](Telegram消息链接)。\n\n随后按实际内容选择下列栏目，不相关的栏目完全不要输出：\n- `### 💬 主要话题`：日常交流、综合讨论、一般观点或群内共识。\n- `### 🧩 技术与项目`：技术方案、配置、开发、排障、版本更新、命令和实现细节。\n- `### 📰 资源分享`：重要外部链接、文件、工具、新闻或可复用资源。\n- `### 👥 重要互动`：明确的求助、答复、邀请、提醒、分工、争议或值得关注的人际互动。\n- `### 🗂 零散信息`：无法归入其他栏目但值得保留的版本、环境、数据、状态、背景或简短结论。\n- `### 🕒 时间线梳理`：仅在同一轮聊天出现多个明确时间点，且时间顺序有助于理解事件进展时输出。\n\n不要输出“待处理事项”“行动项”“下一步”这类面向管理者的栏目；群成员未必负责跟进。若聊天中存在未解决问题、风险或后续计划，将其放入最相关的上述栏目，并使用“仍待确认”“尚待复测”“计划继续”等中性表述。\n\n每个栏目使用以下格式：\n### 栏目标题\n- 要点：说明结论、必要背景、明确分歧、风险或计划 [来源](Telegram消息链接)\n- 要点：说明结论、必要背景、明确分歧、风险或计划 [来源](Telegram消息链接)\n\n规则：\n1. 每个栏目 1-3 条；每条建议 35-90 个中文字符。内容多时优先压缩重复过程，保留结论、关键依据、数据、风险和计划。\n2. 技术内容较多时，可在“🧩 技术与项目”栏目内用单独一行的 **1. 小标题** 分组，小标题下面接列表；最多 3 个小标题，每个小标题只保留 1-2 条。\n3. 时间线每条使用“- `HH:MM`：事件概述 [来源](Telegram消息链接)”；最多 4 条，只保留转折、决定、故障、修复或重要更新。\n4. 命令、模型名、插件名、配置名、版本号、错误码使用 `...`。\n5. 外部链接仅在确实影响后续操作时保留，格式为 [名称](完整URL)，并在同一条末尾保留 Telegram [来源](Telegram消息链接)。\n6. 不输出空栏目、“无”“暂无”“未发现”或处理过程。栏目之间空一行，只输出最终总结。";

fn sum_default_prompt() -> String {
    format!(
        "{SUM_MARKDOWN_PROMPT}\n\n来源链接仅适用于聊天记录里有来源链接的对话。私聊、收藏夹和普通群没有消息链接，此时省略所有来源标注，保留记录中实际出现的外部资源链接。"
    )
}

const SUM_LEGACY_PROMPT: &str = "你是 Telegram 群聊摘要助手。根据以下聊天记录，只输出 Telegram HTML 格式的中文总结。\n\n允许使用 <b>、<code>、<a href=\"...\">、<blockquote expandable>；禁止使用 Markdown、#、**、```、[文字](链接)、裸 URL、<https://...>。聊天记录中每条消息末尾都有“来源”链接。每条摘要、资源、结论、互动、零散信息或时间线条目都必须附带最对应的 Telegram 原消息链接，格式为 <a href=\"Telegram消息链接\">来源</a>；不要编造链接。\n\n只记录聊天中明确出现的事实、反馈、决定和计划。只有存在明确完成反馈、验证结果或维护者确认时，才可使用“已确认”“已解决”“已完成”等表达；个人测试、成员讨论或推测使用“有人反馈”“初步判断”“可能”“尚待复测”“未见最终确认”等表述。不要把“计划支持”“准备测试”“正在修改”写成已经实现或可用。合并重复消息，忽略纯寒暄、表情、广告、机器人状态和无结论闲聊。\n\n总长度控制在 900-1600 个中文字符；重要讨论较多时可接近上限。信息应完整、可回溯，但不要逐条复述聊天记录。\n\n固定输出：\n<b>📌 本次摘要</b>\n用 2-3 句话概括本次聊天背景、关键结果和当前状态；末尾附 1-2 个 <a href=\"Telegram消息链接\">来源</a>。\n\n随后按实际内容选择下列栏目，不相关的栏目完全不要输出：\n<b>💬 主要话题</b>：日常交流、综合讨论、一般观点或群内共识。\n<b>🧩 技术与项目</b>：技术方案、配置、开发、排障、版本更新、命令和实现细节。\n<b>📰 资源分享</b>：重要外部链接、文件、工具、新闻或可复用资源。\n<b>👥 重要互动</b>：明确的求助、答复、邀请、提醒、分工、争议或值得关注的人际互动。\n<b>🗂 零散信息</b>：无法归入其他栏目但值得保留的版本、环境、数据、状态、背景或简短结论。\n<b>🕒 时间线梳理</b>：仅在同一轮聊天出现多个明确时间点，且时间顺序有助于理解事件进展时输出。\n\n不要输出“待处理事项”“行动项”“下一步”这类面向管理者的栏目；群成员未必负责跟进。若聊天中存在未解决问题、风险或后续计划，将其放入最相关的上述栏目，并使用“仍待确认”“尚待复测”“计划继续”等中性表述。\n\n每个栏目使用以下格式：\n<b>栏目标题</b>\n<blockquote expandable>• 要点：说明结论、必要背景、明确分歧、风险或计划 <a href=\"Telegram消息链接\">来源</a>\n• 要点：说明结论、必要背景、明确分歧、风险或计划 <a href=\"Telegram消息链接\">来源</a></blockquote>\n\n规则：\n1. 每个栏目 1-3 条；每条建议 35-90 个中文字符。内容多时优先压缩重复过程，保留结论、关键依据、数据、风险和计划。\n2. 技术内容较多时，可在 <b>🧩 技术与项目</b> 内使用 <b>1. 小标题</b> 分组；最多 3 个小标题，每个小标题只保留 1-2 条。\n3. 时间线每条使用“<code>HH:MM</code>：事件概述 <a href=\"Telegram消息链接\">来源</a>”；最多 4 条，只保留转折、决定、故障、修复或重要更新。\n4. 命令、模型名、插件名、配置名、版本号、错误码使用 <code>...</code>。\n5. 外部链接仅在确实影响后续操作时保留，格式为 <a href=\"完整URL\">名称</a>，并在同一条末尾保留 Telegram <a href=\"Telegram消息链接\">来源</a>。\n6. 不输出空栏目、“无”“暂无”“未发现”或处理过程。每个栏目之间空一行，只输出最终总结。";

static INTERVAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\d+)(m|h|d)$").expect("valid interval re"));
static SIMPLE_INTERVAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\d+[mhd]$").expect("valid simple interval re"));
static CRON_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w*?,/\-#]+$").expect("valid cron token re"));
static DOW_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:[0-9*?,/#-]+|(?:SUN|MON|TUE|WED|THU|FRI|SAT)(?:[-,/#](?:SUN|MON|TUE|WED|THU|FRI|SAT|[0-7*]))*)$",
    )
    .expect("valid dow re")
});
static DIGITS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("valid digits re"));
static QUOTED_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""([^"\\]*(?:\\.[^"\\]*)*)"|'([^'\\]*(?:\\.[^'\\]*)*)'|(\S+)"#)
        .expect("valid quoted token re")
});
static ESCAPED_QUOTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\\(["'])"#).expect("valid escaped quote re"));
static INVITE_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:https?://)?t\.me/(?:\+|joinchat/)([A-Za-z0-9_-]+)").expect("invite re")
});
static PRIVATE_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:https?://)?t\.me/c/(\d+)").expect("private re"));
static PUBLIC_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:https?://)?t\.me/([A-Za-z0-9_]+)").expect("public re"));
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s\]）】>]+").expect("url re"));
static RESPONSES_MODEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(gpt-[5-9]|o[1-9])").expect("responses model re"));
static UNSUPPORTED_ENDPOINT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)unsupported_upstream|endpoint|not supported").expect("unsup re")
});
static API_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)/v[0-9]+(?:(?:alpha|beta)[0-9]*)?$").expect("api ver re"));
static THINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<thinking>.*?</thinking>|<think>.*?</think>").expect("think re")
});

fn de_str_or_num<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = Value::deserialize(deserializer)?;
    match val {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        Value::Null => Ok("0".to_owned()),
        other => Ok(other.to_string()),
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct SumProvider {
    name: String,
    base_url: String,
    api_key: String,
    model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    r#type: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct SumAiConfig {
    providers: HashMap<String, SumProvider>,
    #[serde(skip_serializing_if = "String::is_empty")]
    default_provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    default_prompt: String,
    default_spoiler: bool,
    #[serde(skip_serializing_if = "is_zero_i32")]
    default_timeout: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    default_reasoning_effort: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    default_service_tier: String,
    reply_mode: bool,
    max_output_length: i32,
    link_preview: bool,
    #[serde(rename = "aiMigrated", default, skip_serializing_if = "is_false")]
    ai_migrated: bool,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct SumTask {
    #[serde(deserialize_with = "de_str_or_num")]
    id: String,
    cron: String,
    #[serde(rename = "chatId")]
    chat_id: String,
    #[serde(rename = "chatDisplay", default, skip_serializing_if = "String::is_empty")]
    chat_display: String,
    interval: String,
    #[serde(rename = "messageCount")]
    message_count: i32,
    #[serde(rename = "timeRange", default, skip_serializing_if = "is_zero_i32")]
    time_range: i32,
    #[serde(rename = "pushTarget", default, skip_serializing_if = "String::is_empty")]
    push_target: String,
    #[serde(rename = "aiProvider", default, skip_serializing_if = "String::is_empty")]
    ai_provider: String,
    #[serde(rename = "aiPrompt", default, skip_serializing_if = "String::is_empty")]
    ai_prompt: String,
    #[serde(rename = "useSpoiler")]
    use_spoiler: bool,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SumDb {
    #[serde(deserialize_with = "de_str_or_num")]
    seq: String,
    tasks: Vec<SumTask>,
    #[serde(rename = "aiConfig")]
    ai_config: SumAiConfig,
    #[serde(rename = "defaultPushTarget", default, skip_serializing_if = "String::is_empty")]
    default_push_target: String,
}

impl Default for SumDb {
    fn default() -> Self {
        Self {
            seq: "0".into(),
            tasks: Vec::new(),
            ai_config: SumAiConfig {
                providers: HashMap::new(),
                default_provider: String::new(),
                default_prompt: sum_default_prompt(),
                default_spoiler: false,
                default_timeout: 60000,
                default_reasoning_effort: "auto".into(),
                default_service_tier: "auto".into(),
                reply_mode: false,
                max_output_length: 0,
                link_preview: false,
                ai_migrated: false,
            },
            default_push_target: String::new(),
        }
    }
}

impl SumDb {
    fn normalize(&mut self) {
        if self.seq.is_empty() {
            self.seq = "0".into();
        }
        if self.ai_config.default_prompt == SUM_LEGACY_PROMPT
            || self.ai_config.default_prompt == SUM_MARKDOWN_PROMPT
        {
            self.ai_config.default_prompt = sum_default_prompt();
        }
        for task in &mut self.tasks {
            if task.ai_prompt == SUM_LEGACY_PROMPT || task.ai_prompt == SUM_MARKDOWN_PROMPT {
                task.ai_prompt.clear();
            }
        }
    }
}

fn sum_timeout(db: &SumDb) -> Duration {
    let mut val = db.ai_config.default_timeout;
    if (10..=300).contains(&val) {
        val *= 1000;
    }
    if val <= 0 {
        val = 60000;
    }
    Duration::from_millis(val as u64)
}

#[derive(Debug, Clone)]
struct SumMessage {
    text: String,
    content: String,
    link: String,
    urls: Vec<String>,
    file_name: String,
}

fn sum_message_link(peer: Option<&tl::enums::Peer>, msg_id: i32, username: &str) -> String {
    let Some(tl::enums::Peer::Channel(ch)) = peer else {
        return String::new();
    };
    if !username.is_empty() {
        format!("https://t.me/{username}/{msg_id}")
    } else {
        format!("https://t.me/c/{}/{msg_id}", ch.channel_id)
    }
}

fn sum_file_name(media: Option<&tl::enums::MessageMedia>) -> String {
    match media {
        Some(tl::enums::MessageMedia::Document(m)) => {
            let Some(tl::enums::Document::Document(doc)) = &m.document else {
                return String::new();
            };
            if doc.mime_type == "application/x-tgsticker" || doc.mime_type == "video/webm" {
                return String::new();
            }
            let mut name = String::new();
            for attr in &doc.attributes {
                match attr {
                    tl::enums::DocumentAttribute::Sticker(_)
                    | tl::enums::DocumentAttribute::CustomEmoji(_) => return String::new(),
                    tl::enums::DocumentAttribute::Filename(f) => name = f.file_name.clone(),
                    _ => {}
                }
            }
            if !name.is_empty() {
                name
            } else if !doc.mime_type.is_empty() {
                format!("[{}]", doc.mime_type)
            } else {
                String::new()
            }
        }
        Some(tl::enums::MessageMedia::Photo(_)) => "[图片]".into(),
        _ => String::new(),
    }
}

fn sum_entity_urls(msg: &tl::types::Message) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(entities) = &msg.entities {
        for entity in entities {
            match entity {
                tl::enums::MessageEntity::TextUrl(u) => urls.push(u.url.clone()),
                tl::enums::MessageEntity::Url(u) => {
                    let link = kit::utf16_slice(&msg.message, u.offset, u.length);
                    if !link.is_empty() {
                        urls.push(link);
                    }
                }
                _ => {}
            }
        }
    }
    urls
}

fn sum_message_text(date: i32, sender: &str, content: &str, file_name: &str) -> String {
    let mut text = content.to_owned();
    if !file_name.is_empty() {
        if !text.is_empty() {
            text.push_str(&format!(" [文件: {file_name}]"));
        } else {
            text = format!("[文件: {file_name}]");
        }
    }
    let dt = Local
        .timestamp_opt(i64::from(date), 0)
        .single()
        .unwrap_or_else(Local::now);
    format!("[{}] {sender}: {text}", dt.format("%Y-%m-%d %H:%M"))
}

fn sum_format_messages(rows: &[SumMessage]) -> String {
    let mut lines = Vec::with_capacity(rows.len());
    for row in rows {
        let mut line = row.text.clone();
        if !row.link.is_empty() {
            line.push_str(&format!(" [来源]({})", row.link));
        }
        lines.push(line);
    }
    let mut urls: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for row in rows {
        for u in &row.urls {
            if seen.insert(u.clone()) {
                urls.push((u.clone(), row.link.clone()));
            }
        }
        for m in URL_RE.find_iter(&row.content) {
            let link = m.as_str().to_owned();
            if seen.insert(link.clone()) {
                urls.push((link, row.link.clone()));
            }
        }
        if !row.file_name.is_empty() {
            files.push((row.file_name.clone(), row.link.clone()));
        }
    }
    let mut result = lines.join("\n");
    for (title, items) in [
        ("消息中包含的外部链接", &urls),
        ("消息中包含的附件", &files),
    ] {
        if items.is_empty() {
            continue;
        }
        result.push_str(&format!("\n\n--- {title} ---\n"));
        for (item, link) in items {
            result.push_str(item);
            if !link.is_empty() {
                result.push_str(&format!(" - [查看原消息]({link})"));
            }
            result.push('\n');
        }
    }
    result
}

fn sum_detect_protocol(provider: &SumProvider) -> (String, bool) {
    if !provider.r#type.is_empty() && provider.r#type != "auto" && provider.r#type != "openai" {
        return (provider.r#type.clone(), false);
    }
    if let Ok(u) = Url::parse(&provider.base_url) {
        let protocol = endpoint_protocol(u.path());
        if !protocol.is_empty() {
            return (protocol.to_owned(), true);
        }
    }
    let model = provider.model.to_ascii_lowercase();
    let stripped = model.strip_prefix("models/").unwrap_or(&model);
    if stripped.starts_with("gemini") {
        return ("gemini".into(), true);
    }
    if model.starts_with("claude") {
        return ("anthropic".into(), true);
    }
    if RESPONSES_MODEL_RE.is_match(&model) {
        return ("responses".into(), true);
    }
    ("chat".into(), true)
}

fn sum_provider_endpoint(raw: &str, format: &str, endpoint: &str) -> Result<String, CmdError> {
    let target = provider_endpoint(raw, format, endpoint)?;
    let base = Url::parse(raw).map_err(|_| fail(kit::tr("Invalid BaseURL", "BaseURL 无效")))?;
    let mut address =
        Url::parse(&target).map_err(|_| fail(kit::tr("Invalid BaseURL", "BaseURL 无效")))?;
    let configured_path = base.path().trim_end_matches('/');
    let mut prefix = address
        .path()
        .strip_suffix(&format!("/{endpoint}"))
        .unwrap_or(address.path())
        .to_owned();
    let version = if format == "gemini" { "/v1beta" } else { "/v1" };

    if prefix == configured_path && (prefix.ends_with("/v1") || prefix.ends_with("/v1beta")) {
        if let Some(pos) = prefix.rfind('/') {
            prefix = format!("{}{version}", &prefix[..pos]);
        }
    } else if prefix == configured_path
        && !API_VERSION_RE.is_match(&prefix)
        && !prefix.ends_with("/openai")
    {
        prefix.push_str(version);
    } else if format == "openai"
        && prefix.ends_with("/v1beta")
        && configured_path.starts_with(&format!("{prefix}/models/"))
    {
        prefix = format!(
            "{}{version}",
            prefix.strip_suffix("/v1beta").unwrap_or(&prefix)
        );
    }
    address.set_path(&format!("{prefix}/{endpoint}"));
    Ok(address.into())
}

async fn sum_call_ai(
    tag: &str,
    mut provider: SumProvider,
    messages: &str,
    prompt: &str,
    reasoning: &str,
    tier: &str,
    timeout: Duration,
) -> Result<String, CmdError> {
    if assert_allowed_model(&provider.model).is_err() {
        return Err(fail(kit::tr(
            "Configured model is disallowed by project policy; please switch models",
            "配置模型被项目策略禁止，请更换模型",
        )));
    }
    let input = format!("{prompt}\n\n{messages}");
    let started = Instant::now();
    let mut info = RequestInfo {
        tag: tag.to_owned(),
        model: provider.model.clone(),
        stream: false,
        responses: false,
        timeout,
        stage: "request".into(),
    };
    let mut secret = provider.api_key.clone();
    let mut format_used = String::new();

    let mut do_protocol = async |protocol: &str, prov: &SumProvider| {
        info.responses = protocol == "responses";
        let (fmt_name, endpoint, data): (&str, String, Value) = match protocol {
            "gemini" => (
                "gemini",
                format!(
                    "models/{}:generateContent",
                    url::form_urlencoded::byte_serialize(
                        prov.model.strip_prefix("models/").unwrap_or(&prov.model).as_bytes()
                    )
                    .collect::<String>()
                ),
                json!({
                    "contents": [{ "role": "user", "parts": [{ "text": &input }] }]
                }),
            ),
            "anthropic" => (
                "anthropic",
                "messages".to_owned(),
                json!({
                    "model": &prov.model,
                    "max_tokens": 2000,
                    "messages": [{ "role": "user", "content": &input }]
                }),
            ),
            "responses" => {
                let mut body = json!({
                    "model": &prov.model,
                    "input": [{ "role": "user", "content": [{ "type": "input_text", "text": &input }] }],
                    "max_output_tokens": 2000,
                    "store": false
                });
                if reasoning != "auto" {
                    body["reasoning"] = json!({ "effort": reasoning });
                }
                if tier != "auto" {
                    body["service_tier"] = json!(tier);
                }
                ("openai", "responses".to_owned(), body)
            }
            _ => {
                let limit_field = chat_token_limit_field(&prov.model);
                let mut body = json!({
                    "model": &prov.model,
                    "messages": [{ "role": "user", "content": &input }],
                });
                body[limit_field] = json!(2000);
                if reasoning != "auto" {
                    body["reasoning_effort"] = json!(reasoning);
                }
                if tier != "auto" {
                    body["service_tier"] = json!(tier);
                }
                ("openai", "chat/completions".to_owned(), body)
            }
        };
        format_used = fmt_name.to_owned();
        let target = sum_provider_endpoint(&prov.base_url, fmt_name, &endpoint)?;
        let (target, headers, actual) = provider_auth(fmt_name, &prov.api_key, &target);
        secret = actual;
        let resp = post_json(
            Request {
                method: Method::POST,
                url: target,
                headers,
                timeout: Some(timeout),
                max_bytes: Some(ai::AI_RESPONSE_LIMIT),
                wait_for_headers: true,
                ..Default::default()
            },
            &data,
        )
        .await?;
        Ok::<_, CmdError>(resp)
    };

    let (protocol, auto) = sum_detect_protocol(&provider);
    let mut result = match do_protocol(&protocol, &provider).await {
        Ok(r) => r,
        Err(e) => return Err(info.failure(e, started, &secret)),
    };

    if auto
        && protocol != "chat"
        && (result.status == 404
            || (result.status == 400 && UNSUPPORTED_ENDPOINT_RE.is_match(&String::from_utf8_lossy(&result.body))))
    {
        if protocol == "gemini"
            && let Ok(mut u) = Url::parse(&provider.base_url)
        {
            let pairs: Vec<(String, String)> = u
                .query_pairs()
                .filter(|(k, _)| k != "key")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            if pairs.is_empty() {
                u.set_query(None);
            } else {
                u.query_pairs_mut().clear().extend_pairs(pairs);
            }
            provider.base_url = u.into();
        }
        result = match do_protocol("chat", &provider).await {
            Ok(r) => r,
            Err(e) => return Err(info.failure(e, started, &secret)),
        };
    }

    info.stage = "response".into();
    if !result.ok() {
        let detail = http_failure(result.status, &result.body, &secret);
        let e = fail(if kit::is_zh() {
            format!("AI 调用失败：{detail}")
        } else {
            format!("AI call failed: {detail}")
        });
        return Err(info.failure(e, started, &secret));
    }
    info.stage = "decode".into();
    match parse_text(&result.body, &format_used, &secret) {
        Ok(text) => Ok(text),
        Err(e) => Err(info.failure(e, started, &secret)),
    }
}

enum SumBackend {
    Own { name: String, provider: SumProvider },
    Ai { ai_tag: String, models: Arc<AiService> },
}

fn resolve_backend(
    db: &SumDb,
    requested: &str,
    models: Option<&Arc<AiService>>,
) -> Result<SumBackend, CmdError> {
    let name = if requested.is_empty() {
        db.ai_config.default_provider.as_str()
    } else {
        requested
    };
    if !name.is_empty()
        && let Some(prov) = db.ai_config.providers.get(name)
        && !prov.api_key.trim().is_empty()
    {
        return Ok(SumBackend::Own {
            name: name.to_owned(),
            provider: prov.clone(),
        });
    }
    if let Some(ai_svc) = models {
        if !name.is_empty() && ai_svc.has_provider(name) {
            return Ok(SumBackend::Ai {
                ai_tag: name.to_owned(),
                models: Arc::clone(ai_svc),
            });
        }
        if requested.is_empty() {
            return Ok(SumBackend::Ai {
                ai_tag: String::new(),
                models: Arc::clone(ai_svc),
            });
        }
        return Err(fail(if kit::is_zh() {
            format!("未找到 AI 配置：{requested}")
        } else {
            format!("AI config not found: {requested}")
        }));
    }
    if !name.is_empty() && db.ai_config.providers.contains_key(name) {
        return Err(fail(if kit::is_zh() {
            format!("AI 配置 {name} 缺少 API Key")
        } else {
            format!("AI config {name} is missing an API key")
        }));
    }
    if !requested.is_empty() {
        return Err(fail(if kit::is_zh() {
            format!("未找到 AI 配置：{requested}")
        } else {
            format!("AI config not found: {requested}")
        }));
    }
    Err(fail(kit::tr(
        "Add an AI config with sum config add, or configure a chat model with ai config add and ai model chat",
        "请先用 sum config add 添加 AI 配置，或用 ai config add 和 ai model chat 配置聊天模型",
    )))
}

impl SumBackend {
    async fn call(&self, db: &SumDb, messages: &str, prompt: &str) -> Result<String, CmdError> {
        let timeout = sum_timeout(db);
        match self {
            SumBackend::Own { name, provider } => {
                let reasoning = if valid_reasoning(&db.ai_config.default_reasoning_effort) {
                    db.ai_config.default_reasoning_effort.as_str()
                } else {
                    "auto"
                };
                let tier = if valid_tier(&db.ai_config.default_service_tier) {
                    db.ai_config.default_service_tier.as_str()
                } else {
                    "auto"
                };
                sum_call_ai(
                    name,
                    provider.clone(),
                    messages,
                    prompt,
                    reasoning,
                    tier,
                    timeout,
                )
                .await
            }
            SumBackend::Ai { ai_tag, models, .. } => {
                models
                    .chat(ChatRequest {
                        tag: ai_tag.clone(),
                        text: messages.to_owned(),
                        system_prompt: prompt.to_owned(),
                        max_output_tokens: 2000,
                        fallback_to_chat: true,
                        min_timeout: timeout,
                    })
                    .await
            }
        }
    }
}

fn parse_sum_cron(spec: &str) -> Result<Cron, CmdError> {
    let fields: Vec<&str> = spec.split_whitespace().collect();
    if fields.len() != 5 && fields.len() != 6 {
        return Err(fail(kit::tr("Invalid cron expression", "定时表达式无效")));
    }
    let joined = fields.join(" ");
    let parser = CronParser::builder()
        .seconds(Seconds::Optional)
        .dom_and_dow(true)
        .build();
    parser
        .parse(&joined)
        .map_err(|_| fail(kit::tr("Invalid cron expression", "定时表达式无效")))
}

fn sum_interval_cron(interval: &str) -> Result<String, CmdError> {
    let fields: Vec<&str> = interval.split_whitespace().collect();
    if fields.len() == 5 || fields.len() == 6 {
        let spec = fields.join(" ");
        parse_sum_cron(&spec)?;
        return Ok(spec);
    }
    let Some(caps) = INTERVAL_RE.captures(interval) else {
        return Err(fail(kit::tr(
            "Interval examples: 30m, 2h, 1d, or a 5/6-field cron expression",
            "间隔格式示例：30m、2h、1d，或五、六字段的 Cron",
        )));
    };
    let val: i32 = caps[1].parse().unwrap_or(0);
    if val <= 0 {
        return Err(fail(kit::tr("Interval must be a positive integer", "间隔必须为正整数")));
    }
    match caps[2].to_ascii_lowercase().as_str() {
        "m" => {
            if val > 59 || 60 % val != 0 {
                return Err(fail(kit::tr(
                    "Minute interval must be 1–59 and divide 60 evenly",
                    "分钟间隔须为 1-59 且能整除 60",
                )));
            }
            Ok(format!("*/{val} * * * *"))
        }
        "h" => {
            if val > 23 || 24 % val != 0 {
                return Err(fail(kit::tr(
                    "Hour interval must be 1–23 and divide 24 evenly",
                    "小时间隔须为 1-23 且能整除 24",
                )));
            }
            Ok(format!("0 */{val} * * *"))
        }
        _ => {
            if val != 1 {
                return Err(fail(kit::tr(
                    "Day interval currently only supports 1d",
                    "当前按天间隔仅支持 1d",
                )));
            }
            Ok("0 0 * * *".to_owned())
        }
    }
}

fn sum_next_run(spec: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let cron = parse_sum_cron(spec).ok()?;
    cron.find_next_occurrence(&now, false).ok()
}

fn sum_parse_time(value: &str) -> Option<DateTime<Local>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Local));
    }
    if let Ok(ms) = value.parse::<i64>()
        && ms > 0
    {
        return Local.timestamp_millis_opt(ms).single();
    }
    None
}

fn sum_format_time(dt: DateTime<Local>) -> String {
    dt.format("%Y-%m-%d %H:%M").to_string()
}

fn sum_cron_like(value: &str) -> bool {
    !value.starts_with("--") && CRON_TOKEN_RE.is_match(value)
}

fn sum_day_of_week(value: &str) -> bool {
    if !DOW_TOKEN_RE.is_match(value) {
        return false;
    }
    for m in DIGITS_RE.find_iter(value) {
        if m.as_str().parse::<u32>().unwrap_or(99) > 7 {
            return false;
        }
    }
    true
}

fn sum_add_arguments(inv: &Invocation) -> Vec<String> {
    let full = format!("{}{} {}", inv.prefix, inv.command, inv.raw);
    let line = full.lines().next().unwrap_or("");
    let stripped = line.trim().strip_prefix(&inv.prefix).unwrap_or(line.trim()).trim();
    let mut values = Vec::new();
    for caps in QUOTED_TOKEN_RE.captures_iter(stripped) {
        let raw_val = if let Some(m) = caps.get(1) {
            m.as_str()
        } else if let Some(m) = caps.get(2) {
            m.as_str()
        } else {
            caps.get(3).map_or("", |m| m.as_str())
        };
        values.push(ESCAPED_QUOTE_RE.replace_all(raw_val, "$1").into_owned());
    }
    if values.len() >= 2 && values[1].eq_ignore_ascii_case("add") {
        return values[2..].to_vec();
    }
    if inv.args.len() > 1 {
        return inv.args[1..].to_vec();
    }
    Vec::new()
}

fn sum_add_syntax(args: &[String]) -> (String, String, Vec<String>) {
    if args.is_empty() {
        return (String::new(), String::new(), Vec::new());
    }
    let target = args[0].clone();
    let first = args.get(1).cloned().unwrap_or_default();
    if SIMPLE_INTERVAL_RE.is_match(&first) || first.contains(' ') {
        let start = args.len().min(2);
        return (target, first, args[start..].to_vec());
    }
    if args.len() >= 7
        && args[1..6].iter().all(|v| sum_cron_like(v))
        && sum_day_of_week(&args[6])
    {
        return (target, args[1..7].join(" "), args[7..].to_vec());
    }
    if args.len() >= 6 && args[1..6].iter().all(|v| sum_cron_like(v)) {
        return (target, args[1..6].join(" "), args[6..].to_vec());
    }
    let start = args.len().min(2);
    (target, first, args[start..].to_vec())
}

struct SumAddOptions {
    count: i32,
    time_range: i32,
    provider: String,
    spoiler: Option<bool>,
    remark: String,
}

fn sum_parse_add_options(rest: &[String]) -> Result<SumAddOptions, CmdError> {
    let mut opts = SumAddOptions {
        count: 100,
        time_range: 0,
        provider: String::new(),
        spoiler: None,
        remark: String::new(),
    };
    let mut remark = Vec::new();
    let mut idx = 0;
    while idx < rest.len() {
        let val = &rest[idx];
        if val == "--time" || val == "--provider" {
            if idx + 1 >= rest.len() || rest[idx + 1].is_empty() {
                return Err(fail(if kit::is_zh() {
                    format!("请提供 {val} 的值")
                } else {
                    format!("Please provide a value for {val}")
                }));
            }
            idx += 1;
            if val == "--provider" {
                opts.provider = rest[idx].clone();
            } else {
                let hours = rest[idx].parse::<i32>().unwrap_or(0);
                if hours <= 0 || hours > 720 {
                    return Err(fail(kit::tr(
                        "Time range must be 1–720 hours",
                        "时间范围须为 1-720 小时",
                    )));
                }
                opts.time_range = hours;
            }
        } else if val == "--spoiler" || val == "--no-spoiler" {
            opts.spoiler = Some(val == "--spoiler");
        } else if kit::is_digits(val) {
            opts.count = val.parse::<i32>().unwrap_or(100);
        } else {
            remark.push(val.clone());
        }
        idx += 1;
    }
    opts.remark = remark.join(" ");
    Ok(opts)
}

fn sum_target_kind(target: &str) -> (String, bool) {
    if target.eq_ignore_ascii_case("here") {
        return ("here".into(), false);
    }
    if kit::is_numeric_id(target) {
        return (target.to_owned(), false);
    }
    if let Some(caps) = INVITE_LINK_RE.captures(target) {
        return (caps[1].to_owned(), true);
    }
    if let Some(caps) = PRIVATE_LINK_RE.captures(target) {
        return (format!("-100{}", &caps[1]), false);
    }
    if let Some(caps) = PUBLIC_LINK_RE.captures(target) {
        return (format!("@{}", &caps[1]), false);
    }
    (format!("@{}", target.trim_start_matches('@')), false)
}

fn peer_of_input(input: &tl::enums::InputPeer, self_id: i64) -> Option<tl::enums::Peer> {
    match input {
        tl::enums::InputPeer::Channel(c) => {
            Some(tl::types::PeerChannel { channel_id: c.channel_id }.into())
        }
        tl::enums::InputPeer::Chat(c) => Some(tl::types::PeerChat { chat_id: c.chat_id }.into()),
        tl::enums::InputPeer::User(u) => Some(tl::types::PeerUser { user_id: u.user_id }.into()),
        tl::enums::InputPeer::PeerSelf => Some(tl::types::PeerUser { user_id: self_id }.into()),
        _ => None,
    }
}

fn chat_id_of(chat: &tl::enums::Chat) -> (String, String, String) {
    match chat {
        tl::enums::Chat::Channel(ch) => (
            format!("-100{}", ch.id),
            ch.title.clone(),
            ch.username.clone().unwrap_or_default(),
        ),
        tl::enums::Chat::Chat(c) => (format!("-{}", c.id), c.title.clone(), String::new()),
        _ => (String::new(), String::new(), String::new()),
    }
}

fn sum_display(title: &str, username: &str) -> String {
    let t = title.trim();
    if !username.is_empty() {
        format!("{t} @{username}").trim().to_owned()
    } else {
        t.to_owned()
    }
}

fn chat_title(client: &Client, peer: Option<&tl::enums::Peer>) -> (String, String) {
    let Some(p) = peer else {
        return (String::new(), String::new());
    };
    if let tl::enums::Peer::Channel(ch) = p
        && let Some(info) = client.peers().channel(ch.channel_id)
    {
        return (info.title, info.username);
    }
    (client.peers().title(p), String::new())
}

fn push_target<'a>(db: &'a SumDb, task: &'a SumTask) -> &'a str {
    if !task.push_target.is_empty() {
        &task.push_target
    } else if !db.default_push_target.is_empty() {
        &db.default_push_target
    } else {
        "me"
    }
}

struct SumJob {
    chat_id: String,
    count: i32,
    time_range: i32,
    provider: String,
    prompt: String,
    spoiler: bool,
    before_id: i32,
}

struct SumResult {
    title: String,
    at: DateTime<Local>,
    output: String,
    spoiler: bool,
}

impl SumResult {
    fn heading(&self) -> String {
        format!("{} · {}", self.title, self.at.format("%Y-%m-%d %H:%M"))
    }

    fn html(&self) -> String {
        let mut body = markdown_html(
            &self.output,
            HtmlStyle {
                collapse_safe: self.spoiler,
                plain_headings: true,
                fold_lists: true,
            },
        );
        if self.spoiler && !body.trim().is_empty() {
            body = format!("<blockquote expandable>{body}</blockquote>");
        }
        format!(
            "🧾 <b>{}</b>\n{}\n\n{body}",
            kit::tr("Group Summary", "群组消息摘要"),
            escape(&self.heading())
        )
    }

    fn rich(&self, client: &Client) -> Option<tl::enums::InputRichMessage> {
        if !client.can_send_rich() {
            return None;
        }
        let header = rich_concat(&[
            rich_plain("🧾 "),
            tl::types::TextBold {
                text: rich_plain(kit::tr("Group Summary", "群组消息摘要")),
            }
            .into(),
            rich_plain(&format!("\n{}", self.heading())),
        ]);
        let mut blocks: Vec<tl::enums::PageBlock> = vec![
            tl::types::PageBlockParagraph { text: header }.into(),
        ];
        blocks.extend(markdown_rich(&self.output, self.spoiler));
        if rich_fits(&blocks) {
            Some(
                tl::types::InputRichMessage {
                    rtl: false,
                    noautolink: false,
                    blocks,
                    photos: None,
                    documents: None,
                    users: None,
                }
                .into(),
            )
        } else {
            None
        }
    }
}

async fn send_summary(
    a: &App,
    client: &Client,
    peer: &tl::enums::InputPeer,
    result: &SumResult,
    opts: SendOptions,
) -> CmdResult {
    if let Some(rich) = result.rich(client) {
        match client.send_rich(peer, rich, opts.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) if rich_rejected(&e) => {
                a.logs.record(
                    Level::Warn,
                    "sum.rich_rejected",
                    &[("error", &e.to_string())],
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    for page in html_pages(&result.html(), mibot_command::PAGE_LIMIT) {
        client.send_html(peer, &page, opts.clone()).await?;
    }
    Ok(())
}

struct SumService {
    a: Arc<App>,
    models: Option<Arc<AiService>>,
    store: Store<SumDb>,
    mu: StdMutex<SchedulerState>,
}

#[derive(Default)]
struct SchedulerState {
    tasks: HashMap<String, JoinHandle<()>>,
    running: HashSet<String>,
    client: Option<Arc<Client>>,
    cancel: Option<CancellationToken>,
}

pub fn register(a: &Arc<App>, models: Option<Arc<AiService>>) {
    let svc = Arc::new(SumService {
        a: Arc::clone(a),
        models,
        store: kit::new_store(a, "sum.json", SumDb::default),
        mu: StdMutex::new(SchedulerState::default()),
    });

    {
        let svc_job = Arc::clone(&svc);
        a.on_ready(Arc::new(move |app, cancel| {
            let svc = Arc::clone(&svc_job);
            Box::pin(async move {
                {
                    let mut g = svc.mu.lock().unwrap_or_else(|e| e.into_inner());
                    g.client = Some(app.client.clone());
                    g.cancel = Some(cancel.clone());
                }
                match svc.read() {
                    Ok(db) => {
                        for task in db.tasks {
                            if let Err(e) = svc.schedule_task(&task) {
                                svc.a.logs.record(
                                    Level::Error,
                                    "sum.register_failed",
                                    &[
                                        ("task", &task.id),
                                        ("cron", &task.cron),
                                        ("error", &e.to_string()),
                                    ],
                                );
                            }
                        }
                    }
                    Err(e) => {
                        svc.a.logs.record(
                            Level::Error,
                            "sum.load_failed",
                            &[("error", &e.to_string())],
                        );
                    }
                }
                cancel.cancelled().await;
                let handles: Vec<JoinHandle<()>> = {
                    let mut g = svc.mu.lock().unwrap_or_else(|e| e.into_inner());
                    g.tasks.drain().map(|(_, h)| h).collect()
                };
                for h in handles {
                    h.abort();
                }
            })
        }));
    }

    let svc_cmd = Arc::clone(&svc);
    a.registry.register(
        Command {
            name: "sum".into(),
            group: group::AI.into(),
            description: "Summarize group messages".into(),
            usage: "[count]".into(),
            help_fn: Some(Arc::new(sum_help)),
            timeout: Some(Duration::from_secs(600)),
            handle: Arc::new(move |inv| {
                let svc = Arc::clone(&svc_cmd);
                Box::pin(async move {
                    svc.handle(&inv)
                        .await
                        .map_err(|e| kit::wrap_fail(kit::tr("Summary failed", "摘要失败"), e))
                })
            }),
            ..Default::default()
        }
        .zh("生成群组消息摘要", "[消息数]"),
    );
}

fn sum_help(prefix: &str) -> String {
    let code_cmd = |args: &str| code(&format!("{prefix}sum{args}"));
    if kit::is_zh() {
        format!(
            "🧾 <b>群组消息摘要</b>\n\n\
             • {} 摘要当前群组最近 100 条消息\n\
             • {} 指定消息数量，范围 10–500\n\
             • {} 临时选择 AI 配置（sum 的配置名或 ai 的标签）\n\n\
             <b>定时摘要</b>\n\
             • {} 定时摘要当前群组，推送到默认目标（默认收藏夹）\n\
             • {}\n\
               群组可以是 here、数字 ID、@用户名、t.me 链接或邀请链接；间隔为 30m、2h、1d，或五、六字段 Cron（六字段第一位是秒）\n\
             • {} / {} / {} / {} / {}\n\
             • {} / {} / {}\n\
             • {} 按当前顺序重新编号\n\
             • {} 预览发给 AI 的文本\n\n\
             <b>AI 配置</b>\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {} / {}\n\
             • {}\n\
             • {}\n\
             • {}\n\n没有 sum 自己的 AI 配置时，使用 ai 命令的聊天模型。",
            code_cmd(""),
            code_cmd(" 200"),
            code_cmd(" 100 --provider 名称"),
            code_cmd(" add here 2h 100"),
            code_cmd(" add @群组 \"0 9,21 * * *\" --time 12 --provider 名称 --spoiler 备注"),
            code_cmd(" list"),
            code_cmd(" run ID"),
            code_cmd(" del ID"),
            code_cmd(" disable ID"),
            code_cmd(" enable ID"),
            code_cmd(" edit ID spoiler on|off"),
            code_cmd(" edit ID provider [名称]"),
            code_cmd(" edit ID prompt [内容]"),
            code_cmd(" reorder"),
            code_cmd(" debug [数量]"),
            code_cmd(" config list"),
            code_cmd(" config add 名称 BaseURL API_KEY 模型 [auto|chat|responses|gemini|anthropic]"),
            code_cmd(" config set default 名称"),
            code_cmd(" config set 名称 model|url|key|type 值"),
            code_cmd(" config set preview|spoiler|reply on|off"),
            code_cmd(" config set push 目标"),
            code_cmd(" config set timeout 秒数"),
            code_cmd(" config set maxoutput 字符数"),
            code_cmd(" config set reasoning|service 值"),
            code_cmd(" config set prompt 内容|reset|show"),
            code_cmd(" config del 名称"),
        )
    } else {
        format!(
            "🧾 <b>Group Summary</b>\n\n\
             • {} Summarize the last 100 messages in the current group\n\
             • {} Specify message count (10–500)\n\
             • {} Temporarily pick an AI config (sum config name or ai tag)\n\n\
             <b>Scheduled Summaries</b>\n\
             • {} Schedule summaries for this group to the default target (Saved Messages)\n\
             • {}\n\
               Group can be here, numeric ID, @username, t.me link, or invite link; interval is 30m, 2h, 1d, or a 5/6-field cron (first field is seconds)\n\
             • {} / {} / {} / {} / {}\n\
             • {} / {} / {}\n\
             • {} Renumber tasks in current order\n\
             • {} Preview the text sent to AI\n\n\
             <b>AI Config</b>\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {} / {}\n\
             • {}\n\
             • {}\n\
             • {}\n\nFalls back to the ai command chat model when no sum AI config is set.",
            code_cmd(""),
            code_cmd(" 200"),
            code_cmd(" 100 --provider name"),
            code_cmd(" add here 2h 100"),
            code_cmd(" add @group \"0 9,21 * * *\" --time 12 --provider name --spoiler note"),
            code_cmd(" list"),
            code_cmd(" run ID"),
            code_cmd(" del ID"),
            code_cmd(" disable ID"),
            code_cmd(" enable ID"),
            code_cmd(" edit ID spoiler on|off"),
            code_cmd(" edit ID provider [name]"),
            code_cmd(" edit ID prompt [text]"),
            code_cmd(" reorder"),
            code_cmd(" debug [count]"),
            code_cmd(" config list"),
            code_cmd(" config add name BaseURL API_KEY model [auto|chat|responses|gemini|anthropic]"),
            code_cmd(" config set default name"),
            code_cmd(" config set name model|url|key|type value"),
            code_cmd(" config set preview|spoiler|reply on|off"),
            code_cmd(" config set push target"),
            code_cmd(" config set timeout seconds"),
            code_cmd(" config set maxoutput chars"),
            code_cmd(" config set reasoning|service value"),
            code_cmd(" config set prompt text|reset|show"),
            code_cmd(" config del name"),
        )
    }
}

impl SumService {
    fn read(&self) -> Result<SumDb, CmdError> {
        let mut db = self.store.read()?;
        db.normalize();
        Ok(db)
    }

    fn update<F>(&self, mutate: F) -> Result<(), CmdError>
    where
        F: FnOnce(&mut SumDb) -> Result<(), CmdError>,
    {
        self.store
            .update(|db| {
                db.normalize();
                mutate(db)
            })
            .map_err(|e| match e {
                mibot_store::UpdateError::Store(se) => se.into(),
                mibot_store::UpdateError::Mutate(ce) => ce,
            })
    }

    fn provider_known(&self, db: &SumDb, name: &str) -> bool {
        if db.ai_config.providers.contains_key(name) {
            return true;
        }
        self.models
            .as_ref()
            .is_some_and(|svc| svc.has_provider(name))
    }

    async fn handle(self: &Arc<Self>, inv: &Invocation) -> CmdResult {
        let sub = inv.arg(0).to_ascii_lowercase();
        match sub.as_str() {
            "?" => inv.edit(&sum_help(inv.prefix())).await.map_err(Into::into),
            "config" => self.config(inv).await,
            "list" | "ls" => self.list(inv).await,
            "run" | "now" => self.run_now(inv).await,
            "del" | "rm" | "disable" | "enable" => self.toggle(inv, &sub).await,
            "add" => self.add(inv).await,
            "edit" => self.edit_task(inv).await,
            "reorder" | "sort" => self.reorder(inv).await,
            "debug" => self.debug(inv).await,
            _ => self.instant(inv, &sub).await,
        }
    }

    async fn instant(&self, inv: &Invocation, sub: &str) -> CmdResult {
        let mut count = 100i32;
        if kit::is_digits(sub) {
            count = sub.parse::<i32>().unwrap_or(100);
        }
        if !(10..=500).contains(&count) {
            return Err(fail(kit::tr(
                "Message count must be between 10 and 500",
                "消息数量范围为 10-500",
            )));
        }
        let mut provider = String::new();
        for i in 0..inv.args.len() {
            if (inv.args[i] == "--provider" || inv.args[i] == "-p") && i + 1 < inv.args.len() {
                provider = inv.args[i + 1].clone();
            }
        }
        inv.edit_text(&kit::working(kit::tr(
            "Generating group summary",
            "正在生成群组消息摘要",
        )))
        .await?;
        let db = self.read()?;
        let result = self
            .summarize(
                &inv.client,
                SumJob {
                    chat_id: inv.message.chat_id.clone(),
                    count,
                    time_range: 0,
                    provider,
                    prompt: String::new(),
                    spoiler: db.ai_config.default_spoiler,
                    before_id: inv.message.id,
                },
            )
            .await?;
        let peer = inv.client.input_peer(&inv.message.peer)?;
        if !db.ai_config.reply_mode {
            if let Some(rich) = result.rich(&inv.client) {
                match inv.edit_rich(rich, &result.html()).await {
                    Ok(_) => return Ok(()),
                    Err(e) if rich_rejected(&e) => {
                        self.a.logs.record(
                            Level::Warn,
                            "sum.rich_rejected",
                            &[("error", &e.to_string())],
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            return inv
                .respond_pages(&html_pages(&result.html(), mibot_command::PAGE_LIMIT))
                .await
                .map_err(Into::into);
        }
        let opts = SendOptions {
            reply_to: Some(inv.message.reply_to_id).filter(|&id| id > 0),
            no_webpage: !db.ai_config.link_preview,
            ..Default::default()
        };
        send_summary(&self.a, &inv.client, &peer, &result, opts).await?;
        if let Err(e) = inv.delete().await {
            self.a.logs.record(
                Level::Warn,
                "sum.delete_command_failed",
                &[("error", &e.to_string())],
            );
        }
        Ok(())
    }

    async fn debug(&self, inv: &Invocation) -> CmdResult {
        let mut count = 50i32;
        if let Ok(v) = inv.arg(1).parse::<i32>()
            && v > 0
        {
            count = v.clamp(1, 500);
        }
        inv.edit_text(&kit::working(kit::tr("Reading messages", "正在读取消息")))
            .await?;
        let (rows, _) = self
            .read_messages(&inv.client, &inv.message.chat_id, inv.message.id, count, 0)
            .await?;
        if rows.is_empty() {
            return Err(fail(kit::tr("No messages found", "未找到消息")));
        }
        let mut preview = sum_format_messages(&rows);
        let char_count = preview.chars().count();
        if char_count > 2000 {
            let tail: String = preview.chars().skip(char_count - 2000).collect();
            let omitted = kit::tr("…(earlier messages omitted)…", "…（前面省略）…");
            preview = format!("{omitted}\n\n{tail}");
        }
        let header = kit::tr(
            "📋 Text preview sent to AI (last 2000 chars):",
            "📋 发送给 AI 的文本预览（最后2000字符）：",
        );
        inv.respond_pages(&html_pages(
            &format!("{header}\n\n{}", code(&preview)),
            mibot_command::PAGE_LIMIT,
        ))
        .await
        .map_err(Into::into)
    }

    async fn config(&self, inv: &Invocation) -> CmdResult {
        let mut action = inv.arg(1).to_ascii_lowercase();
        if action.is_empty() {
            action = "list".into();
        }
        let name = inv.arg(2).to_owned();
        let prop = inv.arg(3).to_owned();
        let rest = if inv.args.len() > 4 {
            &inv.args[4..]
        } else {
            &[]
        };
        let db = self.read()?;
        match action.as_str() {
            "list" | "ls" => self.config_list(inv, &db).await,
            "add" => {
                self.config_add(inv, &name, &prop, rest)?;
                inv.edit_text(kit::tr("✅ Summary config updated", "✅ 摘要配置已更新"))
                    .await
                    .map_err(Into::into)
            }
            "del" | "rm" => self.config_delete(inv, &name).await,
            "set" => self.config_set(inv, &db, &name, &prop, rest).await,
            _ => Err(fail(kit::tr("Unknown config subcommand", "未知 config 子命令"))),
        }
    }

    async fn config_list(&self, inv: &Invocation, db: &SumDb) -> CmdResult {
        let zh = kit::is_zh();
        let mut names: Vec<&String> = db.ai_config.providers.keys().collect();
        names.sort();
        let mut rows = Vec::with_capacity(names.len());
        for key in names {
            let marker = if db.ai_config.default_provider == *key {
                kit::tr(" (default)", "（默认）")
            } else {
                ""
            };
            let prov = &db.ai_config.providers[key];
            let kind = if prov.r#type.is_empty() {
                "auto"
            } else {
                &prov.r#type
            };
            rows.push(format!(
                "• {}{marker} · {} · {} · {}",
                code(key),
                escape(&prov.name),
                code(&prov.model),
                escape(kind)
            ));
        }
        let mut body = if rows.is_empty() {
            kit::tr("• No AI configured", "• 尚未配置 AI").to_owned()
        } else {
            rows.join("\n")
        };
        if let Some(svc) = &self.models
            && (db.ai_config.providers.is_empty() || db.ai_config.ai_migrated)
        {
            let (tag, model) = svc.chat_selection();
            body.push_str(&format!(
                "\n• {}{}",
                kit::tr("ai chat model: ", "ai 聊天模型："),
                code(&format!("{} / {}", kit::or_dash(&tag), kit::or_dash(&model)))
            ));
        }
        let default_p = sum_default_prompt();
        let prompt_state = if db.ai_config.default_prompt.is_empty()
            || db.ai_config.default_prompt == default_p
        {
            kit::tr("built-in", "内置")
        } else {
            kit::tr("custom", "自定义")
        };
        let limit = if db.ai_config.max_output_length > 0 {
            if zh {
                format!("{} 字符", db.ai_config.max_output_length)
            } else {
                format!("{} chars", db.ai_config.max_output_length)
            }
        } else {
            kit::tr("unlimited", "不限").to_owned()
        };
        let target = if db.default_push_target.is_empty() {
            "me"
        } else {
            &db.default_push_target
        };
        let html = if zh {
            format!(
                "🧾 <b>摘要 AI 配置</b>\n{body}\n\n提示词：{prompt_state}\n链接预览：{}\n默认推送：{}\n超时：{} 秒 · 输出上限：{limit} · 回复模式：{}",
                kit::on_off_text(db.ai_config.link_preview),
                code(target),
                sum_timeout(db).as_secs(),
                kit::on_off_text(db.ai_config.reply_mode)
            )
        } else {
            format!(
                "🧾 <b>Summary AI Config</b>\n{body}\n\nPrompt: {prompt_state}\nLink preview: {}\nDefault push: {}\nTimeout: {} s · Max output: {limit} · Reply mode: {}",
                kit::on_off_text(db.ai_config.link_preview),
                code(target),
                sum_timeout(db).as_secs(),
                kit::on_off_text(db.ai_config.reply_mode)
            )
        };
        inv.edit(&html).await.map_err(Into::into)
    }

    fn config_add(
        &self,
        inv: &Invocation,
        name: &str,
        base: &str,
        rest: &[String],
    ) -> Result<(), CmdError> {
        if !inv.message.saved {
            return Err(fail(kit::tr(
                "Configs involving API keys can only be run in Saved Messages",
                "涉及 API Key 的配置命令只能在收藏夹使用",
            )));
        }
        let key = rest.first().map(String::as_str).unwrap_or("");
        let model = rest.get(1).map(String::as_str).unwrap_or("");
        let kind = rest
            .get(2)
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| "auto".into());
        if name.is_empty() || base.is_empty() || key.is_empty() || model.is_empty() {
            return Err(kit::usage(
                inv.prefix(),
                kit::tr(
                    "sum config add name BaseURL API_KEY model [type]",
                    "sum config add 名称 BaseURL API Key 模型 [类型]",
                ),
            ));
        }
        if provider_endpoint(base, "openai", "models").is_err() {
            return Err(fail(kit::tr("Invalid BaseURL", "BaseURL 无效")));
        }
        assert_allowed_model(model)?;
        if !SUM_TYPES.contains(&kind.as_str()) {
            return Err(fail(kit::tr("Invalid API type", "无效接口类型")));
        }
        self.update(|db| {
            db.ai_config.providers.insert(
                name.to_owned(),
                SumProvider {
                    name: name.to_owned(),
                    base_url: base.to_owned(),
                    api_key: key.to_owned(),
                    model: model.to_owned(),
                    r#type: kind,
                },
            );
            if db.ai_config.default_provider.is_empty() {
                db.ai_config.default_provider = name.to_owned();
            }
            Ok(())
        })
    }

    async fn config_delete(&self, inv: &Invocation, name: &str) -> CmdResult {
        let mut reset = Vec::new();
        self.update(|db| {
            if db.ai_config.providers.remove(name).is_none() {
                return Err(fail(kit::tr("AI config does not exist", "AI 配置不存在")));
            }
            if db.ai_config.default_provider == name {
                db.ai_config.default_provider.clear();
            }
            for task in &mut db.tasks {
                if task.ai_provider == name {
                    task.ai_provider.clear();
                    reset.push(task.id.clone());
                }
            }
            Ok(())
        })?;
        if reset.is_empty() {
            inv.edit_text(kit::tr("✅ Summary config updated", "✅ 摘要配置已更新"))
                .await
                .map_err(Into::into)
        } else {
            let msg = if kit::is_zh() {
                format!(
                    "✅ 摘要配置已更新，任务 {} 改用全局默认配置",
                    reset.join("、")
                )
            } else {
                format!(
                    "✅ Summary config updated; task(s) {} now use the global default",
                    reset.join(", ")
                )
            };
            inv.edit_text(&msg).await.map_err(Into::into)
        }
    }

    async fn config_set(
        &self,
        inv: &Invocation,
        db: &SumDb,
        name: &str,
        prop: &str,
        rest: &[String],
    ) -> CmdResult {
        match name {
            "default" => {
                self.update(|d| {
                    if !d.ai_config.providers.contains_key(prop) {
                        return Err(fail(kit::tr("AI config does not exist", "AI 配置不存在")));
                    }
                    d.ai_config.default_provider = prop.to_owned();
                    Ok(())
                })?;
            }
            "preview" | "spoiler" | "reply" => {
                let enabled = kit::on_off(prop)?;
                self.update(|d| {
                    match name {
                        "preview" => d.ai_config.link_preview = enabled,
                        "spoiler" => d.ai_config.default_spoiler = enabled,
                        _ => d.ai_config.reply_mode = enabled,
                    }
                    Ok(())
                })?;
            }
            "push" => {
                let mut parts = vec![prop.to_owned()];
                parts.extend_from_slice(rest);
                let target = parts.join(" ").trim().to_owned();
                if target.is_empty() {
                    return Err(fail(kit::tr(
                        "Push target cannot be empty",
                        "推送目标不能为空",
                    )));
                }
                self.update(|d| {
                    d.default_push_target = target;
                    Ok(())
                })?;
            }
            "timeout" => {
                let secs = prop.parse::<i32>().unwrap_or(-1);
                if !(10..=600).contains(&secs) {
                    return Err(fail(kit::tr(
                        "Timeout must be 10–600 seconds",
                        "超时时间须为 10-600 秒",
                    )));
                }
                self.update(|d| {
                    d.ai_config.default_timeout = secs * 1000;
                    Ok(())
                })?;
            }
            "maxoutput" => {
                let len = prop.parse::<i32>().unwrap_or(-1);
                if len < 0 {
                    return Err(fail(kit::tr(
                        "Enter a valid character count (0 for unlimited)",
                        "请输入有效的字符数（0 表示不限制）",
                    )));
                }
                self.update(|d| {
                    d.ai_config.max_output_length = len;
                    Ok(())
                })?;
            }
            "reasoning" | "service" => {
                let ok = if name == "reasoning" {
                    valid_reasoning(prop)
                } else {
                    valid_tier(prop)
                };
                if !ok {
                    return Err(fail(kit::tr("Invalid option", "无效选项")));
                }
                self.update(|d| {
                    if name == "reasoning" {
                        d.ai_config.default_reasoning_effort = prop.to_owned();
                    } else {
                        d.ai_config.default_service_tier = prop.to_owned();
                    }
                    Ok(())
                })?;
            }
            "prompt" => {
                if prop == "show" {
                    let p = if db.ai_config.default_prompt.is_empty() {
                        sum_default_prompt()
                    } else {
                        db.ai_config.default_prompt.clone()
                    };
                    return inv
                        .edit(&format!(
                            "🧾 <b>{}</b>\n\n{}",
                            kit::tr("Current Summary Prompt", "当前摘要提示词"),
                            code(&p)
                        ))
                        .await
                        .map_err(Into::into);
                }
                let prompt = if prop == "reset" {
                    sum_default_prompt()
                } else {
                    let mut parts = vec![prop.to_owned()];
                    parts.extend_from_slice(rest);
                    parts.join(" ").trim().to_owned()
                };
                if prompt.is_empty() {
                    return Err(fail(kit::tr("Prompt cannot be empty", "提示词不能为空")));
                }
                self.update(|d| {
                    d.ai_config.default_prompt = prompt;
                    Ok(())
                })?;
            }
            _ => {
                let val = rest.join(" ").trim().to_owned();
                if !matches!(prop, "model" | "url" | "key" | "type") || val.is_empty() {
                    return Err(kit::usage(
                        inv.prefix(),
                        kit::tr(
                            "sum config set name model|url|key|type value",
                            "sum config set 名称 model|url|key|type 值",
                        ),
                    ));
                }
                if matches!(prop, "key" | "url") && !inv.message.saved {
                    return Err(fail(kit::tr(
                        "API URL and key can only be changed in Saved Messages",
                        "API 地址和 Key 只能在收藏夹中修改",
                    )));
                }
                self.update(|d| {
                    let Some(prov) = d.ai_config.providers.get_mut(name) else {
                        return Err(fail(kit::tr("AI config does not exist", "AI 配置不存在")));
                    };
                    match prop {
                        "model" => {
                            assert_allowed_model(&val)?;
                            prov.model = val;
                        }
                        "url" => {
                            if provider_endpoint(&val, "openai", "models").is_err() {
                                return Err(fail(kit::tr("Invalid BaseURL", "BaseURL 无效")));
                            }
                            prov.base_url = val;
                        }
                        "key" => prov.api_key = val,
                        _ => {
                            if !SUM_TYPES.contains(&val.as_str()) {
                                return Err(fail(kit::tr("Invalid API type", "无效接口类型")));
                            }
                            prov.r#type = val;
                        }
                    }
                    Ok(())
                })?;
            }
        }
        inv.edit_text(kit::tr("✅ Summary config updated", "✅ 摘要配置已更新"))
            .await
            .map_err(Into::into)
    }

    async fn read_messages(
        &self,
        client: &Client,
        chat_id: &str,
        before_id: i32,
        count: i32,
        time_range: i32,
    ) -> Result<(Vec<SumMessage>, String), CmdError> {
        let input = client
            .resolve_target(chat_id)
            .await
            .map_err(|_| {
                fail(kit::tr(
                    "Cannot locate group; send a message there first or use here",
                    "无法定位群组，请先在该群发过消息或用 here",
                ))
            })?;
        let peer = peer_of_input(&input, client.self_id());
        let (mut title, username) = chat_title(client, peer.as_ref());
        if title.is_empty() {
            title = chat_id.to_owned();
        }
        let cutoff = if time_range > 0 {
            Utc::now().timestamp() as i32 - time_range * 3600
        } else {
            0
        };
        let mut rows = Vec::new();
        let mut offset = before_id;
        let mut scanned = 0i32;
        let limit = (count * 3).min(800);
        let mut done = false;

        while !done && (rows.len() as i32) < count && scanned < limit {
            let batch = (limit - scanned).min(100);
            let res = client
                .invoke(&tl::functions::messages::GetHistory {
                    peer: input.clone(),
                    offset_id: offset,
                    offset_date: 0,
                    add_offset: 0,
                    limit: batch,
                    max_id: 0,
                    min_id: 0,
                    hash: 0,
                })
                .await?;
            let msgs = kit::unpack_messages(res);
            if msgs.is_empty() {
                break;
            }
            let msgs_len = msgs.len() as i32;
            for item in msgs {
                scanned += 1;
                let id = kit::message_id(&item);
                if id > 0 && (offset == 0 || id < offset) {
                    offset = id;
                }
                let tl::enums::Message::Message(msg) = item else {
                    continue;
                };
                if before_id > 0 && msg.id >= before_id {
                    continue;
                }
                if cutoff > 0 && msg.date < cutoff {
                    done = true;
                    break;
                }
                let content = mibot_bot::message_text(Some(&msg)).trim().to_owned();
                let file_name = sum_file_name(msg.media.as_ref());
                if content.is_empty() && file_name.is_empty() {
                    continue;
                }
                let sender = if let Some(from) = &msg.from_id {
                    client.peers().title(from)
                } else if msg.post {
                    title.clone()
                } else {
                    kit::tr("Unknown sender", "未知发送者").to_owned()
                };
                rows.push(SumMessage {
                    text: sum_message_text(msg.date, &sender, &content, &file_name),
                    content,
                    link: sum_message_link(peer.as_ref(), msg.id, &username),
                    urls: sum_entity_urls(&msg),
                    file_name,
                });
                if (rows.len() as i32) >= count {
                    break;
                }
            }
            if msgs_len < batch {
                break;
            }
        }
        rows.reverse();
        Ok((rows, title))
    }

    async fn summarize(&self, client: &Client, job: SumJob) -> Result<SumResult, CmdError> {
        let db = self.read()?;
        let backend = resolve_backend(&db, &job.provider, self.models.as_ref())?;
        let (rows, title) = self
            .read_messages(client, &job.chat_id, job.before_id, job.count, job.time_range)
            .await?;
        if rows.is_empty() {
            return Err(fail(kit::tr("No messages to summarize", "没有可以摘要的消息")));
        }
        let prompt = if !job.prompt.is_empty() {
            job.prompt
        } else if !db.ai_config.default_prompt.is_empty() {
            db.ai_config.default_prompt.clone()
        } else {
            sum_default_prompt()
        };
        let mut output = backend.call(&db, &sum_format_messages(&rows), &prompt).await?;
        output = THINK_RE.replace_all(&output, "").trim().to_owned();
        let limit = db.ai_config.max_output_length;
        if limit > 0 {
            let truncated = kit::truncate_runes(&output, limit as usize);
            if truncated != output {
                output = format!(
                    "{truncated}\n\n{}",
                    kit::tr(
                        "Content truncated to configured length.",
                        "内容已按配置长度截断。"
                    )
                );
            }
        }
        Ok(SumResult {
            title,
            at: Local::now(),
            output,
            spoiler: job.spoiler,
        })
    }

    async fn push(&self, client: &Client, task: &SumTask) -> CmdResult {
        let result = self
            .summarize(
                client,
                SumJob {
                    chat_id: task.chat_id.clone(),
                    count: task.message_count,
                    time_range: task.time_range,
                    provider: task.ai_provider.clone(),
                    prompt: task.ai_prompt.clone(),
                    spoiler: task.use_spoiler,
                    before_id: 0,
                },
            )
            .await?;
        let db = self.read()?;
        let target = push_target(&db, task);
        let peer = client
            .resolve_target(target)
            .await
            .map_err(|_| {
                fail(if kit::is_zh() {
                    format!("无法定位推送目标：{target}")
                } else {
                    format!("Cannot locate push target: {target}")
                })
            })?;
        send_summary(
            &self.a,
            client,
            &peer,
            &result,
            SendOptions {
                no_webpage: !db.ai_config.link_preview,
                ..Default::default()
            },
        )
        .await
    }

    async fn resolve_chat(
        &self,
        inv: &Invocation,
        target: &str,
    ) -> Result<(String, String), CmdError> {
        let (normalized, invite) = sum_target_kind(target);
        if invite {
            let invite_res = inv
                .client
                .invoke(&tl::functions::messages::CheckChatInvite {
                    hash: normalized.clone(),
                })
                .await
                .map_err(|e| {
                    fail(if kit::is_zh() {
                        format!("无法处理邀请链接：{}", brief(&e))
                    } else {
                        format!("Cannot process invite link: {}", brief(&e))
                    })
                })?;
            let chat = match invite_res {
                tl::enums::ChatInvite::Already(a) => {
                    inv.client.peers().remember_chats(std::slice::from_ref(&a.chat));
                    a.chat
                }
                tl::enums::ChatInvite::Invite(_) | tl::enums::ChatInvite::Peek(_) => {
                    let join_res = inv
                        .client
                        .invoke(&tl::functions::messages::ImportChatInvite {
                            hash: normalized,
                        })
                        .await
                        .map_err(|e| {
                            if e.is_rpc("INVITE_REQUEST_SENT") {
                                fail(kit::tr(
                                    "Join request submitted; add the task after approval",
                                    "已提交入群申请，通过后再添加任务",
                                ))
                            } else {
                                fail(if kit::is_zh() {
                                    format!("加入群组失败：{}", brief(&e))
                                } else {
                                    format!("Failed to join group: {}", brief(&e))
                                })
                            }
                        })?;
                    let chats = match join_res {
                        tl::enums::messages::ChatInviteJoinResult::Ok(j) => match j.updates {
                            tl::enums::Updates::Updates(u) => u.chats,
                            tl::enums::Updates::Combined(u) => u.chats,
                            _ => Vec::new(),
                        },
                        _ => Vec::new(),
                    };
                    if chats.is_empty() {
                        return Err(fail(kit::tr(
                            "Joined group, but no group info was returned",
                            "加入群组后未拿到群组信息",
                        )));
                    }
                    inv.client.peers().remember_chats(&chats);
                    chats.into_iter().next().expect("non-empty")
                }
            };
            let (mut chat_id, title, username) = chat_id_of(&chat);
            if chat_id.is_empty() {
                return Err(fail(kit::tr(
                    "Invite link does not point to a group",
                    "邀请链接指向的不是群组",
                )));
            }
            if !username.is_empty() {
                chat_id = format!("@{username}");
            }
            return Ok((chat_id, sum_display(&title, &username)));
        }

        let chat_id = if normalized == "here" {
            inv.message.chat_id.clone()
        } else {
            normalized
        };
        let input = inv
            .client
            .resolve_target(&chat_id)
            .await
            .map_err(|e| {
                fail(if kit::is_zh() {
                    format!("无法获取群组信息：{}", brief(&e))
                } else {
                    format!("Failed to get group info: {}", brief(&e))
                })
            })?;
        let peer = peer_of_input(&input, inv.client.self_id());
        let (title, username) = chat_title(&inv.client, peer.as_ref());
        Ok((chat_id, sum_display(&title, &username)))
    }

    async fn add(self: &Arc<Self>, inv: &Invocation) -> CmdResult {
        let args = sum_add_arguments(inv);
        let (target, interval, rest) = sum_add_syntax(&args);
        let opts = sum_parse_add_options(&rest)?;
        if target.is_empty() || interval.is_empty() || !(10..=500).contains(&opts.count) {
            return Err(kit::usage(
                inv.prefix(),
                kit::tr("sum add here|group 2h 100", "sum add here|群组 2h 100"),
            ));
        }
        let spec = sum_interval_cron(&interval)?;
        let db = self.read()?;
        if !opts.provider.is_empty() && !self.provider_known(&db, &opts.provider) {
            return Err(fail(if kit::is_zh() {
                format!("未找到 AI 配置：{}", opts.provider)
            } else {
                format!("AI config not found: {}", opts.provider)
            }));
        }
        let (chat_id, display) = self.resolve_chat(inv, &target).await?;
        let spoiler = opts.spoiler.unwrap_or(db.ai_config.default_spoiler);

        let mut created_task = SumTask::default();
        self.update(|d| {
            let last = d.seq.parse::<i64>().unwrap_or(0);
            let id = (last + 1).to_string();
            created_task = SumTask {
                id: id.clone(),
                cron: spec.clone(),
                chat_id: chat_id.clone(),
                chat_display: display.clone(),
                interval: interval.clone(),
                message_count: opts.count,
                time_range: opts.time_range,
                push_target: d.default_push_target.clone(),
                ai_provider: opts.provider.clone(),
                ai_prompt: String::new(),
                use_spoiler: spoiler,
                created_at: Utc::now().to_rfc3339(),
                remark: opts.remark.clone(),
                ..Default::default()
            };
            d.seq = id;
            d.tasks.push(created_task.clone());
            Ok(())
        })?;

        if let Err(e) = self.schedule_task(&created_task) {
            let tid = created_task.id.clone();
            if let Err(re) = self.update(|d| {
                d.tasks.retain(|t| t.id != tid);
                Ok(())
            }) {
                kit::warn(&self.a, "sum.rollback_task_failed", re.as_ref());
            }
            return Err(e);
        }

        let zh = kit::is_zh();
        let group_display = escape(if display.is_empty() { &chat_id } else { &display });
        let mut lines = if zh {
            vec![
                format!("✅ 已创建摘要任务 {}", code(&created_task.id)),
                format!("群组：{group_display}"),
                format!("间隔：{}", code(&interval)),
            ]
        } else {
            vec![
                format!("✅ Created summary task {}", code(&created_task.id)),
                format!("Group: {group_display}"),
                format!("Interval: {}", code(&interval)),
            ]
        };
        if created_task.time_range > 0 {
            lines.push(if zh {
                format!(
                    "时间范围：过去 {} 小时（最多 {} 条）",
                    created_task.time_range, created_task.message_count
                )
            } else {
                format!(
                    "Time range: past {} hours (max {} messages)",
                    created_task.time_range, created_task.message_count
                )
            });
        } else {
            lines.push(if zh {
                format!("消息数：{}", created_task.message_count)
            } else {
                format!("Messages: {}", created_task.message_count)
            });
        }
        if !created_task.ai_provider.is_empty() {
            lines.push(format!(
                "{}{}",
                kit::tr("AI config: ", "AI 配置："),
                code(&created_task.ai_provider)
            ));
        }
        if created_task.use_spoiler {
            lines.push(kit::tr("Spoiler: yes", "折叠：是").to_owned());
        }
        let db2 = self.read()?;
        lines.push(format!(
            "{}{}",
            kit::tr("Push target: ", "推送："),
            code(push_target(&db2, &created_task))
        ));
        if !created_task.remark.is_empty() {
            lines.push(format!(
                "{}{}",
                kit::tr("Note: ", "备注："),
                escape(&created_task.remark)
            ));
        }
        if let Some(next) = sum_next_run(&spec, Local::now()) {
            lines.push(format!(
                "{}{}",
                kit::tr("Next run: ", "下次执行："),
                sum_format_time(next)
            ));
        }
        inv.edit(&lines.join("\n")).await.map_err(Into::into)
    }

    fn schedule_task(self: &Arc<Self>, task: &SumTask) -> Result<(), CmdError> {
        if task.disabled {
            return Ok(());
        }
        let cron = parse_sum_cron(&task.cron)?;
        let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        if g.tasks.contains_key(&task.id) {
            return Ok(());
        }
        let Some(cancel) = g.cancel.clone() else {
            return Ok(());
        };
        let svc = Arc::clone(self);
        let tid = task.id.clone();
        let tid_map = tid.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok(next) = cron.find_next_occurrence(&Local::now(), false) else {
                    return;
                };
                let delta = (next.timestamp_millis() - Local::now().timestamp_millis()).max(200);
                if !kit::sleep(&cancel, Duration::from_millis(delta as u64)).await {
                    return;
                }
                svc.fire(&tid).await;
            }
        });
        g.tasks.insert(tid_map, handle);
        Ok(())
    }

    fn unschedule_task(&self, id: &str) {
        let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = g.tasks.remove(id) {
            h.abort();
        }
    }

    async fn fire(&self, id: &str) {
        let client = {
            let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
            let Some(c) = g.client.clone() else {
                return;
            };
            if g.running.contains(id) {
                return;
            }
            g.running.insert(id.to_owned());
            c
        };
        struct ClearRunning<'a> {
            svc: &'a SumService,
            id: &'a str,
        }
        impl Drop for ClearRunning<'_> {
            fn drop(&mut self) {
                let mut g = self.svc.mu.lock().unwrap_or_else(|e| e.into_inner());
                g.running.remove(self.id);
            }
        }
        let _clear = ClearRunning { svc: self, id };

        let db = match self.read() {
            Ok(d) => d,
            Err(e) => {
                self.a.logs.record(
                    Level::Error,
                    "sum.scheduled_failed",
                    &[("task", id), ("error", &e.to_string())],
                );
                return;
            }
        };
        let Some(task) = db.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        if task.disabled {
            return;
        }
        let Some(_work_guard): Option<WorkGuard> = self.a.registry.track_work() else {
            self.a.logs.record(
                Level::Info,
                "sum.scheduled_skipped",
                &[("task", id), ("reason", "maintenance or shutting down")],
            );
            return;
        };
        let target = push_target(&db, &task).to_owned();
        let res = tokio::time::timeout(Duration::from_secs(600), self.push(&client, &task)).await;
        let run_err = match res {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some(fail(kit::tr(
                "Scheduled summary timed out",
                "定时摘要执行超时",
            ))),
        };
        let err_msg = run_err.as_ref().map(|e| brief(e.as_ref()));
        if let Err(e) = self.update(|d| {
            for t in &mut d.tasks {
                if t.id == id {
                    t.last_run_at = Utc::now().to_rfc3339();
                    if let Some(m) = &err_msg {
                        t.last_error = m.clone();
                    } else {
                        t.last_result = if kit::is_zh() {
                            format!("摘要完成，已推送到 {target}")
                        } else {
                            format!("Summary sent to {target}")
                        };
                        t.last_error.clear();
                    }
                }
            }
            Ok(())
        }) {
            kit::warn(&self.a, "sum.record_run_failed", e.as_ref());
        }
        if let Some(e) = run_err {
            self.a.logs.record(
                Level::Error,
                "sum.scheduled_failed",
                &[("task", id), ("error", &e.to_string())],
            );
        }
    }

    async fn list(&self, inv: &Invocation) -> CmdResult {
        let zh = kit::is_zh();
        let title = kit::tr("Summary Tasks", "摘要任务");
        let db = self.read()?;
        if db.tasks.is_empty() {
            return inv
                .edit(&format!(
                    "🧾 <b>{title}</b>\n{}",
                    kit::tr("No scheduled tasks", "还没有定时任务")
                ))
                .await
                .map_err(Into::into);
        }
        let mut tasks = db.tasks.clone();
        tasks.sort_by_key(|t| t.id.parse::<i64>().unwrap_or(i64::MAX));
        let now = Local::now();
        let mut blocks = vec![format!("🧾 <b>{title}</b>")];
        for task in &tasks {
            let heading = if !task.remark.is_empty() {
                &task.remark
            } else if !task.chat_display.is_empty() {
                &task.chat_display
            } else {
                &task.chat_id
            };
            let chat_name = if task.chat_display.is_empty() {
                &task.chat_id
            } else {
                &task.chat_display
            };
            let mut lines = vec![
                format!("{} • {}", code(&task.id), escape(heading)),
                format!("{}{}", kit::tr("Group: ", "群组："), escape(chat_name)),
                format!("{}{}", kit::tr("Interval: ", "间隔："), code(&task.interval)),
            ];
            if task.time_range > 0 {
                lines.push(if zh {
                    format!("时间范围：过去 {} 小时", task.time_range)
                } else {
                    format!("Time range: past {} hours", task.time_range)
                });
            } else {
                lines.push(format!(
                    "{}{}",
                    kit::tr("Messages: ", "消息数："),
                    task.message_count
                ));
            }
            if !task.ai_provider.is_empty() {
                lines.push(format!(
                    "{}{}",
                    kit::tr("AI config: ", "AI 配置："),
                    code(&task.ai_provider)
                ));
            } else if !db.ai_config.default_provider.is_empty() {
                lines.push(if zh {
                    format!("AI 配置：默认（{}）", escape(&db.ai_config.default_provider))
                } else {
                    format!("AI config: default ({})", escape(&db.ai_config.default_provider))
                });
            } else {
                lines.push(
                    kit::tr(
                        "AI config: default (ai chat model)",
                        "AI 配置：默认（ai 聊天模型）",
                    )
                    .to_owned(),
                );
            }
            if !task.ai_prompt.is_empty() {
                lines.push(format!(
                    "{}{}",
                    kit::tr("Prompt: ", "提示词："),
                    escape(&kit::truncate_runes(&task.ai_prompt, 30))
                ));
            }
            if task.use_spoiler {
                lines.push(kit::tr("Spoiler: yes", "折叠：是").to_owned());
            }
            lines.push(format!(
                "{}{}",
                kit::tr("Push target: ", "推送："),
                code(push_target(&db, task))
            ));
            if task.disabled {
                lines.push(kit::tr("Status: ⏹ disabled", "状态：⏹ 已停用").to_owned());
            } else if let Some(next) = sum_next_run(&task.cron, now) {
                lines.push(format!(
                    "{}{}",
                    kit::tr("Next: ", "下次："),
                    sum_format_time(next)
                ));
            } else {
                lines.push(
                    kit::tr("Status: invalid cron expression", "状态：定时表达式无效").to_owned(),
                );
            }
            if !task.last_run_at.is_empty()
                && let Some(last) = sum_parse_time(&task.last_run_at)
            {
                lines.push(format!(
                    "{}{}",
                    kit::tr("Last: ", "上次："),
                    sum_format_time(last)
                ));
            }
            if !task.last_result.is_empty() {
                lines.push(format!(
                    "{}{}",
                    kit::tr("Result: ", "结果："),
                    escape(&task.last_result)
                ));
            }
            if !task.last_error.is_empty() {
                lines.push(format!(
                    "{}{}",
                    kit::tr("Error: ", "错误："),
                    escape(&task.last_error)
                ));
            }
            blocks.push(lines.join("\n"));
        }
        inv.respond_pages(&html_pages(
            &blocks.join("\n\n"),
            mibot_command::PAGE_LIMIT,
        ))
        .await
        .map_err(Into::into)
    }

    async fn run_now(&self, inv: &Invocation) -> CmdResult {
        let db = self.read()?;
        let Some(task) = db.tasks.iter().find(|t| t.id == inv.arg(1)).cloned() else {
            return Err(fail(kit::tr("Summary task does not exist", "摘要任务不存在")));
        };
        {
            let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
            if g.running.contains(&task.id) {
                return Err(fail(kit::tr(
                    "Task is already running; try again later",
                    "该任务正在执行，稍后再试",
                )));
            }
            g.running.insert(task.id.clone());
        }
        struct Clear<'a>(&'a SumService, &'a str);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                let mut g = self.0.mu.lock().unwrap_or_else(|e| e.into_inner());
                g.running.remove(self.1);
            }
        }
        let _guard = Clear(self, &task.id);
        inv.edit_text(&kit::working(kit::tr("Generating summary", "正在生成摘要")))
            .await?;
        self.push(&inv.client, &task).await?;
        let target = push_target(&db, &task);
        let done_msg = if kit::is_zh() {
            format!("✅ 摘要已推送到 {target}")
        } else {
            format!("✅ Summary pushed to {target}")
        };
        inv.edit_text(&done_msg).await.map_err(Into::into)
    }

    async fn toggle(self: &Arc<Self>, inv: &Invocation, sub: &str) -> CmdResult {
        let id = inv.arg(1).to_owned();
        let mut updated_task = None;
        self.update(|d| {
            let Some(pos) = d.tasks.iter().position(|t| t.id == id) else {
                return Err(fail(kit::tr("Summary task does not exist", "摘要任务不存在")));
            };
            if sub == "del" || sub == "rm" {
                d.tasks.remove(pos);
            } else {
                let disable = sub == "disable";
                if !disable {
                    parse_sum_cron(&d.tasks[pos].cron)?;
                }
                d.tasks[pos].disabled = disable;
                updated_task = Some(d.tasks[pos].clone());
            }
            Ok(())
        })?;
        if sub == "enable" {
            if let Some(t) = updated_task {
                self.schedule_task(&t)?;
            }
        } else {
            self.unschedule_task(&id);
        }
        inv.edit_text(kit::tr("✅ Summary task updated", "✅ 摘要任务已更新"))
            .await
            .map_err(Into::into)
    }

    async fn edit_task(&self, inv: &Invocation) -> CmdResult {
        let id = inv.arg(1);
        let prop = inv.arg(2).to_ascii_lowercase();
        let value = inv.raw_after(3).trim();
        if id.is_empty() || prop.is_empty() {
            return Err(kit::usage(
                inv.prefix(),
                kit::tr(
                    "sum edit task_id spoiler on|off | provider [name] | prompt [text]",
                    "sum edit 任务 ID spoiler on|off、provider [名称] 或 prompt [内容]",
                ),
            ));
        }
        let db = self.read()?;
        let missing = || fail(kit::tr("Summary task does not exist", "摘要任务不存在"));
        if !db.tasks.iter().any(|t| t.id == id) {
            return Err(missing());
        }
        let zh = kit::is_zh();
        let message = match prop.as_str() {
            "spoiler" => {
                let enabled = match value.to_ascii_lowercase().as_str() {
                    "" => {
                        return Err(fail(kit::tr(
                            "Please provide a value: on or off",
                            "请提供值：on 或 off",
                        )));
                    }
                    "on" | "true" | "1" => true,
                    "off" | "false" | "0" => false,
                    _ => {
                        return Err(fail(kit::tr(
                            "Invalid value; use on or off",
                            "无效的值，请使用 on 或 off",
                        )));
                    }
                };
                self.update(|d| {
                    let t = d.tasks.iter_mut().find(|t| t.id == id).ok_or_else(missing)?;
                    t.use_spoiler = enabled;
                    Ok(())
                })?;
                if zh {
                    if enabled {
                        format!("已启用任务 {id} 的折叠显示")
                    } else {
                        format!("已关闭任务 {id} 的折叠显示")
                    }
                } else if enabled {
                    format!("Enabled spoiler for task {id}")
                } else {
                    format!("Disabled spoiler for task {id}")
                }
            }
            "provider" => {
                if !value.is_empty() && !self.provider_known(&db, value) {
                    return Err(fail(if zh {
                        format!("未找到 AI 配置：{value}")
                    } else {
                        format!("AI config not found: {value}")
                    }));
                }
                self.update(|d| {
                    let t = d.tasks.iter_mut().find(|t| t.id == id).ok_or_else(missing)?;
                    t.ai_provider = value.to_owned();
                    Ok(())
                })?;
                if value.is_empty() {
                    if zh {
                        format!("已清空任务 {id} 的 AI 配置，将使用全局默认配置")
                    } else {
                        format!("Cleared AI config for task {id}; will use global default")
                    }
                } else if zh {
                    format!("已设置任务 {id} 的 AI 配置为 {value}")
                } else {
                    format!("Set AI config for task {id} to {value}")
                }
            }
            "prompt" => {
                self.update(|d| {
                    let t = d.tasks.iter_mut().find(|t| t.id == id).ok_or_else(missing)?;
                    t.ai_prompt = value.to_owned();
                    Ok(())
                })?;
                if value.is_empty() {
                    if zh {
                        format!("已清空任务 {id} 的提示词，将使用全局默认提示词")
                    } else {
                        format!("Cleared prompt for task {id}; will use global default")
                    }
                } else if zh {
                    format!("已设置任务 {id} 的提示词")
                } else {
                    format!("Updated prompt for task {id}")
                }
            }
            _ => {
                return Err(fail(if zh {
                    format!("未知属性：{prop}，支持 spoiler、provider、prompt")
                } else {
                    format!("Unknown property: {prop}; supported: spoiler, provider, prompt")
                }));
            }
        };
        inv.edit_text(&format!("✅ {message}"))
            .await
            .map_err(Into::into)
    }

    async fn reorder(self: &Arc<Self>, inv: &Invocation) -> CmdResult {
        let mut mapping = Vec::new();
        let mut previous = Vec::new();
        let mut tasks = Vec::new();
        self.update(|d| {
            if d.tasks.is_empty() {
                return Err(fail(kit::tr("No tasks to reorder", "暂无任务需要重排序")));
            }
            for (idx, t) in d.tasks.iter_mut().enumerate() {
                let next = (idx + 1).to_string();
                previous.push(t.id.clone());
                mapping.push(format!("{} → {next}", escape(&t.id)));
                t.id = next;
            }
            d.seq = d.tasks.len().to_string();
            tasks = d.tasks.clone();
            Ok(())
        })?;
        for old_id in previous {
            self.unschedule_task(&old_id);
        }
        for t in &tasks {
            if let Err(e) = self.schedule_task(t) {
                self.a.logs.record(
                    Level::Warn,
                    "sum.register_failed",
                    &[("error", &e.to_string())],
                );
            }
        }
        let header = if kit::is_zh() {
            format!("✅ 已重新排序 {} 个任务", tasks.len())
        } else {
            format!("✅ Reordered {} task(s)", tasks.len())
        };
        inv.edit(&format!("{header}\n\n{}", mapping.join(", ")))
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDirGuard(std::path::PathBuf);

    impl TempDirGuard {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("mibot-sum-test-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_sum_interval_cron() {
        assert_eq!(sum_interval_cron("30m").expect("30m"), "*/30 * * * *");
        assert_eq!(sum_interval_cron("2h").expect("2h"), "0 */2 * * *");
        assert_eq!(sum_interval_cron("1d").expect("1d"), "0 0 * * *");
        assert_eq!(
            sum_interval_cron("0 */30 * * * *").expect("6-field"),
            "0 */30 * * * *"
        );
        assert!(sum_interval_cron("7m").is_err());
        assert!(sum_interval_cron("5h").is_err());
        assert!(sum_interval_cron("2d").is_err());
    }

    #[test]
    fn test_sum_add_syntax() {
        let a = vec!["here".into(), "2h".into(), "100".into(), "note".into()];
        let (t, i, r) = sum_add_syntax(&a);
        assert_eq!(t, "here");
        assert_eq!(i, "2h");
        assert_eq!(r, vec!["100", "note"]);

        let b = vec![
            "@group".into(),
            "0".into(),
            "*/30".into(),
            "*".into(),
            "*".into(),
            "*".into(),
            "1-5".into(),
            "--time".into(),
            "12".into(),
        ];
        let (t, i, r) = sum_add_syntax(&b);
        assert_eq!(t, "@group");
        assert_eq!(i, "0 */30 * * * 1-5");
        assert_eq!(r, vec!["--time", "12"]);
    }

    #[test]
    fn test_sum_target_kind_and_protocol() {
        assert_eq!(sum_target_kind("here"), ("here".into(), false));
        assert_eq!(sum_target_kind("t.me/+AbC_1"), ("AbC_1".into(), true));
        assert_eq!(
            sum_target_kind("https://t.me/c/12345/67"),
            ("-10012345".into(), false)
        );
        assert_eq!(
            sum_target_kind("https://t.me/rustlang/1"),
            ("@rustlang".into(), false)
        );

        let p = SumProvider {
            model: "gemini-2.5-flash".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            ..Default::default()
        };
        assert_eq!(sum_detect_protocol(&p), ("gemini".into(), true));

        let p2 = SumProvider {
            model: "gpt-5".into(),
            base_url: "https://api.example.com".into(),
            ..Default::default()
        };
        assert_eq!(sum_detect_protocol(&p2), ("responses".into(), true));
    }

    #[test]
    fn test_sum_normalize_and_resolve_backend() {
        let mut db = SumDb {
            seq: String::new(),
            ai_config: SumAiConfig {
                default_prompt: SUM_LEGACY_PROMPT.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        db.normalize();
        assert_eq!(db.seq, "0");
        assert_eq!(db.ai_config.default_prompt, sum_default_prompt());

        let err = resolve_backend(&db, "", None).err().expect("no backend");
        assert!(err.to_string().contains("sum config add"));
    }

    #[test]
    fn test_sum_endpoint_rewrite() {
        let out = sum_provider_endpoint("https://api.example.com", "gemini", "models/m:generateContent")
            .expect("endpoint");
        assert_eq!(out, "https://api.example.com/v1beta/models/m:generateContent");
        let out2 = sum_provider_endpoint("https://api.example.com/v1beta", "openai", "chat/completions")
            .expect("endpoint2");
        assert_eq!(out2, "https://api.example.com/v1/chat/completions");
    }

    #[test]
    fn test_sum_dir_cleanup_smoke() {
        let g = TempDirGuard::new();
        assert!(g.0.exists());
    }
}
