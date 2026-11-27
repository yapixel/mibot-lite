use crate::ai::{
    AI_PROVIDER_TYPES, AiConfig, AiProvider, AiSelection, Service, ai_provider_failed,
    assert_allowed_model, build_chat_request, decode_reply, http_failure, json_to_string,
    object_of, provider_auth, provider_endpoint, provider_failure, resolve_provider_type,
    string_of,
};
use crate::kit;
use mibot_command::{CmdError, truncate};
use mibot_httpx::{self, HttpxError, Method, Request};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

const CHECK_LIMIT: usize = 2 << 20;
const DEFAULT_CHECK_TIMEOUT_MS: u64 = 90_000;
static CHECK_TIMEOUT_MS: AtomicU64 = AtomicU64::new(DEFAULT_CHECK_TIMEOUT_MS);

fn current_check_timeout() -> Duration {
    Duration::from_millis(CHECK_TIMEOUT_MS.load(Ordering::Relaxed))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSummary {
    pub tag: String,
    pub kind: String,
    pub models: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt: i32,
    pub completion: i32,
    pub total: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimit {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub model: String,
    pub ok: bool,
    pub text: String,
    pub elapsed: Duration,
    pub usage: Option<TokenUsage>,
    pub rate_limits: Vec<RateLimit>,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderIdentity {
    pub tag: String,
    pub kind: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceField {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Balance {
    pub status: String,
    pub fields: Vec<BalanceField>,
}

#[derive(Debug, Clone)]
pub struct Diagnosis {
    pub provider: ProviderIdentity,
    pub balance: Balance,
    pub models: Vec<String>,
    pub models_error: String,
    pub chat: Option<Probe>,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderImport {
    pub tag: String,
    pub url: String,
    pub key: String,
    pub kind: String,
    pub stream: bool,
    pub responses: bool,
    pub models: HashMap<String, String>,
    pub select: Vec<String>,
}

const RATE_LIMIT_HEADERS: &[&str] = &[
    "retry-after",
    "ratelimit-limit",
    "ratelimit-remaining",
    "ratelimit-reset",
    "x-ratelimit-limit-requests",
    "x-ratelimit-remaining-requests",
    "x-ratelimit-reset-requests",
    "x-ratelimit-limit-tokens",
    "x-ratelimit-remaining-tokens",
    "x-ratelimit-reset-tokens",
];

const PROVIDER_HOSTS: &[(&str, &str)] = &[
    ("openrouter", "openrouter"),
    ("deepseek", "deepseek"),
    ("groq", "groq"),
    ("together", "together"),
    ("fireworks", "fireworks"),
    ("mistral", "mistral"),
    ("perplexity", "perplexity"),
    ("siliconflow", "siliconflow"),
    ("deepinfra", "deepinfra"),
    ("vercel", "vercel"),
    ("azure", "azure"),
    ("x.ai", "xai"),
    ("nvidia", "nvidia"),
    ("novita", "novita"),
    ("cerebras", "cerebras"),
];

fn display_name_for(kind: &str) -> &'static str {
    match kind {
        "openai" => "OpenAI",
        "openrouter" => "OpenRouter",
        "deepseek" => "DeepSeek",
        "gemini" => "Google Gemini",
        "anthropic" => "Anthropic",
        "xai" => "xAI (Grok)",
        "nvidia" => "NVIDIA NIM",
        "novita" => "Novita",
        "cerebras" => "Cerebras",
        _ => kit::tr("OpenAI-Compatible API", "OpenAI 兼容 API"),
    }
}

fn benchmark_models_for(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "openai" => &["gpt-4.1-mini", "gpt-4.1-nano", "gpt-4o-mini"],
        "deepseek" => &["deepseek-chat", "deepseek-reasoner"],
        "openrouter" => &[
            "openai/gpt-4.1-mini",
            "anthropic/claude-3.5-haiku",
            "google/gemini-2.5-flash",
        ],
        "groq" => &["llama-3.3-70b-versatile", "mixtral-8x7b-32768", "gemma2-9b-it"],
        "together" => &[
            "meta-llama/Llama-4-Maverick-17B-128E-Instruct-FP8",
            "Qwen/Qwen3-235B-A22B",
        ],
        "fireworks" => &[
            "accounts/fireworks/models/llama-v3p3-70b-instruct",
            "accounts/fireworks/models/deepseek-v3",
        ],
        "mistral" => &["mistral-small-2506", "codestral-2501"],
        "perplexity" => &["sonar-reasoning", "sonar-pro"],
        "siliconflow" => &["Qwen/Qwen3-8B", "deepseek-ai/DeepSeek-V3"],
        "deepinfra" => &["meta-llama/Llama-4-Maverick-17B-128E-Instruct"],
        "vercel" | "azure" => &["gpt-4o-mini"],
        "xai" => &["grok-3-mini", "grok-2-latest"],
        "nvidia" => &["nvidia/llama-3.1-nemotron-ultra-253b-v1"],
        "novita" => &["deepseek/deepseek-v3-0324", "meta-llama/Llama-3.3-70B-Instruct"],
        "cerebras" => &["llama3.1-8b", "llama3.3-70b"],
        _ => return None,
    })
}

struct CheckResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl CheckResponse {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

async fn check_do(
    method: Method,
    target: &str,
    headers: &HashMap<String, String>,
    body: Option<&Value>,
    timeout: Duration,
    limit: usize,
) -> Result<CheckResponse, HttpxError> {
    let mut req_headers = headers.clone();
    req_headers
        .entry("User-Agent".into())
        .or_insert_with(|| mibot_httpx::USER_AGENT.to_owned());
    let encoded = if let Some(b) = body {
        req_headers
            .entry("Content-Type".into())
            .or_insert_with(|| "application/json".into());
        Some(serde_json::to_vec(b).map_err(|e| HttpxError::Other(e.to_string()))?)
    } else {
        None
    };
    let (resp, resp_headers) = mibot_httpx::do_request_with_headers(Request {
        method,
        url: target.to_owned(),
        headers: req_headers,
        body: encoded,
        timeout: Some(if timeout.is_zero() {
            Duration::from_secs(30)
        } else {
            timeout
        }),
        max_bytes: Some(limit),
        wait_for_headers: false,
        no_redirect: false,
    })
    .await?;
    Ok(CheckResponse {
        status: resp.status,
        headers: resp_headers,
        body: resp.body,
    })
}

fn transport_failure(err: &HttpxError) -> String {
    if matches!(err, HttpxError::CredentialRedirect | HttpxError::CrossHostRedirect) {
        kit::tr(
            "API redirected to another host; rejected to protect your key",
            "接口跳转到了别的地址，为保护 Key 已拒绝",
        )
        .into()
    } else {
        mibot_httpx::reason(err)
    }
}

fn check_failure(err: &CmdError) -> String {
    if let Some(u) = mibot_command::errors::find_in_chain::<mibot_command::UserError>(err.as_ref()) {
        return truncate(&u.text, 200);
    }
    if let Some(h) = mibot_command::errors::find_in_chain::<HttpxError>(err.as_ref()) {
        return transport_failure(h);
    }
    mibot_command::brief(err.as_ref())
}

fn rate_limits_of(headers: &HashMap<String, String>) -> Vec<RateLimit> {
    let mut limits = Vec::new();
    for &name in RATE_LIMIT_HEADERS {
        if let Some(val) = headers.get(name) {
            limits.push(RateLimit {
                name: name.to_owned(),
                value: kit::truncate_runes(val, 256),
            });
        }
    }
    limits
}

fn usage_of(payloads: &[Value]) -> (Option<TokenUsage>, String) {
    let mut found = None;
    let mut model = String::new();
    for payload in payloads {
        if let Some(name) = string_of(payload.get("model")) {
            model = name.to_owned();
        } else if let Some(name) = payload
            .get("response")
            .and_then(|r| string_of(r.get("model")))
        {
            model = name.to_owned();
        }
        let usage = object_of(payload.get("usage"))
            .or_else(|| object_of(payload.get("usageMetadata")))
            .or_else(|| payload.get("response").and_then(|r| object_of(r.get("usage"))));
        let Some(usage) = usage else {
            continue;
        };
        let pick = |keys: &[&str]| -> i32 {
            for k in keys {
                if let Some(v) = usage.get(*k).and_then(|v| v.as_f64()) {
                    return v as i32;
                }
            }
            0
        };
        let prompt = pick(&["prompt_tokens", "input_tokens", "promptTokenCount"]);
        let completion = pick(&["completion_tokens", "output_tokens", "candidatesTokenCount"]);
        let mut total = pick(&["total_tokens", "totalTokenCount"]);
        if total == 0 {
            total = prompt + completion;
        }
        if total > 0 {
            found = Some(TokenUsage {
                prompt,
                completion,
                total,
            });
        }
    }
    (found, model)
}

fn configured_chat_model(cfg: &AiConfig, tag: &str) -> String {
    if let Some(p) = cfg.configs.get(tag)
        && let Some(m) = p.models.get("chat")
        && !m.is_empty()
    {
        return m.clone();
    }
    if cfg.current_chat_tag == tag {
        return cfg.current_chat_model.clone();
    }
    String::new()
}

pub(crate) fn identify_provider(tag: &str, provider: &AiProvider) -> ProviderIdentity {
    let mut kind = resolve_provider_type(provider);
    let host = Url::parse(&provider.url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default();
    for &(needle, k) in PROVIDER_HOSTS {
        if host.contains(needle) {
            kind = k.to_owned();
            break;
        }
    }
    ProviderIdentity {
        tag: tag.to_owned(),
        display_name: display_name_for(&kind).to_owned(),
        kind,
    }
}

async fn provider_models(cfg: &AiConfig, tag: &str) -> Result<Vec<String>, CmdError> {
    let Some(provider) = cfg.configs.get(tag) else {
        return Err(kit::fail(kit::tr(
            "Specified AI provider not found",
            "未找到指定的 AI 提供商",
        )));
    };
    if tag.is_empty() {
        return Err(kit::fail(kit::tr(
            "Specified AI provider not found",
            "未找到指定的 AI 提供商",
        )));
    }
    let kind = resolve_provider_type(provider);
    if kind == "codex" {
        return Err(kit::fail(kit::tr(
            "codex API does not support model listing",
            "codex 类型的 API 不支持模型列表",
        )));
    }
    let mut target = provider_endpoint(&provider.url, &kind, "models")?;
    let (resolved_target, headers, secret) = provider_auth(&kind, &provider.key, &target);
    target = resolved_target;

    let timeout = Duration::from_secs(cfg.timeout.max(1) as u64).min(current_check_timeout());
    let deadline = tokio::time::Instant::now() + timeout;
    let mut remaining = CHECK_LIMIT;
    let mut seen = HashSet::new();
    let mut names = Vec::new();

    loop {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        if rem.is_zero() {
            return Err(kit::fail(kit::tr(
                "Failed to fetch model list: request timed out, please try again later",
                "获取模型列表失败：请求超时，稍后再试",
            )));
        }
        let response = check_do(Method::GET, &target, &headers, None, rem, remaining)
            .await
            .map_err(|e| {
                kit::fail(format!(
                    "{}{}",
                    kit::tr("Failed to fetch model list: ", "获取模型列表失败："),
                    transport_failure(&e)
                ))
            })?;
        if !response.ok() {
            return Err(kit::fail(http_failure(response.status, &response.body, &secret)));
        }
        remaining = remaining.saturating_sub(response.body.len());
        let payload: Value = serde_json::from_slice(&response.body)
            .map_err(|_| kit::fail(kit::tr("AI returned invalid JSON", "AI 返回无效 JSON")))?;
        if ai_provider_failed(&payload) {
            return Err(provider_failure(&payload, &secret));
        }
        let rows = payload
            .get("data")
            .and_then(|v| v.as_array())
            .or_else(|| payload.get("models").and_then(|v| v.as_array()));
        if let Some(rows) = rows {
            for row in rows {
                let name = string_of(row.get("id"))
                    .filter(|s| !s.is_empty())
                    .or_else(|| string_of(row.get("name")));
                if let Some(n) = name
                    && !n.is_empty()
                    && seen.insert(n.to_owned())
                {
                    names.push(n.to_owned());
                }
            }
        }
        let (param, cursor) = if kind == "gemini"
            && let Some(c) = string_of(payload.get("nextPageToken"))
            && !c.is_empty()
        {
            ("pageToken", c.to_owned())
        } else if kind == "anthropic"
            && payload.get("has_more").and_then(|v| v.as_bool()) == Some(true)
        {
            (
                "after_id",
                string_of(payload.get("last_id")).unwrap_or("").to_owned(),
            )
        } else {
            names.sort();
            if names.is_empty() {
                return Err(kit::fail(kit::tr(
                    "API returned no models",
                    "接口没有返回任何模型",
                )));
            }
            return Ok(names);
        };

        let invalid_cursor = || {
            kit::fail(kit::tr(
                "Invalid pagination cursor in model list",
                "模型列表的分页游标无效",
            ))
        };
        let mut parsed = Url::parse(&target).map_err(|_| invalid_cursor())?;
        let already = parsed
            .query_pairs()
            .find(|(k, _)| k == param)
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        if cursor.is_empty() || already == cursor {
            return Err(invalid_cursor());
        }
        let mut pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .filter(|(k, _)| k != param)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        pairs.push((param.to_owned(), cursor));
        parsed.query_pairs_mut().clear().extend_pairs(pairs);
        target = parsed.to_string();
    }
}

async fn probe_model(cfg: &AiConfig, tag: &str, model: &str, max_tokens: i32) -> Probe {
    let mut result = Probe {
        model: model.to_owned(),
        ok: false,
        text: String::new(),
        elapsed: Duration::ZERO,
        usage: None,
        rate_limits: Vec::new(),
        error: String::new(),
    };
    if let Err(e) = assert_allowed_model(model) {
        result.error = check_failure(&e);
        return result;
    }
    let mut cfg_copy = cfg.clone();
    if let Some(p) = cfg_copy.configs.get_mut(tag) {
        p.stream = false;
    }
    let req = match build_chat_request(
        &cfg_copy,
        &AiSelection {
            tag: tag.to_owned(),
            model: model.to_owned(),
            reasoning: "auto".into(),
            tier: "auto".into(),
        },
        "ok",
        "",
        &[],
        Some(max_tokens),
    ) {
        Ok(r) => r,
        Err(e) => {
            result.error = check_failure(&e);
            return result;
        }
    };
    let timeout = req.timeout.min(current_check_timeout());
    let started = Instant::now();
    let resp = match check_do(
        Method::POST,
        &req.url,
        &req.headers,
        Some(&req.body),
        timeout,
        super::AI_RESPONSE_LIMIT,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            result.elapsed = started.elapsed();
            result.error = transport_failure(&e);
            return result;
        }
    };
    result.elapsed = started.elapsed();
    result.rate_limits = rate_limits_of(&resp.headers);
    if !resp.ok() {
        result.error = http_failure(resp.status, &resp.body, &req.provider.key);
        return result;
    }
    match decode_reply(&resp.body, &req.format, &req.provider.key) {
        Ok((text, events)) => {
            result.ok = true;
            result.text = text;
            let (usage, model_name) = usage_of(&events);
            result.usage = usage;
            if !model_name.is_empty() {
                result.model = model_name;
            }
        }
        Err(e) => result.error = check_failure(&e),
    }
    result
}

struct BalanceReply {
    ok: bool,
    status: u16,
    data: serde_json::Map<String, Value>,
}

async fn balance_get(
    method: Method,
    target: &str,
    headers: &HashMap<String, String>,
    body: Option<&Value>,
    timeout: Duration,
) -> BalanceReply {
    let Ok(resp) = check_do(method, target, headers, body, timeout, CHECK_LIMIT).await else {
        return BalanceReply {
            ok: false,
            status: 0,
            data: serde_json::Map::new(),
        };
    };
    let text = String::from_utf8_lossy(&resp.body);
    let data = if text.trim().is_empty() {
        serde_json::Map::new()
    } else if let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&resp.body) {
        obj
    } else {
        return BalanceReply {
            ok: false,
            status: resp.status,
            data: serde_json::Map::new(),
        };
    };
    BalanceReply {
        ok: resp.ok(),
        status: resp.status,
        data,
    }
}

fn balance_auth(provider: &AiProvider, kind: &str) -> HashMap<String, String> {
    if kind == "anthropic" {
        HashMap::from([
            ("x-api-key".into(), provider.key.clone()),
            ("anthropic-version".into(), "2023-06-01".into()),
        ])
    } else {
        HashMap::from([("Authorization".into(), format!("Bearer {}", provider.key))])
    }
}

fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "?".into(),
        Some(v) => json_to_string(v),
    }
}

fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => !s.is_empty(),
        _ => true,
    }
}

