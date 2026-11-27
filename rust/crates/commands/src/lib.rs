pub mod aban;
pub mod acn;
pub mod acron;
pub mod aff;
pub mod ai;
pub mod alias;
pub mod bf;
pub mod bgp;
pub mod bin;
pub mod calc;
pub mod checkapi;
pub mod checkin;
pub mod cleanmember;
pub mod core;
pub mod da;
pub mod ddg;
pub mod dig;
pub mod dme;
pub mod eatgif;
pub mod encode;
pub mod gt;
pub mod guestban;
pub mod ids;
pub mod ip;
pub mod keyword;
pub mod kit;
pub mod log;
pub mod prefix;
pub mod privacy;
pub mod rate;
pub mod re;
pub mod restart;
pub mod save;
pub mod search;
pub mod service;
pub mod speedtest;
pub mod sticker;
pub mod subinfo;
pub mod sudo;
pub mod sum;
pub mod tr;
pub mod trace;
pub mod tts;
pub mod update;
pub mod whois;
pub mod yvlu;

use mibot_app::{App, AppError, lock_root};
use mibot_fsutil::write_file_atomic;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// Registers all command groups onto `app`.
pub fn register_all(a: &Arc<App>) {
    core::register(a);
    let restarter = restart::register(a);
    update::register(a, Some(restarter));
    calc::register(a);
    rate::register(a);
    whois::register(a);
    let models = ai::register(a);
    gt::register(a, Some(Arc::clone(&models)));
    tr::register(a);
    speedtest::register(a);
    service::register(a);
    log::register(a);
    bf::register(a);
    save::register(a);
    ip::register(a);
    dig::register(a);
    bgp::register(a);
    ddg::register(a);
    encode::register(a);
    subinfo::register(a);
    bin::register(a);
    ids::register(a);
    sum::register(a, Some(Arc::clone(&models)));
    checkapi::register(a, Some(models));
    re::register(a);
    search::register(a);
    aff::register(a);
    acron::register(a);
    trace::register(a);
    dme::register(a);
    da::register(a);
    aban::register(a);
    keyword::register(a);
    guestban::register(a);
    cleanmember::register(a);
    acn::register(a);
    checkin::register(a);
    yvlu::register(a);
    eatgif::register(a);
    sticker::register(a);
    tts::register(a);
    privacy::register(a);
    prefix::register(a);
    alias::register(a);
    sudo::register(a);
}

type ConvertFn = fn(&[u8]) -> Result<Vec<u8>, String>;

struct MiboxImport {
    source: &'static str,
    target: &'static str,
    convert: Option<ConvertFn>,
}

fn convert_ai(raw: &[u8]) -> Result<Vec<u8>, String> {
    ai::convert_mibox(raw).map_err(|e| e.to_string())
}

fn convert_checkapi(raw: &[u8]) -> Result<Vec<u8>, String> {
    checkapi::convert_legacy(raw).map_err(|e| e.to_string())
}

fn convert_save(raw: &[u8]) -> Result<Vec<u8>, String> {
    save::convert_mibox(raw).map_err(|e| e.to_string())
}

fn convert_speedtest(raw: &[u8]) -> Result<Vec<u8>, String> {
    speedtest::convert_mibox(raw)
}

fn convert_aliases(raw: &[u8]) -> Result<Vec<u8>, String> {
    let aliases = mibot_mibox::aliases(raw).map_err(|e| e.to_string())?;
    serde_json::to_vec_pretty(&serde_json::json!({ "aliases": aliases }))
        .map_err(|e| e.to_string())
}

