use crate::kit;
use grammers_tl_types as tl;
use mibot_app::App;
use mibot_bot::{BotError, Client, Message};
use mibot_command::{CmdError, Invocation, PAGE_LIMIT, code, escape, group, html_pages};
use mibot_imaging::{
    PremulImage, apply_mask, brightness, composite, decode, decode_png, opacity, resize,
    resize_cover, rotate, rotate_expand, write_png,
};
use mibot_media::{
    Frame, StickerAttribute, TempWorkdir, resize_sticker, sticker_document, sticker_webm,
    sticker_webp,
};
use mibot_store::Store;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tokio_util::sync::CancellationToken;
use url::Url;

const EATGIF_ROOT: &str = "https://raw.githubusercontent.com/TeleBoxOrg/TeleBox-Plugins/main/eatgif/";
const EATGIF_MAX_FRAMES: usize = 200;
const DOC_REFRESH: Duration = Duration::from_secs(24 * 3600);
const EAT_SOURCE: &str = "https://raw.githubusercontent.com/TeleBoxOrg/TeleBox-Plugins/main/eat/config.json";

#[derive(Debug, Clone, Deserialize)]
struct EatgifRole {
    x: i32,
    y: i32,
    mask: String,
    #[serde(default)]
    rotate: Option<f64>,
    #[serde(default)]
    brightness: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct EatgifFrame {
    url: String,
    #[serde(default)]
    delay: Option<i64>,
    #[serde(default)]
    me: Option<EatgifRole>,
    #[serde(default)]
    you: Option<EatgifRole>,
}

#[derive(Debug, Clone, Deserialize)]
struct EatgifSpec {
    width: u32,
    height: u32,
    #[serde(rename = "res")]
    frames: Vec<EatgifFrame>,
}

#[derive(Debug, Clone, Deserialize)]
struct EatgifEntry {
    url: String,
    desc: String,
}

#[derive(Default)]
struct EatgifService {
    data_dir: PathBuf,
    logs: Option<Arc<mibot_logtail::Ring>>,
    state: Mutex<EatgifState>,
}

#[derive(Default)]
struct EatgifState {
    catalog: Option<HashMap<String, EatgifEntry>>,
    catalog_at: Option<SystemTime>,
    running: bool,
}

struct RunningGuard(Arc<EatgifService>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

fn safe_relative(value: &str) -> Result<&str, CmdError> {
    if value.is_empty() || value.contains('\\') || value.contains("://") {
        return Err(kit::fail(kit::tr("Invalid asset path", "素材路径无效")));
    }
    for part in value.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(kit::fail(kit::tr("Invalid asset path", "素材路径无效")));
        }
    }
    Ok(value)
}

fn cache_path(data_dir: &Path, directory: &str, key: &str) -> PathBuf {
    let digest = Sha256::digest(key.as_bytes());
    let ext = Path::new(key)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    data_dir
        .join(directory)
        .join(format!("{}{ext}", hex::encode(digest)))
}

fn read_cache(cache: &Path, limit: usize) -> Option<Vec<u8>> {
    let data = fs::read(cache).ok()?;
    if data.is_empty() || data.len() > limit {
        return None;
    }
    Some(data)
}

async fn do_get_cancel(
    cancel: &CancellationToken,
    req: mibot_httpx::Request,
) -> Result<mibot_httpx::Response, mibot_httpx::HttpxError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(mibot_httpx::HttpxError::Other("cancelled".into())),
        r = mibot_httpx::do_request(req) => r,
    }
}

async fn download(
    cancel: &CancellationToken,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    cache: &Path,
    url: &str,
    limit: usize,
) -> Result<Vec<u8>, mibot_httpx::HttpxError> {
    let resp = do_get_cancel(
        cancel,
        mibot_httpx::Request {
            url: url.to_owned(),
            timeout: Some(Duration::from_secs(30)),
            max_bytes: Some(limit),
            ..Default::default()
        },
    )
    .await?;
    if !resp.ok() {
        return Err(mibot_httpx::HttpxError::Status(resp.status));
    }
    if let Err(err) = mibot_fsutil::write_file_atomic(cache, &resp.body, 0o600) {
        let base = cache
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("cache");
        kit::opt_log(
            logs,
            mibot_logtail::Level::Warn,
            "eatgif.cache_write_failed",
            &[("file", base), ("error", &err.to_string())],
        );
    }
    Ok(resp.body)
}

async fn fetch_cached(
    cancel: &CancellationToken,
    data_dir: &Path,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    directory: &str,
    key: &str,
    url: &str,
    limit: usize,
) -> Result<Vec<u8>, mibot_httpx::HttpxError> {
    let cache = cache_path(data_dir, directory, key);
    if let Some(data) = read_cache(&cache, limit) {
        return Ok(data);
    }
    download(cancel, logs, &cache, url, limit).await
}

async fn fetch_fresh(
    cancel: &CancellationToken,
    logs: Option<&Arc<mibot_logtail::Ring>>,
    cache: &Path,
    url: &str,
    limit: usize,
    max_age: Duration,
) -> Result<Vec<u8>, mibot_httpx::HttpxError> {
    if let Ok(meta) = fs::metadata(cache)
        && let Ok(modified) = meta.modified()
        && let Ok(elapsed) = modified.elapsed()
        && elapsed < max_age
        && let Some(data) = read_cache(cache, limit)
    {
        return Ok(data);
    }
    match download(cancel, logs, cache, url, limit).await {
        Ok(data) => Ok(data),
        Err(err) => {
            if let Some(stale) = read_cache(cache, limit) {
                Ok(stale)
            } else {
                Err(err)
            }
        }
    }
}

impl EatgifService {
    async fn asset(
        &self,
        cancel: &CancellationToken,
        relative: &str,
        limit: usize,
    ) -> Result<Vec<u8>, CmdError> {
        let clean = safe_relative(relative)?;
        let url = format!("{EATGIF_ROOT}{clean}");
        fetch_cached(
            cancel,
            &self.data_dir,
            self.logs.as_ref(),
            "eatgif",
            clean,
            &url,
            limit,
        )
        .await
        .map_err(Into::into)
    }

