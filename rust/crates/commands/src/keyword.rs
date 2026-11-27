use crate::kit;
use fancy_regex::RegexBuilder;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BoxFuture, Client, Message, SendOptions, peer_id, utf16_len};
use mibot_command::{CmdResult, Command, Invocation, PAGE_LIMIT, code, escape, group, html_pages};
use mibot_logtail::Level;
use mibot_store::Store;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const MAX_PATTERN_LENGTH: i32 = 512;
const MAX_INPUT_LENGTH: i32 = 4096;
const MAX_DELAY_SECS: f64 = 366.0 * 24.0 * 3600.0;
const APPLY_TIMEOUT: Duration = Duration::from_secs(120);
const SLOTS: usize = 4;

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Task {
    id: i32,
    #[serde(rename = "chatId")]
    chat_id: String,
    key: String,
    response: String,
    include: bool,
    regexp: bool,
    exact: bool,
    #[serde(rename = "caseSensitive")]
    case_sensitive: bool,
    #[serde(rename = "ignoreForward")]
    ignore_forward: bool,
    reply: bool,
    #[serde(rename = "deleteSource")]
    delete_source: bool,
    #[serde(rename = "banSeconds")]
    ban_seconds: f64,
    #[serde(rename = "restrictSeconds")]
    restrict_seconds: f64,
    #[serde(rename = "deleteReplyAfter")]
    delete_reply_after: f64,
    #[serde(rename = "deleteSourceAfter")]
    delete_source_after: f64,
}

#[derive(Debug, Clone, Serialize)]
struct Document {
    #[serde(rename = "schemaVersion")]
    schema_version: i32,
    #[serde(rename = "nextId")]
    next_id: i32,
    tasks: Vec<Task>,
    aliases: BTreeMap<String, String>,
    #[serde(rename = "importedLegacy")]
    imported_legacy: bool,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            schema_version: 1,
            next_id: 1,
            tasks: Vec::new(),
            aliases: BTreeMap::new(),
            imported_legacy: false,
        }
    }
}

impl<'de> Deserialize<'de> for Document {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Loose {
            #[serde(default, rename = "schemaVersion")]
            schema_version: i32,
            #[serde(default, rename = "nextId")]
            next_id: Value,
            #[serde(default)]
            tasks: Vec<Value>,
            #[serde(default)]
            aliases: BTreeMap<String, Value>,
            #[serde(default, rename = "importedLegacy")]
            imported_legacy: bool,
        }
        let loose = Loose::deserialize(deserializer)?;
        let mut doc = Document {
            imported_legacy: loose.imported_legacy,
            ..Default::default()
        };
        if loose.schema_version > 0 {
            doc.schema_version = loose.schema_version;
        }
        if let Some(next) = js_number(&loose.next_id)
            && next >= 1.0
            && next == next.trunc()
            && next < i32::MAX as f64
        {
            doc.next_id = next as i32;
        }
        for raw in loose.tasks {
            if let Value::Object(map) = raw
                && let Some(t) = normalize_task(&map)
            {
                doc.tasks.push(t);
            }
        }
        for (from, to) in loose.aliases {
            if let Some(target) = js_string(&to)
                && !target.is_empty()
            {
                doc.aliases.insert(from, target);
            }
        }
        Ok(doc)
    }
}

fn first_non_null<'a>(map: &'a serde_json::Map<String, Value>, names: &[&str]) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|name| map.get(*name).filter(|v| !v.is_null()))
}

fn is_true(map: &serde_json::Map<String, Value>, names: &[&str]) -> bool {
    names
        .iter()
        .any(|n| matches!(map.get(*n), Some(Value::Bool(true))))
}

fn not_false(map: &serde_json::Map<String, Value>, name: &str) -> bool {
    !matches!(map.get(name), Some(Value::Bool(false)))
}

fn js_number(val: &Value) -> Option<f64> {
    match val {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Some(0.0)
            } else {
                trimmed.parse::<f64>().ok().filter(|n| !n.is_nan())
            }
        }
        _ => None,
    }
}

