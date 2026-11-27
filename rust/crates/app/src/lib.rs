pub mod live;
pub mod subscribe;

pub use live::{LiveHandler, MessageSink};
pub use subscribe::{
    GuardOutcome, ReadyTrigger, SUBSCRIBE_INITIAL_BACKOFF, SUBSCRIBE_MAX_BACKOFF,
    is_unauthorized_bot_error, is_unauthorized_invocation, run_subscribe_guard,
    run_updates_supervisor,
};

use grammers_mtsender::{ConnectionParams, NoRetries, SenderPool};
use grammers_tl_types as tl;
use mibot_bot::{BoxFuture, Client, Message, PeerCache, SentRecorder, envelope, peer_id};
use mibot_command::Registry;
use mibot_config::{self, Config, ConfigError, Env, LOCK_FILE, SESSION_FILE, STATE_FILE};
use mibot_fsutil::ensure_dir;
use mibot_logtail::{Level, Ring};
use mibot_session::{MibotSession, SessionError, StateBackend};
use mibot_tgstate::{TgState, TgStateError};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::num::NonZeroUsize;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub const SEEN_WINDOW: Duration = Duration::from_secs(15 * 60);
const SEEN_CAP: usize = 65536;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum AppError {
    #[error("another mibot-lite instance is already running for this directory")]
    AlreadyRunning,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    State(#[from] TgStateError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

/// Exclusive non-blocking advisory lock held for the lifetime of an active instance.
#[derive(Debug)]
pub struct RootLock {
    _file: File,
    pub path: PathBuf,
}

/// Acquires `LOCK_FILE` inside `root` using non-blocking `flock(LOCK_EX | LOCK_NB)`.
/// Releases automatically when the returned `RootLock` is dropped.
pub fn lock_root(root: &Path) -> Result<RootLock, AppError> {
    let lock_path = root.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;
    // SAFETY: `file.as_raw_fd()` is an open descriptor owned exclusively by `file`,
    // and `flock` with `LOCK_EX | LOCK_NB` performs only kernel-level advisory locking.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(AppError::AlreadyRunning);
        }
        return Err(AppError::Io(err));
    }
    Ok(RootLock {
        _file: file,
        path: lock_path,
    })
}

struct SeenInner {
    cur: HashSet<String>,
    prev: HashSet<String>,
    rotated_at: Instant,
}

/// Two-generation message deduplicator rolling every `SEEN_WINDOW` (15m).
pub struct SeenMessages {
    inner: Mutex<SeenInner>,
}

impl Default for SeenMessages {
    fn default() -> Self {
        Self::new()
    }
}

impl SeenMessages {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SeenInner {
                cur: HashSet::new(),
                prev: HashSet::new(),
                rotated_at: Instant::now(),
            }),
        }
    }

    pub fn first(&self, chat_id: &str, msg_id: i32) -> bool {
        self.first_at(chat_id, msg_id, Instant::now())
    }

    pub fn first_at(&self, chat_id: &str, msg_id: i32, now: Instant) -> bool {
        let key = format!("{chat_id}:{msg_id}");
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        maybe_rotate(&mut g, now);
        if g.cur.contains(&key) || g.prev.contains(&key) {
            return false;
        }
        g.cur.insert(key);
        true
    }

    pub fn mark(&self, chat_id: &str, msg_id: i32) {
        let key = format!("{chat_id}:{msg_id}");
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        maybe_rotate(&mut g, Instant::now());
        g.cur.insert(key);
    }
}

fn maybe_rotate(g: &mut SeenInner, now: Instant) {
    if now.saturating_duration_since(g.rotated_at) >= SEEN_WINDOW || g.cur.len() >= SEEN_CAP {
        let old_cur = std::mem::take(&mut g.cur);
        g.prev = old_cur;
        g.rotated_at = now;
    }
}

pub fn sent_before_start(date: i32, started_unix: i64) -> bool {
    i64::from(date) < started_unix
}

pub fn is_edit_update(update: &tl::enums::Update) -> bool {
    matches!(
        update,
        tl::enums::Update::EditMessage(_) | tl::enums::Update::EditChannelMessage(_)
    )
}

