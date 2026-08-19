# LLM sessions & storage

Agent-facing deep dive. The canonical system description lives in
`CONTRIBUTING.md` (Terminology, LLM sessions & caching); this file holds the
wire/storage-level details agents need before touching `nota-llm`, sessions,
or the SQLite store.

## Session abstraction

- `nota-core::session` owns the abstractions: `Session` / `SessionManager`,
  `SessionItem`, `MessageRole`, `ToolCall` / `ToolCallKind`. No LLM wire
  types live in core.
- `nota-llm::SqliteSessionManager` is the **only public llm surface** and the
  only session manager implementation. It is constructed by the composition
  root (`nota-cli`) with:
  - the storage root path (one directory per conversation, chosen by
    `PersonaRuntime`);
  - the system prompt — a fixed constant built once at startup, **not**
    derived from persona files (persona content is injected as `Context`
    items at session creation);
  - the persona context string;
  - the shared in-memory `ToolRegistry`;
  - the `ConversationManager` / `PermissionRegistry` ports.
- API: `create()` / `load(id)` / `archive(id)` / `list()`. Sessions are
  **conversation-agnostic**: plain uuid v4 ids, flat `<uuid>.db` files under
  the root. Which session is current is a caller concern (`current.json`,
  written by `PersonaRuntime`; `//clear` archives and starts fresh).
- `Session::send(content, request_id)` runs the whole turn internally and
  returns nothing: delivery happens through tools and through
  `PersonaRuntime`'s auto-delivery of the final assistant text.

## Turn loop (`SqliteSession::run_turn`)

1. Append the user `Message` item (the conversation layer passes
   `prefix + content`).
2. Build `ToolDef` list once per turn from the live registry (sorted by name
   for prefix-cache stability).
3. Call the LLM via the internal `ChatLlm` trait (`OpenAiLlm` is the real
   implementation; tests substitute mocks). Save `response_id` when
   returned.
4. Persist `reasoning` items verbatim — even empty ones: DeepSeek thinking
   mode requires every prior `reasoning_text` to be echoed.
5. For each `tool_call`:
   - `function_call`: validate arguments against the tool's `parameters`
     (see Tool args below), execute the tool with a per-session
     `ToolContext`, and persist the `tool_call` immediately followed by its
     `tool_call_output` (DeepSeek requires each output to directly follow
     its call).
   - `web_search_call`: persist the call only — the provider executes the
     search server-side, so no output item is produced.
6. Only a locally executed `function_call` continues the loop (max
   `MAX_ITERATIONS = 16`). A response with only built-in calls
   (`web_search`, …) uses its final text and ends the turn.
7. The final assistant `Message` item is persisted; the conversation layer
   delivers it into the chat.

## Storage (`nota-llm::store`)

One SQLite file per session (`rusqlite` bundled). Tables:

- `meta(key, value)`: `created_at` (Unix millis), monotonic `seq`,
  `version` (= `env!("CARGO_PKG_VERSION")` — a future release converts old
  files), `response_id` (last Responses API id, for future stateful
  providers), `archived`.
- `item(id INTEGER PK AUTOINCREMENT, type INTEGER NOT NULL, role INTEGER,
  content TEXT, kind INTEGER, call_id TEXT, name TEXT, arguments TEXT,
  output TEXT, timestamp INTEGER NOT NULL)`.

`type` (numeric, `0` reserved): `1` message, `2` reasoning, `3` tool_call,
`4` tool_call_output. `role` (numeric): `0` reserved, `1` user,
`2` assistant, `3` context — the system prompt is never stored.
`kind` for tool calls: `1` function_call, `2` web_search_call. Per-kind
payload columns are used; the rest stay `NULL`.

## Responses API wire details

- Request: `POST {api_url}/responses`. Top-level `instructions` comes only
  from the system prompt. Tools use the flat shape
  `{"type":"function", name, description, parameters}` plus the built-in
  `{"type":"web_search"}` when `Config.web_search` (default `true`).
- `input` mapping (stored order — keep the prefix byte-identical for the
  provider's prefix cache):
  - messages: `user` / `assistant` / `context` → `system` input message;
  - `reasoning` items → `reasoning` input items, echoed verbatim (including
    empty ones);
  - `function_call` → `function_call` input item (call_id + name +
    arguments);
  - `web_search_call` → passed back as
    `{"type":"web_search_call","id",…,"action":{"type":"web_search_call_action","query"}}`
    — DeepSeek restores the server-side search results from the id, so
    follow-up turns keep the search context;
  - `function_call_output` → `function_call_output` input item.
- Output parsing:
  - `message` content parts → reply text;
  - `function_call` → `ToolCall` (kind `function_call`) via `call_id`
    (fallback `id`);
  - `reasoning` (content + summary parts) → `LlmResponse.reasoning`;
  - `web_search_call` → `ToolCall` (kind `web_search_call`,
    `name = "web_search"`, `arguments = {"query": …}` from `action.query` —
    the only content the provider returns; results are injected into the
    model context and DeepSeek ignores `include`);
  - unknown items are ignored.
- DeepSeek is stateless: cost savings come from its automatic prefix cache,
  so the request prefix must stay byte-identical between turns (stored
  history order, tool list sorted by name, fixed `instructions`, `Context`
  items first). Cache hit/miss tokens are logged at DEBUG.

## Tool args contract

`Tool::run` receives parsed `HashMap<String, Value>` using core's own
JSON-like `Value` type (mirroring `serde_json::Value`'s shape — `serde_json`
itself stays out of core). The llm layer deserializes the model's raw
arguments and validates them against the tool's `parameters` before calling:
required present, no unknown properties, type and enum match. Rejections are
split in two:

- operator diagnostic: provider (base url), model, the raw output, and the
  reasons, logged via `log::warn!`;
- model feedback: the `function_call_output` handed back re-sends the full
  tool definition verbatim so the model can self-correct.

## Debug CLI

`cargo run -p nota-llm --example chat -- <args>` creates / loads sessions
against a live API; session files land in the current working directory.
Missing `--url` / `--model` / `--key` fall back to `.nota/config.toml` (cwd
first, then home) via `nota-infra` as a dev-dependency. Handy for
reproducing provider-side issues such as `web_search`.
