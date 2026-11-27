use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
pub use reqwest::Method;
use reqwest::{Client, StatusCode};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;
use url::Url;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub const USER_AGENT: &str = "MiBot-Lite/1";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_BYTES: usize = 2 << 20;

const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
];

#[derive(Debug, Error)]
pub enum HttpxError {
    #[error("response body exceeds the size limit")]
    TooLarge,
    #[error("refusing credential redirect to another origin")]
    CredentialRedirect,
    #[error("redirect not allowed")]
    CrossHostRedirect,
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("HTTP {0}")]
    Status(u16),
    #[error("request timeout")]
    Timeout,
    #[error("SSRF blocked address: {0}")]
    BlockedAddress(IpAddr),
    #[error("{0}")]
    UserFacing(String),
    #[error("{stage}: {message}")]
    Stage {
        stage: &'static str,
        is_timeout: bool,
        is_connect_or_dns: bool,
        message: String,
    },
    #[error("{0}")]
    Other(String),
}

impl HttpxError {
    pub fn is_timeout(&self) -> bool {
        match self {
            HttpxError::Timeout => true,
            HttpxError::Stage { is_timeout, .. } => *is_timeout,
            _ => false,
        }
    }

    pub fn is_unsent(&self) -> bool {
        match self {
            HttpxError::Stage {
                stage,
                is_connect_or_dns,
                ..
            } => *stage == "request" && *is_connect_or_dns,
            HttpxError::BlockedAddress(_) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<Vec<u8>>,
    pub no_redirect: bool,
    pub wait_for_headers: bool,
    pub timeout: Option<Duration>,
    pub max_bytes: Option<usize>,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            method: Method::GET,
            url: String::new(),
            headers: HashMap::new(),
            body: None,
            no_redirect: false,
            wait_for_headers: false,
            timeout: None,
            max_bytes: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub fn ensure_rustls_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn shared_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        ensure_rustls_provider();
        Client::builder()
            .user_agent(USER_AGENT)
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .expect("build shared reqwest client")
    })
}

fn has_credentials(url: &Url, headers: &HashMap<String, String>) -> bool {
    if url.query_pairs().any(|(k, v)| k == "key" && !v.is_empty()) {
        return true;
    }
    headers.iter().any(|(k, v)| {
        !v.is_empty()
            && CREDENTIAL_HEADERS
                .iter()
                .any(|c| k.eq_ignore_ascii_case(c))
    })
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme().eq_ignore_ascii_case(b.scheme())
        && a.host_str()
            .zip(b.host_str())
            .is_some_and(|(ha, hb)| ha.eq_ignore_ascii_case(hb))
        && a.port_or_known_default() == b.port_or_known_default()
}

fn build_header_map(headers: &HashMap<String, String>) -> Result<HeaderMap, HttpxError> {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| HttpxError::Other(format!("invalid header name: {e}")))?;
        let val = HeaderValue::from_str(v)
            .map_err(|e| HttpxError::Other(format!("invalid header value: {e}")))?;
        map.insert(name, val);
    }
    Ok(map)
}

pub async fn do_request(request: Request) -> Result<Response, HttpxError> {
    let (resp, _) = do_request_with_headers(request).await?;
    Ok(resp)
}

