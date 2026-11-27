use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{Client, Message, PeerCache, peer_id};
use mibot_command::{CmdResult, Command, Invocation, PAGE_LIMIT, code, escape, group, html_pages};
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const QUEUE_SIZE: usize = 256;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct GroupConfig {
    enabled: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    title: String,
    #[serde(skip_serializing_if = "is_false")]
    strict: bool,
    #[serde(skip_serializing_if = "is_zero")]
    bots: usize,
    #[serde(skip_serializing_if = "is_zero")]
    messages: usize,
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    groups: BTreeMap<String, GroupConfig>,
    allow: Vec<String>,
}

impl Config {
    fn allowed(&self, id: i64, username: &str) -> bool {
        let lower = username.to_ascii_lowercase();
        let id_str = id.to_string();
        self.allow
            .iter()
            .any(|entry| *entry == id_str || (!lower.is_empty() && *entry == lower))
    }
}

#[derive(Debug, Clone)]
struct Event {
    chat_id: String,
    channel: tl::types::InputChannel,
    message: i32,
    bot_id: i64,
    via: tl::enums::Peer,
}

struct Service {
    app: Arc<App>,
    store: Store<Config>,
    cached: RwLock<Option<Arc<Config>>>,
    write_mu: Mutex<()>,
    tx: mpsc::Sender<Event>,
    rx: Mutex<Option<mpsc::Receiver<Event>>>,
    rate_mu: Mutex<RateState>,
}

#[derive(Default)]
struct RateState {
    banned: HashMap<String, Instant>,
    warned: HashMap<String, Instant>,
}

impl Service {
    fn new(app: Arc<App>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        Arc::new(Self {
            store: kit::new_store(&app, "guestban.json", Config::default),
            app,
            cached: RwLock::new(None),
            write_mu: Mutex::new(()),
            tx,
            rx: Mutex::new(Some(rx)),
            rate_mu: Mutex::new(RateState::default()),
        })
    }

