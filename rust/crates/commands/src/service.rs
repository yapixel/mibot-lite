use crate::kit;
use mibot_app::App;
use mibot_command::{Command, Invocation, code, escape, group};
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

const RUN_TIMEOUT: Duration = Duration::from_secs(8);

const PROPERTIES: &[&str] = &[
    "Id",
    "Description",
    "LoadState",
    "ActiveState",
    "SubState",
    "UnitFileState",
    "Result",
    "MainPID",
    "NRestarts",
    "ActiveEnterTimestampMonotonic",
    "TasksCurrent",
    "TasksMax",
    "MemoryCurrent",
    "MemoryPeak",
    "MemoryHigh",
    "MemoryMax",
    "CPUUsageNSec",
    "ExecMainCode",
    "ExecMainStatus",
];

const UNIT_SUFFIXES: &[&str] = &[
    ".service",
    ".socket",
    ".timer",
    ".target",
    ".mount",
    ".automount",
    ".path",
    ".swap",
    ".slice",
    ".scope",
    ".device",
];

static UNIT_PATTERN: OnceLock<Regex> = OnceLock::new();

fn unit_pattern() -> &'static Regex {
    UNIT_PATTERN.get_or_init(|| Regex::new(r"^[A-Za-z0-9:_.@\\-]+$").unwrap())
}

pub fn unit_name(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('-')
        || value.starts_with('.')
        || !unit_pattern().is_match(value)
    {
        return None;
    }
    for &suffix in UNIT_SUFFIXES {
        if value.ends_with(suffix) && value.len() > suffix.len() {
            return Some(value.to_owned());
        }
    }
    if value.len() + ".service".len() > 256 {
        return None;
    }
    Some(format!("{value}.service"))
}

pub fn cgroup_unit(content: &str) -> Option<String> {
    for line in content.split('\n') {
        let mut parts = line.trim().splitn(3, ':');
        let _hier = parts.next()?;
        let ctrl = parts.next()?;
        let path = parts.next()?;
        if !ctrl.is_empty() && ctrl != "name=systemd" {
            continue;
        }
        if path.contains("/user@") {
            return None;
        }
        let last = match path.rfind('/') {
            Some(i) => &path[i + 1..],
            None => path,
        };
        if !last.ends_with(".service") {
            continue;
        }
        if let Some(name) = unit_name(last) {
            return Some(name);
        }
    }
    None
}

type Status = HashMap<String, String>;

fn parse_show(output: &str) -> Status {
    let mut values = HashMap::new();
    for line in output.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if let Some((k, v)) = trimmed.split_once('=')
            && !k.is_empty()
        {
            values.insert(k.to_owned(), v.to_owned());
        }
    }
    values
}

fn number(s: &Status, key: &str) -> Option<u64> {
    let val = s.get(key)?.trim().parse::<u64>().ok()?;
    if val == u64::MAX { None } else { Some(val) }
}

fn translate_active(val: &str) -> &str {
    if !kit::is_zh() {
        return val;
    }
    match val {
        "active" => "活跃",
        "reloading" => "重新加载中",
        "inactive" => "未运行",
        "failed" => "失败",
        "activating" => "启动中",
        "deactivating" => "停止中",
        "maintenance" => "维护中",
        "refreshing" => "刷新中",
        other => other,
    }
}

fn translate_sub(val: &str) -> &str {
    if !kit::is_zh() {
        return val;
    }
    match val {
        "running" => "运行中",
        "exited" => "已退出",
        "dead" => "已停止",
        "failed" => "失败",
        "start" | "start-pre" | "start-post" => "启动中",
        "auto-restart" => "等待自动重启",
        "stop" | "stop-sigterm" | "stop-sigkill" | "stop-post" => "停止中",
        "reload" => "重新加载中",
        "listening" => "监听中",
        "waiting" => "等待中",
        "elapsed" => "已触发",
        "mounted" => "已挂载",
        "plugged" => "已接入",
        "active" => "活跃",
        other => other,
    }
}

fn translate_file(val: &str) -> &str {
    if !kit::is_zh() {
        return match val {
            "enabled-runtime" => "enabled (runtime only)",
            "static" => "static (started by another unit)",
            other => other,
        };
    }
    match val {
        "enabled" => "开启",
        "enabled-runtime" => "开启（仅本次开机）",
        "disabled" => "关闭",
        "static" => "静态（由别的单元拉起）",
        "masked" => "已屏蔽",
        "indirect" => "间接",
        "generated" => "自动生成",
        "transient" => "临时",
        "alias" => "别名",
        other => other,
    }
}

fn format_uptime(d: Duration) -> String {
    mibot_sysinfo::format_uptime(d)
}

