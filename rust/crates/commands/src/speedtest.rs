use crate::kit;
use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use mibot_app::App;
use mibot_bot::MediaOptions;
use mibot_command::{CmdError, CmdResult, Invocation, code, escape, group};
use mibot_httpx::{Request, do_request, get_json};
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tar::Archive;
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const OOKLA_VERSION: &str = "1.2.0";

fn ookla_spec() -> Option<(&'static str, &'static str)> {
    match std::env::consts::ARCH {
        "x86_64" => Some((
            "x86_64",
            "5690596c54ff9bed63fa3732f818a05dbc2db19ad36ed68f21ca5f64d5cfeeb7",
        )),
        "aarch64" => Some((
            "aarch64",
            "3953d231da3783e2bf8904b6dd72767c5c6e533e163d3742fd0437affa431bd3",
        )),
        _ => None,
    }
}

#[derive(Debug, Default, Clone)]
struct Reading {
    source: String,
    latency: Duration,
    jitter: Duration,
    download: f64,
    upload: f64,
    download_bytes: f64,
    upload_bytes: f64,
    server: String,
    server_id: i32,
    isp: String,
    link: String,
    external_ip: String,
    timestamp: String,
    asn: String,
    country: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OoklaResult {
    timestamp: String,
    ping: OoklaPing,
    download: OoklaTransfer,
    upload: OoklaTransfer,
    isp: String,
    server: OoklaServer,
    result: OoklaResUrl,
    #[serde(rename = "interface")]
    iface: OoklaIface,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OoklaPing {
    latency: f64,
    jitter: f64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OoklaTransfer {
    bandwidth: f64,
    bytes: f64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OoklaServer {
    id: i32,
    name: String,
    location: String,
    country: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OoklaResUrl {
    url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OoklaIface {
    #[serde(rename = "externalIp")]
    external_ip: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PythonResult {
    download: f64,
    upload: f64,
    ping: f64,
    timestamp: String,
    bytes_sent: f64,
    bytes_received: f64,
    server: PythonServer,
    client: PythonClient,
    share: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PythonServer {
    id: serde_json::Value,
    name: String,
    country: String,
    sponsor: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PythonClient {
    ip: String,
    isp: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct SpeedServer {
    id: i32,
    host: String,
    name: String,
    location: String,
    country: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolKind {
    Ookla,
    Python,
}

impl ToolKind {
    fn as_str(&self) -> &'static str {
        match self {
            ToolKind::Ookla => "ookla",
            ToolKind::Python => "python",
        }
    }
}

#[derive(Debug)]
struct CliFailure {
    kind: &'static str,
    exit_desc: String,
    detail: String,
    tested: i32,
    tested_name: String,
    timed_out: bool,
    explained: String,
}

impl std::fmt::Display for CliFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed", self.kind)?;
        if self.tested > 0 {
            write!(f, " on server {}", self.tested)?;
        }
        write!(f, ": {}", self.exit_desc)?;
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        Ok(())
    }
}

impl std::error::Error for CliFailure {}

impl CliFailure {
    fn retryable(&self) -> bool {
        if self.timed_out {
            return false;
        }
        match reason_for(&self.detail) {
            Some(r) => r.retry,
            None => true,
        }
    }
}

struct CliReason {
    markers: &'static [&'static str],
    en: &'static str,
    zh: &'static str,
    retry: bool,
}

const CLI_REASONS: &[CliReason] = &[
    CliReason {
        markers: &["Limit reached", "Too many requests"],
        en: "Too many requests; rate-limited by Speedtest, try again later",
        zh: "测得太频繁，被 Speedtest 限流了，稍后再试",
        retry: false,
    },
    CliReason {
        markers: &["NoServersException", "No servers defined"],
        en: "Specified server not found; ID may be invalid or offline",
        zh: "找不到指定的测速服务器，ID 可能不对或已经下线",
        retry: true,
    },
    CliReason {
        markers: &["Configuration"],
        en: "Speedtest temporarily rejected requests from this host (usually rate-limiting); try again later",
        zh: "Speedtest 暂时拒绝了这台主机的请求，通常是短时间内测得太频繁，稍后再试",
        retry: false,
    },
    CliReason {
        markers: &["Cannot read from socket", "Latency test failed"],
        en: "Cannot reach this server right now; try another ID or auto-select",
        zh: "这台测速服务器现在连不上，换一个 ID 或用自动挑选",
        retry: true,
    },
    CliReason {
        markers: &["Cannot read:", "Cannot write:", "Connection reset", "Broken pipe"],
        en: "Connection dropped mid-test; try again later or pin a different server",
        zh: "测速服务器测到一半断开了连接，稍后再试，或换一台测速服务器固定下来",
        retry: true,
    },
];

fn reason_for(detail: &str) -> Option<&'static CliReason> {
    CLI_REASONS
        .iter()
        .find(|r| r.markers.iter().any(|m| detail.contains(m)))
}

fn explain_cli(detail: &str) -> &'static str {
    match reason_for(detail) {
        Some(r) => kit::tr(r.en, r.zh),
        None => kit::tr("Speedtest tool failed; see logs for details", "测速工具报错了，详情见日志"),
    }
}

fn log_stamp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\[[^\]]*\]\s*)+").expect("valid log stamp regex"))
}

fn failure_detail(stdout: &str, stderr: &str) -> String {
    let mut reasons = Vec::new();
    let mut seen = HashSet::new();
    let mut prose = Vec::new();
    let mut add = |text: &str| {
        let t = text.trim();
        if !t.is_empty() && seen.insert(t.to_owned()) {
            reasons.push(t.to_owned());
        }
    };
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct LogRec {
        error: String,
        message: String,
        level: String,
    }
    for line in stdout.split('\n').chain(stderr.split('\n')) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<LogRec>(line) else {
            let cleaned = log_stamp_re().replace(line, "").into_owned();
            prose.push(cleaned);
            continue;
        };
        if !rec.error.is_empty() {
            add(&rec.error);
        } else if !rec.message.is_empty() && rec.level != "info" && rec.level != "debug" {
            add(&rec.message);
        }
    }
    add(&prose.join(" "));
    let detail = reasons.join("; ");
    mibot_command::truncate(&detail, 300)
}

fn ookla_cache() -> &'static std::sync::Mutex<HashMap<String, bool>> {
    static MAP: OnceLock<std::sync::Mutex<HashMap<String, bool>>> = OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

enum ProbeErr {
    NotOokla(String),
    Exec(String),
}

async fn probe_version(path: &Path, home: &Path) -> Result<String, ProbeErr> {
    std::fs::create_dir_all(home).map_err(|e| ProbeErr::Exec(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
    }
    let mut cmd = TokioCommand::new(path);
    cmd.arg("--version")
        .env("HOME", home)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| ProbeErr::Exec("deadline exceeded".into()))?
        .map_err(|e| ProbeErr::Exec(e.to_string()))?;
    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let first_raw = stdout_str.trim().lines().next().unwrap_or("").trim();
    let mut first = first_raw.to_owned();
    if first.len() > 160 {
        let mut idx = 160;
        while !first.is_char_boundary(idx) {
            idx -= 1;
        }
        first.truncate(idx);
    }
    if !out.status.success() {
        return Err(ProbeErr::Exec(format!("exit status {}", out.status)));
    }
    if !stdout_str.to_lowercase().contains("speedtest by ookla") {
        return Err(ProbeErr::NotOokla(first));
    }
    Ok(first)
}

async fn is_ookla(path: &Path, home: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = format!("{}|{}|{mtime}", path.display(), meta.len());
    if let Some(&known) = ookla_cache().lock().unwrap().get(&key) {
        return known;
    }
    let ok = probe_version(path, home).await.is_ok();
    ookla_cache().lock().unwrap().insert(key, ok);
    ok
}

fn find_in_path(name: &str, path_env: Option<&str>) -> Option<PathBuf> {
    let owned;
    let raw = match path_env {
        Some(p) => p,
        None => {
            owned = std::env::var_os("PATH")?.to_string_lossy().into_owned();
            &owned
        }
    };
    for dir in std::env::split_paths(raw) {
        let cand = dir.join(name);
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (meta.permissions().mode() & 0o111) != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

async fn external_tool_with_path(
    data_dir: &Path,
    path_env: Option<&str>,
) -> Option<(PathBuf, ToolKind)> {
    let home = data_dir.join("speedtest");
    if let Some(p) = find_in_path("speedtest", path_env)
        && is_ookla(&p, &home).await
    {
        return Some((p, ToolKind::Ookla));
    }
    let local = home.join("speedtest");
    if is_executable(&local) {
        return Some((local, ToolKind::Ookla));
    }
    if let Some(p) = find_in_path("speedtest-cli", path_env) {
        return Some((p, ToolKind::Python));
    }
    None
}

fn extract_ookla(archive: &[u8], target: &Path) -> Result<PathBuf, CmdError> {
    let gz = GzDecoder::new(archive);
    let mut tar = Archive::new(gz);
    let entries = tar.entries()?;
    for entry_res in entries {
        let mut entry = entry_res?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path_in_tar = entry.path()?;
        let name_str = path_in_tar.to_string_lossy();
        let file_name = path_in_tar.file_name().and_then(|s| s.to_str());
        if file_name != Some("speedtest") || name_str.contains('/') || name_str.contains('\\') {
            continue;
        }
        let dest = target.join("speedtest");
        let part = target.join("speedtest.part");
        let mut bytes = Vec::new();
        entry.by_ref().take(64 << 20).read_to_end(&mut bytes)?;
        mibot_fsutil::write_file_atomic(&part, &bytes, 0o700)?;
        std::fs::rename(&part, &dest)?;
        return Ok(dest);
    }
    Err(kit::fail(kit::tr(
        "No executable found in Ookla CLI archive",
        "Ookla CLI 压缩包里没有可执行文件",
    )))
}

async fn install_ookla(cancel: &CancellationToken, data_dir: &Path) -> Result<PathBuf, CmdError> {
    let Some((arch, digest)) = ookla_spec() else {
        let msg = if kit::is_zh() {
            format!("没有 {} 架构的 Ookla CLI 构件", std::env::consts::ARCH)
        } else {
            format!("No Ookla CLI binary for architecture {}", std::env::consts::ARCH)
        };
        return Err(kit::fail(msg));
    };
    let url = format!(
        "https://install.speedtest.net/app/cli/ookla-speedtest-{OOKLA_VERSION}-linux-{arch}.tgz"
    );
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(kit::fail(kit::tr("Operation cancelled", "操作已取消"))),
        r = do_request(Request {
            url,
            timeout: Some(Duration::from_secs(90)),
            max_bytes: Some(16 << 20),
            ..Default::default()
        }) => r?,
    };
    if !resp.ok() {
        let msg = if kit::is_zh() {
            format!("下载 Ookla CLI 失败：HTTP {}", resp.status)
        } else {
            format!("Failed to download Ookla CLI: HTTP {}", resp.status)
        };
        return Err(kit::fail(msg));
    }
    let sum = hex::encode(Sha256::digest(&resp.body));
    if sum != digest {
        return Err(kit::fail(kit::tr(
            "Ookla CLI SHA-256 mismatch, discarded",
            "Ookla CLI 的 SHA-256 与预期不符，已丢弃",
        )));
    }
    let target = data_dir.join("speedtest");
    std::fs::create_dir_all(&target)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700));
    }
    extract_ookla(&resp.body, &target)
}

