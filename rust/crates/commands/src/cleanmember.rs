use crate::kit;
use chrono::{DateTime, SecondsFormat};
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BoxFuture, Client, DialogPages, MediaOptions};
use mibot_command::{CmdError, CmdResult, Command, Invocation, brief, escape, group, html_pages, PAGE_LIMIT};
use mibot_fsutil::write_file_atomic;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{FileTimes, OpenOptions};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

const PAGE_SIZE: i32 = 200;
const MAX_OFFSET: i32 = 50000;
const FLOOD_ATTEMPTS: usize = 3;
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_LIMIT: usize = 50;

type NowFn = Arc<dyn Fn() -> SystemTime + Send + Sync>;
type PauseFn = Arc<dyn Fn(CancellationToken, Duration) -> BoxFuture<'static, bool> + Send + Sync>;
type GapFn = Arc<dyn Fn() -> Duration + Send + Sync>;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct UserInfo {
    id: String,
    username: String,
    first_name: String,
    last_name: String,
    is_deleted: bool,
    last_online: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    error_message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct CacheData {
    chat_id: String,
    chat_title: String,
    mode: String,
    day: i64,
    search_time: String,
    total_found: usize,
    users: Vec<UserInfo>,
    #[serde(rename = "expiresAt")]
    expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    mode: String,
    day: i64,
    search: bool,
    limit: usize,
    chat: String,
}

#[derive(Clone)]
struct Group {
    peer: tl::enums::InputPeer,
    channel: Option<tl::types::InputChannel>,
    basic: i64,
    chat_id: String,
    title: String,
    broadcast: bool,
}

struct Member {
    user: tl::types::User,
    admin: bool,
}

#[derive(Default)]
struct Outcome {
    scanned: usize,
    found: Vec<UserInfo>,
    failed: Vec<UserInfo>,
    removed: usize,
    skipped: usize,
    limited: bool,
    reasons: HashMap<String, usize>,
}

struct Cache {
    dir: PathBuf,
}

fn cache_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+_[1-5]_\d+$").expect("valid cache key regex"))
}

fn formula_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[=+\-@\t\r]").expect("valid formula regex"))
}

fn cache_key(chat_id: &str, mode: &str, day: i64) -> String {
    format!("{chat_id}_{mode}_{day}")
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn iso_time(t: SystemTime) -> String {
    let dt: DateTime<chrono::Utc> = t.into();
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl Cache {
    fn path(&self, key: &str) -> Option<PathBuf> {
        cache_key_re().is_match(key).then(|| self.dir.join(format!("{key}.json")))
    }

    fn load(&self, key: &str, now: SystemTime) -> Option<CacheData> {
        let path = self.path(key)?;
        let raw = std::fs::read(path).ok()?;
        let data: CacheData = serde_json::from_slice(&raw).ok()?;
        (data.expires_at > unix_ms(now)).then_some(data)
    }

    fn save(&self, key: &str, data: &CacheData, now: SystemTime) -> Result<(), CmdError> {
        let Some(path) = self.path(key) else {
            return Err(format!("invalid cache key {key}").into());
        };
        let raw = serde_json::to_vec(data)?;
        write_file_atomic(&path, &raw, 0o600)?;
        let _ = OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_times(FileTimes::new().set_modified(now)));
        let _ = self.prune(now);
        Ok(())
    }

    fn prune(&self, now: SystemTime) -> std::io::Result<()> {
        let entries = std::fs::read_dir(&self.dir)?;
        let mut kept: Vec<(PathBuf, SystemTime)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if path.is_dir() || !cache_key_re().is_match(stem) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(modified) = meta.modified() else { continue };
            if now.duration_since(modified).unwrap_or(Duration::ZERO) >= CACHE_TTL {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            kept.push((path, modified));
        }
        if kept.len() <= CACHE_LIMIT {
            return Ok(());
        }
        kept.sort_by_key(|a| a.1);
        for (old, _) in &kept[..kept.len() - CACHE_LIMIT] {
            let _ = std::fs::remove_file(old);
        }
        Ok(())
    }

    fn clear_chat(&self, chat_id: &str) -> std::io::Result<()> {
        let prefix = format!("{chat_id}_");
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with(&prefix)
                && name.ends_with(".json")
                && let Err(err) = std::fs::remove_file(&path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                return Err(err);
            }
        }
        Ok(())
    }
}

struct Service {
    cache: Cache,
    logs: Option<Arc<mibot_logtail::Ring>>,
    now: NowFn,
    pause: PauseFn,
    gap: GapFn,
    progress_every: Duration,
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    let c = |text: &str| format!("<code>{p}clean_member{text}</code>");
    format!(
        "🧽 <b>群成员搜索与清理</b>\n\n\
         按最后上线时间、发言情况或账号状态找出群组成员，生成 CSV 报告，或者直接移出。\n\n\
         <b>格式</b>\n{}\n\n\
         <b>模式</b>\n\
         • <code>1 天数</code> 最后上线超过这么多天；看不到上线时间的跳过\n\
         • <code>2 天数</code> 这么多天里没有发言\n\
         • <code>3 条数</code> 能搜到的发言少于这么多条\n\
         • <code>4</code> 已注销的账号\n\
         • <code>5</code> 所有普通成员\n\
         模式 1、2 的天数少于 7 时按 7 天算。\n\n\
         <b>选项</b>\n\
         • <code>search</code> 只搜索、出报告，不移出；不写就直接移出\n\
         • <code>limit:50</code> 最多移出 50 人，只对移出起作用\n\
         • <code>chat:-1001234567890</code> 指定群组，也可以写 @用户名；不写就是当前群组\n\n\
         <b>示例</b>\n\
         • {} 看看超过 30 天没上线的成员\n\
         • {} 重新查一遍，移出其中 10 人\n\
         • {} 搜索指定群组里的注销账号\n\
         • {} 搜索发言少于 5 条的成员\n\n\
         <b>说明</b>\n\
         • 移出要群主身份或封禁成员权限；搜索只要看得到成员列表\n\
         • 管理员和你自己不会被列入\n\
         • 移出是封禁后马上解封，对方还能重新加入\n\
         • 同一群组、模式和参数的搜索结果缓存 24 小时；移出总是重新查，查完清掉这个群组的缓存\n\
         • 报告以 CSV 文件发到收藏夹，不会出现在群组里\n\
         • 上线时间和发言统计受对方隐私设置和 Telegram 的可见范围限制，成员很多时 Telegram 可能只给出一部分\n\
         • 频道的订阅者不能发言，模式 2、3 只能用于群组\n\
         • 参数写错会直接报错，不会当成移出执行",
        c(" 模式 [参数] [chat:对话 ID] [limit:数量] [search]"),
        c(" 1 30 search"),
        c(" 1 30 limit:10"),
        c(" 4 chat:-1001234567890 search"),
        c(" 3 5 search"),
    )
}

fn mode_name(mode: &str, n: i64) -> String {
    match mode {
        "1" => format!("未上线超过 {n} 天的用户"),
        "2" => format!("未发言超过 {n} 天的用户"),
        "3" => format!("发言少于 {n} 条的用户"),
        "4" => "已注销的账号".into(),
        "5" => "所有普通成员".into(),
        _ => "未知".into(),
    }
}

fn option<'a>(arg: &'a str, key: &str) -> Option<&'a str> {
    if arg.len() < key.len() || !arg.is_char_boundary(key.len()) {
        return None;
    }
    let (head, tail) = arg.split_at(key.len());
    if !head.eq_ignore_ascii_case(key) {
        return None;
    }
    tail.strip_prefix(':').or_else(|| tail.strip_prefix('：'))
}