    async fn asset_json<T: serde::de::DeserializeOwned>(
        &self,
        cancel: &CancellationToken,
        relative: &str,
    ) -> Result<T, CmdError> {
        let clean = safe_relative(relative)?;
        let url = format!("{EATGIF_ROOT}{clean}");
        let cache = cache_path(&self.data_dir, "eatgif", clean);
        let data = fetch_fresh(
            cancel,
            self.logs.as_ref(),
            &cache,
            &url,
            1 << 20,
            DOC_REFRESH,
        )
        .await?;
        serde_json::from_slice(&data).map_err(|_| kit::fail(kit::tr("Invalid asset config", "素材配置无效")))
    }

    async fn get_catalog(
        &self,
        cancel: &CancellationToken,
    ) -> Result<HashMap<String, EatgifEntry>, CmdError> {
        {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let (Some(cat), Some(at)) = (&guard.catalog, guard.catalog_at)
                && at.elapsed().is_ok_and(|d| d < DOC_REFRESH)
            {
                return Ok(cat.clone());
            }
        }
        let catalog: HashMap<String, EatgifEntry> =
            self.asset_json(cancel, "config.json").await?;
        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.catalog = Some(catalog.clone());
            guard.catalog_at = Some(SystemTime::now());
        }
        Ok(catalog)
    }

    fn start(self: &Arc<Self>) -> Option<RunningGuard> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if guard.running {
            return None;
        }
        guard.running = true;
        Some(RunningGuard(Arc::clone(self)))
    }

    fn finish(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.running = false;
    }

    async fn paste(
        &self,
        cancel: &CancellationToken,
        canvas: &mut PremulImage,
        role: &EatgifRole,
        face: &PremulImage,
    ) -> Result<(), CmdError> {
        let mask_data = self.asset(cancel, &role.mask, 5 << 20).await?;
        paste_with_mask(canvas, role, face, &mask_data)
    }

    fn help(&self, prefix: &str) -> String {
        let p = escape(prefix);
        let text = if kit::is_zh() {
            format!(
                "🎬 <b>头像动画贴纸</b>\n\n\
                回复一条消息（用户或频道发的都可以），把双方头像合成为动画贴纸。\n\n\
                • 回复一条消息发 <code>{p}eatgif 名称</code> 生成\n\
                • <code>{p}eatgif list</code> 列出全部可用动画\n\
                • <code>{p}eatgif clear</code> 清空素材缓存\n\n"
            )
        } else {
            format!(
                "🎬 <b>Avatar Animated Stickers</b>\n\n\
                Reply to a message (from a user or channel) to combine both avatars into an animated sticker.\n\n\
                • Reply to a message with <code>{p}eatgif name</code> to generate\n\
                • <code>{p}eatgif list</code> list all available animations\n\
                • <code>{p}eatgif clear</code> clear asset cache\n\n"
            )
        };
        let cat_opt = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .catalog
            .clone();
        let Some(catalog) = cat_opt else {
            return if kit::is_zh() {
                format!("{text}动画列表暂时读不到，稍后发 <code>{p}eatgif list</code> 查看。")
            } else {
                format!("{text}Animation list is unavailable right now; send <code>{p}eatgif list</code> later.")
            };
        };
        let mut names: Vec<&String> = catalog.keys().collect();
        names.sort();
        let mut lines = vec![if kit::is_zh() {
            format!("🎬 <b>全部动画</b>（{} 款）", names.len())
        } else {
            format!("🎬 <b>All Animations</b> ({})", names.len())
        }];
        for name in names {
            lines.push(format!("• {} {}", code(name), escape(&catalog[name].desc)));
        }
        if kit::is_zh() {
            format!(
                "{text}{}\n\n素材首次使用时从远程下载并缓存，需要主机装有 ffmpeg。",
                lines.join("\n")
            )
        } else {
            format!(
                "{text}{}\n\nAssets are downloaded and cached on first use; requires ffmpeg on host.",
                lines.join("\n")
            )
        }
    }

    async fn handle(self: Arc<Self>, inv: &Invocation) -> Result<(), CmdError> {
        let sub = inv.arg(0).to_ascii_lowercase();
        if sub == "clear" {
            return self.clear(inv).await;
        }
        let catalog = self
            .get_catalog(&inv.cancel)
            .await
            .map_err(|e| kit::wrap_fail(kit::tr("Failed to load asset list", "无法读取素材列表"), e))?;
        if sub.is_empty() || sub == "list" || sub == "ls" {
            return self.list(inv, &catalog).await;
        }
        let Some(selected) = catalog.get(&sub).cloned() else {
            return Err(kit::fail(if kit::is_zh() {
                format!("找不到 {sub}，发 {}eatgif list 看全部动画", inv.prefix())
            } else {
                format!("Unknown animation {sub}; send {}eatgif list to view all", inv.prefix())
            }));
        };
        let Some(reply) = kit::reply_msg(inv).await? else {
            return Err(kit::fail(kit::tr(
                "Reply to a message (user or channel) first",
                "请回复一条消息后再生成，用户或频道发的都可以",
            )));
        };
        let Some(_running) = self.start() else {
            return Err(kit::fail(kit::tr(
                "Another animation is already being generated; please wait",
                "已有一个动图正在生成，请稍候",
            )));
        };
        self.generate(inv, &selected, &reply).await
    }

    async fn clear(&self, inv: &Invocation) -> Result<(), CmdError> {
        let dir = self.data_dir.join("eatgif");
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.catalog = None;
            guard.catalog_at = None;
        }
        inv.edit_text(kit::tr(
            "✅ Asset cache cleared; will re-download on next use",
            "✅ 已清理素材缓存，下次使用时重新下载",
        ))
        .await
        .map_err(Into::into)
    }

    async fn list(
        &self,
        inv: &Invocation,
        catalog: &HashMap<String, EatgifEntry>,
    ) -> Result<(), CmdError> {
        let mut names: Vec<&String> = catalog.keys().collect();
        names.sort();
        let mut lines = if kit::is_zh() {
            vec![
                "🎬 <b>头像动画贴纸</b>".to_owned(),
                format!("回复一条消息发 {} 生成", code(&format!("{}eatgif 名称", inv.prefix()))),
                String::new(),
            ]
        } else {
            vec![
                "🎬 <b>Avatar Animated Stickers</b>".to_owned(),
                format!("Reply to a message with {}", code(&format!("{}eatgif name", inv.prefix()))),
                String::new(),
            ]
        };
        for name in names {
            lines.push(format!("• {} {}", code(name), escape(&catalog[name].desc)));
        }
        inv.respond_pages(&html_pages(&lines.join("\n"), PAGE_LIMIT))
            .await
            .map_err(Into::into)
    }

    async fn generate(
        &self,
        inv: &Invocation,
        selected: &EatgifEntry,
        reply: &Message,
    ) -> Result<(), CmdError> {
        inv.edit_text(&kit::working(&if kit::is_zh() {
            format!("正在生成「{}」", selected.desc)
        } else {
            format!("Generating \"{}\"", selected.desc)
        }))
        .await?;
        let spec: EatgifSpec = self
            .asset_json(&inv.cancel, &selected.url)
            .await
            .map_err(|e| kit::wrap_fail(kit::tr("Failed to load animation spec", "无法读取动画定义"), e))?;
        if spec.width < 1
            || spec.height < 1
            || spec.width > 512
            || spec.height > 512
            || spec.frames.is_empty()
            || spec.frames.len() > EATGIF_MAX_FRAMES
        {
            return Err(kit::fail(kit::tr("Invalid animation spec", "动画定义无效")));
        }
        let faces = load_faces(inv, reply).await?;
        let webm = self.render(&inv.cancel, &spec, &faces).await?;
        let peer = inv.peer_result()?;
        send_sticker_doc(
            &inv.client,
            &peer,
            &webm,
            "sticker",
            "✨",
            inv.message.reply_to_id,
            inv.message.topic_id,
        )
        .await?;
        inv.delete().await.map_err(Into::into)
    }

    async fn render(
        &self,
        cancel: &CancellationToken,
        spec: &EatgifSpec,
        faces: &AvatarPair,
    ) -> Result<Vec<u8>, CmdError> {
        let workdir = TempWorkdir::create()
            .map_err(|e| kit::fail_with(kit::tr("Video encoding failed", "视频编码失败"), e))?;
        let mut frames = Vec::with_capacity(spec.frames.len());
        for (idx, entry) in spec.frames.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(BotError::Cancelled.into());
            }
            let canvas_data = self
                .asset(cancel, &entry.url, 5 << 20)
                .await
                .map_err(|e| kit::wrap_fail(kit::tr("Asset download failed", "素材下载失败"), e))?;
            let mut canvas = frame_canvas(&canvas_data)
                .map_err(|_| kit::fail(kit::tr("Invalid asset image", "素材图片无效")))?;
            if let Some(ref you) = entry.you {
                self.paste(cancel, &mut canvas, you, &faces.you)
                    .await
                    .map_err(|e| kit::wrap_fail(kit::tr("Composition failed", "合成失败"), e))?;
            }
            if let Some(ref me) = entry.me {
                self.paste(cancel, &mut canvas, me, &faces.me)
                    .await
                    .map_err(|e| kit::wrap_fail(kit::tr("Composition failed", "合成失败"), e))?;
            }
            let frame_path = workdir.path().join(format!("frame{idx:04}.png"));
            let mut file = File::create(&frame_path)?;
            write_png(&mut file, &canvas)
                .map_err(|e| kit::fail_with(kit::tr("Composition failed", "合成失败"), e))?;
            let delay = entry.delay.map(|d| d.clamp(20, 5000)).unwrap_or(100) as u64;
            frames.push(Frame {
                path: frame_path,
                delay: Duration::from_millis(delay),
            });
        }
        sticker_webm(workdir.path(), &frames, spec.width, spec.height)
            .await
            .map_err(|e| kit::fail_with(kit::tr("Video encoding failed", "视频编码失败"), e))
    }
}