fn tested_server(output: &[u8]) -> (i32, String) {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Rec {
        #[serde(rename = "type")]
        kind: String,
        server: OoklaServer,
    }
    let s = String::from_utf8_lossy(output);
    for line in s.lines() {
        if let Ok(rec) = serde_json::from_str::<Rec>(line.trim())
            && rec.kind == "testStart"
        {
            let where_s = format!("{} {}", rec.server.name, rec.server.location)
                .trim()
                .to_owned();
            return (rec.server.id, where_s);
        }
    }
    (0, String::new())
}

fn parse_result(kind: &ToolKind, output: &[u8]) -> Result<Reading, CmdError> {
    let text = String::from_utf8_lossy(output);
    let trimmed = text.trim();
    let lines: Vec<&str> = trimmed.lines().collect();
    let mut chosen = lines.last().copied().unwrap_or("").trim();
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct KindTag {
        #[serde(rename = "type")]
        kind: String,
    }
    for &line in lines.iter().rev() {
        let l = line.trim();
        if let Ok(tag) = serde_json::from_str::<KindTag>(l)
            && tag.kind == "result"
        {
            chosen = l;
            break;
        }
    }
    match kind {
        ToolKind::Ookla => {
            let parsed: OoklaResult = serde_json::from_str(chosen).map_err(|_| {
                kit::fail(kit::tr(
                    "Failed to parse speedtest output",
                    "无法解析 speedtest 的输出",
                ))
            })?;
            let where_s = format!("{} {}", parsed.server.name, parsed.server.location)
                .trim()
                .to_owned();
            Ok(Reading {
                source: "Ookla Speedtest".into(),
                latency: duration_from_millis(parsed.ping.latency),
                jitter: duration_from_millis(parsed.ping.jitter),
                download: parsed.download.bandwidth * 8.0,
                upload: parsed.upload.bandwidth * 8.0,
                download_bytes: parsed.download.bytes,
                upload_bytes: parsed.upload.bytes,
                server: where_s,
                server_id: parsed.server.id,
                isp: parsed.isp,
                link: parsed.result.url,
                external_ip: parsed.iface.external_ip,
                timestamp: parsed.timestamp,
                asn: String::new(),
                country: String::new(),
            })
        }
        ToolKind::Python => {
            let parsed: PythonResult = serde_json::from_str(chosen).map_err(|_| {
                kit::fail(kit::tr(
                    "Failed to parse speedtest-cli output",
                    "无法解析 speedtest-cli 的输出",
                ))
            })?;
            let where_s = format!("{} {}", parsed.server.sponsor, parsed.server.name)
                .trim()
                .to_owned();
            let server_id = match &parsed.server.id {
                serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) as i32,
                serde_json::Value::String(s) => s.parse::<i32>().unwrap_or(0),
                _ => 0,
            };
            Ok(Reading {
                source: "speedtest-cli".into(),
                latency: duration_from_millis(parsed.ping),
                jitter: Duration::ZERO,
                download: parsed.download,
                upload: parsed.upload,
                download_bytes: parsed.bytes_received,
                upload_bytes: parsed.bytes_sent,
                server: where_s,
                server_id,
                isp: parsed.client.isp,
                link: parsed.share.unwrap_or_default(),
                external_ip: parsed.client.ip,
                timestamp: parsed.timestamp,
                asn: String::new(),
                country: String::new(),
            })
        }
    }
}

