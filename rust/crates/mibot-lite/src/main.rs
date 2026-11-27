use chrono::Utc;
use mibot_app::{AppError, GuardOutcome, Options, lock_root};
use mibot_backup::BackupError;
use mibot_fsutil::write_file_atomic;
use mibot_login::Options as LoginOptions;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

const USAGE_TEXT: &str = r#"Usage: mibot-lite <mode> [--root <dir>]

Modes (pick one):
  --serve                 Connect to Telegram and serve (used by systemd/OpenRC)
  --check                 Check deployment directory readability offline
  --verify                Connect to account and run read-only checks in Saved Messages (stop service first)
  --login [--force]       Sign in and write config.json (--api-id / --api-hash skip first two prompts)
  --backup <file>         Write .bf backup archive to file offline
  --restore <file> [--force]  Restore archive into --root (stop service first; --force overwrites existing account)
  --import-mibox <dir>    Import MiBox command config into --root/data and write TB_PREFIX to .env;
                          can be combined with --serve, --check, --verify
  --version               Show version and exit

Common:
  --root <dir>            Deployment directory (default ".")
  --verbose               Log at debug level including MTProto details

Flags:
  -api-hash string
    	With --login: api_hash from my.telegram.org
  -api-id int
    	With --login: api_id from my.telegram.org
  -backup string
    	Write backup archive offline to this path
  -check
    	Verify deployment directory readability offline without modifying files
  -force
    	With --login or --restore: replace existing account files
  -import-mibox string
    	Import MiBox deployment config into --root/data
  -login
    	Sign in to Telegram and write config.json into --root
  -restore string
    	Extract .bf / --backup archive into --root (stop service first)
  -root string
    	Deployment directory containing config.json (default ".")
  -serve
    	Connect to Telegram and run the bot
  -verbose
    	Use debug log level including protocol details
  -verify
    	Run live read-only command self-check in Saved Messages
  -version
    	Show version and exit
"#;

#[derive(Debug, Clone)]
struct Flags {
    root: PathBuf,
    serve: bool,
    check: bool,
    verify_live: bool,
    sign_in: bool,
    force: bool,
    restore_from: Option<PathBuf>,
    backup_to: Option<PathBuf>,
    api_id: Option<i32>,
    api_hash: Option<String>,
    import_mibox: Option<PathBuf>,
    verbose: bool,
    show_version: bool,
}

impl Default for Flags {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            serve: false,
            check: false,
            verify_live: false,
            sign_in: false,
            force: false,
            restore_from: None,
            backup_to: None,
            api_id: None,
            api_hash: None,
            import_mibox: None,
            verbose: false,
            show_version: false,
        }
    }
}

fn display_version() -> &'static str {
    option_env!("MIBOT_VERSION")
        .filter(|s| !s.is_empty())
        .unwrap_or(concat!("v", env!("CARGO_PKG_VERSION")))
}

fn parse_flags<I, S>(args: I) -> Result<Flags, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut flags = Flags::default();
    let raw: Vec<String> = args.into_iter().map(|s| s.as_ref().to_owned()).collect();
    let mut i = 0;

    while i < raw.len() {
        let arg = &raw[i];
        if !arg.starts_with('-') || arg == "-" {
            return Err(format!("unexpected argument: {arg}"));
        }
        let stripped = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')).unwrap_or(arg);
        let (name, inline_val) = match stripped.split_once('=') {
            Some((k, v)) => (k, Some(v.to_owned())),
            None => (stripped, None),
        };
        let next_val = |idx: &mut usize| -> Result<String, String> {
            if let Some(v) = &inline_val {
                return Ok(v.clone());
            }
            *idx += 1;
            raw.get(*idx)
                .cloned()
                .ok_or_else(|| format!("flag needs an argument: -{name}"))
        };

        match name {
            "h" | "help" => return Err(String::new()),
            "serve" => flags.serve = parse_bool_opt(inline_val.as_deref(), true)?,
            "check" => flags.check = parse_bool_opt(inline_val.as_deref(), true)?,
            "verify" => flags.verify_live = parse_bool_opt(inline_val.as_deref(), true)?,
            "login" => flags.sign_in = parse_bool_opt(inline_val.as_deref(), true)?,
            "force" => flags.force = parse_bool_opt(inline_val.as_deref(), true)?,
            "verbose" => flags.verbose = parse_bool_opt(inline_val.as_deref(), true)?,
            "version" => flags.show_version = parse_bool_opt(inline_val.as_deref(), true)?,
            "root" => flags.root = PathBuf::from(next_val(&mut i)?),
            "restore" => flags.restore_from = Some(PathBuf::from(next_val(&mut i)?)),
            "backup" => flags.backup_to = Some(PathBuf::from(next_val(&mut i)?)),
            "import-mibox" => flags.import_mibox = Some(PathBuf::from(next_val(&mut i)?)),
            "api-hash" => flags.api_hash = Some(next_val(&mut i)?),
            "api-id" => {
                let v = next_val(&mut i)?;
                let parsed = v
                    .parse::<i32>()
                    .map_err(|_| format!("invalid value {v:?} for flag -api-id: parse error"))?;
                flags.api_id = Some(parsed);
            }
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
        i += 1;
    }

    Ok(flags)
}

