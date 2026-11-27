use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{
    MediaOptions, rich_code, rich_concat, rich_plain,
};
use mibot_command::{Command, GROUPS, Invocation, Registry, code, escape, group};
use mibot_httpx::{Method, Request, do_request};
use mibot_logtail::Level;
use mibot_statuscard::{Card, Gauge, render as render_card};
use mibot_sysinfo::{Capacity, Resources, format_bytes, format_uptime, megabytes, read_host, read_memory, sample_resources};
use regex::Regex;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

static DRAWING: OnceLock<Mutex<()>> = OnceLock::new();
static PING_TARGET: OnceLock<Regex> = OnceLock::new();

fn drawing_lock() -> &'static Mutex<()> {
    DRAWING.get_or_init(|| Mutex::new(()))
}

fn ping_target_re() -> &'static Regex {
    PING_TARGET.get_or_init(|| Regex::new(r"^[A-Za-z0-9._:-]{1,253}$").unwrap())
}

pub fn register(a: &Arc<App>) {
    let app_ver = Arc::clone(a);
    let app_status = Arc::clone(a);
    let app_help = Arc::clone(a);

    a.registry.register(kit::make_cmd(
        "ping",
        &[],
        group::SYSTEM,
        "测试网络延迟",
        "[域名|IP]",
        ping_help,
        move |inv: Invocation| {
            Box::pin(async move {
                let target = inv.arg(0).to_owned();
                if !target.is_empty() {
                    let msg = probe(&target).await;
                    return inv.edit(&msg).await.map_err(Into::into);
                }
                let elapsed = inv
                    .client
                    .ping()
                    .await
                    .map_err(|e| kit::fail_with_err("Telegram 延迟测试失败", e))?;
                let editing = Instant::now();
                inv.edit_text("🏓 Pong").await?;
                let edit_ms = editing.elapsed().as_millis();
                inv.edit(&format!(
                    "🏓 <b>延迟</b>\nTelegram 接口：{} ms\n消息编辑：{edit_ms} ms",
                    elapsed.as_millis()
                ))
                .await
                .map_err(Into::into)
            })
        },
    ));

    a.registry.register(kit::make_cmd(
        "version",
        &["ver"],
        group::SYSTEM,
        "查看版本信息",
        "",
        version_help,
        move |inv: Invocation| {
            let app = Arc::clone(&app_ver);
            Box::pin(async move { inv.edit(&version_text(&app)).await.map_err(Into::into) })
        },
    ));

    a.registry.register(kit::make_cmd(
        "memory",
        &[],
        group::SYSTEM,
        "查看进程内存占用",
        "",
        memory_help,
        move |inv: Invocation| {
            Box::pin(async move { inv.edit(&memory_report()).await.map_err(Into::into) })
        },
    ));

    a.registry.register(kit::make_cmd(
        "status",
        &[],
        group::SYSTEM,
        "查看运行状态卡片",
        "",
        status_help,
        move |inv: Invocation| {
            let app = Arc::clone(&app_status);
            Box::pin(async move { run_status(&app, &inv).await })
        },
    ));

    a.registry.register(kit::make_cmd(
        "sysinfo",
        &[],
        group::SYSTEM,
        "查看详细系统信息",
        "",
        sysinfo_help,
        move |inv: Invocation| {
            Box::pin(async move {
                let machine = read_host();
                let process = read_memory();
                let used_mem = machine.total_memory.saturating_sub(machine.free_memory);
                let lines = [
                    "🖥 <b>系统信息</b>".to_owned(),
                    String::new(),
                    format!("主机：{}", code(&machine.hostname)),
                    format!("系统：{}", code(&machine.platform)),
                    format!("内核：{}", code(&machine.kernel_release)),
                    format!("运行时间：{}", code(&format_uptime(machine.uptime))),
                    format!(
                        "负载：{}",
                        code(&format!(
                            "{:.2} / {:.2} / {:.2}",
                            machine.load_average[0], machine.load_average[1], machine.load_average[2]
                        ))
                    ),
                    format!("CPU：{}", code(&format!("{} 核", machine.cpus))),
                    format!(
                        "系统内存：{}",
                        code(&format!(
                            "{:.2} / {:.2} MB",
                            megabytes(used_mem),
                            megabytes(machine.total_memory)
                        ))
                    ),
                    String::new(),
                    "<b>MiBot Lite 进程</b>".to_owned(),
                    format!("Rust：{}", code("rustc (pure-rust)")),
                    format!("PID：{}", code(&std::process::id().to_string())),
                    format!("RSS：{}", code(&format!("{:.2} MB", megabytes(process.rss)))),
                    format!(
                        "Heap：{}",
                        code(&format!(
                            "{:.2} / {:.2} MB",
                            megabytes(process.heap_alloc),
                            megabytes(process.heap_sys)
                        ))
                    ),
                    format!("Sys：{}", code(&format!("{:.2} MB", megabytes(process.sys)))),
                ];
                inv.edit(&lines.join("\n")).await.map_err(Into::into)
            })
        },
    ));

    a.registry.register(kit::make_cmd(
        "help",
        &["h"],
        group::SYSTEM,
        "查看命令列表或说明",
        "[命令|分组]",
        help_help,
        move |inv: Invocation| {
            let app = Arc::clone(&app_help);
            Box::pin(async move {
                let name = inv.raw.trim();
                let reg = &app.registry;
                let prefix = inv.prefix();
                if name.is_empty() {
                    let blocks = help_list_rich(reg, prefix);
                    let rich = tl::types::InputRichMessage {
                        rtl: false,
                        noautolink: false,
                        blocks,
                        photos: None,
                        documents: None,
                        users: None,
                    }
                    .into();
                    let html = render_help_list(reg, prefix);
                    return inv.edit_rich(rich, &html).await.map(|_| ()).map_err(Into::into);
                }
                if lookup_for_help(reg, name).is_none()
                    && let Some(grp) = find_group(name)
                {
                    let blocks = group_help_rich(reg, prefix, grp);
                    let rich = tl::types::InputRichMessage {
                        rtl: false,
                        noautolink: false,
                        blocks,
                        photos: None,
                        documents: None,
                        users: None,
                    }
                    .into();
                    let html = render_group_help(reg, prefix, grp);
                    return inv.edit_rich(rich, &html).await.map(|_| ()).map_err(Into::into);
                }
                inv.edit(&render_command_help(reg, prefix, name))
                    .await
                    .map_err(Into::into)
            })
        },
    ));
}

