use crate::ai::escape_html;
use grammers_tl_types as tl;
use mibot_bot::{RICH_COLUMN_LIMIT, rich_concat, rich_plain};
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::OnceLock;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MdBlockKind {
    Paragraph,
    Heading,
    Code,
    Quote,
    List,
    Rule,
    Table,
    MathBlock,
}

#[derive(Debug, Clone)]
struct MdItem {
    ordered: bool,
    number: i32,
    #[allow(dead_code)]
    indent: usize,
    task: bool,
    checked: bool,
    lines: Vec<String>,
    children: Vec<MdItem>,
}

#[derive(Debug, Clone)]
struct MdBlock {
    kind: MdBlockKind,
    level: usize,
    lang: String,
    text: String,
    lines: Vec<String>,
    items: Vec<MdItem>,
    rows: Vec<Vec<String>>,
    align: Vec<u8>,
    html: bool,
    expandable: bool,
}

impl Default for MdBlock {
    fn default() -> Self {
        Self {
            kind: MdBlockKind::Paragraph,
            level: 0,
            lang: String::new(),
            text: String::new(),
            lines: Vec::new(),
            items: Vec::new(),
            rows: Vec::new(),
            align: Vec::new(),
            html: false,
            expandable: false,
        }
    }
}

const MD_ESCAPABLE: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";
const MD_MAX_LIST_DEPTH: usize = 3;
const MD_MAX_TAG_DEPTH: usize = 16;

fn rule_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:---|\*\*\*)\s*$").unwrap())
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$").unwrap())
}

fn bullet_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\s{0,12})([-*+])\s+(.+)$").unwrap())
}

fn ordered_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\s{0,12})(\d{1,9})[.)]\s+(.+)$").unwrap())
}

fn indented_code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^ {4,}\S").unwrap())
}

fn cite_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)<\s*/?\s*cite\s*>|<\s*cite\s*/\s*>|&lt;\s*/?\s*cite\s*&gt;|&lt;\s*cite\s*/\s*&gt;").unwrap()
    })
}

fn language_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9_+-]+$").unwrap())
}

fn leading_spaces_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^( {2,})(.*)$").unwrap())
}

fn task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[([ xX])\]\s+").unwrap())
}

fn table_delimiter_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*)*\|?\s*$").unwrap())
}

fn quote_open_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*<blockquote(\s+expandable)?\s*>").unwrap())
}

fn quote_close_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)</blockquote\s*>").unwrap())
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"^<(/?)([a-zA-Z][a-zA-Z0-9-]*)((?:\s+[a-zA-Z-]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s"'>]+))?)*)\s*(/?)>"#).unwrap()
    })
}

fn attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"([a-zA-Z-]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+)))?"#).unwrap()
    })
}

fn entity_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^&(?:#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z][a-zA-Z0-9]{1,31});").unwrap())
}

fn md_fence(line: &str) -> bool {
    line.trim_start_matches([' ', '\t']).starts_with("```")
}

fn md_quote_line(line: &str) -> bool {
    line.trim_start_matches([' ', '\t']).starts_with('>')
}

fn md_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn md_marker(line: &str) -> Option<(usize, bool, i32, String)> {
    if let Some(cap) = bullet_re().captures(line) {
        return Some((cap[1].len(), false, 0, cap[3].to_owned()));
    }
    if let Some(cap) = ordered_re().captures(line) {
        let num = cap[2].parse::<i32>().unwrap_or(1);
        return Some((cap[1].len(), true, num, cap[3].to_owned()));
    }
    None
}

fn md_display_math_start(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("$$") || t.starts_with(r"\[")
}

fn parse_display_math(lines: &[String], start: usize) -> Option<(String, usize)> {
    let trimmed = lines[start].trim();
    let (open, close) = if trimmed.starts_with(r"\[") {
        (r"\[", r"\]")
    } else {
        ("$$", "$$")
    };
    let mut parts = vec![trimmed.strip_prefix(open).unwrap_or(trimmed).to_owned()];
    for (offset, raw) in lines[start..].iter().enumerate() {
        if offset > 0 {
            if md_blank(raw) {
                break;
            }
            parts.push(raw.trim().to_owned());
        }
        let last = parts.last()?.clone();
        let Some(end) = last.find(close) else {
            continue;
        };
        if !last[end + close.len()..].trim().is_empty() {
            return None;
        }
        *parts.last_mut()? = last[..end].to_owned();
        let source = parts.join("\n").trim().to_owned();
        if source.is_empty() {
            return None;
        }
        return Some((source, start + offset + 1));
    }
    None
}

fn parse_html_quote(lines: &mut [String], start: usize) -> (MdBlock, usize) {
    let cap = quote_open_re().captures(&lines[start]).unwrap();
    let expandable = cap.get(1).is_some();
    let match_len = cap.get(0).unwrap().end();
    let mut rest = lines[start][match_len..].to_owned();
    let mut out_lines = Vec::new();
    let mut index = start;
    loop {
        if let Some(m) = quote_close_re().find(&rest) {
            out_lines.push(rest[..m.start()].to_owned());
            let after = rest[m.end()..].to_owned();
            if !after.trim().is_empty() {
                lines[index] = after;
            } else {
                index += 1;
            }
            break;
        }
        out_lines.push(rest);
        index += 1;
        if index >= lines.len() {
            break;
        }
        rest = lines[index].clone();
    }
    while out_lines.first().is_some_and(|l| md_blank(l)) {
        out_lines.remove(0);
    }
    while out_lines.last().is_some_and(|l| md_blank(l)) {
        out_lines.pop();
    }
    (
        MdBlock {
            kind: MdBlockKind::Quote,
            lines: out_lines,
            html: true,
            expandable,
            ..Default::default()
        },
        index,
    )
}

fn md_table_cells(line: &str) -> Vec<String> {
    let mut line = line.trim();
    line = line.strip_prefix('|').unwrap_or(line);
    if line.ends_with('|') && !line.ends_with(r"\|") {
        line = &line[..line.len() - 1];
    }
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_code = false;
    let bytes = line.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'\\' && idx + 1 < bytes.len() && bytes[idx + 1] == b'|' {
            cell.push('|');
            idx += 2;
            continue;
        }
        if bytes[idx] == b'`' {
            in_code = !in_code;
            cell.push('`');
            idx += 1;
            continue;
        }
        if bytes[idx] == b'|' && !in_code {
            cells.push(cell.trim().to_owned());
            cell.clear();
            idx += 1;
            continue;
        }
        let ch = line[idx..].chars().next().unwrap();
        cell.push(ch);
        idx += ch.len_utf8();
    }
    cells.push(cell.trim().to_owned());
    cells
}

fn md_table_start(lines: &[String], index: usize) -> bool {
    if index + 1 >= lines.len()
        || !lines[index].contains('|')
        || !lines[index + 1].contains('|')
        || !table_delimiter_re().is_match(&lines[index + 1])
    {
        return false;
    }
    md_table_cells(&lines[index]).len() == md_table_cells(&lines[index + 1]).len()
}

fn parse_table(lines: &[String], start: usize) -> (MdBlock, usize) {
    let header = md_table_cells(&lines[start]);
    let cols = header.len();
    let mut rows = vec![header];
    let mut align = Vec::with_capacity(cols);
    for cell in md_table_cells(&lines[start + 1]) {
        let a = if cell.starts_with(':') && cell.ends_with(':') {
            1
        } else if cell.ends_with(':') {
            2
        } else {
            0
        };
        align.push(a);
    }
    let mut index = start + 2;
    while index < lines.len() {
        let line = &lines[index];
        if md_blank(line)
            || !line.contains('|')
            || md_quote_line(line)
            || md_fence(line)
            || heading_re().is_match(line)
        {
            break;
        }
        let mut cells = md_table_cells(line);
        cells.resize(cols, String::new());
        rows.push(cells);
        index += 1;
    }
    (
        MdBlock {
            kind: MdBlockKind::Table,
            rows,
            align,
            ..Default::default()
        },
        index,
    )
}

