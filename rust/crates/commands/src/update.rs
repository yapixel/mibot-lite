use crate::kit;
use crate::restart::Restarter;
use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Asia::Shanghai;
use mibot_app::App;
use mibot_bot::{Client, SendOptions, code, escape, first_sent_message_id};
use mibot_command::{
    CmdError, CmdResult, Command, Invocation, brief, fail, fail_with, group, is_user_error,
    truncate,
};
use mibot_httpx::{Request, do_request, download_file};
use mibot_logtail::Level;
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::process::Command as ProcessCommand;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

const DEFAULT_REPO: &str = "Yapixel/mibot-lite";
const DEFAULT_GITHUB_API: &str = "https://api.github.com";
const MAX_WAIT: Duration = Duration::from_secs(2 * 3600);
const RESTART_WAIT: Duration = Duration::from_secs(30 * 60);
const IDLE_POLL: Duration = Duration::from_secs(10);
const QUIESCE_WAIT: Duration = Duration::from_secs(30);
const RESTART_GRACE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct AutoConfig {
    enabled: bool,
    time: String,
    #[serde(rename = "checkedDate", default, skip_serializing_if = "String::is_empty")]
    checked_date: String,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time: "04:00".to_owned(),
            checked_date: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoAction {
    Idle,
    Wait,
    GiveUp,
    Check,
}

#[derive(Debug)]
struct SwapError {
    text: String,
    cause: String,
}

impl fmt::Display for SwapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.text, self.cause)
    }
}

impl std::error::Error for SwapError {}

fn swap_user_err(text: String, cause: String) -> CmdError {
    let inner: Arc<dyn std::error::Error + Send + Sync> = Arc::new(SwapError {
        text: text.clone(),
        cause,
    });
    Box::new(mibot_command::UserError {
        text,
        cause: Some(inner),
    })
}

static BUSY_LOCK: LazyLock<Arc<AsyncMutex<()>>> =
    LazyLock::new(|| Arc::new(AsyncMutex::new(())));

struct Updater {
    a: Arc<App>,
    restarter: Option<Arc<Restarter>>,
    repo: String,
    github_api: String,
    settings: Store<AutoConfig>,
    binary: Option<PathBuf>,
    running_file_id: Option<(u64, u64)>,
    located_err: Option<String>,
    idle_poll: Duration,
    quiesce_wait: Duration,
    restart_wait: Duration,
}

pub fn register(a: &Arc<App>, restarter: Option<Arc<Restarter>>) {
    let (binary, running_file_id, located_err) = match executable_path() {
        Ok(p) => match fs::metadata(&p) {
            Ok(meta) => (Some(p), Some((meta.dev(), meta.ino())), None),
            Err(e) => (Some(p), None, Some(e.to_string())),
        },
        Err(e) => (None, None, Some(e)),
    };
    let repo = a
        .env
        .get("MIBOT_UPDATE_REPO", DEFAULT_REPO)
        .trim()
        .to_owned();
    let repo = if repo.is_empty() {
        DEFAULT_REPO.to_owned()
    } else {
        repo
    };
    let github_api = a
        .env
        .get("MIBOT_GITHUB_API", DEFAULT_GITHUB_API)
        .trim()
        .trim_end_matches('/')
        .to_owned();
    let github_api = if github_api.is_empty() {
        DEFAULT_GITHUB_API.to_owned()
    } else {
        github_api
    };

    let u = Arc::new(Updater {
        a: Arc::clone(a),
        restarter,
        repo,
        github_api,
        settings: kit::new_store(a, "update.json", AutoConfig::default),
        binary,
        running_file_id,
        located_err,
        idle_poll: IDLE_POLL,
        quiesce_wait: QUIESCE_WAIT,
        restart_wait: RESTART_WAIT,
    });

    {
        let u_job = Arc::clone(&u);
        a.on_ready(Arc::new(move |app, cancel| {
            let u = Arc::clone(&u_job);
            Box::pin(async move {
                u.schedule(app.client.clone(), cancel).await;
            })
        }));
    }

    let u_cmd = Arc::clone(&u);
    a.registry.register(
        Command {
            name: "update".into(),
            group: group::SYSTEM.into(),
            description: "Check and update binary".into(),
            usage: "[check|run|rollback|auto]".into(),
            help_fn: Some(Arc::new(help)),
            timeout: Some(Duration::from_secs(600)),
            handle: Arc::new(move |inv| {
                let u = Arc::clone(&u_cmd);
                Box::pin(async move { u.handle(&inv).await })
            }),
            ..Default::default()
        }
        .zh("检查并更新程序", "[check|run|rollback|auto]"),
    );
}

fn help(prefix: &str) -> String {
    let c = |text: &str| code(&format!("{prefix}update{text}"));
    if kit::is_zh() {
        return format!(
            "⬆️ <b>程序更新</b>\n\n\
             • {} 当前版本、回滚和自动更新的状态\n\
             • {} 读取 GitHub Releases 检查新版本\n\
             • {} 下载、校验、试跑、替换并重启\n\
             • {} 换回上一版本并重启\n\
             • {} 开启或关闭自动更新\n\
             • {} 自动更新每天检查的时间（北京时间）\n\n\
             自动更新默认开启：每天到点检查一次，有新版本就走和 {} 一样的流程，\
             开始和结果都发到收藏夹。它要重启服务，所以等没有命令在跑时再装，最多等两小时，还不空就改天。\n\n\
             发布仓库由 {} 指定。",
            c(""),
            c(" check"),
            c(" run"),
            c(" rollback"),
            c(" auto on|off"),
            c(" auto time 04:00"),
            c(" run"),
            code("MIBOT_UPDATE_REPO")
        );
    }
    format!(
        "⬆️ <b>Self-Update</b>\n\n\
         • {} Current version, rollback, and auto-update status\n\
         • {} Check GitHub Releases for a new version\n\
         • {} Download, verify, test-run, replace, and restart\n\
         • {} Revert to the previous version and restart\n\
         • {} Enable or disable auto-update\n\
         • {} Daily auto-update check time (UTC+8)\n\n\
         Auto-update is enabled by default: it checks once a day at the configured time and follows the same flow as {}, \
         sending start and result notices to Saved Messages. Because it restarts the service, it waits up to 2 hours for active commands to finish, skipping to the next day if still busy.\n\n\
         Release repository is set by {}.",
        c(""),
        c(" check"),
        c(" run"),
        c(" rollback"),
        c(" auto on|off"),
        c(" auto time 04:00"),
        c(" run"),
        code("MIBOT_UPDATE_REPO")
    )
}

