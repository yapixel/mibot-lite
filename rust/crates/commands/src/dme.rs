use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BotError, MediaOptions, Message};
use mibot_command::{CmdResult, Command, Invocation, code, escape, group};
use mibot_logtail::Level;
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const MAX_KEYWORD_CHARS: usize = 64;
const MAX_CHAT_TRIGGERS: usize = 20;
const TRIGGER_RUNS: usize = 2;
const ALL_MESSAGES: i32 = 999_999;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DmeTrigger {
    #[serde(rename = "chatId")]
    chat_id: String,
    keyword: String,
    count: i32,
    #[serde(default, skip_serializing_if = "is_false")]
    anti: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct DmeConfig {
    #[serde(rename = "batchSize")]
    batch_size: usize,
    #[serde(rename = "searchLimit")]
    search_limit: usize,
    #[serde(rename = "retryAttempts")]
    retry_attempts: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    triggers: Vec<DmeTrigger>,
}

impl Default for DmeConfig {
    fn default() -> Self {
        Self {
            batch_size: 50,
            search_limit: 100,
            retry_attempts: 3,
            triggers: Vec::new(),
        }
    }
}

fn placeholder_png() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let img = image::RgbaImage::from_pixel(256, 256, image::Rgba([40, 40, 40, 255]));
        let mut buf = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut buf);
        let _ = image::ImageEncoder::write_image(
            encoder,
            img.as_raw(),
            256,
            256,
            image::ExtendedColorType::Rgba8,
        );
        buf
    })
}

#[derive(Clone)]
struct Indexed {
    trigger: DmeTrigger,
    lower: String,
}

#[derive(Default)]
struct TriggerIndex {
    by_chat: HashMap<String, Vec<Indexed>>,
}

impl TriggerIndex {
    fn build(triggers: &[DmeTrigger]) -> Self {
        let mut by_chat: HashMap<String, Vec<Indexed>> = HashMap::new();
        for t in triggers {
            by_chat.entry(t.chat_id.clone()).or_default().push(Indexed {
                trigger: t.clone(),
                lower: t.keyword.to_lowercase(),
            });
        }
        Self { by_chat }
    }

    fn match_text(&self, chat_id: &str, text: &str) -> Option<DmeTrigger> {
        let list = self.by_chat.get(chat_id)?;
        let lower = text.to_lowercase();
        list.iter()
            .find(|item| lower.contains(&item.lower))
            .map(|item| item.trigger.clone())
    }
}

struct Service {
    app: Arc<App>,
    store: Store<DmeConfig>,
    write_mu: Mutex<()>,
    cached: RwLock<Option<Arc<TriggerIndex>>>,
    run_mu: Mutex<HashSet<String>>,
    slots: Arc<Semaphore>,
}

impl Service {
    fn new(app: Arc<App>) -> Arc<Self> {
        Arc::new(Self {
            store: kit::new_store(&app, "dme.json", DmeConfig::default),
            app,
            write_mu: Mutex::new(()),
            cached: RwLock::new(None),
            run_mu: Mutex::new(HashSet::new()),
            slots: Arc::new(Semaphore::new(TRIGGER_RUNS)),
        })
    }

    fn claim(&self, chat_id: &str) -> bool {
        let mut g = self.run_mu.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(chat_id.to_owned())
    }

    fn release(&self, chat_id: &str) {
        self.run_mu
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(chat_id);
    }

    fn config(&self) -> Result<DmeConfig, mibot_store::StoreError> {
        let mut cfg = self.store.read()?;
        if !(5..=100).contains(&cfg.batch_size) {
            cfg.batch_size = 50;
        }
        if !(10..=500).contains(&cfg.search_limit) {
            cfg.search_limit = 100;
        }
        if cfg.retry_attempts > 10 {
            cfg.retry_attempts = 3;
        }
        Ok(cfg)
    }

