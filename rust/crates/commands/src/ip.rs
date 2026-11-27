use crate::kit;
use mibot_app::App;
use mibot_command::{CmdResult, Invocation, code, escape, group};
use mibot_httpx::{Request, do_request};
use regex::Regex;
use serde::Deserialize;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

fn ip_api_url(encoded: &str) -> String {
    let lang = kit::tr("en", "zh-CN");
    format!(
        "http://ip-api.com/json/{encoded}?lang={lang}&fields=status,message,country,regionName,city,isp,org,as,query,timezone,proxy,hosting"
    )
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct IpResult {
    status: String,
    message: String,
    country: String,
    #[serde(rename = "regionName")]
    region_name: String,
    city: String,
    isp: String,
    org: String,
    #[serde(rename = "as")]
    as_field: String,
    query: String,
    timezone: String,
    proxy: bool,
    hosting: bool,
}

fn ip_candidate_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:\b[a-f\d]{1,4}:|::)[a-f\d:.]*|\b(?:\d{1,3}\.){3}\d{1,3}\b")
            .expect("valid ip candidate regex")
    })
}

fn domain_in_text_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)+[A-Za-z]{2,}\b")
            .expect("valid domain regex")
    })
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_address(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut ipv6 = String::new();
    for m in ip_candidate_re().find_iter(text) {
        let start = m.start();
        let end = m.end();
        if (start > 0 && (is_word_byte(bytes[start - 1]) || bytes[start - 1] == b'.'))
            || (end < bytes.len() && is_word_byte(bytes[end]))
        {
            continue;
        }
        let candidate = m.as_str().trim_end_matches('.');
        if let Ok(addr) = candidate.parse::<IpAddr>() {
            if addr.is_ipv4() {
                return candidate.to_owned();
            }
            if ipv6.is_empty() {
                ipv6 = candidate.to_owned();
            }
        }
    }
    if !ipv6.is_empty() {
        return ipv6;
    }
    domain_in_text_re()
        .find(text)
        .map(|m| m.as_str().to_owned())
        .unwrap_or_default()
}

fn reply_query(text: &str) -> String {
    let found = find_address(text);
    if !found.is_empty() {
        return found;
    }
    text.split_whitespace()
        .next()
        .map(str::to_owned)
        .unwrap_or_default()
}

fn pct_encode_path(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn render_ip(result: &IpResult) -> String {
    let zh = kit::is_zh();
    let mut lines = Vec::new();
    if result.proxy {
        lines.push(kit::tr("⚠️ Possible proxy IP", "⚠️ 可能是代理 IP").to_owned());
    }
    if result.hosting {
        lines.push(kit::tr("🖥 Possible datacenter IP", "🖥 可能是数据中心 IP").to_owned());
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    let place = [result.country.as_str(), result.region_name.as_str(), result.city.as_str()]
        .into_iter()
        .filter(|v| !v.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    if zh {
        lines.push("🌍 <b>IP 查询</b>".to_owned());
        lines.push(String::new());
        lines.push(format!("🔍 地址：{}", code(&result.query)));
        lines.push(format!("📍 位置：{}", escape(kit::or_dash(&place))));
        lines.push(format!("🏢 ISP：{}", escape(kit::or_dash(&result.isp))));
        lines.push(format!("🏦 组织：{}", escape(kit::or_dash(&result.org))));
        lines.push(format!("🔢 AS：{}", code(kit::or_dash(&result.as_field))));
        if !result.timezone.is_empty() {
            lines.push(format!("⏰ 时区：{}", escape(&result.timezone)));
        }
    } else {
        lines.push("🌍 <b>IP Lookup</b>".to_owned());
        lines.push(String::new());
        lines.push(format!("🔍 Address: {}", code(&result.query)));
        lines.push(format!("📍 Location: {}", escape(kit::or_dash(&place))));
        lines.push(format!("🏢 ISP: {}", escape(kit::or_dash(&result.isp))));
        lines.push(format!("🏦 Org: {}", escape(kit::or_dash(&result.org))));
        lines.push(format!("🔢 AS: {}", code(kit::or_dash(&result.as_field))));
        if !result.timezone.is_empty() {
            lines.push(format!("⏰ Timezone: {}", escape(&result.timezone)));
        }
    }
    if let Some(rest) = result.as_field.strip_prefix("AS")
        && let Some((number, _)) = rest.split_once(' ')
    {
        let esc = escape(number);
        lines.push(String::new());
        lines.push(if zh {
            format!("<a href=\"https://bgp.he.net/AS{esc}\">在 bgp.he.net 查看 AS{esc}</a>")
        } else {
            format!("<a href=\"https://bgp.he.net/AS{esc}\">View AS{esc} on bgp.he.net</a>")
        });
    }
    lines.join("\n")
}

fn ip_failure(message: &str) -> &'static str {
    match message.trim().to_ascii_lowercase().as_str() {
        "private range" => kit::tr("Private IP range", "这是内网地址"),
        "reserved range" => kit::tr("Reserved IP range", "这是保留地址"),
        "invalid query" => kit::tr("Invalid IP or domain", "不是有效的 IP 或域名"),
        _ => kit::tr("No record found", "没有这个地址的记录"),
    }
}

fn ip_help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "🌍 <b>IP 查询</b>\n\n查 IP 或域名的地理位置、运营商和 AS 号。\n\n\
             • <code>{p}ip 8.8.8.8</code>\n\
             • <code>{p}ip example.com</code>\n\
             • 回复一条含 IP、域名或链接的消息发 <code>{p}ip</code>\n\n数据来自 ip-api.com。"
        )
    } else {
        format!(
            "🌍 <b>IP Lookup</b>\n\nLook up geolocation, ISP, and ASN for an IP or domain.\n\n\
             • <code>{p}ip 8.8.8.8</code>\n\
             • <code>{p}ip example.com</code>\n\
             • Reply to a message containing an IP, domain, or URL with <code>{p}ip</code>\n\nPowered by ip-api.com."
        )
    }
}

