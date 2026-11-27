use grammers_tl_types as tl;
use mibot_bot::{
    Client, SendOptions, escape, first_sent_message_id, message_text, observe_sent,
    without_ip_privacy,
};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy)]
pub struct Case {
    pub command: &'static str,
    pub expect: &'static str,
    pub network: bool,
    pub wait: Duration,
    pub reply: bool,
}

impl Case {
    pub const fn new(command: &'static str, expect: &'static str) -> Self {
        Self {
            command,
            expect,
            network: false,
            wait: Duration::ZERO,
            reply: false,
        }
    }
}

pub const CASES: &[Case] = &[
    Case::new("ping", "Message edit"),
    Case::new("version", "MiBot Lite"),
    Case::new("memory", "RSS"),
    Case {
        command: "status",
        expect: "Uptime",
        network: false,
        wait: Duration::ZERO,
        reply: true,
    },
    Case::new("sysinfo", "System Info"),
    Case::new("help", "Command List"),
    Case::new("help calc", "Calculator"),
    Case::new("calc 2+2*5", "12"),
    Case::new("calc (10-3)*4", "28"),
    Case::new("update", "Self-Update"),
    Case::new("acn status", "Auto-update"),
    Case::new("yvlu config", "Sticker pack"),
    Case::new("ai config list", "AI"),
    Case::new("sum config list", "Summary AI Config"),
    Case::new("aban", "Group Moderation"),
    Case::new("log debug", "Current log level"),
    Case::new("help log", "Runtime Logs"),
    Case::new("help bf", "Config Backup"),
    Case::new("alias", "Alias"),
    Case::new("prefix", "Current prefixes"),
    Case::new("sudo ls", "user"),
    Case::new("sure msg ls", "rule"),
    Case::new("sticker", "Save Stickers"),
    Case::new("privacy", "IP Privacy"),
    Case::new("ts", "Voices"),
    Case::new("help save", "Save Messages"),
    Case::new("help bin", "BIN Lookup"),
    Case::new("ids", "Registered"),
    Case::new("dc", "Datacenter"),
    Case {
        command: "ip 8.8.8.8",
        expect: "Google",
        network: true,
        wait: Duration::from_secs(30),
        reply: false,
    },
    Case {
        command: "ping example.com",
        expect: "HTTP",
        network: true,
        wait: Duration::from_secs(30),
        reply: false,
    },
    Case {
        command: "eatgif list",
        expect: "Avatar Animated Stickers",
        network: true,
        wait: Duration::from_secs(60),
        reply: false,
    },
    Case {
        command: "eat",
        expect: "Avatar Meme Stickers",
        network: true,
        wait: Duration::from_secs(60),
        reply: false,
    },
    Case {
        command: "whois example.com",
        expect: "example.com",
        network: true,
        wait: Duration::from_secs(60),
        reply: false,
    },
    Case {
        command: "rate BTC",
        expect: "updated",
        network: true,
        wait: Duration::from_secs(90),
        reply: false,
    },
];

#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub case: Case,
    pub passed: bool,
    pub skipped: bool,
    pub detail: String,
    pub took: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    Cancelled,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("verification cancelled"),
        }
    }
}

impl std::error::Error for VerifyError {}

const DEFAULT_WAIT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(700);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Sends each case to Saved Messages, dispatches it locally, polls for the edited/replied message,
/// deletes sent and reply messages afterward, and returns the count of non-third-party failures.
pub async fn run_verification<F, W>(
    cancel: &CancellationToken,
    client: &Client,
    prefix: &str,
    out: &mut W,
    cases: &[Case],
    mut dispatch: F,
) -> Result<usize, VerifyError>
where
    F: FnMut(CancellationToken, tl::types::Message) -> bool,
    W: Write,
{
    let self_peer = tl::enums::InputPeer::PeerSelf;
    let _ = writeln!(
        out,
        "verifying {} commands as {}, in Saved Messages\n",
        cases.len(),
        client.peers().self_id()
    );
    let mut results = Vec::with_capacity(cases.len());

    for &item in cases {
        if cancel.is_cancelled() {
            return Err(VerifyError::Cancelled);
        }
        let result = run_case(cancel, client, &self_peer, prefix, item, &mut dispatch).await;
        let status = if result.passed {
            "pass"
        } else if result.skipped {
            "warn"
        } else {
            "FAIL"
        };
        let _ = writeln!(
            out,
            "{status:<4}  {:<22}  {:>6}ms  {}",
            format!("{prefix}{}", item.command),
            result.took.as_millis(),
            result.detail
        );
        results.push(result);
    }

    let (mut passed, mut failed, mut warned) = (0usize, 0usize, 0usize);
    for res in &results {
        if res.passed {
            passed += 1;
        } else if res.skipped {
            warned += 1;
        } else {
            failed += 1;
        }
    }
    let memory = mibot_sysinfo::read_memory();
    let _ = writeln!(
        out,
        "\n{passed} passed, {failed} failed, {warned} third-party warnings"
    );
    let _ = writeln!(
        out,
        "after the run: RSS {:.1} MB, heap {:.1} MB, {} tasks",
        mibot_sysinfo::megabytes(memory.rss),
        mibot_sysinfo::megabytes(memory.heap_alloc),
        memory.tasks
    );
    Ok(failed)
}