fn parse_markdown_list(lines: &[String], start: usize) -> (Vec<MdItem>, usize) {
    // Store items via tree builder using an explicit index path stack.
    let mut root_items: Vec<MdItem> = Vec::new();
    let mut path: Vec<(usize, usize)> = Vec::new(); // (indent, child_index_in_level)
    let mut index = start;

    fn get_item_mut<'a>(root: &'a mut [MdItem], path: &[(usize, usize)]) -> Option<&'a mut MdItem> {
        let first = path.first()?;
        let mut cur = root.get_mut(first.1)?;
        for &(_, idx) in &path[1..] {
            cur = cur.children.get_mut(idx)?;
        }
        Some(cur)
    }

    while index < lines.len() {
        let line = &lines[index];
        if md_blank(line)
            || rule_re().is_match(line)
            || md_quote_line(line)
            || md_fence(line)
            || heading_re().is_match(line)
            || quote_open_re().is_match(line)
        {
            break;
        }
        let Some((indent, ordered, number, mut text)) = md_marker(line) else {
            if let Some(cur) = get_item_mut(&mut root_items, &path) {
                cur.lines.push(line.trim().to_owned());
            }
            index += 1;
            continue;
        };
        while let Some(&(top_indent, _)) = path.last() {
            if top_indent >= indent {
                path.pop();
            } else {
                break;
            }
        }
        if path.len() >= MD_MAX_LIST_DEPTH {
            if let Some(cur) = get_item_mut(&mut root_items, &path) {
                cur.lines.push(text);
            }
            index += 1;
            continue;
        }
        let mut task = false;
        let mut checked = false;
        if let Some(cap) = task_re().captures(&text) {
            task = true;
            checked = &cap[1] != " ";
            let m_end = cap.get(0).unwrap().end();
            text = text[m_end..].to_owned();
        }
        let item = MdItem {
            ordered,
            number,
            indent,
            task,
            checked,
            lines: vec![text],
            children: Vec::new(),
        };
        let new_idx = if path.is_empty() {
            root_items.push(item);
            root_items.len() - 1
        } else {
            let parent = get_item_mut(&mut root_items, &path).unwrap();
            parent.children.push(item);
            parent.children.len() - 1
        };
        path.push((indent, new_idx));
        index += 1;
    }
    (root_items, index)
}

fn parse_markdown(source: &str) -> Vec<MdBlock> {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_owned).collect();
    let mut blocks = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();

    let flush = |blocks: &mut Vec<MdBlock>, paragraph: &mut Vec<String>| {
        if !paragraph.is_empty() && !paragraph.join("\n").trim().is_empty() {
            blocks.push(MdBlock {
                kind: MdBlockKind::Paragraph,
                lines: std::mem::take(paragraph),
                ..Default::default()
            });
        } else {
            paragraph.clear();
        }
    };

    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].clone();
        if md_blank(&line) {
            flush(&mut blocks, &mut paragraph);
            index += 1;
        } else if rule_re().is_match(&line) {
            flush(&mut blocks, &mut paragraph);
            blocks.push(MdBlock {
                kind: MdBlockKind::Rule,
                ..Default::default()
            });
            index += 1;
        } else if md_fence(&line) {
            flush(&mut blocks, &mut paragraph);
            let info = line.trim_start_matches([' ', '\t']).trim_start_matches('`').trim();
            let lang = info.split_whitespace().next().unwrap_or("").to_owned();
            index += 1;
            let mut code = Vec::new();
            while index < lines.len() && !md_fence(&lines[index]) {
                code.push(lines[index].clone());
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            blocks.push(MdBlock {
                kind: MdBlockKind::Code,
                lang,
                text: code.join("\n"),
                ..Default::default()
            });
        } else if md_display_math_start(&line) {
            if let Some((src, next)) = parse_display_math(&lines, index) {
                flush(&mut blocks, &mut paragraph);
                blocks.push(MdBlock {
                    kind: MdBlockKind::MathBlock,
                    text: src,
                    ..Default::default()
                });
                index = next;
            } else {
                paragraph.push(line);
                index += 1;
            }
        } else if quote_open_re().is_match(&line) {
            flush(&mut blocks, &mut paragraph);
            let (block, next) = parse_html_quote(&mut lines, index);
            blocks.push(block);
            index = next;
        } else if paragraph.is_empty()
            && indented_code_re().is_match(&line)
            && md_marker(&line).is_none()
        {
            let mut code = Vec::new();
            while index < lines.len()
                && (indented_code_re().is_match(&lines[index]) || md_blank(&lines[index]))
            {
                if md_blank(&lines[index]) {
                    code.push(String::new());
                } else {
                    code.push(lines[index].strip_prefix("    ").unwrap_or(&lines[index]).to_owned());
                }
                index += 1;
            }
            while code.last().is_some_and(|l| l.is_empty()) {
                code.pop();
            }
            blocks.push(MdBlock {
                kind: MdBlockKind::Code,
                text: code.join("\n"),
                ..Default::default()
            });
        } else if md_quote_line(&line) {
            flush(&mut blocks, &mut paragraph);
            let mut quote = Vec::new();
            while index < lines.len() && md_quote_line(&lines[index]) {
                quote.push(lines[index].clone());
                index += 1;
            }
            blocks.push(MdBlock {
                kind: MdBlockKind::Quote,
                lines: quote,
                ..Default::default()
            });
        } else if let Some(cap) = heading_re().captures(&line) {
            flush(&mut blocks, &mut paragraph);
            blocks.push(MdBlock {
                kind: MdBlockKind::Heading,
                level: cap[1].len(),
                text: cap[2].trim().to_owned(),
                ..Default::default()
            });
            index += 1;
        } else if md_table_start(&lines, index) {
            flush(&mut blocks, &mut paragraph);
            let (block, next) = parse_table(&lines, index);
            blocks.push(block);
            index = next;
        } else if md_marker(&line).is_some() {
            flush(&mut blocks, &mut paragraph);
            let (items, next) = parse_markdown_list(&lines, index);
            blocks.push(MdBlock {
                kind: MdBlockKind::List,
                items,
                ..Default::default()
            });
            index = next;
        } else {
            paragraph.push(line);
            index += 1;
        }
    }
    flush(&mut blocks, &mut paragraph);
    blocks
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MdInlineKind {
    Text,
    CodeSpan,
    Bold,
    Italic,
    Strike,
    Underline,
    Spoiler,
    Link,
    Image,
    Math,
    Break,
    Marked,
    Sub,
    Sup,
    Span,
}

#[derive(Debug, Clone)]
struct MdInline {
    kind: MdInlineKind,
    text: String,
    url: String,
    children: Vec<MdInline>,
}

impl MdInline {
    fn text(s: impl Into<String>) -> Self {
        Self {
            kind: MdInlineKind::Text,
            text: s.into(),
            url: String::new(),
            children: Vec::new(),
        }
    }
    fn wrap(kind: MdInlineKind, children: Vec<MdInline>) -> Self {
        Self {
            kind,
            text: String::new(),
            url: String::new(),
            children,
        }
    }
}

fn inline_tag_kind(name: &str) -> Option<MdInlineKind> {
    Some(match name {
        "b" | "strong" => MdInlineKind::Bold,
        "i" | "em" => MdInlineKind::Italic,
        "u" | "ins" => MdInlineKind::Underline,
        "s" | "strike" | "del" => MdInlineKind::Strike,
        "tg-spoiler" | "span" => MdInlineKind::Spoiler,
        "code" => MdInlineKind::CodeSpan,
        "a" => MdInlineKind::Link,
        "mark" => MdInlineKind::Marked,
        "sub" => MdInlineKind::Sub,
        "sup" => MdInlineKind::Sup,
        "tg-emoji" => MdInlineKind::Span,
        _ => return None,
    })
}

fn safe_url(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(u) if (u.scheme() == "http" || u.scheme() == "https") && u.host_str().is_some() => {
            raw.to_owned()
        }
        _ => String::new(),
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn url_stop(r: char) -> bool {
    r.is_whitespace()
        || matches!(r, '<' | '>' | '(' | ')')
        || ('\u{3000}'..='\u{303f}').contains(&r)
        || ('\u{ff00}'..='\u{ffef}').contains(&r)
}

fn has_prefix_fold(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.is_char_boundary(prefix.len())
        && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn unescape_html_basic(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        if let Some(semi) = rest.find(';')
            && semi <= 12
            && let Some(ch) = decode_html_entity(&rest[1..semi])
        {
            out.push(ch);
            rest = &rest[semi + 1..];
            continue;
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn decode_html_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00a0}'),
        _ if name.starts_with("#x") || name.starts_with("#X") => {
            u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32)
        }
        _ if name.starts_with('#') => name[1..].parse::<u32>().ok().and_then(char::from_u32),
        _ => None,
    }
}

