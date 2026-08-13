# AGENTS.md

## Required reading

Before making changes, read:
- `.agent/guide.md` — architecture, commit conventions, pitfalls
- `.agent/notes.md` — design decisions, refactor history, naming rules

## Commands

```sh
cargo build                          # build (default: nota-cli)
cargo run -p nota-cli                # run server (REST + WS on :2349)
cargo run -p nota-cli -- onboard     # configure API + create a persona
cargo check                          # type-check
cargo check -p nota-core             # type-check single crate
cargo clippy --all-targets           # lint
```

No tests, no CI exists in this repo.

## Architecture

```
nota-cli → nota-infra → nota-core
nota-cli → nota-onebot → nota-core   (one-way; core never sees axum/reqwest/tungstenite)
```

| Crate | What it does |
|-------|--------------|
| `nota-core` | Domain types + port traits (`PersonaStore`, `LlmClient`, `Tool`, `ToolRegistry`, `AgentRunner`), `EventBus`, `PermissionRegistry`. Pure: no I/O deps. |
| `nota-infra` | Adapters: `axum` HTTP (REST + WebSocket), filesystem persona store, `OpenAiLlm`, TOML config, `ToolRegistryImpl`, built-in tools. |
| `nota-onebot` | OneBot 11 forward-WS transport: `OnebotConfig`, protocol types, WS client, bus bridge, `read_group_chat` tool. Depends only on `nota-core`. |
| `nota-cli` | Binary (`nota`). Wires adapters into core, subcommands `onboard` / (default) run server. |

## Critical rules

- **Do not delete or modify comments** without understanding them. Chinese comments are authoritative.
- **Keep core pure**: never add `axum`, `reqwest`, `serde_json`, `tracing`, `dialoguer`, `dirs`, `walkdir`, `tokio-tungstenite` to `nota-core`. `tokio` (sync only) and `serde` are fine.
- **Domain types over generic wrappers**: `nota-core` defines its own types for domain concepts (e.g. `ToolParams`, `PropertyDef` for JSON Schema). Do NOT use `serde_json::Value` or raw `String` as parameter types — model the domain directly. Serialization to/from JSON happens at the infra boundary.
- **Logging boundary**: core/infra use `log::*` facade; only `nota-cli` uses `tracing`. `tracing-log::LogTracer` bridges them.
- **DI only**: no `OnceLock<T>` or `RwLock<Option<T>>` for manager singletons. `nota-cli` creates adapters and injects them via `Arc`.
- **Edition 2024**: requires nightly Rust.
- **Never edit `solo.md` without asking**: both the `nota-infra/assets/solo.md`
  template and user persona files (`~/.nota/personas/*/solo.md`) are treated as
  user configuration. Propose changes first, apply only after explicit approval.
