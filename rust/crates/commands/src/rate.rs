use crate::kit;
use chrono::{FixedOffset, Utc};
use mibot_app::App;
use mibot_bot::BoxFuture;
use mibot_command::{CmdResult, Command, Invocation, code, group};
use mibot_httpx::{Request, Response, do_request, reason as httpx_reason};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct CurrencyDef {
    code: &'static str,
    symbol: &'static str,
    aliases: &'static [&'static str],
}

const FIAT_CURRENCIES: &[CurrencyDef] = &[
    CurrencyDef { code: "usd", symbol: "USD", aliases: &[] },
    CurrencyDef { code: "eur", symbol: "EUR", aliases: &[] },
    CurrencyDef { code: "gbp", symbol: "GBP", aliases: &[] },
    CurrencyDef { code: "jpy", symbol: "JPY", aliases: &[] },
    CurrencyDef { code: "cny", symbol: "CNY", aliases: &["rmb", "yuan", "cnh"] },
    CurrencyDef { code: "cad", symbol: "CAD", aliases: &[] },
    CurrencyDef { code: "aud", symbol: "AUD", aliases: &[] },
    CurrencyDef { code: "chf", symbol: "CHF", aliases: &[] },
    CurrencyDef { code: "nzd", symbol: "NZD", aliases: &[] },
    CurrencyDef { code: "sek", symbol: "SEK", aliases: &[] },
    CurrencyDef { code: "nok", symbol: "NOK", aliases: &[] },
    CurrencyDef { code: "dkk", symbol: "DKK", aliases: &[] },
    CurrencyDef { code: "isk", symbol: "ISK", aliases: &[] },
    CurrencyDef { code: "pln", symbol: "PLN", aliases: &[] },
    CurrencyDef { code: "czk", symbol: "CZK", aliases: &[] },
    CurrencyDef { code: "huf", symbol: "HUF", aliases: &[] },
    CurrencyDef { code: "ron", symbol: "RON", aliases: &[] },
    CurrencyDef { code: "bgn", symbol: "BGN", aliases: &[] },
    CurrencyDef { code: "hrk", symbol: "HRK", aliases: &[] },
    CurrencyDef { code: "rsd", symbol: "RSD", aliases: &[] },
    CurrencyDef { code: "bam", symbol: "BAM", aliases: &[] },
    CurrencyDef { code: "mkd", symbol: "MKD", aliases: &[] },
    CurrencyDef { code: "all", symbol: "ALL", aliases: &[] },
    CurrencyDef { code: "rub", symbol: "RUB", aliases: &[] },
    CurrencyDef { code: "uah", symbol: "UAH", aliases: &[] },
    CurrencyDef { code: "byn", symbol: "BYN", aliases: &[] },
    CurrencyDef { code: "mdl", symbol: "MDL", aliases: &[] },
    CurrencyDef { code: "try", symbol: "TRY", aliases: &[] },
    CurrencyDef { code: "gel", symbol: "GEL", aliases: &[] },
    CurrencyDef { code: "amd", symbol: "AMD", aliases: &[] },
    CurrencyDef { code: "azn", symbol: "AZN", aliases: &[] },
    CurrencyDef { code: "brl", symbol: "BRL", aliases: &[] },
    CurrencyDef { code: "mxn", symbol: "MXN", aliases: &[] },
    CurrencyDef { code: "ars", symbol: "ARS", aliases: &[] },
    CurrencyDef { code: "cop", symbol: "COP", aliases: &[] },
    CurrencyDef { code: "pen", symbol: "PEN", aliases: &[] },
    CurrencyDef { code: "clp", symbol: "CLP", aliases: &[] },
    CurrencyDef { code: "uyu", symbol: "UYU", aliases: &[] },
    CurrencyDef { code: "pyg", symbol: "PYG", aliases: &[] },
    CurrencyDef { code: "bob", symbol: "BOB", aliases: &[] },
    CurrencyDef { code: "ves", symbol: "VES", aliases: &[] },
    CurrencyDef { code: "gyd", symbol: "GYD", aliases: &[] },
    CurrencyDef { code: "srd", symbol: "SRD", aliases: &[] },
    CurrencyDef { code: "ttd", symbol: "TTD", aliases: &[] },
    CurrencyDef { code: "jmd", symbol: "JMD", aliases: &[] },
    CurrencyDef { code: "bbd", symbol: "BBD", aliases: &[] },
    CurrencyDef { code: "bsd", symbol: "BSD", aliases: &[] },
    CurrencyDef { code: "bzd", symbol: "BZD", aliases: &[] },
    CurrencyDef { code: "crc", symbol: "CRC", aliases: &[] },
    CurrencyDef { code: "gtq", symbol: "GTQ", aliases: &[] },
    CurrencyDef { code: "hnl", symbol: "HNL", aliases: &[] },
    CurrencyDef { code: "nio", symbol: "NIO", aliases: &[] },
    CurrencyDef { code: "pab", symbol: "PAB", aliases: &[] },
    CurrencyDef { code: "dop", symbol: "DOP", aliases: &[] },
    CurrencyDef { code: "htg", symbol: "HTG", aliases: &[] },
    CurrencyDef { code: "cub", symbol: "CUP", aliases: &[] },
    CurrencyDef { code: "sgd", symbol: "SGD", aliases: &[] },
    CurrencyDef { code: "hkd", symbol: "HKD", aliases: &[] },
    CurrencyDef { code: "krw", symbol: "KRW", aliases: &[] },
    CurrencyDef { code: "inr", symbol: "INR", aliases: &[] },
    CurrencyDef { code: "thb", symbol: "THB", aliases: &[] },
    CurrencyDef { code: "myr", symbol: "MYR", aliases: &[] },
    CurrencyDef { code: "php", symbol: "PHP", aliases: &[] },
    CurrencyDef { code: "idr", symbol: "IDR", aliases: &[] },
    CurrencyDef { code: "vnd", symbol: "VND", aliases: &[] },
    CurrencyDef { code: "lak", symbol: "LAK", aliases: &[] },
    CurrencyDef { code: "khr", symbol: "KHR", aliases: &[] },
    CurrencyDef { code: "mmk", symbol: "MMK", aliases: &[] },
    CurrencyDef { code: "bnd", symbol: "BND", aliases: &[] },
    CurrencyDef { code: "twd", symbol: "TWD", aliases: &[] },
    CurrencyDef { code: "mop", symbol: "MOP", aliases: &[] },
    CurrencyDef { code: "fjd", symbol: "FJD", aliases: &[] },
    CurrencyDef { code: "pgk", symbol: "PGK", aliases: &[] },
    CurrencyDef { code: "sbd", symbol: "SBD", aliases: &[] },
    CurrencyDef { code: "vuv", symbol: "VUV", aliases: &[] },
    CurrencyDef { code: "top", symbol: "TOP", aliases: &[] },
    CurrencyDef { code: "wst", symbol: "WST", aliases: &[] },
    CurrencyDef { code: "lkr", symbol: "LKR", aliases: &[] },
    CurrencyDef { code: "pkr", symbol: "PKR", aliases: &[] },
    CurrencyDef { code: "bdt", symbol: "BDT", aliases: &[] },
    CurrencyDef { code: "npr", symbol: "NPR", aliases: &[] },
    CurrencyDef { code: "btn", symbol: "BTN", aliases: &[] },
    CurrencyDef { code: "mvr", symbol: "MVR", aliases: &[] },
    CurrencyDef { code: "afn", symbol: "AFN", aliases: &[] },
    CurrencyDef { code: "kzt", symbol: "KZT", aliases: &[] },
    CurrencyDef { code: "uzs", symbol: "UZS", aliases: &[] },
    CurrencyDef { code: "kgs", symbol: "KGS", aliases: &[] },
    CurrencyDef { code: "tjs", symbol: "TJS", aliases: &[] },
    CurrencyDef { code: "tmm", symbol: "TMT", aliases: &[] },
    CurrencyDef { code: "zar", symbol: "ZAR", aliases: &[] },
    CurrencyDef { code: "ils", symbol: "ILS", aliases: &[] },
    CurrencyDef { code: "aed", symbol: "AED", aliases: &[] },
    CurrencyDef { code: "sar", symbol: "SAR", aliases: &[] },
    CurrencyDef { code: "qar", symbol: "QAR", aliases: &[] },
    CurrencyDef { code: "kwd", symbol: "KWD", aliases: &[] },
    CurrencyDef { code: "bhd", symbol: "BHD", aliases: &[] },
    CurrencyDef { code: "omr", symbol: "OMR", aliases: &[] },
    CurrencyDef { code: "jod", symbol: "JOD", aliases: &[] },
    CurrencyDef { code: "lbp", symbol: "LBP", aliases: &[] },
    CurrencyDef { code: "syp", symbol: "SYP", aliases: &[] },
    CurrencyDef { code: "iqd", symbol: "IQD", aliases: &[] },
    CurrencyDef { code: "irr", symbol: "IRR", aliases: &[] },
    CurrencyDef { code: "yer", symbol: "YER", aliases: &[] },
    CurrencyDef { code: "egp", symbol: "EGP", aliases: &[] },
    CurrencyDef { code: "mad", symbol: "MAD", aliases: &[] },
    CurrencyDef { code: "dzd", symbol: "DZD", aliases: &[] },
    CurrencyDef { code: "tnd", symbol: "TND", aliases: &[] },
    CurrencyDef { code: "lyd", symbol: "LYD", aliases: &[] },
    CurrencyDef { code: "sdg", symbol: "SDG", aliases: &[] },
    CurrencyDef { code: "etb", symbol: "ETB", aliases: &[] },
    CurrencyDef { code: "ern", symbol: "ERN", aliases: &[] },
    CurrencyDef { code: "djf", symbol: "DJF", aliases: &[] },
    CurrencyDef { code: "sos", symbol: "SOS", aliases: &[] },
    CurrencyDef { code: "ngn", symbol: "NGN", aliases: &[] },
    CurrencyDef { code: "ghs", symbol: "GHS", aliases: &[] },
    CurrencyDef { code: "xof", symbol: "XOF", aliases: &[] },
    CurrencyDef { code: "sll", symbol: "SLL", aliases: &[] },
    CurrencyDef { code: "lrd", symbol: "LRD", aliases: &[] },
    CurrencyDef { code: "gmd", symbol: "GMD", aliases: &[] },
    CurrencyDef { code: "gnf", symbol: "GNF", aliases: &[] },
    CurrencyDef { code: "cvs", symbol: "CVE", aliases: &[] },
    CurrencyDef { code: "kes", symbol: "KES", aliases: &[] },
    CurrencyDef { code: "ugx", symbol: "UGX", aliases: &[] },
    CurrencyDef { code: "tzs", symbol: "TZS", aliases: &[] },
    CurrencyDef { code: "rwf", symbol: "RWF", aliases: &[] },
    CurrencyDef { code: "bif", symbol: "BIF", aliases: &[] },
    CurrencyDef { code: "mzn", symbol: "MZN", aliases: &[] },
    CurrencyDef { code: "mwk", symbol: "MWK", aliases: &[] },
    CurrencyDef { code: "zmw", symbol: "ZMW", aliases: &[] },
    CurrencyDef { code: "zwd", symbol: "ZWL", aliases: &[] },
    CurrencyDef { code: "mga", symbol: "MGA", aliases: &[] },
    CurrencyDef { code: "mur", symbol: "MUR", aliases: &[] },
    CurrencyDef { code: "scr", symbol: "SCR", aliases: &[] },
    CurrencyDef { code: "kmf", symbol: "KMF", aliases: &[] },
    CurrencyDef { code: "xaf", symbol: "XAF", aliases: &[] },
    CurrencyDef { code: "cdf", symbol: "CDF", aliases: &[] },
    CurrencyDef { code: "aoa", symbol: "AOA", aliases: &[] },
    CurrencyDef { code: "std", symbol: "STN", aliases: &[] },
    CurrencyDef { code: "gqe", symbol: "XAF", aliases: &[] },
    CurrencyDef { code: "bwp", symbol: "BWP", aliases: &[] },
    CurrencyDef { code: "nad", symbol: "NAD", aliases: &[] },
    CurrencyDef { code: "szl", symbol: "SZL", aliases: &[] },
    CurrencyDef { code: "lsl", symbol: "LSL", aliases: &[] },
];

