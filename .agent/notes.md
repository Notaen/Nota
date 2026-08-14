# Developer Notes

## Code Modification Rules
- Do NOT delete, modify, or remove existing comments without explicit approval.
- When in doubt about a comment change, ask first.
- Do NOT edit `solo.md` (neither the template in `nota-infra/assets/` nor any
  user persona file under `~/.nota/personas/`) without explicit user approval.
  Persona prompts are user configuration; propose the wording first and apply
  it only after they agree.

## Directory Naming
- Use `personas` (plural) for the persona directory under `base_dir()`:
  `base_dir().join("personas")`, not `"persona"`.

## Current Architecture (2026-08)

Workspace `nota-cli → nota-infra → nota-core`, plus `nota-onebot → nota-core`
(one-way; core never sees axum/reqwest/tungstenite). Hexagonal ports &
adapters; see `AGENTS.md` and `.agent/guide.md` for the full layout.

- **Domain purity**: `nota-core` carries no I/O deps — no axum, reqwest,
  serde_json, tracing, dialoguer, dirs, walkdir, tokio-tungstenite. `tokio`
  (sync only) and `serde` are fine. Persistence row structs and JSON
  (de)serialization live only in `nota-infra`.
- **DI only**: no `OnceLock<T>` / `RwLock<Option<T>>` for manager singletons;
  `nota-cli` builds adapters in `main` and injects them via `Arc`. (The
  `OnceLock<RwLock<FileCache>>` inside `FilePersonaStore` is a read cache, not
  a manager — allowed.)
- **Logging boundary**: core/infra use the `log::*` facade; only `nota-cli`
  uses `tracing`, bridged via `tracing-log::LogTracer`.
- **base_dir**: resolved in `nota-cli` (`dirs::home_dir().join(".nota")`) and
  injected into adapters; core never touches paths.
- **Domain types over generics**: `ToolParams`/`PropertyDef` (JSON Schema),
  `LlmItem`/`MessageRole`, `HistoryKind`, … model the domain directly. No raw
  `serde_json::Value` or `String` as parameter types in core.

## History & storage

- **Persona workspaces** are plain files:
  `~/.nota/personas/<name>/{solo.md, memory.md}`, read through
  `FilePersonaStore` (mtime-cached reads, write-through). Workspace files are
  read generically by filename; `solo.md`/`memory.md` are conveniences.
- **Chat is session-based**: every chat endpoint (QQ friend/group, web socket)
  maps to a session; storage is `~/.nota/sessions/<session_id>/history.db`
  (one SQLite DB per session, `rusqlite` bundled). Rows carry a `kind` column
  (`HistoryKind`): `0` clear boundary, `1` user, `2` assistant, `3` tool
  call/result. `//clear` appends a boundary — rows are never deleted — and the
  LLM context is built from rows after the last boundary. Raw history is served
  by `GET /api/personas/{name}/chatlog/{session_id}`. Adapter-assigned session
  ids: `onebot_private_<qq>` / `onebot_group_<qq>` / `web_<uuid>`.
- **No default persona**: no hardcoded default name, and `nota` never
  auto-creates or auto-jumps — running the server with a missing/corrupt
  `config.toml` errors out with "run `nota onboard`"; with zero personas it
  errors out with "run `nota persona create` / `nota onboard`". Both fail
  fast with guidance, no interactive detour. Setup paths: `nota onboard`
  (wizard + persona prompt) or `nota persona create`.

## Event bus & permissions

- `EventBus` is a multi-producer / multi-consumer FIFO: every subscriber
  (`bus.subscribe()`) gets its own unbounded mpsc receiver; `bus.send` clones
  the event to all.
- `BusEvent { kind, sender, content, timestamp, context, request_id,
  parent_request_id, target }`. `target: Option<String>` routes to one persona
  (`None` = broadcast). Personas skip events where `sender == name`.
