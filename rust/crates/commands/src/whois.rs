use crate::kit;
use chrono::{DateTime, Utc};
use mibot_app::App;
use mibot_command::{CmdError, CmdResult, Command, Invocation, PAGE_LIMIT, code, escape, escaped_pages, group};
use mibot_httpx::{HttpxError, Request, do_request};
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_BATCH: usize = 10;
const WHOIS_URL: &str = "https://namebeta.com/api/search/check";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct WhoisItem {
    domain: String,
    #[serde(rename = "rawData")]
    raw_data: String,
    #[serde(rename = "queryTime")]
    query_time: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct WhoisData {
    history: Vec<WhoisItem>,
    cache: HashMap<String, WhoisItem>,
}

fn domain_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)+[A-Za-z]{2,}$")
            .expect("valid whois domain regex")
    })
}

fn loose_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:https?://)?(?:www\.)?[a-z0-9][a-z0-9.-]*\.[a-z]{2,}")
            .expect("valid loose domain regex")
    })
}

fn nameserver_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?im)^(?:Name Server|nserver|NS):[ \t]*(.+)$").expect("valid nserver regex"))
}

fn scheme_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^https?://").expect("valid scheme regex"))
}

fn www_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^www\.").expect("valid www regex"))
}

fn normalize_domain(raw: &str) -> Option<String> {
    let val = scheme_re().replace(raw.trim(), "");
    let val = www_re().replace(&val, "");
    let host = val.split('/').next().unwrap_or("").to_ascii_lowercase();
    if host.len() > 253 || !domain_re().is_match(&host) {
        return None;
    }
    Some(host)
}

fn extract_whois(raw: &str) -> String {
    #[derive(Default, Deserialize)]
    struct Inner {
        #[serde(default)]
        whois: String,
    }
    #[derive(Default, Deserialize)]
    struct Data {
        #[serde(default)]
        whois: Inner,
    }
    #[derive(Deserialize)]
    struct Ev {
        #[serde(default)]
        data: Data,
    }
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("data:")
            && let Ok(ev) = serde_json::from_str::<Ev>(rest.trim())
            && !ev.data.whois.whois.is_empty()
        {
            return ev.data.whois.whois;
        }
    }
    String::new()
}

fn whois_field(raw: &str, name: &str) -> String {
    let pat = format!(r"(?im)^{}:[ \t]*(.*)$", regex::escape(name));
    let Ok(re) = Regex::new(&pat) else {
        return String::new();
    };
    re.captures(raw)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_owned())
        .unwrap_or_default()
}

fn whois_report(domain: &str, raw: &str) -> Vec<String> {
    let zh = kit::is_zh();
    let mut expiry = whois_field(raw, "Registry Expiry Date");
    if expiry.is_empty() {
        expiry = whois_field(raw, "Registrar Registration Expiration Date");
    }
    let mut lines = vec![format!("{}\n{domain}", kit::tr("WHOIS Result", "WHOIS 结果"))];
    for (label, val) in [
        (kit::tr("Registrar", "注册商"), whois_field(raw, "Registrar")),
        (kit::tr("Created", "注册日期"), whois_field(raw, "Creation Date")),
        (kit::tr("Updated", "更新日期"), whois_field(raw, "Updated Date")),
        (kit::tr("Expires", "到期日期"), expiry.clone()),
        (kit::tr("Status", "域名状态"), whois_field(raw, "Domain Status")),
    ] {
        if !val.is_empty() {
            lines.push(format!("{label}: {val}"));
        }
    }
    if !expiry.is_empty()
        && let Ok(when) = DateTime::parse_from_rfc3339(expiry.trim())
    {
        let remaining = when.with_timezone(&Utc) - Utc::now();
        if remaining < chrono::Duration::zero() {
            lines.push(kit::tr("Expiry Notice: Expired", "到期提醒：已过期").into());
        } else if remaining < chrono::Duration::days(90) {
            lines.push(if zh {
                format!("到期提醒：{} 天后过期", remaining.num_days())
            } else {
                format!("Expiry Notice: Expires in {} days", remaining.num_days())
            });
        }
    }
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    for caps in nameserver_re().captures_iter(raw) {
        if let Some(m) = caps.get(1) {
            let v = m.as_str().trim().to_owned();
            if seen.insert(v.clone()) {
                ordered.push(v);
            }
        }
    }
    if !ordered.is_empty() {
        lines.push(if zh {
            format!("DNS 服务器：\n{}", ordered.join("\n"))
        } else {
            format!("Name Servers:\n{}", ordered.join("\n"))
        });
    }
    let mut pages = Vec::new();
    for p in escaped_pages(&lines.join("\n"), PAGE_LIMIT) {
        pages.push(format!("<pre>{p}</pre>"));
    }
    for p in escaped_pages(raw, PAGE_LIMIT) {
        pages.push(format!("<blockquote expandable>{p}</blockquote>"));
    }
    pages
}

