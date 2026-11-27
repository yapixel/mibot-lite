use crate::kit;
use chrono::{DateTime, FixedOffset, Utc};
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::Client;
use mibot_command::{Command, Invocation, LONG_TASKS, escape, group};
use mibot_logtail::Level;
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct DaTask {
    #[serde(rename = "chatId")]
    chat_id: String,
    #[serde(rename = "chatName")]
    chat_name: String,
    #[serde(rename = "startTime")]
    start_time: i64,
    #[serde(rename = "deletedMessages")]
    deleted_messages: usize,
    #[serde(rename = "isRunning")]
    is_running: bool,
    #[serde(rename = "isPaused")]
    is_paused: bool,
    #[serde(rename = "sleepUntil")]
    sleep_until: Option<i64>,
    #[serde(rename = "lastUpdate")]
    last_update: i64,
    #[serde(rename = "lastLogTime")]
    last_log_time: i64,
    errors: Vec<String>,
    #[serde(rename = "savedMessageId", default, skip_serializing_if = "is_zero")]
    saved_message_id: i32,
}

fn is_zero(v: &i32) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct DaDb {
    tasks: Vec<DaTask>,
    imported: bool,
}

#[derive(Clone)]
struct DaSlot {
    id: u64,
    cancel: CancellationToken,
}

struct Service {
    app: Arc<App>,
    store: Store<DaDb>,
    mu: Mutex<ActiveMap>,
    next_slot_id: Mutex<u64>,
}

#[derive(Default)]
struct ActiveMap {
    slots: HashMap<String, DaSlot>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn format_ts_ms(ms: i64) -> String {
    let secs = ms / 1000;
    let dt = DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_else(Utc::now);
    let offset = FixedOffset::east_opt(8 * 3600).unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
    dt.with_timezone(&offset).format("%Y-%m-%d %H:%M:%S").to_string()
}

impl Service {
    fn new(app: Arc<App>) -> Arc<Self> {
        Arc::new(Self {
            store: kit::new_store(&app, "da.json", DaDb::default),
            app,
            mu: Mutex::new(ActiveMap::default()),
            next_slot_id: Mutex::new(1),
        })
    }

    fn pause_interrupted(&self) {
        if let Err(err) = kit::update_ok(&self.store, |db| {
            for t in &mut db.tasks {
                if t.is_running {
                    t.is_paused = true;
                }
                t.is_running = false;
                t.sleep_until = None;
                if !t.errors.is_empty() {
                    t.errors = vec!["历史任务存在删除失败".into()];
                } else {
                    t.errors = Vec::new();
                }
            }
        }) {
            kit::warn(&self.app, "da.pause_interrupted_failed", &err);
        }
    }

    fn save(&self, task: &mut DaTask) {
        task.last_update = now_ms();
        let copied = task.clone();
        if let Err(err) = kit::update_ok(&self.store, |db| {
            for existing in &mut db.tasks {
                if existing.chat_id == copied.chat_id {
                    *existing = copied;
                    return;
                }
            }
            db.tasks.push(copied);
        }) {
            kit::warn(&self.app, "da.save_failed", &err);
        }
    }

    fn current(&self, chat_id: &str) -> Option<DaTask> {
        let db = match self.store.read() {
            Ok(d) => d,
            Err(err) => {
                kit::warn(&self.app, "da.read_failed", &err);
                return None;
            }
        };
        db.tasks.into_iter().rfind(|t| t.chat_id == chat_id)
    }

    async fn progress(&self, client: &Client, task: &DaTask, status: &str) -> i32 {
        let now = now_ms();
        let seconds = ((now - task.start_time) / 1000).max(1);
        let state = if let Some(until) = task.sleep_until
            && until > now
        {
            let wait_secs = ((until - now) as f64 / 1000.0).ceil() as i64;
            format!("休眠中（{wait_secs} 秒）")
        } else if task.is_running {
            "运行中".to_owned()
        } else if task.is_paused {
            "已暂停".to_owned()
        } else {
            "已停止".to_owned()
        };
        let tail_start = task.errors.len().saturating_sub(3);
        let tail = task.errors[tail_start..].join("\n");
        let rate = task.deleted_messages as f64 / seconds as f64;
        let text = format!(
            "🧹 <b>删除任务：{}</b>\n群组：{}\n状态：{state}\n已删除：{} 条\n删除速度：{rate:.2} 条/秒\n运行时长：{}小时 {}分钟 {}秒\n最后更新：{}\n{}",
            escape(status),
            escape(&task.chat_name),
            task.deleted_messages,
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60,
            format_ts_ms(task.last_update),
            escape(&tail)
        );
        if task.saved_message_id > 0
            && client
                .edit_message_html(
                    &tl::enums::InputPeer::PeerSelf,
                    task.saved_message_id,
                    &text,
                    false,
                )
                .await
                .is_ok()
        {
            return task.saved_message_id;
        }
        kit::send_self(client, &text)
            .await
            .unwrap_or(task.saved_message_id)
    }

