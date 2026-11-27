use crate::kit;
use chrono::Local;
use mibot_app::App;
use mibot_bot::MediaOptions;
use mibot_command::{Invocation, code, group};
use mibot_logtail::{Level, at_least};
use std::sync::Arc;
use std::time::Duration;

const LOG_DEFAULT_LINES: usize = 200;
const LOG_MIME_TYPE: &str = "text/plain";

type LinePred = Box<dyn Fn(&str) -> bool + Send>;

fn log_filter(inv: &Invocation) -> (usize, Option<LinePred>, String) {
    let mut count = LOG_DEFAULT_LINES;
    let mut terms = Vec::new();
    for arg in &inv.args {
        if let Ok(val) = arg.parse::<usize>()
            && val > 0
        {
            count = val;
            continue;
        }
        terms.push(arg.as_str());
    }
    if terms.is_empty() {
        return (count, None, String::new());
    }
    match terms[0].to_ascii_lowercase().as_str() {
        "error" | "err" | "错误" => (
            count,
            Some(Box::new(|line| at_least(line, Level::Error))),
            "仅错误".to_owned(),
        ),
        "warn" | "warning" | "警告" => (
            count,
            Some(Box::new(|line| at_least(line, Level::Warn))),
            "警告及以上".to_owned(),
        ),
        _ => {
            let needle = terms.join(" ").to_ascii_lowercase();
            let n2 = needle.clone();
            (
                count,
                Some(Box::new(move |line| {
                    line.to_ascii_lowercase().contains(&n2)
                })),
                format!("包含 {needle}"),
            )
        }
    }
}

fn short_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 86400.0 {
        format!("{:.1} 天", secs / 86400.0)
    } else if secs >= 3600.0 {
        format!("{:.1} 小时", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0} 分钟", secs / 60.0)
    } else {
        format!("{secs:.0} 秒")
    }
}

fn log_header(a: &App, shown: usize, held: usize, filter: &str) -> String {
    let level = a.logs.level().as_str();
    let mut lines = vec![
        format!("mibot-lite {}", a.version),
        format!("运行时长 {}", short_duration(a.started_at.elapsed())),
        format!("命令 {} 个", a.registry.list().len()),
        format!("日志级别 {level}"),
        format!("导出 {shown} 行，进程内共保留 {held} 行"),
    ];
    if !filter.is_empty() {
        lines.push(format!("过滤 {filter}"));
    }
    lines.push(String::new());
    lines.push(
        "已脱敏：聊天与账号 ID 换成 #xxxx（同一个仍是同一个），消息内容、密钥、".to_owned(),
    );
    lines.push(
        "URL 路径与 IP 地址已移除。进程重启后这里就空了，那种情况要看主机上的日志。".to_owned(),
    );
    lines.push("-".repeat(60));
    lines.push(String::new());
    lines.join("\n")
}

fn log_help(prefix: &str) -> String {
    let p = mibot_command::escape(prefix);
    format!(
        "📄 <b>运行日志</b>\n\n把进程内保留的日志导出成文件发出来，方便转给别人看。\n\n\
         • <code>{p}log</code> 最近 {LOG_DEFAULT_LINES} 行\n\
         • <code>{p}log 50</code> 指定行数\n\
         • <code>{p}log error</code> 只要错误\n\
         • <code>{p}log warn</code> 警告及以上\n\
         • <code>{p}log speedtest</code> 含该词的行\n\
         • <code>{p}log debug on</code> 临时调到 debug 级别，<code>off</code> 调回来\n\n\
         <b>脱敏</b>\n\
         聊天与账号 ID 在写进内存时就换成了 #xxxx，同一个仍然是同一个，但看不出是谁；\
         消息内容、密钥、URL 路径和 IP 地址直接丢弃。文件可以放心转发。\n\n\
         日志只留在进程里，重启就没了。要看重启之前的，得到主机上翻 journalctl。"
    )
}

async fn set_log_level(a: &App, inv: &Invocation) -> mibot_command::CmdResult {
    match inv.arg(1).to_ascii_lowercase().as_str() {
        "on" | "开" => a.logs.set_level(Level::Debug),
        "off" | "关" => a.logs.set_level(Level::Info),
        "" => {
            return inv
                .edit(&format!(
                    "当前日志级别 {}\n用 {} 或 {}",
                    code(a.logs.level().as_str()),
                    code(&format!("{}log debug on", inv.prefix())),
                    code(&format!("{}log debug off", inv.prefix())),
                ))
                .await
                .map_err(Into::into);
        }
        _ => return Err(kit::usage(inv.prefix(), "log debug on|off")),
    }
    let note = if a.logs.level() == Level::Debug {
        "\n\n<i>debug 很吵，查完记得关掉。</i>"
    } else {
        ""
    };
    inv.edit(&format!(
        "日志级别已设为 {}{note}",
        code(a.logs.level().as_str())
    ))
    .await
    .map_err(Into::into)
}

pub fn register(a: &Arc<App>) {
    let app = Arc::clone(a);
    a.registry.register(kit::make_cmd(
        "log",
        &[],
        group::SYSTEM,
        "导出脱敏后的运行日志",
        "[行数|error|warn|关键词|debug on|off]",
        log_help,
        move |inv: Invocation| {
            let app = Arc::clone(&app);
            Box::pin(async move {
                if matches!(inv.arg(0).to_ascii_lowercase().as_str(), "debug" | "level") {
                    return set_log_level(&app, &inv).await;
                }
                let (count, keep, filter) = log_filter(&inv);
                let lines = match keep {
                    Some(pred) => app.logs.tail(count, Some(move |s: &str| pred(s))),
                    None => app.logs.tail::<fn(&str) -> bool>(count, None),
                };
                if lines.is_empty() {
                    return inv.edit_text("没有匹配的日志").await.map_err(Into::into);
                }
                let header = log_header(&app, lines.len(), app.logs.held(), &filter);
                let mut body = header;
                body.push_str(&lines.join("\n"));
                body.push('\n');
                let peer = inv.peer_result()?;
                let name = format!("mibot-lite-{}.log", Local::now().format("%Y%m%d-%H%M"));
                let caption = format!(
                    "📄 运行日志 {} · {} 行\n<i>已脱敏，可直接转发</i>",
                    code(&app.version),
                    lines.len()
                );
                inv.client
                    .send_document(
                        &peer,
                        body.as_bytes(),
                        MediaOptions {
                            name,
                            mime_type: LOG_MIME_TYPE.to_owned(),
                            caption,
                            html: true,
                            ..Default::default()
                        },
                    )
                    .await?;
                inv.client
                    .delete_message(&inv.message)
                    .await
                    .map_err(Into::into)
            })
        },
    ));
}
