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

`nota-cli → nota-infra → nota-core`, `nota-cli → nota-llm → nota-core`, and
`nota-cli → nota-onebot → nota-core`. Only the composition root references
`nota-llm`; infra/onebot hold the core abstractions. Hexagonal ports &
adapters; see `AGENTS.md` and `.agent/guide.md` for the full layout.

### LLM purification (2026-08-17, user-directed) — superseded

Superseded by "Session manager + tool contract in core" below (2026-08-18);
kept for history.

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

### Terminology (2026-08-17) — superseded

Superseded by the current terminology in `AGENTS.md` / `CONTRIBUTING.md`
(session = one conversation-namespaced LLM dialogue, managed via the core
`SessionManager` abstraction).

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

### Explicit sending + tool registry fail-fast (2026-08-18, user-directed)

Learned from the dsh-onebot plugin design:
- **No auto-send**: `PersonaRuntime` no longer routes the final assistant
  text back to the conversation at the end of a turn. Sending is explicit:
  `reply` delivers into the current conversation; `onebot_send_msg` sends to
  a specific QQ chat (`target = private:<id>` / `group:<id>`). Each call
  sends immediately and can be repeated within one turn.
- **`skip_reply` removed**: with no auto-send there is nothing to suppress;
  the `suppress_reply` flag was dropped from `ToolContext` and the tool no
  longer exists. Staying silent = not calling a send tool (「不要回答」).
- **Tool registration fails fast**: `ToolRegistry::register` returns
  `Result<()>` and errors on a duplicate name; every registration site
  (`register_builtin_tools`, `register_chat_tools`,
  `OneBotBridge::register_tools`) propagates it, so startup aborts instead of
  silently shadowing the earlier tool (the registry is a HashMap insert —
  shadowing used to be silent).
- **OneBot toolset renamed to `onebot_*`** (dsh-style):
  `send_message` → `reply` (+ `onebot_send_msg`), `read_group_chat` →
  `onebot_get_msg_history` (now `target`-based, private + group), `get_msg`
  → `onebot_get_content`, `get_login_info` → `onebot_status` (adds
  connection state), `get_voice_text` → `onebot_voice_text`. Built-in
  runtime names stay reserved (`file_read` / `file_write` / `schedule` /
  `status` / `reply`).
- Slash-command acks (`//clear`, `//allow_read`) are still routed directly
  by `PersonaRuntime` — runtime feedback, not model speech.

### Session manager + tool contract in core (2026-08-18, user-directed)

Architectural reversal of the 2026-08-17 "tools into llm" decision: the llm
crate is now a pure **session implementation**, and other modules never
reference it.
- **Core owns the abstractions**: `nota-core` gains `session.rs`
  (`Session` / `SessionManager` traits + `SessionItem`/`MessageRole`/
  `ToolCall`) and `tool.rs` (`Tool` / `ToolContext` / `ToolParams` /
  `PropertyDef` + a concrete in-memory `ToolRegistry` — the trait/impl split
  is gone). `ToolContext` lives with the routing/approval types it needs, so
  the old llm→core concrete coupling disappears. Core adds `serde` (allowed).
- **No `LlmClient`, no `AgentRunner`**: the LLM call and the turn loop moved
  inside `SqliteSession` (`send(content, request_id)` runs the whole turn:
  append user item → LLM call with system prompt + live tool list → execute
  tool calls with a per-session `ToolContext` → persist items/response id).
  The Responses client is internal (`ChatLlm` is a crate-private test seam).
- **`SqliteSessionManager` is the only public llm surface**: one per persona,
  constructed by `nota-cli` with the storage root, system prompt (built once
  at startup from `solo.md`/`memory.md`), shared `ToolRegistry`, and the
  `ConversationManager`/`PermissionRegistry` ports. API:
  `create(conversation_id)` / `current(conversation_id)` / `load(id)` /
  `archive(id)` / `list(conversation_id)`; `current.json` moved inside the
  manager.
