use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use thiserror::Error;

pub const CONFIG_FILE: &str = "config.json";
pub const ENV_FILE: &str = ".env";
pub const SESSION_FILE: &str = "gotd-session.json";
pub const STATE_FILE: &str = "updates.json";
pub const RESTART_RECEIPT_FILE: &str = "restart-receipt.json";
pub const LOCK_FILE: &str = "mibot-lite.lock";

pub const MAX_CONFIG_BYTES: usize = 1 << 20;
pub const DEFAULT_DEVICE_MODEL: &str = "MiBot Lite";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Message(String),
    #[error("config.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub ip: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl Proxy {
    /// Formats a percent-encoded `socks5://[user:pass@]host:port` URL compatible with `grammers-mtsender`.
    pub fn socks5_url(&self) -> String {
        let host = if self.ip.contains(':') && !self.ip.starts_with('[') {
            format!("[{}]", self.ip)
        } else {
            self.ip.clone()
        };
        if self.username.is_empty() && self.password.is_empty() {
            format!("socks5://{host}:{}", self.port)
        } else {
            format!(
                "socks5://{}:{}@{}:{}",
                pct_encode(&self.username),
                pct_encode(&self.password),
                host,
                self.port
            )
        }
    }
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub session: String,
    pub device_model: String,
    pub proxy: Option<Proxy>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    api_id: Option<serde_json::Value>,
    #[serde(default)]
    api_hash: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    app_name: String,
    proxy: Option<RawProxy>,
}

#[derive(Debug, Deserialize)]
struct RawProxy {
    #[serde(rename = "socksType")]
    socks_type: Option<serde_json::Value>,
    #[serde(default)]
    ip: String,
    port: Option<serde_json::Value>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

fn json_int(val: Option<&serde_json::Value>) -> Option<i64> {
    match val? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

impl RawProxy {
    fn parse(self) -> Result<Proxy, ConfigError> {
        let socks_type = json_int(self.socks_type.as_ref());
        if socks_type != Some(5) {
            return Err(ConfigError::Message(
                "config.json: only proxy socksType 5 is supported".into(),
            ));
        }
        let port = json_int(self.port.as_ref()).filter(|&p| (1..=65535).contains(&p));
        let Some(port) = port else {
            return Err(ConfigError::Message(
                "config.json: proxy port must be between 1 and 65535".into(),
            ));
        };
        if self.ip.is_empty() {
            return Err(ConfigError::Message("config.json: proxy ip is required".into()));
        }
        Ok(Proxy {
            ip: self.ip,
            port: port as u16,
            username: self.username,
            password: self.password,
        })
    }
}

pub fn parse_proxy(raw: &[u8]) -> Result<Option<Proxy>, ConfigError> {
    #[derive(Deserialize)]
    struct Wrapper {
        proxy: Option<RawProxy>,
    }
    let parsed: Wrapper = serde_json::from_slice(raw)?;
    parsed.proxy.map(RawProxy::parse).transpose()
}

/// Parses a JSON proxy specification either wrapped (`{"proxy":{...}}`) or bare (`{"socksType":5,...}`).
pub fn parse_proxy_value(raw: &[u8]) -> Result<Option<Proxy>, ConfigError> {
    let trimmed = std::str::from_utf8(raw)
        .map_err(|_| ConfigError::Message("proxy JSON must be UTF-8".into()))?
        .trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(None);
    }
    let val: serde_json::Value = serde_json::from_str(trimmed)?;
    if let Some(inner) = val.get("proxy") {
        if inner.is_null() {
            return Ok(None);
        }
        let rp: RawProxy = serde_json::from_value(inner.clone())?;
        return rp.parse().map(Some);
    }
    let rp: RawProxy = serde_json::from_value(val)?;
    rp.parse().map(Some)
}

pub fn parse_partial(raw: &[u8]) -> Result<Config, ConfigError> {
    let mut de = serde_json::Deserializer::from_slice(raw);
    let parsed = RawConfig::deserialize(&mut de)?;
    if de.end().is_err() {
        return Err(ConfigError::Message("config.json has trailing content".into()));
    }
    let api_id = match parsed.api_id.as_ref() {
        None => 0i32,
        Some(v) => {
            let id = json_int(Some(v)).filter(|&id| id >= 0).ok_or_else(|| {
                ConfigError::Message("config.json: api_id must be a positive integer".into())
            })?;
            i32::try_from(id).map_err(|_| {
                ConfigError::Message("config.json: api_id must be a positive integer".into())
            })?
        }
    };
    let device_model = if parsed.app_name.trim().is_empty() {
        DEFAULT_DEVICE_MODEL.to_owned()
    } else {
        parsed.app_name
    };
    let proxy = parsed.proxy.map(RawProxy::parse).transpose()?;
    Ok(Config {
        api_id,
        api_hash: parsed.api_hash,
        session: parsed.session,
        device_model,
        proxy,
    })
}

pub fn parse(raw: &[u8]) -> Result<Config, ConfigError> {
    let cfg = parse_partial(raw)?;
    if cfg.api_id <= 0 {
        return Err(ConfigError::Message(
            "config.json: api_id must be a positive integer".into(),
        ));
    }
    if cfg.api_hash.trim().is_empty() {
        return Err(ConfigError::Message("config.json: api_hash is required".into()));
    }
    if cfg.session.trim().is_empty() {
        return Err(ConfigError::Message(
            "config.json: session is required (run --login first)".into(),
        ));
    }
    Ok(cfg)
}

#[derive(serde::Serialize)]
struct EncodeProxy<'a> {
    #[serde(rename = "socksType")]
    socks_type: u8,
    ip: &'a str,
    port: u16,
    #[serde(skip_serializing_if = "str::is_empty")]
    username: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    password: &'a str,
}

