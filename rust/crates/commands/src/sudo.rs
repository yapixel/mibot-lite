use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{
    Client, Message, SendOptions, envelope, first_sent_message_id, peer_from_id, peer_id,
    without_ip_privacy,
};
use mibot_command::{DispatchOutcome, Invocation, Route, code, escape, group};
use mibot_logtail::Level;
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

enum Lending {
    All,
    OwnerOnly(&'static [&'static str]),
    Only(fn(&str) -> bool),
}

fn speedtest_use(first: &str) -> bool {
    matches!(
        first,
        "" | "list" | "servers" | "列表" | "help" | "h" | "config"
    ) || kit::is_digits(first)
}

fn sum_use(first: &str) -> bool {
    first.is_empty() || kit::is_digits(first)
}

const ALL_DELEGABLE_NAMES: &[&str] = &[
    "ai", "b64decode", "b64encode", "ban", "bgp", "bin", "calc", "dc", "ddg", "dig", "eat",
    "eat2", "eatgif", "encode", "gt", "help", "ids", "ip", "kick", "memory", "mute", "ping",
    "rate", "re", "speedtest", "status", "sum", "tr", "unban", "unmute", "urldecode", "urlencode",
    "version", "whois", "yvlu",
];

fn lending_rule(cmd_name: &str) -> Option<Lending> {
    Some(match cmd_name {
        "ping" | "help" | "version" | "status" | "memory" | "calc" | "rate" | "gt" | "ip"
        | "bin" | "ids" | "dc" | "re" | "dig" | "bgp" | "ddg" | "encode" | "b64encode"
        | "b64decode" | "urlencode" | "urldecode" | "ban" | "unban" | "kick" | "mute"
        | "unmute" => Lending::All,
        "tr" => Lending::OwnerOnly(&["set"]),
        "yvlu" => Lending::OwnerOnly(&["config", "s"]),
        "whois" => Lending::OwnerOnly(&["clear", "history"]),
        "eatgif" => Lending::OwnerOnly(&["clear"]),
        "eat" | "eat2" => Lending::OwnerOnly(&["set"]),
        "ai" => Lending::OwnerOnly(&[
            "config",
            "model",
            "reasoning",
            "service",
            "prompt",
            "collapse",
            "timeout",
            "telegraph",
        ]),
        "sum" => Lending::Only(sum_use),
        "speedtest" => Lending::Only(speedtest_use),
        _ => return None,
    })
}

pub fn delegation_allowed(route: &Route) -> bool {
    let Some(rule) = lending_rule(&route.command) else {
        return false;
    };
    let first = route
        .args
        .first()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match rule {
        Lending::All => true,
        Lending::OwnerOnly(disallowed) => first.is_empty() || !disallowed.contains(&first.as_str()),
        Lending::Only(pred) => pred(&first),
    }
}

pub fn delegable() -> Vec<String> {
    ALL_DELEGABLE_NAMES.iter().map(|s| (*s).to_owned()).collect()
}

pub fn lending(name: &str) -> &'static str {
    match lending_rule(name) {
        None => "none",
        Some(Lending::All) => "all",
        Some(Lending::OwnerOnly(_) | Lending::Only(_)) => "partly",
    }
}