fn frame_canvas(data: &[u8]) -> Result<PremulImage, mibot_imaging::ImagingError> {
    decode(data)
}

fn paste_with_mask(
    canvas: &mut PremulImage,
    role: &EatgifRole,
    face: &PremulImage,
    mask_data: &[u8],
) -> Result<(), CmdError> {
    let comp_err = || kit::tr("Composition failed", "合成失败");
    let mask = decode_png(mask_data).map_err(|e| kit::fail_with(comp_err(), e))?;
    if mask.width() > 512 || mask.height() > 512 {
        return Err(kit::fail(kit::tr(
            "Invalid asset mask dimensions",
            "素材遮罩尺寸异常",
        )));
    }
    let mut shaped = resize(face, mask.width(), mask.height())
        .map_err(|e| kit::fail_with(comp_err(), e))?;
    if let Some(deg) = role.rotate
        && deg != 0.0
    {
        shaped = rotate(&shaped, deg.clamp(-360.0, 360.0))
            .map_err(|e| kit::fail_with(comp_err(), e))?;
    }
    if let Some(factor) = role.brightness
        && (factor - 1.0).abs() > f64::EPSILON
    {
        shaped = brightness(&shaped, factor.clamp(0.1, 2.0));
    }
    let masked = apply_mask(&shaped, &mask).map_err(|e| kit::fail_with(comp_err(), e))?;
    composite(canvas, &masked, role.x, role.y);
    Ok(())
}

struct AvatarPair {
    me: PremulImage,
    you: PremulImage,
}

async fn load_faces(inv: &Invocation, reply: &Message) -> Result<AvatarPair, CmdError> {
    let me = load_face(&inv.client, &own_face(inv), true).await?;
    let peer = inv
        .client
        .input_peer(&reply.sender)
        .map_err(|_| kit::fail(kit::tr("Cannot resolve target user", "无法解析对方的身份")))?;
    let you = load_face(&inv.client, &peer, false).await?;
    Ok(AvatarPair { me, you })
}