    fn current(&self) -> Arc<TriggerIndex> {
        if let Some(idx) = self.cached.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return idx;
        }
        let _guard = self.write_mu.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(idx) = self.cached.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return idx;
        }
        let cfg = match self.store.read() {
            Ok(c) => c,
            Err(err) => {
                self.app.logs.record(
                    Level::Error,
                    "dme.load_failed",
                    &[("error", &err.to_string())],
                );
                DmeConfig::default()
            }
        };
        let idx = Arc::new(TriggerIndex::build(&cfg.triggers));
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&idx));
        idx
    }

    fn update<F>(&self, mutate: F) -> Result<(), mibot_store::StoreError>
    where
        F: FnOnce(&mut DmeConfig) -> Result<(), mibot_store::StoreError>,
    {
        let _guard = self.write_mu.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = kit::update_read(&self.store, mutate)?;
        let idx = Arc::new(TriggerIndex::build(&cfg.triggers));
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(idx);
        Ok(())
    }

    fn offer(self: &Arc<Self>, msg: &Message, cancel: &CancellationToken) -> bool {
        let text = msg.content();
        if text.is_empty() || msg.out {
            return false;
        }
        let Some(hit) = self.current().match_text(&msg.chat_id, &text) else {
            return false;
        };
        let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
            self.app.logs.record(
                Level::Warn,
                "dme.trigger_dropped",
                &[("chat", &msg.chat_id), ("reason", "too many running")],
            );
            return false;
        };
        if !self.claim(&msg.chat_id) {
            self.app.logs.record(
                Level::Debug,
                "dme.trigger_dropped",
                &[("chat", &msg.chat_id), ("reason", "already deleting here")],
            );
            return false;
        }
        struct ClaimGuard {
            svc: Arc<Service>,
            chat_id: String,
        }
        impl Drop for ClaimGuard {
            fn drop(&mut self) {
                self.svc.release(&self.chat_id);
            }
        }
        let guard = ClaimGuard {
            svc: Arc::clone(self),
            chat_id: msg.chat_id.clone(),
        };
        let s = Arc::clone(self);
        let msg_cloned = msg.clone();
        self.app.registry.background(cancel.clone(), move |task_cancel| {
            Box::pin(async move {
                let _permit = permit;
                let _guard = guard;
                s.sweep_triggered(&msg_cloned, &hit, &task_cancel).await;
            })
        });
        false
    }

    async fn sweep_triggered(&self, msg: &Message, t: &DmeTrigger, cancel: &CancellationToken) {
        let title = self.app.peers.title(&msg.peer);
        self.app.logs.record(
            Level::Info,
            "dme.triggered",
            &[
                ("chat", &msg.chat_id),
                ("message", &msg.id.to_string()),
                ("count", &t.count.to_string()),
            ],
        );
        let notify_fail = |err: &str| {
            self.app.logs.record(
                Level::Error,
                "dme.trigger_failed",
                &[("chat", &msg.chat_id), ("error", err)],
            );
        };
        let cfg = match self.config() {
            Ok(c) => c,
            Err(err) => {
                notify_fail(&err.to_string());
                let _ = kit::send_self(
                    &self.app.client,
                    &escape(&format!(
                        "❌ 在「{title}」按触发词删除自己的消息失败，请检查权限、网络和 Telegram 限制"
                    )),
                )
                .await;
                return;
            }
        };
        let mut run = match DmeRun::new(
            Arc::clone(&self.app),
            msg.clone(),
            t.anti,
            cfg,
            cancel.clone(),
        )
        .await
        {
            Ok(r) => r,
            Err(err) => {
                notify_fail(&err.to_string());
                let _ = kit::send_self(
                    &self.app.client,
                    &escape(&format!(
                        "❌ 在「{title}」按触发词删除自己的消息失败，请检查权限、网络和 Telegram 限制"
                    )),
                )
                .await;
                return;
            }
        };
        if let Err(err) = run.sweep(t.count).await {
            if !cancel.is_cancelled() {
                notify_fail(&err.to_string());
                let _ = kit::send_self(
                    &self.app.client,
                    &escape(&format!(
                        "❌ 在「{title}」按触发词删除自己的消息失败，请检查权限、网络和 Telegram 限制"
                    )),
                )
                .await;
            }
            return;
        }
        if run.deleted > 0 {
            let _ = kit::send_self(
                &self.app.client,
                &escape(&format!(
                    "✅ 「{title}」里有人发了触发词「{}」，已删除自己的 {} 条消息",
                    t.keyword, run.deleted
                )),
            )
            .await;
        }
    }

    async fn command(&self, inv: Invocation) -> CmdResult {
        let sub = inv.arg(0).to_ascii_lowercase();
        match sub.as_str() {
            "" => {
                inv.edit(&help(inv.prefix())).await?;
                return Ok(());
            }
            "kw" => return self.keyword_command(&inv).await,
            _ => {}
        }
        let anti = sub == "-f";
        let token = if anti { inv.arg(1) } else { inv.arg(0) };
        let Ok(count) = token.parse::<i32>() else {
            return Err(kit::usage(inv.prefix(), "dme [-f] 条数（正整数）"));
        };
        if !kit::is_digits(token) || count <= 0 {
            return Err(kit::usage(inv.prefix(), "dme [-f] 条数（正整数）"));
        }
        if !self.claim(&inv.message.chat_id) {
            return Err(kit::fail("这个对话已有一个删除任务在执行，请等它完成"));
        }
        struct ClaimGuard<'a> {
            svc: &'a Service,
            chat_id: &'a str,
        }
        impl Drop for ClaimGuard<'_> {
            fn drop(&mut self) {
                self.svc.release(self.chat_id);
            }
        }
        let _guard = ClaimGuard {
            svc: self,
            chat_id: &inv.message.chat_id,
        };
        let cfg = self.config()?;
        let mut run = DmeRun::new(
            Arc::clone(&self.app),
            inv.message.clone(),
            anti,
            cfg,
            inv.cancel.clone(),
        )
        .await?;
        let peer = run.peer.clone();
        let msg_id = run.message.id;
        if let Err(err) = kit::retry_flood(&inv.cancel, run.cfg.retry_attempts, || {
            let client = &inv.client;
            let peer_ref = &peer;
            async move { client.delete_messages(peer_ref, &[msg_id]).await }
        })
        .await
        {
            self.app.logs.record(
                Level::Warn,
                "dme.command_delete",
                &[("error", &err.to_string())],
            );
        }
        if let Err(err) = run.sweep(count).await {
            if inv.cancel.is_cancelled() {
                return Err(err.into());
            }
            self.app.logs.record(
                Level::Error,
                "dme.failed",
                &[("error", &err.to_string())],
            );
            let title = self.app.peers.title(&inv.message.peer);
            if kit::send_self(
                &inv.client,
                &escape(&format!(
                    "❌ 在「{title}」删除自己的消息失败，请检查权限、网络和 Telegram 限制后重试"
                )),
            )
            .await
            .is_err()
            {
                return Err(mibot_command::fail_with("删除失败", err));
            }
        }
        Ok(())
    }

    async fn keyword_command(&self, inv: &Invocation) -> CmdResult {
        let sub = inv.arg(1).to_ascii_lowercase();
        if sub == "all" || (sub.is_empty() && inv.message.saved) {
            let text = self.list_all()?;
            return respond(inv, &text).await;
        }
        if sub.is_empty() {
            let text = self.list_chat(&inv.message);
            return respond(inv, &text).await;
        }
        if inv.message.saved {
            return Err(kit::fail(
                "收藏夹里没有别人发消息，到要生效的对话里添加或删除触发词",
            ));
        }
        if sub == "rm" {
            let keyword = inv.raw_after(2).trim().to_owned();
            if keyword.is_empty() {
                return Err(kit::usage(inv.prefix(), "dme kw rm 关键词"));
            }
            let chat_id = inv.message.chat_id.clone();
            let kw_lower = keyword.clone();
            let mut removed = false;
            self.update(|cfg| {
                cfg.triggers.retain(|t| {
                    let hit = t.chat_id == chat_id && t.keyword.eq_ignore_ascii_case(&kw_lower);
                    if hit {
                        removed = true;
                        false
                    } else {
                        true
                    }
                });
                Ok(())
            })?;
            if !removed {
                return Err(kit::fail(format!(
                    "这个对话没有这个触发词，{}dme kw 可以看全部",
                    inv.prefix()
                )));
            }
            let title = self.app.peers.title(&inv.message.peer);
            return respond(
                inv,
                &escape(&format!("✅ 已删除「{title}」的触发词「{keyword}」")),
            )
            .await;
        }

        let trigger = parse_trigger(inv)?;
        let prefix = inv.prefix().to_owned();
        let t_clone = trigger.clone();
        let _guard = self.write_mu.lock().unwrap_or_else(|e| e.into_inner());
        let res = self.store.update(|cfg| {
            for item in &mut cfg.triggers {
                if item.chat_id == t_clone.chat_id
                    && item.keyword.eq_ignore_ascii_case(&t_clone.keyword)
                {
                    *item = t_clone;
                    return Ok(());
                }
            }
            let chat_count = cfg
                .triggers
                .iter()
                .filter(|t| t.chat_id == t_clone.chat_id)
                .count();
            if chat_count >= MAX_CHAT_TRIGGERS {
                return Err(kit::fail(format!(
                    "一个对话最多 {MAX_CHAT_TRIGGERS} 个触发词，先用 {prefix}dme kw rm 删掉不用的"
                )));
            }
            cfg.triggers.push(t_clone);
            Ok(())
        });
        match res {
            Ok(()) => {
                if let Ok(cfg) = self.store.read() {
                    let idx = Arc::new(TriggerIndex::build(&cfg.triggers));
                    *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(idx);
                }
            }
            Err(mibot_store::UpdateError::Store(e)) => return Err(e.into()),
            Err(mibot_store::UpdateError::Mutate(e)) => return Err(e),
        }
        drop(_guard);
        let title = self.app.peers.title(&inv.message.peer);
        respond(
            inv,
            &escape(&format!(
                "✅ 已在「{title}」添加触发词「{}」：别人发的消息包含它时，{}",
                trigger.keyword,
                describe_trigger(&trigger)
            )),
        )
        .await
    }

    fn list_chat(&self, msg: &Message) -> String {
        let title = format!("🗑️ <b>{} 的触发词</b>\n", escape(&self.app.peers.title(&msg.peer)));
        let idx = self.current();
        let Some(list) = idx.by_chat.get(&msg.chat_id) else {
            return format!("{title}这个对话还没有触发词");
        };
        if list.is_empty() {
            return format!("{title}这个对话还没有触发词");
        }
        let rows: Vec<String> = list
            .iter()
            .map(|t| format!("• {} {}", code(&t.trigger.keyword), escape(&describe_trigger(&t.trigger))))
            .collect();
        format!("{title}{}", rows.join("\n"))
    }

    fn list_all(&self) -> Result<String, mibot_store::StoreError> {
        let cfg = self.store.read()?;
        if cfg.triggers.is_empty() {
            return Ok("🗑️ <b>触发词</b>\n还没有触发词".into());
        }
        let mut order: Vec<String> = Vec::new();
        let mut by_chat: HashMap<String, Vec<DmeTrigger>> = HashMap::new();
        for t in cfg.triggers {
            if !by_chat.contains_key(&t.chat_id) {
                order.push(t.chat_id.clone());
            }
            by_chat.entry(t.chat_id.clone()).or_default().push(t);
        }
        let mut sections = vec!["🗑️ <b>触发词</b>".to_owned()];
        for chat in order {
            let name = mibot_bot::peer_from_id(&chat)
                .map(|p| self.app.peers.title(&p))
                .unwrap_or_else(|| chat.clone());
            let mut rows = vec![format!("<b>{}</b>", escape(&name))];
            for t in &by_chat[&chat] {
                rows.push(format!("• {} {}", code(&t.keyword), escape(&describe_trigger(t))));
            }
            sections.push(rows.join("\n"));
        }
        Ok(sections.join("\n\n"))
    }
}

