use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use grammers_crypto::AuthKey;
use grammers_session::types::{ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState};
use grammers_session::{BoxFuture, Session, SessionData};
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Fixed byte length of a Telegram MTProto authorization key.
pub const AUTH_KEY_LENGTH: usize = 256;

/// Teleproto StringSession format version byte.
pub const VERSION: u8 = b'1';

const MAX_ADDRESS_LENGTH: usize = 100;
const GOTD_SESSION_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("{0}")]
    Message(String),
    #[error("session lock is poisoned")]
    Poisoned,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Decoded GramJS / Teleproto / Telethon StringSession.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringSession {
    pub dc_id: i32,
    pub address: String,
    pub port: u16,
    pub auth_key: [u8; AUTH_KEY_LENGTH],
}

impl StringSession {
    /// Decodes a version `'1'` GramJS / Teleproto / Telethon StringSession.
    pub fn parse(text: &str) -> Result<Self, SessionError> {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return Err(SessionError::Message("session string is empty".into()));
        }
        if bytes[0] != VERSION {
            return Err(SessionError::Message(format!(
                "unsupported session version {:?}, expected {:?}",
                bytes[0] as char, VERSION as char
            )));
        }
        let payload = &text[1..];
        let data = decode_base64(payload)
            .map_err(|e| SessionError::Message(format!("decode session: {e}")))?;
        if data.is_empty() {
            return Err(SessionError::Message("session payload is empty".into()));
        }

