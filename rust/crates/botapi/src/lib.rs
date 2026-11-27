use mibot_delivery::UncertainError;
use mibot_httpx::{HttpxError, Request, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};

#[derive(Debug, Error)]
pub enum BotApiError {
    #[error("Bot API 推送排队已满")]
    QueueFull,
    #[error("Bot API 返回 {code}：{description}")]
    Api {
        code: i64,
        description: String,
        retry_after: Option<Duration>,
    },
    #[error("Bot API: {0}")]
    Transport(String),
    #[error("{0}")]
    Uncertain(#[source] UncertainError),
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep(&self, dur: Duration) -> BoxFut<'static, ()>;
}

pub trait Poster: Send + Sync {
    fn post_json<'a>(
        &'a self,
        url: String,
        body: Vec<u8>,
        timeout: Duration,
        limit: usize,
    ) -> BoxFut<'a, Result<Response, HttpxError>>;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, dur: Duration) -> BoxFut<'static, ()> {
        Box::pin(tokio::time::sleep(dur))
    }
}

struct ReqwestPoster;

impl Poster for ReqwestPoster {
    fn post_json<'a>(
        &'a self,
        url: String,
        body: Vec<u8>,
        timeout: Duration,
        limit: usize,
    ) -> BoxFut<'a, Result<Response, HttpxError>> {
        Box::pin(async move {
            let mut headers = HashMap::new();
            headers.insert("Content-Type".into(), "application/json".into());
            mibot_httpx::do_request(Request {
                method: mibot_httpx::Method::POST,
                url,
                headers,
                body: Some(body),
                no_redirect: true,
                wait_for_headers: false,
                timeout: Some(timeout),
                max_bytes: Some(limit),
            })
            .await
        })
    }
}

#[derive(Default)]
struct Budget {
    global: Option<Instant>,
    cooldown: Option<Instant>,
    chats: HashMap<String, Instant>,
}