fn dollar_math(text: &str) -> Option<(String, usize)> {
    if let Some(body) = text.strip_prefix("$$") {
        let end = body.find("$$")?;
        let source = body[..end].trim().to_owned();
        if source.is_empty() {
            return None;
        }
        return Some((source, 2 + end + 2));
    }
    let bytes = text.as_bytes();
    if bytes.len() < 3 || bytes[1] == b' ' || bytes[1] == b'\t' {
        return None;
    }
    let end = text[1..].find('$')? + 1;
    if end <= 1 || bytes[end - 1] == b' ' || bytes[end - 1] == b'\t' {
        return None;
    }
    if end + 1 < bytes.len() && bytes[end + 1].is_ascii_digit() {
        return None;
    }
    Some((text[1..end].to_owned(), end + 1))
}

fn tag_attrs(raw: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
    for cap in attr_re().captures_iter(raw) {
        let val = format!(
            "{}{}{}",
            cap.get(2).map(|m| m.as_str()).unwrap_or(""),
            cap.get(3).map(|m| m.as_str()).unwrap_or(""),
            cap.get(4).map(|m| m.as_str()).unwrap_or("")
        );
        attrs.insert(cap[1].to_ascii_lowercase(), val);
    }
    attrs
}

fn tag_content<'a>(text: &'a str, name: &str, start: usize) -> Option<(&'a str, usize)> {
    let mut depth = 1usize;
    let bytes = text.as_bytes();
    let mut offset = start;
    while offset < bytes.len() {
        if bytes[offset] != b'<' {
            offset += 1;
            continue;
        }
        let rest = text[offset + 1..].strip_prefix('/').unwrap_or(&text[offset + 1..]);
        if rest.len() <= name.len()
            || !rest.is_char_boundary(name.len())
            || !rest[..name.len()].eq_ignore_ascii_case(name)
            || !rest[name.len()..].chars().next().is_some_and(|c| " \t\n/>".contains(c))
        {
            offset += 1;
            continue;
        }
        let Some(cap) = tag_re().captures(&text[offset..]) else {
            offset += 1;
            continue;
        };
        if !cap[2].eq_ignore_ascii_case(name) || !cap[4].is_empty() {
            offset += 1;
            continue;
        }
        if cap[1].is_empty() {
            depth += 1;
            if depth > MD_MAX_TAG_DEPTH {
                return None;
            }
            offset += 1;
            continue;
        }
        depth -= 1;
        if depth == 0 {
            let close_len = cap.get(0).unwrap().end();
            return Some((&text[start..offset], offset + close_len));
        }
        offset += 1;
    }
    None
}

fn inline_tag(text: &str, in_link: bool) -> Option<(MdInline, usize)> {
    let cap = tag_re().captures(text)?;
    if !cap[1].is_empty() {
        return None;
    }
    let full_len = cap.get(0).unwrap().end();
    let name = cap[2].to_ascii_lowercase();
    if name == "br" {
        return Some((MdInline::wrap(MdInlineKind::Break, Vec::new()), full_len));
    }
    let kind = inline_tag_kind(&name)?;
    let attrs = tag_attrs(&cap[3]);
    if !cap[4].is_empty()
        || (name == "span" && attrs.get("class").map(String::as_str) != Some("tg-spoiler"))
        || (name == "a" && in_link)
    {
        return None;
    }
    let (inner, size) = tag_content(text, &name, full_len)?;
    match kind {
        MdInlineKind::CodeSpan => Some((
            MdInline {
                kind: MdInlineKind::CodeSpan,
                text: unescape_html_basic(inner),
                url: String::new(),
                children: Vec::new(),
            },
            size,
        )),
        MdInlineKind::Link => {
            let safe = safe_url(&unescape_html_basic(attrs.get("href").map(String::as_str).unwrap_or("")));
            if safe.is_empty() {
                Some((MdInline::wrap(MdInlineKind::Span, parse_inline(inner, true)), size))
            } else {
                Some((
                    MdInline {
                        kind: MdInlineKind::Link,
                        text: String::new(),
                        url: safe,
                        children: parse_inline(inner, true),
                    },
                    size,
                ))
            }
        }
        _ => Some((MdInline::wrap(kind, parse_inline(inner, in_link)), size)),
    }
}

fn delimited<'a>(text: &'a str, marker: &str, word_boundary: bool) -> Option<(&'a str, usize)> {
    let body = &text[marker.len()..];
    let mut offset = 0usize;
    while offset < body.len() {
        let rel = body[offset..].find(marker)?;
        let end = offset + rel;
        let after = &body[end + marker.len()..];
        if after.starts_with(&marker[..1])
            || (word_boundary && after.as_bytes().first().is_some_and(|&b| is_word_byte(b)))
        {
            offset = end + 1;
            continue;
        }
        let inner = &body[..end];
        if inner.is_empty() || inner.trim() != inner {
            return None;
        }
        return Some((inner, marker.len() + end + marker.len()));
    }
    None
}

fn link_at(text: &str) -> Option<(&str, &str, usize)> {
    if !text.starts_with('[') {
        return None;
    }
    let close = text.find(']')?;
    if close <= 1 || text[1..close].contains(['[', '\n']) || !text[close + 1..].starts_with('(') {
        return None;
    }
    let rest = &text[close + 2..];
    let end = rest.find(')')?;
    if end == 0 || rest[..end].chars().any(char::is_whitespace) {
        return None;
    }
    Some((&text[1..close], &rest[..end], close + 2 + end + 1))
}