fn memory_text(s: &Status) -> String {
    let Some(current) = number(s, "MemoryCurrent") else {
        return String::new();
    };
    let mut text = code(&kit::format_bytes(current as i64));
    let mut notes = Vec::new();
    if let Some(peak) = number(s, "MemoryPeak") {
        notes.push(format!("{} {}", kit::tr("peak", "峰值"), kit::format_bytes(peak as i64)));
    }
    if let Some(high) = number(s, "MemoryHigh") {
        notes.push(format!("{} {}", kit::tr("high", "限流线"), kit::format_bytes(high as i64)));
    }
    if let Some(limit) = number(s, "MemoryMax") {
        notes.push(format!("{} {}", kit::tr("limit", "上限"), kit::format_bytes(limit as i64)));
    }
    if !notes.is_empty() {
        if kit::is_zh() {
            text.push_str(&format!("（{}）", notes.join(" · ")));
        } else {
            text.push_str(&format!(" ({})", notes.join(" · ")));
        }
    }
    text
}

fn report(name: &str, origin: &str, s: &Status, booted: Duration) -> String {
    let zh = kit::is_zh();
    let row = |label: &str, value: &str| {
        if zh {
            format!("• {label}：{value}")
        } else {
            format!("• {label}: {value}")
        }
    };
    let id = s.get("Id").map(String::as_str).unwrap_or("");
    let mut title = format!(
        "⚙️ <b>{}</b>\n{}",
        kit::tr("Service Status", "服务状态"),
        code(kit::or_default(id, name))
    );
    if !origin.is_empty() {
        if zh {
            title.push_str(&format!("（{origin}）"));
        } else {
            title.push_str(&format!(" ({origin})"));
        }
    }
    if let Some(desc) = s.get("Description") {
        let trimmed = desc.trim();
        if !trimmed.is_empty() && trimmed != id && !trimmed.contains('/') {
            title.push('\n');
            title.push_str(&escape(trimmed));
        }
    }
    let active = s.get("ActiveState").map(String::as_str).unwrap_or("");
    let sub = s.get("SubState").map(String::as_str).unwrap_or("");
    let mut state = translate_active(active).to_owned();
    if !sub.is_empty() && translate_sub(sub) != state {
        if zh {
            state.push_str(&format!("（{}）", translate_sub(sub)));
        } else {
            state.push_str(&format!(" ({})", translate_sub(sub)));
        }
    }
    let mut rows = vec![row(kit::tr("State", "状态"), &escape(&state))];
    let res_str = s.get("Result").map(String::as_str).unwrap_or("");
    if active == "failed" || (active == "inactive" && !res_str.is_empty() && res_str != "success") {
        let mut detail = code(kit::or_default(res_str, kit::tr("unknown", "未知")));
        if let Some(exit_code) = number(s, "ExecMainStatus")
            && exit_code != 0
        {
            let code_type = s.get("ExecMainCode").map(String::as_str).unwrap_or("");
            if zh {
                match code_type {
                    "2" | "3" => detail.push_str(&format!("（信号 {exit_code}）")),
                    _ => detail.push_str(&format!("（退出码 {exit_code}）")),
                }
            } else {
                match code_type {
                    "2" | "3" => detail.push_str(&format!(" (signal {exit_code})")),
                    _ => detail.push_str(&format!(" (exit code {exit_code})")),
                }
            }
        }
        rows.push(row(kit::tr("Last result", "上次结果"), &detail));
    }
    let running = matches!(active, "active" | "reloading" | "deactivating");
    let mut alive: Option<Duration> = None;
    if running
        && let Some(entered) = number(s, "ActiveEnterTimestampMonotonic")
        && entered > 0
        && !booted.is_zero()
    {
        let entered_dur = Duration::from_micros(entered);
        if let Some(val) = booted.checked_sub(entered_dur) {
            alive = Some(val);
            rows.push(row(kit::tr("Uptime", "运行时间"), &code(&format_uptime(val))));
        }
    }
    if let Some(pid) = number(s, "MainPID")
        && pid > 0
    {
        rows.push(row(kit::tr("Main PID", "主进程 PID"), &code(&pid.to_string())));
    }
    if let Some(current) = number(s, "TasksCurrent") {
        let mut val = current.to_string();
        if let Some(limit) = number(s, "TasksMax") {
            val.push_str(&format!(" / {limit}"));
        }
        rows.push(row(kit::tr("Tasks", "任务数"), &code(&val)));
    }
    let memory = memory_text(s);
    if !memory.is_empty() {
        rows.push(row(kit::tr("Memory", "内存"), &memory));
    }
    if let Some(cpu_ns) = number(s, "CPUUsageNSec") {
        let mut val = code(&format_uptime(Duration::from_nanos(cpu_ns)));
        if let Some(al) = alive
            && !al.is_zero()
        {
            let pct = (cpu_ns as f64) / (al.as_nanos() as f64) * 100.0;
            if zh {
                val.push_str(&format!("（平均 {pct:.1}%）"));
            } else {
                val.push_str(&format!(" (avg {pct:.1}%)"));
            }
        }
        rows.push(row(kit::tr("CPU time", "CPU 时间"), &val));
    }
    if let Some(restarts) = number(s, "NRestarts") {
        let count = if zh {
            format!("{restarts} 次")
        } else {
            restarts.to_string()
        };
        rows.push(row(kit::tr("Restarts", "自动重启"), &code(&count)));
    }
    if let Some(file) = s.get("UnitFileState")
        && !file.is_empty()
    {
        rows.push(row(kit::tr("Autostart", "开机启动"), &escape(translate_file(file))));
    }
    format!("{title}\n\n{}", rows.join("\n"))
}

