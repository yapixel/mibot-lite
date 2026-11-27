pub use mibot_bot::html::{bold, code, escape, utf16_len};

pub const PAGE_LIMIT: usize = 3800;
const MAX_OPEN_TAGS: usize = 16;

#[derive(Debug, Clone)]
struct OpenTag {
    name: String,
    open: String,
    close: String,
}

#[derive(Debug, Clone)]
enum HtmlToken {
    Text(String),
    Entity(String, usize),
    OpenTag(OpenTag),
    CloseTag(String),
}

fn escaped_char_utf16(ch: char) -> usize {
    match ch {
        '<' | '>' => 4,
        '&' => 5,
        _ => ch.len_utf16(),
    }
}

/// Splits raw text into multiple HTML-escaped pages, each guaranteed to fit within `limit` UTF-16 units.
pub fn escaped_pages(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let limit = if limit == 0 { PAGE_LIMIT } else { limit };
    let mut pages = Vec::new();
    let mut current = String::new();
    let mut current_u16 = 0usize;

    let lines: Vec<&str> = text.split('\n').collect();
    for (idx, line) in lines.iter().enumerate() {
        let mut line_u16 = 0usize;
        for ch in line.chars() {
            line_u16 += escaped_char_utf16(ch);
        }
        let need_nl = usize::from(!current.is_empty());
        if !current.is_empty() && current_u16 + need_nl + line_u16 <= limit {
            current.push('\n');
            current.push_str(&escape(line));
            current_u16 += need_nl + line_u16;
            continue;
        }
        if !current.is_empty() {
            pages.push(std::mem::take(&mut current));
            current_u16 = 0;
        }
        if line_u16 <= limit {
            current.push_str(&escape(line));
            current_u16 = line_u16;
        } else {
            // Single line exceeds limit: split cleanly on scalar boundaries without breaking entities.
            for ch in line.chars() {
                let w = escaped_char_utf16(ch);
                if current_u16 + w > limit && !current.is_empty() {
                    pages.push(std::mem::take(&mut current));
                    current_u16 = 0;
                }
                match ch {
                    '<' => current.push_str("&lt;"),
                    '>' => current.push_str("&gt;"),
                    '&' => current.push_str("&amp;"),
                    other => current.push(other),
                }
                current_u16 += w;
            }
        }
        let _ = idx;
    }

    if !current.is_empty() {
        pages.push(current);
    }
    pages
}

/// Splits Telegram HTML across pages of at most `limit` rendered UTF-16 code units,
/// cleanly closing open formatting tags at page ends and reopening them on subsequent pages.
pub fn html_pages(html: &str, limit: usize) -> Vec<String> {
    if html.is_empty() {
        return Vec::new();
    }
    let limit = if limit == 0 { PAGE_LIMIT } else { limit };
    let tokens = tokenize_html(html);

    let mut pages = Vec::new();
    let mut stack: Vec<OpenTag> = Vec::new();
    let mut cur = String::new();
    let mut cur_u16 = 0usize;
    let mut has_content = false;

    let close_suffix = |stack: &[OpenTag]| -> String {
        let mut s = String::new();
        for tag in stack.iter().rev() {
            s.push_str(&tag.close);
        }
        s
    };

    let open_prefix = |stack: &[OpenTag]| -> String {
        let mut s = String::new();
        for tag in stack {
            s.push_str(&tag.open);
        }
        s
    };

    for tok in tokens {
        match tok {
            HtmlToken::OpenTag(tag) => {
                if stack.len() < MAX_OPEN_TAGS {
                    cur.push_str(&tag.open);
                    stack.push(tag);
                }
            }
            HtmlToken::CloseTag(name) => {
                if let Some(pos) = stack.iter().rposition(|t| t.name == name) {
                    while stack.len() > pos {
                        if let Some(popped) = stack.pop() {
                            cur.push_str(&popped.close);
                        }
                    }
                }
            }
            HtmlToken::Entity(raw, u16_len) => {
                if cur_u16 + u16_len > limit && has_content {
                    cur.push_str(&close_suffix(&stack));
                    pages.push(cur);
                    cur = open_prefix(&stack);
                    cur_u16 = 0;
                }
                cur.push_str(&raw);
                cur_u16 += u16_len;
                has_content = true;
            }
            HtmlToken::Text(seg) => {
                for ch in seg.chars() {
                    let w = ch.len_utf16();
                    if cur_u16 + w > limit && has_content {
                        if let Some(split_cur) = try_split_at_newline(&cur, &stack) {
                            let (head, tail) = split_cur;
                            pages.push(head);
                            cur = tail;
                            cur_u16 = visible_utf16_len(&cur);
                            has_content = cur_u16 > 0;
                            if ch == '\n' && !has_content {
                                continue;
                            }
                            if cur_u16 + w <= limit {
                                cur.push(ch);
                                cur_u16 += w;
                                has_content = true;
                                continue;
                            }
                        }
                        cur.push_str(&close_suffix(&stack));
                        pages.push(cur);
                        cur = open_prefix(&stack);
                        cur_u16 = 0;
                        has_content = false;
                        if ch == '\n' {
                            continue;
                        }
                    }
                    cur.push(ch);
                    cur_u16 += w;
                    has_content = true;
                }
            }
        }
    }

    if has_content {
        cur.push_str(&close_suffix(&stack));
        pages.push(cur);
    }
    pages
}

