use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BotError, Client, Message, peer_id, source_of};
use mibot_command::{
    CmdError, Invocation, PAGE_LIMIT, code, escape, group, html_pages, utf16_len,
};
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_IMPORT_BYTES: usize = 256 << 10;
const RESOLVE_TTL: Duration = Duration::from_secs(24 * 3600);
const MAX_ADD_PER_CALL: usize = 50;
const LISTED_LINES: usize = 20;
const CAPTION_LIMIT: usize = 1024;
const MAX_MEDIA_BYTES: i64 = 2 << 30;
const ERR_BAD_HANDLE_TEXT: &str = "不是有效的频道用户名或链接";

const DEFAULT_FILTERS: &[&str] = &[
    "广告", "推广", "赞助", "合作", "代理", "招商", "加盟", "投资", "理财", "贷款", "借钱", "网贷",
    "信用卡", "pos机", "刷单", "兼职", "副业", "微商", "代购", "淘宝", "拼多多", "京东",
    "直播带货", "优惠券", "返利", "红包", "现金", "提现", "充值", "游戏币", "点卡", "彩票",
    "博彩", "赌博", "六合彩", "时时彩", "北京赛车", "股票", "期货", "外汇", "数字货币",
    "比特币", "挖矿", "保险", "医疗", "整容", "减肥", "丰胸", "壮阳", "药品", "假货", "高仿",
    "A货", "精仿", "原单", "尾单", "办证", "刻章", "发票", "学历", "文凭", "证书", "黑客",
    "破解", "外挂", "木马", "病毒", "盗号", "vpn", "翻墙", "代理ip", "科学上网", "梯子",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Channel {
    #[serde(default)]
    title: String,
    #[serde(default)]
    handle: String,
    #[serde(default, rename = "linkedGroup", skip_serializing_if = "String::is_empty")]
    linked_group: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    #[serde(default = "schema_one", rename = "schemaVersion")]
    schema_version: i32,
    #[serde(default, rename = "defaultChannel")]
    default_channel: Option<String>,
    #[serde(default, rename = "channelList")]
    channel_list: Vec<Channel>,
    #[serde(default, rename = "adFilters")]
    ad_filters: Vec<String>,
}

fn schema_one() -> i32 {
    1
}

fn defaults() -> Config {
    Config {
        schema_version: 1,
        default_channel: None,
        channel_list: Vec::new(),
        ad_filters: DEFAULT_FILTERS.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn separators_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[-_\s\p{Z}\u{FEFF}.|\\/#]+").unwrap())
}

fn spaces_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\s\p{Z}\u{FEFF}]+").unwrap())
}

fn letter_digits_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)[a-z]+[\s\p{Z}\u{FEFF}]*\d+").unwrap())
}

fn handle_separators_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\s\p{Z}\u{FEFF}\\]+").unwrap())
}

fn username_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]{3,31}$").unwrap())
}

fn invite_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{5,64}$").unwrap())
}

fn normalize(text: &str) -> String {
    let lower = text.to_lowercase();
    let replaced = separators_re().replace_all(&lower, " ");
    spaces_re().replace_all(&replaced, " ").trim().to_owned()
}

fn fuzzy_match(text: &str, query: &str) -> bool {
    if text.contains(query) {
        return true;
    }
    let parts: Vec<&str> = query.split_whitespace().collect();
    let words: Vec<&str> = text.split(' ').collect();
    if parts.len() == 1
        && letter_digits_re().is_match(query)
        && text.replace(' ', "").contains(&query.replace(' ', ""))
    {
        return true;
    }
    for part in parts {
        if !words.iter().any(|w| w.contains(part)) {
            return false;
        }
    }
    true
}

fn video_document(
    message: &tl::types::Message,
) -> Option<(&tl::types::Document, &tl::types::DocumentAttributeVideo)> {
    let tl::enums::MessageMedia::Document(media) = message.media.as_ref()? else {
        return None;
    };
    let tl::enums::Document::Document(doc) = media.document.as_ref()? else {
        return None;
    };
    for attr in &doc.attributes {
        if let tl::enums::DocumentAttribute::Video(video) = attr {
            return Some((doc, video));
        }
    }
    None
}

fn file_name(message: &tl::types::Message) -> &str {
    let Some((doc, _)) = video_document(message) else {
        return "";
    };
    for attr in &doc.attributes {
        if let tl::enums::DocumentAttribute::Filename(f) = attr {
            return &f.file_name;
        }
    }
    ""
}

fn duration(message: &tl::types::Message) -> f64 {
    video_document(message)
        .map(|(_, v)| v.duration)
        .unwrap_or(0.0)
}

fn matches(message: &tl::types::Message, query: &str) -> bool {
    let norm = normalize(query);
    for src in [message.message.as_str(), file_name(message)] {
        if !src.is_empty() && fuzzy_match(&normalize(src), &norm) {
            return true;
        }
    }
    false
}

fn score(message: &tl::types::Message, query: &str) -> i32 {
    let norm = normalize(query);
    let mut total = 0;
    let name = file_name(message);
    if !name.is_empty() && normalize(name).contains(&norm) {
        total += 100;
    }
    if !message.message.is_empty() && normalize(&message.message).contains(&norm) {
        total += 50;
    }
    total
}