        let dc_id = i32::from(data[0]);
        let mut offset = 1usize;
        let address = if payload.len() == 352 {
            if data.len() < offset + 4 {
                return Err(SessionError::Message(
                    "session payload is truncated before its address".into(),
                ));
            }
            let ip = Ipv4Addr::new(data[offset], data[offset + 1], data[offset + 2], data[offset + 3]);
            offset += 4;
            ip.to_string()
        } else {
            if data.len() < offset + 2 {
                return Err(SessionError::Message(
                    "session payload is truncated before its address length".into(),
                ));
            }
            let addr_len = i16::from_be_bytes([data[offset], data[offset + 1]]);
            if usize::try_from(addr_len).is_ok_and(|n| n > MAX_ADDRESS_LENGTH) {
                if data.len() < offset + 16 {
                    return Err(SessionError::Message(
                        "session payload is truncated before its IPv6 address".into(),
                    ));
                }
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&data[offset..offset + 16]);
                offset += 16;
                Ipv6Addr::from(octets).to_string()
            } else {
                if addr_len < 0 {
                    return Err(SessionError::Message(format!(
                        "session address length is negative: {addr_len}"
                    )));
                }
                let addr_len = addr_len as usize;
                offset += 2;
                if data.len() < offset + addr_len {
                    return Err(SessionError::Message(
                        "session payload is truncated inside its address".into(),
                    ));
                }
                let addr = std::str::from_utf8(&data[offset..offset + addr_len])
                    .map_err(|_| SessionError::Message("session address is not valid UTF-8".into()))?
                    .to_owned();
                offset += addr_len;
                addr
            }
        };

        if data.len() < offset + 2 {
            return Err(SessionError::Message(
                "session payload is truncated before its port".into(),
            ));
        }
        let port = u16::from_be_bytes([data[offset], data[offset + 1]]);
        offset += 2;

        let key_slice = &data[offset..];
        if key_slice.len() != AUTH_KEY_LENGTH {
            return Err(SessionError::Message(format!(
                "session auth key is {} bytes, expected {AUTH_KEY_LENGTH}",
                key_slice.len()
            )));
        }
        let mut auth_key = [0u8; AUTH_KEY_LENGTH];
        auth_key.copy_from_slice(key_slice);

        if address.is_empty() {
            return Err(SessionError::Message("session has no server address".into()));
        }
        if port == 0 {
            return Err(SessionError::Message(format!(
                "session port is out of range: {port}"
            )));
        }

        Ok(Self {
            dc_id,
            address,
            port,
            auth_key,
        })
    }

    /// Encodes the session in GramJS / Teleproto format (`'1'` + base64).
    pub fn encode(&self) -> Result<String, SessionError> {
        if !(1..=255).contains(&self.dc_id) {
            return Err(SessionError::Message(format!(
                "session DC is out of range: {}",
                self.dc_id
            )));
        }
        if self.address.is_empty() {
            return Err(SessionError::Message("session has no server address".into()));
        }
        if self.address.len() > MAX_ADDRESS_LENGTH {
            return Err(SessionError::Message(format!(
                "session address is {} bytes, more than the reader's {MAX_ADDRESS_LENGTH}",
                self.address.len()
            )));
        }
        if self.port == 0 {
            return Err(SessionError::Message(format!(
                "session port is out of range: {}",
                self.port
            )));
        }

        let mut payload = Vec::with_capacity(1 + 2 + self.address.len() + 2 + AUTH_KEY_LENGTH);
        payload.push(self.dc_id as u8);
        payload.extend_from_slice(&(self.address.len() as u16).to_be_bytes());
        payload.extend_from_slice(self.address.as_bytes());
        payload.extend_from_slice(&self.port.to_be_bytes());
        payload.extend_from_slice(&self.auth_key);

        let mut text = String::with_capacity(1 + payload.len() * 2);
        text.push(VERSION as char);
        text.push_str(&STANDARD.encode(&payload));

        let reparsed = Self::parse(&text).map_err(|e| {
            SessionError::Message(format!("encoded session does not read back: {e}"))
        })?;
        if reparsed != *self {
            return Err(SessionError::Message(
                "encoded session reads back as a different session".into(),
            ));
        }
        Ok(text)
    }

    /// Converts this StringSession into a `grammers_session::types::DcOption`,
    /// preserving fallback v4/v6 addresses from `defaults` if available.
    pub fn to_dc_option(&self, defaults: Option<&DcOption>) -> Result<DcOption, SessionError> {
        let addr_ip: IpAddr = self.address.parse().map_err(|_| {
            SessionError::Message(format!("session address {:?} is not a valid IP", self.address))
        })?;
        let (ipv4, ipv6) = match addr_ip {
            IpAddr::V4(v4) => (
                SocketAddrV4::new(v4, self.port),
                defaults.map(|d| d.ipv6).unwrap_or_else(|| {
                    SocketAddrV6::new(v4.to_ipv6_mapped(), self.port, 0, 0)
                }),
            ),
            IpAddr::V6(v6) => (
                defaults
                    .map(|d| d.ipv4)
                    .unwrap_or_else(|| SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
                SocketAddrV6::new(v6, self.port, 0, 0),
            ),
        };
        Ok(DcOption {
            id: self.dc_id,
            ipv4,
            ipv6,
            auth_key: Some(self.auth_key),
        })
    }

    /// Converts this StringSession into `SessionData` seeded with known DC options.
    pub fn to_session_data(&self) -> Result<SessionData, SessionError> {
        if !(1..=255).contains(&self.dc_id) {
            return Err(SessionError::Message(format!(
                "session DC is out of range: {}",
                self.dc_id
            )));
        }
        let mut data = SessionData::default();
        let dc_option = self.to_dc_option(data.dc_options.get(&self.dc_id))?;
        data.home_dc = self.dc_id;
        data.dc_options.insert(self.dc_id, dc_option);
        Ok(data)
    }

    /// Exports a `DcOption` (preferring IPv4 over IPv6) back to a `StringSession`.
    pub fn from_dc_option(option: &DcOption) -> Result<Self, SessionError> {
        let Some(auth_key) = option.auth_key else {
            return Err(SessionError::Message(format!(
                "no auth key is stored for data centre {}",
                option.id
            )));
        };
        let (address, port) = if !option.ipv4.ip().is_unspecified() && option.ipv4.port() > 0 {
            (option.ipv4.ip().to_string(), option.ipv4.port())
        } else if !option.ipv6.ip().is_unspecified() && option.ipv6.port() > 0 {
            (option.ipv6.ip().to_string(), option.ipv6.port())
        } else {
            return Err(SessionError::Message(format!(
                "no address is known for data centre {}",
                option.id
            )));
        };
        Ok(Self {
            dc_id: option.id,
            address,
            port,
            auth_key,
        })
    }

    /// Exports the home DC from `SessionData` back to a `StringSession`.
    pub fn from_session_data(data: &SessionData) -> Result<Self, SessionError> {
        let opt = data.dc_options.get(&data.home_dc).ok_or_else(|| {
            SessionError::Message(format!("no data centre option found for DC {}", data.home_dc))
        })?;
        // If stored DC option lacks a valid address, fall back to static production DCs.
        if (opt.ipv4.ip().is_unspecified() || opt.ipv4.port() == 0)
            && (opt.ipv6.ip().is_unspecified() || opt.ipv6.port() == 0)
            && let Some(fallback) = SessionData::default().dc_options.get(&data.home_dc)
        {
            let mut merged = fallback.clone();
            merged.auth_key = opt.auth_key;
            return Self::from_dc_option(&merged);
        }
        Self::from_dc_option(opt)
    }
}

