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

Workspace `nota-cli → nota-infra → nota-core`, plus
`nota-onebot → nota-llm → nota-core` (onebot implements tools against
`nota-llm`) and `nota-infra → nota-llm → nota-core`. Hexagonal ports &
adapters; see `AGENTS.md` and `.agent/guide.md` for the full layout.

### LLM purification (2026-08-17, user-directed)

The llm module was purified:
- **Sessions are one dialogue, conversation-agnostic.** `LlmSession` has only
  an opaque uuid v4 `id` — no persona, no conversation id. Each session has
  **its own SQLite file** (`<session_id>.db`); the llm crate has **no default
  store path** — the caller supplies a directory. `PersonaRuntime` gives each
  conversation its own directory (`~/.nota/conversation/<conversation_id>/`,
  flat `<session_id>.db` files inside), so a whole conversation can be
  cleaned up by removing its directory. Items are stored verbatim as
  OpenAI-style message items (JSON: `message` / `function_call` /
  `function_call_output`); the system prompt is **not** persisted (callers
  inject it per request, matching LangChain-style frameworks).
- **Explicit create / get.** No `get_or_create` ambiguity:
  `LlmSessionManager::create()` makes a fresh uuid session,
  `session(id)` retrieves one (or `None`); `list()` enumerates the directory
  (order is deterministic via a per-directory monotonic `seq` in the session
  meta). "Which session is current" is a **caller concern**: `PersonaRuntime`
  persists `{"session_id": "…"}` in `current.json` inside the conversation
  directory and just reads it; `//clear` calls `create()` and rewrites the
  pointer. Old sessions stay archived as files.
- **Tools moved into `nota-llm`** (`tool.rs`): `Tool`, `ToolRegistry`,
  `ToolRegistryImpl`, `ToolContext`, `ToolParams`, `PropertyDef` (formerly in
  `nota-core` / `nota-infra`). External crates register tools on a shared
  registry instance; `AgentRunner::register_tool` delegates to it and the
  loop auto-attaches definitions to every request. `nota-core` no longer has
  a `tool` module.
- **Consequence**: `nota-onebot` now depends on `nota-llm` (for `Tool` /
  `ToolContext`), so the old "onebot → core only" rule changed. `ToolContext`
  still references core's `ConversationManager` / `PermissionRegistry` — the
  session layer stays pure; the tool execution layer touches routing by
  design.
- Old per-conversation `~/.nota/sessions/<id>/history.db` files are left
  untouched (user handles migration themselves; no automatic migration).

### Terminology (2026-08-17)

- **session** = one LLM-level dialogue: an ordered list of OpenAI-style
  message items (messages + tool calls/results; system prompt excluded),
  managed by `nota-llm` (`LlmSession` / `LlmSessionManager`). Each session is
  a uuid v4 id with its own SQLite file under the caller-specified directory.
  A dialogue can rotate through several sessions over time; old sessions stay
  archived as files.
- **conversation** = the user-visible chat (OneBot private/group, web) owned by an
  adapter; `nota-core` routes by conversation
  (`Conversation` / `ConversationManager`).

The old `Session`/`SessionManager` (adapter-assigned chat routing) was renamed
to `Conversation`/`ConversationManager`; the LLM history moved into
`nota-llm` as `LlmSession`. Slash commands now live in `PersonaRuntime`
(`nota-infra`), because `//clear` operates on the LLM sessions: runtime
intercepts before the LLM and rotates to a fresh session
(`session_manager.create()`); the old session stays archived. Legacy
`ClearBoundary` rows only exist in old per-conversation DBs, which are left
untouched; new code never writes them.

- **Domain purity**: `nota-core` carries no I/O deps — no axum, reqwest,
  serde_json, tracing, dialoguer, dirs, walkdir, tokio-tungstenite, rusqlite.
  `tokio` (sync only) and `serde` are fine. It also carries **no LLM content**:
  `LlmItem`/`LlmClient`/`AgentRunner`, session history, and the tool
  abstractions (`Tool`/`ToolRegistry`/`ToolContext`/`ToolParams`) live in
  `nota-llm`. Persistence and JSON (de)serialization live only in `nota-llm`.