const CRYPTO_CURRENCIES: &[CurrencyDef] = &[
    CurrencyDef { code: "btc", symbol: "BTC", aliases: &["bitcoin"] },
    CurrencyDef { code: "eth", symbol: "ETH", aliases: &["ethereum"] },
    CurrencyDef { code: "usdt", symbol: "USDT", aliases: &["tether"] },
    CurrencyDef { code: "bnb", symbol: "BNB", aliases: &["binancecoin"] },
    CurrencyDef { code: "sol", symbol: "SOL", aliases: &["solana"] },
    CurrencyDef { code: "usdc", symbol: "USDC", aliases: &["usd-coin"] },
    CurrencyDef { code: "xrp", symbol: "XRP", aliases: &["ripple"] },
    CurrencyDef { code: "doge", symbol: "DOGE", aliases: &["dogecoin"] },
    CurrencyDef { code: "ton", symbol: "TON", aliases: &["toncoin"] },
    CurrencyDef { code: "ada", symbol: "ADA", aliases: &["cardano"] },
    CurrencyDef { code: "shib", symbol: "SHIB", aliases: &["shiba-inu"] },
    CurrencyDef { code: "avax", symbol: "AVAX", aliases: &["avalanche-2"] },
    CurrencyDef { code: "trx", symbol: "TRX", aliases: &["tron"] },
    CurrencyDef { code: "dot", symbol: "DOT", aliases: &["polkadot"] },
    CurrencyDef { code: "link", symbol: "LINK", aliases: &["chainlink"] },
    CurrencyDef { code: "matic", symbol: "MATIC", aliases: &["matic-network"] },
    CurrencyDef { code: "wbtc", symbol: "WBTC", aliases: &["wrapped-bitcoin"] },
    CurrencyDef { code: "ltc", symbol: "LTC", aliases: &["litecoin"] },
    CurrencyDef { code: "bch", symbol: "BCH", aliases: &["bitcoin-cash"] },
    CurrencyDef { code: "uni", symbol: "UNI", aliases: &["uniswap"] },
    CurrencyDef { code: "atom", symbol: "ATOM", aliases: &["cosmos"] },
    CurrencyDef { code: "etc", symbol: "ETC", aliases: &["ethereum-classic"] },
    CurrencyDef { code: "xlm", symbol: "XLM", aliases: &["stellar"] },
    CurrencyDef { code: "okb", symbol: "OKB", aliases: &[] },
    CurrencyDef { code: "icp", symbol: "ICP", aliases: &["internet-computer"] },
    CurrencyDef { code: "fil", symbol: "FIL", aliases: &["filecoin"] },
    CurrencyDef { code: "hbar", symbol: "HBAR", aliases: &["hedera-hashgraph"] },
    CurrencyDef { code: "ldo", symbol: "LDO", aliases: &["lido-dao"] },
    CurrencyDef { code: "crv", symbol: "CRV", aliases: &["curve-dao-token"] },
    CurrencyDef { code: "arb", symbol: "ARB", aliases: &["arbitrum"] },
];

