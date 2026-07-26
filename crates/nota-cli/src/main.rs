use std::fs::create_dir_all;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_appender::{
    non_blocking,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_log::LogTracer;
use tracing_subscriber::{
    filter::LevelFilter,
    fmt::{self, format::Writer, time::FormatTime},
    prelude::*,
};

use nota_core::bus::EventBus;
use nota_core::permissions::PermissionRegistry;
use nota_core::persona::{Persona, PersonaRuntime, PersonaStore};
use nota_infra::{
    ApiState, AppContext, ConfigStore, FilePersonaStore, OpenAiLlm, ToolRegistryImpl,
    http_serve, register_builtin_tools,
};

mod config_wizard;

#[derive(Parser)]
#[command(name = "nota", about = "AI agent persona framework")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Onboard,
    Webui,
}

#[derive(Clone)]
struct ChronoLocalTimer;

impl FormatTime for ChronoLocalTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

fn ensure_dir(base: &Path) -> Result<()> {
    create_dir_all(base)?;
    create_dir_all(base.join(".logs"))?;
    create_dir_all(base.join("personas"))?;
    Ok(())
}

fn init_tracing(base: &Path) -> Result<non_blocking::WorkerGuard> {
    LogTracer::init().ok();

    let timer = ChronoLocalTimer;

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_suffix("log")
        .max_log_files(30)
        .build(base.join(".logs"))?;

    let (non_blocking_writer, guard) = non_blocking(file_appender);

    let console_layer = fmt::layer()
        .with_timer(timer.clone())
        .with_target(false)
        .with_file(false)
        .with_line_number(false)
        .with_filter(LevelFilter::INFO);

    let file_layer = fmt::layer()
        .with_writer(non_blocking_writer)
        .with_timer(timer)
        .with_target(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(false)
        .with_filter(LevelFilter::DEBUG);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init()
        .ok();

    Ok(guard)
}

fn load_or_create_config(store: &ConfigStore) -> Result<nota_infra::Config> {
    match store.load() {
        Ok(()) => Ok(store.get().unwrap()),
        Err(e) => {
            tracing::warn!("The config.toml doesn't exist or failed to load: {e}");
            let cfg = config_wizard::run_wizard(None)?;
            store.set(cfg.clone());
            store.save(&cfg)?;
            info!("Config saved");
            Ok(cfg)
        }
    }
}

async fn run_server(
    base: &Path,
    config: nota_infra::Config,
    cancel_token: CancellationToken,
) -> Result<()> {
    let bus = Arc::new(EventBus::new());
    let permissions = Arc::new(PermissionRegistry::new());

    let persona_store: Arc<dyn PersonaStore> = Arc::new(FilePersonaStore::new(base));
    let llm: Arc<dyn nota_core::llm::LlmClient> = Arc::new(OpenAiLlm::new(
        &config.api_url,
        &config.api_key,
        &config.model,
    ));

    let tool_registry: Arc<ToolRegistryImpl> = Arc::new(ToolRegistryImpl::new());
    register_builtin_tools(&tool_registry, base.join("personas"));

    let persona_names = persona_store.list_personas().await?;
    if persona_names.is_empty() {
        tracing::warn!("No personas found in ~/.nota/personas/. Create one via the onboard wizard.");
    }

    for name in &persona_names {
        let persona = Persona { name: name.clone() };
        let runtime = Arc::new(PersonaRuntime::new(
            persona,
            persona_store.clone(),
            llm.clone(),
            tool_registry.clone(),
            permissions.clone(),
        ));

        let persona_loop_bus = bus.clone();
        let persona_loop_runtime = runtime.clone();
        tokio::spawn(async move {
            persona_loop_runtime.run(persona_loop_bus).await;
        });

        info!("Persona '{}' started", name);
    }

    let config_path = base.join("config.toml");
    let config_arc = Arc::new(tokio::sync::RwLock::new(config));
    let api_state = Arc::new(ApiState {
        persona_store,
        config: config_arc,
        config_path,
    });

    let ctx = Arc::new(AppContext {
        bus: bus.clone(),
        permissions: permissions.clone(),
        api_state,
    });

    let addr: SocketAddr = "127.0.0.1:2349".parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("nota server listening on http://{}", addr);
    tokio::spawn(http_serve(listener, ctx, cancel_token.clone()));

    cancel_token.cancelled().await;
    info!("Nota is shutting down");
    Ok(())
}

async fn run_webui(cancel_token: CancellationToken) -> Result<()> {
    let dir = locate_webui_dist()?;
    info!("Serving web UI from {}", dir.display());

    let addr: SocketAddr = "127.0.0.1:5173".parse()?;
    let listener = TcpListener::bind(addr).await?;

    let serve_dir = tower_http::services::ServeDir::new(&dir)
        .precompressed_gzip()
        .fallback(tower_http::services::ServeFile::new(dir.join("index.html")));

    let app = axum::Router::new().fallback_service(serve_dir);
    info!("Web UI available at http://{}", addr);

    let shutdown = async move {
        cancel_token.cancelled().await;
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .ok();

    Ok(())
}

fn locate_webui_dist() -> Result<PathBuf> {
    if let Ok(env) = std::env::var("NOTA_WEBUI_DIR") {
        let p = PathBuf::from(env);
        if p.join("index.html").exists() {
            return Ok(p);
        }
    }
    let candidates = [
        PathBuf::from("webui/dist"),
        PathBuf::from("../webui/dist"),
        PathBuf::from("../../webui/dist"),
    ];
    candidates
        .into_iter()
        .find(|p| p.join("index.html").exists())
        .ok_or_else(|| anyhow::anyhow!("could not locate webui/dist. Run `bun run build` in webui/ first."))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = dirs::home_dir().unwrap().join(".nota");
    ensure_dir(&base)?;
    let _guard = init_tracing(&base)?;

    match cli.command {
        Some(Command::Onboard) => {
            let config_store = ConfigStore::new(&base);
            let existing = config_store.load().ok().and_then(|_| config_store.get());
            let cfg = config_wizard::run_wizard(existing.as_ref())?;
            config_store.save(&cfg)?;
            info!("Configuration updated");

            let persona_store = FilePersonaStore::new(&base);
            let persona_name = config_wizard::prompt_create_persona()?;
            persona_store.create_persona(&persona_name).await?;
            info!("Persona '{}' created", persona_name);
        }
        Some(Command::Webui) => {
            let cancel_token = CancellationToken::new();
            run_webui(cancel_token).await?;
        }
        None => {
            info!("Nota started");
            let config_store = ConfigStore::new(&base);
            let cancel_token = CancellationToken::new();
            let config = load_or_create_config(&config_store)?;
            run_server(&base, config, cancel_token).await?;
        }
    }

    Ok(())
}
