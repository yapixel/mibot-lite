use crate::peers::PeerCache;
use crate::BotError;
use grammers_tl_types as tl;

const MENTION_PREFIX: &str = "tg://user?id=";

/// Counts Telegram UTF-16 code units in `text`.
pub fn utf16_len(text: &str) -> i32 {
    text.encode_utf16().count() as i32
}

pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn code(value: &str) -> String {
    format!("<code>{}</code>", escape(value))
}

pub fn bold(value: &str) -> String {
    format!("<b>{}</b>", escape(value))
}

#[derive(Clone)]
struct OpenTag {
    tag_name: String,
    entity_idx: Option<usize>,
}

/// Panic-free Telegram HTML parser matching `gotd`'s `html.HTML` and `mibot-lite`'s safe mention rule:
/// `<a href="tg://user?id=...">` produces a mention entity only when `peers` holds a non-zero `access_hash`
/// for that user; otherwise plain text is emitted with no entity.
pub fn parse_html(
    input: &str,
    peers: Option<&PeerCache>,
) -> Result<(String, Vec<tl::enums::MessageEntity>), BotError> {
    let mut text = String::with_capacity(input.len());
    let mut entities: Vec<tl::enums::MessageEntity> = Vec::new();
    let mut stack: Vec<OpenTag> = Vec::new();
    let mut offset: i32 = 0;

    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && let Some((tag_raw, next_idx)) = read_tag(input, i)
        {
            i = next_idx;
            let trimmed = tag_raw.trim();
            if let Some(closing_body) = trimmed.strip_prefix('/') {
                let close_name = closing_body.trim().to_ascii_lowercase();
                if let Some(pos) = stack.iter().rposition(|t| t.tag_name == close_name) {
                    while stack.len() > pos {
                        if let Some(open) = stack.pop()
                            && let Some(idx) = open.entity_idx
                        {
                            finish_entity(&mut entities[idx], offset);
                        }
                    }
                }
            } else if !trimmed.starts_with('!') && !trimmed.starts_with('?') {
                let self_closing = trimmed.ends_with('/');
                let body = trimmed.strip_suffix('/').unwrap_or(trimmed).trim();
                let (name, attrs) = split_tag_attrs(body);
                let name_lower = name.to_ascii_lowercase();
                let entity_idx = open_entity_for_tag(
                    &name_lower,
                    attrs,
                    offset,
                    &mut entities,
                    peers,
                );
                if !self_closing {
                    stack.push(OpenTag {
                        tag_name: name_lower,
                        entity_idx,
                    });
                } else if let Some(idx) = entity_idx {
                    finish_entity(&mut entities[idx], offset);
                }
            }
            continue;
        }

        if bytes[i] == b'&'
            && let Some((decoded, next_idx)) = decode_entity(input, i)
        {
            offset += utf16_len(&decoded);
            text.push_str(&decoded);
            i = next_idx;
            continue;
        }

        let next_special = input[i..]
            .find(['<', '&'])
            .map_or(bytes.len(), |rel| i + rel);
        let chunk = if next_special == i {
            // Unclosed '<' or '&' at index i: advance one UTF-8 char.
            let ch_len = input[i..].chars().next().map_or(1, char::len_utf8);
            &input[i..i + ch_len]
        } else {
            &input[i..next_special]
        };
        offset += utf16_len(chunk);
        text.push_str(chunk);
        i += chunk.len();
    }

    while let Some(open) = stack.pop() {
        if let Some(idx) = open.entity_idx {
            finish_entity(&mut entities[idx], offset);
        }
    }

    entities.retain(|e| e.length() > 0);
    Ok((text, entities))
}

fn read_tag(input: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = input.as_bytes();
    let after = *bytes.get(start + 1)?;
    if !(after.is_ascii_alphabetic() || matches!(after, b'/' | b'!' | b'?')) {
        return None;
    }
    let mut quote: Option<u8> = None;
    let mut idx = start + 1;
    while idx < bytes.len() {
        let b = bytes[idx];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'>' {
                    return Some((&input[start + 1..idx], idx + 1));
                }
            }
        }
        idx += 1;
    }
    None
}

fn split_tag_attrs(body: &str) -> (&str, &str) {
    match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], body[idx..].trim()),
        None => (body, ""),
    }
}