async fn run_external(
    cancel: &CancellationToken,
    path: &Path,
    kind: &ToolKind,
    home: &Path,
    server: i32,
) -> Result<Reading, Box<CliFailure>> {
    std::fs::create_dir_all(home).map_err(|e| {
        Box::new(CliFailure {
            kind: kind.as_str(),
            exit_desc: e.to_string(),
            detail: String::new(),
            tested: 0,
            tested_name: String::new(),
            timed_out: false,
            explained: String::new(),
        })
    })?;
    let mut args: Vec<String> = match kind {
        ToolKind::Ookla => vec![
            "-f".into(),
            "jsonl".into(),
            "--accept-license".into(),
            "--accept-gdpr".into(),
        ],
        ToolKind::Python => vec!["--json".into(), "--secure".into()],
    };
    if server > 0 {
        args.push(match kind {
            ToolKind::Ookla => "-s".into(),
            ToolKind::Python => "--server".into(),
        });
        args.push(server.to_string());
    }
    let mut cmd = TokioCommand::new(path);
    cmd.args(&args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let out = tokio::select! {
        _ = cancel.cancelled() => {
            return Err(Box::new(CliFailure {
                kind: kind.as_str(),
                exit_desc: "cancelled".into(),
                detail: String::new(),
                tested: 0,
                tested_name: String::new(),
                timed_out: false,
                explained: kit::tr("Operation cancelled", "操作已取消").into(),
            }));
        }
        r = tokio::time::timeout(Duration::from_secs(120), cmd.output()) => {
            match r {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    return Err(Box::new(CliFailure {
                        kind: kind.as_str(),
                        exit_desc: e.to_string(),
                        detail: String::new(),
                        tested: 0,
                        tested_name: String::new(),
                        timed_out: false,
                        explained: String::new(),
                    }));
                }
                Err(_) => {
                    return Err(Box::new(CliFailure {
                        kind: kind.as_str(),
                        exit_desc: "deadline exceeded".into(),
                        detail: String::new(),
                        tested: 0,
                        tested_name: String::new(),
                        timed_out: true,
                        explained: kit::tr(
                            "Speedtest timed out after 2 minutes; try again later or choose another server",
                            "测速两分钟还没测完，已经中止，稍后再试或换一台测速服务器",
                        )
                        .into(),
                    }));
                }
            }
        }
    };

    if !out.status.success() {
        let out_s = String::from_utf8_lossy(&out.stdout);
        let err_s = String::from_utf8_lossy(&out.stderr);
        let detail = failure_detail(&out_s, &err_s);
        let (tested, tested_name) = tested_server(&out.stdout);
        let explained = if detail.is_empty() {
            String::new()
        } else {
            explain_cli(&detail).to_owned()
        };
        let exit_desc = match out.status.code() {
            Some(c) => format!("exit status {c}"),
            None => format!("{}", out.status),
        };
        return Err(Box::new(CliFailure {
            kind: kind.as_str(),
            exit_desc,
            detail,
            tested,
            tested_name,
            timed_out: false,
            explained,
        }));
    }

    parse_result(kind, &out.stdout).map_err(|e| {
        Box::new(CliFailure {
            kind: kind.as_str(),
            exit_desc: e.to_string(),
            detail: String::new(),
            tested: 0,
            tested_name: String::new(),
            timed_out: false,
            explained: e.to_string(),
        })
    })
}