async fn respond(inv: &Invocation, html: &str) -> CmdResult {
    if inv.message.saved {
        inv.edit(html).await?;
        return Ok(());
    }
    kit::send_self(&inv.client, html).await?;
    inv.delete().await?;
    Ok(())
}

fn parse_trigger(inv: &Invocation) -> Result<DmeTrigger, mibot_command::CmdError> {
    let usage = kit::usage(inv.prefix(), "dme kw [-f] 条数 关键词");
    let anti = inv.arg(1).eq_ignore_ascii_case("-f");
    let idx = if anti { 2 } else { 1 };
    let token = inv.arg(idx);
    let Ok(mut count) = token.parse::<i32>() else {
        return Err(usage);
    };
    if !kit::is_digits(token) || count <= 0 {
        return Err(usage);
    }
    if count > 2000 && count != ALL_MESSAGES {
        count = 2000;
    }
    let keyword = inv.raw_after(idx + 1).trim().to_owned();
    if keyword.is_empty() {
        return Err(usage);
    }
    if keyword.chars().count() > MAX_KEYWORD_CHARS {
        return Err(kit::fail(format!("触发词最多 {MAX_KEYWORD_CHARS} 个字")));
    }
    Ok(DmeTrigger {
        chat_id: inv.message.chat_id.clone(),
        keyword,
        count,
        anti,
    })
}

