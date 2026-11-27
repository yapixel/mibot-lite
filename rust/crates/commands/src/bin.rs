use crate::kit;
use mibot_app::App;
use mibot_command::{Invocation, code, escape, group};
use mibot_httpx::{Request, do_request};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Debug, Default, Deserialize)]
struct BinCountry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    emoji: String,
    #[serde(default)]
    currency: String,
}

#[derive(Debug, Default, Deserialize)]
struct BinBank {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct BinResult {
    #[serde(default)]
    scheme: String,
    #[serde(default, rename = "type")]
    card_type: String,
    #[serde(default)]
    brand: String,
    #[serde(default)]
    prepaid: Option<bool>,
    #[serde(default)]
    country: Option<BinCountry>,
    #[serde(default)]
    bank: Option<BinBank>,
}

static RE_LEVEL: OnceLock<Regex> = OnceLock::new();
static RE_BUSINESS: OnceLock<Regex> = OnceLock::new();

fn card_level_re() -> &'static Regex {
    RE_LEVEL.get_or_init(|| {
        Regex::new(r"(?i)BUSINESS|CORPORATE|PLATINUM|GOLD|CLASSIC|SIGNATURE|INFINITE|WORLD|PREMIUM|REWARDS").unwrap()
    })
}

fn business_re() -> &'static Regex {
    RE_BUSINESS.get_or_init(|| Regex::new(r"(?i)BUSINESS|CORPORATE|COMMERCIAL").unwrap())
}

fn scheme_name(raw: &str) -> &str {
    match raw.to_ascii_lowercase().as_str() {
        "visa" => "Visa",
        "mastercard" => "Mastercard",
        "amex" => "American Express",
        "diners" => "Diners Club",
        "discover" => "Discover",
        "jcb" => "JCB",
        "unionpay" => "UnionPay",
        "maestro" => "Maestro",
        "mir" => "MIR",
        _ => "",
    }
}

fn card_type_name(raw: &str) -> &str {
    match raw.to_ascii_lowercase().as_str() {
        "credit" => kit::tr("Credit", "贷记卡"),
        "debit" => kit::tr("Debit", "借记卡"),
        "charge" => kit::tr("Charge", "签账卡"),
        "prepaid" => kit::tr("Prepaid", "预付卡"),
        _ => "",
    }
}

fn currency_name(code_str: &str) -> Option<&'static str> {
    Some(match code_str {
        "USD" => "美元",
        "CNY" => "人民币",
        "HKD" => "港币",
        "TWD" => "新台币",
        "EUR" => "欧元",
        "JPY" => "日元",
        "GBP" => "英镑",
        "AUD" => "澳元",
        "CAD" => "加元",
        "SGD" => "新加坡元",
        "KRW" => "韩元",
        "RUB" => "卢布",
        _ => return None,
    })
}

pub fn bin_digits(input: &str) -> Option<String> {
    let mut digits = String::new();
    for ch in input.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            if digits.len() == 8 {
                break;
            }
        }
    }
    if digits.len() < 6 {
        None
    } else {
        Some(digits)
    }
}

pub fn render_bin(bin: &str, result: &BinResult) -> String {
    let zh = kit::is_zh();
    let scheme = match scheme_name(&result.scheme) {
        "" => kit::or_dash(&result.scheme),
        s => s,
    };
    let kind = match card_type_name(&result.card_type) {
        "" => kit::or_dash(&result.card_type),
        s => s,
    };
    let level = card_level_re()
        .find(&result.brand)
        .map(|m| m.as_str().to_ascii_uppercase())
        .unwrap_or_else(|| "—".to_owned());

    let (country, currency) = match &result.country {
        Some(c) => {
            let name = c.name.trim_end_matches(" (the)").trim();
            let country = format!("{} {name}", c.emoji).trim().to_owned();
            let currency = if c.currency.is_empty() {
                "—".to_owned()
            } else if zh && let Some(cn) = currency_name(&c.currency) {
                format!("{cn}（{}）", c.currency)
            } else {
                c.currency.clone()
            };
            (country, currency)
        }
        None => ("—".to_owned(), "—".to_owned()),
    };

    let bank = result
        .bank
        .as_ref()
        .map(|b| b.name.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("—");

    let prepaid = match result.prepaid {
        Some(true) => kit::tr("Yes", "是"),
        Some(false) => kit::tr("No", "否"),
        None => kit::tr("Unknown", "未知"),
    };
    let business = if business_re().is_match(&result.brand) {
        kit::tr("Yes", "是")
    } else {
        kit::tr("No", "否")
    };

    if zh {
        format!(
            "💳 <b>卡头查询</b>\n\n\
             🔢 卡头：{}\n\
             💳 品牌：{}\n\
             🔖 类型：{}\n\
             💹 等级：{}\n\n\
             🗺 国家：{}\n\
             💸 货币：{}\n\
             🏦 银行：{}\n\n\
             💰 预付卡：{prepaid}\n\
             🧾 商业卡：{business}",
            code(bin),
            escape(scheme),
            escape(kind),
            escape(&level),
            escape(&country),
            escape(&currency),
            escape(bank),
        )
    } else {
        format!(
            "💳 <b>BIN Lookup</b>\n\n\
             🔢 BIN: {}\n\
             💳 Scheme: {}\n\
             🔖 Type: {}\n\
             💹 Tier: {}\n\n\
             🗺 Country: {}\n\
             💸 Currency: {}\n\
             🏦 Bank: {}\n\n\
             💰 Prepaid: {prepaid}\n\
             🧾 Commercial: {business}",
            code(bin),
            escape(scheme),
            escape(kind),
            escape(&level),
            escape(&country),
            escape(&currency),
            escape(bank),
        )
    }
}

fn help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "💳 <b>卡头查询</b>\n\n查银行卡号前 6–8 位（BIN）对应的卡组织、卡种、国家和发卡行。\n\n\
             • <code>{p}bin 415042</code>\n\n\
             只会用前 8 位去查，多输入的数字在发请求之前就丢掉了。数据来自 binlist.net，免费额度很小，查多了会限流。"
        )
    } else {
        format!(
            "💳 <b>BIN Lookup</b>\n\nLook up card network, type, country, and issuer bank from the first 6–8 digits (BIN).\n\n\
             • <code>{p}bin 415042</code>\n\n\
             Only the first 8 digits are sent; extra digits are stripped locally. Powered by binlist.net (strict free rate limit)."
        )
    }
}

