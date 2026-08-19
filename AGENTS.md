# AGENTS.md

## Required reading

Start with the human docs to understand what Nota is and how it works:

- `README.md` / `README.zh.md` — what Nota does, quick start, OneBot setup
- `CONTRIBUTING.md` — the system reference: architecture, HTTP API,
  directory layout, hard rules & pitfalls

Then read the `.agent` file for the area you are changing, before making
changes:

- `.agent/session.md` — LLM sessions, storage schema, Responses API wire
  details (before touching `nota-llm`, sessions, storage, web_search)
- `.agent/tool.md` — tool contract, registry, built-ins, chat tools (before
  touching tools, `ToolRegistry`, `reply`)
- `.agent/onebot.md` — OneBot 11 adapter (before touching `nota-onebot`)
- `.agent/decision.md` — design decisions & refactor history (read before
  reversing or extending a past decision)

**One fact lives in exactly one doc.** `CONTRIBUTING.md` is the canonical
system reference; `.agent/*` hold agent-facing depth and history. When a
behavior changes, update the single owning doc plus any code docstrings —
not several copies.

## Commands

```sh
cargo build                          # build (default: nota-cli)
cargo run -p nota-cli                # run server (REST + WS on :2349)
cargo run -p nota-cli -- onboard     # configure API + create a persona
cargo check                          # type-check
cargo check -p nota-core             # type-check single crate
cargo clippy --all-targets           # lint
cargo test --workspace               # run all tests
```

Tests live next to the code (unit tests per crate); there is no CI yet.

## Hard rules

The numbered invariants in `CONTRIBUTING.md` → **Hard rules (do not break)**
apply to every change. Agent-specific rules below are in addition:

- **Comments are contracts**: never delete or modify a comment without
  understanding it; Chinese comments are authoritative.
- **Never edit `solo.md` without asking**: both the
  `nota-infra/assets/solo.md` template and user persona files
  (`~/.nota/personas/*/solo.md`) are user configuration. Propose wording
  first, apply only after explicit approval.
- **Documentation conventions**: `README.md` English / `README.zh.md`
  Chinese (kept in sync) / `CONTRIBUTING.md` English / `.agent/*` English.
  Keep architecture/API detail out of the READMEs. Before editing docs,
  list the repo root (`Get-ChildItem -Force`) to discover all doc files.
- **Commits** use Conventional Commits (see `CONTRIBUTING.md` → Build &
  verify), and only when the user asks.

## Orientation

```
nota-cli → nota-infra → nota-core
nota-cli → nota-llm → nota-core
nota-cli → nota-onebot → nota-core
```

Only the composition root (`nota-cli`) references `nota-llm`; infra and
onebot hold the core abstractions and never see the LLM client or the turn
loop. Full crate table + terminology: `CONTRIBUTING.md` → Architecture
overview / Terminology.

- **session** = one LLM-level dialogue (plain uuid, flat `<uuid>.db`);
  **conversation** = the user-visible chat an adapter owns.
- **Auto-delivered answer**: the turn's final assistant text is delivered
  into the current conversation by `PersonaRuntime`; `reply` /
  `onebot_send_msg` remain for explicit or intermediate sends.

## General lessons

- When the user says "add", don't replace existing types — keep both.
- Don't auto-add defaults (persona, …) unless explicitly requested; don't
  hardcode names that belong in filesystem config.
- Don't over-engineer minimal asks.
- Idiomatic English in logs and user-facing strings.
