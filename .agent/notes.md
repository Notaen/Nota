# Developer Notes

## Code Modification Rules
- Do NOT delete, modify, or remove existing comments without explicit approval.
- When in doubt about a comment change, ask first.
- Do NOT edit `solo.md` (neither the template in `nota-infra/assets/` nor any
  user persona file under `~/.nota/personas/`) without explicit user approval.
  The user treats persona prompts as their own configuration; propose the
  wording first and apply it only after they agree.

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
- `status` — detailed runtime info: version, platform (os/arch/family),
  pid, uptime, and the current persona/session/request
- Registered via `register_builtin_tools(registry, personas_dir)`

### OpenAiLlm
- OpenAI Responses API (`POST {api_url}/responses`) since 2026-08-13; the
  legacy Chat Completions path was removed later the same day (see below).
- Request uses typed structs (`ResponsesRequest`, `ResponsesInputItem`,
  `ResponsesTool`), NOT raw `serde_json::Value` or `json!()`.
- History roles map to Responses input items: `tool_call` → `function_call`,
  `tool_result` → `function_call_output`.
- Core conversation items are `LlmItem`s (`Message` / `FunctionCall` /
  `FunctionCallOutput`) with a typed `MessageRole`; see the core refactor
  note below.

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
- Non-text segments render uniformly as `[{segment_type} msg id:<id>]`
  (e.g. `[image msg id:123]`, `[reply msg id:456]`), so the persona knows
  what kind of media arrived and can fetch its content with a tool
  (`get_msg`, `get_voice_text`, …). The `get_msg` tool is the standard
  OneBot API, verified live against NapCat: `get_msg` /
  `get_friend_msg_history` return numeric `message_id` + `message_seq`.
- `get_login_info` tool exposes the bot's own QQ number/nickname.

### Voice messages: persona-driven transcription (2026-08-13)
- Voice (`record`) segments are rendered with the containing message id —
  `[record msg id:<id>]` — in inbound events, `get_msg`, and
  `format_history`, so the persona can see there is a voice message and
  knows which one it is.
- Transcription is **not** automatic in the bridge: the persona calls the
  `get_voice_text` tool with `message_id` when it wants to read the voice
  content. The tool invokes NapCat's extended `fetch_ptt_text` action
  (param `message_id`, number or string; requires NapCat >= 4.18.2) and
  returns the transcript text.
- `MessageContent::to_text_with_id` / `segment_to_text_with_id`, the
  `fetch_ptt_text` action serialization + response parsing, and the
  `get_voice_text` tool's success/failure paths are covered by unit tests.
- Live testing against NapCat: `fetch_ptt_text` is **intermittent** — the
  same message can return `retcode=1200` (voice still processing / service
  hiccup) and succeed on a later retry, so `get_voice_text` retries up to 3
  times (2s apart) before failing, and its error message tells the persona
  the voice may still be processing. `message_id` is sent as a string, which
  proved more reliable than a number against NapCat.

### Uniform non-text segment rendering (2026-08-13)
- `segment_to_text` / `segment_to_text_with_id` no longer special-case each
  segment type with a Chinese placeholder (`[图片]` / `[表情]` / `[语音]` /
  `[回复消息ID:…]` …). Text segments keep their raw content; **every other
  segment type renders uniformly as `[{segment_type} msg id:<id>]`** where
  `id` is the containing message id. The persona sees e.g. `[image msg
  id:1758219899]` and decides whether to fetch content with a tool — nothing
  is auto-processed in the bridge.
- The id-less `to_text` (used for local parsing like approval commands) keeps
  text content and renders non-text segments as `[{type}]` without an id.
- Consequence: a `reply` quote no longer surfaces the *quoted* message id
  directly — the persona gets the containing message id and can resolve the
  quote with `get_msg` if needed. Voice transcription hooks stay unchanged
  (`get_voice_text` takes the id from `[record msg id:…]`).