#[derive(serde::Serialize)]
struct EncodeConfig<'a> {
    api_id: i32,
    api_hash: &'a str,
    session: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    app_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy: Option<EncodeProxy<'a>>,
}

/// Encodes `Config` back into `config.json` format with trailing newline.
pub fn to_json_bytes(cfg: &Config) -> Result<Vec<u8>, ConfigError> {
    let app_name = if cfg.device_model.trim() == DEFAULT_DEVICE_MODEL {
        ""
    } else {
        cfg.device_model.trim()
    };
    let doc = EncodeConfig {
        api_id: cfg.api_id,
        api_hash: &cfg.api_hash,
        session: &cfg.session,
        app_name,
        proxy: cfg.proxy.as_ref().map(|p| EncodeProxy {
            socks_type: 5,
            ip: &p.ip,
            port: p.port,
            username: &p.username,
            password: &p.password,
        }),
    };
    let mut out = serde_json::to_vec_pretty(&doc)?;
    out.push(b'\n');
    Ok(out)
}

pub fn read_partial(root: &Path) -> Result<Option<Config>, ConfigError> {
    let path = root.join(CONFIG_FILE);
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ConfigError::Io(e)),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES as u64 {
        return Err(ConfigError::Message(
            "config.json must be a regular file within the size limit".into(),
        ));
    }
    let mut raw = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64).read_to_end(&mut raw)?;
    if raw.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::Message(
            "config.json must be a regular file within the size limit".into(),
        ));
    }
    parse_partial(&raw).map(Some)
}

pub fn read(root: &Path) -> Result<Config, ConfigError> {
    let path = root.join(CONFIG_FILE);
    let file = fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES as u64 {
        return Err(ConfigError::Message(
            "config.json must be a regular file within the size limit".into(),
        ));
    }
    let mut raw = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64).read_to_end(&mut raw)?;
    if raw.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::Message(
            "config.json must be a regular file within the size limit".into(),
        ));
    }
    parse(&raw)
}

#[derive(Debug, Clone, Default)]
pub struct Env(pub HashMap<String, String>);

impl Env {
    pub fn prefixes(&self) -> Vec<String> {
        if let Some(val) = self.0.get("MIBOT_PREFIX") {
            let parts: Vec<String> = val.split_whitespace().map(str::to_owned).collect();
            if !parts.is_empty() {
                return parts;
            }
        }
        vec![".".into(), "。".into(), "$".into()]
    }

    pub fn get<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        match self.0.get(key) {
            Some(v) if !v.trim().is_empty() => v.trim(),
            _ => fallback,
        }
    }
}

pub fn read_env<'a, I>(root: &Path, environ: I) -> Env
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut env = HashMap::new();
    if let Ok(content) = fs::read_to_string(root.join(ENV_FILE)) {
        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let key = k.trim();
            let mut val = v.trim();
            if val.len() >= 2
                && ((val.starts_with('"') && val.ends_with('"'))
                    || (val.starts_with('\'') && val.ends_with('\'')))
            {
                val = &val[1..val.len() - 1];
            }
            env.insert(key.to_owned(), val.to_owned());
        }
    }
    for (k, v) in environ {
        if k.starts_with("MIBOT_") {
            env.insert(k.to_owned(), v.to_owned());
        }
    }
    Env(env)
}

