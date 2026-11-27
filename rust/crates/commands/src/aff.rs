use crate::kit;
use chrono::Utc;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::parse_html;
use mibot_command::{CmdResult, Invocation, code, escape, group, truncate};
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

const MAX_ENTRIES: usize = 32;
const MAX_LENGTH: i32 = 4000;
const PAGE_SIZE: usize = 10;
const PREVIEW_RUNES: usize = 30;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Entry {
    text: String,
    #[serde(rename = "webPage", default, skip_serializing_if = "Option::is_none")]
    web_page: Option<bool>,
    #[serde(rename = "web_page", default, skip_serializing_if = "Option::is_none")]
    legacy_web_page: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    format: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    created_at: i64,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

impl Entry {
    fn is_html(&self) -> bool {
        self.format == "html" || self.legacy_web_page.is_some()
    }

    fn plain(&self) -> String {
        if self.is_html()
            && let Ok((txt, _)) = parse_html(&self.text, None)
        {
            return txt;
        }
        self.text.clone()
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Document {
    affs: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    aff: Option<Entry>,
}

fn defaults() -> Document {
    Document {
        affs: Vec::new(),
        aff: None,
    }
}

fn link_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"https?://\S+").expect("valid link regex"))
}

fn index_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^-?\d+$").expect("valid index regex"))
}

#[derive(Clone)]
struct Span {
    start: i32,
    end: i32,
    open: String,
    close: String,
}

