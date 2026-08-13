# Nota

A persona-driven AI agent framework built around an in-process **event bus**.
Each persona is an independent runtime that owns its chatlog, system prompt,
and LLM session. Storage is file-based per persona — no database, no global
session registry. Adapters over `axum` expose a small REST API plus a
WebSocket channel for streaming chat and permission requests.

## Build & Run

```sh
cargo build
cargo run -p nota-cli -- onboard   # configure API + create your first persona
cargo run -p nota-cli              # start the server (REST + WS on :2349)
```

## OneBot 11 (QQ bot)

OneBot 11 support is a standalone Rust crate, `nota-onebot` (no JS/plugin runtime).
It currently speaks **forward WebSocket**: `nota` connects to your OneBot
implementation (NapCat / LLOneBot / Lagrange, default `ws://127.0.0.1:3001`),
forwards incoming private/group messages to a persona, and sends the persona's
reply back to the originating chat.

Configure it in `~/.nota/config.toml` (or answer the prompts in
`nota onboard`):

```toml
[onebot]
enabled = true
mode = "ws"                        # only forward WebSocket for now
ws_url = "ws://127.0.0.1:3001"     # your OneBot implementation's WS server
access_token = ""                  # optional
persona = "default"                # persona that answers; empty = first persona
prefix = ""                        # optional: only reply to messages starting with this
friend_ids = [123456789]            # allowlist: only these friends get a reply
group_ids = []                      # allowlist: only these groups get a reply
```

Notes:

- Non-text segments (image, face, at, …) are flattened to placeholders before
  reaching the LLM; replies are sent as plain text and chunked at 4000 chars.
- Inbound messages are prefixed with the chat identity and QQ numbers
  (`[私聊 昵称(QQ) → bot(QQ)]` / `[群 群号 昵称(QQ) → bot(QQ)]`), so the
  persona always knows who is talking (sender QQ, group QQ, bot's own QQ).
- A `reply` quote segment carries its message id as `[回复消息ID:…]`; the
  `get_msg` tool fetches the quoted message by that id, and group history
  lines include `消息ID:…` so the persona can correlate quotes to messages.
- **Sessions & send/receive**: each chat endpoint (a QQ friend, a group, a
  web client) is its own conversation **session** with two history layers:
  `deep.json` (full LLM context) and `shallow.json` (only messages actually
  delivered to the user). The persona's final answer is auto-routed back to
  the originating session; `skip_reply` / empty output suppress it
  ("不要回答" is honored). `send_message(target: "private:<QQ>" |
  "group:<QQ>", content)` lets the persona proactively message any
  allowlisted session (e.g. from a private chat into a group), and every
  delivered message lands in that session's shallow layer.
- **Allowlist**: the persona only responds to `friend_ids` / `group_ids`.
  Messages from anyone else are dropped before the LLM is ever called.
  Empty list means nobody in that category is allowed.
- `read_group_chat` tool: the persona can actively fetch recent messages of
  *any* group via NapCat's `get_group_msg_history` (extended API over the same
  WS) — e.g. ask it "what did group 123456 talk about?" without it ever
  speaking in the group. Each line includes the speaker's QQ number.
- `get_login_info` tool: the persona can query the bot's own QQ number and
  nickname via the standard OneBot API.
- OneBot tools are registered together by `OneBotBridge::register_tools`;
  the CLI never touches individual OneBot tool types.
- OneBot has no interactive permission channel yet, so tool permission
  requests are auto-denied with a notice to the chat.
- `enabled = true` requires at least one persona to exist (or a valid
  `persona` name); the server refuses to start otherwise.

## Architecture

The Cargo workspace has four crates; dependency flow is strictly one-way
`nota-cli → nota-infra → nota-core`.