async fn whois_query(
    cancel: &CancellationToken,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    endpoint: &str,
    name: &str,
    data: &Store<WhoisData>,
) -> Result<String, CmdError> {
    let current = data.read()?;
    if let Some(cached) = current.cache.get(name)
        && let Ok(when) = DateTime::parse_from_rfc3339(&cached.query_time)
        && (Utc::now() - when.with_timezone(&Utc)) < chrono::Duration::hours(24)
    {
        return Ok(cached.raw_data.clone());
    }
    let q: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
    let mut headers = HashMap::new();
    headers.insert("User-Agent".into(), "Mi Box".into());
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(kit::fail(kit::tr("Cancelled", "操作已取消"))),
        r = do_request(Request {
            url: format!("{endpoint}?query={q}"),
            headers,
            timeout: Some(Duration::from_secs(15)),
            max_bytes: Some(4 << 20),
            ..Default::default()
        }) => r?,
    };
    if !resp.ok() {
        return Err(Box::new(HttpxError::Status(resp.status)));
    }
    let result = extract_whois(&String::from_utf8_lossy(&resp.body));
    if result.is_empty() {
        return Ok(String::new());
    }
    let item = WhoisItem {
        domain: name.to_owned(),
        raw_data: result.clone(),
        query_time: Utc::now().to_rfc3339(),
    };
    if let Err(err) = kit::update_ok(data, move |d| {
        d.history.insert(0, item.clone());
        if d.history.len() > 100 {
            d.history.truncate(100);
        }
        d.cache.insert(item.domain.clone(), item);
    }) {
        kit::opt_log(
            logs,
            mibot_logtail::Level::Warn,
            "whois.record_failed",
            &[("error", &err.to_string())],
        );
    }
    Ok(result)
}

fn whois_help(prefix: &str) -> String {
    if kit::is_zh() {
        format!(
            "🔍 <b>WHOIS 域名查询</b>\n\n• {}\n• {} 回复含域名的消息\n• {} 批量查询，最多 {MAX_BATCH} 个\n• {} 查看历史\n• {} 清除历史\n查询结果缓存 24 小时。",
            code(&format!("{prefix}whois example.com")),
            code(&format!("{prefix}whois")),
            code(&format!("{prefix}whois batch a.com b.com")),
            code(&format!("{prefix}whois history")),
            code(&format!("{prefix}whois clear")),
        )
    } else {
        format!(
            "🔍 <b>WHOIS Lookup</b>\n\n• {}\n• {} Reply to a message containing a domain\n• {} Batch lookup (up to {MAX_BATCH} domains)\n• {} View history\n• {} Clear history\nResults are cached for 24 hours.",
            code(&format!("{prefix}whois example.com")),
            code(&format!("{prefix}whois")),
            code(&format!("{prefix}whois batch a.com b.com")),
            code(&format!("{prefix}whois history")),
            code(&format!("{prefix}whois clear")),
        )
    }
}

