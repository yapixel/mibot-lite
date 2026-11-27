use crate::kit;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use mibot_app::App;
use mibot_command::{Command, Invocation, PAGE_LIMIT, code, escape, group, html_pages};
use std::sync::Arc;

pub const MAX_INPUT: usize = 16384;
pub const PREVIEW_RUNES: usize = 200;

#[derive(Clone, Copy)]
pub struct Operation {
    pub name: &'static str,
    pub title: &'static str,
    pub title_zh: &'static str,
    pub transform: fn(&str) -> Result<String, &'static str>,
}

impl Operation {
    pub fn title_text(&self) -> &'static str {
        kit::tr(self.title, self.title_zh)
    }
}

pub static OPERATIONS: &[Operation] = &[
    Operation {
        name: "b64encode",
        title: "Base64 Encode",
        title_zh: "Base64 编码",
        transform: |s| Ok(STANDARD.encode(s.as_bytes())),
    },
    Operation {
        name: "b64decode",
        title: "Base64 Decode",
        title_zh: "Base64 解码",
        transform: decode_base64,
    },
    Operation {
        name: "urlencode",
        title: "URL Encode",
        title_zh: "URL 编码",
        transform: |s| Ok(encode_uri_component(s)),
    },
    Operation {
        name: "urldecode",
        title: "URL Decode",
        title_zh: "URL 解码",
        transform: decode_uri_component,
    },
];

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let c = |t: &str| format!("<code>{p}{t}</code>");
    if kit::is_zh() {
        format!(
            "🔣 <b>编码解码</b>\n\n\
             • {} Base64 编码\n\
             • {} Base64 解码\n\
             • {} URL 编码\n\
             • {} URL 解码\n\n\
             不写文本时处理所回复的那条消息。\n\n<b>示例</b>\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {}\n\n\
             Base64 解码也认 URL 安全的写法（- 和 _）；URL 编码的规则同 JavaScript 的 encodeURIComponent，空格编成 %20。",
            c("b64encode 文本"),
            c("b64decode 文本"),
            c("urlencode 文本"),
            c("urldecode 文本"),
            c("b64encode Hello World"),
            c("b64decode SGVsbG8gV29ybGQ="),
            c("urlencode 你好世界"),
            c("urldecode %E4%BD%A0%E5%A5%BD"),
        )
    } else {
        format!(
            "🔣 <b>Encode &amp; Decode</b>\n\n\
             • {} Base64 encode\n\
             • {} Base64 decode\n\
             • {} URL encode\n\
             • {} URL decode\n\n\
             Processes the replied message when no text is given.\n\n<b>Examples</b>\n\
             • {}\n\
             • {}\n\
             • {}\n\
             • {}\n\n\
             Base64 decode also accepts URL-safe (- and _); URL encoding follows JavaScript encodeURIComponent (spaces as %20).",
            c("b64encode text"),
            c("b64decode text"),
            c("urlencode text"),
            c("urldecode text"),
            c("b64encode Hello World"),
            c("b64decode SGVsbG8gV29ybGQ="),
            c("urlencode hello world"),
            c("urldecode %E4%BD%A0%E5%A5%BD"),
        )
    }
}

pub fn register(a: &Arc<App>) {
    a.registry.register(
        kit::make_cmd(
            "encode",
            &[],
            group::TOOLS,
            "Encode or decode",
            "",
            help,
            move |inv: Invocation| {
                Box::pin(async move { inv.edit(&help(inv.prefix())).await.map_err(Into::into) })
            },
        )
        .zh("查看编码解码命令", ""),
    );
    for &op in OPERATIONS {
        let mut cmd: Command = kit::make_cmd(
            op.name,
            &[],
            group::TOOLS,
            op.title,
            "[text]",
            help,
            move |inv: Invocation| Box::pin(async move { run(inv, op).await }),
        )
        .zh(format!("{}文本", op.title_zh), "[文本]");
        cmd.free_text = true;
        a.registry.register(cmd);
    }
}

