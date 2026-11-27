use crate::kit;
use mibot_app::App;
use mibot_command::{
    CmdError, CmdResult, Command, Invocation, PAGE_LIMIT, code, escape, group, truncate,
};
use mibot_httpx::{HttpxError, Method, Request, do_request, post_json};
use scraper::{Html, Selector};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 15;
const MAX_QUERY: usize = 200;
const MAX_URL: usize = 1500;
const TITLE_RUNES: usize = 200;
const SNIPPET_RUNES: usize = 400;
const DDG_URL: &str = "https://html.duckduckgo.com/html/";
const FIRECRAWL_URL: &str = "https://api.firecrawl.dev/v2/search";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Ddg,
    Firecrawl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    source: Source,
}

fn parse_args(args: &[String]) -> (String, usize) {
    let mut limit = DEFAULT_LIMIT;
    let mut words = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let cur = args[i].as_str();
        if matches!(cur, "-n" | "--num" | "-l" | "--limit")
            && i + 1 < args.len()
            && let Ok(n) = args[i + 1].parse::<i64>()
        {
            limit = kit::clamp(n, 1, MAX_LIMIT as i64) as usize;
            i += 2;
            continue;
        }
        words.push(cur);
        i += 1;
    }
    (words.join(" ").trim().to_owned(), limit)
}

fn checked_url(parsed: &url::Url) -> Option<String> {
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(str::is_empty) {
        return None;
    }
    let val = parsed.as_str();
    if escape(val).len() > MAX_URL {
        return None;
    }
    Some(val.to_owned())
}

