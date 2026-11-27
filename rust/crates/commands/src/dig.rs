use crate::kit;
use mibot_app::App;
use mibot_bot::BoxFuture;
use mibot_command::{
    CmdError, CmdResult, Command, Invocation, PAGE_LIMIT, code, escape, group, html_pages, truncate,
};
use mibot_httpx::{HttpxError, Request, post_json};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command as ProcCommand;
use tokio_util::sync::CancellationToken;

const RUN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT: usize = 32 << 10;
const MAX_STDERR: usize = 4 << 10;
const BATCH_API: &str = "http://ip-api.com/batch?fields=status,country,regionName,city,as,query&lang=zh-CN";
const MAX_LOOKUPS: usize = 50;

const RECORD_TYPES: &[&str] = &["A", "AAAA", "MX", "CNAME", "TXT", "NS", "SOA", "PTR", "SRV", "CAA"];
const OUTPUT_FLAGS: &[&str] = &["+short", "+noall", "+answer", "+stats", "+comments", "+tcp"];

fn domain_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?\.)+(?:[A-Za-z]{2,63}|xn--[A-Za-z0-9-]{1,59})\.?$")
            .expect("valid domain regex")
    })
}

fn server_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)+(?:[A-Za-z]{2,63}|xn--[A-Za-z0-9-]{1,59})\.?$")
            .expect("valid server regex")
    })
}

fn answer_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\S+\s+\d+\s+IN\s+(?:A|AAAA)\s+(\S+)\s*$").expect("valid answer regex"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Query {
    name: String,
    reverse: bool,
    kind: String,
    server: String,
    flags: Vec<String>,
}

fn parse_ip(value: &str) -> Option<String> {
    if value.contains('%') {
        return None;
    }
    let addr: IpAddr = value.parse().ok()?;
    let unmapped = match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    Some(unmapped.to_string())
}

#[derive(Clone, Copy)]
enum Cidr {
    V4 { net: u32, mask: u32 },
    V6 { net: u128, mask: u128 },
}

impl Cidr {
    fn parse(spec: &str) -> Self {
        let (addr_s, bits_s) = spec.split_once('/').expect("valid cidr spec");
        let bits: u32 = bits_s.parse().expect("valid prefix length");
        match addr_s.parse::<IpAddr>().expect("valid cidr ip") {
            IpAddr::V4(ip) => {
                let mask = if bits == 0 { 0 } else { !0u32 << (32 - bits) };
                Cidr::V4 {
                    net: u32::from(ip) & mask,
                    mask,
                }
            }
            IpAddr::V6(ip) => {
                let mask = if bits == 0 { 0 } else { !0u128 << (128 - bits) };
                Cidr::V6 {
                    net: u128::from(ip) & mask,
                    mask,
                }
            }
        }
    }

    fn contains(&self, addr: IpAddr) -> bool {
        match (self, addr) {
            (Cidr::V4 { net, mask }, IpAddr::V4(ip)) => (u32::from(ip) & *mask) == *net,
            (Cidr::V6 { net, mask }, IpAddr::V6(ip)) => (u128::from(ip) & *mask) == *net,
            _ => false,
        }
    }
}

fn non_public_cidrs() -> &'static [Cidr] {
    static CIDRS: OnceLock<Vec<Cidr>> = OnceLock::new();
    CIDRS.get_or_init(|| {
        [
            "0.0.0.0/8",
            "100.64.0.0/10",
            "192.0.0.0/24",
            "192.0.2.0/24",
            "192.88.99.0/24",
            "198.18.0.0/15",
            "198.51.100.0/24",
            "203.0.113.0/24",
            "240.0.0.0/4",
            "::/96",
            "64:ff9b::/96",
            "64:ff9b:1::/48",
            "100::/64",
            "2001::/23",
            "2001:db8::/32",
            "2002::/16",
            "3fff::/20",
            "5f00::/16",
            "fec0::/10",
        ]
        .into_iter()
        .map(Cidr::parse)
        .collect()
    })
}

fn is_v6_unique_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

fn is_v6_unicast_link_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

