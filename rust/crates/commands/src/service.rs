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
        notes.push(format!("峰值 {}", kit::format_bytes(peak as i64)));
    }
    if let Some(high) = number(s, "MemoryHigh") {
        notes.push(format!("限流线 {}", kit::format_bytes(high as i64)));
    }
    if let Some(limit) = number(s, "MemoryMax") {
        notes.push(format!("上限 {}", kit::format_bytes(limit as i64)));
    }
    if !notes.is_empty() {
        text.push_str(&format!("（{}）", notes.join(" · ")));
    }
    text
}

fn report(name: &str, origin: &str, s: &Status, booted: Duration) -> String {
    let row = |label: &str, value: &str| format!("• {label}：{value}");
    let id = s.get("Id").map(String::as_str).unwrap_or("");
    let mut title = format!("⚙️ <b>服务状态</b>\n{}", code(kit::or_default(id, name)));
    if !origin.is_empty() {
        title.push_str(&format!("（{origin}）"));
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
        state.push_str(&format!("（{}）", translate_sub(sub)));
    }
    let mut rows = vec![row("状态", &escape(&state))];
    let res_str = s.get("Result").map(String::as_str).unwrap_or("");
    if active == "failed" || (active == "inactive" && !res_str.is_empty() && res_str != "success") {
        let mut detail = code(kit::or_default(res_str, "未知"));
        if let Some(exit_code) = number(s, "ExecMainStatus")
            && exit_code != 0
        {
            match s.get("ExecMainCode").map(String::as_str).unwrap_or("") {
                "2" | "3" => detail.push_str(&format!("（信号 {exit_code}）")),
                _ => detail.push_str(&format!("（退出码 {exit_code}）")),
            }
        }
        rows.push(row("上次结果", &detail));
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
            rows.push(row("运行时间", &code(&format_uptime(val))));
        }
    }
    if let Some(pid) = number(s, "MainPID")
        && pid > 0
    {
        rows.push(row("主进程 PID", &code(&pid.to_string())));
    }
    if let Some(current) = number(s, "TasksCurrent") {
        let mut val = current.to_string();
        if let Some(limit) = number(s, "TasksMax") {
            val.push_str(&format!(" / {limit}"));
        }
        rows.push(row("任务数", &code(&val)));
    }
    let memory = memory_text(s);
    if !memory.is_empty() {
        rows.push(row("内存", &memory));
    }
    if let Some(cpu_ns) = number(s, "CPUUsageNSec") {
        let mut val = code(&format_uptime(Duration::from_nanos(cpu_ns)));
        if let Some(al) = alive
            && !al.is_zero()
        {
            let pct = (cpu_ns as f64) / (al.as_nanos() as f64) * 100.0;
            val.push_str(&format!("（平均 {pct:.1}%）"));
        }
        rows.push(row("CPU 时间", &val));
    }
    if let Some(restarts) = number(s, "NRestarts") {
        rows.push(row("自动重启", &code(&format!("{restarts} 次"))));
    }
    if let Some(file) = s.get("UnitFileState")
        && !file.is_empty()
    {
        rows.push(row("开机启动", &escape(translate_file(file))));
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

async fn run_systemctl(cancel: &CancellationToken, args: &[String]) -> Result<String, SysctlErr> {
    let bin_path = systemctl_path()?;
    let args_owned = args.to_vec();
    let child_cancel = cancel.child_token();
    let res = tokio::time::timeout(
        RUN_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let mut cmd = std::process::Command::new(&bin_path);
            cmd.args(&args_owned)
                .env_clear()
                .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
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
            if !status.success() {
                return Err(SysctlErr::Failed(format!(
                    "exit {status}: {}",
                    kit::truncate_runes(err_s.trim(), 300)
                )));
            }
            Ok(out_s)
        }),
    )
    .await;

    match res {
        Ok(Ok(inner)) => inner,
        Ok(Err(join_err)) => Err(SysctlErr::Failed(join_err.to_string())),
        Err(_) => Err(SysctlErr::Timeout),
    }
}

fn no_systemd(msg: &str) -> bool {
    msg.contains("not been booted with systemd") || msg.contains("Failed to connect to bus")
}

fn detect(a: &App) -> (String, &'static str) {
    if let Ok(content) = std::fs::read_to_string("/proc/self/cgroup")
        && let Some(name) = cgroup_unit(&content)
    {
        return (name, "当前进程");
    }
    let name = unit_name(a.env.get("MIBOT_SERVICE", "mibot-lite.service"))
        .unwrap_or_else(|| "mibot-lite.service".to_owned());
    (name, "默认")
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
            return Err(kit::fail_err("主机上没有 systemctl，查不了服务状态"));
        }
        Err(SysctlErr::Timeout) => {
            return Err(kit::fail_err("systemctl 没有应答，稍后再试"));
        }
        Err(SysctlErr::Failed(msg)) if no_systemd(&msg) => {
            return Err(kit::fail_err("连不上 systemd，主机可能没有用 systemd 管理服务"));
        }
        Err(e) => {
            return Err(kit::fail_with_err("读取服务状态失败", e));
        }
    };
    let s = parse_show(&raw);
    match s.get("LoadState").map(String::as_str).unwrap_or("") {
        "not-found" => {
            if !origin.is_empty() {
                Err(kit::fail_err(format!(
                    "没有叫 {name} 的服务，用 {prefix}service 服务名 指定"
                )))
            } else {
                Err(kit::fail_err(format!("没有叫 {name} 的服务")))
            }
        }
        "" => Err(kit::fail_err("systemctl 没有返回服务状态")),
        _ => Ok(report(name, origin, &s, booted)),
    }
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "⚙️ <b>服务状态</b>\n\n查看 systemd 服务的运行状态、运行时间、内存和 CPU。\n\n\
         • <code>{p}service</code> 查看本程序所在的服务\n\
         • <code>{p}service 服务名</code> 查看指定服务，如 {}\n\n\
         不写后缀时按 .service 查，也可以写 .timer、.socket 等其他单元。需要主机上有 systemd。",
        code(&format!("{prefix}service ssh"))
    )
}

