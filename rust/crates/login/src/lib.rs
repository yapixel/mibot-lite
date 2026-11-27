use grammers_client::client::ClientConfiguration;
use grammers_client::{Client, SignInError};
use grammers_mtsender::{ConnectionParams, NoRetries, SenderPool};
use grammers_session::SessionData;
use mibot_app::{AppError, lock_root};
use mibot_config::{
    CONFIG_FILE, Config, ConfigError, DEFAULT_DEVICE_MODEL, Proxy, RESTART_RECEIPT_FILE,
    SESSION_FILE, STATE_FILE,
};
use mibot_fsutil::{ensure_dir, write_file_atomic};
use mibot_session::{FileStorage, MibotSession, SessionError};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoginError {
    #[error(transparent)]
    App(#[from] AppError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub api_id: Option<i32>,
    pub api_hash: Option<String>,
    pub device_model: Option<String>,
    pub proxy_raw: Option<String>,
    pub force: bool,
    pub phone: Option<String>,
}

impl Options {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            api_id: None,
            api_hash: None,
            device_model: None,
            proxy_raw: None,
            force: false,
            phone: None,
        }
    }
}

pub trait PromptIo {
    fn prompt_line(&mut self, label: &str) -> Result<String, LoginError>;
    fn prompt_secret(&mut self, label: &str) -> Result<String, LoginError>;
    fn println(&mut self, msg: &str);
}

pub struct StdioPrompt;

impl PromptIo for StdioPrompt {
    fn prompt_line(&mut self, label: &str) -> Result<String, LoginError> {
        print!("{label}");
        io::stdout().flush()?;
        let mut line = String::new();
        let n = io::stdin().lock().read_line(&mut line)?;
        if n == 0 {
            return Err(LoginError::Message("unexpected EOF reading input".into()));
        }
        Ok(line.trim().to_owned())
    }

    fn prompt_secret(&mut self, label: &str) -> Result<String, LoginError> {
        if io::stdin().is_terminal() {
            print!("{label}");
            io::stdout().flush()?;
            let secret = rpassword::read_password()?;
            Ok(secret.trim().to_owned())
        } else {
            self.println("(input is not a terminal; the next value will not be hidden)");
            self.prompt_line(label)
        }
    }

    fn println(&mut self, msg: &str) {
        println!("{msg}");
    }
}

#[derive(Debug)]
pub struct PreparedLogin {
    pub lock: mibot_app::RootLock,
    pub api_id: i32,
    pub api_hash: String,
    pub device_model: String,
    pub proxy: Option<Proxy>,
}

/// Validates lock, existing session overwrite rules (`--force`), and `--proxy` JSON BEFORE
/// prompting anything interactively or initiating any network connection.
pub fn precheck(opts: &Options, io_ui: &mut dyn PromptIo) -> Result<PreparedLogin, LoginError> {
    ensure_dir(&opts.root)?;
    let lock = lock_root(&opts.root)?;

    let existing = mibot_config::read_partial(&opts.root)?;
    if !opts.force
        && let Some(cfg) = &existing
        && !cfg.session.trim().is_empty()
    {
        return Err(LoginError::Message(
            "config.json already contains a session; rerun with --force to replace it".into(),
        ));
    }

    let proxy = match &opts.proxy_raw {
        Some(raw) => mibot_config::parse_proxy_value(raw.as_bytes())?,
        None => existing.as_ref().and_then(|c| c.proxy.clone()),
    };

    let mut api_id = opts
        .api_id
        .filter(|&v| v > 0)
        .or_else(|| existing.as_ref().map(|c| c.api_id).filter(|&v| v > 0))
        .unwrap_or(0);
    let mut api_hash = opts
        .api_hash
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| existing.as_ref().map(|c| c.api_hash.clone()).filter(|s| !s.trim().is_empty()))
        .unwrap_or_default();

    let device_model = opts
        .device_model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| existing.as_ref().map(|c| c.device_model.clone()).filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_DEVICE_MODEL.to_owned());

    while api_id <= 0 {
        let raw = io_ui.prompt_line("API ID: ")?;
        match raw.trim().parse::<i32>() {
            Ok(v) if v > 0 => api_id = v,
            _ => io_ui.println("API ID must be a positive integer."),
        }
    }
    while api_hash.trim().is_empty() {
        api_hash = io_ui.prompt_line("API hash: ")?.trim().to_owned();
    }

    Ok(PreparedLogin {
        lock,
        api_id,
        api_hash,
        device_model,
        proxy,
    })
}