pub async fn do_request_with_headers(
    request: Request,
) -> Result<(Response, HashMap<String, String>), HttpxError> {
    let timeout = request.timeout.filter(|d| !d.is_zero()).unwrap_or(DEFAULT_TIMEOUT);
    let limit = request.max_bytes.filter(|&n| n > 0).unwrap_or(DEFAULT_MAX_BYTES);
    let deadline = Instant::now() + timeout;

    let first_url =
        Url::parse(&request.url).map_err(|e| HttpxError::Other(format!("invalid URL: {e}")))?;
    let credential = has_credentials(&first_url, &request.headers);
    let header_map = build_header_map(&request.headers)?;

    let client = shared_client();
    let mut current_url = first_url.clone();
    let mut current_method = request.method.clone();
    let mut current_body = request.body.clone();

    for hop in 0..=3usize {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(if request.wait_for_headers {
                HttpxError::Stage {
                    stage: "response_headers",
                    is_timeout: true,
                    is_connect_or_dns: false,
                    message: "deadline exceeded".into(),
                }
            } else {
                HttpxError::Timeout
            });
        }
        let header_timeout = if request.wait_for_headers {
            remaining
        } else {
            remaining.min(Duration::from_secs(30))
        };

        let mut builder = client
            .request(current_method.clone(), current_url.clone())
            .headers(header_map.clone())
            .timeout(header_timeout);
        if let Some(body) = &current_body {
            builder = builder.body(body.clone());
        }

        let mut resp = builder.send().await.map_err(|e| {
            let is_timeout = e.is_timeout();
            let is_connect = e.is_connect();
            if request.wait_for_headers {
                HttpxError::Stage {
                    stage: if is_connect { "request" } else { "response_headers" },
                    is_timeout,
                    is_connect_or_dns: is_connect,
                    message: strip_url_error(&e),
                }
            } else if is_timeout {
                HttpxError::Timeout
            } else {
                HttpxError::Stage {
                    stage: "request",
                    is_timeout: false,
                    is_connect_or_dns: is_connect,
                    message: strip_url_error(&e),
                }
            }
        })?;

        let status = resp.status();
        if status.is_redirection() && !request.no_redirect {
            if hop == 3 {
                return Err(HttpxError::TooManyRedirects);
            }
            let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
                return Ok((
                    Response {
                        status: status.as_u16(),
                        body: Vec::new(),
                    },
                    extract_headers(resp.headers()),
                ));
            };
            let loc_str = loc
                .to_str()
                .map_err(|e| HttpxError::Other(format!("invalid redirect location: {e}")))?;
            let next_url = current_url
                .join(loc_str)
                .map_err(|e| HttpxError::Other(format!("invalid redirect URL: {e}")))?;
            if credential && !same_origin(&first_url, &next_url) {
                return Err(HttpxError::CredentialRedirect);
            }
            if matches!(
                status,
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
            ) && current_method != Method::GET
                && current_method != Method::HEAD
            {
                current_method = Method::GET;
                current_body = None;
            }
            current_url = next_url;
            continue;
        }

        let resp_headers = extract_headers(resp.headers());
        let mut body = Vec::new();
        loop {
            let chunk_rem = deadline.saturating_duration_since(Instant::now());
            if chunk_rem.is_zero() {
                return Err(HttpxError::Timeout);
            }
            let chunk = tokio::time::timeout(chunk_rem, resp.chunk())
                .await
                .map_err(|_| HttpxError::Timeout)?
                .map_err(|e| HttpxError::Stage {
                    stage: "response_body",
                    is_timeout: e.is_timeout(),
                    is_connect_or_dns: false,
                    message: strip_url_error(&e),
                })?;
            let Some(bytes) = chunk else { break };
            if body.len() + bytes.len() > limit {
                return Err(HttpxError::TooLarge);
            }
            body.extend_from_slice(&bytes);
        }

        return Ok((
            Response {
                status: status.as_u16(),
                body,
            },
            resp_headers,
        ));
    }

    Err(HttpxError::TooManyRedirects)
}

fn extract_headers(map: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, val) in map {
        if let Ok(s) = val.to_str() {
            out.entry(name.as_str().to_ascii_lowercase())
                .or_insert_with(|| s.to_owned());
        }
    }
    out
}

fn strip_url_error(err: &reqwest::Error) -> String {
    if let Some(src) = err.source() {
        src.to_string()
    } else {
        "http request failed".to_owned()
    }
}