    async fn report(&self, inv: &Invocation, sub: &str) {
        let id = &inv.message.chat_id;
        let cancel_opt = {
            let g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
            g.slots.get(id).map(|s| s.cancel.clone())
        };
        let had_slot = cancel_opt.is_some();
        if sub == "stop"
            && let Some(c) = cancel_opt
        {
            c.cancel();
        }
        let Some(task) = self.current(id) else {
            return;
        };
        let status = if sub == "stop" {
            if had_slot {
                "正在停止，等待当前请求结束"
            } else {
                "已手动停止"
            }
        } else {
            "状态查询"
        };
        self.progress(&inv.client, &task, status).await;
    }

    fn start(self: &Arc<Self>, inv: &Invocation) -> Result<bool, mibot_command::CmdError> {
        let id = inv.message.chat_id.clone();
        let slot = {
            let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
            if g.slots.contains_key(&id) {
                return Ok(false);
            }
            if g.slots.len() >= LONG_TASKS {
                return Err(kit::fail(format!(
                    "已经有 {LONG_TASKS} 个群在删除，等其中一个结束后再开始"
                )));
            }
            let slot_id = {
                let mut n = self.next_slot_id.lock().unwrap_or_else(|e| e.into_inner());
                let v = *n;
                *n += 1;
                v
            };
            let task_cancel = CancellationToken::new();
            let slot = DaSlot {
                id: slot_id,
                cancel: task_cancel,
            };
            g.slots.insert(id.clone(), slot.clone());
            slot
        };

        struct SlotGuard {
            svc: Arc<Service>,
            chat_id: String,
            slot_id: u64,
            cancel: CancellationToken,
        }
        impl Drop for SlotGuard {
            fn drop(&mut self) {
                self.cancel.cancel();
                let mut g = self.svc.mu.lock().unwrap_or_else(|e| e.into_inner());
                if g.slots.get(&self.chat_id).is_some_and(|s| s.id == self.slot_id) {
                    g.slots.remove(&self.chat_id);
                }
            }
        }

        let guard = SlotGuard {
            svc: Arc::clone(self),
            chat_id: id.clone(),
            slot_id: slot.id,
            cancel: slot.cancel.clone(),
        };
        let svc = Arc::clone(self);
        let msg = inv.message.clone();
        let accepted = self.app.registry.long_running(slot.cancel, move |task_cancel| {
            Box::pin(async move {
                let _guard = guard;
                svc.run(task_cancel, id, msg).await;
            })
        });
        if !accepted {
            return Err(kit::fail("服务正在更新或退出，没有开始删除"));
        }
        Ok(true)
    }

    fn load_task(&self, id: &str) -> Result<DaTask, mibot_store::StoreError> {
        let db = self.store.read()?;
        let now = now_ms();
        let mut task = db
            .tasks
            .into_iter()
            .rfind(|t| t.chat_id == id)
            .unwrap_or_else(|| DaTask {
                chat_id: id.to_owned(),
                chat_name: id.to_owned(),
                start_time: now,
                last_update: now,
                last_log_time: now,
                ..Default::default()
            });
        task.is_running = true;
        task.is_paused = false;
        task.sleep_until = None;
        Ok(task)
    }