fn show_args(name: &str) -> Vec<String> {
    vec![
        "show".into(),
        "--no-pager".into(),
        format!("--property={}", PROPERTIES.join(",")),
        "--".into(),
        name.to_owned(),
    ]
}

#[derive(Debug, Error)]
enum SysctlErr {
    #[error("systemctl not found")]
    NoSystemctl,
    #[error("systemctl timed out")]
    Timeout,
    #[error("systemctl: {0}")]
    Failed(String),
}

struct LimitedBuf {
    buf: Vec<u8>,
    limit: usize,
}

impl LimitedBuf {
    fn new(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            limit,
        }
    }
    fn push_slice(&mut self, data: &[u8]) {
        let room = self.limit.saturating_sub(self.buf.len());
        if room > 0 {
            self.buf.extend_from_slice(&data[..data.len().min(room)]);
        }
    }
    fn read_from<R: std::io::Read>(&mut self, mut r: R) {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            self.push_slice(&chunk[..n]);
        }
    }
    fn into_string_lossy(self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitSystem {
    Systemd,
    OpenRc,
}

pub(crate) fn rc_service_path() -> Option<String> {
    for dir in ["/sbin", "/usr/sbin", "/bin", "/usr/bin", "/usr/local/sbin", "/usr/local/bin"] {
        let p = format!("{dir}/rc-service");
        if Path::new(&p).exists() {
            return Some(p);
        }
    }
    None
}

pub(crate) fn detect_init_system() -> Option<InitSystem> {
    if Path::new("/run/systemd/system").exists() {
        return Some(InitSystem::Systemd);
    }
    if Path::new("/run/openrc").exists() {
        return Some(InitSystem::OpenRc);
    }
    if systemctl_path().is_ok() {
        return Some(InitSystem::Systemd);
    }
    if rc_service_path().is_some() {
        return Some(InitSystem::OpenRc);
    }
    None
}

pub(crate) fn openrc_service_name(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('-')
        || value.starts_with('.')
        || !unit_pattern().is_match(value)
    {
        return None;
    }
    let stripped = value.strip_suffix(".service").unwrap_or(value);
    if stripped.is_empty() || stripped.starts_with('-') || stripped.starts_with('.') {
        return None;
    }
    Some(stripped.to_owned())
}

fn systemctl_path() -> Result<String, SysctlErr> {
    if Path::new("/usr/bin/systemctl").exists() {
        return Ok("/usr/bin/systemctl".into());
    }
    for dir in ["/bin", "/usr/local/bin"] {
        let p = format!("{dir}/systemctl");
        if Path::new(&p).exists() {
            return Ok(p);
        }
    }
    Err(SysctlErr::NoSystemctl)
}

async fn run_cmd_limited(
    cancel: &CancellationToken,
    bin_path: String,
    args: &[String],
) -> Result<(std::process::ExitStatus, String, String), SysctlErr> {
    let args_owned = args.to_vec();
    let child_cancel = cancel.child_token();
    let res = tokio::time::timeout(
        RUN_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new(&bin_path);
            cmd.args(&args_owned)
                .env_clear()
                .env("PATH", "/usr/sbin:/sbin:/usr/bin:/bin")
                .env("LC_ALL", "C")
                .env("SYSTEMD_PAGER", "")
                .env("SYSTEMD_COLORS", "0")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| SysctlErr::Failed(e.to_string()))?;
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let t_out = std::thread::spawn(move || {
                let mut lb = LimitedBuf::new(64 << 10);
                if let Some(r) = stdout {
                    lb.read_from(r);
                }
                lb.into_string_lossy()
            });
            let t_err = std::thread::spawn(move || {
                let mut lb = LimitedBuf::new(4 << 10);
                if let Some(r) = stderr {
                    lb.read_from(r);
                }
                lb.into_string_lossy()
            });
            let status = loop {
                if child_cancel.is_cancelled() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = t_out.join();
                    let _ = t_err.join();
                    return Err(SysctlErr::Timeout);
                }
                match child.try_wait() {
                    Ok(Some(st)) => break st,
                    Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = t_out.join();
                        let _ = t_err.join();
                        return Err(SysctlErr::Failed(e.to_string()));
                    }
                }
            };
            let out_s = t_out.join().unwrap_or_default();
            let err_s = t_err.join().unwrap_or_default();
            Ok((status, out_s, err_s))
        }),
    )
    .await;

    match res {
        Ok(Ok(inner)) => inner,
        Ok(Err(join_err)) => Err(SysctlErr::Failed(join_err.to_string())),
        Err(_) => Err(SysctlErr::Timeout),
    }
}

