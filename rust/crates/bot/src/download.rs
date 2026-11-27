use crate::{BotError, Client};
use futures_util::stream::{FuturesUnordered, StreamExt};
use grammers_mtsender::InvocationError;
use grammers_tl_types as tl;
use std::io::{self, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

pub const DC_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub const PART_SIZE: i32 = 512 * 1024;
pub const BIG_FILE_THRESHOLD: usize = 10 * 1024 * 1024;
pub const UPLOAD_WORKERS: usize = 4;
pub const DOWNLOAD_WORKERS: usize = 4;

#[derive(Debug, Clone)]
pub struct MediaFile {
    pub data: Vec<u8>,
    pub mime_type: String,
    pub sticker: bool,
    pub file_name: String,
}

#[derive(Debug, Clone)]
pub struct MediaSource {
    pub location: tl::enums::InputFileLocation,
    pub dc_id: i32,
    pub size: i64,
    pub photo: bool,
    pub mime_type: String,
    pub file_name: String,
    pub attributes: Vec<tl::enums::DocumentAttribute>,
}

pub fn largest_photo_size(sizes: &[tl::enums::PhotoSize]) -> Option<String> {
    let mut best: Option<String> = None;
    let mut best_area = 0i64;
    for entry in sizes {
        match entry {
            tl::enums::PhotoSize::Size(s) => {
                let area = i64::from(s.w) * i64::from(s.h);
                if area > best_area {
                    best_area = area;
                    best = Some(s.r#type.clone());
                }
            }
            tl::enums::PhotoSize::Progressive(s) => {
                let area = i64::from(s.w) * i64::from(s.h);
                if area > best_area {
                    best_area = area;
                    best = Some(s.r#type.clone());
                }
            }
            _ => {}
        }
    }
    best
}

pub fn document_of(message: &tl::types::Message) -> Option<tl::enums::InputDocument> {
    let Some(tl::enums::MessageMedia::Document(media)) = &message.media else {
        return None;
    };
    let Some(tl::enums::Document::Document(doc)) = &media.document else {
        return None;
    };
    Some(
        tl::types::InputDocument {
            id: doc.id,
            access_hash: doc.access_hash,
            file_reference: doc.file_reference.clone(),
        }
        .into(),
    )
}

pub fn source_of(message: &tl::types::Message) -> Option<MediaSource> {
    match message.media.as_ref()? {
        tl::enums::MessageMedia::Photo(media) => {
            let Some(tl::enums::Photo::Photo(photo)) = &media.photo else {
                return None;
            };
            let thumb_size = largest_photo_size(&photo.sizes)?;
            Some(MediaSource {
                location: tl::types::InputPhotoFileLocation {
                    id: photo.id,
                    access_hash: photo.access_hash,
                    file_reference: photo.file_reference.clone(),
                    thumb_size,
                }
                .into(),
                dc_id: photo.dc_id,
                size: 0,
                photo: true,
                mime_type: "image/jpeg".into(),
                file_name: String::new(),
                attributes: Vec::new(),
            })
        }
        tl::enums::MessageMedia::Document(media) => {
            let Some(tl::enums::Document::Document(doc)) = &media.document else {
                return None;
            };
            let mut file_name = String::new();
            for attr in &doc.attributes {
                if let tl::enums::DocumentAttribute::Filename(f) = attr {
                    file_name = f.file_name.clone();
                }
            }
            Some(MediaSource {
                location: tl::types::InputDocumentFileLocation {
                    id: doc.id,
                    access_hash: doc.access_hash,
                    file_reference: doc.file_reference.clone(),
                    thumb_size: String::new(),
                }
                .into(),
                dc_id: doc.dc_id,
                size: doc.size,
                photo: false,
                mime_type: doc.mime_type.clone(),
                file_name,
                attributes: doc.attributes.clone(),
            })
        }
        _ => None,
    }
}

impl Client {
    /// Downloads a Telegram file location into memory up to `limit` bytes.
    /// Starts on home DC and migrates on `303 FILE_MIGRATE_X` with a 20s initial connect timeout.
    pub async fn download_file(
        &self,
        location: tl::enums::InputFileLocation,
        dc_id: i32,
        limit: usize,
    ) -> Result<Vec<u8>, BotError> {
        match self.stream_download(&location, None, limit).await {
            Ok(bytes) => Ok(bytes),
            Err(err) => {
                let target = migrate_dc_from_err(&err).map(|d| if d == 0 { dc_id } else { d });
                let Some(target_dc) = target.filter(|&d| d != 0) else {
                    return Err(err);
                };
                self.prepare_media_dc(target_dc).await?;
                self.stream_download(&location, Some(target_dc), limit).await
            }
        }
    }

    async fn prepare_media_dc(&self, dc_id: i32) -> Result<(), BotError> {
        tokio::time::timeout(DC_CONNECT_TIMEOUT, self.copy_auth_to_dc(dc_id))
            .await
            .map_err(|_| BotError::Message(format!("connect to DC {dc_id}: timeout")))?
    }

    async fn stream_download(
        &self,
        location: &tl::enums::InputFileLocation,
        mut dc_id: Option<i32>,
        limit: usize,
    ) -> Result<Vec<u8>, BotError> {
        let mut out = Vec::new();
        let mut offset = 0i64;
        loop {
            let req = tl::functions::upload::GetFile {
                precise: false,
                cdn_supported: false,
                location: location.clone(),
                offset,
                limit: PART_SIZE,
            };
            let res = match self.invoke_optional_dc(dc_id, &req).await {
                Ok(r) => r,
                Err(BotError::Invocation(InvocationError::Rpc(rpc)))
                    if rpc.name == "AUTH_KEY_UNREGISTERED" =>
                {
                    if let Some(dc) = dc_id {
                        self.prepare_media_dc(dc).await?;
                        continue;
                    }
                    return Err(BotError::Invocation(InvocationError::Rpc(rpc)));
                }
                Err(e) => return Err(e),
            };
            match res {
                tl::enums::upload::File::File(f) => {
                    if out.len().saturating_add(f.bytes.len()) > limit {
                        return Err(BotError::TooLarge(format!("exceeds {limit} bytes")));
                    }
                    let chunk_len = f.bytes.len();
                    out.extend_from_slice(&f.bytes);
                    if chunk_len < PART_SIZE as usize {
                        break;
                    }
                    offset += chunk_len as i64;
                }
                tl::enums::upload::File::CdnRedirect(_) => {
                    return Err(BotError::Message("unexpected CDN redirect during download".into()));
                }
            }
            if dc_id.is_none() {
                dc_id = None;
            }
        }
        if out.is_empty() {
            return Err(BotError::EmptyDownload);
        }
        Ok(out)
    }

    pub async fn download_profile_photo(
        &self,
        peer: &tl::enums::InputPeer,
        big: bool,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, BotError> {
        let Some((photo_id, dc_id)) = self.photo_of(peer) else {
            return Ok(None);
        };
        let location = tl::types::InputPeerPhotoFileLocation {
            big,
            peer: peer.clone(),
            photo_id,
        }
        .into();
        self.download_file(location, dc_id, limit).await.map(Some)
    }

    fn photo_of(&self, peer: &tl::enums::InputPeer) -> Option<(i64, i32)> {
        match peer {
            tl::enums::InputPeer::PeerSelf => {
                let user = self.self_user()?;
                if let Some(tl::enums::UserProfilePhoto::Photo(p)) = user.photo {
                    Some((p.photo_id, p.dc_id))
                } else {
                    None
                }
            }
            tl::enums::InputPeer::User(u) => self
                .peers
                .user(u.user_id)
                .filter(|info| info.photo_id != 0)
                .map(|info| (info.photo_id, info.photo_dc)),
            tl::enums::InputPeer::Channel(ch) => self
                .peers
                .channel(ch.channel_id)
                .filter(|info| info.photo_id != 0)
                .map(|info| (info.photo_id, info.photo_dc)),
            tl::enums::InputPeer::Chat(c) => self
                .peers
                .chat(c.chat_id)
                .filter(|info| info.photo_id != 0)
                .map(|info| (info.photo_id, info.photo_dc)),
            _ => None,
        }
    }

    pub async fn download_media(
        &self,
        message: &tl::types::Message,
        limit: usize,
    ) -> Result<MediaFile, BotError> {
        let Some(media) = &message.media else {
            return Err(BotError::NoMedia);
        };
        match media {
            tl::enums::MessageMedia::Photo(p) => {
                let Some(tl::enums::Photo::Photo(photo)) = &p.photo else {
                    return Err(BotError::NoMedia);
                };
                let thumb_size = largest_photo_size(&photo.sizes).ok_or(BotError::NoMedia)?;
                let location = tl::types::InputPhotoFileLocation {
                    id: photo.id,
                    access_hash: photo.access_hash,
                    file_reference: photo.file_reference.clone(),
                    thumb_size,
                }
                .into();
                let data = self.download_file(location, photo.dc_id, limit).await?;
                Ok(MediaFile {
                    data,
                    mime_type: "image/jpeg".into(),
                    sticker: false,
                    file_name: String::new(),
                })
            }
            tl::enums::MessageMedia::Document(d) => {
                let Some(tl::enums::Document::Document(doc)) = &d.document else {
                    return Err(BotError::NoMedia);
                };
                if doc.size < 0 || doc.size as usize > limit {
                    return Err(BotError::TooLarge(format!("{} bytes", doc.size)));
                }
                let location = tl::types::InputDocumentFileLocation {
                    id: doc.id,
                    access_hash: doc.access_hash,
                    file_reference: doc.file_reference.clone(),
                    thumb_size: String::new(),
                }
                .into();
                let data = self.download_file(location, doc.dc_id, limit).await?;
                let mut file = MediaFile {
                    data,
                    mime_type: doc.mime_type.clone(),
                    sticker: false,
                    file_name: String::new(),
                };
                for attr in &doc.attributes {
                    match attr {
                        tl::enums::DocumentAttribute::Sticker(_) => file.sticker = true,
                        tl::enums::DocumentAttribute::Filename(f) => {
                            file.file_name = f.file_name.clone();
                        }
                        _ => {}
                    }
                }
                Ok(file)
            }
            _ => Err(BotError::NoMedia),
        }
    }

    /// Streams `source` directly to `file` with up to 4 concurrent chunk workers,
    /// migrating DCs and truncating on `FILE_MIGRATE_X`.
    pub async fn download_to(
        &self,
        source: &MediaSource,
        file: &mut tokio::fs::File,
    ) -> Result<(), BotError> {
        match self.parallel_download(&source.location, source.size, None, file).await {
            Ok(()) => Ok(()),
            Err(err) => {
                let target = migrate_dc_from_err(&err).map(|d| if d == 0 { source.dc_id } else { d });
                let Some(target_dc) = target.filter(|&d| d != 0) else {
                    return Err(err);
                };
                self.prepare_media_dc(target_dc).await?;
                file.set_len(0).await?;
                file.seek(SeekFrom::Start(0)).await?;
                self.parallel_download(&source.location, source.size, Some(target_dc), file)
                    .await
            }
        }
    }

    async fn parallel_download(
        &self,
        location: &tl::enums::InputFileLocation,
        size: i64,
        dc_id: Option<i32>,
        file: &mut tokio::fs::File,
    ) -> Result<(), BotError> {
        if size <= 0 {
            let bytes = self.stream_download(location, dc_id, usize::MAX).await?;
            file.write_all(&bytes).await?;
            file.flush().await?;
            return Ok(());
        }
        let part_index = Arc::new(AtomicI64::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, Vec<u8>)>(DOWNLOAD_WORKERS * 2);
        let mut tasks = FuturesUnordered::new();

        for _ in 0..DOWNLOAD_WORKERS {
            let client = self.clone();
            let loc = location.clone();
            let idx_ref = Arc::clone(&part_index);
            let tx = tx.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let i = idx_ref.fetch_add(1, Ordering::Relaxed);
                    let offset = i64::from(PART_SIZE) * i;
                    if offset >= size {
                        break;
                    }
                    let req = tl::functions::upload::GetFile {
                        precise: true,
                        cdn_supported: false,
                        location: loc.clone(),
                        offset,
                        limit: PART_SIZE,
                    };
                    let res = match client.invoke_optional_dc(dc_id, &req).await {
                        Ok(r) => r,
                        Err(BotError::Invocation(InvocationError::Rpc(rpc)))
                            if rpc.name == "AUTH_KEY_UNREGISTERED" =>
                        {
                            if let Some(dc) = dc_id {
                                client.prepare_media_dc(dc).await?;
                                client.invoke_optional_dc(Some(dc), &req).await?
                            } else {
                                return Err(BotError::Invocation(InvocationError::Rpc(rpc)));
                            }
                        }
                        Err(e) => return Err(e),
                    };
                    match res {
                        tl::enums::upload::File::File(f) => {
                            if tx.send((offset as u64, f.bytes)).await.is_err() {
                                break;
                            }
                        }
                        tl::enums::upload::File::CdnRedirect(_) => {
                            return Err(BotError::Message(
                                "unexpected CDN redirect during download".into(),
                            ));
                        }
                    }
                }
                Ok::<(), BotError>(())
            }));
        }
        drop(tx);

        let mut pos = 0u64;
        while let Some((offset, bytes)) = rx.recv().await {
            if offset != pos {
                file.seek(SeekFrom::Start(offset)).await?;
            }
            file.write_all(&bytes).await?;
            pos = offset + bytes.len() as u64;
        }

        while let Some(res) = tasks.next().await {
            let inner = res.map_err(|e| BotError::Io(io::Error::other(e)))?;
            inner?;
        }
        file.flush().await?;
        Ok(())
    }

    /// Uploads raw bytes through `Client::invoke` (`Retrier` protected).
    pub async fn upload_bytes(
        &self,
        name: &str,
        data: &[u8],
    ) -> Result<tl::enums::InputFile, BotError> {
        let file_id = crate::random_id();
        let file_name = if name.is_empty() {
            "a".to_owned()
        } else {
            name.to_owned()
        };
        let chunk_size = PART_SIZE as usize;
        let total_parts = data.len().div_ceil(chunk_size).max(1) as i32;

        if data.len() > BIG_FILE_THRESHOLD {
            let next_part = Arc::new(AtomicI64::new(0));
            let shared = Arc::new(data.to_vec());
            let mut tasks = FuturesUnordered::new();
            for _ in 0..UPLOAD_WORKERS {
                let client = self.clone();
                let next_part = Arc::clone(&next_part);
                let shared = Arc::clone(&shared);
                tasks.push(tokio::spawn(async move {
                    loop {
                        let part = next_part.fetch_add(1, Ordering::Relaxed) as i32;
                        if part >= total_parts {
                            break;
                        }
                        let start = part as usize * chunk_size;
                        let end = (start + chunk_size).min(shared.len());
                        let ok = client
                            .invoke(&tl::functions::upload::SaveBigFilePart {
                                file_id,
                                file_part: part,
                                file_total_parts: total_parts,
                                bytes: shared[start..end].to_vec(),
                            })
                            .await?;
                        if !ok {
                            return Err(BotError::Message(
                                "server failed to store uploaded data".into(),
                            ));
                        }
                    }
                    Ok::<(), BotError>(())
                }));
            }
            while let Some(res) = tasks.next().await {
                res.map_err(|e| BotError::Io(io::Error::other(e)))??;
            }
            Ok(tl::types::InputFileBig {
                id: file_id,
                parts: total_parts,
                name: file_name,
            }
            .into())
        } else {
            let mut md5 = md5::Context::new();
            for (part, chunk) in data.chunks(chunk_size).enumerate() {
                md5.consume(chunk);
                let ok = self
                    .invoke(&tl::functions::upload::SaveFilePart {
                        file_id,
                        file_part: part as i32,
                        bytes: chunk.to_vec(),
                    })
                    .await?;
                if !ok {
                    return Err(BotError::Message(
                        "server failed to store uploaded data".into(),
                    ));
                }
            }
            Ok(tl::types::InputFile {
                id: file_id,
                parts: total_parts,
                name: file_name,
                md5_checksum: format!("{:x}", md5.finalize()),
            }
            .into())
        }
    }

    /// Streams a file from disk through `Client::invoke` (`Retrier` protected) without loading
    /// multi-gigabyte media into memory.
    pub async fn upload_path(
        &self,
        name: &str,
        path: &std::path::Path,
    ) -> Result<tl::enums::InputFile, BotError> {
        let len = tokio::fs::metadata(path).await?.len();
        if len <= BIG_FILE_THRESHOLD as u64 {
            let data = tokio::fs::read(path).await?;
            return self.upload_bytes(name, &data).await;
        }
        let file_id = crate::random_id();
        let file_name = if name.is_empty() {
            "a".to_owned()
        } else {
            name.to_owned()
        };
        let chunk_size = PART_SIZE as u64;
        let total_parts = len.div_ceil(chunk_size).max(1) as i32;
        let next_part = Arc::new(AtomicI64::new(0));
        let path_owned = path.to_path_buf();
        let mut tasks = FuturesUnordered::new();
        for _ in 0..UPLOAD_WORKERS {
            let client = self.clone();
            let next_part = Arc::clone(&next_part);
            let worker_path = path_owned.clone();
            tasks.push(tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut file = tokio::fs::File::open(&worker_path).await?;
                let mut buf = vec![0u8; chunk_size as usize];
                loop {
                    let part = next_part.fetch_add(1, Ordering::Relaxed) as i32;
                    if part >= total_parts {
                        break;
                    }
                    let offset = u64::from(part as u32) * chunk_size;
                    let remaining = len.saturating_sub(offset) as usize;
                    let want = remaining.min(chunk_size as usize);
                    file.seek(SeekFrom::Start(offset)).await?;
                    let slice = &mut buf[..want];
                    file.read_exact(slice).await?;
                    let ok = client
                        .invoke(&tl::functions::upload::SaveBigFilePart {
                            file_id,
                            file_part: part,
                            file_total_parts: total_parts,
                            bytes: slice.to_vec(),
                        })
                        .await?;
                    if !ok {
                        return Err(BotError::Message(
                            "server failed to store uploaded data".into(),
                        ));
                    }
                }
                Ok::<(), BotError>(())
            }));
        }
        while let Some(res) = tasks.next().await {
            res.map_err(|e| BotError::Io(io::Error::other(e)))??;
        }
        Ok(tl::types::InputFileBig {
            id: file_id,
            parts: total_parts,
            name: file_name,
        }
        .into())
    }
}

fn migrate_dc_from_err(err: &BotError) -> Option<i32> {
    let rpc = err.rpc_error()?;
    if rpc.code == 303 && rpc.name == "FILE_MIGRATE" {
        Some(rpc.value.unwrap_or(0) as i32)
    } else {
        None
    }
}