fn js_string(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn field_seconds(map: &serde_json::Map<String, Value>, names: &[&str]) -> f64 {
    let Some(raw) = first_non_null(map, names) else {
        return 0.0;
    };
    let Some(v) = js_number(raw) else {
        return 0.0;
    };
    if v < 0.0 || v.is_infinite() {
        0.0
    } else {
        v
    }
}

fn normalize_task(map: &serde_json::Map<String, Value>) -> Option<Task> {
    let id_val = first_non_null(map, &["id", "task_id"])?;
    let id = js_number(id_val)?;
    if id < 1.0 || id != id.trunc() || id > i32::MAX as f64 {
        return None;
    }
    let chat_val = first_non_null(map, &["chatId", "cid"])?;
    let chat_id = js_string(chat_val)?;
    let key = match map.get("key") {
        Some(Value::String(s)) => s.clone(),
        _ => return None,
    };
    let response = match first_non_null(map, &["response", "msg"]) {
        Some(Value::String(s)) => s.clone(),
        _ => return None,
    };
    Some(Task {
        id: id as i32,
        chat_id,
        key,
        response,
        include: not_false(map, "include"),
        regexp: is_true(map, &["regexp"]),
        exact: is_true(map, &["exact"]),
        case_sensitive: is_true(map, &["caseSensitive", "case"]),
        ignore_forward: is_true(map, &["ignoreForward", "ignore_forward"]),
        reply: not_false(map, "reply"),
        delete_source: is_true(map, &["deleteSource", "delete"]),
        ban_seconds: field_seconds(map, &["banSeconds", "ban"]),
        restrict_seconds: field_seconds(map, &["restrictSeconds", "restrict"]),
        delete_reply_after: field_seconds(map, &["deleteReplyAfter", "delay_delete"]),
        delete_source_after: field_seconds(map, &["deleteSourceAfter", "source_delay_delete"]),
    })
}

fn compile_pattern(
    pattern: &str,
    case_sensitive: bool,
) -> Result<fancy_regex::Regex, fancy_regex::Error> {
    let pat = if case_sensitive {
        pattern.to_owned()
    } else {
        format!("(?i){pattern}")
    };
    RegexBuilder::new(&pat).backtrack_limit(100_000).build()
}

fn duration_from_secs(seconds: f64) -> Duration {
    if seconds <= 0.0 || seconds.is_nan() {
        Duration::ZERO
    } else if seconds >= MAX_DELAY_SECS {
        Duration::from_secs_f64(MAX_DELAY_SECS)
    } else {
        Duration::from_secs_f64(seconds)
    }
}

fn format_seconds(seconds: f64) -> String {
    if seconds == seconds.trunc() && seconds.abs() < i64::MAX as f64 {
        (seconds as i64).to_string()
    } else {
        seconds.to_string()
    }
}

fn sections(text: &str) -> Vec<String> {
    let norm = text.replace("\r\n", "\n");
    let mut parts = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in norm.split('\n') {
        if line.trim() == "+++" {
            parts.push(cur.join("\n"));
            cur.clear();
        } else {
            cur.push(line);
        }
    }
    parts.push(cur.join("\n"));
    parts
}

fn has_separator(text: &str) -> bool {
    sections(text).len() > 1
}

fn valid_moderation_seconds(seconds: f64) -> bool {
    (30.0..=MAX_DELAY_SECS).contains(&seconds)
}

fn action_seconds(action: &str, name: &str) -> Result<f64, mibot_command::CmdError> {
    let digits = action.strip_prefix(name).unwrap_or("");
    let Ok(seconds) = digits.parse::<i64>() else {
        return Err(kit::fail(format!(
            "{name} 后面要紧跟秒数，如 {name}3600"
        )));
    };
    if !kit::is_digits(digits) || seconds <= 0 {
        return Err(kit::fail(format!(
            "{name} 后面要紧跟秒数，如 {name}3600"
        )));
    }
    if !valid_moderation_seconds(seconds as f64) {
        return Err(kit::fail("封禁或禁言时长要在 30 秒到 366 天之间"));
    }
    Ok(seconds as f64)
}

fn parse_task(text: &str) -> Result<Task, mibot_command::CmdError> {
    let parts = sections(text);
    if parts.len() < 2 {
        return Err(kit::fail("任务格式不对：关键词和回复之间要有单独一行 +++"));
    }
    if parts.len() > 6 {
        return Err(kit::fail(
            "任务最多 6 段：关键词、回复、匹配选项、动作和两个删除秒数",
        ));
    }
    let key = parts[0].trim().to_owned();
    let response = parts[1].trim().to_owned();
    if key.is_empty() {
        return Err(kit::fail("关键词不能为空"));
    }
    if response.is_empty() {
        return Err(kit::fail("回复内容不能为空"));
    }
    let mut t = Task {
        id: 0,
        chat_id: String::new(),
        key,
        response,
        include: true,
        regexp: false,
        exact: false,
        case_sensitive: false,
        ignore_forward: false,
        reply: true,
        delete_source: false,
        ban_seconds: 0.0,
        restrict_seconds: 0.0,
        delete_reply_after: 0.0,
        delete_source_after: 0.0,
    };
    let part = |idx: usize| -> &str { parts.get(idx).map(|s| s.trim()).unwrap_or("") };
    for option in part(2).split_whitespace() {
        match option.to_ascii_lowercase().as_str() {
            "include" => t.include = true,
            "exact" => {
                t.include = false;
                t.exact = true;
            }
            "regexp" => t.regexp = true,
            "case" => t.case_sensitive = true,
            "ignore_forward" => t.ignore_forward = true,
            _ => {
                return Err(kit::fail(format!(
                    "不认识的匹配选项 {option}，只能是 include、exact、regexp、case、ignore_forward"
                )));
            }
        }
    }
    for action in part(3).split_whitespace() {
        let lower = action.to_ascii_lowercase();
        if lower == "reply" {
            t.reply = true;
        } else if lower == "delete" {
            t.delete_source = true;
        } else if lower.starts_with("restrict") {
            t.restrict_seconds = action_seconds(&lower, "restrict")?;
        } else if lower.starts_with("ban") {
            t.ban_seconds = action_seconds(&lower, "ban")?;
        } else {
            return Err(kit::fail(format!(
                "不认识的动作 {action}，只能是 reply、delete、ban秒数、restrict秒数"
            )));
        }
    }
    for (idx, field) in [
        (4usize, &mut t.delete_reply_after),
        (5usize, &mut t.delete_source_after),
    ] {
        let val = part(idx);
        if val.is_empty() {
            continue;
        }
        let Ok(seconds) = val.parse::<f64>() else {
            return Err(kit::fail(format!(
                "第 {} 段要写不小于 0 的秒数，现在是 {val}",
                idx + 1
            )));
        };
        if seconds < 0.0 || seconds.is_nan() || seconds.is_infinite() {
            return Err(kit::fail(format!(
                "第 {} 段要写不小于 0 的秒数，现在是 {val}",
                idx + 1
            )));
        }
        *field = seconds;
    }
    if t.regexp {
        if utf16_len(&t.key) > MAX_PATTERN_LENGTH {
            return Err(kit::fail(format!(
                "正则表达式太长，最多 {MAX_PATTERN_LENGTH} 个字符"
            )));
        }
        if compile_pattern(&t.key, t.case_sensitive).is_err() {
            return Err(kit::fail("正则表达式无效"));
        }
    }
    Ok(t)
}

#[derive(Clone)]
struct Compiled {
    task: Task,
    pattern: Option<fancy_regex::Regex>,
}

struct View {
    chats: HashMap<String, Vec<Compiled>>,
    aliases: BTreeMap<String, String>,
}

impl View {
    fn tasks_for(&self, chat_id: &str) -> Vec<Compiled> {
        let own = self.chats.get(chat_id).cloned().unwrap_or_default();
        let Some(inherited) = self.aliases.get(chat_id) else {
            return own;
        };
        if inherited.is_empty() || inherited == chat_id {
            return own;
        }
        let from = self.chats.get(inherited).cloned().unwrap_or_default();
        if from.is_empty() {
            return own;
        }
        let mut out = Vec::with_capacity(from.len() + own.len());
        out.extend(from);
        out.extend(own);
        out
    }

    fn build(doc: &Document, app: &App) -> Self {
        let mut chats: HashMap<String, Vec<Compiled>> = HashMap::new();
        for t in &doc.tasks {
            let pattern = if t.regexp {
                match compile_pattern(&t.key, t.case_sensitive) {
                    Ok(re) => Some(re),
                    Err(err) => {
                        app.logs.record(
                            Level::Warn,
                            "keyword.regexp_invalid",
                            &[("task", &t.id.to_string()), ("error", &err.to_string())],
                        );
                        None
                    }
                }
            } else {
                None
            };
            chats
                .entry(t.chat_id.clone())
                .or_default()
                .push(Compiled {
                    task: t.clone(),
                    pattern,
                });
        }
        Self {
            chats,
            aliases: doc.aliases.clone(),
        }
    }
}

fn matches_task(
    t: &Task,
    pattern: Option<&fancy_regex::Regex>,
    text: &str,
    forwarded: bool,
) -> Result<bool, fancy_regex::Error> {
    if text.is_empty() || (t.ignore_forward && forwarded) {
        return Ok(false);
    }
    if t.regexp {
        let Some(re) = pattern else {
            return Ok(false);
        };
        if utf16_len(text) > MAX_INPUT_LENGTH {
            return Ok(false);
        }
        return re.is_match(text);
    }
    if t.case_sensitive {
        if t.exact {
            return Ok(text == t.key);
        }
        return Ok(t.include && text.contains(&t.key));
    }
    let lower_text = text.to_lowercase();
    let lower_key = t.key.to_lowercase();
    if t.exact {
        Ok(lower_text == lower_key)
    } else {
        Ok(t.include && lower_text.contains(&lower_key))
    }
}

#[derive(Default)]
struct SenderInfo {
    id: String,
    name: String,
    user: bool,
}

fn render(t: &Task, from: &SenderInfo) -> String {
    let mention = if !from.id.is_empty() {
        let escaped = escape(&from.name);
        if from.user {
            format!(r#"<a href="tg://user?id={}">{escaped}</a>"#, from.id)
        } else {
            escaped
        }
    } else {
        String::new()
    };
    let delay = if t.delete_reply_after > 0.0 {
        format_seconds(t.delete_reply_after)
    } else {
        String::new()
    };
    let id_esc = escape(&from.id);
    let name_esc = escape(&from.name);

    let mut out = String::with_capacity(t.response.len());
    let mut rest = t.response.as_str();
    while !rest.is_empty() {
        if rest.starts_with("$mention") {
            out.push_str(&mention);
            rest = &rest["$mention".len()..];
        } else if rest.starts_with("$code_id") {
            out.push_str(&id_esc);
            rest = &rest["$code_id".len()..];
        } else if rest.starts_with("$code_name") {
            out.push_str(&name_esc);
            rest = &rest["$code_name".len()..];
        } else if rest.starts_with("$delay_delete") {
            out.push_str(&delay);
            rest = &rest["$delay_delete".len()..];
        } else if let Some(ch) = rest.chars().next() {
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

fn summary(t: &Task) -> String {
    let mut items = Vec::new();
    if t.regexp {
        items.push("正则".to_owned());
    } else if t.exact {
        items.push("整条一致".to_owned());
    } else if !t.include {
        items.push("不匹配（旧数据）".to_owned());
    }
    if t.case_sensitive {
        items.push("区分大小写".to_owned());
    }
    if t.ignore_forward {
        items.push("忽略转发".to_owned());
    }
    if !t.reply {
        items.push("不引用原消息".to_owned());
    }
    if t.delete_source {
        if t.delete_source_after > 0.0 {
            items.push(format!(
                "{} 秒后删除原消息",
                format_seconds(t.delete_source_after)
            ));
        } else {
            items.push("删除原消息".to_owned());
        }
    }
    if t.ban_seconds > 0.0 {
        items.push(format!("封禁 {} 秒", format_seconds(t.ban_seconds)));
    } else if t.restrict_seconds > 0.0 {
        items.push(format!("禁言 {} 秒", format_seconds(t.restrict_seconds)));
    }
    if t.delete_reply_after > 0.0 {
        items.push(format!(
            "回复 {} 秒后删除",
            format_seconds(t.delete_reply_after)
        ));
    }
    items.join(" · ")
}

type AfterHook = Arc<dyn Fn(Duration, CancellationToken, Box<dyn FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send>) + Send + Sync>;

struct Service {
    app: Arc<App>,
    store: Store<Document>,
    write_mu: Mutex<()>,
    cached: RwLock<Option<Arc<View>>>,
    slots: Arc<Semaphore>,
    warned: Mutex<HashMap<String, Instant>>,
    after_hook: Option<AfterHook>,
}

impl Service {
    fn new(app: Arc<App>) -> Arc<Self> {
        Arc::new(Self {
            store: kit::new_store(&app, "keyword.json", Document::default),
            app,
            write_mu: Mutex::new(()),
            cached: RwLock::new(None),
            slots: Arc::new(Semaphore::new(SLOTS)),
            warned: Mutex::new(HashMap::new()),
            after_hook: None,
        })
    }

    fn current(&self) -> Arc<View> {
        if let Some(v) = self.cached.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return v;
        }
        let _guard = self.write_mu.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = self.cached.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return v;
        }
        let doc = match self.store.read() {
            Ok(d) => d,
            Err(err) => {
                self.app.logs.record(
                    Level::Error,
                    "keyword.load_failed",
                    &[("error", &err.to_string())],
                );
                Document::default()
            }
        };
        let v = Arc::new(View::build(&doc, &self.app));
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&v));
        v
    }

    fn update<F>(&self, mutate: F) -> Result<(), mibot_store::StoreError>
    where
        F: FnOnce(&mut Document) -> Result<(), mibot_store::StoreError>,
    {
        let _guard = self.write_mu.lock().unwrap_or_else(|e| e.into_inner());
        let doc = kit::update_read(&self.store, mutate)?;
        let v = Arc::new(View::build(&doc, &self.app));
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(v);
        Ok(())
    }

    fn hits_of(&self, msg: &Message, tasks: &[Compiled]) -> Vec<Task> {
        let text = msg.content();
        let mut hits = Vec::new();
        for entry in tasks {
            match matches_task(&entry.task, entry.pattern.as_ref(), &text, msg.forward) {
                Ok(true) => hits.push(entry.task.clone()),
                Ok(false) => {}
                Err(err) => {
                    self.app.logs.record(
                        Level::Warn,
                        "keyword.regexp_failed",
                        &[
                            ("task", &entry.task.id.to_string()),
                            ("error", &err.to_string()),
                        ],
                    );
                }
            }
        }
        hits
    }

    fn offer(self: &Arc<Self>, msg: &Message, cancel: &CancellationToken) -> bool {
        if msg.out || msg.content().is_empty() {
            return false;
        }
        let tasks = self.current().tasks_for(&msg.chat_id);
        if tasks.is_empty() {
            return false;
        }
        let hits = self.hits_of(msg, &tasks);
        if hits.is_empty() {
            return false;
        }
        let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
            self.warn_dropped(&msg.chat_id);
            return false;
        };
        let s = Arc::clone(self);
        let msg_cloned = msg.clone();
        let admitted = self.app.registry.background(cancel.clone(), move |task_cancel| {
            Box::pin(async move {
                let _permit = permit;
                for t in hits {
                    if task_cancel.is_cancelled() {
                        break;
                    }
                    let _ = tokio::time::timeout(
                        APPLY_TIMEOUT,
                        s.apply(&msg_cloned, &t, &task_cancel),
                    )
                    .await;
                }
            })
        });
        if !admitted {
            // permit drops automatically if scheduler rejected
        }
        false
    }

    fn warn_dropped(&self, chat_id: &str) {
        let now = Instant::now();
        let mut g = self.warned.lock().unwrap_or_else(|e| e.into_inner());
        if g.get(chat_id)
            .is_some_and(|at| now.saturating_duration_since(*at) < Duration::from_secs(60))
        {
            return;
        }
        g.retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(60));
        g.insert(chat_id.to_owned(), now);
        drop(g);
        self.app.logs.record(
            Level::Warn,
            "keyword.dropped_busy",
            &[("chat", chat_id), ("slots", &SLOTS.to_string())],
        );
    }

    async fn apply(self: &Arc<Self>, msg: &Message, t: &Task, cancel: &CancellationToken) {
        let client = &self.app.client;
        let Ok(peer) = client.input_peer(&msg.peer) else {
            self.app.logs.record(
                Level::Warn,
                "keyword.unaddressable",
                &[("chat", &msg.chat_id), ("task", &t.id.to_string())],
            );
            return;
        };
        let mut sent = 0i32;
        let text = render(t, &sender_of(client, msg));
        if !text.trim().is_empty() {
            let opts = SendOptions {
                topic_id: Some(msg.topic_id).filter(|&v| v > 0),
                reply_to: if t.reply { Some(msg.id) } else { None },
                no_webpage: false,
                entities: None,
            };
            match client.send_html(&peer, &text, opts).await {
                Ok(u) => sent = mibot_bot::first_sent_message_id(&u).unwrap_or(0),
                Err(err) => {
                    self.app.logs.record(
                        Level::Warn,
                        "keyword.reply_failed",
                        &[("chat", &msg.chat_id), ("error", &err.to_string())],
                    );
                }
            }
        }
        if let Err(err) = self.moderate(client, &peer, msg, t).await {
            self.app.logs.record(
                Level::Warn,
                "keyword.moderate_failed",
                &[("chat", &msg.chat_id), ("error", &err.to_string())],
            );
        }
        if t.delete_source {
            self.delete_after(
                peer.clone(),
                msg.id,
                duration_from_secs(t.delete_source_after),
                cancel,
            )
            .await;
        }
        if t.delete_reply_after > 0.0 && sent > 0 {
            self.delete_after(
                peer,
                sent,
                duration_from_secs(t.delete_reply_after),
                cancel,
            )
            .await;
        }
        self.app.logs.record(
            Level::Info,
            "keyword.triggered",
            &[("chat", &msg.chat_id), ("task", &t.id.to_string())],
        );
    }

    async fn delete_after(
        self: &Arc<Self>,
        peer: tl::enums::InputPeer,
        msg_id: i32,
        delay: Duration,
        cancel: &CancellationToken,
    ) {
        let app = Arc::clone(&self.app);
        let run = move |c: CancellationToken| -> BoxFuture<'static, ()> {
            Box::pin(async move {
                let ids = [msg_id];
                let res = tokio::select! {
                    _ = c.cancelled() => return,
                    r = tokio::time::timeout(
                        Duration::from_secs(30),
                        app.client.delete_messages(&peer, &ids),
                    ) => r,
                };
                if let Ok(Err(err)) = res
                    && !err.is_rpc("MESSAGE_ID_INVALID")
                {
                    app.logs.record(
                        Level::Warn,
                        "keyword.delete_failed",
                        &[("message", &msg_id.to_string()), ("error", &err.to_string())],
                    );
                }
            })
        };
        if delay.is_zero() {
            run(cancel.clone()).await;
            return;
        }
        if let Some(hook) = &self.after_hook {
            hook(delay, cancel.clone(), Box::new(run));
        } else {
            self.app.registry.after(cancel.clone(), delay, run);
        }
    }

    async fn moderate(
        &self,
        client: &Client,
        chat: &tl::enums::InputPeer,
        msg: &Message,
        t: &Task,
    ) -> Result<(), String> {
        let (seconds, ban) = if t.ban_seconds > 0.0 {
            (t.ban_seconds, true)
        } else {
            (t.restrict_seconds, false)
        };
        if seconds <= 0.0 || peer_id(&msg.sender) == msg.chat_id {
            return Ok(());
        }
        if !valid_moderation_seconds(seconds) {
            return Err("封禁或禁言时长超出 30 秒到 366 天，Telegram 会当成永久".into());
        }
        if msg.sender_is_user(client.self_id()) {
            return Ok(());
        }
        let target = client.input_peer(&msg.sender).map_err(|e| e.to_string())?;
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let until = (now_unix + duration_from_secs(seconds).as_secs() as i64) as i32;
        let rights = if ban {
            kit::ban_rights(until)
        } else {
            kit::mute_rights(until)
        };
        if let Some(ch) = kit::input_channel(chat) {
            client
                .invoke(&tl::functions::channels::EditBanned {
                    channel: ch.into(),
                    participant: target,
                    banned_rights: rights.into(),
                })
                .await
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
        if let tl::enums::InputPeer::Chat(g) = chat
            && ban
            && let Some(user) = kit::input_user(&target)
        {
            client
                .invoke(&tl::functions::messages::DeleteChatUser {
                    revoke_history: false,
                    chat_id: g.chat_id,
                    user_id: user,
                })
                .await
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
        self.app.logs.record(
            Level::Info,
            "keyword.moderate_unsupported",
            &[("chat", &msg.chat_id)],
        );
        Ok(())
    }

    async fn command(&self, inv: Invocation) -> CmdResult {
        let body = inv.raw.trim_start();
        if has_separator(body) {
            return self.add(&inv, body).await;
        }
        match inv.arg(0).to_ascii_lowercase().as_str() {
            "" | "h" | "help" => {
                let pages = html_pages(&help(inv.prefix()), PAGE_LIMIT);
                inv.respond_pages(&pages).await?;
                Ok(())
            }
            "list" => self.list(&inv, inv.arg(1).eq_ignore_ascii_case("all")).await,
            "rm" => self.remove(&inv).await,
            "alias" => self.alias(&inv).await,
            _ => self.add(&inv, body).await,
        }
    }

    async fn add(&self, inv: &Invocation, body: &str) -> CmdResult {
        let mut parsed = match parse_task(body) {
            Ok(t) => t,
            Err(err) => {
                if !has_separator(body) && mibot_command::is_user_error(err.as_ref()) {
                    return Err(kit::fail(format!(
                        "{err}，{}keyword help 看格式",
                        inv.prefix()
                    )));
                }
                return Err(err);
            }
        };
        parsed.chat_id = inv.message.chat_id.clone();
        let mut new_id = 0i32;
        self.update(|doc| {
            let mut id = doc.next_id.max(1);
            for existing in &doc.tasks {
                id = id.max(existing.id + 1);
            }
            parsed.id = id;
            new_id = id;
            doc.next_id = id + 1;
            doc.tasks.push(parsed.clone());
            Ok(())
        })?;
        inv.edit(&format!("✅ 已添加关键词任务 {}", code(&new_id.to_string())))
            .await?;
        Ok(())
    }

    async fn list(&self, inv: &Invocation, all: bool) -> CmdResult {
        self.store.read()?;
        let v = self.current();
        let here = &inv.message.chat_id;
        if !all {
            let own = v.chats.get(here);
            let mut lines = Vec::new();
            match own {
                None => lines.push("当前对话没有关键词任务".to_owned()),
                Some(entries) if entries.is_empty() => lines.push("当前对话没有关键词任务".to_owned()),
                Some(entries) => {
                    lines.push(format!("💬 <b>当前对话的关键词任务</b>（{} 个）", entries.len()));
                    for entry in entries {
                        lines.push(format_line(entry));
                    }
                }
            }
            if let Some(from) = v.aliases.get(here)
                && !from.is_empty()
                && from != here
            {
                let count = v.chats.get(from).map(Vec::len).unwrap_or(0);
                lines.push(String::new());
                lines.push(format!(
                    "另外继承 {}（{}）的 {count} 个任务",
                    code(from),
                    escape(&chat_title(&inv.client, from)),
                ));
            }
            let pages = html_pages(&lines.join("\n"), PAGE_LIMIT);
            inv.respond_pages(&pages).await?;
            return Ok(());
        }

        let mut chats: Vec<(&String, &Vec<Compiled>)> = v
            .chats
            .iter()
            .filter(|(_, entries)| !entries.is_empty())
            .collect();
        let total: usize = chats.iter().map(|(_, entries)| entries.len()).sum();
        if total == 0 {
            inv.edit("还没有任何关键词任务").await?;
            return Ok(());
        }
        chats.sort_by_key(|(_, entries)| entries[0].task.id);
        let mut lines = vec![format!("💬 <b>全部关键词任务</b>（{total} 个）")];
        for (chat, entries) in chats {
            lines.push(String::new());
            lines.push(format!(
                "<b>{}</b> {}",
                escape(&chat_title(&inv.client, chat)),
                code(chat)
            ));
            for entry in entries {
                lines.push(format_line(entry));
            }
        }
        let pages = html_pages(&lines.join("\n"), PAGE_LIMIT);
        inv.respond_pages(&pages).await?;
        Ok(())
    }

    async fn remove(&self, inv: &Invocation) -> CmdResult {
        let ids = parse_ids(&inv.args[1..])?;
        if ids.is_empty() {
            return Err(kit::usage(inv.prefix(), "keyword rm 编号[,编号…]"));
        }
        let id_set: HashSet<i32> = ids.iter().copied().collect();
        let mut removed = HashSet::new();
        self.update(|doc| {
            doc.tasks.retain(|t| {
                if id_set.contains(&t.id) {
                    removed.insert(t.id);
                    false
                } else {
                    true
                }
            });
            Ok(())
        })?;
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !removed.contains(id))
            .map(i32::to_string)
            .collect();
        if removed.is_empty() {
            return Err(kit::fail(format!(
                "没有编号为 {} 的任务，{}keyword list all 可以看全部编号",
                missing.join("、"),
                inv.prefix()
            )));
        }
        let mut text = format!("✅ 已删除 {} 个任务", removed.len());
        if !missing.is_empty() {
            text.push_str(&format!("（没有编号 {}）", escape(&missing.join("、"))));
        }
        inv.edit(&text).await?;
        Ok(())
    }

    async fn alias(&self, inv: &Invocation) -> CmdResult {
        let here = &inv.message.chat_id;
        let target = inv.arg(1);
        if target.is_empty() {
            let current = self.store.read()?;
            match current.aliases.get(here) {
                None => inv.edit("当前对话没有继承别的对话").await?,
                Some(from) => {
                    inv.edit(&format!(
                        "当前对话继承自 {}（{}）",
                        code(from),
                        escape(&chat_title(&inv.client, from))
                    ))
                    .await?
                }
            }
            return Ok(());
        }
        if target.eq_ignore_ascii_case("rm") {
            let mut existed = false;
            self.update(|doc| {
                existed = doc.aliases.remove(here).is_some();
                Ok(())
            })?;
            if !existed {
                return Err(kit::fail("当前对话没有继承别的对话"));
            }
            inv.edit("✅ 已取消继承").await?;
            return Ok(());
        }
        if !kit::is_numeric_id(target) {
            return Err(kit::fail(format!(
                "要写对话 ID，如 -1001234567890，{}ids 可以查",
                inv.prefix()
            )));
        }
        if target == here {
            return Err(kit::fail("不能继承当前对话自己"));
        }
        let mut count = 0usize;
        let here_owned = here.clone();
        let target_owned = target.to_owned();
        self.update(|doc| {
            doc.aliases.insert(here_owned, target_owned.clone());
            count = doc
                .tasks
                .iter()
                .filter(|t| t.chat_id == target_owned)
                .count();
            Ok(())
        })?;
        let mut text = format!(
            "✅ 已继承 {}（{}）的任务",
            code(target),
            escape(&chat_title(&inv.client, target))
        );
        if count == 0 {
            text.push_str("\n⚠️ 那个对话现在还没有任务");
        }
        inv.edit(&text).await?;
        Ok(())
    }
}