pub fn register(a: &Arc<App>) {
    let app = Arc::clone(a);
    let mut cmd: Command = kit::make_cmd(
        "service",
        &[],
        group::SYSTEM,
        "查看 systemd 服务状态",
        "[服务名]",
        help,
        move |inv: Invocation| {
            let app = Arc::clone(&app);
            Box::pin(async move {
                if inv.args.len() > 1 {
                    return Err(kit::usage(inv.prefix(), "service [服务名]"));
                }
                let (name, origin) = if !inv.arg(0).is_empty() {
                    let Some(checked) = unit_name(inv.arg(0)) else {
                        return Err(kit::fail_err("服务名只能有字母、数字和 - _ . @ :"));
                    };
                    (checked, "")
                } else {
                    detect(&app)
                };
                inv.edit_text(&kit::working(&format!("正在查询 {name}")))
                    .await?;
                let args = show_args(&name);
                let out = run_systemctl(&inv.cancel, &args).await;
                let text = inspect_with::<(), ()>(inv.prefix(), &name, origin, host_uptime(), out)?;
                inv.edit(&text).await.map_err(Into::into)
            })
        },
    );
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
        let text = report("mibot-lite.service", "当前进程", &parse_show(RUNNING), Duration::from_secs(7200));
        for wanted in [
            "⚙️ <b>服务状态</b>\n<code>mibot-lite.service</code>（当前进程）\nMiBot Lite",
            "• 状态：活跃（运行中）",
            "• 运行时间：<code>01:00:00</code>",
            "• 主进程 PID：<code>4242</code>",
            "• 任务数：<code>12 / 4915</code>",
            "• 内存：<code>30.0 MB</code>（峰值 45.0 MB · 上限 512.0 MB）",
            "• CPU 时间：<code>00:00:36</code>（平均 1.0%）",
            "• 自动重启：<code>2 次</code>",
            "• 开机启动：开启",
        ] {
            assert!(text.contains(wanted), "missing {wanted} in:\n{text}");
        }
        assert!(!text.contains("限流线") && !text.contains("上次结果"));
        parse_html(&text, None).unwrap();
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
            "• 状态：失败",
            "• 上次结果：<code>exit-code</code>（退出码 1）",
            "• 开机启动：关闭",
        ] {
            assert!(text.contains(wanted), "missing {wanted} in:\n{text}");
        }
        for unwanted in ["/srv", "运行时间", "PID", "内存", "CPU", "任务数"] {
            assert!(!text.contains(unwanted), "unexpected {unwanted} in:\n{text}");
        }
        let killed = report(
            "k.service",
            "",
            &parse_show("Id=k.service\nLoadState=loaded\nActiveState=failed\nSubState=failed\nResult=signal\nExecMainCode=2\nExecMainStatus=9\n"),
            Duration::from_secs(3600),
        );
        assert!(killed.contains("<code>signal</code>（信号 9）"));
        let dead = report(
            "y.service",
            "",
            &parse_show("Id=y.service\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\nUnitFileState=static\n"),
            Duration::from_secs(3600),
        );
        assert!(dead.contains("• 状态：未运行（已停止）") && !dead.contains("上次结果") && dead.contains("静态"));
        let odd = report("z.service", "", &parse_show(RUNNING), Duration::from_secs(1800));
        assert!(!odd.contains("运行时间") && !odd.contains("平均"));
    }

    #[test]
    fn test_inspect_errors_and_limited_buf() {
        let ok = inspect_with::<(), ()>(".", "mibot-lite.service", "当前进程", Duration::from_secs(7200), Ok(RUNNING.into())).unwrap();
        assert!(ok.contains("运行时间：<code>01:00:00</code>"));

        for (out, origin, want) in [
            (Ok("Id=nope.service\nLoadState=not-found\nActiveState=inactive\n".into()), "", "没有叫 nope.service 的服务"),
            (Ok("Id=nope.service\nLoadState=not-found\n".into()), "默认", ".service 服务名 指定"),
            (Err(SysctlErr::NoSystemctl), "", "没有 systemctl"),
            (Err(SysctlErr::Timeout), "", "没有应答"),
            (Err(SysctlErr::Failed("exit 1: System has not been booted with systemd as init system (PID 1).".into())), "", "连不上 systemd"),
            (Err(SysctlErr::Failed("exit 1: Access denied at /run/x".into())), "", "读取服务状态失败"),
            (Ok("garbage".into()), "", "没有返回服务状态"),
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
}
