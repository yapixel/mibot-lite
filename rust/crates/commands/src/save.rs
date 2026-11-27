use crate::kit;
use chrono::{DateTime, Utc};
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BotError, Client, DialogPages, MediaSource, SendOptions, peer_id, source_of};
use mibot_command::{CmdError, CmdResult, Command, Invocation, bold, brief, code, escape, group};
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::fs::File;
use tokio_util::sync::CancellationToken;

const SAVE_RANGE_LIMIT: usize = 500;
const SAVE_MAX_BYTES: i64 = 4 << 30;
const ALBUM_RADIUS: i32 = 30;

#[derive(Debug)]
pub struct NothingToCopyError;

impl std::fmt::Display for NothingToCopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("消息里没有能保存的内容")
    }
}

impl StdError for NothingToCopyError {}

pub fn is_nothing_to_copy(err: &(dyn StdError + 'static)) -> bool {
    mibot_command::errors::find_in_chain::<NothingToCopyError>(err).is_some()
}

fn err_nothing_to_copy() -> CmdError {
    Box::new(mibot_command::UserError::with_cause(
        "消息里没有能保存的内容",
        NothingToCopyError,
    ))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct SaveDocument {
    #[serde(skip_serializing_if = "String::is_empty")]
    target: String,
    #[serde(skip_serializing_if = "is_false")]
    source: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct MiboxUser {
    target: String,
    #[serde(rename = "showSource")]
    show_source: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct MiboxConfig {
    users: HashMap<String, MiboxUser>,
}

pub fn convert_mibox(raw: &[u8]) -> Result<Vec<u8>, serde_json::Error> {
    let config: MiboxConfig = serde_json::from_slice(raw)?;
    let mut ids: Vec<&String> = config.users.keys().collect();
    ids.sort();
    let mut chosen = MiboxUser::default();
    let pickers: [fn(&MiboxUser) -> bool; 3] = [
        |u| !is_self_target(&u.target),
        |u| u.show_source,
        |_| true,
    ];
    'outer: for pick in pickers {
        for id in &ids {
            if let Some(u) = config.users.get(*id)
                && pick(u)
            {
                chosen = u.clone();
                break 'outer;
            }
        }
    }
    let mut doc = SaveDocument {
        target: String::new(),
        source: chosen.show_source,
    };
    if !is_self_target(&chosen.target) {
        doc.target = chosen.target.trim().to_owned();
    }
    serde_json::to_vec_pretty(&doc)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MessageLink {
    username: String,
    chat_id: String,
    id: i32,
    raw: String,
}

impl MessageLink {
    fn same_chat(&self, other: &Self) -> bool {
        self.chat_id == other.chat_id && self.username.eq_ignore_ascii_case(&other.username)
    }

    fn url(&self) -> String {
        if !self.username.is_empty() {
            return format!("https://t.me/{}/{}", self.username, self.id);
        }
        if let Some(ch) = self.chat_id.strip_prefix("-100") {
            return format!("https://t.me/c/{ch}/{}", self.id);
        }
        String::new()
    }
}

fn link_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:https?://)?(?:t|telegram)\.me/(?:c/(\d+)|([A-Za-z][A-Za-z0-9_]{3,31}))(?:/\d+)?/(\d+)(?:[/?#].*)?$",
        )
        .expect("valid link regex")
    })
}

fn unsafe_segment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^\p{L}\p{N}._-]+").expect("valid segment regex"))
}

fn parse_link(text: &str) -> Option<MessageLink> {
    let caps = link_pattern().captures(text.trim())?;
    let id: i32 = caps.get(3)?.as_str().parse().ok()?;
    if id <= 0 {
        return None;
    }
    let mut link = MessageLink {
        id,
        raw: text.to_owned(),
        ..Default::default()
    };
    if let Some(ch) = caps.get(1) {
        link.chat_id = format!("-100{}", ch.as_str());
    } else if let Some(user) = caps.get(2) {
        link.username = user.as_str().to_owned();
    }
    Some(link)
}

#[derive(Debug, Default)]
struct SaveRequest {
    links: Vec<MessageLink>,
    range: Option<[MessageLink; 2]>,
    target: String,
}

fn parse_save_args(args: &[String]) -> Result<SaveRequest, CmdError> {
    let mut req = SaveRequest::default();
    let mut others = Vec::new();
    for arg in args {
        if let Some((left, right)) = arg.split_once('|') {
            if req.range.is_some() {
                return Err(kit::fail("一次只能保存一个消息范围"));
            }
            let (Some(mut from), Some(mut to)) = (parse_link(left), parse_link(right)) else {
                return Err(kit::fail("范围两头都得是消息链接：链接1|链接2"));
            };
            if !from.same_chat(&to) {
                return Err(kit::fail("范围的两个链接必须在同一个对话里"));
            }
            if from.id > to.id {
                std::mem::swap(&mut from, &mut to);
            }
            req.range = Some([from, to]);
            continue;
        }
        if let Some(link) = parse_link(arg) {
            req.links.push(link);
            continue;
        }
        others.push(arg.as_str());
    }
    match others.len() {
        0 => {}
        1 => req.target = others[0].to_owned(),
        _ => return Err(kit::fail(format!("看不懂 {}：链接之外只能跟一个目标", others.join(" ")))),
    }
    if req.range.is_some() && !req.links.is_empty() {
        return Err(kit::fail("范围和单独的链接不能混在一起"));
    }
    Ok(req)
}

fn is_self_target(target: &str) -> bool {
    matches!(target.trim().to_ascii_lowercase().as_str(), "" | "me" | "self" | "收藏夹")
}

fn is_local_target(target: &str) -> bool {
    target.trim().eq_ignore_ascii_case("local")
}

fn describe_target(target: &str) -> String {
    if is_self_target(target) {
        "收藏夹".into()
    } else if is_local_target(target) {
        "主机本地的 save/ 目录".into()
    } else {
        target.to_owned()
    }
}

type ProgressFn = Arc<Mutex<(Instant, String, Box<dyn FnMut(&str) + Send>)>>;

struct Saver {
    client: Arc<Client>,
    root: PathBuf,
    partial: PathBuf,
    learned: bool,
    status: String,
    topic: i32,
    last_id: i32,
    usernames: HashMap<String, tl::enums::InputPeer>,
    progress_cb: Option<ProgressFn>,
}

impl Saver {
    fn new_background(client: Arc<Client>, root: PathBuf, topic: i32) -> Self {
        Self {
            client,
            partial: root.join("save").join(".partial"),
            root,
            learned: false,
            status: String::new(),
            topic,
            last_id: 0,
            usernames: HashMap::new(),
            progress_cb: None,
        }
    }