fn yes_no_text(v: bool) -> &'static str {
    if v {
        kit::tr("Yes", "是")
    } else {
        kit::tr("No", "否")
    }
}

async fn open_ai_balance(provider: &AiProvider) -> Balance {
    let headers = balance_auth(provider, "openai");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut targets = Vec::with_capacity(3);
    for path in ["dashboard/billing/subscription", "dashboard/billing/usage", "models"] {
        let Ok(mut target) = provider_endpoint(&provider.url, "openai", path) else {
            return Balance {
                status: "error".into(),
                fields: Vec::new(),
            };
        };
        if path == "dashboard/billing/usage"
            && let Ok(mut u) = Url::parse(&target)
        {
            u.query_pairs_mut()
                .append_pair("start_date", &(now - 90 * 86400).to_string())
                .append_pair("end_date", &now.to_string());
            target = u.to_string();
        }
        targets.push(target);
    }
    let (sub, usage, models) = tokio::join!(
        balance_get(Method::GET, &targets[0], &headers, None, Duration::from_secs(10)),
        balance_get(Method::GET, &targets[1], &headers, None, Duration::from_secs(10)),
        balance_get(Method::GET, &targets[2], &headers, None, Duration::from_secs(10)),
    );
    if !models.ok && !sub.ok && !usage.ok {
        return Balance {
            status: if models.status == 401 {
                "invalid"
            } else {
                "error"
            }
            .into(),
            fields: Vec::new(),
        };
    }
    let mut fields = Vec::new();
    if sub.ok {
        let plan = sub.data.get("plan").and_then(|v| v.as_object());
        fields.push(BalanceField {
            label: kit::tr("Plan", "套餐").into(),
            value: js_string(plan.and_then(|p| p.get("title"))),
        });
        fields.push(BalanceField {
            label: kit::tr("Hard limit", "硬上限").into(),
            value: js_string(sub.data.get("hard_limit_usd")),
        });
        fields.push(BalanceField {
            label: kit::tr("Soft limit", "软上限").into(),
            value: js_string(sub.data.get("soft_limit_usd")),
        });
        fields.push(BalanceField {
            label: kit::tr("System limit", "系统上限").into(),
            value: js_string(sub.data.get("system_hard_limit_usd")),
        });
    } else {
        fields.push(BalanceField {
            label: kit::tr("Billing", "账单").into(),
            value: kit::tr(
                "Unavailable (may be a platform key)",
                "可能是 platform Key，看不到",
            )
            .into(),
        });
    }
    if let Some(secs) = sub.data.get("access_until").and_then(|v| v.as_f64())
        && let Some(dt) = chrono::DateTime::from_timestamp(secs as i64, 0)
    {
        fields.push(BalanceField {
            label: kit::tr("Expires", "有效期").into(),
            value: dt.format("%Y-%m-%d").to_string(),
        });
    }
    if sub.data.contains_key("has_payment_method") {
        fields.push(BalanceField {
            label: kit::tr("Payment method", "支付方式").into(),
            value: yes_no_text(js_truthy(sub.data.get("has_payment_method"))).into(),
        });
    }
    if usage.ok {
        let total = usage.data.get("total_usage").and_then(|v| v.as_f64()).unwrap_or(0.0);
        fields.push(BalanceField {
            label: kit::tr("Last 90 days", "近 90 天").into(),
            value: format!("${:.4}", total / 100.0),
        });
    }
    if models.ok
        && let Some(rows) = models.data.get("data").and_then(|v| v.as_array())
    {
        let mut owners = Vec::new();
        let mut tier = String::new();
        for row in rows.iter().take(50) {
            if let Some(owner) = row.get("owned_by").and_then(|v| v.as_str())
                && !owners.iter().any(|o| o == owner)
            {
                owners.push(owner.to_owned());
            }
            if let Some(t) = row.get("max_tier").and_then(|v| v.as_str()) {
                tier = t.to_owned();
            }
        }
        owners.truncate(3);
        if !owners.is_empty() {
            fields.push(BalanceField {
                label: "Org".into(),
                value: owners.join(", "),
            });
        }
        if !tier.is_empty() {
            fields.push(BalanceField {
                label: "Tier".into(),
                value: tier,
            });
        }
    }
    Balance {
        status: "ok".into(),
        fields,
    }
}

