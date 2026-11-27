use crate::kit;
use chrono::Utc;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BotError, Client, SendOptions, peer_from_id};
use mibot_command::{Invocation, code, escape, group};
use mibot_config::RESTART_RECEIPT_FILE;
use mibot_logtail::Level;
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    #[serde(rename = "messageId")]
    pub message_id: i32,
    #[serde(rename = "requestedAt")]
    pub requested_at: i64,
    #[serde(rename = "bootId")]
    pub boot_id: String,
    pub kind: String,
    #[serde(rename = "peerType", default, skip_serializing_if = "String::is_empty")]
    pub peer_type: String,
    #[serde(rename = "accessHash", default, skip_serializing_if = "is_zero")]
    pub access_hash: i64,
    #[serde(rename = "topicId", default, skip_serializing_if = "is_zero_i32")]
    pub topic_id: i32,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReceiptDocument {
    pub pending: Option<Receipt>,
}

#[derive(Debug, Error)]
pub enum ScheduleError {
    #[error("重启请求已提交，请稍候")]
    Pending,
    #[error("systemctl restart: {0}")]
    Trigger(String),
    #[error("{0}")]
    Other(String),
}

pub struct Restarter {
    a: Arc<App>,
    pub service: String,
    pub store: Store<ReceiptDocument>,
    pending: Mutex<bool>,
}

struct ReceiptTarget<'a> {
    peer: Option<&'a tl::enums::InputPeer>,
    chat_id: &'a str,
    message_id: i32,
    topic_id: i32,
}

fn systemctl_path() -> &'static str {
    if Path::new("/usr/bin/systemctl").exists() {
        "/usr/bin/systemctl"
    } else {
        "systemctl"
    }
}

pub fn receipt_peer(peer: Option<&tl::enums::InputPeer>) -> (String, i64) {
    match peer {
        Some(tl::enums::InputPeer::PeerSelf) => ("self".into(), 0),
        Some(tl::enums::InputPeer::User(u)) => ("user".into(), u.access_hash),
        Some(tl::enums::InputPeer::Chat(_)) => ("chat".into(), 0),
        Some(tl::enums::InputPeer::Channel(c)) => ("channel".into(), c.access_hash),
        _ => (String::new(), 0),
    }
}

pub fn receipt_input_peer(note: &Receipt, self_id: i64) -> Option<tl::enums::InputPeer> {
    let peer = peer_from_id(&note.chat_id)?;
    match note.peer_type.as_str() {
        "self" => match peer {
            tl::enums::Peer::User(u) if u.user_id == self_id => Some(tl::enums::InputPeer::PeerSelf),
            _ => None,
        },
        "user" => match peer {
            tl::enums::Peer::User(u) if note.access_hash != 0 => Some(
                tl::types::InputPeerUser {
                    user_id: u.user_id,
                    access_hash: note.access_hash,
                }
                .into(),
            ),
            _ => None,
        },
        "chat" => match peer {
            tl::enums::Peer::Chat(c) => Some(tl::types::InputPeerChat { chat_id: c.chat_id }.into()),
            _ => None,
        },
        "channel" => match peer {
            tl::enums::Peer::Channel(c) if note.access_hash != 0 => Some(
                tl::types::InputPeerChannel {
                    channel_id: c.channel_id,
                    access_hash: note.access_hash,
                }
                .into(),
            ),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Debug)]
enum TriggerFailure {
    Interrupted,
    Timeout(String),
    Exit(String),
}

async fn default_trigger_restart(cancel: CancellationToken, service: String) -> Result<(), TriggerFailure> {
    if cancel.is_cancelled() {
        return Err(TriggerFailure::Interrupted);
    }
    let bin = systemctl_path();
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(bin)
            .args(["--no-block", "restart", &service])
            .status(),
    )
    .await;

    if cancel.is_cancelled() {
        return Err(TriggerFailure::Interrupted);
    }
    match res {
        Err(_) => Err(TriggerFailure::Timeout("systemctl 5 秒没有返回".into())),
        Ok(Err(e)) => Err(TriggerFailure::Exit(e.to_string())),
        Ok(Ok(st)) if st.success() => Ok(()),
        Ok(Ok(st)) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                if st.signal().is_some() {
                    return Err(TriggerFailure::Interrupted);
                }
            }
            Err(TriggerFailure::Exit(format!("exit status {st}")))
        }
    }
}