    fn set_status(&mut self, status: String) {
        self.status = status.clone();
        if let Some(cb) = &self.progress_cb {
            let mut g = cb.lock().unwrap_or_else(|e| e.into_inner());
            g.1 = status;
        }
    }

    fn progress(&self, step: &str) {
        if let Some(cb) = &self.progress_cb {
            let mut g = cb.lock().unwrap_or_else(|e| e.into_inner());
            if g.0.elapsed() < Duration::from_secs(3) {
                return;
            }
            g.0 = Instant::now();
            let text = format!("{} {step}", g.1).trim().to_owned();
            (g.2)(&text);
        }
    }

    async fn peer_of(&mut self, link: &MessageLink) -> Result<tl::enums::InputPeer, CmdError> {
        if !link.username.is_empty() {
            let key = link.username.to_ascii_lowercase();
            if let Some(p) = self.usernames.get(&key) {
                return Ok(p.clone());
            }
            let p = self.client.resolve_username(&link.username).await?;
            self.usernames.insert(key, p.clone());
            return Ok(p);
        }
        match self.client.input_peer_from_chat_id(&link.chat_id) {
            Ok(p) => Ok(p),
            Err(err) if err.is_unaddressable_peer() && !self.learned => {
                self.learned = true;
                learn_dialogs(&self.client).await?;
                self.client
                    .input_peer_from_chat_id(&link.chat_id)
                    .map_err(|_| kit::fail("找不到这个对话：账号得是它的成员"))
            }
            Err(_) => Err(kit::fail("找不到这个对话：账号得是它的成员")),
        }
    }

    async fn target_of(&mut self, target: &str) -> Result<tl::enums::InputPeer, CmdError> {
        if is_self_target(target) {
            return Ok(tl::enums::InputPeer::PeerSelf);
        }
        match self.client.resolve_target(target).await {
            Ok(p) => Ok(p),
            Err(err) if err.is_unaddressable_peer() && !self.learned => {
                self.learned = true;
                learn_dialogs(&self.client).await?;
                self.client
                    .resolve_target(target)
                    .await
                    .map_err(|_| kit::fail(format!("找不到目标 {target}")))
            }
            Err(_) => Err(kit::fail(format!("找不到目标 {target}"))),
        }
    }

    async fn fetch(
        &self,
        peer: &tl::enums::InputPeer,
        ids: &[i32],
    ) -> Result<Vec<tl::types::Message>, BotError> {
        let mut found = Vec::new();
        for chunk in ids.chunks(100) {
            let batch = self.client.get_messages(peer, chunk).await?;
            found.extend(batch);
        }
        Ok(found)
    }

    async fn send(
        &mut self,
        cancel: &CancellationToken,
        message: &tl::types::Message,
        from: &tl::enums::InputPeer,
        to: &tl::enums::InputPeer,
    ) -> Result<bool, CmdError> {
        if !message.noforwards {
            match self.forward(cancel, std::slice::from_ref(message), from, to).await {
                Ok(()) => return Ok(false),
                Err(err) if err.is_rpc("CHAT_FORWARDS_RESTRICTED") => {}
                Err(err) => return Err(Box::new(err)),
            }
        }
        let full = match self.client.full_rich(from, message.clone()).await {
            Ok(f) => f,
            Err(_) => message.clone(),
        };
        self.copy(cancel, &full, to).await?;
        Ok(true)
    }

    async fn forward(
        &mut self,
        cancel: &CancellationToken,
        messages: &[tl::types::Message],
        from: &tl::enums::InputPeer,
        to: &tl::enums::InputPeer,
    ) -> Result<(), BotError> {
        let ids: Vec<i32> = messages.iter().map(|m| m.id).collect();
        let topic = (self.topic > 0).then_some(self.topic);
        let updates = on_flood(cancel, || self.client.forward_messages(from, to, &ids, topic)).await?;
        self.last_id = sent_id(&updates);
        Ok(())
    }

    async fn copy(
        &mut self,
        cancel: &CancellationToken,
        message: &tl::types::Message,
        to: &tl::enums::InputPeer,
    ) -> CmdResult {
        let media = self.remake(cancel, message).await?;
        let updates = match media {
            None => {
                if message.rich_message.is_some() {
                    let updates = on_flood(cancel, || {
                        self.client.copy_rich(
                            to,
                            message,
                            SendOptions {
                                topic_id: (self.topic > 0).then_some(self.topic),
                                ..Default::default()
                            },
                        )
                    })
                    .await?;
                    self.last_id = sent_id(&updates);
                    return Ok(());
                }
                if message.message.trim().is_empty() {
                    return Err(err_nothing_to_copy());
                }
                let opts = SendOptions {
                    entities: message.entities.clone(),
                    topic_id: (self.topic > 0).then_some(self.topic),
                    ..Default::default()
                };
                on_flood(cancel, || self.client.send_plain(to, &message.message, None, opts.clone())).await?
            }
            Some(input_media) => {
                let opts = mibot_bot::MediaOptions {
                    caption: message.message.clone(),
                    entities: message.entities.clone(),
                    topic_id: (self.topic > 0).then_some(self.topic),
                    html: false,
                    ..Default::default()
                };
                on_flood(cancel, || self.client.send_media(to, input_media.clone(), opts.clone())).await?
            }
        };
        self.last_id = sent_id(&updates);
        Ok(())
    }

    async fn album(
        &self,
        peer: &tl::enums::InputPeer,
        message: &tl::types::Message,
    ) -> Result<Vec<tl::types::Message>, BotError> {
        let Some(group_id) = message.grouped_id else {
            return Ok(vec![message.clone()]);
        };
        let ids: Vec<i32> = (message.id - ALBUM_RADIUS..=message.id + ALBUM_RADIUS)
            .filter(|&id| id > 0)
            .collect();
        let found = self.fetch(peer, &ids).await?;
        let mut album: Vec<tl::types::Message> = found
            .into_iter()
            .filter(|m| m.grouped_id == Some(group_id))
            .collect();
        if album.is_empty() {
            return Ok(vec![message.clone()]);
        }
        album.sort_by_key(|m| m.id);
        Ok(album)
    }