fn extract_attr(attrs: &str, target: &str) -> Option<(bool, String)> {
    let mut rest = attrs.trim();
    while !rest.is_empty() {
        let key_end = rest
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(rest.len());
        let key = &rest[..key_end];
        rest = rest[key_end..].trim_start();
        let val = if let Some(after_eq) = rest.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            if let Some(q) = after_eq.chars().next().filter(|&c| c == '"' || c == '\'') {
                let inner = &after_eq[1..];
                if let Some(end) = inner.find(q) {
                    let raw = &inner[..end];
                    rest = inner[end + 1..].trim_start();
                    unescape_html(raw)
                } else {
                    rest = "";
                    unescape_html(inner)
                }
            } else {
                let end = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
                let raw = &after_eq[..end];
                rest = after_eq[end..].trim_start();
                unescape_html(raw)
            }
        } else {
            String::new()
        };
        if key.eq_ignore_ascii_case(target) {
            return Some((true, val));
        }
    }
    None
}

fn open_entity_for_tag(
    name: &str,
    attrs: &str,
    offset: i32,
    entities: &mut Vec<tl::enums::MessageEntity>,
    peers: Option<&PeerCache>,
) -> Option<usize> {
    let length = 0;
    let entity: tl::enums::MessageEntity = match name {
        "b" | "strong" => tl::types::MessageEntityBold { offset, length }.into(),
        "i" | "em" => tl::types::MessageEntityItalic { offset, length }.into(),
        "u" | "ins" => tl::types::MessageEntityUnderline { offset, length }.into(),
        "s" | "strike" | "del" => tl::types::MessageEntityStrike { offset, length }.into(),
        "tg-spoiler" => tl::types::MessageEntitySpoiler { offset, length }.into(),
        "span" => {
            if extract_attr(attrs, "class").is_some_and(|(_, v)| v.contains("tg-spoiler")) {
                tl::types::MessageEntitySpoiler { offset, length }.into()
            } else {
                return None;
            }
        }
        "blockquote" => {
            let collapsed = extract_attr(attrs, "expandable").is_some();
            tl::types::MessageEntityBlockquote {
                offset,
                length,
                collapsed,
            }
            .into()
        }
        "pre" => {
            let language = extract_attr(attrs, "language")
                .map(|(_, v)| v)
                .unwrap_or_default();
            tl::types::MessageEntityPre {
                offset,
                length,
                language,
            }
            .into()
        }
        "code" => {
            if let Some(tl::enums::MessageEntity::Pre(pre)) = entities.last_mut()
                && pre.length == 0
                && pre.language.is_empty()
            {
                if let Some((_, cls)) = extract_attr(attrs, "class")
                    && let Some(lang) = cls.strip_prefix("language-")
                {
                    pre.language = lang.to_owned();
                }
                return None;
            }
            tl::types::MessageEntityCode { offset, length }.into()
        }
        "a" => {
            let url = extract_attr(attrs, "href")
                .map(|(_, v)| v)
                .unwrap_or_default();
            if let Some(id_str) = url.strip_prefix(MENTION_PREFIX) {
                let user_id = id_str.parse::<i64>().ok()?;
                let peer_cache = peers?;
                let info = peer_cache.user(user_id).filter(|u| u.has_hash)?;
                tl::types::InputMessageEntityMentionName {
                    offset,
                    length,
                    user_id: tl::types::InputUser {
                        user_id: info.id,
                        access_hash: info.hash,
                    }
                    .into(),
                }
                .into()
            } else if !url.is_empty() {
                tl::types::MessageEntityTextUrl {
                    offset,
                    length,
                    url,
                }
                .into()
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let idx = entities.len();
    entities.push(entity);
    Some(idx)
}

fn finish_entity(entity: &mut tl::enums::MessageEntity, end_offset: i32) {
    let len = (end_offset - entity.offset()).max(0);
    match entity {
        tl::enums::MessageEntity::Bold(e) => e.length = len,
        tl::enums::MessageEntity::Italic(e) => e.length = len,
        tl::enums::MessageEntity::Underline(e) => e.length = len,
        tl::enums::MessageEntity::Strike(e) => e.length = len,
        tl::enums::MessageEntity::Code(e) => e.length = len,
        tl::enums::MessageEntity::Pre(e) => e.length = len,
        tl::enums::MessageEntity::TextUrl(e) => e.length = len,
        tl::enums::MessageEntity::InputMessageEntityMentionName(e) => e.length = len,
        tl::enums::MessageEntity::Spoiler(e) => e.length = len,
        tl::enums::MessageEntity::Blockquote(e) => e.length = len,
        _ => {}
    }
}

fn unescape_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some((decoded, next)) = decode_entity(raw, i)
        {
            out.push_str(&decoded);
            i = next;
            continue;
        }
        let ch = raw[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_entity(input: &str, start: usize) -> Option<(String, usize)> {
    let rest = &input[start + 1..];
    let semi = rest.find(';')?;
    if semi == 0 || semi > 16 {
        return None;
    }
    let body = &rest[..semi];
    let decoded = match body {
        "amp" => "&".to_owned(),
        "lt" => "<".to_owned(),
        "gt" => ">".to_owned(),
        "quot" => "\"".to_owned(),
        "apos" => "'".to_owned(),
        "nbsp" => "\u{00a0}".to_owned(),
        _ if body.starts_with("#x") || body.starts_with("#X") => {
            let code = u32::from_str_radix(&body[2..], 16).ok()?;
            char::from_u32(code)?.to_string()
        }
        _ if body.starts_with('#') => {
            let code = body[1..].parse::<u32>().ok()?;
            char::from_u32(code)?.to_string()
        }
        _ => return None,
    };
    Some((decoded, start + 1 + semi + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_and_parse_basic_and_expandable_blockquote() {
        assert_eq!(escape("<a & b>"), "&lt;a &amp; b&gt;");
        let (text, entities) = parse_html("<b>bold</b> plain <code>x</code>", None).unwrap();
        assert_eq!(text, "bold plain x");
        assert_eq!(entities.len(), 2);

        let (qtext, qents) = parse_html("<blockquote expandable>long</blockquote>", None).unwrap();
        assert_eq!(qtext, "long");
        assert_eq!(qents.len(), 1);
        assert!(matches!(
            &qents[0],
            tl::enums::MessageEntity::Blockquote(b) if b.collapsed && b.offset == 0 && b.length == 4
        ));
    }

    #[test]
    fn malformed_mention_does_not_panic_and_uncached_user_drops_entity() {
        let (t1, e1) = parse_html(r#"<a href="tg://user?id=notanumber">Bob</a>"#, None).unwrap();
        assert_eq!(t1, "Bob");
        assert!(e1.is_empty());

        let (t2, e2) = parse_html(r#"<a href="tg://user?id=42">Bob</a>"#, None).unwrap();
        assert_eq!(t2, "Bob");
        assert!(e2.is_empty());

        let peers = PeerCache::new();
        peers.remember_users(&[tl::types::User {
            is_self: false,
            contact: false,
            mutual_contact: false,
            deleted: false,
            bot: false,
            bot_chat_history: false,
            bot_nochats: false,
            verified: false,
            restricted: false,
            min: false,
            bot_inline_geo: false,
            support: false,
            scam: false,
            apply_min_photo: false,
            fake: false,
            bot_attach_menu: false,
            premium: false,
            attach_menu_enabled: false,
            bot_can_edit: false,
            close_friend: false,
            stories_hidden: false,
            stories_unavailable: false,
            contact_require_premium: false,
            bot_business: false,
            bot_has_main_app: false,
            bot_forum_view: false,
            bot_forum_can_manage_topics: false,
            bot_can_manage_bots: false,
            bot_guestchat: false,
            bot_guard: false,
            bot_forum_sidebar: false,
            id: 42,
            access_hash: Some(999),
            first_name: None,
            last_name: None,
            username: None,
            phone: None,
            photo: None,
            status: None,
            bot_info_version: None,
            restriction_reason: None,
            bot_inline_placeholder: None,
            lang_code: None,
            emoji_status: None,
            usernames: None,
            stories_max_id: None,
            color: None,
            profile_color: None,
            bot_active_users: None,
            bot_verification_icon: None,
            send_paid_messages_stars: None,
            linked_community_id: None,
        }
        .into()]);
        let (t3, e3) = parse_html(r#"<a href="tg://user?id=42">😀 Bob</a>"#, Some(&peers)).unwrap();
        assert_eq!(t3, "😀 Bob");
        assert_eq!(e3.len(), 1);
        assert_eq!(e3[0].offset(), 0);
        assert_eq!(e3[0].length(), 6);
    }
}
