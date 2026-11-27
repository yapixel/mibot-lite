use crate::kit;
use mibot_app::App;
use mibot_command::{Invocation, code, escape, group};
use mibot_logtail::Level;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AliasDocument {
    #[serde(default)]
    aliases: HashMap<String, String>,
}

fn alias_help(prefix: &str) -> String {
    let p = escape(prefix);
    if kit::is_zh() {
        return format!(
            "🔀 <b>命令别名</b>\n\n给命令起个顺手的名字，可以连参数一起。\n\n\
             • <code>{p}alias</code> 列出所有别名\n\
             • <code>{p}alias set 别名 原命令 [参数]</code> 添加或修改\n\
             • <code>{p}alias del 别名</code> 删除\n\n\
             <b>例子</b>\n\
             <code>{p}alias set 测速 speedtest 48463</code>\n之后发 <code>{p}测速</code> 就等于 <code>{p}speedtest 48463</code>。\n\n\
             <code>{p}alias set 译 gt</code>\n之后 <code>{p}译 你好</code> 等于 <code>{p}gt 你好</code>，别名后面的内容原样带过去，换行也保留。\n\n\
             <b>规则</b>\n\
             • 原命令写名字就行，不用带前缀。\n\
             • 别名可以是中文，也可以是几个词。\n\
             • 不能给别名再起别名；和现有命令同名的单字别名不会生效，所以不让设。\n\
             • 同一条原命令只留一个别名，设新的会替换旧的。"
        );
    }
    format!(
        "🔀 <b>Command Aliases</b>\n\nGive commands a shorter or more convenient name, optionally with arguments.\n\n\
         • <code>{p}alias</code> List all aliases\n\
         • <code>{p}alias set name cmd [args]</code> Add or update an alias\n\
         • <code>{p}alias del name</code> Delete an alias\n\n\
         <b>Examples</b>\n\
         <code>{p}alias set fast speedtest 48463</code>\nSending <code>{p}fast</code> runs <code>{p}speedtest 48463</code>.\n\n\
         <code>{p}alias set tl gt</code>\nSending <code>{p}tl hello</code> runs <code>{p}gt hello</code>, preserving trailing text and newlines.\n\n\
         <b>Rules</b>\n\
         • Write the target command without its prefix.\n\
         • Alias names can contain any characters or multiple words.\n\
         • Aliases cannot target other aliases, nor override existing command names.\n\
         • Only one alias is kept per target command and args; setting a new one replaces the old one."
    )
}

pub fn split_alias<F>(tokens: &[&str], mut is_command: F) -> Option<(String, String)>
where
    F: FnMut(&str) -> bool,
{
    for idx in 1..tokens.len() {
        if is_command(tokens[idx]) {
            return Some((tokens[..idx].join(" "), tokens[idx..].join(" ")));
        }
    }
    None
}

pub fn render_aliases(aliases: &HashMap<String, String>, prefix: &str) -> String {
    let p = escape(prefix);
    if aliases.is_empty() {
        return if kit::is_zh() {
            format!(
                "🔀 还没有别名。\n\n例如 <code>{p}alias set 测速 speedtest</code>，之后发 <code>{p}测速</code> 就能测速。"
            )
        } else {
            format!(
                "🔀 No aliases yet.\n\nFor example, <code>{p}alias set fast speedtest</code> lets you run speedtest with <code>{p}fast</code>."
            )
        };
    }
    let mut names: Vec<&String> = aliases.keys().collect();
    names.sort();
    let mut lines = vec![
        kit::tr("🔀 <b>Command Aliases</b>", "🔀 <b>命令别名</b>").to_owned(),
        String::new(),
    ];
    for name in names {
        if let Some(target) = aliases.get(name) {
            lines.push(format!(
                "{} → {}",
                code(&format!("{prefix}{name}")),
                code(&format!("{prefix}{target}"))
            ));
        }
    }
    lines.push(String::new());
    if kit::is_zh() {
        lines.push(format!(
            "<i>{} 添加，{} 删除</i>",
            escape(&format!("{prefix}alias set 别名 原命令")),
            escape(&format!("{prefix}alias del 别名"))
        ));
    } else {
        lines.push(format!(
            "<i>{} to add, {} to delete</i>",
            escape(&format!("{prefix}alias set name cmd")),
            escape(&format!("{prefix}alias del name"))
        ));
    }
    lines.join("\n")
}