- Permission flow: a tool calls `ToolContext::request_permission(prompt)` →
  oneshot in `PermissionRegistry` + a `PermissionRequest` bus event
  (`parent_request_id` = originating request) → the adapter forwards it to the
  user → on Allow/Deny the adapter calls `PermissionRegistry::resolve(id,
  approved)` directly (no bus event) → the tool resumes.
- The HTTP/WS adapter subscribes once and forwards only events whose
  `request_id` (or `parent_request_id`) is in its per-connection
  `active_request_ids` set, so multiple clients never leak each other's
  messages.

## LLM (Responses API)

- `OpenAiLlm` speaks **only** `POST {api_url}/responses` (the Chat Completions
  path and `Config.api_mode` were removed).
- System prompt → top-level `instructions`; history `LlmItem`s map 1:1 onto
  `input` items; tools use the flat Responses shape
  `{"type":"function", name, description, parameters}`.
- DeepSeek requires each `function_call_output` to directly follow its matching
  `function_call`, so `AgentRunner` emits interleaved pairs
  (`[fc, output, fc, output]`) — adjacency holds by construction.
- Output parsing: `output[]` message items → reply text; `function_call` →
  `ToolCall` via `call_id` (fallback `id`); `reasoning` / `web_search_call` /
  unknown items are ignored.
- Built-in `web_search` tool (DeepSeek executes it server-side) is attached
  when `Config.web_search` (default `true`); the wizard asks for it.

## Tool system

- Tool loop: max 16 iterations, LLM → tool_calls → execute → append results.
  `ToolDef` + `ToolCall` + `LlmResponse` live in `nota-core::llm`.
- Built-ins (`nota-infra`, registered by `register_builtin_tools`):
  `file_read`/`file_write` (sandboxed to the persona workspace; `//allow_read
  <path>` grants workspace-external reads without per-call approval, unlisted
  paths go through the permission round-trip), `schedule` (Scheduler port +
  TokioScheduler; ISO-8601 reminders delivered into the target session with
  `sender = "scheduler"`), `status` (version, platform, pid, uptime, current
  persona/session/request).
- Slash commands are intercepted in `SessionManager::deliver` before the
  persona: `//clear` (appends a boundary, acks), `//allow_read <path>`.

## Wizard & config

- `Config` (TOML): `api_url`, `api_key`, `model`, `web_search`, `[onebot]`.
  Provider metadata lives in `crates/nota-infra/assets/providers.toml`
  (`include_str!`), used only by the wizard to pre-fill defaults. Saved config
  is flat — no provider type at runtime.
- `nota onboard` runs the wizard standalone; plain `nota` starts the server
  (auto-wizard if config missing). `run_wizard` accepts an existing `Config`
  as defaults and prints a summary before saving.
- Secrets (API key, OneBot access token) use `prompt_masked` (2026-08-13):
  `dialoguer::Password` disables echo for the whole line
  (`Term::read_secure_line`) — nothing appears while typing/pasting, and only
  after Enter does it print `[hidden]`, which looks like the input never
  arrived. `prompt_masked` (console `Term::read_key`, one `*` per char) gives
  immediate masked feedback, erases the stars on Enter and prints
  `[hidden]`/`[empty]`; empty-but-required re-prompts. Ctrl+C still interrupts
  (Unix: SIGINT from `read_key`; Windows: `Key::Char('\x03')` → error).
  `console` is a workspace dep (already in the lock via dialoguer).

## OneBot 11 (2026-08)

- Adapter in `nota-onebot` (depends only on `nota-core`), **forward WebSocket
  only** (`mode = "ws"`, default `ws://127.0.0.1:3001`; auth via
  `Authorization: Bearer <token>`). `OnebotConfig` in `config.toml [onebot]`:
  `enabled`, `mode`, `ws_url`, `access_token`, `persona` (empty = first persona
  found), `prefix`, `friend_ids`, `group_ids`. `enabled = true` requires ≥ 1
  persona (or a valid `persona` name) or the server refuses to start.