fn parse_inline(text: &str, in_link: bool) -> Vec<MdInline> {
    let mut nodes = Vec::new();
    let mut plain = String::new();
    let flush = |nodes: &mut Vec<MdInline>, plain: &mut String| {
        if !plain.is_empty() {
            nodes.push(MdInline::text(std::mem::take(plain)));
        }
    };
    let mut index = 0usize;
    while index < text.len() {
        let rest = &text[index..];
        let prev = if index > 0 {
            text.as_bytes()[index - 1]
        } else {
            b' '
        };
        let b0 = rest.as_bytes()[0];

        if b0 == b'`' {
            let run = rest.len() - rest.trim_start_matches('`').len();
            let fence = &rest[..run];
            if let Some(end) = rest[run..].find(fence)
                && end > 0
                && !rest[run + end + run..].starts_with('`')
            {
                flush(&mut nodes, &mut plain);
                nodes.push(MdInline {
                    kind: MdInlineKind::CodeSpan,
                    text: rest[run..run + end].to_owned(),
                    url: String::new(),
                    children: Vec::new(),
                });
                index += run + end + run;
                continue;
            }
            plain.push_str(fence);
            index += run;
            continue;
        }
        if rest.starts_with(r"\(") || rest.starts_with(r"\[") {
            let close = if rest.as_bytes()[1] == b'[' { r"\]" } else { r"\)" };
            if let Some(end) = rest[2..].find(close)
                && end > 0
            {
                let src = rest[2..2 + end].trim();
                if !src.is_empty() {
                    flush(&mut nodes, &mut plain);
                    nodes.push(MdInline {
                        kind: MdInlineKind::Math,
                        text: src.to_owned(),
                        url: String::new(),
                        children: Vec::new(),
                    });
                    index += 2 + end + close.len();
                    continue;
                }
            }
            plain.push(rest.as_bytes()[1] as char);
            index += 2;
            continue;
        }
        if b0 == b'\\' && rest.len() > 1 && MD_ESCAPABLE.contains(rest.as_bytes()[1] as char) {
            plain.push(rest.as_bytes()[1] as char);
            index += 2;
            continue;
        }
        if b0 == b'$'
            && let Some((src, size)) = dollar_math(rest)
        {
            flush(&mut nodes, &mut plain);
            nodes.push(MdInline {
                kind: MdInlineKind::Math,
                text: src,
                url: String::new(),
                children: Vec::new(),
            });
            index += size;
            continue;
        }
        if b0 == b'<'
            && let Some((node, size)) = inline_tag(rest, in_link)
        {
            flush(&mut nodes, &mut plain);
            nodes.push(node);
            index += size;
            continue;
        }
        if b0 == b'&'
            && let Some(m) = entity_re().find(rest)
        {
            let entity = m.as_str();
            let decoded = unescape_html_basic(entity);
            if decoded != entity {
                plain.push_str(&decoded);
                index += entity.len();
                continue;
            }
        }
        if !in_link
            && rest.starts_with("![")
            && let Some((label, target, size)) = link_at(&rest[1..])
        {
            flush(&mut nodes, &mut plain);
            nodes.push(MdInline {
                kind: MdInlineKind::Image,
                text: label.to_owned(),
                url: safe_url(target),
                children: Vec::new(),
            });
            index += 1 + size;
            continue;
        }
        if !in_link
            && b0 == b'['
            && let Some((label, target, size)) = link_at(rest)
        {
            flush(&mut nodes, &mut plain);
            let safe = safe_url(target);
            if !safe.is_empty() {
                nodes.push(MdInline {
                    kind: MdInlineKind::Link,
                    text: String::new(),
                    url: safe,
                    children: parse_inline(label, true),
                });
            } else {
                nodes.extend(parse_inline(label, true));
            }
            index += size;
            continue;
        }
        if !in_link
            && !is_word_byte(prev)
            && (has_prefix_fold(rest, "http://") || has_prefix_fold(rest, "https://"))
        {
            let end = rest.find(url_stop).unwrap_or(rest.len());
            let link = rest[..end].trim_end_matches(['.', ',', '!', '?', ';', ':', '\'', '"']);
            let safe = safe_url(link);
            if !safe.is_empty() {
                flush(&mut nodes, &mut plain);
                nodes.push(MdInline {
                    kind: MdInlineKind::Link,
                    text: String::new(),
                    url: safe,
                    children: vec![MdInline::text(link)],
                });
                index += link.len();
                continue;
            }
        }
        if rest.starts_with("||")
            && let Some((inner, size)) = delimited(rest, "||", false)
        {
            flush(&mut nodes, &mut plain);
            nodes.push(MdInline::wrap(MdInlineKind::Spoiler, parse_inline(inner, in_link)));
            index += size;
            continue;
        }
        if rest.starts_with("~~")
            && let Some((inner, size)) = delimited(rest, "~~", false)
        {
            flush(&mut nodes, &mut plain);
            nodes.push(MdInline::wrap(MdInlineKind::Strike, parse_inline(inner, in_link)));
            index += size;
            continue;
        }
        if rest.starts_with("**")
            && let Some((inner, size)) = delimited(rest, "**", false)
        {
            flush(&mut nodes, &mut plain);
            nodes.push(MdInline::wrap(MdInlineKind::Bold, parse_inline(inner, in_link)));
            index += size;
            continue;
        }
        if rest.starts_with("__")
            && !is_word_byte(prev)
            && let Some((inner, size)) = delimited(rest, "__", true)
        {
            flush(&mut nodes, &mut plain);
            nodes.push(MdInline::wrap(MdInlineKind::Underline, parse_inline(inner, in_link)));
            index += size;
            continue;
        }
        if b0 == b'*'
            && prev != b'*'
            && let Some((inner, size)) = delimited(rest, "*", false)
            && !inner.contains('*')
        {
            flush(&mut nodes, &mut plain);
            nodes.push(MdInline::wrap(MdInlineKind::Italic, parse_inline(inner, in_link)));
            index += size;
            continue;
        }
        if b0 == b'_'
            && prev != b'_'
            && !is_word_byte(prev)
            && let Some((inner, size)) = delimited(rest, "_", true)
            && !inner.contains('_')
        {
            flush(&mut nodes, &mut plain);
            nodes.push(MdInline::wrap(MdInlineKind::Italic, parse_inline(inner, in_link)));
            index += size;
            continue;
        }

        let ch = rest.chars().next().unwrap();
        plain.push(ch);
        index += ch.len_utf8();
    }
    flush(&mut nodes, &mut plain);
    nodes
}

fn leading_indent(line: &str) -> (String, &str) {
    if let Some(cap) = leading_spaces_re().captures(line) {
        let count = cap.get(1).unwrap().as_str().len();
        let rest_start = cap.get(2).unwrap().start();
        ("\u{00a0}".repeat(count), &line[rest_start..])
    } else {
        (String::new(), line)
    }
}

fn quote_depth(line: &str) -> (usize, &str) {
    let bytes = line.as_bytes();
    let mut depth = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' => index += 1,
            b'>' => {
                depth += 1;
                index += 1;
                if index < bytes.len() && bytes[index] == b' ' {
                    index += 1;
                }
            }
            _ => break,
        }
    }
    (depth, &line[index..])
}

