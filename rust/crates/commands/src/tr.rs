use crate::kit;
use mibot_app::App;
use mibot_command::{
    CmdResult, Invocation, PAGE_LIMIT, code, escape, escaped_pages, group,
};
use mibot_httpx::{Method, Request, do_request};
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const TRANSLATE_ENDPOINT: &str = "https://clients5.google.com/translate_a/t?client=dict-chrome-ex";
const TRANSLATE_LIMIT: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct TranslateConfig {
    target: String,
}

impl Default for TranslateConfig {
    fn default() -> Self {
        Self {
            target: "zh-CN".into(),
        }
    }
}

fn languages() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            ("ar", "阿拉伯语"),
            ("bg", "保加利亚语"),
            ("cs", "捷克语"),
            ("da", "丹麦语"),
            ("de", "德语"),
            ("el", "希腊语"),
            ("en", "英语"),
            ("es", "西班牙语"),
            ("fa", "波斯语"),
            ("fi", "芬兰语"),
            ("fr", "法语"),
            ("he", "希伯来语"),
            ("hi", "印地语"),
            ("hu", "匈牙利语"),
            ("id", "印尼语"),
            ("it", "意大利语"),
            ("ja", "日语"),
            ("ko", "韩语"),
            ("ms", "马来语"),
            ("nl", "荷兰语"),
            ("no", "挪威语"),
            ("pl", "波兰语"),
            ("pt", "葡萄牙语"),
            ("ro", "罗马尼亚语"),
            ("ru", "俄语"),
            ("sv", "瑞典语"),
            ("th", "泰语"),
            ("tr", "土耳其语"),
            ("uk", "乌克兰语"),
            ("vi", "越南语"),
            ("zh-CN", "简体中文"),
            ("zh-TW", "繁体中文"),
        ])
    })
}

fn canonical_language(value: &str) -> String {
    let trimmed = value.trim();
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        "zh" | "cn" | "zh-cn" | "zh-hans" | "chinese" | "中文" | "简体" => "zh-CN".into(),
        "tw" | "zh-tw" | "zh-hant" | "繁体" | "繁體" => "zh-TW".into(),
        "jp" | "ja" | "日语" | "日文" => "ja".into(),
        "kr" | "ko" | "韩语" => "ko".into(),
        "en" | "eng" | "english" | "英语" | "英文" => "en".into(),
        _ if languages().contains_key(lower.as_str()) => lower,
        _ => value.to_owned(),
    }
}

fn language_name(code_s: &str) -> String {
    let canon = canonical_language(code_s);
    languages()
        .get(canon.as_str())
        .map(|s| (*s).to_owned())
        .unwrap_or_else(|| code_s.to_owned())
}

fn named_language(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let canon = canonical_language(value);
    languages().contains_key(canon.as_str()).then_some(canon)
}

fn has_han(text: &str) -> bool {
    text.chars().any(|c| {
        let u = c as u32;
        (0x4E00..=0x9FFF).contains(&u)
            || (0x3400..=0x4DBF).contains(&u)
            || (0xF900..=0xFAFF).contains(&u)
            || (0x20000..=0x2EBEF).contains(&u)
    })
}

#[derive(Debug, PartialEq, Eq)]
struct TranslateResult {
    text: String,
    source: String,
}

fn parse_translation(raw: &[u8]) -> Result<TranslateResult, mibot_command::CmdError> {
    let outer: Vec<Value> = serde_json::from_slice(raw)
        .ok()
        .filter(|v: &Vec<Value>| !v.is_empty())
        .ok_or_else(|| kit::fail("翻译服务返回了无法解析的内容"))?;
    match &outer[0] {
        Value::String(s) => {
            if s.trim().is_empty() {
                return Err(kit::fail("翻译结果为空"));
            }
            Ok(TranslateResult {
                text: s.clone(),
                source: String::new(),
            })
        }
        Value::Array(arr) if !arr.is_empty() => {
            let Some(txt) = arr[0].as_str() else {
                return Err(kit::fail("翻译服务返回了无法解析的内容"));
            };
            if txt.trim().is_empty() {
                return Err(kit::fail("翻译结果为空"));
            }
            let src = arr.get(1).and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Ok(TranslateResult {
                text: txt.to_owned(),
                source: src,
            })
        }
        _ => Err(kit::fail("翻译服务返回了无法解析的内容")),
    }
}

async fn translate_text(
    cancel: &CancellationToken,
    endpoint_base: &str,
    text: &str,
    target: &str,
) -> Result<TranslateResult, mibot_command::CmdError> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", text)
        .finish();
    let tl = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("tl", target)
        .finish();
    let url = format!("{endpoint_base}&sl=auto&{tl}");
    let mut headers = HashMap::new();
    headers.insert("Content-Type".into(), "application/x-www-form-urlencoded".into());
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(kit::fail("操作已取消")),
        r = do_request(Request {
            method: Method::POST,
            url,
            headers,
            body: Some(form.into_bytes()),
            timeout: Some(Duration::from_secs(30)),
            max_bytes: Some(4 << 20),
            ..Default::default()
        }) => r?,
    };
    if resp.status == 429 {
        return Err(kit::fail("翻译服务暂时拒绝了请求（429），稍后再试"));
    }
    if !resp.ok() {
        return Err(kit::fail(format!("翻译服务返回 HTTP {}", resp.status)));
    }
    parse_translation(&resp.body)
}

