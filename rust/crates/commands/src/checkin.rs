use crate::kit;
use chrono::{DateTime, Datelike, FixedOffset, Timelike, Utc};
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{Client, SendOptions, first_sent_message_id, without_ip_privacy};
use mibot_botapi::{Client as BotApiClient};
use mibot_command::{CmdError, CmdResult, Command, Invocation, PAGE_LIMIT, bold, brief, code, escape, group, html_pages, truncate};
use mibot_delivery::is_uncertain;
use mibot_store::Store;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct SignTarget {
    id: String,
    name: String,
    target: String,
    command: String,
    #[serde(rename = "callbackData", skip_serializing_if = "String::is_empty")]
    callback_data: String,
    #[serde(rename = "buttonText", skip_serializing_if = "String::is_empty")]
    button_text: String,
    enabled: bool,
}

impl SignTarget {
    fn matcher(&self) -> String {
        if !self.callback_data.is_empty() {
            format!("{} {}", kit::tr("callback data", "回调数据"), code(&self.callback_data))
        } else if !self.button_text.is_empty() {
            format!("{} {}", kit::tr("button text", "按钮文字"), code(&self.button_text))
        } else {
            String::new()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    #[serde(rename = "runTime")]
    run_time: String,
    #[serde(rename = "runTimeEnd")]
    run_time_end: String,
    #[serde(rename = "logChat")]
    log_chat: String,
    #[serde(rename = "botToken")]
    bot_token: String,
    #[serde(rename = "pushChatId")]
    push_chat_id: String,
    #[serde(rename = "randomDelay")]
    random_delay: usize,
    #[serde(rename = "lastRunDate")]
    last_run_date: String,
    targets: Vec<SignTarget>,
    #[serde(rename = "currentRunTime", skip_serializing_if = "String::is_empty")]
    current_run_time: String,
    #[serde(rename = "plannedDate", skip_serializing_if = "String::is_empty")]
    planned_date: String,
    #[serde(rename = "attemptDate", skip_serializing_if = "String::is_empty")]
    attempt_date: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attempted: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            run_time: "10:00".into(),
            run_time_end: "11:30".into(),
            log_chat: String::new(),
            bot_token: String::new(),
            push_chat_id: String::new(),
            random_delay: 0,
            last_run_date: String::new(),
            targets: Vec::new(),
            current_run_time: String::new(),
            planned_date: String::new(),
            attempt_date: String::new(),
            attempted: Vec::new(),
        }
    }
}

impl Config {
    fn enabled(&self) -> Vec<SignTarget> {
        self.targets.iter().filter(|t| t.enabled).cloned().collect()
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.targets.iter().position(|t| t.id == id)
    }
}

fn shanghai_offset() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("valid +08:00 offset")
}

fn to_shanghai(t: SystemTime) -> DateTime<FixedOffset> {
    DateTime::<Utc>::from(t).with_timezone(&shanghai_offset())
}

fn schedule_date(cfg: &Config, now: SystemTime) -> String {
    let mut local = to_shanghai(now);
    let start = parse_clock(&cfg.run_time).unwrap_or(0);
    if let Some(end) = parse_clock(&cfg.run_time_end)
        && end <= start
        && (local.hour() * 60 + local.minute()) < start as u32
    {
        local -= chrono::Duration::days(1);
    }
    format!("{}/{}/{}", local.year(), local.month(), local.day())
}

fn planned_time(cfg: &Config) -> Option<SystemTime> {
    let mut parts = cfg.planned_date.split('/');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    let minute = parse_clock(&cfg.current_run_time)?;
    let start = parse_clock(&cfg.run_time).unwrap_or(0);
    let offset = shanghai_offset();
    let mut dt = chrono::NaiveDate::from_ymd_opt(y, m, d)?
        .and_hms_opt(0, 0, 0)?
        .and_local_timezone(offset)
        .single()?;
    if let Some(end) = parse_clock(&cfg.run_time_end)
        && end <= start
        && minute < start
    {
        dt += chrono::Duration::days(1);
    }
    dt += chrono::Duration::minutes(minute as i64);
    Some(UNIX_EPOCH + Duration::from_secs(dt.timestamp().max(0) as u64))
}

fn parse_clock(value: &str) -> Option<usize> {
    let norm = value.trim().replace('：', ":");
    let (h_str, m_str) = norm.split_once(':')?;
    if !(1..=2).contains(&h_str.len())
        || m_str.len() != 2
        || !kit::is_digits(h_str)
        || !kit::is_digits(m_str)
    {
        return None;
    }
    let h: usize = h_str.parse().ok()?;
    let m: usize = m_str.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

fn format_clock(minutes: usize) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn pick(cfg: &Config, random: impl FnOnce(usize) -> usize) -> String {
    let start = parse_clock(&cfg.run_time).unwrap_or(10 * 60);
    let Some(mut end) = parse_clock(&cfg.run_time_end) else {
        return format_clock(start);
    };
    if end <= start {
        end += 24 * 60;
    }
    format_clock((start + random(end - start)) % (24 * 60))
}

fn rand_below(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    nanos % n
}

fn parse_matcher(raw: &str) -> (String, String) {
    let t = raw.trim();
    if t.is_empty() {
        return (String::new(), String::new());
    }
    if let Some(rest) = t.strip_prefix("data:") {
        return (rest.trim().to_owned(), String::new());
    }
    if let Some(rest) = t.strip_prefix("text:") {
        return (String::new(), rest.trim().to_owned());
    }
    (t.to_owned(), String::new())
}

fn button_data(message: &tl::types::Message, target: &SignTarget) -> Option<Vec<u8>> {
    let Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(markup)) = &message.reply_markup else {
        return None;
    };
    for tl::enums::KeyboardInlineButtonRow::Row(row) in &markup.rows {
        for tl::enums::KeyboardInlineButton::Button(btn) in &row.buttons {
            let tl::enums::InlineButtonType::Callback(cb) = &btn.r#type else {
                continue;
            };
            if !target.callback_data.is_empty() && cb.data == target.callback_data.as_bytes() {
                return Some(cb.data.clone());
            }
            if target.callback_data.is_empty()
                && !target.button_text.is_empty()
                && btn.text == target.button_text
            {
                return Some(cb.data.clone());
            }
        }
    }
    None
}

fn button_label(target: &SignTarget) -> String {
    if !target.callback_data.is_empty() {
        format!("{} {}", kit::tr("callback data", "回调数据"), target.callback_data)
    } else {
        format!("{} {}", kit::tr("button text", "按钮文字"), target.button_text)
    }
}

fn mask(secret: &str) -> String {
    if secret.chars().count() <= 5 {
        return "***".into();
    }
    let prefix: String = secret.chars().take(5).collect();
    format!("{prefix}…")
}

fn brief_reply(reply: &str) -> String {
    let collapsed = reply.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        kit::tr("(no text in reply)", "（回复里没有文字）").into()
    } else {
        truncate(&collapsed, 80)
    }
}

fn reason(err: &(dyn std::error::Error + 'static), cancelled: bool) -> String {
    if cancelled {
        return kit::tr("Stopped before completion", "服务停了，没签完").into();
    }
    if kit::is_user_error(err) {
        return err.to_string();
    }
    brief(err)
}

fn remaining(pages: &[String], sent: usize, uncertain: bool) -> Vec<String> {
    if sent == 0 && !uncertain {
        return pages.to_vec();
    }
    let mut rest = pages[sent.min(pages.len())..].to_vec();
    let mut note = String::new();
    if kit::is_zh() {
        if sent > 0 {
            note.push_str(&format!("签到汇总的前 {sent} 页已经发出。"));
        }
        if uncertain && !rest.is_empty() {
            note.push_str(&format!(
                "第 {} 页不确定是否送达，为免重复没有在这里重发。",
                sent + 1
            ));
            rest.remove(0);
        }
        if !rest.is_empty() {
            note.push_str(&format!("以下是其余 {} 页。", rest.len()));
        }
    } else {
        if sent > 0 {
            note.push_str(&format!("First {sent} page(s) of check-in summary already sent. "));
        }
        if uncertain && !rest.is_empty() {
            note.push_str(&format!(
                "Page {} delivery uncertain, skipped to avoid duplicates. ",
                sent + 1
            ));
            rest.remove(0);
        }
        if !rest.is_empty() {
            note.push_str(&format!("Remaining {} page(s) below.", rest.len()));
        }
    }
    let mut out = vec![format!("ℹ️ {}", note.trim_end())];
    out.extend(rest);
    out
}

#[derive(Clone)]
struct RuntimeConfig {
    reply_wait: Duration,
    poll_every: Duration,
    gap: Duration,
    bot_api: String,
    bot_pusher: Arc<BotApiClient>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            reply_wait: Duration::from_secs(10),
            poll_every: Duration::from_secs(1),
            gap: Duration::from_secs(2),
            bot_api: "https://api.telegram.org".into(),
            bot_pusher: Arc::new(BotApiClient::new()),
        }
    }
}