fn sender_of(client: &Client, msg: &Message) -> SenderInfo {
    match &msg.sender {
        tl::enums::Peer::User(u) => {
            let name = client
                .peers()
                .user(u.user_id)
                .map(|i| i.first_name.trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "用户".into());
            SenderInfo {
                id: u.user_id.to_string(),
                name,
                user: true,
            }
        }
        tl::enums::Peer::Channel(ch) => SenderInfo {
            id: ch.channel_id.to_string(),
            name: client.peers().title(&msg.sender),
            user: false,
        },
        _ => SenderInfo::default(),
    }
}

fn chat_title(client: &Client, chat_id: &str) -> String {
    mibot_bot::peer_from_id(chat_id)
        .map(|p| client.peers().title(&p))
        .unwrap_or_else(|| chat_id.to_owned())
}

fn format_line(entry: &Compiled) -> String {
    let mut text = format!(
        "{} {} → {}",
        code(&entry.task.id.to_string()),
        code(&entry.task.key),
        escape(&entry.task.response)
    );
    let mut extra = summary(&entry.task);
    if entry.task.regexp && entry.pattern.is_none() {
        if extra.is_empty() {
            extra = "⚠️ 正则无法使用，不会命中".into();
        } else {
            extra.push_str(" · ⚠️ 正则无法使用，不会命中");
        }
    }
    if !extra.is_empty() {
        text.push_str(&format!("\n　{extra}"));
    }
    text
}

