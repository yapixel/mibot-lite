use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("JSON serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Thread-safe atomic JSON document store under `data/*.json`.
pub struct Store<T> {
    path: PathBuf,
    defaults: Box<dyn Fn() -> T + Send + Sync>,
    mu: Mutex<()>,
    cached: Mutex<Option<Arc<T>>>,
}

impl<T> Store<T>
where
    T: Clone + serde::Serialize + serde::de::DeserializeOwned,
{
    pub fn new<F>(path: impl Into<PathBuf>, defaults: F) -> Self
    where
        F: Fn() -> T + Send + Sync + 'static,
    {
        Self {
            path: path.into(),
            defaults: Box::new(defaults),
            mu: Mutex::new(()),
            cached: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_locked(&self) -> Result<Arc<T>, StoreError> {
        {
            let guard = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(arc) = guard.as_ref() {
                return Ok(Arc::clone(arc));
            }
        }
        let value = match fs::read(&self.path) {
            Ok(raw) if !raw.is_empty() => {
                serde_json::from_slice(&raw).map_err(|source| StoreError::Json {
                    path: self.path.clone(),
                    source,
                })?
            }
            Ok(_) => (self.defaults)(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (self.defaults)(),
            Err(e) => return Err(StoreError::Io(e)),
        };
        let arc = Arc::new(value);
        let mut guard = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Arc::clone(&arc));
        Ok(arc)
    }

    /// Returns a read-only snapshot of the most recently committed document.
    pub fn snapshot(&self) -> Result<Arc<T>, StoreError> {
        {
            let guard = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(arc) = guard.as_ref() {
                return Ok(Arc::clone(arc));
            }
        }
        let _lock = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        self.load_locked()
    }

    /// Returns an owned copy of the stored document.
    pub fn read(&self) -> Result<T, StoreError> {
        Ok((*self.snapshot()?).clone())
    }

    /// Applies `mutate` to a working copy and atomically writes the result to disk.
    /// If `mutate` returns an error, the on-disk file and cached snapshot remain untouched.
    pub fn update<E, F>(&self, mutate: F) -> Result<(), UpdateError<E>>
    where
        F: FnOnce(&mut T) -> Result<(), E>,
    {
        let _lock = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.load_locked().map_err(UpdateError::Store)?;
        let mut working = (*current).clone();
        mutate(&mut working).map_err(UpdateError::Mutate)?;

        let mut encoded = serde_json::to_vec_pretty(&working)
            .map_err(|e| UpdateError::Store(StoreError::Serialize(e)))?;
        encoded.push(b'\n');
        mibot_fsutil::write_file_atomic(&self.path, &encoded, 0o600)
            .map_err(|e| UpdateError::Store(StoreError::Io(e)))?;

        let mut guard = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Arc::new(working));
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum UpdateError<E> {
    #[error(transparent)]
    Store(StoreError),
    #[error(transparent)]
    Mutate(E),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mibot-store-test-{}-{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    struct Doc {
        count: i64,
        items: BTreeMap<String, i64>,
    }

    #[derive(Debug, Error)]
    #[error("{0}")]
    struct DummyErr(&'static str);

    #[test]
    fn read_missing_file_returns_defaults() {
        let dir = temp_dir();
        let store = Store::new(dir.join("doc.json"), Doc::default);
        let value = store.read().unwrap();
        assert_eq!(value.count, 0);
        assert!(value.items.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_persists_with_0600_mode() {
        let dir = temp_dir();
        let path = dir.join("doc.json");
        let store = Store::new(&path, Doc::default);
        store
            .update::<DummyErr, _>(|v| {
                v.count = 3;
                v.items.insert("a".into(), 1);
                Ok(())
            })
            .unwrap();

        let reopened = Store::new(&path, Doc::default);
        let value = reopened.read().unwrap();
        assert_eq!(value.count, 3);
        assert_eq!(value.items.get("a"), Some(&1));

        let perm = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(perm, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_update_leaves_file_alone() {
        let dir = temp_dir();
        let store = Store::new(dir.join("doc.json"), Doc::default);
        store
            .update::<DummyErr, _>(|v| {
                v.count = 1;
                Ok(())
            })
            .unwrap();

        let res = store.update(|v| {
            v.count = 99;
            Err(DummyErr("no"))
        });
        assert!(matches!(res, Err(UpdateError::Mutate(_))));
        assert_eq!(store.read().unwrap().count, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_returns_copy_and_snapshot_stays_valid_during_update() {
        let dir = temp_dir();
        let store = Store::new(dir.join("doc.json"), Doc::default);
        let prev = store.snapshot().unwrap();
        store
            .update::<DummyErr, _>(|next| {
                next.items.insert("new".into(), 2);
                let committed = store.snapshot().unwrap();
                assert!(Arc::ptr_eq(&committed, &prev));
                assert!(committed.items.is_empty());
                Ok(())
            })
            .unwrap();
        let mut copied = store.read().unwrap();
        copied.items.insert("new".into(), 42);
        assert_eq!(store.read().unwrap().items.get("new"), Some(&2));
        assert!(prev.items.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
