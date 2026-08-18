# Contributing

This document is for developers who read or modify the Nota codebase. For
normal usage see [README.md](README.md) (English) or
[README.zh.md](README.zh.md) (中文).

## Documentation languages

- `README.md` — English, end-user focused.
- `README.zh.md` — Chinese, end-user focused (kept in sync with `README.md`).
- `CONTRIBUTING.md` — English, developer focused.
- `AGENTS.md` / `.agent/*` — English, for AI coding assistants (they reference
  the human docs above; human docs never reference agent docs).

When changing docs, list the repo root first (`Get-ChildItem -Force`) and keep
each file in its language and audience. Do not put architecture details in
the READMEs — they belong here.

## Build & verify

```sh
cargo build                          # build (default: nota-cli)
cargo run -p nota-cli                # run server (REST + WS on :2349)
cargo run -p nota-cli -- onboard     # config wizard + create a persona
cargo test --workspace               # unit tests
cargo clippy --all-targets           # lint
```

Commits use [Conventional Commits](https://www.conventionalcommits.org/):
`feat:` / `fix:` / `refactor:` / `docs:` / `chore:` with a scope, e.g.
`refactor(llm): move LLM sessions into nota-llm`.

## Terminology (important)

- **session** = one LLM-level dialogue: an ordered list of OpenAI-style
  message items (messages + tool calls/results; the system prompt is a fixed
  constant injected per request and not stored, while persona context is
  stored as `Context` items), abstracted in `nota-core` (`Session` /
  `SessionManager`) and implemented by `nota-llm`'s `SqliteSessionManager`.
  Ids are conversation-namespaced (`<conversation_id>/<uuid>`), stored as
  `<uuid>.db` under `~/.nota/conversation/<persona>/<conversation_id>/`. The
  manager tracks the current session per conversation (`current.json`) and
  archives old sessions on `//clear`; callers only `send(content,
  request_id)` into a session — the turn loop, tools, and delivery are
  internal to the session.
- **conversation** = the user-visible chat (OneBot private/group, web) owned by an
  adapter; `nota-core`'s `Conversation` / `ConversationManager` routes by it.

Never mix the two: user-facing chats are **conversations**, model contexts are
**sessions**. A conversation can rotate through several sessions over time
(each is a separate file); `//clear` creates a fresh one while older ones stay
archived.

## Architecture overview

Five crates with strictly one-way dependencies:
`nota-cli → nota-infra → nota-core`, `nota-cli → nota-llm → nota-core`, and
`nota-cli → nota-onebot → nota-core`. Only the composition root (`nota-cli`)
references `nota-llm`; `nota-infra` and `nota-onebot` hold the core
abstractions and never see the LLM client or the turn loop.

| Crate | Role | Notable deps |
|-------|------|--------------|
| `nota-core` | Domain types and ports: `Persona`/`PersonaStore`, `Conversation`/`ConversationManager`, `PermissionRegistry`, `PathPolicy`, `Scheduler`, `Session`/`SessionManager`, and the tool contract (`Tool`/`ToolContext`/`ToolParams`/`ToolRegistry`, in-memory). Pure: no I/O, no LLM wire types. | `log`, `async-trait`, `chrono`, `anyhow`, `serde`, `tokio` (sync only), `uuid` |
| `nota-llm` | Concrete session manager (`SqliteSessionManager`): SQLite, one `<uuid>.db` per session, runs the whole turn (Responses API + tool loop) internally. No public `LlmClient`/`AgentRunner`. | `nota-core`, `reqwest`, `rusqlite`, `serde_json`, `tokio`, `uuid` |
| `nota-infra` | Adapters: axum HTTP (REST + WS), filesystem persona store, `PersonaRuntime` (conversation → session turn, slash commands), TOML config, built-in tools, scheduler. | `nota-core`, `nota-onebot`, axum, `serde_json` |
| `nota-onebot` | OneBot 11 forward-WebSocket transport: protocol types, WS client, allowlist + approval bridge, `onebot_*` tools. | `nota-core`, `tokio-tungstenite`, `serde_json`, `uuid` |
| `nota-cli` | Binary `nota`: `onboard` wizard / run server; composition root (DI) — assembles `ToolRegistry`, per-persona `SqliteSessionManager`, and adapters. | `nota-core`, `nota-llm`, `nota-infra`, `nota-onebot`, tracing, dialoguer, console |

### Runtime model

There is no global broadcast bus: `ConversationManager` routes by conversation.

- **Inbound**: an adapter calls `manager.deliver(&Conversation, sender,
  prefix, content, request_id)` → the message lands in the target persona's
  inbox (`subscribe_persona`), consumed by `PersonaRuntime`.
- **Outbound**: the persona calls `manager.route_outbound(conversation_id,
  target, content, request_id)` → the adapter(s) with that conversation's
  prefix receive `AdapterEvent::Outbound` (or all adapters when only a
  channel-agnostic `target` is set).
- **Permissions**: `manager.send_permission(...)` routes the request to the
  conversation's adapter for user confirmation.

`PersonaRuntime` (in `nota-infra`) turns an inbound message into a session
turn: resolve the conversation's current session via the core
`SessionManager` (creating one on first contact) → `session.send(display,
request_id)`. The session — inside `nota-llm` — appends the user item, runs
the LLM + tool loop with the system prompt and tool registry it was created
with, and persists the result. **Sending is explicit**: nothing is delivered
automatically at the end of a turn — the persona speaks only by calling
`reply` (current conversation) or an adapter send tool such as
`onebot_send_msg` (other chats); there is no `skip_reply`.

Slash commands are intercepted by `PersonaRuntime` before anything reaches the
session: `//clear` archives the current session via the manager and creates a
fresh one (history rows are never deleted; the old session stays readable via
the chatlog API); `//allow_read <path>` grants workspace-external reads
without per-call approval.

### LLM sessions & caching

The composition root creates one `SqliteSessionManager` per persona with a
storage root (`~/.nota/conversation/<persona>/`), a fixed system prompt (not
derived from persona files), the persona context (`solo.md` / `memory.md`,
seeded into every new session as `Context` items), the shared core
`ToolRegistry`, and the routing/approval ports, then injects it as
`Arc<dyn SessionManager>`.
Sessions are conversation-namespaced (`<conversation_id>/<uuid>`, files at
`<root>/<conversation_id>/<uuid>.db`); the manager tracks the current session
per conversation in `current.json` and `archive()` marks old ones on
`//clear`. Each session stores its last Responses API id in its `meta` table
(for future stateful providers). Tools are resolved **live** from the
registry on every call, so registering/unregistering takes effect
immediately. Roles (`MessageRole`) are stored as plain numbers — `0`
reserved, `1` user, `2` assistant, `3` context — and the llm crate maps them
to provider string roles only when building a request (the system prompt is a
`SessionManager` constructor argument sent as `instructions`, never a stored
role; persona `Context` items are emitted as `system` input messages).
DeepSeek's Responses endpoint is stateless, so cost savings rely on its
automatic prefix cache: keep the request prefix byte-identical (stored
history order, tool list sorted by name). Cache hit/miss tokens are logged
at DEBUG.

### Permission flow

When a tool needs approval (e.g. `file_read` outside the workspace) it calls
`ToolContext::request_permission(prompt)` → `PermissionRegistry` registers a
oneshot → `ConversationManager::send_permission` routes to the conversation's
adapter → the adapter surfaces the prompt → on allow/deny the adapter calls
`PermissionRegistry::resolve(id, approved)` directly → the tool resumes.

The OneBot channel has no interactive approval panel: users answer 「同意」 /
「拒绝」 in chat (「同意N」 / 「拒绝N」 selects the N-th queued request). The web
channel uses the WS `permission_needed` / `permission` messages.

## HTTP API

The server listens on `127.0.0.1:2349`.

### REST

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/health` | Health check |
| GET | `/api/personas` | List personas |
| POST | `/api/personas` | Create persona (`{"name": "..."}`) |
| GET | `/api/personas/:name` | Persona info |
| DELETE | `/api/personas/:name` | Delete persona |
| GET | `/api/personas/:name/files/:filename` | Read persona file |
| PUT | `/api/personas/:name/files/:filename` | Write persona file (`{"content": "..."}`) |
| GET | `/api/personas/:name/chatlog/:conversation_id` | Raw LLM history of all sessions in the conversation's directory (`[{session_id, created_at, messages: [(row_id, item)]}]`) |
| GET | `/api/settings` | Get config |
| PUT | `/api/settings` | Update config |
| POST | `/admin/stop` | Graceful shutdown |

### WebSocket (`/ws/chat`)

Each connection is its own `web_<uuid>` conversation; connections never leak
each other's messages.

```jsonc
// client → server
{ "type": "send",       "persona": "alice", "content": "hi", "request_id": "<uuid>" }
{ "type": "permission", "permission_id": "<uuid>", "approved": true }

// server → client
{ "type": "message",           "content": "hi",  "request_id": "<uuid>" }
{ "type": "permission_needed", "permission_id": "<uuid>", "prompt": "...", "request_id": "<uuid>" }
{ "type": "error",             "content": "..." }
```

## Directory layout

```
crates/
├── nota-core/    # domain types + ports (incl. Session/SessionManager) + tool contract
├── nota-llm/     # concrete SqliteSessionManager (turn loop + SQLite), internal Responses client
├── nota-infra/   # adapters (HTTP/WS, persona store, persona runtime, config, built-in tools)
├── nota-onebot/  # OneBot 11 forward-WS transport + onebot_* tools
└── nota-cli/     # binary: nota (server) / nota onboard

~/.nota/
├── personas/<name>/              # solo.md (personality), memory.md (memory)
├── conversation/<persona>/<conversation_id>/  # per-conversation dirs; inside: current.json + <uuid>.db files
├── .logs/                        # rotating logs (30-day)
└── config.toml                   # api_url, api_key, model, web_search, [onebot]
```

`base_dir()` (default `~/.nota`) is resolved in `nota-cli` and injected into
the adapters; core never touches paths.

## Tech stack

Rust 2024 (rustc ≥ 1.85) · Axum 0.8 (REST + WS) · Tokio · reqwest ·
rusqlite (bundled SQLite) · serde/serde_json · TOML · `log`
(core/llm/infra) and `tracing` (cli, bridged via `tracing-log`) ·
dialoguer + console (wizard)

## Hard rules (do not break)

Numbered invariants, each with the failure symptom it prevents. If you hit
one of those symptoms, you are almost certainly violating the corresponding
rule.

1. **Keep core pure** — never add `axum` / `reqwest` / `serde_json` /
   `tracing` / `dialoguer` / `dirs` / `walkdir` / `tokio-tungstenite` /
   `rusqlite` to `nota-core`, and no LLM wire types (Responses API,
   reqwest, SQLite) there. `tokio` (sync only) and `serde` are fine; the
   session and tool abstractions live in core by design. Breaking this
   drags I/O and framework types into the domain and the port/adapters
   boundary stops being testable.
2. **Domain types over generics** — model the domain directly
   (`ToolParams`/`PropertyDef`, `SessionItem`); no raw
   `serde_json::Value`/`String` as parameter types in core. JSON
   (de)serialization happens at the boundary. Generic wrappers leak the wire
   format into core, so changing a JSON shape silently changes the domain
   API.
3. **No global singletons** — managers are created by `nota-cli` and injected
   via `Arc`; no `OnceLock<T>` / `RwLock<Option<T>>` manager singletons.
   Hidden shared state makes startup order matter and makes adapters
   impossible to swap or test in isolation.
4. **Comments are contracts** — Chinese comments are authoritative: read them
   before changing and never delete one you do not understand. `solo.md`
   (both the template and user persona files) is user configuration: never
   edit without explicit user approval. Editing a persona file behind the
   user's back changes the bot's personality without consent.
5. **Workspace deps** — direct dependency versions are controlled in
   `[workspace.dependencies]` at the root; each crate picks its own
   features. Adding a version locally lets crates drift apart and duplicate
   the same dependency in the lockfile.
6. **Tool names are namespaced** — runtime/built-in names (`file_read`,
   `file_write`, `schedule`, `status`, `reply`) are reserved;
   adapter tool families use a prefix (the OneBot family is `onebot_*`).
   `ToolRegistry::register` **fails** on a duplicate name and every
   registration site propagates that error, so a conflict stops startup
   instead of silently shadowing the earlier tool.
7. **Sending is explicit** — nothing is delivered automatically at the end
   of a turn. The persona speaks only by calling `reply` (current
   conversation) or an adapter send tool such as `onebot_send_msg` (other
   chats). There is no reply slot and no `skip_reply`: staying silent means
   simply not calling a send tool.

## See also

- [README.md](README.md) / [README.zh.md](README.zh.md) — end-user docs