fn version_text(a: &App) -> String {
    [
        "🔖 <b>版本信息</b>".to_owned(),
        String::new(),
        format!("MiBot Lite：{}", code(kit::version(a))),
        format!("Rust：{}", code("pure-rust / rustls")),
        format!(
            "平台：{}",
            code(&format!("{} {}", std::env::consts::OS, std::env::consts::ARCH))
        ),
        format!("PID：{}", code(&std::process::id().to_string())),
    ]
    .join("\n")
}

fn memory_report() -> String {
    let current = read_memory();
    let mut lines = vec!["🧠 <b>内存状态</b>".to_owned(), String::new()];
    if current.rss > 0 {
        lines.push(format!(
            "RSS（进程总占用）：{}",
            code(&format!("{:.2} MB", megabytes(current.rss)))
        ));
    } else {
        lines.push(format!("RSS（进程总占用）：{}", code("不可用")));
    }
    lines.push(format!(
        "Heap（已用 / 预留）：{}",
        code(&format!(
            "{:.2} / {:.2} MB",
            megabytes(current.heap_alloc),
            megabytes(current.heap_sys)
        ))
    ));
    lines.push(format!(
        "Sys（运行时预留）：{}",
        code(&format!("{:.2} MB", megabytes(current.sys)))
    ));
    lines.push(format!("Tasks：{}", code(&current.tasks.to_string())));
    lines.join("\n")
}

