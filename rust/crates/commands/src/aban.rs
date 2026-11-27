use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BoxFuture, Client, DialogPages, peer_from_id, peer_id};
use mibot_command::{CmdError, CmdResult, Command, Invocation, brief, code, escape, group};
use mibot_logtail::Level;
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

const ABAN_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const ABAN_PARALLEL: usize = 4;
const RESULT_LIFETIME: Duration = Duration::from_secs(10);
const BATCH_LIFETIME: Duration = Duration::from_secs(30);
const HISTORY_ROUNDS: usize = 100;
const RECENT_PAGES: usize = 5;
const RECENT_PAGE_SIZE: i32 = 200;
const RESOLVE_PARALLEL: usize = 6;

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct ManagedGroup {
    id: i64,
    title: String,
    channel: bool,
    #[serde(rename = "accessHash", skip_serializing_if = "is_zero_i64")]
    hash: i64,
}

impl ManagedGroup {
    fn input(&self) -> tl::enums::InputPeer {
        if self.channel {
            tl::types::InputPeerChannel {
                channel_id: self.id,
                access_hash: self.hash,
            }
            .into()
        } else {
            tl::types::InputPeerChat { chat_id: self.id }.into()
        }
    }

    fn matches_peer(&self, peer: &tl::enums::InputPeer) -> bool {
        match peer {
            tl::enums::InputPeer::Channel(c) => self.channel && self.id == c.channel_id,
            tl::enums::InputPeer::Chat(c) => !self.channel && self.id == c.chat_id,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct AbanCache {
    groups: Vec<ManagedGroup>,
    #[serde(rename = "updatedAt")]
    updated_at: i64,
}

type LaterTask = Box<dyn FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static>;
type LaterFn = Arc<dyn Fn(Duration, LaterTask) -> bool + Send + Sync>;

#[derive(Debug, Clone)]
struct AbanTarget {
    peer: tl::enums::InputPeer,
    id: i64,
    channel: bool,
    display: String,
}

#[derive(Debug)]
struct UnresolvedUserError;

impl std::fmt::Display for UnresolvedUserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("aban: user is not resolvable")
    }
}

impl std::error::Error for UnresolvedUserError {}

fn is_unresolved_user(err: &(dyn std::error::Error + 'static)) -> bool {
    mibot_command::errors::find_in_chain::<UnresolvedUserError>(err).is_some()
}

struct AbanService {
    store: Store<AbanCache>,
    logs: Option<Arc<mibot_logtail::Ring>>,
    mu: Mutex<()>,
    later: LaterFn,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_unix_i32() -> i32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i32)
        .unwrap_or(0)
}

impl AbanService {
    fn fresh(&self) -> bool {
        let Ok(cached) = self.store.read() else {
            return false;
        };
        if cached.groups.is_empty() {
            return false;
        }
        let age_ms = now_ms().saturating_sub(cached.updated_at);
        age_ms >= 0 && age_ms < ABAN_CACHE_TTL.as_millis() as i64
    }

    async fn managed_groups(&self, client: &Client, refresh: bool) -> Result<Vec<ManagedGroup>, CmdError> {
        let _guard = self.mu.lock().await;
        if !refresh && let Ok(cached) = self.store.read() {
            let age_ms = now_ms().saturating_sub(cached.updated_at);
            if !cached.groups.is_empty() && age_ms >= 0 && age_ms < ABAN_CACHE_TTL.as_millis() as i64 {
                return Ok(cached.groups);
            }
        }
        let mut groups = Vec::new();
        let mut seen = HashSet::new();
        for folder in [0, 1] {
            let peers = client.peers();
            client
                .each_dialog(
                    DialogPages {
                        folder,
                        max_pages: 20,
                    },
                    |dialog| match &dialog.peer {
                        tl::enums::Peer::Channel(ch) => {
                            let Some(info) = peers.channel(ch.channel_id) else {
                                return;
                            };
                            if seen.contains(&ch.channel_id) || info.left {
                                return;
                            }
                            let can_ban = info.admin_rights.as_ref().is_some_and(|r| r.ban_users || r.delete_messages);
                            if !info.creator && !can_ban {
                                return;
                            }
                            seen.insert(ch.channel_id);
                            groups.push(ManagedGroup {
                                id: ch.channel_id,
                                title: info.title,
                                channel: true,
                                hash: info.hash,
                            });
                        }
                        tl::enums::Peer::Chat(c) => {
                            let Some(info) = peers.chat(c.chat_id) else {
                                return;
                            };
                            let can_ban = info.admin_rights.as_ref().is_some_and(|r| r.ban_users);
                            if seen.contains(&c.chat_id) || info.left || (!info.creator && !can_ban) {
                                return;
                            }
                            seen.insert(c.chat_id);
                            groups.push(ManagedGroup {
                                id: c.chat_id,
                                title: info.title,
                                channel: false,
                                hash: 0,
                            });
                        }
                        _ => {}
                    },
                )
                .await?;
        }
        let snapshot = groups.clone();
        if let Err(err) = kit::update_ok(&self.store, move |cache| {
            cache.groups = snapshot;
            cache.updated_at = now_ms();
        }) {
            kit::opt_log(self.logs.as_ref(), Level::Warn, "aban.cache_write_failed", &[("error", &err.to_string())]);
        }
        Ok(groups)
    }

    async fn show(&self, inv: &Invocation, html: &str, lifetime: Duration) -> CmdResult {
        inv.edit(html).await?;
        if !lifetime.is_zero() {
            self.delete_later(inv, lifetime);
        }
        Ok(())
    }

    fn delete_later(&self, inv: &Invocation, lifetime: Duration) {
        let client = Arc::clone(&inv.client);
        let msg = inv.message.clone();
        let chat_id = msg.chat_id.clone();
        let msg_id = msg.id.to_string();
        let logs = self.logs.clone();
        let task: LaterTask = Box::new(move |_tok| {
            Box::pin(async move {
                let res = tokio::time::timeout(Duration::from_secs(15), client.delete_message(&msg)).await;
                if let Ok(Err(err)) = res
                    && !err.is_rpc("MESSAGE_ID_INVALID")
                {
                    kit::opt_log(
                        logs.as_ref(),
                        Level::Warn,
                        "aban.auto_delete",
                        &[("chat", &chat_id), ("message", &msg_id), ("error", &err.to_string())],
                    );
                }
            })
        });
        (self.later)(lifetime, task);
    }

    async fn fail(&self, inv: &Invocation, err: CmdError) -> CmdResult {
        self.show_failure(inv, kit::tr("Operation failed", "操作失败"), err)
            .await
    }

    async fn show_failure(&self, inv: &Invocation, label: &str, err: CmdError) -> CmdResult {
        if inv.cancel.is_cancelled() {
            return Err(err);
        }
        if !kit::is_user_error(err.as_ref()) {
            kit::opt_log(
                self.logs.as_ref(),
                Level::Warn,
                "aban.failed",
                &[("action", label), ("error", &err.to_string())],
            );
        }
        let wrapped = kit::fail_with(label, mibot_command::UserError::new(brief(err.as_ref())));
        let msg = brief(wrapped.as_ref());
        self.show(inv, &format!("❌ {}", escape(&msg)), RESULT_LIFETIME).await
    }

    async fn fail_target(&self, inv: &Invocation, err: CmdError, unresolved: &str) -> CmdResult {
        if is_unresolved_user(err.as_ref()) {
            return self.fail(inv, kit::fail(unresolved)).await;
        }
        self.fail(inv, err).await
    }

    async fn resolve_target(
        &self,
        inv: &Invocation,
        here: &tl::enums::InputPeer,
    ) -> Result<AbanTarget, CmdError> {
        let args = target_args(&inv.args);
        if !args.is_empty() {
            return self.resolve_arg(&inv.cancel, &inv.client, here, &args[0]).await;
        }
        let reply = kit::reply_msg(inv).await?;
        let Some(reply) = reply else {
            return Err(kit::fail(kit::tr(
                "Reply to a message, or specify @username or user ID",
                "请回复一条消息，或者指定 @用户名、用户 ID",
            )));
        };
        let sender = reply.sender;
        match sender {
            tl::enums::Peer::User(u) => {
                let Some(peer) = lookup_user(&inv.client, here, u.user_id).await else {
                    return Err(Box::new(UnresolvedUserError));
                };
                user_target(&inv.client, peer, u.user_id)
            }
            tl::enums::Peer::Channel(c) => channel_target(&inv.client, c.channel_id).await,
            _ => Err(kit::fail(kit::tr(
                "Unable to identify the sender of that message",
                "无法识别该消息的发送者",
            ))),
        }
    }