fn delegable_list(prefix: &str) -> String {
    delegable()
        .into_iter()
        .map(|n| format!("{prefix}{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DelegateEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SureRule {
    #[serde(default)]
    id: i32,
    #[serde(default)]
    msg: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    redirect: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DelegateDocument {
    #[serde(default)]
    users: Vec<DelegateEntry>,
    #[serde(default)]
    chats: Vec<DelegateEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    messages: Vec<SureRule>,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    next_id: i32,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

impl DelegateDocument {
    fn has_user(&self, id: &str) -> bool {
        self.users.iter().any(|u| u.id == id)
    }

    fn allows_chat(&self, id: &str) -> bool {
        self.chats.is_empty() || self.chats.iter().any(|c| c.id == id)
    }

    fn match_text(&self, text: &str) -> Option<String> {
        for rule in &self.messages {
            if let Some(prefix) = rule.msg.strip_prefix("_command:") {
                let Some(rest) = text.strip_prefix(prefix) else {
                    continue;
                };
                if !rest.is_empty() && !rest.starts_with(' ') {
                    continue;
                }
                if !rule.redirect.is_empty() {
                    return Some(format!("{}{rest}", rule.redirect));
                }
                return Some(text.to_owned());
            }
            if rule.msg == text {
                if !rule.redirect.is_empty() {
                    return Some(rule.redirect.clone());
                }
                return Some(text.to_owned());
            }
        }
        None
    }
}

fn sender_user_id(message: &Message) -> String {
    match &message.sender {
        tl::enums::Peer::User(u) => u.user_id.to_string(),
        _ => String::new(),
    }
}

fn active_delegate(
    saved: &Store<DelegateDocument>,
    user: &str,
    chat: &str,
) -> Option<DelegateDocument> {
    let doc = saved.read().ok()?;
    if doc.has_user(user) && doc.allows_chat(chat) {
        Some(doc)
    } else {
        None
    }
}

async fn relay(
    cancel: CancellationToken,
    a: Arc<App>,
    client: Arc<Client>,
    trigger: Message,
    text: String,
    entities: Option<Vec<tl::enums::MessageEntity>>,
) {
    let user = sender_user_id(&trigger);
    let mut is_command = false;
    let mut parsed_route = None;
    if let Some(route) = a.registry.parse(&text)
        && a.registry.command(&route.command).is_some()
    {
        is_command = true;
        parsed_route = Some(route);
    }

    let Ok(peer) = client.input_peer(&trigger.peer) else {
        a.logs.record(
            Level::Warn,
            "delegate.unaddressable",
            &[("from", &user), ("chat", &trigger.chat_id)],
        );
        return;
    };

    let topic = Some(trigger.topic_id).filter(|&id| id > 0);
    if is_command && let Some(ref route) = parsed_route && !delegation_allowed(route) {
        a.logs.record(
            Level::Info,
            "delegate.refused",
            &[
                ("from", &user),
                ("chat", &trigger.chat_id),
                ("command", &route.command),
            ],
        );
        let _ = client
            .send_plain(
                &peer,
                &format!("⛔ {}{} 只有账号本人能用", route.prefix, route.command),
                None,
                SendOptions {
                    reply_to: Some(trigger.id),
                    topic_id: topic,
                    ..Default::default()
                },
            )
            .await;
        return;
    }

    let updates = match without_ip_privacy(|| {
        client.send_plain(
            &peer,
            &text,
            entities,
            SendOptions {
                reply_to: Some(trigger.reply_to_id).filter(|&id| id > 0),
                topic_id: topic,
                ..Default::default()
            },
        )
    })
    .await
    {
        Ok(u) => u,
        Err(err) => {
            let err_s = err.to_string();
            a.logs.record(
                Level::Warn,
                "delegate.send_failed",
                &[
                    ("from", &user),
                    ("chat", &trigger.chat_id),
                    ("error", &err_s),
                ],
            );
            return;
        }
    };
    let Some(msg_id) = first_sent_message_id(&updates) else {
        return;
    };
    let cmd_label = parsed_route.as_ref().map(|r| r.command.as_str()).unwrap_or("");
    let msg_id_s = msg_id.to_string();
    a.logs.record(
        Level::Info,
        "delegate.relayed",
        &[
            ("from", &user),
            ("chat", &trigger.chat_id),
            ("command", cmd_label),
            ("message", &msg_id_s),
        ],
    );
    if !is_command {
        return;
    }
    let Ok(sent) = client.get_messages(&peer, &[msg_id]).await else {
        a.logs.record(
            Level::Warn,
            "delegate.read_back_failed",
            &[("from", &user), ("chat", &trigger.chat_id)],
        );
        return;
    };
    let Some(first) = sent.into_iter().next() else {
        a.logs.record(
            Level::Warn,
            "delegate.read_back_failed",
            &[("from", &user), ("chat", &trigger.chat_id)],
        );
        return;
    };
    let Some(env) = envelope(&first, client.self_id(), false, client.peers()) else {
        return;
    };
    if matches!(
        a.registry
            .dispatch_for_with_gate(env, false, cancel, delegation_allowed),
        DispatchOutcome::Rejected(_)
    ) {
        let _ = client
            .send_plain(
                &peer,
                "⏳ 同时执行的命令太多或服务正在更新，这条没有执行，稍后再试",
                None,
                SendOptions {
                    reply_to: Some(msg_id),
                    topic_id: topic,
                    ..Default::default()
                },
            )
            .await;
    }
}

async fn resolve_user(inv: &Invocation, arg: &str) -> Result<DelegateEntry, mibot_command::CmdError> {
    if !arg.is_empty() {
        if let Ok(id) = arg.parse::<i64>() {
            if id <= 0 {
                return Err(kit::fail_err(format!("{arg} 不是用户 ID")));
            }
            let p = tl::enums::Peer::User(tl::types::PeerUser { user_id: id });
            return Ok(DelegateEntry {
                id: arg.to_owned(),
                name: inv.client.peers().title(&p),
            });
        }
        let clean = arg.trim_start_matches('@');
        let peer = inv
            .client
            .resolve_username(clean)
            .await
            .map_err(|_| kit::fail_err(format!("找不到 {arg}")))?;
        let tl::enums::InputPeer::User(u) = peer else {
            return Err(kit::fail_err(format!("{arg} 不是用户")));
        };
        return Ok(DelegateEntry {
            id: u.user_id.to_string(),
            name: format!("@{clean}"),
        });
    }
    if inv.message.reply_to_id == 0 {
        return Err(kit::fail_err("回复对方的消息，或者带上数字 ID、@用户名"));
    }
    let Some(reply) = kit::reply_msg(inv).await? else {
        return Err(kit::fail_err("回复对方的消息，或者带上数字 ID、@用户名"));
    };
    let id = sender_user_id(&reply);
    if id.is_empty() {
        return Err(kit::fail_err(
            "被回复的消息不是用户发的（频道身份或匿名管理员不能授权）",
        ));
    }
    Ok(DelegateEntry {
        id,
        name: inv.client.peers().title(&reply.sender),
    })
}

async fn resolve_chat(inv: &Invocation, arg: &str) -> Result<DelegateEntry, mibot_command::CmdError> {
    if arg.is_empty() {
        return Ok(DelegateEntry {
            id: inv.message.chat_id.clone(),
            name: inv.client.peers().title(&inv.message.peer),
        });
    }
    if let Some(peer) = peer_from_id(arg) {
        return Ok(DelegateEntry {
            id: arg.to_owned(),
            name: inv.client.peers().title(&peer),
        });
    }
    let peer = inv
        .client
        .resolve_target(arg)
        .await
        .map_err(|_| kit::fail_err(format!("找不到 {arg}")))?;
    let id = match peer {
        tl::enums::InputPeer::Channel(c) => {
            peer_id(&tl::enums::Peer::Channel(tl::types::PeerChannel {
                channel_id: c.channel_id,
            }))
        }
        tl::enums::InputPeer::Chat(c) => peer_id(&tl::enums::Peer::Chat(tl::types::PeerChat {
            chat_id: c.chat_id,
        })),
        tl::enums::InputPeer::User(u) => peer_id(&tl::enums::Peer::User(tl::types::PeerUser {
            user_id: u.user_id,
        })),
        _ => return Err(kit::fail_err(format!("找不到 {arg}"))),
    };
    Ok(DelegateEntry {
        id,
        name: arg.to_owned(),
    })
}

fn render_entries(title: &str, entries: &[DelegateEntry], empty: &str) -> String {
    if entries.is_empty() {
        return empty.to_owned();
    }
    let mut lines = vec![format!("<b>{}</b>", escape(title))];
    for e in entries {
        let name = if e.name.is_empty() || e.name == e.id {
            "（未知名称）"
        } else {
            &e.name
        };
        lines.push(format!("• {} {}", escape(name), code(&e.id)));
    }
    lines.join("\n")
}

fn add_entry(entries: &mut Vec<DelegateEntry>, entry: DelegateEntry) {
    if let Some(existing) = entries.iter_mut().find(|e| e.id == entry.id) {
        *existing = entry;
    } else {
        entries.push(entry);
    }
}

fn remove_entry(entries: &mut Vec<DelegateEntry>, id: &str) -> bool {
    let before = entries.len();
    entries.retain(|e| e.id != id);
    entries.len() != before
}

async fn manage_lists(
    inv: &Invocation,
    saved: &Store<DelegateDocument>,
    name: &str,
) -> Result<bool, mibot_command::CmdError> {
    let mut action = inv.arg(0).to_ascii_lowercase();
    let mut target = inv.arg(1).to_owned();
    let is_chat = action == "chat";
    if is_chat {
        action = inv.arg(1).to_ascii_lowercase();
        if action.is_empty() {
            action = "ls".to_owned();
        }
        target = inv.arg(2).to_owned();
    }
    let (title, empty, missing_fmt, added, removed) = if is_chat {
        (
            "对话名单".to_owned(),
            "⚠️ 没有设对话名单，所有对话里都能用".to_owned(),
            "对话名单里没有 {id}".to_owned(),
            format!("已加入 {name} 对话名单"),
            format!("已移出 {name} 对话名单"),
        )
    } else {
        (
            format!("{name} 用户名单"),
            "当前没有任何用户".to_owned(),
            format!("{name} 名单里没有 {{id}}"),
            "已授权".to_owned(),
            "已取消授权".to_owned(),
        )
    };

    match action.as_str() {
        "ls" | "list" => {
            let current = saved.read()?;
            let entries = if is_chat { &current.chats } else { &current.users };
            inv.edit(&render_entries(&title, entries, &empty)).await?;
            Ok(true)
        }
        "add" | "del" => {
            let entry = if is_chat {
                resolve_chat(inv, &target).await?
            } else {
                resolve_user(inv, &target).await?
            };
            if !is_chat && entry.id == inv.client.self_id().to_string() {
                return Err(kit::fail_err("不能把账号自己加进名单"));
            }
            let mut changed = false;
            let entry_clone = entry.clone();
            saved.update(|doc| {
                let list = if is_chat { &mut doc.chats } else { &mut doc.users };
                if action == "add" {
                    add_entry(list, entry_clone);
                    changed = true;
                } else {
                    changed = remove_entry(list, &entry_clone.id);
                }
                Ok::<(), std::convert::Infallible>(())
            })?;
            let verb = if action == "del" {
                if !changed {
                    return Err(kit::fail_err(missing_fmt.replace("{id}", &entry.id)));
                }
                removed
            } else {
                added
            };
            inv.edit(&format!(
                "✅ {verb}：{} {}",
                escape(&entry.name),
                code(&entry.id)
            ))
            .await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn manage_rules(
    inv: &Invocation,
    saved: &Store<DelegateDocument>,
) -> mibot_command::CmdResult {
    let action = inv.arg(1).to_ascii_lowercase();
    let raw = |skip: usize| -> String {
        inv.raw_after(skip)
            .split('\n')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    match action.as_str() {
        "ls" | "list" | "" => {
            let current = saved.read()?;
            if current.messages.is_empty() {
                return inv
                    .edit(&format!(
                        "⚠️ 还没有消息规则，sure 不会生效。\n{} 添加一条",
                        code(&format!("{}sure msg add _command:/sb", inv.prefix()))
                    ))
                    .await
                    .map_err(Into::into);
            }
            let mut lines = vec!["🎯 <b>消息规则</b>".to_owned()];
            for rule in &current.messages {
                let mut line = format!("{} {}", code(&rule.id.to_string()), code(&rule.msg));
                if !rule.redirect.is_empty() {
                    line.push_str(&format!(" → {}", code(&rule.redirect)));
                }
                lines.push(line);
            }
            inv.edit(&lines.join("\n")).await.map_err(Into::into)
        }
        "add" => {
            let text = raw(2);
            if text.is_empty() {
                return Err(kit::usage(inv.prefix(), "sure msg add 消息原文"));
            }
            let mut id = 0i32;
            saved.update(|doc| {
                doc.next_id += 1;
                id = doc.next_id;
                doc.messages.push(SureRule {
                    id,
                    msg: text.clone(),
                    redirect: String::new(),
                });
                Ok::<(), std::convert::Infallible>(())
            })?;
            inv.edit(&format!(
                "✅ 已添加规则 {}：{}",
                code(&id.to_string()),
                code(&text)
            ))
            .await
            .map_err(Into::into)
        }
        "redirect" | "del" => {
            let id: i32 = inv.arg(2).parse().map_err(|_| {
                kit::fail_err(format!("请给出规则编号，{}sure msg ls 可以看", inv.prefix()))
            })?;
            let target = raw(3);
            let mut found = false;
            saved.update(|doc| {
                if let Some(pos) = doc.messages.iter().position(|r| r.id == id) {
                    found = true;
                    if action == "del" {
                        doc.messages.remove(pos);
                    } else {
                        doc.messages[pos].redirect = target.clone();
                    }
                }
                Ok::<(), std::convert::Infallible>(())
            })?;
            if !found {
                return Err(kit::fail_err(format!("没有编号为 {id} 的规则")));
            }
            if action == "del" {
                return inv
                    .edit(&format!("✅ 已删除规则 {}", code(&id.to_string())))
                    .await
                    .map_err(Into::into);
            }
            if target.is_empty() {
                return inv
                    .edit(&format!("✅ 已清除规则 {} 的重定向", code(&id.to_string())))
                    .await
                    .map_err(Into::into);
            }
            inv.edit(&format!(
                "✅ 规则 {} 重定向到 {}",
                code(&id.to_string()),
                code(&target)
            ))
            .await
            .map_err(Into::into)
        }
        _ => inv.edit(&sure_help(inv.prefix())).await.map_err(Into::into),
    }
}

fn sudo_help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "🔐 <b>sudo：让别人用你的账号执行命令</b>\n\n\
         名单里的人发一条命令（用你的前缀），账号就以你的身份在同一个对话里发出这条命令并执行，回复同一个目标。\n\n\
         • <code>{p}sudo add</code> 回复对方的消息，或带上数字 ID、@用户名\n\
         • <code>{p}sudo del</code> 同上\n\
         • <code>{p}sudo ls</code> 用户名单\n\
         • <code>{p}sudo chat add</code> 把当前对话（或带上 ID、@名称）加进对话名单\n\
         • <code>{p}sudo chat del</code> / <code>chat ls</code>\n\n\
         ⚠️ 没设对话名单时，名单里的人在所有有你的对话里都能用。\n\n\
         <b>能借出去的命令</b>\n{}\n\
         其中改设置的子命令（如 ai config、tr set、speedtest set）只限本人；sum 只能借「摘要当前群组」，定时任务和配置都不行。\n\n\
         <b>只限你本人</b>\n授权管理、删消息、改昵称或前缀别名、备份与日志、转存消息、重启更新、跨所有群的封禁、看主机信息。\
         名单里的人发这些命令，账号只会回一句没有权限。",
        escape(&delegable_list(prefix)),
    )
}

fn sure_help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "🎯 <b>sure：让别人触发指定的消息或命令</b>\n\n\
         比 sudo 窄：名单里的人发的消息要和规则对上，账号才会以你的身份发出去；规则可以重定向成别的命令。\n\n\
         <b>用户和对话</b>\n\
         • <code>{p}sure add</code> / <code>del</code> / <code>ls</code> 和 sudo 一样\n\
         • <code>{p}sure chat add</code> / <code>del</code> / <code>ls</code> 对话名单，没设就处处可用\n\n\
         <b>消息规则（至少要有一条才会生效）</b>\n\
         • <code>{p}sure msg add 消息原文</code> 整条消息一致才算\n\
         • <code>{p}sure msg add _command:/sb</code> 以 /sb 开头的都算，如 /sb 和 /sb 123\n\
         • <code>{p}sure msg redirect 编号 {p}ban</code> 重定向：/sb 123 会变成 {p}ban 123；不写目标就是清除重定向\n\
         • <code>{p}sure msg del 编号</code> / <code>msg ls</code>\n\n\
         <b>典型用法</b>\n规则 <code>_command:/sb</code> 重定向到 <code>{p}ban</code>，再把群成员加进用户名单。他们回复某人发 /sb，就会以你的身份执行 <code>{p}ban</code>。\
         触发的那条消息 5 秒后删掉（需要管理员权限）。\n\n\
         ⚠️ 重定向出来的命令和 sudo 一样受限，只限本人的命令不会被执行。"
    )
}

pub fn register(a: &Arc<App>) {
    let sudo_list = Arc::new(kit::new_store(a, "sudo.json", DelegateDocument::default));
    let sure_list = Arc::new(kit::new_store(a, "sure.json", DelegateDocument::default));

    // Register sure listener before sudo listener (specific matching + redirect beats generic sudo).
    let sure_for_foreign = Arc::clone(&sure_list);
    a.on_foreign(Arc::new(move |message, app, cancel| {
        let user = sender_user_id(message);
        if user.is_empty() {
            return false;
        }
        let Some(current) = active_delegate(&sure_for_foreign, &user, &message.chat_id) else {
            return false;
        };
        let Some(text) = current.match_text(&message.text) else {
            return false;
        };
        let sure_ref = Arc::clone(&sure_for_foreign);
        let app_ref = Arc::clone(app);
        let client_ref = Arc::clone(&app.client);
        let msg_clone = message.clone();
        app.registry.background(cancel.clone(), move |tok| {
            Box::pin(async move {
                let recheck = active_delegate(&sure_ref, &user, &msg_clone.chat_id)
                    .and_then(|d| d.match_text(&msg_clone.text));
                if recheck.as_deref() != Some(text.as_str()) {
                    return;
                }
                relay(tok.clone(), app_ref, client_ref.clone(), msg_clone.clone(), text, None).await;
                if kit::sleep(&tok, Duration::from_secs(5)).await {
                    let _ = client_ref.delete_message(&msg_clone).await;
                }
            })
        });
        true
    }));

    let sudo_for_foreign = Arc::clone(&sudo_list);
    a.on_foreign(Arc::new(move |message, app, cancel| {
        let user = sender_user_id(message);
        if user.is_empty() {
            return false;
        }
        if active_delegate(&sudo_for_foreign, &user, &message.chat_id).is_none() {
            return false;
        }
        let Some(route) = app.registry.parse(&message.text) else {
            return false;
        };
        if app.registry.command(&route.command).is_none() {
            return false;
        }
        let entities = message.raw.entities.clone();
        let sudo_ref = Arc::clone(&sudo_for_foreign);
        let app_ref = Arc::clone(app);
        let client_ref = Arc::clone(&app.client);
        let msg_clone = message.clone();
        app.registry.background(cancel.clone(), move |tok| {
            Box::pin(async move {
                if active_delegate(&sudo_ref, &user, &msg_clone.chat_id).is_some() {
                    let text = msg_clone.text.clone();
                    relay(tok, app_ref, client_ref, msg_clone, text, entities).await;
                }
            })
        });
        true
    }));

    let sudo_cmd = Arc::clone(&sudo_list);
    a.registry.register(kit::make_cmd(
        "sudo",
        &[],
        group::ACCOUNT,
        "把命令借给名单里的人",
        "[add|del|ls|chat]",
        sudo_help,
        move |inv: Invocation| {
            let saved = Arc::clone(&sudo_cmd);
            Box::pin(async move {
                if manage_lists(&inv, &saved, "sudo").await? {
                    return Ok(());
                }
                inv.edit(&sudo_help(inv.prefix())).await.map_err(Into::into)
            })
        },
    ));

    let sure_cmd = Arc::clone(&sure_list);
    a.registry.register(kit::make_cmd(
        "sure",
        &[],
        group::ACCOUNT,
        "让名单里的人触发指定消息",
        "[add|del|ls|chat|msg]",
        sure_help,
        move |inv: Invocation| {
            let saved = Arc::clone(&sure_cmd);
            Box::pin(async move {
                if inv.arg(0).eq_ignore_ascii_case("msg") {
                    return manage_rules(&inv, &saved).await;
                }
                if manage_lists(&inv, &saved, "sure").await? {
                    return Ok(());
                }
                inv.edit(&sure_help(inv.prefix())).await.map_err(Into::into)
            })
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use mibot_command::{Command, Registry};
    use mibot_logtail::Ring;
    use std::collections::HashMap;

    #[test]
    fn test_sure_rule_matching() {
        let rules = DelegateDocument {
            messages: vec![
                SureRule { id: 1, msg: "_command:/sb".into(), redirect: ".ban".into() },
                SureRule { id: 2, msg: "_command:/ping".into(), redirect: String::new() },
                SureRule { id: 3, msg: "晚安".into(), redirect: ".ping".into() },
            ],
            ..Default::default()
        };
        for (text, want) in [
            ("/sb", Some(".ban")),
            ("/sb 1 2", Some(".ban 1 2")),
            ("/sbx", None),
            (" /sb", None),
            ("/ping now", Some("/ping now")),
            ("晚安", Some(".ping")),
            ("晚安 ", None),
        ] {
            assert_eq!(rules.match_text(text).as_deref(), want, "{text:?}");
        }
    }

    #[test]
    fn test_delegation_allowed() {
        let reg = Registry::new();
        reg.set_prefixes(vec![".".into()]);
        for name in [
            "ping", "da", "ai", "sudo", "sb", "sum", "speedtest", "yvlu", "whois", "tr",
        ] {
            reg.register(Command {
                name: name.into(),
                group: group::SYSTEM.into(),
                description: "test command".into(),
                handle: Arc::new(|_| Box::pin(async { Ok(()) })),
                ..Default::default()
            });
        }
        let mut aliases = HashMap::new();
        aliases.insert("wipe".into(), "da true".into());
        aliases.insert("p".into(), "ping".into());
        aliases.insert("key".into(), "ai config key".into());
        reg.set_aliases(&aliases);

        for (text, want) in [
            (".ping", true),
            (".p", true),
            (".ai 你好", true),
            (".da true", false),
            (".wipe", false),
            (".sudo add 1", false),
            (".sb 1", false),
            (".ai config key x", false),
            (".ai CONFIG", false),
            (".key x", false),
            (".sum", true),
            (".sum 200", true),
            (".sum now", false),
            (".sum edit 1 prompt x", false),
            (".sum ls", false),
            (".sum nosuch", false),
            (".speedtest", true),
            (".speedtest 12345", true),
            (".speedtest list", true),
            (".speedtest fix", false),
            (".speedtest set 1", false),
            (".speedtest diagnose", false),
            (".yvlu", true),
            (".yvlu 3", true),
            (".yvlu s", false),
            (".whois a.com", true),
            (".whois history", false),
            (".speedtest config", true),
            (".tr hello", true),
            (".tr en hello", true),
            (".tr set en", false),
            (".tr SET en", false),
        ] {
            let route = reg.parse(text).expect(text);
            assert_eq!(delegation_allowed(&route), want, "{text:?}");
        }
        let _ = Ring::new();
    }
}
