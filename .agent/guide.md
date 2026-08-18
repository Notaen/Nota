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
- The numbered hard rules in `CONTRIBUTING.md` are invariants — read them
  before touching core dependencies or tool registration.

## Architecture (Hexagonal / Ports & Adapters)

One-way dependency flow: `nota-cli → nota-infra → nota-core`, plus
`nota-cli → nota-llm → nota-core`, and `nota-cli → nota-onebot → nota-core`.
Only the composition root (`nota-cli`) references `nota-llm`; `nota-infra`
and `nota-onebot` hold the core abstractions (`SessionManager`, `Tool`) and
never see the LLM client or the turn loop. Core never sees
axum/reqwest/tungstenite/rusqlite and holds no LLM wire types. See
`AGENTS.md` for the crate table and `.agent/notes.md` for design decisions.

### Terminology

- **session** = one LLM-level dialogue (OpenAI-style message items: messages
  and tool calls/results), abstracted in `nota-core` (`Session` /
  `SessionManager`) and implemented by `nota-llm`'s `SqliteSessionManager`.
  Ids are conversation-namespaced (`<conversation_id>/<uuid>`), one SQLite
  file per session. The manager tracks the current session per conversation
  (`current.json`) and archives old ones on `//clear`; callers only
  `send(content, request_id)` into a session.
- **conversation** = the user-visible chat (OneBot private/group, web) owned by an
  adapter; `nota-core` routes by conversation. A conversation can rotate
  through several sessions; old ones stay archived.

### Source Layout

```
crates/nota-core/src/
├── permissions.rs          # PermissionRegistry + PathPolicy
├── conversation.rs         # Conversation + ConversationManager (deliver / route_outbound / send_permission)
├── scheduler.rs            # Scheduler port
├── session.rs              # Session + SessionManager traits + SessionItem/MessageRole/ToolCall
├── tool.rs                 # Tool / ToolContext / ToolParams/PropertyDef + concrete ToolRegistry (in-memory, duplicate-fail)
└── persona/mod.rs          # Persona + PersonaStore trait

crates/nota-llm/src/
├── session.rs              # SqliteSessionManager (implements core SessionManager) + SqliteSession (turn loop inside)
├── store.rs                # SqliteSessionStore (one <uuid>.db per session, per-conversation dir)
└── responses.rs            # internal Responses API client + ToolDef/LlmResponse wire types

crates/nota-infra/src/
├── persona_store/mod.rs    # FilePersonaStore (solo.md/memory.md, mtime cache)
├── persona_runtime.rs      # PersonaRuntime: conversation → session.send + slash commands
├── config/mod.rs           # Config + ConfigStore
├── scheduler.rs            # TokioScheduler
├── tool/{mod,builtin,chat}.rs  # file_read/file_write/schedule/status + reply (registered on core ToolRegistry)
└── http/{mod,ws,api,admin}.rs  # axum router: REST /api/*, WS /ws/chat, /admin/stop

crates/nota-onebot/src/
├── config.rs               # OnebotConfig
├── types.rs                # OneBot 11 events, segments, actions
├── client.rs               # forward-WS client (reconnect, echo correlation)
├── api.rs                  # OneBotApi::call (echo → oneshot, 15s timeout)
├── bridge.rs               # allowlist filter, routing, approval round-trip
└── tools.rs                # onebot_send_msg / onebot_get_msg_history / onebot_get_content / onebot_status / onebot_voice_text
```

### Runtime Layout

```
~/.nota/
├── personas/<name>/                  # persona workspace: solo.md, memory.md
├── conversation/<persona>/<conversation_id>/  # per-conversation dirs; inside: current.json + <uuid>.db files
├── .logs/                            # rotating logs (30-day)
└── config.toml                       # api_url, api_key, model, web_search, [onebot]
```

`base_dir()` is resolved in `nota-cli` (`dirs::home_dir().join(".nota")`) and
injected into adapters; the core never touches paths.

Full HTTP API, tech stack, and directory layout: see `CONTRIBUTING.md`.

## Conversation Routing

There is no global broadcast bus: `nota-core::conversation::ConversationManager`
routes messages by conversation.

- **Inbound**: an adapter calls `manager.deliver(&Conversation, sender,
  prefix, content, request_id)` → the message lands in the target persona's
  inbox (`subscribe_persona`), consumed by `PersonaRuntime`.