### Send/receive separation (2026-08-13)
- The bridge no longer auto-routes persona replies. Inbound OneBot events
  only reach the persona; the encoded reply target travels in
  `BusEvent.context` (`private:<user_id>` / `group:<group_id>`), and
  `PersonaRuntime` copies it into the new `ToolContext.reply_target`.
- The persona sends messages explicitly:
  - `reply` — answers the current message using the injected target;
    not calling it means no reply (so "不要回答" is actually honored).
  - `send_private_msg` / `send_group_msg` — proactive messages; multiple
    calls per turn allow consecutive messages and proactive outreach.
- All sends go through `Outbound` (allowlist re-check + 4000-char chunking);
  `send_reply` remains only for the auto-denied permission notices.
- OneBot tools are registered via `OneBotBridge::register_tools`; the CLI
  no longer names individual OneBot tool types.

### Sessions replace the broadcast-only bus (2026-08-13)
- The event bus is kept as the message backbone, but every chat endpoint is
  now a **session** (`SessionStore` + `Session { persona, session_id }`).
  Sessions are independent of personas: storage lives under
  `~/.nota/sessions/<session_id>/` (the historical SQLite session stack was
  intentionally not revived).

### Global EventBus removed; session-scoped routing (2026-08-13)
- The global `EventBus` is gone. `SessionManager` routes messages by session:
  - **Inbound**: adapters deliver straight to the target persona's inbox
    (`subscribe_persona`), so the persona always receives its session's
    messages — no broadcast, no filtering.
  - **Outbound**: persona replies route to the session's adapter
    (`route_outbound` with the session id); `send_message` broadcasts an
    adapter-agnostic target and each adapter claims what it understands.
  - **Permissions** route to the session's adapter (`AdapterEvent::Permission`)
    for the user's 同意/拒绝 approval.
  - **Slash commands** are intercepted in `SessionManager::deliver` before
    reaching the persona: `//clear` drops the session history and acks.
- Adapter-assigned session ids: `onebot_private_<qq>` /
  `onebot_group_<qq>` for OneBot, `web_<uuid>` per WebSocket connection.
  `BusEvent` and `ToolContext` carry `session_id`; each adapter only routes
  events for its own session prefix, so one persona can serve many channels
  without history bleeding between them.
- Persona replies are auto-routed back to the originating session (final
  assistant text → bus event with `session_id`); `skip_reply` tool sets a
  per-turn `suppress_reply` flag to honor "不要回答"; empty final text is
  also suppressed. `reply`/`send_*` tools emit `OutboundMessage` events that
  the bridge forwards after the allowlist check.
- `GET /api/personas/{name}/chatlog/{session_id}` replaced the global
  chatlog endpoint.

### reply tool removed (2026-08-13)
- The `reply` tool was deleted: the persona's final assistant text IS the
  reply (auto-routed to the session). No reply = empty final text, or call
  `skip_reply`; both suppress the outbound push entirely. Proactive messages
  still go through `send_private_msg` / `send_group_msg`.
- This keeps exactly one reply path, eliminating double replies caused by
  the LLM both calling `reply` and emitting final text.
- `skip_reply`'s tool description now spells out the silence contract
  explicitly ("unless you call skip_reply, your final text WILL be sent"),
  because the persona kept replying eagerly when nothing needed saying.

### Two-layer sessions + session-level send_message (2026-08-13)
- The shallow/deep split was dropped: a session has one history file,
  `~/.nota/sessions/<session_id>/chatlog.jsonl` (JSONL, append-only), owned
  by the persona module (`PersonaStore::append_history/read_history/clear_history`).
  Tool calls are stored with their raw payload (rendered by the llm module,
  no `serde_json` in core) with `sender = "tool"`; user/persona messages use
  their real senders.
- The adapter-specific `send_private_msg` / `send_group_msg` tools were
  replaced by one channel-agnostic `send_message(target, content)` in
  `nota-infra` (`tool/chat.rs`, with `skip_reply`). Target format is
  `private:<QQ>` / `group:<QQ>`; the OneBot bridge maps it to its session
  and enforces the allowlist.
