use crate::kit;
use mibot_app::App;
use mibot_bot::{IpPolicy, current_ip_policy, set_ip_policy};
use mibot_command::{Invocation, escape, group};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrivacyDoc {
    mode: String,
    ipv4: u8,
    ipv6: u8,
}

impl Default for PrivacyDoc {
    fn default() -> Self {
        let p = IpPolicy::default();
        Self {
            mode: p.mode,
            ipv4: p.ipv4_segments,
            ipv6: p.ipv6_segments,
        }
    }
}

impl From<IpPolicy> for PrivacyDoc {
    fn from(p: IpPolicy) -> Self {
        Self {
            mode: p.mode,
            ipv4: p.ipv4_segments,
            ipv6: p.ipv6_segments,
        }
    }
}

impl From<PrivacyDoc> for IpPolicy {
    fn from(d: PrivacyDoc) -> Self {
        Self {
            mode: d.mode,
            ipv4_segments: d.ipv4,
            ipv6_segments: d.ipv6,
        }
    }
}

pub fn describe(policy: &IpPolicy) -> String {
    if policy.mode == "hide" {
        return kit::tr("Hide completely", "完全隐藏").to_owned();
    }
    if kit::is_zh() {
        format!(
            "IPv4 末尾 {} 段、IPv6 末尾 {} 段打码",
            policy.ipv4_segments, policy.ipv6_segments
        )
    } else {
        format!(
            "Mask last {} IPv4, {} IPv6 segments",
            policy.ipv4_segments, policy.ipv6_segments
        )
    }
}

fn usage(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        return format!(
            "<code>{p}privacy ip mask 2 4</code> IPv4 遮后 2 段、IPv6 遮后 4 段（IPv4 1–4，IPv6 1–8，IPv6 可省略）\n\
             <code>{p}privacy ip hide</code> 整个地址换成「[IP已隐藏]」"
        );
    }
    format!(
        "<code>{p}privacy ip mask 2 4</code> Mask last 2 IPv4 and 4 IPv6 segments (IPv4 1–4, IPv6 1–8; IPv6 optional)\n\
         <code>{p}privacy ip hide</code> Replace full address with “[IP hidden]”"
    )
}

pub fn help(prefix: &str) -> String {
    if kit::is_zh() {
        return format!(
            "🛡 <b>IP 显示</b>\n\n账号发出和编辑的每条消息，里面的 IP 地址都按这里的设置打码，文件名里的也一样；\
             指向 IP 的链接会去掉。默认 IPv4 遮后 2 段、IPv6 遮后 4 段。\n\n{}\n\n\
             只影响之后发出的消息，已经发出去的不会改。图片、附件内容不在范围内。替别人代发的命令（.sudo、.sure）不打码，\
             那些地址本来就是对方自己打出来的。",
            usage(prefix)
        );
    }
    format!(
        "🛡 <b>IP Privacy</b>\n\nEvery message and filename sent or edited by this account masks IP addresses according to this policy, and strips links pointing to IPs. Default masks the last 2 IPv4 and 4 IPv6 segments.\n\n{}\n\n\
         Applies only to future outgoing messages; previously sent messages are unchanged. Images and attachment contents are not affected. Relayed commands (.sudo, .sure) are exempt since those addresses were typed by the sender.",
        usage(prefix)
    )
}

pub fn register(a: &Arc<App>) {
    let saved = Arc::new(kit::new_store(a, "privacy.json", PrivacyDoc::default));
    if let Ok(current) = saved.read() {
        let policy: IpPolicy = current.into();
        let _ = set_ip_policy(policy);
    }

    a.registry.register(
        kit::make_cmd(
            "privacy",
            &[],
            group::SYSTEM,
            "Configure IP masking policy",
            "[ip mask 2 4|ip hide]",
            help,
            move |inv: Invocation| {
                let saved = Arc::clone(&saved);
                Box::pin(async move {
                    let scope = inv.arg(0).to_owned();
                    let action = inv.arg(1).to_ascii_lowercase();
                    if scope.is_empty() {
                        let header = if kit::is_zh() {
                            format!(
                                "🛡 IP 显示：{}\n\n{}",
                                escape(&describe(&current_ip_policy())),
                                usage(inv.prefix())
                            )
                        } else {
                            format!(
                                "🛡 IP Privacy: {}\n\n{}",
                                escape(&describe(&current_ip_policy())),
                                usage(inv.prefix())
                            )
                        };
                        return inv.edit(&header).await.map_err(Into::into);
                    }
                    let usage_msg = || format!("{}\n{}", kit::tr("Usage:", "用法："), usage(inv.prefix()));
                    if !scope.eq_ignore_ascii_case("ip") {
                        return inv.edit(&usage_msg()).await.map_err(Into::into);
                    }
                let next = if action == "hide" && inv.args.len() == 2 {
                    let mut p = current_ip_policy();
                    p.mode = "hide".to_owned();
                    Some(p)
                } else if action == "mask" && (inv.args.len() == 3 || inv.args.len() == 4) {
                    let mut p = current_ip_policy();
                    p.mode = "mask".to_owned();
                    if let Ok(v4) = inv.arg(2).parse::<u8>() {
                        p.ipv4_segments = v4;
                        if inv.arg(3).is_empty() {
                            Some(p)
                        } else if let Ok(v6) = inv.arg(3).parse::<u8>() {
                            p.ipv6_segments = v6;
                            Some(p)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let Some(next) = next else {
                    return inv.edit(&usage_msg()).await.map_err(Into::into);
                };
                if next.validate().is_err() {
                    return inv.edit(&usage_msg()).await.map_err(Into::into);
                }
                let doc = PrivacyDoc::from(next.clone());
                saved.update(|value| {
                    *value = doc;
                    Ok::<(), std::convert::Infallible>(())
                })?;
                let _ = set_ip_policy(next.clone());
                let updated = if kit::is_zh() {
                    format!("✅ IP 显示已更新：{}", describe(&next))
                } else {
                    format!("✅ IP privacy updated: {}", describe(&next))
                };
                inv.edit_text(&updated).await.map_err(Into::into)
            })
        },
        )
        .zh("设置 IP 地址打码方式", "[ip mask 2 4|ip hide]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_describe() {
        let got = describe(&IpPolicy::default());
        assert!(got.contains("2 IPv4") && got.contains("4 IPv6"), "got: {got}");
        assert_eq!(
            describe(&IpPolicy {
                mode: "hide".into(),
                ipv4_segments: 2,
                ipv6_segments: 4,
            }),
            "Hide completely"
        );
        let h = help(".");
        assert!(h.contains(".privacy ip mask 2 4") && h.contains(".privacy ip hide"));
        mibot_i18n::with_lang(mibot_i18n::Lang::Zh, || {
            let zh = describe(&IpPolicy::default());
            assert!(zh.contains("IPv4 末尾 2 段") && zh.contains("IPv6 末尾 4 段"));
        });
    }
}