async fn anthropic_balance(provider: &AiProvider, model: &str) -> Balance {
    if model.is_empty() {
        return Balance {
            status: "unsupported".into(),
            fields: vec![BalanceField {
                label: kit::tr("Balance", "余额").into(),
                value: kit::tr("Check on provider dashboard", "请到官网查看").into(),
            }],
        };
    }
    let body = json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let Ok(target) = provider_endpoint(&provider.url, "anthropic", "messages") else {
        return Balance {
            status: "error".into(),
            fields: Vec::new(),
        };
    };
    let reply = balance_get(
        Method::POST,
        &target,
        &balance_auth(provider, "anthropic"),
        Some(&body),
        Duration::from_secs(15),
    )
    .await;
    if reply.ok || reply.status == 429 {
        let state = if reply.status == 429 {
            kit::tr("rate-limited", "限流")
        } else {
            kit::tr("ok", "正常")
        };
        return Balance {
            status: "ok".into(),
            fields: vec![
                BalanceField {
                    label: "Key".into(),
                    value: if kit::is_zh() {
                        format!("有效（{state}）")
                    } else {
                        format!("Valid ({state})")
                    },
                },
                BalanceField {
                    label: kit::tr("Balance", "余额").into(),
                    value: kit::tr("Check on provider dashboard", "请到官网查看").into(),
                },
            ],
        };
    }
    Balance {
        status: if reply.status == 401 || reply.status == 403 {
            "invalid"
        } else {
            "error"
        }
        .into(),
        fields: Vec::new(),
    }
}