- The bridge holds `SessionStore`: after a message is actually sent (auto
  reply or explicit send), it records the delivered content in the target
  session's shallow layer — persona intent lives in deep, delivered messages
  live in shallow.
- Future `dream` runs will learn from the shallow layer (what the persona
  really said) to self-optimize the persona; not implemented yet.

### Outbound approval for non-allowlisted targets (2026-08-13)
- `send_message` to a target inside the allowlist is delivered immediately.
  To a non-allowlisted target, the OneBot bridge registers a permission
  oneshot, queues it per source session, and notifies the originating session
  with a human-friendly prompt (`回复「同意」批准，或「拒绝」拒绝`; the
  machine-readable `权限ID：<uuid>` line is kept for web/programmatic
  approval). It only sends after approval (bypassing the allowlist via
  `Outbound::*_approved`).
- Approval commands are plain `同意` / `拒绝` (optionally `同意N` / `拒绝N`
  when several requests are pending in the same session); the bridge matches
  them against its per-session queue and resolves the permission oneshot.
  Web clients use the existing `{type:"permission", …}` command with the id
  from the notice.
- The approval round-trip is an **OneBot adapter concern**: the notice is a
  plain QQ reply to the originating OneBot session, and the 同意/拒绝 replies
  are matched by the bridge's per-session queue. Non-OneBot sources are
  dropped (other channels implement their own approval); there is no generic
  "system notification" event on the bus.

### Persona naming & chat headers (2026-08-13)
- The `solo.md` template uses a `{name}` placeholder; `create_persona`
  substitutes the name given at creation time. Persona folders are named
  after the persona (`~/.nota/personas/<name>/`).
- Inbound message headers no longer include the bot's own identity:
  `[好友 昵称(QQ)]` for private messages, `[群 群号 昵称(QQ)]` for groups.
  The persona learns its own QQ/nickname via the `get_login_info` tool.

### Scheduler, read allowlist, command interception (2026-08-13)
- `Scheduler` port (core) + `TokioScheduler` (infra): `schedule` tool
  registers an ISO-8601 reminder; when due, the message is delivered into the
  target session with `sender = "scheduler"` so the persona can react.
- `PathPolicy` (core) + `//allow_read <path>`: user-guided allowlist lets
  `file_read` read outside the workspace without per-call approval; unlisted
  paths still go through the 同意/拒绝 permission round-trip.
- Slash commands are intercepted in `SessionManager::deliver` before the
  persona. Inbound messages carry the identity header separately
  (`InboundMessage.prefix`, e.g. `[好友 昵称(QQ)] `) from the user's real
  content, so commands reach the session manager verbatim.

### SQLite history with kind field + clear boundaries (2026-08-13)
- Conversation history moved from `chatlog.jsonl` to SQLite
  (`~/.nota/sessions/<session_id>/history.db`, one database per session,
  `rusqlite` bundled). Rows are stored verbatim with a `kind` column instead
  of the ambiguous `sender`:
  - `0` clear boundary (`//clear` appends one; nothing is ever deleted)
  - `1` user message, `2` assistant message, `3` tool call/result
- The LLM context query returns only rows after the last clear boundary
  (`id > MAX(id WHERE kind = 0)`); raw history (boundaries included) is
  exposed via `GET /api/personas/{name}/chatlog/{session_id}`.

### Responses API as the default LLM format (2026-08-13)
- `OpenAiLlm` posts to `{api_url}/responses` (no mode switch; the legacy
  Chat Completions path and `Config.api_mode` were removed, see below):
  - the system prompt becomes the top-level `instructions`;
  - history `LlmItem`s map one-to-one onto `input` items (`message` with
    `input_text` / `output_text` parts, `function_call`,
    `function_call_output`);
  - tools use the flat Responses shape
    `{"type":"function", name, description, parameters}`.
