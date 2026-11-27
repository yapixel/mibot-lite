use crate::ai::{
    CheckApiBackend, ChatRequest, Diagnosis, Probe, ProviderIdentity, ProviderImport,
    ProviderSummary, Service as AiService,
};
use crate::kit;
use mibot_app::App;
use mibot_bot::BotError;
use mibot_command::{
    CmdError, CmdResult, Invocation, PAGE_LIMIT, brief, code, escape, escaped_pages, group,
    html_pages, truncate,
};
use mibot_logtail::{Level, Ring};
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct State {
    #[serde(rename = "schemaVersion")]
    schema_version: i32,
    entries: Vec<Value>,
    #[serde(rename = "legacyImported")]
    legacy_imported: bool,
    #[serde(rename = "aiMigrated")]
    ai_migrated: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: 2,
            entries: Vec::new(),
            legacy_imported: false,
            ai_migrated: false,
        }
    }
}

struct LegacyDefault {
    url: &'static str,
    #[allow(dead_code)]
    kind: &'static str,
    model: &'static str,
}

fn legacy_provider(name: &str) -> Option<LegacyDefault> {
    Some(match name {
        "openai" => LegacyDefault {
            url: "https://api.openai.com/v1",
            kind: "openai",
            model: "gpt-4o-mini",
        },
        "anthropic" => LegacyDefault {
            url: "https://api.anthropic.com",
            kind: "anthropic",
            model: "claude-3-5-haiku-20241022",
        },
        "gemini" => LegacyDefault {
            url: "https://generativelanguage.googleapis.com/v1beta",
            kind: "gemini",
            model: "gemini-2.5-flash",
        },
        "deepseek" => LegacyDefault {
            url: "https://api.deepseek.com",
            kind: "openai-compatible",
            model: "deepseek-chat",
        },
        "openrouter" => LegacyDefault {
            url: "https://openrouter.ai/api/v1",
            kind: "openai-compatible",
            model: "openai/gpt-4o-mini",
        },
        "groq" => LegacyDefault {
            url: "https://api.groq.com/openai/v1",
            kind: "openai-compatible",
            model: "gpt-4o-mini",
        },
        "together" => LegacyDefault {
            url: "https://api.together.xyz/v1",
            kind: "openai-compatible",
            model: "gpt-4o-mini",
        },
        "fireworks" => LegacyDefault {
            url: "https://api.fireworks.ai/inference/v1",
            kind: "openai-compatible",
            model: "gpt-4o-mini",
        },
        "xai" => LegacyDefault {
            url: "https://api.x.ai/v1",
            kind: "openai-compatible",
            model: "gpt-4o-mini",
        },
        "nvidia" => LegacyDefault {
            url: "https://integrate.api.nvidia.com/v1",
            kind: "openai-compatible",
            model: "gpt-4o-mini",
        },
        _ => return None,
    })
}

fn legacy_kind(provider: &str, host: &str) -> &'static str {
    let h = host.to_ascii_lowercase();
    if provider == "anthropic" || h.contains("anthropic") {
        "anthropic"
    } else if provider == "gemini" || h.contains("generativelanguage") || h.contains("googleapis") {
        "gemini"
    } else if h == "api.openai.com" {
        "openai"
    } else {
        "openai-compatible"
    }
}

struct LegacyEntry {
    name: String,
    key: String,
    base_url: String,
    kind: String,
    model: String,
}

fn parse_entry(raw: &Value) -> Option<LegacyEntry> {
    let obj = raw.as_object()?;
    let name = obj.get("name").and_then(|v| v.as_str())?.to_owned();
    let key = obj.get("key").and_then(|v| v.as_str())?.trim().to_owned();
    if key.is_empty() || key.len() > 8192 {
        return None;
    }
    let provider = obj
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut base = obj
        .get("baseUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let known = legacy_provider(&provider);
    if base.is_empty() {
        base = known.as_ref()?.url.to_owned();
    }
    let parsed = Url::parse(&base).ok()?;
    let host = parsed.host_str()?.to_owned();
    if base.len() > 2048
        || (parsed.scheme() != "https" && parsed.scheme() != "http")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    let kind = legacy_kind(&provider, &host);
    if kind == "gemini" && (parsed.path().is_empty() || parsed.path() == "/") {
        let port = parsed
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        base = format!("{}://{host}{port}/v1beta", parsed.scheme());
    }
    let model = if kind == "anthropic" {
        legacy_provider("anthropic").unwrap().model
    } else if kind == "gemini" {
        legacy_provider("gemini").unwrap().model
    } else if let Some(ref k) = known {
        k.model
    } else if host.to_ascii_lowercase().contains("deepseek") {
        legacy_provider("deepseek").unwrap().model
    } else {
        "gpt-4o-mini"
    };
    Some(LegacyEntry {
        name,
        key,
        base_url: base,
        kind: kind.to_owned(),
        model: model.to_owned(),
    })
}

fn unsafe_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^A-Za-z0-9._-]").unwrap())
}