fn parse_bool_opt(inline: Option<&str>, default: bool) -> Result<bool, String> {
    match inline {
        None => Ok(default),
        Some("true" | "1" | "t" | "T" | "TRUE" | "True") => Ok(true),
        Some("false" | "0" | "f" | "F" | "FALSE" | "False") => Ok(false),
        Some(other) => Err(format!("invalid boolean value {other:?}")),
    }
}

fn validate_modes(f: &Flags) -> bool {
    let service_mode = f.serve || f.check || f.verify_live;
    let modes = [
        f.serve,
        f.check,
        f.verify_live,
        f.sign_in,
        f.restore_from.is_some(),
        f.backup_to.is_some(),
        f.show_version,
    ]
    .into_iter()
    .filter(|&b| b)
    .count();

    let has_import = f.import_mibox.is_some();
    !(modes > 1 || (has_import && modes == 1 && !service_mode) || (modes == 0 && !has_import))
}

fn print_usage() {
    let _ = io::stderr().write_all(USAGE_TEXT.as_bytes());
}

fn run_backup(f: &Flags) -> i32 {
    let Some(target) = &f.backup_to else {
        return 2;
    };
    match mibot_backup::create(&f.root, display_version(), Utc::now()) {
        Ok((archive, names)) => match write_file_atomic(target, &archive, 0o600) {
            Ok(()) => {
                println!(
                    "Backed up {} file(s) to {}. Keep it private — it contains your login session.",
                    names.len(),
                    target.display()
                );
                0
            }
            Err(e) => {
                eprintln!("mibot-lite --backup: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("mibot-lite --backup: {e}");
            1
        }
    }
}

fn restore_archive(file: &Path, root: &Path, overwrite: bool) -> Result<(), String> {
    let archive = File::open(file).map_err(|e| format!("open {}: {e}", file.display()))?;
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
    }
    let _lock = match lock_root(root) {
        Ok(l) => l,
        Err(AppError::AlreadyRunning) => {
            return Err(format!(
                "service is running on {}; stop it first (e.g. systemctl stop mibot-lite or rc-service mibot-lite stop)",
                root.display()
            ));
        }
        Err(e) => return Err(e.to_string()),
    };
    let names = match mibot_backup::restore(archive, root, overwrite) {
        Ok(n) => n,
        Err(BackupError::Exists) => {
            return Err(format!(
                "{} already contains an account; pass --force to replace it",
                root.display()
            ));
        }
        Err(e) => return Err(e.to_string()),
    };
    println!("Restored {} file(s) to {}:", names.len(), root.display());
    for name in names {
        println!("  {name}");
    }
    Ok(())
}

fn run_restore(f: &Flags) -> i32 {
    let Some(file) = &f.restore_from else {
        return 2;
    };
    if let Err(err) = restore_archive(file, &f.root, f.force) {
        eprintln!("mibot-lite --restore: {err}");
        return 1;
    }
    0
}

fn install_signal_cancel(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    if let Some(ref mut s) = sigterm {
                        s.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        cancel.cancel();
    });
}

async fn run_async(f: Flags) -> i32 {
    let cancel = CancellationToken::new();
    install_signal_cancel(cancel.clone());

    let service_mode = f.serve || f.check || f.verify_live;

    if let Some(mibox_dir) = &f.import_mibox {
        let mut out = io::stdout();
        if let Err(e) = mibot_commands::import_mibox(mibox_dir, &f.root.join("data"), &mut out) {
            eprintln!("mibot-lite --import-mibox: {e}");
            return 1;
        }
        if !service_mode {
            return 0;
        }
    }

    if f.backup_to.is_some() {
        return run_backup(&f);
    }
    if f.restore_from.is_some() {
        return run_restore(&f);
    }
    if f.sign_in {
        let mut opts = LoginOptions::new(&f.root);
        opts.api_id = f.api_id;
        opts.api_hash = f.api_hash;
        opts.force = f.force;
        match mibot_login::run(opts).await {
            Ok(()) => return 0,
            Err(e) => {
                eprintln!("mibot-lite --login: {e}");
                return 1;
            }
        }
    }

    run_service(cancel, f).await
}