struct Service {
    app: Option<Arc<App>>,
    store: Arc<Store<Config>>,
    running: Arc<Mutex<()>>,
    rt: RuntimeConfig,
}

impl Service {
    async fn await_message<F>(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        peer: &tl::enums::InputPeer,
        mut match_fn: F,
    ) -> Result<Option<tl::types::Message>, CmdError>
    where
        F: FnMut(&tl::types::Message) -> bool,
    {
        let deadline = tokio::time::Instant::now() + self.rt.reply_wait;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline || cancel.is_cancelled() {
                break;
            }
            let step = (deadline - now).min(self.rt.poll_every);
            if !kit::sleep(cancel, step).await {
                break;
            }
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                break;
            }
            let req = tl::functions::messages::GetHistory {
                peer: peer.clone(),
                offset_id: 0,
                offset_date: 0,
                add_offset: 0,
                limit: 8,
                max_id: 0,
                min_id: 0,
                hash: 0,
            };
            let res = tokio::select! {
                _ = cancel.cancelled() => break,
                r = tokio::time::timeout(rem, client.invoke(&req)) => match r {
                    Ok(Ok(v)) => v,
                    Ok(Err(err)) => {
                        if !cancel.is_cancelled() {
                            kit::opt_log(
                                self.app.as_ref().map(|a| &a.logs),
                                mibot_logtail::Level::Debug,
                                "checkin.poll_failed",
                                &[("error", &err.to_string())],
                            );
                        }
                        continue;
                    }
                    Err(_) => break,
                }
            };
            let msgs = kit::unpack_messages(res);
            let mut found: Option<tl::types::Message> = None;
            for item in msgs {
                if let tl::enums::Message::Message(m) = item
                    && match_fn(&m)
                    && found.as_ref().is_none_or(|cur| m.id < cur.id)
                {
                    found = Some(m);
                }
            }
            if let Some(m) = found {
                return Ok(Some(m));
            }
        }
        if cancel.is_cancelled() {
            return Err(kit::fail(kit::tr(
                "Stopped before completion",
                "服务停了，没签完",
            )));
        }
        Ok(None)
    }

    async fn click(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        peer: &tl::enums::InputPeer,
        message: &tl::types::Message,
        data: Vec<u8>,
    ) -> Result<String, CmdError> {
        let req = tl::functions::messages::GetBotCallbackAnswer {
            game: false,
            peer: peer.clone(),
            msg_id: message.id,
            data: Some(data),
            password: None,
        };
        match client.invoke(&req).await {
            Ok(tl::enums::messages::BotCallbackAnswer::Answer(ans)) => {
                if let Some(msg) = ans.message
                    && !msg.is_empty()
                {
                    return Ok(msg);
                }
            }
            Err(err) if err.is_rpc("BOT_RESPONSE_TIMEOUT") => {}
            Err(err) => return Err(kit::fail_with(kit::tr("Failed to click button", "点按钮失败"), err)),
        }
        let edited = message.edit_date.unwrap_or(0);
        let msg_id = message.id;
        let reply = self
            .await_message(cancel, client, peer, |cand| {
                !cand.out
                    && (cand.id > msg_id
                        || (cand.id == msg_id && cand.edit_date.unwrap_or(0) > edited))
            })
            .await?;
        Ok(match reply {
            None => kit::tr("Button clicked, no reply received", "已点按钮，没收到回复").into(),
            Some(m) => m.message,
        })
    }

    async fn sign_with_attempt<F>(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        target: &SignTarget,
        begin: Option<F>,
    ) -> Result<String, CmdError>
    where
        F: FnOnce() -> Result<(), CmdError>,
    {
        let peer = client
            .resolve_target(&target.target)
            .await
            .map_err(|e| {
                kit::fail_with(
                    if kit::is_zh() {
                        format!("找不到目标 {}", target.target)
                    } else {
                        format!("Target not found: {}", target.target)
                    },
                    e,
                )
            })?;
        if cancel.is_cancelled() {
            return Err(kit::fail(kit::tr(
                "Stopped before completion",
                "服务停了，没签完",
            )));
        }
        if let Some(f) = begin {
            f()?;
        }
        let sent_updates = without_ip_privacy(|| {
            client.send_text(&peer, &target.command, SendOptions::default())
        })
        .await
        .map_err(|e| kit::fail_with(kit::tr("Failed to send check-in command", "签到命令发送失败"), e))?;
        let sent = first_sent_message_id(&sent_updates).unwrap_or(0);
        let wants_button = !target.callback_data.is_empty() || !target.button_text.is_empty();
        let reply = self
            .await_message(cancel, client, &peer, |m| {
                if m.out || m.id <= sent {
                    return false;
                }
                button_data(m, target).is_some() || !wants_button
            })
            .await?;
        let Some(reply) = reply else {
            let secs = self.rt.reply_wait.as_secs();
            if wants_button {
                return Err(kit::fail(if kit::is_zh() {
                    format!("{secs} 秒内没收到带这个按钮的回复（{}）", button_label(target))
                } else {
                    format!(
                        "No reply with matching button within {secs}s ({})",
                        button_label(target)
                    )
                }));
            }
            return Err(kit::fail(if kit::is_zh() {
                format!("{secs} 秒内没收到回复")
            } else {
                format!("No reply within {secs}s")
            }));
        };
        if !wants_button {
            return Ok(reply.message);
        }
        let data = button_data(&reply, target).unwrap_or_default();
        self.click(cancel, client, &peer, &reply, data).await
    }

    async fn run_all(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        targets: &[SignTarget],
        source: &str,
        date: &str,
    ) -> Result<String, CmdError> {
        let started = SystemTime::now();
        let mut lines = Vec::new();
        let mut succeeded = 0usize;
        let mut skipped = 0usize;
        let stopped = || kit::tr("Stopped before completion", "服务停了，没签完");

        for (index, target) in targets.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(kit::fail(stopped()));
            }
            if index > 0 && !kit::sleep(cancel, self.rt.gap).await {
                return Err(kit::fail(stopped()));
            }
            let cfg = self.store.read()?;
            if cfg.attempt_date == date && cfg.attempted.iter().any(|id| id == &target.id) {
                skipped += 1;
                lines.push(if kit::is_zh() {
                    format!(
                        "ℹ️ {}：上次已开始，结果可能未确认，没有重复发送",
                        escape(&target.name)
                    )
                } else {
                    format!(
                        "ℹ️ {}: already attempted earlier, skipped to avoid duplicate",
                        escape(&target.name)
                    )
                });
                continue;
            }
            let store_ref = Arc::clone(&self.store);
            let date_owned = date.to_owned();
            let tid = target.id.clone();
            let cancel_ref = cancel.clone();
            let begin = move || -> Result<(), CmdError> {
                kit::update_ok(&store_ref, |c| {
                    if c.attempt_date != date_owned {
                        c.attempt_date = date_owned.clone();
                        c.attempted.clear();
                    }
                    c.attempted.push(tid.clone());
                })?;
                if cancel_ref.is_cancelled() {
                    let _ = kit::update_ok(&store_ref, |c| {
                        if c.attempt_date == date_owned {
                            c.attempted.retain(|x| x != &tid);
                        }
                    });
                    return Err(kit::fail(kit::tr(
                        "Stopped before completion",
                        "服务停了，没签完",
                    )));
                }
                Ok(())
            };
            let res = self.sign_with_attempt(cancel, client, target, Some(begin)).await;
            if cancel.is_cancelled() {
                return Err(kit::fail(stopped()));
            }
            match res {
                Err(err) => {
                    kit::opt_log(
                        self.app.as_ref().map(|a| &a.logs),
                        mibot_logtail::Level::Warn,
                        "checkin.failed",
                        &[("target", &target.id), ("error", &err.to_string())],
                    );
                    lines.push(if kit::is_zh() {
                        format!(
                            "❌ {}：{}",
                            escape(&target.name),
                            escape(&reason(err.as_ref(), cancel.is_cancelled()))
                        )
                    } else {
                        format!(
                            "❌ {}: {}",
                            escape(&target.name),
                            escape(&reason(err.as_ref(), cancel.is_cancelled()))
                        )
                    });
                }
                Ok(reply) => {
                    succeeded += 1;
                    lines.push(if kit::is_zh() {
                        format!(
                            "✅ {}：{}",
                            escape(&target.name),
                            escape(&brief_reply(&reply))
                        )
                    } else {
                        format!(
                            "✅ {}: {}",
                            escape(&target.name),
                            escape(&brief_reply(&reply))
                        )
                    });
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(kit::fail(stopped()));
        }
        kit::update_ok(&self.store, |c| {
            c.last_run_date = date.to_owned();
            c.attempt_date.clear();
            c.attempted.clear();
        })?;
        let dt = to_shanghai(started);
        let failed = targets.len().saturating_sub(succeeded + skipped);
        let mut out = if kit::is_zh() {
            let mut count_line = format!("成功 {succeeded} 个，失败 {failed} 个");
            if skipped > 0 {
                count_line.push_str(&format!("，跳过上次已开始的 {skipped} 个"));
            }
            vec![
                "📅 <b>签到汇总</b>".to_owned(),
                format!("{source} · {}（北京时间）", dt.format("%Y-%m-%d %H:%M")),
                count_line,
                String::new(),
            ]
        } else {
            let mut count_line = format!("{succeeded} succeeded, {failed} failed");
            if skipped > 0 {
                count_line.push_str(&format!(", {skipped} skipped"));
            }
            vec![
                "📅 <b>Check-in Summary</b>".to_owned(),
                format!("{source} · {} (UTC+8)", dt.format("%Y-%m-%d %H:%M")),
                count_line,
                String::new(),
            ]
        };
        out.extend(lines);
        Ok(out.join("\n"))
    }

    async fn deliver(
        &self,
        client: &Client,
        cfg: &Config,
        summary: &str,
        inv: Option<&Invocation>,
    ) -> CmdResult {
        let mut pages = html_pages(summary, PAGE_LIMIT);
        if !cfg.bot_token.is_empty() && !cfg.push_chat_id.is_empty() {
            let (sent, err_opt) = push_via_bot(
                &self.rt.bot_pusher,
                &self.rt.bot_api,
                &cfg.bot_token,
                &cfg.push_chat_id,
                &pages,
            )
            .await;
            let Some(err) = err_opt else {
                return remove_command(inv).await;
            };
            kit::opt_log(
                self.app.as_ref().map(|a| &a.logs),
                mibot_logtail::Level::Warn,
                "checkin.bot_push_failed",
                &[("sent", &sent.to_string()), ("pages", &pages.len().to_string()), ("error", &err.to_string())],
            );
            pages = remaining(&pages, sent, is_uncertain(&err));
        }
        if !cfg.log_chat.is_empty() {
            let (sent, err_opt) = send_pages(client, &cfg.log_chat, &pages).await;
            let Some(err) = err_opt else {
                return remove_command(inv).await;
            };
            kit::opt_log(
                self.app.as_ref().map(|a| &a.logs),
                mibot_logtail::Level::Warn,
                "checkin.log_chat_failed",
                &[
                    ("chat", &cfg.log_chat),
                    ("sent", &sent.to_string()),
                    ("pages", &pages.len().to_string()),
                    ("error", &err.to_string()),
                ],
            );
            pages = remaining(&pages, sent, is_uncertain(&err));
        }
        if let Some(inv) = inv {
            inv.respond_pages(&pages).await?;
            return Ok(());
        }
        let (_, err_opt) = send_pages(client, "me", &pages).await;
        match err_opt {
            None => Ok(()),
            Some(err) => Err(Box::new(err)),
        }
    }

    async fn tick(&self, cancel: &CancellationToken, client: &Client, now: SystemTime) {
        let Ok(mut cfg) = self.store.read() else {
            return;
        };
        let today = schedule_date(&cfg, now);
        if cfg.last_run_date == today || cfg.enabled().is_empty() {
            return;
        }
        if cfg.planned_date != today || cfg.current_run_time.is_empty() {
            cfg.planned_date = today.clone();
            cfg.current_run_time = pick(&cfg, rand_below);
            let pd = cfg.planned_date.clone();
            let crt = cfg.current_run_time.clone();
            if let Err(err) = kit::update_ok(&self.store, move |stored| {
                stored.planned_date = pd;
                stored.current_run_time = crt;
            }) {
                kit::opt_log(
                    self.app.as_ref().map(|a| &a.logs),
                    mibot_logtail::Level::Warn,
                    "checkin.save_failed",
                    &[("error", &err.to_string())],
                );
                return;
            }
        }
        let Some(at) = planned_time(&cfg) else {
            return;
        };
        if now < at {
            return;
        }
        let reg = self.app.as_ref().map(|a| Arc::clone(&a.registry));
        let done_tok = cancel.child_token();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let mut tracked = false;
        if let Some(r) = &reg {
            let mut tx = Some(done_tx);
            let track_ok = r.long_running(done_tok.clone(), move |t| {
                Box::pin(async move {
                    t.cancelled().await;
                    if let Some(sender) = tx.take() {
                        let _ = sender.send(());
                    }
                })
            });
            if !track_ok {
                return;
            }
            tracked = true;
        }
        let _track_guard = TrackCancelGuard(done_tok);
        if cfg.random_delay > 0 {
            let max_secs = cfg.random_delay * 60;
            let wait_secs = rand_below(max_secs.max(1));
            if !kit::sleep(cancel, Duration::from_secs(wait_secs as u64)).await {
                drop(_track_guard);
                if tracked {
                    let _ = done_rx.await;
                    tokio::task::yield_now().await;
                }
                return;
            }
        }
        let Ok(_run_guard) = self.running.try_lock() else {
            drop(_track_guard);
            if tracked {
                let _ = done_rx.await;
                tokio::task::yield_now().await;
            }
            return;
        };
        let Ok(cfg) = self.store.read() else {
            drop(_track_guard);
            if tracked {
                let _ = done_rx.await;
                tokio::task::yield_now().await;
            }
            return;
        };
        if cfg.last_run_date == today || cfg.planned_date != today {
            drop(_track_guard);
            if tracked {
                let _ = done_rx.await;
                tokio::task::yield_now().await;
            }
            return;
        }
        if planned_time(&cfg).is_none_or(|p| now < p) {
            drop(_track_guard);
            if tracked {
                let _ = done_rx.await;
                tokio::task::yield_now().await;
            }
            return;
        }
        let targets = cfg.enabled();
        if targets.is_empty() {
            drop(_track_guard);
            if tracked {
                let _ = done_rx.await;
                tokio::task::yield_now().await;
            }
            return;
        }
        kit::opt_log(
            self.app.as_ref().map(|a| &a.logs),
            mibot_logtail::Level::Info,
            "checkin.start",
            &[("targets", &targets.len().to_string()), ("planned", &cfg.current_run_time)],
        );
        match self.run_all(cancel, client, &targets, kit::tr("Scheduled", "定时"), &today).await {
            Err(err) => kit::opt_log(
                self.app.as_ref().map(|a| &a.logs),
                mibot_logtail::Level::Warn,
                "checkin.interrupted",
                &[("error", &err.to_string())],
            ),
            Ok(summary) => {
                if let Err(err) = self.deliver(client, &cfg, &summary, None).await {
                    kit::opt_log(
                        self.app.as_ref().map(|a| &a.logs),
                        mibot_logtail::Level::Warn,
                        "checkin.report_failed",
                        &[("error", &err.to_string())],
                    );
                }
            }
        }
        drop(_track_guard);
        if tracked {
            let _ = done_rx.await;
            tokio::task::yield_now().await;
        }
    }

    async fn handle(&self, inv: &Invocation) -> CmdResult {
        let action = inv.arg(0).to_ascii_lowercase();
        match action.as_str() {
            "" => self.run_now(inv).await,
            "add" => self.add(inv).await,
            "del" | "delete" | "rm" => self.remove(inv).await,
            "list" | "ls" => self.list(inv).await,
            "toggle" => self.toggle(inv).await,
            "test" => self.test(inv).await,
            "set" => self.set(inv).await,
            "config" | "settings" | "info" => self.show(inv).await,
            "reset" => {
                kit::update_ok(&self.store, |c| {
                    c.last_run_date.clear();
                    c.planned_date.clear();
                    c.current_run_time.clear();
                    c.attempt_date.clear();
                    c.attempted.clear();
                })?;
                inv.edit_text(kit::tr(
                    "✅ Cleared today's run record; scheduled check-in will run again",
                    "✅ 已清掉今天的执行记录，到时间会再签一次",
                ))
                .await?;
                Ok(())
            }
            _ => Err(kit::fail(if kit::is_zh() {
                format!("没有这个子命令：{}，{}help checkin 看用法", inv.arg(0), inv.prefix())
            } else {
                format!(
                    "Unknown subcommand: {}; run {}help checkin for usage",
                    inv.arg(0),
                    inv.prefix()
                )
            })),
        }
    }

    async fn run_now(&self, inv: &Invocation) -> CmdResult {
        let cfg = self.store.read()?;
        if cfg.targets.is_empty() {
            return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
        }
        let targets = cfg.enabled();
        if targets.is_empty() {
            return Err(kit::fail(if kit::is_zh() {
                format!("签到目标都关着，{}checkin toggle ID 可以打开", inv.prefix())
            } else {
                format!(
                    "All targets are disabled; enable one with {}checkin toggle ID",
                    inv.prefix()
                )
            }));
        }
        let Ok(_guard) = self.running.try_lock() else {
            return Err(kit::fail(kit::tr(
                "Check-in is already running; please wait",
                "正在签到，请稍候",
            )));
        };
        inv.edit_text(&kit::working(&if kit::is_zh() {
            format!("正在签到 {} 个目标", targets.len())
        } else {
            format!("Checking in {} target(s)", targets.len())
        }))
        .await?;
        let today = schedule_date(&cfg, SystemTime::now());
        let summary = self
            .run_all(&inv.cancel, &inv.client, &targets, kit::tr("Manual", "手动"), &today)
            .await?;
        self.deliver(&inv.client, &cfg, &summary, Some(inv)).await
    }

    async fn add(&self, inv: &Invocation) -> CmdResult {
        let (head, sign) = inv.raw_after(0).split_once('\n').unwrap_or(("", ""));
        let fields: Vec<&str> = head.split_whitespace().collect();
        let sign = sign.trim();
        if fields.len() < 4 || sign.is_empty() {
            return Err(kit::fail(if kit::is_zh() {
                format!(
                    "用法：{}checkin add ID 名称 目标 [data:回调数据|text:按钮文字]，签到命令写在第二行",
                    inv.prefix()
                )
            } else {
                format!(
                    "Usage: {}checkin add ID name target [data:callback|text:button], put the command on the second line",
                    inv.prefix()
                )
            }));
        }
        let (callback_data, button_text) = parse_matcher(&fields[4..].join(" "));
        let next = SignTarget {
            id: fields[1].to_owned(),
            name: fields[2].to_owned(),
            target: fields[3].to_owned(),
            command: sign.to_owned(),
            callback_data,
            button_text,
            enabled: true,
        };
        let mut updated = false;
        kit::update_ok(&self.store, |cfg| {
            if let Some(idx) = cfg.find(&next.id) {
                cfg.targets[idx] = next.clone();
                updated = true;
            } else {
                cfg.targets.push(next.clone());
            }
        })?;
        let mut lines = if kit::is_zh() {
            let verb = if updated { "更新" } else { "添加" };
            vec![
                format!("✅ 已{verb}签到目标 {}（{}）", bold(&next.name), code(&next.id)),
                format!("目标：{}", code(&next.target)),
                format!("命令：{}", code(&next.command)),
            ]
        } else {
            let verb = if updated { "Updated" } else { "Added" };
            vec![
                format!("✅ {verb} check-in target {} ({})", bold(&next.name), code(&next.id)),
                format!("Target: {}", code(&next.target)),
                format!("Command: {}", code(&next.command)),
            ]
        };
        let matcher = next.matcher();
        if !matcher.is_empty() {
            lines.push(if kit::is_zh() {
                format!("然后点：{matcher}")
            } else {
                format!("Then click: {matcher}")
            });
        }
        inv.edit(&lines.join("\n")).await?;
        Ok(())
    }

    async fn remove(&self, inv: &Invocation) -> CmdResult {
        let id = inv.arg(1);
        if id.is_empty() {
            return Err(kit::usage(inv.prefix(), "checkin del ID"));
        }
        let mut found = false;
        kit::update_ok(&self.store, |c| {
            if let Some(idx) = c.find(id) {
                c.targets.remove(idx);
                found = true;
            }
        })?;
        if !found {
            return Err(not_found(inv, id));
        }
        inv.edit(&if kit::is_zh() {
            format!("✅ 已删除签到目标 {}", code(id))
        } else {
            format!("✅ Deleted check-in target {}", code(id))
        })
        .await?;
        Ok(())
    }

    async fn toggle(&self, inv: &Invocation) -> CmdResult {
        let id = inv.arg(1);
        if id.is_empty() {
            return Err(kit::usage(inv.prefix(), "checkin toggle ID"));
        }
        let mut found: Option<SignTarget> = None;
        kit::update_ok(&self.store, |c| {
            if let Some(idx) = c.find(id) {
                c.targets[idx].enabled = !c.targets[idx].enabled;
                found = Some(c.targets[idx].clone());
            }
        })?;
        let Some(t) = found else {
            return Err(not_found(inv, id));
        };
        inv.edit(&if kit::is_zh() {
            format!("✅ {} 已{}", bold(&t.name), kit::on_off_text(t.enabled))
        } else {
            format!("✅ {} turned {}", bold(&t.name), kit::on_off_text(t.enabled))
        })
        .await?;
        Ok(())
    }

    async fn list(&self, inv: &Invocation) -> CmdResult {
        let cfg = self.store.read()?;
        if cfg.targets.is_empty() {
            inv.edit(&if kit::is_zh() {
                format!(
                    "📅 <b>签到目标</b>\n\n还没有，{} 添加",
                    code(&format!("{}checkin add", inv.prefix()))
                )
            } else {
                format!(
                    "📅 <b>Check-in Targets</b>\n\nNone yet; add with {}",
                    code(&format!("{}checkin add", inv.prefix()))
                )
            })
            .await?;
            return Ok(());
        }
        let mut lines = vec![if kit::is_zh() {
            format!(
                "📅 <b>签到目标</b>（{}/{} 个开启）",
                cfg.enabled().len(),
                cfg.targets.len()
            )
        } else {
            format!(
                "📅 <b>Check-in Targets</b> ({}/{} enabled)",
                cfg.enabled().len(),
                cfg.targets.len()
            )
        }];
        for (idx, t) in cfg.targets.iter().enumerate() {
            let mut title = format!("<b>{}. {}</b>", idx + 1, escape(&t.name));
            if !t.enabled {
                title.push_str(kit::tr(" (disabled)", "（关闭）"));
            }
            lines.push(String::new());
            lines.push(title);
            if kit::is_zh() {
                lines.push(format!("ID：{} · 目标：{}", code(&t.id), code(&t.target)));
                lines.push(format!("命令：{}", code(&t.command)));
            } else {
                lines.push(format!("ID: {} · Target: {}", code(&t.id), code(&t.target)));
                lines.push(format!("Command: {}", code(&t.command)));
            }
            let matcher = t.matcher();
            if !matcher.is_empty() {
                lines.push(if kit::is_zh() {
                    format!("然后点：{matcher}")
                } else {
                    format!("Then click: {matcher}")
                });
            }
        }
        inv.respond_pages(&html_pages(&lines.join("\n"), PAGE_LIMIT)).await?;
        Ok(())
    }

    async fn test(&self, inv: &Invocation) -> CmdResult {
        let id = inv.arg(1);
        if id.is_empty() {
            return Err(kit::usage(inv.prefix(), "checkin test ID"));
        }
        let cfg = self.store.read()?;
        let Some(idx) = cfg.find(id) else {
            return Err(not_found(inv, id));
        };
        let target = cfg.targets[idx].clone();
        let Ok(_guard) = self.running.try_lock() else {
            return Err(kit::fail(kit::tr(
                "Check-in is already running; please wait",
                "正在签到，请稍候",
            )));
        };
        inv.edit_text(&kit::working(&if kit::is_zh() {
            format!("正在试签 {}", target.name)
        } else {
            format!("Testing check-in for {}", target.name)
        }))
        .await?;
        match self.sign_with_attempt::<fn() -> Result<(), CmdError>>(&inv.cancel, &inv.client, &target, None).await {
            Ok(reply) => {
                inv.edit(&if kit::is_zh() {
                    format!("✅ 已签到 {}\n回复：{}", bold(&target.name), escape(&brief_reply(&reply)))
                } else {
                    format!("✅ Checked in {}\nReply: {}", bold(&target.name), escape(&brief_reply(&reply)))
                })
                .await?;
                Ok(())
            }
            Err(err) => Err(kit::fail(if kit::is_zh() {
                format!(
                    "{} 签到失败：{}",
                    target.name,
                    reason(err.as_ref(), inv.cancel.is_cancelled())
                )
            } else {
                format!(
                    "{} check-in failed: {}",
                    target.name,
                    reason(err.as_ref(), inv.cancel.is_cancelled())
                )
            })),
        }
    }

    async fn set(&self, inv: &Invocation) -> CmdResult {
        let value = inv.arg(2);
        match inv.arg(1).to_ascii_lowercase().as_str() {
            "time" | "t" => {
                let Some(at) = parse_clock(value) else {
                    return Err(kit::fail(kit::tr(
                        "Time must be HH:MM, e.g. 10:30",
                        "时间要写成 HH:MM，例如 10:30",
                    )));
                };
                let fc = format_clock(at);
                let msg = if kit::is_zh() {
                    format!("开始时间已设为 {fc}")
                } else {
                    format!("Start time set to {fc}")
                };
                self.update_setting(inv, |c| c.run_time = fc.clone(), &msg).await
            }
            "range" | "r" => {
                if value.is_empty() {
                    return self
                        .update_setting(
                            inv,
                            |c| c.run_time_end.clear(),
                            kit::tr("Switched back to fixed-time check-in", "已改回固定时间签到"),
                        )
                        .await;
                }
                let Some(end) = parse_clock(value) else {
                    return Err(kit::fail(kit::tr(
                        "Time must be HH:MM, e.g. 11:30",
                        "时间要写成 HH:MM，例如 11:30",
                    )));
                };
                let fc = format_clock(end);
                let msg = if kit::is_zh() {
                    format!("结束时间已设为 {fc}，每天在这段时间里随机挑一分钟签")
                } else {
                    format!("End time set to {fc}; a random minute in this window is picked daily")
                };
                self.update_setting(inv, |c| c.run_time_end = fc.clone(), &msg).await
            }
            "delay" | "d" => {
                let Some(minutes) = value.parse::<usize>().ok().filter(|&m| m <= 60) else {
                    return Err(kit::fail(kit::tr(
                        "Random delay must be 0 to 60 minutes",
                        "随机延迟要写 0 到 60 之间的分钟数",
                    )));
                };
                let msg = if kit::is_zh() {
                    format!("随机延迟已设为 {minutes} 分钟")
                } else {
                    format!("Random delay set to {minutes} min")
                };
                self.update_setting(inv, |c| c.random_delay = minutes, &msg).await
            }
            "bot" | "b" => {
                if value.eq_ignore_ascii_case("off") {
                    return self
                        .update_setting(
                            inv,
                            |c| {
                                c.bot_token.clear();
                                c.push_chat_id.clear();
                            },
                            kit::tr("Bot push disabled", "已关闭机器人推送"),
                        )
                        .await;
                }
                let chat = inv.arg(3);
                if value.is_empty() || chat.is_empty() {
                    return Err(kit::usage(
                        inv.prefix(),
                        kit::tr(
                            "checkin set bot token chat_id|off",
                            "checkin set bot Token 对话 ID|off",
                        ),
                    ));
                }
                if !inv.message.saved {
                    return Err(kit::fail(kit::tr(
                        "Bot Token can only be configured in Saved Messages",
                        "Bot Token 只能在收藏夹中配置",
                    )));
                }
                let v = value.to_owned();
                let ch = chat.to_owned();
                let msg = if kit::is_zh() {
                    format!("机器人推送已设置：Token {}，发到 {chat}", mask(value))
                } else {
                    format!("Bot push enabled: token {}, chat {chat}", mask(value))
                };
                self.update_setting(
                    inv,
                    move |c| {
                        c.bot_token = v;
                        c.push_chat_id = ch;
                    },
                    &msg,
                )
                .await
            }
            "log" | "l" => {
                if value.is_empty() {
                    return Err(kit::usage(
                        inv.prefix(),
                        kit::tr("checkin set log chat|off", "checkin set log 对话|off"),
                    ));
                }
                let next = if value.eq_ignore_ascii_case("off") { "" } else { value };
                let note = if next.is_empty() {
                    kit::tr("Summary chat cleared", "已取消汇总对话").to_owned()
                } else if kit::is_zh() {
                    format!("汇总对话已设为 {next}")
                } else {
                    format!("Summary chat set to {next}")
                };
                let n = next.to_owned();
                self.update_setting(inv, move |c| c.log_chat = n, &note).await
            }
            _ => Err(kit::fail(if kit::is_zh() {
                format!("能设的有 time、range、delay、bot、log，{}help checkin 看用法", inv.prefix())
            } else {
                format!(
                    "Available settings: time, range, delay, bot, log; run {}help checkin for usage",
                    inv.prefix()
                )
            })),
        }
    }

    async fn update_setting<F: FnOnce(&mut Config)>(&self, inv: &Invocation, mutate: F, done: &str) -> CmdResult {
        kit::update_ok(&self.store, |cfg| {
            let before = format!("{}{}", cfg.run_time, cfg.run_time_end);
            mutate(cfg);
            if format!("{}{}", cfg.run_time, cfg.run_time_end) != before {
                cfg.planned_date.clear();
                cfg.current_run_time.clear();
            }
        })?;
        inv.edit_text(&format!("✅ {done}")).await?;
        Ok(())
    }

    async fn show(&self, inv: &Invocation) -> CmdResult {
        let cfg = self.store.read()?;
        let today = schedule_date(&cfg, SystemTime::now());
        let unset = kit::tr("Unset", "未设置");
        let lines = if kit::is_zh() {
            let schedule = if cfg.run_time_end.is_empty() {
                format!("每天 {}（北京时间）", code(&cfg.run_time))
            } else {
                format!(
                    "每天 {} 之间随机挑一分钟（北京时间）",
                    code(&format!("{} ~ {}", cfg.run_time, cfg.run_time_end))
                )
            };
            let status = if cfg.last_run_date == today {
                "今天已经签过".into()
            } else if cfg.planned_date == today && !cfg.current_run_time.is_empty() {
                format!("今天在 {} 签", code(&cfg.current_run_time))
            } else {
                "今天还没签".into()
            };
            let push = if !cfg.bot_token.is_empty() && !cfg.push_chat_id.is_empty() {
                format!(
                    "Token {}，发到 {}",
                    code(&mask(&cfg.bot_token)),
                    code(&cfg.push_chat_id)
                )
            } else {
                unset.into()
            };
            [
                "📅 <b>签到设置</b>".to_owned(),
                String::new(),
                format!("时间：{schedule}"),
                format!("状态：{status}"),
                format!("随机延迟：{}", code(&format!("{} 分钟", cfg.random_delay))),
                format!("机器人推送：{push}"),
                format!("汇总对话：{}", code(kit::or_default(&cfg.log_chat, unset))),
                format!("签到目标：{}/{} 个开启", cfg.enabled().len(), cfg.targets.len()),
            ]
        } else {
            let schedule = if cfg.run_time_end.is_empty() {
                format!("Daily at {} (UTC+8)", code(&cfg.run_time))
            } else {
                format!(
                    "Daily at a random minute in {} (UTC+8)",
                    code(&format!("{} ~ {}", cfg.run_time, cfg.run_time_end))
                )
            };
            let status = if cfg.last_run_date == today {
                "Completed today".into()
            } else if cfg.planned_date == today && !cfg.current_run_time.is_empty() {
                format!("Scheduled at {} today", code(&cfg.current_run_time))
            } else {
                "Pending today".into()
            };
            let push = if !cfg.bot_token.is_empty() && !cfg.push_chat_id.is_empty() {
                format!(
                    "Token {}, chat {}",
                    code(&mask(&cfg.bot_token)),
                    code(&cfg.push_chat_id)
                )
            } else {
                unset.into()
            };
            [
                "📅 <b>Check-in Settings</b>".to_owned(),
                String::new(),
                format!("Schedule: {schedule}"),
                format!("Status: {status}"),
                format!("Random delay: {}", code(&format!("{} min", cfg.random_delay))),
                format!("Bot push: {push}"),
                format!("Summary chat: {}", code(kit::or_default(&cfg.log_chat, unset))),
                format!("Targets: {}/{} enabled", cfg.enabled().len(), cfg.targets.len()),
            ]
        };
        inv.edit(&lines.join("\n")).await?;
        Ok(())
    }
}