pub async fn run(opts: Options) -> Result<(), LoginError> {
    let mut ui = StdioPrompt;
    run_with_io(opts, &mut ui).await
}

pub async fn run_with_io(opts: Options, ui: &mut dyn PromptIo) -> Result<(), LoginError> {
    let prep = precheck(&opts, ui)?;
    let _lock = prep.lock;

    let session = Arc::new(MibotSession::for_login(false));
    let params = ConnectionParams {
        device_model: prep.device_model.clone(),
        retry_policy: Box::new(NoRetries),
        proxy_url: prep.proxy.as_ref().map(|p| p.socks5_url()),
        ..Default::default()
    };
    let pool = SenderPool::with_configuration(Arc::clone(&session), prep.api_id, params);
    let handle = pool.handle.clone();
    let runner_task = tokio::spawn(pool.runner.run());

    let client = Client::with_configuration(
        handle.clone(),
        ClientConfiguration {
            auto_cache_peers: false,
        },
    );

    let flow_res = authenticate_interactive(
        &client,
        &prep.api_hash,
        opts.phone.as_deref(),
        ui,
    )
    .await;

    let user = match flow_res {
        Ok(u) => u,
        Err(e) => {
            handle.thin.quit();
            let _ = runner_task.await;
            return Err(e);
        }
    };

    let string_session = session.export_string_session()?.encode()?;
    let session_data = session.snapshot_data()?;

    handle.thin.quit();
    let _ = runner_task.await;

    let cfg = Config {
        api_id: prep.api_id,
        api_hash: prep.api_hash,
        session: string_session,
        device_model: prep.device_model,
        proxy: prep.proxy,
    };
    write_account(&opts.root, &cfg, &session_data)?;

    let name = match &user.raw {
        grammers_client::tl::enums::User::User(u) => {
            format!("{} ({})", u.first_name.as_deref().unwrap_or("user"), u.id)
        }
        grammers_client::tl::enums::User::Empty(u) => format!("user ({})", u.id),
    };
    ui.println(&format!("logged in as {name}; wrote {CONFIG_FILE} and {SESSION_FILE}"));
    Ok(())
}

