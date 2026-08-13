# Developer Notes

## Code Modification Rules
- Do NOT delete, modify, or remove existing comments without explicit approval.
- When in doubt about a comment change, ask first.

## Directory Naming
- Use `personas` (plural) for the persona directory under `base_dir()`.
  - `base_dir().join("personas")` not `base_dir().join("persona")`.

## Design Decisions (from Chinese review comments)

### No Default Persona
- `PersonaManager` must NOT auto-create a default persona on init.
- **No hardcoded persona names** (removed `const DEFAULT_PERSONA`).
- The `current_persona` field starts as `None`.
- Persona creation must be explicit — user must opt in (via CLI wizard, config, or API).
- Ref: `src/persona/manager.rs`

### OnceLock over RwLock<Option<T>>
- Use `OnceLock<T>` for singletons that are set once at startup and never unset.
- Do NOT use `RwLock<Option<T>>` — it adds unnecessary complexity and allows invalid states.
- Ref: `src/persona/manager.rs`

### Persona File Caching
- Persona files (`solo.md`, `memory.md`, etc.) are cached in a global `HashMap<PathBuf, (String, SystemTime)>`.
- Cache key: file path. Cache value: (content, mtime).
- On read: check file mtime against cache. If unchanged, return cached content.
- Write-through: after reading from disk, update cache.
- Ref: `src/persona/mod.rs`

### Persona Extensibility
- `Persona::read_file(filename)` is the generic method for any file under the persona workspace.
- `read_solo()` and `read_memory()` are convenience wrappers.
- `PersonaHandler` iterates over `PERSONA_FILES` slice — adding new files just means appending to the list.
- Ref: `src/persona/mod.rs`, `src/persona/handler.rs`

### Reduce Module Coupling
- `session::db` is private (not `pub mod db`); types are re-exported from `session/mod.rs`.
- `persona::handler` imports from `crate::session` instead of `crate::session::db`.
- TODO: A shared types module (`crate::types`) may be needed long-term to fully decouple `persona` and `session`.

### Consolidate Time Dependencies
- Use only `chrono` — removed the `time` crate.
- Custom `ChronoLocalTimer` implements `tracing_subscriber::fmt::time::FormatTime`.
- No more redundant timestamp libraries.

### SQLx Migration Naming
- Files must follow `YYYYMMDDHHMMSS_description.sql` format.
- Fixed: `20260706_init_session_db.sql` → `20260706000000_init_session_db.sql`.

### English/Grammar Cleanup
- Log messages and user-facing strings should be idiomatic English.

## Provider System

### Built-in Providers (DeepSeek, OpenRouter, Custom)
- Provider metadata (URL, default model) lives in `crates/nota-infra/assets/providers.toml`,
  compiled in via `include_str!`. Used ONLY by the config wizard to pre-fill defaults.
- Saved `config.toml` is flat: `api_url`, `api_key`, `model` — no provider type distinction at runtime.
- The wizard (`config_wizard::run_wizard`) accepts an existing `Config` as defaults for editing.
  Final config is displayed as a summary before saving.

### `nota onboard` command
- Uses `clap` derive. Runs the wizard standalone (no server start).
- `nota` with no subcommand starts the server normally (auto-wizard if config missing).

## Tool System (nota-core)

### Domain types over generics
- `ToolParams` + `PropertyDef` structs model JSON Schema directly, NOT `serde_json::Value`
  or raw `String`. These are domain types with clear semantics, not serialization helpers.
- `ToolParams::object(properties, required)` is the canonical constructor.
- Serialization to actual JSON happens only in `nota-infra` (via `serde_json::to_value`).
- This was the result of multiple review rounds:
  1. First tried `serde_json::Value` (wrong — serialization lib in core)
  2. Then tried `String` (wrong — lost type safety, unreadable)
  3. Then tried custom `JsonValue` enum (wrong — still a generic container, not domain-specific)
  4. Finally: `ToolParams` + `PropertyDef` (correct — models the domain)

### AgentRunner
- Tool calling loop: max 16 iterations, LLM → tool_calls → execute → append results → repeat.
- `ToolDef` + `ToolCall` + `LlmResponse` types in `nota-core::llm`.
- Tool calls/results stored as messages (`role: "tool_call"` / `"tool_result"`).
- The runner returns all new messages; caller is responsible for persistence.