async fn run_case<F>(
    cancel: &CancellationToken,
    client: &Client,
    peer: &tl::enums::InputPeer,
    prefix: &str,
    item: Case,
    dispatch: &mut F,
) -> VerifyResult
where
    F: FnMut(CancellationToken, tl::types::Message) -> bool,
{
    let wait = if item.wait.is_zero() {
        DEFAULT_WAIT
    } else {
        item.wait
    };
    let started = Instant::now();
    let text = format!("{prefix}{}", item.command);

    let escaped = escape(&text);
    let send_res = without_ip_privacy(|| {
        client.send_html(peer, &escaped, SendOptions::default())
    })
    .await;
    let updates = match send_res {
        Ok(u) => u,
        Err(e) => {
            return VerifyResult {
                case: item,
                passed: false,
                skipped: false,
                detail: format!("send failed: {e}"),
                took: started.elapsed(),
            };
        }
    };
    let Some(id) = first_sent_message_id(&updates) else {
        return VerifyResult {
            case: item,
            passed: false,
            skipped: false,
            detail: "send succeeded without message id".into(),
            took: started.elapsed(),
        };
    };

    let mut cleanup_ids = vec![id];
    let result = poll_case_outcome(
        cancel,
        client,
        peer,
        &text,
        item,
        id,
        wait,
        started,
        &mut cleanup_ids,
        dispatch,
    )
    .await;

    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, client.delete_messages(peer, &cleanup_ids)).await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn poll_case_outcome<F>(
    cancel: &CancellationToken,
    client: &Client,
    peer: &tl::enums::InputPeer,
    text: &str,
    item: Case,
    id: i32,
    wait: Duration,
    started: Instant,
    cleanup_ids: &mut Vec<i32>,
    dispatch: &mut F,
) -> VerifyResult
where
    F: FnMut(CancellationToken, tl::types::Message) -> bool,
{
    let sent = match client.get_messages(peer, &[id]).await {
        Ok(msgs) if !msgs.is_empty() => msgs,
        _ => {
            return VerifyResult {
                case: item,
                passed: false,
                skipped: false,
                detail: "could not read the sent message back".into(),
                took: started.elapsed(),
            };
        }
    };

    let poll_cancel = cancel.child_token();
    let deadline = tokio::time::Instant::now() + wait;
    let self_id = client.peers().self_id();
    let (reply_tx, mut reply_rx) = mpsc::channel::<i32>(1);

    let dispatched = if item.reply {
        let tx = reply_tx.clone();
        let observer: mibot_bot::sent::SentCallback = Arc::new(move |p, reply_id| {
            if let tl::enums::Peer::User(u) = p
                && (self_id == 0 || u.user_id == self_id)
            {
                let _ = tx.try_send(reply_id);
            }
        });
        observe_sent(observer, || async {
            dispatch(poll_cancel.clone(), sent[0].clone())
        })
        .await
    } else {
        dispatch(poll_cancel.clone(), sent[0].clone())
    };

    if !dispatched {
        return VerifyResult {
            case: item,
            passed: false,
            skipped: false,
            detail: format!("no command matched {text}"),
            took: started.elapsed(),
        };
    }

    let mut answer = String::new();
    let mut matched = false;
    let mut result_id = id;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() || poll_cancel.is_cancelled() {
            break;
        }
        tokio::select! {
            _ = poll_cancel.cancelled() => break,
            _ = tokio::time::sleep(POLL_INTERVAL.min(remaining)) => {}
        }
        if let Ok(reply_id) = reply_rx.try_recv() {
            result_id = reply_id;
            if !cleanup_ids.contains(&reply_id) {
                cleanup_ids.push(reply_id);
            }
        }
        let Ok(found) = client.get_messages(peer, &[result_id]).await else {
            continue;
        };
        let Some(first) = found.first() else {
            continue;
        };
        let current = message_text(Some(first));
        if current == text || current.trim().is_empty() {
            continue;
        }
        answer = current.clone();
        if current
            .to_ascii_lowercase()
            .contains(&item.expect.to_ascii_lowercase())
        {
            matched = true;
            break;
        }
    }

    let took = started.elapsed();
    if answer.is_empty() {
        return VerifyResult {
            case: item,
            passed: false,
            skipped: false,
            detail: format!("no answer within {wait:?}"),
            took,
        };
    }
    let snippet = first_line(&answer).replace('\n', " ");
    if matched {
        return VerifyResult {
            case: item,
            passed: true,
            skipped: false,
            detail: snippet,
            took,
        };
    }
    if item.network {
        return VerifyResult {
            case: item,
            passed: false,
            skipped: true,
            detail: format!("third party: {snippet}"),
            took,
        };
    }
    VerifyResult {
        case: item,
        passed: false,
        skipped: false,
        detail: format!("expected {:?}, got {:?}", item.expect, snippet),
        took,
    }
}