fn public_ip(value: &str) -> bool {
    if value.contains('%') {
        return false;
    }
    let Ok(raw) = value.parse::<IpAddr>() else {
        return false;
    };
    let addr = match raw {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    match addr {
        IpAddr::V4(v4) => {
            if v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4 == Ipv4Addr::UNSPECIFIED
            {
                return false;
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback()
                || is_v6_unique_local(v6)
                || is_v6_unicast_link_local(v6)
                || v6.is_multicast()
                || v6.is_unspecified()
                || v6 == Ipv6Addr::UNSPECIFIED
            {
                return false;
            }
        }
    }
    !non_public_cidrs().iter().any(|c| c.contains(addr))
}

fn borrowed_server_allowed(server: &str) -> bool {
    if server.is_empty() {
        return true;
    }
    parse_ip(server).is_some() && public_ip(server)
}

fn ascii_domain(value: &str) -> Result<String, CmdError> {
    let name = if value.is_ascii() {
        value.to_owned()
    } else {
        idna::domain_to_ascii(value).map_err(|_| kit::fail("域名格式无效"))?
    };
    if name.len() > 253 || !domain_re().is_match(&name) {
        return Err(kit::fail("域名格式无效"));
    }
    Ok(name)
}

fn valid_server(server: &str) -> bool {
    parse_ip(server).is_some() || (server.len() <= 253 && server_re().is_match(server))
}

fn parse_args(input: &[String]) -> Result<Query, CmdError> {
    let mut flags = Vec::new();
    let mut servers = Vec::new();
    let mut rest = Vec::new();
    for value in input {
        if value.starts_with('+') {
            let flag = value.to_ascii_lowercase();
            if !OUTPUT_FLAGS.contains(&flag.as_str()) {
                return Err(kit::fail(format!(
                    "不支持的选项 {value}，只能用 {}",
                    OUTPUT_FLAGS.join(" ")
                )));
            }
            if !flags.contains(&flag) {
                flags.push(flag);
            }
        } else if let Some(s) = value.strip_prefix('@') {
            servers.push(s.to_owned());
        } else {
            rest.push(value.clone());
        }
    }
    if servers.len() > 1 {
        return Err(kit::fail("只能指定一个 DNS 服务器"));
    }
    if rest.is_empty() {
        return Err(kit::fail("请写上要查的域名"));
    }
    if rest.len() > 3 || (servers.len() == 1 && rest.len() > 2) {
        return Err(kit::fail("参数过多"));
    }
    let mut q = Query {
        name: String::new(),
        reverse: false,
        kind: "A".into(),
        server: String::new(),
        flags,
    };
    if rest.len() > 1 {
        q.kind = rest[1].to_ascii_uppercase();
        if !RECORD_TYPES.contains(&q.kind.as_str()) {
            return Err(kit::fail(format!(
                "不支持的记录类型 {}，可用 {}",
                rest[1],
                RECORD_TYPES.join(" ")
            )));
        }
    }
    if let Some(ip) = parse_ip(&rest[0]) {
        if rest.len() > 1 && q.kind != "PTR" {
            return Err(kit::fail("IP 只能反查 PTR 记录"));
        }
        q.name = ip;
        q.kind = "PTR".into();
        q.reverse = true;
    } else {
        q.name = ascii_domain(&rest[0])?;
    }
    if servers.len() == 1 {
        q.server = servers.remove(0);
        if q.server.is_empty() {
            return Err(kit::fail("DNS 服务器格式无效"));
        }
    } else if rest.len() == 3 {
        q.server = rest.remove(2);
    }
    if !q.server.is_empty() && !valid_server(&q.server) {
        return Err(kit::fail("DNS 服务器格式无效，写 IP 或主机名"));
    }
    Ok(q)
}

fn build_args(q: &Query) -> Vec<String> {
    let mut args = Vec::new();
    if !q.server.is_empty() {
        args.push(format!("@{}", q.server));
    }
    if q.reverse {
        args.push("-x".into());
        args.push(q.name.clone());
    } else {
        args.push(q.name.clone());
        args.push(q.kind.clone());
    }
    if q.flags.is_empty() {
        args.push("+short".into());
    } else {
        args.extend(q.flags.iter().cloned());
    }
    args.push("+time=3".into());
    args.push("+tries=2".into());
    args
}

#[derive(Debug)]
enum DigRunError {
    NoDig,
    Timeout,
    Exit { code: Option<i32>, stderr: String, stdout: String },
    Io(std::io::Error),
}

impl std::fmt::Display for DigRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DigRunError::NoDig => f.write_str("dig not found"),
            DigRunError::Timeout => f.write_str("dig timed out"),
            DigRunError::Io(e) => write!(f, "dig: {e}"),
            DigRunError::Exit { code, stderr, stdout } => write!(
                f,
                "dig exit {:?}: {}",
                code,
                kit::truncate_runes(&format!("{stderr} {stdout}"), 300)
            ),
        }
    }
}

