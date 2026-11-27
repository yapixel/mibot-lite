use crate::html::utf16_len;
use crate::rich::{rich_plain, rich_plain_text};
use grammers_tl_types as tl;
use regex::Regex;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::{OnceLock, RwLock};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
#[error("invalid IP privacy policy: IPv4 segments 1–4, IPv6 segments 1–8")]
pub struct InvalidIpPolicyError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpPolicy {
    /// `"mask"` or `"hide"`.
    pub mode: String,
    pub ipv4_segments: u8,
    pub ipv6_segments: u8,
}

impl Default for IpPolicy {
    fn default() -> Self {
        Self {
            mode: "mask".into(),
            ipv4_segments: 2,
            ipv6_segments: 4,
        }
    }
}

impl IpPolicy {
    pub fn validate(&self) -> Result<(), InvalidIpPolicyError> {
        if (self.mode != "mask" && self.mode != "hide")
            || !(1..=4).contains(&self.ipv4_segments)
            || !(1..=8).contains(&self.ipv6_segments)
        {
            return Err(InvalidIpPolicyError);
        }
        Ok(())
    }
}

static IP_POLICY: OnceLock<RwLock<IpPolicy>> = OnceLock::new();

fn policy_lock() -> &'static RwLock<IpPolicy> {
    IP_POLICY.get_or_init(|| RwLock::new(IpPolicy::default()))
}

pub fn set_ip_policy(policy: IpPolicy) -> Result<(), InvalidIpPolicyError> {
    policy.validate()?;
    *policy_lock().write().unwrap_or_else(|e| e.into_inner()) = policy;
    Ok(())
}

pub fn current_ip_policy() -> IpPolicy {
    policy_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

tokio::task_local! {
    static IP_PRIVACY_OFF: bool;
}

/// Exempts outgoing sends inside `fut` from `.privacy` IP masking
/// (used by `.sudo`/`.sure` and `--verify`).
pub async fn without_ip_privacy<F, Fut, T>(fut: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    IP_PRIVACY_OFF.scope(true, fut()).await
}

pub async fn with_optional_ip_privacy_off<F, Fut, T>(off: bool, fut: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    if off {
        IP_PRIVACY_OFF.scope(true, fut()).await
    } else {
        fut().await
    }
}

pub fn is_ip_privacy_off() -> bool {
    IP_PRIVACY_OFF.try_with(|&v| v).unwrap_or(false)
}

struct IpPatterns {
    ipv6: Regex,
    ipv4: Regex,
}

fn patterns() -> &'static IpPatterns {
    static PATS: OnceLock<IpPatterns> = OnceLock::new();
    PATS.get_or_init(|| IpPatterns {
        ipv6: Regex::new(r"(?i)(?:[a-f\d]{0,4}:){2,}[a-f\d:.]*(?:%[\w.-]+)?")
            .expect("valid ipv6 pattern"),
        ipv4: Regex::new(r"(?:\d{1,3}\.){3}\d{1,3}").expect("valid ipv4 pattern"),
    })
}

struct IpReplacement {
    start: usize,
    end: usize,
    value: String,
}

fn word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn parse_addr_with_zone(address: &str, version: u8) -> bool {
    if version == 4 {
        return Ipv4Addr::from_str(address).is_ok();
    }
    match address.split_once('%') {
        Some((base, zone)) => !zone.is_empty() && Ipv6Addr::from_str(base).is_ok(),
        None => Ipv6Addr::from_str(address).is_ok(),
    }
}