pub async fn get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    timeout: Option<Duration>,
    max_bytes: Option<usize>,
) -> Result<T, HttpxError> {
    let resp = do_request(Request {
        url: url.to_owned(),
        timeout,
        max_bytes,
        ..Default::default()
    })
    .await?;
    if !resp.ok() {
        return Err(HttpxError::Status(resp.status));
    }
    serde_json::from_slice(&resp.body).map_err(|e| HttpxError::Other(format!("invalid JSON: {e}")))
}

pub async fn post_json<P: serde::Serialize>(
    mut request: Request,
    payload: &P,
) -> Result<Response, HttpxError> {
    let encoded = serde_json::to_vec(payload)
        .map_err(|e| HttpxError::Other(format!("encode JSON: {e}")))?;
    request
        .headers
        .entry("Content-Type".into())
        .or_insert_with(|| "application/json".into());
    request.method = Method::POST;
    request.body = Some(encoded);
    do_request(request).await
}

pub fn reason(err: &HttpxError) -> String {
    match err {
        HttpxError::UserFacing(msg) => msg.clone(),
        HttpxError::Status(429) => "请求过于频繁，稍后再试".into(),
        HttpxError::Status(code) => format!("服务返回 HTTP {code}"),
        HttpxError::Timeout => "请求超时，稍后再试".into(),
        HttpxError::TooLarge => "响应数据过大".into(),
        HttpxError::Stage { is_timeout: true, .. } => "请求超时，稍后再试".into(),
        _ => "网络请求失败，稍后再试".into(),
    }
}

pub async fn download_file(url: &str, target: &Path, limit: usize) -> Result<String, HttpxError> {
    let resp = do_request(Request {
        url: url.to_owned(),
        timeout: Some(Duration::from_secs(300)),
        max_bytes: Some(limit),
        ..Default::default()
    })
    .await?;
    if resp.status != 200 {
        return Err(HttpxError::Status(resp.status));
    }
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    opts.mode(0o700);
    let mut file = opts
        .open(target)
        .map_err(|e| HttpxError::Other(e.to_string()))?;
    file.write_all(&resp.body)
        .map_err(|e| HttpxError::Other(e.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(&resp.body);
    Ok(hex::encode(hasher.finalize()))
}

/// Checks if an IP address is internal, loopback, link-local, or otherwise non-global for SSRF guards.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_v4(mapped);
            }
            let segs = v6.segments();
            v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (segs[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (segs[0] & 0xffc0) == 0xfe80 // Link-local fe80::/10
                || (segs[0] == 0x2001 && segs[1] == 0x0db8) // Documentation 2001:db8::/32
                || (segs[0] == 0x0064 && segs[1] == 0xff9b) // NAT64 64:ff9b::/96
        }
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_unspecified()
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64) // CGNAT 100.64.0.0/10
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol 192.0.0.0/24
        || (o[0] == 192 && o[1] == 0 && o[2] == 2) // TEST-NET-1
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // Benchmark 198.18.0.0/15
        || (o[0] == 198 && o[1] == 51 && o[2] == 100) // TEST-NET-2
        || (o[0] == 203 && o[1] == 0 && o[2] == 113) // TEST-NET-3
        || o[0] >= 240 // Reserved
}

/// Custom DNS resolver for SSRF-guarded fetches (`subinfo`).
pub struct SsrfResolver;

impl reqwest::dns::Resolve for SsrfResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Box<dyn StdError + Send + Sync>)?
                .collect();
            for addr in &addrs {
                if is_blocked_ip(addr.ip()) {
                    return Err(Box::new(HttpxError::BlockedAddress(addr.ip()))
                        as Box<dyn StdError + Send + Sync>);
                }
            }
    Ok(Box::new(addrs.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
        })
    }
}

#[derive(Debug, Clone)]
pub struct GuardedResponse {
    pub status: u16,
    pub headers: HashMap<String, Vec<String>>,
    pub body: Vec<u8>,
}