pub fn set_env(root: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let path = root.join(ENV_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut lines: Vec<String> = if raw.is_empty() {
        Vec::new()
    } else {
        raw.strip_suffix('\n')
            .unwrap_or(&raw)
            .split('\n')
            .map(str::to_owned)
            .collect()
    };
    let mut replaced = false;
    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = trimmed.split_once('=')
            && name.trim() == key
        {
            *line = format!("{key}={value}");
            replaced = true;
        }
    }
    if !replaced {
        lines.push(format!("{key}={value}"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    mibot_fsutil::write_file_atomic(&path, out.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mibot-config-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_accepts_mibox_config() {
        let cfg = parse(br#"{"api_id": 1234567, "api_hash": "abc", "session": "1xyz"}"#).unwrap();
        assert_eq!(cfg.api_id, 1234567);
        assert_eq!(cfg.api_hash, "abc");
        assert_eq!(cfg.session, "1xyz");
        assert_eq!(cfg.device_model, DEFAULT_DEVICE_MODEL);
    }

    #[test]
    fn parse_rejects_incomplete() {
        for raw in [
            br#"{"api_hash":"a","session":"1x"}"#.as_slice(),
            br#"{"api_id":1,"session":"1x"}"#.as_slice(),
            br#"{"api_id":1,"api_hash":"a"}"#.as_slice(),
            br#"{"api_id":1,"api_hash":"a","session":"1x"} junk"#.as_slice(),
        ] {
            assert!(parse(raw).is_err());
        }
    }

    #[test]
    fn parse_proxy_validates_socks5() {
        let cfg = parse(
            br#"{"api_id":1,"api_hash":"a","session":"1x","proxy":{"socksType":5,"ip":"1.2.3.4","port":1080,"username":"u","password":"p"}}"#,
        )
        .unwrap();
        let p = cfg.proxy.unwrap();
        assert_eq!(p.ip, "1.2.3.4");
        assert_eq!(p.port, 1080);
        assert_eq!(p.username, "u");
        assert!(
            parse(br#"{"api_id":1,"api_hash":"a","session":"1x","proxy":{"socksType":4,"ip":"1.2.3.4","port":1080}}"#)
                .is_err()
        );
    }

    #[test]
    fn env_file_and_prefixes() {
        let root = temp_dir();
        fs::write(
            root.join(".env"),
            "# comment\nMIBOT_PREFIX=! ！\nMIBOT_SERVICE=\"custom.service\"\n\nBROKEN\n",
        )
        .unwrap();
        let env = read_env(
            &root,
            [("MIBOT_UPDATE_REPO", "owner/repo"), ("PATH", "/bin")],
        );
        assert_eq!(env.prefixes(), vec!["!", "！"]);
        assert_eq!(env.get("MIBOT_SERVICE", ""), "custom.service");
        assert_eq!(env.get("MIBOT_UPDATE_REPO", ""), "owner/repo");
        assert_eq!(env.get("PATH", "unset"), "unset");

        let empty_dir = temp_dir();
        assert_eq!(read_env(&empty_dir, []).prefixes(), vec![".", "。", "$"]);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&empty_dir);
    }

    #[test]
    fn set_env_changes_only_that_line_and_appends() {
        let root = temp_dir();
        let original = "# 部署设置\nMIBOT_SERVICE=\"custom.service\"\n\nMIBOT_PREFIX=. 。\nOTHER=1\n\n";
        fs::write(root.join(".env"), original).unwrap();
        set_env(&root, "MIBOT_PREFIX", "! ！ ~").unwrap();
        let raw = fs::read_to_string(root.join(".env")).unwrap();
        assert_eq!(
            raw,
            "# 部署设置\nMIBOT_SERVICE=\"custom.service\"\n\nMIBOT_PREFIX=! ！ ~\nOTHER=1\n\n"
        );
        assert_eq!(read_env(&root, []).prefixes(), vec!["!", "！", "~"]);
        let perm = fs::metadata(root.join(".env")).unwrap().permissions().mode() & 0o777;
        assert_eq!(perm, 0o600);

        let root2 = temp_dir();
        set_env(&root2, "MIBOT_PREFIX", "!").unwrap();
        assert_eq!(fs::read_to_string(root2.join(".env")).unwrap(), "MIBOT_PREFIX=!\n");
        fs::write(root2.join(".env"), "OTHER=1").unwrap();
        set_env(&root2, "MIBOT_PREFIX", "$").unwrap();
        assert_eq!(
            fs::read_to_string(root2.join(".env")).unwrap(),
            "OTHER=1\nMIBOT_PREFIX=$\n"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&root2);
    }
}