fn auto_summary(cfg: &AutoConfig) -> String {
    if !cfg.enabled {
        return kit::tr("off", "关闭").to_owned();
    }
    if kit::is_zh() {
        format!("开启，每天 {}（北京时间）检查", code(&cfg.time))
    } else {
        format!("on, checks daily at {} (UTC+8)", code(&cfg.time))
    }
}

fn executable_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    fs::canonicalize(&exe).map_err(|e| e.to_string())
}

fn target_asset_name() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("mibot-lite-{}-{arch}", std::env::consts::OS)
}

fn normalize_version(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed.strip_prefix('v').unwrap_or(trimmed)
}

fn newer(current: &str, latest: &str) -> bool {
    let cur = normalize_version(current);
    let lat = normalize_version(latest);
    if cur.is_empty() || cur == "dev" {
        return true;
    }
    cur != lat
}

fn version_parts(value: &str) -> Option<[i64; 3]> {
    let mut parts = [0i64; 3];
    let fields: Vec<&str> = normalize_version(value).split('.').collect();
    if fields.len() != 3 {
        return None;
    }
    for (idx, field) in fields.iter().enumerate() {
        let num = field.parse::<i64>().ok()?;
        if num < 0 {
            return None;
        }
        parts[idx] = num;
    }
    Some(parts)
}

fn strictly_newer(current: &str, latest: &str) -> bool {
    let Some(have) = version_parts(current) else {
        return false;
    };
    let Some(want) = version_parts(latest) else {
        return false;
    };
    for i in 0..3 {
        if want[i] != have[i] {
            return want[i] > have[i];
        }
    }
    false
}

fn parse_clock(value: &str) -> Option<i32> {
    let norm = value.trim().replace('：', ":");
    let (h_str, m_str) = norm.split_once(':')?;
    if h_str.is_empty()
        || h_str.len() > 2
        || m_str.len() != 2
        || !kit::is_digits(h_str)
        || !kit::is_digits(m_str)
    {
        return None;
    }
    let h: i32 = h_str.parse().ok()?;
    let m: i32 = m_str.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

fn decide(cfg: &AutoConfig, now: DateTime<Utc>, busy: usize) -> (AutoAction, String) {
    let local = now.with_timezone(&Shanghai);
    let today = local.format("%Y-%m-%d").to_string();
    if !cfg.enabled || cfg.checked_date == today {
        return (AutoAction::Idle, today);
    }
    let at = parse_clock(&cfg.time).unwrap_or(4 * 60);
    let minute = (local.hour() * 60 + local.minute()) as i32;
    if minute < at {
        return (AutoAction::Idle, today);
    }
    if busy == 0 {
        return (AutoAction::Check, today);
    }
    if i64::from(minute - at) * 60 >= MAX_WAIT.as_secs() as i64 {
        return (AutoAction::GiveUp, today);
    }
    (AutoAction::Wait, today)
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_owned();
    }
    let keep: String = trimmed.chars().skip(count - max_chars).collect();
    format!("…{keep}")
}

fn swap_binary<F>(mut rename: F, binary: &Path, incoming: &Path, aside: &Path) -> CmdResult
where
    F: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    if let Err(err) = rename(binary, aside) {
        return Err(swap_user_err(
            kit::tr(
                "Could not move current binary aside; nothing changed",
                "无法挪开当前的程序文件，什么都没改",
            )
            .into(),
            err.to_string(),
        ));
    }
    if let Err(err) = rename(incoming, binary) {
        if let Err(restore_err) = rename(aside, binary) {
            let msg = if kit::is_zh() {
                format!(
                    "程序文件没能换回来，服务下次启动会失败。原来的版本在 {}，请手动把它改名为 {}",
                    aside.display(),
                    binary.display()
                )
            } else {
                format!(
                    "Failed to restore original binary; service will fail on next start. Original is at {}; rename it back to {}",
                    aside.display(),
                    binary.display()
                )
            };
            return Err(swap_user_err(
                msg,
                format!("{err}; restore: {restore_err}"),
            ));
        }
        return Err(swap_user_err(
            kit::tr(
                "Could not place new binary; restored original version",
                "无法放入新的程序文件，已恢复原来的版本",
            )
            .into(),
            err.to_string(),
        ));
    }
    Ok(())
}

async fn fetch_release(github_api: &str, repo: &str) -> Result<Release, CmdError> {
    let mut req = Request {
        url: format!("{github_api}/repos/{repo}/releases/latest"),
        timeout: Some(Duration::from_secs(20)),
        max_bytes: Some(1 << 20),
        ..Default::default()
    };
    req.headers
        .insert("Accept".into(), "application/vnd.github+json".into());
    req.headers
        .insert("User-Agent".into(), "mibot-lite-updater".into());
    let resp = do_request(req).await?;
    if !resp.ok() {
        return Err(Box::new(mibot_httpx::HttpxError::Status(resp.status)));
    }
    let parsed: Release = serde_json::from_slice(&resp.body)?;
    if parsed.tag_name.is_empty() {
        return Err(fail("release has no tag"));
    }
    Ok(parsed)
}