- **Storage layout change (breaking)**: sessions now live at
  `~/.nota/conversation/<persona>/<conversation_id>/<uuid>.db` (was
  `conversation/<conversation_id>/<uuid>.db`); ids are
  `<conversation_id>/<uuid>`. Old history dirs are orphaned — dev phase,
  accepted.
- **Roles are numeric (breaking)**: `MessageRole` is a `u8` — `0` reserved,
  `1` user, `2` assistant, `3` context — serialized as plain numbers in
  sqlite/JSON, not strings. The `System` role was **removed** — the system
  prompt is not a stored role: it is passed to `SessionManager` at
  construction (a fixed constant, deliberately **not** derived from persona
  files) and sent as `instructions` per request. The remaining roles were
  renumbered (`1` user, `2` assistant, `3` context), so rows written with the
  old numbering shift or fail on deserialization — dev phase, accepted.
  Persona content (`solo.md` / `memory.md`) is injected at session creation
  as `Context` items and emitted as `system` input messages at call time. The
  llm crate maps roles to wire strings only when building the API request.
- **Dependency graph**: `nota-cli → nota-infra → nota-core`,
  `nota-cli → nota-llm → nota-core`, `nota-cli → nota-onebot → nota-core`.
  `nota-infra`/`nota-onebot` dropped the llm dependency; `nota-cli` gained it
  (composition root). `PersonaRuntime` is the **conversation layer**: it
  lazily creates one session manager per conversation (via an injected
  factory) with that conversation's tool set — including a conversation-bound
  `reply` tool whose `conversation_id` is baked into the struct, so the
  session and `ToolContext` never reference conversations. The chatlog API
  reads history through the same runtime.

## History & storage

- **Persona workspaces** are plain files:
  `~/.nota/personas/<name>/{solo.md, memory.md}`, read through
  `FilePersonaStore` (mtime-cached reads, write-through). Workspace files are
  read generically by filename; `solo.md`/`memory.md` are conveniences.
- **Sessions are one SQLite file each** (`rusqlite` bundled, owned by
  `nota-llm`), stored as `<uuid>.db` under
  the manager's root — sessions are **conversation-agnostic** (plain uuid
  ids, no conversation naming). Each file has a `meta(key, value)` table
  (`created_at`, monotonic `seq`, `version`, `response_id`, `archived`) and
  an `item` table (`id, type, role, content, kind, call_id, name, arguments,
  output, timestamp`) where `type` is a plain number (`0` reserved,
  `1` message, `2` reasoning, `3` tool_call, `4` tool_call_output) and
  `tool_call.kind` is also numeric (`1` function_call, `2` web_search_call);
  per-kind payload columns are used, the rest stay `NULL`. `meta.version`
  records the writer's program version (`env!("CARGO_PKG_VERSION")`) so a
  future release can detect old files and convert them.
  `PersonaRuntime` gives each conversation its own directory
  (`~/.nota/conversation/<persona>/<conversation_id>/`, delete the
  conversation dir to wipe it) and writes `current.json`
  (`{"session_id": "…"}`) inside it. `//clear` archives the old session and
  creates a fresh one — nothing is deleted. The raw history of **all**
  sessions of a conversation is served by
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
- System prompt (from `SessionManager` construction) → top-level
  `instructions`; history `SessionItem`s map 1:1 onto `input` items
  (`Context` items as role `system`; `reasoning` items are passed back as
  `reasoning` input items — DeepSeek thinking mode requires every prior
  `reasoning_text` to be echoed, including empty ones; `web_search_call`
  items are passed back as `web_search_call` input items so DeepSeek
  restores the server-side search results); tools use the flat Responses
  shape
  `{"type":"function", name, description, parameters}`.
- DeepSeek requires each `function_call_output` to directly follow its matching
  `function_call`, so the session turn loop emits interleaved pairs
  (`[fc, output, fc, output]`) — adjacency holds by construction.
- Output parsing: `output[]` message items → reply text; `function_call` →
  `ToolCall` (kind `function_call`) via `call_id` (fallback `id`);
  `reasoning` → `LlmResponse.reasoning` (persisted as a `Reasoning` item and
  echoed back as a `reasoning` input item); `web_search_call` → `ToolCall`
  (kind `web_search_call`, recorded but never executed locally); unknown
  items are ignored.
