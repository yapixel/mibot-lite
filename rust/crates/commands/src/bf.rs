use crate::kit;
use chrono::{Local, Utc};
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::MediaOptions;
use mibot_command::{Command, Invocation, escape, group};
use mibot_logtail::Level;
use std::sync::Arc;
use std::time::Duration;

const INSTALL_URL: &str = "https://raw.githubusercontent.com/Yapixel/mibot-lite/rust/scripts/install.sh";
const CAPTION_FILE_LIMIT: usize = 12;

pub fn backup_caption(version: &str, names: &[String], size: usize) -> String {
    let listed = &names[..names.len().min(CAPTION_FILE_LIMIT)];
    let quoted: Vec<String> = listed.iter().map(|n| escape(n)).collect();
    if kit::is_zh() {
        let more = if names.len() > CAPTION_FILE_LIMIT {
            format!(" 等 {} 个", names.len())
        } else {
            String::new()
        };
        return format!(
            "📦 <b>mibot-lite 配置备份</b> · {}\n{} 个文件，{}：{quoted}{more}\n\n\
             ⚠️ <b>这个文件就是你的账号</b>：里面有登录会话和各命令的 API 密钥。不要转发给任何人。\n\n\
             <b>在新主机上恢复</b>\n\
             <code>bash &lt;(curl -fsSL {INSTALL_URL}) --restore 备份文件路径</code>\n\
             恢复前先停掉旧主机上的服务：同一个账号不能在两处同时在线。",
            escape(version),
            names.len(),
            escape(&kit::format_bytes(size as i64)),
            quoted = quoted.join("、"),
        );
    }
    let more = if names.len() > CAPTION_FILE_LIMIT {
        format!(" ({} total)", names.len())
    } else {
        String::new()
    };
    let count_label = if names.len() == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", names.len())
    };
    format!(
        "📦 <b>mibot-lite Config Backup</b> · {}\n{count_label}, {}: {quoted}{more}\n\n\
         ⚠️ <b>This file is your account</b>: it contains your login session and command API keys. Do not forward it to anyone.\n\n\
         <b>Restore on a new host</b>\n\
         <code>bash &lt;(curl -fsSL {INSTALL_URL}) --restore path/to/backup</code>\n\
         Stop the service on the old host first: the same account cannot stay online in two places at once.",
        escape(version),
        escape(&kit::format_bytes(size as i64)),
        quoted = quoted.join(", "),
    )
}

pub fn backup_help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        return format!(
            "📦 <b>配置备份</b>\n\n把这个部署的配置打包，发到本账号的收藏夹。重装系统或换主机时用它恢复，不用重新登录，也不用重新配各命令。\n\n\
             • <code>{p}bf</code> 打包并发到收藏夹\n\n\
             <b>包含</b>\n登录会话（config.json、gotd-session.json）、.env，以及 data/ 下每个命令的配置。\n\n\
             <b>不包含</b>\neatgif 素材和测速 CLI 这类缓存，用到时会自己重新下载；也不含程序本身。\n\n\
             <b>恢复</b>\n在新主机上把备份文件传上去，然后运行：\n\
             <code>bash &lt;(curl -fsSL {INSTALL_URL}) --restore 备份文件路径</code>\n\
             已经装好的主机可以用 <code>mibot-lite --restore 文件 --root 部署目录</code>，\
             要覆盖已有账号得加 <code>--force</code>，并且先停掉服务。\n\n\
             ⚠️ 无论在哪个对话里执行，备份都只发到收藏夹，不会出现在当前对话。\
             这个文件等同于你的账号，不要转发给别人——要给别人看问题，用 <code>{p}log</code>，那个是脱敏的。"
        );
    }
    format!(
        "📦 <b>Config Backup</b>\n\nPackage this deployment's configuration and send it to your Saved Messages. Use it to restore when reinstalling or switching hosts without logging in or reconfiguring commands.\n\n\
         • <code>{p}bf</code> Package and send to Saved Messages\n\n\
         <b>Included</b>\nLogin session (config.json, gotd-session.json), .env, and per-command configs under data/.\n\n\
         <b>Excluded</b>\nCaches like eatgif assets and the speedtest CLI (re-downloaded on demand), and the binary itself.\n\n\
         <b>Restore</b>\nUpload the backup file to the new host and run:\n\
         <code>bash &lt;(curl -fsSL {INSTALL_URL}) --restore path/to/backup</code>\n\
         On an already installed host, run <code>mibot-lite --restore file --root deploy_dir</code>. \
         Add <code>--force</code> to overwrite an existing account, and stop the service first.\n\n\
         ⚠️ Regardless of where you run this, the backup is only sent to Saved Messages, never the current chat. \
         This file is equivalent to your account—do not forward it to others. To share diagnostics, use <code>{p}log</code>, which is sanitized."
    )
}