const MIBOX_IMPORTS: &[MiboxImport] = &[
    MiboxImport {
        source: "assets/ai/config.json",
        target: "ai.json",
        convert: Some(convert_ai),
    },
    MiboxImport {
        source: "assets/sum/database.json",
        target: "sum.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/da/database.json",
        target: "da.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/dme/config.json",
        target: "dme.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/whois/whois_data.json",
        target: "whois.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/whois/data.json",
        target: "whois.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/aban/aban_cache.json",
        target: "aban.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/autochangename/autochangename.json",
        target: "acn.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/yvlu/config.json",
        target: "yvlu.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/t/tts_data.json",
        target: "t.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/sticker/config.json",
        target: "sticker.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/privacy/ip.json",
        target: "privacy.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/checkin/checkin_config.json",
        target: "checkin.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/acron/acron_config.json",
        target: "acron.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/aff/data.json",
        target: "aff.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/keyword/config.json",
        target: "keyword.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/trace/db.json",
        target: "trace.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/search/channel_search_config.json",
        target: "search.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/checkapi/keys-v2.json",
        target: "checkapi.json",
        convert: None,
    },
    MiboxImport {
        source: "assets/checkapi/keys.json",
        target: "checkapi.json",
        convert: Some(convert_checkapi),
    },
    MiboxImport {
        source: "assets/save/config.json",
        target: "save.json",
        convert: Some(convert_save),
    },
    MiboxImport {
        source: "assets/prometheus/config.json",
        target: "save.json",
        convert: Some(convert_save),
    },
    MiboxImport {
        source: "assets/speedtest/v2-config.json",
        target: "speedtest.json",
        convert: Some(convert_speedtest),
    },
    MiboxImport {
        source: "assets/speedtest/speedtest.json",
        target: "speedtest.json",
        convert: Some(convert_speedtest),
    },
    MiboxImport {
        source: "assets/alias/alias.db",
        target: "alias.json",
        convert: Some(convert_aliases),
    },
];