    async fn finish(&self, cancel: &CancellationToken, task: &mut DaTask, id: &str, completed: bool) {
        task.is_running = false;
        task.is_paused = cancel.is_cancelled();
        task.sleep_until = None;
        self.save(task);
        let status = if cancel.is_cancelled() {
            "已停止"
        } else if completed && !task.errors.is_empty() {
            "完成，存在删除失败"
        } else if completed {
            "任务完成"
        } else {
            "执行失败"
        };
        if let Ok(saved_id) =
            tokio::time::timeout(Duration::from_secs(5), self.progress(&self.app.client, task, status))
                .await
        {
            task.saved_message_id = saved_id;
        }
        self.save(task);
        if completed
            && task.errors.is_empty()
            && !cancel.is_cancelled()
            && let Err(err) = kit::update_ok(&self.store, |db| {
                db.tasks.retain(|item| item.chat_id != id);
            })
        {
            kit::warn(&self.app, "da.remove_finished_failed", &err);
        }
    }

    async fn run(&self, cancel: CancellationToken, id: String, message: mibot_bot::Message) {
        let mut task = match self.load_task(&id) {
            Ok(t) => t,
            Err(err) => {
                self.app.logs.record(
                    Level::Error,
                    "da.store_failed",
                    &[("chat", &id), ("error", &err.to_string())],
                );
                return;
            }
        };
        self.save(&mut task);

        let Ok(peer) = self.app.client.input_peer(&message.peer) else {
            task.errors.push("无法定位该群组".into());
            self.finish(&cancel, &mut task, &id, false).await;
            return;
        };
        task.chat_name = self.app.peers.title(&message.peer);
        let mut run = DaRun {
            cancel: cancel.clone(),
            svc: self,
            peer: peer.clone(),
            task: &mut task,
        };
        let admin = run.is_admin().await;
        let _ = self.app.client.delete_messages(&peer, &[message.id]).await;
        run.task.saved_message_id = self.progress(&self.app.client, run.task, "任务已启动").await;
        self.save(run.task);
        let completed = if admin {
            run.sweep_all().await
        } else {
            run.sweep_own().await
        };
        self.finish(&cancel, &mut task, &id, completed).await;
    }
}

struct DaRun<'a> {
    cancel: CancellationToken,
    svc: &'a Service,
    peer: tl::enums::InputPeer,
    task: &'a mut DaTask,
}

