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
  message items (messages + tool calls/results; the system prompt is injected
  per request, not stored), managed by `nota-llm` (`LlmSession` /
  `LlmSessionManager`). Each session has a uuid v4 id and its own SQLite file;
  the llm crate has no default store path — the caller supplies a directory
  (typically `~/.nota/conversation/<conversation_id>/` with flat
  `<session_id>.db` files). Sessions are conversation-agnostic; upper layers
  persist the current session id in `current.json` in that directory and read
  it directly.
- **conversation** = the user-visible chat (OneBot private/group, web) owned by an
  adapter; `nota-core`'s `Conversation` / `ConversationManager` routes by it.

Never mix the two: user-facing chats are **conversations**, model contexts are
**sessions**. A conversation can rotate through several sessions over time
(each is a separate file); `//clear` creates a fresh one while older ones stay
archived.

## Architecture overview

Five crates with strictly one-way dependencies: `nota-cli → nota-infra →
nota-core`, `nota-cli → nota-onebot → nota-llm → nota-core` (onebot
implements tools against `nota-llm`), and `nota-infra → nota-llm → nota-core`.

| Crate | Role | Notable deps |
|-------|------|--------------|
| `nota-core` | Domain types and ports: `Persona`/`PersonaStore`, `Conversation`/`ConversationManager`, `PermissionRegistry`, `PathPolicy`, `Scheduler`. Pure: no I/O, no LLM content, no tools. | `log`, `async-trait`, `chrono`, `anyhow`, `tokio` (sync only), `uuid` |
| `nota-llm` | LLM capability: Responses API client, `LlmItem`/`LlmClient` types, `AgentRunner` (LLM ↔ tool loop), conversation-agnostic session management (SQLite, one dialogue per session), tool abstractions (`Tool`/`ToolRegistry`/`ToolContext`). | `nota-core`, `reqwest`, `rusqlite`, `serde_json`, `uuid` |
| `nota-infra` | Adapters: axum HTTP (REST + WS), filesystem persona store, `PersonaRuntime` (conversation → LLM session → reply loop, slash commands), TOML config, built-in tools, scheduler. | `nota-core`, `nota-llm`, `nota-onebot`, axum, `serde_json` |
| `nota-onebot` | OneBot 11 forward-WebSocket transport: protocol types, WS client, allowlist + approval bridge, tools. | `nota-core`, `nota-llm`, `tokio-tungstenite`, `serde_json`, `uuid` |
| `nota-cli` | Binary `nota`: `onboard` wizard / run server; wires adapters (DI). | `nota-core`, `nota-infra`, `nota-onebot`, tracing, dialoguer, console |

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

`PersonaRuntime` (in `nota-infra`) turns an inbound message into an LLM turn:
open the conversation's directory (`conversation/<conversation_id>/`), read
the `current.json` pointer (or create a fresh session and persist it on first
contact) → append the user item → build the system prompt from the persona
files → run the agent loop (LLM ↔ tools) → append the result items → route
the final text back through the conversation.

Slash commands are intercepted by `PersonaRuntime` before anything reaches the
LLM: `//clear` rotates to a fresh LLM session (history rows are never deleted;
the old session stays archived and its context no longer reaches the model);
`//allow_read <path>` grants workspace-external reads without per-call
approval.

### LLM sessions & caching

`nota-llm` owns the dialogues: `LlmSessionManager` has no default store path
— the caller points it at a directory (one per conversation, holding flat
`<session_id>.db` files). `create()` makes a fresh uuid session, `session(id)`
retrieves one, and `list()` exposes every archived session and its raw items.
Which session is current is a caller concern: `PersonaRuntime` persists the
id in `current.json` inside the conversation directory. Each session stores
its last Responses API id in its `meta` table (for future stateful
providers). The system prompt is **not** persisted — callers inject it per
request, so persona file edits apply immediately. DeepSeek's Responses
endpoint is stateless, so cost savings rely on its automatic prefix cache:
keep the request prefix byte-identical (stored history order, tool list
sorted by name). Cache hit/miss tokens are logged at DEBUG.

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
├── nota-core/    # domain types + ports + conversation routing + permissions
├── nota-llm/     # LLM client, agent loop, LLM session/history management
├── nota-infra/   # adapters (HTTP/WS, persona store, persona runtime, config, tools)
├── nota-onebot/  # OneBot 11 forward-WS transport
└── nota-cli/     # binary: nota (server) / nota onboard

~/.nota/
├── personas/<name>/              # solo.md (personality), memory.md (memory)
├── conversation/<conversation_id>/   # per-conversation dirs; inside: current.json + <session_id>.db files
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

## Code conventions

- **Core purity**: never add axum/reqwest/serde_json/tracing/dialoguer/dirs/
  walkdir/tokio-tungstenite/rusqlite to `nota-core`, and no LLM types or tool
  abstractions there. `tokio` (sync only) and `serde` are fine.
- **Domain types over generics**: model the domain directly
  (`ToolParams`/`PropertyDef`, `LlmItem`); no raw
  `serde_json::Value`/`String` as parameter types in core. JSON
  (de)serialization happens at the boundary.
- **No global singletons**: managers are created by `nota-cli` and injected
  via `Arc`; no `OnceLock<T>` / `RwLock<Option<T>>` manager singletons.
- **Comments are contracts**: Chinese comments are authoritative — read them
  before changing. `solo.md` (both the template and user persona files) is
  user configuration: never edit without explicit user approval.
- **Workspace deps**: direct dependency versions are controlled in
  `[workspace.dependencies]` at the root; each crate picks its own features.

## See also

- [README.md](README.md) / [README.zh.md](README.zh.md) — end-user docs