impl Restarter {
    pub fn new(a: Arc<App>) -> Arc<Self> {
        let service = a.env.get("MIBOT_SERVICE", "mibot-lite.service").to_owned();
        let store = Store::new(a.root.join(RESTART_RECEIPT_FILE), ReceiptDocument::default);
        Arc::new(Self {
            a,
            service,
            store,
            pending: Mutex::new(false),
        })
    }

    pub async fn now(
        &self,
        inv: &Invocation,
        kind: &str,
        progress: &str,
        failure: &str,
    ) -> mibot_command::CmdResult {
        self.command(inv, kind, progress, failure).await
    }

    pub async fn schedule(
        &self,
        cancel: &CancellationToken,
        peer: Option<&tl::enums::InputPeer>,
        chat_id: &str,
        message_id: i32,
        topic_id: i32,
        kind: &str,
    ) -> Result<(), ScheduleError> {
        let target = ReceiptTarget {
            peer,
            chat_id,
            message_id,
            topic_id,
        };
        self.schedule_with(cancel, &target, kind, |c, svc| {
            Box::pin(default_trigger_restart(c, svc))
        })
        .await
    }

    async fn schedule_with<F, Fut>(
        &self,
        cancel: &CancellationToken,
        target: &ReceiptTarget<'_>,
        kind: &str,
        trigger: F,
    ) -> Result<(), ScheduleError>
    where
        F: FnOnce(CancellationToken, String) -> Fut,
        Fut: Future<Output = Result<(), TriggerFailure>>,
    {
        {
            let mut g = self.pending.lock().await;
            if *g {
                return Err(ScheduleError::Pending);
            }
            *g = true;
        }
        let release = || async {
            let mut g = self.pending.lock().await;
            *g = false;
        };

        let (peer_type, access_hash) = receipt_peer(target.peer);
        let note = Receipt {
            chat_id: target.chat_id.to_owned(),
            message_id: target.message_id,
            requested_at: Utc::now().timestamp_millis(),
            boot_id: self.a.boot_id.clone(),
            kind: kind.to_owned(),
            peer_type,
            access_hash,
            topic_id: target.topic_id,
        };
        if let Err(err) = self.store.update(|doc| {
            doc.pending = Some(note);
            Ok::<(), std::convert::Infallible>(())
        }) {
            release().await;
            return Err(ScheduleError::Other(err.to_string()));
        }

        match trigger(cancel.clone(), self.service.clone()).await {
            Ok(()) | Err(TriggerFailure::Interrupted) => Ok(()),
            Err(TriggerFailure::Timeout(msg)) | Err(TriggerFailure::Exit(msg)) => {
                release().await;
                if let Err(err) = self.store.update(|doc| {
                    doc.pending = None;
                    Ok::<(), std::convert::Infallible>(())
                }) {
                    kit::warn(&self.a, "restart.receipt_clear_failed", &err);
                }
                Err(ScheduleError::Trigger(msg))
            }
        }
    }

    pub async fn status(&self) -> String {
        format!("{}\n{}", self.status_rows().await, escape(&owner_hint()))
    }

    async fn status_rows(&self) -> String {
        let bin = systemctl_path();
        let mut rows = Vec::with_capacity(4);
        for field in ["LoadState", "ActiveState", "SubState", "FragmentPath"] {
            let out = tokio::time::timeout(
                Duration::from_millis(1500),
                tokio::process::Command::new(bin)
                    .args(["show", "--value", "-p", field, &self.service])
                    .output(),
            )
            .await;
            let value = match out {
                Ok(Ok(o)) if o.status.success() => {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                    if s.is_empty() { "unknown".into() } else { s }
                }
                _ => "unavailable".into(),
            };
            rows.push(format!("{}: {}", escape(field), escape(&value)));
        }
        rows.join("\n")
    }

    async fn command(
        &self,
        inv: &Invocation,
        kind: &str,
        progress: &str,
        failure: &str,
    ) -> mibot_command::CmdResult {
        inv.edit(progress).await?;
        let peer = inv.client.input_peer(&inv.message.peer).ok();
        match self
            .schedule(
                &inv.cancel,
                peer.as_ref(),
                &inv.message.chat_id,
                inv.message.id,
                inv.message.topic_id,
                kind,
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(ScheduleError::Pending) => inv
                .edit_text("重启请求已提交，请稍候")
                .await
                .map_err(Into::into),
            Err(ScheduleError::Trigger(_)) => {
                let st = self.status().await;
                let msg = format!(
                    "{}\n状态：\n{st}\n\n可执行 {} 查看详情。",
                    escape(failure),
                    code(&format!("systemctl status {} --no-pager", self.service))
                );
                inv.edit(&msg).await.map_err(Into::into)
            }
            Err(ScheduleError::Other(e)) => Err(kit::fail_err(e)),
        }
    }

