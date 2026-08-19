# Tool system

Agent-facing deep dive. The canonical system description lives in
`CONTRIBUTING.md` (Hard rules, permission flow); this file holds the
tool-contract details agents need before touching tools, the registry, or
`reply`.

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
  `route_outbound`. `PersonaRuntime` also auto-delivers the turn's final
  assistant text into the conversation, so `reply` is for explicit or
  intermediate sends. Known inconsistency: the description still says
  "reaches the user only through this tool" — see `.agent/decision.md`.
- Adapter tools are namespaced (`onebot_*`, see `.agent/onebot.md`).

## Naming & registration

- Reserved runtime names: `file_read`, `file_write`, `schedule`, `status`,
  `reply`.
- Adapter families use a prefix: the OneBot family is `onebot_*`.
- Registration sites: `register_builtin_tools`, `register_chat_tools`,
  `OneBotBridge::register_tools`; all propagate duplicate-name errors.

## Tool loop (inside the session)

Max 16 iterations. Each `function_call` is validated against its
`parameters` before execution; the `function_call_output` immediately
follows the call (DeepSeek adjacency rule). `web_search_call` is recorded
but never executed locally — the provider runs it server-side.