fn own_face(inv: &Invocation) -> tl::enums::InputPeer {
    speaker_of(&inv.client, &inv.message)
}

fn speaker_of(client: &Client, message: &Message) -> tl::enums::InputPeer {
    if let tl::enums::Peer::Channel(_) = &message.sender
        && let Ok(peer) = client.input_peer(&message.sender)
    {
        return peer;
    }
    tl::enums::InputPeer::PeerSelf
}

async fn load_face(
    client: &Client,
    peer: &tl::enums::InputPeer,
    is_self: bool,
) -> Result<PremulImage, CmdError> {
    let no_photo = || {
        if is_self {
            kit::tr("You have no public profile photo", "你没有公开头像，合成不了")
        } else {
            kit::tr("Target has no public profile photo", "对方没有公开头像，合成不了")
        }
    };
    let fetch_err = || {
        if is_self {
            kit::tr("Failed to fetch your profile photo", "无法获取你的头像")
        } else {
            kit::tr("Failed to fetch target profile photo", "无法获取对方的头像")
        }
    };
    let decode_err = || {
        if is_self {
            kit::tr("Failed to decode your profile photo", "无法解析你的头像")
        } else {
            kit::tr("Failed to decode target profile photo", "无法解析对方的头像")
        }
    };
    let mut data = client
        .download_profile_photo(peer, false, 2 << 20)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    if data.is_empty() {
        data = match client.download_profile_photo(peer, true, 2 << 20).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err(kit::fail(no_photo())),
            Err(_) => return Err(kit::fail(fetch_err())),
        };
    }
    if data.is_empty() {
        return Err(kit::fail(no_photo()));
    }
    decode(&data).map_err(|_| kit::fail(decode_err()))
}

async fn send_sticker_doc(
    client: &Client,
    peer: &tl::enums::InputPeer,
    data: &[u8],
    base_name: &str,
    alt: &str,
    reply_to: i32,
    topic_id: i32,
) -> Result<(), BotError> {
    let media = sticker_document(base_name, data, alt, reply_to);
    let attrs: Vec<tl::enums::DocumentAttribute> = media
        .attributes
        .into_iter()
        .map(|a| match a {
            StickerAttribute::Sticker { alt } => tl::types::DocumentAttributeSticker {
                mask: false,
                alt,
                stickerset: tl::enums::InputStickerSet::Empty,
                mask_coords: None,
            }
            .into(),
            StickerAttribute::ImageSize { w, h } => tl::types::DocumentAttributeImageSize {
                w: w as i32,
                h: h as i32,
            }
            .into(),
            StickerAttribute::Video { w, h, duration } => tl::types::DocumentAttributeVideo {
                round_message: false,
                supports_streaming: true,
                nosound: false,
                duration,
                w: w as i32,
                h: h as i32,
                preload_prefix_size: None,
                video_start_ts: None,
                video_codec: None,
            }
            .into(),
        })
        .collect();
    client
        .send_document(
            peer,
            data,
            mibot_bot::MediaOptions {
                name: media.name,
                mime_type: media.mime_type.to_owned(),
                caption: String::new(),
                reply_to: Some(reply_to).filter(|&id| id > 0),
                topic_id: Some(topic_id).filter(|&id| id > 0),
                entities: None,
                html: false,
                force_document: false,
                attributes: attrs,
            },
        )
        .await?;
    Ok(())
}

// ---------- .eat and .eat2 ----------

#[derive(Debug, Clone, Default, Deserialize)]
struct EatStamp {
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    scale: Option<f64>,
    #[serde(default)]
    rotate: Option<f64>,
    #[serde(default)]
    opacity: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct EatEntry {
    name: String,
    url: String,
    #[serde(default)]
    me: Option<EatgifRole>,
    #[serde(default)]
    you: Option<EatgifRole>,
    #[serde(default)]
    stamp: Option<EatStamp>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct EatSettings {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    source: String,
}

struct EatService {
    data_dir: PathBuf,
    logs: Option<Arc<mibot_logtail::Ring>>,
    settings: Store<EatSettings>,
    state: Mutex<Option<(HashMap<String, EatEntry>, String)>>,
}

fn eat_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{1,40}$").unwrap())
}

fn eat_root(source: &str) -> Result<String, CmdError> {
    let bad_host = || kit::tr("Source URL must be on raw.githubusercontent.com", "素材目录只能是 raw.githubusercontent.com 上的链接");
    let Ok(parsed) = Url::parse(source) else {
        return Err(kit::fail(bad_host()));
    };
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("raw.githubusercontent.com")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(kit::fail(bad_host()));
    }
    let parts: Vec<&str> = parsed.path().trim_start_matches('/').split('/').collect();
    let keep = if parts.len() > 4 && parts[2] == "refs" && parts[3] == "heads" {
        5
    } else {
        3
    };
    if parts.len() <= keep {
        return Err(kit::fail(kit::tr(
            "Incomplete source URL; must point to config.json in a repo",
            "素材目录链接不完整，要指向仓库里的 config.json",
        )));
    }
    Ok(format!(
        "https://raw.githubusercontent.com/{}/",
        parts[..keep].join("/")
    ))
}

fn eat_asset_url(root: &str, value: &str) -> Result<String, CmdError> {
    if value.starts_with("https://") {
        let bad_url = || kit::tr("Asset URL must point to GitHub", "素材链接只能指向 GitHub");
        let Ok(parsed) = Url::parse(value) else {
            return Err(kit::fail(bad_url()));
        };
        let host = parsed.host_str().unwrap_or("");
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || (host != "github.com" && host != "raw.githubusercontent.com")
        {
            return Err(kit::fail(bad_url()));
        }
        return Ok(value.to_owned());
    }
    let clean = safe_relative(value)?;
    Ok(format!("{root}{clean}"))
}

#[derive(Deserialize)]
struct EatConfigDoc {
    resources: HashMap<String, EatEntry>,
}