fn entity_tags(entity: &tl::enums::MessageEntity) -> Option<(String, String)> {
    use tl::enums::MessageEntity::*;
    match entity {
        Bold(_) => Some(("<b>".into(), "</b>".into())),
        Italic(_) => Some(("<i>".into(), "</i>".into())),
        Underline(_) => Some(("<u>".into(), "</u>".into())),
        Strike(_) => Some(("<s>".into(), "</s>".into())),
        Spoiler(_) => Some(("<tg-spoiler>".into(), "</tg-spoiler>".into())),
        Code(_) => Some(("<code>".into(), "</code>".into())),
        Pre(v) => {
            if !v.language.is_empty() {
                Some((
                    format!(r#"<pre><code class="language-{}">"#, escape(&v.language)),
                    "</code></pre>".into(),
                ))
            } else {
                Some(("<pre>".into(), "</pre>".into()))
            }
        }
        TextUrl(v) => Some((format!(r#"<a href="{}">"#, escape(&v.url)), "</a>".into())),
        MentionName(v) => Some((format!(r#"<a href="tg://user?id={}">"#, v.user_id), "</a>".into())),
        Blockquote(v) => {
            if v.collapsed {
                Some(("<blockquote expandable>".into(), "</blockquote>".into()))
            } else {
                Some(("<blockquote>".into(), "</blockquote>".into()))
            }
        }
        CustomEmoji(v) => Some((
            format!(r#"<tg-emoji emoji-id="{}">"#, v.document_id),
            "</tg-emoji>".into(),
        )),
        _ => None,
    }
}

fn entity_offset_len(entity: &tl::enums::MessageEntity) -> (i32, i32) {
    use tl::enums::MessageEntity::*;
    match entity {
        Bold(e) => (e.offset, e.length),
        Italic(e) => (e.offset, e.length),
        Underline(e) => (e.offset, e.length),
        Strike(e) => (e.offset, e.length),
        Spoiler(e) => (e.offset, e.length),
        Code(e) => (e.offset, e.length),
        Pre(e) => (e.offset, e.length),
        TextUrl(e) => (e.offset, e.length),
        MentionName(e) => (e.offset, e.length),
        Blockquote(e) => (e.offset, e.length),
        CustomEmoji(e) => (e.offset, e.length),
        _ => (0, 0),
    }
}

fn entities_html(text: &str, entities: &[tl::enums::MessageEntity]) -> String {
    let total = text.encode_utf16().count() as i32;
    let mut spans = Vec::new();
    for ent in entities {
        let Some((open, close)) = entity_tags(ent) else {
            continue;
        };
        let (off, len) = entity_offset_len(ent);
        let start = off.max(0);
        let end = (off + len).min(total);
        if start < end {
            spans.push(Span { start, end, open, close });
        }
    }
    spans.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));

    let mut out = String::with_capacity(text.len());
    let mut stack: Vec<Span> = Vec::new();
    let mut next = 0usize;
    let mut position = 0i32;

    for ch in text.chars() {
        if let Some(lowest) = stack.iter().position(|s| s.end <= position) {
            let popped: Vec<Span> = stack.drain(lowest..).collect();
            for item in popped.iter().rev() {
                out.push_str(&item.close);
            }
            for item in popped {
                if item.end > position {
                    out.push_str(&item.open);
                    stack.push(item);
                }
            }
        }
        while next < spans.len() && spans[next].start <= position {
            out.push_str(&spans[next].open);
            stack.push(spans[next].clone());
            next += 1;
        }
        let mut buf = [0u8; 4];
        out.push_str(&escape(ch.encode_utf8(&mut buf)));
        position += ch.len_utf16() as i32;
    }
    for item in stack.iter().rev() {
        out.push_str(&item.close);
    }
    out
}

fn load_doc(store: &Store<Document>) -> CmdResult<Document> {
    let doc = store.read()?;
    if doc.aff.is_none() {
        return Ok(doc);
    }
    store.update(|d| {
        if let Some(legacy) = d.aff.take() {
            d.affs.push(legacy);
        }
        Ok::<(), mibot_store::StoreError>(())
    })?;
    Ok(store.read()?)
}

fn new_entry(msg: Option<&tl::types::Message>, text: &str, now_ms: i64) -> Entry {
    let entities = msg
        .and_then(|m| m.entities.as_deref())
        .unwrap_or(&[]);
    let content = entities_html(text, entities);
    let has_link = link_pattern().is_match(&content);
    Entry {
        text: content,
        web_page: Some(has_link),
        legacy_web_page: None,
        format: "html".into(),
        created_at: now_ms,
    }
}

fn pages(count: usize) -> usize {
    count.div_ceil(PAGE_SIZE).max(1)
}

fn parse_page(raw: &str, count: usize) -> Option<usize> {
    if !kit::is_digits(raw) || raw.starts_with('0') {
        return None;
    }
    let page = raw.parse::<usize>().ok()?;
    (page <= pages(count)).then_some(page)
}

fn preview(item: &Entry) -> String {
    let plain = item.plain();
    let joined: Vec<&str> = plain.split_whitespace().collect();
    truncate(&joined.join(" "), PREVIEW_RUNES)
}

fn render_list(prefix: &str, items: &[Entry], page: usize) -> String {
    let total = pages(items.len());
    let start = (page - 1) * PAGE_SIZE;
    let end = (page * PAGE_SIZE).min(items.len());
    let rows: Vec<String> = (start..end)
        .map(|i| format!("{}. {}", i + 1, escape(&preview(&items[i]))))
        .collect();
    let idx_cmd = code(&format!("{prefix}aff {}", kit::tr("index", "序号")));
    let mut text = if kit::is_zh() {
        format!(
            "✈️ <b>Aff 列表</b> · {page}/{total}\n\n{}\n\n{idx_cmd} 发送指定的一条",
            rows.join("\n"),
        )
    } else {
        format!(
            "✈️ <b>Aff List</b> · {page}/{total}\n\n{}\n\n{idx_cmd} to send an entry",
            rows.join("\n"),
        )
    };
    if page < total {
        let next_cmd = code(&format!("{prefix}aff list {}", page + 1));
        if kit::is_zh() {
            text.push_str(&format!("，{next_cmd} 看下一页"));
        } else {
            text.push_str(&format!(", {next_cmd} for next page"));
        }
    }
    text
}

async fn send_entry(inv: &Invocation, item: &Entry) -> CmdResult {
    if item.is_html() && parse_html(&item.text, None).is_ok() {
        return inv.edit(&item.text).await.map_err(Into::into);
    }
    inv.edit_text(&item.text).await.map_err(Into::into)
}

async fn list_cmd(inv: &Invocation, items: &[Entry], raw: &str) -> CmdResult {
    if items.is_empty() {
        let save_cmd = code(&format!("{}aff save", inv.prefix()));
        return inv
            .edit(&if kit::is_zh() {
                format!("✈️ <b>Aff 列表</b>\n\n还没有保存任何一条，回复一条消息发 {save_cmd} 保存")
            } else {
                format!("✈️ <b>Aff List</b>\n\nNo entries saved yet. Reply to a message and send {save_cmd} to save one.")
            })
            .await
            .map_err(Into::into);
    }
    let Some(page) = parse_page(raw, items.len()) else {
        let total = pages(items.len());
        return Err(kit::fail(if kit::is_zh() {
            format!("页码无效，共 {total} 页")
        } else {
            format!("Invalid page; {total} page(s) total")
        }));
    };
    inv.edit(&render_list(inv.prefix(), items, page))
        .await
        .map_err(Into::into)
}

fn append_entry(data: &Store<Document>, item: Entry) -> CmdResult<usize> {
    let mut index = 0usize;
    data.update(|doc| {
        if doc.affs.len() >= MAX_ENTRIES {
            let msg = if kit::is_zh() {
                format!("__USER__已经存了 {MAX_ENTRIES} 条，先删掉不需要的再保存")
            } else {
                format!("__USER__Already saved {MAX_ENTRIES} entries; remove unused ones first")
            };
            return Err(mibot_store::StoreError::Io(std::io::Error::other(msg)));
        }
        doc.affs.push(item.clone());
        index = doc.affs.len();
        Ok::<(), mibot_store::StoreError>(())
    })
    .map_err(|e| {
        let s = e.to_string();
        if let Some(msg) = s.strip_prefix("__USER__") {
            kit::fail(msg)
        } else {
            e.into()
        }
    })?;
    Ok(index)
}

fn remove_entry(data: &Store<Document>, index: usize) -> CmdResult {
    data.update(|doc| {
        if index < 1 || index > doc.affs.len() {
            let msg = if kit::is_zh() {
                format!("__USER__没有第 {index} 条")
            } else {
                format!("__USER__No entry #{index}")
            };
            return Err(mibot_store::StoreError::Io(std::io::Error::other(msg)));
        }
        doc.affs.remove(index - 1);
        Ok::<(), mibot_store::StoreError>(())
    })
    .map_err(|e| {
        let s = e.to_string();
        if let Some(msg) = s.strip_prefix("__USER__") {
            kit::fail(msg)
        } else {
            e.into()
        }
    })?;
    Ok(())
}

async fn save_cmd(inv: &Invocation, data: &Store<Document>) -> CmdResult {
    let Some(reply) = kit::reply_msg(inv).await? else {
        return Err(kit::fail(if kit::is_zh() {
            format!("请回复要保存的消息再发 {}aff save", inv.prefix())
        } else {
            format!("Reply to a message and send {}aff save", inv.prefix())
        }));
    };
    let text = reply.content();
    if text.trim().is_empty() {
        return Err(kit::fail(kit::tr(
            "Replied message has no text",
            "被回复的消息没有文字",
        )));
    }
    if mibot_bot::utf16_len(&text) > MAX_LENGTH {
        return Err(kit::fail(if kit::is_zh() {
            format!("文字超过 {MAX_LENGTH} 字，没有保存，缩短后再试")
        } else {
            format!("Text exceeds {MAX_LENGTH} chars; shorten it and try again")
        }));
    }
    let idx = append_entry(data, new_entry(Some(&reply.raw), &text, Utc::now().timestamp_millis()))?;
    inv.edit_text(&if kit::is_zh() {
        format!("✅ 已保存为第 {idx} 条")
    } else {
        format!("✅ Saved as entry #{idx}")
    })
    .await
    .map_err(Into::into)
}

async fn remove_cmd(inv: &Invocation, data: &Store<Document>) -> CmdResult {
    let raw = inv.arg(1);
    if raw.is_empty() {
        return Err(kit::usage(
            inv.prefix(),
            kit::tr("aff remove <index>", "aff remove 序号"),
        ));
    }
    if !kit::is_digits(raw) {
        return Err(kit::fail(kit::tr(
            "Index must be a positive integer",
            "序号要写正整数",
        )));
    }
    let Ok(idx) = raw.parse::<usize>().ok().filter(|&n| n >= 1).ok_or(()) else {
        return Err(kit::fail(kit::tr(
            "Index must be a positive integer",
            "序号要写正整数",
        )));
    };
    remove_entry(data, idx)?;
    inv.edit_text(&if kit::is_zh() {
        format!("✅ 已删除第 {idx} 条")
    } else {
        format!("✅ Deleted entry #{idx}")
    })
    .await
    .map_err(Into::into)
}

async fn handle(inv: &Invocation, data: &Store<Document>) -> CmdResult {
    let doc = load_doc(data)?;
    let sub = inv.arg(0).to_ascii_lowercase();
    match sub.as_str() {
        "" => match doc.affs.len() {
            0 => Err(kit::fail(if kit::is_zh() {
                format!("还没有保存 Aff，回复一条消息发 {}aff save 保存", inv.prefix())
            } else {
                format!("No aff entries saved yet; reply to a message with {}aff save", inv.prefix())
            })),
            1 => send_entry(inv, &doc.affs[0]).await,
            _ => list_cmd(inv, &doc.affs, "1").await,
        },
        "list" | "ls" => list_cmd(inv, &doc.affs, kit::or_default(inv.arg(1), "1")).await,
        "save" => save_cmd(inv, data).await,
        "remove" | "rm" | "del" => remove_cmd(inv, data).await,
        _ if index_pattern().is_match(&sub) => {
            let Ok(idx) = sub
                .parse::<usize>()
                .ok()
                .filter(|&n| n >= 1 && n <= doc.affs.len())
                .ok_or(())
            else {
                return Err(kit::fail(if kit::is_zh() {
                    format!("没有第 {sub} 条，{}aff list 查看全部", inv.prefix())
                } else {
                    format!("No entry #{sub}; send {}aff list to view all", inv.prefix())
                }));
            };
            send_entry(inv, &doc.affs[idx - 1]).await
        }
        _ => Err(kit::fail(if kit::is_zh() {
            format!("没有这个子命令：{}，{}help aff 查看用法", inv.arg(0), inv.prefix())
        } else {
            format!("Unknown subcommand: {}; see {}help aff", inv.arg(0), inv.prefix())
        })),
    }
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let c = |txt: &str| format!("<code>{p}aff{txt}</code>");
    if kit::is_zh() {
        format!(
            "✈️ <b>机场 Aff 信息</b>\n\n在别人打算买机场的时候光速发出自己的 Aff 信息，可以存多条。\n\n\
             • {} 发送默认的一条；存了多条时显示列表\n\
             • {} 发送指定的一条\n\
             • {} 回复一条消息，把它存为新的一条，格式和链接都保留\n\
             • {} 查看列表，每页 {PAGE_SIZE} 条\n\
             • {} 删除指定的一条\n\n\
             最多存 {MAX_ENTRIES} 条，每条最多 {MAX_LENGTH} 字，序号从 1 开始。",
            c(""),
            c(" 序号"),
            c(" save"),
            c(" list [页码]"),
            c(" remove 序号"),
        )
    } else {
        format!(
            "✈️ <b>Aff Messages</b>\n\nSave and quickly send your affiliate/referral messages.\n\n\
             • {} Send default entry; or show list if multiple saved\n\
             • {} Send a specific entry\n\
             • {} Reply to a message to save it (keeps formatting and links)\n\
             • {} View list ({PAGE_SIZE} per page)\n\
             • {} Delete a specific entry\n\n\
             Up to {MAX_ENTRIES} entries, max {MAX_LENGTH} chars each, indexed from 1.",
            c(""),
            c(" index"),
            c(" save"),
            c(" list [page]"),
            c(" remove index"),
        )
    }
}

pub fn register(app: &Arc<App>) {
    let data = Arc::new(kit::new_store(app, "aff.json", defaults));
    let mut cmd = kit::make_cmd(
        "aff",
        &[],
        group::MEDIA,
        "Manage and send aff links",
        "[index|save|list [page]|remove index]",
        help,
        move |inv| {
            let d = Arc::clone(&data);
            Box::pin(async move { handle(&inv, &d).await })
        },
    )
    .zh("管理并发送 Aff 信息", "[序号|save|list [页码]|remove 序号]");
    cmd.timeout = Some(std::time::Duration::from_secs(30));
    app.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "mibot-aff-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_entities_html_and_roundtrip() {
        let cases: Vec<(&str, Vec<tl::enums::MessageEntity>, &str)> = vec![
            (r#"a<b> & "c""#, vec![], r#"a&lt;b&gt; &amp; &quot;c&quot;"#),
            (
                "注册 点这里 立减",
                vec![
                    tl::types::MessageEntityBold { offset: 0, length: 2 }.into(),
                    tl::types::MessageEntityTextUrl { offset: 3, length: 3, url: "https://x.example/?a=1&b=2".into() }.into(),
                ],
                r#"<b>注册</b> <a href="https://x.example/?a=1&amp;b=2">点这里</a> 立减"#,
            ),
            (
                "🎉 优惠 码",
                vec![tl::types::MessageEntityBold { offset: 3, length: 2 }.into()],
                "🎉 <b>优惠</b> 码",
            ),
            (
                "abcdef",
                vec![
                    tl::types::MessageEntityBold { offset: 0, length: 4 }.into(),
                    tl::types::MessageEntityItalic { offset: 2, length: 4 }.into(),
                ],
                "<b>ab<i>cd</i></b><i>ef</i>",
            ),
            (
                "⭐ hi",
                vec![
                    tl::types::MessageEntityCustomEmoji { offset: 0, length: 1, document_id: 5368324170671202286 }.into(),
                    tl::types::MessageEntityMentionName { offset: 2, length: 2, user_id: 42 }.into(),
                ],
                r#"<tg-emoji emoji-id="5368324170671202286">⭐</tg-emoji> <a href="tg://user?id=42">hi</a>"#,
            ),
        ];
        for (txt, ents, want) in cases {
            let got = entities_html(txt, &ents);
            assert_eq!(got, want);
            let (plain, _) = parse_html(&got, None).unwrap();
            assert_eq!(plain, txt);
        }

        let td = TempDir::new();
        let path = td.path().join("aff.json");
        std::fs::write(&path, r#"{"aff":{"text":"old","web_page":false},"affs":[{"text":"a","format":"html"}]}"#).unwrap();
        let store = Store::new(&path, defaults);
        let doc = load_doc(&store).unwrap();
        assert!(doc.aff.is_none() && doc.affs.len() == 2 && doc.affs[1].text == "old" && doc.affs[1].is_html());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("\"aff\""));

        for i in 1..=MAX_ENTRIES {
            let idx = append_entry(&store, Entry { text: format!("t{i}"), format: "html".into(), ..Default::default() }).unwrap_or(0);
            if idx == 0 {
                // Stop once capped at MAX_ENTRIES
                break;
            }
        }
        assert!(append_entry(&store, Entry { text: "extra".into(), ..Default::default() }).is_err());
        remove_entry(&store, 2).unwrap();
        assert!(remove_entry(&store, 0).is_err());
    }
}