async fn whois_batch(
    inv: &Invocation,
    logs: &Arc<mibot_logtail::Ring>,
    data: &Store<WhoisData>,
    inputs: &[String],
) -> CmdResult {
    if inputs.is_empty() {
        return inv.edit(&whois_help(inv.prefix())).await.map_err(Into::into);
    }
    if inputs.len() > MAX_BATCH {
        return inv
            .edit_text(&if kit::is_zh() {
                format!("批量查询最多支持 {MAX_BATCH} 个域名")
            } else {
                format!("Batch lookup supports up to {MAX_BATCH} domains")
            })
            .await
            .map_err(Into::into);
    }
    inv.edit_text(&kit::working(&if kit::is_zh() {
        format!("正在批量查询 {} 个域名", inputs.len())
    } else {
        format!("Looking up {} domains", inputs.len())
    }))
    .await?;
    let mut results = Vec::with_capacity(inputs.len());
    for input in inputs {
        if inv.cancel.is_cancelled() {
            return Err(kit::fail(kit::tr("Cancelled", "操作已取消")));
        }
        let Some(name) = normalize_domain(input) else {
            results.push(if kit::is_zh() {
                format!("❌ {}：格式无效", escape(input))
            } else {
                format!("❌ {}: invalid format", escape(input))
            });
            continue;
        };
        let res = whois_query(&inv.cancel, Some(logs), WHOIS_URL, &name, data).await;
        if inv.cancel.is_cancelled() {
            return Err(kit::fail(kit::tr("Cancelled", "操作已取消")));
        }
        match res {
            Ok(s) if !s.is_empty() => results.push(format!("✅ {}", code(&name))),
            _ => results.push(if kit::is_zh() {
                format!("❌ {}：查询失败", code(&name))
            } else {
                format!("❌ {}: lookup failed", code(&name))
            }),
        }
    }
    inv.edit(&if kit::is_zh() {
        format!("🔍 <b>WHOIS 批量查询</b>\n\n{}", results.join("\n"))
    } else {
        format!("🔍 <b>WHOIS Batch Lookup</b>\n\n{}", results.join("\n"))
    })
    .await?;
    Ok(())
}

async fn handle(inv: &Invocation, app: &App, data: &Store<WhoisData>) -> CmdResult {
    let mut raw = inv.arg(0).to_owned();
    let lower = raw.to_ascii_lowercase();
    if lower == "batch" {
        return whois_batch(inv, &app.logs, data, &inv.args[1..]).await;
    }
    if raw.is_empty() {
        if let Some(reply) = kit::reply_msg(inv).await?
            && let Some(m) = loose_re().find(&reply.content())
        {
            raw = m.as_str().to_owned();
        }
        if raw.is_empty() {
            return inv.edit(&whois_help(inv.prefix())).await.map_err(Into::into);
        }
    }
    let lower = raw.to_ascii_lowercase();
    if lower == "clear" {
        let mut counts = (0usize, 0usize);
        if let Err(err) = kit::update_ok(data, |d| {
            counts = (d.history.len(), d.cache.len());
            d.history.clear();
            d.cache.clear();
        }) {
            kit::opt_log(Some(&app.logs), mibot_logtail::Level::Warn, "whois.clear_failed", &[("error", &err.to_string())]);
            return Err(kit::fail(kit::tr(
                "Failed to clear; history and cache remain",
                "清除失败，历史和缓存都还在",
            )));
        }
        return inv
            .edit_text(&if kit::is_zh() {
                format!("已清除历史 {} 条、缓存 {} 个域名", counts.0, counts.1)
            } else {
                format!("Cleared {} history entries and {} cached domains", counts.0, counts.1)
            })
            .await
            .map_err(Into::into);
    }
    if lower == "history" {
        let current = data.read().map_err(|e| {
            kit::opt_log(Some(&app.logs), mibot_logtail::Level::Warn, "whois.history_read_failed", &[("error", &e.to_string())]);
            kit::fail(kit::tr("Failed to read query history", "读不到查询历史"))
        })?;
        let mut rows = Vec::new();
        for (idx, item) in current.history.iter().take(20).enumerate() {
            let mut stamp = item.query_time.as_str();
            let short = if stamp.len() >= 16 && stamp.is_char_boundary(5) && stamp.is_char_boundary(16) {
                stamp = &stamp[5..16];
                stamp.replacen('T', " ", 1)
            } else {
                stamp.to_owned()
            };
            rows.push(format!("{}. {} <i>{}</i>", idx + 1, code(&item.domain), escape(&short)));
        }
        let body = if rows.is_empty() {
            kit::tr("No query history yet", "暂无查询历史").to_owned()
        } else {
            rows.join("\n")
        };
        return inv
            .edit(&if kit::is_zh() {
                format!(
                    "🔍 <b>WHOIS 查询历史</b>\n\n{body}\n\n共 {} 条，缓存 {} 个",
                    current.history.len(),
                    current.cache.len()
                )
            } else {
                format!(
                    "🔍 <b>WHOIS History</b>\n\n{body}\n\nTotal {} entries, {} cached",
                    current.history.len(),
                    current.cache.len()
                )
            })
            .await
            .map_err(Into::into);
    }
    let name = normalize_domain(&raw).ok_or_else(|| {
        kit::fail(kit::tr(
            "Enter a valid domain, e.g. example.com",
            "请输入有效域名，例如 example.com",
        ))
    })?;
    inv.edit_text(&kit::working(&if kit::is_zh() {
        format!("正在查询 {name}")
    } else {
        format!("Looking up {name}")
    }))
    .await?;
    let result = whois_query(&inv.cancel, Some(&app.logs), WHOIS_URL, &name, data)
        .await
        .map_err(|e| {
            kit::fail_with(
                kit::tr("WHOIS lookup failed; try again later", "WHOIS 查询失败，稍后再试"),
                mibot_command::UserError::new(e.to_string()),
            )
        })?;
    if result.is_empty() {
        return inv
            .edit_text(kit::tr("No WHOIS data returned", "未取得 WHOIS 数据"))
            .await
            .map_err(Into::into);
    }
    inv.respond_pages(&whois_report(&name, &result)).await?;
    Ok(())
}

