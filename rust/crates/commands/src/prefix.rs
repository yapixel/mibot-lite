use crate::kit;
use mibot_app::App;
use mibot_command::{Invocation, code, escape, group};
use mibot_logtail::Level;
use std::sync::Arc;
use tokio::sync::Mutex;

const PREFIX_LIMIT: usize = 8;

fn prefix_help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        format!(
            "🛠 <b>命令前缀</b>\n\n\
             • <code>{p}prefix</code> 查看当前前缀\n\
             • <code>{p}prefix set . ！</code> 换成这几个\n\
             • <code>{p}prefix add ~</code> 追加\n\
             • <code>{p}prefix del $</code> 删除\n\n\
             <b>规则</b>\n\
             • 可以同时有几个前缀，至少留一个；重复的只算一次。\n\
             • 前缀不能含英文字母、数字、下划线：前缀后面紧跟命令名，没有分隔，含这些字符就分不清前缀到哪里结束。\n\
             • 前缀不能含引号，最长 8 个字符。\n\
             • 改完立刻生效，并写进 <code>.env</code>，重启后仍然有效；写不进去的话只在本次运行里生效。\n\
             • 只看消息的第一行。"
        )
    } else {
        format!(
            "🛠 <b>Command Prefixes</b>\n\n\
             • <code>{p}prefix</code> View current prefixes\n\
             • <code>{p}prefix set . !</code> Replace with these\n\
             • <code>{p}prefix add ~</code> Append prefix\n\
             • <code>{p}prefix del $</code> Remove prefix\n\n\
             <b>Rules</b>\n\
             • Multiple prefixes allowed, at least one required; duplicates ignored.\n\
             • No ASCII letters, digits, or underscores (no separator between prefix and command).\n\
             • No quotes; maximum length 8 characters.\n\
             • Takes effect immediately and persists to <code>.env</code> when writable.\n\
             • Only the first line is parsed."
        )
    }
}

pub fn check_prefix(prefix: &str) -> &'static str {
    if prefix.chars().count() > PREFIX_LIMIT {
        return kit::tr("too long, max 8 characters", "太长，前缀最多 8 个字符");
    }
    for ch in prefix.chars() {
        if (ch as u32) < 0x80 && (ch.is_ascii_alphabetic() || ch.is_ascii_digit() || ch == '_') {
            return kit::tr(
                "contains ASCII letters, digits, or '_', ambiguous before command names",
                "含英文字母、数字或下划线，会和后面的命令名连在一起分不开",
            );
        }
        if ch == '"' || ch == '\'' {
            return kit::tr(
                "contains quotes, which change when saved to .env",
                "含引号，写进 .env 后读回来会变",
            );
        }
        if ch.is_whitespace() || ch.is_control() {
            return kit::tr(
                "prefix cannot contain whitespace or control characters",
                "前缀里不能有空白或控制字符",
            );
        }
    }
    ""
}

fn check_prefix_msg(prefix: &str) -> Option<String> {
    if prefix.chars().count() > PREFIX_LIMIT {
        return Some(if kit::is_zh() {
            format!("「{prefix}」太长，前缀最多 8 个字符")
        } else {
            format!("\"{prefix}\" is too long (max 8 characters)")
        });
    }
    for ch in prefix.chars() {
        if (ch as u32) < 0x80 && (ch.is_ascii_alphabetic() || ch.is_ascii_digit() || ch == '_') {
            return Some(if kit::is_zh() {
                format!("「{prefix}」含英文字母、数字或下划线，会和后面的命令名连在一起分不开")
            } else {
                format!("\"{prefix}\" contains ASCII letters, digits, or '_'")
            });
        }
        if ch == '"' || ch == '\'' {
            return Some(if kit::is_zh() {
                format!("「{prefix}」含引号，写进 .env 后读回来会变")
            } else {
                format!("\"{prefix}\" contains quotes, which break .env parsing")
            });
        }
        if ch.is_whitespace() || ch.is_control() {
            return Some(
                kit::tr(
                    "prefix cannot contain whitespace or control characters",
                    "前缀里不能有空白或控制字符",
                )
                .to_owned(),
            );
        }
    }
    None
}

pub fn next_prefixes(action: &str, current: &[String], tokens: &[String]) -> Vec<String> {
    let candidates: Vec<String> = match action {
        "set" => tokens.to_vec(),
        "add" => current.iter().chain(tokens.iter()).cloned().collect(),
        _ => current
            .iter()
            .filter(|p| !tokens.iter().any(|t| t == *p))
            .cloned()
            .collect(),
    };
    let mut result = Vec::new();
    for p in candidates {
        if !result.contains(&p) {
            result.push(p);
        }
    }
    result
}

fn quote_prefixes(prefixes: &[String]) -> String {
    prefixes
        .iter()
        .map(|p| code(p))
        .collect::<Vec<_>>()
        .join(" • ")
}