struct TrackCancelGuard(CancellationToken);
impl Drop for TrackCancelGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn remove_command(inv: Option<&Invocation>) -> CmdResult {
    if let Some(i) = inv {
        i.delete().await?;
    }
    Ok(())
}

async fn send_pages(client: &Client, chat: &str, pages: &[String]) -> (usize, Option<mibot_bot::BotError>) {
    let peer = match client.resolve_target(chat).await {
        Ok(p) => p,
        Err(e) => return (0, Some(e)),
    };
    for (idx, page) in pages.iter().enumerate() {
        if let Err(err) = client.send_html(&peer, page, SendOptions::default()).await {
            return (idx, Some(err));
        }
    }
    (pages.len(), None)
}

async fn push_via_bot(
    pusher: &BotApiClient,
    endpoint: &str,
    token: &str,
    chat: &str,
    pages: &[String],
) -> (usize, Option<mibot_botapi::BotApiError>) {
    for (idx, page) in pages.iter().enumerate() {
        if let Err(err) = pusher.send_html(endpoint, token, chat, page).await {
            return (idx, Some(err));
        }
    }
    (pages.len(), None)
}

fn not_found(inv: &Invocation, id: &str) -> CmdError {
    kit::fail(if kit::is_zh() {
        format!("没有 ID 是 {id} 的签到目标，{}checkin list 可以看全部", inv.prefix())
    } else {
        format!(
            "No target with ID {id}; run {}checkin list to view all",
            inv.prefix()
        )
    })
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let c = |text: &str| format!("<code>{p}checkin{text}</code>");
    if kit::is_zh() {
        format!(
            "📅 <b>定时签到</b>\n\n每天在设定的时间（北京时间）向机器人或群组发签到命令，需要时再点一下回调按钮，最后发一份汇总。\n\n\
             <b>签到</b>\n\
             • {} 立即签到全部开启的目标，当天不再定时签\n\
             • {} 只试一个\n\
             • {} 清掉今天的执行记录，到时间会再签一次\n\n\
             <b>目标</b>\n\
             • {}，签到命令写在第二行，可以带空格\n\
             • {}、{}、{}（开启或关闭）\n\n\
             <b>设置</b>\n\
             • {} 开始时间\n\
             • {} 在开始时间到这个时间之间随机挑一分钟签，可以跨午夜；不写时间就改回固定时间\n\
             • {} 到点后再随机多等 0 到这么多分钟，最多 60\n\
             • {} 汇总改用机器人推送\n\
             • {} 汇总发到这个对话\n\
             • {} 查看设置\n\n\
             汇总默认发回执行命令的对话，定时签到时发到收藏夹。错过了时间（比如服务停着），当天稍后补上。涉及 Bot Token 的命令请在收藏夹里执行。\n\n\
             <b>示例</b>\n<pre>{p}checkin add storm Storm签到 @storm_bot data:checkin\n/sign 123456</pre>",
            c(""),
            c(" test ID"),
            c(" reset"),
            c(" add ID 名称 目标 [data:回调数据|text:按钮文字]"),
            c(" list"),
            c(" del ID"),
            c(" toggle ID"),
            c(" set time 10:00"),
            c(" set range 11:30"),
            c(" set delay 分钟数"),
            c(" set bot Token 对话 ID|off"),
            c(" set log 对话|off"),
            c(" config"),
        )
    } else {
        format!(
            "📅 <b>Scheduled Check-in</b>\n\nSend check-in commands to bots or groups at a scheduled time (UTC+8), optionally click an inline button, and send a summary.\n\n\
             <b>Check-in</b>\n\
             • {} run all enabled targets now (skips today's scheduled run)\n\
             • {} test one target\n\
             • {} clear today's run record so scheduled run can execute again\n\n\
             <b>Targets</b>\n\
             • {} (put the command on the second line; spaces allowed)\n\
             • {}, {}, {} (enable or disable)\n\n\
             <b>Settings</b>\n\
             • {} start time\n\
             • {} pick a random minute between start and end time (can cross midnight); omit time for fixed time\n\
             • {} extra random delay (0-60 min)\n\
             • {} push summary via Bot API\n\
             • {} send summary to a specific chat\n\
             • {} view settings\n\n\
             Summary goes back to the invoking chat (or Saved Messages for scheduled runs). Missed runs are caught up later the same day. Commands involving Bot Token must run in Saved Messages.\n\n\
             <b>Example</b>\n<pre>{p}checkin add storm Storm @storm_bot data:checkin\n/sign 123456</pre>",
            c(""),
            c(" test ID"),
            c(" reset"),
            c(" add ID name target [data:callback|text:button]"),
            c(" list"),
            c(" del ID"),
            c(" toggle ID"),
            c(" set time 10:00"),
            c(" set range 11:30"),
            c(" set delay minutes"),
            c(" set bot token chat_id|off"),
            c(" set log chat|off"),
            c(" config"),
        )
    }
}