fn result_url(raw: &str) -> Option<String> {
    let base = url::Url::parse("https://duckduckgo.com/").ok()?;
    let mut parsed = base.join(raw).ok()?;
    if let Some((_, wrapped)) = parsed.query_pairs().find(|(k, _)| k == "uddg") {
        let wrapped_owned = wrapped.into_owned();
        if !wrapped_owned.is_empty() {
            parsed = url::Url::parse(&wrapped_owned).ok()?;
        }
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if host == "duckduckgo.com" || host.ends_with(".duckduckgo.com") {
        return None;
    }
    checked_url(&parsed)
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, max: usize) -> String {
    truncate(s.trim(), max)
}

fn parse_ddg(body: &[u8], limit: usize) -> Vec<SearchResult> {
    static RESULT_SEL: OnceLock<Selector> = OnceLock::new();
    static LINK_SEL: OnceLock<Selector> = OnceLock::new();
    static SNIPPET_SEL: OnceLock<Selector> = OnceLock::new();
    let result_sel = RESULT_SEL.get_or_init(|| Selector::parse(".result").expect("valid selector"));
    let link_sel = LINK_SEL.get_or_init(|| Selector::parse("a.result__a").expect("valid selector"));
    let snippet_sel = SNIPPET_SEL.get_or_init(|| Selector::parse(".result__snippet").expect("valid selector"));

    let html_str = String::from_utf8_lossy(body);
    let doc = Html::parse_document(&html_str);
    let mut items = Vec::new();
    let mut seen = HashSet::new();

    for block in doc.select(result_sel) {
        if items.len() >= limit {
            break;
        }
        // Skip nested .result blocks inside another .result
        if block
            .ancestors()
            .skip(1)
            .any(|anc| anc.value().as_element().is_some_and(|e| e.has_class("result", scraper::CaseSensitivity::CaseSensitive)))
        {
            continue;
        }
        if block
            .value()
            .has_class("result--ad", scraper::CaseSensitivity::CaseSensitive)
        {
            continue;
        }
        let Some(link) = block.select(link_sel).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Some(target) = result_url(href) else {
            continue;
        };
        let raw_title: String = link.text().collect::<Vec<_>>().join(" ");
        let title = clip(&collapse_ws(&raw_title), TITLE_RUNES);
        if title.is_empty() || !seen.insert(target.clone()) {
            continue;
        }
        let snippet = if let Some(node) = block.select(snippet_sel).next() {
            let raw_snip: String = node.text().collect::<Vec<_>>().join(" ");
            clip(&collapse_ws(&raw_snip), SNIPPET_RUNES)
        } else {
            String::new()
        };
        items.push(SearchResult {
            title,
            url: target,
            snippet,
            source: Source::Ddg,
        });
    }
    items
}

#[derive(Deserialize)]
struct FirecrawlEnvelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: serde_json::Value,
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
struct FirecrawlItem {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
}

fn parse_firecrawl(body: &[u8], limit: usize) -> Result<Vec<SearchResult>, CmdError> {
    let decoded: FirecrawlEnvelope =
        serde_json::from_slice(body).map_err(|e| format!("invalid firecrawl response: {e}"))?;
    if !decoded.success && !decoded.error.is_empty() {
        return Err(format!("firecrawl: {}", decoded.error).into());
    }
    let list: Vec<FirecrawlItem> = if let Some(web) = decoded.data.get("web") {
        serde_json::from_value(web.clone()).map_err(|e| format!("unexpected firecrawl data: {e}"))?
    } else {
        serde_json::from_value(decoded.data).map_err(|e| format!("unexpected firecrawl data: {e}"))?
    };
    let mut items = Vec::new();
    for entry in list {
        if items.len() >= limit {
            break;
        }
        let Ok(parsed) = url::Url::parse(entry.url.trim()) else {
            continue;
        };
        let Some(target) = checked_url(&parsed) else {
            continue;
        };
        let mut title = clip(&collapse_ws(&entry.title), TITLE_RUNES);
        if title.is_empty() {
            title = parsed.host_str().unwrap_or("").to_owned();
        }
        let snippet = clip(&collapse_ws(&entry.description), SNIPPET_RUNES);
        items.push(SearchResult {
            title,
            url: target,
            snippet,
            source: Source::Firecrawl,
        });
    }
    Ok(items)
}

fn merge(mut items: Vec<SearchResult>, more: Vec<SearchResult>, limit: usize) -> Vec<SearchResult> {
    let mut seen: HashSet<String> = items.iter().map(|i| i.url.clone()).collect();
    for item in more {
        if items.len() >= limit {
            break;
        }
        if seen.insert(item.url.clone()) {
            items.push(item);
        }
    }
    items
}

fn source_note(items: &[SearchResult]) -> String {
    let Some(first) = items.iter().position(|i| i.source == Source::Firecrawl) else {
        return String::new();
    };
    if first == 0 {
        return "DuckDuckGo 没有给出结果，以下来自 Firecrawl".into();
    }
    if first + 1 == items.len() {
        return format!("第 {} 条来自 Firecrawl", first + 1);
    }
    format!("第 {}–{} 条来自 Firecrawl", first + 1, items.len())
}

fn render(query: &str, items: &[SearchResult]) -> Vec<String> {
    let mut header = format!("🦆 <b>DuckDuckGo</b> · {} · {} 条", code(query), items.len());
    let note = source_note(items);
    if !note.is_empty() {
        header.push('\n');
        header.push_str(&note);
    }
    let encoded_q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let footer = format!(
        "<a href=\"https://duckduckgo.com/?q={}\">在 DuckDuckGo 打开</a>",
        escape(&encoded_q)
    );
    if items.is_empty() {
        return vec![format!("{header}\n\n没有找到结果，换个关键词试试\n\n{footer}")];
    }
    let mut blocks = Vec::with_capacity(items.len() + 1);
    for (idx, item) in items.iter().enumerate() {
        let mut block = format!(
            "<b>{}.</b> <a href=\"{}\">{}</a>",
            idx + 1,
            escape(&item.url),
            escape(&item.title)
        );
        if !item.snippet.is_empty() {
            block.push_str(&format!("\n<blockquote expandable>{}</blockquote>", escape(&item.snippet)));
        }
        blocks.push(block);
    }
    blocks.push(footer);

    let mut pages = Vec::new();
    let mut page = format!("{header}\n");
    for block in blocks {
        if kit::utf16_len_usize(&page) + 1 + kit::utf16_len_usize(&block) > PAGE_LIMIT {
            pages.push(page.trim_end_matches('\n').to_owned());
            page = "🦆 <b>DuckDuckGo</b> · 续\n".to_owned();
        }
        page.push('\n');
        page.push_str(&block);
    }
    pages.push(page);
    pages
}

async fn search_ddg(
    cancel: &CancellationToken,
    ddg_url: &str,
    query: &str,
    limit: usize,
) -> Result<(Vec<SearchResult>, bool), CmdError> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .finish()
        .into_bytes();
    let mut headers = HashMap::new();
    headers.insert("Content-Type".into(), "application/x-www-form-urlencoded".into());
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(kit::fail("操作已取消")),
        r = do_request(Request {
            method: Method::POST,
            url: ddg_url.to_owned(),
            headers,
            body: Some(body),
            timeout: Some(Duration::from_secs(15)),
            max_bytes: Some(2 << 20),
            ..Default::default()
        }) => r?,
    };
    let blocked = resp.body.windows(7).any(|w| w == b"anomaly");
    if !resp.ok() {
        return Err(Box::new(HttpxError::Status(resp.status)));
    }
    Ok((parse_ddg(&resp.body, limit), blocked))
}