    async fn remake(
        &self,
        cancel: &CancellationToken,
        message: &tl::types::Message,
    ) -> Result<Option<tl::enums::InputMedia>, CmdError> {
        let Some(media) = &message.media else {
            return Ok(None);
        };
        match media {
            tl::enums::MessageMedia::WebPage(_) | tl::enums::MessageMedia::Empty => Ok(None),
            tl::enums::MessageMedia::Geo(g) => {
                if let tl::enums::GeoPoint::Point(pt) = &g.geo {
                    Ok(Some(
                        tl::types::InputMediaGeoPoint {
                            geo_point: tl::types::InputGeoPoint {
                                lat: pt.lat,
                                long: pt.long,
                                accuracy_radius: pt.accuracy_radius,
                            }
                            .into(),
                        }
                        .into(),
                    ))
                } else {
                    Err(kit::fail("这种消息在禁止转发的对话里没法复制"))
                }
            }
            tl::enums::MessageMedia::Venue(v) => {
                if let tl::enums::GeoPoint::Point(pt) = &v.geo {
                    Ok(Some(
                        tl::types::InputMediaVenue {
                            geo_point: tl::types::InputGeoPoint {
                                lat: pt.lat,
                                long: pt.long,
                                accuracy_radius: pt.accuracy_radius,
                            }
                            .into(),
                            title: v.title.clone(),
                            address: v.address.clone(),
                            provider: v.provider.clone(),
                            venue_id: v.venue_id.clone(),
                            venue_type: v.venue_type.clone(),
                        }
                        .into(),
                    ))
                } else {
                    Err(kit::fail("这种消息在禁止转发的对话里没法复制"))
                }
            }
            tl::enums::MessageMedia::Contact(c) => Ok(Some(
                tl::types::InputMediaContact {
                    phone_number: c.phone_number.clone(),
                    first_name: c.first_name.clone(),
                    last_name: c.last_name.clone(),
                    vcard: c.vcard.clone(),
                }
                .into(),
            )),
            tl::enums::MessageMedia::Poll(p) => {
                let tl::enums::Poll::Poll(poll) = &p.poll;
                Ok(Some(
                    tl::types::InputMediaPoll {
                        poll: tl::types::Poll {
                            id: mibot_bot::random_id(),
                            closed: false,
                            public_voters: false,
                            multiple_choice: poll.multiple_choice,
                            quiz: false,
                            open_answers: false,
                            revoting_disabled: false,
                            shuffle_answers: false,
                            hide_results_until_close: false,
                            creator: false,
                            subscribers_only: false,
                            question: poll.question.clone(),
                            answers: poll.answers.clone(),
                            close_period: None,
                            close_date: None,
                            countries_iso2: None,
                            hash: 0,
                        }
                        .into(),
                        correct_answers: None,
                        attached_media: None,
                        solution: None,
                        solution_entities: None,
                        solution_media: None,
                    }
                    .into(),
                ))
            }
            tl::enums::MessageMedia::Photo(_) | tl::enums::MessageMedia::Document(_) => {
                let Some(source) = source_of(message) else {
                    return Err(kit::fail("这条消息的媒体已经不可用"));
                };
                if source.size > SAVE_MAX_BYTES {
                    return Err(kit::fail(format!(
                        "文件 {}，超过 Telegram 的上限",
                        kit::format_bytes(source.size)
                    )));
                }
                if cancel.is_cancelled() {
                    return Err(kit::fail("操作已取消"));
                }
                let path = self.download(&source).await?;
                let cleanup = TempFileGuard(path.clone());
                self.progress(&kit::working("正在上传"));
                let bytes = tokio::fs::read(&cleanup.0).await?;
                let upload_name = if source.file_name.is_empty() {
                    if source.photo { "photo.jpg" } else { "file" }
                } else {
                    &source.file_name
                };
                let file = self.client.upload_bytes(upload_name, &bytes).await?;
                if source.photo {
                    Ok(Some(
                        tl::types::InputMediaUploadedPhoto {
                            spoiler: false,
                            live_photo: false,
                            file,
                            stickers: None,
                            ttl_seconds: None,
                            video: None,
                        }
                        .into(),
                    ))
                } else {
                    Ok(Some(
                        tl::types::InputMediaUploadedDocument {
                            nosound_video: false,
                            force_file: false,
                            spoiler: false,
                            file,
                            thumb: None,
                            mime_type: source.mime_type,
                            attributes: source.attributes,
                            stickers: None,
                            video_cover: None,
                            video_timestamp: None,
                            ttl_seconds: None,
                        }
                        .into(),
                    ))
                }
            }
            _ => Err(kit::fail("这种消息在禁止转发的对话里没法复制")),
        }
    }

    async fn download(&self, source: &MediaSource) -> Result<PathBuf, CmdError> {
        if source.size > SAVE_MAX_BYTES {
            return Err(kit::fail(format!(
                "文件 {}，超过 Telegram 的上限",
                kit::format_bytes(source.size)
            )));
        }
        std::fs::create_dir_all(&self.partial)?;
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_nanos();
        let part_path = self.partial.join(format!("part-{nanos}"));
        let mut file = File::create(&part_path).await?;
        if source.size > 0 {
            self.progress(&kit::working(&format!("正在下载 {}", kit::format_bytes(source.size))));
        } else {
            self.progress(&kit::working("正在下载"));
        }
        if let Err(err) = self.client.download_to(source, &mut file).await {
            drop(file);
            let _ = std::fs::remove_file(&part_path);
            return Err(Box::new(err));
        }
        file.sync_data().await.ok();
        drop(file);
        Ok(part_path)
    }

    async fn save_local(
        &self,
        message: &tl::types::Message,
        link: &MessageLink,
    ) -> Result<Option<String>, CmdError> {
        let Some(source) = source_of(message) else {
            return Ok(None);
        };
        let path = self.download(&source).await?;
        let cleanup = TempFileGuard(path.clone());
        let chat = if !link.username.is_empty() {
            link.username.as_str()
        } else {
            link.chat_id.strip_prefix("-100").filter(|s| !s.is_empty()).unwrap_or("chat")
        };
        let directory = self.root.join("save").join(sanitize_segment(chat));
        std::fs::create_dir_all(&directory)?;
        let name = if !source.file_name.is_empty() {
            format!("{}_{}", message.id, sanitize_segment(&source.file_name))
        } else {
            format!("{}{}", message.id, local_extension(&source))
        };
        let dt = DateTime::<Utc>::from(UNIX_EPOCH + Duration::from_secs(message.date.max(0) as u64));
        let meta = serde_json::to_vec_pretty(&serde_json::json!({
            "source": link.url(),
            "message_id": message.id,
            "date": dt.to_rfc3339(),
            "text": message.message,
            "mime_type": source.mime_type,
        }))?;
        let final_path = publish_local(&cleanup.0, &directory.join(name), &meta)?;
        let rel = final_path
            .strip_prefix(&self.root)
            .unwrap_or(&final_path)
            .to_string_lossy()
            .into_owned();
        Ok(Some(rel))
    }

    fn source_of_saved(&self, job: &SaveJob, message: &tl::types::Message) -> SavedSource {
        SavedSource {
            link: MessageLink {
                username: job.link.username.clone(),
                chat_id: job.link.chat_id.clone(),
                id: message.id,
                raw: String::new(),
            },
            title: self.client.peers().title(&message.peer_id),
        }
    }
}