fn gauge_of(cap: Capacity) -> Gauge {
    match cap.percent() {
        Some(pct) => Gauge {
            percent: pct,
            known: true,
        },
        None => Gauge {
            percent: 0.0,
            known: false,
        },
    }
}

fn status_caption(a: &App, registry: &Registry, resources: &Resources) -> String {
    let process = read_memory();
    let host = read_host();
    let load = if host.load_average != [0.0; 3] {
        format!(
            "{:.2} / {:.2} / {:.2}",
            host.load_average[0], host.load_average[1], host.load_average[2]
        )
    } else {
        "不可用".to_owned()
    };
    let host_uptime = if !host.uptime.is_zero() {
        format_uptime(host.uptime)
    } else {
        "不可用".to_owned()
    };
    let row = |label: &str, value: &str| format!("• {label}：{}", code(value));
    [
        "<b>进程</b>".to_owned(),
        row("版本", kit::version(a)),
        row("运行时间", &format_uptime(a.started_at.elapsed())),
        row(
            "PID",
            &format!("{} · {} 个任务", std::process::id(), process.tasks),
        ),
        row("CPU", &format!("{:.1}%", resources.process_cpu)),
        row(
            "RSS · 堆内存",
            &format!("{} · {}", format_bytes(process.rss), format_bytes(process.heap_alloc)),
        ),
        row(
            "命令 · 前缀",
            &format!(
                "{} 个 · {}",
                registry.list().len(),
                registry.prefixes().join(" ")
            ),
        ),
        String::new(),
        "<b>主机</b>".to_owned(),
        row("系统", &format!("{} · {}", resources.os, host.platform)),
        row("内核", kit::or_default(&host.kernel_release, "不可用")),
        row("主机在线", &host_uptime),
        row("负载 1 / 5 / 15 分钟", &load),
        row("状态采样", &format!("{} ms", resources.sampled.as_millis())),
    ]
    .join("\n")
}

