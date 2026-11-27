use crate::BotError;
use grammers_tl_types as tl;

pub const RICH_TEXT_LIMIT: usize = 32768;
pub const RICH_BLOCK_LIMIT: usize = 500;
pub const RICH_COLUMN_LIMIT: usize = 20;

pub fn rich_plain(text: &str) -> tl::enums::RichText {
    if text.is_empty() {
        tl::enums::RichText::TextEmpty
    } else {
        tl::types::TextPlain {
            text: text.to_owned(),
        }
        .into()
    }
}

pub fn rich_code(text: &str) -> tl::enums::RichText {
    tl::types::TextFixed {
        text: rich_plain(text),
    }
    .into()
}

pub fn rich_concat(parts: &[tl::enums::RichText]) -> tl::enums::RichText {
    let mut texts: Vec<tl::enums::RichText> = Vec::new();
    for part in parts {
        match part {
            tl::enums::RichText::TextEmpty => continue,
            tl::enums::RichText::TextConcat(c) => {
                for item in &c.texts {
                    push_rich_part(&mut texts, item);
                }
                continue;
            }
            _ => push_rich_part(&mut texts, part),
        }
    }
    match texts.len() {
        0 => tl::enums::RichText::TextEmpty,
        1 => texts.into_iter().next().unwrap_or(tl::enums::RichText::TextEmpty),
        _ => tl::types::TextConcat { texts }.into(),
    }
}

fn push_rich_part(texts: &mut Vec<tl::enums::RichText>, part: &tl::enums::RichText) {
    match part {
        tl::enums::RichText::TextEmpty => {}
        tl::enums::RichText::TextPlain(p) => {
            if let Some(tl::enums::RichText::TextPlain(last)) = texts.last_mut() {
                last.text.push_str(&p.text);
                return;
            }
            texts.push(part.clone());
        }
        _ => texts.push(part.clone()),
    }
}

pub fn rich_fits(blocks: &[tl::enums::PageBlock]) -> bool {
    let mut lines = Vec::new();
    rich_blocks_text(blocks, "", &mut lines);
    let chars: usize = lines.iter().map(|line| line.chars().count()).sum();
    chars <= RICH_TEXT_LIMIT && rich_block_count(blocks) <= RICH_BLOCK_LIMIT
}

pub fn rich_block_count(blocks: &[tl::enums::PageBlock]) -> usize {
    let mut count = blocks.len();
    for block in blocks {
        match block {
            tl::enums::PageBlock::BlockquoteBlocks(b) => count += rich_block_count(&b.blocks),
            tl::enums::PageBlock::Details(b) => count += rich_block_count(&b.blocks),
            tl::enums::PageBlock::Table(b) => count += b.rows.len(),
            tl::enums::PageBlock::List(b) => {
                for item in &b.items {
                    count += 1;
                    if let tl::enums::PageListItem::Blocks(bl) = item {
                        count += rich_block_count(&bl.blocks);
                    }
                }
            }
            tl::enums::PageBlock::OrderedList(b) => {
                for item in &b.items {
                    count += 1;
                    if let tl::enums::PageListOrderedItem::Blocks(bl) = item {
                        count += rich_block_count(&bl.blocks);
                    }
                }
            }
            _ => {}
        }
    }
    count
}

/// Returns true only when Telegram explicitly rejected the rich message (`code < 500` RPC,
/// excluding `FLOOD_WAIT` and uncertain failures), so caller may safely fall back to plain text.
pub fn rich_rejected(err: &BotError) -> bool {
    if err.is_uncertain() {
        return false;
    }
    if err.flood_wait().is_some() {
        return false;
    }
    err.rpc_error().is_some_and(|rpc| rpc.code < 500)
}

pub fn plain_copy(text: &str) -> String {
    const LIMIT: usize = 4000;
    let mut units = 0usize;
    for (idx, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > LIMIT {
            let suffix = mibot_i18n::tr("… (truncated)", "…（内容过长，已截断）");
            return format!("{}\n{suffix}", &text[..idx]);
        }
    }
    text.to_owned()
}

