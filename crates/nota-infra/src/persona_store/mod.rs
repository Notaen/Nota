use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::persona::{ChatLogEntry, PersonaStore};
use nota_core::session::{Session, SessionStore};
use tokio::fs;
use tokio::sync::RwLock;

const SOLO_FILENAME: &str = "solo.md";
const MEMORY_FILENAME: &str = "memory.md";
const DEEP_FILENAME: &str = "deep.json";
const SHALLOW_FILENAME: &str = "shallow.json";
/// Legacy single-layer chatlog file; read as a fallback for the deep layer
/// and migrated to `deep.json` on the first write.
const LEGACY_CHATLOG_FILENAME: &str = "chatlog.json";

type FileCache = HashMap<PathBuf, (String, SystemTime)>;

static PERSONA_FILE_CACHE: OnceLock<RwLock<FileCache>> = OnceLock::new();

fn ensure_cache() -> &'static RwLock<FileCache> {
    PERSONA_FILE_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

pub struct FilePersonaStore {
    personas_dir: PathBuf,
}

impl FilePersonaStore {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            personas_dir: base_dir.join("personas"),
        }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        self.personas_dir.join(name)
    }

    fn session_dir(&self, session: &Session) -> PathBuf {
        self.workspace(&session.persona)
            .join("sessions")
            .join(&session.session_id)
    }

    async fn read_cached(&self, path: &Path) -> Result<Option<String>> {
        match fs::metadata(path).await {
            Ok(meta) => {
                let mtime = meta.modified()?;
                {
                    let cache = ensure_cache().read().await;
                    if let Some((content, cached_mtime)) = cache.get(path)
                        && *cached_mtime == mtime
                    {
                        return Ok(Some(content.clone()));
                    }
                }
                let content = fs::read_to_string(path).await?;
                let mut cache = ensure_cache().write().await;
                cache.insert(path.to_path_buf(), (content.clone(), mtime));
                Ok(Some(content))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn invalidate_cache(&self, path: &Path) {
        if let Some(cache) = PERSONA_FILE_CACHE.get() {
            cache.write().await.remove(path);
        }
    }
}

#[async_trait]
impl PersonaStore for FilePersonaStore {
    async fn read_persona_file(&self, name: &str, filename: &str) -> Result<String> {
        let path = self.workspace(name).join(filename);
        match self.read_cached(&path).await? {
            Some(content) => Ok(content),
            None => anyhow::bail!("persona file not found: {}/{}", name, filename),
        }
    }

    async fn write_persona_file(
        &self,
        name: &str,
        filename: &str,
        content: &str,
    ) -> Result<()> {
        let path = self.workspace(name).join(filename);
        fs::write(&path, content).await?;
        self.invalidate_cache(&path).await;
        Ok(())
    }

    async fn create_persona(&self, name: &str) -> Result<()> {
        let workspace = self.workspace(name);
        fs::create_dir_all(&workspace).await?;

        let solo_path = workspace.join(SOLO_FILENAME);
        if !fs::try_exists(&solo_path).await.unwrap_or(false) {
            fs::write(&solo_path, include_str!("../../assets/solo.md")).await?;
        }

        let memory_path = workspace.join(MEMORY_FILENAME);
        if !fs::try_exists(&memory_path).await.unwrap_or(false) {
            fs::write(&memory_path, "").await?;
        }

        Ok(())
    }

    async fn delete_persona(&self, name: &str) -> Result<()> {
        let workspace = self.workspace(name);
        if fs::try_exists(&workspace).await.unwrap_or(false) {
            fs::remove_dir_all(&workspace).await?;
        }
        Ok(())
    }

    async fn list_personas(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut entries = fs::read_dir(&self.personas_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let solo_path = entry.path().join(SOLO_FILENAME);
                if fs::try_exists(&solo_path).await.unwrap_or(false)
                    && let Some(name) = entry.file_name().to_str()
                {
                    names.push(name.to_string());
                }
            }
        }
        Ok(names)
    }
}

#[async_trait]
impl SessionStore for FilePersonaStore {
    async fn append_deep(
        &self,
        session: &Session,
        entries: &[ChatLogEntry],
    ) -> Result<()> {
        let dir = self.session_dir(session);
        let path = dir.join(DEEP_FILENAME);
        let legacy = dir.join(LEGACY_CHATLOG_FILENAME);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        // One-time migration from the legacy single-layer chatlog.
        let mut existing: Vec<ChatLogEntry> = match self.read_cached(&path).await? {
            Some(content) => serde_json::from_str(&content).unwrap_or_default(),
            None => match self.read_cached(&legacy).await? {
                Some(content) => serde_json::from_str(&content).unwrap_or_default(),
                None => Vec::new(),
            },
        };
        existing.extend(entries.iter().cloned());
        let json = serde_json::to_string(&existing)?;
        fs::write(&path, &json).await?;
        self.invalidate_cache(&path).await;
        Ok(())
    }

    async fn read_deep(
        &self,
        session: &Session,
        since: Option<i64>,
    ) -> Result<Vec<ChatLogEntry>> {
        let dir = self.session_dir(session);
        let path = dir.join(DEEP_FILENAME);
        let legacy = dir.join(LEGACY_CHATLOG_FILENAME);
        let content = match self.read_cached(&path).await? {
            Some(c) => c,
            None => match self.read_cached(&legacy).await? {
                Some(c) => c,
                None => return Ok(Vec::new()),
            },
        };
        let entries: Vec<ChatLogEntry> =
            serde_json::from_str(&content).unwrap_or_default();
        if let Some(ts) = since {
            Ok(entries.into_iter().filter(|e| e.timestamp >= ts).collect())
        } else {
            Ok(entries)
        }
    }

    async fn append_shallow(
        &self,
        session: &Session,
        entries: &[ChatLogEntry],
    ) -> Result<()> {
        let path = self.session_dir(session).join(SHALLOW_FILENAME);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut existing: Vec<ChatLogEntry> = match self.read_cached(&path).await? {
            Some(content) => serde_json::from_str(&content).unwrap_or_default(),
            None => Vec::new(),
        };
        existing.extend(entries.iter().cloned());
        let json = serde_json::to_string(&existing)?;
        fs::write(&path, &json).await?;
        self.invalidate_cache(&path).await;
        Ok(())
    }

    async fn read_shallow(
        &self,
        session: &Session,
        since: Option<i64>,
    ) -> Result<Vec<ChatLogEntry>> {
        let path = self.session_dir(session).join(SHALLOW_FILENAME);
        let content = match self.read_cached(&path).await? {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };
        let entries: Vec<ChatLogEntry> =
            serde_json::from_str(&content).unwrap_or_default();
        if let Some(ts) = since {
            Ok(entries.into_iter().filter(|e| e.timestamp >= ts).collect())
        } else {
            Ok(entries)
        }
    }
}