impl EatService {
    async fn load(
        &self,
        cancel: &CancellationToken,
        force: bool,
    ) -> Result<(HashMap<String, EatEntry>, String), CmdError> {
        if !force {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = guard.as_ref() {
                return Ok(cached.clone());
            }
        }
        let settings = self.settings.read()?;
        let source = kit::or_default(&settings.source, EAT_SOURCE);
        let root = eat_root(source)?;
        let resp = do_get_cancel(
            cancel,
            mibot_httpx::Request {
                url: source.to_owned(),
                timeout: Some(Duration::from_secs(30)),
                max_bytes: Some(1 << 20),
                ..Default::default()
            },
        )
        .await?;
        if !resp.ok() {
            return Err(mibot_httpx::HttpxError::Status(resp.status).into());
        }
        let bad_fmt = || kit::tr("Invalid asset catalog format", "素材目录格式不对");
        let Ok(doc) = serde_json::from_slice::<EatConfigDoc>(&resp.body) else {
            return Err(kit::fail(bad_fmt()));
        };
        if doc.resources.is_empty() || doc.resources.len() > 200 {
            return Err(kit::fail(bad_fmt()));
        }
        for (key, entry) in &doc.resources {
            if !eat_key_re().is_match(key)
                || entry.name.is_empty()
                || entry.name.chars().count() > 100
                || entry.url.is_empty()
            {
                return Err(kit::fail(bad_fmt()));
            }
        }
        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some((doc.resources.clone(), root.clone()));
        }
        Ok((doc.resources, root))
    }

    async fn asset(
        &self,
        cancel: &CancellationToken,
        root: &str,
        value: &str,
    ) -> Result<Vec<u8>, CmdError> {
        let link = eat_asset_url(root, value)?;
        fetch_cached(
            cancel,
            &self.data_dir,
            self.logs.as_ref(),
            "eat",
            &link,
            &link,
            10 << 20,
        )
        .await
        .map_err(Into::into)
    }

    async fn compose(
        &self,
        inv: &Invocation,
        root: &str,
        entry: &EatEntry,
        you: &PremulImage,
    ) -> Result<PremulImage, CmdError> {
        let base_data = self.asset(&inv.cancel, root, &entry.url).await?;
        let base = decode(&base_data)
            .map_err(|_| kit::fail(kit::tr("Invalid asset image", "素材图片无效")))?;
        if let Some(ref st) = entry.stamp {
            return stamp(st, you, &base);
        }
        let mut canvas = base;
        if let Some(ref role) = entry.you {
            let mask_data = self.asset(&inv.cancel, root, &role.mask).await?;
            paste_with_mask(&mut canvas, role, you, &mask_data)?;
        }
        if let Some(ref role) = entry.me {
            let me = load_face(&inv.client, &own_face(inv), true).await?;
            let mask_data = self.asset(&inv.cancel, root, &role.mask).await?;
            paste_with_mask(&mut canvas, role, &me, &mask_data)?;
        }
        Ok(canvas)
    }

    async fn render(
        &self,
        inv: &Invocation,
        reply: &Message,
        root: &str,
        entry: &EatEntry,
        from_image: bool,
    ) -> Result<Vec<u8>, CmdError> {
        let you = if from_image {
            replied_image(inv, reply).await?
        } else {
            let peer = inv
                .client
                .input_peer(&reply.sender)
                .map_err(|_| kit::fail(kit::tr("Cannot resolve target user", "无法解析对方的身份")))?;
            load_face(&inv.client, &peer, false).await?
        };
        let composed = self.compose(inv, root, entry, &you).await?;
        let gen_err = || kit::tr("Generation failed", "生成失败");
        let resized = resize_sticker(&composed).map_err(|e| kit::fail_with(gen_err(), e))?;
        let png = mibot_imaging::encode_png(&resized).map_err(|e| kit::fail_with(gen_err(), e))?;
        sticker_webp(&png)
            .await
            .map_err(|e| kit::fail_with(gen_err(), e))
    }

    async fn set(
        &self,
        inv: &Invocation,
        name: &str,
        link: &str,
    ) -> Result<(), CmdError> {
        if !link.is_empty() {
            let source = if link.eq_ignore_ascii_case("default") {
                String::new()
            } else {
                eat_root(link)?;
                link.to_owned()
            };
            kit::update_ok(&self.settings, move |s| s.source = source)?;
        }
        inv.edit_text(&kit::working(kit::tr(
            "Re-downloading asset catalog",
            "正在重新下载素材目录",
        )))
        .await?;
        let (catalog, _) = self
            .load(&inv.cancel, true)
            .await
            .map_err(|e| kit::wrap_fail(kit::tr("Failed to read asset catalog", "读不到素材目录"), e))?;
        let dir = self.data_dir.join("eat");
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        inv.respond_pages(&html_pages(
            &format!(
                "{}\n{}",
                kit::tr("✅ Asset catalog updated", "✅ 素材目录已更新"),
                eat_list(&catalog, inv.prefix(), name)
            ),
            PAGE_LIMIT,
        ))
        .await
        .map_err(Into::into)
    }

    fn help(&self, prefix: &str) -> String {
        let p = escape(prefix);
        let text = if kit::is_zh() {
            format!(
                "😋 <b>头像表情贴纸</b>\n\n\
                把头像合成到表情图里，发成贴纸。\n\n\
                <b>用法</b>\n\
                • 回复一条消息发 <code>{p}eat 名称</code>，用对方的头像生成，比如 <code>{p}eat bc</code>；不写名称随机挑一款\n\
                • <code>{p}eat2 名称</code> 同上，但用被回复的图片或贴纸代替头像\n\
                • 不回复消息发 <code>{p}eat</code> 也能看到下面这份列表\n\
                • <code>{p}eat set</code> 重新下载素材目录；<code>{p}eat set 链接</code> 换成别的目录\
                （要是 raw.githubusercontent.com 上的 config.json），<code>{p}eat set default</code> 换回默认\n\n"
            )
        } else {
            format!(
                "😋 <b>Avatar Meme Stickers</b>\n\n\
                Compose avatars onto meme templates and send as stickers.\n\n\
                <b>Usage</b>\n\
                • Reply to a message with <code>{p}eat name</code> using the sender's avatar (e.g. <code>{p}eat bc</code>); omit name for random\n\
                • <code>{p}eat2 name</code> same as above, but uses the replied image or sticker instead of avatar\n\
                • <code>{p}eat</code> without replying to view the list below\n\
                • <code>{p}eat set</code> re-download catalog; <code>{p}eat set url</code> use a custom catalog \
                (must be config.json on raw.githubusercontent.com); <code>{p}eat set default</code> restore default\n\n"
            )
        };
        let cat_opt = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(c, _)| c.clone());
        let Some(catalog) = cat_opt else {
            return if kit::is_zh() {
                format!("{text}素材目录暂时读不到，稍后发 <code>{p}eat</code> 查看全部款式。")
            } else {
                format!("{text}Catalog is unavailable right now; send <code>{p}eat</code> later to view all styles.")
            };
        };
        let mut lines = vec![
            if kit::is_zh() {
                format!("😋 <b>全部款式</b>（{} 款）", catalog.len())
            } else {
                format!("😋 <b>All Styles</b> ({})", catalog.len())
            },
            eat_legend().to_owned(),
        ];
        lines.extend(eat_catalog_lines(&catalog));
        if kit::is_zh() {
            format!(
                "{text}{}\n\n素材来自 TeleBox 插件仓库，首次使用时下载并缓存，需要主机装有 ffmpeg。",
                lines.join("\n")
            )
        } else {
            format!(
                "{text}{}\n\nAssets from TeleBox plugin repo, downloaded and cached on first use; requires ffmpeg on host.",
                lines.join("\n")
            )
        }
    }
}