pub type RegisterFn = Box<dyn FnOnce(&Arc<App>) + Send>;
pub type ReadyCallback = Arc<dyn Fn(Arc<App>, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync>;
pub type ForeignListener = Arc<dyn Fn(&Message, &Arc<App>, &CancellationToken) -> bool + Send + Sync>;

pub struct Options {
    pub root: PathBuf,
    pub version: String,
    pub verbose: bool,
    pub read_only: bool,
    pub register: Option<RegisterFn>,
    pub after_ready: Option<ReadyCallback>,
    pub foreign: Option<ForeignListener>,
}

impl Options {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            version: String::new(),
            verbose: false,
            read_only: false,
            register: None,
            after_ready: None,
            foreign: None,
        }
    }
}

pub struct App {
    _lock: Option<RootLock>,
    pub root: PathBuf,
    pub version: String,
    pub read_only: bool,
    pub config: Config,
    pub env: Env,
    pub session: Arc<MibotSession>,
    pub state: Option<Arc<TgState>>,
    pub client: Arc<Client>,
    pub registry: Arc<Registry>,
    pub logs: Arc<Ring>,
    pub peers: Arc<PeerCache>,
    pub seen: Arc<SeenMessages>,
    pub started_unix: i64,
    pub started_at: Instant,
    pub boot_id: String,
    foreign: RwLock<Vec<ForeignListener>>,
    after_ready: Option<ReadyCallback>,
    ready_hooks: Mutex<Vec<ReadyCallback>>,
    pool: Mutex<Option<SenderPool>>,
}