fn describe_trigger(t: &DmeTrigger) -> String {
    let mut what = if t.count == ALL_MESSAGES {
        "删掉自己在这里之前的全部消息".to_owned()
    } else {
        format!("删掉自己在这里之前的 {} 条消息", t.count)
    };
    if t.anti {
        what.push_str("（先替换内容再删）");
    }
    what
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageOutcome {
    Ready,
    Switched,
    Exhausted,
}

struct DmeRun {
    app: Arc<App>,
    message: Message,
    peer: tl::enums::InputPeer,
    cfg: DmeConfig,
    anti: bool,
    cancel: CancellationToken,
    is_channel: bool,
    direct: bool,
    identities: HashSet<String>,
    topic_id: i32,
    history: bool,
    offset_id: i32,
    empty: usize,
    seen: HashSet<i32>,
    placeholder: Option<tl::enums::InputPhoto>,
    matched: usize,
    deleted: usize,
    failed: usize,
    edited: usize,
    failed_edits: usize,
}

impl DmeRun {
    async fn new(
        app: Arc<App>,
        message: Message,
        anti: bool,
        cfg: DmeConfig,
        cancel: CancellationToken,
    ) -> Result<Self, BotError> {
        let client = &app.client;
        let peer = client.input_peer(&message.peer)?;
        let channel_id = message.channel_id();
        let is_channel = channel_id.is_some();
        let ch_info = channel_id.and_then(|id| client.peers().channel(id));
        let mut noforwards = ch_info.as_ref().is_some_and(|i| i.noforwards);
        if let tl::enums::Peer::Chat(c) = &message.peer
            && let Some(info) = client.peers().chat(c.chat_id)
        {
            noforwards = info.noforwards;
        }
        let mut direct = message.saved;
        let input_ch = kit::input_channel(&peer);
        if anti
            && message.out
            && ch_info.as_ref().is_some_and(|i| i.broadcast)
            && let Some(ch) = &input_ch
            && let Ok(tl::enums::channels::ChannelParticipant::Participant(p)) = client
                .invoke(&tl::functions::channels::GetParticipant {
                    channel: ch.clone().into(),
                    participant: tl::enums::InputPeer::PeerSelf,
                })
                .await
            && matches!(
                p.participant,
                tl::enums::ChannelParticipant::Creator(_)
            )
        {
            direct = true;
        }
        let mut identities = HashSet::new();
        if !direct
            && is_channel
            && let Ok(tl::enums::channels::SendAsPeers::Peers(res)) = client
                .invoke(&tl::functions::channels::GetSendAs {
                    for_paid_reactions: false,
                    for_live_stories: false,
                    peer: peer.clone(),
                })
                .await
        {
            for item in res.peers {
                let tl::enums::SendAsPeer::Peer(p) = item;
                let k = kit::peer_key(&p.peer);
                if !k.is_empty() {
                    identities.insert(k);
                }
            }
        }
        let history = direct || is_channel || noforwards;
        let topic_id = message.topic_id;
        let offset_id = message.id;
        Ok(Self {
            app,
            message,
            peer,
            cfg,
            anti,
            cancel,
            is_channel,
            direct,
            identities,
            topic_id,
            history,
            offset_id,
            empty: 0,
            seen: HashSet::new(),
            placeholder: None,
            matched: 0,
            deleted: 0,
            failed: 0,
            edited: 0,
            failed_edits: 0,
        })
    }

    fn mine(&self, m: &tl::types::Message) -> bool {
        if m.out {
            return true;
        }
        if let Some(from) = &m.from_id {
            if let tl::enums::Peer::User(u) = from
                && u.user_id == self.app.client.self_id()
            {
                return true;
            }
            return self.identities.contains(&kit::peer_key(from));
        }
        false
    }

    fn eligible(&self, m: &tl::types::Message) -> bool {
        if m.id <= 0 || m.id >= self.message.id || (!self.direct && !self.mine(m)) {
            return false;
        }
        if self.topic_id != 0 {
            return topic_of(m) == self.topic_id;
        }
        !matches!(
            &m.reply_to,
            Some(tl::enums::MessageReplyHeader::Header(h)) if h.forum_topic
        )
    }

    async fn scrub(&mut self, m: &tl::types::Message) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if m.date > 0 && now - m.date as i64 > 172_800 {
            return;
        }
        let client = &self.app.client;
        if m.media.is_none() {
            if m.message != "占位符"
                && client
                    .edit_text(&self.peer, m.id, "占位符", true)
                    .await
                    .is_err()
            {
                self.failed_edits += 1;
                return;
            }
            self.edited += 1;
            return;
        }
        match m.media.as_ref() {
            Some(tl::enums::MessageMedia::WebPage(_)) => return,
            Some(tl::enums::MessageMedia::Document(doc)) => {
                if let Some(tl::enums::Document::Document(d)) = &doc.document
                    && d.attributes
                        .iter()
                        .any(|a| matches!(a, tl::enums::DocumentAttribute::Sticker(_)))
                {
                    return;
                }
            }
            _ => {}
        }
        if let Some(photo) = self.placeholder.clone() {
            let media: tl::enums::InputMedia = tl::types::InputMediaPhoto {
                spoiler: false,
                live_photo: false,
                id: photo,
                ttl_seconds: None,
                video: None,
            }
            .into();
            match client
                .edit_media(&self.peer, m.id, media, MediaOptions::default())
                .await
            {
                Ok(_) => {
                    self.edited += 1;
                    return;
                }
                Err(err) if file_reference_expired(&err) => {
                    self.placeholder = None;
                }
                Err(_) => {
                    self.failed_edits += 1;
                    return;
                }
            }
        }
        let Ok(file) = client.upload_bytes("dme.png", placeholder_png()).await else {
            self.failed_edits += 1;
            return;
        };
        let media: tl::enums::InputMedia = tl::types::InputMediaUploadedPhoto {
            spoiler: false,
            live_photo: false,
            file,
            stickers: None,
            ttl_seconds: None,
            video: None,
        }
        .into();
        match client
            .edit_media(&self.peer, m.id, media, MediaOptions::default())
            .await
        {
            Ok(updates) => {
                self.placeholder = edited_photo(&updates);
                self.edited += 1;
            }
            Err(_) => {
                self.failed_edits += 1;
            }
        }
    }

    async fn delete_batch(&mut self, ids: &[i32]) -> Result<(), BotError> {
        let mut size = self.cfg.batch_size;
        let mut offset = 0usize;
        while offset < ids.len() {
            let end = (offset + size).min(ids.len());
            let batch = &ids[offset..end];
            let res = kit::retry_flood(&self.cancel, self.cfg.retry_attempts, || {
                let client = &self.app.client;
                let peer = &self.peer;
                async move { client.delete_messages(peer, batch).await }
            })
            .await;
            match res {
                Ok(()) => {
                    self.deleted += batch.len();
                    offset += batch.len();
                    size = (size + 5).min(100);
                    let _ = self.app.client.invoke(&tl::functions::updates::GetState {}).await;
                }
                Err(_) => {
                    if self.cancel.is_cancelled() {
                        return Err(BotError::Cancelled);
                    }
                    if batch.len() == 1 {
                        self.failed += 1;
                        offset += 1;
                    } else {
                        size = (batch.len() / 2).max(1);
                    }
                }
            }
            if !kit::sleep(&self.cancel, Duration::from_millis(200)).await {
                return Err(BotError::Cancelled);
            }
        }
        Ok(())
    }

    fn page_limit(&self, target: usize, unlimited: bool) -> i32 {
        if self.history {
            100
        } else if unlimited {
            100.min(self.cfg.search_limit) as i32
        } else {
            100.min(self.cfg.search_limit)
                .min(target.saturating_sub(self.matched)) as i32
        }
    }

    async fn fetch(&self, limit: i32) -> Result<Vec<tl::enums::Message>, BotError> {
        let res = if self.history {
            self.app
                .client
                .invoke(&tl::functions::messages::GetHistory {
                    peer: self.peer.clone(),
                    offset_id: self.offset_id,
                    offset_date: 0,
                    add_offset: 0,
                    limit,
                    max_id: self.message.id,
                    min_id: 0,
                    hash: 0,
                })
                .await?
        } else {
            self.app
                .client
                .invoke(&tl::functions::messages::Search {
                    peer: self.peer.clone(),
                    q: String::new(),
                    from_id: Some(tl::enums::InputPeer::PeerSelf),
                    saved_peer_id: None,
                    saved_reaction: None,
                    top_msg_id: Some(self.topic_id).filter(|&v| v > 0),
                    filter: tl::enums::MessagesFilter::InputMessagesFilterEmpty,
                    min_date: 0,
                    max_date: 0,
                    offset_id: self.offset_id,
                    add_offset: 0,
                    limit,
                    max_id: self.message.id,
                    min_id: 0,
                    hash: 0,
                })
                .await?
        };
        Ok(kit::unpack_messages(res))
    }

    async fn next_page(
        &mut self,
        limit: i32,
    ) -> Result<(Vec<tl::enums::Message>, PageOutcome), BotError> {
        let res = kit::retry_flood(&self.cancel, 2, || self.fetch(limit)).await;
        let page = match res {
            Ok(p) => p,
            Err(err) => {
                if self.cancel.is_cancelled() || self.history {
                    return Err(err);
                }
                self.history = true;
                self.offset_id = self.message.id;
                return Ok((Vec::new(), PageOutcome::Switched));
            }
        };
        if page.is_empty() {
            if !self.history {
                self.history = true;
                self.offset_id = self.message.id;
                return Ok((Vec::new(), PageOutcome::Switched));
            }
            return Ok((Vec::new(), PageOutcome::Exhausted));
        }
        let mut next = 0i32;
        for item in &page {
            let id = kit::message_id(item);
            if id > 0 && (next == 0 || id < next) {
                next = id;
            }
        }
        if next == 0 || next >= self.offset_id {
            return Err(BotError::Message(
                "DME history cursor did not advance".into(),
            ));
        }
        self.offset_id = next;
        Ok((page, PageOutcome::Ready))
    }

    fn pick(
        &self,
        page: Vec<tl::enums::Message>,
        target: usize,
        unlimited: bool,
    ) -> Vec<tl::types::Message> {
        let mut batch = Vec::new();
        for item in page {
            if let tl::enums::Message::Message(m) = item
                && self.eligible(&m)
                && !self.seen.contains(&m.id)
            {
                batch.push(m);
            }
        }
        if !unlimited {
            let rem = target.saturating_sub(self.matched);
            if batch.len() > rem {
                batch.truncate(rem);
            }
        }
        batch
    }

    fn give_up(&mut self) -> bool {
        self.empty += 1;
        let limit = if self.is_channel { 300 } else { 3 };
        self.history && !self.direct && self.empty >= limit
    }

    async fn process(&mut self, batch: Vec<tl::types::Message>) -> Result<(), BotError> {
        self.empty = 0;
        let mut ids = Vec::with_capacity(batch.len());
        for m in &batch {
            self.seen.insert(m.id);
            ids.push(m.id);
        }
        self.matched += batch.len();
        if self.anti && !self.direct {
            let before = self.edited;
            for m in &batch {
                if self.cancel.is_cancelled() {
                    return Err(BotError::Cancelled);
                }
                self.scrub(m).await;
            }
            if self.edited > before && !kit::sleep(&self.cancel, Duration::from_secs(1)).await {
                return Err(BotError::Cancelled);
            }
        }
        self.delete_batch(&ids).await
    }

    fn forget_passed(&mut self) {
        if self.history {
            let offset = self.offset_id;
            self.seen.retain(|&id| id < offset);
        }
    }

    async fn report(&self, requested: i32) {
        self.app.logs.record(
            Level::Info,
            "dme.complete",
            &[
                ("requested", &requested.to_string()),
                ("matched", &self.matched.to_string()),
                ("deleted", &self.deleted.to_string()),
                ("edited", &self.edited.to_string()),
                ("failed", &self.failed.to_string()),
                ("failed_edits", &self.failed_edits.to_string()),
            ],
        );
        if self.failed > 0 || self.failed_edits > 0 {
            let title = self.app.peers.title(&self.message.peer);
            let _ = kit::send_self(
                &self.app.client,
                &escape(&format!(
                    "⚠️ 在「{title}」删除自己的消息：已删除 {} 条，删除失败 {} 条，防撤回编辑失败 {} 条",
                    self.deleted, self.failed, self.failed_edits
                )),
            )
            .await;
        }
    }

    async fn sweep(&mut self, count: i32) -> Result<(), BotError> {
        let unlimited = count == ALL_MESSAGES;
        let target = if unlimited {
            usize::MAX
        } else {
            (count as usize).min(2000)
        };
        while unlimited || self.matched < target {
            if self.cancel.is_cancelled() {
                return Err(BotError::Cancelled);
            }
            let limit = self.page_limit(target, unlimited);
            let (page, outcome) = self.next_page(limit).await?;
            match outcome {
                PageOutcome::Switched => continue,
                PageOutcome::Exhausted => break,
                PageOutcome::Ready => {}
            }
            let batch = self.pick(page, target, unlimited);
            if batch.is_empty() {
                if self.give_up() {
                    break;
                }
            } else {
                self.process(batch).await?;
            }
            self.forget_passed();
            if !kit::sleep(&self.cancel, Duration::from_millis(100)).await {
                return Err(BotError::Cancelled);
            }
        }
        self.report(count).await;
        Ok(())
    }
}

