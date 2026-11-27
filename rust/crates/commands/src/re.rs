use crate::kit;
use crate::save;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{Client, Message};
use mibot_command::{CmdResult, Invocation, group};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn re_usage() -> &'static str {
    kit::tr("re [count] [times]", "re [消息数] [复读次数]")
}

fn help(prefix: &str) -> String {
    let p = mibot_command::escape(prefix);
    if kit::is_zh() {
        format!(
            "🔁 <b>消息复读</b>\n\n回复一条消息，把从它开始往后的若干条消息转发到当前对话。\n\n\
             • 回复消息发送 <code>{p}re</code> 转发一条，一次\n\
             • <code>{p}re 3</code> 转发被回复的消息和它之后的 2 条，已删除的消息不算数\n\
             • <code>{p}re 3 2</code> 将这 3 条消息重复转发 2 次\n\n\
             消息数最多 20，复读次数最多 10。对话禁止转发时，改为把内容重新发一遍。"
        )
    } else {
        format!(
            "🔁 <b>Repeat Messages</b>\n\nReply to a message to forward it and the following messages into the current chat.\n\n\
             • Reply with <code>{p}re</code> to forward 1 message once\n\
             • <code>{p}re 3</code> forward the replied message and 2 following ones (deleted messages are skipped)\n\
             • <code>{p}re 3 2</code> repeat forwarding those 3 messages 2 times\n\n\
             Max 20 messages and 10 repeats. If forwarding is restricted, contents are re-sent instead."
        )
    }
}

fn forum_topic(message: &Message) -> i32 {
    let Some(tl::enums::MessageReplyHeader::Header(header)) = &message.raw.reply_to else {
        return 0;
    };
    if header.forum_topic {
        message.topic_id
    } else {
        0
    }
}

fn is_command_message(inv: &Invocation, msg: &tl::types::Message) -> bool {
    if msg.id == inv.message.id {
        return true;
    }
    if inv.foreign
        && let Some(route) = inv.registry.parse(&msg.message)
        && inv.registry.command(&route.command).is_some()
    {
        return true;
    }
    false
}

async fn fetch_from(
    inv: &Invocation,
    peer: &tl::enums::InputPeer,
    first: i32,
    count: i32,
) -> Result<Vec<tl::types::Message>, mibot_bot::BotError> {
    let res = inv
        .client
        .invoke(&tl::functions::messages::GetHistory {
            peer: peer.clone(),
            offset_id: first,
            offset_date: 0,
            add_offset: -count,
            limit: count,
            max_id: 0,
            min_id: 0,
            hash: 0,
        })
        .await?;
    let mut messages: Vec<tl::types::Message> = kit::unpack_messages(res)
        .into_iter()
        .filter_map(|item| match item {
            tl::enums::Message::Message(m)
                if m.id >= first && !is_command_message(inv, &m) =>
            {
                Some(m)
            }
            _ => None,
        })
        .collect();
    messages.sort_by_key(|m| m.id);
    Ok(messages)
}

async fn repeat(root: &Path, inv: &Invocation) -> CmdResult {
    let reply = kit::reply_msg(inv).await?;
    let raw_count = inv.arg(0).parse::<i64>().unwrap_or(0);
    let raw_times = inv.arg(1).parse::<i64>().unwrap_or(0);
    let count = kit::clamp(raw_count, 1, 20) as i32;
    let times = kit::clamp(raw_times, 1, 10) as i32;
    let Some(reply) = reply else {
        return Err(kit::fail(if kit::is_zh() {
            format!("请回复一条消息再用：{}{}", inv.prefix(), re_usage())
        } else {
            format!("Reply to a message first: {}{}", inv.prefix(), re_usage())
        }));
    };
    let source = inv.client.input_peer(&reply.peer)?;
    let target = inv.peer_result()?;
    let messages = fetch_from(inv, &source, reply.id, count).await?;
    if messages.is_empty() {
        return Err(kit::fail(kit::tr(
            "Replied message is no longer available",
            "被回复的消息已经不在了",
        )));
    }
    let ids: Vec<i32> = messages.iter().map(|m| m.id).collect();
    let topic = forum_topic(&inv.message);
    let topic_opt = (topic > 0).then_some(topic);

    let mut forwarded = 0;
    while forwarded < times {
        match inv
            .client
            .forward_messages(&source, &target, &ids, topic_opt)
            .await
        {
            Ok(_) => forwarded += 1,
            Err(e) if inv.cancel.is_cancelled() => return Err(e.into()),
            Err(e) if e.is_rpc("CHAT_FORWARDS_RESTRICTED") => break,
            Err(e) => return Err(kit::wrap_fail(kit::tr("Repeat failed", "复读失败"), Box::new(e))),
        }
    }

    while forwarded < times {
        for message in &messages {
            match save::copy_message(&inv.cancel, &inv.client, root, message, &target, topic).await {
                Ok(()) => {}
                Err(e) if save::is_nothing_to_copy(e.as_ref()) => {}
                Err(e) if inv.cancel.is_cancelled() => return Err(e),
                Err(e) => return Err(kit::wrap_fail(kit::tr("Repeat failed", "复读失败"), e)),
            }
        }
        forwarded += 1;
    }

    inv.delete().await.map_err(Into::into)
}