const RATE_BRIDGES: &[&str] = &["USDT", "USDC"];

fn code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9-]{0,63}$").expect("valid rate code regex"))
}

fn arg_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z0-9.+-]+$").expect("valid rate arg regex"))
}

#[derive(Debug, Clone)]
struct RateFailure(String);

impl std::fmt::Display for RateFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RateFailure {}

type FetchFn = Arc<dyn Fn(Request) -> BoxFuture<'static, Result<Response, mibot_httpx::HttpxError>> + Send + Sync>;

struct CachedFiat {
    rates: HashMap<String, f64>,
    at: Instant,
}

#[derive(Default)]
struct RateState {
    fiat_cache: HashMap<String, CachedFiat>,
    dynamic_fiats: Option<HashSet<String>>,
    dynamic_at: Option<Instant>,
    active: usize,
}

struct RateService {
    mu: Arc<Mutex<RateState>>,
    fetch: FetchFn,
}

impl RateService {
    fn new(fetch: FetchFn) -> Self {
        Self {
            mu: Arc::new(Mutex::new(RateState::default())),
            fetch,
        }
    }

    async fn enter(&self) -> Option<LeaveGuard> {
        let mut g = self.mu.lock().await;
        if g.active >= 4 {
            return None;
        }
        g.active += 1;
        Some(LeaveGuard(Arc::clone(&self.mu)))
    }