fn parse_ids(args: &[String]) -> Result<Vec<i32>, mibot_command::CmdError> {
    let joined = args.join(" ");
    let mut ids = Vec::new();
    for field in joined.split(|c: char| c == ',' || c == '，' || c.is_whitespace()) {
        if field.is_empty() {
            continue;
        }
        let Ok(id) = field.parse::<i32>() else {
            return Err(kit::fail(format!("任务编号要是正整数，{field} 不是")));
        };
        if id < 1 {
            return Err(kit::fail(format!("任务编号要是正整数，{field} 不是")));
        }
        ids.push(id);
    }
    Ok(ids)
}

pub fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let c = |text: &str| format!("<code>{p}keyword{text}</code>");
    format!(
        "💬 <b>关键词自动回复</b>\n\n\
         别人在对话里发的消息命中关键词时，账号自动回复，还可以删掉原消息、封禁或禁言发送者。\
         任务在哪个对话里添加，就只在哪个对话里生效。\n\n\
         <b>管理</b>\n\
         • {} 当前对话的任务\n\
         • {} 所有对话的任务\n\
         • {} 按编号删除任务\n\
         • {} 查看当前对话继承了哪个对话\n\
         • {} 当前对话也用另一个对话的任务（对话 ID 可以用 {p}ids 查）\n\
         • {} 取消继承\n\n\
         <b>添加任务</b>\n\
         各段之间用单独一行 <code>+++</code> 隔开，前两段必填，后面的可以省略或留空：\n\
         <pre>{p}keyword 关键词\n+++\n回复内容\n+++\n匹配选项\n+++\n动作\n+++\n回复几秒后删除\n+++\n原消息几秒后删除</pre>\n\n\
         <b>匹配选项</b>（空格分隔）\n\
         • <code>include</code> 消息里包含关键词就算（默认）\n\
         • <code>exact</code> 整条消息和关键词一致\n\
         • <code>regexp</code> 关键词是正则表达式（JavaScript 语法）\n\
         • <code>case</code> 区分大小写\n\
         • <code>ignore_forward</code> 不理会转发的消息（为兼容 MiBox 保留；目前转发来的消息本来就不触发）\n\n\
         <b>动作</b>（空格分隔）\n\
         • <code>reply</code> 回复时引用原消息（默认）\n\
         • <code>delete</code> 删除原消息，第六段可以设延迟\n\
         • <code>ban3600</code> 封禁发送者 3600 秒\n\
         • <code>restrict600</code> 禁言发送者 600 秒\n\
         少于 30 秒或超过 366 天的，Telegram 按永久处理。\n\n\
         <b>回复里的变量</b>\n\
         • <code>$mention</code> 提及发送者\n\
         • <code>$code_id</code> 发送者 ID\n\
         • <code>$code_name</code> 发送者名字\n\
         • <code>$delay_delete</code> 回复几秒后删除\n\
         回复按 HTML 发送，可以用 &lt;b&gt;、&lt;i&gt;、&lt;code&gt; 这些标签。\n\n\
         <b>示例</b>\n\
         <pre>{p}keyword 你好\n+++\n欢迎，$mention</pre>\n\
         <pre>{p}keyword \\d{{11}}\n+++\n请勿发送手机号码\n+++\nregexp\n+++\ndelete\n+++\n10</pre>\n\
         <pre>{p}keyword 广告\n+++\n检测到广告，已封禁 $code_name\n+++\n\n+++\ndelete ban3600</pre>\n\n\
         <b>注意</b>\n\
         • 删除别人的消息、封禁、禁言都要管理员权限\n\
         • 只看别人发的消息，自己发的、转发来的都不会触发\n\
         • 继承只看一层：当前对话先查继承来的任务，再查自己的\n\
         • 任务编号在所有对话里唯一，删掉的编号不再使用",
        c(" list"),
        c(" list all"),
        c(" rm 1,2,3"),
        c(" alias"),
        c(" alias -1001234567890"),
        c(" alias rm"),
    )
}