async fn run_status(a: &App, inv: &Invocation) -> mibot_command::CmdResult {
    let Ok(_guard) = drawing_lock().try_lock() else {
        return Err(kit::fail_err("上一张状态卡片还在生成，请稍候"));
    };
    let resources = sample_resources(&a.root, Duration::from_millis(160)).await;
    let caption = status_caption(a, &a.registry, &resources);
    // Keep footer strictly ASCII + "·" so the embedded subset font covers every glyph.
    let footer = format!("MiBot Lite {}  ·  Rust  ·  grammers 0.12", kit::version(a));
    let cpu = if resources.has_cpu {
        Gauge {
            percent: resources.system_cpu,
            known: true,
        }
    } else {
        Gauge::default()
    };
    let card_spec = Card {
        name: "MiBot Lite".to_owned(),
        uptime: a.started_at.elapsed(),
        cpu,
        memory: gauge_of(resources.memory),
        disk: gauge_of(resources.disk),
        swap: gauge_of(resources.swap),
        footer,
    };
    match render_card(&card_spec) {
        Ok(png) => {
            if let Ok(peer) = inv.client.input_peer(&inv.message.peer) {
                match inv
                    .client
                    .send_photo(
                        &peer,
                        &png,
                        MediaOptions {
                            name: "status.png".to_owned(),
                            caption: caption.clone(),
                            html: true,
                            reply_to: Some(inv.message.reply_to_id).filter(|&id| id > 0),
                            topic_id: Some(inv.message.topic_id).filter(|&id| id > 0),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(_) => return inv.client.delete_message(&inv.message).await.map_err(Into::into),
                    Err(e) => {
                        let e_s = e.to_string();
                        a.logs.record(Level::Warn, "status.card_send_failed", &[("error", &e_s)]);
                    }
                }
            }
        }
        Err(e) => {
            let e_s = e.to_string();
            a.logs.record(Level::Warn, "status.card_render_failed", &[("error", &e_s)]);
        }
    }
    inv.edit(&format!("📊 <b>运行状态</b>\n\n{caption}"))
        .await
        .map_err(Into::into)
}

fn commands_by_group(registry: &Registry) -> std::collections::HashMap<String, Vec<Arc<Command>>> {
    let mut by_group: std::collections::HashMap<String, Vec<Arc<Command>>> =
        std::collections::HashMap::new();
    for cmd in registry.list() {
        by_group.entry(cmd.group.clone()).or_default().push(cmd);
    }
    by_group
}

fn group_lines(commands: &[Arc<Command>], prefix: &str) -> Vec<String> {
    commands
        .iter()
        .map(|cmd| {
            let mut line = code(&format!("{prefix}{}", cmd.name));
            if !cmd.aliases.is_empty() {
                let short: Vec<String> = cmd
                    .aliases
                    .iter()
                    .map(|a| code(&format!("{prefix}{a}")))
                    .collect();
                line.push_str(&format!("（{}）", short.join(" ")));
            }
            format!("{line} — {}", escape(&cmd.description))
        })
        .collect()
}

pub fn render_help_list(registry: &Registry, prefix: &str) -> String {
    let by_group = commands_by_group(registry);
    let mut lines = vec!["📖 <b>命令列表</b>".to_owned()];
    let mut details = Vec::new();
    for &grp in GROUPS {
        let Some(cmds) = by_group.get(grp) else {
            continue;
        };
        if cmds.is_empty() {
            continue;
        }
        let names: Vec<String> = cmds
            .iter()
            .map(|c| code(&format!("{prefix}{}", c.name)))
            .collect();
        lines.push(format!("<b>{}</b>", escape(grp)));
        lines.push(names.join(" "));
        if !details.is_empty() {
            details.push(String::new());
        }
        details.push(format!("<b>{}</b>", escape(grp)));
        details.extend(group_lines(cmds, prefix));
    }
    let quoted: Vec<String> = registry.prefixes().into_iter().map(|p| code(&p)).collect();
    lines.push(String::new());
    lines.push(format!(
        "各命令的说明折在下面，{} 看用法 · 前缀：{}",
        code(&format!("{prefix}help 命令")),
        quoted.join(" ")
    ));
    lines.push(format!(
        "<blockquote expandable>{}</blockquote>",
        details.join("\n")
    ));
    lines.join("\n")
}

pub fn render_group_help(registry: &Registry, prefix: &str, grp: &str) -> String {
    let by_group = commands_by_group(registry);
    let cmds = by_group.get(grp).map(Vec::as_slice).unwrap_or(&[]);
    let mut lines = vec![format!("📖 <b>{}</b>", escape(grp)), String::new()];
    lines.extend(group_lines(cmds, prefix));
    lines.push(String::new());
    lines.push(format!("{} 看用法", code(&format!("{prefix}help 命令"))));
    lines.join("\n")
}

fn rich_bold(text: &str) -> tl::enums::RichText {
    tl::types::TextBold {
        text: rich_plain(text),
    }
    .into()
}

fn rich_title(title: &str) -> tl::enums::PageBlock {
    tl::types::PageBlockParagraph {
        text: rich_concat(&[rich_plain("📖 "), rich_bold(title)]),
    }
    .into()
}

fn cell(header: bool, colspan: Option<i32>, text: tl::enums::RichText) -> tl::enums::PageTableCell {
    tl::types::PageTableCell {
        header,
        align_center: false,
        align_right: false,
        valign_middle: false,
        valign_bottom: false,
        text: Some(text),
        colspan,
        rowspan: None,
    }
    .into()
}

fn header_table_rows() -> Vec<tl::enums::PageTableRow> {
    vec![
        tl::types::PageTableRow {
            cells: vec![
                cell(true, None, rich_plain("命令")),
                cell(true, None, rich_plain("说明")),
            ],
        }
        .into(),
    ]
}

fn command_rows(commands: &[Arc<Command>], prefix: &str) -> Vec<tl::enums::PageTableRow> {
    commands
        .iter()
        .map(|cmd| {
            let mut name_parts = vec![rich_code(&format!("{prefix}{}", cmd.name))];
            if !cmd.aliases.is_empty() {
                name_parts.push(rich_plain("（"));
                for (idx, alias) in cmd.aliases.iter().enumerate() {
                    if idx > 0 {
                        name_parts.push(rich_plain(" "));
                    }
                    name_parts.push(rich_code(&format!("{prefix}{alias}")));
                }
                name_parts.push(rich_plain("）"));
            }
            tl::types::PageTableRow {
                cells: vec![
                    cell(false, None, rich_concat(&name_parts)),
                    cell(false, None, rich_plain(&cmd.description)),
                ],
            }
            .into()
        })
        .collect()
}

pub fn help_list_rich(registry: &Registry, prefix: &str) -> Vec<tl::enums::PageBlock> {
    let by_group = commands_by_group(registry);
    let mut blocks = vec![rich_title("命令列表")];
    let mut rows = header_table_rows();
    for &grp in GROUPS {
        let Some(cmds) = by_group.get(grp) else {
            continue;
        };
        if cmds.is_empty() {
            continue;
        }
        let mut parts = vec![rich_bold(grp), rich_plain("\n")];
        for (idx, cmd) in cmds.iter().enumerate() {
            if idx > 0 {
                parts.push(rich_plain(" "));
            }
            parts.push(rich_code(&format!("{prefix}{}", cmd.name)));
        }
        blocks.push(
            tl::types::PageBlockParagraph {
                text: rich_concat(&parts),
            }
            .into(),
        );
        rows.push(
            tl::types::PageTableRow {
                cells: vec![cell(true, Some(2), rich_plain(grp))],
            }
            .into(),
        );
        rows.extend(command_rows(cmds, prefix));
    }
    let mut hint = vec![
        rich_code(&format!("{prefix}help 命令")),
        rich_plain(" 看用法 · 前缀："),
    ];
    for (idx, p) in registry.prefixes().into_iter().enumerate() {
        if idx > 0 {
            hint.push(rich_plain(" "));
        }
        hint.push(rich_code(&p));
    }
    blocks.push(
        tl::types::PageBlockParagraph {
            text: rich_concat(&hint),
        }
        .into(),
    );
    blocks.push(
        tl::types::PageBlockDetails {
            open: false,
            title: rich_plain("各命令的说明"),
            blocks: vec![
                tl::types::PageBlockTable {
                    bordered: true,
                    striped: false,
                    compact: false,
                    title: tl::enums::RichText::TextEmpty,
                    rows,
                }
                .into(),
            ],
        }
        .into(),
    );
    blocks
}

pub fn group_help_rich(
    registry: &Registry,
    prefix: &str,
    grp: &str,
) -> Vec<tl::enums::PageBlock> {
    let by_group = commands_by_group(registry);
    let cmds = by_group.get(grp).map(Vec::as_slice).unwrap_or(&[]);
    let mut rows = header_table_rows();
    rows.extend(command_rows(cmds, prefix));
    vec![
        rich_title(grp),
        tl::types::PageBlockTable {
            bordered: true,
            striped: false,
            compact: false,
            title: tl::enums::RichText::TextEmpty,
            rows,
        }
        .into(),
        tl::types::PageBlockParagraph {
            text: rich_concat(&[
                rich_code(&format!("{prefix}help 命令")),
                rich_plain(" 看用法"),
            ]),
        }
        .into(),
    ]
}

pub fn find_group(name: &str) -> Option<&'static str> {
    let trimmed = name.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    let mut found = None;
    for &grp in GROUPS {
        if grp.to_ascii_lowercase().contains(&trimmed) {
            if found.is_some() {
                return None;
            }
            found = Some(grp);
        }
    }
    found
}

