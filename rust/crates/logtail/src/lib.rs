use chrono::Local;
use regex::Regex;
use std::collections::VecDeque;
use std::net::Ipv6Addr;
use std::str::FromStr;
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub const LINES: usize = 300;
pub const LINE_LIMIT: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = -4,
    Info = 0,
    Warn = 4,
    Error = 8,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_uppercase().as_str() {
            "DEBUG" => Some(Level::Debug),
            "INFO" => Some(Level::Info),
            "WARN" | "WARNING" => Some(Level::Warn),
            "ERROR" | "ERR" => Some(Level::Error),
            _ => None,
        }
    }
}

fn is_identifying(key: &str) -> bool {
    matches!(
        key,
        "account"
            | "chat"
            | "chat_id"
            | "channel"
            | "channel_id"
            | "peer"
            | "sender"
            | "user"
            | "user_id"
            | "from"
            | "to"
            | "target"
    )
}

fn is_carrying(key: &str) -> bool {
    matches!(
        key,
        "text"
            | "query"
            | "prompt"
            | "caption"
            | "title"
            | "username"
            | "phone"
            | "key"
            | "api_key"
            | "token"
            | "secret"
            | "password"
            | "url"
            | "link"
            | "path"
            | "file"
    )
}

struct Patterns {
    addresses: Regex,
    ipv6: Regex,
    long_ids: Regex,
    tokens: Regex,
    urls: Regex,
}

fn patterns() -> &'static Patterns {
    static PATS: OnceLock<Patterns> = OnceLock::new();
    PATS.get_or_init(|| Patterns {
        addresses: Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}\b").expect("valid ipv4 regex"),
        ipv6: Regex::new(r"(?i)(?:[a-f\d]{0,4}:){2,}[a-f\d:.]*(?:%[\w.-]+)?")
            .expect("valid ipv6 regex"),
        long_ids: Regex::new(r"\b\d{9,}\b").expect("valid long_ids regex"),
        tokens: Regex::new(r"\b(?:sk|xox[a-z]|ghp|gho|Bearer)[-_ ][A-Za-z0-9_.\-]{8,}")
            .expect("valid tokens regex"),
        urls: Regex::new(r#"(?i)https?://[^\s"<>]+"#).expect("valid urls regex"),
    })
}

/// Scrubs URLs, API tokens, IPv4/IPv6 addresses, and >=9-digit IDs from free text.
pub fn scrub(value: &str) -> String {
    let p = patterns();
    let v = p.urls.replace_all(value, |caps: &regex::Captures| {
        let link = &caps[0];
        match url::Url::parse(link) {
            Ok(parsed) if parsed.has_host() => {
                let host = parsed.host_str().unwrap_or("");
                match parsed.port() {
                    Some(port) => format!("{}://{host}:{port}/…", parsed.scheme()),
                    None => format!("{}://{host}/…", parsed.scheme()),
                }
            }
            _ => "[url]".to_owned(),
        }
    });
    let v = p.tokens.replace_all(&v, "[token]");
    let v = p.ipv6.replace_all(&v, |caps: &regex::Captures| {
        let m = &caps[0];
        let trimmed = m.trim_end_matches('.');
        let core = trimmed.split('%').next().unwrap_or(trimmed);
        if Ipv6Addr::from_str(core).is_ok() {
            format!("[ip]{}", &m[trimmed.len()..])
        } else {
            m.to_owned()
        }
    });
    let v = p.addresses.replace_all(&v, "[ip]");
    p.long_ids.replace_all(&v, "[id]").into_owned()
}

/// Computes a stable short FNV-1a digest (`#xxxx`) for a chat/user identifier.
pub fn digest(value: &str) -> String {
    if value.is_empty() {
        return "#none".to_owned();
    }
    let mut hash: u32 = 0x811c_9dc5;
    for b in value.as_bytes() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("#{:04x}", hash & 0xffff)
}

pub fn redact(key: &str, value: &str) -> String {
    if is_identifying(key) {
        digest(value)
    } else if is_carrying(key) {
        "[redacted]".to_owned()
    } else {
        scrub(value)
    }
}