async fn provider_balance(provider: &AiProvider, kind: &str, model: &str) -> Balance {
    if kind == "openai" {
        return open_ai_balance(provider).await;
    }
    if kind == "anthropic" {
        return anthropic_balance(provider, model).await;
    }
    let origin = Url::parse(&provider.url)
        .ok()
        .and_then(|u| u.host_str().map(|h| format!("{}://{h}", u.scheme())))
        .unwrap_or_else(|| provider.url.clone());
    let mut headers = balance_auth(provider, kind);
    let target = match kind {
        "openrouter" => format!("{origin}/api/v1/auth/key"),
        "deepseek" => format!("{origin}/user/balance"),
        _ => {
            let protocol = resolve_provider_type(provider);
            let Ok(models) = provider_endpoint(&provider.url, &protocol, "models") else {
                return Balance {
                    status: "error".into(),
                    fields: Vec::new(),
                };
            };
            let (t, h, _) = provider_auth(&protocol, &provider.key, &models);
            headers = h;
            t
        }
    };
    let reply = balance_get(Method::GET, &target, &headers, None, Duration::from_secs(10)).await;
    if !reply.ok {
        return Balance {
            status: if reply.status == 401 || reply.status == 403 {
                "invalid"
            } else {
                "error"
            }
            .into(),
            fields: Vec::new(),
        };
    }
    let data = &reply.data;
    let mut fields = Vec::new();
    match kind {
        "openrouter" => {
            let root = data
                .get("data")
                .and_then(|v| v.as_object())
                .unwrap_or(data);
            let label = root.get("label").or_else(|| root.get("name"));
            fields.push(BalanceField {
                label: kit::tr("Label", "标签").into(),
                value: js_string(label),
            });
            fields.push(BalanceField {
                label: kit::tr("Balance", "余额").into(),
                value: js_string(root.get("credits")),
            });
            fields.push(BalanceField {
                label: kit::tr("Used", "已用").into(),
                value: js_string(root.get("usage")),
            });
            fields.push(BalanceField {
                label: kit::tr("Limit", "限额").into(),
                value: js_string(root.get("limit")),
            });
            if let Some(rate) = root.get("rate_limit").and_then(|v| v.as_object())
                && !rate.is_empty()
            {
                fields.push(BalanceField {
                    label: kit::tr("Rate limit", "速率").into(),
                    value: format!(
                        "{} req / {}",
                        js_string(rate.get("requests")),
                        js_string(rate.get("interval"))
                    ),
                });
            }
            if let Some(disabled) = root.get("disabled_providers").and_then(|v| v.as_array())
                && !disabled.is_empty()
            {
                fields.push(BalanceField {
                    label: kit::tr("Disabled", "禁用").into(),
                    value: disabled.len().to_string(),
                });
            }
        }
        "deepseek" => {
            fields.push(BalanceField {
                label: kit::tr("Available", "可用").into(),
                value: yes_no_text(js_truthy(data.get("is_available"))).into(),
            });
            if let Some(infos) = data.get("balance_infos").and_then(|v| v.as_array()) {
                for row in infos {
                    let cur = row.get("currency");
                    let label = if cur.is_some() && !cur.unwrap().is_null() {
                        js_string(cur)
                    } else {
                        kit::tr("Balance", "余额").into()
                    };
                    fields.push(BalanceField {
                        label,
                        value: js_string(row.get("total_balance")),
                    });
                }
            }
        }
        _ => {
            let count = data
                .get("data")
                .and_then(|v| v.as_array())
                .or_else(|| data.get("models").and_then(|v| v.as_array()))
                .map(|a| a.len())
                .unwrap_or(0);
            fields.push(BalanceField {
                label: kit::tr("Status", "状态").into(),
                value: kit::tr("Valid", "有效").into(),
            });
            fields.push(BalanceField {
                label: kit::tr("Models", "模型").into(),
                value: count.to_string(),
            });
        }
    }
    Balance {
        status: "ok".into(),
        fields,
    }
}