fn quote_inner(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| {
            let trimmed = l.trim_start_matches([' ', '\t']);
            let trimmed = trimmed.strip_prefix('>').unwrap_or(trimmed);
            trimmed.strip_prefix(' ').unwrap_or(trimmed).to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HtmlStyle {
    pub collapse_safe: bool,
    pub plain_headings: bool,
    pub fold_lists: bool,
}

pub fn markdown_html(markdown: &str, style: HtmlStyle) -> String {
    let cleaned = cite_re().replace_all(markdown, "");
    let mut rendered = Vec::new();
    for block in parse_markdown(&cleaned) {
        let h = render_block_html(&block, style);
        if !h.is_empty() {
            rendered.push(h);
        }
    }
    rendered.join("\n\n")
}

pub fn markdown_to_html(markdown: &str, collapse_safe: bool) -> String {
    markdown_html(
        markdown,
        HtmlStyle {
            collapse_safe,
            ..Default::default()
        },
    )
}

fn wrap_quote(body: &str, expandable: bool, collapse_safe: bool) -> String {
    if collapse_safe || body.trim().is_empty() {
        body.to_owned()
    } else if expandable {
        format!("<blockquote expandable>{body}</blockquote>")
    } else {
        format!("<blockquote>{body}</blockquote>")
    }
}

fn heading_html(text: &str, level: usize, plain: bool) -> String {
    let body = inline_html(&parse_inline(text.trim(), false));
    if plain {
        return format!("<b>{body}</b>");
    }
    match level {
        2 => format!("<b>▌{body}</b>"),
        3 => format!("<b>• {body}</b>"),
        _ => format!("<b>{body}</b>"),
    }
}

fn render_list_html(items: &[MdItem], depth: usize, out: &mut Vec<String>) {
    for item in items {
        let indent = "\u{00a0}".repeat((depth - 1) * 2);
        let mut marker = if item.ordered {
            format!("{}. ", item.number)
        } else {
            "• ".to_owned()
        };
        if item.task {
            marker.push_str(if item.checked { "☑ " } else { "☐ " });
        }
        out.push(format!(
            "{indent}{marker}{}",
            inline_html(&parse_inline(&item.lines[0], false))
        ));
        for line in &item.lines[1..] {
            let cont = "\u{00a0}".repeat((depth - 1) * 2 + 2);
            out.push(format!("{cont}{}", inline_html(&parse_inline(line, false))));
        }
        render_list_html(&item.children, depth + 1, out);
    }
}

fn render_block_html(block: &MdBlock, style: HtmlStyle) -> String {
    match block.kind {
        MdBlockKind::Rule => "────────────────".into(),
        MdBlockKind::Heading => heading_html(&block.text, block.level, style.plain_headings),
        MdBlockKind::MathBlock => {
            format!(r#"<pre><code class="language-latex">{}</code></pre>"#, escape_html(&block.text))
        }
        MdBlockKind::Table => {
            let mut out = Vec::with_capacity(block.rows.len());
            for (idx, row) in block.rows.iter().enumerate() {
                let cells: Vec<String> = row
                    .iter()
                    .map(|c| inline_html(&parse_inline(c, false)))
                    .collect();
                let line = cells.join(" | ");
                out.push(if idx == 0 {
                    format!("<b>{line}</b>")
                } else {
                    line
                });
            }
            out.join("\n")
        }
        MdBlockKind::Code => {
            if block.text.trim().is_empty() {
                return String::new();
            }
            let lang = block.lang.trim().to_ascii_lowercase();
            if !lang.is_empty() && language_re().is_match(&lang) {
                format!(
                    r#"<pre><code class="language-{lang}">{}</code></pre>"#,
                    escape_html(&block.text)
                )
            } else {
                format!("<pre><code>{}</code></pre>", escape_html(&block.text))
            }
        }
        MdBlockKind::Quote => {
            let mut out = Vec::new();
            if block.html {
                for line in &block.lines {
                    let (indent, rest) = leading_indent(line);
                    out.push(format!("{indent}{}", inline_html(&parse_inline(rest, false))));
                }
                return wrap_quote(&out.join("\n"), block.expandable, style.collapse_safe);
            }
            for line in &block.lines {
                let (depth, rest) = quote_depth(line);
                let rest = rest.trim_end_matches([' ', '\t']);
                let html = if let Some(cap) = heading_re().captures(rest) {
                    heading_html(cap[2].trim(), cap[1].len(), style.plain_headings)
                } else {
                    inline_html(&parse_inline(rest, false))
                };
                let prefix = if depth > 1 {
                    "┃ ".repeat((depth - 1).min(3))
                } else {
                    String::new()
                };
                out.push(format!("{prefix}{html}"));
            }
            wrap_quote(&out.join("\n"), false, style.collapse_safe)
        }
        MdBlockKind::List => {
            let mut out = Vec::new();
            render_list_html(&block.items, 1, &mut out);
            let body = out.join("\n");
            if style.fold_lists {
                wrap_quote(&body, true, style.collapse_safe)
            } else {
                body
            }
        }
        MdBlockKind::Paragraph => {
            let mut out = Vec::new();
            for line in &block.lines {
                let (indent, rest) = leading_indent(line);
                out.push(format!("{indent}{}", inline_html(&parse_inline(rest, false))));
            }
            out.join("\n").trim_end_matches([' ', '\n']).to_owned()
        }
    }
}

fn inline_html(nodes: &[MdInline]) -> String {
    let mut b = String::new();
    for node in nodes {
        match node.kind {
            MdInlineKind::Text => b.push_str(&escape_html(&node.text)),
            MdInlineKind::CodeSpan | MdInlineKind::Math => {
                b.push_str(&format!("<code>{}</code>", escape_html(&node.text)));
            }
            MdInlineKind::Bold => b.push_str(&format!("<b>{}</b>", inline_html(&node.children))),
            MdInlineKind::Italic => b.push_str(&format!("<i>{}</i>", inline_html(&node.children))),
            MdInlineKind::Strike => b.push_str(&format!("<s>{}</s>", inline_html(&node.children))),
            MdInlineKind::Underline => b.push_str(&format!("<u>{}</u>", inline_html(&node.children))),
            MdInlineKind::Spoiler => {
                b.push_str(&format!("<tg-spoiler>{}</tg-spoiler>", inline_html(&node.children)));
            }
            MdInlineKind::Link => b.push_str(&format!(
                r#"<a href="{}">{}</a>"#,
                escape_html(&node.url),
                inline_html(&node.children)
            )),
            MdInlineKind::Break => b.push('\n'),
            MdInlineKind::Marked | MdInlineKind::Sub | MdInlineKind::Sup | MdInlineKind::Span => {
                b.push_str(&inline_html(&node.children));
            }
            MdInlineKind::Image => {
                if node.url.is_empty() {
                    b.push_str(&escape_html(&node.text));
                } else {
                    let label = if node.text.is_empty() {
                        &node.url
                    } else {
                        &node.text
                    };
                    b.push_str(&format!(
                        r#"<a href="{}">{}</a>"#,
                        escape_html(&node.url),
                        escape_html(label)
                    ));
                }
            }
        }
    }
    b
}

pub fn markdown_rich(markdown: &str, fold_sections: bool) -> Vec<tl::enums::PageBlock> {
    let cleaned = cite_re().replace_all(markdown, "");
    let blocks = blocks_to_rich(&parse_markdown(&cleaned));
    if fold_sections {
        fold_rich_sections(blocks)
    } else {
        blocks
    }
}

fn blocks_to_rich(blocks: &[MdBlock]) -> Vec<tl::enums::PageBlock> {
    let mut out = Vec::new();
    for block in blocks {
        match block.kind {
            MdBlockKind::Rule => out.push(tl::enums::PageBlock::Divider),
            MdBlockKind::Heading => out.push(rich_heading(
                block.level,
                inline_rich(&parse_inline(&block.text, false)),
            )),
            MdBlockKind::Code => {
                if block.text.trim().is_empty() {
                    continue;
                }
                let lang = block.lang.trim().to_ascii_lowercase();
                if lang == "math" {
                    out.push(tl::types::PageBlockMath { source: block.text.clone() }.into());
                    continue;
                }
                let lang = if language_re().is_match(&lang) {
                    lang
                } else {
                    String::new()
                };
                out.push(
                    tl::types::PageBlockPreformatted {
                        text: rich_plain(&block.text),
                        language: lang,
                    }
                    .into(),
                );
            }
            MdBlockKind::MathBlock => {
                out.push(tl::types::PageBlockMath { source: block.text.clone() }.into());
            }
            MdBlockKind::Quote => {
                if block.html {
                    if block.lines.join("").trim().is_empty() {
                        continue;
                    }
                    out.push(
                        tl::types::PageBlockBlockquote {
                            collapsed: block.expandable,
                            text: rich_lines(&block.lines),
                            caption: tl::enums::RichText::TextEmpty,
                        }
                        .into(),
                    );
                } else {
                    let inner = blocks_to_rich(&parse_markdown(&quote_inner(&block.lines)));
                    if !inner.is_empty() {
                        out.push(
                            tl::types::PageBlockBlockquoteBlocks {
                                blocks: inner,
                                caption: tl::enums::RichText::TextEmpty,
                            }
                            .into(),
                        );
                    }
                }
            }
            MdBlockKind::List => out.extend(list_rich(&block.items)),
            MdBlockKind::Table => out.push(table_rich(block)),
            MdBlockKind::Paragraph => out.push(
                tl::types::PageBlockParagraph {
                    text: rich_lines(&block.lines),
                }
                .into(),
            ),
        }
    }
    out
}

fn rich_heading(level: usize, text: tl::enums::RichText) -> tl::enums::PageBlock {
    match level {
        1 => tl::types::PageBlockHeading1 { text }.into(),
        2 => tl::types::PageBlockHeading2 { text }.into(),
        3 => tl::types::PageBlockHeading3 { text }.into(),
        4 => tl::types::PageBlockHeading4 { text }.into(),
        5 => tl::types::PageBlockHeading5 { text }.into(),
        _ => tl::types::PageBlockHeading6 { text }.into(),
    }
}

pub(crate) fn rich_heading_of(block: &tl::enums::PageBlock) -> Option<(usize, &tl::enums::RichText)> {
    match block {
        tl::enums::PageBlock::Heading1(b) => Some((1, &b.text)),
        tl::enums::PageBlock::Heading2(b) => Some((2, &b.text)),
        tl::enums::PageBlock::Heading3(b) => Some((3, &b.text)),
        tl::enums::PageBlock::Heading4(b) => Some((4, &b.text)),
        tl::enums::PageBlock::Heading5(b) => Some((5, &b.text)),
        tl::enums::PageBlock::Heading6(b) => Some((6, &b.text)),
        _ => None,
    }
}

fn fold_rich_sections(blocks: Vec<tl::enums::PageBlock>) -> Vec<tl::enums::PageBlock> {
    let mut top = 0usize;
    for block in &blocks {
        if let Some((lvl, _)) = rich_heading_of(block)
            && (top == 0 || lvl < top)
        {
            top = lvl;
        }
    }
    if top == 0 {
        if blocks.is_empty() {
            return blocks;
        }
        return vec![
            tl::types::PageBlockDetails {
                open: false,
                blocks,
                title: rich_plain("展开全文"),
            }
            .into(),
        ];
    }
    let mut out = Vec::new();
    let mut heading: Option<tl::enums::PageBlock> = None;
    let mut section_title: Option<tl::enums::RichText> = None;
    let mut section_blocks: Vec<tl::enums::PageBlock> = Vec::new();

    let close_section = |out: &mut Vec<tl::enums::PageBlock>,
                         heading: &mut Option<tl::enums::PageBlock>,
                         title: &mut Option<tl::enums::RichText>,
                         sec_blocks: &mut Vec<tl::enums::PageBlock>| {
        if let Some(t) = title.take()
            && !sec_blocks.is_empty()
        {
            out.push(
                tl::types::PageBlockDetails {
                    open: false,
                    blocks: std::mem::take(sec_blocks),
                    title: t,
                }
                .into(),
            );
            *heading = None;
            return;
        }
        sec_blocks.clear();
        if let Some(h) = heading.take() {
            out.push(h);
        }
    };

    for block in blocks {
        if let Some((lvl, text)) = rich_heading_of(&block)
            && lvl <= top
        {
            close_section(&mut out, &mut heading, &mut section_title, &mut section_blocks);
            section_title = Some(text.clone());
            heading = Some(block);
            continue;
        }
        if section_title.is_none() {
            out.push(block);
        } else {
            section_blocks.push(block);
        }
    }
    close_section(&mut out, &mut heading, &mut section_title, &mut section_blocks);
    out
}

fn list_rich(items: &[MdItem]) -> Vec<tl::enums::PageBlock> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let mut end = start;
        while end < items.len() && items[end].ordered == items[start].ordered {
            end += 1;
        }
        let group = &items[start..end];
        if !group[0].ordered {
            let list_items = group
                .iter()
                .map(|it| {
                    tl::types::PageListItemBlocks {
                        checkbox: it.task,
                        checked: it.checked,
                        blocks: item_rich(it),
                    }
                    .into()
                })
                .collect();
            out.push(tl::types::PageBlockList { items: list_items }.into());
        } else {
            let list_items = group
                .iter()
                .map(|it| {
                    tl::types::PageListOrderedItemBlocks {
                        checkbox: it.task,
                        checked: it.checked,
                        num: None,
                        blocks: item_rich(it),
                        value: Some(it.number),
                        r#type: None,
                    }
                    .into()
                })
                .collect();
            out.push(
                tl::types::PageBlockOrderedList {
                    reversed: false,
                    items: list_items,
                    start: Some(group[0].number),
                    r#type: None,
                }
                .into(),
            );
        }
        start = end;
    }
    out
}

fn item_rich(item: &MdItem) -> Vec<tl::enums::PageBlock> {
    let mut blocks = vec![
        tl::types::PageBlockParagraph {
            text: rich_lines(&item.lines),
        }
        .into(),
    ];
    blocks.extend(list_rich(&item.children));
    blocks
}

fn table_rich(block: &MdBlock) -> tl::enums::PageBlock {
    if block.rows[0].len() > RICH_COLUMN_LIMIT {
        let lines: Vec<String> = block.rows.iter().map(|r| r.join(" | ")).collect();
        return tl::types::PageBlockParagraph {
            text: rich_lines(&lines),
        }
        .into();
    }
    let rows = block
        .rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| {
            let cells = row
                .iter()
                .enumerate()
                .map(|(col, text)| {
                    let al = block.align.get(col).copied().unwrap_or(0);
                    tl::types::PageTableCell {
                        header: row_idx == 0,
                        align_center: al == 1,
                        align_right: al == 2,
                        valign_middle: false,
                        valign_bottom: false,
                        text: Some(inline_rich(&parse_inline(text, false))),
                        colspan: None,
                        rowspan: None,
                    }
                    .into()
                })
                .collect();
            tl::types::PageTableRow { cells }.into()
        })
        .collect();
    tl::types::PageBlockTable {
        bordered: true,
        striped: false,
        compact: false,
        title: tl::enums::RichText::TextEmpty,
        rows,
    }
    .into()
}

