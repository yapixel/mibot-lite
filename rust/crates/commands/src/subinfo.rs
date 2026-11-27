use crate::kit;
use chrono::{DateTime, Utc};
use mibot_app::App;
use mibot_bot::{MediaOptions, parse_html, utf16_len};
use mibot_command::{
    CmdError, CmdResult, Invocation, PAGE_LIMIT, code, escape, group, html_pages, truncate,
};
use mibot_httpx::{HttpxError, do_guarded_get, reason as httpx_reason};
#[cfg(test)]
use mibot_httpx::is_blocked_ip;
use mibot_sysinfo::format_bytes;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

const MAX_LINKS: usize = 20;
const LISTED_NODES: usize = 200;
const LISTED_PROTOCOLS: usize = 20;
const NAME_LIMIT: usize = 64;
const MAX_PAGES: usize = 5;
const MAPPINGS_URL: &str =
    "https://raw.githubusercontent.com/Hyy800/Quantumult-X/refs/heads/Nana/ymys.txt";
const MAPPINGS_TTL: Duration = Duration::from_secs(3600);
const MAX_EXPIRE: u64 = 8_640_000_000_000;

const PROTOCOLS: &[&str] = &[
    "vmess",
    "vless",
    "trojan",
    "ss",
    "ssr",
    "hysteria2",
    "hy2",
    "tuic",
    "socks",
    "socks5",
    "hysteria",
    "hy",
    "wireguard",
    "http",
    "https",
    "shadowtls",
    "naive",
];

struct RegionRule {
    name: &'static str,
    words: &'static [&'static str],
}

const REGIONS: &[RegionRule] = &[
    RegionRule { name: "香港", words: &["香港", "hong kong", "hongkong", "hkg", "hk"] },
    RegionRule { name: "台湾", words: &["台湾", "taiwan", "taipei", "tpe", "tw"] },
    RegionRule { name: "日本", words: &["日本", "japan", "osaka", "jap", "jp", "tokyo"] },
    RegionRule { name: "新加坡", words: &["新加坡", "singapore", "sgp", "sg"] },
    RegionRule { name: "韩国", words: &["韩国", "korea", "seoul", "kor", "kr"] },
    RegionRule { name: "印度", words: &["印度", "india"] },
    RegionRule { name: "马来西亚", words: &["马来西亚", "malaysia"] },
    RegionRule { name: "泰国", words: &["泰国", "thailand"] },
    RegionRule { name: "越南", words: &["越南", "vietnam"] },
    RegionRule { name: "印度尼西亚", words: &["印度尼西亚", "indonesia"] },
    RegionRule { name: "菲律宾", words: &["菲律宾", "philippines"] },
    RegionRule { name: "土耳其", words: &["土耳其", "turkey"] },
    RegionRule { name: "美国", words: &["美国", "united states", "usa", "us"] },
    RegionRule { name: "加拿大", words: &["加拿大", "canada", "ca"] },
    RegionRule { name: "英国", words: &["英国", "united kingdom", "uk", "london"] },
    RegionRule { name: "德国", words: &["德国", "germany", "de"] },
    RegionRule { name: "法国", words: &["法国", "france"] },
    RegionRule { name: "荷兰", words: &["荷兰", "netherlands"] },
    RegionRule { name: "瑞士", words: &["瑞士", "switzerland"] },
    RegionRule { name: "意大利", words: &["意大利", "italy"] },
    RegionRule { name: "西班牙", words: &["西班牙", "spain"] },
    RegionRule { name: "澳大利亚", words: &["澳大利亚", "australia", "au"] },
    RegionRule { name: "新西兰", words: &["新西兰", "new zealand"] },
    RegionRule { name: "巴西", words: &["巴西", "brazil"] },
    RegionRule { name: "阿联酋", words: &["阿联酋", "uae"] },
    RegionRule { name: "以色列", words: &["以色列", "israel"] },
    RegionRule { name: "南非", words: &["南非", "south africa"] },
    RegionRule { name: "俄罗斯", words: &["俄罗斯", "russia"] },
];

fn word_patterns() -> &'static HashMap<&'static str, Regex> {
    static MAP: OnceLock<HashMap<&'static str, Regex>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = HashMap::new();
        for rule in REGIONS {
            for &word in rule.words {
                if !word.is_empty() && word.bytes().all(|b| b.is_ascii_lowercase()) {
                    let pat = Regex::new(&format!(r"(?:^|[^a-z]){}(?:$|[^a-z])", regex::escape(word)))
                        .expect("valid region regex");
                    map.insert(word, pat);
                }
            }
        }
        map
    })
}

fn link_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i:https?)://[^\s"'<>\x{80}-\x{10FFFF}]+"#).expect("valid link regex")
    })
}

fn clash_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?m)^(?:(?:proxies|"proxies"|'proxies')[\s\p{Z}\x{FEFF}]*:|\{)"#)
            .expect("valid clash start regex")
    })
}

fn clash_type_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z][a-z0-9-]{0,31}$").expect("valid clash type regex"))
}

fn title_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?is)<title(?:\s[^>]*)?>(.*?)</title>").expect("valid title regex")
    })
}

fn space_run_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\s\p{Z}\x{FEFF}]+").expect("valid space run regex"))
}

fn login_prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^登录[\s\p{Z}]*[—|-][\s\p{Z}]*").expect("valid login prefix regex"))
}

fn login_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\s\p{Z}]*[|][\s\p{Z}]*登录$").expect("valid login suffix regex"))
}

fn userinfo_field_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(upload|download|total|expire|starttime)[\s\p{Z}]*=[\s\p{Z}]*(\d{1,20})$")
            .expect("valid userinfo field regex")
    })
}

fn tag_pattern_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[^>]+>").expect("valid tag pattern regex"))
}