pub fn message_text(message: Option<&tl::types::Message>) -> String {
    let Some(msg) = message else {
        return String::new();
    };
    if !msg.message.is_empty() {
        return msg.message.clone();
    }
    rich_message_text(msg)
}

pub fn rich_message_text(message: &tl::types::Message) -> String {
    let Some(tl::enums::RichMessage::Message(rich)) = &message.rich_message else {
        return String::new();
    };
    let mut lines = Vec::new();
    rich_blocks_text(&rich.blocks, "", &mut lines);
    lines.join("\n").trim().to_owned()
}

pub fn input_rich_fits(rich: &tl::enums::InputRichMessage) -> bool {
    match rich {
        tl::enums::InputRichMessage::Message(m) => rich_fits(&m.blocks),
        tl::enums::InputRichMessage::Html(_) | tl::enums::InputRichMessage::Markdown(_) => true,
    }
}

pub fn input_rich_text(rich: &tl::enums::InputRichMessage) -> String {
    match rich {
        tl::enums::InputRichMessage::Message(m) => {
            let mut lines = Vec::new();
            rich_blocks_text(&m.blocks, "", &mut lines);
            lines.join("\n").trim().to_owned()
        }
        tl::enums::InputRichMessage::Html(h) => h.html.clone(),
        tl::enums::InputRichMessage::Markdown(md) => md.markdown.clone(),
    }
}

fn push_rich_line(text: &tl::enums::RichText, cur_prefix: &mut String, lines: &mut Vec<String>) {
    let line = rich_plain_text(text);
    if !line.trim().is_empty() {
        lines.push(format!("{cur_prefix}{line}"));
        *cur_prefix = " ".repeat(cur_prefix.chars().count());
    }
}