impl App {
    /// Prepares configuration, single-instance lock, session, persistent peer/state backend,
    /// MTProto `SenderPool` (`NoRetries`), `Client`, and `Registry`. When `opts.read_only` is true,
    /// neither locks nor creates files/directories on disk.
    pub fn prepare(mut opts: Options) -> Result<Arc<Self>, AppError> {
        let lock = if opts.read_only {
            None
        } else {
            Some(lock_root(&opts.root)?)
        };

        let cfg = mibot_config::read(&opts.root)?;
        let proc_env: Vec<(String, String)> = std::env::vars().collect();
        let env = mibot_config::read_env(
            &opts.root,
            proc_env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        );
        if let Some(lang) = mibot_i18n::Lang::parse(env.get("MIBOT_LANG", "")) {
            mibot_i18n::set(lang);
        }
        if !opts.read_only {
            ensure_dir(&opts.root.join("data"))?;
        }

        let logs = Arc::new(Ring::new());
        let default_level = if opts.verbose { Level::Debug } else { Level::Info };
        let level = mibot_logtail::Level::parse(env.get("MIBOT_LOG_LEVEL", ""))
            .unwrap_or(default_level);
        logs.set_level(level);

        let state_path = opts.root.join(STATE_FILE);
        let state = Arc::new(TgState::open(&state_path)?);
        let state_logs = Arc::clone(&logs);
        if !opts.read_only {
            state.set_on_flush_error(move |err| {
                state_logs.record(
                    Level::Error,
                    "tgstate.flush_failed",
                    &[("err", &err.to_string())],
                );
            });
        }

        let backend: Option<Arc<dyn StateBackend>> = if opts.read_only {
            None
        } else {
            Some(Arc::clone(&state) as Arc<dyn StateBackend>)
        };
        let session = Arc::new(MibotSession::open(
            Some(opts.root.join(SESSION_FILE)),
            &cfg.session,
            opts.read_only,
            backend,
        )?);

        let peers = Arc::new(PeerCache::new());
        if opts.read_only {
            peers.set_durable_lookup(Arc::clone(&state) as Arc<dyn mibot_bot::HashLookup>);
        } else {
            peers.set_durable_store(Arc::clone(&state) as Arc<dyn mibot_bot::HashStore>);
        }

        let seen = Arc::new(SeenMessages::new());
        let seen_for_sent = Arc::clone(&seen);
        let sent = Arc::new(SentRecorder::new(Arc::new(move |p, id| {
            seen_for_sent.mark(&peer_id(p), id);
        })));

        let params = ConnectionParams {
            device_model: cfg.device_model.clone(),
            retry_policy: Box::new(NoRetries),
            updates_channel_capacity: NonZeroUsize::new(1024).unwrap_or(NonZeroUsize::MIN),
            proxy_url: cfg.proxy.as_ref().map(|p| p.socks5_url()),
            ..Default::default()
        };

        let pool = SenderPool::with_configuration(Arc::clone(&session), cfg.api_id, params);
        let client = Arc::new(Client::from_sender_pool(
            pool.handle.thin.clone(),
            Arc::clone(&session),
            Arc::clone(&peers),
            sent,
        ));

        let registry = Arc::new(Registry::new());
        registry.set_prefixes(env.prefixes());
        registry.set_logs(Arc::clone(&logs));
        registry.bind_client(Arc::clone(&client));
        let register = opts.register.take();

        let started_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let started_at = Instant::now();
        let boot_id = format!("{}-{:016x}", std::process::id(), mibot_bot::random_id() as u64);
        let initial_foreign: Vec<ForeignListener> = opts.foreign.take().into_iter().collect();

        let app = Arc::new(Self {
            _lock: lock,
            root: opts.root,
            version: opts.version,
            read_only: opts.read_only,
            config: cfg,
            env,
            session,
            state: Some(state),
            client,
            registry,
            logs,
            peers,
            seen,
            started_unix,
            started_at,
            boot_id,
            foreign: RwLock::new(initial_foreign),
            after_ready: opts.after_ready,
            ready_hooks: Mutex::new(Vec::new()),
            pool: Mutex::new(Some(pool)),
        });

        if let Some(reg_fn) = register {
            reg_fn(&app);
        }
        Ok(app)
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Constructs a lightweight `App` for unit/integration tests without opening locks or sockets.
    pub fn for_testing(
        root: PathBuf,
        version: &str,
        boot_id: &str,
        client: Arc<Client>,
        peers: Arc<PeerCache>,
        logs: Arc<Ring>,
    ) -> Arc<Self> {
        let registry = Arc::new(Registry::new());
        registry.set_logs(Arc::clone(&logs));
        registry.bind_client(Arc::clone(&client));
        Arc::new(Self {
            _lock: None,
            root,
            version: version.to_owned(),
            read_only: true,
            config: Config {
                api_id: 1,
                api_hash: "test".into(),
                session: String::new(),
                device_model: "test".into(),
                proxy: None,
            },
            env: Env::default(),
            session: Arc::new(MibotSession::from_session_data(Default::default())),
            state: None,
            client,
            registry,
            logs,
            peers,
            seen: Arc::new(SeenMessages::new()),
            started_unix: 0,
            started_at: Instant::now(),
            boot_id: boot_id.to_owned(),
            foreign: RwLock::new(Vec::new()),
            after_ready: None,
            ready_hooks: Mutex::new(Vec::new()),
            pool: Mutex::new(None),
        })
    }

    pub fn warn(&self, event: &str, msg: &str) {
        self.logs.record(Level::Warn, event, &[("error", msg)]);
    }

    pub fn checkpoint_state(&self) -> Result<(), AppError> {
        if let Some(st) = &self.state
            && !self.read_only
        {
            st.checkpoint()?;
        }
        Ok(())
    }

    pub fn on_ready(&self, cb: ReadyCallback) {
        self.ready_hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(cb);
    }

    pub fn dispatch_message(&self, cancel: CancellationToken, msg: tl::types::Message) -> bool {
        self.handle_with_app(None, &msg, false, &cancel)
    }

    pub fn set_foreign_listener(&self, listener: ForeignListener) {
        *self.foreign.write().unwrap_or_else(|e| e.into_inner()) = vec![listener];
    }

    pub fn on_foreign(&self, listener: ForeignListener) {
        self.foreign
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(listener);
    }

    /// Evaluates a single raw Telegram `tl::types::Message` for identity, start-time cut-off,
    /// dedup, edit/forward suppression, foreign relay, and command dispatch.
    pub fn handle(
        &self,
        raw: &tl::types::Message,
        edited: bool,
        cancel: &CancellationToken,
    ) -> bool {
        self.handle_with_app(None, raw, edited, cancel)
    }

    pub fn handle_arc(
        self: &Arc<Self>,
        raw: &tl::types::Message,
        edited: bool,
        cancel: &CancellationToken,
    ) -> bool {
        self.handle_with_app(Some(self), raw, edited, cancel)
    }

    fn handle_with_app(
        &self,
        self_arc: Option<&Arc<Self>>,
        raw: &tl::types::Message,
        edited: bool,
        cancel: &CancellationToken,
    ) -> bool {
        if edited {
            return false;
        }
        let chat_key = peer_id(&raw.peer_id);
        if chat_key.is_empty() || !self.seen.first(&chat_key, raw.id) {
            return false;
        }
        let self_id = self.peers.self_id();
        let Some(env) = envelope(raw, self_id, edited, &self.peers) else {
            return false;
        };
        let mine = raw.out || (self_id != 0 && env.sender_is_user(self_id));

        if !mine {
            if !env.edited
                && !env.forward
                && !sent_before_start(raw.date, self.started_unix)
                && let Some(app_arc) = self_arc
            {
                let listeners = self
                    .foreign
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                for listener in listeners {
                    if listener(&env, app_arc, cancel) {
                        break;
                    }
                }
            }
            return false;
        }

        if sent_before_start(raw.date, self.started_unix) {
            self.log_dropped(&env.text, "before_start");
            return false;
        }
        if env.edited || env.forward {
            let reason = if env.edited { "edited" } else { "forward" };
            self.log_dropped(&env.text, reason);
            return false;
        }

        match self.registry.dispatch(env, cancel.clone()) {
            mibot_command::DispatchOutcome::Queued => true,
            mibot_command::DispatchOutcome::Rejected(_) => {
                self.logs.record(Level::Warn, "app.dispatch_rejected", &[]);
                false
            }
            mibot_command::DispatchOutcome::NotCommand => false,
        }
    }

    fn log_dropped(&self, text: &str, reason: &str) {
        let prefixes = self.registry.prefixes();
        if prefixes.iter().any(|p| text.starts_with(p.as_str())) {
            self.logs
                .record(Level::Debug, "app.command_ignored", &[("reason", reason)]);
        }
    }

    /// Starts the MTProto `SenderPoolRunner`, `TgState` periodic checkpoint loop, live update pump,
    /// and supervised `UpdatesReceiver` until `cancel` triggers or session is revoked (401).
    pub async fn run(self: &Arc<Self>, cancel: CancellationToken) -> GuardOutcome {
        let pool = match self.pool.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(p) => p,
            None => return GuardOutcome::Stopped,
        };

        let runner_task = tokio::spawn(pool.runner.run());
        let fat_handle = pool.handle;
        let updates_rx = pool.updates;

        let checkpoint_cancel = cancel.child_token();
        let checkpoint_task = self.state.as_ref().map(|st| {
            let st_clone = Arc::clone(st);
            let c = checkpoint_cancel.clone();
            tokio::spawn(async move {
                st_clone.run_checkpoints(c).await;
            })
        });

        let app_ref = Arc::clone(self);
        let handler_cancel = cancel.clone();
        let live = Arc::new(LiveHandler::new(
            Arc::clone(&self.peers),
            Arc::clone(&self.seen),
            Arc::clone(&self.logs),
            Arc::new(move |msg, edited| app_ref.handle_arc(msg, edited, &handler_cancel)),
        ));

        let app_for_ready = Arc::clone(self);
        let cancel_for_ready = cancel.clone();
        let after_ready_opt = self.after_ready.clone();
        let ready_trigger: Option<ReadyTrigger> = Some(Arc::new(move || {
            app_for_ready.logs.record(Level::Info, "runtime.ready", &[]);
            eprintln!("level=INFO msg=runtime.ready");
            let hooks: Vec<ReadyCallback> = app_for_ready
                .ready_hooks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
                .collect();
            for hook in hooks {
                let a = Arc::clone(&app_for_ready);
                let c = cancel_for_ready.clone();
                tokio::spawn(async move {
                    hook(a, c).await;
                });
            }
            if let Some(cb) = &after_ready_opt {
                let a = Arc::clone(&app_for_ready);
                let c = cancel_for_ready.clone();
                let cb = Arc::clone(cb);
                tokio::spawn(async move {
                    cb(a, c).await;
                });
            }
        }));

        let outcome = run_updates_supervisor(
            cancel.clone(),
            Arc::clone(&self.client),
            Arc::clone(&self.session),
            self.state.clone(),
            Arc::clone(&self.peers),
            Arc::clone(&self.logs),
            fat_handle.clone(),
            updates_rx,
            live,
            ready_trigger,
        )
        .await;

        cancel.cancel();
        let _ = self.registry.quiesce(SHUTDOWN_TIMEOUT).await;
        fat_handle.thin.quit();
        let _ = tokio::time::timeout(Duration::from_secs(5), runner_task).await;

        checkpoint_cancel.cancel();
        if let Some(t) = checkpoint_task {
            let _ = t.await;
        }
        if let Some(st) = &self.state
            && !self.read_only
        {
            let _ = st.close();
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::InvocationError;
    use mibot_bot::BoxFuture;
    use mibot_command::{Command, group};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-app-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct NullTransport;
    impl mibot_bot::RpcTransport for NullTransport {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            _constructor_id: u32,
            _body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async { Err(InvocationError::Dropped("stub")) })
        }
    }

