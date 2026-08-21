# .agent/

Agent-facing depth, organized **by topic**. The agent entry point is
`AGENTS.md` (required reading, commands, hard rules, general lessons);
`CONTRIBUTING.md` is the overview and hard-rule index and points here.

One fact lives in exactly one doc: each topic file below is the single home
for its area's behavior. `CONTRIBUTING.md` never restates a topic fact. When
a behavior changes, edit only the owning topic file plus code docstrings; if
several docs need the same edit, the fact is duplicated — keep the one copy
and replace the others with pointers.

## Files

| File | Read when... | Purpose |
|------|--------------|---------|
| `session.md` | Touching `nota-llm`, sessions, storage, `web_search` | Session manager, turn loop, storage schema, Responses API wire details, tool args validation, debug CLI |
| `tool.md` | Touching tools, `ToolRegistry`, `reply` | Tool contract, registry, built-ins, chat tools, naming/registration |
| `onebot.md` | Touching `nota-onebot` | OneBot 11 config, routing, allowlist, tools, NapCat quirks |