pub fn register(app: &Arc<App>) {
    let data = Arc::new(kit::new_store(app, "whois.json", WhoisData::default));
    let app_ref = Arc::clone(app);
    app.registry.register(
        Command {
            name: "whois".into(),
            group: group::TOOLS.into(),
            description: "Query domain WHOIS info".into(),
            usage: "[domain|batch domains…|history|clear]".into(),
            help_fn: Some(Arc::new(whois_help)),
            timeout: Some(Duration::from_secs(180)),
            handle: Arc::new(move |inv| {
                let a = Arc::clone(&app_ref);
                let d = Arc::clone(&data);
                Box::pin(async move { handle(&inv, &a, &d).await })
            }),
            ..Default::default()
        }
        .zh("查询域名注册信息", "[域名|batch 域名…|history|clear]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_normalize_and_report() {
        for (input, want) in [
            ("https://www.Example.com/path", "example.com"),
            ("github.com", "github.com"),
            ("a.b.co.uk", "a.b.co.uk"),
        ] {
            assert_eq!(normalize_domain(input).as_deref(), Some(want));
        }
        let too_long = format!("{}.com", "a".repeat(260));
        for bad in ["", "not a domain", "localhost", "-bad.com", &too_long] {
            assert!(normalize_domain(bad).is_none());
        }

        let expired = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let pages = whois_report("example.com", &format!("Registry Expiry Date: {expired}"));
        assert!(pages[0].contains("Expired"));

        let raw = "Domain Name: EXAMPLE.COM\nRegistrar: Example Registrar\nCreation Date: 1995-08-14T04:00:00Z\nRegistry Expiry Date: 2030-08-13T04:00:00Z\nName Server: A.IANA-SERVERS.NET\nName Server: A.IANA-SERVERS.NET\nName Server: B.IANA-SERVERS.NET\n";
        let pages = whois_report("example.com", raw);
        let head = &pages[0];
        for want in ["Example Registrar", "1995-08-14", "A.IANA-SERVERS.NET", "B.IANA-SERVERS.NET"] {
            assert!(head.contains(want));
        }
        assert_eq!(head.matches("A.IANA-SERVERS.NET").count(), 1);
        assert!(head.starts_with("<pre>") && pages.last().unwrap().ends_with("</blockquote>"));
    }

    #[tokio::test]
    async fn test_whois_query_status() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-whois-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::new(dir.join("whois.json"), WhoisData::default);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen_q = Arc::new(std::sync::Mutex::new(String::new()));
        let q_ref = Arc::clone(&seen_q);
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_owned();
                *q_ref.lock().unwrap() = line;
                let _ = s.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            }
        });
        let err = whois_query(&CancellationToken::new(), None, &format!("http://{addr}"), "example.com", &store)
            .await
            .unwrap_err();
        assert!(matches!(err.downcast_ref::<HttpxError>(), Some(HttpxError::Status(429))));
        assert!(seen_q.lock().unwrap().contains("query=example.com"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