fn parse_request(args: &[String], prefix: &str) -> Result<Request, CmdError> {
    let mode = args[0].to_ascii_lowercase();
    if mode.len() != 1 || !(mode.as_bytes()[0] >= b'1' && mode.as_bytes()[0] <= b'5') {
        return Err(kit::fail(format!(
            "没有模式 {}，模式是 1 到 5，{prefix}clean_member help 看说明",
            args[0]
        )));
    }
    let mut rest = &args[1..];
    let mut day = 0i64;
    if mode.as_str() <= "3" {
        let what = if mode == "3" { "发言条数" } else { "天数" };
        if rest.is_empty() {
            return Err(kit::fail(format!("模式 {mode} 后面要写{what}")));
        }
        let parsed = rest[0].parse::<i64>().ok().filter(|&v| v >= 1 && kit::is_digits(&rest[0]));
        let Some(mut d) = parsed else {
            return Err(kit::fail(format!("{what}要是正整数，{} 不是", rest[0])));
        };
        if mode != "3" {
            d = d.max(7);
        }
        day = d;
        rest = &rest[1..];
    }
    let mut req = Request {
        mode,
        day,
        search: false,
        limit: 0,
        chat: String::new(),
    };
    for arg in rest {
        if arg.eq_ignore_ascii_case("search") {
            req.search = true;
            continue;
        }
        if let Some(val) = option(arg, "limit") {
            let parsed = val.parse::<usize>().ok().filter(|&v| v >= 1 && kit::is_digits(val));
            let Some(limit) = parsed else {
                return Err(kit::fail("limit 要是正整数，如 limit:50"));
            };
            req.limit = limit;
            continue;
        }
        if let Some(val) = option(arg, "chat") {
            if val.is_empty() {
                return Err(kit::fail("chat: 后面要写对话 ID 或 @用户名"));
            }
            req.chat = val.to_owned();
            continue;
        }
        return Err(kit::fail(format!(
            "不认识的参数 {arg}，{prefix}clean_member help 看说明"
        )));
    }
    Ok(req)
}

async fn resolve_group(
    client: &Client,
    message: &mibot_bot::Message,
    target: &str,
) -> Result<Group, CmdError> {
    let peer = if target.is_empty() {
        client.input_peer(&message.peer)?
    } else {
        let mut res = client.resolve_target(target).await;
        if let Err(err) = &res
            && err.is_unaddressable_peer()
        {
            for folder in [0, 1] {
                client
                    .each_dialog(DialogPages { folder, max_pages: 20 }, |_| {})
                    .await
                    .map_err(|e| kit::fail_with("读取对话列表失败", e))?;
                res = client.resolve_target(target).await;
                if res.is_ok() {
                    break;
                }
            }
        }
        match res {
            Ok(p) => p,
            Err(err) => {
                if err.is_unaddressable_peer() {
                    return Err(kit::fail(format!(
                        "找不到对话 {target}：账号不在这个群组里，或者 ID 写错了"
                    )));
                }
                return Err(kit::fail_with(format!("找不到对话 {target}"), err));
            }
        }
    };
    describe(client, peer)
}

fn describe(client: &Client, peer: tl::enums::InputPeer) -> Result<Group, CmdError> {
    match peer.clone() {
        tl::enums::InputPeer::Channel(c) => {
            let broadcast = client.peers().channel(c.channel_id).is_some_and(|i| i.broadcast);
            let title = client
                .peers()
                .title(&tl::types::PeerChannel { channel_id: c.channel_id }.into());
            let ch = tl::types::InputChannel {
                channel_id: c.channel_id,
                access_hash: c.access_hash,
            };
            Ok(Group {
                chat_id: c.channel_id.to_string(),
                peer,
                channel: Some(ch),
                basic: 0,
                title,
                broadcast,
            })
        }
        tl::enums::InputPeer::Chat(c) => {
            let title = client
                .peers()
                .title(&tl::types::PeerChat { chat_id: c.chat_id }.into());
            Ok(Group {
                chat_id: c.chat_id.to_string(),
                peer,
                channel: None,
                basic: c.chat_id,
                title,
                broadcast: false,
            })
        }
        _ => Err(kit::fail("只能用于群组：在群组里用，或者用 chat:对话 ID 指定")),
    }
}

fn participant_user(p: &tl::enums::ChannelParticipant) -> Option<(i64, bool)> {
    match p {
        tl::enums::ChannelParticipant::Participant(x) => Some((x.user_id, false)),
        tl::enums::ChannelParticipant::ParticipantSelf(x) => Some((x.user_id, false)),
        tl::enums::ChannelParticipant::Admin(x) => Some((x.user_id, true)),
        _ => None,
    }
}

fn participant_info(p: &tl::enums::ChannelParticipant) -> Option<(i64, bool)> {
    match p {
        tl::enums::ChannelParticipant::Participant(x) => Some((x.user_id, false)),
        tl::enums::ChannelParticipant::ParticipantSelf(x) => Some((x.user_id, false)),
        tl::enums::ChannelParticipant::Admin(x) => Some((x.user_id, true)),
        tl::enums::ChannelParticipant::Creator(x) => Some((x.user_id, true)),
        _ => None,
    }
}

fn users_by_id(users: &[tl::enums::User]) -> HashMap<i64, tl::types::User> {
    let mut map = HashMap::with_capacity(users.len());
    for entry in users {
        if let tl::enums::User::User(u) = entry {
            map.insert(u.id, u.clone());
        }
    }
    map
}

fn last_seen_days(status: Option<&tl::enums::UserStatus>, now: SystemTime) -> Option<i64> {
    match status? {
        tl::enums::UserStatus::Online(_) | tl::enums::UserStatus::Recently(_) => Some(0),
        tl::enums::UserStatus::Offline(o) => {
            if o.was_online == 0 {
                return None;
            }
            let diff = unix_secs(now) - i64::from(o.was_online);
            Some(diff / 86400)
        }
        tl::enums::UserStatus::LastWeek(_) => Some(7),
        tl::enums::UserStatus::LastMonth(_) => Some(30),
        tl::enums::UserStatus::Empty => None,
    }
}

fn last_online(status: Option<&tl::enums::UserStatus>) -> Option<String> {
    match status? {
        tl::enums::UserStatus::Offline(o) => {
            if o.was_online == 0 {
                Some("offline".into())
            } else {
                Some(iso_time(UNIX_EPOCH + Duration::from_secs(o.was_online as u64)))
            }
        }
        tl::enums::UserStatus::Online(_) => Some("online".into()),
        tl::enums::UserStatus::Recently(_) => Some("recently".into()),
        tl::enums::UserStatus::LastWeek(_) => Some("lastweek".into()),
        tl::enums::UserStatus::LastMonth(_) => Some("lastmonth".into()),
        tl::enums::UserStatus::Empty => Some("empty".into()),
    }
}

fn info_of(u: &tl::types::User) -> UserInfo {
    UserInfo {
        id: u.id.to_string(),
        username: u.username.clone().unwrap_or_default(),
        first_name: u.first_name.clone().unwrap_or_default(),
        last_name: u.last_name.clone().unwrap_or_default(),
        is_deleted: u.deleted,
        last_online: last_online(u.status.as_ref()),
        error_message: String::new(),
    }
}

fn csv_cell(value: &str) -> String {
    let safe = if formula_start_re().is_match(value) {
        format!("'{value}")
    } else {
        value.to_owned()
    };
    format!("\"{}\"", safe.replace('"', "\"\""))
}

