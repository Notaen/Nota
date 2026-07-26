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

The web UI lives in a separate repo ([`Notaen/Nota.Webui`](https://github.com/Notaen/Nota.Webui))
included as a git submodule. Build it once, then serve it via the CLI:

```sh
git submodule update --init
cd webui && bun install && bun run build && cd ..
cargo run -p nota-cli -- webui     # serves webui/dist/ on :5173
```

Then open <http://127.0.0.1:5173>. The browser talks to the Rust server on
`:2349` via WebSocket (`/ws/chat`) and REST (`/api/*`).

## Architecture

The Cargo workspace has three crates; dependency flow is strictly one-way
`nota-cli → nota-infra → nota-core`.

| Crate | Role | Notable deps |
|-------|------|--------------|
| `nota-core` | Domain entities, port traits (`PersonaStore`, `LlmClient`, `Tool`, `ToolRegistry`, `AgentRunner`), `EventBus`, `PermissionRegistry`. Pure: no I/O, no JSON serialization. | `log`, `serde`, `async-trait`, `chrono`, `anyhow`, `tokio` (sync) |
| `nota-infra` | Adapters: `axum` HTTP (REST + WebSocket), filesystem persona store, `OpenAiLlm`, TOML config, built-in tools. Implements the `nota-core` ports. | `nota-core`, `axum` (with `ws` feature), `reqwest`, `serde_json`, `tower-http` |
| `nota-cli` | Binary (`nota`). Subcommands `onboard` (wizard) / `webui` (static server) / default (run server). Wires adapters and starts everything. | `nota-core`, `nota-infra`, `tracing`, `dialoguer` |

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

Frontend: Bun + React 19 + Vite 6 + Tailwind 3 (in `Nota.Webui`)

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
├── crates/
│   ├── nota-core/    # domain + ports + EventBus + PermissionRegistry
│   ├── nota-infra/   # adapters (axum HTTP/WS, persona_store, llm, config, tools)
│   ├── nota-cli/     # binary: `nota` (server) / `nota webui` (static) / `nota onboard`
└── webui/            # git submodule → github.com/Notaen/Nota.Webui
```

Runtime data under the user's home directory:

```
~/.nota/
├── personas/
│   └── <name>/
│       ├── solo.md        # system prompt
│       ├── memory.md      # long-term memory
│       └── chatlog.json   # recent conversation (rewritten on dream)
├── .logs/                 # rotating logs (30-day)
└── config.toml            # api_url, api_key, model
```

`base_dir()` is resolved in `nota-cli` (`dirs::home_dir().join(".nota")`) and
injected into adapters; core never touches paths.

## Documentation

- [`.agent/guide.md`](.agent/guide.md) — architecture, commit conventions, pitfalls
- [`.agent/notes.md`](.agent/notes.md) — design decisions and refactor history
- [`AGENTS.md`](AGENTS.md) — required reading for AI coding assistants