struct TempFileGuard(PathBuf);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn copy_message(
    cancel: &CancellationToken,
    client: &Arc<Client>,
    root: &Path,
    message: &tl::types::Message,
    to: &tl::enums::InputPeer,
    topic: i32,
) -> CmdResult {
    let mut work = Saver::new_background(Arc::clone(client), root.to_path_buf(), topic);
    work.copy(cancel, message, to).await
}

async fn learn_dialogs(client: &Client) -> Result<(), BotError> {
    client.each_dialog(DialogPages::default(), |_| {}).await
}

async fn on_flood<F, Fut, T>(cancel: &CancellationToken, mut call: F) -> Result<T, BotError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, BotError>>,
{
    match call().await {
        Ok(v) => Ok(v),
        Err(err) => {
            if let Some(wait) = err.flood_wait()
                && wait <= Duration::from_secs(5 * 60)
            {
                if !kit::sleep(cancel, wait + Duration::from_secs(1)).await {
                    return Err(BotError::Cancelled);
                }
                return call().await;
            }
            Err(err)
        }
    }
}

fn sent_id(updates: &tl::enums::Updates) -> i32 {
    let mut latest = match updates {
        tl::enums::Updates::UpdateShortSentMessage(u) => u.id,
        tl::enums::Updates::UpdateShortMessage(u) => u.id,
        tl::enums::Updates::UpdateShortChatMessage(u) => u.id,
        _ => 0,
    };
    let list = match updates {
        tl::enums::Updates::Updates(u) => u.updates.as_slice(),
        tl::enums::Updates::Combined(u) => u.updates.as_slice(),
        tl::enums::Updates::UpdateShort(u) => std::slice::from_ref(&u.update),
        _ => &[],
    };
    for u in list {
        let id = match u {
            tl::enums::Update::MessageId(m) => m.id,
            tl::enums::Update::NewMessage(m) => kit::message_id(&m.message),
            tl::enums::Update::NewChannelMessage(m) => kit::message_id(&m.message),
            tl::enums::Update::EditMessage(m) => kit::message_id(&m.message),
            tl::enums::Update::EditChannelMessage(m) => kit::message_id(&m.message),
            _ => 0,
        };
        latest = latest.max(id);
    }
    latest
}

fn sanitize_segment(value: &str) -> String {
    let replaced = unsafe_segment_re().replace_all(value, "_");
    let trimmed = replaced.trim_matches(['.', '_']);
    if trimmed.is_empty() {
        return "file".into();
    }
    let mut end_idx = trimmed.len();
    for (chars_count, (idx, ch)) in trimmed.char_indices().enumerate() {
        let next_bytes = idx + ch.len_utf8();
        if chars_count == 80 || next_bytes > 220 {
            end_idx = idx;
            break;
        }
    }
    trimmed[..end_idx].to_owned()
}

fn local_extension(source: &MediaSource) -> &'static str {
    if source.photo {
        return ".jpg";
    }
    match source.mime_type.as_str() {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "video/quicktime" => ".mov",
        "video/x-matroska" => ".mkv",
        "audio/ogg" => ".ogg",
        "audio/mpeg" => ".mp3",
        "audio/mp4" | "audio/x-m4a" => ".m4a",
        "audio/flac" => ".flac",
        "audio/wav" => ".wav",
        "application/x-tgsticker" => ".tgs",
        "application/pdf" => ".pdf",
        "application/zip" => ".zip",
        "text/plain" => ".txt",
        "text/html" => ".html",
        "text/csv" => ".csv",
        "application/json" => ".json",
        _ => ".bin",
    }
}

fn publish_local(source: &Path, path: &Path, metadata: &[u8]) -> std::io::Result<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let base = parent.join(stem);
    for index in 1.. {
        let candidate = if index == 1 {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("{}_{index}{ext}", base.to_string_lossy()))
        };
        let slot = match OpenOptions::new().create_new(true).write(true).open(&candidate) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        if let Err(e) = slot.sync_data() {
            let _ = std::fs::remove_file(&candidate);
            return Err(e);
        }
        drop(slot);
        let sidecar = PathBuf::from(format!("{}.json", candidate.to_string_lossy()));
        let mut meta_file = match OpenOptions::new().create_new(true).write(true).open(&sidecar) {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_file(&candidate);
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(e);
            }
        };
        let res = meta_file
            .write_all(metadata)
            .and_then(|_| meta_file.sync_data())
            .and_then(|_| {
                drop(meta_file);
                std::fs::rename(source, &candidate)
            });
        if let Err(e) = res {
            let _ = std::fs::remove_file(&sidecar);
            let _ = std::fs::remove_file(&candidate);
            return Err(e);
        }
        return Ok(candidate);
    }
    unreachable!()
}

struct SaveJob {
    link: MessageLink,
    peer: tl::enums::InputPeer,
    messages: Vec<tl::types::Message>,
    album: bool,
}

#[derive(Default)]
struct SaveTally {
    saved: usize,
    copied: usize,
    skipped: usize,
    failures: Vec<String>,
    files: Vec<String>,
    sources: Vec<SavedSource>,
}

#[derive(Clone)]
struct SavedSource {
    link: MessageLink,
    title: String,
}

fn forward_rejected(err: &BotError) -> bool {
    if err.flood_wait().is_some() {
        return false;
    }
    err.rpc_error().is_some_and(|rpc| rpc.code < 500)
}

fn forwardable(messages: &[tl::types::Message]) -> bool {
    messages.iter().all(|m| !m.noforwards)
}