fn unescape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.find(';') {
            let ent = &after[..semi];
            let decoded = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some('\u{00A0}'),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(ch) = decoded {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
fn public_address(addr: IpAddr) -> bool {
    !is_blocked_ip(addr)
}

fn region_of(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    let patterns = word_patterns();
    for rule in REGIONS {
        for &word in rule.words {
            if let Some(pat) = patterns.get(word) {
                if pat.is_match(&lower) {
                    return rule.name;
                }
            } else if lower.contains(word) {
                return rule.name;
            }
        }
    }
    ""
}

#[derive(Debug, Default, Clone)]
struct Tally {
    names: Vec<String>,
    counts: HashMap<String, usize>,
}

impl Tally {
    fn add(&mut self, name: &str) {
        self.add_n(name, 1);
    }

    fn add_n(&mut self, name: &str, n: usize) {
        let entry = self.counts.entry(name.to_owned()).or_insert_with(|| {
            self.names.push(name.to_owned());
            0
        });
        *entry += n;
    }

    fn lines(&self) -> Vec<String> {
        self.names
            .iter()
            .map(|name| format!("{name}：{}", self.counts[name]))
            .collect()
    }
}

#[derive(Debug, Default)]
struct Summary {
    names: Vec<String>,
    protocols: Tally,
    regions: Tally,
}

fn decode_base64(text: &str) -> Option<String> {
    use base64::Engine as _;
    let mut cleaned: String = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    while cleaned.ends_with('=') {
        cleaned.pop();
    }
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    let decoded = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(&cleaned)
        .ok()?;
    Some(String::from_utf8_lossy(&decoded).into_owned())
}

#[derive(Debug)]
struct ParseError(String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

fn parse_subscription(raw: &str) -> Result<Summary, ParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ParseError("订阅内容为空".into()));
    }
    let text = match decode_base64(trimmed) {
        Some(decoded) if decoded.contains("://") => decoded,
        _ => trimmed.to_owned(),
    };
    let mut result = Summary::default();
    let mut add = |kind: &str, name: String| {
        result.names.push(name);
        result.protocols.add(kind);
    };
    if clash_start_re().is_match(&text) {
        parse_clash(&text, &mut add)?;
    } else {
        let mut count = 0usize;
        for raw_line in text.split('\n') {
            let line = raw_line.trim();
            let lower = line.to_lowercase();
            for &kind in PROTOCOLS {
                let prefix = format!("{kind}://");
                if lower.starts_with(&prefix) {
                    let name = node_name(line, kind, count);
                    add(kind, name);
                    count += 1;
                    break;
                }
            }
        }
    }
    let mut other = 0usize;
    for name in &result.names {
        let found = region_of(name);
        if !found.is_empty() {
            result.regions.add(found);
        } else {
            other += 1;
        }
    }
    if other > 0 {
        result.regions.add_n("其他", other);
    }
    Ok(result)
}

fn parse_clash(text: &str, add: &mut dyn FnMut(&str, String)) -> Result<(), ParseError> {
    let document: Value = serde_saphyr::from_str(text)
        .map_err(|_| ParseError("订阅不是有效的 Clash 配置".into()))?;
    let proxies = document
        .as_object()
        .and_then(|obj| obj.get("proxies"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| ParseError("无效的 Clash 节点列表".into()))?;
    for (index, item) in proxies.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| ParseError("无效的 Clash 节点".into()))?;
        let raw_kind = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ParseError("无效的 Clash 节点".into()))?;
        if raw_kind.trim().is_empty() {
            return Err(ParseError("无效的 Clash 节点".into()));
        }
        let kind = raw_kind.to_lowercase();
        if !clash_type_re().is_match(&kind) {
            return Err(ParseError("无效的 Clash 协议".into()));
        }
        let name = match obj.get("name").and_then(|v| v.as_str()) {
            Some(n) if !n.trim().is_empty() => n.to_owned(),
            _ => format!("{} {}", kind.to_uppercase(), index + 1),
        };
        add(&kind, name);
    }
    Ok(())
}

