# AGENTS.md

## Required reading

Before making changes, read:
- `.agent/guide.md` — architecture, commit conventions, pitfalls
- `.agent/notes.md` — design decisions, refactor history, naming rules

## Commands

```sh
cargo build                          # build (default: nota-cli)
cargo run -p nota-cli                # run server (REST + WS on :2349)
cargo run -p nota-cli -- webui       # serve webui/dist/ on :5173
cargo run -p nota-cli -- onboard     # configure API + create a persona
cargo check                          # type-check
cargo check -p nota-core             # type-check single crate
cargo clippy --all-targets           # lint

# webui (submodule — see "Web UI submodule" below)
cd webui && bun install              # one-time
cd webui && bun run dev              # Vite dev server on :5173
cd webui && bun run build            # production build → webui/dist/
```

No tests, no CI exists in this repo.

## Architecture

```
nota-cli → nota-infra → nota-core   (one-way; core never sees axum/reqwest)
```

| Crate | What it does |
|-------|--------------|
| `nota-core` | Domain types + port traits (`PersonaStore`, `LlmClient`, `Tool`, `ToolRegistry`, `AgentRunner`), `EventBus`, `PermissionRegistry`. Pure: no I/O deps. |
| `nota-infra` | Adapters: `axum` HTTP (REST + WebSocket), filesystem persona store, `OpenAiLlm`, TOML config, `ToolRegistryImpl`, built-in tools. |
| `nota-cli` | Binary (`nota`). Wires adapters into core, subcommands `onboard` / `webui` / (default) run server. |

## Web UI submodule

`webui/` is a **git submodule** tracking `https://github.com/Notaen/Nota.Webui.git`. It is its own repository, not part of `Nota.Core`.

Clone with submodules:
```sh
git clone --recurse-submodules https://github.com/Notaen/Nota.Core.git
# or, after a plain clone:
git submodule update --init
```

Build the web UI:
```sh
cd webui
bun install
bun run build           # output → webui/dist/
```

The `nota webui` subcommand serves `webui/dist/` on `http://127.0.0.1:5173`. The browser-side code connects to the running `nota` server (default `http://127.0.0.1:2349`) via WebSocket and REST.

Override the webui directory via `NOTA_WEBUI_DIR=<path>`.

When updating the webui submodule:
```sh
cd webui
git pull origin main            # or whatever branch
cd ..
git add webui
git commit -m "chore: bump webui submodule"
```

## Critical rules

- **Do not delete or modify comments** without understanding them. Chinese comments are authoritative.
- **Keep core pure**: never add `axum`, `reqwest`, `serde_json`, `tracing`, `dialoguer`, `dirs`, `walkdir`, `tokio-tungstenite` to `nota-core`. `tokio` (sync only) and `serde` are fine.
- **Domain types over generic wrappers**: `nota-core` defines its own types for domain concepts (e.g. `ToolParams`, `PropertyDef` for JSON Schema). Do NOT use `serde_json::Value` or raw `String` as parameter types — model the domain directly. Serialization to/from JSON happens at the infra boundary.
- **Logging boundary**: core/infra use `log::*` facade; only `nota-cli` uses `tracing`. `tracing-log::LogTracer` bridges them.
- **DI only**: no `OnceLock<T>` or `RwLock<Option<T>>` for manager singletons. `nota-cli` creates adapters and injects them via `Arc`.
- **Edition 2024**: requires nightly Rust.
