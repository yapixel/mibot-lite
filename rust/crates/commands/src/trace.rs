use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{Client, Message, peer_from_id, peer_id, utf16_len};
use mibot_command::{
    CmdResult, Command, Invocation, PAGE_LIMIT, code, escape, group, html_pages,
};
use mibot_store::Store;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
struct Reaction {
    #[serde(skip_serializing_if = "String::is_empty")]
    emoticon: String,
    #[serde(rename = "documentId", skip_serializing_if = "String::is_empty")]
    document_id: String,
}

impl<'de> Deserialize<'de> for Reaction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Helper {
            Str(String),
            Obj {
                #[serde(default)]
                emoticon: String,
                #[serde(default, rename = "documentId")]
                document_id: String,
            },
        }
        match Helper::deserialize(deserializer)? {
            Helper::Str(s) => {
                if kit::is_digits(&s) && !s.starts_with('0') {
                    Ok(Reaction {
                        emoticon: String::new(),
                        document_id: s,
                    })
                } else {
                    Ok(Reaction {
                        emoticon: s,
                        document_id: String::new(),
                    })
                }
            }
            Helper::Obj {
                emoticon,
                document_id,
            } => Ok(Reaction {
                emoticon,
                document_id,
            }),
        }
    }
}

impl Reaction {
    fn display(&self) -> String {
        if !self.document_id.is_empty() {
            format!("自定义：{}", self.document_id)
        } else {
            self.emoticon.clone()
        }
    }