async fn run_systemctl(cancel: &CancellationToken, args: &[String]) -> Result<String, SysctlErr> {
    let bin_path = systemctl_path()?;
    let (status, out_s, err_s) = run_cmd_limited(cancel, bin_path, args).await?;
    if !status.success() {
        return Err(SysctlErr::Failed(format!(
            "exit {status}: {}",
            kit::truncate_runes(err_s.trim(), 300)
        )));
    }
    Ok(out_s)
}

fn no_systemd(msg: &str) -> bool {
    msg.contains("not been booted with systemd") || msg.contains("Failed to connect to bus")
}

fn detect(a: &App) -> (String, &'static str) {
    if let Ok(content) = std::fs::read_to_string("/proc/self/cgroup")
        && let Some(name) = cgroup_unit(&content)
    {
        return (name, kit::tr("current process", "当前进程"));
    }
    let name = unit_name(a.env.get("MIBOT_SERVICE", "mibot-lite.service"))
        .unwrap_or_else(|| "mibot-lite.service".to_owned());
    (name, kit::tr("default", "默认"))
}

fn host_uptime() -> Duration {
    let Ok(content) = std::fs::read_to_string("/proc/uptime") else {
        return Duration::ZERO;
    };
    let Some(first) = content.split_whitespace().next() else {
        return Duration::ZERO;
    };
    first
        .parse::<f64>()
        .ok()
        .filter(|&s| s >= 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(Duration::ZERO)
}

fn inspect_with<F, Fut>(
    prefix: &str,
    name: &str,
    origin: &str,
    booted: Duration,
    output: Result<String, SysctlErr>,
) -> mibot_command::CmdResult<String> {
    let _ = None::<fn(F) -> Fut>;
    let raw = match output {
        Ok(out) => out,
        Err(SysctlErr::NoSystemctl) => {
            return Err(kit::fail_err(kit::tr(
                "systemctl is not available on this host",
                "主机上没有 systemctl，查不了服务状态",
            )));
        }
        Err(SysctlErr::Timeout) => {
            return Err(kit::fail_err(kit::tr(
                "systemctl did not respond, please try again later",
                "systemctl 没有应答，稍后再试",
            )));
        }
        Err(SysctlErr::Failed(msg)) if no_systemd(&msg) => {
            return Err(kit::fail_err(kit::tr(
                "Cannot connect to systemd; host may not use systemd",
                "连不上 systemd，主机可能没有用 systemd 管理服务",
            )));
        }
        Err(e) => {
            return Err(kit::fail_with_err(
                kit::tr("Failed to read service status", "读取服务状态失败"),
                e,
            ));
        }
    };
    let s = parse_show(&raw);
    match s.get("LoadState").map(String::as_str).unwrap_or("") {
        "not-found" => {
            if !origin.is_empty() {
                let msg = if kit::is_zh() {
                    format!("没有叫 {name} 的服务，用 {prefix}service 服务名 指定")
                } else {
                    format!("No service named {name}; specify one with {prefix}service <name>")
                };
                Err(kit::fail_err(msg))
            } else {
                let msg = if kit::is_zh() {
                    format!("没有叫 {name} 的服务")
                } else {
                    format!("No service named {name}")
                };
                Err(kit::fail_err(msg))
            }
        }
        "" => Err(kit::fail_err(kit::tr(
            "systemctl returned no service status",
            "systemctl 没有返回服务状态",
        ))),
        _ => Ok(report(name, origin, &s, booted)),
    }
}

struct OpenRcProc {
    pid: u32,
    rss_bytes: Option<u64>,
    cpu_ns: Option<u64>,
    started_mono_us: Option<u64>,
    threads: Option<u64>,
}

fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse::<u32>().ok().filter(|&p| p > 0)
}

fn find_openrc_pid(name: &str) -> Option<u32> {
    for direct in [
        Path::new("/run").join(format!("{name}.pid")),
        Path::new("/run").join(name).join(format!("{name}.pid")),
    ] {
        if let Some(pid) = read_pid_file(&direct)
            && Path::new(&format!("/proc/{pid}")).exists()
        {
            return Some(pid);
        }
    }
    let sup_pid = read_pid_file(&Path::new("/run").join(format!("supervise-{name}.pid")))?;
    let task_dir = Path::new("/proc").join(sup_pid.to_string()).join("task");
    let entries = std::fs::read_dir(task_dir).ok()?;
    for entry in entries.flatten() {
        if let Ok(children) = std::fs::read_to_string(entry.path().join("children"))
            && let Some(child_pid) = children
                .split_whitespace()
                .filter_map(|s| s.parse::<u32>().ok())
                .find(|&p| p > 0)
        {
            return Some(child_pid);
        }
    }
    None
}