pub fn lookup_for_help(registry: &Registry, raw_name: &str) -> Option<Arc<Command>> {
    let mut name = raw_name;
    for p in registry.prefixes() {
        if let Some(trimmed) = name.strip_prefix(p.as_str())
            && !trimmed.is_empty()
        {
            name = trimmed;
            break;
        }
    }
    let first_prefix = registry
        .prefixes()
        .into_iter()
        .next()
        .unwrap_or_else(|| ".".into());
    let route = registry
        .parse(&format!("{first_prefix}{name}"))
        .or_else(|| registry.parse(&format!("{first_prefix}{}", name.to_ascii_lowercase())))?;
    registry.command(&route.command)
}

pub fn render_command_help(registry: &Registry, prefix: &str, name: &str) -> String {
    if let Some(cmd) = lookup_for_help(registry, name) {
        return registry.help_text_for(&cmd, prefix);
    }
    if let Some(grp) = find_group(name) {
        return render_group_help(registry, prefix, grp);
    }
    format!(
        "❌ 没有这个命令或分组：{}，{} 看全部命令",
        code(name),
        code(&format!("{prefix}help"))
    )
}

pub async fn probe(target: &str) -> String {
    if !ping_target_re().is_match(target) {
        return "❌ 无效的目标：只能是域名或 IP".to_owned();
    }
    let started = Instant::now();
    let host = match target.parse::<IpAddr>() {
        Ok(ip) if ip.is_ipv6() => format!("[{target}]"),
        _ => target.to_owned(),
    };
    match do_request(Request {
        method: Method::HEAD,
        url: format!("https://{host}"),
        timeout: Some(Duration::from_secs(5)),
        max_bytes: Some(1),
        ..Default::default()
    })
    .await
    {
        Ok(resp) => code(&format!(
            "{target}: HTTP {}，{} ms",
            resp.status,
            started.elapsed().as_millis()
        )),
        Err(_) => "❌ 网络测试失败或目标不可达".to_owned(),
    }
}