- **Outbound**: the persona calls `manager.route_outbound(conversation_id,
  target, content, request_id)` → every adapter with that conversation's
  prefix receives an `AdapterEvent::Outbound` (or all adapters when only a
  channel-agnostic `target` is set).
- **Permissions**: `manager.send_permission(...)` routes the request to the
  conversation's adapter, which surfaces the prompt to the user.

The HTTP/WS layer only forwards events whose `conversation_id` matches its
own web conversation, so multiple WS clients coexist without leaking each
other's messages.

## LLM Sessions

`nota-cli` creates one `SqliteSessionManager` per persona (storage root
`~/.nota/conversation/<persona>/`, a fixed system prompt plus the persona
context from `solo.md` / `memory.md`, shared core `ToolRegistry`,
routing/approval ports) and injects it as `Arc<dyn SessionManager>`. Sessions are
conversation-namespaced (`<conversation_id>/<uuid>`, files at
`<root>/<conversation_id>/<uuid>.db`); the manager tracks the current session
per conversation in `current.json`, and `//clear` archives the old session
and creates a fresh one. Roles are plain numbers (`0` reserved, `1` user,
`2` assistant, `3` context); the system prompt is a `SessionManager`
constructor argument, never a stored role. Persona context is seeded as
`Context` items and emitted as `system` input messages, and the llm crate
maps roles to provider strings only when building a request. Each session
runs the whole turn internally — append the user item, call the LLM with the
system prompt and the live tool list, execute tool calls with a per-session
`ToolContext`, persist items and the last Responses API `response_id`.
DeepSeek is stateless: cost savings come from its automatic prefix cache, so
keep request prefixes byte-identical (history in stored order, tools sorted
by name).

HTTP endpoints and the `/ws/chat` protocol: see `CONTRIBUTING.md`.

## OneBot 11

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
   `dialoguer`/`dirs`/`walkdir`/`tokio-tungstenite`/`rusqlite` to `nota-core`.
   `tokio` (sync only) and `serde` are fine. LLM wire types and I/O live in
   `nota-llm`; the session and tool abstractions live in `nota-core`.
3. **No global state** — `OnceLock<T>` / `RwLock<Option<T>>` are forbidden for
   manager singletons. `nota-cli` creates adapters and injects them via `Arc`.
4. **Domain types over generics** — model the domain directly
   (`ToolParams`/`PropertyDef` for JSON Schema, `SessionItem` for dialogue
   items). Serialization happens at the boundary (llm wire layer), not in
   core.
5. **Logging** — `nota-core`/`nota-llm`/`nota-infra` use the `log` facade;
   `nota-cli` bridges it into `tracing` via `tracing_log::LogTracer`. Don't
   call `tracing::*` from core/llm/infra.
6. **Async blocking** — never call `blocking_*` on a tokio RwLock from inside
   an async context. Use `.write().await`. `blocking_*` panics the runtime.
7. **WebSocket ↔ conversation routing** — the WS handler filters events by its
   own `conversation_id`; events with mismatched ids are silently dropped
   (don't echo other clients' messages).
8. **No auto-created default persona** — personas are never silently created
   (no hardcoded default name). `nota` fails fast with guidance instead of
   auto-jumping: missing/corrupt `config.toml` → error "run `nota onboard`";
   zero personas → error "run `nota persona create` / `nota onboard`".
9. **NapCat forward WS drops client pings** — NapCat (and likely other OneBot
   implementations) resets the WebSocket connection immediately when the
   *client* sends an unsolicited Ping frame (observed: disconnect exactly at
   the client ping interval). The OneBot WS client must NOT send pings; just
   respond to the server's pings with pongs. Verified end-to-end with NapCat
   on 2026-08-13.
10. **Tool name collisions abort startup** — `ToolRegistry::register`
    returns an error when a name is already registered, and every
    registration site propagates it, so a conflict stops `nota` from
    starting instead of silently shadowing the first tool. Built-in runtime
    names (`file_read`, `file_write`, `schedule`, `status`, `reply`)
    are reserved; adapter tools use a namespaced prefix (`onebot_*`). If
    startup fails with `duplicate tool name '…'`, rename the new tool.
11. **No auto-send, no `skip_reply`** — `PersonaRuntime` never delivers the
    final assistant text on its own. The persona speaks only via
    `reply` (current conversation) or `onebot_send_msg` (other chats);
    silence means not calling a send tool.
