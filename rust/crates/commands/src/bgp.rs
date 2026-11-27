use crate::kit;
use mibot_app::App;
use mibot_command::{CmdError, CmdResult, Command, Invocation, code, escape, group, truncate};
use mibot_httpx::{HttpxError, Request, do_request};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_API_BASE: &str = "https://stat.ripe.net/data";
const SOURCE_APP: &str = "mibot-lite";
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const LOOKUP_BUDGET: Duration = Duration::from_secs(20);
const MAX_UPSTREAMS: usize = 5;
const HOLDER_WIDTH: usize = 60;
const MAX_LISTED: usize = 3;

const SMALL_BODY: usize = 256 << 10;
const GEO_BODY: usize = 1 << 20;
const NEIGHBOURS_BODY: usize = 2 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ip,
    Prefix,
    Asn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Prefix {
    addr: IpAddr,
    bits: u8,
}

impl Prefix {
    fn parse(value: &str) -> Option<Self> {
        let (ip_s, bits_s) = value.split_once('/')?;
        if ip_s.contains('%') {
            return None;
        }
        let raw_ip: IpAddr = ip_s.parse().ok()?;
        if let IpAddr::V6(v6) = raw_ip
            && v6.to_ipv4_mapped().is_some()
        {
            return None;
        }
        let bits: u8 = bits_s.parse().ok()?;
        let masked = match raw_ip {
            IpAddr::V4(v4) => {
                if bits > 32 {
                    return None;
                }
                let mask = if bits == 0 { 0 } else { !0u32 << (32 - u32::from(bits)) };
                IpAddr::V4(Ipv4Addr::from(u32::from(v4) & mask))
            }
            IpAddr::V6(v6) => {
                if bits > 128 {
                    return None;
                }
                let mask = if bits == 0 { 0 } else { !0u128 << (128 - u32::from(bits)) };
                IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
            }
        };
        Some(Self { addr: masked, bits })
    }

    fn is_single_ip(&self) -> bool {
        match self.addr {
            IpAddr::V4(_) => self.bits == 32,
            IpAddr::V6(_) => self.bits == 128,
        }
    }

    fn contains(&self, other: IpAddr) -> bool {
        match (self.addr, other) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let mask = if self.bits == 0 { 0 } else { !0u32 << (32 - u32::from(self.bits)) };
                (u32::from(a) & mask) == (u32::from(b) & mask)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let mask = if self.bits == 0 { 0 } else { !0u128 << (128 - u32::from(self.bits)) };
                (u128::from(a) & mask) == (u128::from(b) & mask)
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Prefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.bits)
    }
}

#[derive(Debug, Clone)]
struct Target {
    kind: Kind,
    addr: IpAddr,
    prefix: Prefix,
    asn: u32,
}

impl Target {
    fn label(&self) -> String {
        match self.kind {
            Kind::Prefix => self.prefix.to_string(),
            Kind::Asn => as_label(self.asn),
            Kind::Ip => self.addr.to_string(),
        }
    }
}

fn as_label(asn: u32) -> String {
    format!("AS{asn}")
}

fn address_in_text_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?|[0-9a-f]{0,4}(?::[0-9a-f]{0,4}){2,7}(?:/\d{1,3})?")
            .expect("valid bgp address regex")
    })
}

fn asn_in_text_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bASN?(\d{1,10})\b").expect("valid asn regex"))
}

fn parse_asn(mut value: &str) -> Option<u32> {
    if value.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("AS")) {
        value = &value[2..];
    }
    if value.is_empty() || value.len() > 10 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse::<u32>().ok()
}

fn parse_address(value: &str) -> Option<Target> {
    if value.contains('/') {
        let p = Prefix::parse(value)?;
        if p.is_single_ip() {
            return Some(Target {
                kind: Kind::Ip,
                addr: p.addr,
                prefix: p,
                asn: 0,
            });
        }
        return Some(Target {
            kind: Kind::Prefix,
            addr: p.addr,
            prefix: p,
            asn: 0,
        });
    }
    if value.contains('%') {
        return None;
    }
    let raw: IpAddr = value.parse().ok()?;
    let unmapped = match raw {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    Some(Target {
        kind: Kind::Ip,
        addr: unmapped,
        prefix: Prefix {
            addr: unmapped,
            bits: if unmapped.is_ipv4() { 32 } else { 128 },
        },
        asn: 0,
    })
}

fn find_target(text: &str) -> Option<Target> {
    let bytes = text.as_bytes();
    for m in address_in_text_re().find_iter(text) {
        let start = m.start();
        let end = m.end();
        if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || matches!(bytes[start - 1], b'_' | b'.' | b':')) {
            continue;
        }
        if end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            continue;
        }
        let matched = m.as_str();
        if matched.contains(':') && end + 1 < bytes.len() && bytes[end] == b'.' && bytes[end + 1].is_ascii_digit() {
            continue;
        }
        if let Some(t) = parse_address(matched) {
            return Some(t);
        }
    }
    if let Some(caps) = asn_in_text_re().captures(text)
        && let Some(asn) = caps.get(1).and_then(|m| parse_asn(m.as_str()))
    {
        return Some(Target {
            kind: Kind::Asn,
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            prefix: Prefix {
                addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                bits: 0,
            },
            asn,
        });
    }
    None
}