fn csv_report(d: &CacheData, failed: bool) -> Vec<u8> {
    let heading = if failed {
        "群组清理失败用户报告"
    } else {
        "群组清理报告"
    };
    let mut rows: Vec<Vec<String>> = vec![
        vec![heading.into()],
        vec!["群组名称".into(), d.chat_title.clone()],
        vec!["群组ID".into(), d.chat_id.clone()],
        vec!["清理条件".into(), mode_name(&d.mode, d.day)],
        vec!["搜索时间".into(), d.search_time.clone()],
        vec!["符合条件用户数量".into(), d.total_found.to_string()],
        vec![],
    ];
    let mut header = vec![
        "用户ID".to_owned(),
        "用户名".to_owned(),
        "姓名".to_owned(),
        "最后上线时间".to_owned(),
        "是否注销".to_owned(),
    ];
    if failed {
        header.push("失败原因".into());
    }
    rows.push(header);
    for user in &d.users {
        let online = user
            .last_online
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("未知")
            .to_owned();
        let deleted = if user.is_deleted { "是" } else { "否" }.to_owned();
        let name = format!("{} {}", user.first_name, user.last_name).trim().to_owned();
        let mut row = vec![
            user.id.clone(),
            user.username.clone(),
            name,
            online,
            deleted,
        ];
        if failed {
            row.push(user.error_message.clone());
        }
        rows.push(row);
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|row| row.iter().map(|v| csv_cell(v)).collect::<Vec<_>>().join(","))
        .collect();
    format!("\u{feff}{}", lines.join("\n")).into_bytes()
}

fn report_name(d: &CacheData, failed: bool, now: SystemTime) -> String {
    let kind = if failed { "failed" } else { "report" };
    format!("{kind}_{}_{}_{}_{}.csv", d.chat_id, d.mode, d.day, unix_ms(now))
}

fn verb(req: &Request) -> &'static str {
    if req.search { "搜索" } else { "清理" }
}

fn progress_text(g: &Group, req: &Request, out: &Outcome) -> String {
    let mut text = format!(
        "{}\n已扫描 {} 人，符合条件 {} 人",
        kit::working(&format!(
            "正在{} {} 里{}",
            verb(req),
            g.title,
            mode_name(&req.mode, req.day)
        )),
        out.scanned,
        out.found.len()
    );
    if !req.search {
        text.push_str(&format!("，已移出 {} 人", out.removed));
    }
    text
}

fn top_reasons(reasons: &HashMap<String, usize>, n: usize) -> String {
    let mut keys: Vec<&String> = reasons.keys().collect();
    keys.sort_by(|a, b| reasons[*b].cmp(&reasons[*a]).then_with(|| a.cmp(b)));
    keys.truncate(n);
    keys.into_iter()
        .map(|k| format!("{k}×{}", reasons[k]))
        .collect::<Vec<_>>()
        .join("、")
}

fn kick_rights(until_date: i32) -> tl::types::ChatBannedRights {
    tl::types::ChatBannedRights {
        view_messages: true,
        send_messages: true,
        send_media: true,
        send_stickers: true,
        send_gifs: true,
        send_games: true,
        send_inline: true,
        send_polls: true,
        change_info: true,
        invite_users: true,
        pin_messages: true,
        until_date,
        ..kit::default_banned_rights()
    }
}

fn failure_reason(err: &mibot_bot::BotError) -> String {
    if err.is_rpc("CHAT_ADMIN_REQUIRED") {
        return "权限不足".into();
    }
    if err.flood_wait().is_some() {
        return "请求频率受限".into();
    }
    format!("移出失败（{}）", brief(err))
}

impl Service {
    async fn retry_flood<F, Fut, T>(&self, cancel: &CancellationToken, mut call: F) -> Result<T, mibot_bot::BotError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, mibot_bot::BotError>>,
    {
        for attempt in 1..=FLOOD_ATTEMPTS {
            match call().await {
                Ok(v) => return Ok(v),
                Err(err) => {
                    let Some(wait) = err.flood_wait() else {
                        return Err(err);
                    };
                    if attempt >= FLOOD_ATTEMPTS || cancel.is_cancelled() {
                        return Err(err);
                    }
                    if !(self.pause)(cancel.clone(), wait + Duration::from_secs(1)).await {
                        return Err(mibot_bot::BotError::Cancelled);
                    }
                }
            }
        }
        unreachable!()
    }

    async fn basic_members(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        chat_id: i64,
    ) -> Result<Vec<Member>, CmdError> {
        let tl::enums::messages::ChatFull::Full(full) = self
            .retry_flood(cancel, || async move {
                let req = tl::functions::messages::GetFullChat { chat_id };
                client.invoke(&req).await
            })
            .await?;
        client.peers().remember_entities(&full.users, &full.chats);
        let tl::enums::ChatFull::Full(info) = full.full_chat else {
            return Err(kit::fail("读不到这个群组的成员列表"));
        };
        let tl::enums::ChatParticipants::Participants(list) = info.participants else {
            return Err(kit::fail("看不到这个群组的成员列表"));
        };
        let users = users_by_id(&full.users);
        let mut members = Vec::new();
        for participant in list.participants {
            let (uid, admin) = match participant {
                tl::enums::ChatParticipant::Creator(c) => (c.user_id, true),
                tl::enums::ChatParticipant::Admin(a) => (a.user_id, true),
                tl::enums::ChatParticipant::Participant(p) => (p.user_id, false),
            };
            if let Some(user) = users.get(&uid) {
                members.push(Member {
                    user: user.clone(),
                    admin,
                });
            }
        }
        Ok(members)
    }

    async fn can_ban(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        g: &Group,
    ) -> Result<bool, CmdError> {
        let Some(channel) = &g.channel else {
            let members = self.basic_members(cancel, client, g.basic).await?;
            return Ok(members
                .iter()
                .find(|m| m.user.id == client.self_id())
                .is_some_and(|m| m.admin));
        };
        let tl::enums::channels::ChannelParticipant::Participant(res) = client
            .invoke(&tl::functions::channels::GetParticipant {
                channel: channel.clone().into(),
                participant: tl::enums::InputPeer::PeerSelf,
            })
            .await?;
        client.peers().remember_users(&res.users);
        Ok(match res.participant {
            tl::enums::ChannelParticipant::Creator(_) => true,
            tl::enums::ChannelParticipant::Admin(a) => {
                let tl::enums::ChatAdminRights::Rights(r) = a.admin_rights;
                r.ban_users
            }
            _ => false,
        })
    }

    async fn admins(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        g: &Group,
    ) -> Result<HashSet<i64>, CmdError> {
        let mut ids = HashSet::new();
        let Some(channel) = &g.channel else {
            return Ok(ids);
        };
        let mut offset = 0i32;
        loop {
            let res = self
                .retry_flood(cancel, || async move {
                    let req = tl::functions::channels::GetParticipants {
                        channel: channel.clone().into(),
                        filter: tl::enums::ChannelParticipantsFilter::ChannelParticipantsAdmins,
                        offset,
                        limit: PAGE_SIZE,
                        hash: 0,
                    };
                    client.invoke(&req).await
                })
                .await?;
            let tl::enums::channels::ChannelParticipants::Participants(list) = res else {
                return Ok(ids);
            };
            client.peers().remember_users(&list.users);
            for p in &list.participants {
                if let Some((id, true)) = participant_info(p) {
                    ids.insert(id);
                }
            }
            if (list.participants.len() as i32) < PAGE_SIZE {
                return Ok(ids);
            }
            if offset >= MAX_OFFSET {
                return Err("admin list exceeds the supported size".into());
            }
            offset += PAGE_SIZE;
        }
    }

    async fn page(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        g: &Group,
        offset: i32,
    ) -> Result<(Vec<Member>, usize), CmdError> {
        let Some(channel) = &g.channel else {
            if offset > 0 {
                return Ok((Vec::new(), 0));
            }
            let members = self.basic_members(cancel, client, g.basic).await?;
            let n = members.len();
            return Ok((members, n));
        };
        let res = self
            .retry_flood(cancel, || async move {
                let req = tl::functions::channels::GetParticipants {
                    channel: channel.clone().into(),
                    filter: tl::enums::ChannelParticipantsFilter::ChannelParticipantsRecent,
                    offset,
                    limit: PAGE_SIZE,
                    hash: 0,
                };
                client.invoke(&req).await
            })
            .await?;
        let tl::enums::channels::ChannelParticipants::Participants(list) = res else {
            return Ok((Vec::new(), 0));
        };
        client.peers().remember_users(&list.users);
        let users = users_by_id(&list.users);
        let mut members = Vec::new();
        for p in &list.participants {
            if let Some((id, admin)) = participant_info(p)
                && let Some(user) = users.get(&id)
            {
                members.push(Member {
                    user: user.clone(),
                    admin,
                });
            }
        }
        Ok((members, list.participants.len()))
    }