fn parse_proc_metrics(pid: u32, status_raw: &str, stat_raw: &str) -> OpenRcProc {
    let mut rss_bytes = None;
    for line in status_raw.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok());
            rss_bytes = kb.map(|v| v.saturating_mul(1024));
            break;
        }
    }
    let mut cpu_ns = None;
    let mut started_mono_us = None;
    let mut threads = None;
    if let Some(after_comm) = stat_raw.rfind(')').map(|idx| &stat_raw[idx + 1..]) {
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        if fields.len() > 19 {
            let utime = fields[11].parse::<u64>().ok();
            let stime = fields[12].parse::<u64>().ok();
            if let (Some(u), Some(s)) = (utime, stime) {
                cpu_ns = Some(u.saturating_add(s).saturating_mul(10_000_000));
            }
            threads = fields[17].parse::<u64>().ok();
            started_mono_us = fields[19]
                .parse::<u64>()
                .ok()
                .map(|ticks| ticks.saturating_mul(10_000));
        }
    }
    OpenRcProc {
        pid,
        rss_bytes,
        cpu_ns,
        started_mono_us,
        threads,
    }
}

fn openrc_proc_metrics(name: &str) -> Option<OpenRcProc> {
    let pid = find_openrc_pid(name)?;
    let status_raw = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let stat_raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    Some(parse_proc_metrics(pid, &status_raw, &stat_raw))
}

fn parse_openrc_status_word(combined: &str) -> (&'static str, String) {
    for line in combined.lines() {
        if let Some((_, rhs)) = line.split_once("status:") {
            let word = rhs.trim().to_ascii_lowercase();
            return match word.as_str() {
                "started" => ("active", "running".into()),
                "stopped" => ("inactive", "dead".into()),
                "crashed" => ("failed", "failed".into()),
                "starting" => ("activating", "start".into()),
                "stopping" => ("deactivating", "stop".into()),
                _ => ("inactive", word),
            };
        }
    }
    ("inactive", String::new())
}

fn build_openrc_status(
    name: &str,
    combined: &str,
    init_exists: bool,
    enabled: bool,
    proc_info: Option<OpenRcProc>,
) -> Status {
    let mut s = Status::new();
    s.insert("Id".into(), name.to_owned());
    if !init_exists || combined.contains("does not exist") {
        s.insert("LoadState".into(), "not-found".into());
        s.insert("ActiveState".into(), "inactive".into());
        return s;
    }
    s.insert("LoadState".into(), "loaded".into());
    let (active, sub) = parse_openrc_status_word(combined);
    s.insert("ActiveState".into(), active.into());
    if !sub.is_empty() {
        s.insert("SubState".into(), sub);
    }
    s.insert(
        "UnitFileState".into(),
        if enabled { "enabled" } else { "disabled" }.into(),
    );
    if active == "active"
        && let Some(p) = proc_info
    {
        s.insert("MainPID".into(), p.pid.to_string());
        if let Some(rss) = p.rss_bytes {
            s.insert("MemoryCurrent".into(), rss.to_string());
        }
        if let Some(cpu) = p.cpu_ns {
            s.insert("CPUUsageNSec".into(), cpu.to_string());
        }
        if let Some(started) = p.started_mono_us {
            s.insert("ActiveEnterTimestampMonotonic".into(), started.to_string());
        }
        if let Some(t) = p.threads {
            s.insert("TasksCurrent".into(), t.to_string());
        }
    }
    s
}