fn parse_target(text: &str) -> Option<Target> {
    let trimmed = text.trim();
    if let Some(asn) = parse_asn(trimmed) {
        return Some(Target {
            kind: Kind::Asn,
            addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            prefix: Prefix {
                addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                bits: 0,
            },
            asn,
        });
    }
    if let Some(t) = parse_address(trimmed) {
        return Some(t);
    }
    find_target(trimmed)
}

fn reserved_v4() -> &'static [Prefix] {
    static LIST: OnceLock<Vec<Prefix>> = OnceLock::new();
    LIST.get_or_init(|| {
        [
            "0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16",
            "172.16.0.0/12", "192.0.0.0/24", "192.0.2.0/24", "192.168.0.0/16", "198.18.0.0/15",
            "198.51.100.0/24", "203.0.113.0/24", "224.0.0.0/4", "240.0.0.0/4",
        ]
        .into_iter()
        .map(|s| Prefix::parse(s).expect("valid v4 prefix"))
        .collect()
    })
}

fn reserved_v6() -> &'static [Prefix] {
    static LIST: OnceLock<Vec<Prefix>> = OnceLock::new();
    LIST.get_or_init(|| {
        ["2001:db8::/32", "3fff::/20", "2001:2::/48"]
            .into_iter()
            .map(|s| Prefix::parse(s).expect("valid v6 prefix"))
            .collect()
    })
}

fn reserved_prefix(p: &Prefix) -> bool {
    let within = |outer: &Prefix| outer.bits <= p.bits && outer.contains(p.addr);
    let list = if p.addr.is_ipv6() {
        let global = Prefix::parse("2000::/3").expect("valid global v6");
        if !within(&global) {
            return true;
        }
        reserved_v6()
    } else {
        reserved_v4()
    };
    list.iter().any(within)
}

fn reserved_asn(asn: u32) -> bool {
    asn == 0 || asn == 23456 || (64496..=131071).contains(&asn) || asn >= 4_200_000_000
}

