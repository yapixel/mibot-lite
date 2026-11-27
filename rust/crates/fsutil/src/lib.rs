use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically writes `data` to `path` with file permission `mode`:
/// creates parent directories (`0700` on Unix), writes to a unique sibling temp
/// file, `fsync`s, closes, and renames over `path`.
pub fn write_file_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    create_dir_all_restricted(parent)?;

    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();

    let (mut file, tmp_path) = loop {
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{base}.{pid}.{seq}.tmp"));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(mode);
        match opts.open(&candidate) {
            Ok(f) => break (f, candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };

    let write_res = (|| -> io::Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        Ok(())
    })();

    drop(file);
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

fn create_dir_all_restricted(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir)
}

/// Ensures a directory exists with `0700` permissions when created.
pub fn ensure_dir(dir: &Path) -> io::Result<PathBuf> {
    create_dir_all_restricted(dir)?;
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn write_file_atomic_concurrent() {
        let temp = std::env::temp_dir().join(format!(
            "mibot-fsutil-test-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&temp);
        let path = temp.join("sub").join("asset.png");

        let a = Arc::new(vec![b'a'; 1 << 20]);
        let b = Arc::new(vec![b'b'; 1 << 20]);

        let mut handles = Vec::new();
        for _ in 0..8 {
            for payload in [&a, &b] {
                let p = path.clone();
                let data = Arc::clone(payload);
                handles.push(thread::spawn(move || {
                    write_file_atomic(&p, &data, 0o600).expect("atomic write");
                }));
            }
        }
        for h in handles {
            h.join().expect("thread join");
        }

        let got = fs::read(&path).expect("read back");
        assert!(got == *a || got == *b, "mixed write content: len={}", got.len());

        let meta = fs::metadata(&path).expect("stat");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        let entries: Vec<_> = fs::read_dir(path.parent().unwrap())
            .expect("readdir")
            .collect();
        assert_eq!(entries.len(), 1, "temp files left behind");

        let _ = fs::remove_dir_all(&temp);
    }
}