- Routing: inbound message → **targeted** bus event for the persona; the reply
  target travels in `BusEvent.context` (`private:<QQ>` / `group:<QQ>`). The
  persona sends explicitly via the `send_message(target, content)` tool (final
  text is auto-routed as the reply; `skip_reply` or empty output suppress it —
  "不要回答" is honored). Sends re-check the allowlist and are chunked at
  4000 chars.
- Allowlist: only `friend_ids` / `group_ids` reach the persona or get replies;
  empty list = nobody in that category. Outbound to a non-allowlisted target
  triggers a permission oneshot resolved by plain `同意` / `拒绝` (or `同意N` /
  `拒绝N`) in the originating session.
- Non-text segments render uniformly as `[{segment_type} msg id:<id>]` (e.g.
  `[image msg id:123]`; `reply` quotes become `[reply msg id:…]`); text keeps
  its content. Inbound headers: `[好友 昵称(QQ)]` (private) / `[群 群号 昵称(QQ)]`
  (group). Voice → `[record msg id:<id>]`, transcribed on demand via
  `get_voice_text` (NapCat `fetch_ptt_text`, retries 3× 2s apart, id sent as
  string).
- Tools: `read_group_chat` (NapCat `get_group_msg_history`; reading any group
  is not allowlist-gated), `get_msg`, `get_login_info`; registered via
  `OneBotBridge::register_tools`. OneBot has no interactive approval channel,
  so tool permission requests are auto-denied with a notice to the chat.
- NapCat quirks (verified live 2026-08-13): it resets the WS connection when
  the **client** sends an unsolicited Ping — only pong the server's pings.
  It must listen on `0.0.0.0` for LAN clients; it does not queue events for
  disconnected clients (messages sent during a disconnect window are lost);
  `fetch_ptt_text` is intermittent (retcode=1200 then success on retry).

### solo.md template + session message DEBUG logs (2026-08-14)
- `nota-infra/assets/solo.md` template rewritten (user-approved): identity,
  chat-style reply rules, honesty + "不要回答" silence contract, tool/memory
  usage (`file_read/write` in workspace, `schedule`, `send_message`,
  `status`, memory.md), and session context (independent sessions, identity
  headers like `[好友 昵称(QQ)]`). Only affects newly created personas —
  existing `~/.nota/personas/*/solo.md` are user config and untouched.
- File logs at DEBUG now carry the session message flow: `SessionManager`
  logs `[in] session '…' -> persona '…' from '…': <content>` (deliver) and
  `[out] session '…' target '…': <content>` (route_outbound, covers replies,
  `send_message`, slash acks). Core stays on the `log::*` facade.
- Transport-crate DEBUG noise is filtered out of the file layer with
  **`Targets`** (built-in): `with_default(INFO)` + the four `nota_*` crates at
  `TRACE`. New third-party deps are automatically INFO — no blacklist to
  maintain. The worst offender was `h2` logging every HTTP/2 frame
  (`received frame=Data { stream_id: StreamId(1) }`) on every LLM API call.
- Pitfall (2026-08-14, verified with a unit test): in tracing-subscriber
  0.3.23, **hand-rolled `Filter` impls (even with `enabled` +
  `event_enabled` + `callsite_enabled` + `max_level_hint`, and even
  `filter_fn`) do NOT gate events in this setup — events leak through**
  (probes showed every method returning `false` yet the event was recorded),
  while the built-in `Targets` works. The console layer's `LevelFilter::INFO`
  filter is fine. The regression test
  `our_crates_debug_filter_blocks_third_party_noise` pins the two-layer
  behavior (console INFO + file Targets).

## General lessons
- When the user says "add", don't replace existing types — keep both.
- Don't auto-add defaults (persona, …) unless explicitly requested; don't
  hardcode names that belong in filesystem config.
- Don't over-engineer minimal asks.
- Idiomatic English in logs and user-facing strings.
- `chrono` is the only time library (no `time`).
