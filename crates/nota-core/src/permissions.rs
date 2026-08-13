use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tokio::sync::{RwLock, oneshot};

pub struct PermissionRegistry {
    pending: RwLock<HashMap<String, oneshot::Sender<bool>>>,
}

impl PermissionRegistry {
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
        }
    }

    pub async fn register(&self) -> (String, oneshot::Receiver<bool>) {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.write().await.insert(id.clone(), tx);
        (id, rx)
    }

    pub async fn resolve(&self, id: &str, approved: bool) -> bool {
        let mut pending = self.pending.write().await;
        if let Some(tx) = pending.remove(id) {
            tx.send(approved).is_ok()
        } else {
            false
        }
    }
}

impl Default for PermissionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// User-guided read allowlist: paths the user explicitly told the persona it
/// may read outside its workspace (`//allow_read <path>`). Reads under these
/// prefixes skip the per-call approval round-trip.
pub struct PathPolicy {
    allowed_read: RwLock<HashSet<PathBuf>>,
}

impl PathPolicy {
    pub fn new() -> Self {
        Self {
            allowed_read: RwLock::new(HashSet::new()),
        }
    }

    /// Record a user-approved path prefix for reading outside the workspace.
    pub async fn allow_read(&self, path: PathBuf) {
        self.allowed_read.write().await.insert(path);
    }

    /// Whether `path` is inside a user-allowed read prefix.
    pub async fn is_read_allowed(&self, path: &Path) -> bool {
        let allowed = self.allowed_read.read().await;
        allowed.iter().any(|prefix| path.starts_with(prefix))
    }
}

impl Default for PathPolicy {
    fn default() -> Self {
        Self::new()
    }
}