fn dash_run_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"-+").unwrap())
}

fn legacy_tag(name: &str, index: usize) -> String {
    let replaced = unsafe_tag_re().replace_all(name, "-");
    let collapsed = dash_run_re().replace_all(&replaced, "-");
    let mut tag = collapsed.trim_matches('-').to_owned();
    if tag.is_empty() {
        tag = format!("checkapi-{}", index + 1);
    }
    if tag.len() > 64 {
        tag.truncate(64);
    }
    tag
}

pub fn convert_legacy(raw: &[u8]) -> Result<Vec<u8>, serde_json::Error> {
    let list: Vec<Value> = serde_json::from_slice(raw)?;
    let entries: Vec<Value> = list.into_iter().filter(|it| parse_entry(it).is_some()).collect();
    serde_json::to_vec_pretty(&State {
        schema_version: 2,
        entries,
        legacy_imported: true,
        ai_migrated: false,
    })
}

struct Service {
    models: Option<Arc<dyn CheckApiBackend>>,
    store: Store<State>,
    logs: Option<Arc<Ring>>,
}

impl Service {
    fn migrate(&self) -> Result<usize, CmdError> {
        let current = self.store.read()?;
        if current.ai_migrated || current.entries.is_empty() {
            return Ok(0);
        }
        let Some(ref models) = self.models else {
            return Ok(0);
        };
        let mut imported = 0usize;
        let mut valid = 0usize;
        for raw in &current.entries {
            let Some(entry) = parse_entry(raw) else {
                let name = raw.get("name").and_then(|v| v.as_str()).unwrap_or("");
                kit::opt_log(
                    self.logs.as_ref(),
                    Level::Warn,
                    "checkapi.legacy_skipped",
                    &[("name", name), ("reason", "记录不完整或地址无效")],
                );
                continue;
            };
            let select = if valid == 0 {
                vec!["chat".to_owned()]
            } else {
                Vec::new()
            };
            let tag = legacy_tag(&entry.name, valid);
            valid += 1;
            match models.import_provider(ProviderImport {
                tag,
                url: entry.base_url,
                key: entry.key,
                kind: entry.kind,
                models: HashMap::from([("chat".into(), entry.model)]),
                select,
                ..Default::default()
            }) {
                Ok(_) => imported += 1,
                Err(err) => {
                    if kit::is_user_error(err.as_ref()) {
                        kit::opt_log(
                            self.logs.as_ref(),
                            Level::Warn,
                            "checkapi.legacy_skipped",
                            &[("name", &entry.name), ("reason", &err.to_string())],
                        );
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        self.store.update(|val| {
            val.schema_version = 2;
            val.entries.clear();
            val.legacy_imported = true;
            val.ai_migrated = true;
            Ok::<(), std::convert::Infallible>(())
        })?;
        Ok(imported)
    }

    async fn handle(&self, inv: &Invocation) -> CmdResult {
        let sub = inv.arg(0).to_ascii_lowercase();
        if sub.is_empty() || sub == "help" || sub == "h" {
            return inv
                .respond_pages(&html_pages(&help(inv.prefix()), PAGE_LIMIT))
                .await
                .map_err(Into::into);
        }
        let Some(ref models) = self.models else {
            return Err(kit::fail("ai 命令没有启用"));
        };
        match self.migrate() {
            Ok(count) if count > 0 => {
                let cs = count.to_string();
                kit::opt_log(
                    self.logs.as_ref(),
                    Level::Info,
                    "checkapi.migrated",
                    &[("count", &cs)],
                );
            }
            Err(err) => {
                let es = err.to_string();
                kit::opt_log(
                    self.logs.as_ref(),
                    Level::Warn,
                    "checkapi.migrate_failed",
                    &[("error", &es)],
                );
            }
            _ => {}
        }
        let tag = inv.arg(1);
        match sub.as_str() {
            "save" | "del" | "delete" => {
                let p = escape(inv.prefix());
                inv.edit(&format!(
                    "API 由 ai 命令统一管理：<code>{p}ai config add</code> 添加、<code>{p}ai config del</code> 删除、<code>{p}ai model chat</code> 选聊天模型"
                ))
                .await
                .map_err(Into::into)
            }
            "list" => {
                let providers = models.providers()?;
                inv.respond_pages(&html_pages(&render_list(&providers, inv.prefix()), PAGE_LIMIT))
                    .await
                    .map_err(Into::into)
            }
            "check" => self.check(inv, models.as_ref(), tag).await,
            "models" => {
                if tag.is_empty() {
                    return Err(kit::usage(inv.prefix(), "checkapi models 标签"));
                }
                inv.edit_text(&kit::working("正在获取模型列表")).await?;
                let list = models.models(tag).await?;
                inv.respond_pages(&html_pages(&render_models(tag, &list), PAGE_LIMIT))
                    .await
                    .map_err(Into::into)
            }
            "ask" => self.ask(inv, models.as_ref(), tag).await,
            "speed" => {
                if tag.is_empty() {
                    return Err(kit::usage(inv.prefix(), "checkapi speed 标签"));
                }
                inv.edit_text(&kit::working("正在测速")).await?;
                let (identity, probes) = models.benchmark(tag).await?;
                inv.respond_pages(&html_pages(&render_speed(&identity, &probes), PAGE_LIMIT))
                    .await
                    .map_err(Into::into)
            }
            "compare" => self.compare(inv, models.as_ref(), tag, inv.arg(2)).await,
            _ => Err(kit::fail(format!(
                "没有 {} 这个子命令，{}checkapi help 查看用法",
                inv.arg(0),
                inv.prefix()
            ))),
        }
    }

    async fn check(
        &self,
        inv: &Invocation,
        models: &dyn CheckApiBackend,
        tag: &str,
    ) -> CmdResult {
        inv.edit_text(&kit::working("正在检测")).await?;
        if !tag.is_empty() {
            let list = models.models(tag).await?;
            return inv
                .edit(&kit::feedback(
                    "success",
                    &format!("已连通 {tag}，共 {} 个模型", list.len()),
                    "",
                ))
                .await
                .map_err(Into::into);
        }
        let providers = models.providers()?;
        let mut rows = Vec::with_capacity(providers.len());
        for p in providers {
            match models.models(&p.tag).await {
                Ok(list) => rows.push((p.tag, String::new(), list.len())),
                Err(e) => {
                    if inv.cancel.is_cancelled() {
                        return Err(BotError::Cancelled.into());
                    }
                    let msg = kit::or_default(&first_message(e.as_ref()), "检测失败").to_owned();
                    rows.push((p.tag, msg, 0));
                }
            }
        }
        inv.respond_pages(&html_pages(&render_check(&rows, inv.prefix()), PAGE_LIMIT))
            .await
            .map_err(Into::into)
    }

    async fn ask(&self, inv: &Invocation, models: &dyn CheckApiBackend, tag: &str) -> CmdResult {
        if tag.is_empty() {
            return Err(kit::usage(inv.prefix(), "checkapi ask 标签 [问题]"));
        }
        let raw_q = inv.raw_after(2).trim();
        let question = if raw_q.is_empty() { "say hello" } else { raw_q };
        inv.edit_text(&kit::working("正在提问")).await?;
        let answer = models
            .chat(ChatRequest {
                tag: tag.to_owned(),
                text: question.to_owned(),
                max_output_tokens: 100,
                ..Default::default()
            })
            .await?;
        let mut pages = escaped_pages(&answer, PAGE_LIMIT);
        pages[0] = format!("🩺 <b>测试提问</b>（{}）\n\n{}", code(tag), pages[0]);
        inv.respond_pages(&pages).await.map_err(Into::into)
    }

    async fn compare(
        &self,
        inv: &Invocation,
        models: &dyn CheckApiBackend,
        first: &str,
        second: &str,
    ) -> CmdResult {
        if first.is_empty() || second.is_empty() {
            return Err(kit::usage(inv.prefix(), "checkapi compare 标签1 标签2"));
        }
        inv.edit_text(&kit::working("正在检测两个 API")).await?;
        let (left_res, right_res) = tokio::join!(models.diagnose(first), models.diagnose(second));
        let left = left_res?;
        let right = right_res?;
        inv.respond_pages(&html_pages(&render_compare(&left, &right), PAGE_LIMIT))
            .await
            .map_err(Into::into)
    }
}

fn first_message(err: &(dyn std::error::Error + 'static)) -> String {
    if let Some(u) = mibot_command::errors::find_in_chain::<mibot_command::UserError>(err) {
        return truncate(&u.text, 80);
    }
    brief(err)
}

fn render_list(providers: &[ProviderSummary], prefix: &str) -> String {
    let mut rows = Vec::new();
    for p in providers {
        let mut row = format!("• {} · {}", code(&p.tag), escape(&p.kind));
        for mode in ["chat", "search", "image", "video"] {
            if let Some(m) = p.models.get(mode)
                && !m.is_empty()
            {
                row.push_str(&format!(" · {}", escape(&format!("{mode}={m}"))));
            }
        }
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(format!(
            "• 还没有配置，用 {} 添加",
            code(&format!("{prefix}ai config add"))
        ));
    }
    format!("🩺 <b>AI 接口</b>\n\n{}", rows.join("\n"))
}

fn render_check(rows: &[(String, String, usize)], prefix: &str) -> String {
    if rows.is_empty() {
        return format!(
            "🩺 <b>API 检测</b>\n\n还没有配置 API，用 {} 添加",
            code(&format!("{prefix}ai config add"))
        );
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|(tag, failure, count)| {
            if !failure.is_empty() {
                format!("{} · ❌ {}", code(tag), escape(failure))
            } else {
                format!("{} · ✅ {count} 个模型", code(tag))
            }
        })
        .collect();
    format!("🩺 <b>API 检测</b>\n\n{}", lines.join("\n"))
}

fn render_models(tag: &str, models: &[String]) -> String {
    let lines: Vec<String> = models.iter().map(|m| code(m)).collect();
    format!(
        "🩺 <b>模型列表</b>\n{} · {} 个\n\n{}",
        code(tag),
        models.len(),
        lines.join("\n")
    )
}

fn ms_str(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

fn tps_str(p: &Probe) -> String {
    if !p.ok || p.usage.is_none() || p.elapsed.is_zero() {
        return "?".into();
    }
    format!("{:.1}", p.usage.as_ref().unwrap().total as f64 / p.elapsed.as_secs_f64())
}

fn render_speed(identity: &ProviderIdentity, probes: &[Probe]) -> String {
    let mut lines = vec![
        format!("🩺 <b>{} 速度测试</b>", escape(&identity.display_name)),
        format!("{} · 每次最多 50 token", code(&identity.tag)),
        String::new(),
    ];
    for p in probes {
        if p.ok {
            lines.push(format!(
                "✅ {}：{}（{} tok/s）",
                code(&p.model),
                ms_str(p.elapsed),
                tps_str(p)
            ));
        } else {
            lines.push(format!(
                "❌ {}：{}",
                code(&p.model),
                escape(kit::or_default(&p.error, "失败"))
            ));
        }
    }
    lines.join("\n")
}

fn diagnosis_lines(res: &Diagnosis) -> Vec<String> {
    let mut lines = vec![
        format!(
            "<b>{}</b>（{}）· {}",
            escape(&res.provider.display_name),
            escape(&res.provider.kind),
            code(&res.provider.tag)
        ),
        String::new(),
        "<b>账户余额</b>".to_owned(),
    ];
    if !res.balance.fields.is_empty() {
        for f in &res.balance.fields {
            lines.push(format!("{}: {}", escape(&f.label), escape(&f.value))
                .replace(": ", "："));
        }
    } else if res.balance.status == "unsupported" {
        lines.push("请到官网查看余额".into());
    } else if res.balance.status == "invalid" {
        lines.push("⚠️ Key 无效或没有权限".into());
    } else {
        lines.push("⚠️ 查询失败".into());
    }
    lines.push(String::new());
    lines.push("<b>对话测试</b>".into());
    match &res.chat {
        None => lines.push("❌ 没有配置聊天模型".into()),
        Some(chat) if chat.ok => {
            lines.push(format!(
                "✅ 回复「{}」，{} · {}",
                escape(&chat.text),
                ms_str(chat.elapsed),
                code(&chat.model)
            ));
            if let Some(u) = &chat.usage {
                lines.push(format!(
                    "Token：输入 {} · 输出 {} · 合计 {}",
                    u.prompt, u.completion, u.total
                ));
            }
            for lim in &chat.rate_limits {
                lines.push(format!("{}：{}", escape(&lim.name), escape(&lim.value)));
            }
        }
        Some(chat) => {
            lines.push(format!(
                "❌ {}：{}",
                code(&chat.model),
                escape(kit::or_default(&chat.error, "失败"))
            ));
        }
    }
    lines.push(String::new());
    lines.push("<b>可用模型</b>".into());
    if !res.models_error.is_empty() {
        lines.push(format!("❌ 获取失败：{}", escape(&res.models_error)));
        return lines;
    }
    let names: Vec<String> = res.models.iter().map(|m| code(m)).collect();
    lines.push(format!("共 {} 个", res.models.len()));
    if !names.is_empty() {
        lines.push(names.join(" · "));
    }
    lines
}

fn render_compare(left: &Diagnosis, right: &Diagnosis) -> String {
    let mut lines = vec!["🩺 <b>API 对比</b>".to_owned(), String::new()];
    lines.extend(diagnosis_lines(left));
    lines.push(String::new());
    lines.push("━━━━━━━━━━━━━━━━".into());
    lines.push(String::new());
    lines.extend(diagnosis_lines(right));
    lines.join("\n")
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let item = |args: &str, text: &str| {
        format!("• <code>{p}checkapi {}</code> {text}\n", escape(args))
    };
    format!(
        "🩺 <b>API 检测</b>\n\n检测 ai 命令统一管理的 API。地址、Key、类型和模型都在 ai 里配置，这里只按标签检测。\n\n{}{}{}{}{}{}\n添加或删除 API 用 <code>{p}ai config add</code>、<code>{p}ai config del</code>，选聊天模型用 <code>{p}ai model chat</code>。",
        item("list", "查看 ai 配置里的标签"),
        item("check [标签]", "验证一个或全部标签的模型接口"),
        item("models 标签", "列出这个标签的全部模型"),
        item("ask 标签 [问题]", "发一次测试提问，省略问题时发 say hello，最多输出 100 token"),
        item("speed 标签", "逐个模型测响应速度，每次最多 50 token"),
        item("compare 标签1 标签2", "对比两个标签的余额、对话和模型"),
    )
}

pub fn register(a: &Arc<App>, models: Option<Arc<AiService>>) {
    let svc = Arc::new(Service {
        models: models.map(|m| m.checker()),
        store: kit::new_store(a, "checkapi.json", State::default),
        logs: Some(Arc::clone(&a.logs)),
    });
    let mut cmd = kit::make_cmd(
        "checkapi",
        &[],
        group::AI,
        "检测 AI 接口与模型",
        "list|check|models|ask|speed|compare [标签]",
        help,
        move |inv: Invocation| {
            let s = Arc::clone(&svc);
            Box::pin(async move {
                s.handle(&inv)
                    .await
                    .map_err(|e| kit::wrap_fail("API 检测失败", e))
            })
        },
    );
    cmd.timeout = Some(Duration::from_secs(15 * 60));
    a.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{Balance, BalanceField, RateLimit, TokenUsage};
    use grammers_mtsender::InvocationError;
    use grammers_tl_types::{self as tl, Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, Client, PeerCache, RpcTransport, SentRecorder};
    use mibot_command::Registry;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    struct FakeAi {
        providers: Vec<ProviderSummary>,
        models: HashMap<String, Vec<String>>,
        imported: Mutex<Vec<ProviderImport>>,
        reject_bad: Option<&'static str>,
        chats: Mutex<Vec<ChatRequest>>,
    }

    impl CheckApiBackend for FakeAi {
        fn providers(&self) -> Result<Vec<ProviderSummary>, CmdError> {
            Ok(self.providers.clone())
        }
        fn models<'a>(
            &'a self,
            tag: &'a str,
        ) -> BoxFuture<'a, Result<Vec<String>, CmdError>> {
            Box::pin(async move {
                if let Some(list) = self.models.get(tag) {
                    Ok(list.clone())
                } else {
                    Err(kit::fail("AI 接口返回 HTTP 401：Incorrect API key provided: ***"))
                }
            })
        }
        fn chat<'a>(&'a self, req: ChatRequest) -> BoxFuture<'a, Result<String, CmdError>> {
            Box::pin(async move {
                self.chats.lock().unwrap().push(req);
                Ok("hello <world>".into())
            })
        }
        fn benchmark<'a>(
            &'a self,
            tag: &'a str,
        ) -> BoxFuture<'a, Result<(ProviderIdentity, Vec<Probe>), CmdError>> {
            Box::pin(async move {
                Ok((
                    ProviderIdentity {
                        tag: tag.into(),
                        kind: "deepseek".into(),
                        display_name: "DeepSeek".into(),
                    },
                    vec![
                        Probe {
                            model: "deepseek-chat".into(),
                            ok: true,
                            text: String::new(),
                            elapsed: Duration::from_millis(800),
                            usage: Some(TokenUsage {
                                prompt: 0,
                                completion: 0,
                                total: 20,
                            }),
                            rate_limits: vec![],
                            error: String::new(),
                        },
                        Probe {
                            model: "deepseek-reasoner".into(),
                            ok: false,
                            text: String::new(),
                            elapsed: Duration::ZERO,
                            usage: None,
                            rate_limits: vec![],
                            error: "请求超时，稍后再试".into(),
                        },
                    ],
                ))
            })
        }
        fn diagnose<'a>(&'a self, tag: &'a str) -> BoxFuture<'a, Result<Diagnosis, CmdError>> {
            Box::pin(async move {
                if tag == "missing" {
                    return Err(kit::fail("未找到指定的 AI 提供商"));
                }
                Ok(Diagnosis {
                    provider: ProviderIdentity {
                        tag: tag.into(),
                        kind: "openai".into(),
                        display_name: "OpenAI".into(),
                    },
                    balance: Balance {
                        status: "ok".into(),
                        fields: vec![BalanceField {
                            label: "套餐".into(),
                            value: "Plus".into(),
                        }],
                    },
                    models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
                    models_error: String::new(),
                    chat: Some(Probe {
                        model: "gpt-4o-mini".into(),
                        ok: true,
                        text: "ok".into(),
                        elapsed: Duration::from_millis(321),
                        usage: Some(TokenUsage {
                            prompt: 5,
                            completion: 1,
                            total: 6,
                        }),
                        rate_limits: vec![RateLimit {
                            name: "x-ratelimit".into(),
                            value: "10".into(),
                        }],
                        error: String::new(),
                    }),
                })
            })
        }
        fn import_provider(&self, value: ProviderImport) -> Result<(String, bool), CmdError> {
            if let Some(msg) = self.reject_bad
                && (value.tag == "bad" || msg == "disk full")
            {
                return if msg == "disk full" {
                    Err(Box::new(std::io::Error::other("disk full")))
                } else {
                    Err(kit::fail(msg))
                };
            }
            let tag = value.tag.clone();
            self.imported.lock().unwrap().push(value);
            Ok((tag, true))
        }
    }