fn rich_lines(lines: &[String]) -> tl::enums::RichText {
    let mut parts = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            parts.push(rich_plain("\n"));
        }
        let (indent, rest) = leading_indent(line);
        if !indent.is_empty() {
            parts.push(rich_plain(&indent));
        }
        parts.push(inline_rich(&parse_inline(rest, false)));
    }
    rich_concat(&parts)
}

fn inline_rich(nodes: &[MdInline]) -> tl::enums::RichText {
    let mut parts = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node.kind {
            MdInlineKind::Text => parts.push(rich_plain(&node.text)),
            MdInlineKind::CodeSpan => parts.push(
                tl::types::TextFixed {
                    text: rich_plain(&node.text),
                }
                .into(),
            ),
            MdInlineKind::Bold => parts.push(
                tl::types::TextBold {
                    text: inline_rich(&node.children),
                }
                .into(),
            ),
            MdInlineKind::Italic => parts.push(
                tl::types::TextItalic {
                    text: inline_rich(&node.children),
                }
                .into(),
            ),
            MdInlineKind::Strike => parts.push(
                tl::types::TextStrike {
                    text: inline_rich(&node.children),
                }
                .into(),
            ),
            MdInlineKind::Underline => parts.push(
                tl::types::TextUnderline {
                    text: inline_rich(&node.children),
                }
                .into(),
            ),
            MdInlineKind::Spoiler => parts.push(
                tl::types::TextSpoiler {
                    text: inline_rich(&node.children),
                }
                .into(),
            ),
            MdInlineKind::Marked => parts.push(
                tl::types::TextMarked {
                    text: inline_rich(&node.children),
                }
                .into(),
            ),
            MdInlineKind::Sub => parts.push(
                tl::types::TextSubscript {
                    text: inline_rich(&node.children),
                }
                .into(),
            ),
            MdInlineKind::Sup => parts.push(
                tl::types::TextSuperscript {
                    text: inline_rich(&node.children),
                }
                .into(),
            ),
            MdInlineKind::Span => parts.push(inline_rich(&node.children)),
            MdInlineKind::Math => parts.push(
                tl::types::TextMath {
                    source: node.text.clone(),
                }
                .into(),
            ),
            MdInlineKind::Break => parts.push(rich_plain("\n")),
            MdInlineKind::Link => parts.push(
                tl::types::TextUrl {
                    text: inline_rich(&node.children),
                    url: node.url.clone(),
                    webpage_id: 0,
                }
                .into(),
            ),
            MdInlineKind::Image => {
                if node.url.is_empty() {
                    parts.push(rich_plain(&node.text));
                } else {
                    let label = if node.text.is_empty() {
                        &node.url
                    } else {
                        &node.text
                    };
                    parts.push(
                        tl::types::TextUrl {
                            text: rich_plain(label),
                            url: node.url.clone(),
                            webpage_id: 0,
                        }
                        .into(),
                    );
                }
            }
        }
    }
    rich_concat(&parts)
}

pub(crate) fn telegraph_nodes(markdown: &str) -> Vec<serde_json::Value> {
    let cleaned = cite_re().replace_all(markdown, "");
    blocks_to_telegraph(&parse_markdown(&cleaned))
}

fn telegraph_node(tag: &str, children: Vec<serde_json::Value>) -> serde_json::Value {
    if children.is_empty() {
        json!({ "tag": tag })
    } else {
        json!({ "tag": tag, "children": children })
    }
}

fn blocks_to_telegraph(blocks: &[MdBlock]) -> Vec<serde_json::Value> {
    let mut nodes = Vec::new();
    for block in blocks {
        match block.kind {
            MdBlockKind::Rule => nodes.push(telegraph_node("hr", Vec::new())),
            MdBlockKind::Heading => {
                let tag = if block.level <= 2 { "h3" } else { "h4" };
                nodes.push(telegraph_node(tag, inline_telegraph(&parse_inline(&block.text, false))));
            }
            MdBlockKind::Code | MdBlockKind::MathBlock => {
                nodes.push(telegraph_node(
                    "pre",
                    vec![telegraph_node("code", vec![json!(block.text)])],
                ));
            }
            MdBlockKind::Quote => {
                if block.html {
                    nodes.push(telegraph_node(
                        "blockquote",
                        vec![telegraph_node("p", lines_telegraph(&block.lines))],
                    ));
                } else {
                    nodes.push(telegraph_node(
                        "blockquote",
                        blocks_to_telegraph(&parse_markdown(&quote_inner(&block.lines))),
                    ));
                }
            }
            MdBlockKind::List => nodes.extend(list_telegraph(&block.items)),
            MdBlockKind::Table => {
                let mut children = Vec::new();
                for (row_idx, row) in block.rows.iter().enumerate() {
                    if row_idx > 0 {
                        children.push(telegraph_node("br", Vec::new()));
                    }
                    let mut cells = Vec::new();
                    for (col, cell) in row.iter().enumerate() {
                        if col > 0 {
                            cells.push(json!(" | "));
                        }
                        cells.extend(inline_telegraph(&parse_inline(cell, false)));
                    }
                    if row_idx == 0 {
                        cells = vec![telegraph_node("strong", compact_telegraph(cells))];
                    }
                    children.extend(cells);
                }
                nodes.push(telegraph_node("p", compact_telegraph(children)));
            }
            MdBlockKind::Paragraph => {
                nodes.push(telegraph_node("p", lines_telegraph(&block.lines)));
            }
        }
    }
    nodes
}

fn lines_telegraph(lines: &[String]) -> Vec<serde_json::Value> {
    let mut children = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            children.push(telegraph_node("br", Vec::new()));
        }
        let (indent, rest) = leading_indent(line);
        if !indent.is_empty() {
            children.push(json!(indent));
        }
        children.extend(inline_telegraph(&parse_inline(rest, false)));
    }
    compact_telegraph(children)
}

