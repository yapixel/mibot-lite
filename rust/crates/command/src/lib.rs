pub mod errors;
pub mod scheduler;
pub mod text;

pub use errors::{CmdError, CmdResult, UserError, brief, fail, fail_with, is_user_error, truncate};
pub use scheduler::{
    COMMAND_QUEUE, COMMAND_WORKERS, DELAYED_QUEUE, DELAYED_WORKERS, LONG_TASKS, MAX_TIMERS,
    PanicHook, QuiesceTimeout, Scheduler, TASK_QUEUE, TASK_WORKERS, WorkGuard,
};
pub use text::{PAGE_LIMIT, bold, code, escape, escaped_pages, html_pages, utf16_len};

use grammers_tl_types as tl;
use mibot_bot::{BotError, BoxFuture, Client, Message, SendOptions};
use mibot_logtail::{Level, Ring};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub mod group {
    pub const SYSTEM: &str = "Running & Ops";
    pub const TOOLS: &str = "Lookups & Tools";
    pub const AI: &str = "AI";
    pub const MEDIA: &str = "Messages & Stickers";
    pub const ADMIN: &str = "Group Admin";
    pub const ACCOUNT: &str = "Account";
    pub const GENERAL: &str = SYSTEM;
    pub const STATUS: &str = SYSTEM;
    pub const OPS: &str = SYSTEM;
    pub const LOGS: &str = SYSTEM;

    pub fn zh_name(grp: &str) -> Option<&'static str> {
        match grp {
            SYSTEM => Some("运行与维护"),
            TOOLS => Some("查询与工具"),
            AI => Some("AI"),
            MEDIA => Some("消息与贴纸"),
            ADMIN => Some("群组管理"),
            ACCOUNT => Some("账号"),
            _ => None,
        }
    }

    pub fn display(grp: &str) -> &str {
        if mibot_i18n::is_zh()
            && let Some(zh) = zh_name(grp)
        {
            return zh;
        }
        grp
    }

    pub fn matches(grp: &str, needle_lower: &str) -> bool {
        grp.to_ascii_lowercase().contains(needle_lower)
            || zh_name(grp).is_some_and(|zh| zh.to_ascii_lowercase().contains(needle_lower))
    }
}

pub const GROUPS: &[&str] = &[
    group::SYSTEM,
    group::TOOLS,
    group::AI,
    group::MEDIA,
    group::ADMIN,
    group::ACCOUNT,
];

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const RELAY_DEDUP_WINDOW: Duration = Duration::from_secs(10 * 60);
const RELAY_DEDUP_CAP: usize = 2048;

pub type HandlerFn = Arc<dyn Fn(Invocation) -> BoxFuture<'static, CmdResult> + Send + Sync>;
pub type HelpFn = Arc<dyn Fn(&str) -> String + Send + Sync>;
pub type ErrorHook = Arc<dyn Fn(&Invocation, &CmdError) + Send + Sync>;

#[derive(Clone)]
pub struct Command {
    pub name: String,
    pub aliases: Vec<String>,
    pub group: String,
    pub description: String,
    pub description_zh: String,
    pub usage: String,
    pub usage_zh: String,
    pub help: String,
    pub help_fn: Option<HelpFn>,
    pub handle: HandlerFn,
    pub timeout: Option<Duration>,
    pub free_text: bool,
}

impl Command {
    pub fn desc_text(&self) -> &str {
        if mibot_i18n::is_zh() && !self.description_zh.is_empty() {
            &self.description_zh
        } else {
            &self.description
        }
    }

    pub fn usage_text(&self) -> &str {
        if mibot_i18n::is_zh() && !self.usage_zh.is_empty() {
            &self.usage_zh
        } else {
            &self.usage
        }
    }

    pub fn zh(mut self, description: impl Into<String>, usage: impl Into<String>) -> Self {
        self.description_zh = description.into();
        self.usage_zh = usage.into();
        self
    }
}