pub fn first_line(text: &str) -> String {
    let line = match text.find('\n') {
        Some(idx) => &text[..idx],
        None => text,
    };
    let mut chars = line.char_indices();
    if let Some((idx, _)) = chars.nth(60) {
        format!("{}…", &line[..idx])
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::InvocationError;
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, PeerCache, RpcTransport, SentRecorder, envelope};
    use mibot_command::{Command, Registry, group};
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    struct FakeChatTransport {
        self_id: i64,
        next_id: Mutex<i32>,
        messages: Mutex<HashMap<i32, tl::types::Message>>,
        deleted: Mutex<Vec<i32>>,
    }

    impl FakeChatTransport {
        fn new(self_id: i64) -> Self {
            Self {
                self_id,
                next_id: Mutex::new(100),
                messages: Mutex::new(HashMap::new()),
                deleted: Mutex::new(Vec::new()),
            }
        }

        fn make_msg(&self, id: i32, text: String) -> tl::types::Message {
            let peer: tl::enums::Peer = tl::types::PeerUser {
                user_id: self_id_from(self.self_id),
            }
            .into();
            tl::types::Message {
                out: true,
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
                peer_id: peer,
                saved_peer_id: None,
                fwd_from: None,
                via_bot_id: None,
                via_business_bot_id: None,
                guestchat_via_from: None,
                reply_to: None,
                date: 1_800_000_000,
                message: text,
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
            }
        }
    }

    fn self_id_from(v: i64) -> i64 {
        v
    }

    impl RpcTransport for FakeChatTransport {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                let payload = body.get(4..).unwrap_or(&[]);
                match constructor_id {
                    tl::functions::messages::SendMessage::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::SendMessage::from_bytes(payload)
                            .map_err(|_| InvocationError::Dropped("bad SendMessage"))?;
                        let mut nid = self.next_id.lock().await;
                        *nid += 1;
                        let id = *nid;
                        self.messages
                            .lock()
                            .await
                            .insert(id, self.make_msg(id, req.message));
                        let upd = tl::enums::Updates::UpdateShortSentMessage(
                            tl::types::UpdateShortSentMessage {
                                out: true,
                                id,
                                pts: id,
                                pts_count: 1,
                                date: 100,
                                media: None,
                                entities: None,
                                ttl_period: None,
                            },
                        );
                        Ok(upd.to_bytes())
                    }
                    tl::functions::messages::EditMessage::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::EditMessage::from_bytes(payload)
                            .map_err(|_| InvocationError::Dropped("bad EditMessage"))?;
                        if let Some(msg) = req.message
                            && let Some(entry) = self.messages.lock().await.get_mut(&req.id)
                        {
                            entry.message = msg;
                        }
                        Ok(tl::enums::Updates::TooLong.to_bytes())
                    }
                    tl::functions::messages::GetMessages::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::GetMessages::from_bytes(payload)
                            .map_err(|_| InvocationError::Dropped("bad GetMessages"))?;
                        let map = self.messages.lock().await;
                        let mut out = Vec::new();
                        for entry in req.id {
                            if let tl::enums::InputMessage::Id(id) = entry
                                && let Some(msg) = map.get(&id.id)
                            {
                                out.push(msg.clone().into());
                            }
                        }
                        let resp: tl::enums::messages::Messages = tl::types::messages::Messages {
                            messages: out,
                            topics: vec![],
                            chats: vec![],
                            users: vec![],
                        }
                        .into();
                        Ok(resp.to_bytes())
                    }
                    tl::functions::messages::DeleteMessages::CONSTRUCTOR_ID => {
                        let req = tl::functions::messages::DeleteMessages::from_bytes(payload)
                            .map_err(|_| InvocationError::Dropped("bad DeleteMessages"))?;
                        let mut map = self.messages.lock().await;
                        let mut del = self.deleted.lock().await;
                        for id in req.id {
                            map.remove(&id);
                            del.push(id);
                        }
                        let affected: tl::enums::messages::AffectedMessages =
                            tl::types::messages::AffectedMessages { pts: 1, pts_count: 1 }.into();
                        Ok(affected.to_bytes())
                    }
                    _ => Err(InvocationError::Dropped("unexpected RPC")),
                }
            })
        }
    }

    #[test]
    fn default_cases_has_35_entries() {
        assert_eq!(CASES.len(), 35);
    }

    #[tokio::test]
    async fn run_verification_checks_edit_reply_network_warn_fail_and_cleanup() {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(7);
        let sent = Arc::new(SentRecorder::default());
        let transport = Arc::new(FakeChatTransport::new(7));
        let client = Arc::new(Client::from_transport(transport.clone(), peers.clone(), sent));

        let reg = Arc::new(Registry::new());
        reg.bind_client(Arc::clone(&client));
        reg.register(Command {
            name: "ping".into(),
            aliases: vec![],
            group: group::GENERAL.into(),
            description: "ping".into(),
            usage: "".into(),
            help: "".into(),
            handle: Arc::new(|inv| {
                Box::pin(async move {
                    inv.respond("Message edit: 1ms").await?;
                    Ok(())
                })
            }),
            ..Default::default()
        });
        reg.register(Command {
            name: "status".into(),
            aliases: vec![],
            group: group::STATUS.into(),
            description: "status".into(),
            usage: "".into(),
            help: "".into(),
            handle: Arc::new(|inv| {
                Box::pin(async move {
                    inv.reply("Uptime 10m").await?;
                    Ok(())
                })
            }),
            ..Default::default()
        });
        reg.register(Command {
            name: "ip".into(),
            aliases: vec![],
            group: group::TOOLS.into(),
            description: "ip".into(),
            usage: "".into(),
            help: "".into(),
            handle: Arc::new(|inv| {
                Box::pin(async move {
                    // Verify that IP argument wasn't masked when sent by verify!
                    assert_eq!(inv.args, vec!["8.8.8.8"]);
                    inv.respond("upstream error").await?;
                    Ok(())
                })
            }),
            ..Default::default()
        });
        reg.register(Command {
            name: "broken".into(),
            aliases: vec![],
            group: group::TOOLS.into(),
            description: "broken".into(),
            usage: "".into(),
            help: "".into(),
            handle: Arc::new(|inv| {
                Box::pin(async move {
                    inv.respond("wrong output").await?;
                    Ok(())
                })
            }),
            ..Default::default()
        });

        let cases = [
            Case::new("ping", "Message edit"),
            Case {
                command: "status",
                expect: "Uptime",
                network: false,
                wait: Duration::from_secs(3),
                reply: true,
            },
            Case {
                command: "ip 8.8.8.8",
                expect: "Google",
                network: true,
                wait: Duration::from_millis(850),
                reply: false,
            },
            Case {
                command: "broken",
                expect: "ok",
                network: false,
                wait: Duration::from_millis(850),
                reply: false,
            },
        ];

        let mut report = Vec::new();
        let failed = run_verification(&CancellationToken::new(), &client, ".", &mut report, &cases, |tok, raw| {
            let Some(env) = envelope(&raw, 7, false, &peers) else {
                return false;
            };
            matches!(
                reg.dispatch(env, tok),
                mibot_command::DispatchOutcome::Queued
            )
        })
        .await
        .unwrap();

        let text = String::from_utf8(report).unwrap();
        assert_eq!(failed, 1, "report:\n{text}");
        assert!(text.contains("pass  .ping"));
        assert!(text.contains("pass  .status"));
        assert!(text.contains("warn  .ip 8.8.8.8"));
        assert!(text.contains("FAIL  .broken"));
        assert!(text.contains("2 passed, 1 failed, 1 third-party warnings"));

        // Verify sent messages AND .status reply message were all deleted cleanly!
        assert!(transport.messages.lock().await.is_empty());
        let deleted = transport.deleted.lock().await;
        // 4 sent commands + 1 reply from .status = 5 deleted IDs.
        assert_eq!(deleted.len(), 5);
    }
}