fn list_telegraph(items: &[MdItem]) -> Vec<serde_json::Value> {
    let mut nodes = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let mut end = start;
        while end < items.len() && items[end].ordered == items[start].ordered {
            end += 1;
        }
        let tag = if items[start].ordered { "ol" } else { "ul" };
        let mut children = Vec::new();
        for item in &items[start..end] {
            let mut content = Vec::new();
            if item.task {
                content.push(json!(if item.checked { "☑ " } else { "☐ " }));
            }
            for (idx, line) in item.lines.iter().enumerate() {
                if idx > 0 {
                    content.push(telegraph_node("br", Vec::new()));
                }
                content.extend(inline_telegraph(&parse_inline(line, false)));
            }
            content.extend(list_telegraph(&item.children));
            children.push(telegraph_node("li", compact_telegraph(content)));
        }
        nodes.push(telegraph_node(tag, children));
        start = end;
    }
    nodes
}

fn inline_telegraph(nodes: &[MdInline]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for node in nodes {
        match node.kind {
            MdInlineKind::Text => out.push(json!(node.text)),
            MdInlineKind::CodeSpan | MdInlineKind::Math => {
                out.push(telegraph_node("code", vec![json!(node.text)]));
            }
            MdInlineKind::Bold => {
                out.push(telegraph_node("strong", inline_telegraph(&node.children)));
            }
            MdInlineKind::Italic => {
                out.push(telegraph_node("em", inline_telegraph(&node.children)));
            }
            MdInlineKind::Strike => {
                out.push(telegraph_node("s", inline_telegraph(&node.children)));
            }
            MdInlineKind::Underline => {
                out.push(telegraph_node("u", inline_telegraph(&node.children)));
            }
            MdInlineKind::Spoiler
            | MdInlineKind::Marked
            | MdInlineKind::Sub
            | MdInlineKind::Sup
            | MdInlineKind::Span => out.extend(inline_telegraph(&node.children)),
            MdInlineKind::Break => out.push(telegraph_node("br", Vec::new())),
            MdInlineKind::Link => {
                let mut link = telegraph_node("a", inline_telegraph(&node.children));
                if let Some(obj) = link.as_object_mut() {
                    obj.insert("attrs".into(), json!({ "href": node.url }));
                }
                out.push(link);
            }
            MdInlineKind::Image => {
                if node.url.is_empty() {
                    out.push(json!(node.text));
                    continue;
                }
                let image = json!({ "tag": "img", "attrs": { "src": node.url } });
                if node.text.trim().is_empty() {
                    out.push(image);
                    continue;
                }
                out.push(telegraph_node(
                    "figure",
                    vec![
                        image,
                        telegraph_node("figcaption", inline_telegraph(&parse_inline(&node.text, true))),
                    ],
                ));
            }
        }
    }
    compact_telegraph(out)
}

fn compact_telegraph(nodes: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    for node in nodes {
        if let Some(text) = node.as_str() {
            if text.is_empty() {
                continue;
            }
            if let Some(last) = out.last_mut().and_then(|v| v.as_str().map(str::to_owned)) {
                *out.last_mut().unwrap() = json!(format!("{last}{text}"));
                continue;
            }
        }
        out.push(node);
    }
    out
}

#[derive(Clone)]
struct OpenTag {
    name: String,
    raw: String,
}

#[derive(Clone)]
struct BreakPoint {
    offset: usize,
    visible: usize,
    stack: Vec<OpenTag>,
}