async fn search_firecrawl(
    cancel: &CancellationToken,
    fc_url: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, CmdError> {
    #[derive(serde::Serialize)]
    struct FcReq<'a> {
        query: &'a str,
        limit: usize,
    }
    let req_body = FcReq { query, limit };
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(kit::fail("操作已取消")),
        r = post_json(
            Request {
                url: fc_url.to_owned(),
                timeout: Some(Duration::from_secs(20)),
                max_bytes: Some(2 << 20),
                ..Default::default()
            },
            &req_body,
        ) => r?,
    };
    if !resp.ok() {
        return Err(Box::new(HttpxError::Status(resp.status)));
    }
    parse_firecrawl(&resp.body, MAX_LIMIT)
}

async fn search(
    cancel: &CancellationToken,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    ddg_url: &str,
    fc_url: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, CmdError> {
    let items = match search_ddg(cancel, ddg_url, query, limit).await {
        Ok((items, blocked)) => {
            if blocked && items.is_empty() {
                kit::opt_log(logs, mibot_logtail::Level::Info, "ddg.primary_blocked", &[]);
            }
            items
        }
        Err(err) => {
            if cancel.is_cancelled() {
                return Err(err);
            }
            kit::opt_log(
                logs,
                mibot_logtail::Level::Info,
                "ddg.primary_failed",
                &[("error", &err.to_string())],
            );
            Vec::new()
        }
    };
    if items.len() >= limit {
        return Ok(items);
    }
    match search_firecrawl(cancel, fc_url, query, limit - items.len()).await {
        Ok(more) => Ok(merge(items, more, limit)),
        Err(err) => {
            if items.is_empty() {
                return Err(err);
            }
            kit::opt_log(
                logs,
                mibot_logtail::Level::Info,
                "ddg.supplement_failed",
                &[("error", &err.to_string())],
            );
            Ok(items)
        }
    }
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "🦆 <b>DuckDuckGo 搜索</b>\n\n搜索网页，列出标题、链接和摘要。DuckDuckGo 拦下请求或结果不够时，用 Firecrawl 的免 Key 搜索补上。\n\n\
         • <code>{p}ddg 关键词</code> 默认 {DEFAULT_LIMIT} 条\n\
         • <code>{p}ddg 关键词 -n 5</code> 指定条数，1–{MAX_LIMIT}\n\
         • <code>{p}duckduckgo 关键词</code> 同 {}\n\n\
         关键词最多 {MAX_QUERY} 字。",
        code(&format!("{prefix}ddg"))
    )
}