impl GuardedResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(&name.to_ascii_lowercase())
            .and_then(|v| v.first())
            .map(String::as_str)
            .unwrap_or("")
    }

    pub fn header_values(&self, name: &str) -> &[String] {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn ssrf_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        ensure_rustls_provider();
        Client::builder()
            .user_agent(USER_AGENT)
            .redirect(Policy::none())
            .no_proxy()
            .dns_resolver(std::sync::Arc::new(SsrfResolver))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .expect("build ssrf reqwest client")
    })
}

fn check_literal_ip(url: &Url) -> Result<(), HttpxError> {
    if let Some(host) = url.host_str() {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = bare.parse::<IpAddr>()
            && is_blocked_ip(ip)
        {
            return Err(HttpxError::BlockedAddress(ip));
        }
    }
    Ok(())
}

fn extract_blocked_address(err: &reqwest::Error) -> Option<IpAddr> {
    let mut cur: Option<&dyn StdError> = Some(err);
    while let Some(src) = cur {
        if let Some(HttpxError::BlockedAddress(ip)) = src.downcast_ref::<HttpxError>() {
            return Some(*ip);
        }
        cur = src.source();
    }
    None
}

/// Fetches `url` with response headers, enforcing optional SSRF blocking and same-host redirects.
pub async fn do_guarded_get(
    url: &str,
    headers: &HashMap<String, String>,
    timeout: Duration,
    max_bytes: usize,
    guard_ssrf: bool,
    allowed_host: Option<&str>,
    max_redirects: usize,
) -> Result<GuardedResponse, HttpxError> {
    let timeout = if timeout.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        timeout
    };
    let limit = if max_bytes == 0 {
        DEFAULT_MAX_BYTES
    } else {
        max_bytes
    };
    let deadline = Instant::now() + timeout;
    let mut current_url =
        Url::parse(url).map_err(|e| HttpxError::Other(format!("invalid URL: {e}")))?;
    let header_map = build_header_map(headers)?;
    let client = if guard_ssrf {
        ssrf_client()
    } else {
        shared_client()
    };

    for hop in 0..=max_redirects {
        let scheme = current_url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(HttpxError::CrossHostRedirect);
        }
        if let Some(allowed) = allowed_host
            && !current_url
                .host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(allowed))
        {
            return Err(HttpxError::CrossHostRedirect);
        }
        if guard_ssrf {
            check_literal_ip(&current_url)?;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HttpxError::Timeout);
        }
        let mut resp = client
            .get(current_url.clone())
            .headers(header_map.clone())
            .timeout(remaining.min(Duration::from_secs(30)))
            .send()
            .await
            .map_err(|e| {
                if let Some(ip) = extract_blocked_address(&e) {
                    return HttpxError::BlockedAddress(ip);
                }
                if e.is_timeout() {
                    HttpxError::Timeout
                } else {
                    HttpxError::Stage {
                        stage: "request",
                        is_timeout: false,
                        is_connect_or_dns: e.is_connect(),
                        message: strip_url_error(&e),
                    }
                }
            })?;

        let status = resp.status();
        if status.is_redirection() {
            if hop == max_redirects {
                return Err(HttpxError::CrossHostRedirect);
            }
            let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
                let hdrs = collect_response_headers(resp.headers());
                return Ok(GuardedResponse {
                    status: status.as_u16(),
                    headers: hdrs,
                    body: Vec::new(),
                });
            };
            let loc_str = loc
                .to_str()
                .map_err(|e| HttpxError::Other(format!("invalid redirect location: {e}")))?;
            let next_url = current_url
                .join(loc_str)
                .map_err(|e| HttpxError::Other(format!("invalid redirect URL: {e}")))?;
            let next_scheme = next_url.scheme();
            if next_scheme != "http" && next_scheme != "https" {
                return Err(HttpxError::CrossHostRedirect);
            }
            if let Some(allowed) = allowed_host
                && !next_url
                    .host_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case(allowed))
            {
                return Err(HttpxError::CrossHostRedirect);
            }
            current_url = next_url;
            continue;
        }

        let hdrs = collect_response_headers(resp.headers());
        let mut body = Vec::new();
        loop {
            let chunk_rem = deadline.saturating_duration_since(Instant::now());
            if chunk_rem.is_zero() {
                return Err(HttpxError::Timeout);
            }
            let chunk = tokio::time::timeout(chunk_rem, resp.chunk())
                .await
                .map_err(|_| HttpxError::Timeout)?
                .map_err(|e| HttpxError::Stage {
                    stage: "response_body",
                    is_timeout: e.is_timeout(),
                    is_connect_or_dns: false,
                    message: strip_url_error(&e),
                })?;
            let Some(bytes) = chunk else { break };
            if body.len() + bytes.len() > limit {
                return Err(HttpxError::TooLarge);
            }
            body.extend_from_slice(&bytes);
        }

        return Ok(GuardedResponse {
            status: status.as_u16(),
            headers: hdrs,
            body,
        });
    }

    Err(HttpxError::CrossHostRedirect)
}