fn decode_base64(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let mut last_err = None;
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        match engine.decode(text) {
            Ok(bytes) => return Ok(bytes),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.expect("at least one base64 engine is tried"))
}

/// Picks a standard client endpoint (`!media_only && !cdn && !tcpo_only`) for `dc_id`,
/// preferring IPv4 over IPv6.
pub fn resolve_dc_address(options: &[tl::enums::DcOption], dc_id: i32) -> Option<String> {
    let mut fallback: Option<String> = None;
    for item in options {
        let tl::enums::DcOption::Option(opt) = item;
        if opt.id != dc_id || opt.media_only || opt.cdn || opt.tcpo_only {
            continue;
        }
        let addr = format_host_port(&opt.ip_address, opt.port as u16);
        if opt.ipv6 || opt.ip_address.contains(':') {
            if fallback.is_none() {
                fallback = Some(addr);
            }
            continue;
        }
        return Some(addr);
    }
    fallback
}

/// Formats `DcOption`'s preferred address (`host:port`, IPv4 preferred over IPv6).
pub fn resolve_dc_option_address(opt: &DcOption) -> Option<String> {
    if !opt.ipv4.ip().is_unspecified() && opt.ipv4.port() > 0 {
        Some(opt.ipv4.to_string())
    } else if !opt.ipv6.ip().is_unspecified() && opt.ipv6.port() > 0 {
        Some(opt.ipv6.to_string())
    } else {
        None
    }
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Hook allowing `MibotSession` to delegate update cursor and peer access-hash storage
/// (e.g. to `mibot-tgstate`) without circular crate dependencies.
pub trait StateBackend: Send + Sync + 'static {
    fn updates_state(&self) -> UpdatesState;
    fn set_update_state(&self, update: UpdateState);
    fn peer(&self, peer: PeerId) -> Option<PeerInfo>;
    fn cache_peer(&self, peer: PeerInfo);
    fn cache_peers(&self, peers: Vec<PeerInfo>) {
        for p in peers {
            self.cache_peer(p);
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GotdSessionFile {
    #[serde(rename = "Version", default)]
    version: u32,
    #[serde(rename = "Data")]
    data: GotdSessionData,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GotdSessionData {
    #[serde(rename = "DC", default)]
    dc: i32,
    #[serde(rename = "Addr", default)]
    addr: String,
    #[serde(rename = "AuthKey", default)]
    auth_key: String,
    #[serde(rename = "AuthKeyID", default)]
    auth_key_id: String,
    #[serde(rename = "Salt", default)]
    salt: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extra_dcs: Vec<DcOption>,
}

/// Persistence adapter for `gotd-session.json` compatible with Go's `gotd/td/session` layout.
#[derive(Debug, Clone)]
pub struct FileStorage {
    pub path: PathBuf,
}

impl FileStorage {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<SessionData>, SessionError> {
        let raw = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(SessionError::Io(err)),
        };
        let parsed: GotdSessionFile = serde_json::from_slice(&raw)?;
        if parsed.data.dc <= 0 || parsed.data.auth_key.is_empty() {
            return Err(SessionError::Message(
                "gotd-session.json is missing DC or AuthKey".into(),
            ));
        }
        let key_bytes = decode_base64(parsed.data.auth_key.trim())
            .map_err(|e| SessionError::Message(format!("invalid session AuthKey: {e}")))?;
        if key_bytes.len() != AUTH_KEY_LENGTH {
            return Err(SessionError::Message(format!(
                "session AuthKey is {} bytes, expected {AUTH_KEY_LENGTH}",
                key_bytes.len()
            )));
        }
        let mut auth_key = [0u8; AUTH_KEY_LENGTH];
        auth_key.copy_from_slice(&key_bytes);

        let mut session_data = SessionData {
            home_dc: parsed.data.dc,
            ..Default::default()
        };
        let mut home_opt = session_data
            .dc_options
            .get(&parsed.data.dc)
            .cloned()
            .unwrap_or_else(|| DcOption {
                id: parsed.data.dc,
                ipv4: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
                ipv6: SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0),
                auth_key: None,
            });
        home_opt.auth_key = Some(auth_key);

        if !parsed.data.addr.trim().is_empty()
            && let Ok(sock) = parsed.data.addr.trim().parse::<SocketAddr>()
        {
            match sock {
                SocketAddr::V4(v4) => {
                    home_opt.ipv4 = v4;
                    if home_opt.ipv6.ip().is_unspecified() {
                        home_opt.ipv6 =
                            SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0);
                    }
                }
                SocketAddr::V6(v6) => {
                    home_opt.ipv6 = v6;
                }
            }
        }
        session_data.dc_options.insert(parsed.data.dc, home_opt);

        for extra in parsed.data.extra_dcs {
            if extra.id > 0 {
                session_data.dc_options.insert(extra.id, extra);
            }
        }

        Ok(Some(session_data))
    }

    pub fn save(&self, data: &SessionData) -> Result<(), SessionError> {
        let home_opt = data.dc_options.get(&data.home_dc).ok_or_else(|| {
            SessionError::Message(format!("missing DcOption for home DC {}", data.home_dc))
        })?;
        let auth_key = home_opt.auth_key.ok_or_else(|| {
            SessionError::Message(format!("missing auth_key for home DC {}", data.home_dc))
        })?;
        let addr = resolve_dc_option_address(home_opt).unwrap_or_default();
        let key_id = AuthKey::from_bytes(auth_key).key_id();

        let mut extra_dcs: Vec<DcOption> = data
            .dc_options
            .values()
            .filter(|opt| opt.id != data.home_dc && opt.auth_key.is_some())
            .cloned()
            .collect();
        extra_dcs.sort_by_key(|opt| opt.id);

        let file = GotdSessionFile {
            version: GOTD_SESSION_VERSION,
            data: GotdSessionData {
                dc: data.home_dc,
                addr,
                auth_key: STANDARD.encode(auth_key),
                auth_key_id: STANDARD.encode(key_id),
                salt: 0,
                extra_dcs,
            },
        };
        let mut encoded = serde_json::to_vec(&file)?;
        encoded.push(b'\n');
        mibot_fsutil::write_file_atomic(&self.path, &encoded, 0o600)?;
        Ok(())
    }
}

struct SessionInner {
    data: SessionData,
    self_user_id: Option<i64>,
    storage: Option<FileStorage>,
    read_only: bool,
    backend: Option<Arc<dyn StateBackend>>,
}

/// Custom `grammers_session::Session` backed by `gotd-session.json`, `StringSession`,
/// and an optional `StateBackend` (`mibot-tgstate`) without any SQLite/C dependency.
pub struct MibotSession {
    inner: Mutex<SessionInner>,
}

impl Default for MibotSession {
    fn default() -> Self {
        Self::new_memory()
    }
}

impl MibotSession {
    pub fn new_memory() -> Self {
        Self::from_session_data(SessionData::default())
    }

    pub fn from_session_data(data: SessionData) -> Self {
        Self {
            inner: Mutex::new(SessionInner {
                data,
                self_user_id: None,
                storage: None,
                read_only: false,
                backend: None,
            }),
        }
    }

    /// Opens a session from `gotd-session.json` (if present) or imports `string_session`.
    /// When `read_only` is true (`--check`), existing files are inspected in memory without writing.
    pub fn open(
        path: Option<PathBuf>,
        string_session: &str,
        read_only: bool,
        backend: Option<Arc<dyn StateBackend>>,
    ) -> Result<Self, SessionError> {
        let parsed = StringSession::parse(string_session)?;
        let storage = path.map(FileStorage::new);
        let existing = match &storage {
            Some(store) => store.load()?,
            None => None,
        };
        let data = match existing {
            Some(loaded) => loaded,
            None => {
                let imported = parsed.to_session_data()?;
                if !read_only && let Some(store) = &storage {
                    store.save(&imported)?;
                }
                imported
            }
        };
        Ok(Self {
            inner: Mutex::new(SessionInner {
                data,
                self_user_id: None,
                storage,
                read_only,
                backend,
            }),
        })
    }

    /// Opens an empty or existing file-backed session for `--login`.
    pub fn for_login(read_only: bool) -> Self {
        Self {
            inner: Mutex::new(SessionInner {
                data: SessionData::default(),
                self_user_id: None,
                storage: None,
                read_only,
                backend: None,
            }),
        }
    }

    pub fn set_backend(&self, backend: Arc<dyn StateBackend>) -> Result<(), SessionError> {
        let mut guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
        guard.backend = Some(backend);
        Ok(())
    }

    pub fn snapshot_data(&self) -> Result<SessionData, SessionError> {
        let guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
        Ok(SessionData {
            home_dc: guard.data.home_dc,
            dc_options: guard.data.dc_options.clone(),
            peer_infos: guard.data.peer_infos.clone(),
            updates_state: guard.data.updates_state.clone(),
        })
    }

    pub fn export_string_session(&self) -> Result<StringSession, SessionError> {
        let guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
        StringSession::from_session_data(&guard.data)
    }

    /// Seeds the authenticated user's own `PeerInfo` (including `is_self = Some(true)`),
    /// which `UpdatesReceiver::create` checks via `PeerId::self_user()`.
    pub fn seed_self(&self, id: i64, access_hash: Option<i64>, is_bot: bool) -> Result<(), SessionError> {
        let mut guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
        guard.self_user_id = Some(id);
        let peer_info = PeerInfo::User {
            id,
            auth: access_hash.filter(|&h| h != 0).map(grammers_session::types::PeerAuth::from_hash),
            bot: Some(is_bot),
            is_self: Some(true),
        };
        let pid = peer_info.id();
        guard
            .data
            .peer_infos
            .entry(pid)
            .or_insert_with(|| peer_info.clone())
            .extend_info(&peer_info);
        if let Some(backend) = guard.backend.clone() {
            drop(guard);
            backend.cache_peer(peer_info);
        }
        Ok(())
    }

    fn persist_dc_if_needed(inner: &SessionInner) -> Result<(), SessionError> {
        if inner.read_only {
            return Ok(());
        }
        let Some(store) = &inner.storage else {
            return Ok(());
        };
        if inner
            .data
            .dc_options
            .get(&inner.data.home_dc)
            .and_then(|opt| opt.auth_key)
            .is_some()
        {
            store.save(&inner.data)?;
        }
        Ok(())
    }
}

fn cache_peer_in_map(map: &mut HashMap<PeerId, PeerInfo>, self_user_id: &mut Option<i64>, peer: &PeerInfo) {
    if let PeerInfo::User {
        id,
        is_self: Some(true),
        ..
    } = peer
    {
        *self_user_id = Some(*id);
    }
    map.entry(peer.id())
        .or_insert_with(|| peer.clone())
        .extend_info(peer);
}

impl Session for MibotSession {
    type Error = SessionError;

    fn home_dc_id(&self) -> Result<i32, Self::Error> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .data
            .home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
            if guard.data.home_dc != dc_id {
                guard.data.home_dc = dc_id;
                Self::persist_dc_if_needed(&guard)?;
            }
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Self::Error> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| SessionError::Poisoned)?
            .data
            .dc_options
            .get(&dc_id)
            .cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Self::Error>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            let mut guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
            let changed = guard.data.dc_options.get(&dc_option.id) != Some(&dc_option);
            if changed {
                let auth_or_home = dc_option.id == guard.data.home_dc || dc_option.auth_key.is_some();
                guard.data.dc_options.insert(dc_option.id, dc_option);
                if auth_or_home {
                    Self::persist_dc_if_needed(&guard)?;
                }
            }
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Self::Error>> {
        Box::pin(async move {
            let (direct, backend) = {
                let guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
                let lookup_id = if peer == PeerId::self_user() {
                    guard
                        .self_user_id
                        .and_then(PeerId::user)
                        .or_else(|| {
                            guard.data.peer_infos.values().find_map(|info| match info {
                                PeerInfo::User {
                                    is_self: Some(true), ..
                                } => Some(info.id()),
                                _ => None,
                            })
                        })
                } else {
                    Some(peer)
                };
                let direct = lookup_id.and_then(|id| guard.data.peer_infos.get(&id).cloned());
                (direct, guard.backend.clone())
            };
            if direct.is_some() {
                return Ok(direct);
            }
            Ok(backend.and_then(|b| b.peer(peer)))
        })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let backend = {
                let mut guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
                let mut self_id = guard.self_user_id;
                cache_peer_in_map(&mut guard.data.peer_infos, &mut self_id, &peer);
                guard.self_user_id = self_id;
                guard.backend.clone()
            };
            if let Some(b) = backend {
                b.cache_peer(peer);
            }
            Ok(())
        })
    }

    fn cache_peers(&self, peers: Vec<PeerInfo>) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let backend = {
                let mut guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
                let mut self_id = guard.self_user_id;
                for peer in &peers {
                    cache_peer_in_map(&mut guard.data.peer_infos, &mut self_id, peer);
                }
                guard.self_user_id = self_id;
                guard.backend.clone()
            };
            if let Some(b) = backend {
                b.cache_peers(peers);
            }
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Self::Error>> {
        Box::pin(async move {
            let (fallback, backend) = {
                let guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
                (guard.data.updates_state.clone(), guard.backend.clone())
            };
            Ok(match backend {
                Some(b) => b.updates_state(),
                None => fallback,
            })
        })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Self::Error>> {
        Box::pin(async move {
            let backend = {
                let mut guard = self.inner.lock().map_err(|_| SessionError::Poisoned)?;
                match &update {
                    UpdateState::All(state) => guard.data.updates_state = state.clone(),
                    UpdateState::Primary { pts, date, seq } => {
                        guard.data.updates_state.pts = *pts;
                        guard.data.updates_state.date = *date;
                        guard.data.updates_state.seq = *seq;
                    }
                    UpdateState::Secondary { qts } => {
                        guard.data.updates_state.qts = *qts;
                    }
                    UpdateState::Channel { id, pts } => {
                        guard.data.updates_state.channels.retain(|c| c.id != *id);
                        guard
                            .data
                            .updates_state
                            .channels
                            .push(ChannelState { id: *id, pts: *pts });
                    }
                }
                guard.backend.clone()
            };
            if let Some(b) = backend {
                b.set_update_state(update);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mibot-session-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> StringSession {
        let mut auth_key = [0u8; AUTH_KEY_LENGTH];
        for (i, b) in auth_key.iter_mut().enumerate() {
            *b = i as u8;
        }
        StringSession {
            dc_id: 2,
            address: "149.154.167.50".into(),
            port: 443,
            auth_key,
        }
    }

    #[test]
    fn round_trip_string_session() {
        let orig = sample();
        let encoded = orig.encode().unwrap();
        assert!(encoded.starts_with('1'));
        let parsed = StringSession::parse(&encoded).unwrap();
        assert_eq!(parsed, orig);
    }

    #[test]
    fn parse_gramjs_and_telethon_and_ipv6_layouts() {
        // GramJS layout
        let key = [7u8; AUTH_KEY_LENGTH];
        let address = b"91.108.56.130";
        let mut payload = vec![5u8];
        payload.extend_from_slice(&(address.len() as u16).to_be_bytes());
        payload.extend_from_slice(address);
        payload.extend_from_slice(&443u16.to_be_bytes());
        payload.extend_from_slice(&key);
        let parsed = StringSession::parse(&format!("1{}", STANDARD.encode(&payload))).unwrap();
        assert_eq!(parsed.dc_id, 5);
        assert_eq!(parsed.address, "91.108.56.130");
        assert_eq!(parsed.port, 443);
        assert_eq!(parsed.auth_key, key);

        // Telethon 352-char IPv4 layout: 1 + 4 + 2 + 256 = 263 bytes -> 352 base64 chars
        let mut telethon_payload = vec![2u8, 149, 154, 167, 50];
        telethon_payload.extend_from_slice(&443u16.to_be_bytes());
        telethon_payload.extend_from_slice(&key);
        let b64 = STANDARD.encode(&telethon_payload);
        assert_eq!(b64.len(), 352);
        let parsed_telethon = StringSession::parse(&format!("1{b64}")).unwrap();
        assert_eq!(parsed_telethon.address, "149.154.167.50");
        assert_eq!(parsed_telethon.port, 443);

        // Raw IPv6 (when first 2 bytes interpret as > 100)
        let v6: Ipv6Addr = "2001:67c:4e8:f002::a".parse().unwrap();
        let mut v6_payload = vec![2u8];
        v6_payload.extend_from_slice(&v6.octets());
        v6_payload.extend_from_slice(&443u16.to_be_bytes());
        v6_payload.extend_from_slice(&key);
        let parsed_v6 = StringSession::parse(&format!("1{}", STANDARD.encode(&v6_payload))).unwrap();
        assert_eq!(parsed_v6.address, "2001:67c:4e8:f002::a");
    }

    #[test]
    fn parse_rejects_invalid_inputs() {
        let short = format!("1{}", STANDARD.encode([2, 0, 1, b'a', 1, 187]));
        for bad in ["", "2abc", "1!!!!", short.as_str()] {
            assert!(StringSession::parse(bad).is_err(), "expected err for {bad:?}");
        }
    }

    #[test]
    fn session_data_round_trip_and_resolve_dc() {
        let orig = sample();
        let data = orig.to_session_data().unwrap();
        assert_eq!(data.home_dc, 2);
        let opt = data.dc_options.get(&2).unwrap();
        assert_eq!(opt.ipv4, "149.154.167.50:443".parse::<SocketAddrV4>().unwrap());
        assert_eq!(opt.auth_key, Some(orig.auth_key));

        let exported = StringSession::from_session_data(&data).unwrap();
        assert_eq!(exported, orig);

        let options = vec![
            tl::enums::DcOption::Option(tl::types::DcOption {
                ipv6: false,
                media_only: true,
                tcpo_only: false,
                cdn: false,
                r#static: true,
                this_port_only: false,
                id: 2,
                ip_address: "10.0.0.1".into(),
                port: 443,
                secret: None,
            }),
            tl::enums::DcOption::Option(tl::types::DcOption {
                ipv6: false,
                media_only: false,
                tcpo_only: false,
                cdn: true,
                r#static: true,
                this_port_only: false,
                id: 2,
                ip_address: "10.0.0.2".into(),
                port: 443,
                secret: None,
            }),
            tl::enums::DcOption::Option(tl::types::DcOption {
                ipv6: false,
                media_only: false,
                tcpo_only: true,
                cdn: false,
                r#static: true,
                this_port_only: false,
                id: 2,
                ip_address: "10.0.0.3".into(),
                port: 443,
                secret: None,
            }),
            tl::enums::DcOption::Option(tl::types::DcOption {
                ipv6: false,
                media_only: false,
                tcpo_only: false,
                cdn: false,
                r#static: true,
                this_port_only: false,
                id: 2,
                ip_address: "10.0.0.7".into(),
                port: 8443,
                secret: None,
            }),
        ];
        assert_eq!(resolve_dc_address(&options, 2).as_deref(), Some("10.0.0.7:8443"));
    }

    #[test]
    fn file_storage_and_read_only_mode() {
        let dir = temp_dir();
        let path = dir.join("gotd-session.json");
        let encoded = sample().encode().unwrap();

        // Read-only does not create gotd-session.json
        let ro = MibotSession::open(Some(path.clone()), &encoded, true, None).unwrap();
        assert!(!path.exists());
        assert_eq!(ro.home_dc_id().unwrap(), 2);

        // Read-write writes 0600 gotd-session.json
        let rw = MibotSession::open(Some(path.clone()), &encoded, false, None).unwrap();
        assert!(path.exists());
        let perm = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(perm, 0o600);

        // Reloading preserves auth key
        let loaded = FileStorage::new(&path).load().unwrap().unwrap();
        assert_eq!(StringSession::from_session_data(&loaded).unwrap(), sample());
        assert_eq!(rw.export_string_session().unwrap(), sample());

        let _ = fs::remove_dir_all(&dir);
    }
}
