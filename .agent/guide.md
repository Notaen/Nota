# Project Guide

## Commit Convention

Use [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): description`

- `feat:` — new feature
- `fix:` — bug fix
- `refactor:` — code change that neither fixes nor adds
- `docs:` — documentation only
- `chore:` — tooling, deps, CI

## Code Modification Rules

- Never delete or modify existing comments without explicit approval. Chinese comments are authoritative.
- Read `.agent/notes.md` before making changes.

## Architecture (Hexagonal / Ports & Adapters)

One-way dependency flow: `nota-cli → nota-infra → nota-core`, plus
`nota-onebot → nota-core` (core never sees axum/reqwest/tungstenite). See
`AGENTS.md` for the crate table and `.agent/notes.md` for design decisions.

### Source Layout

```
crates/nota-core/src/
├── bus.rs                  # EventBus (mpsc broadcast) + BusEvent + EventKind
├── permissions.rs          # PermissionRegistry (pending permission oneshots keyed by id)
├── llm.rs                  # LlmClient trait + ToolDef/ToolCall/LlmResponse/LlmItem
├── history.rs              # HistoryKind + HistoryEntry (session chat history)
├── session.rs              # SessionManager + Session (deliver, slash commands)
├── scheduler.rs            # Scheduler port
├── tool.rs                 # Tool / ToolRegistry traits + ToolContext (bus + permissions)
├── agent/mod.rs            # AgentRunner: LLM ↔ tool loop, returns LlmItem list
└── persona/mod.rs          # Persona + PersonaStore trait + PersonaRuntime (event loop)

crates/nota-infra/src/
├── persona_store/mod.rs    # FilePersonaStore (solo.md/memory.md, mtime cache)
├── llm/mod.rs              # OpenAiLlm (Responses API)
├── history.rs              # SqliteHistoryStore (per-session history.db)
├── config/mod.rs           # Config + ConfigStore
├── scheduler.rs            # TokioScheduler
├── tool/{mod,builtin}.rs   # ToolRegistryImpl + file_read/file_write/schedule/status
└── http/{mod,ws,api,admin}.rs  # axum router: REST /api/*, WS /ws/chat, /admin/stop

crates/nota-onebot/src/
├── config.rs               # OnebotConfig
├── types.rs                # OneBot 11 events, segments, actions
├── client.rs               # forward-WS client (reconnect, echo correlation)
├── api.rs                  # OneBotApi::call (echo → oneshot, 15s timeout)
├── bridge.rs               # bus bridge: allowlist filter, routing, permission auto-deny
└── tools.rs                # send_message / read_group_chat / get_msg / get_login_info / get_voice_text
```

### Runtime Layout

```
~/.nota/
├── personas/<name>/          # persona workspace: solo.md, memory.md
├── sessions/<id>/history.db  # per-session chat history (SQLite, kind column)
├── .logs/                    # rotating logs (30-day)
└── config.toml               # api_url, api_key, model, web_search, [onebot]
```

`base_dir()` is resolved in `nota-cli` (`dirs::home_dir().join(".nota")`) and
injected into adapters; the core never touches paths.

## Tech Stack

- Rust (edition 2024, rustc ≥ 1.85)
- Axum 0.8 (with `ws` feature), Tokio, reqwest, rusqlite (bundled SQLite)
- serde, serde_json, TOML
- log (core/infra) / tracing (cli only, via `tracing-log::LogTracer`)
- dialoguer + console (cli onboarding wizard)

## Event Bus

`nota-core::bus::EventBus` is a multi-producer / multi-consumer FIFO. Every
subscriber (`bus.subscribe()`) gets its own unbounded mpsc receiver; `bus.send(event)`
clones the event to all of them.

`BusEvent`:
```rust
pub struct BusEvent {
    pub kind: EventKind,                  // Message | PermissionRequest
    pub sender: String,
    pub content: String,
    pub timestamp: i64,
    pub context: String,
    pub request_id: Option<String>,
    pub parent_request_id: Option<String>,
    pub target: Option<String>,           // if Some, only the persona with this name processes it
}
```

Persona loop filters by `target`:
```rust
if event.sender == name { continue; }
if let Some(ref t) = event.target { if t != &name { continue; } }
```

The HTTP layer subscribes once, tracks `active_request_ids` per WS connection,
and only forwards events whose `request_id` (or `parent_request_id` for permission
requests) is in that set — so multiple WS clients coexist without leaking each
other's messages.

## API Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/health` | Health check |
| GET | `/api/personas` | List personas |
| POST | `/api/personas` | Create persona (`{"name": "..."}`) |
| GET | `/api/personas/:name` | Persona info |
| DELETE | `/api/personas/:name` | Delete persona |
| GET | `/api/personas/:name/files/:filename` | Read persona file |
| PUT | `/api/personas/:name/files/:filename` | Write persona file (`{"content": "..."}`) |
| GET | `/api/personas/:name/chatlog/:session_id` | Read session history |
| GET | `/api/settings` | Get config (api_url, api_key, model) |
| PUT | `/api/settings` | Update config |
| GET | `/ws/chat` | WebSocket: chat channel |
| POST | `/admin/stop` | Graceful shutdown |