    fn current(&self) -> Arc<Config> {
        if let Some(cfg) = self.cached.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return cfg;
        }
        let _guard = self.write_mu.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cfg) = self.cached.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return cfg;
        }
        let cfg = Arc::new(self.store.read().unwrap_or_default());
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&cfg));
        cfg
    }

    fn update<F>(&self, mutate: F) -> Result<(), mibot_store::StoreError>
    where
        F: FnOnce(&mut Config) -> Result<(), mibot_store::StoreError>,
    {
        let _guard = self.write_mu.lock().unwrap_or_else(|e| e.into_inner());
        let updated = kit::update_read(&self.store, mutate)?;
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(updated));
        Ok(())
    }

    fn settings_for(&self, peers: &PeerCache, chat_id: &str, bot_id: i64) -> Option<GroupConfig> {
        let cfg = self.current();
        let settings = cfg.groups.get(chat_id)?;
        if !settings.enabled {
            return None;
        }
        let username = peers.user(bot_id).map(|u| u.username).unwrap_or_default();
        if cfg.allowed(bot_id, &username) {
            return None;
        }
        Some(settings.clone())
    }

    fn offer(&self, msg: &Message) -> bool {
        let Some(via) = msg.raw.guestchat_via_from.as_ref() else {
            return false;
        };
        let Some(tl::enums::Peer::User(sender)) = msg.raw.from_id.as_ref() else {
            return false;
        };
        if self
            .settings_for(&self.app.peers, &msg.chat_id, sender.user_id)
            .is_none()
        {
            return false;
        }
        let Ok(peer) = self.app.client.input_peer(&msg.peer) else {
            return false;
        };
        let Some(channel) = kit::input_channel(&peer) else {
            return false;
        };
        let ev = Event {
            chat_id: msg.chat_id.clone(),
            channel,
            message: msg.id,
            bot_id: sender.user_id,
            via: via.clone(),
        };
        if let Err(err) = self.tx.try_send(ev)
            && matches!(err, mpsc::error::TrySendError::Full(_))
        {
            self.app.logs.record(
                mibot_logtail::Level::Warn,
                "guestban.queue_full",
                &[
                    ("chat", &msg.chat_id),
                    ("message", &msg.id.to_string()),
                ],
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
                    let Some(ev) = item else { return; };
                    if self.app.registry.is_closed() {
                        continue;
                    }
                    self.process(&cancel, &ev).await;
                }
            }
        }
    }

    async fn process(&self, cancel: &CancellationToken, item: &Event) {
        if self
            .settings_for(&self.app.peers, &item.chat_id, item.bot_id)
            .is_none()
        {
            return;
        }
        let client = &self.app.client;
        let run = async {
            let mut deleted = 0usize;
            match client
                .invoke(&tl::functions::channels::DeleteMessages {
                    channel: item.channel.clone().into(),
                    id: vec![item.message],
                })
                .await
            {
                Ok(_) => deleted = 1,
                Err(err) => self.warn_once(&item.chat_id, "delete", &err.to_string()),
            }

            let mut bots = 0usize;
            if self
                .settings_for(&self.app.peers, &item.chat_id, item.bot_id)
                .is_some()
            {
                let bot_peer = tl::enums::Peer::User(tl::types::PeerUser {
                    user_id: item.bot_id,
                });
                if let Ok(target) = client.input_peer(&bot_peer)
                    && self.first_ban(&item.chat_id, &item.bot_id.to_string())
                {
                    match ban(client, &item.channel, target).await {
                        Ok(()) => {
                            bots = 1;
                            self.app.logs.record(
                                mibot_logtail::Level::Info,
                                "guestban.banned",
                                &[
                                    ("chat", &item.chat_id),
                                    ("bot", &item.bot_id.to_string()),
                                ],
                            );
                        }
                        Err(err) => self.warn_once(&item.chat_id, "ban_bot", &err.to_string()),
                    }
                }
            }

            if let Some(settings) = self.settings_for(&self.app.peers, &item.chat_id, item.bot_id)
                && settings.strict
            {
                let invoker = peer_id(&item.via);
                if let Ok(target) = client.input_peer(&item.via)
                    && self.first_ban(&item.chat_id, &invoker)
                {
                    match ban(client, &item.channel, target).await {
                        Ok(()) => {
                            self.app.logs.record(
                                mibot_logtail::Level::Info,
                                "guestban.banned_invoker",
                                &[("chat", &item.chat_id), ("invoker", &invoker)],
                            );
                        }
                        Err(err) => self.warn_once(&item.chat_id, "ban_invoker", &err.to_string()),
                    }
                }
            }

            if deleted + bots > 0
                && let Err(err) = self.update(|cfg| {
                    if let Some(entry) = cfg.groups.get_mut(&item.chat_id) {
                        entry.messages += deleted;
                        entry.bots += bots;
                    }
                    Ok(())
                })
            {
                kit::warn(&self.app, "guestban.save_failed", &err);
            }
        };

        tokio::select! {
            _ = cancel.cancelled() => {},
            _ = tokio::time::timeout(Duration::from_secs(120), run) => {},
        }
    }

    fn first_ban(&self, chat_id: &str, target_id: &str) -> bool {
        let key = format!("{chat_id}/{target_id}");
        let now = Instant::now();
        let mut g = self.rate_mu.lock().unwrap_or_else(|e| e.into_inner());
        g.banned
            .retain(|_, at| now.saturating_duration_since(*at) <= Duration::from_secs(600));
        if g.banned.contains_key(&key) {
            return false;
        }
        g.banned.insert(key, now);
        true
    }

    fn warn_once(&self, chat_id: &str, action: &str, err: &str) {
        let key = format!("{chat_id}/{action}");
        let now = Instant::now();
        let mut g = self.rate_mu.lock().unwrap_or_else(|e| e.into_inner());
        let quiet = g
            .warned
            .get(&key)
            .is_some_and(|at| now.saturating_duration_since(*at) < Duration::from_secs(3600));
        if !quiet {
            g.warned.insert(key, now);
        }
        drop(g);
        if !quiet {
            self.app.logs.record(
                mibot_logtail::Level::Warn,
                "guestban.failed",
                &[("chat", chat_id), ("action", action), ("error", err)],
            );
        }
    }

    async fn handle(&self, inv: Invocation) -> CmdResult {
        let action = inv.arg(0).to_ascii_lowercase();
        match action.as_str() {
            "" => self.status(&inv).await,
            "on" | "off" => self.toggle(&inv, action == "on").await,
            "strict" => self.strict(&inv).await,
            "allow" => self.allow(&inv).await,
            "list" | "ls" => self.list(&inv).await,
            _ => Err(kit::usage(
                inv.prefix(),
                "guestban [on|off|strict on|off|allow|list]",
            )),
        }
    }

    async fn status(&self, inv: &Invocation) -> CmdResult {
        if here(&inv.client, &inv.message).is_err() {
            return self.list(inv).await;
        }
        let cfg = self.current();
        let entry = cfg.groups.get(&inv.message.chat_id);
        let prefix = inv.prefix();
        match entry {
            Some(e) if e.enabled => {
                let html = format!(
                    "🚫 <b>访客机器人清理</b>\n本群：开启{}\n已封禁 {} 个机器人，删除 {} 条消息",
                    strict_note(e.strict),
                    e.bots,
                    e.messages
                );
                inv.edit(&html).await?;
            }
            _ => {
                let html = format!(
                    "🚫 <b>访客机器人清理</b>\n本群：关闭，{} 开启",
                    code(&format!("{prefix}guestban on"))
                );
                inv.edit(&html).await?;
            }
        }
        Ok(())
    }

    async fn toggle(&self, inv: &Invocation, enable: bool) -> CmdResult {
        let channel_id = here(&inv.client, &inv.message)?;
        if enable {
            let info = inv.client.peers().channel(channel_id);
            let allowed = info.as_ref().is_some_and(|i| {
                i.creator
                    || i.admin_rights
                        .as_ref()
                        .is_some_and(|r| r.ban_users && r.delete_messages)
            });
            if !allowed {
                return Err(kit::fail(
                    "账号在本群没有封禁成员和删除消息的权限，开启了也清理不了",
                ));
            }
            let title = info.map(|i| i.title).unwrap_or_default();
            let chat_id = inv.message.chat_id.clone();
            self.update(move |cfg| {
                let entry = cfg.groups.entry(chat_id).or_default();
                entry.enabled = true;
                entry.title = title;
                Ok(())
            })?;
            inv.edit_text("✅ 本群已开启访客机器人清理").await?;
            return Ok(());
        }
        let chat_id = inv.message.chat_id.clone();
        self.update(move |cfg| {
            if let Some(entry) = cfg.groups.get_mut(&chat_id) {
                entry.enabled = false;
            }
            Ok(())
        })?;
        inv.edit_text("✅ 本群已关闭访客机器人清理").await?;
        Ok(())
    }

    async fn strict(&self, inv: &Invocation) -> CmdResult {
        here(&inv.client, &inv.message)?;
        let Ok(value) = kit::on_off(inv.arg(1)) else {
            return Err(kit::usage(inv.prefix(), "guestban strict on|off"));
        };
        let cfg = self.current();
        if !cfg
            .groups
            .get(&inv.message.chat_id)
            .is_some_and(|e| e.enabled)
        {
            return Err(kit::fail(format!(
                "本群还没开启，先发 {}guestban on",
                inv.prefix()
            )));
        }
        let chat_id = inv.message.chat_id.clone();
        self.update(move |cfg| {
            if let Some(entry) = cfg.groups.get_mut(&chat_id) {
                entry.strict = value;
            }
            Ok(())
        })?;
        inv.edit_text(&format!("✅ 严格模式已{}", kit::on_off_text(value)))
            .await?;
        Ok(())
    }

    async fn allow(&self, inv: &Invocation) -> CmdResult {
        let a1 = inv.arg(1);
        let remove = a1.eq_ignore_ascii_case("rm") || a1.eq_ignore_ascii_case("del");
        let name = if remove { inv.arg(2) } else { a1 };
        let entry = name.trim().trim_start_matches('@').to_ascii_lowercase();
        if entry.is_empty() {
            let cfg = self.current();
            if cfg.allow.is_empty() {
                inv.edit(&format!(
                    "🚫 <b>放行的访客机器人</b>\n还没有，{} 添加",
                    code(&format!("{}guestban allow @机器人", inv.prefix()))
                ))
                .await?;
                return Ok(());
            }
            let mut lines = vec!["🚫 <b>放行的访客机器人</b>".to_owned()];
            for item in &cfg.allow {
                if kit::is_digits(item) {
                    lines.push(format!("• {}", code(item)));
                } else {
                    lines.push(format!("• {}", code(&format!("@{item}"))));
                }
            }
            inv.edit(&lines.join("\n")).await?;
            return Ok(());
        }
        if !kit::is_digits(&entry) && !valid_username(&entry) {
            return Err(kit::fail("写机器人的 @用户名 或者数字 ID"));
        }
        let mut changed = false;
        let entry_clone = entry.clone();
        self.update(|cfg| {
            let pos = cfg.allow.iter().position(|x| x == &entry_clone);
            match (remove, pos) {
                (true, Some(i)) => {
                    cfg.allow.remove(i);
                    changed = true;
                }
                (false, None) => {
                    cfg.allow.push(entry_clone);
                    changed = true;
                }
                _ => {}
            }
            Ok(())
        })?;
        let label = if kit::is_digits(&entry) {
            entry
        } else {
            format!("@{entry}")
        };
        if remove && changed {
            inv.edit_text(&format!("✅ 已取消放行 {label}")).await?;
        } else if remove {
            return Err(kit::fail(format!("放行名单里没有 {label}")));
        } else if changed {
            inv.edit_text(&format!("✅ 已放行 {label}")).await?;
        } else {
            inv.edit_text(&format!("✅ {label} 已经在放行名单里"))
                .await?;
        }
        Ok(())
    }

    async fn list(&self, inv: &Invocation) -> CmdResult {
        let cfg = self.current();
        let mut lines = Vec::new();
        for (id, entry) in &cfg.groups {
            if !entry.enabled {
                continue;
            }
            let title = kit::or_default(&entry.title, id);
            lines.push(format!(
                "• {}{}：封禁 {} 个机器人，删除 {} 条",
                escape(title),
                strict_note(entry.strict),
                entry.bots,
                entry.messages
            ));
        }
        if lines.is_empty() {
            inv.edit(&format!(
                "🚫 <b>访客机器人清理</b>\n还没有群组开启，在群里发 {}",
                code(&format!("{}guestban on", inv.prefix()))
            ))
            .await?;
            return Ok(());
        }
        let header = format!(
            "🚫 <b>访客机器人清理</b>（{} 个群组，放行 {} 个机器人）",
            lines.len(),
            cfg.allow.len()
        );
        let body = format!("{header}\n{}", lines.join("\n"));
        let pages = html_pages(&body, PAGE_LIMIT);
        inv.respond_pages(&pages).await?;
        Ok(())
    }
}

