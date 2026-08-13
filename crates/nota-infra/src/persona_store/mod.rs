use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::persona::PersonaStore;
use tokio::fs;
use tokio::sync::RwLock;

const SOLO_FILENAME: &str = "solo.md";
const MEMORY_FILENAME: &str = "memory.md";

type FileCache = HashMap<PathBuf, (String, SystemTime)>;

static PERSONA_FILE_CACHE: OnceLock<RwLock<FileCache>> = OnceLock::new();

fn ensure_cache() -> &'static RwLock<FileCache> {
    PERSONA_FILE_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

async fn read_cached(path: &Path) -> Result<Option<String>> {
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

async fn invalidate_cache(path: &Path) {
    if let Some(cache) = PERSONA_FILE_CACHE.get() {
        cache.write().await.remove(path);
    }
}

/// Persona files (`solo.md`, `memory.md`). Conversation history lives in the
/// SQLite `HistoryStore`, not here.
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
}

#[async_trait]
impl PersonaStore for FilePersonaStore {
    async fn read_persona_file(&self, name: &str, filename: &str) -> Result<String> {
        let path = self.workspace(name).join(filename);
        match read_cached(&path).await? {
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
        invalidate_cache(&path).await;
        Ok(())
    }

    async fn create_persona(&self, name: &str) -> Result<()> {
        let workspace = self.workspace(name);
        fs::create_dir_all(&workspace).await?;

        let solo_path = workspace.join(SOLO_FILENAME);
        if !fs::try_exists(&solo_path).await.unwrap_or(false) {
            let solo = include_str!("../../assets/solo.md").replace("{name}", name);
            fs::write(&solo_path, solo).await?;
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
