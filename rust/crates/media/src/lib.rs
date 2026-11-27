use mibot_imaging::PremulImage;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;

const WEBM_MAGIC: &[u8] = &[0x1a, 0x45, 0xdf, 0xa3];
const ENCODE_TIMEOUT: Duration = Duration::from_secs(180);

const EBML_SEGMENT: u32 = 0x1853_8067;
const EBML_INFO: u32 = 0x1549_a966;
const EBML_TIMESCALE: u32 = 0x002a_d7b1;
const EBML_DURATION: u32 = 0x4489;
const EBML_TRACKS: u32 = 0x1654_ae6b;
const EBML_TRACK: u32 = 0xae;
const EBML_VIDEO: u32 = 0xe0;
const EBML_WIDTH: u32 = 0xb0;
const EBML_HEIGHT: u32 = 0xba;
const EBML_CLUSTER: u32 = 0x1f43_b675;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("ffmpeg is unavailable; please install ffmpeg first")]
    FfmpegUnavailable,
    #[error("not a WebM file")]
    NotWebM,
    #[error("no frames to encode")]
    NoFrames,
    #[error("video encoding did not finish within {0:?}")]
    EncodeTimeout(Duration),
    #[error("ffmpeg failed: {0}")]
    FfmpegFailed(String),
    #[error("{0}")]
    Message(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("imaging error: {0}")]
    Imaging(#[from] mibot_imaging::ImagingError),
}

impl MediaError {
    pub fn user_message(&self) -> Option<&'static str> {
        match self {
            MediaError::FfmpegUnavailable => Some(mibot_i18n::tr(
                "ffmpeg is not installed on this host",
                "主机没有安装 ffmpeg，请先安装",
            )),
            _ => None,
        }
    }
}