fn stamp(config: &EatStamp, face: &PremulImage, mark: &PremulImage) -> Result<PremulImage, CmdError> {
    let size = config.size.map(|s| s.clamp(64, 512)).unwrap_or(512) as u32;
    let scale = config.scale.map(|v| v.clamp(0.05, 4.0)).unwrap_or(0.9);
    let deg = config.rotate.map(|v| v.clamp(-360.0, 360.0)).unwrap_or(-12.0);
    let alpha = config.opacity.map(|v| v.clamp(0.0, 1.0)).unwrap_or(0.6);

    let gen_err = || kit::tr("Generation failed", "生成失败");
    let mut canvas = resize_cover(face, size, size).map_err(|e| kit::fail_with(gen_err(), e))?;
    let width = ((f64::from(size) * scale).round() as i64).max(1) as u32;
    let height = ((f64::from(mark.height()) * f64::from(width) / f64::from(mark.width().max(1))).round() as i64).max(1) as u32;
    let resized_mark = resize(mark, width, height).map_err(|e| kit::fail_with(gen_err(), e))?;
    let rotated = rotate_expand(&resized_mark, deg).map_err(|e| kit::fail_with(gen_err(), e))?;
    let turned = opacity(&rotated, alpha);
    let fit = (f64::from(size) / f64::from(turned.width().max(1)))
        .min(f64::from(size) / f64::from(turned.height().max(1)));
    let fit_w = ((f64::from(turned.width()) * fit) as i64).max(1) as u32;
    let fit_h = ((f64::from(turned.height()) * fit) as i64).max(1) as u32;
    let fitted = resize(&turned, fit_w, fit_h).map_err(|e| kit::fail_with(gen_err(), e))?;
    let x = ((size as i32) - (fitted.width() as i32)) / 2;
    let y = ((size as i32) - (fitted.height() as i32)) / 2;
    composite(&mut canvas, &fitted, x, y);
    Ok(canvas)
}

async fn replied_image(inv: &Invocation, reply: &Message) -> Result<PremulImage, CmdError> {
    let mut source = reply.raw.clone();
    if let Some(wrapped) = web_page_media(&reply.raw) {
        source = wrapped;
    }
    let doc = match source.media.as_ref() {
        Some(tl::enums::MessageMedia::Document(d)) => match d.document.as_ref() {
            Some(tl::enums::Document::Document(raw)) => Some(raw.clone()),
            _ => None,
        },
        _ => None,
    };
    if let Some(ref d) = doc
        && !d.mime_type.starts_with("image/")
    {
        return document_thumb(&inv.client, d).await;
    }
    match inv.client.download_media(&source, 10 << 20).await {
        Ok(file) => {
            if let Ok(decoded) = decode(&file.data) {
                return Ok(decoded);
            }
            if let Some(ref d) = doc {
                return document_thumb(&inv.client, d).await;
            }
            Err(kit::fail(kit::tr("Cannot decode this image", "这张图片解析不了")))
        }
        Err(_) => {
            if let Some(ref d) = doc {
                return document_thumb(&inv.client, d).await;
            }
            Err(kit::fail(kit::tr(
                "Reply to an image or sticker",
                "请回复一条图片或贴纸",
            )))
        }
    }
}

fn web_page_media(message: &tl::types::Message) -> Option<tl::types::Message> {
    let tl::enums::MessageMedia::WebPage(preview) = message.media.as_ref()? else {
        return None;
    };
    let tl::enums::WebPage::Page(page) = &preview.webpage else {
        return None;
    };
    let mut wrapped = kit::blank_tl_message(message.id, message.peer_id.clone());
    if let Some(doc @ tl::enums::Document::Document(_)) = &page.document {
        wrapped.media = Some(
            tl::types::MessageMediaDocument {
                nopremium: false,
                spoiler: false,
                video: false,
                round: false,
                voice: false,
                document: Some(doc.clone()),
                alt_documents: None,
                video_cover: None,
                video_timestamp: None,
                ttl_seconds: None,
            }
            .into(),
        );
        return Some(wrapped);
    }
    if let Some(photo @ tl::enums::Photo::Photo(_)) = &page.photo {
        wrapped.media = Some(
            tl::types::MessageMediaPhoto {
                spoiler: false,
                live_photo: false,
                photo: Some(photo.clone()),
                ttl_seconds: None,
                video: None,
            }
            .into(),
        );
        return Some(wrapped);
    }
    None
}