fn import_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]{1,64}$").unwrap())
}

const IMPORT_MODES: &[&str] = &["chat", "search", "image", "video"];

impl Service {
    pub fn providers(&self) -> Result<Vec<ProviderSummary>, CmdError> {
        let cfg = self.read()?;
        let mut tags: Vec<String> = cfg.configs.keys().cloned().collect();
        tags.sort();
        Ok(tags
            .into_iter()
            .map(|tag| {
                let p = &cfg.configs[&tag];
                ProviderSummary {
                    tag,
                    kind: if p.kind.is_empty() {
                        "auto".into()
                    } else {
                        p.kind.clone()
                    },
                    models: p.models.clone(),
                }
            })
            .collect())
    }

    pub async fn models(&self, tag: &str) -> Result<Vec<String>, CmdError> {
        let cfg = self.read()?;
        provider_models(&cfg, tag).await
    }

    pub async fn benchmark(&self, tag: &str) -> Result<(ProviderIdentity, Vec<Probe>), CmdError> {
        let cfg = self.read()?;
        let Some(provider) = cfg.configs.get(tag) else {
            return Err(kit::fail(kit::tr(
                "Specified AI provider not found",
                "未找到指定的 AI 提供商",
            )));
        };
        let identity = identify_provider(tag, provider);
        let models: Vec<String> = match benchmark_models_for(&identity.kind) {
            Some(list) => list.iter().map(|s| (*s).to_owned()).collect(),
            None => {
                let m = configured_chat_model(&cfg, tag);
                if !m.is_empty() {
                    vec![m]
                } else {
                    let listed = provider_models(&cfg, tag).await?;
                    listed.into_iter().take(1).collect()
                }
            }
        };
        let mut probes = Vec::with_capacity(models.len());
        for m in &models {
            probes.push(probe_model(&cfg, tag, m, 50).await);
        }
        Ok((identity, probes))
    }