pub fn register(a: &Arc<App>) {
    a.registry.register(
        kit::make_cmd(
            "bin",
            &[],
            group::TOOLS,
            "Look up card BIN info",
            "<6-8 card digits>",
            help,
            move |inv: Invocation| {
                Box::pin(async move {
                    let input = inv.raw.trim();
                    if input.is_empty() {
                        return inv.edit(&help(inv.prefix())).await.map_err(Into::into);
                    }
                    let Some(bin) = bin_digits(input) else {
                        return Err(kit::fail_err(kit::tr(
                            "BIN requires at least 6 digits",
                            "卡头至少要 6 位数字",
                        )));
                    };
                    inv.edit_text(&kit::working(&if kit::is_zh() {
                        format!("正在查询卡头 {bin}")
                    } else {
                        format!("Looking up BIN {bin}")
                    }))
                    .await?;
                    let mut headers = HashMap::new();
                    headers.insert("Accept-Version".to_owned(), "3".to_owned());
                    let resp = do_request(Request {
                        url: format!("https://lookup.binlist.net/{bin}"),
                        headers,
                        timeout: Some(Duration::from_secs(15)),
                        max_bytes: Some(64 << 10),
                        ..Default::default()
                    })
                    .await
                    .map_err(|e| {
                        kit::fail_with_err(
                            kit::tr(
                                "Lookup service unreachable; try again later",
                                "查询服务暂时连不上，稍后再试",
                            ),
                            e,
                        )
                    })?;

                    if resp.status == 404 {
                        return Err(kit::fail_err(if kit::is_zh() {
                            format!("没有这个卡头的记录：{bin}")
                        } else {
                            format!("No record found for BIN {bin}")
                        }));
                    }
                    if resp.status == 429 {
                        return Err(kit::fail_err(kit::tr(
                            "Rate-limited by binlist.net (free tier allows only a few lookups per hour); try again later",
                            "binlist.net 限流了，免费额度每小时只有几次，稍后再试",
                        )));
                    }
                    if !resp.ok() {
                        return Err(kit::fail_err(if kit::is_zh() {
                            format!("查询服务出错了（HTTP {}），稍后再试", resp.status)
                        } else {
                            format!("Lookup service error (HTTP {}); try again later", resp.status)
                        }));
                    }
                    let result: BinResult = serde_json::from_slice(&resp.body).map_err(|_| {
                        kit::fail_err(kit::tr(
                            "Lookup service returned invalid response",
                            "查询服务返回了无法解析的内容",
                        ))
                    })?;
                    inv.edit(&render_bin(&bin, &result))
                        .await
                        .map_err(Into::into)
                })
            },
        )
        .zh("查卡头对应的发卡行", "卡号前 6–8 位"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bin_digits_never_sends_more_than_eight() {
        assert_eq!(bin_digits("4150 4212 3456 7890"), Some("41504212".into()));
        assert_eq!(bin_digits("415042"), Some("415042".into()));
        assert_eq!(bin_digits("41504"), None);
    }

    #[test]
    fn test_render_bin_from_a_real_response() {
        let raw = r#"{"number":{},"scheme":"visa","type":"credit","brand":"Visa Rewards","country":{"numeric":"643","alpha2":"RU","name":"Russian Federation (the)","emoji":"🇷🇺","currency":"RUB"},"bank":{"name":"(Ofac Sanctioned) Vtb Bank Pjsc"}}"#;
        let result: BinResult = serde_json::from_str(raw).unwrap();
        let text = render_bin("415042", &result);
        for wanted in [
            "Visa",
            "Credit",
            "REWARDS",
            "Russian Federation",
            "RUB",
            "Vtb Bank",
            "Prepaid: Unknown",
        ] {
            assert!(text.contains(wanted), "missing {wanted} in:\n{text}");
        }
        assert!(!text.contains("(the)"), "{text}");
    }
}