fn collect_response_headers(map: &HeaderMap) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (name, val) in map {
        if let Ok(s) = val.to_str() {
            out.entry(name.as_str().to_ascii_lowercase())
                .or_default()
                .push(s.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_server<F>(handler: F) -> (String, tokio::task::JoinHandle<()>)
    where
        F: Fn() -> String + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let resp = handler();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        if sock.write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn connection_reuse_and_body_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let conns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let conns_bg = std::sync::Arc::clone(&conns);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                conns_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
                        if sock.write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let url = format!("http://{addr}");
        for _ in 0..3 {
            let r = do_request(Request {
                url: url.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
            assert_eq!(r.body, b"hello");
        }
        assert_eq!(conns.load(std::sync::atomic::Ordering::SeqCst), 1);

        let err = do_request(Request {
            url,
            max_bytes: Some(3),
            ..Default::default()
        })
        .await
        .unwrap_err();
        assert!(matches!(err, HttpxError::TooLarge));
    }

    #[tokio::test]
    async fn credential_redirect_blocked_and_no_redirect() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_bg = std::sync::Arc::clone(&hits);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                hits_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .await;
            }
        });
        let target_url = format!("http://{target_addr}");
        let (redir_url, _) = spawn_server(move || {
            format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {target_url}\r\nContent-Length: 0\r\n\r\n")
        })
        .await;

        let mut headers = HashMap::new();
        headers.insert("X-Api-Key".into(), "SECRET".into());
        let err = do_request(Request {
            url: redir_url.clone(),
            headers,
            ..Default::default()
        })
        .await
        .unwrap_err();
        assert!(matches!(err, HttpxError::CredentialRedirect));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

        let pub_resp = do_request(Request {
            url: redir_url.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(pub_resp.body, b"ok");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        let no_redir = do_request(Request {
            url: redir_url,
            no_redirect: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(no_redir.status, 307);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timeout_and_reason_never_leak_url() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((_sock, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        let err = do_request(Request {
            url: format!("http://{addr}"),
            wait_for_headers: true,
            timeout: Some(Duration::from_millis(30)),
            ..Default::default()
        })
        .await
        .unwrap_err();
        assert!(err.is_timeout());

        assert_eq!(
            reason(&HttpxError::UserFacing("素材目录格式不对".into())),
            "素材目录格式不对"
        );
        assert!(reason(&HttpxError::Status(429)).contains("频繁"));
        assert!(reason(&HttpxError::Timeout).contains("超时"));
        let leak_msg = reason(&HttpxError::Stage {
            stage: "request",
            is_timeout: false,
            is_connect_or_dns: true,
            message: "connection refused".into(),
        });
        assert!(!leak_msg.contains("http"));
    }

    #[test]
    fn ssrf_ip_blocks_internal_and_mapped_ipv4() {
        assert!(is_blocked_ip("127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("100.64.0.1".parse().unwrap()));
        assert!(is_blocked_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("fe80::1".parse().unwrap()));
        assert!(!is_blocked_ip("1.1.1.1".parse().unwrap()));
    }
}