    fn mk_raw_msg(
        id: i32,
        peer: tl::enums::Peer,
        from: Option<tl::enums::Peer>,
        out: bool,
        forward: bool,
        date: i32,
        text: &str,
    ) -> tl::types::Message {
        tl::types::Message {
            out,
            mentioned: false,
            media_unread: false,
            silent: false,
            post: false,
            from_scheduled: false,
            legacy: false,
            edit_hide: false,
            pinned: false,
            noforwards: false,
            invert_media: false,
            offline: false,
            video_processing_pending: false,
            paid_suggested_post_stars: false,
            paid_suggested_post_ton: false,
            id,
            from_id: from,
            from_boosts_applied: None,
            from_rank: None,
            peer_id: peer,
            saved_peer_id: None,
            fwd_from: if forward {
                Some(
                    tl::types::MessageFwdHeader {
                        imported: false,
                        saved_out: false,
                        from_id: None,
                        from_name: Some("Other".into()),
                        date: 1,
                        channel_post: None,
                        post_author: None,
                        saved_from_peer: None,
                        saved_from_msg_id: None,
                        saved_from_id: None,
                        saved_from_name: None,
                        saved_date: None,
                        psa_type: None,
                    }
                    .into(),
                )
            } else {
                None
            },
            via_bot_id: None,
            via_business_bot_id: None,
            guestchat_via_from: None,
            reply_to: None,
            date,
            message: text.into(),
            media: None,
            reply_markup: None,
            entities: None,
            views: None,
            forwards: None,
            replies: None,
            edit_date: None,
            post_author: None,
            grouped_id: None,
            reactions: None,
            restriction_reason: None,
            ttl_period: None,
            quick_reply_shortcut_id: None,
            effect: None,
            factcheck: None,
            report_delivery_until_date: None,
            paid_message_stars: None,
            suggested_post: None,
            schedule_repeat_period: None,
            summary_from_language: None,
            rich_message: None,
        }
    }

