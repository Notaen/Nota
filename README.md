# Nota

A persona-driven AI agent framework. Each persona is an independent runtime
with its own system prompt (`solo.md`), memory (`memory.md`), and LLM session;
persona storage is plain files. Chat happens in per-endpoint **sessions**
(`~/.nota/sessions/<id>/history.db`). Adapters over `axum` expose a small REST
API plus a WebSocket channel for streaming chat and permission requests.

## Build & Run

```sh
cargo build
cargo run -p nota-cli -- onboard   # configure API + create your first persona
cargo run -p nota-cli              # start the server (REST + WS on :2349)
```

First run: `nota` fails fast instead of auto-configuring — run `nota onboard`
once to set up the API and create a first persona (or `nota persona create`).

## OneBot 11 (QQ bot)

`nota-onebot` is a standalone Rust crate (no JS/plugin runtime) speaking
**forward WebSocket**: `nota` connects to your OneBot implementation
(NapCat / LLOneBot / Lagrange, default `ws://127.0.0.1:3001`), forwards
private/group messages to a persona, and routes replies back.

Configure in `~/.nota/config.toml` (or answer `nota onboard`):

```toml
[onebot]
enabled = true
mode = "ws"                        # only forward WebSocket for now
ws_url = "ws://127.0.0.1:3001"     # your OneBot implementation's WS server
access_token = ""                  # optional (sent as Authorization: Bearer)
persona = "default"                # persona that answers; empty = first persona
prefix = ""                        # optional: only reply to messages starting with this
friend_ids = [123456789]           # allowlist: only these friends get replies
group_ids = []                     # allowlist: only these groups get replies
```

Notes:

- **Routing**: each chat (friend/group/web) is its own session. The persona's
  final answer is auto-routed back to the originating session, and
  `skip_reply` / empty output suppresses it ("不要回答" is honored).
  `send_message(target: "private:<QQ>" | "group:<QQ>", content)` sends
  proactively to any allowlisted session.
- **Allowlist**: only `friend_ids` / `group_ids` reach the persona or get
  replies; empty list = nobody in that category. Outbound to a non-allowlisted
  target asks for approval via `同意` / `拒绝`.
- **Media**: non-text segments (image, face, at, …) arrive as
  `[{segment_type} msg id:<id>]` (e.g. `[image msg id:123]`) so the persona
  knows what arrived and which message to fetch with a tool; replies are plain
  text, chunked at 4000 chars.
- **Tools**: `read_group_chat` (fetch recent messages of *any* group via
  NapCat's `get_group_msg_history`, without speaking there), `get_msg`,
  `get_login_info`, `get_voice_text` (NapCat `fetch_ptt_text` transcription).
- OneBot has no interactive permission channel, so tool permission requests
  are auto-denied with a notice to the chat.
- `enabled = true` requires at least one persona (or a valid `persona` name).

## Architecture

Four crates; dependency flow is strictly one-way
`nota-cli → nota-infra → nota-core` (`nota-onebot` also depends only on core).

| Crate | Role | Notable deps |
|-------|------|--------------|
| `nota-core` | Domain entities, port traits (`PersonaStore`, `LlmClient`, `Tool`, `ToolRegistry`, `AgentRunner`), `EventBus`, `PermissionRegistry`, `SessionManager`. Pure: no I/O. | `log`, `serde`, `async-trait`, `chrono`, `anyhow`, `tokio` (sync) |
| `nota-infra` | Adapters: `axum` HTTP (REST + WS), filesystem persona store, `OpenAiLlm` (Responses API), SQLite history store, TOML config, built-in tools. | `nota-core`, `nota-onebot`, `axum` (ws), `reqwest`, `rusqlite`, `serde_json` |
| `nota-onebot` | OneBot 11 forward-WS transport: protocol types, WS client, bus bridge, tools. | `nota-core`, `tokio-tungstenite`, `serde_json`, `uuid` |
| `nota-cli` | Binary (`nota`): `onboard` wizard / run server. Wires adapters (DI). | `nota-core`, `nota-infra`, `nota-onebot`, `tracing`, `dialoguer`, `console` |

### Runtime model

The bus carries `BusEvent { kind, sender, content, request_id,
parent_request_id, target, … }`. `target` routes a message to one persona;
without it, all subscribers receive the event. Each persona runs a
`PersonaRuntime` loop: receive event → build prompt from `solo.md` + history →
call LLM → handle tool calls → post the response back to the bus. The HTTP/WS
layer is a subscriber that only forwards events matching each connection's
`active_request_ids`, so multiple clients never leak each other's messages.

### Permission flow

A tool that needs approval (e.g. `file_read` outside the workspace) calls
`ToolContext::request_permission(prompt)` → oneshot in `PermissionRegistry` +
`PermissionRequest` bus event → the WS layer forwards
`{type:"permission_needed", permission_id, prompt, request_id}` to the client →
the user answers `{type:"permission", permission_id, approved}` → the resolver
completes the oneshot → the tool resumes and the final response flows back as
`{type:"message", content, request_id}`.

## Tech Stack

Rust 2024 · Axum 0.8 (REST + WebSocket) · Tokio · reqwest · rusqlite · serde ·
TOML · `log` (core/infra) / `tracing` (cli) · dialoguer + console (wizard)

## API

REST:

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/health` | Health check |
| GET | `/api/personas` | List personas |
| POST | `/api/personas` | Create persona (`{"name": "..."}`) |
| GET | `/api/personas/:name` | Persona info |
| DELETE | `/api/personas/:name` | Delete persona |
| GET | `/api/personas/:name/files/:filename` | Read persona file |
| PUT | `/api/personas/:name/files/:filename` | Write persona file |
| GET | `/api/personas/:name/chatlog/:session_id` | Read session history |
| GET | `/api/settings` | Get config |
| PUT | `/api/settings` | Update config |
| POST | `/admin/stop` | Graceful shutdown |

WebSocket (`/ws/chat`):

```
# client → server
{ "type": "send",       "persona": "alice", "content": "hi", "request_id": "<uuid>" }
{ "type": "permission", "permission_id": "<uuid>", "approved": true }

# server → client
{ "type": "message",           "content": "hi",  "request_id": "<uuid>" }
{ "type": "permission_needed", "permission_id": "<uuid>", "prompt": "...", "request_id": "<uuid>" }
{ "type": "error",             "content": "..." }
```

## Layout

```
crates/
├── nota-core/    # domain + ports + EventBus + PermissionRegistry
├── nota-infra/   # adapters (HTTP/WS, persona_store, llm, history, config, tools)
├── nota-onebot/  # OneBot 11 forward-WS transport
└── nota-cli/     # binary: `nota` (server) / `nota onboard`

~/.nota/
├── personas/<name>/       # solo.md (system prompt), memory.md
├── sessions/<id>/history.db  # per-session chat history (SQLite)
├── .logs/                 # rotating logs (30-day)
└── config.toml            # api_url, api_key, model, web_search, [onebot]
```

`base_dir()` is resolved in `nota-cli` (`dirs::home_dir().join(".nota")`) and
injected into adapters; core never touches paths.

## Documentation

- [`.agent/guide.md`](.agent/guide.md) — architecture, commit conventions, pitfalls
- [`.agent/notes.md`](.agent/notes.md) — design decisions and current architecture
- [`AGENTS.md`](AGENTS.md) — required reading for AI coding assistants