    pub async fn notify_ready(&self, client: &Client) {
        let Ok(doc) = self.store.read() else {
            return;
        };
        let Some(note) = doc.pending else {
            return;
        };
        if note.boot_id == self.a.boot_id {
            return;
        }
        let forget = || {
            if let Err(err) = self.store.update(|d| {
                if let Some(p) = &d.pending
                    && p.boot_id == note.boot_id
                    && p.requested_at == note.requested_at
                {
                    d.pending = None;
                }
                Ok::<(), std::convert::Infallible>(())
            }) {
                kit::warn(&self.a, "restart.receipt_clear_failed", &err);
            }
        };

        let age = Utc::now().timestamp_millis() - note.requested_at;
        if !kit::is_numeric_id(&note.chat_id) || note.message_id <= 0 || !(0..=600_000).contains(&age) {
            forget();
            return;
        }
        let peer = match receipt_input_peer(&note, client.self_id()) {
            Some(p) => p,
            None => match client.input_peer_from_chat_id(&note.chat_id) {
                Ok(p) => p,
                Err(_) => {
                    self.a.logs.record(
                        Level::Warn,
                        "restart.receipt_unaddressable",
                        &[("chat", &note.chat_id)],
                    );
                    forget();
                    return;
                }
            },
        };

        let text = match note.kind.as_str() {
            "update" => "⬆️ <b>程序更新</b>\n✅ 更新完成，新版本已就绪".to_owned(),
            "rollback" => "⬆️ <b>程序更新</b>\n✅ 回滚完成，已换回上一版本".to_owned(),
            "auto-update" => format!(
                "⬆️ <b>自动更新</b>\n✅ 已更新到 {}",
                code(kit::version(&self.a))
            ),
            _ => "🔄 <b>重启服务</b>\n✅ 重启成功，服务已就绪".to_owned(),
        };

        if let Err(err) = client
            .edit_message_html(&peer, note.message_id, &text, false)
            .await
        {
            let err_s = err.to_string();
            self.a.logs.record(
                Level::Warn,
                "restart.receipt_failed",
                &[("error", &err_s)],
            );
            if edit_rejected(&err)
                && let Err(send_err) = client
                    .send_html(
                        &peer,
                        &text,
                        SendOptions {
                            topic_id: Some(note.topic_id).filter(|&id| id > 0),
                            no_webpage: true,
                            ..Default::default()
                        },
                    )
                    .await
            {
                let se_s = send_err.to_string();
                self.a.logs.record(
                    Level::Warn,
                    "restart.receipt_send_failed",
                    &[("error", &se_s)],
                );
            }
        }
        forget();
    }
}

fn edit_rejected(err: &BotError) -> bool {
    !err.is_uncertain() && err.rpc_error().is_some_and(|rpc| rpc.code < 500)
}

fn owner_hint() -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if let Ok(meta) = std::fs::metadata("/proc/self") {
            let uid = meta.uid();
            return if uid == 0 {
                "当前运行在 root 用户。".to_owned()
            } else {
                format!("当前运行用户 UID={uid}，通常需要 root 或 systemd 管理权限。")
            };
        }
    }
    "当前环境无 UID 查询。".to_owned()
}

fn restart_help(prefix: &str) -> String {
    format!(
        "🔄 <b>重启服务</b>\n\n{} 通过 systemd 重启服务。正在跑的命令会被中断，重启前发的命令重启后也不会补跑；\
         重启完成后这条消息会改成「重启成功」。",
        code(&format!("{prefix}restart"))
    )
}