    fn tl(&self) -> Option<tl::enums::Reaction> {
        if !self.document_id.is_empty() {
            let id = self.document_id.parse::<i64>().ok().filter(|&v| v > 0)?;
            return Some(tl::types::ReactionCustomEmoji { document_id: id }.into());
        }
        if self.emoticon.trim().is_empty() {
            return None;
        }
        Some(
            tl::types::ReactionEmoji {
                emoticon: self.emoticon.clone(),
            }
            .into(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct TraceSettings {
    #[serde(rename = "keepLog")]
    keep_log: bool,
    big: bool,
}

impl Default for TraceSettings {
    fn default() -> Self {
        Self {
            keep_log: true,
            big: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    users: BTreeMap<String, Vec<Reaction>>,
    keywords: BTreeMap<String, Vec<Reaction>>,
    config: TraceSettings,
}

impl Config {
    fn match_msg(&self, sender: &str, text: &str) -> Vec<Reaction> {
        if let Some(items) = self.users.get(sender)
            && !items.is_empty()
        {
            return items.clone();
        }
        for (keyword, items) in &self.keywords {
            if !keyword.is_empty() && text.contains(keyword.as_str()) {
                return items.clone();
            }
        }
        Vec::new()
    }
}

const AVAILABLE: &[&str] = &[
    "👍", "👎", "❤️", "🔥", "🥰", "👏", "😁", "🤔", "🤯", "😱", "🤬", "😢", "🎉", "🤩", "🤮",
    "💩", "🙏", "👌", "🕊", "🤡", "🥱", "🥴", "😍", "🐳", "❤️‍🔥", "🌚", "🌭", "💯", "🤣", "⚡️",
    "🍌", "🏆", "💔", "🤨", "😐", "🍓", "🍾", "💋", "🖕", "😈", "😎", "😇", "😤",
];

fn standard_emoji() -> &'static [&'static str] {
    static SORTED: OnceLock<Vec<&'static str>> = OnceLock::new();
    SORTED.get_or_init(|| {
        let mut list = AVAILABLE.to_vec();
        list.sort_by_key(|a| std::cmp::Reverse(a.len()));
        list
    })
}

#[derive(Debug, Clone)]
struct Job {
    peer: tl::enums::InputPeer,
    message: i32,
    reactions: Vec<Reaction>,
    big: bool,
}

struct Service {
    app: Arc<App>,
    store: Store<Config>,
    tx: mpsc::Sender<Job>,
    rx: Mutex<Option<mpsc::Receiver<Job>>>,
}

impl Service {
    fn new(app: Arc<App>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(128);
        Arc::new(Self {
            store: kit::new_store(&app, "trace.json", Config::default),
            app,
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }

    fn update<F>(&self, mutate: F) -> Result<(), mibot_store::StoreError>
    where
        F: FnOnce(&mut Config),
    {
        kit::update_ok(&self.store, mutate)
    }

    fn offer(&self, msg: &Message) -> bool {
        if msg.saved {
            return false;
        }
        let sender_id = peer_id(&msg.sender);
        if sender_id.is_empty() {
            return false;
        }
        let Ok(cfg) = self.store.snapshot() else {
            return false;
        };
        if cfg.users.is_empty() && cfg.keywords.is_empty() {
            return false;
        }
        let items = cfg.match_msg(&sender_id, &msg.content());
        if items.is_empty() {
            return false;
        }
        let Ok(peer) = self.app.client.input_peer(&msg.peer) else {
            return false;
        };
        let job = Job {
            peer,
            message: msg.id,
            reactions: items,
            big: cfg.config.big,
        };
        if let Err(err) = self.tx.try_send(job)
            && matches!(err, mpsc::error::TrySendError::Full(_))
        {
            self.app.logs.record(
                mibot_logtail::Level::Warn,
                "trace.queue_full",
                &[("chat", &msg.chat_id)],
            );
        }
        false
    }

    async fn work(self: &Arc<Self>, cancel: CancellationToken) {
        let mut rx = {
            let mut g = self.rx.lock().unwrap_or_else(|e| e.into_inner());
            g.take()
        };
        let Some(mut rx) = rx.take() else {
            return;
        };
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                item = rx.recv() => {
                    let Some(job) = item else { return; };
                    if self.app.registry.is_closed() {
                        continue;
                    }
                    let res = tokio::select! {
                        _ = cancel.cancelled() => return,
                        r = tokio::time::timeout(Duration::from_secs(60), react(&self.app.client, &job)) => r,
                    };
                    if let Ok(Err(err)) = res
                        && !cancel.is_cancelled()
                    {
                        self.app.logs.record(
                            mibot_logtail::Level::Debug,
                            "trace.reaction_failed",
                            &[("error", &err.to_string())],
                        );
                    }
                }
            }
        }
    }

    async fn handle(&self, inv: Invocation) -> CmdResult {
        let sub = inv.arg(0).to_ascii_lowercase();
        match sub.as_str() {
            "status" => self.status(&inv).await,
            "clean" | "reset" => {
                let reset = sub == "reset";
                self.update(|cfg| {
                    if reset {
                        *cfg = Config::default();
                    } else {
                        cfg.users.clear();
                        cfg.keywords.clear();
                    }
                })?;
                if reset {
                    self.receipt(&inv, "✅ 已重置自动回应的追踪和设置").await
                } else {
                    self.receipt(&inv, "✅ 已清空所有追踪").await
                }
            }
            "log" | "big" => {
                let Ok(val) = on_off(inv.arg(1)) else {
                    return Err(kit::usage(inv.prefix(), &format!("trace {sub} on|off")));
                };
                self.update(|cfg| {
                    if sub == "log" {
                        cfg.config.keep_log = val;
                    } else {
                        cfg.config.big = val;
                    }
                })?;
                let label = if sub == "big" {
                    "大号表情动画"
                } else {
                    "保留回执"
                };
                self.receipt(&inv, &format!("✅ {label}已{}", kit::on_off_text(val)))
                    .await
            }
            "kw" => self.keyword(&inv).await,
            _ => self.user(&inv).await,
        }
    }

    async fn keyword(&self, inv: &Invocation) -> CmdResult {
        let action = inv.arg(1).to_ascii_lowercase();
        let word = inv.arg(2);
        if word.is_empty() || (action != "add" && action != "del") {
            return Err(kit::usage(
                inv.prefix(),
                "trace kw add 关键词 表情|kw del 关键词",
            ));
        }
        let mut items = Vec::new();
        if action == "add" {
            items = reactions_after(inv, 3);
            if items.is_empty() {
                return Err(kit::fail(format!(
                    "没找到能用的表情，{}help trace 看可用的表情",
                    inv.prefix()
                )));
            }
            need_premium(&inv.client, &items)?;
        }
        let mut found = true;
        let word_owned = word.to_owned();
        let items_clone = items.clone();
        self.update(|cfg| {
            if action == "add" {
                cfg.keywords.insert(word_owned, items_clone);
            } else {
                found = cfg.keywords.remove(&word_owned).is_some();
            }
        })?;
        if !found {
            return Err(kit::fail(format!("没有追踪关键词 {word}")));
        }
        if action == "add" {
            self.receipt(inv, &format!("✅ 已追踪关键词 {word}：{}", display(&items)))
                .await
        } else {
            self.receipt(inv, &format!("✅ 已取消追踪关键词 {word}")).await
        }
    }

    async fn user(&self, inv: &Invocation) -> CmdResult {
        let reply = kit::reply_msg(inv).await?;
        let Some(reply) = reply else {
            inv.edit(&help(inv.prefix())).await?;
            return Ok(());
        };
        let id = peer_id(&reply.sender);
        if id.is_empty() || reply.sender_is_user(inv.client.self_id()) {
            return Err(kit::fail("不能追踪自己"));
        }
        if inv.args.is_empty() {
            let mut found = false;
            let id_clone = id.clone();
            self.update(|cfg| {
                found = cfg.users.remove(&id_clone).is_some();
            })?;
            if !found {
                return Err(kit::fail(format!("没有在追踪 {id}")));
            }
            return self.receipt(inv, &format!("✅ 已取消追踪 {id}")).await;
        }
        let items = reactions_after(inv, 0);
        if items.is_empty() {
            return Err(kit::fail(format!(
                "没找到能用的表情，{}help trace 看可用的表情",
                inv.prefix()
            )));
        }
        need_premium(&inv.client, &items)?;
        let id_clone = id.clone();
        let items_clone = items.clone();
        self.update(|cfg| {
            cfg.users.insert(id_clone, items_clone);
        })?;
        let cfg = self.store.snapshot()?;
        if let Ok(peer) = inv.client.input_peer(&reply.peer) {
            let job = Job {
                peer,
                message: reply.id,
                reactions: items.clone(),
                big: cfg.config.big,
            };
            if let Err(err) = react(&inv.client, &job).await {
                return Err(mibot_command::fail_with(
                    format!("已追踪 {id}，但这次没能点上表情"),
                    err,
                ));
            }
        }
        self.receipt(inv, &format!("✅ 已追踪 {id}：{}", display(&items)))
            .await
    }

    async fn receipt(&self, inv: &Invocation, text: &str) -> CmdResult {
        inv.edit_text(text).await?;
        let cfg = self.store.snapshot()?;
        if !cfg.config.keep_log && kit::sleep(&inv.cancel, Duration::from_secs(10)).await {
            let _ = inv.delete().await;
        }
        Ok(())
    }

    async fn status(&self, inv: &Invocation) -> CmdResult {
        let cfg = self.store.snapshot()?;
        let mut lines = vec![
            "👀 <b>自动回应</b>".to_owned(),
            format!(
                "追踪 {} 个用户、{} 个关键词",
                cfg.users.len(),
                cfg.keywords.len()
            ),
            String::new(),
            "<b>用户</b>".to_owned(),
        ];
        if cfg.users.is_empty() {
            lines.push("无".to_owned());
        } else {
            for (id, items) in &cfg.users {
                let mut name = id.clone();
                if let Some(peer) = peer_from_id(id) {
                    let title = inv.client.peers().title(&peer);
                    if !title.is_empty() && title != *id {
                        name = format!("{title}（{id}）");
                    }
                }
                lines.push(format!("• {}：{}", escape(&name), escape(&display(items))));
            }
        }
        lines.push(String::new());
        lines.push("<b>关键词</b>".to_owned());
        if cfg.keywords.is_empty() {
            lines.push("无".to_owned());
        } else {
            for (word, items) in &cfg.keywords {
                lines.push(format!("• {}：{}", code(word), escape(&display(items))));
            }
        }
        lines.push(String::new());
        lines.push(format!(
            "保留回执：{} · 大号动画：{}",
            kit::on_off_text(cfg.config.keep_log),
            kit::on_off_text(cfg.config.big)
        ));
        let pages = html_pages(&lines.join("\n"), PAGE_LIMIT);
        inv.respond_pages(&pages).await?;
        Ok(())
    }
}

async fn react(client: &Client, job: &Job) -> Result<(), mibot_bot::BotError> {
    let values: Vec<tl::enums::Reaction> = job.reactions.iter().filter_map(Reaction::tl).collect();
    if values.is_empty() {
        return Ok(());
    }
    client
        .invoke(&tl::functions::messages::SendReaction {
            big: job.big,
            add_to_recent: false,
            peer: job.peer.clone(),
            msg_id: job.message,
            reaction: Some(values),
        })
        .await?;
    Ok(())
}

fn on_off(value: &str) -> Result<bool, mibot_command::CmdError> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => kit::on_off(value),
    }
}

fn need_premium(client: &Client, items: &[Reaction]) -> CmdResult {
    if items.iter().any(|i| !i.document_id.is_empty()) && !client.can_send_rich() {
        return Err(kit::fail("自定义表情要会员才能用"));
    }
    Ok(())
}

fn reactions_after(inv: &Invocation, skip: usize) -> Vec<Reaction> {
    let raw = inv.raw_after(skip);
    if let Some(before) = inv.message.text.strip_suffix(raw) {
        let start = utf16_len(before);
        let entities = inv.message.raw.entities.as_deref().unwrap_or(&[]);
        parse(raw, start, entities)
    } else {
        parse(raw, 0, &[])
    }
}

fn parse(text: &str, start: i32, entities: &[tl::enums::MessageEntity]) -> Vec<Reaction> {
    let mut found: Vec<(i32, Reaction)> = Vec::new();
    let mut spans: Vec<(i32, i32)> = Vec::new();
    let text_u16 = utf16_len(text);
    for entity in entities {
        if let tl::enums::MessageEntity::CustomEmoji(c) = entity {
            if c.offset < start || c.offset + c.length > start + text_u16 {
                continue;
            }
            let rel = c.offset - start;
            found.push((
                rel,
                Reaction {
                    emoticon: String::new(),
                    document_id: c.document_id.to_string(),
                },
            ));
            spans.push((rel, rel + c.length));
        }
    }
    let in_custom = |at: i32| spans.iter().any(|&(s, e)| at >= s && at < e);
    let mut units = 0i32;
    let mut index = 0usize;
    let emojis = standard_emoji();
    while index < text.len() {
        let rest = &text[index..];
        let matched = emojis.iter().find(|&&e| rest.starts_with(e)).copied();
        match matched {
            None => {
                if let Some(ch) = rest.chars().next() {
                    let mut buf = [0u8; 4];
                    units += utf16_len(ch.encode_utf8(&mut buf));
                    index += ch.len_utf8();
                } else {
                    break;
                }
            }
            Some(emoji) => {
                if !in_custom(units) {
                    found.push((
                        units,
                        Reaction {
                            emoticon: emoji.to_owned(),
                            document_id: String::new(),
                        },
                    ));
                }
                units += utf16_len(emoji);
                index += emoji.len();
            }
        }
    }
    found.sort_by_key(|(at, _)| *at);
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for (_, item) in found {
        if seen.insert(item.clone()) {
            result.push(item);
        }
    }
    result
}

fn display(items: &[Reaction]) -> String {
    items
        .iter()
        .map(Reaction::display)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn help(prefix: &str) -> String {
    let c = |text: &str| code(&format!("{prefix}trace{text}"));
    format!(
        "👀 <b>自动回应</b>\n\n追踪指定的用户或关键词，他们发消息时账号自动点上表情回应。\n\n\
         <b>用户</b>\n\
         • 回复一条消息发 {} 追踪发这条消息的人，并马上给这条点上\n\
         • 回复一条消息发 {} 取消追踪这个人\n\n\
         <b>关键词</b>\n\
         • {} 消息里有这个词就点上\n\
         • {} 删除\n\n\
         <b>管理</b>\n\
         • {} 查看追踪的用户和关键词\n\
         • {} 清空所有追踪，{} 连设置一起重置\n\
         • {} 保留命令回执（关闭时 10 秒后删掉）\n\
         • {} 大号表情动画\n\n\
         可用的表情：{}。会员可以用自定义表情；不是会员的账号一条消息只能点一个表情。",
        c(" 👍🔥"),
        c(""),
        c(" kw add 关键词 👍🔥"),
        c(" kw del 关键词"),
        c(" status"),
        c(" clean"),
        c(" reset"),
        c(" log on|off"),
        c(" big on|off"),
        AVAILABLE.concat(),
    )
}

pub fn register(app: &Arc<App>) {
    let s = Service::new(Arc::clone(app));
    let s_offer = Arc::clone(&s);
    app.on_foreign(Arc::new(move |msg, _app, _cancel| s_offer.offer(msg)));
    let s_ready = Arc::clone(&s);
    app.on_ready(Arc::new(move |_app, cancel| {
        let s = Arc::clone(&s_ready);
        Box::pin(async move { s.work(cancel).await })
    }));
    let s_cmd = Arc::clone(&s);
    app.registry.register(Command {
        name: "trace".into(),
        group: group::MEDIA.into(),
        description: "自动给指定用户点表情".into(),
        usage: "[表情…|kw|status|clean|reset]".into(),
        help: help("."),
        help_fn: Some(Arc::new(help)),
        handle: Arc::new(move |inv| {
            let s = Arc::clone(&s_cmd);
            Box::pin(async move { s.handle(inv).await })
        }),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::InvocationError;
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, PeerCache, RpcTransport, SentRecorder};
    use mibot_logtail::Ring;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct Recorder {
        requests: Mutex<Vec<tl::functions::messages::SendReaction>>,
    }

    impl RpcTransport for Recorder {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                if constructor_id == tl::functions::messages::SendReaction::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::SendReaction::from_bytes(&body[4..]).unwrap();
                    self.requests.lock().unwrap().push(req);
                    let resp = tl::enums::Updates::Updates(tl::types::Updates {
                        updates: vec![],
                        users: vec![],
                        chats: vec![],
                        date: 0,
                        seq: 0,
                    });
                    return Ok(resp.to_bytes());
                }
                Err(InvocationError::Dropped("unexpected rpc"))
            })
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-trace-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_parse() {
        let got = parse("👍 abc ❤️‍🔥👍🔥", 0, &[]);
        assert_eq!(display(&got), "👍 ❤️‍🔥 🔥");

        let text = "👍😎";
        let start = utf16_len(".trace ");
        let entities = vec![
            tl::types::MessageEntityCustomEmoji {
                offset: start + 2,
                length: 2,
                document_id: 555,
            }
            .into(),
        ];
        let got = parse(text, start, &entities);
        assert_eq!(display(&got), "👍 自定义：555");
        assert!(parse("hello", 0, &[]).is_empty());
    }

    #[test]
    fn test_reaction_json() {
        let raw = r#"{"users":{"42":["👍",{"emoticon":"🔥"},"123"]},"keywords":{},"config":{"keepLog":false,"big":true}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let items = &cfg.users["42"];
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].emoticon, "👍");
        assert_eq!(items[1].emoticon, "🔥");
        assert_eq!(items[2].document_id, "123");
        assert!(!cfg.config.keep_log);
        assert!(cfg.config.big);
    }

    #[test]
    fn test_match() {
        let mut cfg = Config::default();
        cfg.users.insert(
            "42".into(),
            vec![Reaction {
                emoticon: "👍".into(),
                document_id: String::new(),
            }],
        );
        cfg.keywords.insert(
            "机场".into(),
            vec![Reaction {
                emoticon: "🔥".into(),
                document_id: String::new(),
            }],
        );
        cfg.keywords.insert(
            "测速".into(),
            vec![Reaction {
                emoticon: "🤡".into(),
                document_id: String::new(),
            }],
        );
        assert_eq!(display(&cfg.match_msg("42", "机场")), "👍");
        assert_eq!(display(&cfg.match_msg("7", "机场测速")), "🔥");
        assert!(cfg.match_msg("7", "你好").is_empty());
    }

    #[tokio::test]
    async fn test_offer_reacts_and_reads_rich() {
        let dir = temp_dir("offer");
        let fake = Arc::new(Recorder::default());
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let sent = Arc::new(SentRecorder::new(Arc::new(|_, _| {})));
        let client = Arc::new(Client::from_transport(
            fake.clone(),
            Arc::clone(&peers),
            sent,
        ));
        let app = App::for_testing(
            dir.clone(),
            "test",
            "boot",
            client.clone(),
            peers,
            Arc::new(Ring::new()),
        );
        let s = Service::new(app);
        s.update(|cfg| {
            cfg.keywords.insert(
                "机场".into(),
                vec![
                    Reaction {
                        emoticon: "🔥".into(),
                        document_id: String::new(),
                    },
                    Reaction {
                        emoticon: String::new(),
                        document_id: "9".into(),
                    },
                ],
            );
        })
        .unwrap();

        let msg = Message {
            id: 7,
            peer: tl::types::PeerChat { chat_id: 5 }.into(),
            chat_id: "-5".into(),
            chat_type: mibot_bot::ChatType::Group,
            text: "推荐个机场".into(),
            out: false,
            edited: false,
            forward: false,
            post: false,
            saved: false,
            sender: tl::types::PeerUser { user_id: 42 }.into(),
            reply_to_id: 0,
            reply_to_peer: None,
            topic_id: 0,
            quote_text: String::new(),
            quote_entities: Vec::new(),
            raw: kit::blank_tl_message(
                7,
                tl::types::PeerChat { chat_id: 5 }.into(),
            ),
        };
        assert!(!s.offer(&msg));

        let mut rich_raw =
            kit::blank_tl_message(9, tl::types::PeerChat { chat_id: 5 }.into());
        rich_raw.rich_message = Some(
            tl::types::RichMessage {
                rtl: false,
                part: false,
                blocks: vec![
                    tl::types::PageBlockParagraph {
                        text: mibot_bot::rich_plain("推荐个机场"),
                    }
                    .into(),
                ],
                photos: vec![],
                documents: vec![],
            }
            .into(),
        );
        let rich_msg = Message {
            id: 9,
            text: String::new(),
            raw: rich_raw,
            ..msg.clone()
        };
        assert!(!s.offer(&rich_msg));

        let mut rx = s.rx.lock().unwrap().take().unwrap();
        let job1 = rx.try_recv().unwrap();
        let job2 = rx.try_recv().unwrap();
        assert!(rx.try_recv().is_err());

        react(&client, &job1).await.unwrap();
        react(&client, &job2).await.unwrap();
        let reqs = fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].msg_id, 7);
        assert!(reqs[0].big);
        let r = reqs[0].reaction.as_ref().unwrap();
        assert_eq!(r.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_failed_write_keeps_active_reactions() {
        let dir = temp_dir("failed-write");
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let sent = Arc::new(SentRecorder::new(Arc::new(|_, _| {})));
        let client = Arc::new(Client::from_transport(
            Arc::new(Recorder::default()),
            Arc::clone(&peers),
            sent,
        ));
        let app = App::for_testing(
            dir.clone(),
            "test",
            "boot",
            client,
            peers,
            Arc::new(Ring::new()),
        );
        let s = Service::new(app);
        s.update(|cfg| {
            cfg.keywords.insert(
                "hello".into(),
                vec![Reaction {
                    emoticon: "👍".into(),
                    document_id: String::new(),
                }],
            );
        })
        .unwrap();
        let path = dir.join("data/trace.json");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        assert!(s.update(|cfg| { cfg.keywords.clear(); }).is_err());
        let msg = Message {
            id: 7,
            peer: tl::types::PeerChat { chat_id: 5 }.into(),
            chat_id: "-5".into(),
            chat_type: mibot_bot::ChatType::Group,
            text: "hello".into(),
            out: false,
            edited: false,
            forward: false,
            post: false,
            saved: false,
            sender: tl::types::PeerUser { user_id: 42 }.into(),
            reply_to_id: 0,
            reply_to_peer: None,
            topic_id: 0,
            quote_text: String::new(),
            quote_entities: Vec::new(),
            raw: kit::blank_tl_message(
                7,
                tl::types::PeerChat { chat_id: 5 }.into(),
            ),
        };
        s.offer(&msg);
        let mut rx = s.rx.lock().unwrap().take().unwrap();
        assert!(rx.try_recv().is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
