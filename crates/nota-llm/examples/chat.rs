//! Debug CLI for the LLM session module: create / load sessions and chat
//! through the real API, so tool calls (including server-side tools like
//! `web_search`) can be exercised against a live provider.
//!
//! Run from anywhere; session files land in the **current working
//! directory** (`./<conversation_id>/<uuid>.db`):
//!
//! ```text
//! cargo run -p nota-llm --example chat -- create "hello"
//! cargo run -p nota-llm --example chat -- <uuid> "what's new?"
//! cargo run -p nota-llm --example chat -- list
//! ```
//!
//! Missing `--url` / `--model` / `--key` options are read from
//! `.nota/config.toml` (cwd first, then the home directory); the chosen
//! config source is printed at startup.

#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use nota_core::conversation::ConversationManager;
use nota_core::permissions::PermissionRegistry;
use nota_core::session::{MessageRole, SessionItem, SessionManager, ToolCallKind};
use nota_core::tool::ToolRegistry;
use nota_infra::config::Config;
use nota_llm::{LlmConfig, SqliteSessionManager};

const DEFAULT_SYSTEM: &str = "You are Nota, a helpful assistant. Answer concisely in the \
                              user's language. When you need current or external information, \
                              use the web_search tool.";

#[derive(Default)]
struct Options {
    url: Option<String>,
    model: Option<String>,
    key: Option<String>,
    no_web_search: bool,
    system: Option<String>,
    root: Option<PathBuf>,
}

enum Command {
    Create { message: Option<String> },
    Send {
        session_id: String,
        message: String,
    },
    List,
}

struct Cli {
    command: Command,
    options: Options,
}

fn usage() -> ! {
    println!(
        r#"Nota LLM debug CLI

Usage:
  chat create [message...]                     create a session and optionally chat
  chat <session_id> <message...>               load a session by id and chat
  chat list                                    list all sessions

Options:
  --url <URL>       API base url (e.g. https://api.deepseek.com/v1)
  --model <MODEL>   model name
  --key <KEY>       API key
  --no-web-search   disable the server-side web_search tool
  --system <TEXT>   system prompt (default: built-in)
  --root <DIR>      session storage root (default: current directory)
  -h, --help        print this help

Missing options are read from .nota/config.toml (cwd first, then home).
Sessions are stored under <root>/<conversation_id>/<uuid>.db."#
    );
    std::process::exit(0);
}

fn parse_args() -> Result<Cli> {
    let mut options = Options::default();
    let mut positionals: Vec<String> = Vec::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => usage(),
            "--url" => {
                i += 1;
                options.url = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("--url needs a value"))?,
                );
            }
            "--model" => {
                i += 1;
                options.model = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("--model needs a value"))?,
                );
            }
            "--key" => {
                i += 1;
                options.key = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("--key needs a value"))?,
                );
            }
            "--system" => {
                i += 1;
                options.system = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("--system needs a value"))?,
                );
            }
            "--root" => {
                i += 1;
                options.root = Some(
                    args.get(i)
                        .cloned()
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow!("--root needs a value"))?,
                );
            }
            "--no-web-search" => options.no_web_search = true,
            flag if flag.starts_with("--") => bail!("unknown option: {flag}"),
            _ => positionals.push(args[i].clone()),
        }
        i += 1;
    }

    let command = match positionals.as_slice() {
        [] => usage(),
        [head, rest @ ..] if head == "create" => Command::Create {
            message: (!rest.is_empty()).then(|| rest.join(" ")),
        },
        [head, ..] if head == "list" => Command::List,
        [session_id, rest @ ..] => {
            if rest.is_empty() {
                bail!("a message is required: chat <session_id> <message...>");
            }
            Command::Send {
                session_id: session_id.clone(),
                message: rest.join(" "),
            }
        }
    };
    Ok(Cli { command, options })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn config_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join(".nota").join("config.toml"));
    }
    if let Some(home) = home_dir() {
        out.push(home.join(".nota").join("config.toml"));
    }
    out
}