/// Migrates plugin data and `TB_PREFIX` from a MiBox deployment into `data_dir` without overwriting.
pub fn import_mibox(
    mibox_root: &Path,
    data_dir: &Path,
    out: &mut dyn Write,
) -> Result<(), AppError> {
    let meta = fs::metadata(mibox_root)?;
    if !meta.is_dir() {
        return Err(AppError::Message(format!(
            "{} is not a MiBox deployment directory",
            mibox_root.display()
        )));
    }
    let root = data_dir.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o700));
    }
    let _lock = lock_root(root)?;
    fs::create_dir_all(data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700));
    }

    let mut imported = 0usize;
    let mut written: HashSet<&str> = HashSet::new();
    for item in MIBOX_IMPORTS {
        if written.contains(item.target) {
            continue;
        }
        let src = mibox_root.join(item.source);
        let raw = match fs::read(&src) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let dest = data_dir.join(item.target);
        match fs::symlink_metadata(&dest) {
            Ok(_) => {
                let _ = writeln!(out, "skip {} (already exists)", dest.display());
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let final_bytes = if let Some(conv) = item.convert {
            let wal = mibox_root.join(format!("{}-wal", item.source));
            if let Ok(wmeta) = fs::metadata(&wal)
                && wmeta.len() > 0
            {
                let _ = writeln!(
                    out,
                    "warning: {}-wal exists; stop MiBox first so recent changes are merged into {}",
                    item.source, item.source
                );
            }
            match conv(&raw) {
                Ok(converted) => converted,
                Err(err) => {
                    let _ = writeln!(out, "skip {}: {err}", item.source);
                    continue;
                }
            }
        } else {
            raw
        };
        write_file_atomic(&dest, &final_bytes, 0o600)?;
        written.insert(item.target);
        let _ = writeln!(out, "imported {} -> {}", item.source, dest.display());
        imported += 1;
    }
    let _ = writeln!(out, "{imported} file(s) imported");
    import_prefix(mibox_root, root, out)
}

fn import_prefix(mibox_root: &Path, root: &Path, out: &mut dyn Write) -> Result<(), AppError> {
    let mibox_env = mibot_config::read_env(mibox_root, std::iter::empty::<(&str, &str)>());
    let Some(prefixes) = mibox_env.0.get("TB_PREFIX") else {
        return Ok(());
    };
    if prefixes.trim().is_empty() {
        return Ok(());
    }
    let target_env = mibot_config::read_env(root, std::iter::empty::<(&str, &str)>());
    if target_env.0.contains_key("MIBOT_PREFIX") {
        let _ = writeln!(out, "skip TB_PREFIX (MIBOT_PREFIX already set)");
        return Ok(());
    }
    let value = prefixes.split_whitespace().collect::<Vec<_>>().join(" ");
    mibot_config::set_env(root, "MIBOT_PREFIX", &value)?;
    let _ = writeln!(out, "imported TB_PREFIX -> MIBOT_PREFIX={value}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_mtsender::InvocationError;
    use grammers_tl_types::{self as tl, Deserializable, Identifiable, Serializable};
    use mibot_bot::{BoxFuture, Client, PeerCache, RpcTransport, SentRecorder, parse_html};
    use mibot_command::{GROUPS, Invocation};
    use mibot_logtail::Ring;
    use regex::Regex;
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "mibot-commands-lib-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_app(root: PathBuf, read_only: bool) -> Arc<App> {
        struct DummyTransport;
        impl RpcTransport for DummyTransport {
            fn invoke_raw<'a>(
                &'a self,
                _dc: Option<i32>,
                _id: u32,
                _body: Vec<u8>,
            ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                Box::pin(async { Err(InvocationError::Dropped("dummy")) })
            }
        }
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            Arc::new(DummyTransport),
            Arc::clone(&peers),
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));
        let mut a = Arc::try_unwrap(App::for_testing(
            root,
            "dev",
            "test-boot",
            client,
            peers,
            Arc::new(Ring::new()),
        ))
        .unwrap_or_else(|_| panic!("unique Arc"));
        a.read_only = read_only;
        let a = Arc::new(a);
        register_all(&a);
        a
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
    }

    fn is_emoji(ch: char) -> bool {
        let cp = ch as u32;
        (0x2190..=0x2BFF).contains(&cp)
            || (0x1F000..=0x1FAFF).contains(&cp)
            || cp == 0xFE0F
            || cp == 0x200D
            || cp == 0x20E3
    }

    fn leading_emoji(text: &str) -> &str {
        let mut end = 0;
        for (idx, ch) in text.char_indices() {
            if !is_emoji(ch) {
                break;
            }
            end = idx + ch.len_utf8();
        }
        &text[..end]
    }

    #[test]
    fn test_command_text_style() {
        let dir = TempDirGuard::new("style");
        let a = test_app(dir.0.clone(), false);
        let han_ascii = Regex::new(r"\p{Han}[:(]").expect("han_ascii");
        let emoji_in_bold =
            Regex::new(r"<b>[\x{2190}-\x{2BFF}\x{1F000}-\x{1FAFF}]").expect("emoji_bold");
        let status_emojis = ["✅", "❌", "⏳", "⚠️", "⚠"];
        let mut owner: HashMap<String, String> = HashMap::new();

        for cmd in a.registry.list() {
            let name = format!(".{}", cmd.name);
            let desc_len = cmd.description.chars().count();
            assert!(
                (4..=16).contains(&desc_len),
                "{name} description {:?} has {desc_len} chars (expected 4..=16)",
                cmd.description
            );
            assert!(
                !cmd.description.contains(['，', ',', '。']) && !cmd.description.ends_with('.'),
                "{name} description {:?} contains punctuation",
                cmd.description
            );
            assert!(
                !cmd.usage.contains(" | ") && !cmd.usage.contains("..."),
                "{name} usage {:?} violates format",
                cmd.usage
            );
            let help_str = match &cmd.help_fn {
                Some(f) => f("."),
                None if !cmd.help.is_empty() => cmd.help.clone(),
                None => panic!("{name} has no help"),
            };
            let emoji = leading_emoji(&help_str);
            assert!(
                !emoji.is_empty() && help_str[emoji.len()..].starts_with(" <b>"),
                "{name} help does not start with 'emoji <b>...</b>': {:?}",
                help_str.lines().next()
            );
            assert!(
                !status_emojis.contains(&emoji),
                "{name} uses status emoji {emoji}"
            );
            if let Some(prev) = owner.get(emoji) {
                assert_eq!(prev, &help_str, "{name} duplicates title emoji {emoji}");
            } else {
                owner.insert(emoji.to_owned(), help_str.clone());
            }
            for text in [&help_str, &cmd.description] {
                assert!(!text.contains("..."), "{name} contains ... instead of …");
                assert!(
                    !han_ascii.is_match(text),
                    "{name} contains ASCII :/(: right after Han char"
                );
                assert!(
                    !emoji_in_bold.is_match(text),
                    "{name} places emoji inside <b>"
                );
                assert!(
                    !text.contains("</code> - "),
                    "{name} contains '</code> - '"
                );
            }
        }
    }

    #[test]
    fn test_readme_matches_registry() {
        let dir = TempDirGuard::new("readme");
        let a = test_app(dir.0.clone(), false);
        let root = repo_root();
        let cmd_re = Regex::new(r"`\.([a-z0-9_]+)").expect("cmd_re");
        let langs = [
            ("README.md", "## Commands", "partly"),
            ("README.ja.md", "## コマンド", "一部"),
        ];

        for (file, heading, partly) in langs {
            let raw = fs::read_to_string(root.join(file)).expect("read README");
            let mut in_section = false;
            let mut group_idx: isize = -1;
            let mut in_table = false;
            let mut seen: HashMap<String, usize> = HashMap::new();

            for line in raw.lines() {
                if line.starts_with("## ") {
                    in_section = line == heading;
                    continue;
                }
                if !in_section {
                    continue;
                }
                if line.starts_with("**") && line.ends_with("**") {
                    group_idx += 1;
                    in_table = false;
                    continue;
                }
                if !line.starts_with('|') {
                    continue;
                }
                if !in_table {
                    in_table = true;
                    continue;
                }
                let cells: Vec<&str> = line.trim_matches('|').split(" | ").collect();
                if cells[0].starts_with("---") {
                    continue;
                }
                assert!(
                    group_idx >= 0 && (group_idx as usize) < GROUPS.len(),
                    "{file}: group index out of bounds: {line}"
                );
                let mut names = Vec::new();
                for caps in cmd_re.captures_iter(cells[0]) {
                    names.push(caps[1].to_owned());
                }
                let lend = match cells.last().map(|s| s.trim()) {
                    Some("✓") => "all",
                    Some(x) if x == partly => "partly",
                    _ => "none",
                };
                for name in &names {
                    let cmd = a
                        .registry
                        .command(name)
                        .unwrap_or_else(|| panic!("{file}: missing command .{name} in {line}"));
                    *seen.entry(name.clone()).or_insert(0) += 1;
                    assert_eq!(
                        cmd.group,
                        GROUPS[group_idx as usize],
                        "{file}: .{name} group mismatch"
                    );
                    if cmd.name == *name {
                        assert_eq!(
                            sudo::lending(name),
                            lend,
                            "{file}: .{name} lending column mismatch"
                        );
                        for alias in &cmd.aliases {
                            assert!(
                                names.contains(alias),
                                "{file}: .{name} row missing alias .{alias}"
                            );
                        }
                    }
                }
            }
            for cmd in a.registry.list() {
                assert_eq!(
                    seen.get(&cmd.name).copied().unwrap_or(0),
                    1,
                    "{file}: .{} seen count mismatch",
                    cmd.name
                );
            }
        }
    }

    #[test]
    fn test_delegable_names_exist() {
        let dir = TempDirGuard::new("delegable");
        let a = test_app(dir.0.clone(), false);
        let delegable = sudo::delegable();
        for name in &delegable {
            assert!(
                a.registry.command(name).is_some(),
                "delegable command {name:?} is not registered"
            );
        }
        for owner_only in [
            "sudo",
            "sure",
            "dme",
            "da",
            "acn",
            "autochangename",
            "prefix",
            "alias",
            "bf",
            "log",
            "save",
            "restart",
            "update",
            "sb",
            "unsb",
            "sysinfo",
            "refresh",
            "aban",
        ] {
            assert!(
                !delegable.iter().any(|n| n == owner_only),
                "{owner_only:?} must not be delegable"
            );
        }
    }

    #[test]
    fn test_commands_have_group_and_description() {
        let dir = TempDirGuard::new("groups");
        let a = test_app(dir.0.clone(), false);
        for cmd in a.registry.list() {
            assert!(
                GROUPS.contains(&cmd.group.as_str()),
                ".{} has invalid group {:?}",
                cmd.name,
                cmd.group
            );
        }
    }

    #[tokio::test]
    async fn test_text_commands_only_show_help_for_standalone_help() {
        struct EditCapture {
            edited: Mutex<String>,
        }
        impl RpcTransport for EditCapture {
            fn invoke_raw<'a>(
                &'a self,
                _dc: Option<i32>,
                constructor_id: u32,
                body: Vec<u8>,
            ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
                Box::pin(async move {
                    if constructor_id == tl::functions::messages::EditMessage::CONSTRUCTOR_ID {
                        let req =
                            tl::functions::messages::EditMessage::from_bytes(&body[4..]).unwrap();
                        let plain = parse_html(req.message.as_deref().unwrap_or(""), None)
                            .map(|(t, _)| t)
                            .unwrap_or_default();
                        *self.edited.lock().unwrap() = plain;
                    }
                    Ok(tl::enums::Updates::Updates(tl::types::Updates {
                        updates: vec![],
                        users: vec![],
                        chats: vec![],
                        date: 0,
                        seq: 0,
                    })
                    .to_bytes())
                })
            }
        }

        let dir = TempDirGuard::new("help-standalone");
        let a = test_app(dir.0.clone(), false);
        let capture = Arc::new(EditCapture {
            edited: Mutex::new(String::new()),
        });
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let client = Arc::new(Client::from_transport(
            Arc::clone(&capture) as Arc<dyn RpcTransport>,
            peers,
            Arc::new(SentRecorder::new(Arc::new(|_, _| {}))),
        ));

        let long_tr = format!("h {}", "字".repeat(5000));
        let cases = [
            ("ai", "AI 助手", "help me", "AI 思考中", "选择模型"),
            ("gt", "AI 翻译", "help me", "正在翻译", "选择模型"),
            ("tr", "翻译", long_tr.as_str(), "", "文本过长"),
            ("t", "文字转语音", "help me", "还没设 API Key", ""),
        ];

        for (command, help_text, args_text, status, err_substr) in cases {
            let run_once = |args: &str| {
                let a = Arc::clone(&a);
                let client = Arc::clone(&client);
                let capture = Arc::clone(&capture);
                let input = format!(".{command} {args}");
                async move {
                    *capture.edited.lock().unwrap() = String::new();
                    let route = a.registry.parse(&input).expect("parse route");
                    let cmd = a.registry.command(&route.command).expect("lookup cmd");
                    let mut raw =
                        kit::blank_tl_message(7, tl::types::PeerUser { user_id: 1 }.into());
                    raw.message = input.clone();
                    raw.out = true;
                    let msg = mibot_bot::Message {
                        id: 7,
                        peer: tl::types::PeerUser { user_id: 1 }.into(),
                        chat_id: "1".into(),
                        chat_type: mibot_bot::ChatType::Private,
                        text: input,
                        out: true,
                        edited: false,
                        forward: false,
                        post: false,
                        saved: true,
                        sender: tl::types::PeerUser { user_id: 1 }.into(),
                        reply_to_id: 0,
                        reply_to_peer: None,
                        topic_id: 0,
                        quote_text: String::new(),
                        quote_entities: Vec::new(),
                        raw,
                    };
                    let inv = Invocation {
                        client,
                        registry: Arc::clone(&a.registry),
                        message: msg,
                        prefix: route.prefix,
                        command: route.command,
                        args: route.args,
                        raw: route.raw,
                        cancel: CancellationToken::new(),
                        foreign: false,
                    };
                    (cmd.handle)(inv).await
                }
            };

            let help_res = run_once("help").await;
            let edited_help = capture.edited.lock().unwrap().clone();
            assert!(
                help_res.is_ok() && edited_help.contains(help_text),
                "{command} help={edited_help:?} err={help_res:?}"
            );

            let res = run_once(args_text).await;
            let edited = capture.edited.lock().unwrap().clone();
            let err_str = res.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
            assert!(
                edited.contains(status)
                    && (err_substr.is_empty() == err_str.is_empty())
                    && err_str.contains(err_substr),
                "{command} status={edited:?} err={err_str:?}"
            );
        }
    }

    #[test]
    fn test_import_mibox_and_refuses_locked() {
        let dir = TempDirGuard::new("import");
        let mibox = dir.0.join("mibox");
        let root = dir.0.join("dest");
        fs::create_dir_all(&mibox).expect("mkdir mibox");
        fs::create_dir_all(&root).expect("mkdir root");

        let write = |p: PathBuf, data: &[u8]| {
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("mkdir parent");
            }
            fs::write(p, data).expect("write");
        };

        let alias_db = fs::read(
            repo_root().join("rust/crates/mibox/testdata/alias.db"),
        )
        .expect("read alias.db");
        write(mibox.join("assets/alias/alias.db"), &alias_db);
        write(mibox.join("assets/dme/config.json"), br#"{"x":1}"#);
        write(mibox.join(".env"), b"TB_PREFIX=\"!  \xEF\xBC\x81\"\n");
        write(root.join(".env"), "# 已有的\nOTHER=1\n".as_bytes());
        write(
            mibox.join("assets/speedtest/v2-config.json"),
            br#"{"default_server_id": 222}"#,
        );
        write(
            mibox.join("assets/speedtest/speedtest.json"),
            br#"{"default_server_id": 111}"#,
        );

        let mut log = Vec::new();
        import_mibox(&mibox, &root.join("data"), &mut log).expect("import_mibox");
        let alias_raw = fs::read(root.join("data/alias.json")).expect("read alias.json");
        let parsed: serde_json::Value = serde_json::from_slice(&alias_raw).expect("parse alias.json");
        assert_eq!(parsed["aliases"]["测速"], "speedtest 12345");
        assert_eq!(parsed["aliases"].as_object().map(|o| o.len()), Some(403));

        assert_eq!(
            fs::read(root.join("data/dme.json")).expect("read dme.json"),
            br#"{"x":1}"#
        );
        let speed = fs::read_to_string(root.join("data/speedtest.json")).expect("read speedtest");
        assert!(speed.contains("222"));

        let env = fs::read_to_string(root.join(".env")).expect("read .env");
        assert!(env.contains("MIBOT_PREFIX=! ！"));
        assert!(env.contains("OTHER=1"));

        let mut log2 = Vec::new();
        import_mibox(&mibox, &root.join("data"), &mut log2).expect("second import");
        let log2_str = String::from_utf8_lossy(&log2);
        assert!(log2_str.contains("0 file(s) imported"));
        assert!(log2_str.contains("MIBOT_PREFIX already set"));

        // Locked deployment refusal
        let locked_root = dir.0.join("locked");
        fs::create_dir_all(&locked_root).expect("mkdir locked");
        let _lock = lock_root(&locked_root).expect("acquire lock");
        let mut sink = Vec::new();
        let err = import_mibox(&mibox, &locked_root.join("data"), &mut sink).unwrap_err();
        assert!(matches!(err, AppError::AlreadyRunning));
        assert!(!locked_root.join("data").exists());
    }

    #[test]
    fn test_read_only_registration_writes_nothing() {
        fn snapshot(root: &Path) -> BTreeMap<PathBuf, (std::time::SystemTime, Vec<u8>)> {
            let mut out = BTreeMap::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                for entry in fs::read_dir(&dir).expect("read_dir") {
                    let entry = entry.expect("entry");
                    let path = entry.path();
                    let meta = entry.metadata().expect("meta");
                    if meta.is_dir() {
                        stack.push(path);
                    } else {
                        out.insert(
                            path.clone(),
                            (meta.modified().expect("mtime"), fs::read(&path).expect("read")),
                        );
                    }
                }
            }
            out
        }

        let dir = TempDirGuard::new("readonly");
        fs::create_dir_all(dir.0.join("data")).expect("mkdir data");
        fs::write(
            dir.0.join("data/da.json"),
            r#"{"tasks":[{"chatId":"-1001","isRunning":true,"errors":[]}],"imported":true}"#,
        )
        .expect("write da.json");
        fs::write(dir.0.join("data/acn.json"), r#"{"users":{}}"#).expect("write acn.json");

        let before = snapshot(&dir.0);
        let _a = test_app(dir.0.clone(), true);
        let after = snapshot(&dir.0);
        assert_eq!(before, after, "read-only registration modified files");
    }
}