fn ip_replacements(text: &str, policy: &IpPolicy) -> Vec<IpReplacement> {
    let pats = patterns();
    let mut found: Vec<IpReplacement> = Vec::new();
    let overlaps = |items: &[IpReplacement], start: usize, end: usize| {
        items.iter().any(|it| start < it.end && end > it.start)
    };

    let mut scan = |re: &Regex, before: fn(u8) -> bool, after: fn(&[u8]) -> bool, version: u8| {
        let bytes = text.as_bytes();
        let mut pos = 0usize;
        while pos < text.len() {
            let Some(m) = re.find(&text[pos..]) else {
                return;
            };
            let start = pos + m.start();
            let raw_end = pos + m.end();
            let address = text[start..raw_end].trim_end_matches('.');
            let end = start + address.len();
            let valid_boundary = !address.is_empty()
                && (start == 0 || !before(bytes[start - 1]))
                && after(&bytes[end..]);
            let valid = valid_boundary && parse_addr_with_zone(address, version);
            if !valid || overlaps(&found, start, end) {
                let step = text[start..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8);
                pos = start + step;
                continue;
            }
            found.push(IpReplacement {
                start,
                end,
                value: masked_address(address, version, policy),
            });
            pos = end;
        }
    };

    scan(
        &pats.ipv6,
        |b| word_byte(b) || b == b':',
        |rest| rest.is_empty() || !(word_byte(rest[0]) || rest[0] == b':'),
        6,
    );
    scan(
        &pats.ipv4,
        |b| word_byte(b) || b == b'.',
        |rest| {
            rest.is_empty()
                || !(word_byte(rest[0])
                    || (rest[0] == b'.' && rest.len() > 1 && rest[1].is_ascii_digit()))
        },
        4,
    );

    found.sort_by_key(|item| item.start);
    found
}

fn masked_address(address: &str, version: u8, policy: &IpPolicy) -> String {
    if policy.mode == "hide" {
        return mibot_i18n::tr("[IP hidden]", "[IP已隐藏]").into();
    }
    let (mut parts, sep, count) = if version == 6 {
        (ipv6_groups(address), ":", usize::from(policy.ipv6_segments))
    } else {
        (
            address.split('.').map(str::to_owned).collect(),
            ".",
            usize::from(policy.ipv4_segments),
        )
    };
    let len = parts.len();
    for (idx, part) in parts.iter_mut().enumerate() {
        if idx >= len.saturating_sub(count) {
            *part = "*".into();
        }
    }
    parts.join(sep)
}

fn ipv6_groups(address: &str) -> Vec<String> {
    let source = address.split('%').next().unwrap_or(address);
    let normalized = if source.contains('.') {
        if let Some(idx) = source.rfind(':') {
            let mut b = [0u16; 4];
            for (pos, part) in source[idx + 1..].split('.').take(4).enumerate() {
                b[pos] = part.parse::<u16>().unwrap_or(0);
            }
            format!(
                "{}{:x}:{:x}",
                &source[..=idx],
                (b[0] << 8) + b[1],
                (b[2] << 8) + b[3]
            )
        } else {
            source.to_owned()
        }
    } else {
        source.to_owned()
    };

    if let Some((left, right)) = normalized.split_once("::") {
        let head: Vec<String> = if left.is_empty() {
            Vec::new()
        } else {
            left.split(':').map(str::to_owned).collect()
        };
        let tail: Vec<String> = if right.is_empty() {
            Vec::new()
        } else {
            right.split(':').map(str::to_owned).collect()
        };
        let mut groups = head;
        while groups.len() + tail.len() < 8 {
            groups.push("0".into());
        }
        groups.extend(tail);
        groups
    } else {
        normalized.split(':').map(str::to_owned).collect()
    }
}

pub fn mask_ips(text: &str) -> String {
    mask_ips_with(text, &current_ip_policy())
}

pub fn mask_ips_with(text: &str, policy: &IpPolicy) -> String {
    let items = ip_replacements(text, policy);
    apply_ip_replacements(text, &items).0
}