pub fn register(app: &Arc<App>) {
    let svc = Arc::new(Service {
        app: Some(Arc::clone(app)),
        store: Arc::new(kit::new_store(app, "checkin.json", Config::default)),
        running: Arc::new(Mutex::new(())),
        rt: RuntimeConfig::default(),
    });
    let s_cmd = Arc::clone(&svc);
    app.registry.register(
        Command {
            name: "checkin".into(),
            group: group::ACCOUNT.into(),
            description: "Scheduled bot check-in".into(),
            usage: "[subcommand]".into(),
            help_fn: Some(Arc::new(help)),
            timeout: Some(Duration::from_secs(30 * 60)),
            handle: Arc::new(move |inv| {
                let s = Arc::clone(&s_cmd);
                Box::pin(async move { s.handle(&inv).await })
            }),
            ..Default::default()
        }
        .zh("每天定时向机器人签到", "[子命令]"),
    );

    let s_job = Arc::clone(&svc);
    app.on_ready(Arc::new(move |app, cancel| {
        let s = Arc::clone(&s_job);
        Box::pin(async move {
            loop {
                let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                let rem = 60_000 - (now_ms % 60_000);
                if !kit::sleep(&cancel, Duration::from_millis(rem)).await {
                    return;
                }
                s.tick(&cancel, &app.client, SystemTime::now()).await;
            }
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, PeerCache, RpcTransport, SentRecorder};
    use mibot_botapi::{Clock as ApiClock, Poster};
    use mibot_command::Registry;
    use mibot_httpx::{HttpxError, Response};
    use mibot_logtail::Ring;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-checkin-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_clock_matcher_pick_button_and_remaining() {
        for (val, want) in [("10:00", 600), ("9:05", 545), ("23:59", 1439), ("0:00", 0), ("10：30", 630)] {
            assert_eq!(parse_clock(val), Some(want));
        }
        for bad in ["", "24:00", "10:60", "10:5", "123:00", "ab:cd", "10"] {
            assert_eq!(parse_clock(bad), None);
        }
        assert_eq!(parse_matcher("data:checkin"), ("checkin".into(), "".into()));
        assert_eq!(parse_matcher("text: 签到"), ("".into(), "签到".into()));
        assert_eq!(parse_matcher("daily_sign"), ("daily_sign".into(), "".into()));

        let fixed = Config { run_time: "10:00".into(), run_time_end: String::new(), ..Default::default() };
        assert_eq!(pick(&fixed, |_| panic!("should not call")), "10:00");
        let ranged = Config { run_time: "10:00".into(), run_time_end: "11:30".into(), ..Default::default() };
        assert_eq!(pick(&ranged, |n| { assert_eq!(n, 90); 37 }), "10:37");
        let overnight = Config { run_time: "22:00".into(), run_time_end: "02:00".into(), ..Default::default() };
        assert_eq!(pick(&overnight, |n| n - 1), "01:59");

        let pages = vec!["一".to_owned(), "二".to_owned(), "三".to_owned()];
        assert_eq!(remaining(&pages, 0, false), pages);
        assert_eq!(
            remaining(&pages, 1, false),
            vec!["ℹ️ First 1 page(s) of check-in summary already sent. Remaining 2 page(s) below.", "二", "三"]
        );
        assert_eq!(
            remaining(&pages, 0, true),
            vec!["ℹ️ Page 1 delivery uncertain, skipped to avoid duplicates. Remaining 2 page(s) below.", "二", "三"]
        );
        assert_eq!(
            remaining(&pages, 2, true),
            vec!["ℹ️ First 2 page(s) of check-in summary already sent. Page 3 delivery uncertain, skipped to avoid duplicates."]
        );
    }

    fn sign_markup(text: &str, data: &str) -> tl::enums::ReplyMarkup {
        tl::types::ReplyInlineMarkup {
            force_reply: false,
            rows: vec![tl::types::KeyboardInlineButtonRow {
                buttons: vec![tl::types::KeyboardInlineButton {
                    style: None,
                    text: text.into(),
                    r#type: tl::enums::InlineButtonType::Callback(tl::types::InlineButtonTypeCallback {
                        requires_password: false,
                        data: data.as_bytes().to_vec(),
                    }),
                }
                .into()],
            }
            .into()],
        }
        .into()
    }

    type CbHandler = Box<dyn Fn(&str) -> Result<Option<String>, RpcError> + Send + Sync>;

    struct FakeBot {
        next_id: AtomicI32,
        sent: StdMutex<Vec<String>>,
        clicked: StdMutex<Vec<String>>,
        msgs: StdMutex<Vec<tl::types::Message>>,
        on_send: Option<Box<dyn Fn(i32) -> Option<tl::types::Message> + Send + Sync>>,
        on_cb: Option<CbHandler>,
    }

    fn sign_bot_user() -> tl::enums::User {
        tl::types::User {
            bot: true,
            bot_info_version: Some(1),
            access_hash: Some(7),
            username: Some("sign_bot".into()),
            ..kit::blank_tl_user(42)
        }
        .into()
    }

    impl RpcTransport for FakeBot {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            cid: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                if cid == tl::functions::contacts::ResolveUsername::CONSTRUCTOR_ID {
                    let req = tl::functions::contacts::ResolveUsername::from_bytes(&body[4..]).unwrap();
                    if req.username != "sign_bot" {
                        return Err(InvocationError::Rpc(RpcError {
                            code: 400, name: "USERNAME_NOT_OCCUPIED".into(), value: None, caused_by: None,
                        }));
                    }
                    let resp: tl::enums::contacts::ResolvedPeer = tl::types::contacts::ResolvedPeer {
                        peer: tl::types::PeerUser { user_id: 42 }.into(),
                        chats: vec![],
                        users: vec![sign_bot_user()],
                    }
                    .into();
                    return Ok(resp.to_bytes());
                }
                if cid == tl::functions::messages::SendMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::SendMessage::from_bytes(&body[4..]).unwrap();
                    let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
                    self.sent.lock().unwrap().push(req.message.clone());
                    let mut m = kit::blank_tl_message(id, tl::types::PeerUser { user_id: 42 }.into());
                    m.out = true;
                    m.message = req.message;
                    let mut list = self.msgs.lock().unwrap();
                    list.push(m);
                    if let Some(f) = &self.on_send
                        && let Some(reply) = f(id)
                    {
                        self.next_id.store(reply.id, Ordering::SeqCst);
                        list.push(reply);
                    }
                    let resp = tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                        out: true, id, pts: 1, pts_count: 1, date: 0, media: None, entities: None, ttl_period: None,
                    });
                    return Ok(resp.to_bytes());
                }
                if cid == tl::functions::messages::GetHistory::CONSTRUCTOR_ID {
                    let list = self.msgs.lock().unwrap();
                    let rev: Vec<tl::enums::Message> = list.iter().rev().cloned().map(Into::into).collect();
                    let resp = kit::tl_messages(rev, vec![], vec![sign_bot_user()]);
                    return Ok(resp.to_bytes());
                }
                if cid == tl::functions::messages::GetBotCallbackAnswer::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::GetBotCallbackAnswer::from_bytes(&body[4..]).unwrap();
                    let data = String::from_utf8_lossy(&req.data.unwrap_or_default()).into_owned();
                    self.clicked.lock().unwrap().push(data.clone());
                    if let Some(f) = &self.on_cb {
                        match f(&data) {
                            Ok(msg) => {
                                let resp: tl::enums::messages::BotCallbackAnswer =
                                    tl::types::messages::BotCallbackAnswer {
                                        alert: false,
                                        has_url: false,
                                        native_ui: false,
                                        message: msg,
                                        url: None,
                                        cache_time: 0,
                                    }
                                    .into();
                                return Ok(resp.to_bytes());
                            }
                            Err(e) => return Err(InvocationError::Rpc(e)),
                        }
                    }
                }
                Err(InvocationError::Dropped("unexpected"))
            })
        }
    }

    fn test_svc(dir: &Path, transport: Arc<dyn RpcTransport>) -> (Service, Arc<Client>) {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            transport,
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let svc = Service {
            app: None,
            store: Arc::new(Store::new(dir.join("checkin.json"), Config::default)),
            running: Arc::new(Mutex::new(())),
            rt: RuntimeConfig {
                reply_wait: Duration::from_millis(300),
                poll_every: Duration::from_millis(10),
                gap: Duration::from_millis(1),
                ..Default::default()
            },
        };
        (svc, client)
    }

    #[tokio::test]
    async fn test_sign_plain_and_button_and_tick() {
        // 1. Plain preserves IP
        let dir = temp_dir("plain");
        let fake = Arc::new(FakeBot {
            next_id: AtomicI32::new(10),
            sent: StdMutex::new(vec![]),
            clicked: StdMutex::new(vec![]),
            msgs: StdMutex::new(vec![]),
            on_send: Some(Box::new(|id| {
                let mut m = kit::blank_tl_message(id + 1, tl::types::PeerUser { user_id: 42 }.into());
                m.message = "签到成功\n获得 5 积分".into();
                Some(m)
            })),
            on_cb: None,
        });
        let (svc, client) = test_svc(&dir, fake.clone());
        let reply = svc
            .sign_with_attempt::<fn() -> Result<(), CmdError>>(
                &CancellationToken::new(),
                &client,
                &SignTarget {
                    id: "a".into(),
                    name: "A".into(),
                    target: "@sign_bot".into(),
                    command: "/sign 1.2.3.4 abc".into(),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(reply, "签到成功\n获得 5 积分");
        assert_eq!(*fake.sent.lock().unwrap(), vec!["/sign 1.2.3.4 abc"]);
        let _ = std::fs::remove_dir_all(dir);

        // 2. Clicks button
        let dir = temp_dir("btn");
        let fake2 = Arc::new(FakeBot {
            next_id: AtomicI32::new(10),
            sent: StdMutex::new(vec![]),
            clicked: StdMutex::new(vec![]),
            msgs: StdMutex::new(vec![]),
            on_send: Some(Box::new(|id| {
                let mut m = kit::blank_tl_message(id + 1, tl::types::PeerUser { user_id: 42 }.into());
                m.message = "请点按钮签到".into();
                m.reply_markup = Some(sign_markup("每日签到", "checkin"));
                Some(m)
            })),
            on_cb: Some(Box::new(|_| Ok(Some("签到成功 +10".into())))),
        });
        let (svc2, client2) = test_svc(&dir, fake2.clone());
        let reply2 = svc2
            .sign_with_attempt::<fn() -> Result<(), CmdError>>(
                &CancellationToken::new(),
                &client2,
                &SignTarget {
                    target: "@sign_bot".into(),
                    command: "/start".into(),
                    button_text: "每日签到".into(),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(reply2, "签到成功 +10");
        assert_eq!(*fake2.clicked.lock().unwrap(), vec!["checkin"]);
        let _ = std::fs::remove_dir_all(dir);

        // 3. Tick runs once per day
        let dir = temp_dir("tick");
        let fake3 = Arc::new(FakeBot {
            next_id: AtomicI32::new(10),
            sent: StdMutex::new(vec![]),
            clicked: StdMutex::new(vec![]),
            msgs: StdMutex::new(vec![]),
            on_send: Some(Box::new(|id| {
                let mut m = kit::blank_tl_message(id + 1, tl::types::PeerUser { user_id: 42 }.into());
                m.message = "OK".into();
                Some(m)
            })),
            on_cb: None,
        });
        let (svc3, client3) = test_svc(&dir, fake3.clone());
        kit::update_ok(&svc3.store, |c| {
            c.run_time = "10:00".into();
            c.run_time_end.clear();
            c.targets = vec![
                SignTarget { id: "a".into(), name: "A".into(), target: "@sign_bot".into(), command: "/sign".into(), enabled: true, ..Default::default() },
                SignTarget { id: "b".into(), name: "B".into(), target: "@sign_bot".into(), command: "/off".into(), enabled: false, ..Default::default() },
            ];
        })
        .unwrap();
        let day_0959 = shanghai_offset().with_ymd_and_hms(2026, 9, 29, 9, 59, 0).unwrap();
        let day_1007 = shanghai_offset().with_ymd_and_hms(2026, 9, 29, 10, 7, 0).unwrap();
        let day_1008 = shanghai_offset().with_ymd_and_hms(2026, 9, 29, 10, 8, 0).unwrap();
        let to_st = |dt: DateTime<FixedOffset>| UNIX_EPOCH + Duration::from_secs(dt.timestamp() as u64);
        let tok = CancellationToken::new();
        svc3.tick(&tok, &client3, to_st(day_0959)).await;
        assert!(fake3.sent.lock().unwrap().is_empty());
        assert_eq!(svc3.store.read().unwrap().current_run_time, "10:00");
        svc3.tick(&tok, &client3, to_st(day_1007)).await;
        svc3.tick(&tok, &client3, to_st(day_1008)).await;
        let sent = fake3.sent.lock().unwrap();
        let signs: Vec<_> = sent
            .iter()
            .filter(|s| !s.contains("Check-in Summary") && !s.contains("签到汇总"))
            .collect();
        assert_eq!(signs.len(), 1);
        assert_eq!(svc3.store.read().unwrap().last_run_date, "2026/9/29");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_deliver_uncertain_and_saved_messages_guard() {
        struct MockClock;
        impl ApiClock for MockClock {
            fn now(&self) -> std::time::Instant { std::time::Instant::now() }
            fn sleep(&self, _d: Duration) -> BoxFuture<'static, ()> { Box::pin(async {}) }
        }
        struct MockPoster(AtomicUsize);
        impl Poster for MockPoster {
            fn post_json<'a>(
                &'a self,
                _url: String,
                _body: Vec<u8>,
                _t: Duration,
                _l: usize,
            ) -> BoxFuture<'a, Result<Response, HttpxError>> {
                Box::pin(async move {
                    let n = self.0.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        Ok(Response { status: 200, body: br#"{"ok":true}"#.to_vec() })
                    } else {
                        Ok(Response { status: 500, body: br#"{"ok":false,"error_code":500}"#.to_vec() })
                    }
                })
            }
        }
        let dir = temp_dir("deliv");
        let texts = Arc::new(StdMutex::new(Vec::<String>::new()));
        struct LogCapture(Arc<StdMutex<Vec<String>>>);
        impl RpcTransport for LogCapture {
            fn invoke_raw<'a>(&'a self, _: Option<i32>, cid: u32, body: Vec<u8>) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                Box::pin(async move {
                    if cid == tl::functions::messages::SendMessage::CONSTRUCTOR_ID {
                        let req = tl::functions::messages::SendMessage::from_bytes(&body[4..]).unwrap();
                        let mut g = self.0.lock().unwrap();
                        g.push(req.message);
                        let resp = tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                            out: true, id: g.len() as i32, pts: 1, pts_count: 1, date: 0, media: None, entities: None, ttl_period: None,
                        });
                        return Ok(resp.to_bytes());
                    }
                    Err(InvocationError::Dropped("unexpected"))
                })
            }
        }
        let peers = Arc::new(PeerCache::new());
        peers.set_self(100);
        peers.remember_users(&[tl::types::User {
            access_hash: Some(1),
            ..kit::blank_tl_user(200)
        }
        .into()]);
        let client = Arc::new(Client::from_transport(
            Arc::new(LogCapture(Arc::clone(&texts))),
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let svc = Service {
            app: None,
            store: Arc::new(Store::new(dir.join("checkin.json"), Config::default)),
            running: Arc::new(Mutex::new(())),
            rt: RuntimeConfig {
                bot_pusher: Arc::new(BotApiClient::with_backends(
                    Arc::new(MockClock),
                    Arc::new(MockPoster(AtomicUsize::new(0))),
                    Arc::new(|| 0),
                )),
                ..Default::default()
            },
        };
        let summary = "x".repeat(PAGE_LIMIT + 100);
        svc.deliver(
            &client,
            &Config { bot_token: "TEST".into(), push_chat_id: "200".into(), log_chat: "200".into(), ..Default::default() },
            &summary,
            None,
        )
        .await
        .unwrap();
        {
            let sent = texts.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert!(sent[0].starts_with("ℹ️ First 1 page(s) of check-in summary already sent"));
        }

        // Refuses bot token outside saved messages
        let dummy_peers = Arc::new(PeerCache::new());
        let dummy_app = App::for_testing(
            dir.clone(),
            "t",
            "b",
            Arc::clone(&client),
            dummy_peers,
            Arc::new(Ring::new()),
        );
        let dummy_inv = Invocation {
            client: Arc::clone(&dummy_app.client),
            registry: Arc::new(Registry::new()),
            message: kit::blank_bot_message(1, tl::types::PeerUser { user_id: 200 }.into()),
            prefix: ".".into(),
            command: "checkin".into(),
            args: vec!["set".into(), "bot".into(), "test-key".into(), "123".into()],
            raw: "set bot test-key 123".into(),
            cancel: CancellationToken::new(),
            foreign: false,
        };
        let err = svc.set(&dummy_inv).await.unwrap_err();
        assert!(kit::is_user_error(err.as_ref()) && err.to_string().contains("Saved Messages"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