async fn run_save_jobs(
    cancel: &CancellationToken,
    work: &mut Saver,
    jobs: Vec<SaveJob>,
    destination: &tl::enums::InputPeer,
    local: bool,
) -> Result<SaveTally, CmdError> {
    let mut tally = SaveTally::default();
    let total: usize = jobs.iter().map(|j| j.messages.len()).sum();
    let mut index = 0usize;
    for job in jobs {
        if job.album && !local && forwardable(&job.messages) {
            work.set_status(format!("💾 {}/{total}", index + 1));
            work.progress("");
            match work.forward(cancel, &job.messages, &job.peer, destination).await {
                Ok(()) => {
                    index += job.messages.len();
                    for m in &job.messages {
                        tally.saved += 1;
                        tally.sources.push(work.source_of_saved(&job, m));
                    }
                    continue;
                }
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(Box::new(err));
                    }
                    if !forward_rejected(&err) {
                        index += job.messages.len();
                        tally.failures.push(format!(
                            "相册 #{}：没有确认保存，不再重发（{}）",
                            job.messages[0].id,
                            brief(&err)
                        ));
                        continue;
                    }
                }
            }
        }
        for m in &job.messages {
            index += 1;
            work.set_status(format!("💾 {index}/{total}"));
            work.progress("");
            if local {
                let link = MessageLink {
                    username: job.link.username.clone(),
                    chat_id: job.link.chat_id.clone(),
                    id: m.id,
                    raw: String::new(),
                };
                match work.save_local(m, &link).await {
                    Err(err) => tally.failures.push(format!("#{}：{err}", m.id)),
                    Ok(None) => tally.skipped += 1,
                    Ok(Some(path)) => {
                        tally.saved += 1;
                        tally.files.push(path);
                    }
                }
                continue;
            }
            match work.send(cancel, m, &job.peer, destination).await {
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(err);
                    }
                    tally.failures.push(format!("#{}：{}", m.id, brief(err.as_ref())));
                }
                Ok(copied) => {
                    tally.saved += 1;
                    tally.sources.push(work.source_of_saved(&job, m));
                    if copied {
                        tally.copied += 1;
                    }
                }
            }
        }
    }
    Ok(tally)
}

fn id_spans(ids: &[i32]) -> Vec<[i32; 2]> {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut spans: Vec<[i32; 2]> = Vec::new();
    for id in sorted {
        if let Some(last) = spans.last_mut()
            && id <= last[1] + 1
        {
            last[1] = last[1].max(id);
            continue;
        }
        spans.push([id, id]);
    }
    spans
}

fn source_anchor(link: &MessageLink) -> String {
    format!("<a href=\"{}\">{}</a>", escape(&link.url()), link.id)
}

fn source_notice(sources: &[SavedSource], is_range: bool) -> String {
    let linked: Vec<&SavedSource> = sources.iter().filter(|s| !s.link.url().is_empty()).collect();
    if linked.is_empty() {
        return String::new();
    }
    if is_range {
        let first = linked[0];
        let last = linked[linked.len() - 1];
        return format!(
            "🔗 <b>范围保存来源</b>\n\n▶️ <b>起始消息</b>\n• {} / {}\n\n⏹ <b>结尾消息</b>\n• {} / {}",
            bold(&first.title),
            source_anchor(&first.link),
            bold(&last.title),
            source_anchor(&last.link),
        );
    }
    if linked.len() == 1 {
        let s = linked[0];
        return format!(
            "🔗 <b>消息来源</b>\n\n📝 <a href=\"{}\">查看原消息</a>\n👤 来源对话：{}\n#️⃣ 消息 ID：{}",
            escape(&s.link.url()),
            bold(&s.title),
            code(&s.link.id.to_string()),
        );
    }
    struct ChatGroup {
        key: String,
        title: String,
        link: MessageLink,
        ids: Vec<i32>,
    }
    let mut groups: Vec<ChatGroup> = Vec::new();
    let mut idx_map: HashMap<String, usize> = HashMap::new();
    for s in linked {
        let key = format!("{}@{}", s.link.chat_id, s.link.username.to_ascii_lowercase());
        if let Some(&idx) = idx_map.get(&key) {
            groups[idx].ids.push(s.link.id);
        } else {
            idx_map.insert(key.clone(), groups.len());
            groups.push(ChatGroup {
                key,
                title: s.title.clone(),
                link: s.link.clone(),
                ids: vec![s.link.id],
            });
        }
    }
    groups.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.key.cmp(&b.key)));
    let sections: Vec<String> = groups
        .into_iter()
        .map(|g| {
            let spans = id_spans(&g.ids);
            let mut parts = Vec::with_capacity(spans.len());
            let mut count = 0i32;
            for sp in spans {
                let mut start = g.link.clone();
                let mut end = g.link.clone();
                start.id = sp[0];
                end.id = sp[1];
                count += sp[1] - sp[0] + 1;
                if sp[0] == sp[1] {
                    parts.push(source_anchor(&start));
                } else {
                    parts.push(format!("{}-{}", source_anchor(&start), source_anchor(&end)));
                }
            }
            format!("👤 {}（{} 条）：{}", bold(&g.title), count, parts.join(", "))
        })
        .collect();
    let mut body = sections.join("\n");
    if body.chars().count() > 350 || sections.len() > 6 {
        body = format!("<blockquote expandable>{body}</blockquote>");
    }
    format!("🔗 <b>批量保存来源</b>\n\n{body}")
}

fn render_save_result(tally: &SaveTally, target: &str, root: &Path) -> String {
    let mut lines = vec![format!("✅ 已保存 {} 条到{}", tally.saved, describe_target(target))];
    if tally.copied > 0 {
        lines.push(format!("其中 {} 条来自禁止转发的对话，已重新上传", tally.copied));
    }
    if tally.skipped > 0 {
        lines.push(format!("跳过 {} 条纯文本（本地模式只存媒体）", tally.skipped));
    }
    if !tally.files.is_empty() {
        lines.push(format!("文件在 {}/", root.join("save").to_string_lossy()));
    }
    if !tally.failures.is_empty() {
        if tally.saved == 0 {
            lines[0] = "❌ 没有保存成功".into();
        }
        let shown = &tally.failures[..tally.failures.len().min(5)];
        lines.push(format!("失败 {} 条：", tally.failures.len()));
        lines.extend(shown.iter().cloned());
    }
    lines.join("\n")
}

fn about_link(link: &str, err: CmdError) -> CmdError {
    if kit::is_user_error(err.as_ref()) {
        kit::fail(format!("{link}：{}", brief(err.as_ref())))
    } else {
        kit::fail_with(format!("{link} 读取失败"), mibot_command::UserError::new(err.to_string()))
    }
}

async fn show_save_settings(inv: &Invocation, settings: &Store<SaveDocument>) -> CmdResult {
    let current = settings.read()?;
    let source = if current.source { "开" } else { "关" };
    inv.edit(&format!(
        "💾 <b>保存设置</b>\n\n默认目标：{}\n来源链接：{}\n\n<i>{} 修改默认目标</i>",
        code(&describe_target(&current.target)),
        code(source),
        escape(&format!("{}save to 目标", inv.prefix()))
    ))
    .await?;
    Ok(())
}

