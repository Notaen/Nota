# .agent/

Agent-facing depth and history, organized **by topic**. The agent entry point
is `AGENTS.md` (required reading, commands, hard rules, general lessons); the
canonical system reference is `CONTRIBUTING.md`.

One fact lives in exactly one doc: `CONTRIBUTING.md` is canonical, this
directory holds agent-facing depth and history. When a behavior changes,
update the single owning file plus any code docstrings — not several copies.

## Files

| File | Read when... | Purpose |
|------|--------------|---------|
| `session.md` | Touching `nota-llm`, sessions, storage, `web_search` | Session manager, turn loop, storage schema, Responses API wire details, tool args validation, debug CLI |
| `tool.md` | Touching tools, `ToolRegistry`, `reply` | Tool contract, registry, built-ins, chat tools, naming/registration |
| `onebot.md` | Touching `nota-onebot` | OneBot 11 config, routing, allowlist, tools, NapCat quirks |
| `decision.md` | Before reversing or extending a past decision | Design decisions & refactor history |