### Built-in tools (nota-infra)
- `file_read`, `file_write` — sandboxed to persona workspace, request permission on violation
- `schedule` — stub implementation (scheduler not yet built)
- `get_version` — returns `env!("CARGO_PKG_VERSION")`
- Registered via `register_builtin_tools(registry, personas_dir)`

### OpenAiLlm
- OpenAI-compatible chat completions API.
- Request uses typed structs (`ChatMessage`, `ApiTool`), NOT raw `serde_json::Value` or `json!()`.
- Tool role translation: `tool_call` → assistant with `tool_calls`, `tool_result` → tool with `tool_call_id`.
- `ChatMessage` has optional `content`, `tool_calls`, `tool_call_id` fields.

## Hexagonal Refactor (workspace split)

The project was restructured into a Cargo workspace (`nota-core` / `nota-infra` /
`nota-cli`) using ports & adapters. Key decisions:

### Domain Purity (nota-core)
- Core entities (`Metadata`, `Message`, `Schedule`, `Persona`, `Session`) carry
  **no** `crudly::*` / `sqlx::FromRow` derives. Persistence row structs with
  those derives live only in `nota-infra/src/sqlite/row.rs`, bridged to core via
  `From` impls.
- `Session` no longer holds a `SqlitePool`; persistence is delegated to the
  `SessionRepository` port injected into `SessionManager`.
- `nota-core` `Cargo.toml` must NOT contain sqlx/crudly/axum/tracing/dialoguer/
  dirs/walkdir/tracing-subscriber.

### No Global State (DI)
- Removed `OnceLock<SessionManager>`, `OnceLock<PersonaManager>`, and
  `static BASE_DIR`. Managers take their ports (`Arc<dyn SessionRepository>`,
  `Arc<dyn PersonaStore>`, `Arc<dyn LlmClient>`) via constructors; `nota-cli`
  wires adapters in `main` and injects them.
- `PersonaManager` merged the old `PersonaHandler` and now `impl SessionHandler`
  directly; it is registered as the default handler via
  `SessionManager::register_handler_all`.
- `base_dir()` is resolved in `nota-cli` (`dirs::home_dir().join(".nota")`) and
  passed into adapters; the core never touches paths.

### Logging Boundary
- `nota-core`/`nota-infra` use the `log` facade only (`log::*`). `nota-cli` uses
  `tracing` + `tracing-log` (`LogTracer::init()`) to route `log` records into
  the tracing subscriber. Do not call `tracing::*` from core/infra.

### Deadlock Fix (DI side-effect)
- `set_archive_at` previously held `session_map.write()` then reentered the
  global `SessionManager::get().archive_expired_sessions()` — a reentrant
  deadlock. DI removed the global singleton, so the reentry is gone. The
  `// 这有bug` comment is retained with an explanatory addendum. Archive
  scheduling redesign is out of scope.

## Persona + Event Bus + WebSocket (2026)

Replaced the session/SQLite stack with an event-driven persona architecture.
The whole `session` module and `SqliteSessionRepository` were deleted; chat
state lives in `chatlog.json` per persona. The HTTP layer is now REST + WS
on `127.0.0.1:2349`, and the web UI is a separate repo (`Notaen/Nota.Webui`)
cloned as a git submodule.

### BusEvent.target
- `target: Option<String>` lets the HTTP layer route a user message to a
  specific persona. `PersonaRuntime::run` filters by `target`:
  `if let Some(t) = event.target && t != self.name { continue; }`.
  When `target` is `None`, all personas receive the event (broadcast).

### Permission flow
- Tool wants to do something user must approve (e.g. file outside workspace)
  → calls `ToolContext::request_permission(prompt)`.
- That registers a oneshot in `PermissionRegistry` and sends a
  `PermissionRequest` event on the bus with `parent_request_id` set to the
  user request id.
- The WS handler forwards it as `{type:"permission_needed", ...}` to the
  matching client.
- User clicks Allow/Deny → WS message → handler calls
  `PermissionRegistry::resolve(id, approved)` directly (no bus event).
- The tool's blocked await resumes; persona continues; final response
  flows back as `{type:"message", ...}`.

### Web UI as git submodule
- `webui/` was extracted into its own repo (`Notaen/Nota.Webui`) and added
  back to `Nota.Core` as a submodule pinned to a commit (gitlink mode
  `160000`).