async fn run_service(cancel: CancellationToken, f: Flags) -> i32 {
    let failures = Arc::new(AtomicUsize::new(0));
    let mut opts = Options::new(&f.root);
    opts.version = display_version().to_owned();
    opts.verbose = f.verbose;
    opts.read_only = f.check;
    opts.register = Some(Box::new(|a| {
        mibot_commands::register_all(a);
    }));

    if f.verify_live {
        let failures_ref = Arc::clone(&failures);
        opts.after_ready = Some(Arc::new(move |app, c| {
            let failures_inner = Arc::clone(&failures_ref);
            Box::pin(async move {
                let prefix = app
                    .registry
                    .prefixes()
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| ".".into());
                let mut out = io::stdout();
                match mibot_verify::run_verification(
                    &c,
                    &app.client,
                    &prefix,
                    &mut out,
                    mibot_verify::CASES,
                    |tok, raw| app.dispatch_message(tok, raw),
                )
                .await
                {
                    Ok(count) => {
                        failures_inner.store(count, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("mibot-lite --verify: {e}");
                        failures_inner.store(1, Ordering::Relaxed);
                    }
                }
                c.cancel();
            })
        }));
    }

    let current = match mibot_app::App::prepare(opts) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("level=ERROR msg=startup.failed error={e:?}");
            return 1;
        }
    };

    if f.check {
        let memory = mibot_sysinfo::read_memory();
        eprintln!(
            "level=INFO msg=check.ok version={} commands={} rss_mb={:.2}",
            display_version(),
            current.registry.list().len(),
            mibot_sysinfo::megabytes(memory.rss)
        );
        return 0;
    }

    match current.run(cancel).await {
        GuardOutcome::Stopped => {}
        GuardOutcome::Unauthorized(msg) => {
            eprintln!("level=ERROR msg=runtime.failed error={msg:?}");
            return 1;
        }
    }

    if failures.load(Ordering::Relaxed) > 0 {
        1
    } else {
        0
    }
}

fn run_cli<I, S>(args: I) -> i32
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("{msg}");
            }
            print_usage();
            return 2;
        }
    };
    if !validate_modes(&flags) {
        print_usage();
        return 2;
    }
    if flags.show_version {
        println!("{}", display_version());
        return 0;
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("runtime init failed: {e}");
            return 1;
        }
    };
    rt.block_on(run_async(flags))
}

fn main() -> ExitCode {
    mibot_httpx::ensure_rustls_provider();
    let code = run_cli(std::env::args().skip(1));
    ExitCode::from(code as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "mibot-cli-test-{label}-{}-{n}",
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

    #[test]
    fn test_flag_validation_and_exit_codes() {
        assert_eq!(run_cli(std::iter::empty::<&str>()), 2);
        assert_eq!(run_cli(["--version"]), 0);
        assert_eq!(run_cli(["--serve", "--check"]), 2);
        assert_eq!(run_cli(["--import-mibox", "/tmp/x", "--backup", "a.tar"]), 2);

        let dir = TempDirGuard::new("missing");
        assert_eq!(
            run_cli(["--check", "--root", &dir.0.join("nope").to_string_lossy()]),
            1
        );
        assert_eq!(
            run_cli(["--backup", &dir.0.join("out.tar.gz").to_string_lossy(), "--root", &dir.0.join("nope").to_string_lossy()]),
            1
        );
        assert_eq!(
            run_cli(["--restore", &dir.0.join("missing.tar.gz").to_string_lossy(), "--root", &dir.0.to_string_lossy()]),
            1
        );
        assert_eq!(
            run_cli(["--import-mibox", &dir.0.join("missing-mibox").to_string_lossy(), "--root", &dir.0.to_string_lossy()]),
            1
        );
    }

    #[test]
    fn test_check_and_backup_restore_roundtrip() {
        let dir = TempDirGuard::new("roundtrip");
        let root = dir.0.join("deploy");
        fs::create_dir_all(root.join("data")).expect("mkdir");
        let sess = mibot_session::StringSession {
            dc_id: 2,
            address: "149.154.167.50".into(),
            port: 443,
            auth_key: [1u8; mibot_session::AUTH_KEY_LENGTH],
        }
        .encode()
        .expect("encode session");
        let cfg = format!(
            r#"{{"api_id":12345,"api_hash":"0123456789abcdef0123456789abcdef","session":"{sess}"}}"#
        );
        fs::write(root.join("config.json"), cfg).expect("write config");
        fs::write(root.join("data/ai.json"), r#"{"configs":{}}"#).expect("write data");

        assert_eq!(run_cli(["--check", "--root", &root.to_string_lossy()]), 0);

        let archive = dir.0.join("backup.tar.gz");
        assert_eq!(
            run_cli([
                "--backup",
                &archive.to_string_lossy(),
                "--root",
                &root.to_string_lossy(),
            ]),
            0
        );
        assert!(archive.exists());

        let restored = dir.0.join("restored");
        assert_eq!(
            run_cli([
                "--restore",
                &archive.to_string_lossy(),
                "--root",
                &restored.to_string_lossy(),
            ]),
            0
        );
        assert!(restored.join("config.json").exists());
        assert!(restored.join("data/ai.json").exists());

        // Second restore without --force refuses
        assert_eq!(
            run_cli([
                "--restore",
                &archive.to_string_lossy(),
                "--root",
                &restored.to_string_lossy(),
            ]),
            1
        );
        // With --force succeeds
        assert_eq!(
            run_cli([
                "--restore",
                &archive.to_string_lossy(),
                "--root",
                &restored.to_string_lossy(),
                "--force",
            ]),
            0
        );
    }
}