    async fn is_fiat(&self, cancel: &CancellationToken, query: &str, getter: &mut RateGetter<'_>) -> bool {
        {
            let g = self.mu.lock().await;
            if let (Some(set), Some(at)) = (&g.dynamic_fiats, g.dynamic_at)
                && at.elapsed() < Duration::from_secs(6 * 3600)
            {
                return set.contains(query);
            }
        }
        let endpoints = [
            "https://api.coingecko.com/api/v3/simple/supported_vs_currencies",
            "https://api.exchangerate.host/symbols",
            "https://api.frankfurter.app/currencies",
        ];
        for (idx, endpoint) in endpoints.iter().enumerate() {
            let data = match getter.get(cancel, endpoint, Duration::from_secs(8)).await {
                Ok(v) => v,
                Err(_) if cancel.is_cancelled() => return false,
                Err(_) => continue,
            };
            let keys: Vec<String> = match idx {
                0 => data
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default(),
                1 => data
                    .get("symbols")
                    .and_then(|s| s.as_object())
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default(),
                _ => data
                    .as_object()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default(),
            };
            if keys.is_empty() || keys.len() > 1024 {
                continue;
            }
            let mut codes = HashSet::new();
            let mut valid = true;
            for k in keys {
                if !code_re().is_match(&k) {
                    valid = false;
                    break;
                }
                codes.insert(k.to_ascii_lowercase());
            }
            if !valid {
                continue;
            }
            let hit = codes.contains(query);
            let mut g = self.mu.lock().await;
            g.dynamic_fiats = Some(codes);
            g.dynamic_at = Some(Instant::now());
            return hit;
        }
        let fallback: HashSet<String> = FIAT_CURRENCIES.iter().map(|c| c.code.to_owned()).collect();
        let hit = fallback.contains(query);
        let mut g = self.mu.lock().await;
        g.dynamic_fiats = Some(fallback);
        g.dynamic_at = Some(Instant::now());
        hit
    }

    async fn currency(
        &self,
        cancel: &CancellationToken,
        query: &str,
        getter: &mut RateGetter<'_>,
    ) -> RateCurrency {
        if let Some(info) = FIAT_CURRENCIES.iter().find(|c| c.code == query) {
            return RateCurrency { symbol: info.symbol.into(), fiat: true };
        }
        if let Some(info) = CRYPTO_CURRENCIES.iter().find(|c| c.code == query) {
            return RateCurrency { symbol: info.symbol.into(), fiat: false };
        }
        let fiat = self.is_fiat(cancel, query, getter).await;
        RateCurrency {
            symbol: query.to_ascii_uppercase(),
            fiat,
        }
    }