pub fn register(app: &Arc<App>) {
    let s = Service::new(Arc::clone(app));
    let s_offer = Arc::clone(&s);
    app.on_foreign(Arc::new(move |msg, _app, cancel| {
        s_offer.offer(msg, cancel)
    }));
    let s_cmd = Arc::clone(&s);
    app.registry.register(Command {
        name: "keyword".into(),
        group: group::ADMIN.into(),
        description: "设置关键词自动回复".into(),
        usage: "[任务|list|rm|alias]".into(),
        help: help("."),
        help_fn: Some(Arc::new(help)),
        free_text: true,
        handle: Arc::new(move |inv| {
            let s = Arc::clone(&s_cmd);
            Box::pin(async move { s.command(inv).await })
        }),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::InvocationError;
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{PeerCache, RpcTransport, SentRecorder};
    use mibot_logtail::Ring;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct FakeTelegram {
        calls: Mutex<Vec<String>>,
        fail_send: bool,
    }

    impl FakeTelegram {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    impl RpcTransport for FakeTelegram {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                if constructor_id == tl::functions::messages::SendMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::SendMessage::from_bytes(&body[4..]).unwrap();
                    let (mut reply, mut topic) = (0, 0);
                    if let Some(tl::enums::InputReplyTo::Message(r)) = req.reply_to {
                        reply = r.reply_to_msg_id;
                        topic = r.top_msg_id.unwrap_or(0);
                    }
                    let mut mention = String::new();
                    if let Some(ents) = req.entities {
                        for ent in ents {
                            if let tl::enums::MessageEntity::InputMessageEntityMentionName(m) = ent
                                && let tl::enums::InputUser::User(u) = m.user_id
                            {
                                mention = format!(" mention={}", u.user_id);
                            }
                        }
                    }
                    self.calls.lock().unwrap().push(format!(
                        "send {:?} reply={reply} topic={topic}{mention}",
                        req.message
                    ));
                    if self.fail_send {
                        return Err(InvocationError::Rpc(grammers_mtsender::RpcError {
                            code: 403,
                            name: "CHAT_WRITE_FORBIDDEN".into(),
                            value: None,
                            caused_by: None,
                        }));
                    }
                    let resp =
                        tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                            out: true,
                            id: 900,
                            pts: 1,
                            pts_count: 1,
                            date: 1,
                            media: None,
                            entities: None,
                            ttl_period: None,
                        });
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::channels::DeleteMessages::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::DeleteMessages::from_bytes(&body[4..]).unwrap();
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("delete {:?}", req.id));
                    let resp: tl::enums::messages::AffectedMessages =
                        tl::types::messages::AffectedMessages {
                            pts: 1,
                            pts_count: 1,
                        }
                        .into();
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::channels::EditBanned::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::EditBanned::from_bytes(&body[4..]).unwrap();
                    let tl::enums::InputPeer::User(u) = req.participant else {
                        unreachable!()
                    };
                    let tl::enums::ChatBannedRights::Rights(r) = req.banned_rights;
                    self.calls.lock().unwrap().push(format!(
                        "edit-banned user={} view={} send={} media={}",
                        u.user_id, r.view_messages, r.send_messages, r.send_media
                    ));
                    let resp = tl::enums::Updates::Updates(tl::types::Updates {
                        updates: vec![],
                        users: vec![],
                        chats: vec![],
                        date: 0,
                        seq: 0,
                    });
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::messages::EditMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::EditMessage::from_bytes(&body[4..]).unwrap();
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("edit {} {:?}", req.id, req.message.unwrap_or_default()));
                    let resp = tl::enums::Updates::Updates(tl::types::Updates {
                        updates: vec![],
                        users: vec![],
                        chats: vec![],
                        date: 0,
                        seq: 0,
                    });
                    return Ok(resp.to_bytes());
                }
                Err(InvocationError::Dropped("unexpected rpc"))
            })
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-keyword-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_parse_task() {
        let full = parse_task(
            "  \\d{11} \n+++\n请勿发送手机号码\n第二行\n+++ \nregexp CASE\n+++\ndelete ban3600\n+++\n10\n+++\n2.5",
        )
        .unwrap();
        assert_eq!(full.key, r"\d{11}");
        assert_eq!(full.response, "请勿发送手机号码\n第二行");
        assert!(full.include && full.regexp && full.case_sensitive && full.reply && full.delete_source);
        assert_eq!(full.ban_seconds, 3600.0);
        assert_eq!(full.delete_reply_after, 10.0);
        assert_eq!(full.delete_source_after, 2.5);

        let exact = parse_task("违规\n+++\n注意\n+++\nexact").unwrap();
        assert!(!exact.include && exact.exact);

        for (text, want) in [
            ("只有关键词", "关键词和回复之间要有单独一行 +++"),
            ("\n+++\n回复", "关键词不能为空"),
            ("词\n+++\n ", "回复内容不能为空"),
            ("词\n+++\n回\n+++\nfuzzy", "不认识的匹配选项 fuzzy"),
            ("词\n+++\n回\n+++\n\n+++\nkick", "不认识的动作 kick"),
            ("词\n+++\n回\n+++\n\n+++\nban", "ban 后面要紧跟秒数"),
            ("词\n+++\n回\n+++\n\n+++\nban0", "ban 后面要紧跟秒数"),
            ("词\n+++\n回\n+++\n\n+++\nban15", "封禁或禁言时长要在 30 秒到 366 天之间"),
            ("词\n+++\n回\n+++\n\n+++\nrestrict31622401", "封禁或禁言时长要在 30 秒到 366 天之间"),
            ("[\n+++\n回\n+++\nregexp", "正则表达式无效"),
        ] {
            let err = parse_task(text).unwrap_err();
            assert!(mibot_command::is_user_error(err.as_ref()));
            let msg = err.to_string();
            assert!(msg.contains(want), "{text}: {msg}");
        }
    }

    #[test]
    fn test_matches_js_regexp() {
        let re = compile_pattern(r"\d{11}", false).unwrap();
        assert!(
            matches_task(
                &Task {
                    id: 1,
                    chat_id: "1".into(),
                    key: r"\d{11}".into(),
                    response: "r".into(),
                    include: true,
                    regexp: true,
                    exact: false,
                    case_sensitive: false,
                    ignore_forward: false,
                    reply: true,
                    delete_source: false,
                    ban_seconds: 0.0,
                    restrict_seconds: 0.0,
                    delete_reply_after: 0.0,
                    delete_source_after: 0.0,
                },
                Some(&re),
                "电话 13800138000",
                false
            )
            .unwrap()
        );
        let lookahead = compile_pattern(r"^(?!.*白名单).*广告", false).unwrap();
        let t_la = Task {
            id: 2,
            chat_id: "1".into(),
            key: r"^(?!.*白名单).*广告".into(),
            response: "r".into(),
            include: false,
            regexp: true,
            exact: false,
            case_sensitive: false,
            ignore_forward: false,
            reply: true,
            delete_source: false,
            ban_seconds: 0.0,
            restrict_seconds: 0.0,
            delete_reply_after: 0.0,
            delete_source_after: 0.0,
        };
        assert!(!matches_task(&t_la, Some(&lookahead), "白名单里的广告", false).unwrap());
        assert!(matches_task(&t_la, Some(&lookahead), "卖广告", false).unwrap());
    }

    #[test]
    fn test_render_and_mibox_compat() {
        let t = Task {
            id: 1,
            chat_id: "-1".into(),
            key: "hi".into(),
            response: "$mention 你好 $mention，ID $code_id，名字 $code_name，$delay_delete 秒后删除".into(),
            include: true,
            regexp: false,
            exact: false,
            case_sensitive: false,
            ignore_forward: false,
            reply: true,
            delete_source: false,
            ban_seconds: 0.0,
            restrict_seconds: 0.0,
            delete_reply_after: 10.0,
            delete_source_after: 0.0,
        };
        let got = render(
            &t,
            &SenderInfo {
                id: "42".into(),
                name: "<Cat>".into(),
                user: true,
            },
        );
        assert_eq!(
            got,
            r#"<a href="tg://user?id=42">&lt;Cat&gt;</a> 你好 <a href="tg://user?id=42">&lt;Cat&gt;</a>，ID 42，名字 &lt;Cat&gt;，10 秒后删除"#
        );

        let raw = r#"{
          "schemaVersion": 1, "nextId": 4, "importedLegacy": true,
          "aliases": {"-1002": "-1001", "-1003": -1001},
          "tasks": [
            {"id": 1, "chatId": "-1001", "key": "你好", "response": "欢迎", "include": true, "regexp": false, "exact": false,
             "caseSensitive": false, "ignoreForward": false, "reply": true, "deleteSource": true, "banSeconds": 0,
             "restrictSeconds": 600, "deleteReplyAfter": 1.5, "deleteSourceAfter": 0},
            {"task_id": "2", "cid": -1001234567890, "key": "旧", "msg": "旧回复", "case": true, "ignore_forward": true,
             "delete": true, "ban": "300", "delay_delete": 10, "source_delay_delete": -5, "reply": false, "include": false},
            {"id": 3, "chatId": "-1001", "key": null, "response": "没有关键词"}
          ]
        }"#;
        let doc: Document = serde_json::from_str(raw).unwrap();
        assert_eq!(doc.next_id, 4);
        assert_eq!(doc.aliases.get("-1003").map(String::as_str), Some("-1001"));
        assert_eq!(doc.tasks.len(), 2);
        assert_eq!(doc.tasks[1].chat_id, "-1001234567890");
        assert_eq!(doc.tasks[1].ban_seconds, 300.0);
    }

    #[tokio::test]
    async fn test_keyword_actions_and_commands() {
        let dir = temp_dir("actions");
        let fake = Arc::new(FakeTelegram::default());
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        peers.remember_entities(
            &[tl::types::User {
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
                stories_unavailable: true,
                contact_require_premium: false,
                bot_business: false,
                bot_has_main_app: false,
                bot_forum_view: false,
                bot_forum_can_manage_topics: false,
                bot_can_manage_bots: false,
                bot_guestchat: false,
                bot_guard: false,
                bot_forum_sidebar: false,
                id: 42,
                access_hash: Some(7),
                first_name: Some("Cat".into()),
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
            .into()],
            &[tl::types::Channel {
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
                stories_unavailable: true,
                signature_profiles: false,
                autotranslation: false,
                broadcast_messages_allowed: false,
                monoforum: false,
                forum_tabs: false,
                in_news: false,
                forum_sidebar: false,
                id: 1001,
                access_hash: Some(9),
                title: "测试群".into(),
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
            .into()],
        );
        let client = Arc::new(Client::from_transport(
            fake.clone(),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let app = App::for_testing(
            dir.clone(),
            "test",
            "boot",
            client.clone(),
            peers,
            Arc::new(Ring::new()),
        );
        let delayed = Arc::new(Mutex::new(Vec::<Duration>::new()));
        let s = Arc::new(Service {
            app: Arc::clone(&app),
            store: kit::new_store(&app, "keyword.json", Document::default),
            write_mu: Mutex::new(()),
            cached: RwLock::new(None),
            slots: Arc::new(Semaphore::new(SLOTS)),
            warned: Mutex::new(HashMap::new()),
            after_hook: Some(Arc::new({
                let d = Arc::clone(&delayed);
                move |dur, _c, _f| {
                    d.lock().unwrap().push(dur);
                }
            })),
        });

        let cancel = CancellationToken::new();
        let inv = Invocation {
            client: client.clone(),
            registry: app.registry.clone(),
            message: Message {
                id: 10,
                peer: tl::types::PeerChannel { channel_id: 1001 }.into(),
                chat_id: "-1001001".into(),
                chat_type: mibot_bot::ChatType::Supergroup,
                text: ".keyword 广告\n+++\n$mention 别发广告，$delay_delete 秒后删\n+++\n\n+++\ndelete ban3600\n+++\n10".into(),
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
                raw: kit::blank_tl_message(
                    10,
                    tl::types::PeerChannel { channel_id: 1001 }.into(),
                ),
            },
            prefix: ".".into(),
            command: "keyword".into(),
            args: vec![],
            raw: "广告\n+++\n$mention 别发广告，$delay_delete 秒后删\n+++\n\n+++\ndelete ban3600\n+++\n10".into(),
            cancel: cancel.clone(),
            foreign: false,
        };
        s.command(inv).await.unwrap();
        assert_eq!(fake.take(), vec![r#"edit 10 "✅ 已添加关键词任务 1""#]);

        let incoming = Message {
            id: 50,
            peer: tl::types::PeerChannel { channel_id: 1001 }.into(),
            chat_id: "-1001001".into(),
            chat_type: mibot_bot::ChatType::Supergroup,
            text: "卖广告了".into(),
            out: false,
            edited: false,
            forward: false,
            post: false,
            saved: false,
            sender: tl::types::PeerUser { user_id: 42 }.into(),
            reply_to_id: 0,
            reply_to_peer: None,
            topic_id: 3,
            quote_text: String::new(),
            quote_entities: Vec::new(),
            raw: kit::blank_tl_message(
                50,
                tl::types::PeerChannel { channel_id: 1001 }.into(),
            ),
        };
        assert!(!s.offer(&incoming, &cancel));
        app.registry.wait().await;
        let calls = fake.take();
        assert_eq!(
            calls,
            vec![
                r#"send "Cat 别发广告，10 秒后删" reply=50 topic=3 mention=42"#,
                "edit-banned user=42 view=true send=true media=true",
                "delete [50]",
            ]
        );
        assert_eq!(*delayed.lock().unwrap(), vec![Duration::from_secs(10)]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