impl Default for Command {
    fn default() -> Self {
        Self {
            name: String::new(),
            aliases: Vec::new(),
            group: group::GENERAL.into(),
            description: String::new(),
            description_zh: String::new(),
            usage: String::new(),
            usage_zh: String::new(),
            help: String::new(),
            help_fn: None,
            handle: Arc::new(|_| Box::pin(async { Ok(()) })),
            timeout: None,
            free_text: false,
        }
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .field("aliases", &self.aliases)
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct Invocation {
    pub client: Arc<Client>,
    pub registry: Arc<Registry>,
    pub message: Message,
    pub prefix: String,
    pub command: String,
    pub args: Vec<String>,
    pub raw: String,
    pub cancel: CancellationToken,
    pub foreign: bool,
}

impl Invocation {
    /// Returns the raw remainder after skipping the first `n` whitespace-delimited arguments,
    /// preserving original internal spaces and newlines.
    pub fn raw_after(&self, n: usize) -> &str {
        let mut s = self.raw.trim_start_matches(char::is_whitespace);
        for _ in 0..n {
            let next = match s.find(char::is_whitespace) {
                Some(idx) => &s[idx..],
                None => return "",
            };
            s = next.trim_start_matches(char::is_whitespace);
        }
        s
    }

    pub fn arg(&self, idx: usize) -> &str {
        self.args.get(idx).map(String::as_str).unwrap_or("")
    }

    pub fn prefix(&self) -> &str {
        if self.prefix.is_empty() {
            "."
        } else {
            &self.prefix
        }
    }

    pub fn peer_result(&self) -> Result<tl::enums::InputPeer, BotError> {
        self.client.input_peer(&self.message.peer)
    }

    /// Sends a new HTML reply quoting the triggering message.
    pub async fn reply(&self, html: &str) -> Result<tl::enums::Updates, BotError> {
        let peer = self.peer_result()?;
        self.client
            .send_html(
                &peer,
                html,
                SendOptions {
                    reply_to: Some(self.message.id),
                    topic_id: Some(self.message.topic_id).filter(|&id| id > 0),
                    no_webpage: true,
                    entities: None,
                },
            )
            .await
    }

    pub async fn reply_text(&self, text: &str) -> Result<tl::enums::Updates, BotError> {
        self.reply(&escape(text)).await
    }

    /// Edits the triggering message in place when possible, ignoring `MESSAGE_NOT_MODIFIED`.
    pub async fn edit(&self, html: &str) -> Result<(), BotError> {
        let peer = self.peer_result()?;
        if self.message.out && !self.foreign {
            match self
                .client
                .edit_html(
                    &peer,
                    self.message.id,
                    html,
                    SendOptions {
                        no_webpage: true,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) if e.is_rpc("MESSAGE_NOT_MODIFIED") => return Ok(()),
                Err(e) if e.is_uncertain() || e.flood_wait().is_some() => return Err(e),
                Err(_) => {}
            }
        }
        self.reply(html).await.map(|_| ())
    }

    pub async fn edit_text(&self, text: &str) -> Result<(), BotError> {
        self.edit(&escape(text)).await
    }

    /// Edits the triggering outgoing message in place when possible, or falls back to `reply`.
    pub async fn respond(&self, html: &str) -> Result<tl::enums::Updates, BotError> {
        let peer = self.peer_result()?;
        if self.message.out && !self.foreign {
            match self
                .client
                .edit_html(
                    &peer,
                    self.message.id,
                    html,
                    SendOptions {
                        no_webpage: true,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(u) => return Ok(u),
                Err(e) if e.is_rpc("MESSAGE_NOT_MODIFIED") => {
                    return Ok(tl::enums::Updates::TooLong);
                }
                Err(e) if e.is_uncertain() || e.flood_wait().is_some() => return Err(e),
                Err(_) => {}
            }
        }
        self.reply(html).await
    }

    pub async fn respond_text(&self, text: &str) -> Result<tl::enums::Updates, BotError> {
        self.respond(&escape(text)).await
    }

    pub async fn respond_pages(&self, pages: &[String]) -> Result<(), BotError> {
        if pages.is_empty() {
            return Ok(());
        }
        self.respond(&pages[0]).await?;
        for page in &pages[1..] {
            self.reply(page).await?;
        }
        Ok(())
    }

    pub async fn edit_rich(
        &self,
        rich: tl::enums::InputRichMessage,
        fallback_html: &str,
    ) -> Result<tl::enums::Updates, BotError> {
        let peer = self.peer_result()?;
        if self.message.out && !self.foreign && self.client.can_send_rich() {
            match self
                .client
                .edit_rich(
                    &peer,
                    self.message.id,
                    rich,
                    SendOptions {
                        no_webpage: true,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(u) => return Ok(u),
                Err(e) if e.is_rpc("MESSAGE_NOT_MODIFIED") => {
                    return Ok(tl::enums::Updates::TooLong);
                }
                Err(e) if e.is_uncertain() || e.flood_wait().is_some() => return Err(e),
                Err(_) => {}
            }
        }
        self.respond(fallback_html).await
    }

    pub async fn delete(&self) -> Result<(), BotError> {
        self.client.delete_message(&self.message).await
    }

    pub async fn get_reply(&self) -> Result<Option<Message>, BotError> {
        self.client.get_reply(&self.message).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub prefix: String,
    pub command: String,
    pub args: Vec<String>,
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    Busy,
    InvalidTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    NotCommand,
    Queued,
    Rejected(RejectReason),
}

#[derive(Clone)]
struct UserAlias {
    trigger: String,
    word_count: usize,
    expansion: String,
}

#[derive(Default)]
struct RegistryData {
    commands: Vec<Arc<Command>>,
    by_name: HashMap<String, Arc<Command>>,
    builtin_aliases: HashMap<String, Arc<Command>>,
    prefixes: Vec<String>,
    user_aliases: Vec<UserAlias>,
    user_alias_map: HashMap<String, String>,
    logs: Option<Arc<Ring>>,
    on_error: Option<ErrorHook>,
}

struct DedupEntry {
    key: String,
    at: Instant,
}

#[derive(Default)]
struct RelayDedup {
    seen: HashSet<String>,
    order: std::collections::VecDeque<DedupEntry>,
}

impl RelayDedup {
    fn mark(&mut self, key: String, now: Instant) -> bool {
        while let Some(front) = self.order.front() {
            if now.saturating_duration_since(front.at) > RELAY_DEDUP_WINDOW {
                if let Some(removed) = self.order.pop_front() {
                    self.seen.remove(&removed.key);
                }
            } else {
                break;
            }
        }
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(DedupEntry { key, at: now });
        while self.order.len() > RELAY_DEDUP_CAP {
            if let Some(removed) = self.order.pop_front() {
                self.seen.remove(&removed.key);
            }
        }
        true
    }
}

pub struct Registry {
    client: RwLock<Option<Arc<Client>>>,
    data: RwLock<RegistryData>,
    scheduler: Scheduler,
    dedup: Mutex<RelayDedup>,
    active_ids: Mutex<HashMap<String, u64>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let scheduler = Scheduler::new();
        Self {
            client: RwLock::new(None),
            data: RwLock::new(RegistryData {
                prefixes: vec![".".into()],
                ..RegistryData::default()
            }),
            scheduler,
            dedup: Mutex::new(RelayDedup::default()),
            active_ids: Mutex::new(HashMap::new()),
        }
    }

    pub fn bind_client(&self, client: Arc<Client>) {
        *self.client.write().unwrap_or_else(|e| e.into_inner()) = Some(client);
    }

    pub fn set_logs(&self, logs: Arc<Ring>) {
        let ring = Arc::clone(&logs);
        self.scheduler.set_panic_hook(Arc::new(move |pool, msg| {
            ring.record(
                Level::Error,
                "command.panic",
                &[("pool", pool), ("err", msg)],
            );
        }));
        self.data.write().unwrap_or_else(|e| e.into_inner()).logs = Some(logs);
    }

    pub fn set_error_hook(&self, hook: ErrorHook) {
        self.data
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .on_error = Some(hook);
    }

    pub fn set_prefixes(&self, prefixes: Vec<String>) {
        let mut clean: Vec<String> = prefixes.into_iter().filter(|p| !p.is_empty()).collect();
        clean.sort_by_key(|p| std::cmp::Reverse(p.len()));
        clean.dedup();
        if clean.is_empty() {
            clean.push(".".into());
        }
        self.data
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .prefixes = clean;
    }

    pub fn set_aliases(&self, aliases: &HashMap<String, String>) {
        let mut list = Vec::new();
        let mut single = HashMap::new();
        for (raw_trigger, raw_exp) in aliases {
            let trigger = raw_trigger.split_whitespace().collect::<Vec<_>>().join(" ");
            let expansion = raw_exp.trim().to_owned();
            if trigger.is_empty() || expansion.is_empty() {
                continue;
            }
            let wc = trigger.split_whitespace().count();
            if wc == 1 {
                single.insert(trigger.clone(), expansion.clone());
            }
            list.push(UserAlias {
                trigger,
                word_count: wc,
                expansion,
            });
        }
        list.sort_by(|a, b| {
            b.word_count
                .cmp(&a.word_count)
                .then_with(|| b.trigger.len().cmp(&a.trigger.len()))
                .then_with(|| a.trigger.cmp(&b.trigger))
        });
        let mut guard = self.data.write().unwrap_or_else(|e| e.into_inner());
        guard.user_aliases = list;
        guard.user_alias_map = single;
    }

    /// Registers a command. Panics on invalid or duplicate command names or colliding built-in aliases,
    /// catching programming errors at startup (matching Go `command.Registry.Register`).
    pub fn register(&self, cmd: Command) {
        assert!(
            is_valid_name(&cmd.name),
            "invalid command name: {:?}",
            cmd.name
        );
        let mut guard = self.data.write().unwrap_or_else(|e| e.into_inner());
        assert!(
            !guard.by_name.contains_key(&cmd.name)
                && !guard.builtin_aliases.contains_key(&cmd.name),
            "duplicate command name: {}",
            cmd.name
        );
        for alias in &cmd.aliases {
            assert!(
                is_valid_name(alias),
                "invalid alias {:?} on command {}",
                alias,
                cmd.name
            );
            assert!(
                !guard.by_name.contains_key(alias) && !guard.builtin_aliases.contains_key(alias),
                "duplicate alias {} on command {}",
                alias,
                cmd.name
            );
        }
        let arc = Arc::new(cmd);
        guard.by_name.insert(arc.name.clone(), Arc::clone(&arc));
        for alias in &arc.aliases {
            guard
                .builtin_aliases
                .insert(alias.clone(), Arc::clone(&arc));
        }
        guard.commands.push(arc);
    }

    pub fn command(&self, name: &str) -> Option<Arc<Command>> {
        let g = self.data.read().unwrap_or_else(|e| e.into_inner());
        g.by_name
            .get(name)
            .or_else(|| g.builtin_aliases.get(name))
            .cloned()
    }

    pub fn list(&self) -> Vec<Arc<Command>> {
        self.data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .commands
            .clone()
    }

    pub fn prefixes(&self) -> Vec<String> {
        self.data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .prefixes
            .clone()
    }

    pub fn aliases(&self) -> HashMap<String, String> {
        self.data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .user_alias_map
            .clone()
    }

    pub fn help_text(&self, cmd: &Command) -> String {
        let prefix = self
            .prefixes()
            .into_iter()
            .next()
            .unwrap_or_else(|| ".".into());
        self.help_text_for(cmd, &prefix)
    }

    pub fn help_text_for(&self, cmd: &Command, prefix: &str) -> String {
        let mut out = if let Some(f) = &cmd.help_fn {
            f(prefix)
        } else if !cmd.help.is_empty() {
            cmd.help.clone()
        } else if !cmd.usage_text().is_empty() {
            format!(
                "<b>{}</b> — {}\n{}<code>{}</code>",
                escape(&cmd.name),
                escape(cmd.desc_text()),
                mibot_i18n::tr("Usage: ", "用法："),
                escape(cmd.usage_text())
            )
        } else {
            format!("<b>{}</b> — {}", escape(&cmd.name), escape(cmd.desc_text()))
        };
        let unmentioned: Vec<&str> = cmd
            .aliases
            .iter()
            .map(String::as_str)
            .filter(|a| !contains_alias_token(&out, a))
            .collect();
        if !unmentioned.is_empty() {
            out.push_str(mibot_i18n::tr("\nAliases: ", "\n简写："));
            let escaped: Vec<String> = unmentioned.iter().map(|a| code(a)).collect();
            out.push_str(&escaped.join(" "));
        }
        out
    }

    pub fn parse(&self, input: &str) -> Option<Route> {
        let guard = self.data.read().unwrap_or_else(|e| e.into_inner());
        let mut matched_prefix = None;
        let mut matched_rest = None;
        for prefix in &guard.prefixes {
            if let Some(rest) = input.strip_prefix(prefix.as_str()) {
                matched_prefix = Some(prefix.clone());
                matched_rest = Some(rest);
                break;
            }
        }
        let prefix = matched_prefix?;
        let rest = matched_rest?;
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return None;
        }

        // 1. Longest multi-word user alias beats single-word command lookup.
        for ua in guard.user_aliases.iter().filter(|a| a.word_count > 1) {
            if let Some(after) = match_multiword_alias(rest, &ua.trigger) {
                return resolve_expansion(&guard, &prefix, &ua.expansion, after);
            }
        }

        // 2. Extract first whitespace-delimited token.
        let (head, tail) = split_first_token(rest);
        if head.is_empty() {
            return None;
        }
        let raw_tail = tail.trim_start_matches(char::is_whitespace);

        // 3. Registered command or built-in alias beats 1-word user alias.
        if let Some(cmd) = guard
            .by_name
            .get(head)
            .or_else(|| guard.builtin_aliases.get(head))
        {
            return Some(Route {
                prefix,
                command: cmd.name.clone(),
                args: split_args(raw_tail),
                raw: raw_tail.to_owned(),
            });
        }

        // 4. Fall back to 1-word user alias.
        if let Some(expansion) = guard.user_alias_map.get(head) {
            return resolve_expansion(&guard, &prefix, expansion, raw_tail);
        }

        None
    }

    pub fn dispatch(
        self: &Arc<Self>,
        msg: Message,
        cancel: CancellationToken,
    ) -> DispatchOutcome {
        self.dispatch_for(msg, false, cancel)
    }

    pub fn dispatch_for(
        self: &Arc<Self>,
        msg: Message,
        foreign: bool,
        cancel: CancellationToken,
    ) -> DispatchOutcome {
        self.dispatch_for_with_gate(msg, foreign, cancel, |_| true)
    }

    pub fn dispatch_for_with_gate<G>(
        self: &Arc<Self>,
        msg: Message,
        foreign: bool,
        cancel: CancellationToken,
        allowed: G,
    ) -> DispatchOutcome
    where
        G: FnOnce(&Route) -> bool,
    {
        let chat_key = mibot_bot::peer_id(&msg.peer);
        let dedup_key = format!("{chat_key}:{}", msg.id);
        if !self
            .dedup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark(dedup_key, Instant::now())
        {
            return DispatchOutcome::NotCommand;
        }

        let Some(route) = self.parse(&msg.text) else {
            return DispatchOutcome::NotCommand;
        };
        if !allowed(&route) {
            return DispatchOutcome::NotCommand;
        }
        let Some(cmd) = self.command(&route.command) else {
            return DispatchOutcome::NotCommand;
        };
        let Some(client) = self.client.read().unwrap_or_else(|e| e.into_inner()).clone() else {
            return DispatchOutcome::Rejected(RejectReason::InvalidTarget);
        };

        let inv = Invocation {
            client: Arc::clone(&client),
            registry: Arc::clone(self),
            message: msg,
            prefix: route.prefix.clone(),
            command: cmd.name.clone(),
            args: route.args.clone(),
            raw: route.raw.clone(),
            cancel: cancel.child_token(),
            foreign,
        };

        let slot_key = self.track(&inv);
        let reg_ref = Arc::clone(self);
        let release: Box<dyn FnOnce() + Send> = Box::new(move || {
            if let Some(k) = slot_key {
                reg_ref
                    .active_ids
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&k);
            }
        });

        let inv_for_run = inv.clone();
        let cmd_for_run = Arc::clone(&cmd);
        let reg_for_run = Arc::clone(self);
        let sent_observer = mibot_bot::current_sent_observer();
        let ip_privacy_off = mibot_bot::is_ip_privacy_off();

        let admitted = self.scheduler.submit(inv.cancel.clone(), Some(release), move |tok| {
            Box::pin(async move {
                mibot_bot::with_optional_sent_observer(sent_observer, move || {
                    mibot_bot::with_optional_ip_privacy_off(ip_privacy_off, move || async move {
                        let mut inv = inv_for_run;
                        inv.cancel = tok;
                        if wants_help(&cmd_for_run, &inv.args) {
                            let help = reg_for_run.help_text_for(&cmd_for_run, inv.prefix());
                            let _ = inv.respond(&help).await;
                            return;
                        }
                        let timeout = cmd_for_run.timeout.unwrap_or(DEFAULT_TIMEOUT);
                        let child_tok = inv.cancel.clone();
                        let res = if timeout.is_zero() {
                            (cmd_for_run.handle)(inv.clone()).await
                        } else {
                            match tokio::time::timeout(timeout, (cmd_for_run.handle)(inv.clone())).await {
                                Ok(inner) => inner,
                                Err(elapsed) => {
                                    child_tok.cancel();
                                    Err(Box::new(elapsed) as CmdError)
                                }
                            }
                        };
                        if let Err(err) = res {
                            let hook = reg_for_run
                                .data
                                .read()
                                .unwrap_or_else(|e| e.into_inner())
                                .on_error
                                .clone();
                            if let Some(h) = hook {
                                h(&inv, &err);
                            }
                            if let Some(logs) = reg_for_run
                                .data
                                .read()
                                .unwrap_or_else(|e| e.into_inner())
                                .logs
                                .clone()
                            {
                                let err_str = err.to_string();
                                logs.record(
                                    Level::Error,
                                    "command.error",
                                    &[("cmd", &inv.command), ("err", &err_str)],
                                );
                            }
                            let text = format!("❌ {}", escape(&brief(err.as_ref())));
                            let _ = inv.respond(&text).await;
                        }
                    })
                })
                .await;
            })
        });

        if !admitted {
            if !self.scheduler.is_closed() {
                let inv_reject = inv;
                tokio::spawn(async move {
                    let _ = inv_reject
                        .respond(mibot_i18n::tr(
                            "⏳ Too many queued tasks, please try again later",
                            "⏳ 当前排队任务过多，请稍后再试",
                        ))
                        .await;
                });
            }
            return DispatchOutcome::Rejected(RejectReason::Busy);
        }
        DispatchOutcome::Queued
    }

    pub fn track(&self, inv: &Invocation) -> Option<String> {
        let key = format!("{}:{}", mibot_bot::peer_id(&inv.message.peer), inv.message.id);
        let mut g = self.active_ids.lock().unwrap_or_else(|e| e.into_inner());
        let next = g.values().copied().max().unwrap_or(0) + 1;
        g.insert(key.clone(), next);
        Some(key)
    }

    pub fn running(&self) -> Vec<String> {
        self.active_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    pub fn background(
        &self,
        cancel: CancellationToken,
        task: impl FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> bool {
        self.scheduler.background(cancel, task)
    }

    pub fn long_running(
        &self,
        cancel: CancellationToken,
        task: impl FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> bool {
        self.scheduler.long_running(cancel, task)
    }

    pub fn after(
        &self,
        cancel: CancellationToken,
        delay: Duration,
        task: impl FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> bool {
        self.scheduler.after(cancel, delay, task)
    }

    pub async fn quiesce(&self, timeout: Duration) -> Result<(), QuiesceTimeout> {
        self.scheduler.quiesce(timeout).await
    }

    pub fn resume(&self) {
        self.scheduler.resume();
    }

    pub fn is_closed(&self) -> bool {
        self.scheduler.is_closed()
    }

    pub fn busy(&self) -> usize {
        self.scheduler.busy()
    }

    pub fn track_work(&self) -> Option<WorkGuard> {
        self.scheduler.track_work()
    }

    pub fn abort(&self) -> usize {
        self.scheduler.abort()
    }

    pub async fn wait(&self) {
        self.scheduler.wait_idle().await;
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn contains_alias_token(haystack: &str, alias: &str) -> bool {
    let bytes = haystack.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(alias) {
        let pos = start + rel;
        let end = pos + alias.len();
        let left_ok = pos == 0 || !is_ident(bytes[pos - 1]);
        let right_ok = end == bytes.len() || !is_ident(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

fn wants_help(cmd: &Command, args: &[String]) -> bool {
    !cmd.free_text
        && args.len() == 1
        && (args[0] == "-h" || args[0] == "--help")
}

fn split_first_token(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], &s[idx..]),
        None => (s, ""),
    }
}

fn split_args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

fn resolve_expansion(
    guard: &RegistryData,
    prefix: &str,
    expansion: &str,
    raw_tail: &str,
) -> Option<Route> {
    let (exp_head, exp_tail) = split_first_token(expansion.trim_start());
    let cmd = guard
        .by_name
        .get(exp_head)
        .or_else(|| guard.builtin_aliases.get(exp_head))?;
    let exp_tail = exp_tail.trim_start_matches(char::is_whitespace);
    let combined_raw = match (exp_tail.is_empty(), raw_tail.is_empty()) {
        (true, true) => String::new(),
        (false, true) => exp_tail.to_owned(),
        (true, false) => raw_tail.to_owned(),
        (false, false) => format!("{exp_tail} {raw_tail}"),
    };
    let args = split_args(&combined_raw);
    Some(Route {
        prefix: prefix.to_owned(),
        command: cmd.name.clone(),
        args,
        raw: combined_raw,
    })
}

fn match_multiword_alias<'a>(rest: &'a str, trigger: &str) -> Option<&'a str> {
    let mut cursor = rest;
    let trigger_words: Vec<&str> = trigger.split_whitespace().collect();
    if trigger_words.len() < 2 {
        return None;
    }
    for (idx, expected) in trigger_words.iter().enumerate() {
        let (tok, next) = split_first_token(cursor);
        if tok != *expected {
            return None;
        }
        if idx + 1 == trigger_words.len() {
            return Some(next.trim_start_matches(char::is_whitespace));
        }
        if next.is_empty() || !next.starts_with(char::is_whitespace) {
            return None;
        }
        cursor = next.trim_start_matches(char::is_whitespace);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::InvocationError;
    use mibot_bot::{ChatType, PeerCache, RpcTransport, SentRecorder};
    use std::collections::HashMap;
    use tokio::sync::Mutex as AsyncMutex;

    struct RecordingTransport {
        sent_bodies: Arc<AsyncMutex<Vec<String>>>,
    }

    impl RpcTransport for RecordingTransport {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            _constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                use grammers_tl_types::Serializable;
                self.sent_bodies
                    .lock()
                    .await
                    .push(String::from_utf8_lossy(&body).into_owned());
                let resp = tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                    out: true,
                    id: 99,
                    pts: 1,
                    pts_count: 1,
                    date: 100,
                    media: None,
                    entities: None,
                    ttl_period: None,
                });
                Ok(resp.to_bytes())
            })
        }
    }

    fn mk_client() -> (Arc<Client>, Arc<AsyncMutex<Vec<String>>>) {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(10);
        let sent = Arc::new(SentRecorder::default());
        let sent_bodies = Arc::new(AsyncMutex::new(Vec::new()));
        let transport: Arc<dyn RpcTransport> = Arc::new(RecordingTransport {
            sent_bodies: Arc::clone(&sent_bodies),
        });
        (
            Arc::new(Client::from_transport(transport, peers, sent)),
            sent_bodies,
        )
    }

    fn mk_msg(id: i32, text: &str, out: bool) -> Message {
        let peer: tl::enums::Peer = tl::types::PeerUser { user_id: 10 }.into();
        let raw = tl::types::Message {
            out,
            mentioned: false,
            media_unread: false,
            silent: false,
            post: false,
            from_scheduled: false,
            legacy: false,
            edit_hide: false,
            pinned: false,
            noforwards: false,
            invert_media: false,
            offline: false,
            video_processing_pending: false,
            paid_suggested_post_stars: false,
            paid_suggested_post_ton: false,
            id,
            from_id: Some(peer.clone()),
            from_boosts_applied: None,
            from_rank: None,
            peer_id: peer.clone(),
            saved_peer_id: None,
            fwd_from: None,
            via_bot_id: None,
            via_business_bot_id: None,
            guestchat_via_from: None,
            reply_to: None,
            date: 1_800_000_000,
            message: text.into(),
            media: None,
            reply_markup: None,
            entities: None,
            views: None,
            forwards: None,
            replies: None,
            edit_date: None,
            post_author: None,
            grouped_id: None,
            reactions: None,
            restriction_reason: None,
            ttl_period: None,
            quick_reply_shortcut_id: None,
            effect: None,
            factcheck: None,
            report_delivery_until_date: None,
            paid_message_stars: None,
            suggested_post: None,
            schedule_repeat_period: None,
            summary_from_language: None,
            rich_message: None,
        };
        Message {
            id,
            peer: peer.clone(),
            chat_id: "10".into(),
            chat_type: ChatType::Private,
            text: text.into(),
            out,
            edited: false,
            forward: false,
            post: false,
            saved: true,
            sender: peer,
            reply_to_id: 0,
            reply_to_peer: None,
            topic_id: 0,
            quote_text: String::new(),
            quote_entities: Vec::new(),
            raw,
        }
    }

    #[test]
    fn parse_precedence_and_raw_after_preserve_newlines() {
        let reg = Registry::new();
        reg.set_prefixes(vec![".".into(), "..".into()]);
        reg.register(Command {
            name: "echo".into(),
            aliases: vec!["e".into()],
            group: group::GENERAL.into(),
            description: "echo".into(),
            usage: "".into(),
            help: "".into(),
            handle: Arc::new(|_| Box::pin(async { Ok(()) })),
            free_text: true,
            ..Default::default()
        });
        reg.register(Command {
            name: "ping".into(),
            aliases: vec![],
            group: group::GENERAL.into(),
            description: "ping".into(),
            usage: "".into(),
            help: "".into(),
            handle: Arc::new(|_| Box::pin(async { Ok(()) })),
            ..Default::default()
        });

        let mut aliases = HashMap::new();
        // 1-word alias colliding with built-in "e" MUST lose to built-in "e".
        aliases.insert("e".into(), "ping".into());
        // Multi-word alias beats single-word command.
        aliases.insert("ping fast".into(), "echo v2".into());
        reg.set_aliases(&aliases);

        let r1 = reg.parse("..ping   one\n  two   three").unwrap();
        assert_eq!(r1.command, "ping");
        assert_eq!(r1.args, vec!["one", "two", "three"]);
        assert_eq!(r1.raw, "one\n  two   three");

        let r2 = reg.parse(".e hi").unwrap();
        assert_eq!(r2.command, "echo");

        let r3 = reg.parse(".ping fast x\ny").unwrap();
        assert_eq!(r3.command, "echo");
        assert_eq!(r3.args, vec!["v2", "x", "y"]);

        let (client, _) = mk_client();
        let inv = Invocation {
            client,
            registry: Arc::new(reg),
            message: mk_msg(1, ".ping", true),
            prefix: r1.prefix,
            command: "ping".into(),
            args: r1.args,
            raw: r1.raw,
            cancel: CancellationToken::new(),
            foreign: false,
        };
        assert_eq!(inv.raw_after(0), "one\n  two   three");
        assert_eq!(inv.raw_after(1), "two   three");
        assert_eq!(inv.raw_after(2), "three");
        assert_eq!(inv.raw_after(5), "");
    }

    #[tokio::test]
    async fn dispatch_dedups_relay_handles_help_and_redacts_errors() {
        let reg = Arc::new(Registry::new());
        let (client, sent_bodies) = mk_client();
        reg.bind_client(client);

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);
        reg.register(Command {
            name: "failer".into(),
            aliases: vec!["f".into()],
            group: group::TOOLS.into(),
            description: "fails".into(),
            usage: ".failer".into(),
            help: "".into(),
            handle: Arc::new(move |_| {
                let c = Arc::clone(&calls_ref);
                Box::pin(async move {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err(Box::new(std::io::Error::other("/secret/path leaked")) as CmdError)
                })
            }),
            timeout: Some(Duration::from_secs(2)),
            ..Default::default()
        });

        let msg = mk_msg(10, ".f", true);
        assert_eq!(
            reg.dispatch_for(msg.clone(), false, CancellationToken::new()),
            DispatchOutcome::Queued
        );
        // Same (peer, id) relayed again within 10m is deduped.
        assert_eq!(
            reg.dispatch_for(msg, true, CancellationToken::new()),
            DispatchOutcome::NotCommand
        );
        reg.wait().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Help output runs without invoking command handle and appends unmentioned alias.
        let help_msg = mk_msg(11, ".f --help", true);
        assert_eq!(
            reg.dispatch_for(help_msg, false, CancellationToken::new()),
            DispatchOutcome::Queued
        );
        reg.wait().await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let cmd = reg.command("failer").unwrap();
        assert!(reg.help_text(&cmd).contains("Aliases: <code>f</code>"));
        let bodies = sent_bodies.lock().await;
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains("Internal error, see log for details"));
        assert!(!bodies[0].contains("/secret/path"));
        assert!(bodies[1].contains("Aliases: f"));
    }
}