fn translate_help(prefix: &str) -> String {
    let p = escape(prefix);
    let mut codes: Vec<&str> = languages().keys().copied().collect();
    codes.sort_unstable();
    format!(
        "🌐 <b>翻译</b>\n\n直接调用谷歌翻译，不需要任何配置或 API Key。\n\n\
         • <code>{p}tr 文本</code> 翻译。中文译成英文，其他语言译成中文\n\
         • <code>{p}tr en 文本</code> 指定目标语言\n\
         • 回复一条消息发 <code>{p}tr</code> 翻译那条消息\n\
         • 回复消息发 <code>{p}tr ja</code> 译成指定语言\n\
         • <code>{p}tr set 语言</code> 设置默认目标语言\n\n\
         <b>语言代码</b>\n<blockquote expandable>{}</blockquote>\n\
         单次最多 {} 字符，长译文自动分段。",
        escape(&codes.join(" ")),
        code(&TRANSLATE_LIMIT.to_string()),
    )
}

async fn run_translate(
    inv: &Invocation,
    settings: &Store<TranslateConfig>,
) -> CmdResult {
    let first = inv.arg(0).to_ascii_lowercase();
    if inv.args.len() == 1 && (first == "help" || first == "h") {
        return inv.edit(&translate_help(inv.prefix())).await.map_err(Into::into);
    }
    let mut config = settings.read()?;
    if config.target.is_empty() {
        config.target = "zh-CN".into();
    }
    if first == "set" {
        let Some(code_s) = named_language(inv.arg(1)) else {
            return Err(kit::fail(format!(
                "不认识的语言代码：{}。可用代码见 {}tr help",
                inv.arg(1),
                inv.prefix()
            )));
        };
        settings.update(|c| {
            c.target = code_s.clone();
            Ok::<(), mibot_store::StoreError>(())
        })?;
        return inv
            .edit(&kit::feedback(
                "success",
                "默认目标语言已设置",
                &format!("{}（{code_s}）", language_name(&code_s)),
            ))
            .await
            .map_err(Into::into);
    }
    let named_target = named_language(&first);
    let rest = if named_target.is_some() { 1 } else { 0 };
    let mut text = inv.raw_after(rest).to_owned();
    if text.trim().is_empty()
        && let Some(reply) = kit::reply_msg(inv).await?
    {
        text = reply.content();
    }
    if text.trim().is_empty() {
        return inv.edit(&translate_help(inv.prefix())).await.map_err(Into::into);
    }
    if mibot_bot::utf16_len(&text) > TRANSLATE_LIMIT as i32 {
        return Err(kit::fail(format!(
            "文本过长，请保持在 {TRANSLATE_LIMIT} 字符以内"
        )));
    }
    let mut target = named_target.clone().unwrap_or(config.target);
    if named_target.is_none() && has_han(&text) && canonical_language(&target) == "zh-CN" {
        target = "en".into();
    }
    inv.edit(&kit::feedback("working", "翻译中", "")).await?;
    let res = translate_text(&inv.cancel, TRANSLATE_ENDPOINT, &text, &target).await?;
    let source = if res.source.is_empty() {
        "自动检测".into()
    } else {
        language_name(&res.source)
    };
    let preview = kit::truncate_runes(&text, 60);
    let suffix = if text.chars().count() > 60 { "…" } else { "" };
    let mut pages = escaped_pages(&res.text, PAGE_LIMIT);
    pages[0] = format!(
        "🌐 <b>翻译</b>（{} → {}）\n\n<b>原文</b>\n<code>{}{}</code>\n\n<b>译文</b>\n{}",
        escape(&source),
        escape(&language_name(&target)),
        escape(&preview),
        suffix,
        pages[0],
    );
    inv.respond_pages(&pages).await.map_err(Into::into)
}

pub fn register(app: &Arc<App>) {
    let settings = Arc::new(kit::new_store(app, "translate.json", TranslateConfig::default));
    let mut cmd = kit::make_cmd(
        "tr",
        &[],
        group::TOOLS,
        "用谷歌翻译文本",
        "[语言] 文本|set 语言",
        translate_help,
        move |inv| {
            let s = Arc::clone(&settings);
            Box::pin(async move {
                run_translate(&inv, &s)
                    .await
                    .map_err(|e| kit::wrap_fail("翻译失败", e))
            })
        },
    );
    cmd.timeout = Some(Duration::from_secs(120));
    cmd.free_text = true;
    app.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_translation_and_languages() {
        let detected = parse_translation(r#"[["你好世界","en"]]"#.as_bytes()).unwrap();
        assert_eq!(detected, TranslateResult { text: "你好世界".into(), source: "en".into() });
        let named = parse_translation(r#"["早上好"]"#.as_bytes()).unwrap();
        assert_eq!(named, TranslateResult { text: "早上好".into(), source: "".into() });
        for bad in ["", "{}", "[]", "[[]]", r#"[""]"#, r#"[[""]]"#, "not json"] {
            assert!(parse_translation(bad.as_bytes()).is_err(), "bad={bad}");
        }

        for (inp, want) in [
            ("en", "en"), ("EN", "en"), ("english", "en"), ("英文", "en"),
            ("zh", "zh-CN"), ("cn", "zh-CN"), ("中文", "zh-CN"), ("tw", "zh-TW"),
            ("jp", "ja"), ("ja", "ja"), ("ko", "ko"), ("ru", "ru"),
        ] {
            assert_eq!(named_language(inp).as_deref(), Some(want), "inp={inp}");
        }
        for txt in ["", "hello", "this", "翻译一下", "zzz", "xyzzy", "is"] {
            assert!(named_language(txt).is_none(), "txt={txt}");
        }
        assert!(has_han("今天天气不错") && has_han("mixed 中文 text"));
        assert!(!has_han("hello world") && !has_han("こんにちは") && !has_han(""));
        assert_eq!(language_name("en"), "英语");
        assert_eq!(language_name("qqq"), "qqq");
    }
}