- **DI only**: no `OnceLock<T>` / `RwLock<Option<T>>` for manager singletons;
  `nota-cli` builds adapters in `main` and injects them via `Arc`. (The
  `OnceLock<RwLock<FileCache>>` inside `FilePersonaStore` is a read cache, not
  a manager — allowed.)
- **Logging boundary**: core/infra use the `log::*` facade; only `nota-cli`
  uses `tracing`, bridged via `tracing-log::LogTracer`.
- **base_dir**: resolved in `nota-cli` (`dirs::home_dir().join(".nota")`) and
  injected into adapters; core never touches paths.
- **Domain types over generics**: `ToolParams`/`PropertyDef` (JSON Schema),
  `LlmItem`/`MessageRole`, … model the domain directly. No raw
  `serde_json::Value` or `String` as parameter types in core.

## History & storage

- **Persona workspaces** are plain files:
  `~/.nota/personas/<name>/{solo.md, memory.md}`, read through
  `FilePersonaStore` (mtime-cached reads, write-through). Workspace files are
  read generically by filename; `solo.md`/`memory.md` are conveniences.
- **Sessions are one SQLite file each** (`rusqlite` bundled, owned by
  `nota-llm`), stored flat as `<session_id>.db` in a caller-specified
  directory: `PersonaRuntime` uses `~/.nota/conversation/<conversation_id>/`
  (one dir per conversation — delete the dir to wipe the conversation). Each
  file has a `meta(key, value)` table (`created_at`, monotonic `seq`,
  `response_id`) and a `messages(id, item, timestamp)` table. The current
  session id lives in `current.json` (`{"session_id": "…"}`) in the same
  directory, written by the caller. `//clear` calls `create()` — nothing is
  deleted — and the model context is the current session's items. The raw
  history of **all** sessions of a conversation is served by
  `GET /api/personas/{name}/chatlog/{conversation_id}` as a list of
  `{session_id, created_at, messages: [(row_id, item)]}`.
  Adapter-assigned conversation ids: `onebot_private_<id>` /
  `onebot_group_<id>` / `web_<uuid>`.
- **No default persona**: no hardcoded default name, and `nota` never
  auto-creates or auto-jumps — running the server with a missing/corrupt
  `config.toml` errors out with "run `nota onboard`"; with zero personas it
  errors out with "run `nota persona create` / `nota onboard`". Both fail
  fast with guidance, no interactive detour. Setup paths: `nota onboard`
  (wizard + persona prompt) or `nota persona create`.

## Conversation routing & permissions

- No global broadcast bus: `ConversationManager` routes by conversation.
  Persona inboxes (`subscribe_persona`) receive inbound chat messages; adapter
  outboxes (`subscribe_adapter`) receive `AdapterEvent::Outbound` /
  `AdapterEvent::Permission` for their prefix (or every adapter when only a
  channel-agnostic `target` is set).
- Permission flow: a tool calls `ToolContext::request_permission(prompt)` →
  oneshot in `PermissionRegistry` → `ConversationManager::send_permission`
  routes it to the conversation's adapter → the adapter surfaces the prompt to
  the user → on Allow/Deny the adapter calls `PermissionRegistry::resolve(id,
  approved)` directly → the tool resumes.
- The HTTP/WS adapter forwards only events whose `conversation_id` matches its
  own web conversation, so multiple clients never leak each other's messages.

## LLM (Responses API)

- `nota-llm::responses::OpenAiLlm` speaks **only** `POST {api_url}/responses`
  (the Chat Completions path and `Config.api_mode` were removed).
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
- LLM sessions are managed by `nota-llm::LlmSessionManager` (`create` /
  `session(id)` / `latest` / `list`, no default store path — the caller
  supplies the directory); `LlmSession` exposes `append` (verbatim
  `LlmItem`s, OpenAI message-item shape), `context` (the session's full item
  list), `response_id`/`set_response_id`, and `raw_history`. The system
  prompt is injected per request by `PersonaRuntime` (`build_system_prompt`
  from `solo.md` / `memory.md`), never stored in the session.
