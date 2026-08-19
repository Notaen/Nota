# AGENTS.md

## Required reading

Start with the human docs to understand what Nota is and how it works:

- `README.md` (or `README.zh.md`) — what Nota does, quick start, OneBot setup
- `CONTRIBUTING.md` — full architecture, HTTP API, directory layout, code
  conventions

Then read the agent-specific docs before making changes:
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
cargo test --workspace               # run all tests
```

Tests live next to the code (unit tests per crate); there is no CI yet.

## Cross-compilation (linux-arm64)

Verified feasible; all deps cross-compile. The only native C deps are
`rusqlite` (bundled SQLite) and `aws-lc-sys` (via `reqwest` 0.13 → rustls
default provider, which additionally needs host `cmake`). A cross C compiler
is therefore required.

```sh
# Route 1: any host, cargo-zigbuild (zig supplies C compiler + linker)
scoop install zig                 # or download from ziglang.org
cargo install cargo-zigbuild
rustup target add aarch64-unknown-linux-gnu
cargo zigbuild --release --target aarch64-unknown-linux-gnu
# binary: target/aarch64-unknown-linux-gnu/release/nota

# Route 2: Linux host, classic cross gcc
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

Use `aarch64-unknown-linux-musl` for a fully static binary (works with
zigbuild; requires no glibc at runtime).

## Architecture

```
nota-cli → nota-infra → nota-core
nota-cli → nota-llm → nota-core
nota-cli → nota-onebot → nota-core
```

Only the composition root (`nota-cli`) references `nota-llm`; `nota-infra`
and `nota-onebot` hold the core abstractions (`SessionManager` / `Tool` /
`ToolRegistry`) and never see the LLM client or the turn loop.

| Crate | What it does |
|-------|--------------|
| `nota-core` | Domain types + port traits (`PersonaStore`, `Scheduler`, `SessionManager`/`Session`), `ConversationManager` (user-visible chat routing), `PermissionRegistry`, `PathPolicy`, and the tool contract (`Tool`/`ToolContext`/`ToolParams`/`ToolRegistry`, in-memory). Pure: no I/O deps, no LLM wire types. |
| `nota-llm` | Concrete session manager (`SqliteSessionManager`, implements core `SessionManager`; one `<uuid>.db` per session; runs the whole turn — LLM call + tool loop — internally) and the internal Responses API client. No public `LlmClient`/`AgentRunner`; only `nota-cli` references this crate. |
| `nota-infra` | Adapters: `axum` HTTP (REST + WebSocket), filesystem persona store, `PersonaRuntime` (the conversation layer: lazily creates one session manager per conversation, owns conversation → session mapping + `current.json`, slash commands), TOML config, built-in tools, scheduler. |
| `nota-onebot` | OneBot 11 forward-WS transport: `OnebotConfig`, protocol types, WS client, bus bridge, `onebot_*` tools. Depends only on `nota-core`. |
| `nota-cli` | Binary (`nota`). Wires adapters into core, subcommands `onboard` / (default) run server. |

## Terminology

- **session** = one LLM-level dialogue (OpenAI-style message items, system
  prompt excluded), abstracted in `nota-core` (`Session` / `SessionManager`)
  and implemented by `nota-llm`'s `SqliteSessionManager`. Ids are
  **plain uuids** — sessions are conversation-agnostic and stored flat as
  `<uuid>.db` under the manager's storage path. `PersonaRuntime` gives each
  conversation its own directory (`~/.nota/conversation/<persona>/<conversation_id>/`
  with flat `<uuid>.db` files), persists the current session id in
  `current.json` inside that directory, and `//clear` archives the old
  session and starts a fresh one. Each conversation's manager is created
  with its own tool set, including a conversation-bound `reply` tool —
  sessions never know which conversation they belong to. Callers only
  `send(content, request_id)` into a session — the turn loop, tools, and
  delivery are internal.
- **conversation** = the user-visible chat (OneBot private/group, web) owned by an
  adapter; core routes by conversation.

## Documentation conventions (do not violate)

- `README.md` — **English**, end-user focused.
- `README.zh.md` — **Chinese**, end-user focused, kept in sync with
  `README.md`. This file already exists; never write Chinese into
  `README.md` or ignore the zh version.
- `CONTRIBUTING.md` — **English**, developer focused (architecture, API,
  layout).
- `AGENTS.md` / `.agent/*` — **English**, for AI coding assistants.

Before editing any documentation, list the repo root (`Get-ChildItem -Force`)
to discover all doc files. Keep architecture/API details out of the READMEs —
they belong in `CONTRIBUTING.md` and `.agent/`.

## Critical rules

- **Do not delete or modify comments** without understanding them. Chinese comments are authoritative.
- **Keep core pure**: never add `axum`, `reqwest`, `serde_json`, `tracing`,
  `dialoguer`, `dirs`, `walkdir`, `tokio-tungstenite`, `rusqlite` to
  `nota-core`. `tokio` (sync only) and `serde` are fine. The `Tool` contract
  carries parsed arguments as `HashMap<String, Value>` using core's own
  JSON-like `Value` type (mirroring `serde_json::Value`'s shape; the llm
  layer deserializes the model's raw arguments into it and validates them
  against the tool's `parameters` before calling). LLM wire types and I/O
  (Responses API, SQLite) live in `nota-llm`; the session and tool
  abstractions live in `nota-core`.
- **Domain types over generic wrappers**: `nota-core` defines its own types for domain concepts (e.g. `ToolParams`, `PropertyDef` for JSON Schema). Do NOT use `serde_json::Value` or raw `String` as parameter types — model the domain directly. Serialization to/from JSON happens at the infra boundary.
- **Logging boundary**: core/infra use `log::*` facade; only `nota-cli` uses `tracing`. `tracing-log::LogTracer` bridges them.
- **DI only**: no `OnceLock<T>` or `RwLock<Option<T>>` for manager singletons. `nota-cli` creates adapters and injects them via `Arc`.
- **Edition 2024**: requires rustc ≥ 1.85 (stable since 1.85 — nightly is NOT needed).
- **Never edit `solo.md` without asking**: both the `nota-infra/assets/solo.md`
  template and user persona files (`~/.nota/personas/*/solo.md`) are treated as
  user configuration. Propose changes first, apply only after explicit approval.