fn resolve_config(options: &Options) -> Result<(LlmConfig, String)> {
    let mut file: Option<Config> = None;
    let mut source = "CLI arguments".to_string();
    for path in config_candidates() {
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            file = Some(
                toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?,
            );
            source = format!("config file {}", path.display());
            break;
        }
    }
    let (api_url, api_key, model, web_search) = match file {
        Some(cfg) => (
            options
                .url
                .clone()
                .unwrap_or_else(|| cfg.api_url.clone()),
            options
                .key
                .clone()
                .unwrap_or_else(|| cfg.api_key.clone()),
            options
                .model
                .clone()
                .unwrap_or_else(|| cfg.model.clone()),
            if options.no_web_search {
                false
            } else {
                cfg.web_search
            },
        ),
        None => (
            options
                .url
                .clone()
                .ok_or_else(|| anyhow!("missing api_url; pass --url or create .nota/config.toml"))?,
            options
                .key
                .clone()
                .ok_or_else(|| anyhow!("missing api_key; pass --key or create .nota/config.toml"))?,
            options
                .model
                .clone()
                .ok_or_else(|| anyhow!("missing model; pass --model or create .nota/config.toml"))?,
            !options.no_web_search,
        ),
    };
    Ok((
        LlmConfig {
            api_url,
            api_key,
            model,
            web_search,
        },
        source,
    ))
}

fn print_transcript(history: &[(i64, SessionItem)]) {
    for (row, item) in history {
        match item {
            SessionItem::Message { role, content } => {
                let label = match role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Context => "context",
                };
                println!("[{row}] {label}: {content}");
            }
            SessionItem::Reasoning { content } => {
                println!("[{row}] reasoning: {content}");
            }
            SessionItem::ToolCall(call) => match call.kind {
                ToolCallKind::FunctionCall => println!(
                    "[{row}] tool_call: {}({})",
                    call.name.as_deref().unwrap_or("<no name>"),
                    call.arguments.as_deref().unwrap_or("{}")
                ),
                ToolCallKind::WebSearchCall => {
                    let args = call.arguments.as_deref().unwrap_or("<no query>");
                    println!("[{row}] tool_call: web_search (server-side, no local run) args={args}");
                }
            },
            SessionItem::ToolCallOutput { call_id, output } => {
                println!("[{row}] tool_result[{call_id}]: {output}");
            }
        }
    }
}

/// Minimal stderr logger so operator diagnostics (e.g. rejected tool
/// arguments with provider/model/reasons) are visible while debugging.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        eprintln!("[{}] {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

fn init_logger() {
    static LOGGER: StderrLogger = StderrLogger;
    log::set_logger(&LOGGER).ok();
    log::set_max_level(log::LevelFilter::Debug);
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logger();
    let cli = parse_args()?;
    let root = cli
        .options
        .root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    println!("storage root  : {}", root.display());

    let config = if matches!(&cli.command, Command::List) {
        // Listing local sessions needs no API connection.
        LlmConfig {
            api_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            web_search: false,
        }
    } else {
        let (config, config_source) = resolve_config(&cli.options)?;
        println!("config source : {config_source}");
        println!("api_url       : {}", config.api_url);
        println!("model         : {}", config.model);
        println!(
            "web_search    : {}",
            if config.web_search { "enabled" } else { "disabled" }
        );
        config
    };

    let system = cli
        .options
        .system
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM.to_string());

    let manager = SqliteSessionManager::new(
        &root,
        "example".to_string(),
        system,
        String::new(),
        Arc::new(ToolRegistry::new()),
        Arc::new(ConversationManager::new()),
        Arc::new(PermissionRegistry::new()),
        config,
    )?;

    match cli.command {
        Command::Create { message } => {
            let session = manager.create().await?;
            println!("created session : {}", session.id());
            println!(
                "session file    : {}",
                root.join(format!("{}.db", session.id())).display()
            );
            if let Some(message) = message {
                println!(">> {message}");
                session.send(message, None).await?;
                print_transcript(&session.raw_history().await?);
            }
        }
        Command::Send {
            session_id,
            message,
        } => {
            let session = manager
                .load(&session_id)
                .await?
                .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
            println!("loaded session  : {}", session.id());
            println!(">> {message}");
            session.send(message, None).await?;
            print_transcript(&session.raw_history().await?);
        }
        Command::List => {
            let sessions = manager.list().await?;
            for session in sessions {
                println!("{}", session.id());
            }
        }
    }
    Ok(())
}