async fn run(inv: Invocation, op: Operation) -> mibot_command::CmdResult {
    let mut input = inv.raw.trim().to_owned();
    if input.is_empty()
        && let Some(reply) = kit::reply_msg(&inv).await?
    {
        input = reply.content().trim().to_owned();
    }
    if input.is_empty() {
        if inv.message.reply_to_id != 0 {
            return Err(kit::fail_err(kit::tr(
                "Replied message has no text",
                "被回复的消息没有文字",
            )));
        }
        return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
    }
    if kit::utf16_len_usize(&input) > MAX_INPUT {
        return Err(kit::fail_err(if kit::is_zh() {
            format!("文本太长，最多 {MAX_INPUT} 字")
        } else {
            format!("Text too long (max {MAX_INPUT} chars)")
        }));
    }
    let output = (op.transform)(&input).map_err(|msg| {
        kit::fail_err(if kit::is_zh() {
            format!("{}失败：{msg}", op.title_zh)
        } else {
            format!("{} failed: {msg}", op.title)
        })
    })?;
    let pages = html_pages(&render(op, &input, &output), PAGE_LIMIT);
    inv.respond_pages(&pages).await.map_err(Into::into)
}

pub fn render(op: Operation, input: &str, output: &str) -> String {
    let preview = if input.chars().count() > PREVIEW_RUNES {
        format!("{}…", kit::truncate_runes(input, PREVIEW_RUNES))
    } else {
        input.to_owned()
    };
    let result = if output.is_empty() {
        kit::tr("(empty)", "（空）").to_owned()
    } else {
        code(output)
    };
    let (in_label, out_label) = if kit::is_zh() {
        ("原文", "结果")
    } else {
        ("Input", "Output")
    };
    format!(
        "🔣 <b>{}</b>\n\n<b>{in_label}</b>\n{}\n\n<b>{out_label}</b>\n{}",
        escape(op.title_text()),
        code(&preview),
        result
    )
}

pub fn decode_base64(input: &str) -> Result<String, &'static str> {
    let compact: String = input.split_whitespace().collect();
    if compact.is_empty() {
        return Err(kit::tr("Not a valid Base64 string", "不是有效的 Base64 字符串"));
    }
    let url_safe = compact.contains(['-', '_']);
    let padded = compact.ends_with('=');
    let decoded = match (url_safe, padded) {
        (true, true) => URL_SAFE.decode(&compact),
        (true, false) => URL_SAFE_NO_PAD.decode(&compact),
        (false, true) => STANDARD.decode(&compact),
        (false, false) => STANDARD_NO_PAD.decode(&compact),
    }
    .map_err(|_| kit::tr("Not a valid Base64 string", "不是有效的 Base64 字符串"))?;
    String::from_utf8(decoded).map_err(|_| {
        kit::tr(
            "Decoded bytes are not valid UTF-8",
            "解出来的内容不是 UTF-8 文本",
        )
    })
}

fn uri_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
}