fn topic_of(m: &tl::types::Message) -> i32 {
    if let Some(tl::enums::MessageReplyHeader::Header(h)) = &m.reply_to {
        if let Some(top) = h.reply_to_top_id
            && top > 0
        {
            return top;
        }
        if h.forum_topic
            && let Some(r) = h.reply_to_msg_id
            && r > 0
        {
            return r;
        }
    }
    0
}

fn edited_photo(updates: &tl::enums::Updates) -> Option<tl::enums::InputPhoto> {
    let list = match updates {
        tl::enums::Updates::Updates(u) => &u.updates,
        tl::enums::Updates::Combined(u) => &u.updates,
        _ => return None,
    };
    for update in list {
        let msg = match update {
            tl::enums::Update::EditMessage(u) => &u.message,
            tl::enums::Update::EditChannelMessage(u) => &u.message,
            _ => continue,
        };
        if let tl::enums::Message::Message(plain) = msg
            && let Some(tl::enums::MessageMedia::Photo(mp)) = &plain.media
            && let Some(tl::enums::Photo::Photo(photo)) = &mp.photo
        {
            return Some(
                tl::types::InputPhoto {
                    id: photo.id,
                    access_hash: photo.access_hash,
                    file_reference: photo.file_reference.clone(),
                }
                .into(),
            );
        }
    }
    None
}