    async fn fiat_rates(
        &self,
        cancel: &CancellationToken,
        base: &str,
        getter: &mut RateGetter<'_>,
    ) -> Result<HashMap<String, f64>, RateFailure> {
        let key = base.to_ascii_lowercase();
        {
            let g = self.mu.lock().await;
            if let Some(c) = g.fiat_cache.get(&key)
                && c.at.elapsed() < Duration::from_secs(5 * 60)
            {
                return Ok(c.rates.clone());
            }
        }
        let escaped: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
        let upper_escaped: String =
            url::form_urlencoded::byte_serialize(key.to_ascii_uppercase().as_bytes()).collect();
        let endpoints = [
            format!("https://api.exchangerate.host/latest?base={escaped}"),
            format!("https://open.er-api.com/v6/latest/{escaped}"),
            format!("https://api.frankfurter.app/latest?from={escaped}"),
            format!("https://api.coinbase.com/v2/exchange-rates?currency={upper_escaped}"),
            format!("https://cdn.jsdelivr.net/gh/fawazahmed0/currency-api@1/latest/currencies/{escaped}.json"),
        ];
        let mut last = "法币汇率服务不可用".to_owned();
        for endpoint in endpoints {
            let data = match getter.get(cancel, &endpoint, Duration::from_secs(8)).await {
                Ok(v) => v,
                Err(e) if cancel.is_cancelled() => return Err(e),
                Err(e) => {
                    last = e.0;
                    continue;
                }
            };
            let source = data
                .get("rates")
                .and_then(|v| v.as_object())
                .or_else(|| data.get("data").and_then(|d| d.get("rates")).and_then(|v| v.as_object()))
                .or_else(|| data.get(&key).and_then(|v| v.as_object()));
            let success_false = data.get("success").and_then(|v| v.as_bool()) == Some(false);
            let bad_result = data.get("result").is_some_and(|v| v.as_str() != Some("success"));
            let Some(obj) = source.filter(|_| !success_false && !bad_result) else {
                last = "法币汇率数据格式无效".into();
                continue;
            };
            if obj.is_empty() || obj.len() > 1024 {
                last = "法币汇率数据大小无效".into();
                continue;
            }
            let mut rates = HashMap::new();
            for (name, val) in obj {
                if !code_re().is_match(name) {
                    continue;
                }
                let num = if let Some(n) = val.as_f64() {
                    n
                } else if let Some(s) = val.as_str()
                    && let Ok(n) = s.parse::<f64>()
                {
                    n
                } else {
                    continue;
                };
                if num.is_finite() && num > 0.0 {
                    rates.insert(name.to_ascii_lowercase(), num);
                }
            }
            if rates.is_empty() {
                last = "法币汇率数据无有效价格".into();
                continue;
            }
            let mut g = self.mu.lock().await;
            if g.fiat_cache.len() >= 16
                && let Some(first_k) = g.fiat_cache.keys().next().cloned()
            {
                g.fiat_cache.remove(&first_k);
            }
            g.fiat_cache.insert(
                key,
                CachedFiat {
                    rates: rates.clone(),
                    at: Instant::now(),
                },
            );
            return Ok(rates);
        }
        Err(RateFailure(format!("法币汇率服务不可用：{last}")))
    }
}

struct LeaveGuard(Arc<Mutex<RateState>>);

impl Drop for LeaveGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.0.try_lock() {
            g.active = g.active.saturating_sub(1);
        } else {
            let mu = Arc::clone(&self.0);
            tokio::spawn(async move {
                let mut g = mu.lock().await;
                g.active = g.active.saturating_sub(1);
            });
        }
    }
}

struct RateGetter<'a> {
    fetch: &'a FetchFn,
    requests: usize,
}

impl RateGetter<'_> {
    async fn get(
        &mut self,
        cancel: &CancellationToken,
        target: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, RateFailure> {
        if cancel.is_cancelled() {
            return Err(RateFailure("操作已取消".into()));
        }
        self.requests += 1;
        if self.requests > 64 {
            return Err(RateFailure("本次查询已达到请求上限，稍后再试".into()));
        }
        let req = Request {
            url: target.to_owned(),
            timeout: Some(timeout),
            max_bytes: Some(128 << 10),
            ..Default::default()
        };
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(RateFailure("操作已取消".into())),
            r = (self.fetch)(req) => r.map_err(|e| RateFailure(httpx_reason(&e)))?,
        };
        if !resp.ok() {
            if resp.status == 429 {
                return Err(RateFailure("请求太频繁，被汇率服务限流了，过几分钟再试".into()));
            }
            return Err(RateFailure(format!("汇率服务 HTTP {}", resp.status)));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|_| RateFailure("汇率服务返回的数据格式或大小无效".into()))
    }
}

#[derive(Debug, Clone)]
struct RateCurrency {
    symbol: String,
    fiat: bool,
}

fn normalize_currency(token: &str) -> String {
    let key = token.to_ascii_lowercase();
    if key == "rm" {
        return "myr".into();
    }
    for table in [FIAT_CURRENCIES, CRYPTO_CURRENCIES] {
        for info in table {
            if info.aliases.iter().any(|&a| a == key) {
                return info.code.to_owned();
            }
        }
    }
    key
}