async fn handle(inv: &Invocation, app: &App) -> CmdResult {
    let (query, limit) = parse_args(&inv.args);
    if query.is_empty() {
        return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
    }
    if query.chars().count() > MAX_QUERY {
        return Err(kit::fail(format!("关键词太长，最多 {MAX_QUERY} 字")));
    }
    inv.edit_text(&kit::working(&format!("正在搜索 {query}"))).await?;
    let items = search(&inv.cancel, Some(&app.logs), DDG_URL, FIRECRAWL_URL, &query, limit)
        .await
        .map_err(|e| kit::fail_with("搜索失败", mibot_command::UserError::new(e.to_string())))?;
    inv.respond_pages(&render(&query, &items)).await?;
    Ok(())
}

pub fn register(app: &Arc<App>) {
    let app_ref = Arc::clone(app);
    app.registry.register(Command {
        name: "ddg".into(),
        aliases: vec!["duckduckgo".into()],
        group: group::TOOLS.into(),
        description: "搜索网页".into(),
        usage: "关键词 [-n 条数]".into(),
        help_fn: Some(Arc::new(help)),
        timeout: Some(Duration::from_secs(60)),
        free_text: true,
        handle: Arc::new(move |inv| {
            let a = Arc::clone(&app_ref);
            Box::pin(async move { handle(&inv, &a).await })
        }),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use mibot_bot::{parse_html, utf16_len};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const RESULTS_PAGE: &str = r#"<!DOCTYPE html><html><body><div id="links" class="results">
<div class="result results_links results_links_deep result--ad">
  <div class="links_main links_deep result__body"><h2 class="result__title">
  <a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=ads.example&amp;u3=x">Buy Go Books</a></h2>
  <a class="result__snippet" href="https://duckduckgo.com/y.js?x">Sponsored</a></div>
</div>
<div class="result results_links results_links_deep web-result ">
  <div class="links_main links_deep result__body">
    <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fgo.dev%2Fdoc%2F%3Fa%3D1%26b%3D2&amp;rut=abc">The <b>Go</b> Programming Language &amp; Docs</a></h2>
    <div class="result__extras"><a class="result__url" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fgo.dev%2F">go.dev</a></div>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">Go is an   open source
      programming language that makes it <b>simple</b> to build &lt;secure&gt; software.</a>
  </div>
</div>
<div class="result results_links web-result"><div class="result__body">
  <h2 class="result__title"><a href='https://pkg.go.dev/' class='result__a'>Go Packages</a></h2>
</div></div>
<div class="result results_links web-result"><div class="result__body">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fgo.dev%2Fdoc%2F%3Fa%3D1%26b%3D2">Duplicate</a></h2>
</div></div>
<div class="result results_links web-result"><div class="result__body">
  <h2 class="result__title"><a class="result__a" href="javascript:alert(1)">Evil</a></h2>
</div></div>
</div></body></html>"#;

    const ANOMALY_PAGE: &str = r#"<html><body><form id="challenge-form" action="//duckduckgo.com/anomaly.js?sv=html&amp;cc=botnet">
<div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div></form></body></html>"#;

    #[test]
    fn test_parse_args_html_firecrawl_and_render() {
        for (input, want_q, want_l) in [
            (vec!["golang", "generics"], "golang generics", DEFAULT_LIMIT),
            (vec!["-n", "5", "golang"], "golang", 5),
            (vec!["golang", "-n", "99"], "golang", MAX_LIMIT),
            (vec!["golang", "--limit", "0"], "golang", 1),
            (vec!["--num", "3", "a", "-l", "4", "b"], "a b", 4),
            (vec!["-n", "abc"], "-n abc", DEFAULT_LIMIT),
            (vec!["golang", "-n"], "golang -n", DEFAULT_LIMIT),
            (vec![], "", DEFAULT_LIMIT),
        ] {
            let args: Vec<String> = input.into_iter().map(str::to_owned).collect();
            let (q, l) = parse_args(&args);
            assert_eq!((q.as_str(), l), (want_q, want_l));
        }

        let items = parse_ddg(RESULTS_PAGE.as_bytes(), 10);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "The Go Programming Language & Docs");
        assert_eq!(items[0].url, "https://go.dev/doc/?a=1&b=2");
        assert_eq!(items[0].snippet, "Go is an open source programming language that makes it simple to build <secure> software.");
        assert_eq!(items[1].url, "https://pkg.go.dev/");
        assert_eq!(parse_ddg(RESULTS_PAGE.as_bytes(), 1).len(), 1);
        assert!(parse_ddg(ANOMALY_PAGE.as_bytes(), 10).is_empty());

        let v2 = br#"{"success":true,"data":{"web":[
            {"url":"https://go.dev/","title":"The Go   Programming Language","description":"Build simple, secure, scalable systems with Go."},
            {"url":"ftp://files.example.com/x","title":"FTP"},
            {"url":"https://example.org/path?q=1","title":"","description":""},
            {"url":"https://third.example/","title":"Third"}
        ]}}"#;
        let fc = parse_firecrawl(v2, 2).unwrap();
        assert_eq!(fc.len(), 2);
        assert_eq!(fc[0].title, "The Go Programming Language");
        assert_eq!(fc[1].title, "example.org");
        assert!(parse_firecrawl(br#"{"success":false,"error":"Rate limit"}"#, 5).is_err());

        let mut many = Vec::new();
        for i in 0..MAX_LIMIT {
            many.push(SearchResult {
                title: "标题<&>".repeat(30),
                url: format!("https://example.com/?q={}&n={}", "x".repeat(300), char::from(b'a' + i as u8)),
                snippet: "摘要 ".repeat(200),
                source: Source::Ddg,
            });
        }
        let pages = render("a&b", &many);
        assert!(pages.len() >= 2);
        for page in &pages {
            assert!(utf16_len(page) <= PAGE_LIMIT as i32);
            parse_html(page, None).unwrap();
        }
        assert!(pages[0].starts_with("🦆 <b>DuckDuckGo</b> · <code>a&amp;b</code> · 15 条"));
        assert!(pages[1].starts_with("🦆 <b>DuckDuckGo</b> · 续"));
        assert!(pages.last().unwrap().contains("https://duckduckgo.com/?q=a%26b"));
    }

    #[tokio::test]
    async fn test_search_fallback_and_supplements() {
        let asked = Arc::new(std::sync::Mutex::new(0usize));
        let asked_ref = Arc::clone(&asked);
        let ddg_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ddg_addr = ddg_l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = ddg_l.accept().await else { break };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{RESULTS_PAGE}",
                    RESULTS_PAGE.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        let fc_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fc_addr = fc_l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = fc_l.accept().await else { break };
                let asked = Arc::clone(&asked_ref);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]);
                    if let Some((_, body)) = raw.split_once("\r\n\r\n")
                        && let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
                    {
                        *asked.lock().unwrap() = v["limit"].as_u64().unwrap_or(0) as usize;
                    }
                    let b = r#"{"success":true,"data":{"web":[{"url":"https://pkg.go.dev/","title":"dup"},{"url":"https://fca.example/","title":"FC A"},{"url":"https://fcb.example/","title":"FC B"}]}}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                        b.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });

        let items = search(
            &CancellationToken::new(),
            None,
            &format!("http://{ddg_addr}"),
            &format!("http://{fc_addr}"),
            "golang",
            4,
        )
        .await
        .unwrap();
        assert_eq!(*asked.lock().unwrap(), 2);
        assert_eq!(items.len(), 4);
        assert_eq!(source_note(&items), "第 3–4 条来自 Firecrawl");
    }
}