fn apply_ip_replacements(text: &str, items: &[IpReplacement]) -> (String, bool) {
    if items.is_empty() {
        return (text.to_owned(), false);
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for item in items {
        out.push_str(&text[cursor..item.start]);
        out.push_str(&item.value);
        cursor = item.end;
    }
    out.push_str(&text[cursor..]);
    (out, true)
}

fn percent_decode_lossy(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Returns true if a URL encodes an IP address or uses an IP literal host.
pub fn ip_link(value: &str) -> bool {
    let decoded = percent_decode_lossy(value);
    let default_policy = IpPolicy::default();
    if !ip_replacements(&decoded, &default_policy).is_empty() {
        return true;
    }
    if let Ok(parsed) = Url::parse(&decoded)
        && let Some(host) = parsed.host_str()
    {
        let bare = host.trim_matches(|c| c == '[' || c == ']');
        if IpAddr::from_str(bare).is_ok() {
            return true;
        }
    }
    false
}

/// Masks IP addresses in `text`, adjusts UTF-16 entity offsets/lengths,
/// and strips URL entities whose targets contain IP addresses.
pub fn redact_message(
    text: &str,
    entities: &[tl::enums::MessageEntity],
    policy: &IpPolicy,
) -> (String, Vec<tl::enums::MessageEntity>, bool) {
    let items = ip_replacements(text, policy);
    let (masked, mut changed) = apply_ip_replacements(text, &items);

    struct Shift {
        start: i32,
        end: i32,
        length: i32,
    }
    let shifts: Vec<Shift> = items
        .iter()
        .map(|it| Shift {
            start: utf16_len(&text[..it.start]),
            end: utf16_len(&text[..it.end]),
            length: utf16_len(&it.value),
        })
        .collect();

    let position = |offset: i32, end: bool| -> i32 {
        let mut moved = 0i32;
        for s in &shifts {
            if offset <= s.start {
                break;
            }
            if offset < s.end {
                let mut res = s.start + moved;
                if end {
                    res += s.length;
                }
                return res;
            }
            moved += s.length - (s.end - s.start);
        }
        offset + moved
    };

    let units: Vec<u16> = text.encode_utf16().collect();
    let mut kept = Vec::with_capacity(entities.len());

    for entity in entities {
        let offset = entity.offset();
        let length = entity.length();
        match entity {
            tl::enums::MessageEntity::TextUrl(u) if ip_link(&u.url) => {
                changed = true;
                continue;
            }
            tl::enums::MessageEntity::Url(_) if offset >= 0 && length >= 0 => {
                let start = offset as usize;
                let end = start.saturating_add(length as usize);
                if end <= units.len() {
                    let slice = String::from_utf16_lossy(&units[start..end]);
                    if ip_link(&slice) {
                        changed = true;
                        continue;
                    }
                }
            }
            _ => {}
        }
        if shifts.is_empty() {
            kept.push(entity.clone());
            continue;
        }
        let start = position(offset, false);
        let finish = position(offset + length, true);
        if finish <= start {
            continue;
        }
        kept.push(with_entity_range(entity, start, finish - start));
    }

    (masked, kept, changed)
}

fn with_entity_range(
    entity: &tl::enums::MessageEntity,
    offset: i32,
    length: i32,
) -> tl::enums::MessageEntity {
    let mut cloned = entity.clone();
    match &mut cloned {
        tl::enums::MessageEntity::Unknown(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Mention(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Hashtag(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::BotCommand(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Url(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Email(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Bold(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Italic(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Code(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Pre(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::TextUrl(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::MentionName(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::InputMessageEntityMentionName(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Phone(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Cashtag(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Underline(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Strike(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::BankCard(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Spoiler(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::CustomEmoji(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::Blockquote(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::FormattedDate(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::DiffInsert(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::DiffReplace(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::DiffDelete(e) => {
            e.offset = offset;
            e.length = length;
        }
        tl::enums::MessageEntity::TonAddress(e) => {
            e.offset = offset;
            e.length = length;
        }
    }
    cloned
}

pub fn redact_media(media: &tl::enums::InputMedia, policy: &IpPolicy) -> tl::enums::InputMedia {
    let mut cloned = media.clone();
    if let tl::enums::InputMedia::UploadedDocument(doc) = &mut cloned {
        for attr in &mut doc.attributes {
            if let tl::enums::DocumentAttribute::Filename(name) = attr {
                name.file_name = mask_ips_with(&name.file_name, policy);
            }
        }
    }
    cloned
}

pub fn redact_rich_message(
    rich: &tl::enums::InputRichMessage,
    policy: &IpPolicy,
) -> tl::enums::InputRichMessage {
    let mut cloned = rich.clone();
    match &mut cloned {
        tl::enums::InputRichMessage::Message(msg) => {
            for block in &mut msg.blocks {
                redact_block(block, policy);
            }
        }
        tl::enums::InputRichMessage::Html(html) => {
            html.html = mask_ips_with(&html.html, policy);
        }
        tl::enums::InputRichMessage::Markdown(md) => {
            md.markdown = mask_ips_with(&md.markdown, policy);
        }
    }
    cloned
}

fn redact_block(block: &mut tl::enums::PageBlock, policy: &IpPolicy) {
    match block {
        tl::enums::PageBlock::Title(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::Subtitle(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::AuthorDate(b) => redact_rich_text(&mut b.author, policy),
        tl::enums::PageBlock::Header(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::Subheader(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::Paragraph(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::Preformatted(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::Footer(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::Thinking(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::Kicker(b) => redact_rich_text(&mut b.text, policy),
        tl::enums::PageBlock::Blockquote(b) => {
            redact_rich_text(&mut b.text, policy);
            redact_rich_text(&mut b.caption, policy);
        }
        tl::enums::PageBlock::Pullquote(b) => {
            redact_rich_text(&mut b.text, policy);
            redact_rich_text(&mut b.caption, policy);
        }
        tl::enums::PageBlock::BlockquoteBlocks(b) => {
            for child in &mut b.blocks {
                redact_block(child, policy);
            }
            redact_rich_text(&mut b.caption, policy);
        }
        tl::enums::PageBlock::Details(b) => {
            redact_rich_text(&mut b.title, policy);
            for child in &mut b.blocks {
                redact_block(child, policy);
            }
        }
        tl::enums::PageBlock::List(b) => {
            for item in &mut b.items {
                match item {
                    tl::enums::PageListItem::Text(t) => redact_rich_text(&mut t.text, policy),
                    tl::enums::PageListItem::Blocks(bl) => {
                        for child in &mut bl.blocks {
                            redact_block(child, policy);
                        }
                    }
                }
            }
        }
        tl::enums::PageBlock::OrderedList(b) => {
            for item in &mut b.items {
                match item {
                    tl::enums::PageListOrderedItem::Text(t) => redact_rich_text(&mut t.text, policy),
                    tl::enums::PageListOrderedItem::Blocks(bl) => {
                        for child in &mut bl.blocks {
                            redact_block(child, policy);
                        }
                    }
                }
            }
        }
        tl::enums::PageBlock::Table(b) => {
            redact_rich_text(&mut b.title, policy);
            for row in &mut b.rows {
                let tl::enums::PageTableRow::Row(r) = row;
                for cell in &mut r.cells {
                    let tl::enums::PageTableCell::Cell(c) = cell;
                    if let Some(t) = &mut c.text {
                        redact_rich_text(t, policy);
                    }
                }
            }
        }
        tl::enums::PageBlock::Cover(c) => redact_block(&mut c.cover, policy),
        tl::enums::PageBlock::Collage(c) => {
            for child in &mut c.items {
                redact_block(child, policy);
            }
            redact_caption(&mut c.caption, policy);
        }
        tl::enums::PageBlock::Slideshow(s) => {
            for child in &mut s.items {
                redact_block(child, policy);
            }
            redact_caption(&mut s.caption, policy);
        }
        tl::enums::PageBlock::EmbedPost(e) => {
            e.author = mask_ips_with(&e.author, policy);
            if ip_link(&e.url) {
                e.url.clear();
            } else {
                e.url = mask_ips_with(&e.url, policy);
            }
            for child in &mut e.blocks {
                redact_block(child, policy);
            }
            redact_caption(&mut e.caption, policy);
        }
        tl::enums::PageBlock::Photo(p) => {
            if let Some(u) = &mut p.url
                && ip_link(u)
            {
                u.clear();
            }
            redact_caption(&mut p.caption, policy);
        }
        tl::enums::PageBlock::Video(v) => redact_caption(&mut v.caption, policy),
        tl::enums::PageBlock::Audio(a) => redact_caption(&mut a.caption, policy),
        tl::enums::PageBlock::Document(d) => redact_caption(&mut d.caption, policy),
        tl::enums::PageBlock::Map(m) => redact_caption(&mut m.caption, policy),
        tl::enums::PageBlock::InputPageBlockMap(m) => redact_caption(&mut m.caption, policy),
        tl::enums::PageBlock::Math(m) => m.source = mask_ips_with(&m.source, policy),
        _ => {}
    }
}

fn redact_caption(caption: &mut tl::enums::PageCaption, policy: &IpPolicy) {
    let tl::enums::PageCaption::Caption(c) = caption;
    redact_rich_text(&mut c.text, policy);
    redact_rich_text(&mut c.credit, policy);
}

fn redact_rich_text(text: &mut tl::enums::RichText, policy: &IpPolicy) {
    match text {
        tl::enums::RichText::TextEmpty => {}
        tl::enums::RichText::TextPlain(p) => {
            p.text = mask_ips_with(&p.text, policy);
        }
        tl::enums::RichText::TextUrl(u) => {
            if ip_link(&u.url) {
                let plain = mask_ips_with(&rich_plain_text(&u.text), policy);
                *text = rich_plain(&plain);
            } else {
                redact_rich_text(&mut u.text, policy);
            }
        }
        tl::enums::RichText::TextAutoUrl(u) => {
            let label = rich_plain_text(&u.text);
            if ip_link(&label) {
                *text = rich_plain(&mask_ips_with(&label, policy));
            } else {
                redact_rich_text(&mut u.text, policy);
            }
        }
        tl::enums::RichText::TextButton(b) => {
            let button_url = match &b.r#type {
                tl::enums::InlineButtonType::Url(u) => Some(u.url.as_str()),
                tl::enums::InlineButtonType::UrlAuth(u) => Some(u.url.as_str()),
                tl::enums::InlineButtonType::InputInlineButtonTypeUrlAuth(u) => {
                    Some(u.url.as_str())
                }
                tl::enums::InlineButtonType::WebView(u) => Some(u.url.as_str()),
                _ => None,
            };
            if button_url.is_some_and(ip_link) {
                let plain = mask_ips_with(&rich_plain_text(&b.text), policy);
                *text = rich_plain(&plain);
            } else {
                redact_rich_text(&mut b.text, policy);
            }
        }
        tl::enums::RichText::TextConcat(c) => {
            let expected_masked = mask_ips_with(
                &c.texts.iter().map(rich_plain_text).collect::<String>(),
                policy,
            );
            for part in &mut c.texts {
                redact_rich_text(part, policy);
            }
            let after: String = c.texts.iter().map(rich_plain_text).collect();
            if after != expected_masked {
                *text = rich_plain(&expected_masked);
            }
        }
        tl::enums::RichText::TextBold(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextItalic(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextUnderline(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextStrike(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextFixed(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextEmail(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextSubscript(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextSuperscript(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextMarked(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextPhone(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextAnchor(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextMath(x) => x.source = mask_ips_with(&x.source, policy),
        tl::enums::RichText::TextCustomEmoji(x) => x.alt = mask_ips_with(&x.alt, policy),
        tl::enums::RichText::TextSpoiler(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextMention(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextHashtag(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextBotCommand(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextCashtag(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextAutoEmail(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextAutoPhone(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextBankCard(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextMentionName(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextDate(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextDiff(x) => {
            redact_rich_text(&mut x.text, policy);
            redact_rich_text(&mut x.old_text, policy);
        }
        tl::enums::RichText::TextTonAddress(x) => redact_rich_text(&mut x.text, policy),
        tl::enums::RichText::TextImage(_) => {}
    }
}

/// Redacts a `SendMessage` / `EditMessage` / `SendMedia` / `SendMultiMedia` TL call in place
/// before entering `Retrier`, returning `true` if text/links changed (`no_webpage = true`).
pub fn redact_send_message(req: &mut tl::functions::messages::SendMessage, policy: &IpPolicy) {
    if let Some(rich) = &req.rich_message {
        req.rich_message = Some(redact_rich_message(rich, policy));
    }
    let entities = req.entities.as_deref().unwrap_or(&[]);
    let (masked, kept, changed) = redact_message(&req.message, entities, policy);
    req.message = masked;
    req.entities = if kept.is_empty() { None } else { Some(kept) };
    if changed {
        req.no_webpage = true;
    }
}

pub fn redact_edit_message(req: &mut tl::functions::messages::EditMessage, policy: &IpPolicy) {
    if let Some(rich) = &req.rich_message {
        req.rich_message = Some(redact_rich_message(rich, policy));
    }
    if let Some(msg) = &req.message {
        let entities = req.entities.as_deref().unwrap_or(&[]);
        let (masked, kept, changed) = redact_message(msg, entities, policy);
        req.message = Some(masked);
        req.entities = if kept.is_empty() { None } else { Some(kept) };
        if changed {
            req.no_webpage = true;
        }
    }
    if let Some(media) = &req.media {
        req.media = Some(redact_media(media, policy));
    }
}

pub fn redact_send_media(req: &mut tl::functions::messages::SendMedia, policy: &IpPolicy) {
    let entities = req.entities.as_deref().unwrap_or(&[]);
    let (masked, kept, _) = redact_message(&req.message, entities, policy);
    req.message = masked;
    req.entities = if kept.is_empty() { None } else { Some(kept) };
    req.media = redact_media(&req.media, policy);
}

pub fn redact_send_multi_media(
    req: &mut tl::functions::messages::SendMultiMedia,
    policy: &IpPolicy,
) {
    for item in &mut req.multi_media {
        let tl::enums::InputSingleMedia::Media(single) = item;
        let entities = single.entities.as_deref().unwrap_or(&[]);
        let (masked, kept, _) = redact_message(&single.message, entities, policy);
        single.message = masked;
        single.entities = if kept.is_empty() { None } else { Some(kept) };
        single.media = redact_media(&single.media, policy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich::{rich_code, rich_concat};

    #[test]
    fn mask_ips_matches_go_cases() {
        let def = IpPolicy::default();
        let cases = [
            ("出口 1.2.3.4 端口", "出口 1.2.*.* 端口"),
            ("http://10.0.0.8:8080/x", "http://10.0.*.*:8080/x"),
            ("v6 2001:db8::1 结束", "v6 2001:db8:0:0:*:*:*:* 结束"),
            ("映射 ::ffff:1.2.3.4", "映射 0:0:0:0:*:*:*:*"),
            ("两个 8.8.8.8、1.1.1.1。", "两个 8.8.*.*、1.1.*.*。"),
            ("版本 v1.2.3.4", "版本 v1.2.3.4"),
            ("时间 12:30:45", "时间 12:30:45"),
            ("MAC aa:bb:cc:dd:ee:ff", "MAC aa:bb:cc:dd:ee:ff"),
            ("超范围 256.1.1.1", "超范围 256.1.1.1"),
            ("五段 1.2.3.4.5", "五段 1.2.3.4.5"),
            ("前导零 01.2.3.4", "前导零 01.2.3.4"),
            ("句末 1.2.3.4.", "句末 1.2.*.*."),
            ("区域 fe80::1%eth0 后", "区域 fe80:0:0:0:*:*:*:* 后"),
        ];
        for (input, want) in cases {
            assert_eq!(mask_ips_with(input, &def), want, "input={input:?}");
        }

        let hide = IpPolicy {
            mode: "hide".into(),
            ipv4_segments: 2,
            ipv6_segments: 4,
        };
        assert_eq!(mask_ips_with("a 1.2.3.4 b", &hide), "a [IP hidden] b");
        mibot_i18n::with_lang(mibot_i18n::Lang::Zh, || {
            assert_eq!(mask_ips_with("a 1.2.3.4 b", &hide), "a [IP已隐藏] b");
        });

        let full = IpPolicy {
            mode: "mask".into(),
            ipv4_segments: 4,
            ipv6_segments: 8,
        };
        assert_eq!(mask_ips_with("1.2.3.4", &full), "*.*.*.*");

        assert!(
            IpPolicy {
                mode: "mask".into(),
                ipv4_segments: 5,
                ipv6_segments: 4
            }
            .validate()
            .is_err()
        );
        assert!(
            IpPolicy {
                mode: "off".into(),
                ipv4_segments: 2,
                ipv6_segments: 4
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn redact_message_shifts_utf16_entities_and_drops_ip_links() {
        let text = "😀 10.20.30.40 看 链接";
        let entities: Vec<tl::enums::MessageEntity> = vec![
            tl::types::MessageEntityCode {
                offset: 3,
                length: 11,
            }
            .into(),
            tl::types::MessageEntityBold {
                offset: 15,
                length: 1,
            }
            .into(),
            tl::types::MessageEntityTextUrl {
                offset: 17,
                length: 2,
                url: "http://10.20.30.40/admin".into(),
            }
            .into(),
            tl::types::MessageEntityItalic {
                offset: 17,
                length: 2,
            }
            .into(),
        ];
        let (masked, kept, changed) = redact_message(text, &entities, &IpPolicy::default());
        assert_eq!(masked, "😀 10.20.*.* 看 链接");
        assert!(changed);
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].offset(), 3);
        assert_eq!(kept[0].length(), 9);
        assert_eq!(kept[1].offset(), 13);
        assert_eq!(kept[2].offset(), 15);
        assert_eq!(entities[1].offset(), 15);

        let (plain, same, changed2) =
            redact_message("没有地址 1.2.3", &entities[..2], &IpPolicy::default());
        assert_eq!(plain, "没有地址 1.2.3");
        assert!(!changed2);
        assert_eq!(same.len(), 2);
    }

    #[test]
    fn redact_rich_message_masks_split_concat_and_table_links() {
        let original = rich_concat(&[rich_plain("出口 10.20.30.4"), rich_code("0")]);
        let cell = tl::enums::PageTableCell::Cell(tl::types::PageTableCell {
            header: false,
            align_center: false,
            align_right: false,
            valign_middle: false,
            valign_bottom: false,
            text: Some(
                tl::types::TextUrl {
                    text: rich_plain("详情"),
                    url: "https://10%2e20%2e30%2e40/info".into(),
                    webpage_id: 0,
                }
                .into(),
            ),
            colspan: None,
            rowspan: None,
        });
        let rich = tl::enums::InputRichMessage::Message(tl::types::InputRichMessage {
            rtl: false,
            noautolink: false,
            blocks: vec![
                tl::types::PageBlockParagraph { text: original }.into(),
                tl::types::PageBlockTable {
                    bordered: false,
                    striped: false,
                    compact: false,
                    title: rich_plain("表"),
                    rows: vec![tl::types::PageTableRow { cells: vec![cell] }.into()],
                }
                .into(),
            ],
            photos: None,
            documents: None,
            users: None,
        });

        let redacted = redact_rich_message(&rich, &IpPolicy::default());
        let tl::enums::InputRichMessage::Message(r) = redacted else {
            panic!()
        };
        if let tl::enums::PageBlock::Paragraph(p) = &r.blocks[0] {
            assert_eq!(rich_plain_text(&p.text), "出口 10.20.*.*");
        } else {
            panic!()
        }
        if let tl::enums::PageBlock::Table(tbl) = &r.blocks[1] {
            let tl::enums::PageTableRow::Row(row) = &tbl.rows[0];
            let tl::enums::PageTableCell::Cell(c) = &row.cells[0];
            assert!(matches!(c.text, Some(tl::enums::RichText::TextPlain(_))));
            assert_eq!(rich_plain_text(c.text.as_ref().unwrap()), "详情");
        } else {
            panic!()
        }
    }
}
