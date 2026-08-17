# .agent/

**Read this first.** Files in this directory are project-specific instructions for AI agents.

Human docs (start here for context): `README.md` / `README.zh.md` (what Nota
does, quick start) and `CONTRIBUTING.md` (full architecture, HTTP API,
directory layout).

## Hard Rules (non-negotiable)

1. **Never delete or modify existing code comments** without explicit approval.
   Chinese comments are authoritative — they may be self-criticism, TODOs, or design context. When in doubt, ask.
2. **Ask before guessing**. Propose a brief plan first, wait for confirmation, then implement.
3. **Commit only when asked**, using [Conventional Commits](https://www.conventionalcommits.org/) (`feat/fix/refactor/docs/chore(scope): description`).
4. **Proactively fix** English grammar, typos, and code style issues. Call out tech choices, naming, or design problems you disagree with.

## Files

| File | Read when... | Purpose |
|------|--------------|---------|
| `notes.md` | **Always** | Hard rules, current architecture, design decisions. |
| `guide.md` | Working on code (Rust, API, WS) | Architecture, routing, pitfalls (HTTP API / tech stack: see `CONTRIBUTING.md`). |
| `retrospective.md` | Optional | Past session lessons — mistakes to avoid. |

Start with `notes.md`. Read `guide.md` before writing Rust code.