async fn handle(root: &Path, inv: &Invocation) -> CmdResult {
    let _ = Client::self_id;
    repeat(root, inv).await
}

pub fn register(a: &Arc<App>) {
    let root = a.root.clone();
    let mut cmd = kit::make_cmd(
        "re",
        &[],
        group::MEDIA,
        "Repeat replied messages",
        "[count] [times]",
        help,
        move |inv: Invocation| {
            let root = root.clone();
            Box::pin(async move { handle(&root, &inv).await })
        },
    )
    .zh("重复转发回复的消息", "[消息数] [次数]");
    cmd.timeout = Some(Duration::from_secs(30 * 60));
    a.registry.register(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, PeerCache, RpcTransport, SentRecorder, envelope};
    use mibot_command::Registry;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    const GROUP: i64 = 5000;

    struct HistoryFake {
        messages: Mutex<HashMap<i32, tl::enums::Message>>,
        restricted: bool,
        calls: Mutex<Vec<String>>,
    }

    impl RpcTransport for HistoryFake {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                let payload = &body[4..];
                match constructor_id {
                    tl::functions::channels::GetMessages::CONSTRUCTOR_ID => {
                        let req = tl::functions::channels::GetMessages::from_bytes(payload).unwrap();
                        let map = self.messages.lock().unwrap();
                        let mut found = Vec::new();
                        for id in &req.id {
                            let msg_id = match id {
                                tl::enums::InputMessage::Id(m) => m.id,
                                _ => 0,
                            };
                            if let Some(m) = map.get(&msg_id) {
                                found.push(m.clone());
                            }
                        }
                        Ok(kit::tl_messages(found, vec![], vec![]).to_bytes())
                    }
                    tl::functions::messages::GetHistory::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::GetHistory::from_bytes(payload).unwrap();
                        self.calls.lock().unwrap().push(format!(
                            "history offset={} add={} limit={}",
                            req.offset_id, req.add_offset, req.limit
                        ));
                        let map = self.messages.lock().unwrap();
                        let mut ids: Vec<i32> = map.keys().copied().collect();
                        ids.sort_by(|a, b| b.cmp(a));
                        let start = ids.iter().position(|&id| id < req.offset_id).unwrap_or(ids.len());
                        let start = (start as i32 + req.add_offset).max(0) as usize;
                        let end = (start + req.limit.max(0) as usize).min(ids.len());
                        let page: Vec<tl::enums::Message> = ids[start..end]
                            .iter()
                            .filter_map(|id| map.get(id).cloned())
                            .collect();
                        Ok(tl::enums::messages::Messages::ChannelMessages(
                            tl::types::messages::ChannelMessages {
                                inexact: false,
                                pts: 1,
                                count: page.len() as i32,
                                offset_id_offset: None,
                                messages: page,
                                topics: vec![],
                                chats: vec![],
                                users: vec![],
                            },
                        )
                        .to_bytes())
                    }
                    tl::functions::messages::ForwardMessages::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::messages::ForwardMessages::from_bytes(payload).unwrap();
                        self.calls.lock().unwrap().push(format!(
                            "forward {:?} top={}",
                            req.id,
                            req.top_msg_id.unwrap_or(0)
                        ));
                        if self.restricted {
                            return Err(InvocationError::Rpc(RpcError {
                                code: 400,
                                name: "CHAT_FORWARDS_RESTRICTED".into(),
                                value: None,
                                caused_by: None,
                            }));
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
                    tl::functions::messages::SendMessage::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::SendMessage::from_bytes(payload).unwrap();
                        let top = match req.reply_to {
                            Some(tl::enums::InputReplyTo::Message(r)) => r.top_msg_id.unwrap_or(0),
                            _ => 0,
                        };
                        self.calls
                            .lock()
                            .unwrap()
                            .push(format!("send {:?} top={}", req.message, top));
                        Ok(tl::enums::Updates::UpdateShortSentMessage(
                            tl::types::UpdateShortSentMessage {
                                out: true,
                                id: 900,
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
                    tl::functions::channels::DeleteMessages::CONSTRUCTOR_ID => {
                        let req =
                            tl::functions::channels::DeleteMessages::from_bytes(payload).unwrap();
                        self.calls.lock().unwrap().push(format!("delete {:?}", req.id));
                        Ok(tl::enums::messages::AffectedMessages::Messages(
                            tl::types::messages::AffectedMessages {
                                pts: 1,
                                pts_count: req.id.len() as i32,
                            },
                        )
                        .to_bytes())
                    }
                    _ => Err(InvocationError::Dropped("UNEXPECTED_REQUEST")),
                }
            })
        }
    }

    fn chat(restricted: bool) -> Arc<HistoryFake> {
        let mut messages = HashMap::new();
        for id in [10, 12, 13, 15, 20] {
            let mut m = kit::blank_tl_message(id, tl::types::PeerChannel { channel_id: GROUP }.into());
            m.message = format!("第{id}条");
            m.from_id = Some(tl::types::PeerUser { user_id: 2 }.into());
            messages.insert(id, tl::enums::Message::Message(m));
        }
        messages.insert(
            14,
            tl::enums::Message::Service(tl::types::MessageService {
                out: false,
                mentioned: false,
                media_unread: false,
                reactions_are_possible: false,
                silent: false,
                post: false,
                legacy: false,
                id: 14,
                from_id: None,
                peer_id: tl::types::PeerChannel { channel_id: GROUP }.into(),
                saved_peer_id: None,
                reply_to: None,
                date: 0,
                action: tl::enums::MessageAction::PinMessage,
                reactions: None,
                ttl_period: None,
            }),
        );
        Arc::new(HistoryFake {
            messages: Mutex::new(messages),
            restricted,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn invocation(fake: &Arc<HistoryFake>, foreign: bool, forum: bool, args: &[&str]) -> Invocation {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let mut ch = kit::blank_tl_channel(GROUP, "群");
        ch.access_hash = Some(1);
        ch.megagroup = true;
        peers.remember_chats(&[tl::enums::Chat::Channel(ch)]);
        let client = Arc::new(Client::from_transport(
            Arc::clone(fake) as Arc<dyn RpcTransport>,
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let registry = Arc::new(Registry::new());
        registry.register(kit::make_cmd("re", &[], group::MEDIA, "desc", "", |_| String::new(), |_| Box::pin(async { Ok(()) })));
        let mut raw = kit::blank_tl_message(100, tl::types::PeerChannel { channel_id: GROUP }.into());
        raw.out = true;
        raw.reply_to = Some(
            tl::types::MessageReplyHeader {
                reply_to_scheduled: false,
                forum_topic: forum,
                quote: false,
                reply_to_ephemeral: false,
                reply_to_msg_id: Some(12),
                reply_to_peer_id: None,
                reply_from: None,
                reply_media: None,
                reply_to_top_id: Some(7),
                quote_text: None,
                quote_entities: None,
                quote_offset: None,
                todo_item_id: None,
                poll_option: None,
                quote_rich: None,
            }
            .into(),
        );
        let msg = envelope(&raw, 1, false, &peers).unwrap();
        Invocation {
            client,
            registry,
            message: msg,
            prefix: ".".into(),
            command: "re".into(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            raw: args.join(" "),
            cancel: CancellationToken::new(),
            foreign,
        }
    }

    #[tokio::test]
    async fn test_repeat_forwards_copies_topics_and_excludes_commands() {
        let tmp = std::env::temp_dir().join(format!(
            "mibot-re-test-{}-{}",
            std::process::id(),
            mibot_bot::random_id().unsigned_abs()
        ));
        let _ = std::fs::create_dir_all(&tmp);

        let f1 = chat(false);
        handle(&tmp, &invocation(&f1, false, true, &["3", "2"]))
            .await
            .unwrap();
        assert_eq!(
            *f1.calls.lock().unwrap(),
            vec![
                "history offset=12 add=-3 limit=3",
                "forward [12, 13] top=7",
                "forward [12, 13] top=7",
                "delete [100]",
            ]
        );

        let f2 = chat(true);
        handle(&tmp, &invocation(&f2, false, true, &["2", "2"]))
            .await
            .unwrap();
        assert_eq!(
            *f2.calls.lock().unwrap(),
            vec![
                "history offset=12 add=-2 limit=2",
                "forward [12, 13] top=7",
                r#"send "第12条" top=7"#,
                r#"send "第13条" top=7"#,
                r#"send "第12条" top=7"#,
                r#"send "第13条" top=7"#,
                "delete [100]",
            ]
        );

        let f3 = chat(false);
        handle(&tmp, &invocation(&f3, false, false, &["1"]))
            .await
            .unwrap();
        assert_eq!(
            *f3.calls.lock().unwrap(),
            vec![
                "history offset=12 add=-1 limit=1",
                "forward [12] top=0",
                "delete [100]",
            ]
        );

        let f4 = chat(false);
        {
            let mut map = f4.messages.lock().unwrap();
            for id in [99, 100] {
                let mut m =
                    kit::blank_tl_message(id, tl::types::PeerChannel { channel_id: GROUP }.into());
                m.message = ".re 20".into();
                map.insert(id, tl::enums::Message::Message(m));
            }
        }
        handle(&tmp, &invocation(&f4, true, false, &["20"]))
            .await
            .unwrap();
        assert!(
            f4.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == "forward [12, 13, 15, 20] top=0")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
