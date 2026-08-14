use anyhow::Result;
use console::{Key, Term};
use dialoguer::{Confirm, Input, Select};

use nota_infra::{Config, provider_default_model, provider_ids, provider_name, provider_url};
use nota_onebot::OnebotConfig;

/// Run the interactive config wizard. If `existing` is provided, its values
/// are used as defaults so the user can edit an existing configuration.
pub fn run_wizard(existing: Option<&Config>) -> Result<Config> {
    println!("==== Nota Configuration Wizard ====\n");

    let builtin_ids = provider_ids();
    let mut menu_items: Vec<String> = builtin_ids
        .iter()
        .map(|id| provider_name(id).unwrap_or(id).to_string())
        .collect();
    menu_items.push("Custom".to_string());

    let default_idx = existing
        .and_then(|cfg| {
            builtin_ids
                .iter()
                .position(|&id| provider_url(id) == Some(&cfg.api_url))
        })
        .unwrap_or(menu_items.len() - 1);

    let selection = Select::new()
        .with_prompt("API Provider")
        .items(&menu_items)
        .default(default_idx)
        .interact()?;

    let (api_url, existing_key) = if selection < builtin_ids.len() {
        let id = builtin_ids[selection];
        let name = provider_name(id).unwrap_or(id);
        let url = provider_url(id).unwrap_or("").to_string();
        let prompt = format!("{} API Key", name);
        let existing_key = existing
            .filter(|cfg| cfg.api_url == url)
            .map(|cfg| cfg.api_key.clone());
        let api_key = prompt_for_key(&prompt, existing_key)?;
        (url, Some(api_key))
    } else {
        let default_url = existing
            .map(|cfg| cfg.api_url.clone())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let existing_key = existing.map(|cfg| cfg.api_key.clone());
        let api_url: String = Input::new()
            .with_prompt("API Base URL")
            .default(default_url)
            .interact_text()?;
        let api_key = prompt_for_key("API Key", existing_key)?;
        (api_url, Some(api_key))
    };

    // api_key 已经在上面通过 prompt_for_key 获取到了
    let api_key = existing_key.unwrap();

    let default_model = existing
        .map(|cfg| cfg.model.clone())
        .or_else(|| {
            if selection < builtin_ids.len() {
                provider_default_model(builtin_ids[selection]).map(|s| s.to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "gpt-4o".to_string());

    let model: String = Input::new()
        .with_prompt("Model")
        .default(default_model)
        .interact_text()?;

    let web_search = Confirm::new()
        .with_prompt("Enable the built-in web_search tool (Responses API)?")
        .default(existing.map(|cfg| cfg.web_search).unwrap_or(true))
        .interact()?;

    let onebot = prompt_onebot(existing)?;

    let cfg = Config {
        api_url,
        api_key,
        model,
        web_search,
        onebot,
    };

    // 展示最终配置，让用户检查
    println!();
    println!("══════════════════════════════════════");
    println!("  Configuration Summary");
    println!("══════════════════════════════════════");
    println!("  API URL : {}", cfg.api_url);
    println!("  API Key : {}", mask_key(&cfg.api_key));
    println!("  Model   : {}", cfg.model);
    println!("  WebSearch: {}", if cfg.web_search { "enabled" } else { "disabled" });
    match &cfg.onebot {
        Some(ob) if ob.enabled => {
            println!("  OneBot  : enabled ({} -> {})", ob.mode, ob.ws_url);
        }
        _ => println!("  OneBot  : disabled"),
    }
    println!("══════════════════════════════════════");
    println!();

    let save_confirm = Confirm::new()
        .with_prompt("Save this configuration?")
        .default(true)
        .interact()?;

    if !save_confirm {
        anyhow::bail!("Configuration cancelled by user");
    }

    Ok(cfg)
}

/// Ask whether to enable OneBot 11 and collect its connection settings.
/// Returns `None` when the user opts out.
fn prompt_onebot(existing: Option<&Config>) -> Result<Option<OnebotConfig>> {
    println!();
    let enable = Confirm::new()
        .with_prompt("Enable OneBot 11 (QQ bot) support?")
        .default(
            existing
                .and_then(|c| c.onebot.as_ref())
                .map(|ob| ob.enabled)
                .unwrap_or(false),
        )
        .interact()?;

    if !enable {
        return Ok(None);
    }

    let defaults = existing.and_then(|c| c.onebot.clone());
    let default_url = defaults
        .as_ref()
        .map(|ob| ob.ws_url.clone())
        .unwrap_or_else(|| "ws://127.0.0.1:3001".to_string());
    let ws_url: String = Input::new()
        .with_prompt("OneBot WebSocket URL (forward, e.g. ws://127.0.0.1:3001)")
        .default(default_url)
        .interact_text()?;

    let access_token: String = prompt_masked("OneBot access token (leave empty if none)", true)?;

    let default_persona = defaults
        .as_ref()
        .map(|ob| ob.persona.clone())
        .unwrap_or_default();
    let persona: String = Input::new()
        .with_prompt("Persona name to handle OneBot messages (empty = first persona)")
        .default(default_persona)
        .allow_empty(true)
        .interact_text()?;

    let default_friends = defaults
        .as_ref()
        .map(|ob| ob.friend_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    let friends: String = Input::new()
        .with_prompt("Allowed friend QQ ids, comma separated (empty = none)")
        .default(default_friends)
        .allow_empty(true)
        .interact_text()?;

    let default_groups = defaults
        .as_ref()
        .map(|ob| ob.group_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    let groups: String = Input::new()
        .with_prompt("Allowed group ids, comma separated (empty = none)")
        .default(default_groups)
        .allow_empty(true)
        .interact_text()?;

    Ok(Some(OnebotConfig {
        enabled: true,
        mode: "ws".to_string(),
        ws_url,
        access_token,
        persona,
        prefix: String::new(),
        friend_ids: parse_id_list(&friends),
        group_ids: parse_id_list(&groups),
    }))
}

/// Parse a comma/whitespace separated list of numeric ids.
fn parse_id_list(input: &str) -> Vec<i64> {
    input
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect()
}

fn prompt_for_key(prompt: &str, existing: Option<String>) -> Result<String> {
    if let Some(key) = existing
        && !key.is_empty()
    {
        let masked = mask_key(&key);
        let display = format!("{prompt} [current: {masked}]");
        let input = prompt_masked(&display, true)?;
        if input.is_empty() {
            Ok(key)
        } else {
            Ok(input)
        }
    } else {
        prompt_masked(prompt, false)
    }
}

/// Read a secret from the terminal, echoing one `*` per character as it is
/// typed or pasted.
///
/// `dialoguer::Password` reads the whole line with terminal echo disabled
/// (`Term::read_secure_line`), so nothing appears on screen while the user
/// types or pastes — only after Enter does it print `[hidden]`, which makes
/// it look like the input was never received. This prompt shows masked
/// feedback immediately while keeping the actual value hidden.
fn prompt_masked(prompt: &str, allow_empty: bool) -> Result<String> {
    let term = Term::stderr();
    // 与 dialoguer 一致：非终端环境下直接报错，而不是无限等待按键
    if !term.is_term() {
        anyhow::bail!("not a terminal");
    }

    loop {
        term.write_str(&format!("{prompt}: "))?;
        term.flush()?;

        let mut buf = String::new();
        let mut masked = 0usize; // 已打印的掩码字符数
        loop {
            match term.read_key()? {
                Key::Char(c) if !c.is_ascii_control() => {
                    buf.push(c);
                    term.write_str("*")?;
                    masked += 1;
                }
                Key::Backspace if !buf.is_empty() => {
                    buf.pop();
                    if masked > 0 {
                        term.clear_chars(1)?;
                        masked -= 1;
                    }
                }
                // Windows 下 Ctrl+C 以 '\x03' 到达（Unix 下 read_key 会自行触发 SIGINT）
                Key::Char('\x03') => anyhow::bail!("interrupted"),
                Key::Enter => break,
                _ => {}
            }
            term.flush()?;
        }

        // 抹掉输入期间显示的掩码，再用 [hidden]/[empty] 标记收尾
        if masked > 0 {
            term.clear_chars(masked)?;
        }
        if buf.is_empty() && !allow_empty {
            term.write_line("")?;
            term.write_line(&format!("error: {prompt} cannot be empty"))?;
            continue;
        }
        term.write_line(if buf.is_empty() { "[empty]" } else { "[hidden]" })?;
        return Ok(buf);
    }
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        "*".repeat(key.len())
    } else {
        format!("{}****{}", &key[..4], &key[key.len() - 4..])
    }
}

pub fn prompt_create_persona() -> Result<String> {
    println!();
    println!("Now let's create your first persona.");
    println!("A persona is an AI identity with its own solo.md (system prompt)");
    println!("and memory.md (persistent memory).\n");

    let name: String = Input::new()
        .with_prompt("Persona name")
        .interact_text()?;

    Ok(name)
}