fn file_reference_expired(err: &BotError) -> bool {
    err.rpc_error()
        .is_some_and(|rpc| rpc.name.starts_with("FILE_REFERENCE_"))
}

pub fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let c = |text: &str| format!("<code>{p}dme{text}</code>");
    format!(
        "🗑️ <b>智能防撤回删除</b>\n\n{} 快速删除自己的消息\n{} 替换文本和媒体后删除\n{} 删除全部可见的自己的消息\n\
         普通数量单次最多 2000 条；仅处理命令之前的消息及当前话题。\n\
         收藏夹直接删除；-f 模式下广播频道主直接按数量删除。\n\
         防撤回编辑可能因消息类型、编辑时限或权限失败，不保证第三方副本被删除。\n\n\
         <b>触发词</b>\n\
         别人在对话里发的消息包含这个对话的触发词时，自动删掉自己在这里、那条消息之前的消息。\n\
         • {} 在当前对话添加触发词，同一个词再加一次就改成新的条数\n\
         • {} 触发时先替换内容再删\n\
         • {} 当前对话的触发词（在收藏夹里发就列出全部）\n\
         • {} 所有对话的触发词\n\
         • {} 删除当前对话的触发词\n\
         触发词只在添加它的对话里生效，不区分大小写；对话里任何人发出都会触发，自己发的不算。\
         设置和查看触发词时，结果发到收藏夹，命令消息随即删掉，免得对话里的人看到触发词。\
         每次触发删除之后，收藏夹里会收到一条说明。",
        c(" 数量"),
        c(" -f 数量"),
        c(" 999999"),
        c(" kw 条数 关键词"),
        c(" kw -f 条数 关键词"),
        c(" kw"),
        c(" kw all"),
        c(" kw rm 关键词"),
    )
}