async fn handle(inv: &Invocation) -> CmdResult {
    let mut query = inv.raw_after(0).trim().to_owned();
    if query.is_empty()
        && inv.message.reply_to_id != 0
        && let Some(reply) = kit::reply_msg(inv).await?
    {
        query = reply_query(&reply.content());
    }
    if query.is_empty() {
        return inv.edit(&ip_help(inv.prefix())).await.map_err(Into::into);
    }
    let found = find_address(&query);
    if !found.is_empty() {
        query = found;
    }
    inv.edit_text(&kit::working(&if kit::is_zh() {
        format!("正在查询 {query}")
    } else {
        format!("Looking up {query}")
    }))
    .await?;
    let endpoint = ip_api_url(&pct_encode_path(&query));
    let resp = do_request(Request {
        url: endpoint,
        timeout: Some(Duration::from_secs(15)),
        max_bytes: Some(64 << 10),
        ..Default::default()
    })
    .await
    .map_err(|e| {
        kit::fail_with(
            kit::tr(
                "Lookup service unreachable; try again later",
                "查询服务暂时连不上，稍后再试",
            ),
            e,
        )
    })?;
    if !resp.ok() {
        return Err(kit::fail(if kit::is_zh() {
            format!("查询服务出错了（HTTP {}），稍后再试", resp.status)
        } else {
            format!("Lookup service error (HTTP {}); try again later", resp.status)
        }));
    }
    let result: IpResult = serde_json::from_slice(&resp.body).map_err(|_| {
        kit::fail(kit::tr(
            "Lookup service returned invalid response",
            "查询服务返回了无法解析的内容",
        ))
    })?;
    if result.status != "success" {
        return Err(kit::fail(if kit::is_zh() {
            format!("查不到 {query}：{}", ip_failure(&result.message))
        } else {
            format!("Lookup failed for {query}: {}", ip_failure(&result.message))
        }));
    }
    inv.edit(&render_ip(&result)).await?;
    Ok(())
}

pub fn register(app: &Arc<App>) {
    app.registry.register(
        kit::make_cmd(
            "ip",
            &[],
            group::TOOLS,
            "Look up IP or domain info",
            "[IP|domain]",
            ip_help,
            |inv| Box::pin(async move { handle(&inv).await }),
        )
        .zh("查 IP 或域名的位置与运营商", "[IP|域名]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_address_and_reply_query_and_render() {
        let addr_cases = [
            ("连不上 8.8.8.8 了", "8.8.8.8"),
            ("看这个 https://example.com/a?b=1", "example.com"),
            ("example.org 挂了吗", "example.org"),
            ("2001:4860:4860::8888", "2001:4860:4860::8888"),
            ("连接 ::1 失败", "::1"),
            ("IP:8.8.8.8", "8.8.8.8"),
            ("IPv6:2001:4860::8888", "2001:4860::8888"),
            ("连接 2606:4700:: 失败", "2606:4700::"),
            ("https://[2001:4860::8888]:443", "2001:4860::8888"),
            ("地址 ::ffff:8.8.8.8.", "::ffff:8.8.8.8"),
            ("999.1.1.1 之后是 8.8.8.8", "8.8.8.8"),
            ("2606:4700:: 然后是 1.1.1.1", "1.1.1.1"),
            ("std::vector 在 example.com", "example.com"),
            ("example.com 解析到 1.1.1.1", "1.1.1.1"),
            ("https://example.net/ 和 9.9.9.9", "9.9.9.9"),
            ("没有地址", ""),
        ];
        for (text, want) in addr_cases {
            assert_eq!(find_address(text), want, "text={text}");
        }

        for (text, want) in [
            ("看看 example.org", "example.org"),
            ("localhost 挂了", "localhost"),
            ("  \n", ""),
        ] {
            assert_eq!(reply_query(text), want, "text={text}");
        }

        let raw = r#"{"status":"success","country":"美国","regionName":"弗吉尼亚州","city":"Ashburn","timezone":"America/New_York","isp":"Google LLC","org":"Google Public DNS","as":"AS15169 Google LLC","proxy":false,"hosting":true,"query":"8.8.8.8"}"#;
        let result: IpResult = serde_json::from_str(raw).unwrap();
        let text = render_ip(&result);
        for wanted in [
            "8.8.8.8",
            "美国 · 弗吉尼亚州 · Ashburn",
            "Google LLC",
            "datacenter",
            "bgp.he.net/AS15169",
        ] {
            assert!(text.contains(wanted), "missing {wanted} in {text}");
        }
        assert!(!text.contains("proxy"));
    }
}
