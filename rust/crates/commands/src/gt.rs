use crate::ai::Service as AiService;
use crate::kit;
use mibot_app::App;
use mibot_bot::{BotError, message_text};
use mibot_command::{CmdResult, Invocation, PAGE_LIMIT, escape, escaped_pages, group};
use std::sync::Arc;
use std::time::Duration;

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "📘 <b>AI 翻译</b>\n\n• <code>{p}gt 文本</code> 翻译为简体中文\n\
             • <code>{p}gt en 文本</code> 翻译为英文\n\
             • 回复消息后使用 <code>{p}gt</code> 或 <code>{p}gt en</code>\n\n\
             使用 ai 的当前聊天 API、模型及超时设置，请先用 <code>{p}ai config add</code> 和 <code>{p}ai model chat</code> 配置。单次最多 5000 字符，长译文自动分段发送。"
        )
    } else {
        format!(
            "📘 <b>AI Translate</b>\n\n• <code>{p}gt text</code> Translate to Simplified Chinese\n\
             • <code>{p}gt en text</code> Translate to English\n\
             • Reply to a message with <code>{p}gt</code> or <code>{p}gt en</code>\n\n\
             Uses the current chat API, model, and timeout from ai; configure with <code>{p}ai config add</code> and <code>{p}ai model chat</code> first. Up to 5000 characters per request; long translations are split automatically."
        )
    }
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
        return Err(kit::fail(kit::tr(
            "Please provide text to translate or reply to a text message",
            "请提供要翻译的文本，或回复一条文字消息",
        )));
    }
    if kit::utf16_len_usize(&text_owned) > 5000 {
        return Err(kit::fail(kit::tr(
            "Text is too long; keep it within 5000 characters",
            "文本过长，请保持在 5000 字以内",
        )));
    }
    let Some(models) = models else {
        return Err(kit::fail(kit::tr(
            "Please configure an ai chat model first",
            "请先配置 ai 聊天模型",
        )));
    };
    inv.edit_text(&kit::working(kit::tr("Translating", "正在翻译"))).await?;
    let translated = match models.translate(&text_owned, target).await {
        Ok(t) => t,
        Err(err) => {
            if inv.cancel.is_cancelled() {
                return Err(BotError::Cancelled.into());
            }
            return Err(kit::wrap_fail(kit::tr("AI translation failed", "AI 翻译失败"), err));
        }
    };
    let preview = kit::truncate_runes(&text_owned, 50);
    let suffix = if text_owned.chars().count() > 50 {
        "…"
    } else {
        ""
    };
    pages_response(inv, target, &preview, suffix, &translated).await
}

async fn pages_response(
    inv: &Invocation,
    target: &str,
    preview: &str,
    suffix: &str,
    translated: &str,
) -> CmdResult {
    let mut pages = escaped_pages(translated, PAGE_LIMIT);
    if kit::is_zh() {
        let language = if target == "en" { "英文" } else { "中文" };
        pages[0] = format!(
            "📘 <b>AI 翻译结果</b>（→ {language}）\n\n<b>原文</b>\n<code>{}{}</code>\n\n<b>译文</b>\n{}",
            escape(preview),
            suffix,
            pages[0]
        );
    } else {
        let language = if target == "en" { "English" } else { "Chinese" };
        pages[0] = format!(
            "📘 <b>AI Translation</b> (→ {language})\n\n<b>Source</b>\n<code>{}{}</code>\n\n<b>Translation</b>\n{}",
            escape(preview),
            suffix,
            pages[0]
        );
    }
    inv.respond_pages(&pages).await.map_err(Into::into)
}

pub fn register(a: &Arc<App>, models: Option<Arc<AiService>>) {
    let mut cmd = kit::make_cmd(
        "gt",
        &[],
        group::TOOLS,
        "Translate text with AI",
        "[en] text",
        help,
        move |inv: Invocation| {
            let models = models.clone();
            Box::pin(async move { handle(models.as_ref(), &inv).await })
        },
    )
    .zh("用 AI 翻译文本", "[en] 文本");
    cmd.timeout = Some(Duration::from_secs(15 * 60));
    cmd.free_text = true;
    a.registry.register(cmd);
}