pub fn register(a: &Arc<App>) -> Arc<Restarter> {
    let r = Restarter::new(Arc::clone(a));
    let r_cmd = Arc::clone(&r);
    a.registry.register(kit::make_cmd(
        "restart",
        &[],
        group::SYSTEM,
        "重启 systemd 服务",
        "",
        restart_help,
        move |inv: Invocation| {
            let r = Arc::clone(&r_cmd);
            Box::pin(async move {
                if !inv.args.is_empty() {
                    return Err(kit::usage(
                        inv.prefix(),
                        "restart（不带参数）重启服务，重启完成后这条消息会改成「重启成功」",
                    ));
                }
                r.command(
                    &inv,
                    "restart",
                    "🔄 <b>重启服务</b>\n⏳ 正在提交重启请求…",
                    "服务重启命令执行失败。",
                )
                .await
            })
        },
    ));
    let r_ready = Arc::clone(&r);
    a.on_ready(Arc::new(move |app, _cancel| {
        let r = Arc::clone(&r_ready);
        Box::pin(async move {
            r.notify_ready(&app.client).await;
        })
    }));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Serializable};
    use mibot_bot::{BoxFuture, PeerCache, RpcTransport, SentRecorder};
    use mibot_logtail::Ring;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::UNIX_EPOCH;

    struct DummyTransport;
    impl RpcTransport for DummyTransport {
        fn invoke_raw<'a>(&'a self, _: Option<i32>, _: u32, _: Vec<u8>) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async { Err(InvocationError::Dropped("dummy")) })
        }
    }

    fn mk_app(boot_id: &str) -> (Arc<App>, std::path::PathBuf) {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mibot-restart-{}-{}",
            std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let logs = Arc::new(Ring::new());
        let peers = Arc::new(PeerCache::new());
        let sent = Arc::new(SentRecorder::new(Arc::new(|_, _| {})));
        let client = Arc::new(Client::from_transport(Arc::new(DummyTransport), peers.clone(), sent));
        let app = App::for_testing(dir.clone(), "0.3.11", boot_id, client, peers, logs);
        (app, dir)
    }

    #[tokio::test]
    async fn test_schedule_keeps_receipt_when_interrupted() {
        for (name, outcome, canceled, keep) in [
            ("ok", Ok(()), false, true),
            ("sigterm", Err(TriggerFailure::Interrupted), false, true),
            ("ctx", Err(TriggerFailure::Exit("canceled".into())), true, true),
            ("exit1", Err(TriggerFailure::Exit("exit 1".into())), false, false),
            ("timeout", Err(TriggerFailure::Timeout("timed out".into())), false, false),
        ] {
            let (app, dir) = mk_app("old");
            let r = Restarter::new(app);
            let tok = CancellationToken::new();
            if canceled {
                tok.cancel();
            }
            let res = r
                .schedule_with(
                    &tok,
                    &ReceiptTarget {
                        peer: None,
                        chat_id: "812473405",
                        message_id: 309529,
                        topic_id: 0,
                    },
                    "update",
                    |c, _| {
                        let out = outcome;
                        Box::pin(async move {
                            if c.is_cancelled() {
                                return Err(TriggerFailure::Interrupted);
                            }
                            out
                        })
                    },
                )
                .await;
            let doc = r.store.read().unwrap();
            if keep {
                assert!(res.is_ok(), "{name}: {res:?}");
                let p = doc.pending.as_ref().unwrap();
                assert_eq!(p.message_id, 309529);
                assert_eq!(p.kind, "update");
                assert!(matches!(
                    r.schedule_with(
                        &CancellationToken::new(),
                        &ReceiptTarget {
                            peer: None,
                            chat_id: "1",
                            message_id: 1,
                            topic_id: 0,
                        },
                        "restart",
                        |_, _| Box::pin(async { Ok(()) })
                    )
                    .await,
                    Err(ScheduleError::Pending)
                ));
            } else {
                assert!(matches!(res, Err(ScheduleError::Trigger(_))), "{name}");
                assert!(doc.pending.is_none(), "{name}");
                assert!(
                    !matches!(
                        r.schedule_with(
                            &CancellationToken::new(),
                            &ReceiptTarget {
                                peer: None,
                                chat_id: "1",
                                message_id: 1,
                                topic_id: 0,
                            },
                            "restart",
                            |_, _| Box::pin(async { Ok(()) })
                        )
                        .await,
                        Err(ScheduleError::Pending)
                    ),
                    "{name}"
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn test_schedule_stores_peer() {
        let cases: &[(&str, Option<tl::enums::InputPeer>, &str, i64)] = &[
            ("self", Some(tl::enums::InputPeer::PeerSelf), "self", 0),
            ("user", Some(tl::types::InputPeerUser { user_id: 812473405, access_hash: 7 }.into()), "user", 7),
            ("chat", Some(tl::types::InputPeerChat { chat_id: 99 }.into()), "chat", 0),
            ("channel", Some(tl::types::InputPeerChannel { channel_id: 55, access_hash: 11 }.into()), "channel", 11),
            ("none", None, "", 0),
        ];
        for (name, peer, wt, wh) in cases {
            let (app, dir) = mk_app("old");
            let r = Restarter::new(app);
            r.schedule_with(
                &CancellationToken::new(),
                &ReceiptTarget {
                    peer: peer.as_ref(),
                    chat_id: "812473405",
                    message_id: 309529,
                    topic_id: 42,
                },
                "restart",
                |_, _| Box::pin(async { Ok(()) }),
            )
            .await
            .unwrap();
            let p = r.store.read().unwrap().pending.unwrap();
            assert_eq!(p.peer_type, *wt, "{name}");
            assert_eq!(p.access_hash, *wh, "{name}");
            assert_eq!(p.topic_id, 42, "{name}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    struct EditRecorder {
        calls: tokio::sync::Mutex<Vec<(u32, Vec<u8>)>>,
        replies: tokio::sync::Mutex<VecDeque<Result<Vec<u8>, InvocationError>>>,
    }
    impl RpcTransport for EditRecorder {
        fn invoke_raw<'a>(&'a self, _: Option<i32>, cid: u32, body: Vec<u8>) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                self.calls.lock().await.push((cid, body));
                self.replies.lock().await.pop_front().unwrap()
            })
        }
    }

    #[tokio::test]
    async fn test_notify_ready_edit_and_fallback_send() {
        let ok_updates: tl::enums::Updates = tl::types::Updates {
            updates: vec![],
            users: vec![],
            chats: vec![],
            date: 1,
            seq: 1,
        }
        .into();

        // 1. Direct edit using saved access_hash
        let rec = Arc::new(EditRecorder {
            calls: tokio::sync::Mutex::new(vec![]),
            replies: tokio::sync::Mutex::new(VecDeque::from([Ok(ok_updates.to_bytes())])),
        });
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Client::from_transport(rec.clone(), peers, Arc::new(SentRecorder::new(Arc::new(|_, _| {}))));
        let (app, dir) = mk_app("new");
        let r = Restarter::new(app);
        r.store.update(|d| {
            d.pending = Some(Receipt {
                chat_id: "812473405".into(),
                message_id: 309529,
                requested_at: Utc::now().timestamp_millis(),
                boot_id: "old".into(),
                kind: "update".into(),
                peer_type: "user".into(),
                access_hash: 7,
                topic_id: 0,
            });
            Ok::<(), std::convert::Infallible>(())
        }).unwrap();
        r.notify_ready(&client).await;
        let calls = rec.calls.lock().await;
        assert_eq!(calls.len(), 1);
        let req = tl::functions::messages::EditMessage::from_bytes(&calls[0].1[4..]).unwrap();
        assert_eq!(req.id, 309529);
        assert!(matches!(req.peer, tl::enums::InputPeer::User(ref u) if u.user_id == 812473405 && u.access_hash == 7));
        assert!(r.store.read().unwrap().pending.is_none());
        drop(calls);

        // 2. Edit refused with 400 -> fallback SendMessage
        let rec2 = Arc::new(EditRecorder {
            calls: tokio::sync::Mutex::new(vec![]),
            replies: tokio::sync::Mutex::new(VecDeque::from([
                Err(InvocationError::Rpc(RpcError {
                    code: 400,
                    name: "MESSAGE_ID_INVALID".into(),
                    value: None,
                    caused_by: None,
                })),
                Ok(ok_updates.to_bytes()),
            ])),
        });
        let peers2 = Arc::new(PeerCache::new());
        peers2.set_self(1);
        let client2 = Client::from_transport(rec2.clone(), peers2, Arc::new(SentRecorder::new(Arc::new(|_, _| {}))));
        r.store.update(|d| {
            d.pending = Some(Receipt {
                chat_id: "812473405".into(),
                message_id: 309529,
                requested_at: Utc::now().timestamp_millis(),
                boot_id: "old".into(),
                kind: "update".into(),
                peer_type: "user".into(),
                access_hash: 7,
                topic_id: 42,
            });
            Ok::<(), std::convert::Infallible>(())
        }).unwrap();
        r.notify_ready(&client2).await;
        let calls2 = rec2.calls.lock().await;
        assert_eq!(calls2.len(), 2);
        let send_req = tl::functions::messages::SendMessage::from_bytes(&calls2[1].1[4..]).unwrap();
        assert!(matches!(send_req.peer, tl::enums::InputPeer::User(ref u) if u.user_id == 812473405 && u.access_hash == 7));
        assert!(r.store.read().unwrap().pending.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