- DeepSeek's Responses endpoint strictly requires each
  `function_call_output` to directly follow its matching `function_call`
  (OpenAI tolerates interleaving; DeepSeek rejects it with
  "No tool output found for tool call …"). `to_responses_input` therefore
  pairs each tool result with its call on sight (`[fc, output, fc, output]`)
  instead of batching all calls then all outputs.
- Output parsing: `output[]` message items are concatenated into the reply
  text; `function_call` items map to core `ToolCall` using `call_id`
  (falling back to `id`); `reasoning`/unknown items are ignored. The
  response-level `output_text` convenience field is a fallback.
- Verified live against DeepSeek `deepseek-v4-flash`: plain text and
  function-call round trips both return `status: completed` at
  `https://api.deepseek.com/v1/responses`, and an end-to-end WS chat
  through persona Nota produced a reply.

### `nota onboard` wizard secrets: masked feedback (2026-08-13)
- `nota-cli` no longer uses `dialoguer::Password` for secrets (LLM API key,
  OneBot access token). dialoguer 0.12's `Password` reads the whole line with
  terminal echo disabled (`Term::read_secure_line`), so **nothing appears on
  screen while typing/pasting** — only after Enter does it print `[hidden]`,
  which reads as "the paste did nothing".
- New `prompt_masked` in `config_wizard.rs` reads keys one at a time via
  `console::Term::read_key` (echo off per key) and prints one `*` per
  character, so pasting/typing gives immediate feedback while the value stays
  hidden. After Enter the stars are erased and `[hidden]` / `[empty]` is
  printed in their place. Empty-but-required re-prompts with an error line.
- Ctrl+C still interrupts: Unix `read_key` raises SIGINT itself; on Windows
  Ctrl+C arrives as `Key::Char('\x03')` and aborts with an error.
- `console = "0.16"` added to the workspace deps (already in the lock via
  dialoguer, no new crates).

### Built-in web_search tool in Responses API mode (2026-08-13)
- `Config.web_search` (default `true`) attaches DeepSeek's server-side
  built-in `web_search` tool to every Responses API request:
  `tools` gains `{"type":"web_search"}` after the function tools. Set it to
  `false` in `config.toml` to disable; the onboard wizard asks for it.
- DeepSeek executes the search server-side and injects the results into the
  model context; the response surfaces `web_search_call` output items
  (ignored by the parser, like `reasoning`) plus the final answer message.
  `search_context_size` / `user_location` are ignored by DeepSeek, so the
  tool is sent with no extra fields.
- Verified live: a query about 2026-08-13 tech news returned
  `status: completed` with multiple `web_search_call` items and a final
  answer containing real-time news; the same flow works end-to-end through
  persona Nota over the local WS channel.

### Chat Completions API removed (2026-08-13)
- The legacy `chat/completions` fallback was deleted: `OpenAiLlm` only
  speaks the Responses API now. `Config.api_mode`, the wizard's API format
  prompt, and all Chat Completions wire types are gone; `OpenAiLlm::new`
  takes `(api_url, api_key, model, web_search)`.
- Existing `config.toml` files may still contain `api_mode`; serde ignores
  the unknown field, so no user-side change is needed.

### Core LLM types redesigned around Responses items (2026-08-13)
- `ChatMessage` was removed from `nota-core::llm`. Conversation history is
  now a `Vec<LlmItem>`: `Message { role: MessageRole, content }`,
  `FunctionCall(ToolCall)`, and `FunctionCallOutput { call_id, output }`.
  `MessageRole` is a typed enum (`User` / `Assistant`) instead of a free-form
  string.
- `AgentRunner` emits one `FunctionCall` item immediately followed by its
  `FunctionCallOutput` per tool execution (interleaved pairs), so the
  DeepSeek adjacency requirement holds by construction; the infra adapter no
  longer re-pairs calls and outputs.
- History storage keeps the raw JSON payload per item via `LlmItem::raw_json`
  (still no `serde_json` in core): tool rows are replayed as assistant text
  carrying that raw payload, so the model sees the exact call/output content.
- `LlmClient::chat(system, items, tools)` returns `LlmResponse { content,
  tool_calls }`.