async fn list_servers(
    cancel: &CancellationToken,
    path: &Path,
    home: &Path,
) -> Result<Vec<SpeedServer>, CmdError> {
    std::fs::create_dir_all(home)?;
    let mut cmd = TokioCommand::new(path);
    cmd.args(["-f", "json", "-L", "--accept-license", "--accept-gdpr"])
        .env("HOME", home)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::select! {
        _ = cancel.cancelled() => return Err(kit::fail(kit::tr("Operation cancelled", "操作已取消"))),
        r = tokio::time::timeout(Duration::from_secs(45), cmd.output()) => {
            r.map_err(|_| kit::fail(kit::tr("Timed out fetching server list", "读取测速服务器列表超时")))??
        }
    };
    if !out.status.success() {
        let detail = failure_detail(
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        );
        if !detail.is_empty() {
            let msg = if kit::is_zh() {
                format!("读取测速服务器列表失败：{}", explain_cli(&detail))
            } else {
                format!("Failed to list servers: {}", explain_cli(&detail))
            };
            return Err(kit::fail(msg));
        }
        return Err(format!("speedtest -L failed: {}", out.status).into());
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct ServerList {
        servers: Vec<SpeedServer>,
    }
    let parsed: ServerList = serde_json::from_slice(&out.stdout).map_err(|_| {
        kit::fail(kit::tr(
            "Failed to parse server list",
            "无法解析测速服务器列表",
        ))
    })?;
    if parsed.servers.is_empty() {
        return Err(kit::fail(kit::tr(
            "No available speedtest servers",
            "没有可用的测速服务器",
        )));
    }
    Ok(parsed.servers)
}

const SPEED_LIST_LIMIT: usize = 12;
const SPEED_NAME_LIMIT: usize = 28;

fn render_servers(servers: &[SpeedServer], pinned: i32, prefix: &str) -> String {
    let shown = &servers[..servers.len().min(SPEED_LIST_LIMIT)];
    let mut common = shown.first().map(|s| s.country.as_str()).unwrap_or("");
    for s in shown {
        if s.country != common {
            common = "";
            break;
        }
    }
    let mut header = kit::tr(
        "🌐 <b>Available Speedtest Servers</b>",
        "🌐 <b>可用测速服务器</b>",
    )
    .to_owned();
    if !common.is_empty() {
        header.push_str(&format!(" · {}", escape(common)));
    }
    let mut lines = vec![header, String::new()];
    for s in shown {
        let name = kit::truncate_runes(&s.name, SPEED_NAME_LIMIT);
        let mut line = format!("{} {}", code(&s.id.to_string()), escape(&name));
        let where_s = s.location.trim();
        if !where_s.is_empty() {
            line.push_str(&format!(" · {}", escape(where_s)));
        }
        if common.is_empty() && !s.country.is_empty() {
            line.push_str(&format!(" {}", escape(&s.country)));
        }
        if s.id == pinned {
            line.push_str(" ✅");
        }
        lines.push(line);
    }
    let sample = if pinned > 0 {
        pinned.to_string()
    } else {
        shown[0].id.to_string()
    };
    lines.push(String::new());
    if kit::is_zh() {
        lines.push(format!(
            "<i>{} 测这一台，{} 设为默认</i>",
            escape(&format!("{prefix}speedtest {sample}")),
            escape(&format!("{prefix}speedtest set {sample}")),
        ));
    } else {
        lines.push(format!(
            "<i>{} to test this server, {} to set as default</i>",
            escape(&format!("{prefix}speedtest {sample}")),
            escape(&format!("{prefix}speedtest set {sample}")),
        ));
    }
    lines.join("\n")
}

async fn result_image(cancel: &CancellationToken, link: &str) -> Option<Vec<u8>> {
    if !link.starts_with("https://www.speedtest.net/result/") {
        return None;
    }
    let url = format!("{}.png", link.trim_end_matches(".png"));
    let resp = tokio::select! {
        _ = cancel.cancelled() => return None,
        r = do_request(Request {
            url,
            timeout: Some(Duration::from_secs(25)),
            max_bytes: Some(8 << 20),
            ..Default::default()
        }) => r.ok()?,
    };
    if !resp.ok() || resp.body.len() < 1024 || &resp.body[1..4] != b"PNG" {
        return None;
    }
    Some(resp.body)
}

fn duration_from_millis(ms: f64) -> Duration {
    Duration::from_secs_f64((ms / 1000.0).max(0.0))
}

fn mask_address(address: &str) -> String {
    if address.is_empty() {
        return kit::tr("unknown", "未知").into();
    }
    if address.contains(':') {
        let parts: Vec<&str> = address.split(':').collect();
        if parts.len() > 2 {
            return format!("{}:{}:…", parts[0], parts[1]);
        }
        return "…".into();
    }
    let parts: Vec<&str> = address.split('.').collect();
    if parts.len() != 4 {
        return "…".into();
    }
    format!("{}.{}.x.x", parts[0], parts[1])
}

fn format_speed(bps: f64) -> String {
    if bps >= 1e9 {
        format!("{:.2} Gbps", bps / 1e9)
    } else if bps >= 1e6 {
        format!("{:.1} Mbps", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.1} Kbps", bps / 1e3)
    } else {
        format!("{bps:.0} bps")
    }
}

fn format_latency(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn format_timestamp(value: &str) -> String {
    let trimmed = value.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return format!("{} UTC", dt.with_timezone(&Utc).format("%Y-%m-%d %H:%M:%S"));
    }
    if trimmed.len() > 40 {
        let mut idx = 40;
        while !trimmed.is_char_boundary(idx) {
            idx -= 1;
        }
        return trimmed[..idx].to_owned();
    }
    trimmed.to_owned()
}

fn country_flag(code_s: &str) -> String {
    let b = code_s.as_bytes();
    if b.len() != 2 || !b[0].is_ascii_uppercase() || !b[1].is_ascii_uppercase() {
        return String::new();
    }
    let r1 = char::from_u32(0x1F1E6 + u32::from(b[0] - b'A')).unwrap_or(' ');
    let r2 = char::from_u32(0x1F1E6 + u32::from(b[1] - b'A')).unwrap_or(' ');
    format!("{r1}{r2}")
}

#[derive(Default, Clone)]
struct Egress {
    asn: String,
    country: String,
}

async fn lookup_egress(cancel: &CancellationToken, address: &str) -> Egress {
    if address.parse::<IpAddr>().is_err() {
        return Egress::default();
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Ans {
        status: String,
        #[serde(rename = "as")]
        as_s: String,
        #[serde(rename = "countryCode")]
        country_code: String,
    }
    let endpoint = format!("http://ip-api.com/json/{address}?fields=status,as,countryCode");
    let ans = tokio::select! {
        _ = cancel.cancelled() => return Egress::default(),
        r = get_json::<Ans>(&endpoint, Some(Duration::from_secs(8)), Some(64 << 10)) => match r {
            Ok(v) => v,
            Err(_) => return Egress::default(),
        }
    };
    if ans.status != "success" {
        return Egress::default();
    }
    let mut asn = ans.as_s.trim().split(' ').next().unwrap_or("").to_owned();
    if asn.len() > 32 {
        let mut idx = 32;
        while !asn.is_char_boundary(idx) {
            idx -= 1;
        }
        asn.truncate(idx);
    }
    let mut cc = ans.country_code.trim().to_ascii_uppercase();
    if country_flag(&cc).is_empty() {
        cc.clear();
    }
    Egress { asn, country: cc }
}

fn format_speedtest_help(prefix: &str, tool_line: &str, server_line: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        return format!(
            "🚀 <b>网络测速</b>\n\n测量主机的出口带宽。\n\n\
             • <code>{p}speedtest</code> 完整测速\n\
             • <code>{p}st</code> 同上，简写\n\
             • <code>{p}speedtest list</code> 列出可用测速服务器\n\
             • <code>{p}speedtest ID</code> 只这一次用指定的测速服务器\n\
             • <code>{p}speedtest set ID</code> 设为默认测速服务器\n\
             • <code>{p}speedtest clear</code> 恢复自动挑选\n\
             • <code>{p}speedtest config</code> 看当前设置\n\
             • <code>{p}speedtest diagnose</code> 检查 CLI 能否运行\n\
             • <code>{p}speedtest fix</code> / <code>update</code> 重新下载官方 CLI（版本固定为 {OOKLA_VERSION}）\n\n\
             <b>当前来源</b>\n{}\n\n\
             <b>当前测速服务器</b>\n{}\n\n\
             用 Ookla 官方 CLI 测：它自己挑就近的测速服务器，报得出 ISP，还会给一张结果图。没装的话首次\
             运行会把官方静态构件下载到部署目录（校验 SHA-256，不写系统目录），之后直接复用。\n\n\
             指定的测速服务器测不通时会自动退回自动挑选；自动挑到的测速服务器测到一半断开时，会换最近的另一台重测，\
             两种情况都会在结果里说明。被 Speedtest 限流时不重试。输出里的出口地址会打码。",
            escape(tool_line),
            escape(server_line),
        );
    }
    format!(
        "🚀 <b>Speedtest</b>\n\nMeasure host outbound bandwidth.\n\n\
         • <code>{p}speedtest</code> Run full speedtest\n\
         • <code>{p}st</code> Alias for speedtest\n\
         • <code>{p}speedtest list</code> List available servers\n\
         • <code>{p}speedtest ID</code> Test a specific server once\n\
         • <code>{p}speedtest set ID</code> Pin default server\n\
         • <code>{p}speedtest clear</code> Restore auto-selection\n\
         • <code>{p}speedtest config</code> Show current settings\n\
         • <code>{p}speedtest diagnose</code> Check CLI status\n\
         • <code>{p}speedtest fix</code> / <code>update</code> Re-download official CLI (pinned to {OOKLA_VERSION})\n\n\
         <b>Current CLI</b>\n{}\n\n\
         <b>Current Server</b>\n{}\n\n\
         Uses the official Ookla CLI to pick a nearby server, report ISP, and attach a result image. If missing, the static binary is downloaded into the data directory on first run (SHA-256 verified) without touching system paths.\n\n\
         If a pinned server fails, it falls back to auto-select; if an auto-selected server drops mid-test, it retries with the next nearest server (noted in output). Rate-limit errors are not retried. Egress IP is masked in output.",
        escape(tool_line),
        escape(server_line),
    )
}

fn help_server_line(pinned: i32) -> String {
    if pinned > 0 {
        if kit::is_zh() {
            format!("固定用 {pinned}")
        } else {
            format!("Pinned to {pinned}")
        }
    } else {
        kit::tr("Auto-select nearest server", "自动挑选最近的测速服务器").into()
    }
}

async fn speedtest_help(data_dir: &Path, prefix: &str, pinned: i32) -> String {
    let source = match external_tool_with_path(data_dir, None).await {
        Some((tool, kind)) => {
            if kit::is_zh() {
                format!("使用 {}：{}", kind.as_str(), tool.display())
            } else {
                format!("Using {}: {}", kind.as_str(), tool.display())
            }
        }
        None => kit::tr(
            "Not installed; first run auto-downloads Ookla CLI",
            "未安装，首次运行会自动下载 Ookla 官方 CLI",
        )
        .into(),
    };
    format_speedtest_help(prefix, &source, &help_server_line(pinned))
}

fn render(result: &Reading, elapsed: Duration, note: &str) -> String {
    let zh = kit::is_zh();
    let mut lines = vec![
        kit::tr("🚀 <b>Speedtest</b>", "🚀 <b>网络测速</b>").to_owned(),
        String::new(),
    ];
    if !result.server.is_empty() {
        let mut s = if zh {
            format!("📍 节点：{}", code(&result.server))
        } else {
            format!("📍 Server: {}", code(&result.server))
        };
        if result.server_id > 0 {
            s.push_str(&format!(" · ID {}", code(&result.server_id.to_string())));
        }
        lines.push(s);
    }
    let isp = format!("{} {}", result.isp, result.asn).trim().to_owned();
    if !isp.is_empty() {
        lines.push(format!(
            "{}{}",
            kit::tr("🏢 ISP: ", "🏢 运营商："),
            code(&isp)
        ));
    }
    if !result.external_ip.is_empty() {
        let mut addr = format!(
            "{}{}",
            kit::tr("🌐 Egress: ", "🌐 出口："),
            code(&mask_address(&result.external_ip))
        );
        let flag = country_flag(&result.country);
        if !flag.is_empty() {
            addr.push_str(&format!(" {flag} {}", result.country));
        }
        lines.push(addr);
    }
    lines.push(String::new());
    let mut lat = format!(
        "{}{}",
        kit::tr("⏱ Ping: ", "⏱ 延迟："),
        code(&format_latency(result.latency))
    );
    if !result.jitter.is_zero() {
        if zh {
            lat.push_str(&format!("（抖动 {}）", escape(&format_latency(result.jitter))));
        } else {
            lat.push_str(&format!(" (jitter {})", escape(&format_latency(result.jitter))));
        }
    }
    let transfer = |label: &str, bits: f64, bytes: f64| {
        let mut line = format!("{label}{}", code(&format_speed(bits)));
        if bytes > 0.0 {
            if zh {
                line.push_str(&format!("（共 {}）", escape(&kit::format_bytes(bytes as i64))));
            } else {
                line.push_str(&format!(" ({})", escape(&kit::format_bytes(bytes as i64))));
            }
        }
        line
    };
    lines.push(lat);
    lines.push(transfer(
        kit::tr("⬇️ Download: ", "⬇️ 下载："),
        result.download,
        result.download_bytes,
    ));
    lines.push(transfer(
        kit::tr("⬆️ Upload: ", "⬆️ 上传："),
        result.upload,
        result.upload_bytes,
    ));
    if !result.timestamp.is_empty() {
        lines.push(format!(
            "{}{}",
            kit::tr("🕒 Time: ", "🕒 时间："),
            code(&format_timestamp(&result.timestamp))
        ));
    }
    lines.push(String::new());
    if !note.is_empty() {
        lines.push(format!("<i>{}</i>", escape(note)));
    }
    if !result.link.is_empty() {
        lines.push(format!(
            "🔗 <a href=\"{}\">{}</a>",
            escape(&result.link),
            kit::tr("Details", "详细结果")
        ));
    }
    if zh {
        lines.push(format!(
            "<i>来源 {} · 用时 {}</i>",
            escape(&result.source),
            escape(&format!("{:.1} 秒", elapsed.as_secs_f64()))
        ));
    } else {
        lines.push(format!(
            "<i>Source: {} · {:.1}s</i>",
            escape(&result.source),
            elapsed.as_secs_f64()
        ));
    }
    lines.join("\n")
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SpeedtestDocument {
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    server: i32,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

struct Speedtester {
    data_dir: PathBuf,
    settings: Store<SpeedtestDocument>,
    logs: Option<Arc<mibot_logtail::Ring>>,
    with_egress: bool,
    running: Mutex<()>,
}

impl Speedtester {
    fn home(&self) -> PathBuf {
        self.data_dir.join("speedtest")
    }

    fn pinned(&self) -> i32 {
        self.settings.read().map(|d| d.server).unwrap_or(0)
    }

    async fn ensure_tool(&self, inv: &Invocation) -> Result<(PathBuf, ToolKind), CmdError> {
        if let Some(pair) = external_tool_with_path(&self.data_dir, None).await {
            return Ok(pair);
        }
        inv.edit(kit::tr(
            "🚀 <b>Speedtest</b>\n\nFirst run, downloading official Ookla CLI…",
            "🚀 <b>网络测速</b>\n\n首次运行，正在下载 Ookla 官方 CLI…",
        ))
        .await?;
        match install_ookla(&inv.cancel, &self.data_dir).await {
            Ok(p) => Ok((p, ToolKind::Ookla)),
            Err(err) => {
                kit::opt_log(
                    self.logs.as_ref(),
                    mibot_logtail::Level::Warn,
                    "speedtest.install_failed",
                    &[("error", &err.to_string())],
                );
                Err(err)
            }
        }
    }

    async fn setting(&self, inv: &Invocation) -> Result<bool, CmdError> {
        match inv.arg(0).to_ascii_lowercase().as_str() {
            "config" => {
                let text = speedtest_help(&self.data_dir, inv.prefix(), self.pinned()).await;
                inv.edit(&text).await?;
                Ok(true)
            }
            "clear" | "auto" | "自动" => {
                self.settings.update(|doc| {
                    doc.server = 0;
                    Ok::<(), mibot_store::StoreError>(())
                })?;
                let msg = if kit::is_zh() {
                    format!(
                        "✅ 已恢复自动挑选测速服务器\n<i>想再固定一台，{} 看有哪些</i>",
                        escape(&format!("{}speedtest list", inv.prefix()))
                    )
                } else {
                    format!(
                        "✅ Restored automatic server selection\n<i>Run {} to see available servers</i>",
                        escape(&format!("{}speedtest list", inv.prefix()))
                    )
                };
                inv.edit(&msg).await?;
                Ok(true)
            }
            "set" => {
                let Ok(id) = inv.arg(1).parse::<i32>().into_iter().find(|&n| n > 0).ok_or(()) else {
                    let msg = if kit::is_zh() {
                        format!(
                            "用法：{}speedtest set ID，ID 用 {}speedtest list 查",
                            inv.prefix(),
                            inv.prefix()
                        )
                    } else {
                        format!(
                            "Usage: {}speedtest set ID (find IDs with {}speedtest list)",
                            inv.prefix(),
                            inv.prefix()
                        )
                    };
                    return Err(kit::fail(msg));
                };
                self.settings.update(|doc| {
                    doc.server = id;
                    Ok::<(), mibot_store::StoreError>(())
                })?;
                let msg = if kit::is_zh() {
                    format!(
                        "✅ 默认测速服务器已设为 {}\n<i>测不通会自动退回自动挑选；取消固定用 {}</i>",
                        code(&id.to_string()),
                        escape(&format!("{}speedtest clear", inv.prefix()))
                    )
                } else {
                    format!(
                        "✅ Default server set to {}\n<i>Falls back to auto-select if unreachable; clear with {}</i>",
                        code(&id.to_string()),
                        escape(&format!("{}speedtest clear", inv.prefix()))
                    )
                };
                inv.edit(&msg).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn list(&self, inv: &Invocation) -> CmdResult {
        inv.edit_text(&kit::working(kit::tr(
            "Fetching server list",
            "正在读取测速服务器列表",
        )))
        .await?;
        let (tool, kind) = self.ensure_tool(inv).await.map_err(|e| install_problem(&e))?;
        if kind != ToolKind::Ookla {
            return Err(kit::fail(kit::tr(
                "Only the official Ookla CLI supports listing servers (currently using speedtest-cli)",
                "只有 Ookla 官方 CLI 能列出测速服务器，当前用的是 speedtest-cli",
            )));
        }
        let servers = list_servers(&inv.cancel, &tool, &self.home()).await?;
        inv.edit(&render_servers(&servers, self.pinned(), inv.prefix()))
            .await
            .map_err(Into::into)
    }

    async fn measure(
        &self,
        inv: &Invocation,
        tool: &Path,
        kind: &ToolKind,
        server: i32,
    ) -> Result<(Reading, String), Box<CliFailure>> {
        let failure = match run_external(&inv.cancel, tool, kind, &self.home(), server).await {
            Ok(r) => return Ok((r, String::new())),
            Err(f) => f,
        };
        if !failure.retryable() {
            return Err(failure);
        }
        let mut next = 0i32;
        let mut note = String::new();
        if server > 0 {
            note = if kit::is_zh() {
                format!(
                    "测速服务器 {server} 没测通，已改用自动挑选。换一台用 {}speedtest list，取消固定用 {}speedtest clear",
                    inv.prefix(),
                    inv.prefix()
                )
            } else {
                format!(
                    "Server {server} failed; fell back to auto-select. Pick another with {}speedtest list or clear with {}speedtest clear",
                    inv.prefix(),
                    inv.prefix()
                )
            };
        } else if failure.tested > 0
            && *kind == ToolKind::Ookla
            && let Ok(servers) = list_servers(&inv.cancel, tool, &self.home()).await
        {
            if let Some(cand) = servers.into_iter().find(|c| c.id != failure.tested) {
                next = cand.id;
            }
            if next > 0 {
                let dropped = if failure.tested_name.is_empty() {
                    kit::tr("server", "测速服务器").to_owned()
                } else {
                    failure.tested_name.clone()
                };
                note = if kit::is_zh() {
                    format!(
                        "自动挑到的 {dropped}（{}）没测完，已换一台重测",
                        failure.tested
                    )
                } else {
                    format!(
                        "Auto-selected {dropped} ({}) dropped mid-test; retried with another server",
                        failure.tested
                    )
                };
            }
        }
        kit::opt_log(
            self.logs.as_ref(),
            mibot_logtail::Level::Warn,
            "speedtest.retrying",
            &[
                ("tool", kind.as_str()),
                ("server", &server.to_string()),
                ("next", &next.to_string()),
                ("error", &failure.to_string()),
            ],
        );
        let result = run_external(&inv.cancel, tool, kind, &self.home(), next).await?;
        Ok((result, note))
    }

    async fn diagnose(&self, inv: &Invocation) -> CmdResult {
        let zh = kit::is_zh();
        let mut lines = vec![
            kit::tr("🩺 <b>Speedtest Diagnostics</b>", "🩺 <b>Speedtest 诊断</b>").to_owned(),
            String::new(),
        ];
        let home = self.home();
        if let Some(p) = find_in_path("speedtest", None) {
            describe_binary(&mut lines, kit::tr("System speedtest", "系统 speedtest"), &p, &home).await;
        } else {
            lines.push(
                kit::tr(
                    "System speedtest: not installed",
                    "系统 speedtest: 未安装",
                )
                .into(),
            );
        }
        let local = home.join("speedtest");
        if std::fs::metadata(&local).is_ok() {
            describe_binary(&mut lines, kit::tr("Local CLI", "本地 CLI"), &local, &home).await;
        } else if zh {
            lines.push(format!(
                "本地 CLI: 未安装，首次测速或 {} 时下载",
                code(&format!("{}speedtest fix", inv.prefix()))
            ));
        } else {
            lines.push(format!(
                "Local CLI: not installed (downloaded on first run or {})",
                code(&format!("{}speedtest fix", inv.prefix()))
            ));
        }
        if let Some(p) = find_in_path("speedtest-cli", None) {
            lines.push(format!("speedtest-cli: {}", code(&p.display().to_string())));
        }
        lines.push(String::new());
        if let Some((tool, kind)) = external_tool_with_path(&self.data_dir, None).await {
            if zh {
                lines.push(format!(
                    "测速时使用 {}：{}",
                    escape(kind.as_str()),
                    code(&tool.display().to_string())
                ));
            } else {
                lines.push(format!(
                    "Active tool {}: {}",
                    escape(kind.as_str()),
                    code(&tool.display().to_string())
                ));
            }
        } else if zh {
            lines.push(format!("测速时会先下载 Ookla 官方 CLI {OOKLA_VERSION}"));
        } else {
            lines.push(format!(
                "First run will download Ookla CLI {OOKLA_VERSION}"
            ));
        }
        if zh {
            lines.push(format!(
                "<i>本地 CLI 无法运行时用 {} 重新下载</i>",
                escape(&format!("{}speedtest fix", inv.prefix()))
            ));
        } else {
            lines.push(format!(
                "<i>If local CLI fails to run, re-download with {}</i>",
                escape(&format!("{}speedtest fix", inv.prefix()))
            ));
        }
        inv.edit(&lines.join("\n")).await.map_err(Into::into)
    }

    async fn reinstall(&self, inv: &Invocation, done: &str) -> CmdResult {
        let zh = kit::is_zh();
        let progress = if zh {
            format!("🚀 <b>网络测速</b>\n\n正在重新下载 Ookla 官方 CLI {OOKLA_VERSION}…")
        } else {
            format!("🚀 <b>Speedtest</b>\n\nRe-downloading official Ookla CLI {OOKLA_VERSION}…")
        };
        inv.edit(&progress).await?;
        let path = install_ookla(&inv.cancel, &self.data_dir)
            .await
            .map_err(|e| {
                kit::opt_log(
                    self.logs.as_ref(),
                    mibot_logtail::Level::Warn,
                    "speedtest.install_failed",
                    &[("error", &e.to_string())],
                );
                install_problem(&e)
            })?;
        let version = probe_version(&path, &self.home()).await.map_err(|e| {
            let msg = match e {
                ProbeErr::NotOokla(v) => v,
                ProbeErr::Exec(s) => s,
            };
            let trunc = mibot_command::truncate(&msg, 160);
            if zh {
                kit::fail(format!("下载好的 CLI 无法运行：{trunc}"))
            } else {
                kit::fail(format!("Downloaded CLI failed to run: {trunc}"))
            }
        })?;
        let out = if zh {
            format!(
                "✅ Ookla CLI {done}\n路径：{}\n{}",
                code(&path.display().to_string()),
                escape(&version)
            )
        } else {
            format!(
                "✅ Ookla CLI {done}\nPath: {}\n{}",
                code(&path.display().to_string()),
                escape(&version)
            )
        };
        inv.edit(&out).await.map_err(Into::into)
    }

    async fn handle(&self, inv: &Invocation) -> CmdResult {
        if self.setting(inv).await? {
            return Ok(());
        }
        let first = inv.arg(0).to_ascii_lowercase();
        if first == "diagnose" {
            return self.diagnose(inv).await;
        }
        let Ok(_guard) = self.running.try_lock() else {
            return Err(kit::fail(kit::tr(
                "Another speedtest is already in progress, please wait",
                "已有一个测速在进行，请稍候",
            )));
        };
        match first.as_str() {
            "list" | "servers" | "列表" => return self.list(inv).await,
            "fix" => return self.reinstall(inv, kit::tr("re-downloaded", "已重新下载")).await,
            "update" => {
                let done = if kit::is_zh() {
                    format!("已更新到 {OOKLA_VERSION}")
                } else {
                    format!("updated to {OOKLA_VERSION}")
                };
                return self.reinstall(inv, &done).await;
            }
            _ => {}
        }
        let (mut server, mut once) = (self.pinned(), false);
        if !first.is_empty() {
            let Ok(id) = first.parse::<i32>().into_iter().find(|&n| n > 0).ok_or(()) else {
                let text = speedtest_help(&self.data_dir, inv.prefix(), self.pinned()).await;
                return inv.edit(&text).await.map_err(Into::into);
            };
            server = id;
            once = true;
        }
        let started = Instant::now();
        let (tool, kind) = self.ensure_tool(inv).await.map_err(|e| install_problem(&e))?;
        let running_msg = if kit::is_zh() {
            let where_s = if server > 0 {
                format!("（测速服务器 {server}），约需一分钟…")
            } else {
                "，约需一分钟…".into()
            };
            format!(
                "🚀 <b>网络测速</b>\n\n正在通过 {} 测速{}",
                escape(kind.as_str()),
                escape(&where_s)
            )
        } else {
            let where_s = if server > 0 {
                format!(" (server {server}), may take ~1 min…")
            } else {
                ", may take ~1 min…".into()
            };
            format!(
                "🚀 <b>Speedtest</b>\n\nRunning via {}{}",
                escape(kind.as_str()),
                escape(&where_s)
            )
        };
        inv.edit(&running_msg).await?;
        let (mut result, mut note) = match self.measure(inv, &tool, &kind, server).await {
            Ok(v) => v,
            Err(failure) => {
                kit::opt_log(
                    self.logs.as_ref(),
                    mibot_logtail::Level::Warn,
                    "speedtest.external_failed",
                    &[("tool", kind.as_str()), ("error", &failure.to_string())],
                );
                let msg = if failure.explained.is_empty() {
                    kit::tr("Speedtest failed", "测速失败").into()
                } else if kit::is_zh() {
                    format!("测速失败：{}", failure.explained)
                } else {
                    format!("Speedtest failed: {}", failure.explained)
                };
                return Err(kit::fail(&msg));
            }
        };
        if once && note.is_empty() {
            note = kit::tr(
                "Tested specified server for this run only; default unchanged",
                "本次指定了测速服务器，未改动默认设置",
            )
            .into();
        }
        if self.with_egress && !result.external_ip.is_empty() {
            let eg = lookup_egress(&inv.cancel, &result.external_ip).await;
            result.asn = eg.asn;
            result.country = eg.country;
        }
        deliver(
            inv,
            self.logs.as_ref(),
            &render(&result, started.elapsed(), &note),
            &result.link,
        )
        .await
    }
}

async fn describe_binary(lines: &mut Vec<String>, label: &str, path: &Path, home: &Path) {
    let p_code = code(&path.display().to_string());
    match probe_version(path, home).await {
        Ok(ver) => lines.push(format!("{label}: {p_code}\n  {}", escape(&ver))),
        Err(ProbeErr::NotOokla(_)) => lines.push(format!(
            "{label}: {p_code}\n  {}",
            kit::tr(
                "Not official Ookla CLI (possibly Python speedtest-cli); skipped",
                "不是 Ookla 官方 CLI（可能是 Python 版 speedtest-cli），不会当官方 CLI 用",
            )
        )),
        Err(ProbeErr::Exec(err)) => {
            let trunc = escape(&mibot_command::truncate(&err, 160));
            if kit::is_zh() {
                lines.push(format!("{label}: {p_code}\n  无法运行：{trunc}"));
            } else {
                lines.push(format!("{label}: {p_code}\n  Failed to execute: {trunc}"));
            }
        }
    }
}

fn install_problem(err: &CmdError) -> CmdError {
    let detail = if kit::is_user_error(err.as_ref()) {
        err.to_string()
    } else {
        kit::tr(
            "Network error or checksum verification failed",
            "网络不通或构件无法校验",
        )
        .into()
    };
    if kit::is_zh() {
        kit::fail(format!("无法安装 Speedtest CLI：{detail}。手动装好 speedtest 后再试"))
    } else {
        kit::fail(format!(
            "Failed to install Speedtest CLI: {detail}. Install speedtest manually and retry"
        ))
    }
}

async fn deliver(
    inv: &Invocation,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    text: &str,
    link: &str,
) -> CmdResult {
    let Some(img) = result_image(&inv.cancel, link).await else {
        return inv.edit(text).await.map_err(Into::into);
    };
    let peer = match inv.client.input_peer(&inv.message.peer) {
        Ok(p) => p,
        Err(err) => {
            kit::opt_log(
                logs,
                mibot_logtail::Level::Info,
                "speedtest.peer_unresolved",
                &[("error", &err.to_string())],
            );
            return inv.edit(text).await.map_err(Into::into);
        }
    };
    if let Err(err) = inv
        .client
        .send_photo(
            &peer,
            &img,
            MediaOptions {
                name: "speedtest.png".into(),
                caption: text.to_owned(),
                reply_to: Some(inv.message.reply_to_id).filter(|&id| id > 0),
                ..Default::default()
            },
        )
        .await
    {
        kit::opt_log(
            logs,
            mibot_logtail::Level::Info,
            "speedtest.photo_failed",
            &[("error", &err.to_string())],
        );
        return inv.edit(text).await.map_err(Into::into);
    }
    inv.client.delete_message(&inv.message).await.map_err(Into::into)
}

pub fn convert_mibox(raw: &[u8]) -> Result<Vec<u8>, String> {
    let source: HashMap<String, serde_json::Value> = serde_json::from_slice(raw).map_err(|e| {
        if kit::is_zh() {
            format!("MiBox 测速设置不是 JSON 对象：{e}")
        } else {
            format!("MiBox speedtest config is not a JSON object: {e}")
        }
    })?;
    let mut doc = SpeedtestDocument::default();
    if let Some(val) = source.get("default_server_id") {
        let parsed = match val {
            serde_json::Value::Number(n) => n.as_i64().filter(|_| n.is_i64() || n.is_u64()),
            serde_json::Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        };
        if let Some(id) = parsed
            && id > 0
            && id <= i64::from(i32::MAX)
        {
            doc.server = id as i32;
        }
    }
    let pretty = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    let formatted = pretty.replace("\n  \"", "\n \"");
    let mut bytes = formatted.into_bytes();
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn register(app: &Arc<App>) {
    let data_dir = app.data_dir();
    let tester = Arc::new(Speedtester {
        data_dir,
        settings: kit::new_store(app, "speedtest.json", SpeedtestDocument::default),
        logs: Some(Arc::clone(&app.logs)),
        with_egress: true,
        running: Mutex::new(()),
    });
    let help_dir = tester.data_dir.clone();
    let help_settings = Store::new(tester.settings.path(), SpeedtestDocument::default);
    let h_fn = move |prefix: &str| {
        let pinned = help_settings.read().map(|d| d.server).unwrap_or(0);
        let home = help_dir.join("speedtest");
        let tool_line = if is_executable(&home.join("speedtest")) {
            if kit::is_zh() {
                format!("使用 ookla：{}", home.join("speedtest").display())
            } else {
                format!("Using ookla: {}", home.join("speedtest").display())
            }
        } else {
            kit::tr(
                "Not installed; first run auto-downloads Ookla CLI",
                "未安装，首次运行会自动下载 Ookla 官方 CLI",
            )
            .into()
        };
        format_speedtest_help(prefix, &tool_line, &help_server_line(pinned))
    };

    let mut cmd = kit::make_cmd(
        "speedtest",
        &["st"],
        group::TOOLS,
        "Measure host network speed",
        "[ID|list|set ID|clear|config|diagnose|fix|update]",
        h_fn,
        move |inv| {
            let t = Arc::clone(&tester);
            Box::pin(async move { t.handle(&inv).await })
        },
    )
    .zh(
        "测量主机网络速度",
        "[ID|list|set ID|clear|config|diagnose|fix|update]",
    );
    cmd.timeout = Some(Duration::from_secs(300));
    app.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mibot_bot::{Client, PeerCache, RpcTransport, SentRecorder};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "mibot-speedtest-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_format_mask_and_mibox() {
        assert_eq!(mask_address("43.153.150.179"), "43.153.x.x");
        let v6 = mask_address("2400:cb00:1234:5678::1");
        assert!(v6.ends_with('…') && !v6.contains("5678"));
        assert_eq!(mask_address(""), "unknown");
        assert_eq!(mask_address("nonsense"), "…");

        assert_eq!(format_speed(2.5e9), "2.50 Gbps");
        assert_eq!(format_speed(118.4e6), "118.4 Mbps");
        assert_eq!(format_speed(5400.0), "5.4 Kbps");
        assert_eq!(format_speed(12.0), "12 bps");

        let td = TempDir::new();
        assert!(extract_ookla(b"not a gzip stream", td.path()).is_err());

        for (raw, want) in [
            (
                r#"{"schemaVersion":1,"default_server_id":48463,"preferred_type":"photo","legacyImported":true}"#,
                "{\n \"server\": 48463\n}\n",
            ),
            (
                r#"{"default_server_id":12345,"preferred_type":"txt"}"#,
                "{\n \"server\": 12345\n}\n",
            ),
            (r#"{"default_server_id":"777"}"#, "{\n \"server\": 777\n}\n"),
            (r#"{"schemaVersion":1,"default_server_id":null,"preferred_type":null}"#, "{}\n"),
            (r#"{"default_server_id":-5}"#, "{}\n"),
            (r#"{"default_server_id":1.5}"#, "{}\n"),
            (r#"{}"#, "{}\n"),
        ] {
            let got = convert_mibox(raw.as_bytes()).unwrap();
            assert_eq!(String::from_utf8(got).unwrap(), want, "raw={raw}");
        }
        assert!(convert_mibox(b"not json").is_err());
    }

    #[test]
    fn test_servers_failures_and_results() {
        let servers: Vec<SpeedServer> = (0..SPEED_LIST_LIMIT + 5)
            .map(|i| SpeedServer {
                id: 1000 + i as i32,
                host: String::new(),
                name: "Server".into(),
                location: "Tokyo".into(),
                country: "Japan".into(),
            })
            .collect();
        let txt = render_servers(&servers, 1003, ".");
        assert!(txt.contains("1003") && txt.contains("✅") && !txt.contains(&(1000 + SPEED_LIST_LIMIT).to_string()));
        assert_eq!(txt.lines().count(), SPEED_LIST_LIMIT + 4);
        assert_eq!(txt.matches("Japan").count(), 1);
        assert!(txt.contains(".speedtest 1003"));

        let mixed = render_servers(
            &[
                SpeedServer { id: 1, name: "A".into(), location: "Tokyo".into(), country: "Japan".into(), ..Default::default() },
                SpeedServer { id: 2, name: "B".into(), location: "Seoul".into(), country: "Korea".into(), ..Default::default() },
            ],
            0,
            ".",
        );
        assert!(mixed.contains("Japan") && mixed.contains("Korea"));

        assert_eq!(failure_detail(r#"{"error":"Cannot write: "}"#, ""), "Cannot write:");
        let limited = "[2026-09-26 09:23:48.288] [error] Limit reached:\n\nSpeedtest CLI. Too many requests received.\nplease review and adjust the frequency of your requests.\n";
        let got_lim = failure_detail("", limited);
        assert!(got_lim.starts_with("Limit reached: Speedtest CLI") && got_lim.ends_with("requests."));
        assert_eq!(
            explain_cli("Configuration - No servers defined (NoServersException)"),
            "Specified server not found; ID may be invalid or offline"
        );
        assert!(!explain_cli("signal: aborted").contains("signal"));

        let ookla_out = concat!(
            r#"{"type":"log","message":"x"}"#, "\n",
            r#"{"type":"result","timestamp":"2026-09-22T10:53:22Z","ping":{"latency":1.5,"jitter":0.2},"download":{"bandwidth":25000000,"bytes":298520000},"upload":{"bandwidth":3700000,"bytes":41000000},"isp":"Tencent","interface":{"externalIp":"43.153.150.179"},"server":{"id":48463,"name":"IPA CyberLab 400G","location":"Tokyo","country":"Japan"},"result":{"url":"https://www.speedtest.net/result/c/abc"}}"#,
        );
        let parsed = parse_result(&ToolKind::Ookla, ookla_out.as_bytes()).unwrap();
        assert_eq!(parsed.server_id, 48463);
        assert_eq!(parsed.download_bytes, 298520000.0);
        assert_eq!(parsed.download, 200e6);

        let py_out = r#"{"download":93000000,"upload":12000000,"ping":20.5,"timestamp":"2026-09-22T10:53:22.123456Z","bytes_sent":15000000,"bytes_received":117000000,"server":{"id":"12345","name":"Tokyo","sponsor":"Foo"},"client":{"ip":"1.2.3.4","isp":"Bar"},"share":null}"#;
        let pyp = parse_result(&ToolKind::Python, py_out.as_bytes()).unwrap();
        assert_eq!(pyp.server_id, 12345);
        assert_eq!(pyp.download_bytes, 117000000.0);

        let report = render(
            &Reading {
                source: "Ookla Speedtest".into(),
                latency: Duration::from_micros(1500),
                download: 200e6,
                upload: 29.6e6,
                download_bytes: 298520000.0,
                upload_bytes: 41000000.0,
                server: "IPA CyberLab 400G Tokyo".into(),
                server_id: 48463,
                isp: "Tencent".into(),
                asn: "AS132203".into(),
                external_ip: "43.153.150.179".into(),
                country: "JP".into(),
                timestamp: "2026-09-22T10:53:22Z".into(),
                ..Default::default()
            },
            Duration::from_secs(30),
            "",
        );
        assert!(report.contains("ID <code>48463</code>") && report.contains("43.153.x.x") && !report.contains("150.179"));
        assert_eq!(country_flag("JP"), "🇯🇵");
        assert_eq!(country_flag("jp"), "");
        assert_eq!(format_timestamp("not a time"), "not a time");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_measure_and_external_tool_with_scripts() {
        use std::os::unix::fs::PermissionsExt;
        struct NoopTx;
        impl RpcTransport for NoopTx {
            fn invoke_raw<'a>(
                &'a self,
                _: Option<i32>,
                _: u32,
                _: Vec<u8>,
            ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, grammers_mtsender::InvocationError>> {
                Box::pin(async { Err(grammers_mtsender::InvocationError::Dropped("noop")) })
            }
        }

        let td = TempDir::new();
        let py_dir = td.path().join("py");
        std::fs::create_dir_all(&py_dir).unwrap();
        let py_bin = py_dir.join("speedtest");
        std::fs::write(&py_bin, "#!/bin/sh\necho 'speedtest-cli 2.1.3'\nexit 0\n").unwrap();
        std::fs::set_permissions(&py_bin, std::fs::Permissions::from_mode(0o700)).unwrap();

        let data_dir = td.path().join("data");
        assert!(external_tool_with_path(&data_dir, Some(py_dir.to_str().unwrap())).await.is_none());

        let home = data_dir.join("speedtest");
        std::fs::create_dir_all(&home).unwrap();
        let flaky = home.join("speedtest");
        let flaky_script = r#"#!/bin/sh
echo "$*" >> "$HOME/calls"
if [ -f "$HOME/limited" ]; then
  printf '[2026-09-26 09:23:48.288] [error] Limit reached:\n\nSpeedtest CLI. Too many requests received.\n' >&2
  exit 173
fi
case " $* " in
  *" -L "*)
    echo '{"type":"serverList","servers":[{"id":28910,"name":"fdcservers.net","location":"Tokyo","country":"Japan"},{"id":48463,"name":"IPA CyberLab 400G","location":"Tokyo","country":"Japan"}]}'
    exit 0 ;;
  *" -s 48463 "*)
    echo '{"type":"testStart","server":{"id":48463,"name":"IPA CyberLab 400G","location":"Tokyo"}}'
    echo '{"type":"result","ping":{"latency":1.5},"download":{"bandwidth":25000000},"upload":{"bandwidth":3700000},"server":{"id":48463,"name":"IPA CyberLab 400G","location":"Tokyo"},"result":{"url":""}}'
    exit 0 ;;
esac
echo '{"type":"testStart","server":{"id":28910,"name":"fdcservers.net","location":"Tokyo"}}'
echo '{"error":"Cannot write: "}'
exit 2
"#;
        std::fs::write(&flaky, flaky_script).unwrap();
        std::fs::set_permissions(&flaky, std::fs::Permissions::from_mode(0o700)).unwrap();

        let got_local = external_tool_with_path(&data_dir, Some(py_dir.to_str().unwrap())).await.unwrap();
        assert_eq!(got_local, (flaky.clone(), ToolKind::Ookla));

        let tester = Speedtester {
            data_dir: data_dir.clone(),
            settings: Store::new(data_dir.join("speedtest.json"), SpeedtestDocument::default),
            logs: None,
            with_egress: false,
            running: Mutex::new(()),
        };
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            Arc::new(NoopTx),
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let msg = kit::blank_bot_message(1, grammers_tl_types::enums::Peer::User(grammers_tl_types::types::PeerUser { user_id: 1 }));
        let inv = Invocation {
            client,
            registry: Arc::new(mibot_command::Registry::new()),
            message: msg,
            prefix: ".".into(),
            command: "speedtest".into(),
            args: Vec::new(),
            raw: String::new(),
            cancel: CancellationToken::new(),
            foreign: false,
        };
        let (res, note) = tester.measure(&inv, &flaky, &ToolKind::Ookla, 0).await.unwrap();
        assert_eq!(res.server_id, 48463);
        assert!(note.contains("fdcservers.net Tokyo") && note.contains("28910"));

        std::fs::write(home.join("limited"), b"").unwrap();
        let err = tester.measure(&inv, &flaky, &ToolKind::Ookla, 0).await.unwrap_err();
        assert!(err.explained.contains("rate-limited"));
        let help_txt = speedtest_help(&data_dir, ".", 48463).await;
        for w in [".speedtest list", ".speedtest set", ".speedtest clear", ".speedtest config", ".speedtest diagnose", ".speedtest fix", "update", ".st", "48463"] {
            assert!(help_txt.contains(w), "missing {w}");
        }
    }
}