    async fn count_messages(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        g: &Group,
        req: &Request,
        user: &tl::types::User,
    ) -> Result<i32, mibot_bot::BotError> {
        let mut min_date = 0i32;
        if req.mode == "2" {
            let now = unix_secs((self.now)());
            const DAY_SECS: i64 = 86400;
            if req.day <= now / DAY_SECS {
                min_date = (now - req.day * DAY_SECS) as i32;
            }
        }
        let search = tl::functions::messages::Search {
            peer: g.peer.clone(),
            q: String::new(),
            from_id: Some(
                tl::types::InputPeerUser {
                    user_id: user.id,
                    access_hash: user.access_hash.unwrap_or(0),
                }
                .into(),
            ),
            saved_peer_id: None,
            saved_reaction: None,
            top_msg_id: None,
            filter: tl::enums::MessagesFilter::InputMessagesFilterEmpty,
            min_date,
            max_date: 0,
            offset_id: 0,
            add_offset: 0,
            limit: 1,
            max_id: 0,
            min_id: 0,
            hash: 0,
        };
        let res = self.retry_flood(cancel, || client.invoke(&search)).await?;
        let count = match res {
            tl::enums::messages::Messages::Messages(m) => m.messages.len() as i32,
            tl::enums::messages::Messages::Slice(m) => m.count,
            tl::enums::messages::Messages::ChannelMessages(m) => m.count,
            tl::enums::messages::Messages::NotModified(m) => m.count,
        };
        Ok(count)
    }

    async fn matches(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        g: &Group,
        req: &Request,
        user: &tl::types::User,
    ) -> Result<bool, mibot_bot::BotError> {
        match req.mode.as_str() {
            "1" => {
                let days = last_seen_days(user.status.as_ref(), (self.now)());
                return Ok(days.is_some_and(|d| d > req.day));
            }
            "4" => return Ok(user.deleted),
            "5" => return Ok(true),
            _ => {}
        }
        let count = self.count_messages(cancel, client, g, req, user).await?;
        if req.mode == "2" {
            Ok(count == 0)
        } else {
            Ok(i64::from(count) < req.day)
        }
    }

    async fn remove(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        g: &Group,
        user: &tl::types::User,
    ) -> Result<(), mibot_bot::BotError> {
        let Some(channel) = &g.channel else {
            let res = self
                .retry_flood(cancel, || async move {
                    let req = tl::functions::messages::DeleteChatUser {
                        revoke_history: false,
                        chat_id: g.basic,
                        user_id: tl::types::InputUser {
                            user_id: user.id,
                            access_hash: user.access_hash.unwrap_or(0),
                        }
                        .into(),
                    };
                    client.invoke(&req).await
                })
                .await;
            return match res {
                Err(err) if err.is_rpc("USER_NOT_PARTICIPANT") => Ok(()),
                other => other.map(|_| ()),
            };
        };
        let target: tl::enums::InputPeer = tl::types::InputPeerUser {
            user_id: user.id,
            access_hash: user.access_hash.unwrap_or(0),
        }
        .into();
        let until = (unix_secs((self.now)()) + 60) as i32;
        let ban_res = self
            .retry_flood(cancel, || {
                let channel = channel.clone();
                let target = target.clone();
                async move {
                    let req = tl::functions::channels::EditBanned {
                        channel: channel.into(),
                        participant: target,
                        banned_rights: kick_rights(until).into(),
                    };
                    client.invoke(&req).await
                }
            })
            .await;
        match ban_res {
            Err(err) if err.is_rpc("USER_NOT_PARTICIPANT") => return Ok(()),
            Err(err) => return Err(err),
            Ok(_) => {}
        }
        if !(self.pause)(cancel.clone(), Duration::from_secs(2)).await {
            return Err(mibot_bot::BotError::Cancelled);
        }
        let unban_res = self
            .retry_flood(cancel, || {
                let channel = channel.clone();
                let target = target.clone();
                async move {
                    let req = tl::functions::channels::EditBanned {
                        channel: channel.into(),
                        participant: target,
                        banned_rights: kit::default_banned_rights().into(),
                    };
                    client.invoke(&req).await
                }
            })
            .await;
        if let Err(err) = unban_res
            && !cancel.is_cancelled()
        {
            kit::opt_log(
                self.logs.as_ref(),
                mibot_logtail::Level::Warn,
                "clean_member.unban_failed",
                &[("user", &user.id.to_string()), ("error", &err.to_string())],
            );
        }
        Ok(())
    }

    async fn scan(
        &self,
        inv: &Invocation,
        g: &Group,
        req: &Request,
        admins: &HashSet<i64>,
    ) -> Result<Outcome, CmdError> {
        let mut out = Outcome::default();
        let mut seen = HashSet::new();
        let mut last_progress = (self.now)();
        let mut offset = 0i32;

        while offset <= MAX_OFFSET {
            if inv.cancel.is_cancelled() {
                return Err(kit::fail("操作已取消"));
            }
            let (members, count) = self
                .page(&inv.cancel, &inv.client, g, offset)
                .await
                .map_err(|e| {
                    if inv.cancel.is_cancelled() {
                        kit::fail("操作已取消")
                    } else {
                        kit::fail_with("读取成员列表失败", mibot_command::UserError::new(e.to_string()))
                    }
                })?;
            let mut removed_here = 0usize;
            for m in members {
                if inv.cancel.is_cancelled() {
                    return Err(kit::fail("操作已取消"));
                }
                if !seen.insert(m.user.id) {
                    continue;
                }
                out.scanned += 1;
                if m.admin || admins.contains(&m.user.id) || m.user.id == inv.client.self_id() || m.user.is_self {
                    continue;
                }
                let hit = match self.matches(&inv.cancel, &inv.client, g, req, &m.user).await {
                    Ok(h) => h,
                    Err(_) => {
                        if inv.cancel.is_cancelled() {
                            return Err(kit::fail("操作已取消"));
                        }
                        out.skipped += 1;
                        continue;
                    }
                };
                if !hit {
                    continue;
                }
                let mut info = info_of(&m.user);
                out.found.push(info.clone());
                if !req.search {
                    match self.remove(&inv.cancel, &inv.client, g, &m.user).await {
                        Err(err) => {
                            if inv.cancel.is_cancelled() {
                                return Err(kit::fail("操作已取消"));
                            }
                            let reason = failure_reason(&err);
                            info.error_message = reason.clone();
                            out.failed.push(info);
                            *out.reasons.entry(reason).or_insert(0) += 1;
                        }
                        Ok(()) => {
                            out.removed += 1;
                            removed_here += 1;
                            if req.limit > 0 && out.removed >= req.limit {
                                out.limited = true;
                                return Ok(out);
                            }
                            if !(self.pause)(inv.cancel.clone(), (self.gap)()).await {
                                return Err(kit::fail("操作已取消"));
                            }
                        }
                    }
                }
                let now = (self.now)();
                if now.duration_since(last_progress).unwrap_or(Duration::ZERO) >= self.progress_every {
                    last_progress = now;
                    let _ = inv.edit_text(&progress_text(g, req, &out)).await;
                }
            }
            if (count as i32) < PAGE_SIZE || g.channel.is_none() {
                break;
            }
            offset += (count as i32) - (removed_here as i32);
            let now = (self.now)();
            if now.duration_since(last_progress).unwrap_or(Duration::ZERO) >= self.progress_every {
                last_progress = now;
                let _ = inv.edit_text(&progress_text(g, req, &out)).await;
            }
        }
        Ok(out)
    }