| Crate | Role | Notable deps |
|-------|------|--------------|
| `nota-core` | Domain entities, port traits (`PersonaStore`, `LlmClient`, `Tool`, `ToolRegistry`, `AgentRunner`), `EventBus`, `PermissionRegistry`. Pure: no I/O, no JSON serialization. | `log`, `serde`, `async-trait`, `chrono`, `anyhow`, `tokio` (sync) |
| `nota-infra` | Adapters: `axum` HTTP (REST + WebSocket), filesystem persona store, `OpenAiLlm`, TOML config, built-in tools. Implements the `nota-core` ports. | `nota-core`, `nota-onebot`, `axum` (with `ws` feature), `reqwest`, `serde_json` |
| `nota-onebot` | OneBot 11 transport adapter (forward WebSocket): protocol types, WS client, bus bridge, `read_group_chat` tool. Not part of core or infra. | `nota-core`, `tokio-tungstenite`, `serde_json`, `uuid` |
| `nota-cli` | Binary (`nota`). Subcommands `onboard` (wizard) / default (run server). Wires adapters and starts everything. | `nota-core`, `nota-infra`, `nota-onebot`, `tracing`, `dialoguer` |

### Runtime model

```
                         EventBus (mpsc broadcast)
                              │
        ┌──────────────┬──────┴──────────────┐
        ▼              ▼                     ▼
  Persona "alice"  Persona "bob"       HTTP /ws/chat
```

- The bus carries `BusEvent { kind, sender, content, request_id, parent_request_id, target, … }`.
- `BusEvent.target` (optional) routes a message to one specific persona; without it, all
  subscribers receive the event.
- Each persona has its own `PersonaRuntime` event loop: receive event →
  build prompt from `solo.md` + chatlog → call LLM → handle tool calls →
  post assistant response back to the bus.
- The HTTP/WS layer is also a bus subscriber. Each WebSocket connection
  tracks its own `active_request_ids` and only forwards events that match —
  so multiple browser tabs don't leak each other's messages.

### Permission flow

When a tool wants to do something that requires approval (e.g. `file_read`
on a path outside the persona workspace), it calls
`ToolContext::request_permission(prompt)`. That:

1. Registers a oneshot in `PermissionRegistry` under a fresh UUID.
2. Sends a `PermissionRequest` event on the bus with `parent_request_id`
   set to the original user request.
3. Awaits the oneshot.

The WS handler forwards it to the matching browser tab as
`{type:"permission_needed", permission_id, prompt, request_id}`. The user
clicks Allow or Deny; the browser sends
`{type:"permission", permission_id, approved}` back. The WS handler calls
`PermissionRegistry::resolve(id, approved)` directly (no extra bus event).
The tool resumes; the persona finishes; the final response flows back as
`{type:"message", content, request_id}`.

## Tech Stack

Rust 2024 · Axum 0.8 (REST + WebSocket) · Tokio · reqwest · serde ·
serde_json · TOML · `log` (core/infra) / `tracing` (cli) · dialoguer (wizard)

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
| GET | `/api/personas/:name/chatlog` | Read chatlog |
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
{ "type": "permission_needed", "permission_id": "<uuid>", "prompt": "Allow file_read on /etc/passwd?", "request_id": "<uuid>" }
{ "type": "error",             "content": "..." }
```

## Layout

```
nota/
└── crates/
    ├── nota-core/    # domain + ports + EventBus + PermissionRegistry
    ├── nota-infra/   # adapters (axum HTTP/WS, persona_store, llm, config, tools)
    ├── nota-onebot/  # OneBot 11 forward-WS transport + read_group_chat tool
    └── nota-cli/     # binary: `nota` (server) / `nota onboard`
```

Runtime data under the user's home directory:

```
~/.nota/
├── personas/
│   └── <name>/
│       ├── solo.md        # system prompt
│       ├── memory.md      # long-term memory
├── .logs/                 # rotating logs (30-day)
├── sessions/
│   └── <session_id>/history.db  # conversation history (SQLite, per session)
└── config.toml            # api_url, api_key, model
```

`base_dir()` is resolved in `nota-cli` (`dirs::home_dir().join(".nota")`) and
injected into adapters; core never touches paths.

## Documentation

- [`.agent/guide.md`](.agent/guide.md) — architecture, commit conventions, pitfalls
- [`.agent/notes.md`](.agent/notes.md) — design decisions and refactor history
- [`AGENTS.md`](AGENTS.md) — required reading for AI coding assistants