fn parse_rate_args(args: &[String]) -> Result<(String, String, f64), RateFailure> {
    if args.len() > 32 {
        return Err(RateFailure("参数过多，请使用货币代码和有限数量".into()));
    }
    let mut amount = 1.0f64;
    let mut currencies = Vec::new();
    for arg in args {
        if arg.len() > 64 || !arg_re().is_match(arg) {
            return Err(RateFailure("请提供有效的货币代码和有限数量".into()));
        }
        let token = normalize_currency(arg);
        if let Ok(num) = token.parse::<f64>()
            && num.is_finite()
        {
            amount = num;
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if code_re().is_match(&token) && lower != "nan" && lower != "infinity" {
            currencies.push(token);
            continue;
        }
        return Err(RateFailure("请提供有效的货币代码和有限数量".into()));
    }
    let base = currencies.first().cloned().unwrap_or_else(|| "btc".into());
    let quote = currencies.get(1).cloned().unwrap_or_else(|| "usd".into());
    Ok((base, quote, amount))
}

fn positive_rate(v: f64) -> Result<f64, RateFailure> {
    if !v.is_finite() || v <= 0.0 {
        return Err(RateFailure("汇率服务返回了无效价格".into()));
    }
    Ok(v)
}

fn format_thousands(value: f64, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    let (neg, digits) = match whole.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, whole),
    };
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (digits.len() - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

fn format_amount(v: f64) -> String {
    if v >= 1.0 {
        format_thousands(v, 2)
    } else {
        format!("{v:.6}")
    }
}

fn format_price(v: f64) -> String {
    if v >= 1.0 {
        format_amount(v)
    } else if v >= 0.01 {
        format!("{v:.4}")
    } else if v >= 0.0001 {
        format!("{v:.6}")
    } else {
        format!("{v:.2e}")
    }
}

struct RatePricer<'a> {
    cancel: &'a CancellationToken,
    service: &'a RateService,
    get: RateGetter<'a>,
    tickers: HashMap<String, f64>,
}

impl RatePricer<'_> {
    async fn binance(&mut self, pair: &str) -> Result<f64, RateFailure> {
        if let Some(&cached) = self.tickers.get(pair) {
            return Ok(cached);
        }
        let q: String = url::form_urlencoded::byte_serialize(pair.as_bytes()).collect();
        let data = self
            .get
            .get(
                self.cancel,
                &format!("https://api.binance.com/api/v3/ticker/price?symbol={q}"),
                Duration::from_secs(5),
            )
            .await?;
        let val = data.get("price");
        let parsed = if let Some(n) = val.and_then(|v| v.as_f64()) {
            n
        } else if let Some(s) = val.and_then(|v| v.as_str())
            && let Ok(n) = s.parse::<f64>()
        {
            n
        } else {
            return Err(RateFailure("币安交易对价格无效".into()));
        };
        let p = positive_rate(parsed)?;
        self.tickers.insert(pair.to_owned(), p);
        Ok(p)
    }

    async fn in_bridge(&mut self, coin: &str, bridge: &str) -> Result<f64, RateFailure> {
        if coin == bridge {
            return Ok(1.0);
        }
        self.binance(&format!("{coin}{bridge}")).await
    }

    async fn crypto_fiat(&mut self, crypto: &str, fiat: &str) -> Result<f64, RateFailure> {
        let mut last = "交易对不可用".to_owned();
        for bridge in RATE_BRIDGES {
            let price = match self.in_bridge(crypto, bridge).await {
                Ok(p) => p,
                Err(e) if self.cancel.is_cancelled() => return Err(e),
                Err(e) => {
                    last = e.0;
                    continue;
                }
            };
            let rates = match self.service.fiat_rates(self.cancel, "usd", &mut self.get).await {
                Ok(r) => r,
                Err(e) if self.cancel.is_cancelled() => return Err(e),
                Err(e) => {
                    last = e.0;
                    continue;
                }
            };
            let rate = match positive_rate(*rates.get(&fiat.to_ascii_lowercase()).unwrap_or(&0.0)) {
                Ok(r) => r,
                Err(e) => {
                    last = e.0;
                    continue;
                }
            };
            return positive_rate(price * rate);
        }
        Err(RateFailure(format!("无法获取 {crypto} 对 {fiat} 的价格。最后错误：{last}")))
    }

    async fn crypto_crypto(&mut self, first: &str, second: &str) -> Result<f64, RateFailure> {
        match self.binance(&format!("{first}{second}")).await {
            Ok(p) => return Ok(p),
            Err(e) if self.cancel.is_cancelled() => return Err(e),
            Err(_) => {}
        }
        match self.binance(&format!("{second}{first}")).await {
            Ok(p) => return positive_rate(1.0 / p),
            Err(e) if self.cancel.is_cancelled() => return Err(e),
            Err(_) => {}
        }
        for bridge in RATE_BRIDGES {
            let a = match self.in_bridge(first, bridge).await {
                Ok(v) => v,
                Err(e) if self.cancel.is_cancelled() => return Err(e),
                Err(_) => continue,
            };
            let b = match self.in_bridge(second, bridge).await {
                Ok(v) => v,
                Err(e) if self.cancel.is_cancelled() => return Err(e),
                Err(_) => continue,
            };
            return positive_rate(a / b);
        }
        Err(RateFailure(format!("无法找到 {first} 和 {second} 之间的交易对")))
    }

    async fn price(&mut self, source: &RateCurrency, target: &RateCurrency) -> Result<f64, RateFailure> {
        match (source.fiat, target.fiat) {
            (false, false) => self.crypto_crypto(&source.symbol, &target.symbol).await,
            (false, true) => self.crypto_fiat(&source.symbol, &target.symbol).await,
            (true, false) => match self.crypto_fiat(&target.symbol, &source.symbol).await {
                Ok(rev) => positive_rate(1.0 / rev),
                Err(e) if self.cancel.is_cancelled() => Err(e),
                Err(_) => Err(RateFailure(format!(
                    "无法获取 {} 对 {} 的价格来计算反向汇率",
                    target.symbol, source.symbol
                ))),
            },
            (true, true) => {
                let rates = self
                    .service
                    .fiat_rates(self.cancel, &source.symbol, &mut self.get)
                    .await?;
                let val = *rates.get(&target.symbol.to_ascii_lowercase()).unwrap_or(&0.0);
                if val == 0.0 {
                    return Err(RateFailure(format!(
                        "无法获取 {} 到 {} 的汇率",
                        source.symbol, target.symbol
                    )));
                }
                positive_rate(val)
            }
        }
    }
}

