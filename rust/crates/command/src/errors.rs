use mibot_bot::BotError;
use mibot_httpx::HttpxError;
use regex::Regex;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, OnceLock};

pub type CmdError = Box<dyn Error + Send + Sync>;
pub type CmdResult<T = ()> = Result<T, CmdError>;

/// User-facing error whose brief text is safe to display verbatim in Telegram chat.
#[derive(Debug, Clone)]
pub struct UserError {
    pub text: String,
    pub cause: Option<Arc<dyn Error + Send + Sync>>,
}

impl UserError {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cause: None,
        }
    }

    pub fn with_cause<E>(text: impl Into<String>, cause: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            text: text.into(),
            cause: Some(Arc::new(cause)),
        }
    }
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl Error for UserError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause
            .as_deref()
            .map(|e| e as &(dyn Error + 'static))
    }
}

pub fn fail(msg: impl Into<String>) -> CmdError {
    Box::new(UserError::new(msg))
}

pub fn fail_with<E>(msg: impl Into<String>, cause: E) -> CmdError
where
    E: Error + Send + Sync + 'static,
{
    Box::new(UserError::with_cause(msg, cause))
}

pub fn is_user_error(err: &(dyn Error + 'static)) -> bool {
    find_in_chain::<UserError>(err).is_some()
}

pub fn find_in_chain<'a, T: Error + 'static>(mut err: &'a (dyn Error + 'static)) -> Option<&'a T> {
    loop {
        if let Some(found) = err.downcast_ref::<T>() {
            return Some(found);
        }
        err = err.source()?;
    }
}

/// Truncates `text` to at most `max_chars` Unicode scalar values, appending `'…'` when shortened.
pub fn truncate(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return if text.is_empty() {
            String::new()
        } else {
            "…".into()
        };
    }
    let mut chars = text.char_indices();
    if let Some((idx, _)) = chars.nth(max_chars) {
        let mut out = text[..idx].to_owned();
        out.push('…');
        out
    } else {
        text.to_owned()
    }
}

fn rpc_code_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // INFALLIBLE: compile-time constant regex pattern.
        Regex::new(r"\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b").unwrap()
    })
}

/// Converts an internal/user error into a redacted short summary safe for Telegram chat output.
pub fn brief(err: &(dyn Error + 'static)) -> String {
    if let Some(u) = find_in_chain::<UserError>(err) {
        return truncate(&u.text, 80);
    }

    if let Some(bot_err) = find_in_chain::<BotError>(err) {
        if let Some(code) = bot_err.rpc_code() {
            return code;
        }
        if bot_err.is_unaddressable_peer() {
            return "无法定位目标对话".into();
        }
        if matches!(bot_err, BotError::Cancelled) {
            return "超时".into();
        }
    }

    if let Some(inv) = find_in_chain::<grammers_mtsender::InvocationError>(err)
        && let grammers_mtsender::InvocationError::Rpc(rpc) = inv
    {
        return rpc.name.clone();
    }

    if find_in_chain::<tokio::time::error::Elapsed>(err).is_some() {
        return "超时".into();
    }

    if let Some(http_err) = find_in_chain::<HttpxError>(err) {
        if http_err.is_timeout() {
            return "超时".into();
        }
        return mibot_httpx::reason(http_err);
    }

    if let Some(io_err) = find_in_chain::<std::io::Error>(err)
        && io_err.kind() == std::io::ErrorKind::TimedOut
    {
        return "超时".into();
    }

    let raw = err.to_string();
    if !raw.contains("://")
        && let Some(m) = rpc_code_regex().find(&raw)
    {
        return m.as_str().to_owned();
    }

    "内部错误，详情见日志".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::{InvocationError, RpcError};
    use std::io;

    #[test]
    fn brief_redacts_internal_errors_and_preserves_safe_codes() {
        let u = fail("用法错误：缺少参数");
        assert!(is_user_error(u.as_ref()));
        assert_eq!(brief(u.as_ref()), "用法错误：缺少参数");

        let long_user = fail("字".repeat(100));
        let b = brief(long_user.as_ref());
        assert_eq!(b.chars().count(), 81);
        assert!(b.ends_with('…'));

        let cause = io::Error::other("secret /var/lib/mibot/token");
        let wrapped = fail_with("请求失败", cause);
        assert_eq!(brief(wrapped.as_ref()), "请求失败");
        assert!(wrapped.source().is_some());

        let peer_err: CmdError = Box::new(BotError::UnaddressablePeer("123".into()));
        assert_eq!(brief(peer_err.as_ref()), "无法定位目标对话");

        let rpc_err: CmdError = Box::new(BotError::Invocation(InvocationError::Rpc(RpcError {
            code: 400,
            name: "CHANNEL_PRIVATE".into(),
            value: None,
            caused_by: None,
        })));
        assert_eq!(brief(rpc_err.as_ref()), "CHANNEL_PRIVATE");

        let timeout_err: CmdError = Box::new(BotError::Cancelled);
        assert_eq!(brief(timeout_err.as_ref()), "超时");

        let url_with_upper = io::Error::other("https://user:secret_KEY@10.0.0.1/FOO_BAR failed");
        assert_eq!(brief(&url_with_upper), "内部错误，详情见日志");

        let plain_code = io::Error::other("rpc returned FLOOD_WAIT_30");
        assert_eq!(brief(&plain_code), "FLOOD_WAIT_30");
    }
}