pub fn register(a: &Arc<App>) {
    let saved = Arc::new(kit::new_store(a, "alias.json", AliasDocument::default));
    match saved.read() {
        Ok(current) => a.registry.set_aliases(&current.aliases),
        Err(err) => kit::warn(a, "alias.load_failed", &err),
    }

    let app = Arc::clone(a);
    a.registry.register(
        kit::make_cmd(
            "alias",
            &[],
            group::SYSTEM,
            "Create command aliases",
            "[set name cmd|del name]",
            alias_help,
            move |inv: Invocation| {
                let app = Arc::clone(&app);
                let saved = Arc::clone(&saved);
                Box::pin(async move {
                    let publish = || {
                        if let Ok(current) = saved.read() {
                            app.registry.set_aliases(&current.aliases);
                        }
                    };
                    let is_command = |name: &str| app.registry.command(name).is_some();

                    match inv.arg(0).to_ascii_lowercase().as_str() {
                        "" | "ls" | "list" => {
                            return inv
                                .edit(&render_aliases(&app.registry.aliases(), inv.prefix()))
                                .await
                                .map_err(Into::into);
                        }
                        "del" | "rm" | "delete" => {
                            let name = inv.raw_after(1).trim().to_owned();
                            if name.is_empty() {
                                return Err(kit::usage(
                                    inv.prefix(),
                                    kit::tr("alias del name", "alias del 别名"),
                                ));
                            }
                            let mut removed = false;
                            saved.update(|doc| {
                                removed = doc.aliases.remove(&name).is_some();
                                Ok::<(), std::convert::Infallible>(())
                            })?;
                            publish();
                            if !removed {
                                let msg = if kit::is_zh() {
                                    format!("没有叫 {name} 的别名，{}alias 可以看全部", inv.prefix())
                                } else {
                                    format!(
                                        "No alias named {name}; run {}alias to list all",
                                        inv.prefix()
                                    )
                                };
                                return Err(kit::fail_err(msg));
                            }
                            let msg = if kit::is_zh() {
                                format!("✅ 已删除别名 {}", code(&format!("{}{name}", inv.prefix())))
                            } else {
                                format!("✅ Deleted alias {}", code(&format!("{}{name}", inv.prefix())))
                            };
                            return inv.edit(&msg).await.map_err(Into::into);
                        }
                        "set" | "add" => {}
                        _ => {
                            return inv
                                .edit(&alias_help(inv.prefix()))
                                .await
                                .map_err(Into::into);
                        }
                    }

                    let tokens: Vec<&str> = inv.args[1..].iter().map(String::as_str).collect();
                    if tokens.len() < 2 {
                        return Err(kit::usage(
                            inv.prefix(),
                            kit::tr("alias set name cmd [args]", "alias set 别名 原命令 [参数]"),
                        ));
                    }
                    let Some((name, target)) = split_alias(&tokens, is_command) else {
                        let last = tokens[tokens.len() - 1];
                        let second = tokens[1];
                        let has_prefix =
                            last.starts_with(inv.prefix()) || second.starts_with(inv.prefix());
                        let msg = if kit::is_zh() {
                            let mut hint = "后面没有找到存在的命令".to_owned();
                            if has_prefix {
                                hint.push_str("；原命令不用带前缀");
                            }
                            format!("{hint}。例如 {}alias set 测速 speedtest", inv.prefix())
                        } else {
                            let mut hint =
                                "No existing command found after the alias name".to_owned();
                            if has_prefix {
                                hint.push_str("; omit the prefix on the target command");
                            }
                            format!("{hint}. Example: {}alias set fast speedtest", inv.prefix())
                        };
                        return Err(kit::fail_err(msg));
                    };
                    if is_command(&name) {
                        let msg = if kit::is_zh() {
                            format!("{name} 本身就是一个命令，同名的别名永远不会生效")
                        } else {
                            format!(
                                "{name} is already a command; an alias with the same name would never trigger"
                            )
                        };
                        return Err(kit::fail_err(msg));
                    }
                    let first = target.split_whitespace().next().unwrap_or("");
                    if app.registry.aliases().contains_key(first) && !is_command(first) {
                        let msg = if kit::is_zh() {
                            format!("{first} 是别名，不能再给别名起别名")
                        } else {
                            format!("{first} is an alias; you cannot alias an alias")
                        };
                        return Err(kit::fail_err(msg));
                    }

                    let mut replaced = String::new();
                    saved.update(|doc| {
                        let others: Vec<String> = doc
                            .aliases
                            .iter()
                            .filter(|(k, v)| v.as_str() == target && k.as_str() != name)
                            .map(|(k, _)| k.clone())
                            .collect();
                        for other in others {
                            replaced = other.clone();
                            doc.aliases.remove(&other);
                        }
                        doc.aliases.insert(name.clone(), target.clone());
                        Ok::<(), std::convert::Infallible>(())
                    })?;
                    publish();
                    let mut text = format!(
                        "✅ {} → {}",
                        code(&format!("{}{name}", inv.prefix())),
                        code(&format!("{}{target}", inv.prefix()))
                    );
                    if !replaced.is_empty() {
                        if kit::is_zh() {
                            text.push_str(&format!(
                                "\n<i>原来的别名 {} 已替换</i>",
                                escape(&format!("{}{replaced}", inv.prefix()))
                            ));
                        } else {
                            text.push_str(&format!(
                                "\n<i>Replaced previous alias {}</i>",
                                escape(&format!("{}{replaced}", inv.prefix()))
                            ));
                        }
                    }
                    let _ = Level::Info;
                    inv.edit(&text).await.map_err(Into::into)
                })
            },
        )
        .zh("给命令起别名", "[set 别名 原命令|del 别名]"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_alias() {
        let is_cmd = |n: &str| matches!(n, "speedtest" | "ping" | "version");
        assert_eq!(
            split_alias(&["测速", "speedtest", "48463"], is_cmd),
            Some(("测速".into(), "speedtest 48463".into()))
        );
        assert_eq!(
            split_alias(&["ping", "now", "version"], is_cmd),
            Some(("ping now".into(), "version".into()))
        );
        assert_eq!(split_alias(&["a", "b", "c"], is_cmd), None);
        assert_eq!(split_alias(&["测速", ".speedtest"], is_cmd), None);
    }

    #[test]
    fn test_render_aliases() {
        let empty = render_aliases(&HashMap::new(), ".");
        assert!(empty.contains(".alias set fast speedtest"), "{empty}");
        mibot_i18n::with_lang(mibot_i18n::Lang::Zh, || {
            let empty_zh = render_aliases(&HashMap::new(), ".");
            assert!(empty_zh.contains(".alias set 测速 speedtest"), "{empty_zh}");
        });
        let mut map = HashMap::new();
        map.insert("译".into(), "gt".into());
        map.insert("测速".into(), "speedtest 48463".into());
        let listed = render_aliases(&map, ".");
        assert!(listed.contains(".测速") && listed.contains(".speedtest 48463"), "{listed}");
    }
}
