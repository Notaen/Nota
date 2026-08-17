use std::fs::create_dir_all;
use std::net::SocketAddr;
use std::path::Path;
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
    filter::{LevelFilter, Targets},
    fmt::{self, format::Writer, time::FormatTime},
    prelude::*,
};

use nota_core::conversation::ConversationManager;
use nota_core::permissions::{PathPolicy, PermissionRegistry};
use nota_core::persona::{Persona, PersonaStore};
use nota_core::scheduler::Scheduler;
use nota_onebot::{OneBotBridge, OnebotConfig};
use nota_infra::{
    ApiState, AppContext, ConfigStore, FilePersonaStore, LlmClient,
    OpenAiLlm, PersonaRuntime, ToolRegistryImpl, TokioScheduler, http_serve,
    register_builtin_tools, register_chat_tools,
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
    Persona {
        #[command(subcommand)]
        action: PersonaCommand,
    },
}

#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
enum PersonaCommand {
    List,
    Create {
        name: Option<String>,
    },
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

    // 文件层过滤器：默认 INFO，只把我们自己的 crate（nota_*）放开到
    // TRACE/DEBUG——第三方库（h2/hyper/reqwest/tungstenite 等）的帧级/
    // 连接级 DEBUG 噪音（如 h2 的 `received frame=...`）一律不进日志，
    // 也不用维护第三方 crate 黑名单（新依赖自动就是 INFO）。
    // 注意：per-layer Filter 对事件真正生效的是内置 Targets 这类实现；
    // 手写 enabled/callsite_enabled 的 Filter 在本版本里对事件不拦截。
    let file_filter = Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("nota_core", LevelFilter::TRACE)
        .with_target("nota_infra", LevelFilter::TRACE)
        .with_target("nota_llm", LevelFilter::TRACE)
        .with_target("nota_onebot", LevelFilter::TRACE)
        .with_target("nota_cli", LevelFilter::TRACE);

    let file_layer = fmt::layer()
        .with_writer(non_blocking_writer)
        .with_timer(timer)
        .with_target(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(false)
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init()
        .ok();

    Ok(guard)
}

/// 直接运行 `nota` 就是启动服务：配置文件缺失或损坏时直接报错并停止，
/// 不自动进入向导——首次配置请先运行 `nota onboard`。
fn load_config_for_server(store: &ConfigStore) -> Result<nota_infra::Config> {
    store.load().map_err(|e| {
        anyhow::anyhow!(
            "Failed to load ~/.nota/config.toml: {e}. Run `nota onboard` to configure the API first."
        )
    })?;
    store
        .get()
        .ok_or_else(|| anyhow::anyhow!("config loaded but not stored"))
}

async fn run_persona_command(base: &Path, action: PersonaCommand) -> Result<()> {
    let persona_store = FilePersonaStore::new(base);

    match action {
        PersonaCommand::List => {
            let names = persona_store.list_personas().await?;
            if names.is_empty() {
                println!("No personas found.");
            } else {
                for name in names {
                    println!("{name}");
                }
            }
        }
        PersonaCommand::Create { name } => {
            let name = match name {
                Some(name) => name,
                None => config_wizard::prompt_create_persona()?,
            };
            let name = name.trim();
            if name.is_empty() {
                anyhow::bail!("persona name cannot be empty");
            }
            persona_store.create_persona(name).await?;
            println!("Persona '{name}' created");
        }
    }

    Ok(())
}

async fn run_server(
    base: &Path,
    config: nota_infra::Config,
    cancel_token: CancellationToken,
) -> Result<()> {
    let permissions = Arc::new(PermissionRegistry::new());
    let path_policy = Arc::new(PathPolicy::new());

    let persona_store: Arc<dyn PersonaStore> = Arc::new(FilePersonaStore::new(base));
    let conversation_dir = base.join("conversation");
    let manager = Arc::new(ConversationManager::new());
    let scheduler: Arc<dyn Scheduler> = Arc::new(TokioScheduler::new(manager.clone()));
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiLlm::new(
        &config.api_url,
        &config.api_key,
        &config.model,
        config.web_search,
    ));

    let tool_registry: Arc<ToolRegistryImpl> = Arc::new(ToolRegistryImpl::new());
    register_builtin_tools(
        &tool_registry,
        base.join("personas"),
        scheduler.clone(),
        path_policy.clone(),
    );
    register_chat_tools(tool_registry.as_ref());

    let persona_names = persona_store.list_personas().await?;
    if persona_names.is_empty() {
        anyhow::bail!(
            "No personas found in ~/.nota/personas/. Create one via `nota persona create` or `nota onboard` first."
        );
    }

    for name in &persona_names {
        let persona = Persona { name: name.clone() };
        let runtime = Arc::new(PersonaRuntime::new(
            persona,
            persona_store.clone(),
            conversation_dir.clone(),
            llm.clone(),
            tool_registry.clone(),
            permissions.clone(),
            path_policy.clone(),
        ));

        let persona_loop_runtime = runtime.clone();
        let manager_for_task = manager.clone();
        tokio::spawn(async move {
            persona_loop_runtime.run(manager_for_task).await;
        });

        info!("Persona '{}' started", name);
    }

    if let Some(onebot) = &config.onebot
        && onebot.enabled
    {
        start_onebot(
            manager.clone(),
            permissions.clone(),
            persona_store.clone(),
            tool_registry.clone(),
            onebot,
        )
        .await?;
    }

    let config_path = base.join("config.toml");
    let config_arc = Arc::new(tokio::sync::RwLock::new(config));
    let api_state = Arc::new(ApiState {
        persona_store,
        conversation_dir,
        config: config_arc,
        config_path,
    });