async fn inspect_openrc(
    cancel: &CancellationToken,
    prefix: &str,
    name: &str,
    origin: &str,
) -> mibot_command::CmdResult<String> {
    let Some(bin_path) = rc_service_path() else {
        return Err(kit::fail_err(kit::tr(
            "Neither rc-service nor systemctl found on this host",
            "主机上没有 rc-service 或 systemctl，查不了服务状态",
        )));
    };
    let init_exists = Path::new("/etc/init.d").join(name).exists();
    if !init_exists {
        let missing = build_openrc_status(name, "", false, false, None);
        let raw = format!("Id={name}\nLoadState={}\n", missing["LoadState"]);
        return inspect_with::<(), ()>(prefix, name, origin, host_uptime(), Ok(raw));
    }
    let args = vec![name.to_owned(), "status".into()];
    let (status, out_s, err_s) = match run_cmd_limited(cancel, bin_path, &args).await {
        Ok(res) => res,
        Err(SysctlErr::Timeout) => {
            return Err(kit::fail_err(kit::tr(
                "rc-service did not respond, please try again later",
                "rc-service 没有应答，稍后再试",
            )));
        }
        Err(e) => {
            return Err(kit::fail_with_err(
                kit::tr("Failed to read service status", "读取服务状态失败"),
                e,
            ));
        }
    };
    let combined = format!("{out_s}\n{err_s}");
    if !status.success() && !combined.contains("status:") && !combined.contains("does not exist") {
        return Err(kit::fail_err(kit::tr(
            "Failed to read service status",
            "读取服务状态失败",
        )));
    }
    let enabled = Path::new("/etc/runlevels/default").join(name).exists();
    let proc_info = openrc_proc_metrics(name);
    let parsed = build_openrc_status(name, &combined, init_exists, enabled, proc_info);
    if parsed.get("LoadState").map(String::as_str) == Some("not-found") {
        let raw = format!("Id={name}\nLoadState=not-found\n");
        return inspect_with::<(), ()>(prefix, name, origin, host_uptime(), Ok(raw));
    }
    Ok(report(name, origin, &parsed, host_uptime()))
}

fn detect_openrc(a: &App) -> (String, &'static str) {
    if let Ok(svc) = std::env::var("RC_SVCNAME")
        && let Some(clean) = openrc_service_name(svc.trim())
    {
        return (clean, kit::tr("current process", "当前进程"));
    }
    let raw = a.env.get("MIBOT_SERVICE", "mibot-lite.service");
    let clean = openrc_service_name(raw).unwrap_or_else(|| "mibot-lite".to_owned());
    (clean, kit::tr("default", "默认"))
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        return format!(
            "⚙️ <b>服务状态</b>\n\n查看系统服务（systemd / OpenRC）的运行状态、运行时间、内存和 CPU。\n\n\
             • <code>{p}service</code> 查看本程序所在的服务\n\
             • <code>{p}service 服务名</code> 查看指定服务，如 {}\n\n\
             systemd 下不写后缀时按 .service 查，也可查 .timer、.socket 等；OpenRC 下会自动匹配 /etc/init.d/ 服务名。",
            code(&format!("{prefix}service ssh"))
        );
    }
    format!(
        "⚙️ <b>Service Status</b>\n\nCheck status, uptime, memory, and CPU of a system service (systemd / OpenRC).\n\n\
         • <code>{p}service</code> Check the service running this bot\n\
         • <code>{p}service name</code> Check a specific service, e.g. {}\n\n\
         On systemd, .service is assumed if omitted; .timer, .socket, etc. also work. On OpenRC, matches /etc/init.d/ service names.",
        code(&format!("{prefix}service ssh"))
    )
}

