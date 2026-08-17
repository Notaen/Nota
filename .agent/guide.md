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
`nota-cli → nota-onebot → nota-llm → nota-core` (onebot implements tools
against `nota-llm`), and `nota-infra → nota-llm → nota-core`. Core never sees
axum/reqwest/tungstenite/rusqlite and holds **no LLM content and no tools**.
See `AGENTS.md` for the crate table and `.agent/notes.md` for design decisions.

### Terminology

- **session** = one LLM-level dialogue (OpenAI-style message items: messages
  and tool calls/results), managed by `nota-llm`. Sessions are
  conversation-agnostic: uuid v4 ids, one SQLite file per session, no default
  store path — the caller points the manager at a directory and tracks the
  current session id itself.
- **conversation** = the user-visible chat (OneBot private/group, web) owned by an
  adapter; `nota-core` routes by conversation. A conversation can rotate
  through several sessions; old ones stay archived.

### Source Layout

```
crates/nota-core/src/
├── permissions.rs          # PermissionRegistry + PathPolicy
├── conversation.rs         # Conversation + ConversationManager (deliver / route_outbound / send_permission)
├── scheduler.rs            # Scheduler port
└── persona/mod.rs          # Persona + PersonaStore trait

crates/nota-llm/src/
├── llm.rs                  # LlmClient trait + ToolDef/ToolCall/LlmResponse/LlmItem (OpenAI message-item shape)
├── tool.rs                 # Tool / ToolRegistry / ToolRegistryImpl + ToolContext + ToolParams/PropertyDef
├── session.rs              # LlmSession + LlmSessionManager (create / session / latest / list)
├── store.rs                # SqliteSessionStore (one <session_id>.db per session, caller-specified dir)
├── agent.rs                # AgentRunner: LLM ↔ tool loop (register_tool), returns LlmItem list
└── responses.rs            # OpenAiLlm (Responses API)

crates/nota-infra/src/
├── persona_store/mod.rs    # FilePersonaStore (solo.md/memory.md, mtime cache)
├── persona_runtime.rs      # PersonaRuntime: conversation → LLM session → reply loop + slash commands
├── config/mod.rs           # Config + ConfigStore
├── scheduler.rs            # TokioScheduler
├── tool/{mod,builtin,chat}.rs  # ToolRegistryImpl + file_read/file_write/schedule/status/send_message/skip_reply
└── http/{mod,ws,api,admin}.rs  # axum router: REST /api/*, WS /ws/chat, /admin/stop

crates/nota-onebot/src/
├── config.rs               # OnebotConfig
├── types.rs                # OneBot 11 events, segments, actions
├── client.rs               # forward-WS client (reconnect, echo correlation)
├── api.rs                  # OneBotApi::call (echo → oneshot, 15s timeout)
├── bridge.rs               # allowlist filter, routing, approval round-trip
└── tools.rs                # read_group_chat / get_msg / get_login_info / get_voice_text
```

### Runtime Layout

```
~/.nota/
├── personas/<name>/                  # persona workspace: solo.md, memory.md
├── conversation/<conversation_id>/  # per-conversation dirs; inside: current.json + <session_id>.db files
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

`nota-llm` owns the dialogues: sessions are conversation-agnostic, each one an
ordered list of OpenAI-style message items with a uuid v4 id and its own
SQLite file. The llm crate has **no default store path** — `PersonaRuntime`
gives each conversation its own directory (`~/.nota/conversation/<id>/`) and
persists the current session id in `current.json` there; first contact reads
that pointer (creating and persisting a fresh session when absent), and
`//clear` calls `create()` and rewrites the pointer while the old session
stays archived. Items are stored verbatim as JSON in `<session_id>.db`; the
system prompt is **not** persisted — callers inject it per request. Each
session stores the last Responses API `response_id` for future stateful
continuations. DeepSeek is stateless: cost savings come from its automatic
prefix cache, so keep request prefixes byte-identical (history in stored
order, tools sorted by name).

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
   `tokio` (sync only) and `serde` are fine. LLM domain types and tool
   abstractions live in `nota-llm`, never in core.
3. **No global state** — `OnceLock<T>` / `RwLock<Option<T>>` are forbidden for
   manager singletons. `nota-cli` creates adapters and injects them via `Arc`.
4. **Domain types over generics** — model the domain directly
   (`ToolParams`/`PropertyDef` for JSON Schema, `LlmItem` for dialogue
   items). Serialization happens at the infra boundary, not in core.
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