- Built-in `web_search` tool (DeepSeek executes it server-side) is attached
  when `Config.web_search` (default `true`); the wizard asks for it.
- **web_search turn loop (2026-08-19)**: a response may contain a
  `web_search_call` **and** the final text at once. The loop only continues
  when it executed a local `function_call`; a response with only built-in
  calls (web_search, …) uses its text and ends the turn instead of issuing a
  wasteful second request and discarding the answer.
- **web_search_call persists the query (2026-08-19, user-directed)**: the
  `web_search_call` output item is stored as a `tool_call` row (kind
  `web_search_call`) with `name = "web_search"` and `arguments =
  {"query": …}` taken from `action.query` — the only content the provider
  returns (results are injected into the model context; DeepSeek ignores
  `include`, so there is no `tool_call_output` to produce or save). When
  rebuilding `input`, `web_search_call` items are passed back as-is (type +
  id + action.query) — DeepSeek restores the search results from the call
  id, so follow-up turns keep the search context.
- **Tool args contract (2026-08-19)**: `Tool::run` receives parsed
  `HashMap<String, Value>` using core's own JSON-like `Value` type (no
  `serde_json` in core — the llm layer deserializes the model's raw
  arguments into it), replacing the per-tool `serde_json::from_str(...)`
  boilerplate. The session validates against the tool's `parameters` before
  calling — required present, no unknown properties, type and enum match.
  Rejections are split in two: a diagnostic (provider / model / raw output /
  reasons) goes to the operator log, while the `function_call_output` handed
  back to the model re-sends the full tool definition verbatim so it can
  self-correct.
- Debug CLI: `cargo run -p nota-llm --example chat -- <args>`
  (`examples/chat.rs`) creates / loads sessions against a live API; session
  files land in the current working directory, and missing `--url` /
  `--model` / `--key` options fall back to `.nota/config.toml` (cwd first,
  then home) via `nota-infra` as a dev-dependency. Handy for reproducing
  provider-side issues such as `web_search` handling.
- Sessions are managed by `nota-llm::SqliteSessionManager` (implements the
  core `SessionManager` trait; created per persona by `nota-cli` with the
  storage root, system prompt, tool registry, and routing/approval ports).
  `Session::send(content, request_id)` runs the whole turn; `raw_history`
  exposes items for the chatlog API. The system prompt is fixed at manager
  creation (built once from `solo.md` / `memory.md`), so persona file edits
  apply on restart.
- **Caching & cost (2026-08-17)**: DeepSeek's Responses endpoint is
  **stateless** — `previous_response_id`/`conversation`/`store` are not
  supported. Cost savings come from DeepSeek's automatic prefix cache
  (enabled by default): the request prefix must be **byte-identical** between
  turns. We keep the prefix stable by (a) appending history in stored order,
  (b) sorting the tool list by name in `ToolRegistry::list` (HashMap order
  used to be random — a silent cache killer), and (c) keeping the request
  prefix deterministic (fixed `instructions`, persona `Context` items first
  in `input`). The response `id` is parsed and persisted per session in the
  `sessions.response_id` column for future stateful providers; DeepSeek's
  `usage.prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` are logged at
  DEBUG.

## Tool system

- Tool loop: max 16 iterations, LLM → tool_calls → execute → append results.
  It lives inside `SqliteSession::send` (`nota-llm`); the Responses wire
  types (`ToolDef` / `LlmResponse`) are internal to `nota-llm`.
- `Tool` / `ToolContext` / `ToolParams` / `PropertyDef` and the concrete
  in-memory `ToolRegistry` live in `nota-core::tool`; infra and onebot
  register their tools on a shared `ToolRegistry` instance (created in
  `nota-cli`, injected via `Arc`, resolved live on every call).