    pub async fn diagnose(&self, tag: &str) -> Result<Diagnosis, CmdError> {
        let cfg = self.read()?;
        let Some(provider) = cfg.configs.get(tag) else {
            return Err(kit::fail(kit::tr(
                "Specified AI provider not found",
                "未找到指定的 AI 提供商",
            )));
        };
        let identity = identify_provider(tag, provider);
        let selected = configured_chat_model(&cfg, tag);
        let (balance, models_res) = tokio::join!(
            provider_balance(provider, &identity.kind, &selected),
            provider_models(&cfg, tag),
        );
        let (models, models_error) = match models_res {
            Ok(list) => (list, String::new()),
            Err(err) => (Vec::new(), check_failure(&err)),
        };
        let model = if !selected.is_empty() {
            selected
        } else {
            models.first().cloned().unwrap_or_default()
        };
        let chat = if !model.is_empty() {
            Some(probe_model(&cfg, tag, &model, 50).await)
        } else {
            None
        };
        Ok(Diagnosis {
            provider: identity,
            balance,
            models,
            models_error,
            chat,
        })
    }

    pub fn import_provider(&self, mut value: ProviderImport) -> Result<(String, bool), CmdError> {
        value.tag = value.tag.trim().to_owned();
        value.url = value.url.trim().to_owned();
        value.key = value.key.trim().to_owned();
        if !import_tag_re().is_match(&value.tag)
            || ["__proto__", "constructor", "prototype"].contains(&value.tag.as_str())
        {
            return Err(kit::fail(kit::tr("Invalid import tag", "导入标签无效")));
        }
        if value.url.len() > 2048 || value.key.is_empty() || value.key.len() > 8192 {
            return Err(kit::fail(kit::tr("Invalid import config", "导入配置无效")));
        }
        let parsed = Url::parse(&value.url)
            .map_err(|_| kit::fail(kit::tr("Invalid import URL", "导入地址无效")))?;
        if parsed.host_str().is_none()
            || (parsed.scheme() != "https" && parsed.scheme() != "http")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(kit::fail(kit::tr("Invalid import URL", "导入地址无效")));
        }
        if !value.kind.is_empty() && !AI_PROVIDER_TYPES.contains(&value.kind.as_str()) {
            return Err(kit::fail(kit::tr("Invalid import type", "导入类型无效")));
        }
        let mut clean_models = HashMap::new();
        for &mode in IMPORT_MODES {
            if let Some(m) = value.models.get(mode) {
                let t = m.trim();
                if !t.is_empty() {
                    assert_allowed_model(t)?;
                    clean_models.insert(mode.to_owned(), t.to_owned());
                }
            }
        }
        let same = |cur: &AiProvider| -> bool {
            cur.url == value.url
                && cur.key == value.key
                && cur.kind == value.kind
                && cur.stream == value.stream
                && cur.responses == value.responses
        };