fn parse_checksum(body: &[u8], asset_name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let mut expected = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 {
            let name = fields[fields.len() - 1].trim_start_matches('*');
            if name == asset_name {
                let hash = fields[0].to_ascii_lowercase();
                if hash.len() == 64 {
                    expected = Some(hash);
                }
            }
        }
    }
    expected
}

fn has_valid_regular_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

impl Updater {
    async fn handle(&self, inv: &Invocation) -> CmdResult {
        let sub = inv.arg(0).to_ascii_lowercase();
        let _guard = if matches!(sub.as_str(), "run" | "apply" | "rollback") {
            if let Some(err) = &self.located_err {
                kit::warn(&self.a, "update.locate_failed", fail(err).as_ref());
                return Err(fail(kit::tr(
                    "Cannot locate binary path for replacement",
                    "找不到程序文件在哪里，不能替换它",
                )));
            }
            if self.binary.is_none() {
                return Err(fail(kit::tr(
                    "Cannot locate binary path for replacement",
                    "找不到程序文件在哪里，不能替换它",
                )));
            }
            if self.restarter.is_none() {
                return Err(fail(kit::tr(
                    "Restart component is unavailable",
                    "重启组件不可用",
                )));
            }
            let Ok(guard) = BUSY_LOCK.try_lock() else {
                return Err(fail(kit::tr(
                    "Another update or rollback is already in progress, please wait",
                    "已有一个更新或回滚正在进行，请稍候",
                )));
            };
            Some(guard)
        } else {
            None
        };

        let title = kit::tr("Self-Update", "程序更新");
        match sub.as_str() {
            "" | "ver" | "status" => self.status(inv).await,
            "check" => {
                inv.edit(&format!(
                    "⬆️ <b>{title}</b>\n{}",
                    escape(&kit::working(kit::tr(
                        "Fetching release info",
                        "正在读取发布信息"
                    )))
                ))
                .await?;
                let latest = fetch_release(&self.github_api, &self.repo)
                    .await
                    .map_err(|e| {
                        kit::wrap_fail(kit::tr("Update check failed", "检查更新失败"), e)
                    })?;
                if !newer(&self.a.version, &latest.tag_name) {
                    let msg = if kit::is_zh() {
                        format!(
                            "✅ 已是最新版本\n当前：{}\n最新：{}",
                            code(kit::version(&self.a)),
                            code(&latest.tag_name)
                        )
                    } else {
                        format!(
                            "✅ Already up to date\nCurrent: {}\nLatest: {}",
                            code(kit::version(&self.a)),
                            code(&latest.tag_name)
                        )
                    };
                    return inv.edit(&msg).await.map_err(Into::into);
                }
                let notes = truncate(latest.body.trim(), 800);
                let mut text = if kit::is_zh() {
                    format!(
                        "⬆️ <b>发现新版本</b>\n当前：{}\n最新：{}\n执行 {} 安装。",
                        code(kit::version(&self.a)),
                        code(&latest.tag_name),
                        code(&format!("{}update run", inv.prefix()))
                    )
                } else {
                    format!(
                        "⬆️ <b>New Version Available</b>\nCurrent: {}\nLatest: {}\nRun {} to install.",
                        code(kit::version(&self.a)),
                        code(&latest.tag_name),
                        code(&format!("{}update run", inv.prefix()))
                    )
                };
                if !notes.is_empty() {
                    text.push_str("\n\n<blockquote expandable>");
                    text.push_str(&escape(&notes));
                    text.push_str("</blockquote>");
                }
                inv.edit(&text).await.map_err(Into::into)
            }
            "run" | "apply" => {
                let binary = self.binary.as_ref().ok_or_else(|| {
                    fail(kit::tr(
                        "Cannot locate binary path for replacement",
                        "找不到程序文件在哪里，不能替换它",
                    ))
                })?;
                inv.edit(&format!(
                    "⬆️ <b>{title}</b>\n{}",
                    escape(&kit::working(kit::tr(
                        "Fetching release info",
                        "正在读取发布信息"
                    )))
                ))
                .await?;
                let latest = fetch_release(&self.github_api, &self.repo)
                    .await
                    .map_err(|e| {
                        kit::wrap_fail(
                            kit::tr(
                                "Update failed; running binary unchanged",
                                "更新失败，当前运行的版本没有改动",
                            ),
                            e,
                        )
                    })?;
                if !newer(&self.a.version, &latest.tag_name) {
                    let msg = if kit::is_zh() {
                        format!("✅ 已是最新版本\n当前：{}", code(kit::version(&self.a)))
                    } else {
                        format!("✅ Already up to date\nCurrent: {}", code(kit::version(&self.a)))
                    };
                    return inv.edit(&msg).await.map_err(Into::into);
                }
                self.install_with_progress(
                    inv.cancel.clone(),
                    &latest,
                    binary,
                    |msg| {
                        let html = format!("⬆️ <b>{title}</b>\n⏳ {msg}");
                        Box::pin(async move { inv.edit(&html).await.map_err(Into::into) })
                    },
                )
                .await?;
                let restarter = self.restarter.as_ref().ok_or_else(|| {
                    fail(kit::tr(
                        "Restart component is unavailable",
                        "重启组件不可用",
                    ))
                })?;
                let progress = if kit::is_zh() {
                    format!(
                        "⬆️ <b>程序更新</b>\n⏳ 已安装 {}，正在重启…",
                        code(&latest.tag_name)
                    )
                } else {
                    format!(
                        "⬆️ <b>Self-Update</b>\n⏳ Installed {}, restarting…",
                        code(&latest.tag_name)
                    )
                };
                restarter
                    .now(
                        inv,
                        "update",
                        &progress,
                        kit::tr(
                            "Update installed, but restart failed; please restart the service manually.",
                            "更新后重启失败，可手动重启服务。",
                        ),
                    )
                    .await
            }
            "rollback" => {
                let binary = self.binary.as_ref().ok_or_else(|| {
                    fail(kit::tr(
                        "Cannot locate binary path for replacement",
                        "找不到程序文件在哪里，不能替换它",
                    ))
                })?;
                let previous = append_suffix(binary, ".previous");
                if !has_valid_regular_file(&previous) {
                    return Err(fail(kit::tr(
                        "No previous version available to roll back to",
                        "没有可回滚的上一版本",
                    )));
                }
                self.a.checkpoint_state().map_err(|e| {
                    fail_with(
                        kit::tr(
                            "Failed to save connection state; rollback aborted",
                            "保存连接状态失败，没有回滚",
                        ),
                        e,
                    )
                })?;
                let aside = append_suffix(binary, ".rollback");
                if let Err(err) = swap_binary(|f, t| fs::rename(f, t), binary, &previous, &aside) {
                    self.a.logs.record(
                        Level::Error,
                        "update.rollback_swap_failed",
                        &[("error", &err.to_string())],
                    );
                    return Err(err);
                }
                if let Err(err) = fs::rename(&aside, &previous) {
                    self.a.logs.record(
                        Level::Warn,
                        "update.keep_previous_failed",
                        &[
                            ("error", &err.to_string()),
                            ("left_at", &aside.to_string_lossy()),
                        ],
                    );
                }
                let restarter = self.restarter.as_ref().ok_or_else(|| {
                    fail(kit::tr(
                        "Restart component is unavailable",
                        "重启组件不可用",
                    ))
                })?;
                restarter
                    .now(
                        inv,
                        "rollback",
                        kit::tr(
                            "⬆️ <b>Self-Update</b>\n⏳ Reverted to previous version, restarting…",
                            "⬆️ <b>程序更新</b>\n⏳ 已换回上一版本，正在重启…",
                        ),
                        kit::tr(
                            "Rollback complete, but restart failed.",
                            "回滚后重启失败。",
                        ),
                    )
                    .await
            }
            "auto" => self.configure_auto(inv).await,
            _ => Err(kit::usage(inv.prefix(), "update [check|run|rollback|auto]")),
        }
    }