fn ping_help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "🏓 <b>延迟测试</b>\n\n\
         • <code>{p}ping</code> 测 Telegram 接口和消息编辑的延迟\n\
         • <code>{p}ping example.com</code> 对 https://example.com 发一次 HEAD 请求，显示状态码和耗时（5 秒超时）"
    )
}

fn status_help(prefix: &str) -> String {
    format!(
        "📊 <b>运行状态</b>\n\n<code>{}status</code> 发一张状态卡片：健康状态、在线时长，\
         CPU、内存、磁盘、Swap 的占用；卡片下面附进程和主机的文字说明。CPU 要采样约 160 毫秒。",
        escape(prefix)
    )
}

fn version_help(prefix: &str) -> String {
    format!(
        "🔖 <b>版本信息</b>\n\n{}（简写 {}）显示 MiBot Lite 和 Rust 的版本、平台和进程号。",
        code(&format!("{prefix}version")),
        code(&format!("{prefix}ver")),
    )
}

fn memory_help(prefix: &str) -> String {
    format!(
        "🧠 <b>内存状态</b>\n\n{} 显示本进程的 RSS、堆内存、运行时预留的内存和任务数。",
        code(&format!("{prefix}memory"))
    )
}

fn sysinfo_help(prefix: &str) -> String {
    format!(
        "🖥 <b>系统信息</b>\n\n{} 显示主机名、系统、内核、在线时长、负载、CPU 和内存，以及本进程的版本和内存。\
         里面有主机名，只限本人使用；借给别人的是 {}。",
        code(&format!("{prefix}sysinfo")),
        code(&format!("{prefix}status")),
    )
}