    async fn resolve_arg(
        &self,
        cancel: &CancellationToken,
        client: &Arc<Client>,
        here: &tl::enums::InputPeer,
        arg: &str,
    ) -> Result<AbanTarget, CmdError> {
        if !kit::is_numeric_id(arg) {
            let clean = arg.trim_start_matches('@');
            let Ok(peer) = client.resolve_username(clean).await else {
                return Err(kit::fail(kit::tr(
                    "Unable to resolve target; reply to a message or verify username/ID",
                    "无法解析该目标，可先回复其一则消息，或确认用户名和 ID 正确",
                )));
            };
            return match &peer {
                tl::enums::InputPeer::User(u) => {
                    let uid = u.user_id;
                    user_target(client, peer, uid)
                }
                tl::enums::InputPeer::Channel(c) => channel_target(client, c.channel_id).await,
                tl::enums::InputPeer::PeerSelf => user_target(client, peer, client.self_id()),
                _ => Err(kit::fail(kit::tr(
                    "Target must be a user or channel",
                    "目标必须是用户或频道",
                ))),
            };
        }
        let Some(parsed) = peer_from_id(arg) else {
            return Err(kit::fail(kit::tr("Failed to parse target user ID", "获取用户失败")));
        };
        match parsed {
            tl::enums::Peer::User(u) => {
                let peer = self.find_user(cancel, client, here, u.user_id).await?;
                user_target(client, peer, u.user_id)
            }
            tl::enums::Peer::Channel(c) => channel_target(client, c.channel_id).await,
            _ => Err(kit::fail(kit::tr(
                "Target must be a user or channel",
                "目标必须是用户或频道",
            ))),
        }
    }

    async fn find_user(
        &self,
        cancel: &CancellationToken,
        client: &Arc<Client>,
        here: &tl::enums::InputPeer,
        id: i64,
    ) -> Result<tl::enums::InputPeer, CmdError> {
        if let Some(peer) = lookup_user(client, here, id).await {
            return Ok(peer);
        }
        if let Some(peer) = self.across_groups(cancel, client, here, id).await {
            return Ok(peer);
        }
        if cancel.is_cancelled() {
            return Err(kit::fail(kit::tr("Operation cancelled", "操作已取消")));
        }
        if kit::input_channel(here).is_some() {
            return Ok(tl::types::InputPeerUser {
                user_id: id,
                access_hash: 0,
            }
            .into());
        }
        Err(Box::new(UnresolvedUserError))
    }