impl std::error::Error for DigRunError {}

fn binary_path() -> Option<&'static str> {
    ["/usr/bin/dig", "/bin/dig", "/usr/sbin/dig", "/sbin/dig"]
        .into_iter()
        .find(|&p| std::path::Path::new(p).exists())
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 4096];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let room = limit.saturating_sub(out.len());
        if room == 0 {
            truncated = true;
        } else if n > room {
            out.extend_from_slice(&buf[..room]);
            truncated = true;
        } else {
            out.extend_from_slice(&buf[..n]);
        }
    }
    Ok((out, truncated))
}

async fn run_dig(cancel: &CancellationToken, args: &[String]) -> Result<(Vec<u8>, bool), DigRunError> {
    let path = binary_path().unwrap_or("dig");
    let mut cmd = ProcCommand::new(path);
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(DigRunError::NoDig),
        Err(e) => return Err(DigRunError::Io(e)),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let work = async {
        let (out_res, err_res, status_res) =
            tokio::join!(read_capped(stdout, MAX_OUTPUT), read_capped(stderr, MAX_STDERR), child.wait());
        let (out, truncated) = out_res.map_err(DigRunError::Io)?;
        let (err_bytes, _) = err_res.map_err(DigRunError::Io)?;
        let status = status_res.map_err(DigRunError::Io)?;
        if !status.success() {
            return Err(DigRunError::Exit {
                code: status.code(),
                stderr: String::from_utf8_lossy(&err_bytes).trim().to_owned(),
                stdout: String::from_utf8_lossy(&out).into_owned(),
            });
        }
        Ok((out, truncated))
    };

    tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            Err(DigRunError::Timeout)
        }
        res = tokio::time::timeout(RUN_TIMEOUT, work) => match res {
            Ok(inner) => inner,
            Err(_) => {
                let _ = child.kill().await;
                Err(DigRunError::Timeout)
            }
        }
    }
}

fn explain(err: DigRunError) -> CmdError {
    match err {
        DigRunError::NoDig => kit::fail("主机上没有 dig，装上 dnsutils（Debian/Ubuntu）或 bind-utils 后再试"),
        DigRunError::Timeout => kit::fail("查询超时，稍后再试或换一个 DNS 服务器"),
        DigRunError::Exit { code: Some(9), .. } => kit::fail("DNS 服务器没有应答，稍后再试或换一个服务器"),
        DigRunError::Exit { ref stderr, ref stdout, .. }
            if stderr.contains("couldn't get address") || stdout.contains("couldn't get address") =>
        {
            kit::fail("解析不了指定的 DNS 服务器")
        }
        other => kit::fail_with("DNS 查询失败", other),
    }
}

fn valid_utf8_drop_invalid(mut bytes: &[u8]) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if let Ok(s) = std::str::from_utf8(&bytes[..valid]) {
                    out.push_str(s);
                }
                match err.error_len() {
                    Some(n) => bytes = &bytes[valid + n..],
                    None => break,
                }
            }
        }
    }
    out
}

fn trim_output(output: &[u8], truncated: bool) -> String {
    let slice = if truncated
        && let Some(idx) = output.iter().rposition(|&b| b == b'\n')
    {
        &output[..idx]
    } else {
        output
    };
    valid_utf8_drop_invalid(slice)
        .trim_end_matches(['\n', '\r', '\t', ' '])
        .to_owned()
}

fn line_address(line: &str) -> Option<String> {
    let value = line.trim();
    if let Some(ip) = parse_ip(value) {
        return Some(ip);
    }
    let caps = answer_line_re().captures(value)?;
    parse_ip(caps.get(1)?.as_str())
}