async fn authenticate_interactive(
    client: &Client,
    api_hash: &str,
    preset_phone: Option<&str>,
    ui: &mut dyn PromptIo,
) -> Result<grammers_client::peer::User, LoginError> {
    let authed = client
        .is_authorized()
        .await
        .map_err(|e| LoginError::Message(format!("auth check failed: {e}")))?;
    if !authed {
        let phone = match preset_phone {
            Some(p) if !p.trim().is_empty() => p.trim().to_owned(),
            _ => loop {
                let p = ui.prompt_line("Phone number (international format, e.g. +8613800138000): ")?;
                if !p.trim().is_empty() {
                    break p.trim().to_owned();
                }
            },
        };
        let token = client
            .request_login_code(&phone, api_hash)
            .await
            .map_err(|e| LoginError::Message(format!("request login code failed: {e}")))?;

        loop {
            let code = ui.prompt_line("Login code: ")?;
            match client.sign_in(&token, &code).await {
                Ok(_) => break,
                Err(SignInError::SignUpRequired) => {
                    return Err(LoginError::Message(
                        "this phone number has no Telegram account; create one in the official app first"
                            .into(),
                    ));
                }
                Err(SignInError::InvalidCode) => {
                    ui.println("Invalid code; please try again.");
                }
                Err(SignInError::PasswordRequired(mut pw_token)) => {
                    loop {
                        let hint = pw_token
                            .hint()
                            .filter(|h| !h.is_empty())
                            .map(|h| format!(" (hint: {h})"))
                            .unwrap_or_default();
                        let password = ui.prompt_secret(&format!("2FA Password{hint}: "))?;
                        match client.check_password(pw_token, password.as_bytes()).await {
                            Ok(_) => break,
                            Err(SignInError::InvalidPassword(next_tok)) => {
                                pw_token = next_tok;
                                ui.println("Invalid password; please try again.");
                            }
                            Err(e) => {
                                return Err(LoginError::Message(format!("2FA login failed: {e}")));
                            }
                        }
                    }
                    break;
                }
                Err(SignInError::InvalidPassword(_)) => {
                    return Err(LoginError::Message("unexpected invalid password state".into()));
                }
                Err(SignInError::Other(e)) => {
                    return Err(LoginError::Message(format!("sign in failed: {e}")));
                }
            }
        }
    }

    client
        .get_me()
        .await
        .map_err(|e| LoginError::Message(format!("get_me failed: {e}")))
}