- `.gitmodules` declares `url = https://github.com/Notaen/Nota.Webui.git` and
  `branch = main`. Local `.git/config` keeps the working-tree URL so the
  existing checkout still works; once the remote exists, normal
  `git submodule update` will fetch from it.
- `nota webui` is a separate subcommand serving `webui/dist/` on
  `127.0.0.1:5173` via `tower-http::services::ServeDir` with SPA fallback
  to `index.html`. Override via `NOTA_WEBUI_DIR`.

### Default persona is gone
- `nota` no longer auto-creates any persona on startup. It scans
  `~/.nota/personas/` and starts one `PersonaRuntime` per directory that
  has `solo.md`. Use `nota onboard` (wizard prompts for a name) or create
  the directory by hand. The HTTP API also has `POST /api/personas`.

### REST API for personas and settings
- `GET/POST /api/personas`, `GET/DELETE /api/personas/:name`,
  `GET/PUT /api/personas/:name/files/:filename`,
  `GET /api/personas/:name/chatlog`, `GET/PUT /api/settings`.
- `Config` is held in an `Arc<tokio::sync::RwLock<Config>>` inside
  `ApiState`; `PUT /api/settings` updates the in-memory copy and persists
  via `ConfigStore::save`.

### axum `ws` feature
- `axum = { workspace = true, features = ["ws"] }` is required in
  `nota-infra` and `nota-cli` for the WebSocketUpgrade extractor. Without
  it `axum::extract::ws::*` is a private module.

## OneBot 11 (2026-08)

### Scope & placement
- OneBot 11 is an **adapter in `nota-infra`** (`src/onebot/`), NOT part of
  `nota-core`. Written in plain Rust — no JS/plugin runtime (the `deno_core`
  detour was removed earlier; we are not going back).
- Transport: **forward WebSocket only** (`mode = "ws"`). The bot connects to
  the OneBot implementation's WS server (NapCat/LLOneBot default
  `ws://127.0.0.1:3001`), receives `post_type` events and sends
  `send_private_msg` / `send_group_msg` actions on the same connection.
  Reverse WS / HTTP modes are deliberately out of scope for now; `mode` is
  reserved in the config so they can be added later.
- `tokio-tungstenite` was already in the lock via `axum`'s `ws` feature; we
  declare it explicitly with `default-features = false` plus `connect` +
  `handshake` (client side + server handshake for tests).

### Config
- `OnebotConfig` lives in `config.toml` under `[onebot]` (`enabled`, `mode`,
  `ws_url`, `access_token`, `persona`, `prefix`); `enabled = false` by
  default. The `onboard` wizard asks for it.
- `persona` empty means "first persona found"; a configured name that does
  not exist fails server startup with a clear error.

### Routing
- The bridge keeps `request_id -> ReplyRoute`; each incoming OneBot message
  becomes a **targeted** bus event for the persona. The persona's reply
  (matched by `request_id`) is sent back to the originating private/group
  chat; long replies are chunked at 4000 chars.
- Group messages are prefixed `[nickname] ` (card > nickname > user_id) so the
  LLM knows who spoke.
- `BusEvent::message_with_context` was added to `nota-core` and the persona
  runtime now echoes the inbound `context` into reply events — the bus context
  field is no longer dead weight and future channels can route on it.

### Permissions
- OneBot has no interactive approval channel, so `PermissionRequest` events
  whose `parent_request_id` matches an active OneBot route are **auto-denied**
  and a notice is sent to the chat. This prevents the persona loop from
  hanging on an unanswered oneshot.

### Tests
- `onebot/types.rs`: event/segment parsing (array + string `message`),
  action serialization, chunking.
- `onebot/client.rs::ws_roundtrip`: a real WS server accepts the client,
  pushes a private-message event, and verifies the outgoing action.

### Field notes (2026-08-13, live test against NapCat)
- Live round trip verified: QQ message → NapCat forward WS (`ws://192.168.10.138:3001`,
  `Authorization: Bearer <token>`) → nota bridge → persona/DeepSeek → reply →
  `send_private_msg` → NapCat `retcode=0`.
- NapCat's forward WS server **resets the connection when the client sends an
  unsolicited Ping** (disconnects exactly at the client ping interval, 30s and
  later 10s in testing). Fixed by removing client-initiated pings; the client
  only pongs the server's pings. tungstenite also only *queues* a pong on
  receipt and flushes it on the next write, so explicit `send(Pong)` is kept.