    async fn status(&self, inv: &Invocation) -> CmdResult {
        let where_str = match (&self.binary, &self.located_err) {
            (Some(p), None) => code(&p.to_string_lossy()),
            _ => kit::tr("unknown", "无法确定").to_owned(),
        };
        let cfg = self.settings.read()?;
        let has_prev = self
            .binary
            .as_ref()
            .is_some_and(|bin| has_valid_regular_file(&append_suffix(bin, ".previous")));
        let rows = if kit::is_zh() {
            let mut v = vec![
                "⬆️ <b>程序更新</b>".to_owned(),
                format!("当前版本：{}", code(kit::version(&self.a))),
                format!("程序文件：{where_str}"),
                format!("发布仓库：{}", code(&self.repo)),
            ];
            if has_prev {
                v.push(format!(
                    "可回滚到上一版本：{}",
                    code(&format!("{}update rollback", inv.prefix()))
                ));
            } else {
                v.push("暂无可回滚的上一版本".to_owned());
            }
            v.push(format!("自动更新：{}", auto_summary(&cfg)));
            v
        } else {
            let mut v = vec![
                "⬆️ <b>Self-Update</b>".to_owned(),
                format!("Current: {}", code(kit::version(&self.a))),
                format!("Binary: {where_str}"),
                format!("Repository: {}", code(&self.repo)),
            ];
            if has_prev {
                v.push(format!(
                    "Rollback available: {}",
                    code(&format!("{}update rollback", inv.prefix()))
                ));
            } else {
                v.push("No previous version for rollback".to_owned());
            }
            v.push(format!("Auto-update: {}", auto_summary(&cfg)));
            v
        };
        inv.edit(&rows.join("\n")).await.map_err(Into::into)
    }

    async fn configure_auto(&self, inv: &Invocation) -> CmdResult {
        let action = inv.arg(1).to_ascii_lowercase();
        match action.as_str() {
            "" => {
                let cfg = self.settings.read()?;
                inv.edit(&format!(
                    "⬆️ <b>{}</b>\n{}",
                    kit::tr("Auto-Update", "自动更新"),
                    auto_summary(&cfg)
                ))
                .await
                .map_err(Into::into)
            }
            "on" | "off" => {
                let enabled = action == "on";
                kit::update_ok(&self.settings, |cfg| cfg.enabled = enabled)?;
                let msg = if kit::is_zh() {
                    format!("✅ 自动更新已{}", kit::on_off_text(enabled))
                } else {
                    format!("✅ Auto-update {}", kit::on_off_text(enabled))
                };
                inv.edit_text(&msg).await.map_err(Into::into)
            }
            "time" => {
                let Some(at) = parse_clock(inv.arg(2)) else {
                    return Err(fail(kit::tr(
                        "Time must be formatted as HH:MM, e.g. 04:00",
                        "时间要写成 HH:MM，例如 04:00",
                    )));
                };
                let formatted = format!("{:02}:{:02}", at / 60, at % 60);
                let val = formatted.clone();
                kit::update_ok(&self.settings, |cfg| cfg.time = val)?;
                let msg = if kit::is_zh() {
                    format!(
                        "✅ 自动更新的检查时间已设为 {formatted}（北京时间）；今天已经查过的话，从明天开始"
                    )
                } else {
                    format!(
                        "✅ Auto-update check time set to {formatted} (UTC+8); if already checked today, takes effect tomorrow"
                    )
                };
                inv.edit_text(&msg).await.map_err(Into::into)
            }
            _ => Err(kit::usage(
                inv.prefix(),
                "update auto [on|off|time HH:MM]",
            )),
        }
    }