    async fn send_report(&self, client: &Client, data: &CacheData, failed: bool) -> Result<(), mibot_bot::BotError> {
        let mut caption = if failed {
            "🧽 <b>清理失败名单</b>\n".to_owned()
        } else {
            "🧽 <b>群组清理报告</b>\n".to_owned()
        };
        caption.push_str(&format!(
            "{}\n{}：{} 人",
            escape(&data.chat_title),
            escape(&mode_name(&data.mode, data.day)),
            data.total_found
        ));
        client
            .send_document(
                &tl::enums::InputPeer::PeerSelf,
                &csv_report(data, failed),
                MediaOptions {
                    name: report_name(data, failed, (self.now)()),
                    mime_type: "text/csv".into(),
                    caption,
                    force_document: true,
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    async fn report_line(&self, inv: &Invocation, data: &CacheData) -> String {
        if data.total_found == 0 {
            return "没有符合条件的成员，没有生成报告".into();
        }
        match self.send_report(&inv.client, data, false).await {
            Ok(()) => "报告已发到收藏夹".into(),
            Err(err) => {
                if !inv.cancel.is_cancelled() {
                    kit::opt_log(
                        self.logs.as_ref(),
                        mibot_logtail::Level::Warn,
                        "clean_member.report_failed",
                        &[("error", &err.to_string())],
                    );
                }
                format!("⚠️ 报告没能发到收藏夹（{}）", escape(&brief(&err)))
            }
        }
    }

    async fn finish_cached(&self, inv: &Invocation, data: &CacheData) -> CmdResult {
        let when = DateTime::parse_from_rfc3339(&data.search_time)
            .map(|dt| dt.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|_| data.search_time.clone());
        let lines = [
            format!(
                "✅ 已搜索完 {} 里{}（用的是 {} 的缓存）",
                escape(&data.chat_title),
                escape(&mode_name(&data.mode, data.day)),
                escape(&when)
            ),
            format!("符合条件 {} 人", data.total_found),
            self.report_line(inv, data).await,
        ];
        inv.edit(&lines.join("\n")).await?;
        Ok(())
    }

    async fn finish(&self, inv: &Invocation, data: &CacheData, req: &Request, out: &Outcome) -> CmdResult {
        let mut lines = vec![
            format!(
                "✅ 已{}完 {} 里{}",
                verb(req),
                escape(&data.chat_title),
                escape(&mode_name(&req.mode, req.day))
            ),
            format!("扫描 {} 人，符合条件 {} 人", out.scanned, out.found.len()),
        ];
        if !req.search {
            let mut removed = format!("已移出 {} 人", out.removed);
            if !out.found.is_empty() {
                let pct = (out.removed as f64) * 100.0 / (out.found.len() as f64);
                removed.push_str(&format!("（成功率 {pct:.1}%）"));
            }
            lines.push(removed);
        }
        if out.limited {
            lines.push(format!("已达到 limit:{}，后面的成员没有再查", req.limit));
        }
        if out.skipped > 0 {
            lines.push(format!("⚠️ {} 人查不到发言记录，已跳过", out.skipped));
        }
        lines.push(self.report_line(inv, data).await);
        if !out.failed.is_empty() {
            lines.push(format!(
                "⚠️ 移出失败 {} 人（{}）",
                out.failed.len(),
                escape(&top_reasons(&out.reasons, 3))
            ));
            let mut failed = data.clone();
            failed.total_found = out.failed.len();
            failed.users = out.failed.clone();
            match self.send_report(&inv.client, &failed, true).await {
                Ok(()) => lines.push("失败名单另发了一份到收藏夹".into()),
                Err(err) => {
                    if !inv.cancel.is_cancelled() {
                        kit::opt_log(
                            self.logs.as_ref(),
                            mibot_logtail::Level::Warn,
                            "clean_member.failed_report_failed",
                            &[("error", &err.to_string())],
                        );
                    }
                }
            }
        }
        kit::opt_log(
            self.logs.as_ref(),
            mibot_logtail::Level::Info,
            "clean_member.done",
            &[
                ("chat", &data.chat_id),
                ("mode", &req.mode),
                ("scanned", &out.scanned.to_string()),
                ("found", &out.found.len().to_string()),
                ("removed", &out.removed.to_string()),
            ],
        );
        inv.edit(&lines.join("\n")).await?;
        Ok(())
    }

    async fn handle(&self, inv: &Invocation) -> CmdResult {
        if inv.args.is_empty() {
            return inv.respond_pages(&html_pages(&help(inv.prefix()), PAGE_LIMIT)).await.map_err(Into::into);
        }
        let req = parse_request(&inv.args, inv.prefix())?;
        let g = resolve_group(&inv.client, &inv.message, &req.chat).await?;
        if g.broadcast && (req.mode == "2" || req.mode == "3") {
            return Err(kit::fail("频道的订阅者不能发言，模式 2、3 只能用于群组"));
        }
        if !req.search {
            let allowed = self
                .can_ban(&inv.cancel, &inv.client, &g)
                .await
                .map_err(|e| kit::fail_with("查不到你在这个群组的权限", mibot_command::UserError::new(e.to_string())))?;
            if !allowed {
                return Err(kit::fail("权限不足：移出成员要群主身份或封禁成员权限"));
            }
        }
        let key = cache_key(&g.chat_id, &req.mode, req.day);
        if req.search && let Some(cached) = self.cache.load(&key, (self.now)()) {
            return self.finish_cached(inv, &cached).await;
        }
        inv.edit_text(&kit::working(&format!(
            "正在{} {} 里{}",
            verb(&req),
            g.title,
            mode_name(&req.mode, req.day)
        )))
        .await?;

        let admins = match self.admins(&inv.cancel, &inv.client, &g).await {
            Ok(a) => a,
            Err(err) => {
                if inv.cancel.is_cancelled() {
                    return Err(err);
                }
                if !req.search {
                    return Err(kit::fail_with(
                        "读不到完整的管理员列表，已停止清理",
                        mibot_command::UserError::new(err.to_string()),
                    ));
                }
                kit::opt_log(
                    self.logs.as_ref(),
                    mibot_logtail::Level::Warn,
                    "clean_member.admins_failed",
                    &[("error", &err.to_string())],
                );
                HashSet::new()
            }
        };

        let out_res = self.scan(inv, &g, &req, &admins).await;
        if !req.search
            && let Err(err) = self.cache.clear_chat(&g.chat_id)
        {
            kit::opt_log(
                self.logs.as_ref(),
                mibot_logtail::Level::Warn,
                "clean_member.cache_write_failed",
                &[("error", &err.to_string())],
            );
        }
        let out = out_res?;

        let now = (self.now)();
        let data = CacheData {
            chat_id: g.chat_id.clone(),
            chat_title: g.title.clone(),
            mode: req.mode.clone(),
            day: req.day,
            search_time: iso_time(now),
            total_found: out.found.len(),
            users: out.found.clone(),
            expires_at: unix_ms(now + CACHE_TTL),
        };
        if req.search
            && let Err(err) = self.cache.save(&key, &data, now)
        {
            kit::opt_log(
                self.logs.as_ref(),
                mibot_logtail::Level::Warn,
                "clean_member.cache_write_failed",
                &[("error", &err.to_string())],
            );
        }
        self.finish(inv, &data, &req, &out).await
    }
}

pub fn register(app: &Arc<App>) {
    let svc = Arc::new(Service {
        cache: Cache {
            dir: app.data_dir().join("clean_member"),
        },
        logs: Some(Arc::clone(&app.logs)),
        now: Arc::new(SystemTime::now),
        pause: Arc::new(|cancel, d| Box::pin(async move { kit::sleep(&cancel, d).await })),
        gap: Arc::new(|| {
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| u64::from(d.subsec_nanos() % 500))
                .unwrap_or(0);
            Duration::from_millis(1000 + ms)
        }),
        progress_every: Duration::from_secs(5),
    });
    app.registry.register(Command {
        name: "clean_member".into(),
        group: group::ADMIN.into(),
        description: "搜索或清理群组成员".into(),
        usage: "模式 [参数] [chat:对话 ID] [limit:数量] [search]".into(),
        help_fn: Some(Arc::new(help)),
        timeout: Some(Duration::ZERO),
        handle: Arc::new(move |inv| {
            let s = Arc::clone(&svc);
            Box::pin(async move { s.handle(&inv).await })
        }),
        ..Default::default()
    });
    let _ = participant_user;
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{PeerCache, Retrier, RpcTransport, SentRecorder, SystemClock};
    use mibot_command::Registry;
    use std::sync::Mutex as StdMutex;

    const SELF_ID: i64 = 1;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-cleanmember-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_parse_request() {
        let valid = [
            ("1 30 search", Request { mode: "1".into(), day: 30, search: true, limit: 0, chat: String::new() }),
            ("1 3", Request { mode: "1".into(), day: 7, search: false, limit: 0, chat: String::new() }),
            ("2 5 limit:10", Request { mode: "2".into(), day: 7, search: false, limit: 10, chat: String::new() }),
            ("3 5 SEARCH", Request { mode: "3".into(), day: 5, search: true, limit: 0, chat: String::new() }),
            ("4 chat:-1001234 search", Request { mode: "4".into(), day: 0, search: true, limit: 0, chat: "-1001234".into() }),
            ("5 CHAT：@group LIMIT：2", Request { mode: "5".into(), day: 0, search: false, limit: 2, chat: "@group".into() }),
        ];
        for (input, want) in valid {
            let args: Vec<String> = input.split_whitespace().map(str::to_owned).collect();
            assert_eq!(parse_request(&args, ".").unwrap(), want, "input={input}");
        }
        let invalid = [
            ("6", "没有模式 6"),
            ("search", "没有模式 search"),
            ("1", "模式 1 后面要写天数"),
            ("3 x", "发言条数要是正整数，x 不是"),
            ("2 -5", "天数要是正整数"),
            ("1 1.5", "天数要是正整数"),
            ("4 limit:0", "limit 要是正整数"),
            ("4 limit:", "limit 要是正整数"),
            ("4 chat:", "chat: 后面要写对话 ID"),
            ("4 serach", "不认识的参数 serach"),
            ("1 30 30 search", "不认识的参数 30"),
        ];
        for (input, want) in invalid {
            let args: Vec<String> = input.split_whitespace().map(str::to_owned).collect();
            let err = parse_request(&args, ".").unwrap_err();
            assert!(kit::is_user_error(err.as_ref()));
            assert!(err.to_string().contains(want), "input={input} got={err}");
        }
    }

    #[test]
    fn test_last_seen_days_and_online() {
        let now = UNIX_EPOCH + Duration::from_secs(100 * 86400);
        let online: tl::enums::UserStatus = tl::types::UserStatusOnline { expires: 1 }.into();
        let recent: tl::enums::UserStatus = tl::types::UserStatusRecently { by_me: false }.into();
        let off: tl::enums::UserStatus = tl::types::UserStatusOffline {
            was_online: 100 * 86400 - 10 * 86400 - 5,
        }
        .into();
        let week: tl::enums::UserStatus = tl::types::UserStatusLastWeek { by_me: false }.into();
        let month: tl::enums::UserStatus = tl::types::UserStatusLastMonth { by_me: false }.into();
        let empty: tl::enums::UserStatus = tl::enums::UserStatus::Empty;
        let off_zero: tl::enums::UserStatus = tl::types::UserStatusOffline { was_online: 0 }.into();

        assert_eq!(last_seen_days(Some(&online), now), Some(0));
        assert_eq!(last_seen_days(Some(&recent), now), Some(0));
        assert_eq!(last_seen_days(Some(&off), now), Some(10));
        assert_eq!(last_seen_days(Some(&week), now), Some(7));
        assert_eq!(last_seen_days(Some(&month), now), Some(30));
        assert_eq!(last_seen_days(Some(&empty), now), None);
        assert_eq!(last_seen_days(None, now), None);
        assert_eq!(last_seen_days(Some(&off_zero), now), None);

        let off_day1: tl::enums::UserStatus = tl::types::UserStatusOffline { was_online: 86400 }.into();
        assert_eq!(last_online(Some(&off_day1)).as_deref(), Some("1970-01-02T00:00:00.000Z"));
        assert_eq!(last_online(Some(&week)).as_deref(), Some("lastweek"));
        assert_eq!(last_online(None), None);
    }

    #[test]
    fn test_csv_report() {
        let data = CacheData {
            chat_id: "100".into(),
            chat_title: "群\"一\"".into(),
            mode: "1".into(),
            day: 30,
            search_time: "2026-09-30T00:00:00.000Z".into(),
            total_found: 2,
            users: vec![
                UserInfo {
                    id: "5".into(),
                    username: "cat".into(),
                    first_name: "=HYPERLINK(1)".into(),
                    last_name: "S".into(),
                    is_deleted: false,
                    last_online: Some("online".into()),
                    error_message: String::new(),
                },
                UserInfo {
                    id: "6".into(),
                    username: String::new(),
                    first_name: "-1".into(),
                    last_name: String::new(),
                    is_deleted: true,
                    last_online: None,
                    error_message: "权限不足".into(),
                },
            ],
            expires_at: 0,
        };
        let report = String::from_utf8(csv_report(&data, false)).unwrap();
        let want = "\u{feff}\"群组清理报告\"\n\"群组名称\",\"群\"\"一\"\"\"\n\"群组ID\",\"100\"\n\"清理条件\",\"未上线超过 30 天的用户\"\n\"搜索时间\",\"2026-09-30T00:00:00.000Z\"\n\"符合条件用户数量\",\"2\"\n\n\"用户ID\",\"用户名\",\"姓名\",\"最后上线时间\",\"是否注销\"\n\"5\",\"cat\",\"'=HYPERLINK(1) S\",\"online\",\"否\"\n\"6\",\"\",\"'-1\",\"未知\",\"是\"";
        assert_eq!(report, want);
        let failed = String::from_utf8(csv_report(&data, true)).unwrap();
        assert!(failed.contains("\"群组清理失败用户报告\""));
        assert!(failed.ends_with("\"未知\",\"是\",\"权限不足\""));
    }

    #[test]
    fn test_cache_files() {
        let dir = temp_dir("cache");
        let c = Cache { dir: dir.clone() };
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let write_aged = |name: &str, age: Duration| {
            let p = dir.join(name);
            let f = OpenOptions::new().create(true).truncate(true).write(true).open(&p).unwrap();
            std::io::Write::write_all(&mut &f, b"{}").unwrap();
            f.set_times(FileTimes::new().set_modified(now - age)).unwrap();
        };
        write_aged("1_1_7.json", Duration::from_secs(25 * 3600));
        for idx in 0..CACHE_LIMIT {
            write_aged(&format!("1_4_{idx}.json"), Duration::from_secs(((CACHE_LIMIT - idx) * 60) as u64));
        }
        write_aged("notes.txt", Duration::from_secs(48 * 3600));
        c.save(
            "2_5_0",
            &CacheData {
                chat_id: "2".into(),
                mode: "5".into(),
                total_found: 1,
                expires_at: unix_ms(now + Duration::from_secs(3600)),
                ..Default::default()
            },
            now,
        )
        .unwrap();
        assert!(!dir.join("1_1_7.json").exists());
        assert!(!dir.join("1_4_0.json").exists());
        assert!(dir.join("1_4_1.json").exists());
        assert!(dir.join("notes.txt").exists());
        assert_eq!(c.load("2_5_0", now).unwrap().total_found, 1);
        assert!(c.load("2_5_0", now + Duration::from_secs(7200)).is_none());
        assert!(c.load("../x", now).is_none());
        write_aged("10_4_0.json", Duration::from_secs(60));
        c.clear_chat("1").unwrap();
        assert!(!dir.join("1_4_1.json").exists());
        assert!(dir.join("10_4_0.json").exists());
        assert!(dir.join("2_5_0.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    struct FakeGroupInner {
        members: Vec<tl::types::User>,
        admins: HashSet<i64>,
        self_role: &'static str,
        messages: HashMap<i64, i32>,
        fail_ban: HashMap<i64, &'static str>,
        fail_find: HashSet<i64>,
        calls: Vec<String>,
        uploads: HashMap<i64, Vec<u8>>,
        documents: HashMap<String, String>,
    }

    struct FakeGroup {
        inner: StdMutex<FakeGroupInner>,
    }

    fn make_member_user(id: i64, deleted: bool) -> tl::types::User {
        tl::types::User {
            is_self: id == SELF_ID,
            deleted,
            access_hash: Some(3),
            first_name: Some(if id == SELF_ID { "我".into() } else { format!("成员{id}") }),
            status: Some(tl::types::UserStatusRecently { by_me: false }.into()),
            ..kit::blank_tl_user(id)
        }
    }

    impl FakeGroup {
        fn new(members: Vec<tl::types::User>) -> Self {
            let mut all = vec![make_member_user(SELF_ID, false)];
            all.extend(members);
            let mut admins = HashSet::new();
            admins.insert(SELF_ID);
            Self {
                inner: StdMutex::new(FakeGroupInner {
                    members: all,
                    admins,
                    self_role: "creator",
                    messages: HashMap::new(),
                    fail_ban: HashMap::new(),
                    fail_find: HashSet::new(),
                    calls: Vec::new(),
                    uploads: HashMap::new(),
                    documents: HashMap::new(),
                }),
            }
        }

        fn participant(g: &FakeGroupInner, u: &tl::types::User) -> tl::enums::ChannelParticipant {
            if u.id == SELF_ID && g.self_role == "creator" {
                return tl::types::ChannelParticipantCreator {
                    user_id: u.id,
                    admin_rights: kit::default_admin_rights().into(),
                    rank: None,
                }
                .into();
            }
            if g.admins.contains(&u.id) && (u.id != SELF_ID || g.self_role == "admin") {
                return tl::types::ChannelParticipantAdmin {
                    can_edit: false,
                    is_self: false,
                    user_id: u.id,
                    inviter_id: None,
                    promoted_by: 999,
                    date: 1,
                    admin_rights: tl::enums::ChatAdminRights::Rights(tl::types::ChatAdminRights {
                        ban_users: true,
                        ..kit::default_admin_rights()
                    }),
                    rank: None,
                }
                .into();
            }
            tl::types::ChannelParticipant {
                user_id: u.id,
                date: 1,
                subscription_until_date: None,
                rank: None,
            }
            .into()
        }

        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.inner.lock().unwrap().calls)
        }
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

    impl RpcTransport for FakeGroup {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                let mut g = self.inner.lock().unwrap();
                if constructor_id == tl::functions::channels::GetParticipant::CONSTRUCTOR_ID {
                    g.calls.push("self".into());
                    let p = Self::participant(&g, &g.members[0]);
                    let resp: tl::enums::channels::ChannelParticipant =
                        tl::types::channels::ChannelParticipant {
                            participant: p,
                            chats: vec![],
                            users: vec![],
                        }
                        .into();
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::channels::GetParticipants::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::GetParticipants::from_bytes(&body[4..]).unwrap();
                    let picked: Vec<tl::types::User> = match req.filter {
                        tl::enums::ChannelParticipantsFilter::ChannelParticipantsAdmins => {
                            g.calls.push(format!("admins offset={}", req.offset));
                            g.members.iter().filter(|u| g.admins.contains(&u.id)).cloned().collect()
                        }
                        _ => {
                            g.calls.push(format!("recent offset={}", req.offset));
                            let start = (req.offset as usize).min(g.members.len());
                            let end = (req.offset as usize + req.limit as usize).min(g.members.len());
                            g.members[start..end].to_vec()
                        }
                    };
                    let mut users: Vec<tl::enums::User> = picked.iter().cloned().map(Into::into).collect();
                    let participants: Vec<tl::enums::ChannelParticipant> =
                        picked.iter().map(|u| Self::participant(&g, u)).collect();
                    users.push(make_member_user(999, true).into());
                    let resp: tl::enums::channels::ChannelParticipants =
                        tl::types::channels::ChannelParticipants {
                            count: g.members.len() as i32,
                            participants,
                            chats: vec![],
                            users,
                        }
                        .into();
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::channels::EditBanned::CONSTRUCTOR_ID {
                    let req = tl::functions::channels::EditBanned::from_bytes(&body[4..]).unwrap();
                    let tl::enums::InputPeer::User(u) = req.participant else { unreachable!() };
                    let tl::enums::ChatBannedRights::Rights(r) = req.banned_rights;
                    if !r.view_messages {
                        g.calls.push(format!("unban {}", u.user_id));
                        return Ok(empty_updates());
                    }
                    g.calls.push(format!("ban {} until={}", u.user_id, r.until_date));
                    if let Some(&code) = g.fail_ban.get(&u.user_id) {
                        if code == "USER_NOT_PARTICIPANT" {
                            g.members.retain(|m| m.id != u.user_id);
                        }
                        return Err(InvocationError::Rpc(RpcError {
                            code: 400,
                            name: code.into(),
                            value: None,
                            caused_by: None,
                        }));
                    }
                    g.members.retain(|m| m.id != u.user_id);
                    return Ok(empty_updates());
                }
                if constructor_id == tl::functions::messages::GetFullChat::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::GetFullChat::from_bytes(&body[4..]).unwrap();
                    g.calls.push("full-chat".into());
                    let mut participants = Vec::new();
                    let mut users = Vec::new();
                    for u in &g.members {
                        users.push(u.clone().into());
                        if u.id == SELF_ID {
                            participants.push(tl::types::ChatParticipantCreator { user_id: u.id, rank: None }.into());
                        } else if g.admins.contains(&u.id) {
                            participants.push(tl::types::ChatParticipantAdmin { user_id: u.id, inviter_id: 1, date: 1, rank: None }.into());
                        } else {
                            participants.push(tl::types::ChatParticipant { user_id: u.id, inviter_id: 1, date: 1, rank: None }.into());
                        }
                    }
                    let resp: tl::enums::messages::ChatFull = tl::types::messages::ChatFull {
                        full_chat: tl::enums::ChatFull::Full(tl::types::ChatFull {
                            can_set_username: false,
                            has_scheduled: false,
                            translations_disabled: false,
                            has_welcome_messages: false,
                            id: req.chat_id,
                            about: String::new(),
                            participants: tl::enums::ChatParticipants::Participants(tl::types::ChatParticipants {
                                chat_id: req.chat_id,
                                participants,
                                version: 1,
                            }),
                            chat_photo: None,
                            notify_settings: tl::enums::PeerNotifySettings::Settings(tl::types::PeerNotifySettings {
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
                            }),
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
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::messages::DeleteChatUser::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::DeleteChatUser::from_bytes(&body[4..]).unwrap();
                    let tl::enums::InputUser::User(u) = req.user_id else { unreachable!() };
                    g.calls.push(format!("remove {}", u.user_id));
                    g.members.retain(|m| m.id != u.user_id);
                    return Ok(empty_updates());
                }
                if constructor_id == tl::functions::messages::Search::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::Search::from_bytes(&body[4..]).unwrap();
                    let Some(tl::enums::InputPeer::User(u)) = req.from_id else { unreachable!() };
                    g.calls.push(format!("search {} min={}", u.user_id, req.min_date));
                    if g.fail_find.contains(&u.user_id) {
                        return Err(InvocationError::Rpc(RpcError {
                            code: 400,
                            name: "SEARCH_QUERY_EMPTY".into(),
                            value: None,
                            caused_by: None,
                        }));
                    }
                    let count = *g.messages.get(&u.user_id).unwrap_or(&0);
                    let resp = tl::enums::messages::Messages::ChannelMessages(
                        tl::types::messages::ChannelMessages {
                            inexact: false,
                            pts: 1,
                            count,
                            offset_id_offset: None,
                            messages: vec![],
                            topics: vec![],
                            chats: vec![],
                            users: vec![],
                        },
                    );
                    return Ok(resp.to_bytes());
                }
                if constructor_id == tl::functions::messages::EditMessage::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::EditMessage::from_bytes(&body[4..]).unwrap();
                    g.calls.push(format!("edit {}", req.message.unwrap_or_default()));
                    return Ok(empty_updates());
                }
                if constructor_id == tl::functions::upload::SaveFilePart::CONSTRUCTOR_ID {
                    let req = tl::functions::upload::SaveFilePart::from_bytes(&body[4..]).unwrap();
                    g.uploads.entry(req.file_id).or_default().extend_from_slice(&req.bytes);
                    return Ok(true.to_bytes());
                }
                if constructor_id == tl::functions::messages::SendMedia::CONSTRUCTOR_ID {
                    let req = tl::functions::messages::SendMedia::from_bytes(&body[4..]).unwrap();
                    assert!(matches!(req.peer, tl::enums::InputPeer::PeerSelf));
                    let tl::enums::InputMedia::UploadedDocument(doc) = req.media else { unreachable!() };
                    let tl::enums::InputFile::File(f) = doc.file else { unreachable!() };
                    let kind = f.name.split('_').next().unwrap_or("").to_owned();
                    g.calls.push(format!("document {} {kind}: {}", doc.mime_type, req.message));
                    let bytes = g.uploads.get(&f.id).cloned().unwrap_or_default();
                    g.documents.insert(kind, String::from_utf8_lossy(&bytes).into_owned());
                    return Ok(empty_updates());
                }
                Err(InvocationError::Dropped("unexpected rpc"))
            })
        }
    }

    fn make_chan(id: i64, hash: i64, title: &str, broadcast: bool) -> tl::enums::Chat {
        tl::types::Channel {
            broadcast,
            megagroup: !broadcast,
            access_hash: Some(hash),
            ..kit::blank_tl_channel(id, title)
        }
        .into()
    }

    fn new_test_svc(dir: PathBuf, fake: Arc<FakeGroup>) -> (Service, Arc<Client>) {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(SELF_ID);
        peers.remember_users(&[make_member_user(SELF_ID, false).into()]);
        peers.remember_chats(&[
            make_chan(100, 5, "测试群", false),
            make_chan(300, 6, "频道", true),
            tl::types::Chat {
                creator: false,
                left: false,
                deactivated: false,
                call_active: false,
                call_not_empty: false,
                noforwards: false,
                id: 200,
                title: "小群".into(),
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
        let retrier = Arc::new(Retrier::with_clock(
            Duration::from_secs(60),
            5,
            Arc::new(SystemClock),
            Arc::new(|| 1_700_000_000),
        ));
        let client = Arc::new(Client::with_retrier(
            fake,
            retrier,
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let svc = Service {
            cache: Cache { dir: dir.join("clean_member") },
            logs: None,
            now: Arc::new(|| UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            pause: Arc::new(|c, _| Box::pin(async move { !c.is_cancelled() })),
            gap: Arc::new(|| Duration::ZERO),
            progress_every: Duration::from_secs(3600),
        };
        (svc, client)
    }

    async fn run_cmd(svc: &Service, client: &Arc<Client>, peer: tl::enums::Peer, text: &str) -> CmdResult {
        let args: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
        let mut message = kit::blank_bot_message(10, peer);
        message.out = true;
        let inv = Invocation {
            client: Arc::clone(client),
            registry: Arc::new(Registry::new()),
            message,
            prefix: ".".into(),
            command: "clean_member".into(),
            args,
            raw: text.into(),
            cancel: CancellationToken::new(),
            foreign: false,
        };
        svc.handle(&inv).await
    }

    fn last_edit(calls: &[String]) -> String {
        calls
            .iter()
            .rev()
            .find_map(|c| c.strip_prefix("edit ").map(str::to_owned))
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn test_search_and_clean_and_modes_and_refusals_and_basic() {
        let here: tl::enums::Peer = tl::types::PeerChannel { channel_id: 100 }.into();

        // 1. Search pages + cache
        let dir = temp_dir("search");
        let mut members: Vec<_> = (10..260).map(|i| make_member_user(i, false)).collect();
        members.extend((500..503).map(|i| make_member_user(i, true)));
        let fake = Arc::new(FakeGroup::new(members));
        fake.inner.lock().unwrap().admins.insert(10);
        let (svc, client) = new_test_svc(dir.clone(), Arc::clone(&fake));
        run_cmd(&svc, &client, here.clone(), "4 search").await.unwrap();
        let calls = fake.take();
        assert_eq!(calls.iter().filter(|c| c.starts_with("recent")).count(), 2);
        assert_eq!(
            last_edit(&calls),
            "✅ 已搜索完 测试群 里已注销的账号\n扫描 254 人，符合条件 3 人\n报告已发到收藏夹"
        );
        run_cmd(&svc, &client, here.clone(), "4 search").await.unwrap();
        let calls2 = fake.take();
        assert_eq!(calls2.iter().filter(|c| c.starts_with("recent")).count(), 0);
        assert!(last_edit(&calls2).contains("（用的是 2023-11-14 22:13 UTC 的缓存）"));
        let _ = std::fs::remove_dir_all(dir);

        // 2. Clean does not skip shifted members
        let dir = temp_dir("shift");
        let members: Vec<_> = (10..260).map(|i| make_member_user(i, false)).collect();
        let fake = Arc::new(FakeGroup::new(members));
        fake.inner.lock().unwrap().fail_ban.insert(20, "CHAT_ADMIN_REQUIRED");
        fake.inner.lock().unwrap().fail_ban.insert(21, "USER_NOT_PARTICIPANT");
        let (svc, client) = new_test_svc(dir.clone(), Arc::clone(&fake));
        svc.cache
            .save(
                "100_4_0",
                &CacheData {
                    expires_at: 1_700_003_600_000,
                    ..Default::default()
                },
                (svc.now)(),
            )
            .unwrap();
        run_cmd(&svc, &client, here.clone(), "5").await.unwrap();
        let calls = fake.take();
        assert!(calls.iter().any(|c| c == "ban 10 until=1700000060"));
        assert_eq!(fake.inner.lock().unwrap().members.len(), 2);
        assert!(last_edit(&calls).contains("已移出 249 人（成功率 99.6%）"));
        assert!(svc.cache.load("100_4_0", (svc.now)()).is_none());
        let _ = std::fs::remove_dir_all(dir);

        // 3. Message modes + refusals + basic group
        let dir = temp_dir("modes");
        let members: Vec<_> = (10..13).map(|i| make_member_user(i, true)).collect();
        let fake = Arc::new(FakeGroup::new(members));
        {
            let mut g = fake.inner.lock().unwrap();
            g.messages.insert(10, 0);
            g.messages.insert(11, 8);
            g.fail_find.insert(12);
        }
        let (svc, client) = new_test_svc(dir.clone(), Arc::clone(&fake));
        run_cmd(&svc, &client, here.clone(), "2 30 search").await.unwrap();
        let calls = fake.take();
        assert!(last_edit(&calls).contains("符合条件 1 人"));
        assert!(last_edit(&calls).contains("⚠️ 1 人查不到发言记录，已跳过"));

        // Basic group
        fake.inner.lock().unwrap().admins.insert(11);
        run_cmd(&svc, &client, tl::types::PeerChat { chat_id: 200 }.into(), "4")
            .await
            .unwrap();
        let calls = fake.take();
        assert!(calls.iter().any(|c| c == "remove 10"));
        assert!(calls.iter().any(|c| c == "remove 12"));
        assert!(!calls.iter().any(|c| c == "remove 11"));
        assert!(last_edit(&calls).contains("已移出 2 人"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