struct StageCleanup(PathBuf);
impl Drop for StageCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Atomically replaces `config.json` and `gotd-session.json` while purging stale state files
/// (`updates.json`, `updates.json.cursor`, `restart-receipt.json`) so the new session starts clean.
pub fn write_account(root: &Path, cfg: &Config, session_data: &SessionData) -> Result<(), LoginError> {
    ensure_dir(root)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stage = root.join(format!(".login-stage-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&stage)?;
    let _cleanup = StageCleanup(stage.clone());

    let cfg_bytes = mibot_config::to_json_bytes(cfg)?;
    let staged_cfg = stage.join(CONFIG_FILE);
    let staged_session = stage.join(SESSION_FILE);

    write_file_atomic(&staged_cfg, &cfg_bytes, 0o600)?;
    FileStorage::new(&staged_session).save(session_data)?;

    let stale = [
        STATE_FILE.to_owned(),
        format!("{STATE_FILE}.cursor"),
        RESTART_RECEIPT_FILE.to_owned(),
        SESSION_FILE.to_owned(),
    ];
    for rel in &stale {
        let p = root.join(rel);
        if let Err(e) = fs::remove_file(&p)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(LoginError::Io(e));
        }
    }

    fs::rename(&staged_session, root.join(SESSION_FILE))?;
    fs::rename(&staged_cfg, root.join(CONFIG_FILE))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_session::types::DcOption;
    use std::collections::VecDeque;

    struct ScriptedIo {
        lines: VecDeque<String>,
        secrets: VecDeque<String>,
        prompts: Vec<String>,
        messages: Vec<String>,
    }

    impl ScriptedIo {
        fn new(lines: Vec<&str>, secrets: Vec<&str>) -> Self {
            Self {
                lines: lines.into_iter().map(str::to_owned).collect(),
                secrets: secrets.into_iter().map(str::to_owned).collect(),
                prompts: Vec::new(),
                messages: Vec::new(),
            }
        }
    }

    impl PromptIo for ScriptedIo {
        fn prompt_line(&mut self, label: &str) -> Result<String, LoginError> {
            self.prompts.push(label.to_owned());
            self.lines
                .pop_front()
                .ok_or_else(|| LoginError::Message("no scripted line left".into()))
        }

        fn prompt_secret(&mut self, label: &str) -> Result<String, LoginError> {
            self.prompts.push(label.to_owned());
            self.secrets
                .pop_front()
                .ok_or_else(|| LoginError::Message("no scripted secret left".into()))
        }

        fn println(&mut self, msg: &str) {
            self.messages.push(msg.to_owned());
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mibot-login-{name}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn precheck_rejects_bad_proxy_and_existing_session_before_prompting() {
        let dir = temp_dir("precheck");
        let existing_cfg = Config {
            api_id: 123,
            api_hash: "hash".into(),
            session: "1A".into(),
            device_model: "MiBot Lite".into(),
            proxy: None,
        };
        fs::write(
            dir.join(CONFIG_FILE),
            mibot_config::to_json_bytes(&existing_cfg).unwrap(),
        )
        .unwrap();

        let mut ui = ScriptedIo::new(vec!["999"], vec![]);
        let mut opts = Options::new(&dir);
        let err = precheck(&opts, &mut ui).unwrap_err().to_string();
        assert!(err.contains("--force"));
        assert!(ui.prompts.is_empty(), "must fail before prompting");

        // Invalid proxy with --force also fails before prompting.
        opts.force = true;
        opts.proxy_raw = Some(r#"{"socksType":4,"ip":"1.2.3.4","port":1080}"#.into());
        assert!(precheck(&opts, &mut ui).is_err());
        assert!(ui.prompts.is_empty(), "bad proxy must fail before prompting");

        // Invalid proxy already embedded in config.json also fails before prompting.
        opts.proxy_raw = None;
        fs::write(
            dir.join(CONFIG_FILE),
            br#"{"api_id":123,"api_hash":"hash","proxy":{"socksType":4,"ip":"1.2.3.4","port":1080}}"#,
        )
        .unwrap();
        assert!(precheck(&opts, &mut ui).is_err());
        assert!(ui.prompts.is_empty(), "bad existing config proxy must fail before prompting");

        // Restore valid partial config for lock test.
        fs::write(
            dir.join(CONFIG_FILE),
            br#"{"api_id":123,"api_hash":"hash"}"#,
        )
        .unwrap();

        // Locked root fails before prompting.
        opts.proxy_raw = None;
        let _lock = lock_root(&dir).unwrap();
        assert!(matches!(
            precheck(&opts, &mut ui),
            Err(LoginError::App(AppError::AlreadyRunning))
        ));
        assert!(ui.prompts.is_empty());
        drop(_lock);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_account_replaces_files_atomically_and_removes_stale_state() {
        let dir = temp_dir("write");
        for stale in [STATE_FILE, &format!("{STATE_FILE}.cursor"), RESTART_RECEIPT_FILE] {
            fs::write(dir.join(stale), b"stale").unwrap();
        }

        let mut sd = SessionData {
            home_dc: 2,
            ..Default::default()
        };
        sd.dc_options.insert(
            2,
            DcOption {
                id: 2,
                ipv4: "149.154.167.50:443".parse().unwrap(),
                ipv6: "[::1]:443".parse().unwrap(),
                auth_key: Some([5u8; 256]),
            },
        );
        let cfg = Config {
            api_id: 12345,
            api_hash: "abc".into(),
            session: mibot_session::StringSession {
                dc_id: 2,
                address: "149.154.167.50".into(),
                port: 443,
                auth_key: [5u8; 256],
            }
            .encode()
            .unwrap(),
            device_model: "Custom".into(),
            proxy: Some(Proxy {
                ip: "127.0.0.1".into(),
                port: 1080,
                username: "u".into(),
                password: "p".into(),
            }),
        };

        write_account(&dir, &cfg, &sd).unwrap();

        assert!(!dir.join(STATE_FILE).exists());
        assert!(!dir.join(format!("{STATE_FILE}.cursor")).exists());
        assert!(!dir.join(RESTART_RECEIPT_FILE).exists());
        assert!(dir.join(SESSION_FILE).exists());
        let parsed = mibot_config::read(&dir).unwrap();
        assert_eq!(parsed, cfg);

        let _ = fs::remove_dir_all(&dir);
    }
}