fn try_split_at_newline(cur: &str, stack: &[OpenTag]) -> Option<(String, String)> {
    // Only consider newlines outside markup tags (after the reopened tag prefix).
    let open_prefix: String = stack.iter().map(|t| t.open.as_str()).collect();
    if cur.len() <= open_prefix.len() {
        return None;
    }
    let body = &cur[open_prefix.len()..];
    let nl = body.rfind('\n')?;
    if nl == 0 {
        return None;
    }
    let abs_nl = open_prefix.len() + nl;
    let close_suffix: String = stack.iter().rev().map(|t| t.close.as_str()).collect();
    // Skip split if inside a raw HTML tag between open tags (conservative check: balanced '<' before nl).
    let head_raw = &cur[..abs_nl];
    if head_raw.rfind('<').is_some_and(|lt| head_raw[lt..].find('>').is_none()) {
        return None;
    }
    let tail_body = &cur[abs_nl + 1..];
    let mut head = String::with_capacity(head_raw.len() + close_suffix.len());
    head.push_str(head_raw);
    head.push_str(&close_suffix);
    let mut tail = String::with_capacity(open_prefix.len() + tail_body.len());
    tail.push_str(&open_prefix);
    tail.push_str(tail_body);
    Some((head, tail))
}

fn visible_utf16_len(html_fragment: &str) -> usize {
    let mut total = 0usize;
    for tok in tokenize_html(html_fragment) {
        match tok {
            HtmlToken::Text(t) => total += utf16_len(&t) as usize,
            HtmlToken::Entity(_, w) => total += w,
            HtmlToken::OpenTag(_) | HtmlToken::CloseTag(_) => {}
        }
    }
    total
}

fn tokenize_html(input: &str) -> Vec<HtmlToken> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut text_start = 0usize;

    let flush_text = |end: usize, start: &mut usize, tokens: &mut Vec<HtmlToken>| {
        if end > *start {
            tokens.push(HtmlToken::Text(input[*start..end].to_owned()));
        }
        *start = end;
    };

    while i < bytes.len() {
        if bytes[i] == b'<'
            && let Some((tag_end, raw_tag)) = read_tag_slice(input, i)
        {
            flush_text(i, &mut text_start, &mut tokens);
            if let Some(tok) = classify_tag(raw_tag) {
                tokens.push(tok);
            }
            i = tag_end;
            text_start = tag_end;
            continue;
        }
        if bytes[i] == b'&'
            && let Some(semi_rel) = input[i + 1..].find(';')
            && semi_rel > 0
            && semi_rel <= 12
        {
            let entity_str = &input[i..=i + 1 + semi_rel];
            if let Some(decoded_u16) = decoded_entity_utf16(entity_str) {
                flush_text(i, &mut text_start, &mut tokens);
                tokens.push(HtmlToken::Entity(entity_str.to_owned(), decoded_u16));
                i = i + 1 + semi_rel + 1;
                text_start = i;
                continue;
            }
        }
        let ch_len = input[i..]
            .chars()
            .next()
            .map_or(1, |c| c.len_utf8());
        i += ch_len;
    }

    flush_text(bytes.len(), &mut text_start, &mut tokens);
    tokens
}

fn read_tag_slice(input: &str, start: usize) -> Option<(usize, &str)> {
    let mut in_quote: Option<u8> = None;
    let bytes = input.as_bytes();
    let mut idx = start + 1;
    while idx < bytes.len() {
        let b = bytes[idx];
        match in_quote {
            Some(q) if b == q => in_quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => in_quote = Some(b),
            None if b == b'>' => return Some((idx + 1, &input[start + 1..idx])),
            None if b == b'<' => return None,
            None => {}
        }
        idx += 1;
    }
    None
}