pub fn find_ffmpeg() -> Result<PathBuf, MediaError> {
    for candidate in [
        "/usr/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "/opt/homebrew/bin/ffmpeg",
    ] {
        let p = Path::new(candidate);
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("ffmpeg");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(MediaError::FfmpegUnavailable)
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub path: PathBuf,
    pub delay: Duration,
}

#[derive(Debug, Clone)]
pub struct Audio {
    pub data: Vec<u8>,
    pub seconds: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StickerAttribute {
    Sticker { alt: String },
    ImageSize { w: u32, h: u32 },
    Video { w: u32, h: u32, duration: f64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct StickerMedia {
    pub name: String,
    pub mime_type: &'static str,
    pub reply_to: i32,
    pub attributes: Vec<StickerAttribute>,
}

pub fn resize_sticker(source: &PremulImage) -> Result<PremulImage, MediaError> {
    let w = source.width();
    let h = source.height();
    let longest = w.max(h);
    if longest == 512 {
        return Ok(source.clone());
    }
    let scale = 512.0 / f64::from(longest);
    let target_w = ((f64::from(w) * scale).round() as u32).max(1);
    let target_h = ((f64::from(h) * scale).round() as u32).max(1);
    Ok(mibot_imaging::resize(source, target_w, target_h)?)
}

pub fn sticker_document(name: &str, data: &[u8], alt: &str, reply_to: i32) -> StickerMedia {
    let mut attributes = vec![StickerAttribute::Sticker {
        alt: alt.to_owned(),
    }];
    if data.starts_with(WEBM_MAGIC) {
        let (w, h, duration) = webm_info(data).unwrap_or((512, 512, 0.0));
        attributes.push(StickerAttribute::ImageSize { w, h });
        attributes.push(StickerAttribute::Video { w, h, duration });
        return StickerMedia {
            name: with_extension(name, ".webm"),
            mime_type: "video/webm",
            reply_to,
            attributes,
        };
    }
    let (w, h) = mibot_imaging::webp_size(data).unwrap_or((512, 512));
    attributes.push(StickerAttribute::ImageSize { w, h });
    StickerMedia {
        name: with_extension(name, ".webp"),
        mime_type: "image/webp",
        reply_to,
        attributes,
    }
}

fn with_extension(name: &str, ext: &str) -> String {
    let stem = match name.rfind('.') {
        Some(idx) => &name[..idx],
        None => name,
    };
    format!("{stem}{ext}")
}

#[derive(Default)]
struct WebmHeader {
    width: u32,
    height: u32,
    scale: u64,
    duration: f64,
}

pub fn webm_info(data: &[u8]) -> Result<(u32, u32, f64), MediaError> {
    if !data.starts_with(WEBM_MAGIC) {
        return Err(MediaError::NotWebM);
    }
    let mut header = WebmHeader {
        scale: 1_000_000,
        ..Default::default()
    };
    walk_ebml(data, &mut header);
    if header.width == 0 || header.height == 0 {
        return Err(MediaError::NotWebM);
    }
    let mut seconds = header.duration * (header.scale as f64) / 1e9;
    if !seconds.is_finite() || seconds < 0.0 {
        seconds = 0.0;
    }
    Ok((header.width, header.height, seconds))
}

fn walk_ebml(mut data: &[u8], header: &mut WebmHeader) -> bool {
    while !data.is_empty() {
        let Some((id, id_len)) = ebml_id(data) else {
            return false;
        };
        let Some((size, size_len, known)) = ebml_size(&data[id_len..]) else {
            return false;
        };
        let rest = &data[id_len + size_len..];
        let complete = known && size <= rest.len() as u64;
        let body = if complete {
            &rest[..size as usize]
        } else {
            rest
        };

        match id {
            EBML_CLUSTER => return false,
            EBML_SEGMENT | EBML_INFO | EBML_TRACKS | EBML_TRACK | EBML_VIDEO => {
                if !walk_ebml(body, header) || !complete {
                    return false;
                }
            }
            EBML_TIMESCALE => {
                if !complete {
                    return false;
                }
                if let Some(v) = ebml_uint(body)
                    && v > 0
                {
                    header.scale = v;
                }
            }
            EBML_DURATION => {
                if !complete {
                    return false;
                }
                if let Ok(bytes) = <[u8; 4]>::try_from(body) {
                    header.duration = f64::from(f32::from_be_bytes(bytes));
                } else if let Ok(bytes) = <[u8; 8]>::try_from(body) {
                    header.duration = f64::from_be_bytes(bytes);
                }
            }
            EBML_WIDTH | EBML_HEIGHT => {
                if !complete {
                    return false;
                }
                if let Some(v) = ebml_uint(body)
                    && v <= (1 << 16)
                {
                    if id == EBML_WIDTH && header.width == 0 {
                        header.width = v as u32;
                    }
                    if id == EBML_HEIGHT && header.height == 0 {
                        header.height = v as u32;
                    }
                }
            }
            _ => {
                if !complete {
                    return false;
                }
            }
        }
        data = &rest[size as usize..];
    }
    true
}

fn ebml_id(data: &[u8]) -> Option<(u32, usize)> {
    let first = *data.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || data.len() < len {
        return None;
    }
    let mut id = 0u32;
    for &b in &data[..len] {
        id = (id << 8) | u32::from(b);
    }
    Some((id, len))
}

fn ebml_size(data: &[u8]) -> Option<(u64, usize, bool)> {
    let first = *data.first()?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || data.len() < len {
        return None;
    }
    let mask = if len >= 8 { 0 } else { 0xffu8 >> len };
    let mut size = u64::from(first & mask);
    let mut all_ones = size == u64::from(mask);
    for &b in &data[1..len] {
        size = (size << 8) | u64::from(b);
        all_ones = all_ones && b == 0xff;
    }
    Some((size, len, !all_ones))
}

fn ebml_uint(body: &[u8]) -> Option<u64> {
    if body.is_empty() || body.len() > 8 {
        return None;
    }
    let mut v = 0u64;
    for &b in body {
        v = (v << 8) | u64::from(b);
    }
    Some(v)
}

pub struct TempWorkdir {
    path: PathBuf,
}

static WORKDIR_SEQ: AtomicU64 = AtomicU64::new(0);

impl TempWorkdir {
    pub fn create() -> Result<Self, MediaError> {
        let pid = std::process::id();
        loop {
            let seq = WORKDIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("mibot-media-{pid}-{seq}"));
            match fs::create_dir(&dir) {
                Ok(()) => return Ok(Self { path: dir }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(MediaError::Io(e)),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkdir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub async fn sticker_webm(
    directory: &Path,
    frames: &[Frame],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, MediaError> {
    let binary = find_ffmpeg()?;
    if frames.is_empty() {
        return Err(MediaError::NoFrames);
    }
    let mut script = String::new();
    for frame in frames {
        let secs = frame.delay.as_secs_f64();
        let dur = if secs <= 0.0 { 0.1 } else { secs };
        let base = frame
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("frame.png");
        script.push_str(&format!("file '{base}'\nduration {dur:.3}\n"));
    }
    let last_base = frames[frames.len() - 1]
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("frame.png");
    script.push_str(&format!("file '{last_base}'\n"));
    fs::write(directory.join("frames.txt"), script.as_bytes())?;

    let vf = format!("scale={width}:{height}:flags=lanczos");
    let args = [
        "-nostdin",
        "-v",
        "error",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        "frames.txt",
        "-c:v",
        "libvpx-vp9",
        "-pix_fmt",
        "yuva420p",
        "-b:v",
        "0",
        "-crf",
        "41",
        "-deadline",
        "good",
        "-cpu-used",
        "5",
        "-row-mt",
        "1",
        "-threads",
        "2",
        "-auto-alt-ref",
        "0",
        "-an",
        "-vf",
        &vf,
        "-y",
        "sticker.webm",
    ];
    run_ffmpeg(&binary, directory, &args).await?;
    read_bounded(&directory.join("sticker.webm"), 20 << 20)
}

pub async fn to_sticker_webm(input: &[u8], extension: &str) -> Result<Vec<u8>, MediaError> {
    let binary = find_ffmpeg()?;
    let workdir = TempWorkdir::create()?;
    let source = format!("input{extension}");
    fs::write(workdir.path().join(&source), input)?;
    let args = [
        "-nostdin",
        "-v",
        "error",
        "-i",
        &source,
        "-c:v",
        "libvpx-vp9",
        "-pix_fmt",
        "yuva420p",
        "-b:v",
        "400k",
        "-deadline",
        "good",
        "-cpu-used",
        "5",
        "-row-mt",
        "1",
        "-threads",
        "2",
        "-auto-alt-ref",
        "0",
        "-an",
        "-y",
        "converted.webm",
    ];
    run_ffmpeg(&binary, workdir.path(), &args).await?;
    read_bounded(&workdir.path().join("converted.webm"), 20 << 20)
}

pub async fn sticker_webp(png: &[u8]) -> Result<Vec<u8>, MediaError> {
    let binary = find_ffmpeg()?;
    let workdir = TempWorkdir::create()?;
    fs::write(workdir.path().join("sticker.png"), png)?;
    let args = [
        "-nostdin",
        "-v",
        "error",
        "-i",
        "sticker.png",
        "-frames:v",
        "1",
        "-c:v",
        "libwebp",
        "-lossless",
        "0",
        "-quality",
        "95",
        "-y",
        "sticker.webp",
    ];
    run_ffmpeg(&binary, workdir.path(), &args).await?;
    read_bounded(&workdir.path().join("sticker.webp"), 5 << 20)
}

pub async fn voice_ogg(audio: &[u8]) -> Result<Audio, MediaError> {
    let args = [
        "-nostdin",
        "-v",
        "error",
        "-i",
        "speech.mp3",
        "-vn",
        "-c:a",
        "libopus",
        "-b:a",
        "64k",
        "-vbr",
        "on",
        "-y",
        "voice.ogg",
    ];
    encode_audio(audio, None, &args, "voice.ogg", 20 << 20).await
}

pub async fn tagged_mp3(audio: &[u8], tags: &Tags) -> Result<Audio, MediaError> {
    let title_arg = format!("title={}", tags.title);
    let artist_arg = format!("artist={}", tags.artist);
    let album_arg = format!("album={}", tags.album);
    let mut args = vec!["-nostdin", "-v", "error", "-i", "speech.mp3"];
    if tags.cover.as_ref().is_some_and(|c| !c.is_empty()) {
        args.extend([
            "-i",
            "cover.img",
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c:v",
            "mjpeg",
            "-disposition:v",
            "attached_pic",
            "-metadata:s:v",
            "title=Album cover",
            "-metadata:s:v",
            "comment=Cover (front)",
        ]);
    } else {
        args.extend(["-map", "0:a"]);
    }
    args.extend([
        "-c:a",
        "libmp3lame",
        "-q:a",
        "2",
        "-id3v2_version",
        "3",
        "-metadata",
        &title_arg,
        "-metadata",
        &artist_arg,
        "-metadata",
        &album_arg,
        "-y",
        "song.mp3",
    ]);
    encode_audio(audio, tags.cover.as_deref(), &args, "song.mp3", 30 << 20).await
}

async fn encode_audio(
    audio: &[u8],
    cover: Option<&[u8]>,
    args: &[&str],
    output: &str,
    limit: u64,
) -> Result<Audio, MediaError> {
    let binary = find_ffmpeg()?;
    let workdir = TempWorkdir::create()?;
    fs::write(workdir.path().join("speech.mp3"), audio)?;
    if let Some(cov) = cover
        && !cov.is_empty()
    {
        fs::write(workdir.path().join("cover.img"), cov)?;
    }
    run_ffmpeg(&binary, workdir.path(), args).await?;
    let data = read_bounded(&workdir.path().join(output), limit)?;
    let seconds = probe_duration(&binary, workdir.path(), output).await;
    Ok(Audio { data, seconds })
}

async fn probe_duration(binary: &Path, directory: &Path, name: &str) -> u32 {
    static DURATION_RE: OnceLock<Regex> = OnceLock::new();
    let re = DURATION_RE
        .get_or_init(|| Regex::new(r"Duration: (\d+):(\d{2}):(\d{2})\.(\d+)").expect("regex"));
    let mut cmd = Command::new(binary);
    cmd.args(["-nostdin", "-hide_banner", "-i", name])
        .current_dir(directory)
        .kill_on_drop(true);
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await else {
        return 0;
    };
    let text = String::from_utf8_lossy(&out.stderr);
    let Some(caps) = re.captures(&text) else {
        return 0;
    };
    let h: u32 = caps[1].parse().unwrap_or(0);
    let m: u32 = caps[2].parse().unwrap_or(0);
    let s: u32 = caps[3].parse().unwrap_or(0);
    let mut total = h * 3600 + m * 60 + s;
    if caps[4].chars().any(|c| c != '0') {
        total += 1;
    }
    total
}

async fn run_ffmpeg(binary: &Path, directory: &Path, args: &[&str]) -> Result<(), MediaError> {
    let mut cmd = Command::new(binary);
    cmd.args(args).current_dir(directory).kill_on_drop(true);
    let out = tokio::time::timeout(ENCODE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| MediaError::EncodeTimeout(ENCODE_TIMEOUT))??;
    if out.status.success() {
        return Ok(());
    }
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut detail = combined.trim().to_owned();
    if detail.is_empty() {
        detail = format!("exit status {}", out.status);
    }
    if detail.len() > 300 {
        let mut start = detail.len() - 300;
        while !detail.is_char_boundary(start) {
            start += 1;
        }
        detail = detail[start..].to_owned();
    }
    Err(MediaError::FfmpegFailed(detail))
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, MediaError> {
    let meta = fs::metadata(path)?;
    if meta.len() == 0 {
        return Err(MediaError::Message(
            "ffmpeg produced an empty file".into(),
        ));
    }
    if meta.len() > limit {
        return Err(MediaError::Message(format!(
            "ffmpeg produced {} bytes, over the limit",
            meta.len()
        )));
    }
    Ok(fs::read(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(id: &[u8], parts: &[&[u8]]) -> Vec<u8> {
        let body: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
        let mut size = (body.len() as u64).to_be_bytes();
        size[0] = 0x01;
        let mut out = Vec::with_capacity(id.len() + 8 + body.len());
        out.extend_from_slice(id);
        out.extend_from_slice(&size);
        out.extend_from_slice(&body);
        out
    }

    fn unknown_size(id: &[u8], parts: &[&[u8]]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        for p in parts {
            out.extend_from_slice(p);
        }
        out
    }

    #[test]
    fn resize_sticker_cases() {
        for (w, h, want_w, want_h) in [
            (231, 218, 512, 483),
            (1266, 830, 512, 336),
            (512, 510, 512, 510),
            (300, 600, 256, 512),
        ] {
            let src = PremulImage::new(w, h).unwrap();
            let got = resize_sticker(&src).unwrap();
            assert_eq!((got.width(), got.height()), (want_w, want_h));
        }
    }

    #[test]
    fn sticker_document_formats() {
        let webm = sticker_document("quote", &[0x1a, 0x45, 0xdf, 0xa3, 0, 0], "📝", 7);
        assert_eq!(webm.name, "quote.webm");
        assert_eq!(webm.mime_type, "video/webm");
        assert_eq!(webm.reply_to, 7);
        assert_eq!(
            webm.attributes,
            vec![
                StickerAttribute::Sticker { alt: "📝".into() },
                StickerAttribute::ImageSize { w: 512, h: 512 },
                StickerAttribute::Video {
                    w: 512,
                    h: 512,
                    duration: 0.0
                },
            ]
        );

        let webp = sticker_document("sticker.png", b"not a webp", "✨", 0);
        assert_eq!(webp.name, "sticker.webp");
        assert_eq!(webp.mime_type, "image/webp");
        assert_eq!(
            webp.attributes,
            vec![
                StickerAttribute::Sticker { alt: "✨".into() },
                StickerAttribute::ImageSize { w: 512, h: 512 },
            ]
        );
    }

    #[test]
    fn webm_info_parses_ebml_headers() {
        let id_header = [0x1a, 0x45, 0xdf, 0xa3];
        let id_segment = [0x18, 0x53, 0x80, 0x67];
        let id_seek_head = [0x11, 0x4d, 0x9b, 0x74];
        let id_info = [0x15, 0x49, 0xa9, 0x66];
        let id_timescale = [0x2a, 0xd7, 0xb1];
        let id_duration = [0x44, 0x89];
        let id_tracks = [0x16, 0x54, 0xae, 0x6b];
        let id_track = [0xae];
        let id_video = [0xe0];
        let id_width = [0xb0];
        let id_height = [0xba];
        let id_cluster = [0x1f, 0x43, 0xb6, 0x75];

        let doc_type = element(&[0x42, 0x82], &[b"webm"]);
        let header = element(&id_header, &[&doc_type]);
        let vw = element(&id_width, &[&[0x02, 0x00]]);
        let vh = element(&id_height, &[&[0x01, 0x80]]);
        let vid = element(&id_video, &[&vw, &vh]);
        let trk_num = element(&[0xd7], &[&[1]]);
        let trk = element(&id_track, &[&trk_num, &vid]);
        let video = element(&id_tracks, &[&trk]);
        let cluster = element(&id_cluster, &[&[0xe7, 0x81, 0x00]]);

        let seek = element(&id_seek_head, &[&[0xec, 0x80]]);
        let ts1 = element(&id_timescale, &[&[0x0f, 0x42, 0x40]]);
        let dur1 = element(&id_duration, &[&2500.0_f64.to_be_bytes()]);
        let info1 = element(&id_info, &[&ts1, &dur1]);
        let seg1 = element(&id_segment, &[&seek, &info1, &video, &cluster]);
        let full: Vec<u8> = [header.as_slice(), seg1.as_slice()].concat();
        let (w, h, d) = webm_info(&full).unwrap();
        assert_eq!((w, h), (512, 384));
        assert!((d - 2.5).abs() < 1e-6);

        let dur2 = element(&id_duration, &[&1200.0_f32.to_be_bytes()]);
        let info2 = element(&id_info, &[&dur2]);
        let seg2 = unknown_size(&id_segment, &[&info2, &video, &cluster]);
        let unk: Vec<u8> = [header.as_slice(), seg2.as_slice()].concat();
        let (w, h, d) = webm_info(&unk).unwrap();
        assert_eq!((w, h), (512, 384));
        assert!((d - 1.2).abs() < 1e-6);

        let ts3 = element(&id_timescale, &[&[0x98, 0x96, 0x80]]);
        let dur3 = element(&id_duration, &[&30.0_f64.to_be_bytes()]);
        let info3 = element(&id_info, &[&ts3, &dur3]);
        let seg3 = element(&id_segment, &[&info3, &video]);
        let custom: Vec<u8> = [header.as_slice(), seg3.as_slice()].concat();
        let (w, h, d) = webm_info(&custom).unwrap();
        assert_eq!((w, h), (512, 384));
        assert!((d - 0.3).abs() < 1e-6);

        let seg4 = element(&id_segment, &[&video]);
        let nodur: Vec<u8> = [header.as_slice(), seg4.as_slice()].concat();
        assert_eq!(webm_info(&nodur).unwrap(), (512, 384, 0.0));

        assert!(webm_info(b"RIFF....WEBPVP8 ").is_err());
        assert!(webm_info(&header).is_err());
        let bad_order_seg = element(&id_segment, &[&cluster, &video]);
        assert!(webm_info(&[header.as_slice(), bad_order_seg.as_slice()].concat()).is_err());
        let trunc = [header.as_slice(), seg4.as_slice()].concat();
        assert!(webm_info(&trunc[..header.len() + 20]).is_err());
    }
}
