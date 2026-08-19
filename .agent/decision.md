# Design decisions & history

Chronological log of design decisions and refactor history. Read before
reversing a decision: superseded entries explain why the current shape is
what it is. Current behavior lives in `CONTRIBUTING.md` and the `.agent`
topic files (`session.md`, `tool.md`, `onebot.md`); this file records the
*why*.

## 2026-08-13 — masked secrets

Wizard secrets use `prompt_masked` (`console` `Term::read_key`, one `*` per
char, `[hidden]` on Enter, empty-but-required re-prompts, Ctrl+C
interrupts). `dialoguer::Password` disables echo for the whole line and
looks like the input never arrived.

## 2026-08-14 — solo.md template, log filtering, WS isolation

- `nota-infra/assets/solo.md` template rewritten (user-approved): identity,
  chat-style reply rules, honesty + 「不要回答」 silence contract, tool/memory
  usage. Only affects newly created personas; existing
  `~/.nota/personas/*/solo.md` are user config and untouched.
- File logs at DEBUG carry the conversation message flow (`[in]` / `[out]`
  logs in `ConversationManager`). Third-party transport noise (h2 frames) is
  filtered with tracing's built-in `Targets` — hand-rolled `Filter` impls do
  NOT gate events in this setup (verified with a unit test).
- The WS handler filters events by its own `conversation_id`; mismatched
  events are silently dropped so clients never leak each other's messages.

## 2026-08-17 — LLM purification (superseded)

Sessions became one dialogue, conversation-agnostic, each with its own
SQLite file; tools moved into `nota-llm` (`AgentRunner` +
`register_tool`); onebot depended on `nota-llm`. **Superseded on
2026-08-18** by "Session manager + tool contract in core".

## 2026-08-18 — explicit sending + tool registry fail-fast

Learned from the dsh-onebot plugin design:

- Sending was made explicit: `PersonaRuntime` did not route the final
  assistant text back; `reply` (current conversation) and `onebot_send_msg`
  (other chats) were the only send paths. **Superseded on 2026-08-19** by
  auto-delivered final answers (below).
- `skip_reply` removed — nothing to suppress.
- `ToolRegistry::register` fails on duplicate names; every registration site
  propagates, so startup aborts instead of silently shadowing.

## 2026-08-18 — session manager + tool contract in core

Architectural reversal of the 2026-08-17 decision:

- Core owns `Session` / `SessionManager` and `Tool` / `ToolContext` /
  `ToolParams` / `PropertyDef` + a concrete in-memory `ToolRegistry`
  (trait/impl split gone).
- No `LlmClient`, no `AgentRunner`: the LLM call and turn loop moved inside
  `SqliteSession`; `ChatLlm` is a crate-private test seam.
- `SqliteSessionManager` is the only public llm surface, constructed by
  `nota-cli` with the storage root, system prompt (fixed, not from persona
  files), context, shared `ToolRegistry`, and routing ports.
- Storage layout (breaking): sessions at
  `~/.nota/conversation/<persona>/<conversation_id>/<uuid>.db`; ids are
  plain uuids; `current.json` is written by `PersonaRuntime`.
- Roles numeric (breaking): `MessageRole` u8 — `0` reserved, `1` user,
  `2` assistant, `3` context; the system role was removed (the system prompt
  is a `SessionManager` constructor argument sent as `instructions`).
- Dependency graph: `nota-cli → nota-infra/nota-onebot/nota-llm → nota-core`;
  infra/onebot dropped the llm dependency.
- `reply` is a conversation-layer tool with the conversation id baked into
  the struct, so sessions never reference conversations.

## 2026-08-19 — reasoning echo, web_search, tool args, auto-delivery

- **Reasoning echo**: DeepSeek thinking mode requires every prior
  `reasoning_text` to be passed back (including empty ones), so `reasoning`
  items are persisted and echoed as `reasoning` input items.
- **web_search turn loop**: a response may contain a `web_search_call` and
  the final text at once; the loop only continues for locally executed
  `function_call`s, so built-in-call responses use their text and end the
  turn.
- **web_search_call persistence**: the `web_search_call` output item is
  saved as a `tool_call` row (kind `web_search_call`,
  `name = "web_search"`, `arguments = {"query": …}` from `action.query`). No
  `tool_call_output` — DeepSeek injects results into the model context and
  ignores `include`.
- **web_search_call input passback**: rebuilding `input` passes
  `web_search_call` items back as-is (type + id + action.query); DeepSeek
  restores the server-side results from the id, so follow-up turns keep the
  search context.
- **Tool args contract**: `Tool::run` receives `HashMap<String, Value>`
  (core's own `Value`, mirroring `serde_json::Value` — `serde_json` itself
  stays out of core). Validation against `parameters` happens in the llm
  layer; rejections split into operator diagnostics (provider/model/raw
  output) and model feedback (full tool definition re-sent verbatim).
- **Auto-delivered final answer (user-directed)**: `PersonaRuntime` now
  delivers the turn's final assistant text directly into the current
  conversation after each turn (`deliver_assistant_reply`). `reply` /
  `onebot_send_msg` are untouched; silence still means producing no text.
  Known inconsistency: `reply`'s description still says "reaches the user
  only through this tool" — pending user decision.
- **Debug CLI**: `cargo run -p nota-llm --example chat -- <args>` for live
  provider debugging (falls back to `.nota/config.toml`).
