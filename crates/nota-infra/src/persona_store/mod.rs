use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::Result;
use async_trait::async_trait;
use nota_core::persona::{ChatLogEntry, PersonaStore};
use nota_core::session::Session;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

const SOLO_FILENAME: &str = "solo.md";
const MEMORY_FILENAME: &str = "memory.md";
const HISTORY_FILENAME: &str = "chatlog.jsonl";

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

/// Parse a session history file. New files are JSONL (one entry per line);
/// legacy JSON arrays are still accepted and migrated to JSONL on append.
fn parse_entries(content: &str) -> Vec<ChatLogEntry> {
    if content.trim_start().starts_with('[') {
        serde_json::from_str(content).unwrap_or_default()
    } else {
        content
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    None
                } else {
                    serde_json::from_str(line).ok()
                }
            })
            .collect()
    }
}

/// Serialize entries as JSONL and append them to `path` (creating it if
/// needed). A legacy JSON-array file is rewritten to JSONL first.
async fn append_jsonl(
    path: &Path,
    entries: &[ChatLogEntry],
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    if let Some(content) = read_cached(path).await?
        && content.trim_start().starts_with('[')
    {
        // Migrate the legacy JSON array to JSONL, then append.
        let mut existing = parse_entries(&content);
        existing.extend(entries.iter().cloned());
        let jsonl = existing
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<String>, _>>()?
            .join("\n");
        fs::write(path, format!("{jsonl}\n")).await?;
        invalidate_cache(path).await;
        return Ok(());
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    for entry in entries {
        file.write_all(serde_json::to_string(entry)?.as_bytes())
            .await?;
        file.write_all(b"\n").await?;
    }
    invalidate_cache(path).await;
    Ok(())
}

async fn read_entries(path: &Path) -> Result<Vec<ChatLogEntry>> {
    match read_cached(path).await? {
        Some(content) => Ok(parse_entries(&content)),
        None => Ok(Vec::new()),
    }
}

/// Persona files plus per-session conversation history (fed to the LLM).
/// Sessions live independently under `~/.nota/sessions/<id>/`.
pub struct FilePersonaStore {
    personas_dir: PathBuf,
    sessions_dir: PathBuf,
}

impl FilePersonaStore {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            personas_dir: base_dir.join("personas"),
            sessions_dir: base_dir.join("sessions"),
        }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        self.personas_dir.join(name)
    }

    fn history_path(&self, session: &Session) -> PathBuf {
        self.sessions_dir
            .join(&session.session_id)
            .join(HISTORY_FILENAME)
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

    async fn append_history(
        &self,
        session: &Session,
        entries: &[ChatLogEntry],
    ) -> Result<()> {
        append_jsonl(&self.history_path(session), entries).await
    }

    async fn read_history(
        &self,
        session: &Session,
        since: Option<i64>,
    ) -> Result<Vec<ChatLogEntry>> {
        let entries = read_entries(&self.history_path(session)).await?;
        if let Some(ts) = since {
            Ok(entries.into_iter().filter(|e| e.timestamp >= ts).collect())
        } else {
            Ok(entries)
        }
    }

    async fn clear_history(&self, session: &Session) -> Result<()> {
        let path = self.history_path(session);
        if fs::try_exists(&path).await.unwrap_or(false) {
            fs::remove_file(&path).await?;
        }
        invalidate_cache(&path).await;
        Ok(())
    }
}