    #[test]
    fn lock_root_rejects_second_holder_and_releases_on_drop() {
        let dir = temp_dir("lock");
        let guard = lock_root(&dir).unwrap();
        assert!(matches!(lock_root(&dir), Err(AppError::AlreadyRunning)));
        drop(guard);
        let guard2 = lock_root(&dir).unwrap();
        drop(guard2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_read_only_does_not_create_files_or_acquire_lock() {
        let dir = temp_dir("readonly");
        let ss = mibot_session::StringSession {
            dc_id: 2,
            address: "149.154.167.50".into(),
            port: 443,
            auth_key: [9u8; 256],
        }
        .encode()
        .unwrap();
        let cfg = Config {
            api_id: 12345,
            api_hash: "0123456789abcdef0123456789abcdef".into(),
            session: ss,
            device_model: "MiBot Lite".into(),
            proxy: None,
        };
        std::fs::write(
            dir.join(mibot_config::CONFIG_FILE),
            mibot_config::to_json_bytes(&cfg).unwrap(),
        )
        .unwrap();

        let mut opts = Options::new(&dir);
        opts.read_only = true;
        let app1 = App::prepare(opts).unwrap();

        let mut opts2 = Options::new(&dir);
        opts2.read_only = true;
        let app2 = App::prepare(opts2).unwrap();

        assert!(!app1.boot_id.is_empty());
        assert_ne!(
            app1.boot_id, app2.boot_id,
            "consecutive App::prepare calls must produce distinct per-instance boot_id"
        );

        assert!(!dir.join(LOCK_FILE).exists());
        assert!(!dir.join("data").exists());
        assert!(!dir.join(SESSION_FILE).exists());
        assert!(!dir.join(STATE_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seen_messages_rotates_after_window() {
        let seen = SeenMessages::new();
        let t0 = Instant::now();
        assert!(seen.first_at("42", 1, t0));
        assert!(!seen.first_at("42", 1, t0));
        let t1 = t0 + SEEN_WINDOW + Duration::from_secs(1);
        // After 1 rotation, key stays in `prev`.
        assert!(!seen.first_at("42", 1, t1));
        let t2 = t1 + SEEN_WINDOW + Duration::from_secs(1);
        // After 2 rotations, key expires cleanly.
        assert!(seen.first_at("42", 1, t2));
    }

    #[tokio::test]
    async fn handle_distinguishes_channel_id_from_self_and_blocks_prestart_forwards() {
        let dir = temp_dir("handle");
        let ss = mibot_session::StringSession {
            dc_id: 2,
            address: "149.154.167.50".into(),
            port: 443,
            auth_key: [3u8; 256],
        }
        .encode()
        .unwrap();
        let cfg = Config {
            api_id: 12345,
            api_hash: "0123456789abcdef0123456789abcdef".into(),
            session: ss,
            device_model: "MiBot Lite".into(),
            proxy: None,
        };
        let peers = Arc::new(PeerCache::new());
        peers.set_self(777);
        let seen = Arc::new(SeenMessages::new());
        let logs = Arc::new(Ring::new());
        let client = Arc::new(Client::from_transport(
            Arc::new(NullTransport),
            Arc::clone(&peers),
            Arc::new(SentRecorder::default()),
        ));
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_ref = Arc::clone(&ran);
        let reg = Arc::new(Registry::new());
        reg.bind_client(Arc::clone(&client));
        reg.set_logs(Arc::clone(&logs));
        reg.register(Command {
            name: "ping".into(),
            aliases: vec![],
            group: group::GENERAL.into(),
            description: "ping".into(),
            usage: "".into(),
            help: "".into(),
            handle: Arc::new(move |_| {
                let c = Arc::clone(&ran_ref);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }),
            timeout: None,
            free_text: false,
            ..Default::default()
        });

        let state_arc = Arc::new(TgState::open(dir.join(STATE_FILE)).unwrap());
        let session = Arc::new(MibotSession::from_session_data(Default::default()));
        let app = Arc::new(App {
            _lock: None,
            root: dir.clone(),
            version: String::new(),
            read_only: true,
            config: cfg,
            env: Env::default(),
            session,
            state: Some(state_arc),
            client,
            registry: Arc::clone(&reg),
            logs: Arc::clone(&logs),
            peers,
            seen,
            started_unix: 1000,
            started_at: Instant::now(),
            boot_id: String::new(),
            foreign: RwLock::new(Vec::new()),
            after_ready: None,
            ready_hooks: Mutex::new(Vec::new()),
            pool: Mutex::new(None),
        });
        let cancel = CancellationToken::new();

        // Channel post with PeerChannel(channel_id == self_id) and out=false MUST NOT trigger self commands!
        let fake_self_channel = mk_raw_msg(
            1,
            tl::types::PeerChannel { channel_id: 777 }.into(),
            Some(tl::types::PeerChannel { channel_id: 777 }.into()),
            false,
            false,
            2000,
            ".ping",
        );
        assert!(!app.handle(&fake_self_channel, false, &cancel));

        // Pre-start outgoing message is ignored and logged as dropped command.
        let prestart = mk_raw_msg(
            2,
            tl::types::PeerUser { user_id: 777 }.into(),
            Some(tl::types::PeerUser { user_id: 777 }.into()),
            true,
            false,
            900,
            ".ping",
        );
        assert!(!app.handle(&prestart, false, &cancel));
        assert!(!logs.tail::<fn(&str) -> bool>(10, None).is_empty());

        // Forwarded outgoing message is ignored.
        let fwd = mk_raw_msg(
            3,
            tl::types::PeerUser { user_id: 777 }.into(),
            Some(tl::types::PeerUser { user_id: 777 }.into()),
            true,
            true,
            2000,
            ".ping",
        );
        assert!(!app.handle(&fwd, false, &cancel));

        // Fresh outgoing message runs command.
        let ok_msg = mk_raw_msg(
            4,
            tl::types::PeerUser { user_id: 777 }.into(),
            Some(tl::types::PeerUser { user_id: 777 }.into()),
            true,
            false,
            2000,
            ".ping",
        );
        assert!(app.handle(&ok_msg, false, &cancel));
        reg.wait().await;
        assert_eq!(ran.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