pub fn encode_uri_component(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if uri_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

pub fn decode_uri_component(input: &str) -> Result<String, &'static str> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 2 >= bytes.len() {
            return Err(kit::tr("Not valid URL encoding", "不是有效的 URL 编码"));
        }
        let hi = from_hex(bytes[i + 1]).ok_or_else(|| kit::tr("Not valid URL encoding", "不是有效的 URL 编码"))?;
        let lo = from_hex(bytes[i + 2]).ok_or_else(|| kit::tr("Not valid URL encoding", "不是有效的 URL 编码"))?;
        out.push((hi << 4) | lo);
        i += 3;
    }
    String::from_utf8(out).map_err(|_| {
        kit::tr(
            "Decoded bytes are not valid UTF-8",
            "解出来的内容不是 UTF-8 文本",
        )
    })
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mibot_bot::{PeerCache, SentRecorder, Client, RpcTransport, BoxFuture, parse_html};
    use mibot_logtail::Ring;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::UNIX_EPOCH;

    fn find(name: &str) -> Operation {
        OPERATIONS.iter().copied().find(|op| op.name == name).unwrap()
    }

    #[test]
    fn test_round_trip() {
        for text in [
            "Hello World",
            "你好世界",
            "a  b\nc\td",
            "🎉 emoji 🧬",
            "~!@#$%^&*()_+-=[]{}|;':\",./<>?",
        ] {
            for (enc, dec) in [("b64encode", "b64decode"), ("urlencode", "urldecode")] {
                let encoded = (find(enc).transform)(text).unwrap();
                let decoded = (find(dec).transform)(&encoded).unwrap();
                assert_eq!(decoded, text, "{enc}/{dec}({text:?})");
            }
        }
    }

    #[test]
    fn test_encode_uri_component_matches_javascript() {
        for (input, want) in [
            ("你好世界", "%E4%BD%A0%E5%A5%BD%E4%B8%96%E7%95%8C"),
            ("a b&c=d/é", "a%20b%26c%3Dd%2F%C3%A9"),
            ("-_.!~*'()", "-_.!~*'()"),
            ("?#[]@$+,;:\n", "%3F%23%5B%5D%40%24%2B%2C%3B%3A%0A"),
            ("🎉", "%F0%9F%8E%89"),
            ("AZaz09", "AZaz09"),
            ("100% sure", "100%25%20sure"),
            ("https://x.y/?q=1", "https%3A%2F%2Fx.y%2F%3Fq%3D1"),
        ] {
            assert_eq!(encode_uri_component(input), want, "{input:?}");
        }
    }

    #[test]
    fn test_decode_uri_component() {
        for (input, want) in [
            ("%E4%BD%A0%e5%a5%bd", "你好"),
            ("a+b", "a+b"),
            ("plain", "plain"),
            ("100%25", "100%"),
        ] {
            assert_eq!(decode_uri_component(input).unwrap(), want, "{input:?}");
        }
        for bad in ["%", "%4", "%zz", "abc%E4", "%FF", "%C3%28"] {
            assert!(decode_uri_component(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_decode_base64() {
        for (input, want) in [
            ("SGVsbG8gV29ybGQ=", "Hello World"),
            ("SGVsbG8gV29ybGQ", "Hello World"),
            ("SGVs\nbG8g V29y\nbGQ=", "Hello World"),
            ("5L2g5aW9", "你好"),
            ("Pz8/", "???"),
            ("Pz8_", "???"),
            ("fn5-", "~~~"),
        ] {
            assert_eq!(decode_base64(input).unwrap(), want, "{input:?}");
        }
        for bad in [
            "", "   ", "A", "abc$", "SGVsbG8gV29ybGQ==",
            "SGVsbG8gV29ybGR=", "Pz8/Pz8_", "/w==",
        ] {
            assert!(decode_base64(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_render_preview_and_escaping() {
        let long = "字".repeat(PREVIEW_RUNES + 5);
        let text = render(find("b64encode"), &long, "<&>");
        assert!(
            text.contains(&format!("{}…</code>", "字".repeat(PREVIEW_RUNES)))
                && !text.contains(&"字".repeat(PREVIEW_RUNES + 1))
        );
        assert!(text.contains("<code>&lt;&amp;&gt;</code>"));
        assert!(text.starts_with("🔣 <b>Base64 Encode</b>"));
        let output = (find("b64encode").transform)(&"长文本".repeat(4000)).unwrap();
        let pages = html_pages(&render(find("b64encode"), "x", &output), PAGE_LIMIT);
        assert!(pages.len() >= 2);
        for page in &pages {
            parse_html(page, None).unwrap();
        }
    }

    struct Stub;
    impl RpcTransport for Stub {
        fn invoke_raw<'a>(&'a self, _: Option<i32>, _: u32, _: Vec<u8>) -> BoxFuture<'a, Result<Vec<u8>, grammers_mtsender::InvocationError>> {
            Box::pin(async { Err(grammers_mtsender::InvocationError::Dropped("stub")) })
        }
    }

    #[test]
    fn test_register() {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mibot-enc-{}-{}",
            std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let logs = Arc::new(Ring::new());
        let peers = Arc::new(PeerCache::new());
        let sent = Arc::new(SentRecorder::new(Arc::new(|_, _| {})));
        let client = Arc::new(Client::from_transport(Arc::new(Stub), peers.clone(), sent));
        let app = App::for_testing(dir.clone(), "test", "b", client, peers, logs);
        register(&app);
        for name in ["encode", "b64encode", "b64decode", "urlencode", "urldecode"] {
            let cmd = app.registry.command(name).unwrap();
            assert_eq!(cmd.group, group::TOOLS);
            let h = app.registry.help_text_for(&cmd, ".");
            assert!(h.starts_with("🔣 <b>"), "{name}: {h}");
            assert_eq!(cmd.free_text, name != "encode");
            let len = cmd.description.chars().count();
            assert!((4..=24).contains(&len), "{name}: {:?}", cmd.description);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