    async fn across_groups(
        &self,
        cancel: &CancellationToken,
        client: &Arc<Client>,
        here: &tl::enums::InputPeer,
        id: i64,
    ) -> Option<tl::enums::InputPeer> {
        let Ok(groups) = self.managed_groups(client, false).await else {
            return None;
        };
        if groups.is_empty() {
            return None;
        }
        let mut ordered = Vec::with_capacity(groups.len());
        for channels in [true, false] {
            for g in &groups {
                if g.channel == channels && !g.matches_peer(here) {
                    ordered.push(g.clone());
                }
            }
        }
        let search = cancel.child_token();
        let found: Arc<StdMutex<Option<tl::enums::InputPeer>>> = Arc::new(StdMutex::new(None));
        let found_ref = Arc::clone(&found);
        let search_ref = search.clone();
        let client_ref = Arc::clone(client);
        each_group_limit(&search, &ordered, RESOLVE_PARALLEL, move |group| {
            let found_slot = Arc::clone(&found_ref);
            let stop_tok = search_ref.clone();
            let c = Arc::clone(&client_ref);
            Box::pin(async move {
                let res = if group.channel {
                    member_of_channel(
                        &c,
                        &tl::types::InputChannel {
                            channel_id: group.id,
                            access_hash: group.hash,
                        },
                        id,
                        false,
                    )
                    .await
                } else {
                    member_of_chat(&c, group.id, id).await
                };
                if let Some(peer) = res {
                    let mut g = found_slot.lock().unwrap_or_else(|e| e.into_inner());
                    if g.is_none() {
                        *g = Some(peer);
                        stop_tok.cancel();
                    }
                }
            })
        })
        .await;
        found.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    async fn basic(&self, inv: &Invocation, action: &str, _label: &str) -> CmdResult {
        let (progress_label, fail_label) = action_labels(action);
        let duration = if action == "mute" {
            match mute_duration(&target_args(&inv.args)) {
                Ok(d) => d,
                Err(err) => return self.fail(inv, err).await,
            }
        } else {
            Duration::ZERO
        };
        let chat = inv.client.input_peer(&inv.message.peer)?;
        let who = match self.resolve_target(inv, &chat).await {
            Ok(w) => w,
            Err(err) => {
                return self
                    .fail_target(
                        inv,
                        err,
                        kit::tr(
                            "User ID not found: never seen and not in any managed group. Reply to a message from them or verify the ID",
                            "找不到这个用户 ID：账号没见过对方，对方也不在管理群组里。可以先回复对方的一条消息，或者确认 ID 没写错",
                        ),
                    )
                    .await;
            }
        };
        if !confirmed(&inv.args) && target_is_admin(&inv.client, &chat, &who.peer).await {
            let warn_msg = if kit::is_zh() {
                format!("⚠️ 目标是管理员，请在命令后加上 {} 确认执行", code("true"))
            } else {
                format!("⚠️ Target is an admin; append {} to confirm", code("true"))
            };
            return self.show(inv, &warn_msg, RESULT_LIFETIME).await;
        }
        inv.edit(&format!("⏳ {} {}…", escape(progress_label), escape(&who.display)))
            .await?;

        let is_channel = kit::input_channel(&chat).is_some();
        let result = match action {
            "kick" => {
                if let Err(err) = apply_rights(
                    &inv.client,
                    &chat,
                    &who.peer,
                    kit::ban_rights(until_date(Duration::from_secs(60))),
                    true,
                )
                .await
                {
                    return self.action_failed(inv, fail_label, err).await;
                }
                if is_channel
                    && let Err(err) =
                        apply_rights(&inv.client, &chat, &who.peer, kit::default_banned_rights(), false).await
                {
                    return self.action_failed(inv, fail_label, err).await;
                }
                if kit::is_zh() {
                    format!("✅ 已踢出 {}", escape(&who.display))
                } else {
                    format!("✅ Kicked {}", escape(&who.display))
                }
            }
            "ban" => {
                let cleaned = delete_history(&inv.client, &chat, &who.peer).await;
                if let Err(err) =
                    apply_rights(&inv.client, &chat, &who.peer, kit::ban_rights(0), true).await
                {
                    return self.action_failed(inv, fail_label, err).await;
                }
                let suffix = if cleaned {
                    kit::tr(" (messages cleaned)", " (已清理消息)")
                } else {
                    ""
                };
                if is_channel {
                    if kit::is_zh() {
                        format!("✅ 已封禁 {}{}", escape(&who.display), suffix)
                    } else {
                        format!("✅ Banned {}{}", escape(&who.display), suffix)
                    }
                } else if kit::is_zh() {
                    format!("✅ 已移出 {}{}", escape(&who.display), suffix)
                } else {
                    format!("✅ Removed {}{}", escape(&who.display), suffix)
                }
            }
            "unban" | "unmute" => {
                if let Err(err) =
                    apply_rights(&inv.client, &chat, &who.peer, kit::default_banned_rights(), false).await
                {
                    return self.action_failed(inv, fail_label, err).await;
                }
                if action == "unban" {
                    if kit::is_zh() {
                        format!("✅ 已解封 {}", escape(&who.display))
                    } else {
                        format!("✅ Unbanned {}", escape(&who.display))
                    }
                } else if kit::is_zh() {
                    format!("✅ 已解除禁言 {}", escape(&who.display))
                } else {
                    format!("✅ Unmuted {}", escape(&who.display))
                }
            }
            "mute" => {
                if let Err(err) = apply_rights(
                    &inv.client,
                    &chat,
                    &who.peer,
                    kit::mute_rights(until_date(duration)),
                    false,
                )
                .await
                {
                    return self.action_failed(inv, fail_label, err).await;
                }
                if kit::is_zh() {
                    format!("✅ 已禁言 {} {}", escape(&who.display), format_duration(duration))
                } else {
                    format!("✅ Muted {} {}", escape(&who.display), format_duration(duration))
                }
            }
            _ => String::new(),
        };
        kit::opt_log(
            self.logs.as_ref(),
            Level::Info,
            "aban.applied",
            &[
                ("action", action),
                ("target", &who.id.to_string()),
                ("chat", &inv.message.chat_id),
            ],
        );
        self.show(inv, &result, RESULT_LIFETIME).await
    }

    async fn action_failed(&self, inv: &Invocation, label: &str, err: CmdError) -> CmdResult {
        let msg = if kit::is_zh() {
            format!("{label}失败")
        } else {
            format!("{label} failed")
        };
        self.show_failure(inv, &msg, err).await
    }

    async fn batch(&self, inv: &Invocation, ban: bool) -> CmdResult {
        let label_zh = if ban { "封禁" } else { "解封" };
        let here = inv.client.input_peer(&inv.message.peer)?;
        let who = match self.resolve_target(inv, &here).await {
            Ok(w) => w,
            Err(err) => {
                let unresolved = if kit::is_zh() {
                    format!(
                        "找不到这个用户 ID：账号没见过对方，对方也不在任何一个管理群组里。可以先 {}refresh 再试，或者回复对方的一条消息",
                        inv.prefix()
                    )
                } else {
                    format!(
                        "User ID not found: never seen and not in any managed group. Try {}refresh first or reply to a message from them",
                        inv.prefix()
                    )
                };
                return self.fail_target(inv, err, &unresolved).await;
            }
        };
        if !self.fresh() {
            inv.edit_text(&kit::working(kit::tr(
                "Loading managed groups (once per day)",
                "正在读取管理群组列表（每天一次）",
            )))
            .await?;
        }
        let groups = match self.managed_groups(&inv.client, false).await {
            Ok(g) => g,
            Err(err) => return self.fail(inv, err).await,
        };
        if groups.is_empty() {
            return self
                .show(
                    inv,
                    kit::tr("❌ No managed groups found", "❌ 无管理群组"),
                    RESULT_LIFETIME,
                )
                .await;
        }
        if !confirmed(&inv.args) && !who.channel {
            let admin_in = Arc::new(AtomicI32::new(0));
            let admin_in_ref = Arc::clone(&admin_in);
            let client = Arc::clone(&inv.client);
            let target_peer = who.peer.clone();
            each_group(&inv.cancel, &groups, move |group| {
                let c = Arc::clone(&client);
                let p = target_peer.clone();
                let count = Arc::clone(&admin_in_ref);
                Box::pin(async move {
                    if target_is_admin(&c, &group.input(), &p).await {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .await;
            if inv.cancel.is_cancelled() {
                return Err(kit::fail(kit::tr("Operation cancelled", "操作已取消")));
            }
            let count = admin_in.load(Ordering::Relaxed);
            if count > 0 {
                let warn_msg = if kit::is_zh() {
                    format!(
                        "⚠️ 目标在 {count} 个管理群中具有管理员身份，请在命令后加上 {} 确认执行",
                        code("true")
                    )
                } else {
                    format!(
                        "⚠️ Target is an admin in {count} managed group(s); append {} to confirm",
                        code("true")
                    )
                };
                return self.show(inv, &warn_msg, RESULT_LIFETIME).await;
            }
        }
        let working_msg = if kit::is_zh() {
            format!("正在 {} 个群组和频道里{label_zh}这个用户", groups.len())
        } else {
            format!(
                "{} user across {} group(s) and channel(s)",
                if ban { "Banning" } else { "Unbanning" },
                groups.len()
            )
        };
        inv.edit_text(&kit::working(&working_msg)).await?;

        let started = std::time::Instant::now();
        let cleaned = if ban {
            delete_history(&inv.client, &here, &who.peer).await
        } else {
            false
        };
        let rights = if ban {
            kit::ban_rights(0)
        } else {
            kit::default_banned_rights()
        };

        #[derive(Default)]
        struct Tally {
            success: usize,
            failed: usize,
            skipped: usize,
            reasons: HashMap<String, usize>,
        }
        let tally = Arc::new(StdMutex::new(Tally::default()));
        let tally_ref = Arc::clone(&tally);
        let client = Arc::clone(&inv.client);
        let target_peer = who.peer.clone();
        each_group(&inv.cancel, &groups, move |group| {
            let tally = Arc::clone(&tally_ref);
            let c = Arc::clone(&client);
            let p = target_peer.clone();
            let r = rights.clone();
            Box::pin(async move {
                if !group.channel && !ban {
                    tally.lock().unwrap_or_else(|e| e.into_inner()).skipped += 1;
                    return;
                }
                let res = apply_rights(&c, &group.input(), &p, r, ban).await;
                let mut t = tally.lock().unwrap_or_else(|e| e.into_inner());
                match res {
                    Ok(()) => t.success += 1,
                    Err(err) => {
                        t.failed += 1;
                        *t.reasons.entry(brief(err.as_ref())).or_insert(0) += 1;
                    }
                }
            })
        })
        .await;
        if inv.cancel.is_cancelled() {
            return Err(kit::fail(kit::tr("Operation cancelled", "操作已取消")));
        }

        let (text, ok_count, fail_count) = {
            let t = tally.lock().unwrap_or_else(|e| e.into_inner());
            let mut text = if kit::is_zh() {
                format!(
                    "✅ 已在 {} 个群组和频道里{label_zh} {}",
                    t.success,
                    escape(&who.display)
                )
            } else {
                format!(
                    "✅ {} {} across {} group(s) and channel(s)",
                    if ban { "Banned" } else { "Unbanned" },
                    escape(&who.display),
                    t.success
                )
            };
            if t.failed > 0 {
                if kit::is_zh() {
                    text.push_str(&format!(
                        "\n⚠️ 失败 {} 个（{}）",
                        t.failed,
                        top_reasons(&t.reasons, 3).join("、")
                    ));
                } else {
                    text.push_str(&format!(
                        "\n⚠️ Failed in {} ({})",
                        t.failed,
                        top_reasons(&t.reasons, 3).join(", ")
                    ));
                }
            }
            if t.skipped > 0 {
                if kit::is_zh() {
                    text.push_str(&format!(
                        "\n⚠️ 跳过 {} 个基本群组（不支持跨群组解封）",
                        t.skipped
                    ));
                } else {
                    text.push_str(&format!(
                        "\n⚠️ Skipped {} basic group(s) (global unban not supported)",
                        t.skipped
                    ));
                }
            }
            if ban {
                if kit::is_zh() {
                    let mark = if cleaned { "已清理" } else { "未清理" };
                    text.push_str(&format!("\n当前群组的消息：{mark}"));
                } else {
                    let mark = if cleaned { "cleaned" } else { "not cleaned" };
                    text.push_str(&format!("\nMessages in current group: {mark}"));
                }
            }
            if kit::is_zh() {
                text.push_str(&format!("\n用时 {:.1} 秒", started.elapsed().as_secs_f64()));
            } else {
                text.push_str(&format!("\nElapsed: {:.1}s", started.elapsed().as_secs_f64()));
            }
            (text, t.success, t.failed)
        };
        kit::opt_log(
            self.logs.as_ref(),
            Level::Info,
            "aban.batch",
            &[
                ("ban", if ban { "true" } else { "false" }),
                ("target", &who.id.to_string()),
                ("success", &ok_count.to_string()),
                ("failed", &fail_count.to_string()),
            ],
        );
        self.show(inv, &text, BATCH_LIFETIME).await
    }
}

fn action_labels(action: &str) -> (&'static str, &'static str) {
    match action {
        "kick" => (kit::tr("Kicking", "踢出"), kit::tr("Kick", "踢出")),
        "ban" => (kit::tr("Banning", "封禁"), kit::tr("Ban", "封禁")),
        "unban" => (kit::tr("Unbanning", "解封"), kit::tr("Unban", "解封")),
        "mute" => (kit::tr("Muting", "禁言"), kit::tr("Mute", "禁言")),
        "unmute" => (kit::tr("Unmuting", "解除禁言"), kit::tr("Unmute", "解除禁言")),
        _ => ("", ""),
    }
}

async fn each_group<F>(cancel: &CancellationToken, groups: &[ManagedGroup], work: F)
where
    F: Fn(ManagedGroup) -> BoxFuture<'static, ()> + Send + Sync + 'static,
{
    each_group_limit(cancel, groups, ABAN_PARALLEL, work).await;
}

async fn each_group_limit<F>(
    cancel: &CancellationToken,
    groups: &[ManagedGroup],
    limit: usize,
    work: F,
) where
    F: Fn(ManagedGroup) -> BoxFuture<'static, ()> + Send + Sync + 'static,
{
    let sem = Arc::new(Semaphore::new(limit.max(1)));
    let work = Arc::new(work);
    let mut handles = Vec::new();
    for group in groups {
        if cancel.is_cancelled() {
            break;
        }
        let permit = tokio::select! {
            _ = cancel.cancelled() => break,
            p = Arc::clone(&sem).acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => break,
            }
        };
        if cancel.is_cancelled() {
            drop(permit);
            break;
        }
        let g = group.clone();
        let w = Arc::clone(&work);
        handles.push(tokio::spawn(async move {
            w(g).await;
            drop(permit);
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

fn parse_duration(value: &str) -> Duration {
    let lower = value.to_ascii_lowercase();
    let unit_secs: u64 = if lower.contains('d') {
        24 * 60 * 60
    } else if lower.contains('h') {
        60 * 60
    } else if lower.contains('m') {
        60
    } else if lower.contains('s') {
        1
    } else {
        return Duration::ZERO;
    };
    let n = leading_int(&lower);
    if n <= 0 {
        return Duration::ZERO;
    }
    let Some(secs) = (n as u64).checked_mul(unit_secs) else {
        return Duration::ZERO;
    };
    if secs > (i64::MAX as u64) / 1_000_000_000 {
        return Duration::ZERO;
    }
    Duration::from_secs(secs)
}

fn leading_int(value: &str) -> i64 {
    let trimmed = value.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (sign, rest) = if let Some(r) = trimmed.strip_prefix('-') {
        (-1i64, r)
    } else if let Some(r) = trimmed.strip_prefix('+') {
        (1i64, r)
    } else {
        (1i64, trimmed)
    };
    let end = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if end == 0 {
        return 0;
    }
    rest[..end]
        .parse::<i64>()
        .map(|n| sign.saturating_mul(n))
        .unwrap_or(0)
}

fn format_duration(d: Duration) -> String {
    if d.is_zero() {
        return kit::tr("forever", "永久").into();
    }
    let secs = d.as_secs();
    if secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn until_date(d: Duration) -> i32 {
    if d.is_zero() {
        0
    } else {
        now_unix_i32().saturating_add(d.as_secs() as i32)
    }
}

fn mute_duration(args: &[String]) -> Result<Duration, CmdError> {
    if args.len() < 2 {
        return Ok(Duration::ZERO);
    }
    match args[1].to_ascii_lowercase().as_str() {
        "0" | "forever" | "permanent" | "永久" => return Ok(Duration::ZERO),
        _ => {}
    }
    let d = parse_duration(&args[1]);
    if d < Duration::from_secs(30) || d > Duration::from_secs(366 * 86400) {
        return Err(kit::fail(kit::tr(
            "Mute duration must be between 30s and 366d (e.g. 60s, 5m, 1d); omit for permanent",
            "禁言时长要在 30 秒到 366 天之间，例如 60s、5m、1d；省略时长表示永久",
        )));
    }
    Ok(d)
}

fn is_meta_flag(arg: &str) -> bool {
    matches!(arg.trim().to_ascii_lowercase().as_str(), "true" | "false" | "confirm")
}

fn confirmed(args: &[String]) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case("true"))
}

fn target_args(args: &[String]) -> Vec<String> {
    args.iter().filter(|a| !is_meta_flag(a)).cloned().collect()
}

async fn target_is_admin(
    client: &Client,
    chat: &tl::enums::InputPeer,
    participant: &tl::enums::InputPeer,
) -> bool {
    let user_id = match participant {
        tl::enums::InputPeer::Channel(_) => return false,
        tl::enums::InputPeer::User(u) => u.user_id,
        tl::enums::InputPeer::PeerSelf => client.self_id(),
        _ => 0,
    };
    if let Some(channel) = kit::input_channel(chat) {
        let res = client
            .invoke(&tl::functions::channels::GetParticipant {
                channel: channel.into(),
                participant: participant.clone(),
            })
            .await;
        return match res {
            Ok(tl::enums::channels::ChannelParticipant::Participant(p)) => {
                client.peers().remember_users(&p.users);
                matches!(
                    p.participant,
                    tl::enums::ChannelParticipant::Admin(_)
                        | tl::enums::ChannelParticipant::Creator(_)
                )
            }
            Err(err) => err.flood_wait().is_some(),
        };
    }
    let tl::enums::InputPeer::Chat(group) = chat else {
        return false;
    };
    if user_id == 0 {
        return false;
    }
    let res = client
        .invoke(&tl::functions::messages::GetFullChat {
            chat_id: group.chat_id,
        })
        .await;
    let Ok(tl::enums::messages::ChatFull::Full(full)) = res else {
        return res.err().is_some_and(|e| e.flood_wait().is_some());
    };
    client.peers().remember_users(&full.users);
    let tl::enums::ChatFull::Full(info) = full.full_chat else {
        return false;
    };
    let tl::enums::ChatParticipants::Participants(members) = info.participants else {
        return false;
    };
    for member in members.participants {
        let (mid, is_admin) = match member {
            tl::enums::ChatParticipant::Creator(c) => (c.user_id, true),
            tl::enums::ChatParticipant::Admin(a) => (a.user_id, true),
            tl::enums::ChatParticipant::Participant(p) => (p.user_id, false),
        };
        if mid == user_id {
            return is_admin;
        }
    }
    false
}

async fn apply_rights(
    client: &Client,
    chat: &tl::enums::InputPeer,
    target: &tl::enums::InputPeer,
    rights: tl::types::ChatBannedRights,
    remove: bool,
) -> CmdResult {
    if let Some(channel) = kit::input_channel(chat) {
        client
            .invoke(&tl::functions::channels::EditBanned {
                channel: channel.into(),
                participant: target.clone(),
                banned_rights: rights.into(),
            })
            .await?;
        return Ok(());
    }
    let tl::enums::InputPeer::Chat(group) = chat else {
        return Err(kit::fail(kit::tr(
            "Unsupported chat type for this action",
            "这个对话不支持这个操作",
        )));
    };
    if !remove {
        return Err(kit::fail(kit::tr(
            "Basic groups do not support mute or unban",
            "基本群不支持禁言与解封",
        )));
    }
    let Some(user) = kit::input_user(target) else {
        return Err(kit::fail(kit::tr(
            "Target must be a user",
            "目标必须是一个用户",
        )));
    };
    client
        .invoke(&tl::functions::messages::DeleteChatUser {
            revoke_history: false,
            chat_id: group.chat_id,
            user_id: user,
        })
        .await?;
    Ok(())
}

async fn delete_history(
    client: &Client,
    chat: &tl::enums::InputPeer,
    target: &tl::enums::InputPeer,
) -> bool {
    let Some(channel) = kit::input_channel(chat) else {
        return false;
    };
    for _ in 0..HISTORY_ROUNDS {
        let Ok(tl::enums::messages::AffectedHistory::History(res)) = client
            .invoke(&tl::functions::channels::DeleteParticipantHistory {
                channel: channel.clone().into(),
                participant: target.clone(),
            })
            .await
        else {
            return false;
        };
        if res.offset <= 0 {
            return true;
        }
    }
    false
}

async fn lookup_user(
    client: &Client,
    here: &tl::enums::InputPeer,
    id: i64,
) -> Option<tl::enums::InputPeer> {
    if let Some(peer) = client.peers().input_peer(&tl::types::PeerUser { user_id: id }.into()) {
        return Some(peer);
    }
    if let Ok(users) = client
        .invoke(&tl::functions::users::GetUsers {
            id: vec![tl::types::InputUser {
                user_id: id,
                access_hash: 0,
            }
            .into()],
        })
        .await
        && let Some(peer) = user_among(client, &users, id)
    {
        return Some(peer);
    }
    match here {
        tl::enums::InputPeer::Channel(c) => {
            member_of_channel(
                client,
                &tl::types::InputChannel {
                    channel_id: c.channel_id,
                    access_hash: c.access_hash,
                },
                id,
                true,
            )
            .await
        }
        tl::enums::InputPeer::Chat(c) => member_of_chat(client, c.chat_id, id).await,
        _ => None,
    }
}

async fn member_of_channel(
    client: &Client,
    channel: &tl::types::InputChannel,
    id: i64,
    scan: bool,
) -> Option<tl::enums::InputPeer> {
    if let Ok(tl::enums::channels::ChannelParticipant::Participant(res)) = client
        .invoke(&tl::functions::channels::GetParticipant {
            channel: channel.clone().into(),
            participant: tl::types::InputPeerUser {
                user_id: id,
                access_hash: 0,
            }
            .into(),
        })
        .await
        && let Some(peer) = user_among(client, &res.users, id)
    {
        return Some(peer);
    }
    if !scan {
        return None;
    }
    let mut offset = 0i32;
    for _ in 0..RECENT_PAGES {
        let Ok(tl::enums::channels::ChannelParticipants::Participants(list)) = client
            .invoke(&tl::functions::channels::GetParticipants {
                channel: channel.clone().into(),
                filter: tl::enums::ChannelParticipantsFilter::ChannelParticipantsRecent,
                offset,
                limit: RECENT_PAGE_SIZE,
                hash: 0,
            })
            .await
        else {
            return None;
        };
        if let Some(peer) = user_among(client, &list.users, id) {
            return Some(peer);
        }
        if list.participants.is_empty() {
            break;
        }
        offset += list.participants.len() as i32;
    }
    None
}

async fn member_of_chat(client: &Client, chat_id: i64, id: i64) -> Option<tl::enums::InputPeer> {
    let Ok(tl::enums::messages::ChatFull::Full(full)) = client
        .invoke(&tl::functions::messages::GetFullChat { chat_id })
        .await
    else {
        return None;
    };
    user_among(client, &full.users, id)
}

fn user_among(client: &Client, users: &[tl::enums::User], id: i64) -> Option<tl::enums::InputPeer> {
    client.peers().remember_users(users);
    for entry in users {
        if let tl::enums::User::User(u) = entry
            && u.id == id
        {
            return client
                .peers()
                .input_peer(&tl::types::PeerUser { user_id: id }.into());
        }
    }
    None
}

fn user_target(
    client: &Client,
    peer: tl::enums::InputPeer,
    id: i64,
) -> Result<AbanTarget, CmdError> {
    if id == client.self_id() {
        return Err(kit::fail(kit::tr(
            "Cannot perform this action on yourself",
            "不能对自己执行此操作",
        )));
    }
    let display = client
        .peers()
        .user(id)
        .map(|u| u.display_name())
        .unwrap_or_else(|| id.to_string());
    Ok(AbanTarget {
        peer,
        id,
        channel: false,
        display,
    })
}

async fn channel_target(client: &Client, id: i64) -> Result<AbanTarget, CmdError> {
    let p = tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: id });
    let mut peer = client.peers().input_peer(&p);
    if peer.is_none()
        && let Ok(res) = client
            .invoke(&tl::functions::channels::GetChannels {
                id: vec![tl::types::InputChannel {
                    channel_id: id,
                    access_hash: 0,
                }
                .into()],
            })
            .await
    {
        let chats = match res {
            tl::enums::messages::Chats::Chats(c) => c.chats,
            tl::enums::messages::Chats::Slice(c) => c.chats,
        };
        client.peers().remember_chats(&chats);
        peer = client.peers().input_peer(&p);
    }
    let Some(peer) = peer else {
        return Err(kit::fail(kit::tr(
            "Unable to resolve channel; reply to a message from it first",
            "无法解析该频道，可先回复其一则消息",
        )));
    };
    let info = client.peers().channel(id).unwrap_or_default();
    let fallback = format!("-100{id}");
    let title = kit::or_default(&info.title, &fallback);
    let mut display = if kit::is_zh() {
        format!("频道：{title}")
    } else {
        format!("Channel: {title}")
    };
    if !info.username.is_empty() {
        display.push_str(&format!(" (@{})", info.username));
    }
    Ok(AbanTarget {
        peer,
        id,
        channel: true,
        display,
    })
}

fn top_reasons(reasons: &HashMap<String, usize>, n: usize) -> Vec<String> {
    let mut keys: Vec<&String> = reasons.keys().collect();
    keys.sort_by(|a, b| reasons[*b].cmp(&reasons[*a]).then_with(|| a.cmp(b)));
    keys.truncate(n);
    keys.into_iter()
        .map(|k| format!("{}×{}", escape(k), reasons[k]))
        .collect()
}

fn aban_help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "🔨 <b>群组管理</b>\n\n• <code>{p}kick</code> 踢出 · <code>{p}ban</code> 封禁并清理消息\n• <code>{p}unban</code> 解封 · <code>{p}unmute</code> 解除禁言\n• <code>{p}mute [目标] [时长]</code> 禁言，时长如 60s、5m、1h、1d，省略为永久\n• <code>{p}sb [目标]</code> 在所有有管理权的群组和频道封禁，并清理当前群组的消息\n• <code>{p}unsb [目标]</code> 批量解封\n• <code>{p}refresh</code> 刷新管理群组列表（平时缓存一天）\n\n目标可以是回复的消息、@用户名、用户 ID，也可以是频道；目标是管理员时要在后面加 <code>true</code>。\n用户 ID 不要求对方在当前群组，会从对话缓存和管理群组里找。\n基本群组只能踢出：ban 和 sb 在基本群组里是移出，对方还能再加入。"
        )
    } else {
        format!(
            "🔨 <b>Group Moderation</b>\n\n• <code>{p}kick</code> Kick · <code>{p}ban</code> Ban and clean messages\n• <code>{p}unban</code> Unban · <code>{p}unmute</code> Unmute\n• <code>{p}mute [target] [duration]</code> Mute (e.g. 60s, 5m, 1h, 1d; omit for permanent)\n• <code>{p}sb [target]</code> Ban across all managed groups/channels and clean messages here\n• <code>{p}unsb [target]</code> Global unban\n• <code>{p}refresh</code> Refresh managed groups cache (cached for 24h)\n\nTarget can be a replied message, @username, user ID, or channel; append <code>true</code> if target is an admin.\nUser IDs are searched across cached dialogs and managed groups.\nBasic groups only support removal: ban/sb remove the user without a permanent block."
        )
    }
}

fn new_service(app: &Arc<App>) -> Arc<AbanService> {
    let reg = Arc::clone(&app.registry);
    let root_cancel = CancellationToken::new();
    let later: LaterFn = Arc::new(move |delay, task| reg.after(root_cancel.clone(), delay, task));
    Arc::new(AbanService {
        store: kit::new_store(app, "aban.json", AbanCache::default),
        logs: Some(Arc::clone(&app.logs)),
        mu: Mutex::new(()),
        later,
    })
}

type SharedServiceCell = OnceLock<StdMutex<Option<(usize, Arc<AbanService>)>>>;
static SHARED_LOCK: SharedServiceCell = OnceLock::new();

fn service_for(app: &Arc<App>) -> Arc<AbanService> {
    let ptr = Arc::as_ptr(app) as usize;
    let cell = SHARED_LOCK.get_or_init(|| StdMutex::new(None));
    let mut g = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((saved_ptr, svc)) = &*g
        && *saved_ptr == ptr
    {
        return Arc::clone(svc);
    }
    let svc = new_service(app);
    *g = Some((ptr, Arc::clone(&svc)));
    svc
}

pub fn register(app: &Arc<App>) {
    let svc = service_for(app);

    let make_basic = |action: &'static str, desc: &'static str, desc_zh: &'static str| {
        let (usage, usage_zh) = if action == "mute" {
            ("[target] [duration]", "[目标] [时长]")
        } else {
            ("[target]", "[目标]")
        };
        let s = Arc::clone(&svc);
        Command {
            name: action.into(),
            group: group::ADMIN.into(),
            description: desc.into(),
            usage: usage.into(),
            help_fn: Some(Arc::new(aban_help)),
            timeout: Some(Duration::from_secs(120)),
            handle: Arc::new(move |inv| {
                let s = Arc::clone(&s);
                Box::pin(async move { s.basic(&inv, action, "").await })
            }),
            ..Default::default()
        }
        .zh(desc_zh, usage_zh)
    };

    let s_help = Arc::clone(&svc);
    app.registry.register(
        Command {
            name: "aban".into(),
            group: group::ADMIN.into(),
            description: "Group moderation help".into(),
            help_fn: Some(Arc::new(aban_help)),
            handle: Arc::new(move |inv| {
                let s = Arc::clone(&s_help);
                Box::pin(async move { s.show(&inv, &aban_help(inv.prefix()), RESULT_LIFETIME).await })
            }),
            ..Default::default()
        }
        .zh("查看群管理命令说明", ""),
    );

    app.registry.register(make_basic("kick", "Kick user from group", "把用户踢出当前群组"));
    app.registry.register(make_basic("ban", "Ban user in current group", "在当前群组封禁用户"));
    app.registry.register(make_basic("unban", "Unban user in current group", "在当前群组解封用户"));
    app.registry.register(make_basic("mute", "Mute user in current group", "在当前群组禁言用户"));
    app.registry.register(make_basic("unmute", "Unmute user in current group", "在当前群组解除禁言"));

    let s_sb = Arc::clone(&svc);
    app.registry.register(
        Command {
            name: "sb".into(),
            group: group::ADMIN.into(),
            description: "Ban user in all managed groups".into(),
            usage: "[target]".into(),
            help_fn: Some(Arc::new(aban_help)),
            timeout: Some(Duration::from_secs(15 * 60)),
            handle: Arc::new(move |inv| {
                let s = Arc::clone(&s_sb);
                Box::pin(async move { s.batch(&inv, true).await })
            }),
            ..Default::default()
        }
        .zh("在所有管理的群组封禁", "[目标]"),
    );

    let s_unsb = Arc::clone(&svc);
    app.registry.register(
        Command {
            name: "unsb".into(),
            group: group::ADMIN.into(),
            description: "Unban in all managed groups".into(),
            usage: "[target]".into(),
            help_fn: Some(Arc::new(aban_help)),
            timeout: Some(Duration::from_secs(15 * 60)),
            handle: Arc::new(move |inv| {
                let s = Arc::clone(&s_unsb);
                Box::pin(async move { s.batch(&inv, false).await })
            }),
            ..Default::default()
        }
        .zh("在所有管理的群组解封", "[目标]"),
    );

    let s_ref = Arc::clone(&svc);
    app.registry.register(
        Command {
            name: "refresh".into(),
            group: group::ADMIN.into(),
            description: "Refresh managed groups cache".into(),
            help_fn: Some(Arc::new(aban_help)),
            timeout: Some(Duration::from_secs(5 * 60)),
            handle: Arc::new(move |inv| {
                let s = Arc::clone(&s_ref);
                Box::pin(async move {
                    inv.edit_text(&kit::working(kit::tr(
                        "Refreshing managed groups",
                        "正在刷新管理群组列表",
                    )))
                    .await?;
                    match s.managed_groups(&inv.client, true).await {
                        Ok(groups) => {
                            let msg = if kit::is_zh() {
                                format!("✅ 已刷新 {} 个有管理权的群组", groups.len())
                            } else {
                                format!("✅ Refreshed {} managed group(s)", groups.len())
                            };
                            s.show(&inv, &msg, RESULT_LIFETIME).await
                        }
                        Err(err) => {
                            if inv.cancel.is_cancelled() {
                                return Err(err);
                            }
                            s.show_failure(&inv, kit::tr("Refresh failed", "刷新失败"), err)
                                .await
                        }
                    }
                })
            }),
            ..Default::default()
        }
        .zh("刷新管理群组列表", ""),
    );

    let _ = peer_id(&tl::enums::Peer::User(tl::types::PeerUser { user_id: 0 }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{PeerCache, RpcTransport, SentRecorder};
    use mibot_command::Registry;
    use std::path::PathBuf;

    const SELF_ID: i64 = 1;
    const HERE_ID: i64 = 10;
    const OTHER_ID: i64 = 20;
    const BASIC_ID: i64 = 30;
    const LAST_ID: i64 = 40;
    const TARGET_ID: i64 = 42;
    const TARGET_HASH: i64 = 4242;
    const ALIAS_ID: i64 = 77;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-aban-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    type AnswerFn = Box<dyn Fn(u32, &[u8]) -> Option<Result<Vec<u8>, InvocationError>> + Send + Sync>;

    struct Scripted {
        calls: StdMutex<Vec<String>>,
        answer: Option<AnswerFn>,
    }

    fn describe(cid: u32, body: &[u8]) -> String {
        if cid == tl::functions::users::GetUsers::CONSTRUCTOR_ID {
            let req = tl::functions::users::GetUsers::from_bytes(&body[4..]).unwrap();
            let ids: Vec<String> = req
                .id
                .iter()
                .filter_map(|u| match u {
                    tl::enums::InputUser::User(x) => Some(format!("{}#{}", x.user_id, x.access_hash)),
                    _ => None,
                })
                .collect();
            return format!("users.getUsers {}", ids.join(" "));
        }
        if cid == tl::functions::channels::GetParticipant::CONSTRUCTOR_ID {
            let req = tl::functions::channels::GetParticipant::from_bytes(&body[4..]).unwrap();
            return format!("channels.getParticipant {} {}", chan_of(&req.channel), peer_name(&req.participant));
        }
        if cid == tl::functions::channels::GetParticipants::CONSTRUCTOR_ID {
            let req = tl::functions::channels::GetParticipants::from_bytes(&body[4..]).unwrap();
            let f = match req.filter {
                tl::enums::ChannelParticipantsFilter::ChannelParticipantsRecent => "channelParticipantsRecent",
                _ => "other",
            };
            return format!(
                "channels.getParticipants {} {f} offset={} limit={}",
                chan_of(&req.channel),
                req.offset,
                req.limit
            );
        }
        if cid == tl::functions::messages::GetFullChat::CONSTRUCTOR_ID {
            let req = tl::functions::messages::GetFullChat::from_bytes(&body[4..]).unwrap();
            return format!("messages.getFullChat {}", req.chat_id);
        }
        if cid == tl::functions::channels::GetChannels::CONSTRUCTOR_ID {
            return "channels.getChannels".into();
        }
        if cid == tl::functions::channels::EditBanned::CONSTRUCTOR_ID {
            let req = tl::functions::channels::EditBanned::from_bytes(&body[4..]).unwrap();
            let tl::enums::ChatBannedRights::Rights(r) = req.banned_rights;
            return format!(
                "channels.editBanned {} {} view={} send={} timed={}",
                chan_of(&req.channel),
                peer_name(&req.participant),
                r.view_messages,
                r.send_messages,
                r.until_date > 0
            );
        }
        if cid == tl::functions::channels::DeleteParticipantHistory::CONSTRUCTOR_ID {
            let req = tl::functions::channels::DeleteParticipantHistory::from_bytes(&body[4..]).unwrap();
            return format!(
                "channels.deleteParticipantHistory {} {}",
                chan_of(&req.channel),
                peer_name(&req.participant)
            );
        }
        if cid == tl::functions::messages::EditMessage::CONSTRUCTOR_ID {
            let req = tl::functions::messages::EditMessage::from_bytes(&body[4..]).unwrap();
            return format!("edit {} {}", req.id, req.message.unwrap_or_default());
        }
        if cid == tl::functions::channels::DeleteMessages::CONSTRUCTOR_ID {
            let req = tl::functions::channels::DeleteMessages::from_bytes(&body[4..]).unwrap();
            return format!("channels.deleteMessages {} {:?}", chan_of(&req.channel), req.id);
        }
        if cid == tl::functions::channels::GetMessages::CONSTRUCTOR_ID {
            let req = tl::functions::channels::GetMessages::from_bytes(&body[4..]).unwrap();
            return format!("channels.getMessages {}", chan_of(&req.channel));
        }
        format!("{cid:#x}")
    }

    fn chan_of(c: &tl::enums::InputChannel) -> i64 {
        match c {
            tl::enums::InputChannel::Channel(x) => x.channel_id,
            _ => 0,
        }
    }

    fn peer_name(p: &tl::enums::InputPeer) -> String {
        match p {
            tl::enums::InputPeer::User(u) => format!("user{}#{}", u.user_id, u.access_hash),
            tl::enums::InputPeer::Channel(c) => format!("channel{}", c.channel_id),
            tl::enums::InputPeer::PeerSelf => "self".into(),
            _ => "other".into(),
        }
    }

    fn default_notify_settings() -> tl::enums::PeerNotifySettings {
        tl::enums::PeerNotifySettings::Settings(tl::types::PeerNotifySettings {
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
        })
    }

    fn full_chat_resp(chat_id: i64, participants: Vec<tl::enums::ChatParticipant>, users: Vec<tl::enums::User>) -> Vec<u8> {
        let resp: tl::enums::messages::ChatFull = tl::types::messages::ChatFull {
            full_chat: tl::enums::ChatFull::Full(tl::types::ChatFull {
                can_set_username: false,
                has_scheduled: false,
                translations_disabled: false,
                has_welcome_messages: false,
                id: chat_id,
                about: String::new(),
                participants: tl::enums::ChatParticipants::Participants(tl::types::ChatParticipants {
                    chat_id,
                    participants,
                    version: 1,
                }),
                chat_photo: None,
                notify_settings: default_notify_settings(),
                exported_invite: None,
                bot_info: None,
                pinned_msg_id: None,
                folder_id: None,
                call: None,
                ttl_period: None,
                groupcall_default_join_as: None,
                theme_emoticon: None,
                requests_pending: None,
                recent_requesters: None,
                available_reactions: None,
                reactions_limit: None,
            }),
            chats: vec![],
            users,
        }
        .into();
        resp.to_bytes()
    }

    fn empty_updates() -> Vec<u8> {
        tl::enums::Updates::Updates(tl::types::Updates {
            updates: vec![],
            users: vec![],
            chats: vec![],
            date: 0,
            seq: 0,
        })
        .to_bytes()
    }

    fn rpc_err(code: i32, name: &str) -> InvocationError {
        InvocationError::Rpc(RpcError {
            code,
            name: name.into(),
            value: None,
            caused_by: None,
        })
    }

    impl RpcTransport for Scripted {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(describe(constructor_id, &body));
                if let Some(ans) = &self.answer
                    && let Some(res) = ans(constructor_id, &body)
                {
                    return res;
                }
                if constructor_id == tl::functions::users::GetUsers::CONSTRUCTOR_ID {
                    return Ok(Vec::<tl::enums::User>::new().to_bytes());
                }
                if constructor_id == tl::functions::channels::GetParticipant::CONSTRUCTOR_ID {
                    return Err(rpc_err(400, "USER_NOT_PARTICIPANT"));
                }
                if constructor_id == tl::functions::channels::GetParticipants::CONSTRUCTOR_ID {
                    let resp: tl::enums::channels::ChannelParticipants =
                        tl::types::channels::ChannelParticipants {
                            count: 0,
                            participants: vec![],
                            chats: vec![],
                            users: vec![],
                        }
                        .into();
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::messages::GetFullChat::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::GetFullChat::from_bytes(&body[4..]).unwrap();
                    return Ok(full_chat_resp(req.chat_id, vec![], vec![]));
                }
                if constructor_id == tl::functions::channels::GetChannels::CONSTRUCTOR_ID {
                    return Err(rpc_err(400, "CHANNEL_INVALID"));
                }
                if constructor_id == tl::functions::channels::EditBanned::CONSTRUCTOR_ID
                    || constructor_id == tl::functions::messages::EditMessage::CONSTRUCTOR_ID
                    || constructor_id == tl::functions::messages::DeleteChatUser::CONSTRUCTOR_ID
                {
                    return Ok(empty_updates());
                }
                if constructor_id == tl::functions::channels::DeleteParticipantHistory::CONSTRUCTOR_ID {
                    let resp: tl::enums::messages::AffectedHistory =
                        tl::types::messages::AffectedHistory {
                            pts: 1,
                            pts_count: 1,
                            offset: 0,
                        }
                        .into();
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::channels::DeleteMessages::CONSTRUCTOR_ID {
                    let resp: tl::enums::messages::AffectedMessages =
                        tl::types::messages::AffectedMessages { pts: 1, pts_count: 1 }.into();
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::channels::GetMessages::CONSTRUCTOR_ID {
                    let resp = kit::tl_messages(vec![], vec![], vec![]);
                    return Ok(resp.to_bytes());
                }
                Err(InvocationError::Dropped("unexpected rpc"))
            })
        }
    }

    fn make_user(id: i64, hash: i64, name: &str) -> tl::enums::User {
        tl::types::User {
            access_hash: Some(hash),
            first_name: Some(name.into()),
            ..kit::blank_tl_user(id)
        }
        .into()
    }

    struct Fixture {
        dir: PathBuf,
        fake: Arc<Scripted>,
        client: Arc<Client>,
        svc: Arc<AbanService>,
        deletions: Arc<StdMutex<Vec<(Duration, LaterTask)>>>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn new_fixture(name: &str, answer: Option<AnswerFn>) -> Fixture {
        let dir = temp_dir(name);
        let fake = Arc::new(Scripted {
            calls: StdMutex::new(vec![]),
            answer,
        });
        let peers = Arc::new(PeerCache::new());
        peers.set_self(SELF_ID);
        peers.remember_chats(&[
            tl::types::Channel {
                access_hash: Some(111),
                ..kit::blank_tl_channel(HERE_ID, "群")
            }
            .into(),
            tl::types::Channel {
                access_hash: Some(222),
                ..kit::blank_tl_channel(OTHER_ID, "二群")
            }
            .into(),
            tl::types::Channel {
                access_hash: Some(444),
                ..kit::blank_tl_channel(LAST_ID, "四群")
            }
            .into(),
            tl::types::Chat {
                creator: false,
                left: false,
                deactivated: false,
                call_active: false,
                call_not_empty: false,
                noforwards: false,
                id: BASIC_ID,
                title: "基本群".into(),
                photo: tl::enums::ChatPhoto::Empty,
                participants_count: 1,
                date: 0,
                version: 1,
                migrated_to: None,
                admin_rights: None,
                default_banned_rights: None,
            }
            .into(),
        ]);
        let client = Arc::new(Client::from_transport(
            fake.clone(),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let store = Store::new(dir.join("aban.json"), AbanCache::default);
        kit::update_ok(&store, |v| {
            v.groups = vec![
                ManagedGroup { id: HERE_ID, title: "群".into(), channel: true, hash: 111 },
                ManagedGroup { id: OTHER_ID, title: "二群".into(), channel: true, hash: 222 },
                ManagedGroup { id: BASIC_ID, title: "基本群".into(), channel: false, hash: 0 },
                ManagedGroup { id: LAST_ID, title: "四群".into(), channel: true, hash: 444 },
            ];
            v.updated_at = now_ms();
        })
        .unwrap();
        let deletions = Arc::new(StdMutex::new(Vec::new()));
        let del_ref = Arc::clone(&deletions);
        let svc = Arc::new(AbanService {
            store,
            logs: None,
            mu: Mutex::new(()),
            later: Arc::new(move |d, task| {
                del_ref.lock().unwrap().push((d, task));
                true
            }),
        });
        Fixture {
            dir,
            fake,
            client,
            svc,
            deletions,
        }
    }

    fn invocation(f: &Fixture, peer: tl::enums::Peer, reply_to: i32, args: &[&str]) -> Invocation {
        let mut message = kit::blank_bot_message(1000, peer);
        message.reply_to_id = reply_to;
        message.out = true;
        Invocation {
            client: Arc::clone(&f.client),
            registry: Arc::new(Registry::new()),
            message,
            prefix: ".".into(),
            command: "ban".into(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            raw: args.join(" "),
            cancel: CancellationToken::new(),
            foreign: false,
        }
    }

    fn here_input() -> tl::enums::InputPeer {
        tl::types::InputPeerChannel {
            channel_id: HERE_ID,
            access_hash: 111,
        }
        .into()
    }

    fn here_peer() -> tl::enums::Peer {
        tl::types::PeerChannel { channel_id: HERE_ID }.into()
    }

    #[test]
    fn test_parse_duration() {
        let cases = [
            ("60s", Duration::from_secs(60)),
            ("5m", Duration::from_secs(5 * 60)),
            ("1h", Duration::from_secs(3600)),
            ("2d", Duration::from_secs(48 * 3600)),
            ("", Duration::ZERO),
            ("forever", Duration::ZERO),
            ("0m", Duration::ZERO),
            ("-3h", Duration::ZERO),
            ("90", Duration::ZERO),
            ("5min", Duration::from_secs(5 * 60)),
            ("1day", Duration::from_secs(24 * 3600)),
            ("2hours", Duration::from_secs(2 * 3600)),
            ("10sec", Duration::from_secs(10)),
            ("1h30m", Duration::from_secs(3600)),
            (" 7D", Duration::from_secs(7 * 86400)),
            ("+5m", Duration::from_secs(5 * 60)),
            ("m5", Duration::ZERO),
            ("99999999999999999999d", Duration::ZERO),
            ("999999999999d", Duration::ZERO),
        ];
        for (input, want) in cases {
            assert_eq!(parse_duration(input), want, "input={input}");
        }
        assert_eq!(format_duration(Duration::ZERO), "forever");
        assert_eq!(format_duration(Duration::from_secs(90 * 60)), "1h");
    }

    #[tokio::test]
    async fn test_find_user_from_cache_and_channel_and_recent_and_basic_and_fallback() {
        // 1. From cache sends nothing
        let f = new_fixture("cache", None);
        f.client.peers().remember_users(&[make_user(TARGET_ID, TARGET_HASH, "Spam")]);
        let peer = f.svc.find_user(&CancellationToken::new(), &f.client, &here_input(), TARGET_ID).await.unwrap();
        assert_eq!(peer_name(&peer), "user42#4242");
        assert!(f.fake.calls.lock().unwrap().is_empty());

        // 2. Through current channel GetParticipant
        let f = new_fixture(
            "part",
            Some(Box::new(|cid, body| {
                if cid == tl::functions::channels::GetParticipant::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::GetParticipant::from_bytes(&body[4..]).unwrap();
                    if chan_of(&req.channel) == HERE_ID {
                        let resp: tl::enums::channels::ChannelParticipant =
                            tl::types::channels::ChannelParticipant {
                                participant: tl::types::ChannelParticipant {
                                    user_id: TARGET_ID,
                                    date: 1,
                                    subscription_until_date: None,
                                    rank: None,
                                }
                                .into(),
                                chats: vec![],
                                users: vec![make_user(TARGET_ID, TARGET_HASH, "Spam")],
                            }
                            .into();
                        return Some(Ok(resp.to_bytes()));
                    }
                }
                None
            })),
        );
        let peer = f.svc.find_user(&CancellationToken::new(), &f.client, &here_input(), TARGET_ID).await.unwrap();
        assert_eq!(peer_name(&peer), "user42#4242");
        assert_eq!(
            *f.fake.calls.lock().unwrap(),
            vec!["users.getUsers 42#0", "channels.getParticipant 10 user42#0"]
        );

        // 3. Scanning recent members
        let f = new_fixture(
            "recent",
            Some(Box::new(|cid, body| {
                if cid == tl::functions::channels::GetParticipants::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::GetParticipants::from_bytes(&body[4..]).unwrap();
                    let from = if req.offset == 0 { 1000 } else { 2000 };
                    let mut participants = Vec::new();
                    let mut users = Vec::new();
                    for id in from..from + RECENT_PAGE_SIZE {
                        participants.push(
                            tl::types::ChannelParticipant {
                                user_id: i64::from(id),
                                date: 1,
                                subscription_until_date: None,
                                rank: None,
                            }
                            .into(),
                        );
                        users.push(make_user(i64::from(id), 1, "u"));
                    }
                    if req.offset != 0 {
                        users.push(make_user(TARGET_ID, TARGET_HASH, "Spam"));
                    }
                    let resp: tl::enums::channels::ChannelParticipants =
                        tl::types::channels::ChannelParticipants {
                            count: 400,
                            participants,
                            chats: vec![],
                            users,
                        }
                        .into();
                    return Some(Ok(resp.to_bytes()));
                }
                None
            })),
        );
        let peer = f.svc.find_user(&CancellationToken::new(), &f.client, &here_input(), TARGET_ID).await.unwrap();
        assert_eq!(peer_name(&peer), "user42#4242");
        assert_eq!(f.fake.calls.lock().unwrap().len(), 4);

        // 4. Across basic managed group
        let f = new_fixture(
            "basic",
            Some(Box::new(|cid, body| {
                if cid == tl::functions::messages::GetFullChat::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::GetFullChat::from_bytes(&body[4..]).unwrap();
                    if req.chat_id == BASIC_ID {
                        return Some(Ok(full_chat_resp(
                            BASIC_ID,
                            vec![],
                            vec![make_user(TARGET_ID, TARGET_HASH, "Spam")],
                        )));
                    }
                }
                None
            })),
        );
        let peer = f.svc.find_user(&CancellationToken::new(), &f.client, &here_input(), TARGET_ID).await.unwrap();
        assert_eq!(peer_name(&peer), "user42#4242");

        // 5. Fallback to zero hash in channel, fail in PM
        let f = new_fixture("fallback", None);
        let peer = f.svc.find_user(&CancellationToken::new(), &f.client, &here_input(), TARGET_ID).await.unwrap();
        assert_eq!(peer_name(&peer), "user42#0");

        let pm = tl::types::InputPeerUser { user_id: 5, access_hash: 5 }.into();
        let err = f.svc.find_user(&CancellationToken::new(), &f.client, &pm, TARGET_ID).await.unwrap_err();
        assert!(is_unresolved_user(err.as_ref()));
    }

    #[tokio::test]
    async fn test_reply_sender_only_here_and_channel_targets_and_self() {
        let f = new_fixture(
            "reply",
            Some(Box::new(|cid, _| {
                if cid == tl::functions::channels::GetMessages::CONSTRUCTOR_ID {
                    let mut m = kit::blank_tl_message(5, here_peer());
                    m.from_id = Some(tl::types::PeerUser { user_id: TARGET_ID }.into());
                    let resp = kit::tl_messages(
                        vec![m.into()],
                        vec![],
                        vec![tl::enums::User::User(tl::types::User {
                            min: true,
                            ..match make_user(TARGET_ID, 9, "Spam") {
                                tl::enums::User::User(u) => u,
                                _ => unreachable!(),
                            }
                        })],
                    );
                    return Some(Ok(resp.to_bytes()));
                }
                None
            })),
        );
        let err = f
            .svc
            .resolve_target(&invocation(&f, here_peer(), 5, &[]), &here_input())
            .await
            .unwrap_err();
        assert!(is_unresolved_user(err.as_ref()));
        let calls = f.fake.calls.lock().unwrap().clone();
        assert!(!calls.iter().any(|c| c.starts_with("channels.getParticipant 20")));

        // Channel target & self refusal
        let fc = new_fixture(
            "chantarget",
            Some(Box::new(|cid, _| {
                if cid == tl::functions::channels::GetMessages::CONSTRUCTOR_ID {
                    let mut m = kit::blank_tl_message(5, here_peer());
                    m.from_id = Some(tl::types::PeerChannel { channel_id: ALIAS_ID }.into());
                    let ch = tl::types::Channel {
                        broadcast: true,
                        megagroup: false,
                        access_hash: Some(7777),
                        username: Some("spamchan".into()),
                        ..kit::blank_tl_channel(ALIAS_ID, "Spam")
                    };
                    let resp = kit::tl_messages(vec![m.into()], vec![ch.into()], vec![]);
                    return Some(Ok(resp.to_bytes()));
                }
                None
            })),
        );
        let t = fc
            .svc
            .resolve_target(&invocation(&fc, here_peer(), 5, &[]), &here_input())
            .await
            .unwrap();
        assert!(t.channel && t.display == "Channel: Spam (@spamchan)");
        let err = fc
            .svc
            .resolve_target(&invocation(&fc, here_peer(), 0, &["1"]), &here_input())
            .await
            .err()
            .unwrap();
        assert!(!is_unresolved_user(err.as_ref()));
    }

    #[tokio::test]
    async fn test_delete_history_and_top_reasons_and_mute_and_batch() {
        // Delete history loops until offset=0, stops at 100
        let offsets = Arc::new(StdMutex::new(vec![300, 200, 0]));
        let off_ref = Arc::clone(&offsets);
        let f = new_fixture(
            "delhist",
            Some(Box::new(move |cid, _| {
                if cid == tl::functions::channels::DeleteParticipantHistory::CONSTRUCTOR_ID {
                    let mut g = off_ref.lock().unwrap();
                    let next = g.remove(0);
                    let resp: tl::enums::messages::AffectedHistory =
                        tl::types::messages::AffectedHistory {
                            pts: 1,
                            pts_count: 100,
                            offset: next,
                        }
                        .into();
                    return Some(Ok(resp.to_bytes()));
                }
                None
            })),
        );
        let want_found = tl::types::InputPeerUser {
            user_id: TARGET_ID,
            access_hash: TARGET_HASH,
        }
        .into();
        assert!(delete_history(&f.client, &here_input(), &want_found).await);

        let mut reasons = HashMap::new();
        reasons.insert("USER_ADMIN_INVALID".into(), 1);
        reasons.insert("CHAT_ADMIN_REQUIRED".into(), 5);
        reasons.insert("CHANNEL_PRIVATE".into(), 2);
        reasons.insert("A_B".into(), 2);
        reasons.insert("<x>".into(), 1);
        assert_eq!(
            top_reasons(&reasons, 3),
            vec!["CHAT_ADMIN_REQUIRED×5", "A_B×2", "CHANNEL_PRIVATE×2"]
        );

        // Mute 5min + auto delete
        let fm = new_fixture("mute", None);
        fm.client.peers().remember_users(&[make_user(TARGET_ID, TARGET_HASH, "Spam")]);
        fm.svc.basic(&invocation(&fm, here_peer(), 0, &["42", "5min"]), "mute", "禁言").await.unwrap();
        let calls = fm.fake.calls.lock().unwrap().clone();
        assert!(calls.last().unwrap() == "edit 1000 ✅ Muted Spam 5m");
        assert!(calls.iter().any(|c| c == "channels.editBanned 10 user42#4242 view=false send=true timed=true"));
        let task = {
            let mut d = fm.deletions.lock().unwrap();
            assert_eq!(d.len(), 1);
            assert_eq!(d[0].0, RESULT_LIFETIME);
            d.remove(0).1
        };
        task(CancellationToken::new()).await;
        assert!(fm.fake.calls.lock().unwrap().iter().any(|c| c == "channels.deleteMessages 10 [1000]"));

        // Batch ban
        let fb = new_fixture(
            "batch",
            Some(Box::new(|cid, body| {
                if cid == tl::functions::channels::EditBanned::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::EditBanned::from_bytes(&body[4..]).unwrap();
                    if chan_of(&req.channel) != HERE_ID {
                        return Some(Err(rpc_err(400, "CHAT_ADMIN_REQUIRED")));
                    }
                }
                None
            })),
        );
        fb.client.peers().remember_users(&[make_user(TARGET_ID, TARGET_HASH, "Spam")]);
        fb.svc.batch(&invocation(&fb, here_peer(), 0, &["42", "true"]), true).await.unwrap();
        let calls = fb.fake.calls.lock().unwrap().clone();
        let last = calls.last().unwrap();
        assert!(last.contains("✅ Banned Spam across 2 group(s) and channel(s)"));
        assert!(last.contains("Failed in 2 (CHAT_ADMIN_REQUIRED×2)"));
        assert_eq!(fb.deletions.lock().unwrap()[0].0, BATCH_LIFETIME);
    }
}