    async fn install_with_progress<'a, F>(
        &self,
        cancel: CancellationToken,
        latest: &Release,
        binary: &Path,
        mut progress: F,
    ) -> CmdResult
    where
        F: FnMut(String) -> futures_util::future::BoxFuture<'a, CmdResult>,
    {
        let installed = fs::metadata(binary)?;
        if let Some(running) = self.running_file_id
            && (installed.dev(), installed.ino()) != running
        {
            return Err(fail(kit::tr(
                "Binary on disk has changed; restart the service first before updating",
                "安装位置的程序已经更换，请先重启服务使其生效，再执行更新",
            )));
        }

        let asset_name = target_asset_name();
        let mut asset_url = None;
        let mut asset_size = 0i64;
        let mut sums_url = None;
        for asset in &latest.assets {
            if asset.name == asset_name {
                asset_url = Some(asset.browser_download_url.clone());
                asset_size = asset.size;
            } else if asset.name == "checksums.txt" || asset.name == "SHA256SUMS" {
                sums_url = Some(asset.browser_download_url.clone());
            }
        }
        let Some(asset_url) = asset_url else {
            let msg = if kit::is_zh() {
                format!(
                    "更新失败：发布 {} 里没有 {asset_name}。当前运行的版本没有改动",
                    latest.tag_name
                )
            } else {
                format!(
                    "Update failed: release {} has no asset {asset_name}; running binary unchanged",
                    latest.tag_name
                )
            };
            return Err(fail(msg));
        };
        let Some(sums_url) = sums_url else {
            return Err(fail(kit::tr(
                "Update failed: missing checksums.txt in release; refusing unverified binary. Running binary unchanged",
                "更新失败：发布里缺少 checksums.txt，不安装没有校验过的程序。当前运行的版本没有改动",
            )));
        };

        let dl_msg = if kit::is_zh() {
            format!(
                "正在下载 {}（{}）…",
                code(&latest.tag_name),
                kit::format_bytes(asset_size.max(0))
            )
        } else {
            format!(
                "Downloading {} ({})…",
                code(&latest.tag_name),
                kit::format_bytes(asset_size.max(0))
            )
        };
        progress(dl_msg).await?;

        let sums = do_request(Request {
            url: sums_url,
            timeout: Some(Duration::from_secs(30)),
            max_bytes: Some(64 << 10),
            ..Default::default()
        })
        .await
        .map_err(|_| {
            fail(kit::tr(
                "Update failed: could not fetch checksums; running binary unchanged",
                "更新失败：读不到校验文件。当前运行的版本没有改动",
            ))
        })?;
        if !sums.ok() {
            return Err(fail(kit::tr(
                "Update failed: could not fetch checksums; running binary unchanged",
                "更新失败：读不到校验文件。当前运行的版本没有改动",
            )));
        }
        let Some(expected) = parse_checksum(&sums.body, &asset_name) else {
            let msg = if kit::is_zh() {
                format!("更新失败：校验文件里没有 {asset_name} 的哈希。当前运行的版本没有改动")
            } else {
                format!("Update failed: no checksum for {asset_name}; running binary unchanged")
            };
            return Err(fail(msg));
        };

        let candidate = append_suffix(binary, ".download");
        let digest = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = fs::remove_file(&candidate);
                return Err(fail(kit::tr(
                    "Update failed: cancelled; running binary unchanged",
                    "更新失败：任务已取消，当前运行的版本没有改动",
                )));
            }
            res = download_file(&asset_url, &candidate, 200 << 20) => match res {
                Ok(d) => d,
                Err(e) => {
                    let _ = fs::remove_file(&candidate);
                    return Err(fail_with(
                        kit::tr(
                            "Update failed: download failed; running binary unchanged",
                            "更新失败：下载失败，当前运行的版本没有改动",
                        ),
                        e,
                    ));
                }
            }
        };

        if digest != expected {
            let _ = fs::remove_file(&candidate);
            return Err(fail(kit::tr(
                "Update failed: SHA-256 mismatch, download discarded; running binary unchanged",
                "更新失败：SHA-256 不匹配，已丢弃下载。当前运行的版本没有改动",
            )));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)) {
                let _ = fs::remove_file(&candidate);
                return Err(e.into());
            }
        }

        progress(
            kit::tr(
                "Checksum verified, testing new binary…",
                "校验通过，正在试跑新版本…",
            )
            .to_owned(),
        )
        .await?;

        let check_res = tokio::time::timeout(Duration::from_secs(60), async {
            ProcessCommand::new(&candidate)
                .arg("--check")
                .arg("--root")
                .arg(&self.a.root)
                .output()
                .await
        })
        .await;
        let check_failed_msg = || {
            fail(kit::tr(
                "Update failed: new binary failed self-check (see logs), discarded; running binary unchanged",
                "更新失败：新版本试跑没通过（详情见日志），已丢弃。当前运行的版本没有改动",
            ))
        };
        match check_res {
            Ok(Ok(out)) if out.status.success() => {}
            Ok(Ok(out)) => {
                let _ = fs::remove_file(&candidate);
                let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
                self.a.logs.record(
                    Level::Warn,
                    "update.check_failed",
                    &[
                        ("version", &latest.tag_name),
                        ("status", &out.status.to_string()),
                        ("output", &tail_chars(&combined, 400)),
                    ],
                );
                return Err(check_failed_msg());
            }
            Ok(Err(e)) => {
                let _ = fs::remove_file(&candidate);
                self.a.logs.record(
                    Level::Warn,
                    "update.check_failed",
                    &[("version", &latest.tag_name), ("error", &e.to_string())],
                );
                return Err(check_failed_msg());
            }
            Err(_) => {
                let _ = fs::remove_file(&candidate);
                self.a.logs.record(
                    Level::Warn,
                    "update.check_failed",
                    &[("version", &latest.tag_name), ("error", "timeout 60s")],
                );
                return Err(check_failed_msg());
            }
        }

        if let Err(e) = self.a.checkpoint_state() {
            let _ = fs::remove_file(&candidate);
            return Err(fail_with(
                kit::tr(
                    "Failed to save connection state; binary unchanged",
                    "保存连接状态失败，没有替换程序",
                ),
                e,
            ));
        }
        let previous = append_suffix(binary, ".previous");
        if let Err(e) = swap_binary(|f, t| fs::rename(f, t), binary, &candidate, &previous) {
            self.a.logs.record(
                Level::Error,
                "update.swap_failed",
                &[("error", &e.to_string())],
            );
            let _ = fs::remove_file(&candidate);
            return Err(e);
        }
        Ok(())
    }

    async fn schedule(self: Arc<Self>, client: Arc<Client>, cancel: CancellationToken) {
        loop {
            let now = Utc::now();
            let secs = 60 - now.second() as u64;
            let nanos = now.nanosecond() as u64;
            let wait = Duration::from_secs(secs)
                .saturating_sub(Duration::from_nanos(nanos))
                .max(Duration::from_millis(100));
            if !kit::sleep(&cancel, wait).await {
                return;
            }
            self.tick(&client, &cancel, Utc::now()).await;
        }
    }

    async fn tick(&self, client: &Arc<Client>, cancel: &CancellationToken, now: DateTime<Utc>) {
        let Ok(cfg) = self.settings.read() else {
            return;
        };
        let (action, today) = decide(&cfg, now, self.a.registry.busy());
        match action {
            AutoAction::Idle | AutoAction::Wait => return,
            AutoAction::GiveUp => {
                self.a.logs.record(
                    Level::Info,
                    "update.auto_skipped",
                    &[
                        ("reason", "commands still running"),
                        ("waited", "2h0m0s"),
                    ],
                );
                self.mark_checked(&today);
                return;
            }
            AutoAction::Check => {}
        }
        let Ok(_guard) = BUSY_LOCK.try_lock() else {
            return;
        };
        self.mark_checked(&today);
        let latest = match fetch_release(&self.github_api, &self.repo).await {
            Ok(r) => r,
            Err(e) => {
                self.a.logs.record(
                    Level::Warn,
                    "update.auto_check_failed",
                    &[("error", &e.to_string())],
                );
                return;
            }
        };
        if !strictly_newer(&self.a.version, &latest.tag_name) {
            self.a.logs.record(
                Level::Info,
                "update.auto_up_to_date",
                &[
                    ("current", kit::version(&self.a)),
                    ("latest", &latest.tag_name),
                ],
            );
            return;
        }
        self.auto_install(client, cancel, &latest).await;
    }

    fn mark_checked(&self, today: &str) {
        if let Err(e) = kit::update_ok(&self.settings, |cfg| {
            cfg.checked_date = today.to_owned();
        }) {
            kit::warn(&self.a, "update.save_failed", &e);
        }
    }

    async fn auto_install(
        &self,
        client: &Arc<Client>,
        cancel: &CancellationToken,
        latest: &Release,
    ) {
        let Some(restarter) = &self.restarter else {
            return;
        };
        let Some(binary) = &self.binary else {
            if let Some(err) = &self.located_err {
                self.a
                    .logs
                    .record(Level::Warn, "update.locate_failed", &[("error", err)]);
            }
            return;
        };
        let self_peer = grammers_tl_types::enums::InputPeer::PeerSelf;
        let header = kit::tr("⬆️ <b>Auto-Update</b>\n", "⬆️ <b>自动更新</b>\n");
        let start_msg = if kit::is_zh() {
            format!(
                "{header}⏳ 正在从 {} 更新到 {}…",
                code(kit::version(&self.a)),
                code(&latest.tag_name)
            )
        } else {
            format!(
                "{header}⏳ Updating from {} to {}…",
                code(kit::version(&self.a)),
                code(&latest.tag_name)
            )
        };
        let sent = match client
            .send_html(&self_peer, &start_msg, SendOptions::default())
            .await
        {
            Ok(u) => u,
            Err(e) => {
                self.a.logs.record(
                    Level::Warn,
                    "update.auto_notice_failed",
                    &[("error", &e.to_string())],
                );
                return;
            }
        };
        let id = first_sent_message_id(&sent).unwrap_or(0);
        self.a.logs.record(
            Level::Info,
            "update.auto_start",
            &[
                ("current", kit::version(&self.a)),
                ("latest", &latest.tag_name),
            ],
        );

        let show_client = Arc::clone(client);
        let show_peer = self_peer.clone();
        let show = move |text: String| {
            let c = Arc::clone(&show_client);
            let p = show_peer.clone();
            async move {
                c.edit_html(
                    &p,
                    id,
                    &format!("{header}{text}"),
                    SendOptions {
                        no_webpage: true,
                        ..Default::default()
                    },
                )
                .await
                .map(|_| ())
                .map_err(Into::<CmdError>::into)
            }
        };

        let prog_client = Arc::clone(client);
        let prog_peer = self_peer.clone();
        let res = self
            .install_with_progress(cancel.clone(), latest, binary, |text| {
                let c = Arc::clone(&prog_client);
                let p = prog_peer.clone();
                Box::pin(async move {
                    c.edit_html(
                        &p,
                        id,
                        &format!("{header}⏳ {text}"),
                        SendOptions {
                            no_webpage: true,
                            ..Default::default()
                        },
                    )
                    .await
                    .map(|_| ())
                    .map_err(Into::<CmdError>::into)
                })
            })
            .await;

        if let Err(err) = res {
            let reason = if is_user_error(err.as_ref()) {
                err.to_string()
            } else {
                brief(err.as_ref())
            };
            self.a.logs.record(
                Level::Warn,
                "update.auto_failed",
                &[("latest", &latest.tag_name), ("error", &err.to_string())],
            );
            let _ = show(format!("❌ {}", escape(&reason))).await;
            return;
        }

        if !self.quiesce(cancel, self.restart_wait).await {
            self.a.logs.record(
                Level::Info,
                "update.auto_restart_deferred",
                &[("latest", &latest.tag_name), ("reason", "work still running")],
            );
            let pfx = self
                .a
                .registry
                .prefixes()
                .into_iter()
                .next()
                .unwrap_or_else(|| ".".to_owned());
            let msg = if kit::is_zh() {
                format!(
                    "⏸ 已安装 {}，但还有任务在跑，没有重启；任务结束后用 {} 重启生效",
                    code(&latest.tag_name),
                    code(&format!("{pfx}restart"))
                )
            } else {
                format!(
                    "⏸ Installed {}, but commands are still running; run {} when idle to restart",
                    code(&latest.tag_name),
                    code(&format!("{pfx}restart"))
                )
            };
            let _ = show(msg).await;
            return;
        }

        let restarting_msg = if kit::is_zh() {
            format!("⏳ 已安装 {}，正在重启…", code(&latest.tag_name))
        } else {
            format!("⏳ Installed {}, restarting…", code(&latest.tag_name))
        };
        if let Err(e) = show(restarting_msg).await {
            self.a.logs.record(
                Level::Warn,
                "update.auto_notice_failed",
                &[("error", &e.to_string())],
            );
        }

        if let Err(e) = restarter
            .schedule(
                cancel,
                Some(&self_peer),
                &client.self_id().to_string(),
                id,
                0,
                "auto-update",
            )
            .await
        {
            self.a.registry.resume();
            self.a.logs.record(
                Level::Error,
                "update.auto_restart_failed",
                &[("error", &e.to_string())],
            );
            let fail_msg = if kit::is_zh() {
                format!(
                    "❌ 已安装 {}，但重启没提交成功，可以手动重启服务\n状态：\n{}",
                    code(&latest.tag_name),
                    restarter.status().await
                )
            } else {
                format!(
                    "❌ Installed {}, but failed to schedule restart; please restart manually\nStatus:\n{}",
                    code(&latest.tag_name),
                    restarter.status().await
                )
            };
            let _ = show(fail_msg).await;
            return;
        }

        let a_resume = Arc::clone(&self.a);
        let cancel_grace = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_grace.cancelled() => {}
                _ = tokio::time::sleep(RESTART_GRACE) => {
                    a_resume.logs.record(
                        Level::Warn,
                        "update.auto_restart_not_observed",
                        &[("waited", "2m0s")],
                    );
                    a_resume.registry.resume();
                }
            }
        });
    }

    async fn quiesce(&self, cancel: &CancellationToken, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if !self.wait_idle(cancel, deadline).await {
                return false;
            }
            if self.a.registry.quiesce(self.quiesce_wait).await.is_ok() {
                return true;
            }
            if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                return false;
            }
        }
    }

    async fn wait_idle(
        &self,
        cancel: &CancellationToken,
        deadline: tokio::time::Instant,
    ) -> bool {
        while self.a.registry.busy() > 0 {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            if !kit::sleep(cancel, self.idle_poll).await {
                return false;
            }
        }
        !cancel.is_cancelled()
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sha2::{Digest, Sha256};
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(_prefix: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "mibot-update-test-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn shanghai_time(y: i32, mo: u32, d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Shanghai
            .with_ymd_and_hms(y, mo, d, h, m, 0)
            .single()
            .expect("valid shanghai time")
            .with_timezone(&Utc)
    }

    #[test]
    fn test_swap_binary() {
        let dir = TempDirGuard::new("swap");
        let binary = dir.0.join("mibot-lite");
        let incoming = dir.0.join("mibot-lite.download");
        let aside = dir.0.join("mibot-lite.previous");
        fs::write(&binary, b"old").expect("write old");
        fs::write(&incoming, b"new").expect("write new");

        swap_binary(|f, t| fs::rename(f, t), &binary, &incoming, &aside).expect("swap");
        assert_eq!(fs::read(&binary).expect("read bin"), b"new");
        assert_eq!(fs::read(&aside).expect("read aside"), b"old");

        // Error on first rename
        let fail_first = |_from: &Path, _to: &Path| -> std::io::Result<()> {
            Err(std::io::Error::other("injected step 1"))
        };
        let err = swap_binary(fail_first, &binary, &incoming, &aside).unwrap_err();
        assert!(err.to_string().contains("Could not move current binary aside"));

        // Rollback when step 2 fails
        let aside2 = dir.0.join("aside2");
        let inc2 = dir.0.join("inc2");
        fs::write(&inc2, b"candidate").expect("write inc2");
        let call = StdMutex::new(0usize);
        let fail_second = |from: &Path, to: &Path| -> std::io::Result<()> {
            let mut n = call.lock().expect("lock");
            *n += 1;
            if *n == 2 {
                return Err(std::io::Error::other("injected step 2"));
            }
            fs::rename(from, to)
        };
        let err2 = swap_binary(fail_second, &binary, &inc2, &aside2).unwrap_err();
        assert!(err2.to_string().contains("restored original version"));
        assert_eq!(fs::read(&binary).expect("restored"), b"new");

        // Both step 2 and restore fail
        let call3 = StdMutex::new(0usize);
        let fail_both = |from: &Path, to: &Path| -> std::io::Result<()> {
            let mut n = call3.lock().expect("lock");
            *n += 1;
            if *n >= 2 {
                return Err(std::io::Error::other("injected both"));
            }
            fs::rename(from, to)
        };
        let err3 = swap_binary(fail_both, &binary, &inc2, &aside2).unwrap_err();
        assert!(err3.to_string().contains("Original is at"));
        assert!(fs::read(&aside2).is_ok());
    }

    #[test]
    fn test_decide_and_versions() {
        assert!(newer("v0.1.0", "v0.1.1"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(newer("dev", "v0.1.0"));

        assert!(strictly_newer("v0.1.0", "v0.1.1"));
        assert!(!strictly_newer("v0.2.0", "v0.1.9"));
        assert!(!strictly_newer("dev", "v0.1.0"));

        assert_eq!(parse_clock("04:00"), Some(240));
        assert_eq!(parse_clock("4：05"), Some(245));
        assert_eq!(parse_clock("24:00"), None);

        let cfg = AutoConfig {
            enabled: true,
            time: "04:00".into(),
            checked_date: String::new(),
        };
        assert_eq!(
            decide(&cfg, shanghai_time(2026, 4, 15, 3, 59), 0).0,
            AutoAction::Idle
        );
        assert_eq!(
            decide(&cfg, shanghai_time(2026, 4, 15, 4, 0), 0).0,
            AutoAction::Check
        );
        assert_eq!(
            decide(&cfg, shanghai_time(2026, 4, 15, 4, 0), 1).0,
            AutoAction::Wait
        );
        assert_eq!(
            decide(&cfg, shanghai_time(2026, 4, 15, 6, 0), 1).0,
            AutoAction::GiveUp
        );

        let checked = AutoConfig {
            enabled: true,
            time: "04:00".into(),
            checked_date: "2026-04-15".into(),
        };
        assert_eq!(
            decide(&checked, shanghai_time(2026, 4, 15, 5, 0), 0).0,
            AutoAction::Idle
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_running_path_preserved_across_rename() {
        let dir = TempDirGuard::new("exe");
        let a_path = dir.0.join("bin-a");
        let b_path = dir.0.join("bin-b");
        fs::write(&a_path, b"one").expect("write a");
        let meta_a = fs::metadata(&a_path).expect("stat a");
        let running = (meta_a.dev(), meta_a.ino());
        fs::rename(&a_path, &b_path).expect("rename a -> b");
        fs::write(&a_path, b"two").expect("write new a");
        let meta_new_a = fs::metadata(&a_path).expect("stat new a");
        assert_ne!((meta_new_a.dev(), meta_new_a.ino()), running);
    }

    #[tokio::test]
    async fn test_install_verifies_check_and_replaces() {
        let dir = TempDirGuard::new("install");
        let bin = dir.0.join("mibot-lite");
        fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write old");
        let meta = fs::metadata(&bin).expect("stat");

        let new_script = b"#!/bin/sh\necho check ok\nexit 0\n".to_vec();
        let digest = hex::encode(Sha256::digest(&new_script));
        let asset_name = target_asset_name();
        let sums_body = format!("{digest}  {asset_name}\n").into_bytes();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server_new = new_script.clone();
        let server_sums = sums_body.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let new_b = server_new.clone();
                let sums_b = server_sums.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, body) = if path == "/sums" {
                        ("200 OK", sums_b)
                    } else if path == "/bin" {
                        ("200 OK", new_b)
                    } else {
                        ("404 Not Found", b"not found".to_vec())
                    };
                    let hdr = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });

        let peers = Arc::new(mibot_bot::PeerCache::new());
        let client = Arc::new(mibot_bot::Client::from_transport(
            Arc::new(MockTransport),
            Arc::clone(&peers),
            Arc::new(mibot_bot::SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let a = mibot_app::App::for_testing(
            dir.0.clone(),
            "v0.1.0",
            "boot-test",
            client,
            peers,
            Arc::new(mibot_logtail::Ring::new()),
        );
        let u = Updater {
            a,
            restarter: None,
            repo: DEFAULT_REPO.into(),
            github_api: format!("http://{addr}"),
            settings: Store::new(dir.0.join("update.json"), AutoConfig::default),
            binary: Some(bin.clone()),
            running_file_id: Some((meta.dev(), meta.ino())),
            located_err: None,
            idle_poll: Duration::from_millis(5),
            quiesce_wait: Duration::from_millis(20),
            restart_wait: Duration::from_millis(50),
        };
        let release = Release {
            tag_name: "v0.1.1".into(),
            body: String::new(),
            assets: vec![
                ReleaseAsset {
                    name: asset_name,
                    browser_download_url: format!("http://{addr}/bin"),
                    size: new_script.len() as i64,
                },
                ReleaseAsset {
                    name: "checksums.txt".into(),
                    browser_download_url: format!("http://{addr}/sums"),
                    size: sums_body.len() as i64,
                },
            ],
        };
        let cancel = CancellationToken::new();
        u.install_with_progress(cancel, &release, &bin, |_| Box::pin(async { Ok(()) }))
            .await
            .expect("install ok");
        assert_eq!(fs::read(&bin).expect("new bin"), new_script);
        assert_eq!(
            fs::read(append_suffix(&bin, ".previous")).expect("prev bin"),
            b"#!/bin/sh\nexit 0\n"
        );
        handle.abort();
    }

    struct MockTransport;
    impl mibot_bot::RpcTransport for MockTransport {
        fn invoke_raw<'a>(
            &'a self,
            _dc: Option<i32>,
            _id: u32,
            _body: Vec<u8>,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>, grammers_mtsender::InvocationError>>
        {
            Box::pin(async {
                Err(grammers_mtsender::InvocationError::Dropped("mock"))
            })
        }
    }
}