    let ctx = Arc::new(AppContext {
        manager: manager.clone(),
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

/// Wire the OneBot 11 adapter: resolve the target persona and start the bridge.
async fn start_onebot(
    manager: Arc<ConversationManager>,
    permissions: Arc<PermissionRegistry>,
    persona_store: Arc<dyn PersonaStore>,
    tool_registry: Arc<ToolRegistryImpl>,
    cfg: &OnebotConfig,
) -> Result<()> {
    if cfg.mode != "ws" {
        anyhow::bail!(
            "onebot.mode = '{}' is not supported yet; only 'ws' (forward WebSocket) is implemented",
            cfg.mode
        );
    }

    let personas = persona_store.list_personas().await?;
    let persona = if cfg.persona.is_empty() {
        personas.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!(
                "OneBot is enabled but no persona exists; create one or set [onebot].persona"
            )
        })?
    } else {
        if !personas.iter().any(|p| p == &cfg.persona) {
            anyhow::bail!(
                "onebot.persona '{}' does not exist under ~/.nota/personas",
                cfg.persona
            );
        }
        cfg.persona.clone()
    };

    let bridge = OneBotBridge::new(manager, permissions, persona.clone(), cfg.clone());
    bridge.register_tools(tool_registry.as_ref());
    tokio::spawn(async move { bridge.run().await });
    info!(
        "OneBot bridge started (mode={}, url={}, persona={persona})",
        cfg.mode, cfg.ws_url
    );
    Ok(())
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
        Some(Command::Persona { action }) => {
            run_persona_command(&base, action).await?;
        }
        None => {
            info!("Nota started");
            let config_store = ConfigStore::new(&base);
            let cancel_token = CancellationToken::new();
            let config = load_config_for_server(&config_store)?;
            run_server(&base, config, cancel_token).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_persona_list_command() {
        let cli = Cli::try_parse_from(["nota", "persona", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Persona {
                action: PersonaCommand::List,
            })
        ));
    }

    #[test]
    fn parses_persona_create_command() {
        let cli = Cli::try_parse_from(["nota", "persona", "create", "alice"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Persona {
                action: PersonaCommand::Create {
                    name: Some(name),
                },
            }) if name == "alice"
        ));
    }

    #[test]
    fn load_config_for_server_fails_fast_without_config() {
        let dir = std::env::temp_dir().join(format!("nota_config_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 无配置文件：报错并引导 `nota onboard`，而不是自动进向导
        let store = ConfigStore::new(&dir);
        let err = load_config_for_server(&store)
            .err()
            .expect("expected an error without config.toml")
            .to_string();
        assert!(err.contains("nota onboard"), "unexpected error: {err}");

        // 有效配置：正常加载
        std::fs::write(
            dir.join("config.toml"),
            "api_url = \"https://api.deepseek.com/v1\"\napi_key = \"k\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let cfg = load_config_for_server(&store).unwrap();
        assert_eq!(cfg.api_url, "https://api.deepseek.com/v1");
        assert_eq!(cfg.api_key, "k");
        assert_eq!(cfg.model, "m");

        // 损坏的 TOML：同样报错停止
        std::fs::write(dir.join("config.toml"), "not valid toml [[[").unwrap();
        assert!(load_config_for_server(&store).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 端到端验证文件层过滤器：h2 的 DEBUG/TRACE 被挡掉，自己 crate 的
    /// DEBUG 保留，第三方 INFO 保留。（tracing 全局 subscriber 只能初始化
    /// 一次，本测试独占这份初始化。）
    #[test]
    fn our_crates_debug_filter_blocks_third_party_noise() {
        let dir = std::env::temp_dir().join(format!("nota_log_filter_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("out.log");
        let file = std::fs::File::create(&log_path).unwrap();
        let console_file = std::fs::File::create(dir.join("console.log")).unwrap();

        let file_filter = tracing_subscriber::filter::Targets::new()
            .with_default(LevelFilter::INFO)
            .with_target("nota_core", LevelFilter::TRACE)
            .with_target("nota_infra", LevelFilter::TRACE)
            .with_target("nota_llm", LevelFilter::TRACE)
            .with_target("nota_onebot", LevelFilter::TRACE)
            .with_target("nota_cli", LevelFilter::TRACE);

        let init_ok = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(console_file)
                    .with_filter(LevelFilter::INFO),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_filter(file_filter),
            )
            .try_init();
        assert!(
            init_ok.is_ok(),
            "tracing subscriber init failed: {init_ok:?}"
        );

        let h2_noise = "received frame=Data { stream_id: StreamId(1) }";
        tracing::debug!(target: "h2::codec::framed_read", "{}", h2_noise);
        tracing::debug!(
            target: "nota_core::conversation",
            "[in] conversation 'onebot_private_1' -> persona 'alice' from 'u': hello"
        );
        tracing::info!(target: "h2", "connection established");

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            content.contains("nota_core::conversation"),
            "our DEBUG must be recorded, got:\n{content}"
        );
        assert!(
            !content.contains("received frame"),
            "third-party DEBUG must be filtered out, got:\n{content}"
        );
        assert!(
            content.contains("connection established"),
            "third-party INFO must be kept, got:\n{content}"
        );

        // 控制台层（INFO）在全局 current()=TRACE 时也不能漏 DEBUG
        let console_content = std::fs::read_to_string(dir.join("console.log")).unwrap();
        assert!(
            !console_content.contains("received frame"),
            "console must not show h2 DEBUG, got:\n{console_content}"
        );
        assert!(
            console_content.contains("connection established"),
            "console must keep INFO, got:\n{console_content}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