async fn document_thumb(
    client: &Client,
    doc: &tl::types::Document,
) -> Result<PremulImage, CmdError> {
    let mut best = String::new();
    let mut best_area = 0i64;
    if let Some(ref thumbs) = doc.thumbs {
        for item in thumbs {
            if let tl::enums::PhotoSize::Size(sz) = item {
                let area = i64::from(sz.w) * i64::from(sz.h);
                if area > best_area {
                    best_area = area;
                    best = sz.r#type.clone();
                }
            }
        }
    }
    if best.is_empty() {
        return Err(kit::fail(kit::tr(
            "No preview available for this file",
            "这个文件没有可用的预览图",
        )));
    }
    let location = tl::types::InputDocumentFileLocation {
        id: doc.id,
        access_hash: doc.access_hash,
        file_reference: doc.file_reference.clone(),
        thumb_size: best,
    }
    .into();
    let data = client
        .download_file(location, doc.dc_id, 5 << 20)
        .await
        .map_err(|_| kit::fail(kit::tr("Failed to download preview", "预览图下载失败")))?;
    decode(&data).map_err(|_| kit::fail(kit::tr("Cannot decode preview", "预览图解析不了")))
}

fn eat_legend() -> &'static str {
    kit::tr(
        "👥 Uses both avatars  🙋 Uses your avatar only  Others use target's avatar",
        "👥 同时用对方和你的头像　🙋 只用你的头像　其余只用对方的",
    )
}

fn eat_catalog_lines(catalog: &HashMap<String, EatEntry>) -> Vec<String> {
    let mut keys: Vec<&String> = catalog.keys().collect();
    keys.sort();
    let mut lines = Vec::with_capacity(keys.len());
    for key in keys {
        let entry = &catalog[key];
        let mut line = format!("• {} {}", code(key), escape(&entry.name));
        if entry.me.is_some() && entry.you.is_some() {
            line.push_str(" 👥");
        } else if entry.me.is_some() {
            line.push_str(" 🙋");
        }
        lines.push(line);
    }
    lines
}

fn eat_list(catalog: &HashMap<String, EatEntry>, prefix: &str, name: &str) -> String {
    let mut lines = if kit::is_zh() {
        vec![
            format!("😋 <b>头像表情贴纸</b>（{} 款）", catalog.len()),
            format!(
                "回复一条消息发 {}，不写名称随机挑一款",
                code(&format!("{prefix}{name} 名称"))
            ),
            eat_legend().to_owned(),
            String::new(),
        ]
    } else {
        vec![
            format!("😋 <b>Avatar Meme Stickers</b> ({})", catalog.len()),
            format!(
                "Reply to a message with {}; omit name for random",
                code(&format!("{prefix}{name} name"))
            ),
            eat_legend().to_owned(),
            String::new(),
        ]
    };
    lines.extend(eat_catalog_lines(catalog));
    lines.join("\n")
}