fn rate_help(prefix: &str) -> String {
    let line = |args: &str, detail: &str| format!("• {} {detail}\n", code(&format!("{prefix}rate {args}")));
    format!(
        "💱 <b>汇率查询</b>\n\n<b>使用示例</b>\n{}{}{}{}{}",
        line("BTC", "比特币的美元价"),
        line("ETH CNY", "以太坊的人民币价"),
        line("CNY TRY", "人民币兑土耳其里拉"),
        line("BTC CNY 0.5", "0.5 个 BTC 换成人民币"),
        line("CNY USDT 7000", "7000 元换 USDT"),
    )
}

fn render_price_failure(err: &RateFailure, source: &RateCurrency, target: &RateCurrency, fallback: &str) -> String {
    let kind = |c: &RateCurrency| if c.fiat { "法币" } else { "加密货币" };
    format!(
        "{}\n\n<b>识别结果</b>\n• {}（{}）\n• {}（{}）{fallback}",
        kit::feedback("error", "获取价格失败", &err.0),
        code(&source.symbol),
        kind(source),
        code(&target.symbol),
        kind(target),
    )
}

async fn render_rate(
    pricer: &mut RatePricer<'_>,
    source: &RateCurrency,
    target: &RateCurrency,
    amount: f64,
    price: f64,
    converted: f64,
) -> String {
    let shanghai = FixedOffset::east_opt(8 * 3600).expect("valid +08:00");
    let updated = Utc::now().with_timezone(&shanghai).format("%Y-%m-%d %H:%M:%S");
    let mut out = String::from("💱 <b>汇率</b>\n\n");
    if !source.fiat && target.fiat && amount == 1.0 {
        out.push_str(&format!(
            "{}\n\n",
            code(&format!("1 {} = {} {}", source.symbol, format_price(price), target.symbol))
        ));
    } else {
        out.push_str(&format!(
            "{}\n{}\n\n",
            code(&format!("{} {} ≈", format_amount(amount), source.symbol)),
            code(&format!("{} {}", format_amount(converted), target.symbol)),
        ));
        if source.fiat && target.fiat {
            out.push_str(&format!(
                "📊 汇率：{}\n",
                code(&format!("1 {} = {} {}", source.symbol, format_amount(price), target.symbol))
            ));
        } else if !source.fiat && !target.fiat {
            let first = pricer.crypto_fiat(&source.symbol, "USD").await;
            let second = pricer.crypto_fiat(&target.symbol, "USD").await;
            out.push_str(&format!(
                "💎 兑换比率：{}\n",
                code(&format!("1 {} = {} {}", source.symbol, format_amount(price), target.symbol))
            ));
            if let (Ok(f), Ok(s)) = (first, second) {
                out.push_str(&format!(
                    "📊 基准价格：{}\n",
                    code(&format!("{} ${} • {} ${}", source.symbol, format_price(f), target.symbol, format_price(s)))
                ));
            }
        } else if source.fiat {
            out.push_str(&format!(
                "💎 当前汇率：{}\n",
                code(&format!("1 {} = {} {}", target.symbol, format_price(1.0 / price), source.symbol))
            ));
        } else {
            out.push_str(&format!(
                "💎 当前汇率：{}\n",
                code(&format!("1 {} = {} {}", source.symbol, format_price(price), target.symbol))
            ));
        }
    }
    let label = if source.fiat && target.fiat { "更新时间" } else { "数据更新" };
    out.push_str(&format!("⏰ {label}：{updated}"));
    out
}