fn is_ad(message: &tl::types::Message, filters: &[String]) -> bool {
    let text = format!("{}\n{}", message.message, file_name(message)).to_lowercase();
    filters
        .iter()
        .any(|w| !w.is_empty() && text.contains(&w.to_lowercase()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Options {
    spoiler: bool,
    random: bool,
    kkp: bool,
    query: String,
}

fn parse_options(args: &[String], forced_kkp: bool) -> Options {
    let mut opts = Options::default();
    let mut rest = Vec::new();
    for arg in args {
        match arg.to_ascii_lowercase().as_str() {
            "-s" => opts.spoiler = true,
            "-r" => opts.random = true,
            _ => rest.push(arg.as_str()),
        }
    }
    opts.kkp = forced_kkp || rest.first().is_some_and(|w| w.eq_ignore_ascii_case("kkp"));
    if !opts.kkp {
        opts.query = rest.join(" ");
    }
    opts
}

fn split_handles(raw: &str) -> Vec<String> {
    handle_separators_re()
        .split(raw)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn clip_lines(lines: &[String], limit: usize) -> Vec<String> {
    if lines.len() <= limit {
        return lines.to_vec();
    }
    let mut out = lines[..limit].to_vec();
    out.push(format!("…还有 {} 个", lines.len() - limit));
    out
}

#[derive(Debug, Clone)]
struct Source {
    peer: tl::enums::InputPeer,
    title: String,
    broadcast: bool,
    megagroup: bool,
}

#[derive(Clone)]
struct CachedSource {
    value: Source,
    at: Instant,
}

#[derive(Default)]
struct Resolver {
    cache: Mutex<HashMap<String, CachedSource>>,
}

impl Resolver {
    fn get_cached(&self, handle: &str) -> Option<Source> {
        let guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .get(handle)
            .filter(|c| c.at.elapsed() < RESOLVE_TTL)
            .map(|c| c.value.clone())
    }

    fn has(&self, handle: &str) -> bool {
        self.get_cached(handle).is_some()
    }

    fn remember(&self, handle: &str, value: Source) {
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            handle.to_owned(),
            CachedSource {
                value,
                at: Instant::now(),
            },
        );
    }

    fn forget(&self, handle: &str) {
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(handle);
    }

    async fn resolve(&self, client: &Client, handle: &str) -> Result<Source, CmdError> {
        if let Some(cached) = self.get_cached(handle) {
            return Ok(cached);
        }
        let found = lookup(client, handle).await?;
        self.remember(handle, found.clone());
        Ok(found)
    }
}

fn parse_handle(value: &str) -> (String, String, String) {
    let mut trimmed = value.trim().to_owned();
    if kit::is_numeric_id(&trimmed) {
        return (String::new(), String::new(), trimmed);
    }
    let lower = trimmed.to_lowercase();
    for prefix in ["https://", "http://"] {
        if lower.starts_with(prefix) {
            trimmed = trimmed[prefix.len()..].to_owned();
            break;
        }
    }
    let lower = trimmed.to_lowercase();
    if lower.starts_with("www.") {
        trimmed = trimmed[4..].to_owned();
    }
    let lower = trimmed.to_lowercase();
    for host in ["t.me/", "telegram.me/", "telegram.dog/"] {
        if !lower.starts_with(host) {
            continue;
        }
        let mut path = &trimmed[host.len()..];
        if let Some(cut) = path.find(['?', '#']) {
            path = &path[..cut];
        }
        if let Some(rest) = path.strip_prefix('+') {
            return (
                String::new(),
                rest.trim_matches('/').to_owned(),
                String::new(),
            );
        }
        let lower_path = path.to_lowercase();
        if lower_path.starts_with("joinchat/") {
            return (
                String::new(),
                path["joinchat/".len()..].trim_matches('/').to_owned(),
                String::new(),
            );
        }
        if lower_path.starts_with("s/") {
            path = &path[2..];
        }
        let name = path.split('/').next().unwrap_or("");
        return (
            name.trim_start_matches('@').to_owned(),
            String::new(),
            String::new(),
        );
    }
    (
        trimmed.trim_start_matches('@').to_owned(),
        String::new(),
        String::new(),
    )
}

fn valid_handle(handle: &str) -> bool {
    let (username, invite, id) = parse_handle(handle);
    if !id.is_empty() {
        return true;
    }
    if !invite.is_empty() {
        return invite_re().is_match(&invite);
    }
    username_re().is_match(&username)
}

fn describe_chat(chat: &tl::enums::Chat) -> Option<Source> {
    match chat {
        tl::enums::Chat::Channel(ch) => Some(Source {
            peer: tl::types::InputPeerChannel {
                channel_id: ch.id,
                access_hash: ch.access_hash.unwrap_or(0),
            }
            .into(),
            title: ch.title.clone(),
            broadcast: ch.broadcast,
            megagroup: ch.megagroup,
        }),
        tl::enums::Chat::Chat(c) => Some(Source {
            peer: tl::types::InputPeerChat { chat_id: c.id }.into(),
            title: c.title.clone(),
            broadcast: false,
            megagroup: false,
        }),
        _ => None,
    }
}

fn same_peer(chat: &tl::enums::Chat, peer: &tl::enums::Peer) -> bool {
    let chat_id = match chat {
        tl::enums::Chat::Empty(c) => c.id,
        tl::enums::Chat::Chat(c) => c.id,
        tl::enums::Chat::Forbidden(c) => c.id,
        tl::enums::Chat::Channel(c) => c.id,
        tl::enums::Chat::ChannelForbidden(c) => c.id,
        _ => return false,
    };
    match peer {
        tl::enums::Peer::Channel(c) => chat_id == c.channel_id,
        tl::enums::Peer::Chat(c) => chat_id == c.chat_id,
        _ => false,
    }
}

async fn lookup(client: &Client, handle: &str) -> Result<Source, CmdError> {
    if !valid_handle(handle) {
        return Err(kit::fail(ERR_BAD_HANDLE_TEXT));
    }
    let (username, invite, id) = parse_handle(handle);
    let mut want: Option<tl::enums::Peer> = None;
    let chats: Vec<tl::enums::Chat> = if !id.is_empty() {
        let peer = client.input_peer_from_chat_id(&id)?;
        match peer {
            tl::enums::InputPeer::Channel(ch) => {
                want = Some(tl::types::PeerChannel { channel_id: ch.channel_id }.into());
                let res = client
                    .invoke(&tl::functions::channels::GetChannels {
                        id: vec![
                            tl::types::InputChannel {
                                channel_id: ch.channel_id,
                                access_hash: ch.access_hash,
                            }
                            .into(),
                        ],
                    })
                    .await?;
                res.chats()
            }
            tl::enums::InputPeer::Chat(c) => {
                want = Some(tl::types::PeerChat { chat_id: c.chat_id }.into());
                let res = client
                    .invoke(&tl::functions::messages::GetChats {
                        id: vec![c.chat_id],
                    })
                    .await?;
                res.chats()
            }
            _ => return Err(kit::fail("不是频道或群组")),
        }
    } else if !invite.is_empty() {
        let res = client
            .invoke(&tl::functions::messages::CheckChatInvite { hash: invite })
            .await?;
        match res {
            tl::enums::ChatInvite::Already(v) => vec![v.chat],
            tl::enums::ChatInvite::Peek(v) => vec![v.chat],
            tl::enums::ChatInvite::Invite(_) => {
                return Err(kit::fail("还没有加入这个邀请链接的群组或频道"));
            }
        }
    } else if !username.is_empty() {
        let tl::enums::contacts::ResolvedPeer::Peer(res) = client
            .invoke(&tl::functions::contacts::ResolveUsername {
                username,
                referer: None,
            })
            .await?;
        client.peers().remember_entities(&res.users, &res.chats);
        if matches!(res.peer, tl::enums::Peer::User(_)) {
            return Err(kit::fail("不是频道或群组"));
        }
        want = Some(res.peer);
        res.chats
    } else {
        return Err(kit::fail("频道源为空"));
    };

    client.peers().remember_entities(&[], &chats);
    for chat in &chats {
        if let Some(ref w) = want
            && !same_peer(chat, w)
        {
            continue;
        }
        if let Some(found) = describe_chat(chat) {
            return Ok(found);
        }
    }
    Err(kit::fail("无法访问"))
}

async fn linked_group(client: &Client, source: &Source) -> String {
    let Some(input) = kit::input_channel(&source.peer) else {
        return String::new();
    };
    let Ok(tl::enums::messages::ChatFull::Full(full)) = client
        .invoke(&tl::functions::channels::GetFullChannel {
            channel: input.into(),
        })
        .await
    else {
        return String::new();
    };
    client.peers().remember_entities(&full.users, &full.chats);
    let tl::enums::ChatFull::ChannelFull(detail) = full.full_chat else {
        return String::new();
    };
    let Some(linked) = detail.linked_chat_id.filter(|&id| id != 0) else {
        return String::new();
    };
    for chat in full.chats {
        if let tl::enums::Chat::Channel(ch) = chat
            && ch.id == linked
            && let Some(ref uname) = ch.username
            && !uname.is_empty()
        {
            return format!("@{uname}");
        }
    }
    String::new()
}

fn only_messages(page: Vec<tl::enums::Message>) -> Vec<tl::types::Message> {
    page.into_iter()
        .filter_map(|m| match m {
            tl::enums::Message::Message(msg) => Some(msg),
            _ => None,
        })
        .collect()
}

async fn search_messages(
    client: &Client,
    peer: &tl::enums::InputPeer,
    query: &str,
    filter: tl::enums::MessagesFilter,
    limit: usize,
) -> Result<Vec<tl::types::Message>, BotError> {
    let mut found = Vec::new();
    let mut offset = 0i32;
    while found.len() < limit {
        let batch = (limit - found.len()).min(100) as i32;
        let res = client
            .invoke(&tl::functions::messages::Search {
                peer: peer.clone(),
                q: query.to_owned(),
                from_id: None,
                saved_peer_id: None,
                saved_reaction: None,
                top_msg_id: None,
                filter: filter.clone(),
                min_date: 0,
                max_date: 0,
                offset_id: offset,
                add_offset: 0,
                limit: batch,
                max_id: 0,
                min_id: 0,
                hash: 0,
            })
            .await?;
        let page = kit::unpack_messages(res);
        let page_len = page.len();
        let last_id = page.last().map(kit::message_id).unwrap_or(0);
        found.extend(only_messages(page));
        if page_len < batch as usize || last_id <= 0 {
            break;
        }
        offset = last_id;
    }
    Ok(found)
}

async fn history(
    client: &Client,
    peer: &tl::enums::InputPeer,
    offset: i32,
    limit: i32,
) -> Result<Vec<tl::types::Message>, BotError> {
    let res = client
        .invoke(&tl::functions::messages::GetHistory {
            peer: peer.clone(),
            offset_id: offset,
            offset_date: 0,
            add_offset: 0,
            limit,
            max_id: 0,
            min_id: 0,
            hash: 0,
        })
        .await?;
    Ok(only_messages(kit::unpack_messages(res)))
}

async fn replies(
    client: &Client,
    peer: &tl::enums::InputPeer,
    msg_id: i32,
    limit: i32,
) -> Result<Vec<tl::types::Message>, BotError> {
    let res = client
        .invoke(&tl::functions::messages::GetReplies {
            peer: peer.clone(),
            msg_id,
            offset_id: 0,
            offset_date: 0,
            add_offset: 0,
            limit,
            max_id: 0,
            min_id: 0,
            hash: 0,
        })
        .await?;
    Ok(only_messages(kit::unpack_messages(res)))
}

fn forum_topic(message: &Message) -> i32 {
    let Some(tl::enums::MessageReplyHeader::Header(header)) = &message.raw.reply_to else {
        return 0;
    };
    if !header.forum_topic {
        return 0;
    }
    message.topic_id
}

fn caption(text: &str) -> String {
    if utf16_len(text) <= CAPTION_LIMIT as i32 {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        let s: String = chars.iter().collect();
        if utf16_len(&s) < CAPTION_LIMIT as i32 {
            return format!("{s}…");
        }
        chars.pop();
    }
    "…".into()
}

struct Candidate {
    message: tl::types::Message,
    peer: tl::enums::InputPeer,
}

impl Candidate {
    fn key(&self) -> String {
        format!("{}:{}", peer_id(&self.message.peer_id), self.message.id)
    }
}

fn dedupe(list: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        if seen.insert(item.key()) {
            out.push(item);
        }
    }
    out
}

fn choose(
    list: Vec<Candidate>,
    opts: &Options,
    pick: &(dyn Fn(usize) -> usize + Send + Sync),
) -> Candidate {
    if opts.random || opts.kkp {
        let idx = pick(list.len()).min(list.len() - 1);
        return list.into_iter().nth(idx).unwrap();
    }
    let mut items = list;
    items.sort_by(|a, b| {
        let left = score(&a.message, &opts.query);
        let right = score(&b.message, &opts.query);
        right.cmp(&left).then_with(|| {
            duration(&b.message)
                .partial_cmp(&duration(&a.message))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    items.into_iter().next().unwrap()
}

type UploadFn = Arc<
    dyn for<'a> Fn(&'a Invocation, Candidate, String, bool) -> mibot_bot::BoxFuture<'a, Result<(), CmdError>>
        + Send
        + Sync,
>;

struct Service {
    store: Store<Config>,
    sources: Resolver,
    gap: Duration,
    lookup_gap: Duration,
    pick: Arc<dyn Fn(usize) -> usize + Send + Sync>,
    upload: UploadFn,
    logs: Option<Arc<mibot_logtail::Ring>>,
}

impl Service {
    fn new(a: &App) -> Self {
        let partial = a.root.join("search").join(".partial");
        Self {
            store: kit::new_store(a, "search.json", defaults),
            sources: Resolver::default(),
            gap: Duration::from_millis(750),
            lookup_gap: Duration::from_secs(1),
            pick: Arc::new(|n| {
                if n == 0 {
                    0
                } else {
                    (mibot_bot::random_id().unsigned_abs() as usize) % n
                }
            }),
            upload: Arc::new(move |inv, picked, text, spoiler| {
                let partial = partial.clone();
                Box::pin(async move { reupload(inv, &partial, &picked, &text, spoiler).await })
            }),
            logs: Some(Arc::clone(&a.logs)),
        }
    }

    fn read(&self) -> Result<Config, CmdError> {
        Ok(self.store.read()?)
    }

    async fn handle(&self, inv: &Invocation) -> Result<(), CmdError> {
        let sub = inv.arg(0).to_ascii_lowercase();
        match sub.as_str() {
            "" => inv
                .respond_pages(&html_pages(&help(inv.prefix()), PAGE_LIMIT))
                .await
                .map_err(Into::into),
            "help" | "h" if inv.args.len() == 1 => inv
                .respond_pages(&html_pages(&help(inv.prefix()), PAGE_LIMIT))
                .await
                .map_err(Into::into),
            "add" => self.add(inv, split_handles(inv.raw_after(1))).await,
            "del" => self.remove(inv).await,
            "default" => self.set_default(inv).await,
            "list" => self.list(inv).await,
            "export" => self.export(inv).await,
            "import" => self.import_file(inv).await,
            "ad" => self.ad(inv).await,
            "kkp" => {
                self.search(inv, parse_options(&inv.args[1..], true))
                    .await
            }
            _ => self.search(inv, parse_options(&inv.args, false)).await,
        }
    }

    async fn add(&self, inv: &Invocation, mut values: Vec<String>) -> Result<(), CmdError> {
        if values.is_empty() {
            return Err(kit::usage(inv.prefix(), "so add 频道链接或@用户名…"));
        }
        let mut skipped = 0usize;
        if values.len() > MAX_ADD_PER_CALL {
            skipped = values.len() - MAX_ADD_PER_CALL;
            values.truncate(MAX_ADD_PER_CALL);
        }
        let cfg = self.read()?;
        inv.edit_text(&kit::working("正在添加频道")).await?;

        let mut added: Vec<Channel> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        let mut lookups = 0usize;

        for value in values {
            let exists = cfg.channel_list.iter().any(|c| c.handle == value)
                || added.iter().any(|c| c.handle == value);
            if exists {
                failures.push(format!("{value}：已存在"));
                continue;
            }
            if !valid_handle(&value) {
                failures.push(format!("{value}：{ERR_BAD_HANDLE_TEXT}"));
                continue;
            }
            if !self.sources.has(&value) {
                if lookups > 0 && !kit::sleep(&inv.cancel, self.lookup_gap).await {
                    return Err(BotError::Cancelled.into());
                }
                lookups += 1;
            }
            let found = match self.sources.resolve(&inv.client, &value).await {
                Ok(f) => f,
                Err(err) => {
                    if inv.cancel.is_cancelled() {
                        return Err(BotError::Cancelled.into());
                    }
                    let reason = if kit::is_user_error(err.as_ref()) {
                        err.to_string()
                    } else {
                        "无法访问".to_owned()
                    };
                    kit::opt_log(
                        self.logs.as_ref(),
                        mibot_logtail::Level::Info,
                        "search.add_failed",
                        &[("handle", &value), ("error", &err.to_string())],
                    );
                    failures.push(format!("{value}：{reason}"));
                    continue;
                }
            };
            let title = kit::or_default(&found.title, &value).to_owned();
            let linked = if found.broadcast && !found.megagroup {
                linked_group(&inv.client, &found).await
            } else {
                String::new()
            };
            added.push(Channel {
                title,
                handle: value,
                linked_group: linked,
            });
        }

        let mut applied = 0usize;
        kit::update_ok(&self.store, |cfg| {
            for item in added {
                if cfg.channel_list.iter().any(|c| c.handle == item.handle) {
                    continue;
                }
                if cfg.default_channel.is_none() {
                    cfg.default_channel = Some(item.handle.clone());
                }
                cfg.channel_list.push(item);
                applied += 1;
            }
            cfg.schema_version = 1;
        })?;

        let skipped_note = if skipped > 0 {
            format!("一次最多处理 {MAX_ADD_PER_CALL} 个，另外 {skipped} 个没有处理")
        } else {
            String::new()
        };
        if applied == 0 {
            let mut reason = format!(
                "没有添加任何频道：{}",
                clip_lines(&failures, 5).join("；")
            );
            if !skipped_note.is_empty() {
                reason.push('；');
                reason.push_str(&skipped_note);
            }
            return Err(kit::fail(reason));
        }
        let mut text = kit::feedback("success", &format!("已添加 {applied} 个频道"), "");
        if !failures.is_empty() {
            text.push_str("\n\n⚠️ 以下频道没有添加\n");
            text.push_str(&escape(&clip_lines(&failures, LISTED_LINES).join("\n")));
        }
        if !skipped_note.is_empty() {
            text.push_str("\n\n⚠️ ");
            text.push_str(&escape(&skipped_note));
        }
        inv.respond_pages(&html_pages(&text, PAGE_LIMIT))
            .await
            .map_err(Into::into)
    }

    async fn remove(&self, inv: &Invocation) -> Result<(), CmdError> {
        let raw = inv.raw_after(1);
        if raw.is_empty() {
            return Err(kit::usage(inv.prefix(), "so del 频道|序号…"));
        }
        let mut removed = Vec::new();
        match self.store.update(|cfg| {
            let mut targets = HashSet::new();
            if raw.eq_ignore_ascii_case("all") {
                for item in &cfg.channel_list {
                    targets.insert(item.handle.clone());
                }
            } else {
                for token in split_handles(raw) {
                    if let Ok(num) = token.parse::<usize>()
                        && num > 0
                        && num <= cfg.channel_list.len()
                    {
                        targets.insert(cfg.channel_list[num - 1].handle.clone());
                    } else {
                        targets.insert(token);
                    }
                }
            }
            let mut kept = Vec::with_capacity(cfg.channel_list.len());
            for item in cfg.channel_list.drain(..) {
                if targets.contains(&item.handle) {
                    removed.push(item.title);
                    self.sources.forget(&item.handle);
                } else {
                    kept.push(item);
                }
            }
            if removed.is_empty() {
                return Err(kit::fail("列表里没有指定的频道或序号"));
            }
            cfg.channel_list = kept;
            if let Some(ref def) = cfg.default_channel
                && targets.contains(def)
            {
                cfg.default_channel = cfg.channel_list.first().map(|c| c.handle.clone());
            }
            Ok(())
        }) {
            Ok(()) => {}
            Err(mibot_store::UpdateError::Store(e)) => return Err(e.into()),
            Err(mibot_store::UpdateError::Mutate(e)) => return Err(e),
        }
        let text = format!(
            "{}\n{}",
            kit::feedback("success", &format!("已移除 {} 个频道", removed.len()), ""),
            escape(&format!(
                "• {}",
                clip_lines(&removed, LISTED_LINES).join("\n• ")
            ))
        );
        inv.respond_pages(&html_pages(&text, PAGE_LIMIT))
            .await
            .map_err(Into::into)
    }

    async fn set_default(&self, inv: &Invocation) -> Result<(), CmdError> {
        let raw = inv.raw_after(1).trim().to_owned();
        if raw.is_empty() {
            return Err(kit::usage(inv.prefix(), "so default 频道|d"));
        }
        let prefix = inv.prefix().to_owned();
        let raw_c = raw.clone();
        match self.store.update(|cfg| {
            if raw_c == "d" {
                cfg.default_channel = None;
                return Ok(());
            }
            if !cfg.channel_list.iter().any(|c| c.handle == raw_c) {
                return Err(kit::fail(format!("请先用 {prefix}so add 添加这个频道")));
            }
            cfg.default_channel = Some(raw_c);
            Ok(())
        }) {
            Ok(()) => {}
            Err(mibot_store::UpdateError::Store(e)) => return Err(e.into()),
            Err(mibot_store::UpdateError::Mutate(e)) => return Err(e),
        }
        if raw == "d" {
            inv.edit(&kit::feedback("success", "已取消默认频道", ""))
                .await
                .map_err(Into::into)
        } else {
            inv.edit(&kit::feedback("success", &format!("默认频道已设为 {raw}"), ""))
                .await
                .map_err(Into::into)
        }
    }

    async fn list(&self, inv: &Invocation) -> Result<(), CmdError> {
        let cfg = self.read()?;
        inv.respond_pages(&html_pages(&render_list(&cfg, inv.prefix()), PAGE_LIMIT))
            .await
            .map_err(Into::into)
    }

    async fn export(&self, inv: &Invocation) -> Result<(), CmdError> {
        let cfg = self.read()?;
        if cfg.channel_list.is_empty() {
            return Err(kit::fail("没有可导出的频道"));
        }
        let handles: Vec<&str> = cfg.channel_list.iter().map(|c| c.handle.as_str()).collect();
        let peer = inv.peer_result()?;
        let data = handles.join("\n").into_bytes();
        inv.client
            .send_document(
                &peer,
                &data,
                mibot_bot::MediaOptions {
                    name: "search-channels.txt".into(),
                    mime_type: "text/plain".into(),
                    caption: format!("✅ 已导出 {} 个频道源", handles.len()),
                    topic_id: Some(inv.message.topic_id).filter(|&id| id > 0),
                    force_document: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| kit::fail_with("导出失败", e))?;
        delete_command(inv, self.logs.as_ref()).await;
        Ok(())
    }

    async fn import_file(&self, inv: &Invocation) -> Result<(), CmdError> {
        let Some(reply) = kit::reply_msg(inv).await? else {
            return Err(kit::fail("请回复备份文件"));
        };
        if !matches!(
            reply.raw.media,
            Some(tl::enums::MessageMedia::Document(_))
        ) {
            return Err(kit::fail("请回复备份文件（.txt 文件）"));
        }
        let file = match inv.client.download_media(&reply.raw, MAX_IMPORT_BYTES).await {
            Ok(f) => f,
            Err(BotError::TooLarge(_)) => return Err(kit::fail("备份文件超过 256 KB")),
            Err(BotError::NoMedia) => return Err(kit::fail("请回复备份文件")),
            Err(e) => return Err(kit::fail_with("读取备份文件失败", e)),
        };
        let handles: Vec<String> = String::from_utf8_lossy(&file.data)
            .split('\n')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if handles.is_empty() {
            return Err(kit::fail("备份文件无效"));
        }
        self.add(inv, handles).await
    }

    async fn ad(&self, inv: &Invocation) -> Result<(), CmdError> {
        let action = inv.arg(1).to_ascii_lowercase();
        let words: Vec<String> = inv.args.iter().skip(2).cloned().collect();
        match action.as_str() {
            "list" => {
                let cfg = self.read()?;
                if cfg.ad_filters.is_empty() {
                    return inv
                        .edit("🎞 <b>广告过滤词</b>\n\n还没有过滤词")
                        .await
                        .map_err(Into::into);
                }
                let text = format!(
                    "🎞 <b>广告过滤词</b>\n\n{}",
                    escape(&cfg.ad_filters.join("\n"))
                );
                return inv
                    .respond_pages(&html_pages(&text, PAGE_LIMIT))
                    .await
                    .map_err(Into::into);
            }
            "add" | "del" => {
                if words.is_empty() {
                    return Err(kit::usage(inv.prefix(), &format!("so ad {action} 关键词…")));
                }
            }
            _ => return Err(kit::usage(inv.prefix(), "so ad add|del|list [关键词…]")),
        }
        let mut changed = 0usize;
        kit::update_ok(&self.store, |cfg| {
            if action == "add" {
                for word in &words {
                    if !cfg.ad_filters.contains(word) {
                        cfg.ad_filters.push(word.clone());
                        changed += 1;
                    }
                }
            } else {
                let before = cfg.ad_filters.len();
                cfg.ad_filters.retain(|w| !words.contains(w));
                changed = before - cfg.ad_filters.len();
            }
        })?;
        let verb = if action == "del" { "删除" } else { "添加" };
        inv.edit(&kit::feedback(
            "success",
            &format!("已{verb} {changed} 个广告过滤词"),
            "",
        ))
        .await
        .map_err(Into::into)
    }

    async fn search(&self, inv: &Invocation, opts: Options) -> Result<(), CmdError> {
        if !opts.kkp && opts.query.is_empty() {
            return Err(kit::fail("请输入搜索关键词"));
        }
        let cfg = self.read()?;
        if cfg.channel_list.is_empty() {
            return Err(kit::fail(format!(
                "请先用 {}so add 添加至少一个搜索频道",
                inv.prefix()
            )));
        }
        let progress = if opts.kkp {
            "正在随机挑选视频"
        } else {
            "正在搜索视频"
        };
        inv.edit_text(&kit::working(progress)).await?;

        let mut order: Vec<String> = Vec::new();
        if let Some(ref def) = cfg.default_channel
            && !def.is_empty()
        {
            order.push(def.clone());
        }
        for item in &cfg.channel_list {
            if !order.contains(&item.handle) {
                order.push(item.handle.clone());
            }
        }

        let mut found: Vec<Candidate> = Vec::new();
        let mut albums: HashSet<i64> = HashSet::new();
        for (idx, handle) in order.iter().enumerate() {
            if idx > 0 && !kit::sleep(&inv.cancel, self.gap).await {
                return Err(BotError::Cancelled.into());
            }
            let Some(item) = cfg.channel_list.iter().find(|c| &c.handle == handle) else {
                continue;
            };
            match self
                .channel_videos(inv, item, &opts, &cfg.ad_filters, &mut albums)
                .await
            {
                Ok(videos) => {
                    let non_empty = !videos.is_empty();
                    found.extend(videos);
                    if non_empty && !opts.kkp && !opts.random {
                        break;
                    }
                }
                Err((partial_videos, err)) => {
                    if inv.cancel.is_cancelled() {
                        return Err(BotError::Cancelled.into());
                    }
                    kit::opt_log(
                        self.logs.as_ref(),
                        mibot_logtail::Level::Warn,
                        "search.source_failed",
                        &[("source", handle), ("error", &err.to_string())],
                    );
                    if let Err(pe) = self.prune_if_gone(handle, &err) {
                        kit::opt_log(
                            self.logs.as_ref(),
                            mibot_logtail::Level::Warn,
                            "search.prune_failed",
                            &[("source", handle), ("error", &pe.to_string())],
                        );
                    }
                    let non_empty = !partial_videos.is_empty();
                    found.extend(partial_videos);
                    if non_empty && !opts.kkp && !opts.random {
                        break;
                    }
                }
            }
        }

        let unique = dedupe(found);
        if unique.is_empty() {
            return Err(kit::fail(if opts.kkp {
                "没有找到合适的视频"
            } else {
                "所有频道里都没有找到匹配的视频"
            }));
        }
        let picked = choose(unique, &opts, self.pick.as_ref());
        inv.edit_text(&kit::working("正在发送")).await?;
        self.deliver(inv, picked, &opts).await
    }

    fn prune_if_gone(&self, handle: &str, err: &CmdError) -> Result<(), CmdError> {
        let Some(bot_err) = err.downcast_ref::<BotError>() else {
            return Ok(());
        };
        if !bot_err.is_rpc("USERNAME_NOT_OCCUPIED") && !bot_err.is_rpc("USERNAME_INVALID") {
            return Ok(());
        }
        self.sources.forget(handle);
        kit::update_ok(&self.store, |cfg| {
            cfg.channel_list.retain(|c| c.handle != handle);
            if cfg.default_channel.as_deref() == Some(handle) {
                cfg.default_channel = None;
            }
        })?;
        Ok(())
    }

    async fn channel_videos(
        &self,
        inv: &Invocation,
        item: &Channel,
        opts: &Options,
        filters: &[String],
        albums: &mut HashSet<i64>,
    ) -> Result<Vec<Candidate>, (Vec<Candidate>, CmdError)> {
        let found = self
            .sources
            .resolve(&inv.client, &item.handle)
            .await
            .map_err(|e| (Vec::new(), e))?;
        let pure = |msg: &tl::types::Message| video_document(msg).is_some() && !is_ad(msg, filters);

        if opts.kkp {
            let limit = if found.megagroup { 200 } else { 100 };
            let messages = search_messages(
                &inv.client,
                &found.peer,
                "",
                tl::enums::MessagesFilter::InputMessagesFilterVideo,
                limit,
            )
            .await
            .map_err(|e| (Vec::new(), e.into()))?;
            let list = messages
                .into_iter()
                .filter(|m| {
                    let len = duration(m);
                    pure(m) && (20.0..=180.0).contains(&len)
                })
                .map(|m| Candidate {
                    message: m,
                    peer: found.peer.clone(),
                })
                .collect();
            return Ok(list);
        }

        let mut result = Vec::new();
        if !item.linked_group.is_empty() {
            result.extend(
                self.linked_videos(inv, &item.linked_group, &opts.query, filters)
                    .await,
            );
            if inv.cancel.is_cancelled() {
                return Err((Vec::new(), BotError::Cancelled.into()));
            }
        }
        let messages = match search_messages(
            &inv.client,
            &found.peer,
            &opts.query,
            tl::enums::MessagesFilter::InputMessagesFilterEmpty,
            200,
        )
        .await
        {
            Ok(m) => m,
            Err(e) => return Err((result, e.into())),
        };
        for message in messages {
            if !matches(&message, &opts.query) {
                continue;
            }
            let Some(group) = message.grouped_id else {
                if pure(&message) {
                    result.push(Candidate {
                        message,
                        peer: found.peer.clone(),
                    });
                }
                continue;
            };
            if albums.contains(&group) {
                continue;
            }
            let around = match history(&inv.client, &found.peer, message.id + 10, 20).await {
                Ok(a) => a,
                Err(e) => return Err((result, e.into())),
            };
            let mut album = Vec::new();
            for member in around {
                if member.grouped_id == Some(group) && pure(&member) {
                    album.push(Candidate {
                        message: member,
                        peer: found.peer.clone(),
                    });
                }
            }
            if !album.is_empty() {
                result.extend(album);
                albums.insert(group);
            }
        }
        Ok(result)
    }

    async fn linked_videos(
        &self,
        inv: &Invocation,
        handle: &str,
        query: &str,
        filters: &[String],
    ) -> Vec<Candidate> {
        let log_fail = |err: &str| {
            kit::opt_log(
                self.logs.as_ref(),
                mibot_logtail::Level::Info,
                "search.linked_failed",
                &[("group", handle), ("error", err)],
            );
        };
        let linked = match self.sources.resolve(&inv.client, handle).await {
            Ok(l) => l,
            Err(e) => {
                log_fail(&e.to_string());
                return Vec::new();
            }
        };
        let posts = match search_messages(
            &inv.client,
            &linked.peer,
            query,
            tl::enums::MessagesFilter::InputMessagesFilterEmpty,
            100,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                log_fail(&e.to_string());
                return Vec::new();
            }
        };
        let pure = |msg: &tl::types::Message| video_document(msg).is_some() && !is_ad(msg, filters);
        let mut videos = Vec::new();
        for post in posts {
            if post.replies.is_none() || !matches(&post, query) {
                continue;
            }
            let comments = match replies(&inv.client, &linked.peer, post.id, 100).await {
                Ok(c) => c,
                Err(e) => {
                    log_fail(&e.to_string());
                    return Vec::new();
                }
            };
            for comment in comments {
                if pure(&comment) {
                    videos.push(Candidate {
                        message: comment,
                        peer: linked.peer.clone(),
                    });
                }
            }
            if !videos.is_empty() {
                return videos;
            }
        }
        let messages = match search_messages(
            &inv.client,
            &linked.peer,
            query,
            tl::enums::MessagesFilter::InputMessagesFilterVideo,
            100,
        )
        .await
        {
            Ok(m) => m,
            Err(e) => {
                log_fail(&e.to_string());
                return Vec::new();
            }
        };
        for message in messages {
            if pure(&message) {
                videos.push(Candidate {
                    message,
                    peer: linked.peer.clone(),
                });
            }
        }
        videos
    }

    async fn deliver(
        &self,
        inv: &Invocation,
        picked: Candidate,
        opts: &Options,
    ) -> Result<(), CmdError> {
        if !opts.spoiler && !picked.message.noforwards {
            let target = inv.peer_result()?;
            let topic = Some(forum_topic(&inv.message)).filter(|&id| id > 0);
            match inv
                .client
                .forward_messages(&picked.peer, &target, &[picked.message.id], topic)
                .await
            {
                Ok(_) => {
                    delete_command(inv, self.logs.as_ref()).await;
                    return Ok(());
                }
                Err(e) if e.is_rpc("CHAT_FORWARDS_RESTRICTED") => {}
                Err(e) => return Err(kit::wrap_fail("转发视频失败", Box::new(e))),
            }
        }
        let text = if opts.query.is_empty() {
            picked.message.message.clone()
        } else {
            opts.query.clone()
        };
        (self.upload)(inv, picked, text, opts.spoiler).await?;
        delete_command(inv, self.logs.as_ref()).await;
        Ok(())
    }
}

struct TempVideoFile(PathBuf);
impl Drop for TempVideoFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

async fn reupload(
    inv: &Invocation,
    partial: &Path,
    picked: &Candidate,
    text: &str,
    spoiler: bool,
) -> Result<(), CmdError> {
    let Some(media_src) = source_of(&picked.message) else {
        return Err(kit::fail("这条视频已经不可用"));
    };
    let Some((doc, video)) = video_document(&picked.message) else {
        return Err(kit::fail("这条视频已经不可用"));
    };
    if doc.size > MAX_MEDIA_BYTES {
        return Err(kit::fail(format!(
            "视频 {}，超过 2 GB 上限",
            kit::format_bytes(doc.size)
        )));
    }
    fs::create_dir_all(partial)?;
    let path = partial.join(format!(
        "video-{}-{}",
        std::process::id(),
        mibot_bot::random_id().unsigned_abs()
    ));
    let _guard = TempVideoFile(path.clone());
    inv.edit_text(&kit::working(&format!(
        "正在下载视频 {}",
        kit::format_bytes(doc.size)
    )))
    .await?;

    {
        let std_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(std_file);
        inv.client
            .download_to(&media_src, &mut async_file)
            .await
            .map_err(|e| kit::fail_with("下载视频失败", e))?;
        use tokio::io::AsyncWriteExt;
        async_file.flush().await?;
    }
    let meta = fs::metadata(&path)?;
    if meta.len() == 0 {
        return Err(kit::fail("视频下载为空"));
    }

    inv.edit_text(&kit::working("正在上传视频")).await?;
    let uploaded = inv
        .client
        .upload_path("video.mp4", &path)
        .await
        .map_err(|e| kit::fail_with("上传视频失败", e))?;
    let target = inv.peer_result()?;
    let mime_type = if doc.mime_type.starts_with("video/") {
        doc.mime_type.clone()
    } else {
        "video/mp4".to_owned()
    };
    let attrs: Vec<tl::enums::DocumentAttribute> = vec![
        tl::types::DocumentAttributeVideo {
            round_message: false,
            supports_streaming: true,
            nosound: false,
            duration: video.duration,
            w: video.w,
            h: video.h,
            preload_prefix_size: None,
            video_start_ts: None,
            video_codec: None,
        }
        .into(),
        tl::types::DocumentAttributeFilename {
            file_name: "video.mp4".into(),
        }
        .into(),
    ];
    let media: tl::enums::InputMedia = tl::types::InputMediaUploadedDocument {
        nosound_video: false,
        force_file: false,
        spoiler,
        file: uploaded,
        thumb: None,
        mime_type,
        attributes: attrs,
        stickers: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    }
    .into();
    let topic = forum_topic(&inv.message);
    let reply_target = if inv.message.reply_to_id == topic {
        0
    } else {
        inv.message.reply_to_id
    };
    inv.client
        .send_media(
            &target,
            media,
            mibot_bot::MediaOptions {
                caption: caption(text),
                reply_to: Some(reply_target).filter(|&id| id > 0),
                topic_id: Some(topic).filter(|&id| id > 0),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| kit::fail_with("发送视频失败", e))?;
    Ok(())
}

async fn delete_command(inv: &Invocation, logs: Option<&Arc<mibot_logtail::Ring>>) {
    if let Err(err) = inv.delete().await
        && !inv.cancel.is_cancelled()
    {
        kit::opt_log(
            logs,
            mibot_logtail::Level::Warn,
            "search.cleanup_failed",
            &[("error", &err.to_string())],
        );
    }
}

fn render_list(cfg: &Config, prefix: &str) -> String {
    if cfg.channel_list.is_empty() {
        return format!(
            "🎞 <b>搜索频道</b>\n\n还没有添加频道，用 {} 添加",
            code(&format!("{prefix}so add"))
        );
    }
    let lines: Vec<String> = cfg
        .channel_list
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let mut line = format!(
                "{}. {} · {}",
                idx + 1,
                escape(&item.title),
                code(&item.handle)
            );
            if cfg.default_channel.as_deref() == Some(item.handle.as_str()) {
                line.push_str("（默认）");
            }
            line
        })
        .collect();
    format!("🎞 <b>搜索频道</b>\n\n{}", lines.join("\n"))
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let item = |args: &str, text: &str| {
        format!(
            "• <code>{p}so {}</code> {text}\n",
            escape(args)
        )
    };
    format!(
        "🎞 <b>频道视频搜索</b>\n\n在添加的频道里找视频，发到当前对话。\n\n\
        <b>搜索</b>\n{}{}\
        • 加 <code>-s</code> 下载后以防剧透方式发送，加 <code>-r</code> 从匹配结果里随机选\n\n\
        <b>频道源</b>\n{}{}{}{}{}{}\n\
        <b>广告过滤</b>\n{}{}{}\n\
        先转发原消息；对话禁止转发或用了 -s 时下载再发送，上限 2 GB。<code>{p}search</code> 与 <code>{p}so</code> 用法相同。",
        item("关键词", "按关键词搜索，不限大小和时长，先搜默认频道"),
        item("kkp", "随机挑一段 20 秒到 3 分钟的视频"),
        item("add 频道链接或@用户名…", "添加频道、群组或讨论组，多个用空格或 \\ 分隔"),
        item("del 频道|序号…", "移除频道源，del all 全部移除"),
        item("default 频道", "设为默认频道，default d 取消默认"),
        item("list", "查看频道源"),
        item("export", "导出频道源文件"),
        item("import", "回复备份文件导入"),
        item("ad add 关键词…", "添加过滤词"),
        item("ad del 关键词…", "删除过滤词"),
        item("ad list", "查看过滤词"),
    )
}

pub fn register(a: &Arc<App>) {
    let svc = Arc::new(Service::new(a));
    let mut cmd = kit::make_cmd(
        "so",
        &["search"],
        group::MEDIA,
        "从频道搜索并发送视频",
        "关键词|kkp [-s] [-r]",
        help,
        move |inv| {
            let svc = Arc::clone(&svc);
            Box::pin(async move { svc.handle(&inv).await })
        },
    );
    cmd.free_text = true;
    cmd.timeout = Some(Duration::from_secs(30 * 60));
    a.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{PeerCache, RpcTransport, SentRecorder};
    use tokio_util::sync::CancellationToken;

    struct TempDirGuard(PathBuf);
    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "mibot-{prefix}-{}-{}",
                std::process::id(),
                mibot_bot::random_id().unsigned_abs()
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn video(peer: i64, id: i32, text: &str, name: &str, seconds: f64) -> tl::types::Message {
        let mut msg =
            kit::blank_tl_message(id, tl::types::PeerChannel { channel_id: peer }.into());
        msg.message = text.to_owned();
        msg.date = 1;
        msg.media = Some(
            tl::types::MessageMediaDocument {
                nopremium: false,
                spoiler: false,
                video: true,
                round: false,
                voice: false,
                document: Some(
                    tl::types::Document {
                        id: i64::from(id),
                        access_hash: 1,
                        file_reference: vec![1],
                        date: 0,
                        mime_type: "video/mp4".into(),
                        size: 3000,
                        thumbs: None,
                        video_thumbs: None,
                        dc_id: 2,
                        attributes: vec![
                            tl::types::DocumentAttributeVideo {
                                round_message: false,
                                supports_streaming: false,
                                nosound: false,
                                duration: seconds,
                                w: 640,
                                h: 360,
                                preload_prefix_size: None,
                                video_start_ts: None,
                                video_codec: None,
                            }
                            .into(),
                            tl::types::DocumentAttributeFilename {
                                file_name: name.into(),
                            }
                            .into(),
                        ],
                    }
                    .into(),
                ),
                alt_documents: None,
                video_cover: None,
                video_timestamp: None,
                ttl_seconds: None,
            }
            .into(),
        );
        msg
    }

    #[test]
    fn test_parse_options() {
        assert_eq!(
            parse_options(
                &["海贼王".into(), "-S".into(), "第一集".into()],
                false
            ),
            Options {
                spoiler: true,
                random: false,
                kkp: false,
                query: "海贼王 第一集".into()
            }
        );
        assert_eq!(
            parse_options(&["-r".into(), "kkp".into(), "-s".into()], false),
            Options {
                spoiler: true,
                random: true,
                kkp: true,
                query: String::new()
            }
        );
        assert_eq!(
            parse_options(&["KKP".into()], false),
            Options {
                spoiler: false,
                random: false,
                kkp: true,
                query: String::new()
            }
        );
        assert_eq!(
            parse_options(&["-s".into()], true),
            Options {
                spoiler: true,
                random: false,
                kkp: true,
                query: String::new()
            }
        );
        assert_eq!(parse_options(&[], false), Options::default());
    }

    #[test]
    fn test_matching() {
        assert_eq!(
            normalize("  One_Piece-EP.01 | 1080P#HD "),
            "one piece ep 01 1080p hd"
        );
        let msg = video(1, 1, "【海贼王】第 1 集", "One.Piece.EP01.mp4", 60.0);
        for (q, want) in [
            ("海贼王", true),
            ("one piece", true),
            ("piece one", true),
            ("ep01", true),
            ("EP 01", true),
            ("海贼王 第 2 集", false),
            ("naruto", false),
            ("one piece ep", true),
        ] {
            assert_eq!(matches(&msg, q), want, "q={q}");
        }
        assert_eq!(score(&msg, "one piece"), 100);
        assert_eq!(score(&msg, "海贼王"), 50);
        assert!(is_ad(&video(1, 2, "加我 VPN 翻墙", "", 30.0), &["vpn".into()]));
        let defaults_list: Vec<String> = DEFAULT_FILTERS.iter().map(|s| (*s).to_owned()).collect();
        assert!(is_ad(&video(1, 3, "", "推广.mp4", 30.0), &defaults_list));
        assert!(!is_ad(&msg, &[String::new()]));

        let mut web = kit::blank_tl_message(9, tl::types::PeerChannel { channel_id: 1 }.into());
        web.media = Some(
            tl::types::MessageMediaWebPage {
                force_large_media: false,
                force_small_media: false,
                manual: false,
                safe: false,
                webpage: tl::enums::WebPage::Empty(tl::types::WebPageEmpty {
                    id: 1,
                    url: None,
                }),
            }
            .into(),
        );
        assert!(video_document(&web).is_none());
    }

    #[test]
    fn test_parse_handle_caption_split_choose_unicode_valid() {
        for (val, want) in [
            ("@Channel", ("Channel", "", "")),
            ("channel", ("channel", "", "")),
            ("https://t.me/channel/123", ("channel", "", "")),
            ("t.me/s/channel", ("channel", "", "")),
            ("https://www.telegram.me/@channel", ("channel", "", "")),
            ("https://t.me/+AbCdEf", ("", "AbCdEf", "")),
            ("https://t.me/joinchat/AbCdEf", ("", "AbCdEf", "")),
            ("-1001234567890", ("", "", "-1001234567890")),
        ] {
            let (u, inv, id) = parse_handle(val);
            assert_eq!((u.as_str(), inv.as_str(), id.as_str()), want, "val={val}");
        }

        let long = "😀".repeat(600);
        let got_cap = caption(&long);
        assert!(utf16_len(&got_cap) <= CAPTION_LIMIT as i32 && got_cap.ends_with('…'));
        assert_eq!(caption("短"), "短");
        assert_eq!(
            split_handles(" @a \\ @b\n@c  https://t.me/d "),
            vec!["@a", "@b", "@c", "https://t.me/d"]
        );

        let list = || {
            vec![
                Candidate {
                    message: video(1, 1, "", "other.mp4", 100.0),
                    peer: tl::enums::InputPeer::PeerSelf,
                },
                Candidate {
                    message: video(1, 2, "one piece", "x.mp4", 50.0),
                    peer: tl::enums::InputPeer::PeerSelf,
                },
                Candidate {
                    message: video(1, 3, "one piece", "one piece.mp4", 10.0),
                    peer: tl::enums::InputPeer::PeerSelf,
                },
                Candidate {
                    message: video(1, 4, "one piece", "x.mp4", 80.0),
                    peer: tl::enums::InputPeer::PeerSelf,
                },
            ]
        };
        assert_eq!(
            choose(
                list(),
                &Options {
                    query: "one piece".into(),
                    ..Default::default()
                },
                &|_| 0
            )
            .message
            .id,
            3
        );
        assert_eq!(
            choose(
                list()[..2].iter().map(|c| Candidate {
                    message: c.message.clone(),
                    peer: c.peer.clone(),
                }).collect(),
                &Options {
                    query: "one piece".into(),
                    ..Default::default()
                },
                &|_| 0
            )
            .message
            .id,
            2
        );
        let l = list();
        assert_eq!(
            choose(
                vec![
                    Candidate {
                        message: l[1].message.clone(),
                        peer: tl::enums::InputPeer::PeerSelf
                    },
                    Candidate {
                        message: l[3].message.clone(),
                        peer: tl::enums::InputPeer::PeerSelf
                    },
                ],
                &Options {
                    query: "one piece".into(),
                    ..Default::default()
                },
                &|_| 0
            )
            .message
            .id,
            4
        );
        assert_eq!(
            choose(
                list(),
                &Options {
                    random: true,
                    ..Default::default()
                },
                &|n| n - 1
            )
            .message
            .id,
            4
        );

        for (v, w) in [
            ("@first", true),
            ("first_ch", true),
            ("https://t.me/first/12", true),
            ("https://t.me/+AbCdEf12", true),
            ("-1001234567890", true),
            ("@abc", false),
            ("1abcde", false),
            ("中文频道", false),
            ("hello world", false),
            ("https://t.me/", false),
            ("https://t.me/+a", false),
            ("a.b.c.d", false),
        ] {
            assert_eq!(valid_handle(v), w, "valid_handle({v})");
        }

        assert_eq!(normalize("One\u{00a0}Piece\u{3000}第1集"), "one piece 第1集");
        assert!(matches(
            &video(1, 1, "海贼王\u{3000}第1集", "", 10.0),
            "海贼王 第1集"
        ));
        assert!(matches(&video(1, 1, "", "abc\u{3000}123.mp4", 10.0), "abc123"));
        assert_eq!(split_handles("@a\u{3000}@b"), vec!["@a", "@b"]);
    }

    #[test]
    fn test_config_compatible() {
        let raw = r#"{"schemaVersion":1,"defaultChannel":"@a","channelList":[{"title":"A","handle":"@a","linkedGroup":"@a_chat"},{"title":"B","handle":"@b"}],"adFilters":["x"]}"#;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.default_channel.as_deref(), Some("@a"));
        assert_eq!(cfg.channel_list[0].linked_group, "@a_chat");
        assert_eq!(cfg.ad_filters.len(), 1);
        cfg.default_channel = None;
        let encoded = serde_json::to_string(&cfg).unwrap();
        assert!(encoded.contains(r#""defaultChannel":null"#));
        let idx_b = encoded.find(r#""B""#).unwrap();
        assert!(!encoded[idx_b..].contains("linkedGroup"));
        let d = defaults();
        assert_eq!(d.ad_filters.len(), DEFAULT_FILTERS.len());
        assert!(d.default_channel.is_none());
    }

    struct FakeTg {
        channels: HashMap<String, tl::types::Channel>,
        messages: Mutex<HashMap<i64, Vec<tl::types::Message>>>,
        comments: Mutex<HashMap<i32, Vec<tl::types::Message>>>,
        linked: HashMap<i64, i64>,
        failed_search_id: Mutex<i64>,
        restrict: Mutex<bool>,
        forward_err: Mutex<Option<String>>,
        reply: Mutex<Option<tl::types::Message>>,
        edits: Mutex<Vec<String>>,
        content: Mutex<Vec<u8>>,
        calls: Mutex<Vec<String>>,
        uploaded: Mutex<Vec<u8>>,
        sent_media: Mutex<Vec<tl::functions::messages::SendMedia>>,
    }

    impl FakeTg {
        fn new() -> Self {
            let make_ch = |id: i64, name: &str, megagroup: bool| {
                let mut ch = kit::blank_tl_channel(id, &name.to_ascii_uppercase());
                ch.access_hash = Some(id * 10);
                ch.username = Some(name.to_owned());
                ch.broadcast = !megagroup;
                ch.megagroup = megagroup;
                ch
            };
            Self {
                channels: HashMap::from([
                    ("first".into(), make_ch(10, "first", false)),
                    ("second".into(), make_ch(20, "second", false)),
                    ("talk".into(), make_ch(30, "talk", true)),
                ]),
                messages: Mutex::new(HashMap::new()),
                comments: Mutex::new(HashMap::new()),
                linked: HashMap::from([(10, 30)]),
                failed_search_id: Mutex::new(0),
                restrict: Mutex::new(false),
                forward_err: Mutex::new(None),
                reply: Mutex::new(None),
                edits: Mutex::new(Vec::new()),
                content: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                uploaded: Mutex::new(Vec::new()),
                sent_media: Mutex::new(Vec::new()),
            }
        }

        fn channel_by_id(&self, id: i64) -> Option<tl::enums::Chat> {
            self.channels
                .values()
                .find(|c| c.id == id)
                .cloned()
                .map(tl::enums::Chat::Channel)
        }

        fn page(
            list: &[tl::types::Message],
            offset: i32,
            limit: i32,
            keep: impl Fn(&tl::types::Message) -> bool,
        ) -> Vec<tl::enums::Message> {
            let mut sorted = list.to_vec();
            sorted.sort_by_key(|m| std::cmp::Reverse(m.id));
            sorted
                .into_iter()
                .filter(|m| (offset == 0 || m.id < offset) && keep(m))
                .take(limit.max(0) as usize)
                .map(tl::enums::Message::Message)
                .collect()
        }
    }

    fn peer_channel(peer: &tl::enums::InputPeer) -> i64 {
        match peer {
            tl::enums::InputPeer::Channel(c) => c.channel_id,
            _ => 0,
        }
    }

    fn rpc_err(name: &str) -> InvocationError {
        InvocationError::Rpc(RpcError {
            code: 400,
            name: name.into(),
            value: None,
            caused_by: None,
        })
    }

    impl RpcTransport for FakeTg {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            cid: u32,
            body: Vec<u8>,
        ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                let payload = &body[4..];
                match cid {
                    tl::functions::contacts::ResolveUsername::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::contacts::ResolveUsername::from_bytes(payload).unwrap();
                        self.calls
                            .lock()
                            .unwrap()
                            .push(format!("resolve {}", req.username));
                        let Some(ch) = self.channels.get(&req.username.to_ascii_lowercase()) else {
                            return Err(rpc_err("USERNAME_NOT_OCCUPIED"));
                        };
                        Ok(tl::enums::contacts::ResolvedPeer::Peer(
                            tl::types::contacts::ResolvedPeer {
                                peer: tl::types::PeerChannel { channel_id: ch.id }.into(),
                                chats: vec![ch.clone().into()],
                                users: vec![],
                            },
                        )
                        .to_bytes())
                    }
                    tl::functions::channels::GetFullChannel::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::channels::GetFullChannel::from_bytes(payload).unwrap();
                        let tl::enums::InputChannel::Channel(ch) = req.channel else {
                            return Err(rpc_err("CHANNEL_INVALID"));
                        };
                        let linked = self.linked.get(&ch.channel_id).copied();
                        let mut chats = Vec::new();
                        if let Some(c) = self.channel_by_id(ch.channel_id) {
                            chats.push(c);
                        }
                        if let Some(lid) = linked
                            && let Some(c) = self.channel_by_id(lid)
                        {
                            chats.push(c);
                        }
                        let full = tl::types::ChannelFull {
                            can_view_participants: false,
                            can_set_username: false,
                            can_set_stickers: false,
                            hidden_prehistory: false,
                            can_set_location: false,
                            has_scheduled: false,
                            can_view_stats: false,
                            blocked: false,
                            can_delete_channel: false,
                            antispam: false,
                            participants_hidden: false,
                            translations_disabled: false,
                            stories_pinned_available: false,
                            view_forum_as_messages: false,
                            restricted_sponsored: false,
                            can_view_revenue: false,
                            paid_media_allowed: false,
                            can_view_stars_revenue: false,
                            paid_reactions_available: false,
                            stargifts_available: false,
                            paid_messages_available: false,
                            has_welcome_messages: false,
                            id: ch.channel_id,
                            about: String::new(),
                            participants_count: None,
                            admins_count: None,
                            kicked_count: None,
                            banned_count: None,
                            online_count: None,
                            read_inbox_max_id: 0,
                            read_outbox_max_id: 0,
                            unread_count: 0,
                            chat_photo: tl::enums::Photo::Empty(tl::types::PhotoEmpty { id: 0 }),
                            notify_settings: tl::enums::PeerNotifySettings::Settings(
                                tl::types::PeerNotifySettings {
                                    show_previews: None,
                                    silent: None,
                                    mute_until: None,
                                    ios_sound: None,
                                    android_sound: None,
                                    other_sound: None,
                                    stories_muted: None,
                                    stories_hide_sender: None,
                                    stories_ios_sound: None,
                                    stories_android_sound: None,
                                    stories_other_sound: None,
                                },
                            ),
                            exported_invite: None,
                            bot_info: vec![],
                            migrated_from_chat_id: None,
                            migrated_from_max_id: None,
                            pinned_msg_id: None,
                            stickerset: None,
                            available_min_id: None,
                            folder_id: None,
                            linked_chat_id: linked,
                            location: None,
                            slowmode_seconds: None,
                            slowmode_next_send_date: None,
                            stats_dc: None,
                            pts: 0,
                            call: None,
                            ttl_period: None,
                            pending_suggestions: None,
                            groupcall_default_join_as: None,
                            theme_emoticon: None,
                            requests_pending: None,
                            recent_requesters: None,
                            default_send_as: None,
                            available_reactions: None,
                            reactions_limit: None,
                            stories: None,
                            wallpaper: None,
                            boosts_applied: None,
                            boosts_unrestrict: None,
                            emojiset: None,
                            bot_verification: None,
                            stargifts_count: None,
                            send_paid_messages_stars: None,
                            main_tab: None,
                            guard_bot_id: None,
                            share_theme: None,
                        };
                        Ok(tl::enums::messages::ChatFull::Full(tl::types::messages::ChatFull {
                            full_chat: full.into(),
                            chats,
                            users: vec![],
                        })
                        .to_bytes())
                    }
                    tl::functions::messages::Search::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::Search::from_bytes(payload).unwrap();
                        let id = peer_channel(&req.peer);
                        let is_vid = matches!(
                            req.filter,
                            tl::enums::MessagesFilter::InputMessagesFilterVideo
                        );
                        self.calls.lock().unwrap().push(format!(
                            "search {id} q={:?} video={is_vid} limit={}",
                            req.q, req.limit
                        ));
                        if id == *self.failed_search_id.lock().unwrap() {
                            return Err(rpc_err("CHANNEL_PRIVATE"));
                        }
                        let map = self.messages.lock().unwrap();
                        let list = map.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                        let q_low = req.q.to_lowercase();
                        let msgs = Self::page(list, req.offset_id, req.limit, |m| {
                            if is_vid && video_document(m).is_none() {
                                return false;
                            }
                            q_low.is_empty()
                                || format!("{} {}", m.message, file_name(m))
                                    .to_lowercase()
                                    .contains(&q_low)
                        });
                        Ok(kit::tl_messages(msgs, vec![], vec![]).to_bytes())
                    }
                    tl::functions::messages::GetHistory::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::messages::GetHistory::from_bytes(payload).unwrap();
                        let id = peer_channel(&req.peer);
                        self.calls
                            .lock()
                            .unwrap()
                            .push(format!("history {id} offset={}", req.offset_id));
                        let map = self.messages.lock().unwrap();
                        let list = map.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                        let msgs = Self::page(list, req.offset_id, req.limit, |_| true);
                        Ok(kit::tl_messages(msgs, vec![], vec![]).to_bytes())
                    }
                    tl::functions::messages::GetReplies::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::messages::GetReplies::from_bytes(payload).unwrap();
                        self.calls
                            .lock()
                            .unwrap()
                            .push(format!("replies {}", req.msg_id));
                        let map = self.comments.lock().unwrap();
                        let list = map.get(&req.msg_id).map(Vec::as_slice).unwrap_or(&[]);
                        let msgs = Self::page(list, 0, req.limit, |_| true);
                        Ok(kit::tl_messages(msgs, vec![], vec![]).to_bytes())
                    }
                    tl::functions::messages::ForwardMessages::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::messages::ForwardMessages::from_bytes(payload).unwrap();
                        self.calls.lock().unwrap().push(format!(
                            "forward {}#{:?}",
                            peer_channel(&req.from_peer),
                            req.id
                        ));
                        if *self.restrict.lock().unwrap() {
                            return Err(rpc_err("CHAT_FORWARDS_RESTRICTED"));
                        }
                        if let Some(ref e) = *self.forward_err.lock().unwrap() {
                            return Err(rpc_err(e));
                        }
                        Ok(tl::enums::Updates::Updates(tl::types::Updates {
                            updates: vec![],
                            users: vec![],
                            chats: vec![],
                            date: 0,
                            seq: 0,
                        })
                        .to_bytes())
                    }
                    tl::functions::messages::DeleteMessages::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::messages::DeleteMessages::from_bytes(payload).unwrap();
                        self.calls
                            .lock()
                            .unwrap()
                            .push(format!("delete {:?}", req.id));
                        Ok(tl::enums::messages::AffectedMessages::Messages(
                            tl::types::messages::AffectedMessages {
                                pts: 1,
                                pts_count: 1,
                            },
                        )
                        .to_bytes())
                    }
                    tl::functions::messages::EditMessage::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::messages::EditMessage::from_bytes(payload).unwrap();
                        self.edits
                            .lock()
                            .unwrap()
                            .push(req.message.unwrap_or_default());
                        Ok(tl::enums::Updates::Updates(tl::types::Updates {
                            updates: vec![],
                            users: vec![],
                            chats: vec![],
                            date: 0,
                            seq: 0,
                        })
                        .to_bytes())
                    }
                    tl::functions::messages::GetMessages::CONSTRUCTOR_ID => {
                        let found: Vec<tl::enums::Message> = self
                            .reply
                            .lock()
                            .unwrap()
                            .clone()
                            .into_iter()
                            .map(tl::enums::Message::Message)
                            .collect();
                        Ok(kit::tl_messages(found, vec![], vec![]).to_bytes())
                    }
                    tl::functions::upload::GetFile::CONSTRUCTOR_ID => {
                        let req = tl::functions::upload::GetFile::from_bytes(payload).unwrap();
                        let data = self.content.lock().unwrap();
                        let start = (req.offset.max(0) as usize).min(data.len());
                        let end = (start + req.limit.max(0) as usize).min(data.len());
                        Ok(tl::enums::upload::File::File(tl::types::upload::File {
                            r#type: tl::enums::storage::FileType::FileMp4,
                            mtime: 0,
                            bytes: data[start..end].to_vec(),
                        })
                        .to_bytes())
                    }
                    tl::functions::upload::SaveFilePart::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::upload::SaveFilePart::from_bytes(payload).unwrap();
                        self.uploaded.lock().unwrap().extend_from_slice(&req.bytes);
                        Ok(true.to_bytes())
                    }
                    tl::functions::messages::SendMedia::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::SendMedia::from_bytes(payload).unwrap();
                        self.calls.lock().unwrap().push("send-media".into());
                        self.sent_media.lock().unwrap().push(req);
                        Ok(tl::enums::Updates::Updates(tl::types::Updates {
                            updates: vec![],
                            users: vec![],
                            chats: vec![],
                            date: 0,
                            seq: 0,
                        })
                        .to_bytes())
                    }
                    tl::functions::messages::SendMessage::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::messages::SendMessage::from_bytes(payload).unwrap();
                        self.edits.lock().unwrap().push(req.message);
                        Ok(tl::enums::Updates::UpdateShortSentMessage(
                            tl::types::UpdateShortSentMessage {
                                out: true,
                                id: 99,
                                pts: 1,
                                pts_count: 1,
                                date: 0,
                                media: None,
                                entities: None,
                                ttl_period: None,
                            },
                        )
                        .to_bytes())
                    }
                    _ => Err(InvocationError::Dropped("UNEXPECTED_REQUEST")),
                }
            })
        }
    }

    struct Harness {
        _tmp: TempDirGuard,
        fake: Arc<FakeTg>,
        s: Service,
        uploads: Arc<Mutex<Vec<String>>>,
    }

    fn new_harness(fake: Arc<FakeTg>, cfg: Config) -> Harness {
        let tmp = TempDirGuard::new("search");
        let store = Store::new(tmp.0.join("search.json"), defaults);
        let uploads = Arc::new(Mutex::new(Vec::new()));
        let u_clone = Arc::clone(&uploads);
        let s = Service {
            store,
            sources: Resolver::default(),
            gap: Duration::ZERO,
            lookup_gap: Duration::ZERO,
            pick: Arc::new(|_| 0),
            upload: Arc::new(move |_inv, picked, text, spoiler| {
                let u = Arc::clone(&u_clone);
                Box::pin(async move {
                    u.lock().unwrap().push(format!(
                        "{} text={text:?} spoiler={spoiler}",
                        picked.message.id
                    ));
                    Ok(())
                })
            }),
            logs: None,
        };
        kit::update_ok(&s.store, |c| *c = cfg).unwrap();
        Harness {
            _tmp: tmp,
            fake,
            s,
            uploads,
        }
    }

    impl Harness {
        async fn run(&self, args: &[&str]) -> mibot_command::CmdResult {
            self.run_replying(0, args).await
        }

        async fn run_replying(&self, reply_to: i32, args: &[&str]) -> mibot_command::CmdResult {
            let peers = Arc::new(PeerCache::new());
            peers.set_self(1);
            let client = Arc::new(Client::from_transport(
                self.fake.clone(),
                peers,
                Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
            ));
            let text = format!(".so {}", args.join(" "));
            let mut msg = kit::blank_bot_message(500, tl::types::PeerUser { user_id: 1 }.into());
            msg.out = true;
            msg.saved = true;
            msg.reply_to_id = reply_to;
            msg.text = text.clone();
            let inv = Invocation {
                client,
                registry: Arc::new(mibot_command::Registry::new()),
                message: msg,
                prefix: ".".into(),
                command: "so".into(),
                args: args.iter().map(|s| (*s).to_owned()).collect(),
                raw: args.join(" "),
                cancel: CancellationToken::new(),
                foreign: false,
            };
            self.s.handle(&inv).await
        }
    }

    #[tokio::test]
    async fn test_search_forwards_and_discussion_fallback() {
        let fake = Arc::new(FakeTg::new());
        let mut post = kit::blank_tl_message(7, tl::types::PeerChannel { channel_id: 30 }.into());
        post.message = "海贼王 第1集 讨论".into();
        post.date = 1;
        post.replies = Some(tl::types::MessageReplies {
            comments: true,
            replies: 2,
            replies_pts: 1,
            recent_repliers: None,
            channel_id: Some(30),
            max_id: None,
            read_max_id: None,
        }.into());
        fake.messages.lock().unwrap().insert(30, vec![post.clone()]);
        fake.comments.lock().unwrap().insert(
            7,
            vec![
                video(30, 8, "推广 海贼王", "", 60.0),
                video(30, 9, "", "海贼王.mp4", 90.0),
            ],
        );
        fake.messages.lock().unwrap().insert(
            10,
            vec![
                video(10, 1, "海贼王 第1集", "", 30.0),
                video(10, 2, "海贼王 VPN", "", 30.0),
            ],
        );
        fake.messages
            .lock()
            .unwrap()
            .insert(20, vec![video(20, 1, "海贼王", "", 30.0)]);
        let h = new_harness(
            Arc::clone(&fake),
            Config {
                schema_version: 1,
                default_channel: Some("@first".into()),
                channel_list: vec![
                    Channel {
                        title: "Second".into(),
                        handle: "@second".into(),
                        linked_group: String::new(),
                    },
                    Channel {
                        title: "First".into(),
                        handle: "@first".into(),
                        linked_group: "@talk".into(),
                    },
                ],
                ad_filters: vec!["推广".into(), "vpn".into()],
            },
        );
        h.run(&["海贼王"]).await.unwrap();
        let calls = fake.calls.lock().unwrap().join("\n");
        assert!(!calls.contains("search 20"));
        assert!(
            calls.contains("forward 30#[9]") && calls.ends_with("delete [500]"),
            "unexpected calls:\n{calls}"
        );

        // Discussion survives channel search failure
        let fake2 = Arc::new(FakeTg::new());
        *fake2.failed_search_id.lock().unwrap() = 10;
        fake2.messages.lock().unwrap().insert(30, vec![post]);
        fake2
            .comments
            .lock()
            .unwrap()
            .insert(7, vec![video(30, 9, "", "海贼王.mp4", 90.0)]);
        let h2 = new_harness(
            Arc::clone(&fake2),
            Config {
                schema_version: 1,
                default_channel: None,
                channel_list: vec![Channel {
                    title: String::new(),
                    handle: "@first".into(),
                    linked_group: "@talk".into(),
                }],
                ad_filters: vec![],
            },
        );
        h2.run(&["海贼王"]).await.unwrap();
        assert!(fake2.calls.lock().unwrap().contains(&"forward 30#[9]".to_owned()));
    }

    #[tokio::test]
    async fn test_search_album_kkp_fallback_reupload_manage() {
        let fake = Arc::new(FakeTg::new());
        let mut album = vec![
            video(10, 11, "合集 海贼王", "", 5.0),
            video(10, 12, "", "a.mp4", 200.0),
            video(10, 13, "", "b.mp4", 100.0),
        ];
        for m in &mut album {
            m.grouped_id = Some(77);
        }
        album.push(video(10, 14, "别的", "", 60.0));
        fake.messages.lock().unwrap().insert(10, album);
        fake.messages.lock().unwrap().insert(
            20,
            vec![
                video(20, 3, "海贼王", "", 15.0),
                video(20, 4, "", "", 181.0),
                video(20, 5, "", "", 120.0),
            ],
        );
        let mut h = new_harness(
            Arc::clone(&fake),
            Config {
                schema_version: 1,
                default_channel: None,
                channel_list: vec![
                    Channel {
                        title: "First".into(),
                        handle: "@first".into(),
                        linked_group: String::new(),
                    },
                    Channel {
                        title: "Second".into(),
                        handle: "@second".into(),
                        linked_group: String::new(),
                    },
                ],
                ad_filters: vec![],
            },
        );
        let picked_counts = Arc::new(Mutex::new(Vec::new()));
        let pc = Arc::clone(&picked_counts);
        h.s.pick = Arc::new(move |n| {
            pc.lock().unwrap().push(n);
            n - 1
        });
        h.run(&["海贼王", "-r"]).await.unwrap();
        assert_eq!(*picked_counts.lock().unwrap(), vec![4]);
        assert!(fake.calls.lock().unwrap().join("\n").contains("forward 20#[3]"));

        fake.calls.lock().unwrap().clear();
        picked_counts.lock().unwrap().clear();
        h.run(&["kkp", "-s"]).await.unwrap();
        assert_eq!(*picked_counts.lock().unwrap(), vec![3]);
        assert_eq!(
            *h.uploads.lock().unwrap(),
            vec![r#"5 text="" spoiler=true"#]
        );
        assert!(!fake.calls.lock().unwrap().join("\n").contains("forward"));

        // Reupload test
        let fake_u = Arc::new(FakeTg::new());
        *fake_u.content.lock().unwrap() = vec![b'v'; 3000];
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            Arc::clone(&fake_u) as Arc<dyn RpcTransport>,
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let mut msg = kit::blank_bot_message(500, tl::types::PeerUser { user_id: 1 }.into());
        msg.reply_to_id = 42;
        let inv = Invocation {
            client,
            registry: Arc::new(mibot_command::Registry::new()),
            message: msg,
            prefix: ".".into(),
            command: "so".into(),
            args: vec![],
            raw: String::new(),
            cancel: CancellationToken::new(),
            foreign: false,
        };
        let picked = Candidate {
            message: video(10, 5, "", "a.mp4", 61.0),
            peer: tl::types::InputPeerChannel {
                channel_id: 10,
                access_hash: 100,
            }
            .into(),
        };
        let tmp_u = TempDirGuard::new("reupload");
        reupload(&inv, &tmp_u.0, &picked, "关键词", true)
            .await
            .unwrap();
        assert_eq!(*fake_u.uploaded.lock().unwrap(), *fake_u.content.lock().unwrap());
        let sent = fake_u.sent_media.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].message, "关键词");
        assert!(matches!(
            &sent[0].media,
            tl::enums::InputMedia::UploadedDocument(d) if d.spoiler && d.mime_type == "video/mp4"
        ));
        assert_eq!(fs::read_dir(&tmp_u.0).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn test_manage_add_limits_import_forward_err_and_paginate() {
        let fake = Arc::new(FakeTg::new());
        let h = new_harness(Arc::clone(&fake), defaults());
        h.run(&["add", "@first", "\\", "https://t.me/second", "@first", "@nobody"])
            .await
            .unwrap();
        let cfg = h.s.read().unwrap();
        assert_eq!(cfg.channel_list.len(), 2);
        assert_eq!(cfg.channel_list[0].linked_group, "@talk");
        assert_eq!(cfg.default_channel.as_deref(), Some("@first"));
        let err = h.run(&["add", "@first"]).await.unwrap_err();
        assert!(err.to_string().contains("没有添加任何频道：@first：已存在"));

        h.run(&["default", "https://t.me/second"]).await.unwrap();
        assert!(h.run(&["default", "@missing"]).await.is_err());
        assert!(
            render_list(&h.s.read().unwrap(), ".")
                .contains("2. SECOND · <code>https://t.me/second</code>（默认）")
        );
        h.run(&["del", "2"]).await.unwrap();
        assert_eq!(h.s.read().unwrap().default_channel.as_deref(), Some("@first"));
        assert!(h.run(&["del", "@nope"]).await.is_err());
        h.run(&["ad", "add", "vpn", "新词"]).await.unwrap();
        h.run(&["ad", "del", "广告", "新词"]).await.unwrap();
        let f = h.s.read().unwrap().ad_filters;
        assert_eq!(f.len(), DEFAULT_FILTERS.len() - 1);
        h.run(&["del", "all"]).await.unwrap();
        assert!(h.s.read().unwrap().channel_list.is_empty());

        for usage_args in [
            &["add"][..],
            &["del"][..],
            &["default"][..],
            &["ad"][..],
            &["ad", "add"][..],
        ] {
            let e = h.run(usage_args).await.unwrap_err();
            assert!(e.to_string().starts_with("用法：.so "), "{e}");
        }
        assert!(h.run(&["海贼王"]).await.unwrap_err().to_string().contains("请先用 .so add 添加"));
        assert_eq!(h.run(&["-s"]).await.unwrap_err().to_string(), "请输入搜索关键词");

        // Add limits
        let mut many: Vec<String> = vec!["add".into(), "中文".into(), "a.b".into()];
        for i in 0..55 {
            many.push(format!("@chan{i:04}"));
        }
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        fake.calls.lock().unwrap().clear();
        let lim_err = h.run(&refs).await.unwrap_err();
        let ls = lim_err.to_string();
        assert!(ls.contains("一次最多处理 50 个，另外 7 个没有处理") && ls.contains("…还有"));
        let resolved = fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("resolve "))
            .count();
        assert_eq!(resolved, 48);

        // Import needs document
        let mut photo = kit::blank_tl_message(9, tl::types::PeerUser { user_id: 1 }.into());
        photo.media = Some(
            tl::types::MessageMediaPhoto {
                spoiler: false,
                live_photo: false,
                photo: None,
                ttl_seconds: None,
                video: None,
            }
            .into(),
        );
        *fake.reply.lock().unwrap() = Some(photo);
        assert!(h.run_replying(9, &["import"]).await.unwrap_err().to_string().contains("请回复备份文件"));

        let import_bytes = b"@first\r\nhello world text\n\n@second\n".to_vec();
        *fake.content.lock().unwrap() = import_bytes.clone();
        let mut doc_msg = kit::blank_tl_message(9, tl::types::PeerUser { user_id: 1 }.into());
        doc_msg.media = Some(
            tl::types::MessageMediaDocument {
                nopremium: false,
                spoiler: false,
                video: false,
                round: false,
                voice: false,
                document: Some(
                    tl::types::Document {
                        id: 1,
                        access_hash: 1,
                        file_reference: vec![1],
                        date: 0,
                        mime_type: "text/plain".into(),
                        size: import_bytes.len() as i64,
                        thumbs: None,
                        video_thumbs: None,
                        dc_id: 2,
                        attributes: vec![
                            tl::types::DocumentAttributeFilename {
                                file_name: "search-channels.txt".into(),
                            }
                            .into(),
                        ],
                    }
                    .into(),
                ),
                alt_documents: None,
                video_cover: None,
                video_timestamp: None,
                ttl_seconds: None,
            }
            .into(),
        );
        *fake.reply.lock().unwrap() = Some(doc_msg);
        fake.calls.lock().unwrap().clear();
        h.run_replying(9, &["import"]).await.unwrap();
        assert_eq!(h.s.read().unwrap().channel_list.len(), 2);
        let last_edit = fake.edits.lock().unwrap().last().cloned().unwrap();
        assert!(last_edit.contains("hello world text：不是有效的频道用户名或链接"));

        // Forward errors don't reupload + noforwards skips forward
        let fake_f = Arc::new(FakeTg::new());
        *fake_f.forward_err.lock().unwrap() = Some("CHAT_SEND_MEDIA_FORBIDDEN".into());
        fake_f
            .messages
            .lock()
            .unwrap()
            .insert(10, vec![video(10, 1, "海贼王", "", 30.0)]);
        let h_f = new_harness(
            Arc::clone(&fake_f),
            Config {
                schema_version: 1,
                default_channel: None,
                channel_list: vec![Channel {
                    title: "First".into(),
                    handle: "@first".into(),
                    linked_group: String::new(),
                }],
                ad_filters: vec![],
            },
        );
        let ferr = h_f.run(&["海贼王"]).await.unwrap_err();
        assert!(ferr.to_string().contains("转发视频失败（CHAT_SEND_MEDIA_FORBIDDEN）"));
        assert!(h_f.uploads.lock().unwrap().is_empty());

        *fake_f.forward_err.lock().unwrap() = None;
        let mut prot = video(10, 2, "海贼王 保护", "", 30.0);
        prot.noforwards = true;
        fake_f.messages.lock().unwrap().insert(10, vec![prot]);
        fake_f.calls.lock().unwrap().clear();
        h_f.run(&["海贼王"]).await.unwrap();
        assert_eq!(h_f.uploads.lock().unwrap().len(), 1);
        assert!(!fake_f.calls.lock().unwrap().iter().any(|c| c.starts_with("forward ")));

        // Remove many paginates
        let mut big_cfg = defaults();
        for i in 0..150 {
            big_cfg.channel_list.push(Channel {
                title: format!("{}{i}", "频".repeat(60)),
                handle: format!("@c{i:04}"),
                linked_group: String::new(),
            });
        }
        let h_many = new_harness(Arc::clone(&fake), big_cfg);
        h_many.run(&["del", "all"]).await.unwrap();
        let last = fake.edits.lock().unwrap().last().cloned().unwrap();
        assert!(last.contains("已移除 150 个频道") && last.contains("…还有 130 个"));
        assert!(utf16_len(&last) <= 4096);
    }
}