pub fn rich_blocks_text(blocks: &[tl::enums::PageBlock], prefix: &str, lines: &mut Vec<String>) {
    let mut cur_prefix = prefix.to_owned();

    for block in blocks {
        match block {
            tl::enums::PageBlock::Paragraph(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Heading1(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Heading2(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Heading3(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Heading4(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Heading5(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Heading6(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Title(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Subtitle(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Header(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Subheader(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Kicker(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Footer(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Preformatted(b) => {
                push_rich_line(&b.text, &mut cur_prefix, lines)
            }
            tl::enums::PageBlock::Blockquote(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Pullquote(b) => push_rich_line(&b.text, &mut cur_prefix, lines),
            tl::enums::PageBlock::Math(b) => lines.push(format!("{cur_prefix}{}", b.source)),
            tl::enums::PageBlock::BlockquoteBlocks(b) => {
                rich_blocks_text(&b.blocks, &cur_prefix, lines);
            }
            tl::enums::PageBlock::Details(b) => {
                push_rich_line(&b.title, &mut cur_prefix, lines);
                rich_blocks_text(&b.blocks, &cur_prefix, lines);
            }
            tl::enums::PageBlock::List(b) => {
                let item_prefix = format!("{cur_prefix}• ");
                for item in &b.items {
                    match item {
                        tl::enums::PageListItem::Text(t) => {
                            lines.push(format!("{item_prefix}{}", rich_plain_text(&t.text)));
                        }
                        tl::enums::PageListItem::Blocks(bl) => {
                            rich_blocks_text(&bl.blocks, &item_prefix, lines);
                        }
                    }
                }
            }
            tl::enums::PageBlock::OrderedList(b) => {
                for (index, item) in b.items.iter().enumerate() {
                    match item {
                        tl::enums::PageListOrderedItem::Text(t) => {
                            let label = ordered_label(t.num.as_deref(), t.value, index);
                            lines.push(format!("{cur_prefix}{label}{}", rich_plain_text(&t.text)));
                        }
                        tl::enums::PageListOrderedItem::Blocks(bl) => {
                            let label = ordered_label(bl.num.as_deref(), bl.value, index);
                            let child_prefix = format!("{cur_prefix}{label}");
                            rich_blocks_text(&bl.blocks, &child_prefix, lines);
                        }
                    }
                }
            }
            tl::enums::PageBlock::Table(b) => {
                push_rich_line(&b.title, &mut cur_prefix, lines);
                for row in &b.rows {
                    let tl::enums::PageTableRow::Row(r) = row;
                    let cells: Vec<String> = r
                        .cells
                        .iter()
                        .map(|c| {
                            let tl::enums::PageTableCell::Cell(cell) = c;
                            cell.text.as_ref().map_or_else(String::new, rich_plain_text)
                        })
                        .collect();
                    lines.push(format!("{cur_prefix}{}", cells.join(" | ")));
                }
            }
            _ => {}
        }
    }
}

fn ordered_label(num: Option<&str>, value: Option<i32>, index: usize) -> String {
    if let Some(n) = num
        && !n.is_empty()
    {
        return format!("{n} ");
    }
    if let Some(v) = value
        && v != 0
    {
        return format!("{v}. ");
    }
    format!("{}. ", index + 1)
}

pub fn rich_plain_text(text: &tl::enums::RichText) -> String {
    match text {
        tl::enums::RichText::TextEmpty => String::new(),
        tl::enums::RichText::TextPlain(p) => p.text.clone(),
        tl::enums::RichText::TextConcat(c) => {
            let mut out = String::new();
            for part in &c.texts {
                out.push_str(&rich_plain_text(part));
            }
            out
        }
        tl::enums::RichText::TextMath(m) => m.source.clone(),
        tl::enums::RichText::TextCustomEmoji(e) => e.alt.clone(),
        tl::enums::RichText::TextBold(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextItalic(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextUnderline(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextStrike(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextFixed(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextUrl(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextEmail(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextSubscript(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextSuperscript(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextMarked(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextPhone(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextAnchor(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextSpoiler(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextMention(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextHashtag(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextBotCommand(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextCashtag(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextAutoUrl(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextAutoEmail(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextAutoPhone(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextBankCard(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextMentionName(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextDate(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextDiff(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextButton(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextTonAddress(x) => rich_plain_text(&x.text),
        tl::enums::RichText::TextImage(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::utf16_len;
    use crate::message::{blank_tl_message, envelope};
    use crate::peers::PeerCache;
    use grammers_mtsender::{InvocationError, RpcError};
    use mibot_delivery::UncertainError;

    #[test]
    fn rich_message_text_flattens_nested_blocks() {
        let mut msg = blank_tl_message(1, tl::types::PeerUser { user_id: 1 }.into());
        msg.rich_message = Some(
            tl::types::RichMessage {
                rtl: false,
                part: false,
                blocks: vec![
                    tl::types::PageBlockHeading2 {
                        text: rich_plain("标题"),
                    }
                    .into(),
                    tl::types::PageBlockParagraph {
                        text: tl::types::TextConcat {
                            texts: vec![
                                rich_plain("看 "),
                                tl::types::TextUrl {
                                    text: tl::types::TextBold {
                                        text: rich_plain("链接"),
                                    }
                                    .into(),
                                    url: "https://e.com".into(),
                                    webpage_id: 0,
                                }
                                .into(),
                                rich_plain(" 和 "),
                                tl::types::TextMath {
                                    source: "x^2".into(),
                                }
                                .into(),
                            ],
                        }
                        .into(),
                    }
                    .into(),
                    tl::types::PageBlockList {
                        items: vec![
                            tl::types::PageListItemBlocks {
                                checkbox: false,
                                checked: false,
                                blocks: vec![
                                    tl::types::PageBlockParagraph {
                                        text: rich_plain("一"),
                                    }
                                    .into(),
                                    tl::types::PageBlockList {
                                        items: vec![
                                            tl::types::PageListItemText {
                                                checkbox: false,
                                                checked: false,
                                                text: rich_plain("一点一"),
                                            }
                                            .into(),
                                        ],
                                    }
                                    .into(),
                                ],
                            }
                            .into(),
                        ],
                    }
                    .into(),
                    tl::types::PageBlockOrderedList {
                        reversed: false,
                        items: vec![
                            tl::types::PageListOrderedItemBlocks {
                                checkbox: false,
                                checked: false,
                                num: None,
                                blocks: vec![
                                    tl::types::PageBlockParagraph {
                                        text: rich_plain("丙"),
                                    }
                                    .into(),
                                ],
                                value: Some(3),
                                r#type: None,
                            }
                            .into(),
                        ],
                        start: None,
                        r#type: None,
                    }
                    .into(),
                    tl::types::PageBlockTable {
                        bordered: false,
                        striped: false,
                        compact: false,
                        title: tl::enums::RichText::TextEmpty,
                        rows: vec![
                            tl::types::PageTableRow {
                                cells: vec![
                                    tl::types::PageTableCell {
                                        header: false,
                                        align_center: false,
                                        align_right: false,
                                        valign_middle: false,
                                        valign_bottom: false,
                                        text: Some(rich_plain("A")),
                                        colspan: None,
                                        rowspan: None,
                                    }
                                    .into(),
                                    tl::types::PageTableCell {
                                        header: false,
                                        align_center: false,
                                        align_right: false,
                                        valign_middle: false,
                                        valign_bottom: false,
                                        text: Some(rich_plain("B")),
                                        colspan: None,
                                        rowspan: None,
                                    }
                                    .into(),
                                ],
                            }
                            .into(),
                        ],
                    }
                    .into(),
                    tl::types::PageBlockDetails {
                        open: false,
                        blocks: vec![
                            tl::types::PageBlockMath {
                                source: "E=mc^2".into(),
                            }
                            .into(),
                        ],
                        title: rich_plain("详情"),
                    }
                    .into(),
                ],
                photos: vec![],
                documents: vec![],
            }
            .into(),
        );
        let want = "标题\n看 链接 和 x^2\n• 一\n  • 一点一\n3. 丙\nA | B\n详情\nE=mc^2";
        assert_eq!(rich_message_text(&msg), want);

        let mut plain_msg = blank_tl_message(2, tl::types::PeerUser { user_id: 1 }.into());
        plain_msg.message = "普通消息".into();
        assert_eq!(rich_message_text(&plain_msg), "");
    }

    #[test]
    fn rich_fits_and_plain_copy_and_rejected() {
        let para = |s: &str| -> tl::enums::PageBlock {
            tl::types::PageBlockParagraph {
                text: rich_plain(s),
            }
            .into()
        };
        assert!(rich_fits(&[para(&"字".repeat(RICH_TEXT_LIMIT))]));
        assert!(!rich_fits(&[para(&"字".repeat(RICH_TEXT_LIMIT + 1))]));

        let items: Vec<tl::enums::PageListItem> = (0..RICH_BLOCK_LIMIT)
            .map(|_| {
                tl::types::PageListItemBlocks {
                    checkbox: false,
                    checked: false,
                    blocks: vec![para("项")],
                }
                .into()
            })
            .collect();
        assert!(!rich_fits(&[tl::types::PageBlockList { items }.into()]));

        let truncated = plain_copy(&"😀".repeat(3000));
        assert!(utf16_len(&truncated) <= 4096);
        assert!(truncated.ends_with("… (truncated)"));
        assert_eq!(plain_copy("short"), "short");

        assert!(rich_rejected(&BotError::Invocation(InvocationError::Rpc(
            RpcError {
                code: 403,
                name: "PREMIUM_ACCOUNT_REQUIRED".into(),
                value: None,
                caused_by: None,
            }
        ))));
        assert!(!rich_rejected(&BotError::Invocation(InvocationError::Rpc(
            RpcError {
                code: 420,
                name: "FLOOD_WAIT".into(),
                value: Some(5),
                caused_by: None,
            }
        ))));
        assert!(!rich_rejected(&BotError::Uncertain(UncertainError::new(
            std::io::Error::other("err")
        ))));

        // Rich message content vs empty command Text
        let mut msg = blank_tl_message(5, tl::types::PeerUser { user_id: 1 }.into());
        msg.rich_message = Some(
            tl::types::RichMessage {
                rtl: false,
                part: false,
                blocks: vec![para(".restart")],
                photos: vec![],
                documents: vec![],
            }
            .into(),
        );
        let env = envelope(&msg, 1, false, &PeerCache::new()).unwrap();
        assert_eq!(env.text, "");
        assert_eq!(env.content(), ".restart");
    }
}