- **Caching & cost (2026-08-17)**: DeepSeek's Responses endpoint is
  **stateless** — `previous_response_id`/`conversation`/`store` are not
  supported. Cost savings come from DeepSeek's automatic prefix cache
  (enabled by default): the request prefix must be **byte-identical** between
  turns. We keep the prefix stable by (a) appending history in stored order,
  (b) sorting the tool list by name in `ToolRegistryImpl::list` (HashMap order
  used to be random — a silent cache killer), and (c) keeping `instructions`
  built deterministically from persona files. The response `id` is parsed and
  persisted per session in the `sessions.response_id` column for future
  stateful providers; DeepSeek's `usage.prompt_cache_hit_tokens` /
  `prompt_cache_miss_tokens` are logged at DEBUG.

## Tool system

- Tool loop: max 16 iterations, LLM → tool_calls → execute → append results.
  `AgentRunner` and `ToolDef` + `ToolCall` + `LlmResponse` live in `nota-llm`.
- `Tool` / `ToolRegistry` / `ToolRegistryImpl` / `ToolContext` live in
  `nota-llm::tool`; infra and onebot register their tools on a shared
  `ToolRegistryImpl` instance (created in `nota-cli`, injected via `Arc`).
- Built-ins (`nota-infra`, registered by `register_builtin_tools`):
  `file_read`/`file_write` (sandboxed to the persona workspace; `//allow_read
  <path>` grants workspace-external reads without per-call approval, unlisted
  paths go through the permission round-trip), `schedule` (Scheduler port +
  TokioScheduler; ISO-8601 reminders delivered into the target conversation with
  `sender = "scheduler"`), `status` (version, platform, pid, uptime, current
  persona/conversation/request).
- Slash commands are intercepted in `PersonaRuntime::run` before anything
  reaches the LLM: `//clear` (rotates to a fresh LLM session, acks), 
  `//allow_read <path>`.

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

- Adapter in `nota-onebot` (depends on `nota-llm` for the tool trait and on
  `nota-core`), **forward WebSocket
  only** (`mode = "ws"`, default `ws://127.0.0.1:3001`; auth via
  `Authorization: Bearer <token>`). `OnebotConfig` in `config.toml [onebot]`:
  `enabled`, `mode`, `ws_url`, `access_token`, `persona` (empty = first persona
  found), `prefix`, `friend_ids`, `group_ids`. `enabled = true` requires ≥ 1
  persona (or a valid `persona` name) or the server refuses to start.
- Routing: inbound message → targeted persona inbox carrying the
  `Conversation` (`onebot_private_<id>` / `onebot_group_<id>`); replies are
  auto-routed back by conversation id, and `send_message(target, content)`
  sends explicitly to any allowlisted conversation. Final text is auto-routed
  as the reply; `skip_reply` or empty output suppress it — "不要回答" is
  honored. Sends re-check the allowlist and are chunked at 4000 chars.
- Allowlist: only `friend_ids` / `group_ids` reach the persona or get replies;
  empty list = nobody in that category. Outbound to a non-allowlisted target
  triggers a permission oneshot resolved by plain `同意` / `拒绝` (or `同意N` /
  `拒绝N`) in the originating conversation.
- Non-text segments render uniformly as `[{segment_type} msg id:<id>]` (e.g.
  `[image msg id:123]`; `reply` quotes become `[reply msg id:…]`); text keeps
  its content. Inbound headers: `[好友 昵称(id)]` (private) / `[群 群号 昵称(id)]`
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

### solo.md template + conversation message DEBUG logs (2026-08-14)
- `nota-infra/assets/solo.md` template rewritten (user-approved): identity,
  chat-style reply rules, honesty + "不要回答" silence contract, tool/memory
  usage (`file_read/write` in workspace, `schedule`, `send_message`,
  `status`, memory.md), and session context (independent sessions, identity
  headers like `[好友 昵称(id)]`). Only affects newly created personas —
  existing `~/.nota/personas/*/solo.md` are user config and untouched.
- File logs at DEBUG now carry the conversation message flow:
  `ConversationManager` logs `[in] conversation '…' -> persona '…' from '…':
  <content>` (deliver) and `[out] conversation '…' target '…': <content>`
  (route_outbound, covers replies, `send_message`, slash acks). Core stays on
  the `log::*` facade.
- Transport-crate DEBUG noise is filtered out of the file layer with
  **`Targets`** (built-in): `with_default(INFO)` + the five `nota_*` crates at
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