pub fn register(app: &Arc<App>) {
    let s = Service::new(Arc::clone(app));
    let s_offer = Arc::clone(&s);
    app.on_foreign(Arc::new(move |msg, _app, cancel| {
        s_offer.offer(msg, cancel)
    }));
    let s_cmd = Arc::clone(&s);
    app.registry.register(Command {
        name: "dme".into(),
        group: group::MEDIA.into(),
        description: "删除自己最近的消息".into(),
        usage: "[-f] 条数|kw".into(),
        help: help("."),
        help_fn: Some(Arc::new(help)),
        timeout: Some(Duration::ZERO),
        handle: Arc::new(move |inv| {
            let s = Arc::clone(&s_cmd);
            Box::pin(async move { s.command(inv).await })
        }),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, Client, PeerCache, RpcTransport, SentRecorder};
    use mibot_logtail::Ring;
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeTelegram {
        parts: Mutex<usize>,
        media: Mutex<Vec<Option<tl::enums::InputMedia>>>,
        refuse: Mutex<Option<RpcError>>,
    }

    impl RpcTransport for FakeTelegram {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                if constructor_id == tl::functions::upload::SaveFilePart::CONSTRUCTOR_ID {
                    *self.parts.lock().unwrap() += 1;
                    return Ok(true.to_bytes());
                }
                if constructor_id == tl::functions::messages::EditMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::EditMessage::from_bytes(&body[4..]).unwrap();
                    let reused = matches!(req.media, Some(tl::enums::InputMedia::Photo(_)));
                    self.media.lock().unwrap().push(req.media.clone());
                    if reused && let Some(err) = self.refuse.lock().unwrap().take() {
                        return Err(InvocationError::Rpc(err));
                    }
                    let mut raw_msg = kit::blank_tl_message(
                        req.id,
                        tl::types::PeerUser { user_id: 1 }.into(),
                    );
                    raw_msg.media = Some(
                        tl::types::MessageMediaPhoto {
                            spoiler: false,
                            live_photo: false,
                            photo: Some(
                                tl::types::Photo {
                                    has_stickers: false,
                                    id: 77,
                                    access_hash: 5,
                                    file_reference: vec![1],
                                    date: 0,
                                    sizes: vec![],
                                    video_sizes: None,
                                    dc_id: 1,
                                }
                                .into(),
                            ),
                            ttl_seconds: None,
                            video: None,
                        }
                        .into(),
                    );
                    let resp = tl::enums::Updates::Updates(tl::types::Updates {
                        updates: vec![
                            tl::types::UpdateEditMessage {
                                message: raw_msg.into(),
                                pts: 1,
                                pts_count: 1,
                            }
                            .into(),
                        ],
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
        let dir = std::env::temp_dir().join(format!("mibot-dme-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn new_scrub_run(name: &str) -> (DmeRun, Arc<FakeTelegram>, PathBuf) {
        let dir = temp_dir(name);
        let fake = Arc::new(FakeTelegram::default());
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            fake.clone(),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let app = App::for_testing(
            dir.clone(),
            "test",
            "boot",
            client,
            peers,
            Arc::new(Ring::new()),
        );
        let msg = Message {
            id: 100,
            peer: tl::types::PeerUser { user_id: 1 }.into(),
            chat_id: "1".into(),
            chat_type: mibot_bot::ChatType::Private,
            text: String::new(),
            out: true,
            edited: false,
            forward: false,
            post: false,
            saved: true,
            sender: tl::types::PeerUser { user_id: 1 }.into(),
            reply_to_id: 0,
            reply_to_peer: None,
            topic_id: 0,
            quote_text: String::new(),
            quote_entities: Vec::new(),
            raw: kit::blank_tl_message(
                100,
                tl::types::PeerUser { user_id: 1 }.into(),
            ),
        };
        let run = DmeRun {
            app,
            message: msg,
            peer: tl::enums::InputPeer::PeerSelf,
            cfg: DmeConfig::default(),
            anti: true,
            cancel: CancellationToken::new(),
            is_channel: false,
            direct: false,
            identities: HashSet::new(),
            topic_id: 0,
            history: true,
            offset_id: 100,
            empty: 0,
            seen: HashSet::new(),
            placeholder: None,
            matched: 0,
            deleted: 0,
            failed: 0,
            edited: 0,
            failed_edits: 0,
        };
        (run, fake, dir)
    }

    fn photo_msg(id: i32) -> tl::types::Message {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i32;
        let mut m = kit::blank_tl_message(
            id,
            tl::types::PeerUser { user_id: 1 }.into(),
        );
        m.date = now;
        m.media = Some(
            tl::types::MessageMediaPhoto {
                spoiler: false,
                live_photo: false,
                photo: None,
                ttl_seconds: None,
                video: None,
            }
            .into(),
        );
        m
    }

    #[test]
    fn test_general_topic_does_not_delete_other_topics() {
        let (mut run, _, dir) = new_scrub_run("topic");
        let mut general = kit::blank_tl_message(
            70,
            tl::types::PeerChannel { channel_id: 5 }.into(),
        );
        general.out = true;
        let mut topic = kit::blank_tl_message(
            80,
            tl::types::PeerChannel { channel_id: 5 }.into(),
        );
        topic.out = true;
        topic.reply_to = Some(
            tl::types::MessageReplyHeader {
                reply_to_scheduled: false,
                forum_topic: true,
                quote: false,
                reply_to_ephemeral: false,
                reply_to_msg_id: Some(40),
                reply_to_peer_id: None,
                reply_from: None,
                reply_media: None,
                reply_to_top_id: None,
                quote_text: None,
                quote_entities: None,
                quote_offset: None,
                todo_item_id: None,
                poll_option: None,
                quote_rich: None,
            }
            .into(),
        );
        assert!(run.eligible(&general) && !run.eligible(&topic));
        run.topic_id = 40;
        assert!(!run.eligible(&general) && run.eligible(&topic));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_scrub_uploads_placeholder_once_and_reuploads_on_expired() {
        let (mut run, fake, dir) = new_scrub_run("scrub");
        for id in 1..=3 {
            run.scrub(&photo_msg(id)).await;
        }
        assert_eq!(*fake.parts.lock().unwrap(), 1);
        assert_eq!(run.edited, 3);
        assert_eq!(run.failed_edits, 0);

        *fake.refuse.lock().unwrap() = Some(RpcError {
            code: 400,
            name: "FILE_REFERENCE_EXPIRED".into(),
            value: None,
            caused_by: None,
        });
        run.scrub(&photo_msg(4)).await;
        assert_eq!(*fake.parts.lock().unwrap(), 2);
        assert_eq!(run.edited, 4);

        *fake.refuse.lock().unwrap() = Some(RpcError {
            code: 400,
            name: "MESSAGE_EDIT_TIME_EXPIRED".into(),
            value: None,
            caused_by: None,
        });
        run.scrub(&photo_msg(5)).await;
        run.scrub(&photo_msg(6)).await;
        assert_eq!(*fake.parts.lock().unwrap(), 2);
        assert_eq!(run.edited, 5);
        assert_eq!(run.failed_edits, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_trigger_index_and_parse() {
        let idx = TriggerIndex::build(&[
            DmeTrigger {
                chat_id: "-100".into(),
                keyword: "清理".into(),
                count: 10,
                anti: false,
            },
            DmeTrigger {
                chat_id: "-100".into(),
                keyword: ".Sum".into(),
                count: 20,
                anti: false,
            },
        ]);
        assert_eq!(idx.match_text("-100", "麻烦清理一下").unwrap().count, 10);
        assert_eq!(idx.match_text("-100", "跑一下 .SUM 看看").unwrap().count, 20);
        assert!(idx.match_text("7", "清理").is_none());
    }
}
