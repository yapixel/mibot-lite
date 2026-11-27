use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tar::{Archive, Builder, EntryType, Header};
use thiserror::Error;

pub const MANIFEST: &str = "mibot-lite-backup.json";
const FORMAT: u32 = 1;
const MAX_TOTAL: u64 = 32 << 20;
const MAX_ENTRIES: usize = 512;

pub const ROOT_FILES: &[&str] = &[
    mibot_config::CONFIG_FILE,
    mibot_config::SESSION_FILE,
    mibot_config::ENV_FILE,
];

#[derive(Debug, Error)]
pub enum BackupError {
    #[error("this directory already has an account; restoring would replace it")]
    Exists,
    #[error("{0}")]
    Message(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config error: {0}")]
    Config(#[from] mibot_config::ConfigError),
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    #[serde(default)]
    version: String,
    #[serde(default = "Utc::now")]
    created: DateTime<Utc>,
    files: Vec<String>,
}

struct LimitedWriter<W: Write> {
    inner: W,
    remaining: u64,
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() as u64 > self.remaining {
            return Err(io::Error::other("backup exceeds the restore size limit"));
        }
        let n = self.inner.write(buf)?;
        self.remaining -= n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn allowed(name: &str) -> bool {
    if name.is_empty()
        || name.starts_with('/')
        || name.contains("..")
        || name.contains('\\')
        || name.contains("//")
        || name.ends_with('/')
        || name.starts_with("./")
    {
        return false;
    }
    if ROOT_FILES.contains(&name) {
        return true;
    }
    if let Some(rest) = name.strip_prefix("data/") {
        return !rest.is_empty()
            && !rest.contains('/')
            && rest.ends_with(".json")
            && !rest.starts_with('.');
    }
    false
}

fn put_entry<W: Write>(
    tar: &mut Builder<W>,
    name: &str,
    body: &[u8],
    mtime_secs: u64,
) -> Result<(), BackupError> {
    let mut header = Header::new_ustar();
    header.set_mode(0o600);
    header.set_size(body.len() as u64);
    header.set_mtime(mtime_secs);
    header.set_entry_type(EntryType::Regular);
    tar.append_data(&mut header, name, body)?;
    Ok(())
}

pub fn create(
    root: &Path,
    version: &str,
    now: DateTime<Utc>,
) -> Result<(Vec<u8>, Vec<String>), BackupError> {
    let mut names = Vec::new();
    for &name in ROOT_FILES {
        match fs::metadata(root.join(name)) {
            Ok(meta) if meta.is_file() => names.push(name.to_owned()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(BackupError::Io(e)),
        }
    }

    match fs::read_dir(root.join("data")) {
        Ok(entries) => {
            for entry_res in entries {
                let entry = entry_res?;
                let file_name = entry.file_name();
                let Some(s) = file_name.to_str() else {
                    continue;
                };
                let rel = format!("data/{s}");
                if entry.file_type()?.is_file() && s.ends_with(".json") && allowed(&rel) {
                    names.push(rel);
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(BackupError::Io(e)),
    }

    if names.is_empty() {
        return Err(BackupError::Message(
            "nothing to back up: no config.json, .env or data/*.json here".into(),
        ));
    }
    if names.len() + 1 > MAX_ENTRIES {
        return Err(BackupError::Message(
            "too many files to restore this backup".into(),
        ));
    }
    if names.iter().any(|n| n == mibot_config::SESSION_FILE)
        && !names.iter().any(|n| n == mibot_config::CONFIG_FILE)
    {
        return Err(BackupError::Message(
            "a session backup must include config.json".into(),
        ));
    }
    names.sort();

    let manifest_bytes = serde_json::to_vec_pretty(&Manifest {
        format: FORMAT,
        version: version.to_owned(),
        created: now,
        files: names.clone(),
    })?;

    let gz = GzEncoder::new(Vec::new(), Compression::default());
    let limited = LimitedWriter {
        inner: gz,
        remaining: MAX_TOTAL,
    };
    let mut builder = Builder::new(limited);
    let now_secs = now.timestamp().max(0) as u64;
    put_entry(&mut builder, MANIFEST, &manifest_bytes, now_secs)?;

    for name in &names {
        let full = root.join(name);
        let meta = fs::metadata(&full)?;
        if meta.len() > builder.get_ref().remaining {
            return Err(BackupError::Message(format!(
                "{name} exceeds the remaining backup size limit"
            )));
        }
        let body = fs::read(&full)?;
        if name == mibot_config::CONFIG_FILE
            && let Err(e) = mibot_config::parse(&body)
        {
            return Err(BackupError::Message(format!(
                "cannot back up invalid config.json: {e}"
            )));
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(now_secs);
        put_entry(&mut builder, name, &body, mtime)?;
    }

    let limited = builder.into_inner()?;
    let archive = limited.inner.finish()?;
    Ok((archive, names))
}

static RESTORE_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn restore<R: Read>(
    archive: R,
    root: &Path,
    overwrite: bool,
) -> Result<Vec<String>, BackupError> {
    let gz = GzDecoder::new(archive);
    let limited = gz.take(MAX_TOTAL + 1);
    let mut tar = Archive::new(limited);

    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut head: Option<Manifest> = None;
    let mut count = 0usize;

    {
        let entries = tar
            .entries()
            .map_err(|e| BackupError::Message(format!("not a mibot-lite backup or damaged: {e}")))?;
        for entry_res in entries {
            let mut entry = entry_res
                .map_err(|e| BackupError::Message(format!("the backup is damaged: {e}")))?;
            count += 1;
            if count > MAX_ENTRIES {
                return Err(BackupError::Message(
                    "the backup has too many entries to be one of ours".into(),
                ));
            }
            let name = entry
                .path()
                .map_err(|e| BackupError::Message(format!("the backup is damaged: {e}")))?
                .to_string_lossy()
                .into_owned();
            if entry.header().entry_type() != EntryType::Regular {
                return Err(BackupError::Message(format!(
                    "the backup contains {name:?}, which is not a plain file"
                )));
            }
            let mut body = Vec::new();
            entry
                .read_to_end(&mut body)
                .map_err(|e| BackupError::Message(format!("the backup is damaged: {e}")))?;
            if name == MANIFEST {
                head = Some(serde_json::from_slice(&body).map_err(|_| {
                    BackupError::Message("the backup's manifest is unreadable".into())
                })?);
                continue;
            }
            if !allowed(&name) {
                return Err(BackupError::Message(format!(
                    "the backup contains {name:?}, which a mibot-lite backup never holds"
                )));
            }
            if files.contains_key(&name) {
                return Err(BackupError::Message(format!(
                    "the backup contains {name:?} twice"
                )));
            }
            files.insert(name, body);
        }
    }

    let mut remaining = tar.into_inner();
    io::copy(&mut remaining, &mut io::sink())
        .map_err(|e| BackupError::Message(format!("the backup is damaged: {e}")))?;
    if remaining.limit() == 0 {
        return Err(BackupError::Message(
            "the backup is larger than any configuration could be".into(),
        ));
    }

    let Some(head) = head else {
        return Err(BackupError::Message(format!(
            "not a mibot-lite backup: it has no {MANIFEST}"
        )));
    };
    if head.format > FORMAT {
        return Err(BackupError::Message(format!(
            "this backup was made by a newer mibot-lite (format {}); upgrade first",
            head.format
        )));
    }
    if files.is_empty() {
        return Err(BackupError::Message("the backup is empty".into()));
    }
    if head.files.len() != files.len() {
        return Err(BackupError::Message(
            "the backup's files do not match its manifest".into(),
        ));
    }
    let mut listed = HashSet::with_capacity(head.files.len());
    for name in &head.files {
        if !files.contains_key(name) || !listed.insert(name.as_str()) {
            return Err(BackupError::Message(
                "the backup's files do not match its manifest".into(),
            ));
        }
    }

    let account_snapshot = files.contains_key(mibot_config::CONFIG_FILE);
    if files.contains_key(mibot_config::SESSION_FILE) && !account_snapshot {
        return Err(BackupError::Message(
            "a session backup must include config.json".into(),
        ));
    }
    if let Some(cfg_bytes) = files.get(mibot_config::CONFIG_FILE)
        && let Err(e) = mibot_config::parse(cfg_bytes)
    {
        return Err(BackupError::Message(format!(
            "the backup's config.json is not usable: {e}"
        )));
    }
    if !overwrite {
        match fs::symlink_metadata(root.join(mibot_config::CONFIG_FILE)) {
            Ok(_) => return Err(BackupError::Exists),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(BackupError::Io(e)),
        }
    }

    mibot_fsutil::ensure_dir(root)?;
    let staging = create_staging_dir(root)?;
    let _cleanup = StagingGuard(&staging);

    let names: Vec<String> = files.keys().cloned().collect();
    for (name, body) in &files {
        mibot_fsutil::write_file_atomic(&staging.join(name), body, 0o600)?;
    }
    mibot_fsutil::ensure_dir(&root.join("data"))?;

    if account_snapshot {
        remove_if_exists(&root.join(mibot_config::CONFIG_FILE))?;
    }

    let apply = || -> Result<(), BackupError> {
        if account_snapshot {
            let mut stale: Vec<String> = vec![
                mibot_config::ENV_FILE.into(),
                mibot_config::SESSION_FILE.into(),
                mibot_config::STATE_FILE.into(),
                format!("{}.cursor", mibot_config::STATE_FILE),
                mibot_config::RESTART_RECEIPT_FILE.into(),
            ];
            for entry_res in fs::read_dir(root.join("data"))? {
                let entry = entry_res?;
                if let Some(s) = entry.file_name().to_str() {
                    let rel = format!("data/{s}");
                    if !entry.file_type()?.is_dir() && allowed(&rel) {
                        stale.push(rel);
                    }
                }
            }
            for rel in stale {
                if !files.contains_key(&rel) {
                    remove_if_exists(&root.join(&rel))?;
                }
            }
        }
        for name in &names {
            if name == mibot_config::CONFIG_FILE {
                continue;
            }
            fs::rename(staging.join(name), root.join(name))?;
        }
        if account_snapshot {
            fs::rename(
                staging.join(mibot_config::CONFIG_FILE),
                root.join(mibot_config::CONFIG_FILE),
            )?;
        }
        Ok(())
    };

    if let Err(err) = apply() {
        if account_snapshot {
            return Err(BackupError::Message(format!(
                "restore incomplete; config.json is withheld so the service cannot start with mixed account settings; run restore again: {err}"
            )));
        }
        return Err(err);
    }

    Ok(names)
}

fn create_staging_dir(root: &Path) -> io::Result<PathBuf> {
    let pid = std::process::id();
    loop {
        let seq = RESTORE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!(".restore-{pid}-{seq}"));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

struct StagingGuard<'a>(&'a Path);

impl Drop for StagingGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0);
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const ACCOUNT: &str = r#"{"api_id": 1234567, "api_hash": "abc", "session": "1xyz"}"#;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mibot-backup-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn deployment() -> PathBuf {
        let root = temp_dir();
        let files = [
            ("config.json", ACCOUNT),
            ("gotd-session.json", r#"{"dc":2}"#),
            (".env", "MIBOT_PREFIX=.\n"),
            ("updates.json", r#"{"pts":1}"#),
            ("updates.json.cursor", r#"{"pts":2}"#),
            ("restart-receipt.json", r#"{"pending":{"chatId":"999","messageId":1}}"#),
            ("mibot-lite", "binary"),
            ("mibot-lite.lock", ""),
            ("data/ai.json", r#"{"key":"sk-secret"}"#),
            ("data/alias.json", r#"{"aliases":{"测速":"speedtest"}}"#),
            ("data/eatgif/abc.png", "cache"),
            ("data/eatgif/abc.json", r#"{"cache":true}"#),
            ("data/speedtest/speedtest", "cli"),
            ("data/notes.txt", "not json"),
        ];
        for (name, body) in files {
            let full = root.join(name);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, body).unwrap();
        }
        root
    }

    fn forge(entries: &[(&str, &str)], with_manifest: bool, kind: EntryType) -> Vec<u8> {
        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = Builder::new(gz);
        if with_manifest {
            let names: Vec<&str> = entries.iter().map(|(n, _)| *n).collect();
            let raw = serde_json::to_vec(&Manifest {
                format: FORMAT,
                version: "x".into(),
                created: Utc::now(),
                files: names.into_iter().map(str::to_owned).collect(),
            })
            .unwrap();
            put_entry(&mut tar, MANIFEST, &raw, 0).unwrap();
        }
        for (name, body) in entries {
            let mut h = Header::new_ustar();
            h.set_mode(0o600);
            h.set_entry_type(kind);
            if kind == EntryType::Symlink {
                h.set_size(0);
                let _ = h.set_link_name("/etc/passwd");
                tar.append_data(&mut h, name, io::empty()).unwrap();
            } else {
                h.set_size(body.len() as u64);
                tar.append_data(&mut h, name, body.as_bytes()).unwrap();
            }
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn round_trip_keeps_configuration_and_drops_the_rest() {
        let source = deployment();
        let (archive, names) = create(&source, "0.1.12", Utc::now()).unwrap();
        let want = vec![
            ".env",
            "config.json",
            "data/ai.json",
            "data/alias.json",
            "gotd-session.json",
        ];
        assert_eq!(names, want);

        let target = temp_dir();
        let mut restored = restore(Cursor::new(&archive), &target, false).unwrap();
        restored.sort();
        assert_eq!(restored, want);

        for name in &want {
            assert_eq!(fs::read(target.join(name)).unwrap(), fs::read(source.join(name)).unwrap());
            let mode = fs::metadata(target.join(name)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        for left in ["updates.json", "mibot-lite", "data/eatgif", "data/speedtest", "data/notes.txt"] {
            assert!(!target.join(left).exists(), "{left} carried over");
        }
        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&target);
    }

    #[test]
    fn refuses_to_replace_account_unless_overwrite() {
        let source = deployment();
        let (archive, _) = create(&source, "0.1.12", Utc::now()).unwrap();
        let target = temp_dir();
        let existing = r#"{"api_id": 7, "api_hash": "other", "session": "1other"}"#;
        fs::write(target.join("config.json"), existing).unwrap();
        assert!(matches!(
            restore(Cursor::new(&archive), &target, false),
            Err(BackupError::Exists)
        ));
        assert_eq!(fs::read_to_string(target.join("config.json")).unwrap(), existing);
        restore(Cursor::new(&archive), &target, true).unwrap();
        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&target);
    }

    #[test]
    fn overwrite_removes_stale_session_and_target_only_settings() {
        let source = temp_dir();
        fs::write(source.join("config.json"), ACCOUNT).unwrap();
        let (archive, _) = create(&source, "v1", Utc::now()).unwrap();

        let target = deployment();
        for name in ["sudo.json", "sure.json", "acron.json", "keyword.json"] {
            fs::write(target.join("data").join(name), r#"{"users":[{"id":"999"}]}"#).unwrap();
        }
        restore(Cursor::new(&archive), &target, true).unwrap();

        for gone in [
            ".env",
            "gotd-session.json",
            "updates.json",
            "updates.json.cursor",
            "restart-receipt.json",
            "data/ai.json",
            "data/alias.json",
            "data/sudo.json",
            "data/sure.json",
            "data/acron.json",
            "data/keyword.json",
        ] {
            assert!(!target.join(gone).exists(), "{gone} survived");
        }
        for kept in ["config.json", "mibot-lite", "data/eatgif/abc.png", "data/notes.txt"] {
            assert!(target.join(kept).exists(), "{kept} lost");
        }
        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&target);
    }

    #[test]
    fn rejects_disallowed_and_truncated_or_damaged_archives() {
        let forged_cases = [
            forge(&[("mibot-lite", "evil")], true, EntryType::Regular),
            forge(&[("data/eatgif/x.json", "{}")], true, EntryType::Regular),
            forge(&[("data/.x.json", "{}")], true, EntryType::Regular),
            forge(&[("config.json", "")], true, EntryType::Symlink),
            forge(&[("data/ai.json", "{}")], false, EntryType::Regular),
            forge(&[("config.json", r#"{"api_id":0}"#)], true, EntryType::Regular),
            b"PK\x03\x04 not gzip".to_vec(),
        ];
        for bytes in forged_cases {
            let target = temp_dir();
            assert!(restore(Cursor::new(&bytes), &target, true).is_err());
            assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
            let _ = fs::remove_dir_all(&target);
        }

        let source = deployment();
        let (archive, _) = create(&source, "v1", Utc::now()).unwrap();
        let t1 = temp_dir();
        assert!(restore(Cursor::new(&archive[..archive.len() / 2]), &t1, true).is_err());
        assert_eq!(fs::read_dir(&t1).unwrap().count(), 0);

        let mut bad_crc = archive.clone();
        let len = bad_crc.len();
        bad_crc[len - 8] ^= 1;
        for damaged in [&archive[..len - 1], bad_crc.as_slice()] {
            let t2 = deployment();
            assert!(restore(Cursor::new(damaged), &t2, true).is_err());
            assert!(t2.join("updates.json").exists());
            let _ = fs::remove_dir_all(&t2);
        }
        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&t1);
    }

    #[test]
    fn symlink_config_and_settings_only_and_incomplete_and_withheld() {
        let src_dep = deployment();
        let root = temp_dir();
        let link_dst = root.join("config.json");
        symlink(src_dep.join("config.json"), &link_dst).unwrap();
        let (archive, names) = create(&root, "v1", Utc::now()).unwrap();
        assert_eq!(names, vec!["config.json"]);
        assert!(matches!(
            restore(Cursor::new(&archive), &root, false),
            Err(BackupError::Exists)
        ));
        assert!(fs::read_link(&link_dst).is_ok());

        let settings_src = temp_dir();
        fs::create_dir_all(settings_src.join("data")).unwrap();
        fs::write(settings_src.join("data/ai.json"), r#"{"key":"new"}"#).unwrap();
        let (settings_arc, _) = create(&settings_src, "v1", Utc::now()).unwrap();
        let target = deployment();
        restore(Cursor::new(&settings_arc), &target, true).unwrap();
        assert!(target.join("gotd-session.json").exists());
        assert_eq!(fs::read_to_string(target.join("data/ai.json")).unwrap(), r#"{"key":"new"}"#);

        // Withheld config.json when data/ai.json cannot be replaced (directory conflict)
        let src2 = deployment();
        let (arc2, _) = create(&src2, "v1", Utc::now()).unwrap();
        let tgt2 = deployment();
        fs::remove_file(tgt2.join("data/ai.json")).unwrap();
        fs::create_dir_all(tgt2.join("data/ai.json")).unwrap();
        let err = restore(Cursor::new(&arc2), &tgt2, true).unwrap_err();
        assert!(err.to_string().contains("config.json is withheld"));
        assert!(!tgt2.join("config.json").exists());

        for d in [src_dep, root, settings_src, target, src2, tgt2] {
            let _ = fs::remove_dir_all(d);
        }
    }
}
