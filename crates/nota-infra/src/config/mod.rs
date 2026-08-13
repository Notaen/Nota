use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
pub use nota_onebot::OnebotConfig;

// ── 内置 provider 数据（编译时嵌入，仅 wizard 使用） ─────────────────

#[derive(Deserialize)]
struct ProviderDef {
    id: String,
    name: String,
    api_url: String,
    default_model: String,
}

#[derive(Deserialize)]
struct ProvidersFile {
    providers: Vec<ProviderDef>,
}

fn load_providers() -> HashMap<String, ProviderDef> {
    let data: ProvidersFile =
        toml::from_str(include_str!("../../assets/providers.toml"))
            .expect("providers.toml must be valid TOML");
    data.providers.into_iter().map(|p| (p.id.clone(), p)).collect()
}

static PROVIDERS: LazyLock<HashMap<String, ProviderDef>> = LazyLock::new(load_providers);

pub fn provider_ids() -> Vec<&'static str> {
    PROVIDERS.keys().map(|s| s.as_str()).collect()
}

pub fn provider_name(id: &str) -> Option<&'static str> {
    PROVIDERS.get(id).map(|d| d.name.as_str())
}

pub fn provider_url(id: &str) -> Option<&'static str> {
    PROVIDERS.get(id).map(|d| d.api_url.as_str())
}

pub fn provider_default_model(id: &str) -> Option<&'static str> {
    PROVIDERS.get(id).map(|d| d.default_model.as_str())
}

// ── 运行时配置 ─────────────────────────────────────────────────────

/// Application configuration persisted as `config.toml` under the base dir.
#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    /// LLM API format: "responses" (default, OpenAI Responses API) or
    /// "chat" (legacy Chat Completions).
    #[serde(default = "default_api_mode")]
    pub api_mode: String,
    /// Attach the provider's built-in `web_search` tool to Responses API
    /// requests by default (DeepSeek executes it server-side).
    #[serde(default = "default_web_search")]
    pub web_search: bool,
    /// Optional OneBot 11 adapter configuration (`[onebot]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onebot: Option<OnebotConfig>,
}

fn default_api_mode() -> String {
    "responses".to_string()
}

fn default_web_search() -> bool {
    true
}

/// Owns the loaded [`Config`] and the path it is read from / written to.
pub struct ConfigStore {
    config_file: PathBuf,
    inner: RwLock<Option<Config>>,
}

impl ConfigStore {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            config_file: base_dir.join("config.toml"),
            inner: RwLock::new(None),
        }
    }

    pub fn load(&self) -> Result<()> {
        let config: Config = if !std::fs::exists(&self.config_file)? {
            return Err(anyhow!("config.toml not found at {}", self.config_file.display()));
        } else {
            let config_str = std::fs::read_to_string(&self.config_file)?;
            toml::from_str(&config_str)?
        };

        let mut guard = self.inner.write().unwrap();
        *guard = Some(config);
        log::info!("Config loaded");
        Ok(())
    }

    pub fn save(&self, cfg: &Config) -> Result<()> {
        let config_str = toml::to_string_pretty(cfg)?;
        std::fs::write(&self.config_file, config_str)?;
        let mut guard = self.inner.write().unwrap();
        *guard = Some(cfg.clone());
        Ok(())
    }

    /// Take ownership of a config built by an external wizard (e.g. the CLI's
    /// interactive prompt) without first reading it from disk.
    pub fn set(&self, cfg: Config) {
        let mut guard = self.inner.write().unwrap();
        *guard = Some(cfg);
    }

    pub fn get(&self) -> Option<Config> {
        self.inner.read().unwrap().clone()
    }
}