- Built-ins (`nota-infra`, registered by `register_builtin_tools`):
  `file_read`/`file_write` (sandboxed to the persona workspace; `//allow_read
  <path>` grants workspace-external reads without per-call approval, unlisted
  paths go through the permission round-trip), `schedule` (Scheduler port +
  TokioScheduler; ISO-8601 reminders delivered into the target conversation with
  `sender = "scheduler"`), `status` (version, platform, pid, uptime, current
  persona/conversation/request).
- Slash commands are intercepted in `PersonaRuntime::run` before anything
  reaches the session: `//clear` (archives the current session and creates a
  fresh one, acks), `//allow_read <path>`.

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

- Adapter in `nota-onebot` (depends only on `nota-core` — the `Tool` trait
  now lives in core), **forward WebSocket
  only** (`mode = "ws"`, default `ws://127.0.0.1:3001`; auth via
  `Authorization: Bearer <token>`). `OnebotConfig` in `config.toml [onebot]`:
  `enabled`, `mode`, `ws_url`, `access_token`, `persona` (empty = first persona
  found), `prefix`, `friend_ids`, `group_ids`. `enabled = true` requires ≥ 1
  persona (or a valid `persona` name) or the server refuses to start.
- Routing: inbound message → targeted persona inbox carrying the
  `Conversation` (`onebot_private_<id>` / `onebot_group_<id>`); sending is
  **explicit** — `reply` delivers into the current conversation,
  `onebot_send_msg` sends to another allowlisted QQ chat. Nothing is
  auto-routed at turn end and there is no `skip_reply`; "不要回答" is honored
  by the persona simply not calling a send tool. Sends re-check the
  allowlist and are chunked at 4000 chars.
- Allowlist: only `friend_ids` / `group_ids` reach the persona or get replies;
  empty list = nobody in that category. Outbound to a non-allowlisted target
  triggers a permission oneshot resolved by plain `同意` / `拒绝` (or `同意N` /
  `拒绝N`) in the originating conversation.
- Non-text segments render uniformly as `[{segment_type} msg id:<id>]` (e.g.
  `[image msg id:123]`; `reply` quotes become `[reply msg id:…]`); text keeps
  its content. Inbound headers: `[好友 昵称(id)]` (private) / `[群 群号 昵称(id)]`
  (group). Voice → `[record msg id:<id>]`, transcribed on demand via
  `onebot_voice_text` (NapCat `fetch_ptt_text`, retries 3× 2s apart, id sent
  as string).
- Tools (all `onebot_*` prefixed): `onebot_send_msg`, `onebot_get_msg_history`
  (`target` = private/group, via NapCat `get_group_msg_history` /
  `get_friend_msg_history`; reading any chat is not allowlist-gated),
  `onebot_get_content`, `onebot_status` (connection + login info),
  `onebot_voice_text`; registered via `OneBotBridge::register_tools` (fails
  on name collision). OneBot has no interactive approval channel, so tool
  permission requests are auto-denied with a notice to the chat.
- NapCat quirks (verified live 2026-08-13): it resets the WS connection when
  the **client** sends an unsolicited Ping — only pong the server's pings.
  It must listen on `0.0.0.0` for LAN clients; it does not queue events for
  disconnected clients (messages sent during a disconnect window are lost);
  `fetch_ptt_text` is intermittent (retcode=1200 then success on retry).

### solo.md template + conversation message DEBUG logs (2026-08-14)
- `nota-infra/assets/solo.md` template rewritten (user-approved): identity,
  chat-style reply rules, honesty + "不要回答" silence contract, tool/memory
  usage (`file_read/write` in workspace, `schedule`, `reply`,
  `status`, memory.md), and session context (independent sessions, identity
  headers like `[好友 昵称(id)]`). Only affects newly created personas —
  existing `~/.nota/personas/*/solo.md` are user config and untouched.
- File logs at DEBUG now carry the conversation message flow:
  `ConversationManager` logs `[in] conversation '…' -> persona '…' from '…':
  <content>` (deliver) and `[out] conversation '…' target '…': <content>`
  (route_outbound, covers `reply` / `onebot_send_msg`, slash acks). Core stays on
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