fn addresses(output: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in output.split('\n') {
        let Some(ip) = line_address(line) else {
            continue;
        };
        if !public_ip(&ip) || !seen.insert(ip.clone()) {
            continue;
        }
        found.push(ip);
        if found.len() == MAX_LOOKUPS {
            break;
        }
    }
    found
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Location {
    status: String,
    country: String,
    #[serde(rename = "regionName")]
    region_name: String,
    city: String,
    #[serde(rename = "as")]
    as_field: String,
    query: String,
}

impl Location {
    fn label(&self) -> String {
        if self.status != "success" {
            return String::new();
        }
        let mut parts: Vec<String> = Vec::new();
        for raw in [&self.country, &self.region_name, &self.city] {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && !parts.iter().any(|p| p == trimmed) {
                parts.push(trimmed.to_owned());
            }
        }
        let as_trimmed = self.as_field.trim();
        if !as_trimmed.is_empty() {
            parts.push(truncate(as_trimmed, 60));
        }
        parts.join(" · ")
    }
}

fn parse_locations(body: &[u8]) -> Result<HashMap<String, String>, String> {
    let results: Vec<Location> =
        serde_json::from_slice(body).map_err(|e| format!("invalid location response: {e}"))?;
    let mut labels = HashMap::new();
    for res in results {
        let Some(ip) = parse_ip(&res.query) else {
            continue;
        };
        let label = res.label();
        if !label.is_empty() {
            labels.insert(ip, label);
        }
    }
    Ok(labels)
}

async fn locate_url(ips: &[String], url: &str) -> Result<HashMap<String, String>, CmdError> {
    if ips.is_empty() {
        return Ok(HashMap::new());
    }
    let resp = post_json(
        Request {
            url: url.to_owned(),
            timeout: Some(Duration::from_secs(8)),
            max_bytes: Some(256 << 10),
            ..Default::default()
        },
        &ips,
    )
    .await?;
    if !resp.ok() {
        return Err(Box::new(HttpxError::Status(resp.status)));
    }
    parse_locations(&resp.body).map_err(|e| e.into())
}

fn annotate(output: &str, locations: &HashMap<String, String>) -> String {
    if locations.is_empty() {
        return output.to_owned();
    }
    let mut out = Vec::new();
    for line in output.split('\n') {
        out.push(line.to_owned());
        if let Some(ip) = line_address(line)
            && let Some(label) = locations.get(&ip)
            && !label.is_empty()
        {
            out.push(format!("  {label}"));
        }
    }
    out.join("\n")
}

fn render(q: &Query, output: &str, locations: &HashMap<String, String>, truncated: bool) -> String {
    let mut head = format!("🧭 <b>DNS 查询</b>\n{} · {}", code(&q.name), code(&q.kind));
    if !q.server.is_empty() {
        head.push_str(&format!(" · {}", code(&format!("@{}", q.server))));
    }
    if output.is_empty() {
        return format!("{head}\n\n没有记录");
    }
    let body = escape(&annotate(output, locations));
    let mut text = format!("{head}\n\n<pre>{body}</pre>");
    if truncated {
        text.push_str("\n⚠️ 输出超过 32 KB，后面的省略了");
    }
    text
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let c = |text: &str| format!("<code>{p}{text}</code>");
    format!(
        "🧭 <b>DNS 查询</b>\n\n用主机上的 dig 查 DNS 记录，结果里的 IP 附上归属地和 AS 号。\n\n\
         • {} 查 A 记录\n\
         • {} 指定记录类型\n\
         • {} 指定 DNS 服务器，也可以写成第三个参数\n\
         • {} 指定输出选项\n\
         • {} 反查 IP 的 PTR 记录\n\n\
         <b>参数</b>\n\
         • 记录类型：{}\n\
         • 输出选项：{}，默认 {}\n\
         • DNS 服务器写 IP 或主机名；借用时只能写公网 IP\n\n\
         归属地来自 ip-api.com。主机上要装有 dig（dnsutils 或 bind-utils）。",
        c("dig example.com"),
        c("dig example.com MX"),
        c("dig example.com MX @1.1.1.1"),
        c("dig example.com MX +noall +answer"),
        c("dig 8.8.8.8"),
        RECORD_TYPES.join("、"),
        code(&OUTPUT_FLAGS.join(" ")),
        code("+short"),
    )
}

type RunnerFn = dyn Fn(&CancellationToken, &[String]) -> BoxFuture<'static, Result<(Vec<u8>, bool), DigRunError>>
    + Send
    + Sync;

async fn lookup_with(
    cancel: &CancellationToken,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    q: &Query,
    runner: &RunnerFn,
    batch_url: &str,
) -> Result<String, CmdError> {
    let args = build_args(q);
    let (output, truncated) = runner(cancel, &args).await.map_err(explain)?;
    let text = trim_output(&output, truncated);
    let ips = addresses(&text);
    let locations = match locate_url(&ips, batch_url).await {
        Ok(m) => m,
        Err(err) => {
            kit::opt_log(
                logs,
                mibot_logtail::Level::Warn,
                "dig.locate_failed",
                &[("error", &err.to_string())],
            );
            HashMap::new()
        }
    };
    Ok(render(q, &text, &locations, truncated))
}

async fn handle(inv: &Invocation, app: &App) -> CmdResult {
    if inv.args.is_empty() {
        return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
    }
    let q = parse_args(&inv.args)?;
    if inv.foreign && !borrowed_server_allowed(&q.server) {
        return Err(kit::fail("借用时只能用公网 IP 指定 DNS 服务器"));
    }
    inv.edit_text(&kit::working(&format!("正在查询 {}", q.name))).await?;
    let runner = |cancel: &CancellationToken, args: &[String]| {
        let cancel = cancel.clone();
        let args = args.to_vec();
        Box::pin(async move { run_dig(&cancel, &args).await }) as BoxFuture<'static, _>
    };
    let text = lookup_with(&inv.cancel, Some(&app.logs), &q, &runner, BATCH_API).await?;
    inv.respond_pages(&html_pages(&text, PAGE_LIMIT)).await?;
    Ok(())
}

pub fn register(app: &Arc<App>) {
    let app_ref = Arc::clone(app);
    app.registry.register(Command {
        name: "dig".into(),
        group: group::TOOLS.into(),
        description: "查询 DNS 记录".into(),
        usage: "域名 [类型] [@服务器] [+选项…]".into(),
        help_fn: Some(Arc::new(help)),
        timeout: Some(Duration::from_secs(60)),
        handle: Arc::new(move |inv| {
            let a = Arc::clone(&app_ref);
            Box::pin(async move { handle(&inv, &a).await })
        }),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use mibot_bot::parse_html;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    async fn spawn_http_once(
        status: &str,
        body: &'static str,
        seen: Arc<std::sync::Mutex<Option<String>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_owned();
        let handle = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
                if let Some((_, body_part)) = raw.split_once("\r\n\r\n") {
                    *seen.lock().unwrap() = Some(body_part.to_owned());
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn test_parse_args_public_ip_and_render() {
        let tail = ["+time=3", "+tries=2"];
        let good = [
            (vec!["example.com"], vec!["example.com", "A", "+short"]),
            (vec!["example.com", "mx"], vec!["example.com", "MX", "+short"]),
            (vec!["example.com", "MX", "@1.1.1.1"], vec!["@1.1.1.1", "example.com", "MX", "+short"]),
            (vec!["@1.1.1.1", "example.com"], vec!["@1.1.1.1", "example.com", "A", "+short"]),
            (vec!["example.com", "MX", "dns.google"], vec!["@dns.google", "example.com", "MX", "+short"]),
            (vec!["example.com", "MX", "+noall", "+ANSWER", "+noall"], vec!["example.com", "MX", "+noall", "+answer"]),
            (vec!["_dmarc.example.com.", "TXT"], vec!["_dmarc.example.com.", "TXT", "+short"]),
            (vec!["_sip._tcp.example.com", "srv"], vec!["_sip._tcp.example.com", "SRV", "+short"]),
            (vec!["example.com", "AAAA", "@2606:4700:4700::1111"], vec!["@2606:4700:4700::1111", "example.com", "AAAA", "+short"]),
            (vec!["例子.中国"], vec!["xn--fsqu00a.xn--fiqs8s", "A", "+short"]),
            (vec!["8.8.8.8"], vec!["-x", "8.8.8.8", "+short"]),
            (vec!["2001:4860:4860::8888", "PTR", "@1.1.1.1"], vec!["@1.1.1.1", "-x", "2001:4860:4860::8888", "+short"]),
        ];
        for (input, want) in good {
            let args: Vec<String> = input.into_iter().map(str::to_owned).collect();
            let q = parse_args(&args).unwrap();
            let got = build_args(&q);
            let expected: Vec<String> = want.into_iter().chain(tail).map(str::to_owned).collect();
            assert_eq!(got, expected);
        }

        let long64 = format!("{}.com", "a".repeat(64));
        let rejects: &[&[&str]] = &[
            &["-x"], &["-f/etc/passwd"], &["--help"], &["example.com;id"], &["$(id).com"], &["example"],
            &["exa mple.com"], &["example.com", "ANY"], &["example.com", "AXFR"], &["example.com", "-t"],
            &["example.com", "+trace"], &["example.com", "+time=1"], &["example.com", "+short;id"],
            &["example.com", "A", "@1.1.1.1", "@8.8.8.8"], &["example.com", "A", "1.1.1.1", "extra"],
            &["example.com", "A", "@1.1.1.1", "x"], &["example.com", "A", "@"], &["example.com", "A", "@-p5353"],
            &["example.com", "A", "-p5353"], &["example.com", "A", "@dns_bad.example"],
            &["8.8.8.8", "MX"], &["fe80::1%eth0"], &["@1.1.1.1"], &[&long64],
        ];
        for case in rejects {
            let args: Vec<String> = case.iter().map(|s| (*s).to_owned()).collect();
            let err = parse_args(&args).unwrap_err();
            assert!(kit::is_user_error(err.as_ref()), "case={case:?}");
        }

        for (val, want) in [
            ("1.1.1.1", true), ("8.8.8.8", true), ("93.184.215.14", true), ("100.63.255.255", true),
            ("100.128.0.0", true), ("198.17.255.255", true), ("198.20.0.0", true), ("223.255.255.255", true),
            ("2606:4700:4700::1111", true), ("2001:4860:4860::8888", true), ("2400:cb00::1", true), ("64:ff9c::1", true),
            ("10.1.2.3", false), ("172.16.0.1", false), ("192.168.1.1", false), ("127.0.0.1", false),
            ("169.254.169.254", false), ("224.0.0.1", false), ("0.0.0.0", false), ("::", false), ("::1", false),
            ("fe80::1", false), ("fd00::1", false), ("fc00::1", false), ("ff02::1", false),
            ("0.1.2.3", false), ("100.64.0.1", false), ("192.0.0.1", false), ("192.0.2.1", false),
            ("192.88.99.1", false), ("198.18.0.1", false), ("198.51.100.7", false), ("203.0.113.9", false),
            ("240.0.0.1", false), ("255.255.255.255", false), ("::10.0.0.1", false), ("::ffff:10.0.0.1", false),
            ("64:ff9b::a00:1", false), ("64:ff9b:1::1", false), ("100::1", false), ("2001::1", false),
            ("2001:db8::1", false), ("2002:a00:1::1", false), ("3fff::1", false), ("5f00::1", false),
            ("fec0::1", false), ("", false), ("dns.google", false), ("fe80::1%eth0", false),
        ] {
            assert_eq!(public_ip(val), want, "ip={val}");
        }
        assert!(borrowed_server_allowed(""));
        assert!(borrowed_server_allowed("1.1.1.1"));
        assert!(!borrowed_server_allowed("127.0.0.53"));
        assert!(!borrowed_server_allowed("dns.google"));
        assert!(!borrowed_server_allowed("64:ff9b::a00:1"));

        let short_out = "93.184.215.14\n93.184.215.14\n10.0.0.8\nalias.example.net.\n2606:2800:21f:cb07:6820:80da:af6b:8b2c\n";
        assert_eq!(addresses(short_out), vec!["93.184.215.14", "2606:2800:21f:cb07:6820:80da:af6b:8b2c"]);
        let answer_out = "example.com.\t\t3600\tIN\tA\t93.184.215.14\nexample.com.\t\t3600\tIN\tMX\t10 mail.example.com.\nwww.example.com.\t300\tIN\tAAAA\t2606:2800:21f:cb07:6820:80da:af6b:8b2c\n";
        assert_eq!(addresses(answer_out), vec!["93.184.215.14", "2606:2800:21f:cb07:6820:80da:af6b:8b2c"]);

        let batch = r#"[
         {"status":"success","country":"美国","regionName":"马萨诸塞州","city":"Boston","as":"AS15133 Edgecast Inc.","query":"93.184.215.14"},
         {"status":"success","country":"新加坡","regionName":"新加坡","city":"新加坡","as":"AS13335 Cloudflare, Inc.","query":"2606:2800:21f:cb07:6820:80da:af6b:8b2c"},
         {"status":"fail","message":"private range","query":"10.0.0.8"}
        ]"#
        .as_bytes();
        let labels = parse_locations(batch).unwrap();
        assert_eq!(labels["93.184.215.14"], "美国 · 马萨诸塞州 · Boston · AS15133 Edgecast Inc.");
        assert_eq!(labels["2606:2800:21f:cb07:6820:80da:af6b:8b2c"], "新加坡 · AS13335 Cloudflare, Inc.");
        let annotated = annotate(short_out.trim(), &labels);
        assert!(annotated.contains("93.184.215.14\n  美国 · 马萨诸塞州"));
        assert!(!annotated.contains("10.0.0.8\n "));

        assert_eq!(trim_output(b"line one\nl", true), "line one");
        assert_eq!(trim_output(b"good\n\xe5\xa5", true), "good");
        assert_eq!(trim_output(b"\xe5\xa5", false), "");

        let long = "93.184.215.14\n".repeat(2000);
        let q = parse_args(&["example.com".into()]).unwrap();
        for page in html_pages(&render(&q, long.trim(), &HashMap::new(), false), PAGE_LIMIT) {
            parse_html(&page, None).unwrap();
        }
    }

    #[tokio::test]
    async fn test_lookup_with_mock_dig_and_http() {
        let batch_resp = r#"[{"status":"success","country":"美国","regionName":"马萨诸塞州","city":"Boston","as":"AS15133 Edgecast Inc.","query":"93.184.215.14"},{"status":"success","country":"新加坡","regionName":"新加坡","city":"新加坡","as":"AS13335 Cloudflare, Inc.","query":"2606:2800:21f:cb07:6820:80da:af6b:8b2c"}]"#;
        let seen = Arc::new(std::sync::Mutex::new(None));
        let (url, jh) = spawn_http_once("200 OK", batch_resp, Arc::clone(&seen)).await;
        let ran = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let ran_ref = Arc::clone(&ran);
        let runner = move |_cancel: &CancellationToken, args: &[String]| {
            *ran_ref.lock().unwrap() = args.to_vec();
            Box::pin(async move {
                Ok((b"93.184.215.14\n10.0.0.8\n2606:2800:21f:cb07:6820:80da:af6b:8b2c\n".to_vec(), false))
            }) as BoxFuture<'static, Result<(Vec<u8>, bool), DigRunError>>
        };
        let q = parse_args(&["example.com".into(), "@1.1.1.1".into()]).unwrap();
        let text = lookup_with(&CancellationToken::new(), None, &q, &runner, &url).await.unwrap();
        let _ = jh.await;
        assert_eq!(ran.lock().unwrap()[0], "@1.1.1.1");
        assert!(text.contains("Edgecast") && text.contains("新加坡 · AS13335"));
        let body = seen.lock().unwrap().clone().unwrap();
        assert!(body.contains("93.184.215.14") && !body.contains("10.0.0.8"));
        parse_html(&text, None).unwrap();

        // Failure cases redact paths/details cleanly
        let err = explain(DigRunError::Exit {
            code: Some(10),
            stderr: "dig: couldn't get address for 'nope.invalid': not found".into(),
            stdout: String::new(),
        });
        assert!(err.to_string().contains("解析不了"));
        let err2 = explain(DigRunError::Exit {
            code: Some(9),
            stderr: String::new(),
            stdout: "timed out".into(),
        });
        assert!(err2.to_string().contains("没有应答"));
        let err3 = explain(DigRunError::Exit {
            code: Some(10),
            stderr: "/usr/lib/x.so: bad".into(),
            stdout: String::new(),
        });
        assert!(!err3.to_string().contains("/usr"));
    }
}
