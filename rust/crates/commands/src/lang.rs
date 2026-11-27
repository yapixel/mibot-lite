use crate::kit;
use mibot_app::App;
use mibot_command::{Invocation, code, escape, group};
use mibot_i18n::Lang;
use mibot_logtail::Level;
use std::sync::Arc;
use tokio::sync::Mutex;

fn lang_help(prefix: &str) -> String {
    let p = escape(prefix);
    if mibot_i18n::is_zh() {
        format!(
            "🗺 <b>界面语言</b>\n\n\
             • <code>{p}lang</code> 查看当前语言\n\
             • <code>{p}lang en</code> 切换为英文（默认）\n\
             • <code>{p}lang zh</code> 切换为简体中文\n\n\
             改完立即生效，并写进 <code>.env</code> 的 <code>MIBOT_LANG</code>，重启后保持。"
        )
    } else {
        format!(
            "🗺 <b>Interface Language</b>\n\n\
             • <code>{p}lang</code> Show current language\n\
             • <code>{p}lang en</code> Switch to English (default)\n\
             • <code>{p}lang zh</code> Switch to Simplified Chinese\n\n\
             Takes effect immediately and persists <code>MIBOT_LANG</code> in <code>.env</code> across restarts."
        )
    }
}

pub fn register(a: &Arc<App>) {
    let app = Arc::clone(a);
    let mu = Arc::new(Mutex::new(()));
    a.registry.register(
        kit::make_cmd(
            "lang",
            &[],
            group::SYSTEM,
            "Switch UI language",
            "[en|zh]",
            lang_help,
            move |inv: Invocation| {
                let app = Arc::clone(&app);
                let mu = Arc::clone(&mu);
                Box::pin(async move {
                    let _guard = mu.lock().await;
                    let arg = inv.arg(0).trim();
                    if arg.is_empty() {
                        let cur = mibot_i18n::current();
                        let p = escape(inv.prefix());
                        return inv
                            .edit(&format!(
                                "🗺 {}: {} ({})\n<i>{}</i>",
                                kit::tr("Current language", "当前语言"),
                                code(cur.code()),
                                escape(cur.label()),
                                if mibot_i18n::is_zh() {
                                    format!("{p}lang en | zh 切换")
                                } else {
                                    format!("{p}lang en or {p}lang zh to switch")
                                }
                            ))
                            .await
                            .map_err(Into::into);
                    }
                    let Some(target) = Lang::parse(arg) else {
                        return Err(kit::fail_err(kit::tr(
                            "Unsupported language; use en or zh",
                            "不支持的语言，请输入 en 或 zh",
                        )));
                    };
                    mibot_i18n::set(target);

                    let mut note = kit::tr(
                        "Saved to .env and will persist after restart",
                        "已写入 .env，重启后仍然有效",
                    )
                    .to_owned();
                    if let Err(err) = mibot_config::set_env(&app.root, "MIBOT_LANG", target.code())
                    {
                        let err_s = err.to_string();
                        app.logs.record(
                            Level::Error,
                            "lang.persist_failed",
                            &[("error", &err_s)],
                        );
                        note = kit::tr(
                            "⚠️ Failed to write .env; change applies to this run only",
                            "⚠️ 写入 .env 失败，只在本次运行里生效",
                        )
                        .to_owned();
                    } else if let Ok(val) = std::env::var("MIBOT_LANG")
                        && !val.trim().is_empty()
                    {
                        note = kit::tr(
                            "⚠️ Saved to .env, but MIBOT_LANG is also set in process env and takes precedence on restart",
                            "⚠️ 已写入 .env，但服务的环境变量里也设了 MIBOT_LANG，重启后会以那个为准",
                        )
                        .to_owned();
                    }

                    inv.edit(&format!(
                        "✅ {}: {} ({})\n<i>{}</i>",
                        kit::tr("Language switched to", "语言已切换为"),
                        code(target.code()),
                        escape(target.label()),
                        escape(&note),
                    ))
                    .await
                    .map_err(Into::into)
                })
            },
        )
        .zh("查看或切换界面语言", "[en|zh]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_help_renders_both_languages() {
        mibot_i18n::with_lang(Lang::En, || {
            let h = lang_help(".");
            assert!(h.contains("Interface Language") && h.contains(".lang en"));
        });
        mibot_i18n::with_lang(Lang::Zh, || {
            let h = lang_help(".");
            assert!(h.contains("界面语言") && h.contains(".lang zh"));
        });
    }
}