- NapCat must listen on `0.0.0.0` (not `127.0.0.1`) for LAN clients; the WS
  access token is sent as `Authorization: Bearer <token>` on upgrade.
- OneBot does not queue events for disconnected clients — messages sent during
  a disconnect window are lost (observed: the first "你好" never arrived).

### Allowlist policy (2026-08-13)
- `OnebotConfig` now has `friend_ids` / `group_ids` allowlists. Only messages
  from those friends/groups reach the persona (and thus the LLM); everything
  else is dropped in the bridge before any LLM call (debug log only).
- Empty list = nobody in that category is allowed. This replaced the earlier
  "respond to everyone" default — the user explicitly wants a private-by-
  default bot that only talks to configured people/groups.

### OneBot extracted to its own crate (2026-08-13)
- `nota-onebot` is a standalone workspace crate (depends only on `nota-core`),
  containing `OnebotConfig`, protocol types, WS client, bus bridge, and tools.
  Nothing OneBot-related lives in `nota-infra` anymore.
- `nota-infra` keeps `Config.onebot: Option<OnebotConfig>` and re-exports
  `OnebotConfig` from `nota-onebot` (config is cross-cutting); layering is
  `nota-cli → {nota-infra, nota-onebot} → nota-core`.
- WS actions are no longer fire-and-forget: `OneBotApi::call` correlates the
  implementation's response via `echo` (oneshot map + 15s timeout), resolved by
  the WS client loop.
- `read_group_chat` tool calls NapCat's `get_group_msg_history` (go-cqhttp
  compatible extended API over the same WS; params `group_id` + `count`).
  It is **not** gated by the reply allowlist — the bot may read any group
  without ever responding there. `format_history` renders `[HH:MM] name: text`.
- Live check: `get_group_list` and `get_group_msg_history` both return
  `retcode=0` against the user's NapCat.

### Outbound allowlist gate (2026-08-13)
- Sending is now explicitly gated, not just implied by routing:
  `OneBotBridge::send_reply` re-checks the route's `user_id` / `group_id`
  against `friend_ids` / `group_ids` before writing any action to the socket.
  A stale or forged route for a non-allowlisted chat is dropped with a WARN
  (and the route cleaned up). Applies to persona replies and to the
  auto-denied permission notices.
- Tests: `suppresses_reply_to_non_allowlisted_friend` /
  `suppresses_reply_to_non_allowlisted_group` cover the gate; the reply tests
  now configure allowlists explicitly (empty allowlist = deny all sends).

### Workspace dependency policy (2026-08-13)
- All dependency **versions** live in the root `[workspace.dependencies]`;
  each crate selects its own `features` (`{ workspace = true, features = [...] }`).
  Feature-less deps use the short form `dep.workspace = true` (equivalent to
  `dep = { workspace = true }`).
- `cargo update` keeps every direct dependency at the latest crates.io release
  (tokio 1.53, tokio-tungstenite 0.30, axum 0.8.9, reqwest 0.13.4, …).
  Indirect duplicates (axum→tokio-tungstenite 0.29, reqwest→tower-http 0.6.11)
  are intentional and left alone — only direct deps are pinned to latest.

### Web UI removed (2026-08-13)
- `webui/` submodule, `.gitmodules`, `.git/modules/webui`, the `nota webui`
  subcommand, `run_webui`/`locate_webui_dist`, and `find_static_dir` were all
  deleted. `tower-http` and `axum` dropped from `nota-cli` (both were only
  used by the static file server). Docs (AGENTS.md / README / guide.md)
  updated. The backend REST + WS API on `:2349` stays.

### QQ identity in the LLM context (2026-08-13)
- Inbound OneBot messages are prefixed with a chat identity header so the
  persona always sees the QQ numbers:
  `[私聊 昵称(QQ) → bot(QQ)]` / `[群 群号 昵称(QQ) → bot(QQ)]`
  (card > nickname > bare QQ).
- `format_history` renders every history line as
  `[HH:MM] 昵称(QQ) 消息ID:<id>: text`; message ids are parsed leniently
  (number or string) via `de_id_as_string`.
- `reply` quote segments render as `[回复消息ID:<id>]` so the persona can
  resolve the quoted message with the `get_msg` tool (standard OneBot API,
  verified live against NapCat: `get_msg` / `get_friend_msg_history` return
  numeric `message_id` + `message_seq`).
- `get_login_info` tool exposes the bot's own QQ number/nickname.