### WebSocket protocol (`/ws/chat`)

Client → Server:
```json
{ "type": "send", "persona": "alice", "content": "hello", "request_id": "<uuid>" }
{ "type": "permission", "permission_id": "<uuid>", "approved": true }
```

Server → Client:
```json
{ "type": "message", "content": "hi", "request_id": "<uuid>" }
{ "type": "permission_needed", "permission_id": "<uuid>", "prompt": "Allow file_read on /etc/passwd?", "request_id": "<uuid>" }
{ "type": "error", "content": "..." }
```

## OneBot 11 (QQ bot)

Forward-WebSocket only (`mode = "ws"`). Config lives in `[onebot]` inside
`config.toml`; `enabled = false` by default; empty `persona` = first persona
found; a configured persona that does not exist fails server startup. See
README for the config example and `.agent/notes.md` for routing, allowlist,
and permission details.

## Pitfalls

1. **Chinese comments are authoritative** — they may be self-criticism, TODOs,
   or rules. If a comment describes a concrete fix, implement it and record the
   decision in `.agent/notes.md`. Never delete a comment without understanding
   why it was there.
2. **Keep core pure** — never add `axum`/`reqwest`/`serde_json`/`tracing`/
   `dialoguer`/`dirs`/`walkdir`/`tokio-tungstenite` to `nota-core`. `tokio`
   (sync only) and `serde` are fine.
3. **No global state** — `OnceLock<T>` / `RwLock<Option<T>>` are forbidden for
   manager singletons. `nota-cli` creates adapters and injects them via `Arc`.
4. **Domain types over generics** — model the domain directly
   (`ToolParams`/`PropertyDef` for JSON Schema, `LlmItem`/`HistoryKind` for
   history). Serialization happens at the infra boundary, not in core.
5. **Logging** — `nota-core`/`nota-infra` use the `log` facade; `nota-cli`
   bridges it into `tracing` via `tracing_log::LogTracer`. Don't call
   `tracing::*` from core/infra.
6. **Async blocking** — never call `blocking_*` on a tokio RwLock from inside
   an async context. Use `.write().await`. `blocking_*` panics the runtime.
7. **WebSocket ↔ bus routing** — the WS handler filters events by `request_id`
   in its `active_request_ids` set; events with mismatched ids are silently
   dropped (don't echo other clients' messages).
8. **Default persona is gone** — `nota` no longer auto-creates any persona.
   Use `nota onboard` or manually create files under `~/.nota/personas/<name>/`.
9. **NapCat forward WS drops client pings** — NapCat (and likely other OneBot
   implementations) resets the WebSocket connection immediately when the
   *client* sends an unsolicited Ping frame (observed: disconnect exactly at
   the client ping interval). The OneBot WS client must NOT send pings; just
   respond to the server's pings with pongs. Verified end-to-end with NapCat
   on 2026-08-13.