        let mut actual = value.tag.clone();
        let mut imported = false;
        self.update(|cfg| {
            let mut existing = cfg.configs.get(&value.tag).cloned();
            let mut taken = existing.is_some();
            if let Some(ref cur) = existing
                && !same(cur)
            {
                actual.clear();
                taken = false;
                for index in 2..=999 {
                    let suffix = format!("-{index}");
                    let prefix_len = value.tag.len().min(64 - suffix.len());
                    let candidate = format!("{}{}", &value.tag[..prefix_len], suffix);
                    match cfg.configs.get(&candidate) {
                        None => {
                            actual = candidate;
                            existing = None;
                            taken = false;
                            break;
                        }
                        Some(used) if same(used) => {
                            actual = candidate;
                            existing = Some(used.clone());
                            taken = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if actual.is_empty() {
                    return Err(kit::fail(kit::tr(
                        "Exhausted AI import tags",
                        "AI 导入标签已用尽",
                    )));
                }
            }
            let mut merged = existing
                .as_ref()
                .map(|p| p.models.clone())
                .unwrap_or_default();
            for (k, v) in &clean_models {
                merged.entry(k.clone()).or_insert_with(|| v.clone());
            }
            if !taken {
                cfg.configs.insert(
                    actual.clone(),
                    AiProvider {
                        tag: actual.clone(),
                        url: value.url.clone(),
                        key: value.key.clone(),
                        kind: value.kind.clone(),
                        stream: value.stream,
                        responses: value.responses,
                        models: clean_models.clone(),
                    },
                );
                imported = true;
            } else if let Some(mut cur) = existing
                && merged.len() != cur.models.len()
            {
                cur.models = merged.clone();
                cfg.configs.insert(actual.clone(), cur);
            }
            for mode in &value.select {
                let Some(model) = merged.get(mode).cloned().filter(|m| !m.is_empty()) else {
                    continue;
                };
                let (tag_ref, model_ref) = match mode.as_str() {
                    "chat" => (&mut cfg.current_chat_tag, &mut cfg.current_chat_model),
                    "search" => (&mut cfg.current_search_tag, &mut cfg.current_search_model),
                    "image" => (&mut cfg.current_image_tag, &mut cfg.current_image_model),
                    "video" => (&mut cfg.current_video_tag, &mut cfg.current_video_model),
                    _ => continue,
                };
                if tag_ref.is_empty() || model_ref.is_empty() {
                    *tag_ref = actual.clone();
                    *model_ref = model;
                }
            }
            Ok(())
        })?;
        Ok((actual, imported))
    }
}

impl Service {
    pub fn checker(self: &Arc<Self>) -> Arc<dyn CheckApiBackend> {
        Arc::clone(self) as Arc<dyn CheckApiBackend>
    }
}

pub trait CheckApiBackend: Send + Sync {
    fn providers(&self) -> Result<Vec<ProviderSummary>, CmdError>;
    fn models<'a>(&'a self, tag: &'a str) -> mibot_bot::BoxFuture<'a, Result<Vec<String>, CmdError>>;
    fn chat<'a>(
        &'a self,
        req: super::ChatRequest,
    ) -> mibot_bot::BoxFuture<'a, Result<String, CmdError>>;
    fn benchmark<'a>(
        &'a self,
        tag: &'a str,
    ) -> mibot_bot::BoxFuture<'a, Result<(ProviderIdentity, Vec<Probe>), CmdError>>;
    fn diagnose<'a>(
        &'a self,
        tag: &'a str,
    ) -> mibot_bot::BoxFuture<'a, Result<Diagnosis, CmdError>>;
    fn import_provider(&self, value: ProviderImport) -> Result<(String, bool), CmdError>;
}