pub fn register(a: &Arc<App>) {
    let app = Arc::clone(a);
    let mut cmd: Command = kit::make_cmd(
        "service",
        &[],
        group::SYSTEM,
        "Check system service status",
        "[service]",
        help,
        move |inv: Invocation| {
            let app = Arc::clone(&app);
            Box::pin(async move {
                if inv.args.len() > 1 {
                    return Err(kit::usage(
                        inv.prefix(),
                        kit::tr("service [name]", "service [服务名]"),
                    ));
                }
                if detect_init_system() == Some(InitSystem::OpenRc) {
                    let (name, origin) = if !inv.arg(0).is_empty() {
                        let Some(checked) = openrc_service_name(inv.arg(0)) else {
                            return Err(kit::fail_err(kit::tr(
                                "Service name may only contain letters, digits, and - _ . @",
                                "服务名只能有字母、数字和 - _ . @",
                            )));
                        };
                        (checked, "")
                    } else {
                        detect_openrc(&app)
                    };
                    inv.edit_text(&kit::working(&format!(
                        "{}{name}",
                        kit::tr("Checking ", "正在查询 ")
                    )))
                    .await?;
                    let text = inspect_openrc(&inv.cancel, inv.prefix(), &name, origin).await?;
                    return inv.edit(&text).await.map_err(Into::into);
                }
                let (name, origin) = if !inv.arg(0).is_empty() {
                    let Some(checked) = unit_name(inv.arg(0)) else {
                        return Err(kit::fail_err(kit::tr(
                            "Service name may only contain letters, digits, and - _ . @ :",
                            "服务名只能有字母、数字和 - _ . @ :",
                        )));
                    };
                    (checked, "")
                } else {
                    detect(&app)
                };
                inv.edit_text(&kit::working(&format!(
                    "{}{name}",
                    kit::tr("Checking ", "正在查询 ")
                )))
                .await?;
                let args = show_args(&name);
                let out = run_systemctl(&inv.cancel, &args).await;
                let text = inspect_with::<(), ()>(inv.prefix(), &name, origin, host_uptime(), out)?;
                inv.edit(&text).await.map_err(Into::into)
            })
        },
    )
    .zh("查看系统服务状态", "[服务名]");
    cmd.timeout = Some(Duration::from_secs(60));
    a.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mibot_bot::parse_html;

    const RUNNING: &str = "\
Id=mibot-lite.service
Description=MiBot Lite
LoadState=loaded
ActiveState=active
SubState=running
UnitFileState=enabled
Result=success
MainPID=4242
NRestarts=2
ActiveEnterTimestampMonotonic=3600000000
TasksCurrent=12
TasksMax=4915
MemoryCurrent=31457280
MemoryPeak=47185920
MemoryHigh=infinity
MemoryMax=536870912
CPUUsageNSec=36000000000
ExecMainCode=0
ExecMainStatus=0
Unknown=value
";

    #[test]
    fn test_unit_name() {
        for (input, want) in [
            ("ssh", "ssh.service"),
            ("mibot-lite.service", "mibot-lite.service"),
            ("certbot.timer", "certbot.timer"),
            ("getty@tty1.service", "getty@tty1.service"),
            ("docker.socket", "docker.socket"),
            ("nginx.conf", "nginx.conf.service"),
            (r"systemd-fsck@dev-disk-by\x2duuid-1234.service", r"systemd-fsck@dev-disk-by\x2duuid-1234.service"),
            ("dbus-org.freedesktop.login1", "dbus-org.freedesktop.login1.service"),
        ] {
            assert_eq!(unit_name(input).as_deref(), Some(want), "{input:?}");
        }
        let long = "a".repeat(257);
        for bad in [
            "", "-H", "--help", "--host=x", ".service", ".hidden", "a b", "a;b", "a/b",
            "../x", "$(id)", "`id`", "x*", "ssh\n", &long,
        ] {
            assert_eq!(unit_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn test_show_args() {
        let args = show_args("ssh.service");
        assert!(args.len() >= 3);
        assert_eq!(args[0], "show");
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args[args.len() - 1], "ssh.service");
        for a in &args {
            assert!(!a.contains("FragmentPath") && !a.contains("ExecStart="));
        }
    }

    #[test]
    fn test_cgroup_unit() {
        for (content, want) in [
            ("0::/system.slice/mibot-lite.service\n", "mibot-lite.service"),
            ("12:memory:/system.slice/x.service\n1:name=systemd:/system.slice/mibot.service\n0::/\n", "mibot.service"),
            ("0::/system.slice/system-getty.slice/getty@tty1.service", "getty@tty1.service"),
        ] {
            assert_eq!(cgroup_unit(content).as_deref(), Some(want), "{content:?}");
        }
        for content in [
            "",
            "0::/",
            "0::/user.slice/user-1000.slice/session-3.scope",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/mibot.service",
            "0::/docker/0123456789abcdef",
            "12:memory:/system.slice/x.service",
        ] {
            assert_eq!(cgroup_unit(content), None, "{content:?}");
        }
    }

    #[test]
    fn test_report_running() {
        let text = report("mibot-lite.service", "current process", &parse_show(RUNNING), Duration::from_secs(7200));
        for wanted in [
            "⚙️ <b>Service Status</b>\n<code>mibot-lite.service</code> (current process)\nMiBot Lite",
            "• State: active (running)",
            "• Uptime: <code>01:00:00</code>",
            "• Main PID: <code>4242</code>",
            "• Tasks: <code>12 / 4915</code>",
            "• Memory: <code>30.0 MB</code> (peak 45.0 MB · limit 512.0 MB)",
            "• CPU time: <code>00:00:36</code> (avg 1.0%)",
            "• Restarts: <code>2</code>",
            "• Autostart: enabled",
        ] {
            assert!(text.contains(wanted), "missing {wanted} in:\n{text}");
        }
        assert!(!text.contains("high") && !text.contains("Last result"));
        parse_html(&text, None).unwrap();

        mibot_i18n::with_lang(mibot_i18n::Lang::Zh, || {
            let zh = report("mibot-lite.service", "当前进程", &parse_show(RUNNING), Duration::from_secs(7200));
            assert!(zh.contains("⚙️ <b>服务状态</b>\n<code>mibot-lite.service</code>（当前进程）"));
            assert!(zh.contains("• 状态：活跃（运行中）"));
            assert!(zh.contains("• 内存：<code>30.0 MB</code>（峰值 45.0 MB · 上限 512.0 MB）"));
        });
    }

    #[test]
    fn test_report_stopped_and_failed() {
        let failed = parse_show(
            "Id=x.service\nDescription=/srv/app/run.sh wrapper\nLoadState=loaded\nActiveState=failed\nSubState=failed\n\
             Result=exit-code\nMainPID=0\nActiveEnterTimestampMonotonic=100\nMemoryCurrent=[not set]\nCPUUsageNSec=18446744073709551615\n\
             TasksCurrent=18446744073709551615\nExecMainStatus=1\nUnitFileState=disabled\n",
        );
        let text = report("x.service", "", &failed, Duration::from_secs(3600));
        for wanted in [
            "• State: failed",
            "• Last result: <code>exit-code</code> (exit code 1)",
            "• Autostart: disabled",
        ] {
            assert!(text.contains(wanted), "missing {wanted} in:\n{text}");
        }
        for unwanted in ["/srv", "Uptime", "PID", "Memory", "CPU", "Tasks"] {
            assert!(!text.contains(unwanted), "unexpected {unwanted} in:\n{text}");
        }
        let killed = report(
            "k.service",
            "",
            &parse_show("Id=k.service\nLoadState=loaded\nActiveState=failed\nSubState=failed\nResult=signal\nExecMainCode=2\nExecMainStatus=9\n"),
            Duration::from_secs(3600),
        );
        assert!(killed.contains("<code>signal</code> (signal 9)"));
        let dead = report(
            "y.service",
            "",
            &parse_show("Id=y.service\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\nUnitFileState=static\n"),
            Duration::from_secs(3600),
        );
        assert!(dead.contains("• State: inactive (dead)") && !dead.contains("Last result") && dead.contains("static"));
        let odd = report("z.service", "", &parse_show(RUNNING), Duration::from_secs(1800));
        assert!(!odd.contains("Uptime") && !odd.contains("avg"));
    }

    #[test]
    fn test_inspect_errors_and_limited_buf() {
        let ok = inspect_with::<(), ()>(".", "mibot-lite.service", "current process", Duration::from_secs(7200), Ok(RUNNING.into())).unwrap();
        assert!(ok.contains("Uptime: <code>01:00:00</code>"));

        for (out, origin, want) in [
            (Ok("Id=nope.service\nLoadState=not-found\nActiveState=inactive\n".into()), "", "No service named nope.service"),
            (Ok("Id=nope.service\nLoadState=not-found\n".into()), "default", ".service <name>"),
            (Err(SysctlErr::NoSystemctl), "", "systemctl is not available"),
            (Err(SysctlErr::Timeout), "", "did not respond"),
            (Err(SysctlErr::Failed("exit 1: System has not been booted with systemd as init system (PID 1).".into())), "", "Cannot connect to systemd"),
            (Err(SysctlErr::Failed("exit 1: Access denied at /run/x".into())), "", "Failed to read service status"),
            (Ok("garbage".into()), "", "returned no service status"),
        ] {
            let err = inspect_with::<(), ()>(".", "nope.service", origin, Duration::from_secs(7200), out).unwrap_err();
            let msg = mibot_command::brief(err.as_ref());
            assert!(msg.contains(want) && !msg.contains("/run"), "got {msg:?}, want {want:?}");
        }

        let mut lb = LimitedBuf::new(5);
        for chunk in ["abc", "defg", "hij"] {
            lb.push_slice(chunk.as_bytes());
        }
        assert_eq!(lb.into_string_lossy(), "abcde");
    }

    #[test]
    fn test_openrc_parsing_and_proc_metrics() {
        assert_eq!(openrc_service_name("mibot-lite.service").as_deref(), Some("mibot-lite"));
        assert_eq!(openrc_service_name("stat_client").as_deref(), Some("stat_client"));
        assert_eq!(openrc_service_name(".service"), None);
        assert_eq!(openrc_service_name("../bad"), None);

        let proc_info = parse_proc_metrics(
            2142,
            "Name:\tmibot-lite\nVmRSS:\t   28672 kB\nThreads:\t6\n",
            "2142 (mibot lite) S 2141 2141 1 0 -1 4194304 10 0 0 0 150 50 0 0 20 0 6 0 360000 100 200",
        );
        assert_eq!(proc_info.rss_bytes, Some(28672 * 1024));
        assert_eq!(proc_info.cpu_ns, Some(200 * 10_000_000));
        assert_eq!(proc_info.started_mono_us, Some(360_000 * 10_000));
        assert_eq!(proc_info.threads, Some(6));

        let st = build_openrc_status("mibot-lite", " * status: started\n", true, true, Some(proc_info));
        let txt = report("mibot-lite", "default", &st, Duration::from_secs(7200));
        assert!(txt.contains("<code>mibot-lite</code> (default)"));
        assert!(txt.contains("• State: active (running)"));
        assert!(txt.contains("• Main PID: <code>2142</code>"));
        assert!(txt.contains("• Memory: <code>28.0 MB</code>"));
        assert!(txt.contains("• Autostart: enabled"));

        let missing = build_openrc_status("nope", "", false, false, None);
        assert_eq!(missing.get("LoadState").map(String::as_str), Some("not-found"));
    }
}
