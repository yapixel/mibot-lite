use crate::ai::Service as AiService;
use crate::kit;
use mibot_app::App;
use mibot_bot::{BotError, message_text};
use mibot_command::{CmdResult, Invocation, PAGE_LIMIT, escape, escaped_pages, group};
use std::sync::Arc;
use std::time::Duration;

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "📘 <b>AI 翻译</b>\n\n• <code>{p}gt 文本</code> 翻译为简体中文\n\
         • <code>{p}gt en 文本</code> 翻译为英文\n\
         • 回复消息后使用 <code>{p}gt</code> 或 <code>{p}gt en</code>\n\n\
         使用 ai 的当前聊天 API、模型及超时设置，请先用 <code>{p}ai config add</code> 和 <code>{p}ai model chat</code> 配置。单次最多 5000 字符，长译文自动分段发送。"
    )
}

async fn handle(models: Option<&Arc<AiService>>, inv: &Invocation) -> CmdResult {
    let mut text = inv.raw_after(0).trim_start();
    let first = inv.arg(0).to_ascii_lowercase();
    if inv.args.len() == 1 && (first == "help" || first == "h") {
        return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
    }
    let mut target = "zh-CN";
    if first == "en" {
        target = "en";
        text = inv.raw_after(1).trim_start();
    }
    let mut text_owned = text.to_owned();
    if text_owned.trim().is_empty()
        && let Some(reply) = kit::reply_msg(inv).await?
    {
        text_owned = message_text(Some(&reply.raw));
    }
    if text_owned.trim().is_empty() {
        return Err(kit::fail("请提供要翻译的文本，或回复一条文字消息"));
    }
    if kit::utf16_len_usize(&text_owned) > 5000 {
        return Err(kit::fail("文本过长，请保持在 5000 字以内"));
    }
    let Some(models) = models else {
        return Err(kit::fail("请先配置 ai 聊天模型"));
    };
    inv.edit_text(&kit::working("正在翻译")).await?;
    let translated = match models.translate(&text_owned, target).await {
        Ok(t) => t,
        Err(err) => {
            if inv.cancel.is_cancelled() {
                return Err(BotError::Cancelled.into());
            }
            return Err(kit::wrap_fail("AI 翻译失败", err));
        }
    };
    let preview = kit::truncate_runes(&text_owned, 50);
    let suffix = if text_owned.chars().count() > 50 {
        "…"
    } else {
        ""
    };
    let language = if target == "en" { "英文" } else { "中文" };
    let mut pages = escaped_pages(&translated, PAGE_LIMIT);
    pages[0] = format!(
        "📘 <b>AI 翻译结果</b>（→ {language}）\n\n<b>原文</b>\n<code>{}{}</code>\n\n<b>译文</b>\n{}",
        escape(&preview),
        suffix,
        pages[0]
    );
    inv.respond_pages(&pages).await.map_err(Into::into)
}

pub fn register(a: &Arc<App>, models: Option<Arc<AiService>>) {
    let mut cmd = kit::make_cmd(
        "gt",
        &[],
        group::TOOLS,
        "用 AI 翻译文本",
        "[en] 文本",
        help,
        move |inv: Invocation| {
            let models = models.clone();
            Box::pin(async move { handle(models.as_ref(), &inv).await })
        },
    );
    cmd.timeout = Some(Duration::from_secs(15 * 60));
    cmd.free_text = true;
    a.registry.register(cmd);
}