impl CheckApiBackend for Service {
    fn providers(&self) -> Result<Vec<ProviderSummary>, CmdError> {
        Service::providers(self)
    }
    fn models<'a>(
        &'a self,
        tag: &'a str,
    ) -> mibot_bot::BoxFuture<'a, Result<Vec<String>, CmdError>> {
        Box::pin(Service::models(self, tag))
    }
    fn chat<'a>(
        &'a self,
        req: super::ChatRequest,
    ) -> mibot_bot::BoxFuture<'a, Result<String, CmdError>> {
        Box::pin(Service::chat(self, req))
    }
    fn benchmark<'a>(
        &'a self,
        tag: &'a str,
    ) -> mibot_bot::BoxFuture<'a, Result<(ProviderIdentity, Vec<Probe>), CmdError>> {
        Box::pin(Service::benchmark(self, tag))
    }
    fn diagnose<'a>(
        &'a self,
        tag: &'a str,
    ) -> mibot_bot::BoxFuture<'a, Result<Diagnosis, CmdError>> {
        Box::pin(Service::diagnose(self, tag))
    }
    fn import_provider(&self, value: ProviderImport) -> Result<(String, bool), CmdError> {
        Service::import_provider(self, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ai_defaults;
    use crate::ai::service_for_test;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_http<F>(handler: F) -> String
    where
        F: Fn(&str, &str, &str, &str) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..pos]);
                            let cl = head
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    if k.eq_ignore_ascii_case("content-length") {
                                        v.trim().parse::<usize>().ok()
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(0);
                            if buf.len() >= pos + 4 + cl {
                                break;
                            }
                        }
                    }
                    let raw = String::from_utf8_lossy(&buf);
                    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
                    let mut lines = head.lines();
                    let req_line = lines.next().unwrap_or("");
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap_or("GET");
                    let target = parts.next().unwrap_or("/");
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let mut hdrs = String::new();
                    for l in lines {
                        hdrs.push_str(l);
                        hdrs.push('\n');
                    }
                    let (status, extra_headers, resp_body) = handler(method, path, query, &format!("{hdrs}\n{body}"));
                    let mut out = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                        resp_body.len()
                    );
                    for (k, v) in extra_headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    out.push_str(&resp_body);
                    let _ = sock.write_all(out.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn test_models_benchmark_diagnose_redirect_and_import() {
        let other = spawn_http(|_, _, _, _| (200, vec![], r#"{"data":[{"id":"x"}]}"#.into())).await;
        let other_clone = other.clone();
        let main = spawn_http(move |_, path, _query, body_and_hdrs| {
            if path.starts_with("/redir") {
                return (
                    307,
                    vec![("Location".into(), format!("{other_clone}{path}"))],
                    String::new(),
                );
            }
            if path == "/user/balance" {
                return (
                    200,
                    vec![],
                    r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"12.50"}]}"#.into(),
                );
            }
            if path == "/v1/models" {
                if !body_and_hdrs.to_ascii_lowercase().contains("authorization: bearer sk-good") {
                    return (
                        401,
                        vec![],
                        r#"{"error":{"message":"Incorrect API key provided: sk-bad"}}"#.into(),
                    );
                }
                return (
                    200,
                    vec![],
                    r#"{"data":[{"id":"b"},{"id":"a"},{"name":"c"},{"id":"a"},{}]}"#.into(),
                );
            }
            if path == "/v1/chat/completions" {
                if body_and_hdrs.contains(r#""broken""#) {
                    return (
                        404,
                        vec![],
                        r#"{"error":{"message":"model broken not found"}}"#.into(),
                    );
                }
                return (
                    200,
                    vec![("x-ratelimit-remaining-requests".into(), "499".into())],
                    r#"{"model":"m1-2026","choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}"#.into(),
                );
            }
            (404, vec![], String::new())
        })
        .await;

        let mut cfg = ai_defaults();
        cfg.configs.insert(
            "good".into(),
            AiProvider {
                tag: "good".into(),
                url: main.clone(),
                key: "sk-good".into(),
                kind: "openai-compatible".into(),
                models: HashMap::from([("chat".into(), "m1".into())]),
                ..Default::default()
            },
        );
        cfg.configs.insert(
            "bad".into(),
            AiProvider {
                tag: "bad".into(),
                url: format!("{main}/v1/chat/completions"),
                key: "sk-bad".into(),
                kind: "openai-compatible".into(),
                ..Default::default()
            },
        );
        cfg.configs.insert(
            "claude".into(),
            AiProvider {
                tag: "claude".into(),
                url: format!("{main}/redir"),
                key: "sk-ant-secret".into(),
                kind: "anthropic".into(),
                models: HashMap::from([("chat".into(), "claude".into())]),
                ..Default::default()
            },
        );
        let (s, _tmp) = service_for_test(cfg);
        let list = s.models("good").await.unwrap();
        assert_eq!(list, vec!["a", "b", "c"]);

        let bad_err = s.models("bad").await.unwrap_err().to_string();
        assert!(!bad_err.contains("sk-bad") && bad_err.contains("HTTP 401"));

        let (ident, probes) = s.benchmark("good").await.unwrap();
        assert_eq!(ident.display_name, "OpenAI-Compatible API");
        assert!(probes[0].ok && probes[0].model == "m1-2026" && probes[0].usage.as_ref().unwrap().total == 6);
        assert_eq!(
            probes[0].rate_limits,
            vec![RateLimit {
                name: "x-ratelimit-remaining-requests".into(),
                value: "499".into()
            }]
        );

        let diag = s.diagnose("good").await.unwrap();
        assert_eq!(diag.models, vec!["a", "b", "c"]);
        assert!(diag.chat.as_ref().unwrap().ok);

        let redir_err = s.models("claude").await.unwrap_err().to_string();
        assert!(redir_err.contains("rejected to protect your key"));

        // Import tests
        let (tag1, imp1) = s
            .import_provider(ProviderImport {
                tag: "good".into(),
                url: "https://other.example/v1".into(),
                key: "k2".into(),
                kind: "openai-compatible".into(),
                models: HashMap::from([("chat".into(), "gpt-4o-mini".into())]),
                select: vec!["chat".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!((tag1.as_str(), imp1), ("good-2", true));
    }
}