fn reserved_reason(t: &Target) -> Option<String> {
    let zh = kit::is_zh();
    match t.kind {
        Kind::Asn => reserved_asn(t.asn).then(|| {
            if zh {
                format!("{} 是私有或保留的 AS 号，公网上没有它的路由", t.label())
            } else {
                format!("{} is a private or reserved ASN with no public routing", t.label())
            }
        }),
        Kind::Prefix => {
            if (t.prefix.addr.is_ipv4() && t.prefix.bits < 8) || (t.prefix.addr.is_ipv6() && t.prefix.bits < 16) {
                return Some(kit::tr(
                    "Prefix too broad; minimum /8 for IPv4 and /16 for IPv6",
                    "前缀太大，IPv4 最短写到 /8，IPv6 最短写到 /16",
                ).into());
            }
            reserved_prefix(&t.prefix).then(|| {
                if zh {
                    format!("{} 是内网或保留地址段，公网上没有它的路由", t.label())
                } else {
                    format!("{} is a private or reserved prefix with no public routing", t.label())
                }
            })
        }
        Kind::Ip => {
            let p = Prefix {
                addr: t.addr,
                bits: if t.addr.is_ipv4() { 32 } else { 128 },
            };
            reserved_prefix(&p).then(|| {
                if zh {
                    format!("{} 是内网或保留地址，公网上没有它的路由", t.label())
                } else {
                    format!("{} is a private or reserved IP with no public routing", t.label())
                }
            })
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RipeEnvelope<T> {
    #[serde(default)]
    status: String,
    data: Option<T>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PrefixOverview {
    announced: bool,
    resource: String,
    asns: Vec<Origin>,
    related_prefixes: Vec<String>,
    actual_num_related: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Origin {
    asn: u32,
    holder: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AsOverview {
    holder: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RirEntry {
    rir: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RirList {
    rirs: Vec<RirEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Neighbour {
    asn: u32,
    #[serde(rename = "type")]
    kind: String,
    power: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NeighbourList {
    neighbours: Vec<Neighbour>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ReverseDns {
    #[serde(default)]
    result: Option<Vec<String>>,
    error: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GeoLocation {
    country: String,
    city: String,
    covered_percentage: f64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GeoResource {
    locations: Vec<GeoLocation>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GeoLite {
    located_resources: Vec<GeoResource>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AfCount {
    originating: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CountsInner {
    v4: AfCount,
    v6: AfCount,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PrefixCounts {
    counts: CountsInner,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AsNamesResp {
    names: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct UpstreamPeer {
    asn: u32,
    name: String,
}

#[derive(Debug, Clone)]
struct Upstreams {
    of: u32,
    total: usize,
    top: Vec<UpstreamPeer>,
}

struct Report {
    target: Target,
    prefix: Option<PrefixOverview>,
    geo: Option<GeoLite>,
    ptr: Option<ReverseDns>,
    rir: Option<RirList>,
    as_overview: Option<AsOverview>,
    counts: Option<PrefixCounts>,
    upstream: Option<Upstreams>,
    failed: Vec<&'static str>,
}

async fn call_ripe<T: for<'de> Deserialize<'de>>(
    cancel: &CancellationToken,
    api_base: &str,
    name: &str,
    params: &[(&str, &str)],
    max_bytes: usize,
) -> Result<T, CmdError> {
    let query_str = {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in params {
            ser.append_pair(k, v);
        }
        ser.append_pair("sourceapp", SOURCE_APP);
        ser.finish()
    };
    let url = format!("{api_base}/{name}/data.json?{query_str}");
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(kit::fail(kit::tr("Cancelled", "操作已取消"))),
        r = do_request(Request {
            url,
            timeout: Some(CALL_TIMEOUT),
            max_bytes: Some(max_bytes),
            ..Default::default()
        }) => r?,
    };
    if !resp.ok() {
        return Err(Box::new(HttpxError::Status(resp.status)));
    }
    let env: RipeEnvelope<T> = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("{name}: invalid JSON: {e}"))?;
    if env.status != "ok" {
        return Err(format!("{name}: status {:?}", env.status).into());
    }
    env.data.ok_or_else(|| format!("{name}: missing data").into())
}

async fn fetch_upstreams(
    cancel: &CancellationToken,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    api_base: &str,
    asn: u32,
) -> Result<Upstreams, CmdError> {
    let res_str = as_label(asn);
    let list: NeighbourList = call_ripe(
        cancel,
        api_base,
        "asn-neighbours",
        &[("resource", &res_str)],
        NEIGHBOURS_BODY,
    )
    .await?;
    let mut left: Vec<Neighbour> = list.neighbours.into_iter().filter(|n| n.kind == "left").collect();
    left.sort_by(|a, b| b.power.cmp(&a.power).then_with(|| a.asn.cmp(&b.asn)));
    let total = left.len();
    let mut top: Vec<UpstreamPeer> = left
        .into_iter()
        .take(MAX_UPSTREAMS)
        .map(|n| UpstreamPeer { asn: n.asn, name: String::new() })
        .collect();
    if top.is_empty() {
        return Ok(Upstreams { of: asn, total, top });
    }
    let labels = top.iter().map(|p| as_label(p.asn)).collect::<Vec<_>>().join(",");
    match call_ripe::<AsNamesResp>(cancel, api_base, "as-names", &[("resource", &labels)], SMALL_BODY).await {
        Ok(names) => {
            for peer in &mut top {
                if let Some(n) = names.names.get(&peer.asn.to_string()) {
                    peer.name = n.clone();
                }
            }
        }
        Err(err) => {
            kit::opt_log(
                logs,
                mibot_logtail::Level::Warn,
                "bgp.names_failed",
                &[("part", "上游名称"), ("error", &err.to_string())],
            );
        }
    }
    Ok(Upstreams { of: asn, total, top })
}

async fn fetch_ptr(cancel: &CancellationToken, api_base: &str, ip: &str) -> Result<ReverseDns, CmdError> {
    call_ripe(cancel, api_base, "reverse-dns-ip", &[("resource", ip)], SMALL_BODY).await
}

async fn lookup(
    cancel: &CancellationToken,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    api_base: &str,
    t: Target,
) -> Result<Report, CmdError> {
    let query = t.label();
    let mut r = Report {
        target: t.clone(),
        prefix: None,
        geo: None,
        ptr: None,
        rir: None,
        as_overview: None,
        counts: None,
        upstream: None,
        failed: Vec::new(),
    };
    let q_res = [("resource", query.as_str())];
    let q_ris = [
        ("resource", query.as_str()),
        ("list_prefixes", "false"),
        ("types", "o"),
        ("noise", "filter"),
    ];
    let parts: Vec<(&'static str, Option<CmdError>)> = if t.kind == Kind::Asn {
        let (as_res, rir_res, up_res, count_res) = tokio::join!(
            call_ripe::<AsOverview>(cancel, api_base, "as-overview", &q_res, SMALL_BODY),
            call_ripe::<RirList>(cancel, api_base, "rir", &q_res, SMALL_BODY),
            fetch_upstreams(cancel, logs, api_base, t.asn),
            call_ripe::<PrefixCounts>(cancel, api_base, "ris-prefixes", &q_ris, SMALL_BODY)
        );
        r.as_overview = as_res.as_ref().ok().cloned();
        r.rir = rir_res.as_ref().ok().cloned();
        r.upstream = up_res.as_ref().ok().cloned();
        r.counts = count_res.as_ref().ok().cloned();
        vec![
            (kit::tr("AS info", "AS 信息"), as_res.err()),
            ("RIR", rir_res.err()),
            (kit::tr("Announced prefixes", "宣告前缀"), count_res.err()),
            (kit::tr("Upstreams", "上游"), up_res.err()),
        ]
    } else {
        let route_fut = async {
            let p_res = call_ripe::<PrefixOverview>(cancel, api_base, "prefix-overview", &q_res, SMALL_BODY).await;
            match p_res {
                Ok(p) => {
                    let up = if let Some(first_asn) = p.asns.first() {
                        Some(fetch_upstreams(cancel, logs, api_base, first_asn.asn).await)
                    } else {
                        None
                    };
                    (Ok(p), up)
                }
                Err(e) => (Err(e), None),
            }
        };
        let ptr_fut = async {
            if t.kind == Kind::Ip {
                Some(fetch_ptr(cancel, api_base, &query).await)
            } else {
                None
            }
        };
        let ((prefix_res, up_opt), geo_res, rir_res, ptr_opt) = tokio::join!(
            route_fut,
            call_ripe::<GeoLite>(cancel, api_base, "maxmind-geo-lite", &q_res, GEO_BODY),
            call_ripe::<RirList>(cancel, api_base, "rir", &q_res, SMALL_BODY),
            ptr_fut,
        );
        r.prefix = prefix_res.as_ref().ok().cloned();
        let up_err = match up_opt {
            Some(Ok(u)) => {
                r.upstream = Some(u);
                None
            }
            Some(Err(e)) => Some(e),
            None => None,
        };
        r.geo = geo_res.as_ref().ok().cloned();
        r.rir = rir_res.as_ref().ok().cloned();
        let ptr_err = match ptr_opt {
            Some(Ok(p)) => {
                r.ptr = Some(p);
                None
            }
            Some(Err(e)) => Some(e),
            None => None,
        };
        vec![
            (kit::tr("Routing", "路由"), prefix_res.err()),
            (kit::tr("Location", "地区"), geo_res.err()),
            ("RIR", rir_res.err()),
            (kit::tr("Reverse DNS", "反向解析"), ptr_err),
            (kit::tr("Upstreams", "上游"), up_err),
        ]
    };

    let mut first: Option<CmdError> = None;
    for (name, err_opt) in parts {
        let Some(err) = err_opt else {
            continue;
        };
        kit::opt_log(
            logs,
            mibot_logtail::Level::Warn,
            "bgp.part_failed",
            &[("part", name), ("error", &err.to_string())],
        );
        r.failed.push(name);
        if first.is_none() {
            first = Some(err);
        }
    }
    if r.prefix.is_none()
        && r.geo.is_none()
        && r.ptr.is_none()
        && r.rir.is_none()
        && r.as_overview.is_none()
        && r.counts.is_none()
        && r.upstream.is_none()
        && let Some(err) = first
    {
        return Err(err);
    }
    Ok(r)
}

impl Clone for PrefixOverview {
    fn clone(&self) -> Self {
        Self {
            announced: self.announced,
            resource: self.resource.clone(),
            asns: self.asns.clone(),
            related_prefixes: self.related_prefixes.clone(),
            actual_num_related: self.actual_num_related,
        }
    }
}
impl Clone for Origin {
    fn clone(&self) -> Self { Self { asn: self.asn, holder: self.holder.clone() } }
}
impl Clone for AsOverview {
    fn clone(&self) -> Self { Self { holder: self.holder.clone() } }
}
impl Clone for RirEntry {
    fn clone(&self) -> Self { Self { rir: self.rir.clone() } }
}
impl Clone for RirList {
    fn clone(&self) -> Self { Self { rirs: self.rirs.clone() } }
}
impl Clone for ReverseDns {
    fn clone(&self) -> Self { Self { result: self.result.clone(), error: self.error.clone() } }
}
impl Clone for GeoLocation {
    fn clone(&self) -> Self { Self { country: self.country.clone(), city: self.city.clone(), covered_percentage: self.covered_percentage } }
}
impl Clone for GeoResource {
    fn clone(&self) -> Self { Self { locations: self.locations.clone() } }
}
impl Clone for GeoLite {
    fn clone(&self) -> Self { Self { located_resources: self.located_resources.clone() } }
}
impl Clone for AfCount {
    fn clone(&self) -> Self { Self { originating: self.originating } }
}
impl Clone for CountsInner {
    fn clone(&self) -> Self { Self { v4: self.v4.clone(), v6: self.v6.clone() } }
}
impl Clone for PrefixCounts {
    fn clone(&self) -> Self { Self { counts: self.counts.clone() } }
}

fn flag(code: &str) -> String {
    let b = code.as_bytes();
    if b.len() != 2 || !b[0].is_ascii_uppercase() || !b[1].is_ascii_uppercase() {
        return String::new();
    }
    let c1 = char::from_u32(0x1F1E6 + u32::from(b[0] - b'A')).unwrap_or('?');
    let c2 = char::from_u32(0x1F1E6 + u32::from(b[1] - b'A')).unwrap_or('?');
    format!("{c1}{c2} ")
}

fn as_with_name(asn: u32, name: &str) -> String {
    let mut text = code(&as_label(asn));
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        text.push(' ');
        text.push_str(&escape(&truncate(trimmed, HOLDER_WIDTH)));
    }
    text
}

fn list_codes(values: &[String], total: usize) -> String {
    let shown = &values[..values.len().min(MAX_LISTED)];
    let mut text = shown.iter().map(|v| code(v)).collect::<Vec<_>>().join(kit::tr(", ", "、"));
    if total > shown.len() {
        if kit::is_zh() {
            text.push_str(&format!(" 等 {total} 个"));
        } else {
            text.push_str(&format!(" ({total} total)"));
        }
    }
    text
}

fn places(g: &GeoLite) -> String {
    let mut covered: HashMap<String, f64> = HashMap::new();
    let mut city: HashMap<String, String> = HashMap::new();
    for res in &g.located_resources {
        for loc in &res.locations {
            if loc.country.len() != 2 {
                continue;
            }
            *covered.entry(loc.country.clone()).or_insert(0.0) += loc.covered_percentage;
            city.entry(loc.country.clone())
                .or_insert_with(|| loc.city.trim().to_owned());
            if city.get(&loc.country).is_some_and(String::is_empty) && !loc.city.trim().is_empty() {
                city.insert(loc.country.clone(), loc.city.trim().to_owned());
            }
        }
    }
    if covered.is_empty() {
        return kit::or_dash("").to_owned();
    }
    let mut countries: Vec<String> = covered.keys().cloned().collect();
    countries.sort_by(|a, b| {
        covered[b]
            .partial_cmp(&covered[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b))
    });
    if countries.len() == 1 {
        let c = &countries[0];
        let mut text = format!("{}{}", flag(c), escape(c));
        if let Some(ci) = city.get(c)
            && !ci.is_empty()
        {
            text.push_str(&format!(" · {}", escape(ci)));
        }
        return text;
    }
    countries
        .into_iter()
        .take(MAX_LISTED)
        .map(|c| format!("{}{} {:.0}%", flag(&c), escape(&c), covered[&c]))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn rir_line(list: Option<&RirList>) -> Option<String> {
    let list = list?;
    let mut names: Vec<String> = Vec::new();
    for item in &list.rirs {
        let trimmed = item.rir.trim();
        if !trimmed.is_empty() && !names.iter().any(|n| n == trimmed) {
            names.push(trimmed.to_owned());
        }
    }
    if names.is_empty() {
        None
    } else if kit::is_zh() {
        Some(format!("RIR：{}", escape(&names.join("、"))))
    } else {
        Some(format!("RIR: {}", escape(&names.join(", "))))
    }
}

fn ptr_text(ptr: &ReverseDns) -> String {
    if let Some(res) = &ptr.result
        && !res.is_empty()
    {
        return list_codes(res, res.len());
    }
    let lower = ptr.error.to_ascii_lowercase();
    if ["timed out", "timeout", "servfail", "failed", "refused"]
        .iter()
        .any(|w| lower.contains(w))
    {
        kit::tr("Resolution failed", "无法解析").into()
    } else {
        kit::tr("None", "无").into()
    }
}

fn links(r: &Report) -> Vec<String> {
    let mk = |path: &str, label: &str| {
        format!(
            "<a href=\"https://bgp.he.net/{}\">{}</a>",
            escape(path),
            escape(label)
        )
    };
    if r.target.kind == Kind::Asn {
        let lbl = as_label(r.target.asn);
        return vec![mk(&lbl, &lbl)];
    }
    if let Some(p) = &r.prefix
        && p.announced
        && !p.resource.is_empty()
    {
        let mut out = vec![mk(&format!("net/{}", p.resource), &p.resource)];
        if let Some(o) = p.asns.first() {
            let lbl = as_label(o.asn);
            out.push(mk(&lbl, &lbl));
        }
        return out;
    }
    let lbl = r.target.label();
    if r.target.kind == Kind::Ip {
        vec![mk(&format!("ip/{lbl}"), &lbl)]
    } else {
        vec![mk(&format!("net/{lbl}"), &lbl)]
    }
}

fn render(r: &Report) -> String {
    let zh = kit::is_zh();
    let mut lines = vec![
        kit::tr("🛰 <b>BGP Routing Info</b>", "🛰 <b>BGP 路由信息</b>").to_owned(),
        String::new(),
        format!("{}{}", kit::tr("Query: ", "查询："), code(&r.target.label())),
    ];
    if r.target.kind == Kind::Asn {
        if let Some(a) = &r.as_overview {
            lines.push(format!(
                "{}{}",
                kit::tr("Name: ", "名称："),
                escape(kit::or_dash(&truncate(&a.holder, HOLDER_WIDTH)))
            ));
        }
        if let Some(rl) = rir_line(r.rir.as_ref()) {
            lines.push(rl);
        }
        if let Some(c) = &r.counts {
            let v4 = c.counts.v4.originating;
            let v6 = c.counts.v6.originating;
            if v4 + v6 == 0 {
                lines.push(kit::tr("Announced: None", "宣告前缀：无").into());
            } else if zh {
                lines.push(format!("宣告前缀：IPv4 {v4} 个 · IPv6 {v6} 个"));
            } else {
                lines.push(format!("Announced: {v4} IPv4 · {v6} IPv6"));
            }
        }
    } else {
        if let Some(p) = &r.prefix {
            if p.announced {
                lines.push(format!("{}{}", kit::tr("Prefix: ", "前缀："), code(&p.resource)));
            } else if !p.related_prefixes.is_empty() {
                lines.push(kit::tr("Prefix: Not announced as aggregate", "前缀：没有整段宣告").into());
            } else {
                lines.push(kit::tr("Prefix: Not announced publicly", "前缀：未在公网宣告").into());
            }
            for o in &p.asns {
                lines.push(format!(
                    "{}{}",
                    kit::tr("Origin AS: ", "起源 AS："),
                    as_with_name(o.asn, &o.holder)
                ));
            }
            if !p.related_prefixes.is_empty() {
                let total = p.actual_num_related.max(p.related_prefixes.len());
                lines.push(format!(
                    "{}{}",
                    kit::tr("Related prefixes: ", "相关前缀："),
                    list_codes(&p.related_prefixes, total)
                ));
            }
        }
        if let Some(g) = &r.geo {
            lines.push(format!("{}{}", kit::tr("Location: ", "地区："), places(g)));
        }
        if let Some(rl) = rir_line(r.rir.as_ref()) {
            lines.push(rl);
        }
        if let Some(ptr) = &r.ptr {
            lines.push(format!("{}{}", kit::tr("Reverse DNS: ", "反向解析："), ptr_text(ptr)));
        }
    }
    if let Some(up) = &r.upstream {
        lines.push(String::new());
        let mut head = if zh {
            format!("<b>{} 的上游</b>", as_label(up.of))
        } else {
            format!("<b>Upstreams of {}</b>", as_label(up.of))
        };
        if up.total > up.top.len() {
            if zh {
                head.push_str(&format!("（共 {} 个，按可见度列前 {} 个）", up.total, up.top.len()));
            } else {
                head.push_str(&format!(" ({} total, top {} by visibility)", up.total, up.top.len()));
            }
        }
        lines.push(head);
        if up.top.is_empty() {
            lines.push(kit::tr("None", "无").into());
        }
        for p in &up.top {
            lines.push(format!("• {}", as_with_name(p.asn, &p.name)));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "{}{}",
        kit::tr("View on bgp.he.net: ", "在 bgp.he.net 查看："),
        links(r).join(" · ")
    ));
    if !r.failed.is_empty() {
        lines.push(String::new());
        lines.push(if zh {
            format!("⚠️ 部分数据没查到（{}），稍后再试", r.failed.join("、"))
        } else {
            format!("⚠️ Partial data unavailable ({}); try again later", r.failed.join(", "))
        });
    }
    lines.join("\n")
}

fn render_ptr(ip: &str, ptr: &ReverseDns) -> String {
    let sep = kit::tr(": ", "：");
    let mut lines = vec![
        kit::tr("🛰 <b>Reverse DNS</b>", "🛰 <b>反向解析</b>").to_owned(),
        String::new(),
        format!("{}{}", kit::tr("Address: ", "地址："), code(ip)),
    ];
    let list = ptr.result.as_deref().unwrap_or(&[]);
    if list.is_empty() {
        lines.push(format!("PTR{sep}{}", ptr_text(ptr)));
        return lines.join("\n");
    }
    for name in list.iter().take(20) {
        lines.push(format!("PTR{sep}{}", code(name)));
    }
    lines.join("\n")
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "🛰 <b>BGP 路由信息</b>\n\n查 IP、前缀或 AS 号在公网上的路由：所在前缀、起源 AS、上游 AS、地区和反向解析。\n\n\
             • <code>{p}bgp 1.1.1.1</code>\n\
             • <code>{p}bgp 2606:4700::/32</code>\n\
             • <code>{p}bgp AS13335</code>\n\
             • 回复一条含 IP 的消息发 <code>{p}bgp</code>\n\
             • <code>{p}bgp dns 8.8.8.8</code> 只查反向解析（PTR）\n\n数据来自 RIPEstat，只有文字，没有路由图。"
        )
    } else {
        format!(
            "🛰 <b>BGP Routing Info</b>\n\nLook up public BGP routing for an IP, prefix, or ASN: covering prefix, origin AS, upstreams, location, and reverse DNS.\n\n\
             • <code>{p}bgp 1.1.1.1</code>\n\
             • <code>{p}bgp 2606:4700::/32</code>\n\
             • <code>{p}bgp AS13335</code>\n\
             • Reply to a message containing an IP with <code>{p}bgp</code>\n\
             • <code>{p}bgp dns 8.8.8.8</code> Reverse DNS (PTR) only\n\nPowered by RIPEstat (text only, no graphs)."
        )
    }
}

async fn handle(inv: &Invocation, app: &App) -> CmdResult {
    let mut args = inv.args.as_slice();
    let dns = args.first().is_some_and(|a| a.eq_ignore_ascii_case("dns"));
    if dns {
        args = &args[1..];
    }
    let t = {
        let joined = args.join(" ");
        let trimmed = joined.trim();
        if !trimmed.is_empty() {
            parse_target(trimmed).ok_or_else(|| {
                kit::fail(kit::tr(
                    "Unable to parse target; enter an IP, prefix, or ASN, e.g. 1.1.1.1, 1.1.1.0/24, AS13335",
                    "看不出要查什么，写 IP、前缀或 AS 号，例如 1.1.1.1、1.1.1.0/24、AS13335",
                ))
            })?
        } else {
            let Some(reply) = kit::reply_msg(inv).await? else {
                return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
            };
            find_target(&reply.content()).ok_or_else(|| {
                kit::fail(kit::tr(
                    "No IP, prefix, or ASN found in replied message",
                    "被回复的消息里没有 IP、前缀或 AS 号",
                ))
            })?
        }
    };
    if let Some(reason) = reserved_reason(&t) {
        return Err(kit::fail(reason));
    }
    let sub = inv.cancel.child_token();
    if dns {
        if t.kind != Kind::Ip {
            return Err(kit::fail(kit::tr(
                "Reverse DNS lookup only supports a single IP",
                "反向解析只能查单个 IP",
            )));
        }
        let label = t.label();
        inv.edit_text(&kit::working(&if kit::is_zh() {
            format!("正在查询 {label} 的反向解析")
        } else {
            format!("Looking up reverse DNS for {label}")
        }))
        .await?;
        let ptr_fail = || kit::tr("Reverse DNS lookup failed", "反向解析查询失败");
        let ptr = tokio::time::timeout(LOOKUP_BUDGET, fetch_ptr(&sub, DEFAULT_API_BASE, &label))
            .await
            .map_err(|_| kit::fail_with(ptr_fail(), std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")))?
            .map_err(|e| kit::fail_with(ptr_fail(), mibot_command::UserError::new(e.to_string())))?;
        inv.edit(&render_ptr(&label, &ptr)).await?;
        return Ok(());
    }
    inv.edit_text(&kit::working(&if kit::is_zh() {
        format!("正在查询 {}", t.label())
    } else {
        format!("Looking up {}", t.label())
    }))
    .await?;
    let report = tokio::time::timeout(LOOKUP_BUDGET, lookup(&sub, Some(&app.logs), DEFAULT_API_BASE, t))
        .await
        .map_err(|_| kit::fail(kit::tr("BGP lookup failed", "BGP 数据查询失败")))?
        .map_err(|e| {
            if kit::is_user_error(e.as_ref()) {
                e
            } else if kit::is_zh() {
                kit::fail(format!("BGP 数据查询失败（{}）", mibot_command::brief(e.as_ref())))
            } else {
                kit::fail(format!("BGP lookup failed ({})", mibot_command::brief(e.as_ref())))
            }
        })?;
    inv.edit(&render(&report)).await?;
    Ok(())
}

pub fn register(app: &Arc<App>) {
    let app_ref = Arc::clone(app);
    app.registry.register(
        Command {
            name: "bgp".into(),
            group: group::TOOLS.into(),
            description: "Query BGP routing info".into(),
            usage: "[dns] [IP|prefix|AS]".into(),
            help_fn: Some(Arc::new(help)),
            timeout: Some(Duration::from_secs(30)),
            handle: Arc::new(move |inv| {
                let a = Arc::clone(&app_ref);
                Box::pin(async move { handle(&inv, &a).await })
            }),
            ..Default::default()
        }
        .zh("查询 IP 的 BGP 路由", "[dns] [IP|前缀|AS]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mibot_bot::{parse_html, utf16_len};
    use mibot_command::PAGE_LIMIT;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_ripe(answers: HashMap<&'static str, (u16, &'static str)>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_ref = Arc::clone(&calls);
        let answers: HashMap<String, (u16, String)> = answers
            .into_iter()
            .map(|(k, (s, b))| (k.to_owned(), (s, b.to_owned())))
            .collect();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let answers = answers.clone();
                let calls = Arc::clone(&calls_ref);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]);
                    let first_line = raw.lines().next().unwrap_or("");
                    let path_query = first_line.split_whitespace().nth(1).unwrap_or("/");
                    let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
                    let name = path.trim_start_matches('/').trim_end_matches("/data.json");
                    let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
                    let res_param = params.get("resource").cloned().unwrap_or_default();
                    let key = format!("{name} {res_param}");
                    calls.lock().unwrap().push(key.clone());
                    let (status, body) = answers
                        .get(&key)
                        .cloned()
                        .unwrap_or((404, "not found".to_owned()));
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), calls)
    }

    fn check_html(text: &str) {
        parse_html(text, None).unwrap();
        assert!(utf16_len(text) <= PAGE_LIMIT as i32);
    }

    #[test]
    fn test_parse_find_and_reserved() {
        for (input, want) in [
            ("1.1.1.1", "1.1.1.1"),
            (" 8.8.8.8 ", "8.8.8.8"),
            ("2001:4860:4860::8888", "2001:4860:4860::8888"),
            ("1.1.1.0/24", "1.1.1.0/24"),
            ("1.1.1.1/24", "1.1.1.0/24"),
            ("1.1.1.1/32", "1.1.1.1"),
            ("2606:4700::/32", "2606:4700::/32"),
            ("AS13335", "AS13335"),
            ("as13335", "AS13335"),
            ("13335", "AS13335"),
            ("::ffff:1.1.1.1", "1.1.1.1"),
            ("https://1.1.1.1:8443/x", "1.1.1.1"),
            ("[2606:4700::1111]:443", "2606:4700::1111"),
            ("ASN4134", "AS4134"),
            ("看看 AS4134 的上游", "AS4134"),
            ("4294967295", "AS4294967295"),
            ("2001:db8::1 和 9.9.9.9", "2001:db8::1"),
            ("2606:4700::1111.", "2606:4700::1111"),
            ("a 1.1.1.1 b 2.2.2.2", "1.1.1.1"),
            ("AS13335 宣告了 1.1.1.0/24", "1.1.1.0/24"),
            ("fe80::1%eth0", "fe80::1"),
        ] {
            let got = parse_target(input).unwrap();
            assert_eq!(got.label(), want, "input={input}");
        }
        for bad in ["", "example.com", "4294967296", "AS", "1.1.1.1/33", "hello"] {
            assert!(parse_target(bad).is_none(), "bad={bad}");
        }
        for (text, want) in [
            ("连不上 1.1.1.1 了", "1.1.1.1"),
            ("地址是1.1.1.1吗", "1.1.1.1"),
            ("12:30:45 的时候 8.8.4.4 丢包", "8.8.4.4"),
            ("网卡 aa:bb:cc:dd:ee:ff 拿到了 9.9.9.9", "9.9.9.9"),
            ("std::vector 和 AS174", "AS174"),
            ("Cogent 是 as174。", "AS174"),
        ] {
            assert_eq!(find_target(text).unwrap().label(), want, "text={text}");
        }
        for bad in ["12:30:45", "std::vector<int>", "Foo::Bar", "deadbeef:cafe::1", "端口 13335", "1234.1.1.1", "::ffff:1.2.3.4x", "没有地址"] {
            assert!(find_target(bad).is_none(), "bad={bad}");
        }

        for r in [
            "10.0.0.1", "192.168.1.1", "172.16.5.4", "127.0.0.1", "100.64.1.1", "169.254.1.1",
            "0.1.2.3", "192.0.2.10", "198.51.100.1", "203.0.113.9", "198.18.0.1", "224.0.0.1",
            "255.255.255.255", "::1", "fd00::1", "fe80::1", "ff02::1", "2001:db8::1", "3fff::1",
            "::", "10.0.0.0/8", "192.168.0.0/24", "fc00::/7", "2001:db8::/48", "0.0.0.0/0",
            "1.0.0.0/7", "2000::/3", "AS0", "AS23456", "AS64512", "AS65535", "AS4200000000", "AS4294967295",
        ] {
            let t = parse_target(r).unwrap();
            assert!(reserved_reason(&t).is_some(), "expected reserved: {r}");
        }
        for pub_ok in ["1.1.1.1", "8.8.8.0/24", "192.0.0.0/8", "2606:4700::1111", "2001:4860::/32", "AS13335", "AS4134", "AS131072"] {
            let t = parse_target(pub_ok).unwrap();
            assert!(reserved_reason(&t).is_none(), "expected public: {pub_ok}");
        }
        assert_eq!(
            reserved_reason(&parse_target("192.168.1.1").unwrap()).unwrap(),
            "192.168.1.1 is a private or reserved IP with no public routing"
        );
    }

    #[tokio::test]
    async fn test_lookups_and_renders() {
        let overview1111 = r#"{"status":"ok","data":{"is_less_specific":true,"announced":true,"asns":[{"asn":13335,"holder":"CLOUDFLARENET - Cloudflare, Inc."}],"related_prefixes":[],"resource":"1.1.1.0/24","actual_num_related":0}}"#;
        let neighbours13335 = r#"{"status":"ok","data":{"neighbours":[
            {"asn":10026,"type":"left","power":1},{"asn":174,"type":"left","power":5899},
            {"asn":3356,"type":"left","power":5795},{"asn":9002,"type":"left","power":3161},
            {"asn":1299,"type":"left","power":7584},{"asn":3257,"type":"left","power":4865},
            {"asn":34549,"type":"left","power":3063},{"asn":64000,"type":"right","power":99999},
            {"asn":64001,"type":"uncertain","power":88888}]}}"#;
        let names = r#"{"status":"ok","data":{"names":{"1299":"TWELVE99 Arelion, fka Telia Carrier","174":"COGENT-174 - Cogent Communications, LLC","3356":"LEVEL3 - Level 3 Parent, LLC","3257":"GTT-BACKBONE GTT Communications Inc.","9002":"RETN-AS <RETN> & co"}}}"#;
        let geo1111 = r#"{"status":"ok","data":{"located_resources":[{"locations":[{"country":"?","city":"","covered_percentage":100.0}]}]}}"#;
        let ptr1111 = r#"{"status":"ok","data":{"result":["one.one.one.one"],"error":""}}"#;
        let rir_apnic = r#"{"status":"ok","data":{"rirs":[{"rir":"APNIC"}]}}"#;

        let (base, calls) = spawn_ripe(HashMap::from([
            ("prefix-overview 1.1.1.1", (200, overview1111)),
            ("maxmind-geo-lite 1.1.1.1", (200, geo1111)),
            ("reverse-dns-ip 1.1.1.1", (200, ptr1111)),
            ("asn-neighbours AS13335", (200, neighbours13335)),
            ("as-names AS1299,AS174,AS3356,AS3257,AS9002", (200, names)),
            ("rir 1.1.1.1", (200, rir_apnic)),
        ]))
        .await;
        let r = lookup(&CancellationToken::new(), None, &base, parse_target("1.1.1.1").unwrap())
            .await
            .unwrap();
        let text = render(&r);
        check_html(&text);
        assert!(text.contains("Origin AS: <code>AS13335</code> CLOUDFLARENET - Cloudflare, Inc."));
        assert!(text.contains("Location: —") && text.contains("RIR: APNIC") && text.contains("RETN-AS &lt;RETN&gt; &amp; co"));
        assert!(!text.contains("AS64000") && !text.contains("AS34549") && !text.contains("⚠️"));
        assert_eq!(calls.lock().unwrap().len(), 6);

        // Partial failure + PTR rendering
        let ptr = ReverseDns {
            result: Some(vec!["a.example".into(), "b.example".into()]),
            error: String::new(),
        };
        let ptr_text = render_ptr("8.8.8.8", &ptr);
        check_html(&ptr_text);
        assert!(ptr_text.contains("PTR: <code>a.example</code>\nPTR: <code>b.example</code>"));
    }
}
