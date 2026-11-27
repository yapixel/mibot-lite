use grammers_session::types::{
    ChannelState, PeerAuth, PeerId, PeerInfo, PeerKind, UpdateState, UpdatesState,
};
use mibot_session::StateBackend;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEBOUNCE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PERSIST_BATCH: usize = 32;

#[derive(Debug, Error)]
pub enum TgStateError {
    #[error("update state does not exist")]
    NoState,
    #[error("update state is closed")]
    Closed,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountState {
    pub pts: i32,
    pub qts: i32,
    pub date: i32,
    pub seq: i32,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Document {
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    generation: u64,
    #[serde(default)]
    user_id: i64,
    #[serde(default)]
    pts: i32,
    #[serde(default)]
    qts: i32,
    #[serde(default)]
    date: i32,
    #[serde(default)]
    seq: i32,
    #[serde(default)]
    has_state: bool,
    #[serde(default)]
    channels: BTreeMap<String, i32>,
    #[serde(default, rename = "channelHashes")]
    chans: BTreeMap<String, i64>,
    #[serde(default, rename = "userHashes")]
    users: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorDocument {
    generation: u64,
    user_id: i64,
    pts: i32,
    qts: i32,
    date: i32,
    seq: i32,
    has_state: bool,
    channels: BTreeMap<String, i32>,
}

type FlushErrorHandler = Arc<dyn Fn(&TgStateError) + Send + Sync>;

struct Inner {
    doc: Document,
    dirty: bool,
    hash_dirty: bool,
    checkpoint_dirty: bool,
    last_checkpoint: SystemTime,
    timer_armed: bool,
    closed: bool,
    on_flush_error: Option<FlushErrorHandler>,
}

struct StateCore {
    path: PathBuf,
    mu: Mutex<Inner>,
    cancel_timer: Notify,
}

/// Persisted MTProto updates cursor (`updates.json` + `.cursor`) and access-hash store.
#[derive(Clone)]
pub struct TgState {
    core: Arc<StateCore>,
}

impl TgState {
    /// Loads `updates.json` and matching `updates.json.cursor` if present, without writing to disk.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, TgStateError> {
        let path = path.into();
        let mut doc = Document::default();
        let mut last_checkpoint: Option<SystemTime> = None;

        match fs::read(&path) {
            Ok(raw) => {
                doc = serde_json::from_slice(&raw)?;
                if let Ok(meta) = fs::metadata(&path)
                    && let Ok(mtime) = meta.modified()
                {
                    last_checkpoint = Some(mtime);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(TgStateError::Io(e)),
        }

        let mut checkpoint_dirty = false;
        if doc.generation != 0 {
            let cursor_path = cursor_path_for(&path);
            match fs::read(&cursor_path) {
                Ok(raw) => {
                    let cursor: CursorDocument = serde_json::from_slice(&raw)?;
                    if cursor.generation == doc.generation {
                        doc.user_id = cursor.user_id;
                        doc.has_state = cursor.has_state;
                        doc.pts = cursor.pts;
                        doc.qts = cursor.qts;
                        doc.date = cursor.date;
                        doc.seq = cursor.seq;
                        doc.channels = cursor.channels;
                        checkpoint_dirty = true;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(TgStateError::Io(e)),
            }
        }

        let core = Arc::new(StateCore {
            path,
            mu: Mutex::new(Inner {
                doc,
                dirty: false,
                hash_dirty: false,
                checkpoint_dirty,
                last_checkpoint: last_checkpoint.unwrap_or_else(SystemTime::now),
                timer_armed: false,
                closed: false,
                on_flush_error: None,
            }),
            cancel_timer: Notify::new(),
        });
        Ok(Self { core })
    }

    pub fn path(&self) -> &Path {
        &self.core.path
    }

    pub fn dir(&self) -> PathBuf {
        self.core
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn set_on_flush_error<F>(&self, handler: F)
    where
        F: Fn(&TgStateError) + Send + Sync + 'static,
    {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        guard.on_flush_error = Some(Arc::new(handler));
    }

    /// Flushes any pending writes and exports a legacy-compatible full checkpoint before shutdown.
    pub fn close(&self) -> Result<(), TgStateError> {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        guard.closed = true;
        guard.timer_armed = false;
        self.core.cancel_timer.notify_waiters();
        checkpoint_locked(&self.core.path, &mut guard)
    }

    /// Immediately flushes dirty cursor or checkpoint changes to disk.
    pub fn flush(&self) -> Result<(), TgStateError> {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        flush_locked(&self.core.path, &mut guard)
    }

    /// Writes a full `updates.json` snapshot containing both cursor and hashes.
    pub fn checkpoint(&self) -> Result<(), TgStateError> {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        checkpoint_locked(&self.core.path, &mut guard)
    }

    /// Periodically exports full checkpoints compatible with older versions.
    pub async fn run_checkpoints(&self, cancel: CancellationToken) {
        let mut retry = false;
        loop {
            let delay = if retry {
                Duration::from_secs(1)
            } else {
                let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
                if guard.checkpoint_dirty {
                    let deadline = guard.last_checkpoint + CHECKPOINT_INTERVAL;
                    deadline
                        .duration_since(SystemTime::now())
                        .unwrap_or(Duration::ZERO)
                } else {
                    CHECKPOINT_INTERVAL
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            if cancel.is_cancelled() {
                return;
            }
            match self.checkpoint() {
                Ok(()) => retry = false,
                Err(err) => {
                    retry = true;
                    let handler = {
                        let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
                        guard.on_flush_error.clone()
                    };
                    if let Some(cb) = handler {
                        cb(&err);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn backdate_checkpoint_for_test(&self, age: Duration) {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        guard.last_checkpoint = SystemTime::now() - age;
    }

    #[cfg(test)]
    fn crash_for_test(&self) {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        guard.closed = true;
        guard.timer_armed = false;
        self.core.cancel_timer.notify_waiters();
    }

    fn mark_dirty_locked(&self, guard: &mut Inner) {
        guard.dirty = true;
        guard.checkpoint_dirty = true;
        if guard.timer_armed || guard.closed {
            return;
        }
        guard.timer_armed = true;
        let core = Arc::clone(&self.core);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(DEBOUNCE_INTERVAL) => {}
                    _ = core.cancel_timer.notified() => return,
                }
                Self::fire_debounce(core);
            });
        } else {
            let state = self.clone();
            std::thread::spawn(move || {
                let start = Instant::now();
                while start.elapsed() < DEBOUNCE_INTERVAL {
                    let closed = state.core.mu.lock().unwrap_or_else(|e| e.into_inner()).closed;
                    if closed {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Self::fire_debounce(state.core);
            });
        }
    }

    fn fire_debounce(core: Arc<StateCore>) {
        let (err_opt, handler) = {
            let mut guard = core.mu.lock().unwrap_or_else(|e| e.into_inner());
            guard.timer_armed = false;
            if guard.closed {
                return;
            }
            let res = flush_locked(&core.path, &mut guard);
            let err_opt = res.err();
            if err_opt.is_some() {
                // Re-arm retry timer without holding the lock across the callback.
                let state = TgState {
                    core: Arc::clone(&core),
                };
                state.mark_dirty_locked(&mut guard);
            }
            (err_opt, guard.on_flush_error.clone())
        };
        if let (Some(err), Some(cb)) = (err_opt, handler) {
            cb(&err);
        }
    }

    pub fn get_state(&self, user_id: i64) -> Result<Option<AccountState>, TgStateError> {
        let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.doc.has_state || guard.doc.user_id != user_id {
            return Ok(None);
        }
        Ok(Some(AccountState {
            pts: guard.doc.pts,
            qts: guard.doc.qts,
            date: guard.doc.date,
            seq: guard.doc.seq,
        }))
    }

    pub fn set_state(&self, user_id: i64, state: AccountState) -> Result<(), TgStateError> {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return Err(TgStateError::Closed);
        }
        guard.doc.user_id = user_id;
        guard.doc.has_state = true;
        guard.doc.pts = state.pts;
        guard.doc.qts = state.qts;
        guard.doc.date = state.date;
        guard.doc.seq = state.seq;
        guard.doc.channels.clear();
        self.mark_dirty_locked(&mut guard);
        Ok(())
    }

    fn set_field<F>(&self, user_id: i64, f: F) -> Result<(), TgStateError>
    where
        F: FnOnce(&mut Document),
    {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return Err(TgStateError::Closed);
        }
        if !guard.doc.has_state || guard.doc.user_id != user_id {
            return Err(TgStateError::NoState);
        }
        f(&mut guard.doc);
        self.mark_dirty_locked(&mut guard);
        Ok(())
    }

    pub fn set_pts(&self, user_id: i64, pts: i32) -> Result<(), TgStateError> {
        self.set_field(user_id, |doc| doc.pts = pts)
    }

    pub fn set_qts(&self, user_id: i64, qts: i32) -> Result<(), TgStateError> {
        self.set_field(user_id, |doc| doc.qts = qts)
    }

    pub fn set_date(&self, user_id: i64, date: i32) -> Result<(), TgStateError> {
        self.set_field(user_id, |doc| doc.date = date)
    }

    pub fn set_seq(&self, user_id: i64, seq: i32) -> Result<(), TgStateError> {
        self.set_field(user_id, |doc| doc.seq = seq)
    }

    pub fn set_date_seq(&self, user_id: i64, date: i32, seq: i32) -> Result<(), TgStateError> {
        self.set_field(user_id, |doc| {
            doc.date = date;
            doc.seq = seq;
        })
    }

    pub fn get_channel_pts(&self, user_id: i64, channel_id: i64) -> Option<i32> {
        let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.doc.user_id != user_id {
            return None;
        }
        guard.doc.channels.get(&channel_id.to_string()).copied()
    }

    pub fn set_channel_pts(&self, user_id: i64, channel_id: i64, pts: i32) -> Result<(), TgStateError> {
        self.set_field(user_id, |doc| {
            doc.channels.insert(channel_id.to_string(), pts);
        })
    }

    pub fn channel_pts_list(&self, user_id: i64) -> Vec<(i64, i32)> {
        let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.doc.user_id != user_id {
            return Vec::new();
        }
        let mut out: Vec<(i64, i32)> = guard
            .doc
            .channels
            .iter()
            .filter_map(|(k, &v)| k.parse::<i64>().ok().map(|id| (id, v)))
            .collect();
        out.sort_by_key(|&(id, _)| id);
        out
    }

    pub fn set_channel_access_hash(
        &self,
        channel_id: i64,
        access_hash: i64,
    ) -> Result<(), TgStateError> {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return Err(TgStateError::Closed);
        }
        let key = channel_id.to_string();
        if guard.doc.chans.get(&key).copied() != Some(access_hash) {
            guard.doc.chans.insert(key, access_hash);
            guard.hash_dirty = true;
            self.mark_dirty_locked(&mut guard);
        }
        Ok(())
    }

    pub fn get_channel_access_hash(&self, channel_id: i64) -> Option<i64> {
        let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        guard.doc.chans.get(&channel_id.to_string()).copied()
    }

    pub fn set_user_access_hash(
        &self,
        target_user_id: i64,
        access_hash: i64,
    ) -> Result<(), TgStateError> {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return Err(TgStateError::Closed);
        }
        let key = target_user_id.to_string();
        if guard.doc.users.get(&key).copied() != Some(access_hash) {
            guard.doc.users.insert(key, access_hash);
            guard.hash_dirty = true;
            self.mark_dirty_locked(&mut guard);
        }
        Ok(())
    }

    pub fn get_user_access_hash(&self, target_user_id: i64) -> Option<i64> {
        let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        guard.doc.users.get(&target_user_id.to_string()).copied()
    }

    pub fn set_self_user_id(&self, user_id: i64) {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.doc.user_id != user_id && user_id != 0 {
            guard.doc.user_id = user_id;
        }
    }
}

impl StateBackend for TgState {
    fn updates_state(&self) -> UpdatesState {
        let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.doc.has_state {
            return UpdatesState::default();
        }
        let mut channels: Vec<ChannelState> = guard
            .doc
            .channels
            .iter()
            .filter_map(|(k, &pts)| k.parse::<i64>().ok().map(|id| ChannelState { id, pts }))
            .collect();
        channels.sort_by_key(|c| c.id);
        UpdatesState {
            pts: guard.doc.pts,
            qts: guard.doc.qts,
            date: guard.doc.date,
            seq: guard.doc.seq,
            channels,
        }
    }

    fn set_update_state(&self, update: UpdateState) {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return;
        }
        match update {
            UpdateState::All(state) => {
                guard.doc.has_state = true;
                guard.doc.pts = state.pts;
                guard.doc.qts = state.qts;
                guard.doc.date = state.date;
                guard.doc.seq = state.seq;
                guard.doc.channels.clear();
                for ch in state.channels {
                    guard.doc.channels.insert(ch.id.to_string(), ch.pts);
                }
            }
            UpdateState::Primary { pts, date, seq } => {
                guard.doc.has_state = true;
                guard.doc.pts = pts;
                guard.doc.date = date;
                guard.doc.seq = seq;
            }
            UpdateState::Secondary { qts } => {
                guard.doc.has_state = true;
                guard.doc.qts = qts;
            }
            UpdateState::Channel { id, pts } => {
                guard.doc.has_state = true;
                guard.doc.channels.insert(id.to_string(), pts);
            }
        }
        self.mark_dirty_locked(&mut guard);
    }

    fn peer(&self, peer: PeerId) -> Option<PeerInfo> {
        let guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if peer == PeerId::self_user() {
            let uid = guard.doc.user_id;
            if uid <= 0 {
                return None;
            }
            let auth = guard
                .doc
                .users
                .get(&uid.to_string())
                .copied()
                .filter(|&h| h != 0)
                .map(PeerAuth::from_hash);
            return Some(PeerInfo::User {
                id: uid,
                auth,
                bot: None,
                is_self: Some(true),
            });
        }
        let bare = peer.bare_id()?;
        match peer.kind() {
            PeerKind::User => {
                let hash = guard.doc.users.get(&bare.to_string()).copied()?;
                let is_self = (guard.doc.user_id != 0 && guard.doc.user_id == bare).then_some(true);
                Some(PeerInfo::User {
                    id: bare,
                    auth: Some(PeerAuth::from_hash(hash)),
                    bot: None,
                    is_self,
                })
            }
            PeerKind::Channel => {
                let hash = guard.doc.chans.get(&bare.to_string()).copied()?;
                Some(PeerInfo::Channel {
                    id: bare,
                    auth: Some(PeerAuth::from_hash(hash)),
                    kind: None,
                })
            }
            PeerKind::Chat => Some(PeerInfo::Chat { id: bare }),
        }
    }

    fn cache_peer(&self, peer: PeerInfo) {
        self.cache_peers(vec![peer]);
    }

    fn cache_peers(&self, peers: Vec<PeerInfo>) {
        let mut guard = self.core.mu.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return;
        }
        enum Kind {
            User,
            Channel,
        }
        let mut pending: Vec<(Kind, String, i64)> = Vec::new();
        for p in peers {
            match p {
                PeerInfo::User {
                    id,
                    auth,
                    is_self,
                    ..
                } => {
                    if is_self == Some(true) && guard.doc.user_id != id {
                        guard.doc.user_id = id;
                    }
                    if let Some(hash) = auth.map(PeerAuth::hash).filter(|&h| h != 0) {
                        let key = id.to_string();
                        if guard.doc.users.get(&key).copied() != Some(hash) {
                            pending.push((Kind::User, key, hash));
                        }
                    }
                }
                PeerInfo::Channel { id, auth, .. } => {
                    if let Some(hash) = auth.map(PeerAuth::hash).filter(|&h| h != 0) {
                        let key = id.to_string();
                        if guard.doc.chans.get(&key).copied() != Some(hash) {
                            pending.push((Kind::Channel, key, hash));
                        }
                    }
                }
                PeerInfo::Chat { .. } => {}
            }
        }
        if pending.is_empty() || pending.len() > MAX_PERSIST_BATCH {
            return;
        }
        for (kind, key, hash) in pending {
            match kind {
                Kind::User => {
                    guard.doc.users.insert(key, hash);
                }
                Kind::Channel => {
                    guard.doc.chans.insert(key, hash);
                }
            }
        }
        guard.hash_dirty = true;
        self.mark_dirty_locked(&mut guard);
    }
}

fn cursor_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".cursor");
    PathBuf::from(os)
}

fn flush_locked(path: &Path, inner: &mut Inner) -> Result<(), TgStateError> {
    if !inner.dirty {
        return Ok(());
    }
    let elapsed = inner
        .last_checkpoint
        .elapsed()
        .unwrap_or(Duration::ZERO);
    if inner.hash_dirty || inner.doc.generation == 0 || elapsed >= CHECKPOINT_INTERVAL {
        return checkpoint_locked(path, inner);
    }
    let cursor = CursorDocument {
        generation: inner.doc.generation,
        user_id: inner.doc.user_id,
        pts: inner.doc.pts,
        qts: inner.doc.qts,
        date: inner.doc.date,
        seq: inner.doc.seq,
        has_state: inner.doc.has_state,
        channels: inner.doc.channels.clone(),
    };
    let encoded = serde_json::to_vec(&cursor)?;
    mibot_fsutil::write_file_atomic(&cursor_path_for(path), &encoded, 0o600)?;
    inner.dirty = false;
    Ok(())
}

fn checkpoint_locked(path: &Path, inner: &mut Inner) -> Result<(), TgStateError> {
    if !inner.checkpoint_dirty {
        return Ok(());
    }
    if inner.doc.generation == 0 {
        match fs::remove_file(cursor_path_for(path)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(TgStateError::Io(e)),
        }
    }
    let mut next = inner.doc.clone();
    next.generation += 1;
    let encoded = serde_json::to_vec(&next)?;
    mibot_fsutil::write_file_atomic(path, &encoded, 0o600)?;

    inner.doc.generation = next.generation;
    inner.dirty = false;
    inner.hash_dirty = false;
    inner.checkpoint_dirty = false;
    inner.last_checkpoint = SystemTime::now();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mibot-tgstate-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_survives_restart() {
        let dir = temp_dir();
        let path = dir.join("updates.json");
        let state = TgState::open(&path).unwrap();
        state
            .set_state(
                7,
                AccountState {
                    pts: 10,
                    qts: 2,
                    date: 300,
                    seq: 4,
                },
            )
            .unwrap();
        state.set_channel_pts(7, 99, 55).unwrap();
        state.set_channel_access_hash(99, 1234).unwrap();
        state.set_user_access_hash(5, 6789).unwrap();
        state.close().unwrap();

        let reopened = TgState::open(&path).unwrap();
        let got = reopened.get_state(7).unwrap().unwrap();
        assert_eq!(got.pts, 10);
        assert_eq!(got.seq, 4);
        assert_eq!(reopened.get_channel_pts(7, 99), Some(55));
        assert_eq!(reopened.get_channel_access_hash(99), Some(1234));
        assert_eq!(reopened.get_user_access_hash(5), Some(6789));
        reopened.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_field_without_state_fails_and_set_state_resets_channels() {
        let dir = temp_dir();
        let state = TgState::open(dir.join("updates.json")).unwrap();
        assert!(matches!(state.set_pts(7, 5), Err(TgStateError::NoState)));
        assert!(state.get_state(7).unwrap().is_none());

        state.set_state(1, AccountState { pts: 1, ..Default::default() }).unwrap();
        state.set_channel_pts(1, 42, 7).unwrap();
        assert_eq!(state.get_channel_pts(1, 42), Some(7));
        state.set_state(1, AccountState { pts: 2, ..Default::default() }).unwrap();
        assert_eq!(state.get_channel_pts(1, 42), None);
        state.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_list_is_ordered() {
        let dir = temp_dir();
        let state = TgState::open(dir.join("updates.json")).unwrap();
        state.set_state(1, AccountState::default()).unwrap();
        for id in [30, 10, 20] {
            state.set_channel_pts(1, id, id as i32).unwrap();
        }
        let ids: Vec<i64> = state.channel_pts_list(1).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![10, 20, 30]);
        state.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_flush_recovers_without_close_and_skips_full_rewrite() {
        let dir = temp_dir();
        let path = dir.join("updates.json");
        let s = TgState::open(&path).unwrap();
        s.set_state(7, AccountState { pts: 10, ..Default::default() }).unwrap();
        s.set_user_access_hash(42, 99).unwrap();
        s.flush().unwrap();

        let before = fs::read(&path).unwrap();
        s.set_pts(7, 20).unwrap();
        s.set_channel_pts(7, 88, 30).unwrap();
        s.flush().unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after, "cursor flush rewrote full snapshot");

        s.crash_for_test();
        let r = TgState::open(&path).unwrap();
        assert_eq!(r.get_state(7).unwrap().unwrap().pts, 20);
        assert_eq!(r.get_channel_pts(7, 88), Some(30));
        assert_eq!(r.get_user_access_hash(42), Some(99));
        r.close().unwrap();

        let doc: Document = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc.pts, 20);
        assert_eq!(doc.channels.get("88"), Some(&30));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_ignores_stale_cursor_and_old_rewrite() {
        let dir = temp_dir();
        let path = dir.join("updates.json");
        let s = TgState::open(&path).unwrap();
        s.set_state(7, AccountState { pts: 10, ..Default::default() }).unwrap();
        s.flush().unwrap();
        s.set_pts(7, 20).unwrap();
        s.flush().unwrap();
        s.set_user_access_hash(42, 99).unwrap();
        s.flush().unwrap();
        s.close().unwrap();

        let doc: Document = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc.pts, 20);
        assert_eq!(doc.users.get("42"), Some(&99));

        // Legacy binary rewrites without generation.
        let mut legacy_doc = doc;
        legacy_doc.generation = 0;
        legacy_doc.pts = 35;
        fs::write(&path, serde_json::to_vec(&legacy_doc).unwrap()).unwrap();

        let legacy = TgState::open(&path).unwrap();
        assert_eq!(legacy.get_state(7).unwrap().unwrap().pts, 35);
        legacy.set_pts(7, 40).unwrap();
        legacy.flush().unwrap();
        legacy.close().unwrap();

        let again = TgState::open(&path).unwrap();
        assert_eq!(again.get_state(7).unwrap().unwrap().pts, 40);
        again.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn periodic_and_explicit_checkpoint_and_open_does_not_write() {
        let dir = temp_dir();
        let path = dir.join("updates.json");
        let empty = TgState::open(&path).unwrap();
        empty.close().unwrap();
        assert!(!path.exists());

        let s = TgState::open(&path).unwrap();
        s.set_state(7, AccountState { pts: 1, ..Default::default() }).unwrap();
        s.flush().unwrap();
        s.set_pts(7, 2).unwrap();
        s.flush().unwrap();
        s.checkpoint().unwrap();
        let doc: Document = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc.pts, 2);

        s.backdate_checkpoint_for_test(CHECKPOINT_INTERVAL);
        s.set_pts(7, 3).unwrap();
        s.flush().unwrap();
        let doc: Document = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc.pts, 3);
        s.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn continuous_updates_reach_disk_and_error_callback_does_not_deadlock() {
        let dir = temp_dir();
        let path = dir.join("state.json");
        let s = TgState::open(&path).unwrap();
        s.set_state(1, AccountState::default()).unwrap();
        let mut reached = false;
        for pts in 1..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            s.set_pts(1, pts).unwrap();
            if path.exists() && fs::metadata(&path).unwrap().len() > 0 {
                reached = true;
                break;
            }
        }
        assert!(reached, "continuous writes postponed persistence");
        s.close().unwrap();

        // Callback deadlock + retry check
        let blocked_parent = dir.join("blocked");
        let blocked_state = TgState::open(blocked_parent.join("state.json")).unwrap();
        fs::write(&blocked_parent, b"x").unwrap();
        let seen_err = Arc::new(Notify::new());
        let seen_err_clone = Arc::clone(&seen_err);
        let state_ref = blocked_state.clone();
        blocked_state.set_on_flush_error(move |_| {
            let _ = state_ref.get_state(1);
            seen_err_clone.notify_one();
        });
        blocked_state.set_state(1, AccountState { pts: 7, ..Default::default() }).unwrap();
        tokio::time::timeout(Duration::from_secs(3), seen_err.notified())
            .await
            .expect("error callback deadlocked or did not run");

        fs::remove_file(&blocked_parent).unwrap();
        let mut recovered = false;
        for _ in 0..100 {
            if blocked_state.path().exists() {
                recovered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(recovered, "failed flush was never retried");
        blocked_state.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_backend_caps_bulk_peer_persistence() {
        let dir = temp_dir();
        let path = dir.join("updates.json");
        let state = TgState::open(&path).unwrap();
        let bulk: Vec<PeerInfo> = (100i64..100 + MAX_PERSIST_BATCH as i64 + 1)
            .map(|id| PeerInfo::User {
                id,
                auth: Some(PeerAuth::from_hash(id * 3)),
                bot: None,
                is_self: None,
            })
            .collect();
        state.cache_peers(bulk);
        assert!(state.get_user_access_hash(100).is_none());

        state.cache_peers(vec![PeerInfo::User {
            id: 42,
            auth: Some(PeerAuth::from_hash(7)),
            bot: None,
            is_self: None,
        }]);
        assert_eq!(state.get_user_access_hash(42), Some(7));
        state.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
