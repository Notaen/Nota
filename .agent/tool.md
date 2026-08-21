# Tool system

Agent-facing deep dive. Current behavior is described only in
`CONTRIBUTING.md` (Hard rules, permission flow); this file holds tool-depth
details agents need before touching tools, the registry, or `reply`, and
points back to the canonical sections instead of restating them.

## Contract (in `nota-core`)

- `Tool` trait: `name()` / `description()` / `parameters()` /
  `run(HashMap<String, Value>, ToolContext) -> Result<String>`.
- `ToolParams` / `PropertyDef` model JSON Schema directly — no
  `serde_json::Value` or raw `String` as parameter types in core;
  serialization happens at the llm wire layer.
- `ToolContext` is per-session: `persona_name`, `manager`
  (`ConversationManager`), `request_id`, `permissions`
  (`PermissionRegistry`). Sessions never reference conversations —
  conversation-bound tools bake their `conversation_id` into the struct
  (e.g. `ReplyTool`).
- `ToolRegistry`: concrete in-memory registry (no trait/impl split). Tools
  are resolved **live** on every call, and `register` **fails** on a
  duplicate name — every registration site propagates the error, so a name
  conflict stops startup instead of silently shadowing the earlier tool.

## Built-ins (`nota-infra`, `register_builtin_tools`)

- `file_read` / `file_write` — sandboxed to the persona workspace;
  `//allow_read <path>` grants workspace-external reads without per-call
  approval; unlisted paths go through the permission round-trip.
- `schedule` — `Scheduler` port + `TokioScheduler`; ISO-8601 reminders
  delivered into the target conversation with `sender = "scheduler"`.
- `status` — version, platform, pid, uptime, current
  persona/conversation/request.

## Chat tools (`nota-infra`, `register_chat_tools`)

- `reply` — conversation-layer tool with the conversation id baked in at
  construction. Sends a message into the current conversation via
  `route_outbound`.
- `wait` — conversation-layer tool (conversation id baked in) that holds
  the conversation open when a message looks semantically incomplete.
  Parameters: `seconds` (optional integer, default 10, `0` = until the next
  message) and `reason` (optional string). It registers the wait with the
  infra `WaitHub` (per-conversation state, one per conversation, latest
  replaces previous); a real inbound message cancels it (the runtime calls
  `WaitHub::cancel` on every non-`wait_timeout` message and on `//clear`),
  and a timeout delivers a `[等待超时]` notice into the conversation as an
  ordinary message (`sender = "wait_timeout"`) so the model decides what to
  do next — ask, wait again, or stay silent. Consecutive budget:
  `MAX_CONSECUTIVE_WAITS = 3` per conversation, reset by a real message or
  `//clear`; a call beyond the budget is rejected with a tool error. A
  successful `wait` call stops the turn and persists a `Wait` marker instead
  of tool-call rows (storage + turn-loop details: `.agent/session.md`).
- Adapter tools are namespaced (`onebot_*`, see `.agent/onebot.md`).

## Naming & registration

- Reserved runtime names: `file_read`, `file_write`, `schedule`, `status`,
  `reply`, `wait`.
- Adapter families use a prefix: the OneBot family is `onebot_*`.
- Registration sites: `register_builtin_tools`, `register_chat_tools`,
  `OneBotBridge::register_tools`; all propagate duplicate-name errors.

## Tool loop (inside the session)

Max 16 iterations. Each `function_call` is validated against its
`parameters` before execution; a `function_call_output` is persisted for
every executed call (item order: `.agent/session.md` → Turn loop).
`web_search_call` is recorded but never executed locally — the provider runs
it server-side.