pub struct Client {
    budgets: Mutex<HashMap<[u8; 32], Budget>>,
    slots: Semaphore,
    clock: Arc<dyn Clock>,
    poster: Arc<dyn Poster>,
    jitter_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self::with_backends(
            Arc::new(SystemClock),
            Arc::new(ReqwestPoster),
            Arc::new(|| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0);
                u64::from(nanos % 250)
            }),
        )
    }

    pub fn with_backends(
        clock: Arc<dyn Clock>,
        poster: Arc<dyn Poster>,
        jitter_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            budgets: Mutex::new(HashMap::new()),
            slots: Semaphore::new(32),
            clock,
            poster,
            jitter_ms,
        }
    }

    async fn wait(&self, key: [u8; 32], chat: &str) {
        loop {
            let wait_dur = {
                let mut map = self.budgets.lock().await;
                let now = self.clock.now();
                map.retain(|_, b| {
                    b.chats.retain(|_, until| *until > now);
                    !b.chats.is_empty()
                        || b.global.is_some_and(|u| u > now)
                        || b.cooldown.is_some_and(|u| u > now)
                });
                let b = map.entry(key).or_default();
                let mut until = now;
                if let Some(g) = b.global && g > until {
                    until = g;
                }
                if let Some(&c) = b.chats.get(chat) && c > until {
                    until = c;
                }
                if let Some(cd) = b.cooldown && cd > until {
                    until = cd;
                }
                if until <= now {
                    b.global = Some(now + Duration::from_nanos(1_000_000_000_u64.div_ceil(30)));
                    let interval = if chat.starts_with('-') || chat.starts_with('@') {
                        Duration::from_secs(3)
                    } else {
                        Duration::from_secs(1)
                    };
                    b.chats.insert(chat.to_owned(), now + interval);
                    None
                } else {
                    Some(until.duration_since(now))
                }
            };
            match wait_dur {
                None => return,
                Some(d) => self.clock.sleep(d).await,
            }
        }
    }

    async fn pause(&self, key: [u8; 32], dur: Duration) {
        let mut map = self.budgets.lock().await;
        let b = map.entry(key).or_default();
        let until = self.clock.now() + dur;
        if b.cooldown.is_none_or(|cur| until > cur) {
            b.cooldown = Some(until);
        }
    }

    pub async fn send_html(
        &self,
        endpoint: &str,
        token: &str,
        chat: &str,
        text: &str,
    ) -> Result<(), BotApiError> {
        let _permit = self.slots.try_acquire().map_err(|_| BotApiError::QueueFull)?;

        let mut hasher = Sha256::new();
        hasher.update(endpoint.as_bytes());
        hasher.update(b"\x00");
        hasher.update(token.as_bytes());
        let key: [u8; 32] = hasher.finalize().into();

        #[derive(Serialize)]
        struct LinkPreviewOptions {
            is_disabled: bool,
        }
        #[derive(Serialize)]
        struct Payload<'a> {
            chat_id: &'a str,
            text: &'a str,
            parse_mode: &'a str,
            link_preview_options: LinkPreviewOptions,
        }
        #[derive(Deserialize, Default)]
        struct EnvelopeParams {
            #[serde(default)]
            retry_after: i64,
        }
        #[derive(Deserialize)]
        struct Envelope {
            ok: Option<bool>,
            #[serde(default)]
            error_code: i64,
            #[serde(default)]
            description: String,
            #[serde(default)]
            parameters: EnvelopeParams,
        }

        let body = serde_json::to_vec(&Payload {
            chat_id: chat,
            text,
            parse_mode: "HTML",
            link_preview_options: LinkPreviewOptions { is_disabled: true },
        })
        .map_err(|e| BotApiError::Transport(e.to_string()))?;

        let target = format!("{}/bot{token}/sendMessage", endpoint.trim_end_matches('/'));

        for attempt in 0usize.. {
            self.wait(key, chat).await;
            let resp = match self
                .poster
                .post_json(target.clone(), body.clone(), Duration::from_secs(15), 1 << 20)
                .await
            {
                Ok(r) => r,
                Err(err) => {
                    let msg = mibot_httpx::reason(&err);
                    let transport = BotApiError::Transport(msg);
                    if err.is_unsent() {
                        return Err(transport);
                    }
                    return Err(BotApiError::Uncertain(UncertainError::new(transport)));
                }
            };

            let envelope: Envelope = match serde_json::from_slice(&resp.body) {
                Ok(e) => e,
                Err(_) => {
                    let api_err = BotApiError::Api {
                        code: i64::from(resp.status),
                        description: "响应不是 JSON".into(),
                        retry_after: None,
                    };
                    if resp.status >= 500 || resp.ok() {
                        return Err(BotApiError::Uncertain(UncertainError::new(api_err)));
                    }
                    return Err(api_err);
                }
            };

            if resp.ok() {
                match envelope.ok {
                    None => {
                        return Err(BotApiError::Uncertain(UncertainError::new(
                            BotApiError::Api {
                                code: i64::from(resp.status),
                                description: "响应缺少 ok 字段".into(),
                                retry_after: None,
                            },
                        )));
                    }
                    Some(true) => return Ok(()),
                    Some(false) => {}
                }
            }

            let code = if envelope.error_code == 0 {
                i64::from(resp.status)
            } else {
                envelope.error_code
            };
            let description = envelope.description.replace(token, "[redacted]");
            if code >= 500 {
                return Err(BotApiError::Uncertain(UncertainError::new(
                    BotApiError::Api {
                        code,
                        description,
                        retry_after: None,
                    },
                )));
            }
            if code != 429 {
                return Err(BotApiError::Api {
                    code,
                    description,
                    retry_after: None,
                });
            }

            let seconds = envelope.parameters.retry_after;
            if !(0..=86400).contains(&seconds) {
                return Err(BotApiError::Api {
                    code,
                    description,
                    retry_after: None,
                });
            }
            let retry_after = Duration::from_secs(seconds as u64);
            let delay = Duration::from_secs(1u64 << attempt.min(3));
            let wait = retry_after.max(delay) + Duration::from_millis((self.jitter_ms)());
            self.pause(key, wait).await;
            if attempt >= 3 || retry_after > Duration::from_secs(60) {
                return Err(BotApiError::Api {
                    code,
                    description,
                    retry_after: Some(retry_after),
                });
            }
        }
        // INFALLIBLE: loop only exits via explicit return branches above.
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct VirtualClock {
        now: StdMutex<Instant>,
        elapsed: StdMutex<Duration>,
    }

    impl VirtualClock {
        fn new() -> Self {
            Self {
                now: StdMutex::new(Instant::now()),
                elapsed: StdMutex::new(Duration::ZERO),
            }
        }

        fn elapsed(&self) -> Duration {
            *self.elapsed.lock().unwrap()
        }
    }

    impl Clock for VirtualClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, dur: Duration) -> BoxFut<'static, ()> {
            *self.now.lock().unwrap() += dur;
            *self.elapsed.lock().unwrap() += dur;
            Box::pin(async {})
        }
    }

    struct FnPoster<F>(F);

    impl<F> Poster for FnPoster<F>
    where
        F: Fn(String, Vec<u8>) -> Result<Response, HttpxError> + Send + Sync,
    {
        fn post_json<'a>(
            &'a self,
            url: String,
            body: Vec<u8>,
            _timeout: Duration,
            _limit: usize,
        ) -> BoxFut<'a, Result<Response, HttpxError>> {
            let res = (self.0)(url, body);
            Box::pin(async move { res })
        }
    }

    #[tokio::test]
    async fn payload_and_redaction_and_ok_false() {
        let clock = Arc::new(VirtualClock::new());
        let poster = Arc::new(FnPoster(|url: String, body: Vec<u8>| {
            assert!(url.ends_with("/botSECRET/sendMessage"));
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(v.get("disable_web_page_preview").is_none());
            assert_eq!(v["link_preview_options"]["is_disabled"], true);
            Ok(Response {
                status: 200,
                body: br#"{"ok":false,"error_code":400,"description":"bad SECRET"}"#.to_vec(),
            })
        }));
        let c = Client::with_backends(clock, poster, Arc::new(|| 0));
        let err = c
            .send_html("https://example.test", "SECRET", "1", "<b>text</b>")
            .await
            .unwrap_err();
        assert!(matches!(err, BotApiError::Api { code: 400, .. }));
        assert!(!err.to_string().contains("SECRET"));
    }

    #[tokio::test]
    async fn retries_429_after_server_wait() {
        let clock = Arc::new(VirtualClock::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_bg = Arc::clone(&calls);
        let poster = Arc::new(FnPoster(move |_, _| {
            let n = calls_bg.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                Ok(Response {
                    status: 429,
                    body: br#"{"ok":false,"error_code":429,"parameters":{"retry_after":7}}"#
                        .to_vec(),
                })
            } else {
                Ok(Response {
                    status: 200,
                    body: br#"{"ok":true}"#.to_vec(),
                })
            }
        }));
        let c = Client::with_backends(clock.clone(), poster, Arc::new(|| 100));
        c.send_html("https://example.test", "T", "1", "x")
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(clock.elapsed() >= Duration::from_secs(7));
        assert!(clock.elapsed() <= Duration::from_secs(8));
    }

    #[tokio::test]
    async fn chat_group_and_global_budgets() {
        for (chat, next, min_wait) in [
            ("1", "1", Duration::from_secs(1)),
            ("-1001", "-1001", Duration::from_secs(3)),
            ("@group", "@group", Duration::from_secs(3)),
            ("1", "2", Duration::from_nanos(1_000_000_000_u64.div_ceil(30))),
        ] {
            let clock = Arc::new(VirtualClock::new());
            let poster = Arc::new(FnPoster(|_, _| {
                Ok(Response {
                    status: 200,
                    body: br#"{"ok":true}"#.to_vec(),
                })
            }));
            let c = Client::with_backends(clock.clone(), poster, Arc::new(|| 0));
            c.send_html("https://example.test", "T", chat, "x")
                .await
                .unwrap();
            c.send_html("https://example.test", "T", next, "x")
                .await
                .unwrap();
            assert_eq!(clock.elapsed(), min_wait);
            let before = clock.elapsed();
            c.send_html("https://example.test", "OTHER", chat, "x")
                .await
                .unwrap();
            assert_eq!(clock.elapsed(), before);
        }
    }

    #[tokio::test]
    async fn exhaustion_and_long_429_not_retried() {
        for seconds in [0, 61] {
            let clock = Arc::new(VirtualClock::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let calls_bg = Arc::clone(&calls);
            let poster = Arc::new(FnPoster(move |_, _| {
                calls_bg.fetch_add(1, Ordering::SeqCst);
                Ok(Response {
                    status: 429,
                    body: format!(
                        r#"{{"ok":false,"error_code":429,"parameters":{{"retry_after":{seconds}}}}}"#
                    )
                    .into_bytes(),
                })
            }));
            let c = Client::with_backends(clock.clone(), poster, Arc::new(|| 0));
            let err = c
                .send_html("https://example.test", "T", "1", "x")
                .await
                .unwrap_err();
            assert!(matches!(err, BotApiError::Api { code: 429, .. }));
            if seconds == 0 {
                assert_eq!(calls.load(Ordering::SeqCst), 4);
            } else {
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(clock.elapsed(), Duration::ZERO);
            }
        }
    }

    #[tokio::test]
    async fn uncertain_vs_definite_failures() {
        for body in [br#"{"ok":false,"error_code":500}"#.as_slice(), b"{}".as_slice()] {
            let clock = Arc::new(VirtualClock::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let calls_bg = Arc::clone(&calls);
            let body_owned = body.to_vec();
            let poster = Arc::new(FnPoster(move |_, _| {
                calls_bg.fetch_add(1, Ordering::SeqCst);
                Ok(Response {
                    status: if body_owned.len() > 2 { 500 } else { 200 },
                    body: body_owned.clone(),
                })
            }));
            let c = Client::with_backends(clock, poster, Arc::new(|| 0));
            let err = c
                .send_html("https://example.test", "T", "1", "x")
                .await
                .unwrap_err();
            assert!(mibot_delivery::is_uncertain(&err));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        let clock = Arc::new(VirtualClock::new());
        let poster_dial = Arc::new(FnPoster(|_, _| {
            Err(HttpxError::Stage {
                stage: "request",
                is_timeout: false,
                is_connect_or_dns: true,
                message: "connection refused".into(),
            })
        }));
        let c1 = Client::with_backends(clock.clone(), poster_dial, Arc::new(|| 0));
        let err1 = c1
            .send_html("https://example.test", "T", "1", "x")
            .await
            .unwrap_err();
        assert!(!mibot_delivery::is_uncertain(&err1));

        let poster_read = Arc::new(FnPoster(|_, _| {
            Err(HttpxError::Stage {
                stage: "response_body",
                is_timeout: false,
                is_connect_or_dns: false,
                message: "connection reset".into(),
            })
        }));
        let c2 = Client::with_backends(clock, poster_read, Arc::new(|| 0));
        let err2 = c2
            .send_html("https://example.test", "T", "1", "x")
            .await
            .unwrap_err();
        assert!(mibot_delivery::is_uncertain(&err2));
    }
}