async fn handle(inv: &Invocation, service: &RateService) -> CmdResult {
    let Some(_guard) = service.enter().await else {
        return inv.edit(&kit::feedback("error", "汇率查询繁忙", "稍后再试")).await.map_err(Into::into);
    };
    if inv.arg(0).is_empty() {
        return inv.edit(&rate_help(inv.prefix())).await.map_err(Into::into);
    }
    let (base, quote, amount) = match parse_rate_args(&inv.args) {
        Ok(v) => v,
        Err(e) => return inv.edit(&kit::feedback("error", "操作失败", &e.0)).await.map_err(Into::into),
    };
    let raw_query = format!("{amount} {} to {}", base.to_ascii_uppercase(), quote.to_ascii_uppercase());
    let encoded_q: String = url::form_urlencoded::byte_serialize(raw_query.as_bytes()).collect();
    let fallback = format!("\n\n🔎 谷歌兜底：<a href=\"https://www.google.com/search?q={encoded_q}\">点击查看</a>");
    inv.edit(&kit::feedback("working", "正在查询汇率", "")).await?;

    let mut getter = RateGetter {
        fetch: &service.fetch,
        requests: 0,
    };
    let source = service.currency(&inv.cancel, &base, &mut getter).await;
    let target = service.currency(&inv.cancel, &quote, &mut getter).await;
    let mut pricer = RatePricer {
        cancel: &inv.cancel,
        service,
        get: getter,
        tickers: HashMap::new(),
    };
    let price = match pricer.price(&source, &target).await {
        Ok(p) => p,
        Err(e) => {
            if inv.cancel.is_cancelled() {
                return Err(Box::new(e));
            }
            return inv.edit(&render_price_failure(&e, &source, &target, &fallback)).await.map_err(Into::into);
        }
    };
    let converted = amount * price;
    if !converted.is_finite() {
        return inv
            .edit(&format!("{}{fallback}", kit::feedback("error", "操作失败", "换算结果超出有限数值范围")))
            .await
            .map_err(Into::into);
    }
    let html = render_rate(&mut pricer, &source, &target, amount, price, converted).await;
    inv.edit(&html).await?;
    Ok(())
}

pub fn register(app: &Arc<App>) {
    let svc = Arc::new(RateService::new(Arc::new(|req| Box::pin(do_request(req)))));
    app.registry.register(Command {
        name: "rate".into(),
        group: group::TOOLS.into(),
        description: "查询汇率并换算金额".into(),
        usage: "货币 [目标货币] [数量]".into(),
        help_fn: Some(Arc::new(rate_help)),
        timeout: Some(Duration::from_secs(120)),
        handle: Arc::new(move |inv| {
            let s = Arc::clone(&svc);
            Box::pin(async move { handle(&inv, &s).await })
        }),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_and_format() {
        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
        let (b, q, a) = parse_rate_args(&s(&["BTC", "CNY", "0.5"])).unwrap();
        assert_eq!((b.as_str(), q.as_str(), a), ("btc", "cny", 0.5));
        let (b, q, a) = parse_rate_args(&[]).unwrap();
        assert_eq!((b.as_str(), q.as_str(), a), ("btc", "usd", 1.0));
        let (b, _, _) = parse_rate_args(&s(&["rmb"])).unwrap();
        assert_eq!(b, "cny");
        let (b, _, _) = parse_rate_args(&s(&["rm"])).unwrap();
        assert_eq!(b, "myr");
        let long = "a".repeat(80);
        for bad in [vec!["../etc/passwd"], vec!["<script>"], vec!["nan"], vec![long.as_str()]] {
            assert!(parse_rate_args(&s(&bad)).is_err(), "bad={bad:?}");
        }

        assert_eq!(format_amount(1234567.891), "1,234,567.89");
        assert!(format_price(0.00001234).contains('e'));
        assert_eq!(format_price(0.5), "0.5000");
    }

    #[tokio::test]
    async fn test_crypto_fiat_and_cache() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls_ref = Arc::clone(&calls);
        let svc = RateService::new(Arc::new(move |req| {
            let calls = Arc::clone(&calls_ref);
            Box::pin(async move {
                calls.lock().unwrap().push(req.url.clone());
                if req.url.contains("binance.com") {
                    return Ok(Response {
                        status: 200,
                        body: br#"{"price":"85000.0"}"#.to_vec(),
                    });
                }
                if req.url.contains("exchangerate.host/latest") {
                    return Ok(Response {
                        status: 200,
                        body: br#"{"rates":{"cny":7.2,"usd":1.0}}"#.to_vec(),
                    });
                }
                Ok(Response { status: 404, body: vec![] })
            })
        }));
        let cancel = CancellationToken::new();
        let mut getter = RateGetter { fetch: &svc.fetch, requests: 0 };
        let src = svc.currency(&cancel, "btc", &mut getter).await;
        let dst = svc.currency(&cancel, "cny", &mut getter).await;
        let mut pricer = RatePricer { cancel: &cancel, service: &svc, get: getter, tickers: HashMap::new() };
        let price = pricer.price(&src, &dst).await.unwrap();
        assert!((price - 85000.0 * 7.2).abs() < 1e-6);
        let html = render_rate(&mut pricer, &src, &dst, 1.0, price, price).await;
        assert!(html.contains("BTC =") && html.contains("CNY"));
    }
}