fn percent_decode(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let h1 = (bytes[i + 1] as char).to_digit(16).ok_or(())?;
            let h2 = (bytes[i + 2] as char).to_digit(16).ok_or(())?;
            out.push(((h1 << 4) | h2) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn node_name(line: &str, kind: &str, index: usize) -> String {
    if let Some(hash) = line.find('#')
        && hash + 1 < line.len()
    {
        let fragment = &line[hash + 1..];
        return percent_decode(fragment).unwrap_or_else(|_| fragment.to_owned());
    }
    if let Some(scheme_end) = line.find("://") {
        let body = &line[scheme_end + 3..];
        match kind {
            "vmess" => {
                if let Some(decoded) = decode_base64(body)
                    && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&decoded)
                    && let Some(ps) = map.get("ps").and_then(|v| v.as_str())
                    && !ps.trim().is_empty()
                {
                    return ps.to_owned();
                }
            }
            "ssr" => {
                if let Some(decoded) = decode_base64(body)
                    && let Some(q_idx) = decoded.find("/?")
                {
                    for (k, v) in url::form_urlencoded::parse(&decoded.as_bytes()[q_idx + 2..]) {
                        if k == "remarks"
                            && !v.is_empty()
                            && let Some(name) = decode_base64(&v)
                            && !name.trim().is_empty()
                        {
                            return name;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    format!("{} {}", kind.to_uppercase(), index + 1)
}

fn traffic_summary(header: &str, present: bool, now: DateTime<Utc>) -> String {
    if !present || header.is_empty() {
        return "流量统计：未提供".into();
    }
    let mut fields: HashMap<String, u64> = HashMap::new();
    for part in header.split(';') {
        if let Some(caps) = userinfo_field_re().captures(part.trim())
            && let (Some(k), Some(v)) = (caps.get(1), caps.get(2))
            && let Ok(val) = v.as_str().parse::<u64>()
        {
            fields.insert(k.as_str().to_ascii_lowercase(), val);
        }
    }
    let upload = fields.get("upload").copied();
    let download = fields.get("download").copied();
    let total = fields.get("total").copied();
    let fmt_opt = |opt: Option<u64>| match opt {
        Some(v) => format_bytes(v),
        None => "未知".into(),
    };
    let mut lines = vec![
        format!("上传：{}", fmt_opt(upload)),
        format!("下载：{}", fmt_opt(download)),
    ];
    let used = upload.zip(download).map(|(u, d)| u.saturating_add(d));
    if let Some(u) = used {
        lines.push(format!("已用：{}", format_bytes(u)));
    }
    match total {
        None => lines.push("总量：未知".into()),
        Some(0) => lines.push("总量：未设限".into()),
        Some(t) => lines.push(format!("总量：{}", format_bytes(t))),
    }
    if let (Some(t), Some(u)) = (total.filter(|&t| t > 0), used) {
        let remaining = t.saturating_sub(u);
        lines.push(format!("剩余：{}", format_bytes(remaining)));
        if u >= t {
            lines.push("流量状态：已耗尽".into());
        }
    }
    match fields.get("expire").copied() {
        None => lines.push("到期：未提供".into()),
        Some(0) => lines.push("到期：未设期限".into()),
        Some(exp) if exp <= MAX_EXPIRE => {
            if let Some(at) = DateTime::<Utc>::from_timestamp(exp as i64, 0) {
                let mut line = format!("到期：{} UTC", at.format("%Y-%m-%d %H:%M"));
                if at <= now {
                    line.push_str("（已过期）");
                }
                lines.push(line);
            } else {
                lines.push("到期：无效时间".into());
            }
        }
        _ => lines.push("到期：无效时间".into()),
    }
    lines.join("\n")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Mapping {
    key: String,
    name: String,
}

fn parse_mappings(raw: &str) -> Vec<Mapping> {
    let mut list = Vec::new();
    for line in raw.split('\n') {
        let line = line.trim();
        let Some(at) = line.find('=') else { continue };
        if line.is_empty() || line.starts_with('#') || at == 0 {
            continue;
        }
        list.push(Mapping {
            key: line[..at].trim().to_owned(),
            name: line[at + 1..].trim().to_owned(),
        });
    }
    list
}

fn mapped_name<'a>(link: &str, list: &'a [Mapping]) -> &'a str {
    for item in list {
        if !item.key.is_empty() && link.contains(&item.key) {
            return &item.name;
        }
    }
    ""
}

fn header_name(value: &str) -> String {
    for raw in value.split(';') {
        let part = raw.trim();
        let lower = part.to_ascii_lowercase();
        let name = if lower.starts_with("filename*=") {
            part.split("''").last().unwrap_or("")
        } else if lower.starts_with("filename=") {
            let eq = part.find('=').expect("equals present");
            let mut s = part[eq + 1..].trim();
            if s.starts_with('"') || s.starts_with('\'') {
                s = &s[1..];
            }
            if s.ends_with('"') || s.ends_with('\'') {
                s = &s[..s.len() - 1];
            }
            s
        } else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        return percent_decode(name).unwrap_or_default();
    }
    String::new()
}

fn title_of(page: &str) -> String {
    let Some(caps) = title_re().captures(page) else {
        return String::new();
    };
    let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let unescaped = unescape_html(inner);
    let collapsed = space_run_re().replace_all(&unescaped, " ");
    let trimmed = collapsed.trim();
    let stripped_pre = login_prefix_re().replace(trimmed, "");
    login_suffix_re().replace(&stripped_pre, "").into_owned()
}

#[derive(Debug, Clone)]
struct FetchedSub {
    text: String,
    userinfo: String,
    has_userinfo: bool,
    content_disposition: String,
    profile_url: String,
}

#[derive(Clone)]
struct Site {
    website: String,
    name: String,
}

struct MappingCache {
    entries: Vec<Mapping>,
    fetched_at: Option<Instant>,
}

struct Fetcher {
    guard_ssrf: bool,
    mappings_url: String,
    cache: Mutex<MappingCache>,
}

impl Fetcher {
    fn new(guard_ssrf: bool, mappings_url: String) -> Self {
        Self {
            guard_ssrf,
            mappings_url,
            cache: Mutex::new(MappingCache {
                entries: Vec::new(),
                fetched_at: None,
            }),
        }
    }

    async fn get(
        &self,
        cancel: &CancellationToken,
        target: &str,
        headers: &HashMap<String, String>,
        limit: usize,
        timeout: Duration,
        allowed_host: Option<&str>,
    ) -> Result<mibot_httpx::GuardedResponse, HttpxError> {
        tokio::select! {
            _ = cancel.cancelled() => Err(HttpxError::UserFacing("操作已取消".into())),
            r = do_guarded_get(target, headers, timeout, limit, self.guard_ssrf, allowed_host, 5) => r,
        }
    }

    async fn fetch_subscription(
        &self,
        cancel: &CancellationToken,
        link: &Url,
    ) -> Result<FetchedSub, HttpxError> {
        let mut headers = HashMap::new();
        headers.insert("User-Agent".into(), "Mi Box".into());
        let resp = self
            .get(
                cancel,
                link.as_str(),
                &headers,
                2 << 20,
                Duration::from_secs(15),
                link.host_str().map(|s| s.trim_start_matches('[').trim_end_matches(']')),
            )
            .await?;
        if !resp.ok() {
            return Err(HttpxError::Status(resp.status));
        }
        let userinfo_vals = resp.header_values("subscription-userinfo");
        Ok(FetchedSub {
            text: String::from_utf8_lossy(&resp.body).into_owned(),
            has_userinfo: !userinfo_vals.is_empty(),
            userinfo: userinfo_vals.first().cloned().unwrap_or_default(),
            content_disposition: resp.header("content-disposition").to_owned(),
            profile_url: resp.header("profile-web-page-url").to_owned(),
        })
    }

    async fn site_info(&self, cancel: &CancellationToken, link: &Url) -> Site {
        let host = match link.host_str() {
            Some(h) if !h.is_empty() => match link.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_owned(),
            },
            _ => return Site { website: String::new(), name: "连接失败".into() },
        };
        let origin = format!("{}://{host}", link.scheme());
        let allowed = link
            .host_str()
            .map(|s| s.trim_start_matches('[').trim_end_matches(']'));
        let mut headers = HashMap::new();
        headers.insert("User-Agent".into(), "Mozilla/5.0".into());
        for candidate in [format!("{origin}/auth/login"), format!("{origin}/")] {
            let Ok(resp) = self
                .get(
                    cancel,
                    &candidate,
                    &headers,
                    512 << 10,
                    Duration::from_secs(5),
                    allowed,
                )
                .await
            else {
                continue;
            };
            if !resp.ok() {
                continue;
            }
            let body = String::from_utf8_lossy(&resp.body);
            let mut title = title_of(&body);
            if title.contains("Cloudflare") || title.contains("Just a moment") {
                title = "Cloudflare 防护".into();
            } else if title.contains("Access denied") || title.contains("404 Not Found") {
                title = "不是机场面板域名".into();
            }
            return Site {
                website: origin,
                name: title,
            };
        }
        Site {
            website: origin,
            name: "连接失败".into(),
        }
    }

    async fn mappings(&self, cancel: &CancellationToken) -> Vec<Mapping> {
        {
            let guard = self.cache.lock().await;
            if let Some(at) = guard.fetched_at
                && at.elapsed() < MAPPINGS_TTL
            {
                return guard.entries.clone();
            }
        }
        let Ok(url) = Url::parse(&self.mappings_url) else {
            return Vec::new();
        };
        let allowed = url
            .host_str()
            .map(|s| s.trim_start_matches('[').trim_end_matches(']'));
        let Ok(resp) = self
            .get(
                cancel,
                &self.mappings_url,
                &HashMap::new(),
                256 << 10,
                Duration::from_secs(10),
                allowed,
            )
            .await
        else {
            return Vec::new();
        };
        if !resp.ok() {
            return Vec::new();
        }
        let body = String::from_utf8_lossy(&resp.body);
        let list = parse_mappings(&body);
        if !list.is_empty() {
            let mut guard = self.cache.lock().await;
            guard.entries = list.clone();
            guard.fetched_at = Some(Instant::now());
        }
        list
    }
}

fn mask_link(raw: &str) -> String {
    let Ok(parsed) = Url::parse(raw) else {
        return "***".into();
    };
    let Some(host) = parsed.host_str().filter(|h| !h.is_empty()) else {
        return "***".into();
    };
    let authority = match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    };
    let prefix = format!("{}://{authority}", parsed.scheme());
    let rest = raw.strip_prefix(&prefix).unwrap_or("");
    let runes: Vec<char> = rest.chars().collect();
    if runes.len() > 8 {
        let tail: String = runes[runes.len() - 4..].iter().collect();
        format!("{prefix}/…{tail}")
    } else {
        format!("{prefix}/…")
    }
}

fn links(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in link_pattern().find_iter(text) {
        let s = m.as_str();
        if s.len() <= 2048 && seen.insert(s.to_owned()) {
            out.push(s.to_owned());
        }
    }
    out
}

fn entity_links(msg: &mibot_bot::Message) -> String {
    let mut out = Vec::new();
    if let Some(entities) = &msg.raw.entities {
        for ent in entities {
            if let grammers_tl_types::enums::MessageEntity::TextUrl(u) = ent {
                out.push(u.url.as_str());
            }
        }
    }
    out.join(" ")
}

fn failure_text(err: &HttpxError) -> String {
    match err {
        HttpxError::BlockedAddress(_) => "只能读取公网地址的订阅".into(),
        HttpxError::CrossHostRedirect => "订阅跳转到了别的域名".into(),
        other => httpx_reason(other),
    }
}

enum SubError {
    Fetch(HttpxError),
    Parse(String),
    User(String),
}

fn failure_sentence(err: &SubError) -> String {
    match err {
        SubError::Parse(msg) => format!("订阅解析失败：{msg}"),
        SubError::User(msg) => msg.clone(),
        SubError::Fetch(e) => format!("订阅读取失败：{}", failure_text(e)),
    }
}

struct SubReport {
    link: String,
    name: String,
    website: String,
    traffic: String,
    summary: Summary,
}

enum Outcome {
    Ok(SubReport),
    Err { link: String, err: SubError },
}

fn render_report(r: &SubReport, concise: bool, batch: bool, full: bool) -> String {
    let mut out = String::new();
    out.push_str("📈 <b>订阅信息</b>\n");
    out.push_str(&format!("<b>机场名称</b>：{}\n", code(&r.name)));
    out.push_str(&format!("<b>官网链接</b>：{}\n", escape(&r.website)));
    if concise || batch {
        out.push_str(&format!("<b>订阅链接</b>：{}\n", code(&r.link)));
    }
    out.push_str(&format!("<b>节点总数</b>：{}\n\n", r.summary.names.len()));
    out.push_str(&format!("<b>流量与到期</b>\n{}", escape(&r.traffic)));
    if !concise {
        let mut lines = r.summary.protocols.lines();
        if !full && lines.len() > LISTED_PROTOCOLS {
            let extra = lines.len() - LISTED_PROTOCOLS;
            lines.truncate(LISTED_PROTOCOLS);
            lines.push(format!("…还有 {extra} 种"));
        }
        let protocols = if lines.is_empty() {
            "未识别常见节点协议".to_owned()
        } else {
            lines.join("\n")
        };
        out.push_str(&format!("\n\n<b>协议分布</b>\n<pre>{}</pre>", escape(&protocols)));
        let regions = r.summary.regions.lines();
        if !regions.is_empty() {
            out.push_str(&format!("\n<b>地区分布</b>\n{}", escape(&regions.join(" · "))));
        }
    }
    let total = r.summary.names.len();
    if total > 0 {
        let shown = if full { total } else { total.min(LISTED_NODES) };
        let mut lines = Vec::with_capacity(shown + 1);
        for (i, name) in r.summary.names[..shown].iter().enumerate() {
            lines.push(format!("{}. {}", i + 1, truncate(name, NAME_LIMIT)));
        }
        if shown < total {
            lines.push(format!("…共 {total} 个，完整列表用 txt 参数查看"));
        }
        out.push_str(&format!(
            "\n\n<b>节点列表</b>\n<pre>{}</pre>",
            escape(&lines.join("\n"))
        ));
    }
    out
}

fn failed_document(link: &str, err: &SubError) -> String {
    format!(
        "📈 <b>订阅信息</b>\n<b>订阅链接</b>：{}\n❌ {}",
        code(&mask_link(link)),
        escape(&failure_sentence(err))
    )
}

fn render_outcomes(results: &[Outcome], concise: bool, full: bool) -> Vec<String> {
    let batch = results.len() > 1;
    results
        .iter()
        .map(|o| match o {
            Outcome::Ok(r) => render_report(r, concise, batch, full),
            Outcome::Err { link, err } => failed_document(link, err),
        })
        .collect()
}

fn plain_text(documents: &[String]) -> String {
    let joined = documents.join("\n\n");
    let stripped = tag_pattern_re().replace_all(&joined, "");
    format!("{}\n", unescape_html(&stripped))
}

struct Service {
    fetch: Fetcher,
    now: fn() -> DateTime<Utc>,
}

impl Service {
    async fn inspect(
        &self,
        cancel: &CancellationToken,
        raw: &str,
        names: &[Mapping],
    ) -> Result<SubReport, SubError> {
        let Ok(link) = Url::parse(raw) else {
            return Err(SubError::User("订阅链接无效".into()));
        };
        if link.host_str().is_none_or(str::is_empty) {
            return Err(SubError::User("订阅链接无效".into()));
        }
        let fetched = self
            .fetch
            .fetch_subscription(cancel, &link)
            .await
            .map_err(SubError::Fetch)?;
        let site = self.fetch.site_info(cancel, &link).await;
        let summary =
            parse_subscription(&fetched.text).map_err(|e| SubError::Parse(e.0))?;
        let mut name = mapped_name(raw, names).to_owned();
        if name.is_empty() {
            name = header_name(&fetched.content_disposition);
        }
        if name.is_empty() {
            name = kit::or_default(&site.name, "未知").to_owned();
        }
        name = truncate(&name, NAME_LIMIT);
        let mut website = site.website;
        if let Ok(profile) = Url::parse(&fetched.profile_url)
            && let Some(host) = profile.host_str().filter(|h| !h.is_empty())
            && (profile.scheme() == "https" || profile.scheme() == "http")
        {
            website = match profile.port() {
                Some(p) => format!("{}://{host}:{p}", profile.scheme()),
                None => format!("{}://{host}", profile.scheme()),
            };
        }
        Ok(SubReport {
            link: mask_link(raw),
            name,
            website,
            traffic: traffic_summary(&fetched.userinfo, fetched.has_userinfo, (self.now)()),
            summary,
        })
    }

    async fn send_text(
        &self,
        inv: &Invocation,
        logs: Option<&Arc<mibot_logtail::Ring>>,
        documents: &[String],
        count: usize,
        too_long: bool,
    ) -> CmdResult {
        let peer = inv.client.input_peer(&inv.message.peer)?;
        let name = format!("subinfo-{}.txt", (self.now)().format("%Y%m%d-%H%M"));
        let caption = if too_long {
            format!("✅ 结果太长，已生成订阅报告（共 {count} 个链接）")
        } else {
            format!("✅ 已生成订阅报告（共 {count} 个链接）")
        };
        let doc_bytes = plain_text(documents).into_bytes();
        inv.client
            .send_document(
                &peer,
                &doc_bytes,
                MediaOptions {
                    name,
                    mime_type: "text/plain".into(),
                    caption,
                    reply_to: Some(inv.message.reply_to_id).filter(|&id| id > 0),
                    force_document: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| kit::fail_with("发送报告失败", e))?;
        if let Err(err) = inv.client.delete_message(&inv.message).await {
            kit::opt_log(
                logs,
                mibot_logtail::Level::Warn,
                "subinfo.cleanup_failed",
                &[("error", &err.to_string())],
            );
        }
        Ok(())
    }

    async fn handle(
        &self,
        inv: &Invocation,
        logs: Option<&Arc<mibot_logtail::Ring>>,
        concise: bool,
    ) -> CmdResult {
        let mut args: &[String] = &inv.args;
        let txt = args
            .first()
            .is_some_and(|a| a.eq_ignore_ascii_case("txt"));
        if txt {
            args = &args[1..];
        }
        let mut source = args.join(" ");
        if inv.message.reply_to_id != 0 || args.is_empty() {
            match kit::reply_msg(inv).await {
                Err(err) if links(&source).is_empty() => return Err(err),
                Err(err) => {
                    kit::opt_log(
                        logs,
                        mibot_logtail::Level::Warn,
                        "subinfo.reply_unavailable",
                        &[("error", &err.to_string())],
                    );
                }
                Ok(Some(reply)) => {
                    source.push(' ');
                    source.push_str(&reply.content());
                    source.push(' ');
                    source.push_str(&entity_links(&reply));
                }
                Ok(None) => {}
            }
        }
        let list = links(&source);
        if list.is_empty() {
            let pages = html_pages(&help(inv.prefix()), PAGE_LIMIT);
            return inv.respond_pages(&pages).await.map_err(Into::into);
        }
        if list.len() > MAX_LINKS {
            return Err(kit::fail(format!(
                "一次最多查询 {MAX_LINKS} 个订阅，这次有 {} 个",
                list.len()
            )));
        }
        inv.edit_text(&kit::working(&format!("正在读取 {} 个订阅", list.len())))
            .await?;
        let names = self.fetch.mappings(&inv.cancel).await;
        let mut results = Vec::with_capacity(list.len());
        for link in &list {
            if inv.cancel.is_cancelled() {
                return Err(kit::fail("操作已取消"));
            }
            match self.inspect(&inv.cancel, link, &names).await {
                Ok(r) => results.push(Outcome::Ok(r)),
                Err(err) => {
                    if inv.cancel.is_cancelled() {
                        return Err(kit::fail("操作已取消"));
                    }
                    let msg = failure_sentence(&err);
                    kit::opt_log(
                        logs,
                        mibot_logtail::Level::Info,
                        "subinfo.failed",
                        &[("link", &mask_link(link)), ("error", &msg)],
                    );
                    if list.len() == 1 {
                        return Err(kit::fail(&msg));
                    }
                    results.push(Outcome::Err {
                        link: link.clone(),
                        err,
                    });
                }
            }
        }
        if txt {
            let docs = render_outcomes(&results, concise, true);
            return self.send_text(inv, logs, &docs, list.len(), false).await;
        }
        let mut pages = Vec::new();
        for doc in render_outcomes(&results, concise, false) {
            pages.extend(html_pages(&doc, PAGE_LIMIT));
        }
        if pages.len() > MAX_PAGES {
            let docs = render_outcomes(&results, concise, true);
            return self.send_text(inv, logs, &docs, list.len(), true).await;
        }
        inv.respond_pages(&pages).await.map_err(Into::into)
    }
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let item = |cmd: &str, args: &str, text: &str| {
        format!("• <code>{p}{cmd} {args}</code> {text}\n")
    };
    format!(
        "📈 <b>订阅信息查询</b>\n\n查询订阅的节点数量、协议、地区分布、流量与到期时间。\n\n{}{}{}{}\
         • 回复一条带订阅链接的消息，发送 <code>{p}subinfo</code> 或 <code>{p}cha</code>\n\n\
         <b>支持的内容</b>\n\
         • Clash YAML/JSON 的 proxies 列表\n\
         • 明文或 Base64 编码的节点链接\n\
         • VMess、VLESS、Trojan、SS、SSR、Hysteria、TUIC、WireGuard 等协议\n\
         • 流量和到期时间取自服务端返回的 subscription-userinfo 响应头\n\
         • 地区按节点名称识别，认不出的归入「其他」\n\n\
         一次最多 {MAX_LINKS} 个链接，只访问公网地址。聊天里最多列出 {LISTED_NODES} 个节点，结果超过 {MAX_PAGES} 页时改发 TXT 文件。订阅链接里带着 Token，结果里只显示打过码的链接。",
        item("subinfo", "订阅链接…", "查询一个或多个订阅"),
        item("subinfo", "txt 订阅链接…", "结果写成 TXT 文件发送"),
        item("cha", "订阅链接…", "简要查询"),
        item("cha", "txt 订阅链接…", "简要结果写成 TXT 文件发送"),
    )
}

pub fn register(app: &Arc<App>) {
    let svc = Arc::new(Service {
        fetch: Fetcher::new(true, MAPPINGS_URL.to_owned()),
        now: Utc::now,
    });
    for (name, desc, concise) in [
        ("subinfo", "查看订阅的节点与流量", false),
        ("cha", "简要查看订阅信息", true),
    ] {
        let s = Arc::clone(&svc);
        let logs = Arc::clone(&app.logs);
        let mut cmd = kit::make_cmd(
            name,
            &[],
            group::TOOLS,
            desc,
            "[txt] [订阅链接…]",
            help,
            move |inv| {
                let s = Arc::clone(&s);
                let logs = Arc::clone(&logs);
                Box::pin(async move { s.handle(&inv, Some(&logs), concise).await })
            },
        );
        cmd.timeout = Some(Duration::from_secs(15 * 60));
        app.registry.register(cmd);
    }
    let _ = parse_html("", None);
    let _ = utf16_len("");
    let _ = CmdError::from("ok");
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use chrono::TimeZone;
    use grammers_tl_types::{self as tl, Deserializable, Identifiable, Serializable};
    use mibot_bot::{Client, PeerCache, RpcTransport, SentRecorder};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct FakeTg {
        edits: std::sync::Mutex<Vec<String>>,
        uploaded: std::sync::Mutex<Vec<u8>>,
        captions: std::sync::Mutex<Vec<String>>,
        deleted: std::sync::Mutex<Vec<i32>>,
    }

    impl FakeTg {
        fn new() -> Self {
            Self {
                edits: Default::default(),
                uploaded: Default::default(),
                captions: Default::default(),
                deleted: Default::default(),
            }
        }

        fn all(&self) -> String {
            self.edits.lock().unwrap().join("\n---\n")
        }
    }

    impl RpcTransport for FakeTg {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            cid: u32,
            body: Vec<u8>,
        ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, grammers_mtsender::InvocationError>> {
            Box::pin(async move {
                let payload = &body[4..];
                match cid {
                    tl::functions::messages::EditMessage::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::EditMessage::from_bytes(payload).unwrap();
                        self.edits.lock().unwrap().push(req.message.unwrap_or_default());
                        Ok(tl::enums::Updates::Updates(tl::types::Updates {
                            updates: vec![],
                            users: vec![],
                            chats: vec![],
                            date: 0,
                            seq: 0,
                        })
                        .to_bytes())
                    }
                    tl::functions::messages::SendMessage::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::SendMessage::from_bytes(payload).unwrap();
                        self.edits.lock().unwrap().push(req.message);
                        Ok(tl::enums::Updates::UpdateShortSentMessage(
                            tl::types::UpdateShortSentMessage {
                                out: true,
                                id: 100,
                                pts: 1,
                                pts_count: 1,
                                date: 0,
                                media: None,
                                entities: None,
                                ttl_period: None,
                            },
                        )
                        .to_bytes())
                    }
                    tl::functions::upload::SaveFilePart::CONSTRUCTOR_ID => {
                        let req = tl::functions::upload::SaveFilePart::from_bytes(payload).unwrap();
                        self.uploaded.lock().unwrap().extend_from_slice(&req.bytes);
                        Ok(true.to_bytes())
                    }
                    tl::functions::messages::SendMedia::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::SendMedia::from_bytes(payload).unwrap();
                        self.captions.lock().unwrap().push(req.message);
                        Ok(tl::enums::Updates::Updates(tl::types::Updates {
                            updates: vec![],
                            users: vec![],
                            chats: vec![],
                            date: 0,
                            seq: 0,
                        })
                        .to_bytes())
                    }
                    tl::functions::messages::DeleteMessages::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::DeleteMessages::from_bytes(payload).unwrap();
                        self.deleted.lock().unwrap().extend(req.id);
                        Ok(tl::enums::messages::AffectedMessages::Messages(
                            tl::types::messages::AffectedMessages {
                                pts: 1,
                                pts_count: 1,
                            },
                        )
                        .to_bytes())
                    }
                    _ => Err(grammers_mtsender::InvocationError::Dropped("UNEXPECTED_REQUEST")),
                }
            })
        }
    }

    async fn run_cmd(s: &Service, args: &[&str]) -> (Arc<FakeTg>, CmdResult) {
        let tg = Arc::new(FakeTg::new());
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            tg.clone(),
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let mut msg = kit::blank_bot_message(7, tl::enums::Peer::User(tl::types::PeerUser { user_id: 1 }));
        msg.out = true;
        msg.saved = true;
        msg.text = format!(".subinfo {}", args.join(" "));
        let inv = Invocation {
            client,
            registry: Arc::new(mibot_command::Registry::new()),
            message: msg,
            prefix: ".".into(),
            command: "subinfo".into(),
            args: args.iter().map(|v| (*v).to_owned()).collect(),
            raw: args.join(" "),
            cancel: CancellationToken::new(),
            foreign: false,
        };
        let concise = args.first().copied() == Some("cha") || args.get(1).copied() == Some("cha");
        let res = s.handle(&inv, None, concise).await;
        (tg, res)
    }

    #[test]
    fn test_parse_clash_and_links() {
        let raw = r#"port: 7890
proxies:
  - {name: "🇭🇰 香港 01", server: hk.example.com, port: 443, type: ss, cipher: aes-128-gcm, password: "secret"}
  - name: US-West
    type: VMess
    server: us.example.com
    port: 443
    uuid: 0000
    ws-opts: {path: /, headers: {Host: example.com}}
  - {type: trojan, server: x, port: 443, password: p, name: 123}
proxy-groups: []
"#;
        let res = parse_subscription(raw).unwrap();
        assert_eq!(res.names, vec!["🇭🇰 香港 01", "US-West", "TROJAN 3"]);
        assert_eq!(res.protocols.lines(), vec!["ss：1", "vmess：1", "trojan：1"]);
        assert_eq!(res.regions.lines(), vec!["香港：1", "美国：1", "其他：1"]);

        let j = parse_subscription(
            r#"{"proxies":[{"name":"日本 Tokyo","type":"hysteria2","server":"a","port":1},{"name":"x","type":"tuic"}]}"#,
        )
        .unwrap();
        assert_eq!(j.names.len(), 2);
        assert_eq!(j.protocols.lines(), vec!["hysteria2：1", "tuic：1"]);

        for (bad, want) in [
            ("proxies: 3\n", "无效的 Clash 节点列表"),
            ("proxies:\n  - {name: a}\n", "无效的 Clash 节点"),
            ("proxies:\n  - {name: a, type: 'x y'}\n", "无效的 Clash 协议"),
            ("{ not json", "订阅不是有效的 Clash 配置"),
            ("   ", "订阅内容为空"),
        ] {
            let err = parse_subscription(bad).unwrap_err();
            assert_eq!(err.0, want, "raw={bad:?}");
        }

        for key in ["\"proxies\"", "'proxies'"] {
            let r = parse_subscription(&format!("port: 7890\n{key}:\n  - {{name: US-West, type: vmess}}\n"))
                .unwrap();
            assert_eq!(r.names, vec!["US-West"]);
            assert_eq!(r.protocols.lines(), vec!["vmess：1"]);
        }

        let vmess = format!(
            "vmess://{}",
            base64::engine::general_purpose::STANDARD
                .encode(r#"{"v":"2","ps":"新加坡 SG 01","add":"a"}"#)
        );
        let ssr_body = format!(
            "host:443:origin:aes-256-cfb:plain:cGFzcw/?remarks={}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("韩国 Seoul")
        );
        let ssr = format!(
            "ssr://{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&ssr_body)
        );
        let list = [
            vmess,
            "vless://uuid@host:443?security=tls#%E5%8F%B0%E6%B9%BE%20TW".into(),
            "trojan://pass@host:443#UK%20London".into(),
            "ss://YWVzOnBhc3M@host:8388#bad%zz".into(),
            ssr,
            "hysteria2://auth@host:443".into(),
            "tuic://uuid:pass@host:443#Thkx".into(),
            "wireguard://key@host:51820#Germany".into(),
            "not a node".into(),
        ]
        .join("\r\n");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&list);
        let url_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{list}\n"));
        for raw_src in [&list, &b64, &url_b64] {
            let res = parse_subscription(raw_src).unwrap();
            assert_eq!(
                res.names,
                vec![
                    "新加坡 SG 01",
                    "台湾 TW",
                    "UK London",
                    "bad%zz",
                    "韩国 Seoul",
                    "HYSTERIA2 6",
                    "Thkx",
                    "Germany",
                ]
            );
            assert_eq!(
                res.regions.lines(),
                vec!["新加坡：1", "台湾：1", "英国：1", "韩国：1", "德国：1", "其他：3"]
            );
        }
    }

    #[test]
    fn test_traffic_helpers_and_public_addr() {
        let now = Utc.with_ymd_and_hms(2026, 9, 30, 0, 0, 0).unwrap();
        let header = "upload=1073741824; download=2147483648; total=10737418240; expire=1798761600";
        let want = "上传：1.00 GB\n下载：2.00 GB\n已用：3.00 GB\n总量：10.00 GB\n剩余：7.00 GB\n到期：2027-01-01 00:00 UTC";
        assert_eq!(traffic_summary(header, true, now), want);
        assert_eq!(traffic_summary("", false, now), "流量统计：未提供");
        let depleted = traffic_summary("upload=5; download=10; total=10; expire=1000", true, now);
        assert!(depleted.contains("剩余：0 B\n流量状态：已耗尽"));
        assert!(depleted.contains("到期：1970-01-01 00:16 UTC（已过期）"));
        let partial = traffic_summary("UPLOAD = 1;total=0;expire=0;junk=5", true, now);
        assert_eq!(partial, "上传：1 B\n下载：未知\n总量：未设限\n到期：未设期限");
        assert!(traffic_summary("expire=99999999999999", true, now).ends_with("到期：无效时间"));
        let ov = traffic_summary("upload=18446744073709551615; download=10; total=100", true, now);
        assert!(ov.contains("已用：17179869184.00 GB") && ov.contains("流量状态：已耗尽"));

        assert_eq!(header_name("attachment; filename*=UTF-8''%E6%9C%BA%E5%9C%BA"), "机场");
        assert_eq!(header_name(r#"attachment; filename="My Airport""#), "My Airport");
        assert_eq!(header_name("attachment; filename=bad%zz"), "");
        assert_eq!(header_name("inline"), "");

        assert_eq!(title_of("<html><TITLE lang=zh>登录 — 某某云</TITLE>"), "某某云");
        assert_eq!(title_of("<title>\n  Nice &amp; Fast | 登录 </title>"), "Nice & Fast");
        assert_eq!(title_of("<title>登录\u{3000}—\u{3000}某某云</title>"), "某某云");
        assert_eq!(title_of("<title>Fast\u{00a0}\u{00a0}Cloud</title>"), "Fast Cloud");
        assert_eq!(title_of("<p>no title</p>"), "");
        assert!(clash_start_re().is_match("proxies\u{3000}:\n"));

        assert_eq!(
            mask_link("https://sub.example.com/api/v1/client/subscribe?token=abcdef123456"),
            "https://sub.example.com/…3456"
        );
        assert_eq!(mask_link("https://sub.example.com/s"), "https://sub.example.com/…");
        assert_eq!(mask_link("not a link"), "***");

        assert_eq!(
            links(r#"看这个 https://a.example/s?t=1，还有"http://b.example/x" 和 https://a.example/s?t=1 HTTPS://C.example/Sub。"#),
            vec!["https://a.example/s?t=1", "http://b.example/x", "HTTPS://C.example/Sub"]
        );

        let names = vec![
            Mapping { key: "a.example".into(), name: "甲".into() },
            Mapping { key: "example".into(), name: "乙".into() },
        ];
        assert_eq!(mapped_name("https://a.example/s", &names), "甲");
        assert_eq!(mapped_name("https://c.test/s", &names), "");

        for (ip_s, expect_pub) in [
            ("8.8.8.8", true),
            ("2606:4700:4700::1111", true),
            ("127.0.0.1", false),
            ("10.1.2.3", false),
            ("192.168.1.1", false),
            ("172.16.0.1", false),
            ("169.254.169.254", false),
            ("100.64.0.1", false),
            ("0.0.0.0", false),
            ("::1", false),
            ("fd00::1", false),
            ("fe80::1", false),
            ("::ffff:127.0.0.1", false),
            ("64:ff9b::a00:1", false),
            ("224.0.0.1", false),
            ("255.255.255.255", false),
        ] {
            assert_eq!(public_address(ip_s.parse().unwrap()), expect_pub, "ip={ip_s}");
        }
    }

    #[tokio::test]
    async fn test_fetch_guard_and_handle_and_limits() {
        let token = "tok3n-secret-9f8e";
        let nodes = base64::engine::general_purpose::STANDARD
            .encode("trojan://p@h:443#香港 01\nss://x@h:1#Japan 02\n");
        let long_nodes: String = (0..300)
            .map(|i| format!("trojan://p@h:443#{}{i}\n", "n".repeat(60)))
            .collect();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let nodes_c = nodes.clone();
        let long_c = long_nodes.clone();
        tokio::spawn(async move {
            while !stop_bg.load(Ordering::Relaxed) {
                let Ok(Ok((mut sock, _))) =
                    tokio::time::timeout(Duration::from_millis(50), listener.accept()).await
                else {
                    continue;
                };
                let nodes_c = nodes_c.clone();
                let long_c = long_c.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, extra_hdrs, body) = match path {
                        "/s" => ("200 OK", String::new(), "ok".to_owned()),
                        "/jump" => (
                            "302 Found",
                            "Location: http://localhost:1/elsewhere\r\n".to_owned(),
                            String::new(),
                        ),
                        "/ymys.txt" => (
                            "200 OK",
                            String::new(),
                            "# 对照表\nnothing.example=别的机场\n".to_owned(),
                        ),
                        "/auth/login" => (
                            "200 OK",
                            String::new(),
                            "<title>登录 - 喵云</title>".to_owned(),
                        ),
                        p if p.starts_with("/sub") => {
                            let ok_ua = req.to_ascii_lowercase().contains("user-agent: mi box");
                            if !ok_ua || !p.contains(&format!("token={token}")) {
                                ("403 Forbidden", String::new(), String::new())
                            } else {
                                (
                                    "200 OK",
                                    "Subscription-Userinfo: upload=0; download=1048576; total=0\r\n"
                                        .to_owned(),
                                    nodes_c,
                                )
                            }
                        }
                        "/empty" => ("200 OK", String::new(), String::new()),
                        p if p.starts_with("/big") => (
                            "200 OK",
                            format!("Content-Disposition: attachment; filename={}\r\n", "A".repeat(100)),
                            long_c,
                        ),
                        _ => ("404 Not Found", String::new(), String::new()),
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\n{extra_hdrs}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let cancel = CancellationToken::new();
        let guarded = Fetcher::new(true, format!("http://{addr}/ymys.txt"));
        let link_s = Url::parse(&format!("http://{addr}/s")).unwrap();
        let err = guarded.fetch_subscription(&cancel, &link_s).await.unwrap_err();
        assert!(matches!(err, HttpxError::BlockedAddress(_)));
        assert_eq!(failure_text(&err), "只能读取公网地址的订阅");

        let open = Fetcher::new(false, format!("http://{addr}/ymys.txt"));
        let jump_u = Url::parse(&format!("http://{addr}/jump")).unwrap();
        let err2 = open.fetch_subscription(&cancel, &jump_u).await.unwrap_err();
        assert!(matches!(err2, HttpxError::CrossHostRedirect));
        assert!(!failure_text(&err2).contains("localhost"));

        let s = Service {
            fetch: Fetcher::new(false, format!("http://{addr}/ymys.txt")),
            now: || Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap(),
        };
        let sub_link = format!("http://{addr}/sub?token={token}");
        let (tg, res) = run_cmd(&s, &[&sub_link]).await;
        res.unwrap();
        let out = tg.all();
        for want in [
            "订阅信息",
            "机场名称：喵云",
            "节点总数：2",
            "下载：1.0 MB",
            "总量：未设限",
            "trojan：1\nss：1",
            "香港：1 · 日本：1",
            "1. 香港 01\n2. Japan 02",
        ] {
            assert!(out.contains(want), "missing {want}:\n{out}");
        }
        assert!(!out.contains(token) && !out.contains("订阅链接"));

        let missing = format!("http://{addr}/missing?token=zzzz");
        let (tg, res) = run_cmd(&s, &[&sub_link, &missing]).await;
        res.unwrap();
        let out = tg.all();
        assert!(!out.contains(token));
        assert!(out.contains("/…9f8e"));
        assert!(out.contains("❌ 订阅读取失败：服务返回 HTTP 404"));

        let empty_u = format!("http://{addr}/empty");
        let (_, res) = run_cmd(&s, &[&empty_u]).await;
        let err = res.unwrap_err();
        assert!(kit::is_user_error(err.as_ref()));
        assert_eq!(err.to_string(), "订阅解析失败：订阅内容为空");

        let (tg, res) = run_cmd(&s, &["txt", &sub_link]).await;
        res.unwrap();
        let report = String::from_utf8_lossy(&tg.uploaded.lock().unwrap()).into_owned();
        assert_eq!(tg.captions.lock().unwrap().as_slice(), &["✅ 已生成订阅报告（共 1 个链接）"]);
        assert_eq!(tg.deleted.lock().unwrap().as_slice(), &[7]);
        assert!(!report.contains(token) && !report.contains("<b>") && report.contains("机场名称：喵云"));

        // Long result turns into TXT
        let big1 = format!("http://{addr}/big?t=1");
        let big2 = format!("http://{addr}/big?t=2");
        let (tg, res) = run_cmd(&s, &[&big1, &big2]).await;
        res.unwrap();
        let big_rep = String::from_utf8_lossy(&tg.uploaded.lock().unwrap()).into_owned();
        assert!(tg.captions.lock().unwrap()[0].contains("结果太长"));
        assert!(big_rep.contains("300. "));
        assert!(big_rep.contains(&format!("机场名称：{}…", "A".repeat(64))));
        assert!(!big_rep.contains(&"A".repeat(65)));

        // Too many links
        let many: Vec<String> = (0..=MAX_LINKS)
            .map(|i| format!("https://example.invalid/{}", "x".repeat(i + 1)))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let (_, res) = run_cmd(&s, &refs).await;
        let err = res.unwrap_err();
        assert!(err.to_string().contains("一次最多查询 20 个订阅"));

        stop.store(true, Ordering::Relaxed);
        let _ = AtomicUsize::new(0);
    }
}