pub fn register(a: &Arc<App>) {
    let app = Arc::clone(a);
    let mu = Arc::new(Mutex::new(()));
    a.registry.register(
        kit::make_cmd(
            "prefix",
            &[],
            group::SYSTEM,
            "Manage command prefixes",
            "[set|add|del prefix…]",
            prefix_help,
            move |inv: Invocation| {
                let app = Arc::clone(&app);
                let mu = Arc::clone(&mu);
                Box::pin(async move {
                    let _guard = mu.lock().await;
                    let first_line = inv.message.text.split('\n').next().unwrap_or("");
                    let fields: Vec<String> =
                        first_line.split_whitespace().map(str::to_owned).collect();
                    let args = if fields.len() > 1 {
                        &fields[1..]
                    } else {
                        &[][..]
                    };
                    let current = app.registry.prefixes();
                    let action = args
                        .first()
                        .map(|s| s.to_ascii_lowercase())
                        .unwrap_or_default();

                    match action.as_str() {
                        "" => {
                            return inv
                                .edit(&format!(
                                    "🛠 {}{}\n<i>{}</i>",
                                    kit::tr("Current prefixes: ", "当前前缀："),
                                    quote_prefixes(&current),
                                    if kit::is_zh() {
                                        format!(
                                            "{} 修改，{} 看说明",
                                            escape(&format!("{}prefix set . ！", inv.prefix())),
                                            escape(&format!("{}prefix help", inv.prefix())),
                                        )
                                    } else {
                                        format!(
                                            "{} to change, {} for help",
                                            escape(&format!("{}prefix set . !", inv.prefix())),
                                            escape(&format!("{}prefix help", inv.prefix())),
                                        )
                                    }
                                ))
                                .await
                                .map_err(Into::into);
                        }
                        "set" | "add" | "del" => {}
                        _ => {
                            return inv
                                .edit(&prefix_help(inv.prefix()))
                                .await
                                .map_err(Into::into);
                        }
                    }

                    let tokens = &args[1..];
                    if tokens.is_empty() {
                        return Err(kit::fail_err(if kit::is_zh() {
                            format!("{action} 后面要跟前缀，例如 {}prefix {action} ！", inv.prefix())
                        } else {
                            format!("missing prefix after {action}, e.g. {}prefix {action} !", inv.prefix())
                        }));
                    }
                    if action != "del" {
                        for token in tokens {
                            if let Some(problem) = check_prefix_msg(token) {
                                return Err(kit::fail_err(problem));
                            }
                        }
                    }
                    let next = next_prefixes(&action, &current, tokens);
                    if next.is_empty() {
                        return Err(kit::fail_err(kit::tr(
                            "At least one prefix must remain",
                            "至少要保留一个前缀",
                        )));
                    }
                    app.registry.set_prefixes(next.clone());

                    let mut note = kit::tr(
                        "Saved to .env and persists across restarts",
                        "已写入 .env，重启后仍然有效",
                    )
                    .to_owned();
                    if let Err(err) =
                        mibot_config::set_env(&app.root, "MIBOT_PREFIX", &next.join(" "))
                    {
                        let err_s = err.to_string();
                        app.logs.record(
                            Level::Error,
                            "prefix.persist_failed",
                            &[("error", &err_s)],
                        );
                        note = kit::tr(
                            "⚠️ Failed to write .env; applies to this run only",
                            "⚠️ 写入 .env 失败，只在本次运行里生效",
                        )
                        .to_owned();
                    } else if let Ok(val) = std::env::var("MIBOT_PREFIX")
                        && !val.is_empty()
                    {
                        note = kit::tr(
                            "⚠️ Saved to .env, but MIBOT_PREFIX in process env takes precedence on restart",
                            "⚠️ 已写入 .env，但服务的环境变量里也设了 MIBOT_PREFIX，重启后会以那个为准",
                        )
                        .to_owned();
                    }

                    inv.edit(&format!(
                        "✅ {}{}\n<i>{}</i>\n\n{}{}",
                        kit::tr("Prefixes updated: ", "前缀已改为："),
                        quote_prefixes(&next),
                        escape(&note),
                        kit::tr("Example: ", "以后这样用："),
                        code(&format!("{}help", next[0]))
                    ))
                    .await
                    .map_err(Into::into)
                })
            },
        )
        .zh("查看或修改命令前缀", "[set|add|del 前缀…]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_prefix() {
        for good in [".", "。", "！", "~", "$", "!!", "喵"] {
            assert!(check_prefix_msg(good).is_none(), "{good:?} rejected");
        }
        for bad in ["a", "1", "_", ".a", "\"", "'", "~~~~~~~~~", "\t"] {
            assert!(check_prefix_msg(bad).is_some(), "{bad:?} accepted");
        }
    }

    #[test]
    fn test_next_prefixes() {
        let current: Vec<String> = [".", "。", "$"].into_iter().map(String::from).collect();
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("set", &["!", "！", "!"], &["!", "！"]),
            ("add", &["~", "."], &[".", "。", "$", "~"]),
            ("del", &["$", "。"], &["."]),
            ("del", &[".", "。", "$"], &[]),
        ];
        for &(action, tokens, want) in cases {
            let toks: Vec<String> = tokens.iter().map(|s| (*s).to_owned()).collect();
            let got = next_prefixes(action, &current, &toks);
            let want_v: Vec<String> = want.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(got, want_v, "{action} {tokens:?}");
        }
    }
}