pub fn register(a: &Arc<App>) {
    let eat_svc = Arc::new(EatService {
        data_dir: a.data_dir(),
        logs: Some(Arc::clone(&a.logs)),
        settings: kit::new_store(a, "eat.json", EatSettings::default),
        state: Mutex::new(None),
    });
    for (cmd_name, desc_en, desc_zh, from_image) in [
        ("eat", "Avatar meme sticker", "用头像生成表情贴纸", false),
        ("eat2", "Image meme sticker", "用图片生成表情贴纸", true),
    ] {
        let svc_help = Arc::clone(&eat_svc);
        let svc_handle = Arc::clone(&eat_svc);
        let mut cmd = kit::make_cmd(
            cmd_name,
            &[],
            group::MEDIA,
            desc_en,
            "[name|set url]",
            move |p| svc_help.help(p),
            move |inv| {
                let svc = Arc::clone(&svc_handle);
                Box::pin(async move {
                    let sub = inv.arg(0);
                    if sub.eq_ignore_ascii_case("set") && inv.message.reply_to_id == 0 {
                        return svc.set(&inv, cmd_name, inv.arg(1)).await;
                    }
                    let reply = kit::reply_msg(&inv).await?;
                    let (catalog, root) = svc
                        .load(&inv.cancel, false)
                        .await
                        .map_err(|e| kit::wrap_fail(kit::tr("Failed to read asset catalog", "读不到素材目录"), e))?;
                    let Some(reply) = reply else {
                        return inv
                            .respond_pages(&html_pages(
                                &eat_list(&catalog, inv.prefix(), cmd_name),
                                PAGE_LIMIT,
                            ))
                            .await
                            .map_err(Into::into);
                    };
                    let key = if sub.is_empty() {
                        let mut keys: Vec<&String> = catalog.keys().collect();
                        keys.sort();
                        keys[(mibot_bot::random_id().unsigned_abs() as usize) % keys.len()]
                            .clone()
                    } else {
                        sub.to_owned()
                    };
                    let Some(entry) = catalog.get(&key).cloned() else {
                        return Err(kit::fail(if kit::is_zh() {
                            format!(
                                "找不到 {key}，不回复消息发 {}{} 看全部款式",
                                inv.prefix(),
                                cmd_name
                            )
                        } else {
                            format!(
                                "Unknown style {key}; send {}{} without replying to view all",
                                inv.prefix(),
                                cmd_name
                            )
                        }));
                    };
                    inv.edit_text(&kit::working(&if kit::is_zh() {
                        format!("正在生成「{}」", entry.name)
                    } else {
                        format!("Generating \"{}\"", entry.name)
                    }))
                    .await?;
                    let result = svc
                        .render(&inv, &reply, &root, &entry, from_image)
                        .await
                        .map_err(|e| kit::wrap_fail(kit::tr("Generation failed", "生成失败"), e))?;
                    let peer = inv.peer_result()?;
                    send_sticker_doc(
                        &inv.client,
                        &peer,
                        &result,
                        "sticker",
                        &entry.name,
                        inv.message.reply_to_id,
                        inv.message.topic_id,
                    )
                    .await?;
                    inv.delete().await.map_err(Into::into)
                })
            },
        )
        .zh(desc_zh, "[名称|set 链接]");
        cmd.timeout = Some(Duration::from_secs(120));
        a.registry.register(cmd);
    }

    let gif_svc = Arc::new(EatgifService {
        data_dir: a.data_dir(),
        logs: Some(Arc::clone(&a.logs)),
        state: Mutex::new(EatgifState::default()),
    });
    let gif_help = Arc::clone(&gif_svc);
    let gif_handle = Arc::clone(&gif_svc);
    let mut gif_cmd = kit::make_cmd(
        "eatgif",
        &[],
        group::MEDIA,
        "Animated avatar sticker",
        "[name|list|clear]",
        move |p| gif_help.help(p),
        move |inv| {
            let svc = Arc::clone(&gif_handle);
            Box::pin(async move { svc.handle(&inv).await })
        },
    )
    .zh("用双方头像合成动画贴纸", "[名称|list|clear]");
    gif_cmd.timeout = Some(Duration::from_secs(300));
    a.registry.register(gif_cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct TempDirGuard(PathBuf);
    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "mibot-{prefix}-{}-{}",
                std::process::id(),
                mibot_bot::random_id().unsigned_abs()
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_frame_canvas_decodes_png_and_jpeg() {
        let mut img = image::RgbaImage::new(196, 106);
        for px in img.pixels_mut() {
            *px = image::Rgba([200, 200, 200, 255]);
        }
        let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
        let mut jpeg_data = Vec::new();
        image::DynamicImage::ImageRgb8(rgb)
            .write_to(&mut Cursor::new(&mut jpeg_data), image::ImageFormat::Jpeg)
            .unwrap();
        let mut png_data = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut png_data), image::ImageFormat::Png)
            .unwrap();

        for (label, bytes) in [("JPEG", jpeg_data), ("PNG", png_data)] {
            let mut canvas = frame_canvas(&bytes).unwrap();
            assert_eq!((canvas.width(), canvas.height()), (196, 106), "{label}");
            canvas.set_premul_pixel(0, 0, [255, 0, 0, 255]);
        }
        assert!(frame_canvas(b"not an image").is_err());
    }

    #[tokio::test]
    async fn test_fetch_fresh_falls_back_to_stale_copy() {
        let tmp = TempDirGuard::new("eatgif-fresh");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let key = "config.json";
        let url = "https://example.invalid/config.json";
        let cache = cache_path(&tmp.0, "eatgif", key);
        assert!(
            fetch_fresh(&cancel, None, &cache, url, 1 << 20, Duration::from_secs(3600))
                .await
                .is_err()
        );
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, br#"{"old":1}"#).unwrap();
        let data = fetch_fresh(
            &cancel,
            None,
            &cache,
            url,
            1 << 20,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(data, br#"{"old":1}"#);
        assert!(
            fetch_fresh(&cancel, None, &cache, url, 4, Duration::ZERO)
                .await
                .is_err()
        );
    }

    #[test]
    fn test_safe_relative() {
        for good in ["md/md1.png", "config.json", "a/b/c.png"] {
            assert!(safe_relative(good).is_ok(), "{good}");
        }
        for bad in ["", "../secrets", "a/../../b", "a//b", "C:\\x", "https://evil/x", "./x"] {
            assert!(safe_relative(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_eat_root_and_asset_url() {
        for (source, want) in [
            (
                "https://raw.githubusercontent.com/TeleBoxOrg/TeleBox-Plugins/main/eat/config.json",
                "https://raw.githubusercontent.com/TeleBoxOrg/TeleBox-Plugins/main/",
            ),
            (
                "https://raw.githubusercontent.com/TeleBoxOrg/TeleBox-Plugins/refs/heads/main/eat/config.json",
                "https://raw.githubusercontent.com/TeleBoxOrg/TeleBox-Plugins/refs/heads/main/",
            ),
            (
                "https://raw.githubusercontent.com/a/b/dev/config.json",
                "https://raw.githubusercontent.com/a/b/dev/",
            ),
        ] {
            assert_eq!(eat_root(source).unwrap(), want);
        }
        for bad in [
            "http://raw.githubusercontent.com/a/b/main/config.json",
            "https://example.com/a/b/main/config.json",
            "https://user@raw.githubusercontent.com/a/b/main/config.json",
            "https://raw.githubusercontent.com/a/b/main",
            "https://raw.githubusercontent.com/a/b/refs/heads/main",
        ] {
            assert!(eat_root(bad).is_err(), "{bad}");
        }

        let root = "https://raw.githubusercontent.com/o/r/main/";
        for (value, want) in [
            ("eat/eatat.png", "https://raw.githubusercontent.com/o/r/main/eat/eatat.png"),
            (
                "https://github.com/FoKit/PagerMaid_Plugins/raw/modify/eat/eatada.png",
                "https://github.com/FoKit/PagerMaid_Plugins/raw/modify/eat/eatada.png",
            ),
        ] {
            assert_eq!(eat_asset_url(root, value).unwrap(), want);
        }
        for bad in [
            "../secret",
            "eat/../../x",
            "/etc/passwd",
            "https://evil.example/x.png",
            "http://github.com/x.png",
            "file:///etc/passwd",
        ] {
            assert!(eat_asset_url(root, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_stamp() {
        let face = PremulImage::from_solid(300, 200, 255, 0, 0, 255).unwrap();
        let mark = PremulImage::from_solid(400, 100, 0, 0, 255, 255).unwrap();
        let result = stamp(&EatStamp::default(), &face, &mark).unwrap();
        assert_eq!((result.width(), result.height()), (512, 512));
        let corner = result.pixel(5, 5).unwrap();
        assert_eq!((corner[0], corner[1], corner[2]), (255, 0, 0));
        let center = result.pixel(256, 256).unwrap();
        assert!(
            center[2] >= 140 && center[2] <= 165 && center[0] >= 90 && center[0] <= 115,
            "center={center:?}"
        );
        let small = stamp(
            &EatStamp {
                size: Some(256),
                ..Default::default()
            },
            &face,
            &mark,
        )
        .unwrap();
        assert_eq!(small.width(), 256);
    }
}