async fn ban(
    client: &Client,
    channel: &tl::types::InputChannel,
    target: tl::enums::InputPeer,
) -> Result<(), mibot_bot::BotError> {
    client
        .invoke(&tl::functions::channels::EditBanned {
            channel: channel.clone().into(),
            participant: target,
            banned_rights: kit::ban_rights(0).into(),
        })
        .await?;
    Ok(())
}

fn here(client: &Client, msg: &Message) -> Result<i64, mibot_command::CmdError> {
    let tl::enums::Peer::Channel(ch) = &msg.peer else {
        return Err(kit::fail("要在超级群组里用"));
    };
    if client
        .peers()
        .channel(ch.channel_id)
        .is_some_and(|i| i.broadcast)
    {
        return Err(kit::fail("要在超级群组里用，频道里没有访客机器人"));
    }
    Ok(ch.channel_id)
}

fn strict_note(strict: bool) -> &'static str {
    if strict {
        "，严格模式（叫它的人一起封）"
    } else {
        ""
    }
}

fn valid_username(name: &str) -> bool {
    if name.len() < 4 || name.len() > 32 {
        return false;
    }
    let first = name.as_bytes()[0];
    if !first.is_ascii_lowercase() {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

pub fn help(prefix: &str) -> String {
    let c = |text: &str| code(&format!("{prefix}guestban{text}"));
    format!(
        "🚫 <b>访客机器人清理</b>\n\n\
         访客机器人不用进群，群里有人提到它就能把消息贴进来，显示成「机器人名 来自 某人」。\
         广告号拿它刷屏：机器人的名字就是广告词。开启后，在这个群组里看到这种消息就删掉，\
         并在本群封禁那个机器人，之后它再也贴不进来。\n\n\
         <b>本群</b>\n\
         • {} 查看本群的状态\n\
         • {}、{} 在本群开启或关闭\n\
         • {} 严格模式：叫出机器人的人也一起封禁（默认关闭，对方多半是一次性小号）\n\n\
         <b>放行和总览</b>\n\
         • {} 放行正经的访客机器人，{} 取消，{} 查看\n\
         • {} 开启了的群组和清理次数\n\n\
         账号要是本群的管理员，有封禁成员和删除消息的权限。只用于超级群组。",
        c(""),
        c(" on"),
        c(" off"),
        c(" strict on|off"),
        c(" allow @机器人"),
        c(" allow rm @机器人"),
        c(" allow"),
        c(" list"),
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
        name: "guestban".into(),
        group: group::ADMIN.into(),
        description: "自动清理访客机器人广告".into(),
        usage: "[on|off|strict|allow|list]".into(),
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

    struct RecorderInner {
        deleted: Vec<i32>,
        banned: Vec<String>,
        on_delete: Option<Box<dyn FnMut() + Send>>,
    }

    struct Recorder {
        inner: Mutex<RecorderInner>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                inner: Mutex::new(RecorderInner {
                    deleted: Vec::new(),
                    banned: Vec::new(),
                    on_delete: None,
                }),
            }
        }
    }

    impl RpcTransport for Recorder {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                if constructor_id == tl::functions::channels::DeleteMessages::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::DeleteMessages::from_bytes(&body[4..]).unwrap();
                    let cb = {
                        let mut g = self.inner.lock().unwrap();
                        g.deleted.extend(req.id.iter().copied());
                        g.on_delete.take()
                    };
                    if let Some(mut f) = cb {
                        f();
                    }
                    let resp: tl::enums::messages::AffectedMessages =
                        tl::types::messages::AffectedMessages {
                            pts: 1,
                            pts_count: req.id.len() as i32,
                        }
                        .into();
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::channels::EditBanned::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::EditBanned::from_bytes(&body[4..]).unwrap();
                    let mut g = self.inner.lock().unwrap();
                    match req.participant {
                        tl::enums::InputPeer::User(u) => {
                            g.banned.push(format!("user{}", u.user_id));
                        }
                        tl::enums::InputPeer::Channel(c) => {
                            g.banned.push(format!("channel-100{}", c.channel_id));
                        }
                        _ => {}
                    }
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
        let dir = std::env::temp_dir().join(format!("mibot-guestban-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mk_user(id: i64, bot: bool, username: Option<&str>) -> tl::enums::User {
        tl::types::User {
            is_self: id == 1,
            contact: false,
            mutual_contact: false,
            deleted: false,
            bot,
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
            stories_unavailable: true,
            contact_require_premium: false,
            bot_business: false,
            bot_has_main_app: false,
            bot_forum_view: false,
            bot_forum_can_manage_topics: false,
            bot_can_manage_bots: false,
            bot_guestchat: bot,
            bot_guard: false,
            bot_forum_sidebar: false,
            id,
            access_hash: Some(9),
            first_name: None,
            last_name: None,
            username: username.map(str::to_owned),
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
        .into()
    }

    fn mk_megagroup(id: i64, title: &str) -> tl::enums::Chat {
        tl::types::Channel {
            creator: false,
            left: false,
            broadcast: false,
            verified: false,
            megagroup: true,
            restricted: false,
            signatures: false,
            min: false,
            scam: false,
            has_link: false,
            has_geo: false,
            slowmode_enabled: false,
            call_active: false,
            call_not_empty: false,
            fake: false,
            gigagroup: false,
            noforwards: false,
            join_to_send: false,
            join_request: false,
            forum: false,
            stories_hidden: false,
            stories_hidden_min: false,
            stories_unavailable: true,
            signature_profiles: false,
            autotranslation: false,
            broadcast_messages_allowed: false,
            monoforum: false,
            forum_tabs: false,
            in_news: false,
            forum_sidebar: false,
            id,
            access_hash: Some(9),
            title: title.into(),
            username: None,
            photo: tl::enums::ChatPhoto::Empty,
            date: 0,
            restriction_reason: None,
            admin_rights: None,
            banned_rights: None,
            default_banned_rights: None,
            participants_count: None,
            usernames: None,
            stories_max_id: None,
            color: None,
            profile_color: None,
            emoji_status: None,
            level: None,
            subscription_until_date: None,
            bot_verification_icon: None,
            send_paid_messages_stars: None,
            linked_monoforum_id: None,
            linked_community_id: None,
            pinned_msg_id: None,
        }
        .into()
    }

    fn setup(name: &str) -> (Arc<Service>, Arc<Recorder>, PathBuf) {
        let dir = temp_dir(name);
        let fake = Arc::new(Recorder::new());
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        peers.remember_entities(
            &[
                mk_user(42, true, Some("ppzmzbot")),
                mk_user(77, false, None),
                mk_user(78, false, None),
                mk_user(43, true, Some("GoodBot")),
            ],
            &[mk_megagroup(5000, "群")],
        );
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
            client,
            peers,
            Arc::new(Ring::new()),
        );
        (Service::new(app), fake, dir)
    }

    fn guest_message(id: i32, from: i64, via: i64) -> Message {
        let mut raw = kit::blank_tl_message(
            id,
            tl::types::PeerChannel { channel_id: 5000 }.into(),
        );
        raw.from_id = Some(tl::types::PeerUser { user_id: from }.into());
        raw.message = "进直播间当水军，一次给500".into();
        if via != 0 {
            raw.guestchat_via_from = Some(tl::types::PeerUser { user_id: via }.into());
        }
        Message {
            id,
            peer: tl::types::PeerChannel { channel_id: 5000 }.into(),
            chat_id: "-1005000".into(),
            chat_type: mibot_bot::ChatType::Supergroup,
            text: raw.message.clone(),
            out: false,
            edited: false,
            forward: false,
            post: false,
            saved: false,
            sender: tl::types::PeerUser { user_id: from }.into(),
            reply_to_id: 0,
            reply_to_peer: None,
            topic_id: 0,
            quote_text: String::new(),
            quote_entities: Vec::new(),
            raw,
        }
    }

    async fn drain(s: &Service) {
        let cancel = CancellationToken::new();
        let mut rx = s.rx.lock().unwrap().take().unwrap();
        while let Ok(item) = rx.try_recv() {
            s.process(&cancel, &item).await;
        }
        *s.rx.lock().unwrap() = Some(rx);
    }

    #[tokio::test]
    async fn test_guest_bot_cleanup() {
        let (s, fake, dir) = setup("cleanup");
        s.offer(&guest_message(10, 42, 77));
        drain(&s).await;
        assert!(fake.inner.lock().unwrap().deleted.is_empty());

        s.update(|cfg| {
            cfg.groups.insert(
                "-1005000".into(),
                GroupConfig {
                    enabled: true,
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
        for m in [
            guest_message(11, 42, 77),
            guest_message(12, 42, 78),
            guest_message(13, 42, 77),
            guest_message(14, 77, 0),
        ] {
            assert!(!s.offer(&m));
        }
        s.update(|cfg| {
            cfg.allow = vec!["goodbot".into()];
            Ok(())
        })
        .unwrap();
        s.offer(&guest_message(15, 43, 77));
        drain(&s).await;

        let g = fake.inner.lock().unwrap();
        assert_eq!(g.deleted, vec![11, 12, 13]);
        assert_eq!(g.banned, vec!["user42"]);
        drop(g);

        let cfg = s.store.read().unwrap();
        let entry = &cfg.groups["-1005000"];
        assert_eq!(entry.bots, 1);
        assert_eq!(entry.messages, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_strict_bans_invoker() {
        let (s, fake, dir) = setup("strict");
        s.update(|cfg| {
            cfg.groups.insert(
                "-1005000".into(),
                GroupConfig {
                    enabled: true,
                    strict: true,
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
        s.offer(&guest_message(20, 42, 77));
        s.offer(&guest_message(21, 42, 78));
        s.offer(&guest_message(22, 42, 77));
        drain(&s).await;
        assert_eq!(
            fake.inner.lock().unwrap().banned,
            vec!["user42", "user77", "user78"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_queued_guest_cleanup_uses_current_settings() {
        for (name, case) in ["disabled", "allowed", "strict_disabled"]
            .into_iter()
            .enumerate()
        {
            let (s, fake, dir) = setup(&format!("queued-{name}"));
            s.update(|cfg| {
                cfg.groups.insert(
                    "-1005000".into(),
                    GroupConfig {
                        enabled: true,
                        strict: true,
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();
            s.offer(&guest_message(20, 42, 77));
            s.update(|cfg| {
                match case {
                    "disabled" => cfg.groups.get_mut("-1005000").unwrap().enabled = false,
                    "allowed" => cfg.allow = vec!["ppzmzbot".into()],
                    "strict_disabled" => cfg.groups.get_mut("-1005000").unwrap().strict = false,
                    _ => unreachable!(),
                }
                Ok(())
            })
            .unwrap();
            drain(&s).await;
            let g = fake.inner.lock().unwrap();
            if case == "strict_disabled" {
                assert_eq!(g.deleted, vec![20]);
                assert_eq!(g.banned, vec!["user42"]);
            } else {
                assert!(g.deleted.is_empty());
                assert!(g.banned.is_empty());
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn test_disabling_while_delete_runs_prevents_bans() {
        let (s, fake, dir) = setup("disable-midway");
        s.update(|cfg| {
            cfg.groups.insert(
                "-1005000".into(),
                GroupConfig {
                    enabled: true,
                    strict: true,
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
        let s_cb = Arc::clone(&s);
        fake.inner.lock().unwrap().on_delete = Some(Box::new(move || {
            s_cb.update(|cfg| {
                cfg.groups.get_mut("-1005000").unwrap().enabled = false;
                Ok(())
            })
            .unwrap();
        }));
        s.offer(&guest_message(20, 42, 77));
        drain(&s).await;
        let g = fake.inner.lock().unwrap();
        assert_eq!(g.deleted, vec![20]);
        assert!(g.banned.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_allowed_and_username() {
        let cfg = Config {
            groups: BTreeMap::new(),
            allow: vec!["goodbot".into(), "123".into()],
        };
        assert!(cfg.allowed(5, "GoodBot"));
        assert!(cfg.allowed(123, ""));
        assert!(!cfg.allowed(5, "ppzmzbot"));
        assert!(valid_username("ppzmzbot"));
        assert!(!valid_username("abc"));
        assert!(!valid_username("1bot"));
        assert!(!valid_username("bad-name"));
    }
}