    struct FakeTg {
        edits: Mutex<Vec<String>>,
    }

    impl RpcTransport for FakeTg {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                if constructor_id == tl::functions::messages::EditMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::EditMessage::from_bytes(&body[4..]).unwrap();
                    self.edits
                        .lock()
                        .unwrap()
                        .push(req.message.unwrap_or_default());
                } else if constructor_id == tl::functions::messages::SendMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::SendMessage::from_bytes(&body[4..]).unwrap();
                    self.edits.lock().unwrap().push(req.message);
                }
                Ok(tl::enums::Updates::Updates(tl::types::Updates {
                    updates: vec![],
                    users: vec![],
                    chats: vec![],
                    date: 0,
                    seq: 0,
                })
                .to_bytes())
            })
        }
    }

    async fn run_cmd(svc: &Service, args: &[&str]) -> (String, CmdResult) {
        let fake = Arc::new(FakeTg {
            edits: Mutex::new(Vec::new()),
        });
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            Arc::clone(&fake) as Arc<dyn RpcTransport>,
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let mut msg = kit::blank_bot_message(7, tl::types::PeerUser { user_id: 1 }.into());
        msg.saved = true;
        msg.out = true;
        let inv = Invocation {
            client,
            registry: Arc::new(Registry::new()),
            message: msg,
            prefix: ".".into(),
            command: "checkapi".into(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            raw: args.join(" "),
            cancel: CancellationToken::new(),
            foreign: false,
        };
        let res = svc.handle(&inv).await;
        let last = fake.edits.lock().unwrap().last().cloned().unwrap_or_default();
        (last, res)
    }

    #[tokio::test]
    async fn test_subcommands_and_legacy_migration() {
        let dir = std::env::temp_dir().join(format!(
            "mibot-checkapi-test-{}-{}",
            std::process::id(),
            mibot_bot::random_id().unsigned_abs()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let ai = Arc::new(FakeAi {
            providers: vec![
                ProviderSummary {
                    tag: "main".into(),
                    kind: "auto".into(),
                    models: HashMap::from([("chat".into(), "gpt-4o".into())]),
                },
                ProviderSummary {
                    tag: "dead".into(),
                    kind: "gemini".into(),
                    models: HashMap::new(),
                },
            ],
            models: HashMap::from([("main".into(), vec!["a".into(), "b".into(), "c".into()])]),
            imported: Mutex::new(Vec::new()),
            reject_bad: None,
            chats: Mutex::new(Vec::new()),
        });
        let svc = Service {
            models: Some(Arc::clone(&ai) as Arc<dyn CheckApiBackend>),
            store: Store::new(dir.join("checkapi.json"), State::default),
            logs: None,
        };

        let (out, r) = run_cmd(&svc, &["list"]).await;
        r.unwrap();
        assert_eq!(out, "🩺 AI 接口\n\n• main · auto · chat=gpt-4o\n• dead · gemini");

        let (out, r) = run_cmd(&svc, &["check"]).await;
        r.unwrap();
        assert!(out.contains("main · ✅ 3 个模型") && out.contains("dead · ❌ AI 接口返回 HTTP 401"));

        let (out, r) = run_cmd(&svc, &["check", "main"]).await;
        r.unwrap();
        assert_eq!(out, "✅ 已连通 main，共 3 个模型");

        let (out, r) = run_cmd(&svc, &["models", "main"]).await;
        r.unwrap();
        assert_eq!(out, "🩺 模型列表\nmain · 3 个\n\na\nb\nc");

        let (out, r) = run_cmd(&svc, &["ask", "main"]).await;
        r.unwrap();
        assert_eq!(out, "🩺 测试提问（main）\n\nhello <world>");
        assert_eq!(ai.chats.lock().unwrap()[0].text, "say hello");

        let (out, r) = run_cmd(&svc, &["speed", "main"]).await;
        r.unwrap();
        assert!(out.contains("✅ deepseek-chat：800 ms（25.0 tok/s）") && out.contains("❌ deepseek-reasoner：请求超时，稍后再试"));

        let (out, r) = run_cmd(&svc, &["compare", "main", "other"]).await;
        r.unwrap();
        assert!(out.contains("✅ 回复「ok」，321 ms · gpt-4o-mini") && out.contains("Token：输入 5 · 输出 1 · 合计 6"));

        assert!(run_cmd(&svc, &["compare", "main", "missing"]).await.1.is_err());

        // Legacy migration test
        svc.store
            .update(|st| {
                *st = serde_json::from_str(r#"{"schemaVersion":2,"entries":[{"name":"My Relay!","key":"sk-1","baseUrl":"https://relay.example/v1","addedAt":1},{"name":"bad","key":"","baseUrl":"https://x"},{"name":"---","key":"sk-2","baseUrl":"http://second.example"}],"legacyImported":true,"aiMigrated":false}"#).unwrap();
                Ok::<(), std::convert::Infallible>(())
            })
            .unwrap();
        assert_eq!(svc.migrate().unwrap(), 2);
        assert_eq!(ai.imported.lock().unwrap()[0].tag, "My-Relay");
        assert_eq!(ai.imported.lock().unwrap()[1].tag, "checkapi-2");
        assert_eq!(svc.migrate().unwrap(), 0);

        let conv = convert_legacy(br#"[{"name":"a","key":"k","baseUrl":"https://a.example/v1"},{"name":"c","key":"sk-ant-x","provider":"anthropic"},{"name":"d","key":"x","provider":"cohere"}]"#).unwrap();
        let st: State = serde_json::from_slice(&conv).unwrap();
        assert_eq!(st.entries.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