fn classify_tag(raw_inner: &str) -> Option<HtmlToken> {
    let trimmed = raw_inner.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        let name = rest
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if is_allowed_tag(&name) {
            return Some(HtmlToken::CloseTag(name));
        }
        return None;
    }
    let self_closing = trimmed.ends_with('/');
    if self_closing {
        let name = trimmed[..trimmed.len() - 1].trim().to_ascii_lowercase();
        if name == "br" {
            return Some(HtmlToken::Text("\n".into()));
        }
        return None;
    }
    let split_at = trimmed
        .find(|c: char| c.is_whitespace())
        .unwrap_or(trimmed.len());
    let name = trimmed[..split_at].to_ascii_lowercase();
    if name == "br" {
        return Some(HtmlToken::Text("\n".into()));
    }
    let attrs = trimmed[split_at..].trim();
    match name.as_str() {
        "b" | "strong" | "i" | "em" | "u" | "ins" | "s" | "strike" | "del" | "tg-spoiler" => {
            Some(HtmlToken::OpenTag(OpenTag {
                name: name.clone(),
                open: format!("<{name}>"),
                close: format!("</{name}>"),
            }))
        }
        "code" => {
            let class = extract_attr(attrs, "class")
                .filter(|c| c.starts_with("language-") && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
            let open = match class {
                Some(cls) => format!("<code class=\"{cls}\">"),
                None => "<code>".into(),
            };
            Some(HtmlToken::OpenTag(OpenTag {
                name,
                open,
                close: "</code>".into(),
            }))
        }
        "pre" => Some(HtmlToken::OpenTag(OpenTag {
            name,
            open: "<pre>".into(),
            close: "</pre>".into(),
        })),
        "blockquote" => {
            let expandable = attrs
                .split_whitespace()
                .any(|tok| tok.eq_ignore_ascii_case("expandable") || tok.to_ascii_lowercase().starts_with("expandable="));
            let open = if expandable {
                "<blockquote expandable>".into()
            } else {
                "<blockquote>".into()
            };
            Some(HtmlToken::OpenTag(OpenTag {
                name,
                open,
                close: "</blockquote>".into(),
            }))
        }
        "a" => {
            let href = extract_attr(attrs, "href")?;
            if !(href.starts_with("http://") || href.starts_with("https://") || href.starts_with("tg://")) {
                return None;
            }
            let safe_href = escape(&href);
            Some(HtmlToken::OpenTag(OpenTag {
                name,
                open: format!("<a href=\"{safe_href}\">"),
                close: "</a>".into(),
            }))
        }
        _ => None,
    }
}

fn is_allowed_tag(name: &str) -> bool {
    matches!(
        name,
        "b" | "strong"
            | "i"
            | "em"
            | "u"
            | "ins"
            | "s"
            | "strike"
            | "del"
            | "code"
            | "pre"
            | "a"
            | "blockquote"
            | "tg-spoiler"
    )
}

fn extract_attr(attrs: &str, key: &str) -> Option<String> {
    let bytes = attrs.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        let attr_name = &attrs[name_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let val = if bytes[i] == b'"' || bytes[i] == b'\'' {
            let q = bytes[i];
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            let v = &attrs[val_start..i];
            if i < bytes.len() {
                i += 1;
            }
            v
        } else {
            let val_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            &attrs[val_start..i]
        };
        if attr_name.eq_ignore_ascii_case(key) {
            return Some(unescape_basic(val));
        }
    }
    None
}

fn unescape_basic(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn decoded_entity_utf16(entity: &str) -> Option<usize> {
    match entity {
        "&lt;" | "&gt;" | "&amp;" | "&quot;" | "&apos;" | "&nbsp;" => Some(1),
        _ if entity.starts_with("&#x") || entity.starts_with("&#X") => {
            let hex = entity.strip_suffix(';')?.get(3..)?;
            let cp = u32::from_str_radix(hex, 16).ok()?;
            char::from_u32(cp).map(|c| c.len_utf16())
        }
        _ if entity.starts_with("&#") => {
            let dec = entity.strip_suffix(';')?.get(2..)?;
            let cp = dec.parse::<u32>().ok()?;
            char::from_u32(cp).map(|c| c.len_utf16())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_pages_bounds_utf16_length() {
        let raw = "😀<script>&\n".repeat(12);
        let pages = escaped_pages(&raw, 20);
        assert!(pages.len() >= 2);
        for page in &pages {
            assert!(utf16_len(page) as usize <= 20, "page exceeded limit: {page}");
            assert!(!page.contains("<script>"));
        }
    }

    #[test]
    fn html_pages_reopens_nested_tags_and_drops_disallowed() {
        let html = "<b><i>前😀</i></b><script>bad</script>\n<blockquote expandable><b>一二三四五六七八</b></blockquote><a href=\"javascript:x\">bad</a>";
        let pages = html_pages(html, 8);
        assert!(pages.len() >= 2);
        assert!(pages.iter().all(|p| !p.contains("<script") && !p.contains("javascript:")));
        assert!(pages.iter().any(|p| p.contains("<blockquote expandable><b>") && p.ends_with("</b></blockquote>")));
    }

    #[test]
    fn html_pages_handles_deep_nesting_without_panic() {
        let opens = "<b>".repeat(40);
        let closes = "</b>".repeat(40);
        let pages = html_pages(&format!("{opens}hello world{closes}"), 6);
        assert_eq!(pages.len(), 2);
    }
}