async fn save_setting(inv: &Invocation, settings: &Store<SaveDocument>) -> Result<bool, CmdError> {
    match inv.arg(0).to_ascii_lowercase().as_str() {
        "target" | "config" => {
            show_save_settings(inv, settings).await?;
            Ok(true)
        }
        "source" => {
            let choice = inv.arg(1).to_ascii_lowercase();
            if choice == "on" || choice == "off" {
                kit::update_ok(settings, |v| v.source = choice == "on")?;
            }
            show_save_settings(inv, settings).await?;
            Ok(true)
        }
        "to" => {
            let raw_target = inv.raw_after(1).trim().to_owned();
            if raw_target.is_empty() {
                return Err(kit::usage(inv.prefix(), "save to me|@用户名|对话 ID|local"));
            }
            if !is_self_target(&raw_target) && !is_local_target(&raw_target) {
                let mut check = Saver::new_background(Arc::clone(&inv.client), PathBuf::new(), 0);
                check.target_of(&raw_target).await?;
            }
            let target = if is_self_target(&raw_target) {
                String::new()
            } else {
                raw_target
            };
            kit::update_ok(settings, move |v| v.target = target)?;
            show_save_settings(inv, settings).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn collect_save_jobs(
    inv: &Invocation,
    work: &mut Saver,
    req: &SaveRequest,
) -> Result<Vec<SaveJob>, CmdError> {
    if let Some([from, to]) = &req.range {
        let count = (to.id - from.id + 1) as usize;
        if count > SAVE_RANGE_LIMIT {
            return Err(kit::fail(format!(
                "范围有 {count} 条，一次最多 {SAVE_RANGE_LIMIT} 条，分几段来"
            )));
        }
        let peer = work.peer_of(from).await.map_err(|e| about_link(&from.raw, e))?;
        let ids: Vec<i32> = (from.id..=to.id).collect();
        inv.edit_text(&kit::working(&format!("正在读取 {count} 个编号"))).await?;
        let messages = work.fetch(&peer, &ids).await?;
        return Ok(vec![SaveJob {
            link: from.clone(),
            peer,
            messages,
            album: false,
        }]);
    }
    if !req.links.is_empty() {
        let mut jobs = Vec::new();
        let mut albums = HashSet::new();
        for link in &req.links {
            let peer = work.peer_of(link).await.map_err(|e| about_link(&link.raw, e))?;
            let messages = work.fetch(&peer, &[link.id]).await?;
            if messages.is_empty() {
                return Err(kit::fail(format!("{} 这条消息不存在或已删除", link.raw)));
            }
            let Some(group_id) = messages[0].grouped_id else {
                jobs.push(SaveJob {
                    link: link.clone(),
                    peer,
                    messages,
                    album: false,
                });
                continue;
            };
            let key = format!("{}:{group_id}", peer_id(&messages[0].peer_id));
            if !albums.insert(key) {
                continue;
            }
            let album = work.album(&peer, &messages[0]).await?;
            jobs.push(SaveJob {
                link: link.clone(),
                peer,
                messages: album,
                album: true,
            });
        }
        return Ok(jobs);
    }
    let reply = kit::reply_msg(inv).await?;
    let Some(reply) = reply else {
        return Err(kit::fail("读不到被回复的消息"));
    };
    let raw = reply.raw;
    let peer = inv.client.input_peer(&reply.peer)?;
    Ok(vec![SaveJob {
        link: MessageLink {
            chat_id: reply.chat_id,
            id: reply.id,
            ..Default::default()
        },
        peer,
        messages: vec![raw],
        album: false,
    }])
}

async fn save_handle(inv: &Invocation, settings: &Store<SaveDocument>, root: &Path) -> CmdResult {
    if save_setting(inv, settings).await? {
        return Ok(());
    }
    let req = parse_save_args(&inv.args)?;
    if req.range.is_none() && req.links.is_empty() && inv.message.reply_to_id == 0 {
        return inv.edit(&save_help(inv.prefix())).await.map_err(Into::into);
    }
    let current = settings.read()?;
    let target = if !req.target.is_empty() {
        req.target.clone()
    } else {
        current.target.clone()
    };

    let inv_clone = inv.clone();
    let rt = tokio::runtime::Handle::current();
    let mut work = Saver::new_background(Arc::clone(&inv.client), root.to_path_buf(), 0);
    work.progress_cb = Some(Arc::new(Mutex::new((
        Instant::now() - Duration::from_secs(10),
        String::new(),
        Box::new(move |msg: &str| {
            let i = inv_clone.clone();
            let text = msg.to_owned();
            rt.spawn(async move {
                let _ = i.edit_text(&text).await;
            });
        }),
    ))));

    let jobs = collect_save_jobs(inv, &mut work, &req).await?;
    let total: usize = jobs.iter().map(|j| j.messages.len()).sum();
    if total == 0 {
        return Err(kit::fail("范围内没有消息"));
    }
    let local = is_local_target(&target);
    let destination = if local {
        tl::enums::InputPeer::PeerSelf
    } else {
        work.target_of(&target).await?
    };
    let tally = run_save_jobs(&inv.cancel, &mut work, jobs, &destination, local).await?;
    if current.source && !local {
        let notice = source_notice(&tally.sources, req.range.is_some());
        if !notice.is_empty() {
            let _ = work
                .client
                .send_html(
                    &destination,
                    &notice,
                    SendOptions {
                        reply_to: (work.last_id > 0).then_some(work.last_id),
                        ..Default::default()
                    },
                )
                .await;
        }
    }
    inv.edit_text(&render_save_result(&tally, &target, root)).await?;
    Ok(())
}

fn save_help(prefix: &str) -> String {
    let p = escape(prefix);
    format!(
        "💾 <b>保存消息</b>\n\n转发消息；遇到禁止转发的对话，就下载后重新发一份，文字格式和媒体属性都保留。\n\n\
         <b>保存</b>\n\
         • 回复一条消息发 <code>{p}save</code>\n\
         • <code>{p}save 链接 [链接…]</code> 按链接批量保存；链接指向相册里的一条时，整个相册一起保存\n\
         • <code>{p}save 链接1|链接2</code> 保存两条之间的所有消息，缺号自动跳过，一次最多 {SAVE_RANGE_LIMIT} 条\n\
         • 末尾加一个目标只这一次发到那里：<code>{p}save 链接 @某人</code>、<code>{p}save 链接 local</code>\n\n\
         <b>设置</b>\n\
         • <code>{p}save to 目标</code> 默认目标：<code>me</code>（收藏夹）、<code>@用户名</code>、对话 ID、<code>local</code>（存到主机）\n\
         • <code>{p}save target</code> 看当前设置\n\
         • <code>{p}save source on|off</code> 保存后回复一条来源说明，带原消息链接\n\n\
         <b>链接</b>\n支持 <code>t.me/用户名/编号</code>、<code>t.me/c/数字/编号</code>，以及带话题的形式。私密对话要求账号是成员。\n\n\
         <b>本地模式</b>\n只存媒体，纯文本跳过。文件放在部署目录的 <code>save/对话/</code> 下，旁边有同名 <code>.json</code> 记录来源。"
    )
}

pub fn register(app: &Arc<App>) {
    let settings = Arc::new(kit::new_store(app, "save.json", SaveDocument::default));
    let root = app.root.clone();
    app.registry.register(Command {
        name: "save".into(),
        group: group::MEDIA.into(),
        description: "保存或转发消息".into(),
        usage: "[链接…] [目标]".into(),
        help_fn: Some(Arc::new(save_help)),
        timeout: Some(Duration::from_secs(3600)),
        handle: Arc::new(move |inv| {
            let s = Arc::clone(&settings);
            let r = root.clone();
            Box::pin(async move { save_handle(&inv, &s, &r).await })
        }),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use grammers_tl_types::{Deserializable, Identifiable, Serializable};
    use mibot_bot::{PeerCache, RpcTransport, SentRecorder, parse_html};
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-save-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_parse_link_and_args_and_url_and_sanitize() {
        let valid = [
            ("https://t.me/c/1234567890/42", "", "-1001234567890", 42),
            ("t.me/c/1234567890/5/42", "", "-1001234567890", 42),
            ("https://t.me/durov/123", "durov", "", 123),
            ("https://t.me/some_group/7/88?single", "some_group", "", 88),
            ("telegram.me/durov/9", "durov", "", 9),
        ];
        for (txt, u, cid, id) in valid {
            let g = parse_link(txt).unwrap();
            assert_eq!((g.username.as_str(), g.chat_id.as_str(), g.id), (u, cid, id));
        }
        for bad in ["https://t.me/durov", "https://example.com/c/1/2", "t.me/c/abc/1", "hello", "t.me/ab/1", "t.me/durov/4294967301"] {
            assert!(parse_link(bad).is_none(), "bad={bad}");
        }

        let r = parse_save_args(&["https://t.me/durov/1".into(), "https://t.me/durov/2".into(), "@friend".into()]).unwrap();
        assert_eq!(r.links.len(), 2);
        assert_eq!(r.target, "@friend");

        let r = parse_save_args(&["t.me/c/123/100|t.me/c/123/5".into()]).unwrap();
        let rg = r.range.unwrap();
        assert_eq!((rg[0].id, rg[1].id), (5, 100));

        for bad in [
            vec!["t.me/c/1/1|t.me/c/1/2", "t.me/c/1/3|t.me/c/1/4"],
            vec!["t.me/c/1/1|t.me/c/2/9"],
            vec!["t.me/c/1/1|nope"],
            vec!["t.me/durov/1", "@a", "@b"],
            vec!["t.me/c/1/1|t.me/c/1/9", "t.me/durov/1"],
        ] {
            let args: Vec<String> = bad.into_iter().map(str::to_owned).collect();
            assert!(parse_save_args(&args).is_err());
        }

        assert_eq!(sanitize_segment("../../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_segment("视频 2024.mp4"), "视频_2024.mp4");
        assert_eq!(sanitize_segment("..."), "file");
        assert_eq!(sanitize_segment("a/b\\c"), "a_b_c");
    }

    #[test]
    fn test_convert_mibox() {
        let cases = [
            (
                r#"{"users":{"1":{"target":"me","showSource":false},"2":{"target":"@archive","showSource":true}}}"#,
                SaveDocument { target: "@archive".into(), source: true },
            ),
            (
                r#"{"users":{"1":{"target":"me","showSource":false},"2":{"target":"me","showSource":true}}}"#,
                SaveDocument { target: String::new(), source: true },
            ),
            (
                r#"{"users":{"7":{"target":"local","showSource":false}}}"#,
                SaveDocument { target: "local".into(), source: false },
            ),
            (r#"{"users":{}}"#, SaveDocument::default()),
        ];
        for (raw, want) in cases {
            let out = convert_mibox(raw.as_bytes()).unwrap();
            let got: SaveDocument = serde_json::from_slice(&out).unwrap();
            assert_eq!(got, want);
        }
        assert!(convert_mibox(b"not json").is_err());
    }

    #[test]
    fn test_source_notice_and_id_spans_and_sent_id_and_publish_local() {
        let chan = |id| SavedSource {
            link: MessageLink { chat_id: "-1005000".into(), id, ..Default::default() },
            title: "群".into(),
        };
        let pub_s = |id| SavedSource {
            link: MessageLink { username: "durov".into(), id, ..Default::default() },
            title: "Durov".into(),
        };
        let priv_s = SavedSource {
            link: MessageLink { chat_id: "777".into(), id: 3, ..Default::default() },
            title: "某人".into(),
        };
        assert_eq!(source_notice(std::slice::from_ref(&priv_s), false), "");
        let single = source_notice(&[chan(2), priv_s], false);
        assert!(single.contains("https://t.me/c/5000/2") && single.contains("<b>群</b>") && single.contains("<code>2</code>"));

        let batch = source_notice(&[chan(4), pub_s(9), chan(2), chan(3), chan(7)], false);
        assert!(batch.contains("<b>群</b>（4 条）：<a href=\"https://t.me/c/5000/2\">2</a>-<a href=\"https://t.me/c/5000/4\">4</a>, <a href=\"https://t.me/c/5000/7\">7</a>"));
        assert!(batch.find("Durov").unwrap() < batch.find("群").unwrap());
        parse_html(&batch, None).unwrap();

        assert_eq!(id_spans(&[5, 1, 2, 2, 3, 9, 10, 7]), vec![[1, 3], [5, 5], [7, 7], [9, 10]]);

        let updates = tl::enums::Updates::Updates(tl::types::Updates {
            updates: vec![
                tl::enums::Update::NewChannelMessage(tl::types::UpdateNewChannelMessage {
                    message: kit::blank_tl_message(41, tl::types::PeerUser { user_id: 1 }.into()).into(),
                    pts: 1,
                    pts_count: 1,
                }),
                tl::enums::Update::NewChannelMessage(tl::types::UpdateNewChannelMessage {
                    message: kit::blank_tl_message(43, tl::types::PeerUser { user_id: 1 }.into()).into(),
                    pts: 1,
                    pts_count: 1,
                }),
            ],
            users: vec![],
            chats: vec![],
            date: 0,
            seq: 0,
        });
        assert_eq!(sent_id(&updates), 43);

        let dir = temp_dir("pub");
        let path = dir.join(format!("1_{}", sanitize_segment(&"𠀀".repeat(80))));
        for body in ["first", "second"] {
            let src = dir.join("temp");
            std::fs::write(&src, body).unwrap();
            let got = publish_local(&src, &path, body.as_bytes()).unwrap();
            let expected = if body == "second" {
                PathBuf::from(format!("{}_2", path.to_string_lossy()))
            } else {
                path.clone()
            };
            assert_eq!(got, expected);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        let other = dir.join("3_report");
        std::fs::write(format!("{}.json", other.to_string_lossy()), "old attachment").unwrap();
        let src2 = dir.join("temp2");
        std::fs::write(&src2, "new").unwrap();
        let got2 = publish_local(&src2, &other, b"metadata").unwrap();
        assert_eq!(got2, PathBuf::from(format!("{}_2", other.to_string_lossy())));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn short_sent(id: i32) -> Vec<u8> {
        tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
            out: true,
            id,
            pts: 1,
            pts_count: 1,
            date: 0,
            media: None,
            entities: None,
            ttl_period: None,
        })
        .to_bytes()
    }

    #[tokio::test]
    async fn test_album_recovery_and_rich_copy() {
        // 1. Lost album response does not replay
        let count = Arc::new(AtomicUsize::new(0));
        let sent_batches = Arc::new(Mutex::new(Vec::<Vec<i32>>::new()));
        struct AlbumLost {
            count: Arc<AtomicUsize>,
            batches: Arc<Mutex<Vec<Vec<i32>>>>,
        }
        impl RpcTransport for AlbumLost {
            fn invoke_raw<'a>(
                &'a self,
                _dc_id: Option<i32>,
                cid: u32,
                body: Vec<u8>,
            ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                Box::pin(async move {
                    if cid == tl::functions::messages::ForwardMessages::CONSTRUCTOR_ID {
                        let req = tl::functions::messages::ForwardMessages::from_bytes(&body[4..]).unwrap();
                        self.batches.lock().unwrap().push(req.id);
                        let n = self.count.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            return Err(InvocationError::Io(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "eof",
                            )));
                        }
                        return Ok(short_sent(100 + n as i32));
                    }
                    Err(InvocationError::Dropped("unexpected"))
                })
            }
        }
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            Arc::new(AlbumLost {
                count: Arc::clone(&count),
                batches: Arc::clone(&sent_batches),
            }),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let mut work = Saver::new_background(client, PathBuf::new(), 0);
        let self_p: tl::enums::Peer = tl::types::PeerUser { user_id: 1 }.into();
        let jobs = vec![
            SaveJob {
                link: MessageLink::default(),
                peer: tl::enums::InputPeer::PeerSelf,
                messages: vec![kit::blank_tl_message(10, self_p.clone()), kit::blank_tl_message(11, self_p.clone())],
                album: true,
            },
            SaveJob {
                link: MessageLink::default(),
                peer: tl::enums::InputPeer::PeerSelf,
                messages: vec![kit::blank_tl_message(12, self_p.clone())],
                album: false,
            },
        ];
        let tally = run_save_jobs(&CancellationToken::new(), &mut work, jobs, &tl::enums::InputPeer::PeerSelf, false)
            .await
            .unwrap();
        assert_eq!(tally.saved, 1);
        assert_eq!(tally.failures.len(), 1);
        assert_eq!(*sent_batches.lock().unwrap(), vec![vec![10, 11], vec![12]]);

        // 2. Rejected album falls back to single forwards
        struct AlbumRejected;
        impl RpcTransport for AlbumRejected {
            fn invoke_raw<'a>(
                &'a self,
                _dc_id: Option<i32>,
                cid: u32,
                body: Vec<u8>,
            ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                Box::pin(async move {
                    if cid == tl::functions::messages::ForwardMessages::CONSTRUCTOR_ID {
                        let req = tl::functions::messages::ForwardMessages::from_bytes(&body[4..]).unwrap();
                        if req.id.len() > 1 || req.id[0] == 10 {
                            return Err(InvocationError::Rpc(RpcError {
                                code: 400,
                                name: "MESSAGE_ID_INVALID".into(),
                                value: None,
                                caused_by: None,
                            }));
                        }
                        return Ok(short_sent(101));
                    }
                    Err(InvocationError::Dropped("unexpected"))
                })
            }
        }
        let c2 = Arc::new(Client::from_transport(
            Arc::new(AlbumRejected),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let mut w2 = Saver::new_background(c2, PathBuf::new(), 0);
        let jobs2 = vec![SaveJob {
            link: MessageLink::default(),
            peer: tl::enums::InputPeer::PeerSelf,
            messages: vec![kit::blank_tl_message(10, self_p.clone()), kit::blank_tl_message(11, self_p.clone())],
            album: true,
        }];
        let t2 = run_save_jobs(&CancellationToken::new(), &mut w2, jobs2, &tl::enums::InputPeer::PeerSelf, false)
            .await
            .unwrap();
        assert_eq!(t2.saved, 1);
        assert_eq!(t2.failures.len(), 1);

        // 3. Restricted rich message is copied
        let texts = Arc::new(Mutex::new(Vec::new()));
        struct RichCopy(Arc<Mutex<Vec<String>>>);
        impl RpcTransport for RichCopy {
            fn invoke_raw<'a>(
                &'a self,
                _dc_id: Option<i32>,
                cid: u32,
                body: Vec<u8>,
            ) -> mibot_bot::BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                Box::pin(async move {
                    if cid == tl::functions::messages::SendMessage::CONSTRUCTOR_ID {
                        let req = tl::functions::messages::SendMessage::from_bytes(&body[4..]).unwrap();
                        self.0.lock().unwrap().push(req.message);
                        assert!(req.rich_message.is_some());
                        return Ok(short_sent(100));
                    }
                    Err(InvocationError::Dropped("unexpected"))
                })
            }
        }
        let c3 = Arc::new(Client::from_transport(
            Arc::new(RichCopy(Arc::clone(&texts))),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let self_u = tl::types::User {
            is_self: true,
            premium: true,
            ..kit::blank_tl_user(1)
        };
        c3.set_self_user(self_u);
        let mut msg = kit::blank_tl_message(10, self_p);
        msg.noforwards = true;
        msg.rich_message = Some(
            tl::types::RichMessage {
                rtl: false,
                part: false,
                blocks: vec![tl::types::PageBlockParagraph {
                    text: tl::types::TextPlain { text: "正文在块里".into() }.into(),
                }
                .into()],
                photos: vec![],
                documents: vec![],
            }
            .into(),
        );
        let mut w3 = Saver::new_background(c3, PathBuf::new(), 0);
        let t3 = run_save_jobs(
            &CancellationToken::new(),
            &mut w3,
            vec![SaveJob { link: MessageLink::default(), peer: tl::enums::InputPeer::PeerSelf, messages: vec![msg], album: false }],
            &tl::enums::InputPeer::PeerSelf,
            false,
        )
        .await
        .unwrap();
        assert_eq!((t3.saved, t3.copied, t3.failures.len()), (1, 1, 0));
    }
}