impl DaRun<'_> {
    async fn is_admin(&self) -> bool {
        let Some(channel) = kit::input_channel(&self.peer) else {
            return false;
        };
        let req = tl::functions::channels::GetParticipant {
            channel: channel.clone().into(),
            participant: tl::enums::InputPeer::PeerSelf,
        };
        match self.svc.app.client.invoke(&req).await {
            Ok(tl::enums::channels::ChannelParticipant::Participant(p)) => match p.participant {
                tl::enums::ChannelParticipant::Creator(_) => true,
                tl::enums::ChannelParticipant::Admin(a) => {
                    let tl::enums::ChatAdminRights::Rights(r) = a.admin_rights;
                    r.delete_messages
                }
                _ => false,
            },
            Err(err) => {
                self.svc.app.logs.record(
                    Level::Warn,
                    "da.permission",
                    &[("error", &err.to_string())],
                );
                self.in_admin_list(channel).await
            }
        }
    }

    async fn in_admin_list(&self, channel: tl::types::InputChannel) -> bool {
        let req = tl::functions::channels::GetParticipants {
            channel: channel.into(),
            filter: tl::enums::ChannelParticipantsFilter::ChannelParticipantsAdmins,
            offset: 0,
            limit: 100,
            hash: 0,
        };
        let list = match self.svc.app.client.invoke(&req).await {
            Ok(tl::enums::channels::ChannelParticipants::Participants(p)) => p,
            Ok(tl::enums::channels::ChannelParticipants::NotModified) => return false,
            Err(err) => {
                self.svc.app.logs.record(
                    Level::Warn,
                    "da.permission_fallback",
                    &[("error", &err.to_string())],
                );
                return false;
            }
        };
        let self_id = self.svc.app.client.self_id();
        for participant in list.participants {
            match participant {
                tl::enums::ChannelParticipant::Admin(a) if a.user_id == self_id => {
                    let tl::enums::ChatAdminRights::Rights(r) = a.admin_rights;
                    return r.delete_messages;
                }
                tl::enums::ChannelParticipant::Creator(c) if c.user_id == self_id => {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    async fn delete_ids(&mut self, ids: &[i32]) {
        loop {
            if self.cancel.is_cancelled() {
                return;
            }
            match self.svc.app.client.delete_messages(&self.peer, ids).await {
                Ok(()) => {
                    self.task.deleted_messages += ids.len();
                    self.svc.save(self.task);
                    return;
                }
                Err(err) => {
                    if let Some(wait) = err.flood_wait() {
                        let until = now_ms() + wait.as_millis() as i64;
                        self.task.sleep_until = Some(until);
                        self.svc.save(self.task);
                        if !kit::sleep(&self.cancel, wait).await {
                            return;
                        }
                        self.task.sleep_until = None;
                        continue;
                    }
                    self.svc.app.logs.record(
                        Level::Error,
                        "da.delete",
                        &[("count", &ids.len().to_string()), ("error", &err.to_string())],
                    );
                    append_bounded(&mut self.task.errors, "部分消息删除失败", 20);
                    if ids.len() > 1 {
                        for &single in ids {
                            if self.cancel.is_cancelled() {
                                return;
                            }
                            if self
                                .svc
                                .app
                                .client
                                .delete_messages(&self.peer, &[single])
                                .await
                                .is_ok()
                            {
                                self.task.deleted_messages += 1;
                            }
                            let _ = kit::sleep(&self.cancel, Duration::from_millis(50)).await;
                        }
                    }
                    self.svc.save(self.task);
                    return;
                }
            }
        }
    }

    async fn retry_after(&mut self, err: &mibot_bot::BotError, event: &str, failure: &str) -> bool {
        if let Some(wait) = err.flood_wait() {
            return kit::sleep(&self.cancel, wait).await;
        }
        self.svc
            .app
            .logs
            .record(Level::Error, event, &[("error", &err.to_string())]);
        append_bounded(&mut self.task.errors, failure, 20);
        false
    }

    async fn sweep_all(&mut self) -> bool {
        let mut offset_id = 0i32;
        let mut batch: Vec<i32> = Vec::new();
        loop {
            if self.cancel.is_cancelled() {
                return false;
            }
            let req = tl::functions::messages::GetHistory {
                peer: self.peer.clone(),
                offset_id,
                offset_date: 0,
                add_offset: 0,
                limit: 100,
                max_id: 0,
                min_id: 0,
                hash: 0,
            };
            let res = match self.svc.app.client.invoke(&req).await {
                Ok(r) => r,
                Err(err) => {
                    if self.retry_after(&err, "da.history", "读取历史消息失败").await {
                        continue;
                    }
                    return false;
                }
            };
            let msgs = kit::unpack_messages(res);
            if msgs.is_empty() {
                break;
            }
            let (next, ids) = collect_page(&msgs);
            batch.extend(ids);
            if next == 0 || (offset_id > 0 && next >= offset_id) {
                break;
            }
            offset_id = next;
            while batch.len() >= 100 {
                let chunk: Vec<i32> = batch.drain(..100).collect();
                self.delete_ids(&chunk).await;
            }
            if !kit::sleep(&self.cancel, Duration::from_millis(100)).await {
                return false;
            }
        }
        if !batch.is_empty() {
            self.delete_ids(&batch).await;
        }
        true
    }

    async fn sweep_own(&mut self) -> bool {
        let mut offset_id = 0i32;
        loop {
            if self.cancel.is_cancelled() {
                return false;
            }
            let req = tl::functions::messages::Search {
                peer: self.peer.clone(),
                q: String::new(),
                from_id: Some(tl::enums::InputPeer::PeerSelf),
                saved_peer_id: None,
                saved_reaction: None,
                top_msg_id: None,
                filter: tl::enums::MessagesFilter::InputMessagesFilterEmpty,
                min_date: 0,
                max_date: 0,
                offset_id,
                add_offset: 0,
                limit: 100,
                max_id: 0,
                min_id: 0,
                hash: 0,
            };
            let res = match self.svc.app.client.invoke(&req).await {
                Ok(r) => r,
                Err(err) => {
                    if self.retry_after(&err, "da.search", "搜索自己的消息失败").await {
                        continue;
                    }
                    return false;
                }
            };
            let msgs = kit::unpack_messages(res);
            if msgs.is_empty() {
                break;
            }
            let (next, ids) = collect_page(&msgs);
            if next == 0 || (offset_id > 0 && next >= offset_id) {
                break;
            }
            offset_id = next;
            if !ids.is_empty() {
                self.delete_ids(&ids).await;
            }
            if !kit::sleep(&self.cancel, Duration::from_millis(200)).await {
                return false;
            }
        }
        true
    }
}

fn collect_page(messages: &[tl::enums::Message]) -> (i32, Vec<i32>) {
    let mut next = 0i32;
    let mut ids = Vec::new();
    for item in messages {
        let candidate = kit::message_id(item);
        if candidate <= 0 {
            continue;
        }
        if next == 0 || candidate < next {
            next = candidate;
        }
        if matches!(item, tl::enums::Message::Message(_)) {
            ids.push(candidate);
        }
    }
    (next, ids)
}

fn append_bounded(values: &mut Vec<String>, value: &str, limit: usize) {
    values.push(value.to_owned());
    if values.len() > limit {
        let drain = values.len() - limit;
        values.drain(..drain);
    }
}

pub fn help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "🧹 <b>批量删除群组消息</b>\n\n\
         • <code>{p}da true</code> 开始或继续删除\n\
         • <code>{p}da stop</code> 停止任务\n\
         • <code>{p}da status</code> 把进度发到收藏夹\n\n\
         管理员删除全部消息，普通成员只删自己的消息。"
    )
}

pub fn register(app: &Arc<App>) {
    let svc = Service::new(Arc::clone(app));
    if !app.read_only {
        svc.pause_interrupted();
    }
    let svc_cmd = Arc::clone(&svc);
    app.registry.register(Command {
        name: "da".into(),
        group: group::MEDIA.into(),
        description: "批量删除群组消息".into(),
        usage: "true|stop|status".into(),
        help: help("."),
        help_fn: Some(Arc::new(help)),
        timeout: Some(Duration::from_secs(120)),
        handle: Arc::new(move |inv| {
            let svc = Arc::clone(&svc_cmd);
            Box::pin(async move {
                if !inv.message.chat_id.starts_with('-') {
                    return Err(kit::fail("只能在群组里用"));
                }
                let sub = inv.arg(0).to_ascii_lowercase();
                match sub.as_str() {
                    "" => {
                        inv.edit(&help(inv.prefix())).await?;
                        return Ok(());
                    }
                    "stop" | "status" => svc.report(&inv, &sub).await,
                    "true" => {
                        if svc.start(&inv)? {
                            return Ok(());
                        }
                    }
                    _ => {
                        return Err(kit::fail(format!(
                            "未知子命令：{sub}，{}da help 看用法",
                            inv.prefix()
                        )));
                    }
                }
                let _ = inv.delete().await;
                Ok(())
            })
        }),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, PeerCache, RpcTransport, SentRecorder};
    use mibot_logtail::Ring;
    use std::path::PathBuf;
    use tokio::sync::Notify;

    struct PermFake {
        participant: Option<tl::enums::ChannelParticipant>,
        participant_err: Option<RpcError>,
        admins: Vec<tl::enums::ChannelParticipant>,
        admins_err: Option<RpcError>,
        calls: Mutex<Vec<String>>,
    }

    impl RpcTransport for PermFake {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                if constructor_id == tl::functions::channels::GetParticipant::CONSTRUCTOR_ID {
                    self.calls.lock().unwrap().push("participant".into());
                    if let Some(err) = &self.participant_err {
                        return Err(InvocationError::Rpc(err.clone()));
                    }
                    let participant = self.participant.clone().unwrap_or_else(|| {
                        tl::types::ChannelParticipantSelf {
                            via_request: false,
                            user_id: 1,
                            inviter_id: 2,
                            date: 1,
                            subscription_until_date: None,
                            rank: None,
                        }
                        .into()
                    });
                    let resp: tl::enums::channels::ChannelParticipant =
                        tl::types::channels::ChannelParticipant {
                            participant,
                            chats: vec![],
                            users: vec![],
                        }
                        .into();
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::channels::GetParticipants::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::GetParticipants::from_bytes(&body[4..]).unwrap();
                    let filter_name = match req.filter {
                        tl::enums::ChannelParticipantsFilter::ChannelParticipantsAdmins => {
                            "channelParticipantsAdmins"
                        }
                        _ => "other",
                    };
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("participants {filter_name} limit={}", req.limit));
                    if let Some(err) = &self.admins_err {
                        return Err(InvocationError::Rpc(err.clone()));
                    }
                    let resp = tl::enums::channels::ChannelParticipants::Participants(
                        tl::types::channels::ChannelParticipants {
                            count: self.admins.len() as i32,
                            participants: self.admins.clone(),
                            chats: vec![],
                            users: vec![],
                        },
                    );
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
        let dir = std::env::temp_dir().join(format!("mibot-da-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_is_admin_falls_back_to_admin_list() {
        let err = RpcError {
            code: 400,
            name: "CHANNEL_PRIVATE".into(),
            value: None,
            caused_by: None,
        };
        let del_rights = tl::enums::ChatAdminRights::Rights(tl::types::ChatAdminRights {
            delete_messages: true,
            ..kit::default_admin_rights()
        });
        let no_rights = tl::enums::ChatAdminRights::Rights(kit::default_admin_rights());

        let cases = [
            (
                PermFake {
                    participant: None,
                    participant_err: None,
                    admins: vec![],
                    admins_err: None,
                    calls: Mutex::new(vec![]),
                },
                false,
                vec!["participant"],
            ),
            (
                PermFake {
                    participant: Some(
                        tl::types::ChannelParticipantAdmin {
                            can_edit: false,
                            is_self: true,
                            user_id: 1,
                            inviter_id: Some(9),
                            promoted_by: 9,
                            date: 1,
                            admin_rights: no_rights.clone(),
                            rank: None,
                        }
                        .into(),
                    ),
                    participant_err: None,
                    admins: vec![],
                    admins_err: None,
                    calls: Mutex::new(vec![]),
                },
                false,
                vec!["participant"],
            ),
            (
                PermFake {
                    participant: None,
                    participant_err: Some(err.clone()),
                    admins: vec![
                        tl::types::ChannelParticipantCreator {
                            user_id: 9,
                            admin_rights: no_rights.clone(),
                            rank: None,
                        }
                        .into(),
                        tl::types::ChannelParticipantAdmin {
                            can_edit: false,
                            is_self: true,
                            user_id: 1,
                            inviter_id: Some(9),
                            promoted_by: 9,
                            date: 1,
                            admin_rights: del_rights,
                            rank: None,
                        }
                        .into(),
                    ],
                    admins_err: None,
                    calls: Mutex::new(vec![]),
                },
                true,
                vec!["participant", "participants channelParticipantsAdmins limit=100"],
            ),
            (
                PermFake {
                    participant: None,
                    participant_err: Some(err.clone()),
                    admins: vec![
                        tl::types::ChannelParticipantAdmin {
                            can_edit: false,
                            is_self: false,
                            user_id: 5,
                            inviter_id: None,
                            promoted_by: 1,
                            date: 1,
                            admin_rights: no_rights,
                            rank: None,
                        }
                        .into(),
                    ],
                    admins_err: None,
                    calls: Mutex::new(vec![]),
                },
                false,
                vec!["participant", "participants channelParticipantsAdmins limit=100"],
            ),
        ];

        for (fake, want, want_calls) in cases {
            let dir = temp_dir("perm");
            let fake = Arc::new(fake);
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
            let svc = Service::new(app);
            let mut task = DaTask::default();
            let run = DaRun {
                cancel: CancellationToken::new(),
                svc: &svc,
                peer: tl::types::InputPeerChannel {
                    channel_id: 10,
                    access_hash: 1,
                }
                .into(),
                task: &mut task,
            };
            assert_eq!(run.is_admin().await, want);
            assert_eq!(*fake.calls.lock().unwrap(), want_calls);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn test_slot_limit_and_cancel_before_run() {
        let dir = temp_dir("slots");
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        struct NullRpc;
        impl RpcTransport for NullRpc {
            fn invoke_raw<'a>(
                &'a self,
                _dc: Option<i32>,
                _id: u32,
                _body: Vec<u8>,
            ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                Box::pin(async { Err(InvocationError::Dropped("null")) })
            }
        }
        let client = Arc::new(Client::from_transport(
            Arc::new(NullRpc),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let app = App::for_testing(
            dir.clone(),
            "test",
            "boot",
            client.clone(),
            peers,
            Arc::new(Ring::new()),
        );
        let svc = Service::new(Arc::clone(&app));

        let blocker = Arc::new(Notify::new());
        let cancel_pool = CancellationToken::new();
        for _ in 0..LONG_TASKS {
            let b = Arc::clone(&blocker);
            assert!(app.registry.long_running(cancel_pool.clone(), move |_| {
                Box::pin(async move { b.notified().await })
            }));
        }

        let inv = Invocation {
            client,
            registry: app.registry.clone(),
            message: mibot_bot::Message {
                id: 1,
                peer: tl::types::PeerChannel { channel_id: 1 }.into(),
                chat_id: "-1001".into(),
                chat_type: mibot_bot::ChatType::Supergroup,
                text: ".da true".into(),
                out: true,
                edited: false,
                forward: false,
                post: false,
                saved: false,
                sender: tl::types::PeerUser { user_id: 1 }.into(),
                reply_to_id: 0,
                reply_to_peer: None,
                topic_id: 0,
                quote_text: String::new(),
                quote_entities: Vec::new(),
                raw: kit::blank_tl_message(
                    1,
                    tl::types::PeerChannel { channel_id: 1 }.into(),
                ),
            },
            prefix: ".".into(),
            command: "da".into(),
            args: vec!["true".into()],
            raw: "true".into(),
            cancel: CancellationToken::new(),
            foreign: false,
        };
        assert!(svc.start(&inv).is_err());
        cancel_pool.cancel();
        blocker.notify_waiters();
        app.registry.wait().await;

        assert!(svc.start(&inv).unwrap());
        {
            let g = svc.mu.lock().unwrap();
            g.slots["-1001"].cancel.cancel();
        }
        app.registry.wait().await;
        assert!(svc.mu.lock().unwrap().slots.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_status_does_not_overwrite_progress() {
        for remove in [false, true] {
            let dir = temp_dir(&format!("recovery-{remove}"));
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            struct RecoveryFake {
                started: Arc<Notify>,
                release: Arc<Notify>,
            }
            impl RpcTransport for RecoveryFake {
                fn invoke_raw<'a>(
                    &'a self,
                    _dc: Option<i32>,
                    constructor_id: u32,
                    _body: Vec<u8>,
                ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                    Box::pin(async move {
                        if constructor_id == tl::functions::messages::EditMessage::CONSTRUCTOR_ID {
                            self.started.notify_one();
                            self.release.notified().await;
                            return Ok(tl::enums::Updates::Updates(tl::types::Updates {
                                updates: vec![],
                                users: vec![],
                                chats: vec![],
                                date: 0,
                                seq: 0,
                            })
                            .to_bytes());
                        }
                        if constructor_id == tl::functions::messages::DeleteMessages::CONSTRUCTOR_ID
                        {
                            let resp: tl::enums::messages::AffectedMessages =
                                tl::types::messages::AffectedMessages {
                                    pts: 1,
                                    pts_count: 1,
                                }
                                .into();
                            return Ok(resp.to_bytes());
                        }
                        Err(InvocationError::Dropped("unexpected"))
                    })
                }
            }
            let fake = Arc::new(RecoveryFake {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            });
            let peers = Arc::new(PeerCache::new());
            peers.set_self(1);
            let client = Arc::new(Client::from_transport(
                fake,
                Arc::clone(&peers),
                Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
            ));
            let app = App::for_testing(
                dir.clone(),
                "test",
                "boot",
                client.clone(),
                peers,
                Arc::new(Ring::new()),
            );
            let svc = Service::new(app);
            let mut live = DaTask {
                chat_id: "-1009".into(),
                is_running: true,
                saved_message_id: 90,
                ..Default::default()
            };
            svc.save(&mut live);
            let snapshot = svc.current("-1009").unwrap();
            let svc_bg = Arc::clone(&svc);
            let client_bg = Arc::clone(&client);
            let handle = tokio::spawn(async move {
                svc_bg.progress(&client_bg, &snapshot, "状态查询").await;
            });
            started.notified().await;

            let mut run = DaRun {
                cancel: CancellationToken::new(),
                svc: &svc,
                peer: tl::enums::InputPeer::PeerSelf,
                task: &mut live,
            };
            run.delete_ids(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).await;
            let before = svc.store.read().unwrap();
            assert_eq!(before.tasks[0].deleted_messages, 10);
            if remove {
                kit::update_ok(&svc.store, |db| {
                    db.tasks.clear();
                })
                .unwrap();
            }
            release.notify_one();
            handle.await.unwrap();
            let after = svc.store.read().unwrap();
            if remove {
                assert!(after.tasks.is_empty());
            } else {
                assert_eq!(after.tasks[0].deleted_messages, 10);
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