pub fn html_pages(text: &str, limit: usize) -> Vec<String> {
    let mut pages = Vec::new();
    let mut stack: Vec<OpenTag> = Vec::new();
    let mut page = String::new();
    let mut visible = 0usize;
    let mut last: Option<BreakPoint> = None;

    let closing = |tags: &[OpenTag]| -> String {
        tags.iter()
            .rev()
            .map(|t| format!("</{}>", t.name))
            .collect()
    };
    let opening = |tags: &[OpenTag]| -> String {
        tags.iter().map(|t| t.raw.as_str()).collect()
    };

    let cut = |pages: &mut Vec<String>,
               page: &mut String,
               visible: &mut usize,
               last: &mut Option<BreakPoint>,
               stack: &[OpenTag]| {
        let current = page.clone();
        if let Some(bp) = last.take()
            && bp.visible * 2 >= limit
        {
            let head = current[..bp.offset].strip_suffix('\n').unwrap_or(&current[..bp.offset]);
            let tail = &current[bp.offset..];
            pages.push(format!("{head}{}", closing(&bp.stack)));
            page.clear();
            page.push_str(&opening(&bp.stack));
            page.push_str(tail);
            *visible -= bp.visible;
        } else {
            pages.push(format!("{current}{}", closing(stack)));
            page.clear();
            page.push_str(&opening(stack));
            *visible = 0;
        }
    };

    let add = |token: &str,
               width: usize,
               pages: &mut Vec<String>,
               page: &mut String,
               visible: &mut usize,
               last: &mut Option<BreakPoint>,
               stack: &[OpenTag]| {
        if *visible > 0 && *visible + width > limit {
            cut(pages, page, visible, last, stack);
        }
        page.push_str(token);
        *visible += width;
        if token == "\n" {
            *last = Some(BreakPoint {
                offset: page.len(),
                visible: *visible,
                stack: stack.to_vec(),
            });
        }
    };

    let mut index = 0usize;
    while index < text.len() {
        let rest = &text[index..];
        match rest.as_bytes()[0] {
            b'<' => {
                if let Some(end) = rest.find('>') {
                    let token = &rest[..=end];
                    index += end + 1;
                    if token.starts_with("</") {
                        stack.pop();
                        page.push_str(token);
                    } else {
                        let inner = token.trim_matches(['<', '>']);
                        let name = inner.split(' ').next().unwrap_or("").to_ascii_lowercase();
                        stack.push(OpenTag {
                            name,
                            raw: token.to_owned(),
                        });
                        page.push_str(token);
                    }
                } else {
                    add("&lt;", 1, &mut pages, &mut page, &mut visible, &mut last, &stack);
                    index += 1;
                }
            }
            b'&' => {
                if let Some(end) = rest.find(';')
                    && end <= 12
                {
                    let ent = &rest[..=end];
                    add(ent, 1, &mut pages, &mut page, &mut visible, &mut last, &stack);
                    index += end + 1;
                } else {
                    add("&amp;", 1, &mut pages, &mut page, &mut visible, &mut last, &stack);
                    index += 1;
                }
            }
            _ => {
                let ch = rest.chars().next().unwrap();
                add(
                    &rest[..ch.len_utf8()],
                    ch.len_utf16(),
                    &mut pages,
                    &mut page,
                    &mut visible,
                    &mut last,
                    &stack,
                );
                index += ch.len_utf8();
            }
        }
    }
    if visible > 0 || pages.is_empty() {
        pages.push(format!("{page}{}", closing(&stack)));
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiSource, answer_pages, answer_rich};
    use mibot_bot::{parse_html, rich_fits};

    #[test]
    fn test_markdown_to_html_and_styles() {
        let cases = [
            ("a < b & c > \"d\"", "a &lt; b &amp; c &gt; &quot;d&quot;"),
            ("**粗** 和 *斜* 以及 _斜_", "<b>粗</b> 和 <i>斜</i> 以及 <i>斜</i>"),
            ("***都有***", "<b><i>都有</i></b>"),
            ("~~删~~ __下__ ||藏||", "<s>删</s> <u>下</u> <tg-spoiler>藏</tg-spoiler>"),
            ("用 `a*b*<c>` 看", "用 <code>a*b*&lt;c&gt;</code> 看"),
            ("snake_case_name 和 2 * 3 * 4", "snake_case_name 和 2 * 3 * 4"),
            ("[文档](https://example.com/a?b=1&c=2)", r#"<a href="https://example.com/a?b=1&amp;c=2">文档</a>"#),
            ("[点我](javascript:alert(1))", "点我)"),
            ("见 https://example.com/x。然后", r#"见 <a href="https://example.com/x">https://example.com/x</a>。然后"#),
            ("see https://example.com.", r#"see <a href="https://example.com">https://example.com</a>."#),
            ("# 一\n## 二\n### 三\n#### 四", "<b>一</b>\n\n<b>▌二</b>\n\n<b>• 三</b>\n\n<b>四</b>"),
            ("```Go\nfmt.Println(\"<x>\")\n```", r#"<pre><code class="language-go">fmt.Println(&quot;&lt;x&gt;&quot;)</code></pre>"#),
            ("```\na & b\n```", "<pre><code>a &amp; b</code></pre>"),
            ("```py\nprint(1)", r#"<pre><code class="language-py">print(1)</code></pre>"#),
            ("- 一\n- 二\n  - 二点一\n    - 深\n      - 更深", "• 一\n• 二\n\u{00a0}\u{00a0}• 二点一\n\u{00a0}\u{00a0}\u{00a0}\u{00a0}• 深\n\u{00a0}\u{00a0}\u{00a0}\u{00a0}\u{00a0}\u{00a0}更深"),
            ("1. **甲**\n   说明\n2. 乙", "1. <b>甲</b>\n\u{00a0}\u{00a0}说明\n2. 乙"),
            ("> 引用 **重点**\n> > 再深", "<blockquote>引用 <b>重点</b>\n┃ 再深</blockquote>"),
            ("上\n\n---\n\n下", "上\n\n────────────────\n\n下"),
            ("第一行\n  缩进行", "第一行\n\u{00a0}\u{00a0}缩进行"),
            ("结论<cite>1</cite>", "结论1"),
            ("| A | B |\n|:--|--:|\n| `x` | **y** |", "<b>A | B</b>\n<code>x</code> | <b>y</b>"),
            ("表：\n| a | b |\n|---|---|\n| 1 |", "表：\n\n<b>a | b</b>\n1 | "),
            ("$$\nE = mc^2\n$$", r#"<pre><code class="language-latex">E = mc^2</code></pre>"#),
            ("面积 $\\pi r^2$，价钱 $5 和 $10", "面积 <code>\\pi r^2</code>，价钱 $5 和 $10"),
            (r"设 \(a+b\) 和 \[c\]", "设 <code>a+b</code> 和 <code>c</code>"),
            (r"\*不是斜体\* 和 \$5", "*不是斜体* 和 $5"),
            ("- [ ] 待办\n- [x] 完成", "• ☐ 待办\n• ☑ 完成"),
            (
                r#"<b>粗</b> <a href="https://e.com/?a=1&amp;b=2">链</a> <code>a &lt; b</code> <span class="tg-spoiler">藏</span> <x>未知</x>"#,
                r#"<b>粗</b> <a href="https://e.com/?a=1&amp;b=2">链</a> <code>a &lt; b</code> <tg-spoiler>藏</tg-spoiler> &lt;x&gt;未知&lt;/x&gt;"#,
            ),
            (r#"<a href="javascript:x">文字</a>"#, "文字"),
            ("<u>下 **粗**</u><br>换行", "<u>下 <b>粗</b></u>\n换行"),
            ("a &amp; b &lt;c&gt; &bogus;", "a &amp; b &lt;c&gt; &amp;bogus;"),
            (
                "<b>标题</b>\n<blockquote expandable>• 一 <a href=\"https://t.me/c/1/2\">来源</a>\n• 二</blockquote>\n尾",
                "<b>标题</b>\n\n<blockquote expandable>• 一 <a href=\"https://t.me/c/1/2\">来源</a>\n• 二</blockquote>\n\n尾",
            ),
        ];
        for (md, want) in cases {
            assert_eq!(markdown_to_html(md, false), want, "md={md:?}");
        }
        assert_eq!(markdown_to_html("> 引用", true), "引用");

        let sum_src = "### 💬 话题\n- 一\n- 二";
        assert_eq!(
            markdown_html(
                sum_src,
                HtmlStyle {
                    plain_headings: true,
                    fold_lists: true,
                    collapse_safe: false
                }
            ),
            "<b>💬 话题</b>\n\n<blockquote expandable>• 一\n• 二</blockquote>"
        );
        assert_eq!(
            markdown_html(
                sum_src,
                HtmlStyle {
                    plain_headings: true,
                    fold_lists: true,
                    collapse_safe: true
                }
            ),
            "<b>💬 话题</b>\n\n• 一\n• 二"
        );

        let complex = "# 标题\n\n**粗 [链接](https://example.com) `code`**\n\n```js\nlet a = 1 < 2\n```\n\n> 引用\n\n- 列表 ||剧透||\n\n| **A** | `B` |\n|---|---|\n| 1 | $x$ |\n\n<blockquote expandable><i>折</i></blockquote>";
        let rendered = markdown_to_html(complex, false);
        let (plain, ents) = parse_html(&rendered, None).unwrap();
        assert!(!plain.contains("<b>") && !plain.contains("**") && ents.len() >= 6);
    }

    #[test]
    fn test_html_pages_and_answer_and_telegraph() {
        let code = format!(
            r#"<pre><code class="language-go">{}{}{}</code></pre>"#,
            "x".repeat(30),
            "\n",
            "y".repeat(30)
        );
        let pages = html_pages(&format!("<b>头</b>\n{code}"), 40);
        assert_eq!(pages.len(), 2);
        assert!(pages[0].ends_with("</code></pre>"));
        assert!(pages[1].starts_with(r#"<pre><code class="language-go">"#));
        assert!(!pages[0].contains('y') && !pages[1].contains('x'));
        for p in &pages {
            parse_html(p, None).unwrap();
        }
        let long = html_pages(&format!("<i>{}&amp;</i>", "字".repeat(95)), 32);
        assert_eq!(long.len(), 3);
        assert_eq!(html_pages("", 10), vec![""]);

        let ap = answer_pages("问<题>", "<b>答</b>", "gpt-5.1", true);
        assert_eq!(
            ap,
            vec![
                "问：\n<blockquote expandable>问&lt;题&gt;</blockquote>\n答：\n<blockquote expandable><b>答</b></blockquote>\n<i>🍀 由 gpt-5.1 生成</i>"
            ]
        );
        let plain_ap = answer_pages("q", "a", "", false);
        assert_eq!(plain_ap, vec!["问：\nq\n\n答：\na"]);

        let nodes = telegraph_nodes(
            "## 标题\n\n**粗** [链](https://e.com)\n第二行\n\n```go\nx := 1\n```\n\n> 引用\n\n1. 甲\n   - 子\n2. 乙\n\n---",
        );
        let raw = serde_json::to_string(&nodes).unwrap();
        let want = r#"[{"children":["标题"],"tag":"h3"},{"children":[{"children":["粗"],"tag":"strong"}," ",{"attrs":{"href":"https://e.com"},"children":["链"],"tag":"a"},{"tag":"br"},"第二行"],"tag":"p"},{"children":[{"children":["x := 1"],"tag":"code"}],"tag":"pre"},{"children":[{"children":["引用"],"tag":"p"}],"tag":"blockquote"},{"children":[{"children":["甲",{"children":[{"children":["子"],"tag":"li"}],"tag":"ul"}],"tag":"li"},{"children":["乙"],"tag":"li"}],"tag":"ol"},{"tag":"hr"}]"#;
        assert_eq!(raw, want);
    }

    #[test]
    fn test_markdown_rich_and_answer_rich() {
        let source = "# 标题\n段落 **粗** [链](https://e.com) $x^2$\n第二行\n\n- 一\n  - 一点一\n- [x] 完成\n\n3. 丙\n4. 丁\n\n| A | B |\n|:-:|--:|\n| 1 | 2 |\n\n> 引用 *斜*\n\n<blockquote expandable>折叠</blockquote>\n\n```go\nx := 1\n```\n\n$$\nE=mc^2\n$$\n\n---";
        let blocks = markdown_rich(source, false);
        assert_eq!(blocks.len(), 10);

        let folded = markdown_rich("前言\n\n## 甲\n- 一\n\n### 子\n内容\n\n## 乙\n内容", true);
        assert_eq!(folded.len(), 3);
        assert!(matches!(folded[0], tl::enums::PageBlock::Paragraph(_)));
        assert!(matches!(folded[1], tl::enums::PageBlock::Details(_)));
        assert!(matches!(folded[2], tl::enums::PageBlock::Details(_)));

        let whole = markdown_rich("只有一段", true);
        assert_eq!(whole.len(), 1);
        assert!(matches!(whole[0], tl::enums::PageBlock::Details(_)));

        let sources = vec![
            AiSource {
                url: "https://e.com/a".into(),
                title: "甲".into(),
            },
            AiSource {
                url: "https://e.com/b".into(),
                title: String::new(),
            },
        ];
        let rich = answer_rich("问<题>", "**答**", &sources, "gpt-5.1", false);
        assert_eq!(rich.len(), 6);
        assert!(matches!(rich[5], tl::enums::PageBlock::Footer(_)));
        assert!(rich_fits(&rich));

        let long_rich = answer_rich("q", &format!("## 头\n\n{}\n\n第二段\n\n第三段", "长".repeat(400)), &[], "m", true);
        assert_eq!(long_rich.len(), 7);
        assert!(matches!(long_rich[5], tl::enums::PageBlock::Details(_)));
    }
}