pub fn register(a: &Arc<App>) {
    let app = Arc::clone(a);
    let mut cmd = kit::make_cmd(
        "bf",
        &[],
        group::SYSTEM,
        "Back up config to Saved",
        "",
        backup_help,
        move |inv: Invocation| {
            let app = Arc::clone(&app);
            Box::pin(async move {
                inv.edit_text(&kit::working(kit::tr(
                    "Packaging configuration",
                    "正在打包配置",
                )))
                .await?;
                let (archive, names) = mibot_backup::create(&app.root, &app.version, Utc::now())
                    .map_err(|e| kit::fail_with_err(kit::tr("Backup failed", "备份失败"), e))?;
                let name = format!(
                    "mibot-lite-backup-{}.tar.gz",
                    Local::now().format("%Y%m%d-%H%M")
                );
                let caption = backup_caption(&app.version, &names, archive.len());
                inv.client
                    .send_document(
                        &tl::enums::InputPeer::PeerSelf,
                        &archive,
                        MediaOptions {
                            name,
                            mime_type: "application/gzip".to_owned(),
                            caption,
                            html: true,
                            ..Default::default()
                        },
                    )
                    .await?;
                let files_s = names.len().to_string();
                let bytes_s = archive.len().to_string();
                app.logs.record(
                    Level::Info,
                    "backup.sent",
                    &[("files", &files_s), ("bytes", &bytes_s)],
                );
                let done = if kit::is_zh() {
                    format!("✅ 备份已发到收藏夹（{} 个文件）", names.len())
                } else {
                    format!("✅ Backup sent to Saved Messages ({} files)", names.len())
                };
                inv.edit(&done).await.map_err(Into::into)
            })
        },
    )
    .zh("备份配置到收藏夹", "");
    cmd.timeout = Some(Duration::from_secs(120));
    let _: Command = cmd.clone();
    a.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn test_backup_caption_fits() {
        let mut names = vec![
            "config.json".into(),
            "gotd-session.json".into(),
            ".env".into(),
        ];
        for i in 0..40 {
            names.push(format!("data/command{i:02}.json"));
        }
        let caption = backup_caption("0.1.12", &names, 48000);
        let plain = Regex::new("<[^>]+>").unwrap().replace_all(&caption, "");
        assert!(plain.chars().count() <= 1024, "caption too long: {}", plain.chars().count());
        for wanted in ["--restore", "Do not forward", "43 files"] {
            assert!(caption.contains(wanted), "missing {wanted} in {plain}");
        }
        mibot_i18n::with_lang(mibot_i18n::Lang::Zh, || {
            let zh = backup_caption("0.1.12", &names, 48000);
            assert!(zh.contains("不要转发") && zh.contains("43 个文件"));
        });
    }

    #[test]
    fn test_backup_help_says_where_it_goes_and_how_to_restore() {
        let text = backup_help(".");
        for wanted in ["Saved Messages", "--restore", "--force", ".log"] {
            assert!(text.contains(wanted), "missing {wanted}");
        }
        mibot_i18n::with_lang(mibot_i18n::Lang::Zh, || {
            assert!(backup_help(".").contains("收藏夹"));
        });
    }
}