/// Fixed-capacity in-memory ring buffer storing scrubbed log lines.
#[derive(Debug, Clone)]
pub struct Ring {
    inner: Arc<Mutex<VecDeque<String>>>,
    capacity: usize,
    min_level: Arc<AtomicI8>,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(LINES))),
            capacity: LINES,
            min_level: Arc::new(AtomicI8::new(Level::Debug as i8)),
        }
    }

    pub fn set_level(&self, level: Level) {
        self.min_level.store(level as i8, Ordering::Relaxed);
    }

    pub fn level(&self) -> Level {
        match self.min_level.load(Ordering::Relaxed) {
            x if x <= Level::Debug as i8 => Level::Debug,
            x if x <= Level::Info as i8 => Level::Info,
            x if x <= Level::Warn as i8 => Level::Warn,
            _ => Level::Error,
        }
    }

    pub fn add(&self, line: &str) {
        let mut owned = if line.chars().count() > LINE_LIMIT {
            let mut s: String = line.chars().take(LINE_LIMIT).collect();
            s.push('…');
            s
        } else {
            line.to_owned()
        };
        if owned.ends_with('\n') {
            owned.pop();
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() == self.capacity {
            guard.pop_front();
        }
        guard.push_back(owned);
    }

    pub fn record(&self, level: Level, msg: &str, attrs: &[(&str, &str)]) {
        if (level as i8) < self.min_level.load(Ordering::Relaxed) {
            return;
        }
        let stamp = Local::now().format("%H:%M:%S");
        let mut line = format!("{stamp} {} {}", level.as_str(), scrub(msg));
        for (k, v) in attrs {
            line.push(' ');
            line.push_str(k);
            line.push('=');
            line.push_str(&redact(k, v));
        }
        self.add(&line);
    }

    pub fn tail<F>(&self, count: usize, keep: Option<F>) -> Vec<String>
    where
        F: Fn(&str) -> bool,
    {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut matched: Vec<String> = guard
            .iter()
            .filter(|line| match &keep {
                Some(pred) => pred(line),
                None => true,
            })
            .cloned()
            .collect();
        if count > 0 && matched.len() > count {
            matched.drain(0..matched.len() - count);
        }
        matched
    }

    pub fn held(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

pub fn at_least(line: &str, floor: Level) -> bool {
    let mut fields = line.split_whitespace();
    let _stamp = fields.next();
    let Some(level_str) = fields.next() else {
        return false;
    };
    Level::parse(level_str).is_some_and(|lvl| lvl >= floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_sensitive_reaches_the_ring() {
        let ring = Ring::new();
        ring.record(
            Level::Info,
            "dispatch.dropped from 2001:db8::1",
            &[
                ("account", "215959394"),
                ("chat", "-1001771725356"),
                ("text", "我的银行卡密码是 123456"),
                ("error", "dial [2001:db8::2]:443 via 43.153.150.179: refused, see https://alice:private@example.net?token=direct-secret and https://api.example.com/v1/x?key=sk-abcd1234efgh"),
            ],
        );
        let whole = ring.tail::<fn(&str) -> bool>(0, None).join("\n");
        for secret in [
            "215959394",
            "-1001771725356",
            "银行卡",
            "123456",
            "43.153.150.179",
            "2001:db8::",
            "alice:private",
            "direct-secret",
            "sk-abcd1234efgh",
            "/v1/x",
        ] {
            assert!(!whole.contains(secret), "ring leaked {secret:?}:\n{whole}");
        }
        for wanted in ["dispatch.dropped", "refused", "https://api.example.com"] {
            assert!(whole.contains(wanted), "ring lost {wanted:?}:\n{whole}");
        }
    }

    #[test]
    fn digest_is_stable_and_distinct() {
        assert_eq!(digest("-100123"), digest("-100123"));
        assert_ne!(digest("-100123"), digest("-100124"));
        assert_eq!(digest(""), "#none");
    }

    #[test]
    fn scrub_keeps_diagnosis() {
        for (input, want) in [
            ("ookla failed: signal: aborted", "ookla failed: signal: aborted"),
            ("took 15.490826543s", "took 15.490826543s"),
            ("connect to 10.0.0.1 failed", "connect to [ip] failed"),
            ("https://speedtest.net/result/c/abcd", "https://speedtest.net/…"),
            ("https://example.com?token=secret", "https://example.com/…"),
            ("https://example.com#secret", "https://example.com/…"),
            ("http://[2001:db8::1]:8080?q=x", "http://[[ip]]:8080/…"),
            ("dial [fe80::1%en0]:443", "dial [[ip]]:443"),
        ] {
            assert_eq!(scrub(input), want, "input={input}");
        }
    }

    #[test]
    fn ring_wraps_oldest_first_and_tails_from_newest() {
        let ring = Ring::new();
        for i in 0..(LINES + 5) {
            let ch = char::from_u32(('a' as u32) + (i % 26) as u32).unwrap();
            ring.add(&format!("xxx{ch}"));
        }
        let lines = ring.tail::<fn(&str) -> bool>(0, None);
        assert_eq!(lines.len(), LINES);
        assert_eq!(ring.held(), LINES);
        let newest_ch = char::from_u32(('a' as u32) + ((LINES + 4) % 26) as u32).unwrap();
        assert_eq!(lines.last().unwrap(), &format!("xxx{newest_ch}"));

        let small = Ring::new();
        for l in ["one", "two", "three"] {
            small.add(l);
        }
        assert_eq!(
            small.tail::<fn(&str) -> bool>(2, None),
            vec!["two".to_owned(), "three".to_owned()]
        );
    }

    #[test]
    fn at_least_reads_level_and_caps_lines() {
        let ring = Ring::new();
        ring.record(Level::Info, "quiet", &[]);
        ring.record(Level::Error, "loud", &[]);
        let only = ring.tail(0, Some(|line: &str| at_least(line, Level::Error)));
        assert_eq!(only.len(), 1);
        assert!(only[0].contains("loud"));
        assert!(!at_least("", Level::Error));
        assert!(!at_least("garbage line", Level::Error));

        let capped = Ring::new();
        capped.add(&"字".repeat(LINE_LIMIT * 3));
        let line = &capped.tail::<fn(&str) -> bool>(0, None)[0];
        assert!(line.chars().count() <= LINE_LIMIT + 1);
    }
}