fn help_help(prefix: &str) -> String {
    format!(
        "📖 <b>命令帮助</b>\n\n\
         • {} 按分组列出全部命令，说明折在引用里，点开展开\n\
         • {} 列出一组命令和各自的说明，分组名写一段就行（{}）\n\
         • {} 看一条命令的用法和说明，命令可以带前缀、写简写或别名\n\
         • {} 同上，只显示帮助、不执行",
        code(&format!("{prefix}help")),
        code(&format!("{prefix}help 分组")),
        code(&format!("{prefix}help 群组")),
        code(&format!("{prefix}help 命令")),
        code("命令 --help"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_tl_types::Serializable;
    use mibot_bot::{input_rich_fits, rich_message_text};
    use std::collections::HashMap;

    fn noop_cmd(name: &str, aliases: &[&str], grp: &str, desc: &str) -> Command {
        Command {
            name: name.into(),
            aliases: aliases.iter().map(|s| (*s).into()).collect(),
            group: grp.into(),
            description: desc.into(),
            handle: Arc::new(|_| Box::pin(async { Ok(()) })),
            ..Default::default()
        }
    }

    #[test]
    fn test_lookup_for_help() {
        let reg = Registry::new();
        reg.set_prefixes(vec![".".into(), "。".into()]);
        reg.register(noop_cmd("speedtest", &[], group::TOOLS, "测速"));
        reg.register(noop_cmd("ping", &[], group::SYSTEM, "测试延迟"));
        let mut aliases = HashMap::new();
        aliases.insert("测速".into(), "speedtest 12345".into());
        aliases.insert("网络 测速".into(), "speedtest 12345".into());
        reg.set_aliases(&aliases);

        for (name, want) in [
            ("ping", "ping"),
            (".ping", "ping"),
            ("。ping", "ping"),
            ("PING", "ping"),
            ("测速", "speedtest"),
            (".网络 测速", "speedtest"),
        ] {
            let cmd = lookup_for_help(&reg, name).expect(name);
            assert_eq!(cmd.name, want, "{name}");
        }
        assert!(lookup_for_help(&reg, "nosuch").is_none());
    }

    #[tokio::test]
    async fn test_probe_rejects_bad_targets() {
        let long = "a".repeat(254);
        for bad in ["a b", "http://x", "x/y", "a;b", &long] {
            let got = probe(bad).await;
            assert!(got.contains("无效的目标"), "{bad:?} -> {got}");
        }
    }

    #[test]
    fn test_help_list_and_groups() {
        let reg = Registry::new();
        reg.set_prefixes(vec![".".into()]);
        reg.register(noop_cmd("ping", &[], group::SYSTEM, "测试网络延迟"));
        reg.register(noop_cmd("speedtest", &["st"], group::TOOLS, "测量主机网络速度"));
        let mut ai = noop_cmd("ai", &[], group::AI, "与 AI 对话");
        ai.help_fn = Some(Arc::new(|_| "ai 的帮助".into()));
        reg.register(ai);

        let list = render_help_list(&reg, ".");
        let (names, folded) = list.split_once("<blockquote expandable>").unwrap();
        assert!(folded.ends_with("</blockquote>"));
        for want in [
            "<b>运行与维护</b>\n<code>.ping</code>",
            "<b>查询与工具</b>\n<code>.speedtest</code>",
            "<code>.help 命令</code>",
        ] {
            assert!(names.contains(want), "missing {want}:\n{names}");
        }
        assert!(!names.contains("测试网络延迟") && !names.contains(".st") && !list.contains("账号"));
        assert!(folded.contains("<b>查询与工具</b>\n<code>.speedtest</code>（<code>.st</code>） — 测量主机网络速度"));

        let grp = render_command_help(&reg, ".", "工具");
        assert!(grp.contains("<code>.speedtest</code>（<code>.st</code>） — 测量主机网络速度") && !grp.contains(".ping"));
        assert_eq!(render_command_help(&reg, ".", "ai"), "ai 的帮助");
        assert!(render_command_help(&reg, ".", "nosuch").starts_with("❌ 没有这个命令或分组"));

        for (name, want) in [
            ("群组", group::ADMIN),
            ("消息", group::MEDIA),
            ("运行", group::SYSTEM),
            ("账号", group::ACCOUNT),
            ("查询与工具", group::TOOLS),
        ] {
            assert_eq!(find_group(name), Some(want), "{name}");
        }
        assert_eq!(find_group("与"), None);
    }

    fn msg_from_blocks(blocks: Vec<tl::enums::PageBlock>) -> tl::types::Message {
        tl::types::Message {
            out: false,
            mentioned: false,
            media_unread: false,
            silent: false,
            post: false,
            from_scheduled: false,
            legacy: false,
            edit_hide: false,
            pinned: false,
            noforwards: false,
            invert_media: false,
            offline: false,
            video_processing_pending: false,
            paid_suggested_post_stars: false,
            paid_suggested_post_ton: false,
            id: 1,
            from_id: None,
            from_boosts_applied: None,
            from_rank: None,
            peer_id: tl::types::PeerUser { user_id: 1 }.into(),
            saved_peer_id: None,
            fwd_from: None,
            via_bot_id: None,
            via_business_bot_id: None,
            guestchat_via_from: None,
            reply_to: None,
            date: 1,
            message: String::new(),
            media: None,
            reply_markup: None,
            entities: None,
            views: None,
            forwards: None,
            replies: None,
            edit_date: None,
            post_author: None,
            grouped_id: None,
            reactions: None,
            restriction_reason: None,
            ttl_period: None,
            quick_reply_shortcut_id: None,
            effect: None,
            factcheck: None,
            report_delivery_until_date: None,
            paid_message_stars: None,
            suggested_post: None,
            schedule_repeat_period: None,
            summary_from_language: None,
            rich_message: Some(
                tl::types::RichMessage {
                    rtl: false,
                    part: false,
                    blocks,
                    photos: vec![],
                    documents: vec![],
                }
                .into(),
            ),
        }
    }

    #[test]
    fn test_help_rich() {
        let reg = Registry::new();
        reg.set_prefixes(vec![".".into(), "$".into()]);
        reg.register(noop_cmd("ping", &[], group::SYSTEM, "测试网络延迟"));
        reg.register(noop_cmd("speedtest", &["st"], group::TOOLS, "测量主机网络速度"));

        let list = help_list_rich(&reg, ".");
        assert_eq!(list.len(), 5);
        assert_eq!(
            rich_message_text(&msg_from_blocks(list[..4].to_vec())),
            "📖 命令列表\n运行与维护\n.ping\n查询与工具\n.speedtest\n.help 命令 看用法 · 前缀：. $"
        );
        let tl::enums::PageBlock::Details(ref details) = list[4] else {
            panic!("expected Details block");
        };
        assert!(!details.open);
        let tl::enums::PageBlock::Table(ref table) = details.blocks[0] else {
            panic!("expected Table block");
        };
        assert_eq!(table.rows.len(), 5);
        let grp = group_help_rich(&reg, ".", group::TOOLS);
        assert_eq!(
            rich_message_text(&msg_from_blocks(grp.clone())),
            "📖 查询与工具\n命令 | 说明\n.speedtest（.st） | 测量主机网络速度\n.help 命令 看用法"
        );
        for blocks in [list, grp] {
            let input: tl::enums::InputRichMessage = tl::types::InputRichMessage {
                rtl: false,
                noautolink: false,
                blocks,
                photos: None,
                documents: None,
                users: None,
            }
            .into();
            assert!(input_rich_fits(&input));
            assert!(!input.to_bytes().is_empty());
        }
    }
}
